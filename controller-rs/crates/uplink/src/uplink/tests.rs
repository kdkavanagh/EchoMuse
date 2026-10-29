//! Subscriptions driven with synthetic packets. Every packet first passes the
//! protocol crate's stream registry, with the grants the uplink returned, so
//! the sequences here are ones a device may legally send.

use std::collections::HashMap;

use echomuse_protocol::ema::{Flags, Packet, SourceMask};

use crate::buffer::SegmentKind;
use crate::timeline::ReadError;
use echomuse_protocol::streams::StreamRegistry;
use serde_json::{Map, Value, json};

use super::*;
use crate::chunk::ChunkData;
use crate::testutil::{afe, cells, mic_ramp, reference};

const MIC: u64 = 111;
const REF: u64 = 222;

fn nz(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}

fn t(ms: u64) -> MonoInstant {
    MonoInstant(ms * 1_000_000)
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => panic!("object"),
    }
}

fn owner(id: &str) -> Owner {
    Owner::new(id).unwrap()
}

fn sub_id(id: &str) -> SubscriptionId {
    SubscriptionId::from_device(id).unwrap()
}

/// What one delivery says, in brief: its span and `None` for data or the
/// cause of a missing range.
type Piece = (u64, u64, Option<MissingCause>);

struct Rig {
    up: Uplink,
    registry: StreamRegistry,
    seq: HashMap<(StreamId, u64), u64>,
    out: Vec<Delivery>,
}

impl Rig {
    fn new(terms: SessionTerms) -> Self {
        Self::with_config(terms, UplinkConfig::default())
    }

    fn with_config(terms: SessionTerms, config: UplinkConfig) -> Self {
        let mut rig = Self {
            up: Uplink::new(config, terms, false),
            registry: StreamRegistry::new(terms.afe_metadata),
            seq: HashMap::new(),
            out: Vec::new(),
        };
        rig.open_capture(MIC);
        rig.open(StreamId::Reference, REF);
        rig
    }

    fn open_capture(&mut self, epoch: u64) {
        self.open(StreamId::Mic, epoch);
        self.open(StreamId::Cells, epoch);
        if self.up.terms().afe_metadata {
            self.open(StreamId::Afe, epoch);
        }
    }

    fn open(&mut self, stream: StreamId, epoch: u64) {
        let kind = stream.kind();
        let body = object(json!({
            "stream_id": stream.as_str(), "epoch": epoch.to_string(), "kind": kind as u8,
            "sample_rate": 16_000, "format": kind.format() as u8, "reason": "start"
        }));
        let open = StreamOpen::parse(&body).unwrap();
        self.registry.open(&open).unwrap();
        self.up.stream_opened(&open);
    }

    fn end(&mut self, stream: StreamId, epoch: u64, final_sample: u64, now: MonoInstant) {
        let body = object(json!({
            "stream_id": stream.as_str(), "epoch": epoch.to_string(),
            "final_sample": final_sample.to_string(), "reason": "render_epoch"
        }));
        let end = StreamEnd::parse(&body).unwrap();
        self.registry.end(&end).unwrap();
        self.up.stream_ended(&end, now);
    }

    fn grant(&mut self, grants: &[Grant]) {
        for g in grants {
            self.registry.grant_backfill(g.stream, g.grant);
        }
    }

    fn seq(&mut self, stream: StreamId, epoch: u64) -> u64 {
        let next = self.seq.entry((stream, epoch)).or_insert(0);
        *next += 1;
        *next - 1
    }

    fn feed(&mut self, packet: Packet, now: MonoInstant) -> Ingested {
        self.registry.check(&packet).expect("a legal packet");
        self.up.ingest(&packet, now)
    }

    fn mic(
        &mut self,
        epoch: u64,
        first: u64,
        end: u64,
        flags: Flags,
        now: MonoInstant,
    ) -> Ingested {
        let seq = self.seq(StreamId::Mic, epoch);
        self.feed(mic_ramp(epoch, seq, first, end, flags), now)
    }

    /// Mic from `first` to `end` in packets of at most 1,280 samples.
    fn mic_run(&mut self, epoch: u64, first: u64, end: u64, now: MonoInstant) -> Vec<Ingested> {
        let mut out = Vec::new();
        let mut at = first;
        while at < end {
            let next = (at + 1_280).min(end);
            out.push(self.mic(epoch, at, next, Flags::NONE, now));
            at = next;
        }
        out
    }

    fn drain(&mut self) -> Vec<Delivery> {
        self.up.drain(&mut self.out);
        std::mem::take(&mut self.out)
    }
}

fn pieces(deliveries: &[Delivery], id: &SubscriptionId, stream: StreamId) -> Vec<Piece> {
    deliveries
        .iter()
        .filter_map(|d| match d {
            Delivery::Chunk(c) if &c.subscription == id && c.stream == stream => Some(c),
            _ => None,
        })
        .map(|c| match &c.body {
            ChunkBody::Data { .. } => (c.span.start, c.span.end, None),
            ChunkBody::Missing(cause) => (c.span.start, c.span.end, Some(*cause)),
        })
        .collect()
}

/// The pieces cover `[start, end)` in order with no overlap and no hole.
fn assert_tiles(pieces: &[Piece], start: u64, end: u64) {
    let mut at = start;
    for &(a, b, _) in pieces {
        assert_eq!(a, at, "pieces {pieces:?}");
        assert!(b > a);
        at = b;
    }
    assert_eq!(at, end, "pieces {pieces:?}");
}

fn ended<'a>(deliveries: &'a [Delivery], id: &SubscriptionId) -> Option<&'a Ended> {
    deliveries.iter().find_map(|d| match d {
        Delivery::Ended(e) if &e.subscription == id => Some(e),
        _ => None,
    })
}

fn candidate(id: &str, epoch: u64, support_start: u64) -> CandidateOpen {
    CandidateOpen {
        subscription_id: sub_id(id),
        capture_epoch: nz(epoch),
        support_start: Sample(support_start),
    }
}

fn plain() -> SessionTerms {
    SessionTerms::default()
}

#[test]
fn candidate_grants_cover_its_capture_epoch_and_every_reference_epoch() {
    let mut rig = Rig::new(SessionTerms {
        afe_metadata: true,
        near_miss: false,
    });
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 100_000), t(0))
        .unwrap();
    assert_eq!(
        grants,
        vec![
            Grant {
                stream: StreamId::Mic,
                grant: BackfillGrant::Epoch(MIC)
            },
            Grant {
                stream: StreamId::Reference,
                grant: BackfillGrant::EveryOpenEpoch
            },
            Grant {
                stream: StreamId::Cells,
                grant: BackfillGrant::Epoch(MIC)
            },
            Grant {
                stream: StreamId::Afe,
                grant: BackfillGrant::Epoch(MIC)
            },
        ]
    );
    // The device opened it: the controller sends nothing until it renews.
    assert!(rig.up.take_commands().is_empty());
    let view = rig.up.subscription(&sub_id("c1")).unwrap();
    assert_eq!(view.reason, SubscriptionReason::Candidate);
    assert_eq!(view.generation, Generation::FIRST);
    assert_eq!(view.owner.as_str(), "c1");
    // A stale epoch, a duplicate and a mute are refused.
    assert_eq!(
        rig.up.accept_candidate(candidate("c2", 999, 100_000), t(0)),
        Err(OpenError::StaleEpoch {
            current: Some(nz(MIC)),
            requested: nz(999)
        })
    );
    assert_eq!(
        rig.up.accept_candidate(candidate("c1", MIC, 100_000), t(0)),
        Err(OpenError::DuplicateId)
    );
}

#[test]
fn backfill_queued_behind_live_audio_is_delivered_in_order_then_live() {
    let mut rig = Rig::new(plain());
    let diag = rig.up.open_diagnostic(owner("rec"), t(0)).unwrap();
    assert!(diag.grants.is_empty());
    rig.mic_run(MIC, 58_880, 62_720, t(10));

    // support_start 59,584: mic from round_down(54,784, 512) = 54,784.
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 59_584), t(20))
        .unwrap();
    rig.grant(&grants);
    // Live audio the device sent before it saw the acknowledgement.
    rig.mic(MIC, 62_720, 64_000, Flags::NONE, t(30));
    let early = rig.drain();
    assert!(pieces(&early, &sub_id("c1"), StreamId::Mic).is_empty());

    // The candidate's run from its start up to the live position, then live.
    let run = rig.mic_run(MIC, 54_784, 64_000, t(40));
    assert_eq!(run[3], Ingested::Stored { new_samples: 256 });
    assert_eq!(run[4], Ingested::Stored { new_samples: 0 });
    rig.mic(MIC, 64_000, 65_280, Flags::NONE, t(50));
    let late = rig.drain();

    let all: Vec<Delivery> = early.into_iter().chain(late).collect();
    let cand = pieces(&all, &sub_id("c1"), StreamId::Mic);
    assert!(cand.iter().all(|p| p.2.is_none()));
    assert_tiles(&cand, 54_784, 65_280);
    let rec = pieces(&all, &diag.subscription_id, StreamId::Mic);
    assert!(rec.iter().all(|p| p.2.is_none()));
    assert_tiles(&rec, 58_880, 65_280);

    // The PCM is the device's, sample for sample.
    let mut samples = Vec::new();
    for d in &all {
        if let Delivery::Chunk(c) = d
            && c.subscription == sub_id("c1")
            && let ChunkBody::Data {
                data: ChunkData::Pcm(pcm),
                ..
            } = &c.body
        {
            samples.extend(pcm.samples());
        }
    }
    assert_eq!(samples, crate::testutil::ramp(54_784, 65_280 - 54_784));
    // And reads by range see the same.
    let mut read = Vec::new();
    rig.up
        .timeline()
        .read_pcm(
            StreamId::Mic,
            nz(MIC),
            Span::new(54_784, 65_280).unwrap(),
            &mut read,
        )
        .unwrap();
    assert_eq!(read, samples);
}

#[test]
fn a_clipped_backfill_start_is_reported_missing_before_the_audio() {
    let mut rig = Rig::new(plain());
    // support_start 100,000: mic from round_down(95,200, 512) = 94,720. The device's history only
    // reaches back to 95,744, so its run starts there.
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 100_000), t(0))
        .unwrap();
    rig.grant(&grants);
    rig.mic_run(MIC, 95_744, 98_304, t(10));
    let out = rig.drain();
    assert_eq!(
        pieces(&out, &sub_id("c1"), StreamId::Mic),
        vec![
            (94_720, 95_744, Some(MissingCause::Clipped)),
            (95_744, 97_024, None),
            (97_024, 98_304, None)
        ]
    );
}

#[test]
fn device_skips_timeouts_and_overwritten_history_become_missing_ranges() {
    let mut rig = Rig::new(plain());
    let rec = rig.up.open_diagnostic(owner("rec"), t(0)).unwrap();
    rig.mic(MIC, 51_200, 52_480, Flags::NONE, t(10));
    // A skip the device flagged.
    rig.mic(MIC, 53_760, 55_040, Flags::DISCONTINUITY, t(20));
    let out = rig.drain();
    assert_eq!(
        pieces(&out, &rec.subscription_id, StreamId::Mic),
        vec![
            (51_200, 52_480, None),
            (52_480, 53_760, Some(MissingCause::Device)),
            (53_760, 55_040, None)
        ]
    );

    // A candidate whose run never comes: later audio waits behind its start
    // for the hole wait, then the hole is declared missing.
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 50_000), t(100))
        .unwrap();
    rig.grant(&grants);
    rig.mic(MIC, 55_040, 56_320, Flags::NONE, t(100));
    assert!(pieces(&rig.drain(), &sub_id("c1"), StreamId::Mic).is_empty());
    rig.up.poll(t(2_099));
    assert!(pieces(&rig.drain(), &sub_id("c1"), StreamId::Mic).is_empty());
    assert!(rig.up.next_deadline().unwrap() <= t(2_100));
    rig.up.poll(t(2_100));
    let out = rig.drain();
    let cand = pieces(&out, &sub_id("c1"), StreamId::Mic);
    // round_down(50,000 − 4,800, 512) = 45,056; the controller holds the
    // recording's audio from 51,200.
    assert_eq!(cand[0], (45_056, 51_200, Some(MissingCause::TimedOut)));
    assert_tiles(&cand, 45_056, 56_320);
}

#[test]
fn audio_older_than_the_device_history_is_declared_overwritten() {
    let mut rig = Rig::new(plain());
    rig.up.open_diagnostic(owner("rec"), t(0)).unwrap();
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 0), t(0))
        .unwrap();
    rig.grant(&grants);
    // The live position is 98,048: the device's 6 s ring starts at 2,048.
    rig.mic(MIC, 96_768, 98_048, Flags::NONE, t(10));
    let out = rig.drain();
    assert_eq!(
        pieces(&out, &sub_id("c1"), StreamId::Mic),
        vec![(0, 2_048, Some(MissingCause::Overwritten))]
    );
}

#[test]
fn a_new_capture_epoch_ends_every_subscription() {
    let mut rig = Rig::new(plain());
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 10_000), t(0))
        .unwrap();
    rig.grant(&grants);
    rig.mic(MIC, 5_120, 6_400, Flags::NONE, t(10));
    rig.drain();
    rig.open_capture(333);
    let out = rig.drain();
    let end = ended(&out, &sub_id("c1")).unwrap();
    assert_eq!(end.cause, EndCause::CaptureEpoch);
    assert!(rig.up.subscriptions().is_empty());
    rig.up.poll(t(5_000));
    assert!(rig.up.take_commands().is_empty());
    // Audio of the old epoch still in flight is no one's.
    assert_eq!(
        rig.mic(MIC, 6_400, 7_680, Flags::NONE, t(20)),
        Ingested::Unwanted
    );
    // So is the end of the epoch itself.
    let grants = rig
        .up
        .accept_candidate(candidate("c2", 333, 10_000), t(30))
        .unwrap();
    rig.grant(&grants);
    rig.end(StreamId::Mic, 333, 9_000, t(40));
    assert_eq!(
        ended(&rig.drain(), &sub_id("c2")).unwrap().cause,
        EndCause::CaptureEpoch
    );
}

#[test]
fn renewals_run_every_second_and_conversion_bumps_the_generation() {
    let mut rig = Rig::new(plain());
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 10_000), t(0))
        .unwrap();
    rig.grant(&grants);
    assert_eq!(rig.up.next_deadline(), Some(t(1_000)));
    rig.up.poll(t(999));
    assert!(rig.up.take_commands().is_empty());
    rig.up.poll(t(1_000));
    let renew = rig.up.take_commands();
    assert_eq!(renew.len(), 1);
    assert_eq!(renew[0].generation, Generation::FIRST);
    let UplinkCommand::Renew(body) = &renew[0].command else {
        panic!("renew")
    };
    assert_eq!(
        serde_json::to_value(body).unwrap(),
        json!({"lease_id": "c1", "ttl_ms": 3000})
    );

    rig.up
        .convert_to_turn(&sub_id("c1"), owner("turn-1"), t(1_200))
        .unwrap();
    let convert = rig.up.take_commands();
    assert_eq!(convert[0].generation.get(), 2);
    let UplinkCommand::Renew(body) = &convert[0].command else {
        panic!("renew")
    };
    assert_eq!(
        serde_json::to_value(body).unwrap(),
        json!({"lease_id": "c1", "ttl_ms": 3000, "reason": "turn", "owner": "turn-1"})
    );
    let view = rig.up.subscription(&sub_id("c1")).unwrap();
    assert_eq!(
        (view.reason, view.owner.as_str(), view.generation.get()),
        (SubscriptionReason::Turn, "turn-1", 2)
    );
    assert_eq!(
        rig.up
            .convert_to_turn(&sub_id("c1"), owner("turn-2"), t(1_300)),
        Err(CommandError::NotACandidate)
    );
    // The next renewal is a second after the conversion, at generation 2.
    assert_eq!(rig.up.next_deadline(), Some(t(2_200)));
    rig.up.poll(t(2_200));
    let renew = rig.up.take_commands();
    assert_eq!(renew[0].generation.get(), 2);
    assert!(matches!(&renew[0].command, UplinkCommand::Renew(b) if b.conversion.is_none()));
    // Audio released after the conversion carries generation 2.
    rig.mic(MIC, 5_120, 6_400, Flags::NONE, t(2_300));
    let out = rig.drain();
    assert!(out.iter().all(|d| match d {
        Delivery::Chunk(c) => c.generation.get() == 2,
        Delivery::Ended(_) => false,
    }));
    // A rejected renewal of the old generation changes nothing.
    rig.up
        .command_rejected(&sub_id("c1"), Generation::FIRST, None, t(2_400));
    assert!(rig.up.subscription(&sub_id("c1")).is_some());
    // Closing goes out at generation 2 and ends it at once.
    rig.up
        .close(&sub_id("c1"), CloseReason::Committed, t(2_500))
        .unwrap();
    let close = rig.up.take_commands();
    assert_eq!(close[0].generation.get(), 2);
    assert!(matches!(&close[0].command,
        UplinkCommand::Close(b) if b.reason == CloseReason::Committed));
    let out = rig.drain();
    assert_eq!(
        ended(&out, &sub_id("c1")).unwrap().cause,
        EndCause::Closed(CloseReason::Committed)
    );
    assert_eq!(
        rig.up.close(&sub_id("c1"), CloseReason::Closed, t(2_600)),
        Err(CommandError::UnknownSubscription)
    );
}

#[test]
fn a_rejected_command_at_the_current_generation_ends_the_subscription() {
    let mut rig = Rig::new(plain());
    let opened = rig.up.open_diagnostic(owner("rec"), t(0)).unwrap();
    rig.up.take_commands();
    rig.up.command_rejected(
        &opened.subscription_id,
        Generation::FIRST,
        Some("muted".to_owned()),
        t(10),
    );
    assert_eq!(
        ended(&rig.drain(), &opened.subscription_id).unwrap().cause,
        EndCause::Rejected {
            code: Some("muted".to_owned())
        }
    );
}

#[test]
fn near_miss_subscriptions_need_the_capability_and_close_themselves() {
    let mut without = Rig::new(SessionTerms {
        afe_metadata: true,
        near_miss: false,
    });
    let near = NearMissOpen {
        near_miss_id: owner("nm-1"),
        capture_epoch: nz(MIC),
        support_start: Sample(40_000),
        support_end: Sample(48_000),
    };
    assert_eq!(
        without.up.open_near_miss(near.clone(), t(0)),
        Err(OpenError::NearMissUnsupported)
    );
    let streams = BTreeMap::from([(StreamId::Mic, StreamStart::Live)]);
    assert_eq!(
        without
            .up
            .open(OpenReason::NearMiss, owner("nm-1"), streams, t(0)),
        Err(OpenError::NearMissUnsupported)
    );
    assert!(without.up.take_commands().is_empty());

    let mut rig = Rig::new(SessionTerms {
        afe_metadata: true,
        near_miss: true,
    });
    let opened = rig.up.open_near_miss(near, t(0)).unwrap();
    rig.grant(&opened.grants);
    let open = rig.up.take_commands();
    assert_eq!(open[0].generation, Generation::FIRST);
    let UplinkCommand::Open(body) = &open[0].command else {
        panic!("open")
    };
    // mic from round_down(40,000 − 4,800, 512) = 34,816; AFE from 24,000.
    assert_eq!(
        serde_json::to_value(body).unwrap(),
        json!({"lease_id": opened.subscription_id.as_str(), "owner": "nm-1",
               "reason": "near_miss", "ttl_ms": 3000,
               "streams": {"mic": "34816", "afe": "24000"}})
    );
    // Mic through support_end + 4,800 = 52,800 is not enough without the
    // AFE records.
    rig.mic_run(MIC, 34_816, 53_760, t(10));
    assert!(rig.up.take_commands().is_empty());
    // The device rounds the AFE start down to 23,040.
    let seq = rig.seq(StreamId::Afe, MIC);
    rig.feed(afe(MIC, seq, 23_040, 24), t(20));
    let close = rig.up.take_commands();
    assert!(
        matches!(&close[..], [Outgoing { command: UplinkCommand::Close(b), .. }]
        if b.reason == CloseReason::Closed)
    );
    let out = rig.drain();
    let afe_pieces = pieces(&out, &opened.subscription_id, StreamId::Afe);
    assert_eq!(afe_pieces, vec![(23_040, 53_760, None)]);
    assert_eq!(
        ended(&out, &opened.subscription_id).unwrap().cause,
        EndCause::Closed(CloseReason::Closed)
    );
}

#[test]
fn mute_erases_buffers_ends_subscriptions_and_refuses_the_old_epoch() {
    let mut rig = Rig::new(SessionTerms {
        afe_metadata: true,
        near_miss: false,
    });
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 10_000), t(0))
        .unwrap();
    rig.grant(&grants);
    rig.mic_run(MIC, 5_120, 10_240, t(10));
    let seq = rig.seq(StreamId::Reference, REF);
    rig.feed(
        reference(REF, seq, 0, 1_280, false, SourceMask::DIALOG),
        t(10),
    );
    assert!(rig.up.backlog(&sub_id("c1")).unwrap().payload_bytes > 0);

    rig.up.mute();
    let out = rig.drain();
    assert_eq!(out.len(), 1, "only the end notice survives: {out:?}");
    assert_eq!(ended(&out, &sub_id("c1")).unwrap().cause, EndCause::Muted);
    assert_eq!(rig.up.backlog(&sub_id("c1")), None);
    assert!(rig.up.subscriptions().is_empty());
    let timeline = rig.up.timeline();
    assert_eq!(timeline.capture_epoch(), None);
    assert_eq!(timeline.epochs(StreamId::Mic).count(), 0);
    assert_eq!(timeline.epochs(StreamId::Afe).count(), 0);
    assert_eq!(
        timeline.segments(StreamId::Mic, nz(MIC), Span::new(5_120, 6_400).unwrap()),
        Err(ReadError::UnknownEpoch)
    );
    let reference = timeline
        .segments(StreamId::Reference, nz(REF), Span::new(0, 1_280).unwrap())
        .unwrap();
    assert!(reference.iter().all(|s| s.kind == SegmentKind::Unknown));

    // Audio of the muted epoch still queued behind the mute is refused.
    assert_eq!(
        rig.mic(MIC, 10_240, 11_520, Flags::NONE, t(20)),
        Ingested::ErasedEpoch
    );
    assert_eq!(
        rig.up.accept_candidate(candidate("c2", MIC, 10_000), t(20)),
        Err(OpenError::Muted)
    );
    assert_eq!(
        rig.up.open_diagnostic(owner("rec"), t(20)),
        Err(OpenError::Muted)
    );
    assert!(rig.up.take_commands().is_empty());

    // Unmuted, the device starts a new capture epoch; the old stays refused.
    rig.up.unmute();
    rig.open_capture(444);
    let grants = rig
        .up
        .accept_candidate(candidate("c3", 444, 10_000), t(30))
        .unwrap();
    rig.grant(&grants);
    assert_eq!(
        rig.mic(MIC, 11_520, 12_800, Flags::NONE, t(40)),
        Ingested::ErasedEpoch
    );
    assert!(matches!(
        rig.mic(444, 5_120, 6_400, Flags::NONE, t(40)),
        Ingested::Stored { .. }
    ));
}

#[test]
fn uplink_ended_delivers_what_was_in_flight_then_ends() {
    let mut rig = Rig::new(plain());
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 10_000), t(0))
        .unwrap();
    rig.grant(&grants);
    rig.mic(MIC, 5_120, 6_400, Flags::NONE, t(10));
    let report = UplinkEnded::parse(&object(json!({
        "lease_id": "c1", "reason": "ttl",
        "last_sample": {"mic": "7679", "cells": null, "reference": null},
        "clipped_start": {"mic": null, "cells": null, "reference": null}
    })))
    .unwrap();
    rig.up.device_ended(&report, t(500));
    assert!(ended(&rig.drain(), &sub_id("c1")).is_none());
    assert!(rig.up.subscription(&sub_id("c1")).unwrap().draining);
    // No renewals for it now.
    rig.up.poll(t(1_000));
    assert!(rig.up.take_commands().is_empty());
    rig.mic(MIC, 6_400, 7_680, Flags::NONE, t(600));
    let out = rig.drain();
    assert_eq!(
        pieces(&out, &sub_id("c1"), StreamId::Mic),
        vec![(6_400, 7_680, None)]
    );
    assert_eq!(
        ended(&out, &sub_id("c1")).unwrap().cause,
        EndCause::Device(report)
    );

    // When the rest never arrives, the drain runs out after the hole wait.
    let grants = rig
        .up
        .accept_candidate(candidate("c2", MIC, 20_000), t(700))
        .unwrap();
    rig.grant(&grants);
    rig.mic(MIC, 15_360, 16_640, Flags::NONE, t(710));
    rig.drain();
    let report = UplinkEnded::parse(&object(json!({
        "lease_id": "c2", "reason": "overrun",
        "last_sample": {"mic": "19199", "cells": null, "reference": null},
        "clipped_start": {"mic": null, "cells": null, "reference": null}
    })))
    .unwrap();
    rig.up.device_ended(&report, t(800));
    assert_eq!(rig.up.next_deadline(), Some(t(2_800)));
    rig.up.poll(t(2_800));
    let out = rig.drain();
    assert_eq!(
        pieces(&out, &sub_id("c2"), StreamId::Mic),
        vec![(16_640, 19_200, Some(MissingCause::TimedOut))]
    );
    assert!(matches!(
        ended(&out, &sub_id("c2")).unwrap().cause,
        EndCause::Device(_)
    ));
}

#[test]
fn reference_follows_its_epochs_and_cells_and_afe_records_arrive_typed() {
    let mut rig = Rig::new(SessionTerms {
        afe_metadata: true,
        near_miss: false,
    });
    let opened = rig
        .up
        .open_reply(owner("reply-1"), Sample(0), t(0))
        .unwrap();
    rig.grant(&opened.grants);
    let id = opened.subscription_id;
    let seq = rig.seq(StreamId::Reference, REF);
    rig.feed(
        reference(REF, seq, 2_560, 1_280, true, SourceMask::NONE),
        t(10),
    );
    let seq = rig.seq(StreamId::Reference, REF);
    rig.feed(
        reference(REF, seq, 3_840, 1_280, false, SourceMask::CONTENT),
        t(10),
    );
    // A new reference epoch: the reply's reference restarts live on it, and
    // the end of the old one declares what never came.
    rig.open(StreamId::Reference, 555);
    rig.end(StreamId::Reference, REF, 6_000, t(20));
    let seq = rig.seq(StreamId::Reference, 555);
    rig.feed(
        reference(555, seq, 0, 1_280, false, SourceMask::ALERT),
        t(30),
    );
    let seq = rig.seq(StreamId::Cells, MIC);
    rig.feed(cells(MIC, seq, 0, 4), t(30));
    let seq = rig.seq(StreamId::Afe, MIC);
    rig.feed(afe(MIC, seq, 0, 2), t(30));
    let out = rig.drain();

    let refs: Vec<(u64, Piece, u64)> = out
        .iter()
        .filter_map(|d| match d {
            Delivery::Chunk(c) if c.stream == StreamId::Reference => {
                let piece = match &c.body {
                    ChunkBody::Data { .. } => (c.span.start, c.span.end, None),
                    ChunkBody::Missing(m) => (c.span.start, c.span.end, Some(*m)),
                };
                Some((c.epoch.get(), piece, c.payload_bytes()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        refs,
        vec![
            (REF, (2_560, 3_840, None), 0),
            (REF, (3_840, 5_120, None), 2_560),
            (REF, (5_120, 6_000, Some(MissingCause::NotSent)), 0),
            (555, (0, 1_280, None), 2_560),
        ]
    );
    let typed: Vec<&ChunkData> = out
        .iter()
        .filter_map(|d| match d {
            Delivery::Chunk(Chunk {
                body: ChunkBody::Data { data, .. },
                stream: StreamId::Cells | StreamId::Afe,
                ..
            }) => Some(data),
            _ => None,
        })
        .collect();
    assert!(matches!(typed[0], ChunkData::Cells(c) if c.len() == 4));
    assert!(matches!(typed[1], ChunkData::Afe(a) if a.len() == 2));
    assert_eq!(pieces(&out, &id, StreamId::Cells), vec![(0, 2_048, None)]);
    let delivered = rig.up.delivered(&id).unwrap();
    assert_eq!(delivered.payload_bytes, 2 * 2_560 + 4 * 4 + 2 * 14);
    assert_eq!(rig.up.backlog(&id).unwrap(), Backlog::default());

    let mut cells_read = Vec::new();
    assert_eq!(
        rig.up
            .timeline()
            .read_cells(nz(MIC), Span::new(0, 1_000).unwrap(), &mut cells_read),
        Err(ReadError::Misaligned)
    );
    rig.up
        .timeline()
        .read_cells(nz(MIC), Span::new(512, 2_048).unwrap(), &mut cells_read)
        .unwrap();
    assert_eq!(cells_read.len(), 3);
}

#[test]
fn a_consumer_that_falls_behind_closes_its_subscription() {
    let config = UplinkConfig {
        max_backlog_bytes: 3_000,
        ..UplinkConfig::default()
    };
    let mut rig = Rig::with_config(plain(), config);
    let opened = rig.up.open_diagnostic(owner("rec"), t(0)).unwrap();
    rig.up.take_commands();
    rig.mic(MIC, 0, 1_280, Flags::NONE, t(10));
    assert_eq!(
        rig.up.backlog(&opened.subscription_id).unwrap(),
        Backlog {
            deliveries: 1,
            payload_bytes: 2_560
        }
    );
    rig.mic(MIC, 1_280, 2_560, Flags::NONE, t(20));
    let close = rig.up.take_commands();
    assert!(matches!(
        &close[..],
        [Outgoing {
            command: UplinkCommand::Close(_),
            ..
        }]
    ));
    let out = rig.drain();
    assert_eq!(
        ended(&out, &opened.subscription_id).unwrap().cause,
        EndCause::Overflow
    );
    assert_eq!(
        rig.mic(MIC, 2_560, 3_840, Flags::NONE, t(30)),
        Ingested::Unwanted
    );
}

#[test]
fn session_loss_ends_everything_and_keeps_nothing() {
    let mut rig = Rig::new(plain());
    let grants = rig
        .up
        .accept_candidate(candidate("c1", MIC, 10_000), t(0))
        .unwrap();
    rig.grant(&grants);
    rig.mic(MIC, 5_120, 6_400, Flags::NONE, t(10));
    rig.up.session_lost();
    let out = rig.drain();
    assert_eq!(out.len(), 1);
    assert_eq!(
        ended(&out, &sub_id("c1")).unwrap().cause,
        EndCause::SessionLost
    );
    assert_eq!(rig.up.timeline().epochs(StreamId::Mic).count(), 0);
    assert_eq!(rig.up.next_deadline(), None);
}
