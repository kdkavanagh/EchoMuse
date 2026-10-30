//! The session's audio timeline: every stream's epochs and the order in which
//! the device sent their packets.
//!
//! Besides storage, each stream epoch keeps a record of how its packets
//! arrived, modelled on the firmware's sender (`device/internal/uplink`): live
//! packets only move forward, and a packet that goes back below the live
//! position is part of a backfill run. The device queues one run per
//! subscription per stream, in the order the subscriptions became active, and
//! sends them one after another before live audio resumes. When a new run
//! starts, the track hands it to the oldest subscription still waiting for
//! one; that is how a subscription learns where its backfill really began.

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroU64;

use echomuse_protocol::StreamId;
use echomuse_protocol::ema::{AfeRecord, Cell};

use crate::buffer::{EpochBuffer, Segment, SegmentKind, Span};
use crate::config::UplinkConfig;

/// An internal subscription identity, unique within the session even when a
/// device reuses a subscription ID.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SubKey(pub(crate) u64);

/// A subscription waiting for its backfill run on one stream epoch.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Claim {
    pub(crate) key: SubKey,
    /// No other subscription wanted the stream when it was granted, so the
    /// device restarts the stream for it: its run, or live audio when it has
    /// none, is the first packet that follows.
    pub(crate) fresh: bool,
    /// The run cannot start before this sample; `None` when the device maps
    /// the start into the stream's domain itself (the reference).
    pub(crate) floor: Option<u64>,
}

/// How the device came to send a packet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Path {
    /// At or past the live position.
    Advance,
    /// Inside a backfill run, owned by the subscription the run was handed to.
    Run(Option<SubKey>),
}

/// What a run start tells the subscription it was handed to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Attribution {
    /// Its run began at this sample.
    Run { start: u64 },
    /// A later subscription's run began below this one's start, so this one
    /// got no run: its audio comes live.
    NoRun,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    next: u64,
    owner: Option<SubKey>,
}

/// One packet's place in its stream epoch.
#[derive(Debug)]
pub(crate) struct Classified {
    pub(crate) path: Path,
    /// A range the device skipped and flagged with the discontinuity flag.
    pub(crate) missing: Option<Span>,
    pub(crate) attributed: Vec<(SubKey, Attribution)>,
}

/// The arrival order of one stream epoch's packets.
#[derive(Debug, Default)]
pub(crate) struct Track {
    live_end: Option<u64>,
    runs: Vec<Run>,
    claims: VecDeque<Claim>,
}

impl Track {
    /// The live position: one past the furthest packet that moved forward.
    pub(crate) fn live_end(&self) -> Option<u64> {
        self.live_end
    }

    pub(crate) fn claim(&mut self, claim: Claim) {
        self.claims.push_back(claim);
    }

    pub(crate) fn drop_claims(&mut self, key: SubKey) {
        self.claims.retain(|c| c.key != key);
    }

    /// Places a packet of samples `[first, end)`, `discontinuity` its flag.
    pub(crate) fn classify(&mut self, first: u64, end: u64, discontinuity: bool) -> Classified {
        let mut attributed = Vec::new();
        match self.live_end {
            Some(live) if first < live => {
                let continuing = self
                    .runs
                    .iter_mut()
                    .filter(|r| r.next <= first)
                    .max_by_key(|r| r.next);
                // A run skips forward only over missing audio, and then flags
                // its next packet.
                if let Some(run) = continuing.filter(|r| first == r.next || discontinuity) {
                    let missing = (first > run.next).then_some(Span {
                        start: run.next,
                        end: first,
                    });
                    run.next = end;
                    return Classified {
                        path: Path::Run(run.owner),
                        missing,
                        attributed,
                    };
                }
                let owner = self.hand_over(first, false, &mut attributed);
                self.runs.push(Run { next: end, owner });
                Classified {
                    path: Path::Run(owner),
                    missing: None,
                    attributed,
                }
            }
            previous => {
                let missing = match previous {
                    Some(live) if discontinuity && first > live => Some(Span {
                        start: live,
                        end: first,
                    }),
                    _ => None,
                };
                self.runs.clear();
                self.live_end = Some(end);
                self.hand_over(first, true, &mut attributed);
                Classified {
                    path: Path::Advance,
                    missing,
                    attributed,
                }
            }
        }
    }

    /// Hands a run starting at `first` to the oldest waiting claim; with
    /// `fresh_only`, only to a fresh one.
    fn hand_over(
        &mut self,
        first: u64,
        fresh_only: bool,
        out: &mut Vec<(SubKey, Attribution)>,
    ) -> Option<SubKey> {
        while let Some(front) = self.claims.front().copied() {
            if fresh_only && !front.fresh {
                return None;
            }
            match front.floor {
                Some(floor) if first < floor => {
                    if fresh_only {
                        // Not its run: a run never starts before its start.
                        return None;
                    }
                    self.claims.pop_front();
                    out.push((front.key, Attribution::NoRun));
                }
                _ => {
                    self.claims.pop_front();
                    out.push((front.key, Attribution::Run { start: first }));
                    return Some(front.key);
                }
            }
        }
        None
    }
}

/// One epoch of one stream.
#[derive(Debug)]
pub(crate) struct EpochStream {
    pub(crate) epoch: NonZeroU64,
    pub(crate) buffer: EpochBuffer,
    pub(crate) track: Track,
}

#[derive(Debug)]
struct StreamStore {
    epochs: VecDeque<EpochStream>,
}

/// Why a read failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    /// The timeline holds no such stream epoch: never opened, dropped for a
    /// newer one, or erased by a mute.
    UnknownEpoch,
    /// The read asked for PCM from cells or AFE records, or the reverse.
    WrongStream,
    /// The range does not start and end on the stream's record grid.
    Misaligned,
    /// Part of the range is not held; this is its first piece.
    NotHeld(Segment),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::UnknownEpoch => f.write_str("no such stream epoch is held"),
            ReadError::WrongStream => f.write_str("the stream does not carry that kind of record"),
            ReadError::Misaligned => f.write_str("the range is not on the stream's record grid"),
            ReadError::NotHeld(segment) => write!(
                f,
                "samples {}..{} are not held",
                segment.span.start, segment.span.end
            ),
        }
    }
}

impl std::error::Error for ReadError {}

const STREAMS: [StreamId; 4] = [
    StreamId::Mic,
    StreamId::Cells,
    StreamId::Afe,
    StreamId::Reference,
];

fn slot(stream: StreamId) -> usize {
    match stream {
        StreamId::Mic => 0,
        StreamId::Cells => 1,
        StreamId::Afe => 2,
        StreamId::Reference => 3,
    }
}

/// Every stream's retained epochs. Reads never mix epochs: a range is always
/// read from one named epoch.
#[derive(Debug)]
pub struct Timeline {
    config: UplinkConfig,
    stores: [StreamStore; 4],
    capture: Option<NonZeroU64>,
    reference: Option<NonZeroU64>,
    /// Capture epochs a mute erased; their packets are refused.
    erased: HashSet<u64>,
}

impl Timeline {
    pub(crate) fn new(config: UplinkConfig) -> Self {
        Self {
            config,
            stores: std::array::from_fn(|_| StreamStore {
                epochs: VecDeque::new(),
            }),
            capture: None,
            reference: None,
            erased: HashSet::new(),
        }
    }

    /// The current capture epoch, shared by mic, cells and AFE records;
    /// `None` while muted or before the device announced one.
    pub fn capture_epoch(&self) -> Option<NonZeroU64> {
        self.capture
    }

    /// The current reference epoch.
    pub fn reference_epoch(&self) -> Option<NonZeroU64> {
        self.reference
    }

    /// The epochs of `stream` held, oldest first.
    pub fn epochs(&self, stream: StreamId) -> impl Iterator<Item = NonZeroU64> + '_ {
        self.stores[slot(stream)].epochs.iter().map(|e| e.epoch)
    }

    /// One past the highest position known in an epoch, in stream samples.
    pub fn end(&self, stream: StreamId, epoch: NonZeroU64) -> Option<u64> {
        self.get(stream, epoch.get()).map(|e| e.buffer.end())
    }

    /// The ordered pieces of `span` of one stream epoch: held data with its
    /// metadata, ranges the device reported missing, positions older than the
    /// buffer keeps, and positions not known.
    pub fn segments(
        &self,
        stream: StreamId,
        epoch: NonZeroU64,
        span: Span,
    ) -> Result<Vec<Segment>, ReadError> {
        let held = self
            .get(stream, epoch.get())
            .ok_or(ReadError::UnknownEpoch)?;
        check_grid(stream, span)?;
        Ok(held.buffer.segments(span))
    }

    /// Appends the PCM of `span` to `out`, if every sample is held. The
    /// reference's digital silence reads as zeros.
    pub fn read_pcm(
        &self,
        stream: StreamId,
        epoch: NonZeroU64,
        span: Span,
        out: &mut Vec<i16>,
    ) -> Result<(), ReadError> {
        if !matches!(stream, StreamId::Mic | StreamId::Reference) {
            return Err(ReadError::WrongStream);
        }
        let buffer = self.held(stream, epoch, span)?;
        buffer.copy_pcm(span, out);
        Ok(())
    }

    /// Appends the cells of `span`, in capture samples, to `out`, if every
    /// cell is held.
    pub fn read_cells(
        &self,
        epoch: NonZeroU64,
        span: Span,
        out: &mut Vec<Cell>,
    ) -> Result<(), ReadError> {
        let buffer = self.held(StreamId::Cells, epoch, span)?;
        buffer.copy_cells(span, out);
        Ok(())
    }

    /// Appends the AFE records of `span`, in capture samples, to `out`, if
    /// every record is held.
    pub fn read_afe(
        &self,
        epoch: NonZeroU64,
        span: Span,
        out: &mut Vec<AfeRecord>,
    ) -> Result<(), ReadError> {
        let buffer = self.held(StreamId::Afe, epoch, span)?;
        buffer.copy_afe(span, out);
        Ok(())
    }

    fn held(
        &self,
        stream: StreamId,
        epoch: NonZeroU64,
        span: Span,
    ) -> Result<&EpochBuffer, ReadError> {
        let held = self
            .get(stream, epoch.get())
            .ok_or(ReadError::UnknownEpoch)?;
        check_grid(stream, span)?;
        if let Some(gap) = held
            .buffer
            .segments(span)
            .into_iter()
            .find(|s| !matches!(s.kind, SegmentKind::Data { .. }))
        {
            return Err(ReadError::NotHeld(gap));
        }
        Ok(&held.buffer)
    }

    pub(crate) fn get(&self, stream: StreamId, epoch: u64) -> Option<&EpochStream> {
        self.stores[slot(stream)]
            .epochs
            .iter()
            .find(|e| e.epoch.get() == epoch)
    }

    pub(crate) fn get_mut(&mut self, stream: StreamId, epoch: u64) -> Option<&mut EpochStream> {
        self.stores[slot(stream)]
            .epochs
            .iter_mut()
            .find(|e| e.epoch.get() == epoch)
    }

    pub(crate) fn is_erased(&self, stream: StreamId, epoch: u64) -> bool {
        stream != StreamId::Reference && self.erased.contains(&epoch)
    }

    /// The epoch's storage, created when first needed. Keeps at most the
    /// configured number of epochs per stream, dropping the oldest.
    pub(crate) fn ensure(&mut self, stream: StreamId, epoch: NonZeroU64) -> &mut EpochStream {
        let kept = match stream {
            StreamId::Reference => self.config.reference_epochs_kept,
            _ => self.config.capture_epochs_kept,
        }
        .max(1);
        let retention = self.config.retention.of(stream);
        let max_blocks = self.config.max_blocks;
        let store = &mut self.stores[slot(stream)];
        let at = match store.epochs.iter().position(|e| e.epoch == epoch) {
            Some(at) => at,
            None => {
                while store.epochs.len() >= kept {
                    store.epochs.pop_front();
                }
                store.epochs.push_back(EpochStream {
                    epoch,
                    buffer: EpochBuffer::new(stream, retention, max_blocks),
                    track: Track::default(),
                });
                store.epochs.len() - 1
            }
        };
        &mut store.epochs[at]
    }

    pub(crate) fn set_capture(&mut self, epoch: Option<NonZeroU64>) {
        self.capture = epoch;
    }

    pub(crate) fn set_reference(&mut self, epoch: Option<NonZeroU64>) {
        self.reference = epoch;
    }

    /// Drops a subscription's waiting claims everywhere.
    pub(crate) fn drop_claims(&mut self, key: SubKey) {
        for store in &mut self.stores {
            for epoch in &mut store.epochs {
                epoch.track.drop_claims(key);
            }
        }
    }

    /// The privacy mute: forgets every capture epoch and refuses its packets
    /// from now on, and empties the reference epochs while keeping them open,
    /// since the reference epoch does not change with the mute.
    pub(crate) fn erase(&mut self) {
        for stream in STREAMS {
            let store = &mut self.stores[slot(stream)];
            if stream == StreamId::Reference {
                let retention = self.config.retention.of(stream);
                for e in &mut store.epochs {
                    e.buffer = EpochBuffer::new(stream, retention, self.config.max_blocks);
                    e.track = Track::default();
                }
            } else {
                self.erased
                    .extend(store.epochs.drain(..).map(|e| e.epoch.get()));
            }
        }
        if let Some(epoch) = self.capture.take() {
            self.erased.insert(epoch.get());
        }
    }

    /// The session is over: forgets everything.
    pub(crate) fn clear(&mut self) {
        for store in &mut self.stores {
            store.epochs.clear();
        }
        self.capture = None;
        self.reference = None;
    }
}

fn check_grid(stream: StreamId, span: Span) -> Result<(), ReadError> {
    let unit = stream.kind().samples_per_frame();
    if !span.start.is_multiple_of(unit) || !span.end.is_multiple_of(unit) {
        return Err(ReadError::Misaligned);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(key: u64, fresh: bool, floor: Option<u64>) -> Claim {
        Claim {
            key: SubKey(key),
            fresh,
            floor,
        }
    }

    #[test]
    fn runs_go_to_waiting_claims_in_order() {
        let mut t = Track::default();
        assert_eq!(t.classify(10_000, 11_280, false).path, Path::Advance);
        t.claim(claim(1, false, Some(5_000)));
        t.claim(claim(2, false, Some(2_000)));
        // Live audio sent before the device saw either subscription.
        let c = t.classify(11_280, 12_560, false);
        assert_eq!((c.path, c.attributed.len()), (Path::Advance, 0));
        // The first run, clipped to 6,000: subscription 1's.
        let c = t.classify(6_000, 7_280, false);
        assert_eq!(c.path, Path::Run(Some(SubKey(1))));
        assert_eq!(
            c.attributed,
            vec![(SubKey(1), Attribution::Run { start: 6_000 })]
        );
        let c = t.classify(7_280, 8_560, false);
        assert_eq!(c.path, Path::Run(Some(SubKey(1))));
        // The device skipped 8,560..9,000 inside the run.
        let c = t.classify(9_000, 10_000, true);
        assert_eq!(
            c.missing,
            Some(Span {
                start: 8_560,
                end: 9_000
            })
        );
        // Subscription 2's run goes back again.
        let c = t.classify(2_000, 3_280, false);
        assert_eq!(c.path, Path::Run(Some(SubKey(2))));
        // Live resumes.
        let c = t.classify(12_560, 13_840, false);
        assert_eq!(c.path, Path::Advance);
        // A live skip is flagged too.
        let c = t.classify(15_000, 15_100, true);
        assert_eq!(
            c.missing,
            Some(Span {
                start: 13_840,
                end: 15_000
            })
        );
    }

    #[test]
    fn a_fresh_claim_takes_the_first_packet_and_a_run_below_a_start_means_no_run() {
        let mut t = Track::default();
        t.classify(0, 1_280, false);
        t.claim(claim(1, true, Some(30_000)));
        // Below its start: not its run.
        assert!(t.classify(1_280, 2_560, false).attributed.is_empty());
        let c = t.classify(30_720, 32_000, false);
        assert_eq!(
            c.attributed,
            vec![(SubKey(1), Attribution::Run { start: 30_720 })]
        );

        t.claim(claim(2, false, Some(20_000)));
        t.claim(claim(3, false, None));
        // A run starting at 10,000 cannot be subscription 2's.
        let c = t.classify(10_000, 11_280, false);
        assert_eq!(
            c.attributed,
            vec![
                (SubKey(2), Attribution::NoRun),
                (SubKey(3), Attribution::Run { start: 10_000 })
            ]
        );
    }
}
