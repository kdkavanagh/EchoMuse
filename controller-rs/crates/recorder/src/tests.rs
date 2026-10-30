//! The recorder against synthetic sessions: consent, privacy, bounds, files, the format and
//! decision replay. Each test drives the real speech policy and records it exactly as a
//! device runtime would.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use echomuse_core::services::HaError;
use echomuse_core::types::{DeviceId, RuntimeId, SessionId, TurnId};
use echomuse_echo::{EchoAnalyzer, EchoPlan, EchoPolicy, OutputCoverage};
use echomuse_protocol::LinkSessionId;
use echomuse_protocol::ema::{self, Flags, Kind, SourceMask};
use echomuse_turn::{
    ArrivalConditions, AsrKind, AsrObservation, CandidateEcho, CellIndex, Completeness, Decision,
    DeviceCell, DeviceInput, EchoCell, EchoLabel, Epoch, MonoInstant, RecognizerAnswer,
    ReferenceCoverage, ReferenceScore, Sample, SampleSpan, Samples, TextClass, Token, TurnConfig,
    TurnKind, TurnPolicy, TurnSpec, VadInput, VerifierResult, WakeCandidate, WakeDecider, WakeHop,
    WakeOutcome, WakePolicy, WakeProfile, WakeRefusal,
};
use serde_json::json;

use crate::reader::{Limitation, RecordedItem, Recording, Tail};
use crate::record::{
    EchoStep, EndReason, Event, HaBody, HaEntry, HaOperation, IngressOrdinal, Marker, PrivacyState,
    Revisions, TurnCause, TurnControl, WithheldReason,
};
use crate::redact::REDACTED;
use crate::replay::{EchoSource, Equivalence, Mismatch, ReplayOptions, Terminal, replay};
use crate::store::{RecorderConfig, RecordingStore};
use crate::writer::{FrameTaken, GrammarTap, RecorderState, SessionIdentity, SessionRecorder};
use crate::{
    AdministratorId, GrantBook, GrantError, GrantRequest, GrantStatus, QueueBounds, ReadError,
    StreamScope,
};

fn ms(n: u64) -> MonoInstant {
    MonoInstant(n * 1_000_000)
}

fn device() -> DeviceId {
    DeviceId("kitchen/dot 1".to_owned())
}

fn admin() -> AdministratorId {
    AdministratorId::new("kyle").unwrap()
}

fn session() -> SessionIdentity {
    SessionIdentity {
        device: device(),
        runtime: RuntimeId(7),
        session: SessionId(9),
        link_session: LinkSessionId::from("2c1b7c56-1f0e-4f4e-8f0a-1d2b3c4d5e6f".to_owned()),
    }
}

const OPEN: PrivacyState = PrivacyState {
    muted: false,
    capture_permitted: true,
};

fn store_with(bounds: QueueBounds) -> (tempfile::TempDir, RecordingStore) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = RecorderConfig::new(dir.path().join("session_recordings"));
    config.queue = bounds;
    (dir, RecordingStore::open(config).unwrap())
}

fn store() -> (tempfile::TempDir, RecordingStore) {
    store_with(QueueBounds::default())
}

fn grant(book: &GrantBook, streams: StreamScope, secs: u64, now: MonoInstant) {
    book.grant(
        &device(),
        GrantRequest {
            administrator: admin(),
            interval: Duration::from_secs(secs),
            streams,
        },
        now,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    )
    .unwrap();
}

fn start(
    book: &GrantBook,
    store: &RecordingStore,
    privacy: PrivacyState,
    now: MonoInstant,
) -> SessionRecorder {
    let pass = book.pass(&device(), now).expect("live grant");
    SessionRecorder::start(
        pass,
        store,
        session(),
        Revisions {
            controller: "test".to_owned(),
            ..Revisions::default()
        },
        privacy,
        now,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    )
    .unwrap()
}

fn read_only(store: &RecordingStore) -> Recording {
    let files = store.list().unwrap();
    assert_eq!(files.len(), 1, "one recording");
    Recording::read(&files[0].path).unwrap()
}

/// One EMA1 frame of `n` PCM samples from `first`.
fn pcm_frame(kind: Kind, epoch: u64, seq: u64, first: u64, samples: &[i16], flags: Flags) -> Bytes {
    let payload: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    ema::encode(
        &ema::Header {
            kind,
            flags,
            epoch,
            sequence: seq,
            first_sample: first,
            mono_ns: 1_000_000_000 + first * 62_500,
            uncertainty_us: Some(200),
            frame_count: samples.len() as u32,
            generation: 0,
            source_mask: if kind == Kind::Reference {
                SourceMask::CONTENT
            } else {
                SourceMask::NONE
            },
        },
        &payload,
    )
    .unwrap()
}

fn mic(seq: u64) -> Bytes {
    pcm_frame(Kind::Mic, 3, seq, seq * 1_280, &[100; 1_280], Flags::NONE)
}

fn frames_in(r: &Recording) -> Vec<u64> {
    r.frames()
        .map(|f| ema::decode_header(&f.frame).unwrap().sequence)
        .collect()
}

// --- Consent -------------------------------------------------------------------------------

#[test]
fn no_pass_exists_without_a_live_grant() {
    let book = GrantBook::new();
    assert!(book.pass(&device(), ms(0)).is_none());
    assert_eq!(book.status(&device(), ms(0)), GrantStatus::Off);
    let req = |secs| GrantRequest {
        administrator: admin(),
        interval: Duration::from_secs(secs),
        streams: StreamScope::ALL,
    };
    let wall = SystemTime::UNIX_EPOCH;
    assert_eq!(
        book.grant(&device(), req(0), ms(0), wall),
        Err(GrantError::EmptyInterval)
    );
    assert!(matches!(
        book.grant(&device(), req(9 * 3_600), ms(0), wall),
        Err(GrantError::TooLong { .. })
    ));
    book.grant(&device(), req(60), ms(0), wall).unwrap();
    assert!(matches!(
        book.grant(&device(), req(60), ms(1), wall),
        Err(GrantError::AlreadyActive { .. })
    ));
    assert!(book.pass(&device(), ms(59_999)).is_some());
    assert!(book.pass(&DeviceId("other".into()), ms(0)).is_none());
    // Expired.
    assert!(book.pass(&device(), ms(60_000)).is_none());
    assert!(matches!(
        book.status(&device(), ms(60_000)),
        GrantStatus::Expired { .. }
    ));
    // A new grant after expiry, then withdrawn.
    book.grant(&device(), req(60), ms(70_000), wall).unwrap();
    book.withdraw(&device(), admin(), ms(71_000), wall).unwrap();
    assert!(book.pass(&device(), ms(71_000)).is_none());
    assert!(matches!(
        book.status(&device(), ms(71_000)),
        GrantStatus::Withdrawn { .. }
    ));
    assert_eq!(
        book.withdraw(&device(), admin(), ms(72_000), wall),
        Err(GrantError::NotActive)
    );
}

#[test]
fn a_pass_for_another_device_is_refused() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let pass = book.pass(&device(), ms(0)).unwrap();
    let mut other = session();
    other.device = DeviceId("other".into());
    let r = SessionRecorder::start(
        pass,
        &store,
        other,
        Revisions::default(),
        OPEN,
        ms(0),
        SystemTime::now(),
    );
    assert!(matches!(r, Err(crate::StartError::WrongDevice)));
}

#[test]
fn recording_stops_at_the_grants_end() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 1, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    assert!(matches!(
        book.status(&device(), ms(10)),
        GrantStatus::Active {
            recording: true,
            ..
        }
    ));
    assert_eq!(
        rec.frame(IngressOrdinal(1), &mic(0), ms(500)),
        FrameTaken::Queued
    );
    assert_eq!(
        rec.frame(IngressOrdinal(2), &mic(1), ms(1_000)),
        FrameTaken::Ended
    );
    assert!(matches!(
        rec.state(),
        RecorderState::Ended {
            reason: EndReason::Expired,
            ..
        }
    ));
    rec.revisions(Revisions::default(), ms(1_100));
    let state = rec.finish(ms(1_200)).wait();
    assert_eq!(
        state,
        RecorderState::Ended {
            reason: EndReason::Expired,
            written: true
        }
    );
    let r = read_only(&store);
    assert_eq!(frames_in(&r), vec![0]);
    assert_eq!(r.events().count(), 0);
    let end = r.end.unwrap();
    assert_eq!((end.reason, end.t.0), (EndReason::Expired, 1_000_000_000));
    assert!(r.integrity.is_complete());
}

#[test]
fn an_idle_recorder_is_ended_by_the_book_at_expiry() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 1, ms(0));
    let rec = start(&book, &store, OPEN, ms(0));
    book.expire(ms(1_000));
    assert!(matches!(
        rec.state(),
        RecorderState::Ended {
            reason: EndReason::Expired,
            ..
        }
    ));
    rec.finish(ms(1_000)).wait();
    assert_eq!(read_only(&store).integrity.end, Some(EndReason::Expired));
}

#[test]
fn withdrawal_discards_unwritten_entries_and_ends_the_recording() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    rec.frame(IngressOrdinal(1), &mic(0), ms(10));
    // Let the writer save the first frame, then hold it so later entries stay unwritten.
    wait_until(|| rec.queue().pending() == 0);
    std::thread::sleep(Duration::from_millis(50));
    rec.queue().hold(true);
    for k in 1..4 {
        rec.frame(IngressOrdinal(k + 1), &mic(k), ms(20 + k));
    }
    rec.cancelled(crate::CancelTarget::Turn { turn: 1 }, ms(30));
    assert_eq!(rec.queue().pending(), 4);
    book.withdraw(&device(), admin(), ms(40), SystemTime::now())
        .unwrap();
    assert_eq!(
        rec.queue().pending(),
        0,
        "pending entries discarded at once"
    );
    assert_eq!(
        rec.frame(IngressOrdinal(9), &mic(9), ms(50)),
        FrameTaken::Ended
    );
    rec.queue().hold(false);
    rec.finish(ms(60)).wait();
    let r = read_only(&store);
    assert_eq!(frames_in(&r), vec![0]);
    assert_eq!(r.integrity.end, Some(EndReason::Withdrawn));
    assert_eq!(r.integrity.tally.discarded_frames, 3);
    assert_eq!(r.integrity.tally.discarded_events, 1);
    assert!(!r.integrity.is_complete());
    assert!(
        r.integrity
            .limitations()
            .contains(&Limitation::EventsDiscarded { events: 1 })
    );
    assert!(r.items.iter().any(|i| matches!(
        i,
        RecordedItem::Marker(Marker::Discarded {
            frames: 3,
            events: 1
        })
    )));
}

fn wait_until(f: impl Fn() -> bool) {
    for _ in 0..500 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition never held");
}

// --- Privacy -------------------------------------------------------------------------------

#[test]
fn mute_discards_pending_audio_and_records_the_mute() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    rec.queue().hold(true);
    for k in 0..3 {
        assert_eq!(
            rec.frame(IngressOrdinal(k), &mic(k), ms(k)),
            FrameTaken::Queued
        );
    }
    rec.revisions(Revisions::default(), ms(5));
    let muted = PrivacyState {
        muted: true,
        capture_permitted: true,
    };
    rec.privacy(Some(IngressOrdinal(10)), muted, ms(10));
    // Only the non-audio entry and the mute itself remain.
    assert_eq!(rec.queue().pending(), 2);
    for k in 3..5 {
        assert_eq!(
            rec.frame(IngressOrdinal(k + 8), &mic(k), ms(20)),
            FrameTaken::Withheld
        );
    }
    rec.privacy(Some(IngressOrdinal(20)), OPEN, ms(30));
    assert_eq!(
        rec.frame(IngressOrdinal(21), &mic(5), ms(40)),
        FrameTaken::Queued
    );
    rec.queue().hold(false);
    rec.finish(ms(50)).wait();
    let r = read_only(&store);
    assert_eq!(
        frames_in(&r),
        vec![5],
        "no audio from before the mute or during it"
    );
    let events: Vec<&Event> = r.events().map(|e| &e.event).collect();
    assert_eq!(events.len(), 4);
    assert!(matches!(events[0], Event::Revisions(_)));
    assert_eq!(
        events[1],
        &Event::Privacy {
            ingress: Some(IngressOrdinal(10)),
            state: muted
        }
    );
    assert_eq!(
        events[2],
        &Event::AudioWithheld {
            reason: WithheldReason::Muted,
            frames: 2
        }
    );
    assert_eq!(
        events[3],
        &Event::Privacy {
            ingress: Some(IngressOrdinal(20)),
            state: OPEN
        }
    );
    assert_eq!(r.integrity.tally.discarded_frames, 3);
    assert_eq!(r.integrity.tally.withheld_frames, 2);
    assert!(!r.integrity.is_complete());
    assert!(r.integrity.is_redacted());
}

#[test]
fn no_audio_is_kept_while_capture_is_not_permitted_or_frames_are_flagged_muted() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let barred = PrivacyState {
        muted: false,
        capture_permitted: false,
    };
    let mut rec = start(&book, &store, barred, ms(0));
    assert_eq!(
        rec.frame(IngressOrdinal(0), &mic(0), ms(1)),
        FrameTaken::Withheld
    );
    rec.privacy(None, OPEN, ms(2));
    let flagged = pcm_frame(Kind::Mic, 3, 1, 1_280, &[0; 1_280], Flags::MUTED);
    assert_eq!(
        rec.frame(IngressOrdinal(1), &flagged, ms(3)),
        FrameTaken::Withheld
    );
    rec.finish(ms(4)).wait();
    let r = read_only(&store);
    assert_eq!(r.frames().count(), 0);
    let withheld: Vec<WithheldReason> = r
        .events()
        .filter_map(|e| match e.event {
            Event::AudioWithheld { reason, frames: 1 } => Some(reason),
            _ => None,
        })
        .collect();
    assert_eq!(
        withheld,
        vec![
            WithheldReason::CaptureNotPermitted,
            WithheldReason::MutedFrame
        ]
    );
}

#[test]
fn only_granted_uplink_frames_are_kept_and_nothing_else_gets_in() {
    let book = GrantBook::new();
    let (_d, store) = store();
    let scope = StreamScope {
        mic: true,
        ..StreamScope::default()
    };
    grant(&book, scope, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    let reference = pcm_frame(Kind::Reference, 4, 0, 0, &[1; 1_280], Flags::NONE);
    let render = ema::encode_render(1, 0, 0, 1, Flags::NONE, &[0; 480]).unwrap();
    let shell = Bytes::from_static(b"$ cat /data/secrets\n");
    assert_eq!(
        rec.frame(IngressOrdinal(0), &reference, ms(1)),
        FrameTaken::NotInScope
    );
    assert_eq!(
        rec.frame(IngressOrdinal(1), &render, ms(1)),
        FrameTaken::NotInScope
    );
    assert_eq!(
        rec.frame(IngressOrdinal(2), &shell, ms(1)),
        FrameTaken::Invalid
    );
    assert_eq!(
        rec.frame(IngressOrdinal(3), &mic(0), ms(1)),
        FrameTaken::Queued
    );
    rec.finish(ms(2)).wait();
    let r = read_only(&store);
    assert_eq!(frames_in(&r), vec![0]);
    assert_eq!(
        r.integrity.streams_withheld,
        vec![
            crate::record::StreamName::Reference,
            crate::record::StreamName::Cells,
            crate::record::StreamName::Afe
        ]
    );
    assert!(r.integrity.transcripts_withheld);
}

#[test]
fn ha_bodies_are_redacted_before_they_are_written() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdXBlcnNlY3JldCI6MX0.c2VjcmV0c2lnbmF0dXJl";
    rec.ha_request(
        crate::ExchangeId(1),
        Some(TurnId(4)),
        HaOperation::IntentTts,
        &json!({
            "type": "assist_pipeline/run",
            "access_token": "llat-supersecret-1",
            "input": { "text": "turn off the lights" },
            "headers": { "Cookie": "session=supersecret-2", "Authorization": format!("Bearer {jwt}") },
        }),
        ms(1),
    );
    rec.ha_reply(
        crate::ExchangeId(1),
        Ok(&json!({ "tts": { "url": "/api/tts_proxy/x.mp3?authSig=supersecret-3" } })),
        ms(2),
    );
    rec.ha_reply(
        crate::ExchangeId(2),
        Err(&HaError::Rejected {
            code: "invalid_auth".into(),
            message: "token supersecret-4 refused".into(),
        }),
        ms(3),
    );
    rec.finish(ms(4)).wait();
    let path = store.list().unwrap()[0].path.clone();
    let raw = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&raw);
    for secret in ["supersecret", "llat-", jwt] {
        assert!(!text.contains(secret), "{secret} reached the disk");
    }
    let r = Recording::read(&path).unwrap();
    let ha: Vec<&HaEntry> = r
        .events()
        .filter_map(|e| match &e.event {
            Event::Ha(h) => Some(h),
            _ => None,
        })
        .collect();
    let HaEntry::Request {
        body: HaBody::Kept { json, redactions },
        operation,
        turn,
        ..
    } = ha[0]
    else {
        panic!("request body kept");
    };
    assert_eq!(
        (*operation, *turn, *redactions),
        (HaOperation::IntentTts, Some(4), 3)
    );
    assert_eq!(json["access_token"], REDACTED);
    assert_eq!(json["headers"]["Cookie"], REDACTED);
    assert_eq!(json["input"]["text"], "turn off the lights");
    assert_eq!(r.integrity.tally.credential_redactions, 4);
    assert!(r.integrity.is_redacted());
    assert!(matches!(
        ha[2],
        HaEntry::Reply {
            outcome: Err(crate::record::HaFailure::Rejected { .. }),
            ..
        }
    ));
}

#[test]
fn without_a_transcripts_grant_text_and_ha_bodies_are_withheld() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::default(), 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    rec.ha_request(
        crate::ExchangeId(1),
        None,
        HaOperation::Recognize,
        &json!({ "sentences": ["what time is it"] }),
        ms(1),
    );
    rec.wake_verified("c1", &VerifierResult::Text("ophelia".into()), None, ms(2));
    rec.finish(ms(3)).wait();
    let path = store.list().unwrap()[0].path.clone();
    let raw = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.contains("what time"));
    assert!(!text.contains("ophelia"));
}

// --- Bounds and files ----------------------------------------------------------------------

#[test]
fn a_full_queue_drops_and_marks_without_blocking() {
    let book = GrantBook::new();
    let (_d, store) = store_with(QueueBounds {
        max_entries: 4,
        max_bytes: 1 << 20,
    });
    grant(&book, StreamScope::ALL, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    rec.queue().hold(true);
    // The writer takes nothing while held: a push that waited for room would never return.
    let (tx, rx) = mpsc::channel();
    let pusher = std::thread::spawn(move || {
        let taken: Vec<FrameTaken> = (0..10)
            .map(|k| rec.frame(IngressOrdinal(k), &mic(k), ms(k)))
            .collect();
        tx.send(()).unwrap();
        (rec, taken)
    });
    rx.recv_timeout(Duration::from_secs(5))
        .expect("pushing into a full queue returned at once");
    let (mut rec, taken) = pusher.join().unwrap();
    assert_eq!(
        taken.iter().filter(|t| **t == FrameTaken::Queued).count(),
        4
    );
    assert_eq!(taken.iter().filter(|t| **t == FrameTaken::Lost).count(), 6);
    assert_eq!(rec.state(), RecorderState::Recording { incomplete: true });
    // A mute is never lost to a full queue.
    rec.privacy(
        None,
        PrivacyState {
            muted: true,
            capture_permitted: true,
        },
        ms(20),
    );
    rec.queue().hold(false);
    rec.finish(ms(30)).wait();
    let r = read_only(&store);
    assert!(
        r.items
            .iter()
            .any(|i| matches!(i, RecordedItem::Marker(Marker::Lost { entries: 6 })))
    );
    assert!(r.events().any(|e| matches!(e.event, Event::Privacy { .. })));
    assert_eq!(r.integrity.tally.lost_entries, 6);
    assert!(
        r.integrity
            .limitations()
            .contains(&Limitation::EntriesLost { entries: 6 })
    );
    assert!(!r.integrity.is_complete());
}

#[test]
fn files_and_directories_are_private() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("recordings");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    let store = RecordingStore::open(RecorderConfig::new(&root)).unwrap();
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode(&root),
        0o700,
        "an existing wide directory is tightened"
    );
    let book = GrantBook::new();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    rec.frame(IngressOrdinal(0), &mic(0), ms(1));
    let id = rec.id().clone();
    rec.finish(ms(2)).wait();
    let dev_dir = store.device_dir(&device());
    assert_eq!(dev_dir.file_name().unwrap(), "kitchen%2Fdot%201");
    assert_eq!(mode(&dev_dir), 0o700);
    let file = dev_dir.join(format!("{}.emrec", id.as_str()));
    assert_eq!(mode(&file), 0o600);
}

#[test]
fn retention_and_deletion_remove_recordings_and_stop_writers() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(0));
    let rec = start(&book, &store, OPEN, ms(0));
    let first = rec.id().clone();
    rec.finish(ms(1)).wait();
    let mut live = start(&book, &store, OPEN, ms(2));
    live.frame(IngressOrdinal(0), &mic(0), ms(3));
    let anchor = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    wait_until(|| store.list().unwrap().len() == 2);
    // Nothing is due a day after the start; both are due after the default seven days.
    assert!(
        store
            .sweep(anchor + Duration::from_secs(86_400))
            .unwrap()
            .is_empty()
    );
    // Deleting the live one stops its writer, which cannot recreate it.
    assert!(store.delete(live.id()).unwrap());
    assert_eq!(live.state(), RecorderState::Deleted);
    live.frame(IngressOrdinal(1), &mic(1), ms(4));
    live.finish(ms(5)).wait();
    let left = store.list().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].header.as_ref().unwrap().recording, first);
    assert!(left[0].finished);
    let swept = store
        .sweep(anchor + Duration::from_secs(8 * 86_400))
        .unwrap();
    assert_eq!(swept.len(), 1);
    assert!(store.list().unwrap().is_empty());
    // Device deletion removes the directory.
    let rec = start(&book, &store, OPEN, ms(6));
    rec.finish(ms(7)).wait();
    assert_eq!(store.delete_device(&device()).unwrap(), 1);
    assert!(!store.device_dir(&device()).exists());
}

// --- Format --------------------------------------------------------------------------------

#[test]
fn the_format_round_trips_and_refuses_other_versions() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 60, ms(1_000));
    let mut rec = start(&book, &store, OPEN, ms(1_000));
    let frame = mic(0);
    rec.stream_opened(
        IngressOrdinal(1),
        echomuse_protocol::StreamId::Mic,
        Epoch::new(3).unwrap(),
        ms(1_001),
    );
    rec.frame(IngressOrdinal(2), &frame, ms(1_002));
    rec.turn_control(
        TurnId(5),
        TurnControl::Mute,
        &[Decision::Close {
            reason: echomuse_turn::CloseReason::Muted,
            at: Some(Sample(512)),
        }],
        ms(1_003),
    );
    let id = rec.id().clone();
    rec.disconnected(crate::DisconnectReason::DeviceClosed, ms(1_004))
        .wait();
    let path = store.list().unwrap()[0].path.clone();
    let r = Recording::read(&path).unwrap();
    assert_eq!(r.header.recording, id);
    assert_eq!(r.header.session.device, "kitchen/dot 1");
    assert_eq!(
        r.header.session.link_session,
        "2c1b7c56-1f0e-4f4e-8f0a-1d2b3c4d5e6f"
    );
    assert_eq!(r.header.grant.remaining_at_start_ms, 60_000);
    assert_eq!(r.header.anchor.0, 1_800_000_000_000);
    assert_eq!(r.integrity.tail, Tail::Ended);
    assert_eq!(r.integrity.end, Some(EndReason::Disconnected));
    assert!(r.integrity.is_complete());
    let seqs: Vec<u64> = r.items.iter().filter_map(RecordedItem::seq).collect();
    assert_eq!(seqs, vec![0, 1, 2, 3]);
    let f = r.frames().next().unwrap();
    assert_eq!(
        (f.frame.clone(), f.ingress, f.t.0),
        (frame, IngressOrdinal(2), 2_000_000)
    );
    let events: Vec<&Event> = r.events().map(|e| &e.event).collect();
    assert!(matches!(events[0], Event::StreamOpened { .. }));
    assert!(matches!(events[1], Event::Turn { turn: 5, .. }));
    assert!(matches!(events[2], Event::Disconnected { .. }));
    assert_eq!(crate::read_header(&path).unwrap(), r.header);

    let bytes = std::fs::read(&path).unwrap();
    let mut other = bytes.clone();
    other[8] = 2;
    assert!(matches!(
        Recording::from_bytes(other.into()),
        Err(ReadError::UnsupportedVersion {
            found: 2,
            supported: 1
        })
    ));
    let mut not = bytes.clone();
    not[0] = b'X';
    assert!(matches!(
        Recording::from_bytes(not.into()),
        Err(ReadError::NotARecording)
    ));
    // Cut short: what is there is read, and it is marked unended.
    let cut = Recording::from_bytes(bytes[..bytes.len() - 10].to_vec().into()).unwrap();
    assert_eq!(cut.integrity.tail, Tail::Unended);
    assert!(cut.integrity.limitations().contains(&Limitation::NotEnded));
    // A damaged byte stops reading at that record.
    let mut bad = bytes.clone();
    let n = bad.len();
    bad[n - 20] ^= 0xFF;
    let bad = Recording::from_bytes(bad.into()).unwrap();
    assert_eq!(bad.integrity.tail, Tail::Corrupt);
}

// --- Decision replay -----------------------------------------------------------------------

/// The utterance start: cell 100.
const U: u64 = 51_200;
const CELL: u64 = 512;
const BLOCK: u64 = 1_280;

fn epoch() -> Epoch {
    Epoch::new(3).unwrap()
}

fn c(n: u64) -> Sample {
    Sample(U + n * CELL)
}

#[derive(Clone, Default)]
struct Timeline {
    cells: Vec<(f32, i16)>,
    words: Vec<(String, Sample)>,
}

const QUIET: (f32, i16) = (0.05, -7_500);
const SPEECH: (f32, i16) = (0.95, -5_000);

impl Timeline {
    fn then(mut self, cell: (f32, i16), n: usize) -> Self {
        self.cells.extend(std::iter::repeat_n(cell, n));
        self
    }

    fn word(mut self, text: &str, at: Sample) -> Self {
        self.words.push((format!(" {text}"), at));
        self
    }

    fn asr(&self, through: Sample) -> AsrObservation {
        let tokens: Vec<Token> = self
            .words
            .iter()
            .filter(|(_, at)| *at <= through)
            .map(|(t, at)| Token {
                text: t.clone(),
                emitted: *at,
            })
            .collect();
        let last = tokens.last().map_or(Sample(U), |t| t.emitted);
        AsrObservation {
            through,
            tokens,
            trailing_blanks: (through.since(last).0 / 640) as u32,
            kind: AsrKind::Live,
        }
    }
}

fn lights() -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(QUIET, 400)
        .word("turn", c(12))
        .word("off", c(15))
        .word("the", c(17))
        .word("lights", c(21))
}

fn time_question() -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(QUIET, 400)
        .word("what", c(13))
        .word("time", c(15))
        .word("is", c(17))
        .word("it", c(21))
}

#[derive(Default)]
struct Grammar(HashMap<&'static str, TextClass>);

impl Completeness for Grammar {
    fn classify(&self, text: &str) -> TextClass {
        self.0.get(text).copied().unwrap_or(TextClass::Unknown)
    }
}

/// A recorded runtime: drives the real policies and records every call.
struct Sim {
    rec: SessionRecorder,
}

impl Sim {
    fn wake(
        &mut self,
        id: &str,
        producing_sound: bool,
        t: u64,
        steps: &[(u64, WakeInput)],
    ) -> Option<WakeOutcome> {
        let candidate = WakeCandidate {
            candidate_id: id.to_owned(),
            profile: if producing_sound {
                WakeProfile::Playback
            } else {
                WakeProfile::Idle
            },
            threshold: 0.5,
            producing_sound,
            first_crossing_end: Sample(30_000),
            support_start: Sample(20_000),
            hops: vec![
                WakeHop {
                    end: Sample(30_000),
                    raw: Some(0.8),
                    smoothed: Some(0.7),
                },
                WakeHop {
                    end: Sample(31_280),
                    raw: Some(0.9),
                    smoothed: Some(0.8),
                },
            ],
        };
        let policy = WakePolicy::default();
        let conditions = ArrivalConditions::default();
        let mut d = WakeDecider::arrive(candidate.clone(), conditions, policy.clone(), ms(t));
        self.rec
            .wake_arrived(&candidate, conditions, &policy, d.decision(), ms(t));
        for (at, input) in steps {
            let now = ms(*at);
            match input {
                WakeInput::Reference(s) => {
                    let out = d.reference_scored(*s, now);
                    self.rec.wake_reference(id, *s, out, now);
                }
                WakeInput::Echo(e) => {
                    let out = d.echo_compared(*e, now);
                    self.rec.wake_echo(id, *e, out, now);
                }
                WakeInput::Verified(text) => {
                    let v = VerifierResult::Text((*text).to_owned());
                    let out = d.verified(v.clone(), now);
                    self.rec.wake_verified(id, &v, out, now);
                }
                WakeInput::Timer => {
                    let out = d.on_timer(now);
                    self.rec.wake_timer(id, out, now);
                }
            }
        }
        d.decision().map(|x| x.outcome)
    }

    /// A button turn over `tl`, from `t0` ms, one block per 80 ms. `answer` gives HA's
    /// sentence-matcher answer after that block; `mute_at` mutes the device after that block.
    #[allow(clippy::too_many_arguments)]
    fn turn(
        &mut self,
        turn: TurnId,
        config: TurnConfig,
        tl: &Timeline,
        grammar: &dyn Completeness,
        t0: u64,
        answer: Option<(u64, RecognizerAnswer)>,
        mute_at: Option<u64>,
    ) -> Vec<Decision> {
        let spec = TurnSpec {
            epoch: epoch(),
            start: Sample(U),
            trigger: c(10),
            cells_from: CellIndex(U / CELL - 100),
            vad_from: Sample(U).cell(),
            kind: TurnKind::Button,
        };
        let mut policy = TurnPolicy::new(config, spec.clone(), ms(t0)).unwrap();
        self.rec
            .turn_opened(turn, TurnCause::Button, &config, &spec, ms(t0));
        let history = vec![DeviceInput::Cell(DeviceCell::audio(-7_500, 0)); 100];
        let first = CellIndex(U / CELL - 100);
        policy
            .evidence_mut()
            .push_device_cells(epoch(), first, &history)
            .unwrap();
        self.rec
            .turn_device_cells(turn, epoch(), first, &history, ms(t0));
        let mut all = Vec::new();
        let blocks = (tl.cells.len() as u64 * CELL) / BLOCK;
        for k in 1..=blocks {
            let now = ms(t0 + k * 80);
            let from = ((k - 1) * BLOCK).div_ceil(CELL);
            let to = (k * BLOCK).div_ceil(CELL).min(tl.cells.len() as u64);
            for i in from..to {
                let (vad, e) = tl.cells[i as usize];
                let cell = c(i).cell();
                let device = [DeviceInput::Cell(DeviceCell::audio(e, 0))];
                let vad = [VadInput::probability(vad).unwrap()];
                let echo = [EchoCell {
                    label: EchoLabel::NoReference,
                    coverage: ReferenceCoverage::Silent,
                }];
                let ev = policy.evidence_mut();
                ev.push_device_cells(epoch(), cell, &device).unwrap();
                ev.push_vad(epoch(), cell, &vad).unwrap();
                ev.push_echo(epoch(), cell, &echo).unwrap();
                self.rec
                    .turn_device_cells(turn, epoch(), cell, &device, now);
                self.rec.turn_vad(turn, epoch(), cell, &vad, now);
                self.rec.turn_echo(turn, epoch(), cell, &echo, now);
            }
            let obs = tl.asr(Sample(U + k * BLOCK));
            policy
                .evidence_mut()
                .push_asr(epoch(), obs.clone())
                .unwrap();
            self.rec.turn_asr(turn, epoch(), &obs, now);
            let tap = GrammarTap::new(grammar);
            let mut out = Vec::new();
            policy.advance(&tap, now, &mut out);
            self.rec.turn_advanced(turn, &policy, tap.take(), &out, now);
            all.append(&mut out);
            if let Some((kb, a)) = &answer
                && *kb == k
            {
                policy.recognizer_answer(a.clone(), now);
                self.rec.turn_recognizer_answer(turn, a, now);
            }
            if mute_at == Some(k) {
                let muted = PrivacyState {
                    muted: true,
                    capture_permitted: true,
                };
                self.rec.privacy(Some(IngressOrdinal(k)), muted, now);
                policy.mute(&mut out);
                self.rec.turn_control(turn, TurnControl::Mute, &out, now);
                all.append(&mut out);
                self.rec.privacy(None, OPEN, now);
            }
            if policy.is_finished() {
                break;
            }
        }
        all
    }
}

#[derive(Clone)]
enum WakeInput {
    Reference(ReferenceScore),
    Echo(CandidateEcho),
    Verified(&'static str),
    Timer,
}

/// Records the synthetic session: three wake candidates and three turns.
fn record_session(scope: StreamScope) -> (tempfile::TempDir, RecordingStore, Recording) {
    let book = GrantBook::new();
    let (dir, store) = store();
    grant(&book, scope, 600, ms(0));
    let mut sim = Sim {
        rec: start(&book, &store, OPEN, ms(0)),
    };
    use WakeInput::*;
    // Quiet device: accepted at once.
    assert_eq!(
        sim.wake("idle", false, 10, &[]),
        Some(WakeOutcome::Accepted)
    );
    // Playing: the echo explains it.
    assert_eq!(
        sim.wake(
            "echo",
            true,
            100,
            &[
                (150, Reference(ReferenceScore::NoOverlap)),
                (200, Echo(CandidateEcho::EchoOnly))
            ]
        ),
        Some(WakeOutcome::Refused(WakeRefusal::EchoOnly))
    );
    // Playing, double talk, verified.
    assert_eq!(
        sim.wake(
            "verified",
            true,
            300,
            &[
                (320, Reference(ReferenceScore::NoOverlap)),
                (340, Echo(CandidateEcho::NearEndPresent)),
                (500, Verified("ophelia")),
            ]
        ),
        Some(WakeOutcome::Accepted)
    );
    // Playing, no verdict: the deadline refuses it.
    assert_eq!(
        sim.wake(
            "timeout",
            true,
            600,
            &[
                (610, Reference(ReferenceScore::NoOverlap)),
                (620, Echo(CandidateEcho::Unknown)),
                (1_300, Timer),
            ]
        ),
        Some(WakeOutcome::Refused(WakeRefusal::VerifierTimeout))
    );
    let grammar = Grammar(
        [("turn off the lights", TextClass::Complete)]
            .into_iter()
            .collect(),
    );
    let lights_out = sim.turn(
        TurnId(1),
        TurnConfig::default(),
        &lights(),
        &grammar,
        2_000,
        None,
        None,
    );
    assert!(lights_out.iter().any(|d| matches!(d, Decision::Commit(_))));
    let config = TurnConfig {
        recognizer_available: true,
        ..TurnConfig::default()
    };
    let answer = RecognizerAnswer {
        text: "what time is it".into(),
        outcome: Ok(TextClass::Complete),
    };
    let question = sim.turn(
        TurnId(2),
        config,
        &time_question(),
        &Grammar::default(),
        20_000,
        Some((16, answer)),
        None,
    );
    assert!(
        question
            .iter()
            .any(|d| matches!(d, Decision::AskRecognizer { .. }))
    );
    assert!(question.iter().any(|d| matches!(d, Decision::Commit(_))));
    let muted = sim.turn(
        TurnId(3),
        TurnConfig::default(),
        &lights(),
        &grammar,
        40_000,
        None,
        Some(12),
    );
    assert!(muted.iter().any(|d| matches!(
        d,
        Decision::Close {
            reason: echomuse_turn::CloseReason::Muted,
            ..
        }
    )));
    sim.rec.finish(ms(60_000)).wait();
    let r = read_only(&store);
    (dir, store, r)
}

#[test]
fn replay_reproduces_the_recorded_decisions() {
    let (_d, _s, r) = record_session(StreamScope::ALL);
    let report = replay(&r, &ReplayOptions::default());
    assert_eq!(report.mismatches(), vec![]);
    assert_eq!(report.equivalence(), Equivalence::Proven);
    assert!(report.divergences.is_empty(), "{:?}", report.divergences);
    assert_eq!(report.wakes.len(), 4);
    assert_eq!(report.turns.len(), 3);
    let t1 = &report.turns[0];
    assert_eq!(t1.replayed.terminal, Terminal::Committed);
    let span = t1.replayed.committed.as_ref().unwrap();
    assert_eq!(
        span.span,
        SampleSpan::new(Sample(U), c(21) + Samples::from_ms(192))
    );
    assert_eq!(span.text.text(), Some("turn off the lights"));
    let t2 = &report.turns[1];
    // The policy asked about each unknown text as it grew; the last question is the whole one.
    assert_eq!(t2.replayed.asked_ha, t2.recorded.asked_ha);
    assert_eq!(
        t2.replayed.asked_ha.last().and_then(|t| t.text()),
        Some("what time is it")
    );
    assert_eq!(t2.recorded_applied.len(), 1);
    assert_eq!(t2.recorded_applied, t2.replayed_applied);
    assert_eq!(t2.replayed_applied[0].at, Sample(U + 17 * BLOCK));
    assert_eq!(
        report.turns[2].replayed.terminal,
        Terminal::Closed(echomuse_turn::CloseReason::Muted)
    );
}

#[test]
fn replay_reports_a_mismatch_when_the_policy_changes() {
    let (_d, _s, r) = record_session(StreamScope::ALL);
    let mut config = TurnConfig::default();
    config.endpoint.max_utterance = Samples::from_ms(400);
    let report = replay(
        &r,
        &ReplayOptions {
            turn_config: Some(config),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(report.equivalence(), Equivalence::Differs);
    let turns: Vec<u64> = report
        .mismatches()
        .iter()
        .filter_map(|m| match m {
            Mismatch::Turn {
                turn,
                recorded,
                replayed,
            } => {
                assert_eq!(
                    replayed.terminal,
                    Terminal::Closed(echomuse_turn::CloseReason::TooLong)
                );
                assert_ne!(recorded.terminal, replayed.terminal);
                Some(*turn)
            }
            Mismatch::Wake { .. } => None,
        })
        .collect();
    assert_eq!(turns, vec![1, 2, 3]);

    // A stricter verifier refuses the verified wake instead.
    let wake = WakePolicy {
        verify_pass_max: -1.0,
        ..WakePolicy::default()
    };
    let report = replay(
        &r,
        &ReplayOptions {
            wake_policy: Some(wake),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(
        report.mismatches(),
        vec![Mismatch::Wake {
            candidate_id: "verified".into(),
            recorded: Some(WakeOutcome::Accepted),
            replayed: Some(WakeOutcome::Refused(WakeRefusal::UnverifiedWake)),
        }]
    );
}

#[test]
fn replay_fires_a_shorter_deadline_at_its_own_instant() {
    let (_d, _s, r) = record_session(StreamScope::ALL);
    // A 100 ms budget decides the echo candidate before its echo comparison arrives: only the
    // reference is in, and a missing verdict at the deadline refuses as a timeout.
    let report = replay(
        &r,
        &ReplayOptions {
            wake_policy: Some(WakePolicy {
                decision_budget_ms: 100,
                ..WakePolicy::default()
            }),
            ..ReplayOptions::default()
        },
    );
    let echo = report
        .wakes
        .iter()
        .find(|w| w.candidate_id == "echo")
        .unwrap();
    assert_eq!(
        echo.replayed,
        Some(WakeOutcome::Refused(WakeRefusal::VerifierTimeout))
    );
    assert_eq!(
        echo.recorded,
        Some(WakeOutcome::Refused(WakeRefusal::EchoOnly))
    );
}

#[test]
fn a_recording_without_transcripts_is_never_proof() {
    let scope = StreamScope {
        cells: true,
        ..StreamScope::default()
    };
    let (_d, _s, r) = record_session(scope);
    assert!(r.integrity.transcripts_withheld);
    let report = replay(&r, &ReplayOptions::default());
    assert!(matches!(report.equivalence(), Equivalence::Unproven { .. }));
    assert!(report.turns.iter().all(|t| t.not_comparable.is_some()));
}

#[test]
fn an_incomplete_recording_is_never_proof() {
    let (_d, _s, r) = record_session(StreamScope::ALL);
    let mut cut = r.clone();
    cut.end = None;
    cut.integrity.tail = Tail::Unended;
    let report = replay(&cut, &ReplayOptions::default());
    match report.equivalence() {
        Equivalence::Unproven { limitations, .. } => {
            assert!(limitations.contains(&Limitation::NotEnded));
        }
        other => panic!("{other:?}"),
    }
}

/// A deterministic noise source.
fn noise(n: usize, seed: u32) -> Vec<i16> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 16) as i16) / 4
        })
        .collect()
}

#[test]
fn echo_recomputation_from_recorded_frames_reproduces_the_labels() {
    let book = GrantBook::new();
    let (_d, store) = store();
    grant(&book, StreamScope::ALL, 600, ms(0));
    let mut rec = start(&book, &store, OPEN, ms(0));
    let id = crate::AnalyzerId(1);
    let policy = EchoPolicy::default();
    let plan = EchoPlan {
        epoch: epoch(),
        cells_from: CellIndex(0),
        vad_from: CellIndex(0),
    };
    let mut analyzer = EchoAnalyzer::new(policy.clone());
    analyzer.open(plan).unwrap();
    rec.echo_input(id, EchoStep::Opened { policy, plan }, ms(0));
    analyzer.start_labels(CellIndex(40)).unwrap();
    rec.echo_input(
        id,
        EchoStep::LabelsStarted {
            from: CellIndex(40),
        },
        ms(0),
    );
    let ref_epoch = Epoch::new(8).unwrap();
    let blocks = 50u64;
    let reference = noise((blocks * BLOCK) as usize + 400, 7);
    let mut produced = 0;
    for k in 0..blocks {
        let now = ms(k * 80);
        let first = k * BLOCK;
        let r: Vec<i16> = reference[first as usize..(first + BLOCK) as usize].to_vec();
        // The mic hears the reference 160 samples later, at half level.
        let m: Vec<i16> = (first..first + BLOCK)
            .map(|i| {
                if i >= 160 {
                    reference[(i - 160) as usize] / 2
                } else {
                    0
                }
            })
            .collect();
        let mono = 1_000_000_000 + first * 62_500;
        let mf = pcm_frame(Kind::Mic, 3, k, first, &m, Flags::NONE);
        let rf = pcm_frame(Kind::Reference, 8, k, first, &r, Flags::NONE);
        assert_eq!(
            rec.frame(IngressOrdinal(2 * k), &mf, now),
            FrameTaken::Queued
        );
        assert_eq!(
            rec.frame(IngressOrdinal(2 * k + 1), &rf, now),
            FrameTaken::Queued
        );
        analyzer
            .push_mic(epoch(), Sample(first), mono, m.iter().copied())
            .unwrap();
        rec.echo_input(
            id,
            EchoStep::Mic {
                epoch: epoch(),
                first: Sample(first),
                end: Sample(first + BLOCK),
                mono_ns: mono,
            },
            now,
        );
        analyzer
            .push_reference(
                epoch(),
                ref_epoch,
                first,
                mono,
                OutputCoverage::Full,
                r.iter().copied(),
            )
            .unwrap();
        rec.echo_input(
            id,
            EchoStep::Reference {
                epoch: epoch(),
                reference_epoch: ref_epoch,
                first,
                end: first + BLOCK,
                mono_ns: mono,
                coverage: OutputCoverage::Full,
            },
            now,
        );
        // Cells and VAD for every cell this block completes.
        let from = first / CELL;
        let to = (first + BLOCK) / CELL;
        if to > from {
            let cells: Vec<DeviceInput> = (from..to)
                .map(|cell| {
                    let s = (cell * CELL) as usize;
                    let pcm: Vec<i16> = (s..s + CELL as usize)
                        .map(|i| if i >= 160 { reference[i - 160] / 2 } else { 0 })
                        .collect();
                    DeviceInput::Cell(DeviceCell::audio(DeviceCell::loudness_centi(&pcm), 1))
                })
                .collect();
            let vad = vec![VadInput::probability(0.1).unwrap(); cells.len()];
            analyzer
                .push_device_cells(epoch(), CellIndex(from), &cells)
                .unwrap();
            rec.echo_input(
                id,
                EchoStep::DeviceCells {
                    epoch: epoch(),
                    first: CellIndex(from),
                    cells: cells.iter().map(|&c| c.into()).collect(),
                },
                now,
            );
            analyzer.push_vad(epoch(), CellIndex(from), &vad).unwrap();
            rec.echo_input(
                id,
                EchoStep::Vad {
                    epoch: epoch(),
                    first: CellIndex(from),
                    vad: vad.iter().map(|&v| v.into()).collect(),
                },
                now,
            );
        }
        let mut labels = Vec::new();
        analyzer.poll(&mut labels);
        produced += labels.len();
        rec.echo_polled(id, &labels, now);
    }
    let support = SampleSpan::new(Sample(30_000), Sample(50_000));
    let cmp = analyzer.compare_candidate(support);
    rec.echo_compared(id, support, Some("c"), cmp.as_ref(), ms(blocks * 80));
    rec.finish(ms(blocks * 80 + 1)).wait();
    assert!(produced > 0, "the live analyzer produced labels");
    let r = read_only(&store);
    let report = replay(
        &r,
        &ReplayOptions {
            echo: EchoSource::Recompute { policy: None },
            ..ReplayOptions::default()
        },
    );
    assert_eq!(report.echo.len(), 1);
    let e = &report.echo[0];
    assert_eq!(e.unavailable, None);
    assert_eq!(e.labels_compared, produced as u64);
    assert_eq!((e.labels_differing, e.labels_missing), (0, 0));
    assert_eq!((e.candidates_compared, e.candidates_differing), (1, 0));
    assert_eq!(report.equivalence(), Equivalence::Proven);

    // Without the mic audio, the analyzer cannot be recomputed, and the replay says so.
    let mut no_mic = r.clone();
    no_mic.items.retain(|i| match i {
        RecordedItem::Frame(f) => ema::decode_header(&f.frame).unwrap().kind != Kind::Mic,
        _ => true,
    });
    let report = replay(
        &no_mic,
        &ReplayOptions {
            echo: EchoSource::Recompute { policy: None },
            ..ReplayOptions::default()
        },
    );
    assert!(matches!(
        report.echo[0].unavailable,
        Some(crate::replay::EchoUnavailable::MicMissing { .. })
    ));
}
