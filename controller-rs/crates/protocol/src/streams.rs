//! Uplink stream epochs (docs/protocol-v1.md §3 and §4.2, spec §3.1.6):
//! the `stream.open`, `stream.end` and `uplink.ended` bodies, and the
//! [`StreamRegistry`] every uplink packet is checked against.
//!
//! The ordering rules follow from how the firmware's uplink sender
//! (`device/internal/uplink`) produces packets. Each stream epoch starts at
//! sequence 0 and adds one per packet. When an audio subscription starts
//! wanting a stream, the device queues one backfill run for it: packets from
//! the subscription's start up to the stream's live position, in sample
//! order, skipping missing audio. Runs are sent one after another before live
//! audio resumes. So a packet may begin below the highest sample already
//! seen only as the first packet of a granted run, or as the next packet of a
//! run already under way.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::num::NonZeroU64;

use crate::ema::Packet;
use crate::error::{ProtocolError, ProtocolErrorCode};
use crate::json::{Fields, Object};
use crate::vocab::{Reported, StreamId};
use serde_json::Value;

wire_enum! {
    /// Why a stream epoch opened or ended.
    pub enum StreamReason {
        /// The stream started, or the session did.
        Start => "start",
        /// Capture restarted after missing audio.
        Discontinuity => "discontinuity",
        /// The privacy mute changed.
        Privacy => "privacy",
        /// The capture clock fit reset.
        ClockReset => "clock_reset",
        /// A new render epoch restarted the reference.
        RenderEpoch => "render_epoch",
    }
}

fn stream_id(f: &Fields<'_>) -> Result<StreamId, ProtocolError> {
    f.raw("stream_id")
        .and_then(Value::as_str)
        .and_then(StreamId::from_wire)
        .ok_or_else(|| {
            ProtocolError::new(
                ProtocolErrorCode::BadStream,
                format!("unknown stream_id {}", sent(f, "stream_id")),
            )
        })
}

fn sent(f: &Fields<'_>, key: &str) -> String {
    f.raw(key)
        .map_or_else(|| "absent".to_owned(), Value::to_string)
}

fn reason(f: &Fields<'_>) -> Result<Option<Reported<StreamReason>>, ProtocolError> {
    Ok(f.optional_string("reason")?
        .map(|text| match StreamReason::from_wire(text) {
            Some(known) => Reported::Known(known),
            None => Reported::Unrecognized(text.to_owned()),
        }))
}

/// `stream.open`: a new epoch of one uplink stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamOpen {
    /// The stream.
    pub stream_id: StreamId,
    /// The epoch, a random nonzero uint64.
    pub epoch: NonZeroU64,
    /// Why it opened; `None` when not given.
    pub reason: Option<Reported<StreamReason>>,
}

impl StreamOpen {
    /// Decodes a `stream.open`. The stream must be known and its `kind`,
    /// `sample_rate` and `format` must be the ones that stream carries, and
    /// the epoch nonzero (all `bad_stream`); the epoch must be a uint64
    /// decimal string (`malformed_message`).
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        let f = Fields::new(body, "stream.open");
        let stream_id = stream_id(&f)?;
        let epoch = f.u64_string("epoch")?;
        let Some(epoch) = NonZeroU64::new(epoch) else {
            return Err(ProtocolError::new(
                ProtocolErrorCode::BadStream,
                "epoch 0 is reserved",
            ));
        };
        let kind = stream_id.kind();
        let expected = [
            ("kind", u64::from(kind as u8)),
            ("sample_rate", u64::from(kind.sample_rate())),
            ("format", u64::from(kind.format() as u8)),
        ];
        for (key, want) in expected {
            if f.raw(key).and_then(Value::as_u64) != Some(want) {
                let detail = format!("{stream_id} {key} must be {want}, got {}", sent(&f, key));
                return Err(ProtocolError::new(ProtocolErrorCode::BadStream, detail));
            }
        }
        Ok(Self {
            stream_id,
            epoch,
            reason: reason(&f)?,
        })
    }
}

/// `stream.end`: one uplink stream epoch is over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamEnd {
    /// The stream.
    pub stream_id: StreamId,
    /// The epoch.
    pub epoch: u64,
    /// The first sample index the epoch never reached; for cells and AFE in
    /// capture samples.
    pub final_sample: u64,
    /// Why it ended; `None` when not given.
    pub reason: Option<Reported<StreamReason>>,
}

impl StreamEnd {
    /// Decodes a `stream.end`: a known stream (`bad_stream`) and uint64
    /// decimal strings for `epoch` and `final_sample` (`malformed_message`).
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        let f = Fields::new(body, "stream.end");
        Ok(Self {
            stream_id: stream_id(&f)?,
            epoch: f.u64_string("epoch")?,
            final_sample: f.u64_string("final_sample")?,
            reason: reason(&f)?,
        })
    }
}

wire_enum! {
    /// `uplink.ended.reason`.
    pub enum UplinkEndedReason {
        /// The controller closed it.
        Closed => "closed",
        /// No renewal arrived in time, or a candidate was never acknowledged.
        Ttl => "ttl",
        /// The privacy mute went on.
        Mute => "mute",
        /// The capture epoch changed.
        Epoch => "epoch",
        /// Live audio queued too long.
        Overrun => "overrun",
        /// The session ended.
        Session => "session",
    }
}

/// `uplink.ended`: the device ended an audio subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UplinkEnded {
    /// The subscription (the wire's `lease_id`).
    pub subscription_id: String,
    /// Why it ended.
    pub reason: UplinkEndedReason,
    /// Per wanted stream, the last index sent (inclusive); `None` when
    /// nothing was sent.
    pub last_sample: BTreeMap<StreamId, Option<u64>>,
    /// Per wanted stream, the backfill start after clipping to the device's
    /// ring; `None` when unclipped.
    pub clipped_start: BTreeMap<StreamId, Option<u64>>,
}

impl UplinkEnded {
    /// Decodes an `uplink.ended`: a non-empty `lease_id`, a known reason, and
    /// two objects keyed by stream holding uint64 decimal strings or null.
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        let f = Fields::new(body, "uplink.ended");
        let subscription_id = f.nonempty_string("lease_id")?.to_owned();
        let reason = f
            .string("reason")
            .ok()
            .and_then(UplinkEndedReason::from_wire)
            .ok_or_else(|| {
                f.error(
                    "reason",
                    "must be closed, ttl, mute, epoch, overrun or session",
                )
            })?;
        Ok(Self {
            subscription_id,
            reason,
            last_sample: stream_samples(f.object("last_sample")?, "uplink.ended.last_sample")?,
            clipped_start: stream_samples(
                f.object("clipped_start")?,
                "uplink.ended.clipped_start",
            )?,
        })
    }
}

fn stream_samples(
    object: &Object,
    owner: &'static str,
) -> Result<BTreeMap<StreamId, Option<u64>>, ProtocolError> {
    let f = Fields::new(object, owner);
    object
        .keys()
        .map(|key| {
            let stream = StreamId::from_wire(key).ok_or_else(|| f.error(key, "is not a stream"))?;
            Ok((stream, f.optional_u64_string(key)?))
        })
        .collect()
}

/// Which epochs a new subscription's backfill grant covers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackfillGrant {
    /// One epoch: for mic, cells and AFE, the subscription's capture epoch.
    Epoch(u64),
    /// Every open epoch of the stream: for the reference, whose start the
    /// device maps onto its reference epochs itself.
    EveryOpenEpoch,
}

/// How many ended epochs the registry remembers, so that a late packet for
/// one is `stream_ended` rather than `unknown_epoch` and the epoch cannot be
/// reopened. Epochs are random uint64s, so forgetting the oldest only changes
/// which code a very late packet gets.
pub const ENDED_EPOCHS_REMEMBERED: usize = 64;

#[derive(Debug)]
struct EpochState {
    ended: bool,
    last_sequence: Option<u64>,
    /// The highest first sample of any packet so far: the live position.
    live: Option<u64>,
    /// For each backfill run under way, the sample its next packet starts at
    /// or after.
    runs: Vec<u64>,
    /// Granted runs that have not started.
    grants: u32,
}

/// The stream epochs the device announced in the current session, and the
/// ordering state of each.
#[derive(Debug)]
pub struct StreamRegistry {
    afe_metadata: bool,
    epochs: HashMap<(StreamId, u64), EpochState>,
    current: BTreeMap<StreamId, u64>,
    ended: VecDeque<(StreamId, u64)>,
}

impl StreamRegistry {
    /// An empty registry for a session; `afe_metadata` is whether its
    /// `session.ready` asked for the `afe` stream.
    pub fn new(afe_metadata: bool) -> Self {
        Self {
            afe_metadata,
            epochs: HashMap::new(),
            current: BTreeMap::new(),
            ended: VecDeque::new(),
        }
    }

    /// Records a `stream.open`. Opening an open epoch again changes nothing;
    /// reopening an ended one, or opening `afe` in a session that did not ask
    /// for it, is `bad_stream`.
    pub fn open(&mut self, open: &StreamOpen) -> Result<(), ProtocolError> {
        let (stream, epoch) = (open.stream_id, open.epoch.get());
        if stream == StreamId::Afe && !self.afe_metadata {
            return Err(ProtocolError::new(
                ProtocolErrorCode::BadStream,
                "afe stream in a session without afe_metadata",
            ));
        }
        match self.epochs.get(&(stream, epoch)) {
            Some(state) if state.ended => {
                return Err(ProtocolError::new(
                    ProtocolErrorCode::BadStream,
                    format!("{stream} epoch {epoch} already ended"),
                ));
            }
            Some(_) => {}
            None => {
                let state = EpochState {
                    ended: false,
                    last_sequence: None,
                    live: None,
                    runs: Vec::new(),
                    grants: 0,
                };
                self.epochs.insert((stream, epoch), state);
            }
        }
        self.current.insert(stream, epoch);
        Ok(())
    }

    /// Records a `stream.end`. An epoch never opened is `unknown_epoch`; a
    /// second end of the same epoch changes nothing.
    pub fn end(&mut self, end: &StreamEnd) -> Result<(), ProtocolError> {
        let key = (end.stream_id, end.epoch);
        let Some(state) = self.epochs.get_mut(&key) else {
            let detail = format!(
                "stream.end for unknown {} epoch {}",
                end.stream_id, end.epoch
            );
            return Err(ProtocolError::new(ProtocolErrorCode::UnknownEpoch, detail));
        };
        if state.ended {
            return Ok(());
        }
        state.ended = true;
        state.runs.clear();
        state.grants = 0;
        if self.current.get(&end.stream_id) == Some(&end.epoch) {
            self.current.remove(&end.stream_id);
        }
        self.ended.push_back(key);
        if self.ended.len() > ENDED_EPOCHS_REMEMBERED
            && let Some(oldest) = self.ended.pop_front()
        {
            self.epochs.remove(&oldest);
        }
        Ok(())
    }

    /// The stream's most recently opened epoch, unless it has ended.
    pub fn current(&self, stream: StreamId) -> Option<u64> {
        self.current.get(&stream).copied()
    }

    /// Whether `epoch` of `stream` is open.
    pub fn is_open(&self, stream: StreamId, epoch: u64) -> bool {
        self.epochs.get(&(stream, epoch)).is_some_and(|s| !s.ended)
    }

    /// A subscription starts wanting `stream`: allow one more backfill run
    /// on each open epoch the grant covers. A grant made before an epoch's
    /// first packet is kept too: the run may arrive behind another
    /// subscription's packets.
    pub fn grant_backfill(&mut self, stream: StreamId, grant: BackfillGrant) {
        for ((s, epoch), state) in &mut self.epochs {
            let covered = match grant {
                BackfillGrant::Epoch(e) => e == *epoch,
                BackfillGrant::EveryOpenEpoch => true,
            };
            if *s == stream && covered && !state.ended {
                state.grants = state.grants.saturating_add(1);
            }
        }
    }

    /// Checks one parsed uplink packet against its epoch and returns its
    /// stream. In order: render packets, and AFE packets in a session without
    /// `afe_metadata`, are `unexpected_kind`; an epoch never opened is
    /// `unknown_epoch`; an ended one `stream_ended`; a sequence not above the
    /// epoch's last is `sequence`; a packet that starts below the live
    /// position without starting a granted run or continuing one is
    /// `sample_order`. A rejected packet changes no state.
    pub fn check(&mut self, packet: &Packet) -> Result<StreamId, ProtocolError> {
        let header = packet.header();
        let stream = match StreamId::of_kind(header.kind) {
            Some(StreamId::Afe) if !self.afe_metadata => None,
            other => other,
        };
        let Some(stream) = stream else {
            let detail = format!(
                "kind {} is not an uplink stream of this session",
                header.kind as u8
            );
            return Err(ProtocolError::new(
                ProtocolErrorCode::UnexpectedKind,
                detail,
            ));
        };
        let epoch = header.epoch;
        let Some(state) = self.epochs.get_mut(&(stream, epoch)) else {
            let detail = format!("{stream} epoch {epoch} was never opened");
            return Err(ProtocolError::new(ProtocolErrorCode::UnknownEpoch, detail));
        };
        if state.ended {
            return Err(ProtocolError::new(
                ProtocolErrorCode::StreamEnded,
                format!("{stream} epoch {epoch} already ended"),
            ));
        }
        if let Some(last) = state.last_sequence
            && header.sequence <= last
        {
            let detail = format!("{stream} sequence {} after {last}", header.sequence);
            return Err(ProtocolError::new(ProtocolErrorCode::Sequence, detail));
        }
        let first = header.first_sample;
        let end = packet.end_sample();
        match state.live {
            Some(live) if first < live => {
                let continuing = state
                    .runs
                    .iter_mut()
                    .filter(|next| **next <= first)
                    .max_by_key(|next| **next);
                if let Some(next) = continuing {
                    *next = end;
                } else if state.grants > 0 {
                    state.grants -= 1;
                    state.runs.push(end);
                } else {
                    let detail = format!(
                        "{stream} first sample {first} goes back from {live} outside a backfill run"
                    );
                    return Err(ProtocolError::new(ProtocolErrorCode::SampleOrder, detail));
                }
            }
            _ => {
                state.runs.retain(|next| *next < first);
                state.live = Some(first);
            }
        }
        state.last_sequence = Some(header.sequence);
        Ok(stream)
    }

    /// Forgets every epoch: the session is over.
    pub fn clear(&mut self) {
        self.epochs.clear();
        self.current.clear();
        self.ended.clear();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ema::{self, AfeRecord, Cell, CellFlags, Flags, Header, Kind, SourceMask};

    const MIC: u64 = 111;
    const REF: u64 = 222;

    fn object(value: Value) -> Object {
        match value {
            Value::Object(object) => object,
            _ => panic!("object"),
        }
    }

    fn open_body(stream: &str, epoch: u64, kind: u8, format: u8) -> Object {
        object(
            json!({"stream_id": stream, "epoch": epoch.to_string(), "kind": kind,
                      "sample_rate": 16000, "format": format, "reason": "start"}),
        )
    }

    fn end_body(stream: &str, epoch: u64, final_sample: u64) -> Object {
        object(
            json!({"stream_id": stream, "epoch": epoch.to_string(), "final_sample": final_sample.to_string(),
                      "reason": "privacy"}),
        )
    }

    /// A session whose device announced its current mic, cells and
    /// reference epochs, as the firmware does right after `session.ready`.
    fn opened(afe_metadata: bool) -> StreamRegistry {
        let mut r = StreamRegistry::new(afe_metadata);
        let mut streams = vec![
            ("mic", MIC, 1, 1),
            ("cells", MIC, 4, 2),
            ("reference", REF, 2, 1),
        ];
        if afe_metadata {
            streams.push(("afe", MIC, 5, 3));
        }
        for (stream, epoch, kind, format) in streams {
            r.open(&StreamOpen::parse(&open_body(stream, epoch, kind, format)).unwrap())
                .unwrap();
        }
        r
    }

    fn packet(kind: Kind, epoch: u64, sequence: u64, first_sample: u64, frames: u32) -> Packet {
        let header = Header {
            kind,
            flags: Flags::NONE,
            epoch,
            sequence,
            first_sample,
            mono_ns: 0,
            uncertainty_us: None,
            frame_count: frames,
            generation: 0,
            source_mask: SourceMask::NONE,
        };
        let payload: Vec<u8> = match kind {
            Kind::Cells => (0..frames)
                .flat_map(|_| {
                    Cell {
                        e_centi: -100,
                        flags: CellFlags::NONE,
                        mask: SourceMask::NONE,
                    }
                    .to_bytes()
                })
                .collect(),
            Kind::Afe => (0..frames)
                .flat_map(|_| AfeRecord::default().to_bytes())
                .collect(),
            _ => vec![0; frames as usize * 2],
        };
        Packet::parse(ema::encode(&header, &payload).unwrap()).unwrap()
    }

    fn mic(sequence: u64, first: u64) -> Packet {
        packet(Kind::Mic, MIC, sequence, first, 1280)
    }

    fn code(result: Result<StreamId, ProtocolError>) -> ProtocolErrorCode {
        result.unwrap_err().code
    }

    #[test]
    fn stream_open_parameters_must_match_the_stream() {
        let ok = StreamOpen::parse(&open_body("afe", 5, 5, 3)).unwrap();
        assert_eq!(
            (ok.stream_id, ok.epoch.get(), ok.reason),
            (StreamId::Afe, 5, Some(Reported::Known(StreamReason::Start)))
        );
        let cases = [
            open_body("afe", 6, 4, 2),
            open_body("mic", 5, 1, 2),
            open_body("cells", 5, 4, 1),
            object(
                json!({"stream_id": "mic", "epoch": "5", "kind": 1, "sample_rate": 48000, "format": 1}),
            ),
            object(json!({"stream_id": "mic", "epoch": "5", "kind": 1, "format": 1})),
            object(
                json!({"stream_id": "mic", "epoch": "5", "kind": 1.5, "sample_rate": 16000, "format": 1}),
            ),
            object(
                json!({"stream_id": "render", "epoch": "5", "kind": 3, "sample_rate": 48000, "format": 1}),
            ),
            object(json!({"epoch": "5", "kind": 1, "sample_rate": 16000, "format": 1})),
            object(
                json!({"stream_id": "mic", "epoch": "0", "kind": 1, "sample_rate": 16000, "format": 1}),
            ),
        ];
        for body in cases {
            assert_eq!(
                StreamOpen::parse(&body).unwrap_err().code,
                ProtocolErrorCode::BadStream,
                "{body:?}"
            );
        }
        for epoch in [json!(5), json!("-5"), json!("five"), json!(null)] {
            let body = object(
                json!({"stream_id": "mic", "epoch": epoch, "kind": 1, "sample_rate": 16000, "format": 1}),
            );
            assert_eq!(
                StreamOpen::parse(&body).unwrap_err().code,
                ProtocolErrorCode::MalformedMessage,
                "{epoch}"
            );
        }
        let odd = object(
            json!({"stream_id": "mic", "epoch": "5", "kind": 1, "sample_rate": 16000, "format": 1,
                                "reason": "solar_flare"}),
        );
        assert_eq!(
            StreamOpen::parse(&odd).unwrap().reason,
            Some(Reported::Unrecognized("solar_flare".into()))
        );
    }

    #[test]
    fn stream_end_needs_a_known_stream_and_decimal_strings() {
        let end = StreamEnd::parse(&end_body("cells", MIC, 3840)).unwrap();
        assert_eq!(
            (end.stream_id, end.epoch, end.final_sample),
            (StreamId::Cells, MIC, 3840)
        );
        let bad_stream = object(json!({"stream_id": "video", "epoch": "1", "final_sample": "0"}));
        assert_eq!(
            StreamEnd::parse(&bad_stream).unwrap_err().code,
            ProtocolErrorCode::BadStream
        );
        let bad_final = object(json!({"stream_id": "mic", "epoch": "1", "final_sample": 0}));
        assert_eq!(
            StreamEnd::parse(&bad_final).unwrap_err().code,
            ProtocolErrorCode::MalformedMessage
        );
    }

    #[test]
    fn packets_must_name_an_open_epoch() {
        let mut r = opened(false);
        assert_eq!(
            code(r.check(&packet(Kind::Mic, 999, 0, 0, 1280))),
            ProtocolErrorCode::UnknownEpoch
        );
        assert_eq!(r.check(&mic(0, 0)), Ok(StreamId::Mic));
        r.end(&StreamEnd::parse(&end_body("mic", MIC, 1280)).unwrap())
            .unwrap();
        assert_eq!(code(r.check(&mic(1, 1280))), ProtocolErrorCode::StreamEnded);
        assert_eq!(r.current(StreamId::Mic), None);
        assert_eq!(
            r.current(StreamId::Cells),
            Some(MIC),
            "cells has its own epoch state"
        );
        assert!(r.check(&packet(Kind::Cells, MIC, 0, 0, 3)).is_ok());
    }

    #[test]
    fn an_ended_epoch_cannot_reopen_and_ending_twice_is_harmless() {
        let mut r = opened(false);
        let end = StreamEnd::parse(&end_body("reference", REF, 2560)).unwrap();
        r.end(&end).unwrap();
        r.end(&end).unwrap();
        let reopen = StreamOpen::parse(&open_body("reference", REF, 2, 1)).unwrap();
        assert_eq!(
            r.open(&reopen).unwrap_err().code,
            ProtocolErrorCode::BadStream
        );
        let unknown = StreamEnd::parse(&end_body("reference", 7, 0)).unwrap();
        assert_eq!(
            r.end(&unknown).unwrap_err().code,
            ProtocolErrorCode::UnknownEpoch
        );
        let again = StreamOpen::parse(&open_body("mic", MIC, 1, 1)).unwrap();
        r.open(&again).unwrap();
        assert!(r.is_open(StreamId::Mic, MIC));
    }

    #[test]
    fn a_render_epoch_restart_keeps_the_new_reference_epoch_current() {
        let mut r = opened(false);
        let next = 333;
        r.end(&StreamEnd::parse(&end_body("reference", REF, 2560)).unwrap())
            .unwrap();
        r.open(&StreamOpen::parse(&open_body("reference", next, 2, 1)).unwrap())
            .unwrap();
        assert_eq!(r.current(StreamId::Reference), Some(next));
        assert_eq!(
            r.check(&packet(Kind::Reference, next, 0, 0, 1280)),
            Ok(StreamId::Reference)
        );
        assert_eq!(
            code(r.check(&packet(Kind::Reference, REF, 5, 2560, 1280))),
            ProtocolErrorCode::StreamEnded
        );
    }

    #[test]
    fn render_frames_and_unrequested_afe_are_unexpected_kinds() {
        let mut r = opened(false);
        let render =
            Packet::parse(ema::encode_render(REF, 0, 0, 1, Flags::NONE, &[0; 3840]).unwrap())
                .unwrap();
        assert_eq!(code(r.check(&render)), ProtocolErrorCode::UnexpectedKind);
        assert_eq!(
            code(r.check(&packet(Kind::Afe, MIC, 0, 0, 1))),
            ProtocolErrorCode::UnexpectedKind
        );
        let afe = StreamOpen::parse(&open_body("afe", MIC, 5, 3)).unwrap();
        assert_eq!(r.open(&afe).unwrap_err().code, ProtocolErrorCode::BadStream);
        let mut granted = opened(true);
        assert_eq!(
            granted.check(&packet(Kind::Afe, MIC, 0, 1280, 2)),
            Ok(StreamId::Afe)
        );
    }

    #[test]
    fn the_sequence_must_increase_within_each_epoch() {
        let mut r = opened(false);
        r.check(&mic(0, 12_800)).unwrap();
        assert_eq!(code(r.check(&mic(0, 14_080))), ProtocolErrorCode::Sequence);
        r.check(&mic(1, 14_080)).unwrap();
        r.check(&mic(5, 15_360)).unwrap();
        assert_eq!(code(r.check(&mic(4, 16_640))), ProtocolErrorCode::Sequence);
        assert_eq!(
            r.check(&packet(Kind::Cells, MIC, 0, 0, 3)),
            Ok(StreamId::Cells),
            "each stream counts alone"
        );
    }

    #[test]
    fn going_back_without_a_grant_is_a_sample_order_error() {
        let mut r = opened(false);
        r.check(&mic(0, 12_800)).unwrap();
        r.check(&mic(1, 14_080)).unwrap();
        assert_eq!(code(r.check(&mic(2, 0))), ProtocolErrorCode::SampleOrder);
        r.check(&mic(2, 15_360)).unwrap();
    }

    #[test]
    fn a_first_subscription_backfill_is_simply_the_epoch_start() {
        let mut r = opened(false);
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(MIC));
        for (seq, first) in [(0, 2560), (1, 3840), (2, 5120), (3, 6400)] {
            r.check(&mic(seq, first)).unwrap();
        }
        // The grant was not needed, so one later run may still go back.
        r.check(&mic(4, 0)).unwrap();
        r.check(&mic(5, 1280)).unwrap();
        assert_eq!(code(r.check(&mic(6, 0))), ProtocolErrorCode::SampleOrder);
    }

    #[test]
    fn a_second_subscription_runs_its_backfill_then_live_resumes() {
        let mut r = opened(false);
        r.check(&mic(0, 25_600)).unwrap();
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(MIC));
        for (seq, first) in [
            (1, 12_800),
            (2, 14_080),
            (3, 15_360),
            (4, 24_320),
            (5, 25_600),
            (6, 26_880),
        ] {
            assert_eq!(r.check(&mic(seq, first)), Ok(StreamId::Mic), "packet {seq}");
        }
        assert_eq!(
            code(r.check(&mic(7, 1280))),
            ProtocolErrorCode::SampleOrder,
            "a second run nobody granted"
        );
    }

    #[test]
    fn a_backfill_run_may_interleave_with_live_packets_and_skip_missing_audio() {
        let mut r = opened(false);
        r.check(&mic(0, 25_600)).unwrap();
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(MIC));
        r.check(&mic(1, 12_800)).unwrap();
        r.check(&mic(2, 26_880)).unwrap();
        r.check(&mic(3, 14_080)).unwrap();
        r.check(&mic(4, 17_920)).unwrap();
        assert_eq!(
            code(r.check(&mic(5, 16_640))),
            ProtocolErrorCode::SampleOrder,
            "a run moves forward only"
        );
    }

    #[test]
    fn queued_runs_of_two_subscriptions_each_need_their_grant() {
        let mut r = opened(false);
        r.check(&mic(0, 25_600)).unwrap();
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(MIC));
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(MIC));
        for (seq, first) in [
            (1, 12_800),
            (2, 14_080),
            (3, 1280),
            (4, 2560),
            (5, 26_880),
            (6, 3840),
        ] {
            assert_eq!(r.check(&mic(seq, first)), Ok(StreamId::Mic), "packet {seq}");
        }
        assert_eq!(
            code(r.check(&mic(7, 0))),
            ProtocolErrorCode::SampleOrder,
            "a third run nobody granted"
        );
    }

    #[test]
    fn grants_cover_the_named_capture_epoch_or_every_open_reference_epoch() {
        let mut r = opened(false);
        let other = 444;
        r.open(&StreamOpen::parse(&open_body("reference", other, 2, 1)).unwrap())
            .unwrap();
        r.check(&packet(Kind::Reference, REF, 0, 25_600, 1280))
            .unwrap();
        r.check(&packet(Kind::Reference, other, 0, 25_600, 1280))
            .unwrap();
        r.check(&mic(0, 25_600)).unwrap();
        r.grant_backfill(StreamId::Mic, BackfillGrant::Epoch(999));
        assert_eq!(code(r.check(&mic(1, 0))), ProtocolErrorCode::SampleOrder);
        r.grant_backfill(StreamId::Reference, BackfillGrant::EveryOpenEpoch);
        assert!(r.check(&packet(Kind::Reference, REF, 1, 0, 1280)).is_ok());
        assert!(r.check(&packet(Kind::Reference, other, 1, 0, 1280)).is_ok());
        assert_eq!(
            code(r.check(&mic(1, 0))),
            ProtocolErrorCode::SampleOrder,
            "a reference grant is not a mic grant"
        );
    }

    #[test]
    fn cell_and_afe_runs_are_measured_in_capture_samples() {
        let mut r = opened(true);
        r.check(&packet(Kind::Cells, MIC, 0, 51_200, 10)).unwrap();
        r.grant_backfill(StreamId::Cells, BackfillGrant::Epoch(MIC));
        r.check(&packet(Kind::Cells, MIC, 1, 0, 10)).unwrap();
        r.check(&packet(Kind::Cells, MIC, 2, 5120, 10)).unwrap();
        assert_eq!(
            code(r.check(&packet(Kind::Cells, MIC, 3, 4608, 1))),
            ProtocolErrorCode::SampleOrder
        );
        r.check(&packet(Kind::Afe, MIC, 0, 12_800, 1)).unwrap();
        r.grant_backfill(StreamId::Afe, BackfillGrant::Epoch(MIC));
        r.check(&packet(Kind::Afe, MIC, 1, 1280, 2)).unwrap();
        r.check(&packet(Kind::Afe, MIC, 2, 3840, 1)).unwrap();
        assert_eq!(
            code(r.check(&packet(Kind::Afe, MIC, 3, 2560, 1))),
            ProtocolErrorCode::SampleOrder
        );
    }

    #[test]
    fn a_rejected_packet_leaves_the_epoch_unchanged() {
        let mut r = opened(false);
        r.check(&mic(3, 12_800)).unwrap();
        assert_eq!(code(r.check(&mic(4, 0))), ProtocolErrorCode::SampleOrder);
        r.check(&mic(4, 14_080)).unwrap();
    }

    #[test]
    fn session_loss_forgets_every_epoch() {
        let mut r = opened(false);
        r.check(&mic(0, 0)).unwrap();
        r.clear();
        assert_eq!(
            code(r.check(&mic(1, 1280))),
            ProtocolErrorCode::UnknownEpoch
        );
        assert_eq!(r.current(StreamId::Mic), None);
    }

    #[test]
    fn only_the_latest_ended_epochs_are_remembered() {
        let mut r = StreamRegistry::new(false);
        for epoch in 1..=(ENDED_EPOCHS_REMEMBERED as u64 + 1) {
            r.open(&StreamOpen::parse(&open_body("reference", epoch, 2, 1)).unwrap())
                .unwrap();
            r.end(&StreamEnd::parse(&end_body("reference", epoch, 0)).unwrap())
                .unwrap();
        }
        let oldest = packet(Kind::Reference, 1, 0, 0, 1280);
        let latest = packet(
            Kind::Reference,
            ENDED_EPOCHS_REMEMBERED as u64 + 1,
            0,
            0,
            1280,
        );
        assert_eq!(code(r.check(&oldest)), ProtocolErrorCode::UnknownEpoch);
        assert_eq!(code(r.check(&latest)), ProtocolErrorCode::StreamEnded);
    }

    #[test]
    fn uplink_ended_reports_null_for_a_stream_that_sent_nothing() {
        let body = object(json!({"lease_id": "L1", "reason": "ttl",
            "last_sample": {"mic": null, "cells": "40959"}, "clipped_start": {"mic": "4096", "cells": null}}));
        let ended = UplinkEnded::parse(&body).unwrap();
        assert_eq!(ended.reason, UplinkEndedReason::Ttl);
        assert_eq!(
            ended.last_sample,
            BTreeMap::from([(StreamId::Mic, None), (StreamId::Cells, Some(40_959))])
        );
        assert_eq!(
            ended.clipped_start,
            BTreeMap::from([(StreamId::Mic, Some(4096)), (StreamId::Cells, None)])
        );
        for bad in [
            json!({"lease_id": "L1", "reason": "bored", "last_sample": {}, "clipped_start": {}}),
            json!({"lease_id": "", "reason": "ttl", "last_sample": {}, "clipped_start": {}}),
            json!({"lease_id": "L1", "reason": "ttl", "last_sample": {"video": null}, "clipped_start": {}}),
            json!({"lease_id": "L1", "reason": "ttl", "last_sample": {"mic": 5}, "clipped_start": {}}),
            json!({"lease_id": "L1", "reason": "ttl", "clipped_start": {}}),
        ] {
            assert_eq!(
                UplinkEnded::parse(&object(bad.clone())).unwrap_err().code,
                ProtocolErrorCode::MalformedMessage,
                "{bad}"
            );
        }
    }

    #[test]
    fn digital_silence_packets_advance_like_pcm() {
        let mut r = opened(false);
        let header = Header {
            kind: Kind::Reference,
            flags: Flags::DIGITAL_SILENCE,
            epoch: REF,
            sequence: 0,
            first_sample: 0,
            mono_ns: 0,
            uncertainty_us: None,
            frame_count: 1280,
            generation: 0,
            source_mask: SourceMask::NONE,
        };
        let silent = Packet::parse(ema::encode(&header, &[]).unwrap()).unwrap();
        r.check(&silent).unwrap();
        r.check(&packet(Kind::Reference, REF, 1, 1280, 1280))
            .unwrap();
        assert_eq!(
            code(r.check(&packet(Kind::Reference, REF, 2, 0, 1280))),
            ProtocolErrorCode::SampleOrder
        );
    }
}
