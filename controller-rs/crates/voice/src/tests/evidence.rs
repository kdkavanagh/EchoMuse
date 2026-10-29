//! Decision evidence for the session recorder: a turn the machine ran, recorded through a
//! real recorder and replayed by its decision replay, reproduces the machine's outcomes;
//! a changed endpoint policy does not.

use std::time::{Duration, SystemTime};

use echomuse_core::types::{DeviceId, RuntimeId, SessionId};
use echomuse_protocol::LinkSessionId;
use echomuse_recorder::{
    AdministratorId, Equivalence, GrantBook, GrantRequest, PrivacyState, RecorderConfig, Recording,
    RecordingStore, ReplayOptions, Revisions, SessionIdentity, SessionRecorder, StreamScope,
    replay,
};
use echomuse_turn::{MonoInstant, Samples, TurnConfig};

use super::rig::{Options, Rig};
use super::{command, wake};
use crate::evidence::Evidence;

/// Writes the rig's evidence into a recording and reads it back.
fn record(rig: &mut Rig) -> (tempfile::TempDir, Recording) {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordingStore::open(RecorderConfig::new(dir.path().join("recordings"))).unwrap();
    let device = DeviceId("kitchen".to_owned());
    let book = GrantBook::new();
    let start = MonoInstant(0);
    book.grant(
        &device,
        GrantRequest {
            administrator: AdministratorId::new("kyle").unwrap(),
            interval: Duration::from_secs(3_600),
            streams: StreamScope::ALL,
        },
        start,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    )
    .unwrap();
    let mut recorder = SessionRecorder::start(
        book.pass(&device, start).unwrap(),
        &store,
        SessionIdentity {
            device,
            runtime: RuntimeId(1),
            session: SessionId(1),
            link_session: LinkSessionId::from("session-1".to_owned()),
        },
        Revisions::default(),
        PrivacyState {
            muted: false,
            capture_permitted: true,
        },
        start,
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000),
    )
    .unwrap();
    let mut last = start;
    for (at, evidence) in rig.evidence.drain(..) {
        last = last.max(at);
        evidence.record(&mut recorder, at, |m| m);
    }
    recorder.finish(last).wait();
    let files = store.list().unwrap();
    assert_eq!(files.len(), 1);
    let recording = Recording::read(&files[0].path).unwrap();
    (dir, recording)
}

fn recorded_turn(recording: bool) -> Rig {
    let mut rig = Rig::new(Options::default());
    rig.voice.set_recording(recording);
    wake(&mut rig, "c1");
    let _stt = command(&mut rig, &["turn", "on", "the", "kitchen", "light"]);
    rig
}

#[tokio::test(start_paused = true)]
async fn nothing_is_queued_while_no_recording_runs() {
    let rig = recorded_turn(false);
    assert!(rig.evidence.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_recorded_wake_turn_replays_to_the_same_outcomes() {
    let mut rig = recorded_turn(true);
    assert!(matches!(
        rig.evidence.first(),
        Some((_, Evidence::WakeArrived { .. }))
    ));
    assert!(
        rig.evidence
            .iter()
            .any(|(_, e)| matches!(e, Evidence::TurnOpened { .. }))
    );
    assert!(
        rig.evidence
            .iter()
            .any(|(_, e)| matches!(e, Evidence::HaRequest { .. })),
        "HA's requests are noted"
    );
    assert!(
        rig.evidence
            .iter()
            .any(|(_, e)| matches!(e, Evidence::RecognizerAnswer { .. })
                || matches!(e, Evidence::HaReply { .. })),
        "and its answers"
    );
    let (_dir, recording) = record(&mut rig);
    let report = replay(&recording, &ReplayOptions::default());
    assert_eq!(report.wakes.len(), 1, "{report:#?}");
    assert_eq!(report.turns.len(), 1, "{report:#?}");
    assert!(report.divergences.is_empty(), "{:#?}", report.divergences);
    assert_eq!(report.equivalence(), Equivalence::Proven, "{report:#?}");
    let turn = &report.turns[0];
    assert!(
        turn.recorded.committed.is_some(),
        "the turn committed: {turn:#?}"
    );

    // With far longer pauses, the replayed policy never commits: the outcomes differ.
    let mut slow = TurnConfig::default();
    slow.endpoint.pause_complete = Samples(16_000 * 20);
    slow.endpoint.pause_extendable = Samples(16_000 * 20);
    slow.endpoint.pause_needs_more = Samples(16_000 * 20);
    let options = ReplayOptions {
        turn_config: Some(slow),
        ..ReplayOptions::default()
    };
    let report = replay(&recording, &options);
    assert_eq!(report.equivalence(), Equivalence::Differs, "{report:#?}");
}
