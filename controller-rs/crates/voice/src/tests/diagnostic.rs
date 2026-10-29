//! The recording modes' `diagnostic` subscription.

use echomuse_protocol::{StreamId, vocab::AckStatus};
use echomuse_uplink::{ChunkBody, ChunkData, EndCause, UplinkCommand};

use super::rig::{Options, Rig};
use crate::{DiagnosticAudio, Input, Outbound};

fn diagnostic_opens(rig: &Rig) -> usize {
    rig.uplink_commands()
        .iter()
        .filter(|command| {
            matches!(command, UplinkCommand::Open(open)
                if serde_json::to_value(open).expect("serializes")["reason"] == "diagnostic")
        })
        .count()
}

fn mic_samples(rig: &Rig) -> usize {
    rig.diagnostics
        .iter()
        .map(|audio| match audio {
            DiagnosticAudio::Chunk(chunk) if chunk.stream == StreamId::Mic => match &chunk.body {
                ChunkBody::Data {
                    data: ChunkData::Pcm(pcm),
                    ..
                } => pcm.samples().len(),
                _ => 0,
            },
            _ => 0,
        })
        .sum()
}

#[tokio::test(start_paused = true)]
async fn an_armed_mode_records_live_mic_and_refuses_wakes() {
    let mut rig = Rig::new(Options::default());
    rig.silence(500);
    rig.handle(Input::Diagnostic(true));
    assert_eq!(diagnostic_opens(&rig), 1);
    assert!(rig.voice.diagnostic_active());
    rig.silence(1_000);
    assert!(mic_samples(&rig) >= 12_000, "about a second of live mic");
    // A wake candidate is refused while the recording runs (§4.1).
    rig.wake("c1", 9_600, 26_400, false);
    assert!(rig.sent.iter().any(|(body, _)| matches!(body,
        Outbound::CandidateAck(ack) if ack.status == AckStatus::Rejected)));
    rig.handle(Input::Diagnostic(false));
    assert!(
        rig.uplink_commands()
            .iter()
            .any(|command| matches!(command, UplinkCommand::Close(_)))
    );
}

#[tokio::test(start_paused = true)]
async fn a_mute_ends_the_recording_and_an_unmute_reopens_it() {
    let mut rig = Rig::new(Options::default());
    rig.silence(500);
    rig.handle(Input::Diagnostic(true));
    rig.silence(500);
    rig.handle(Input::Privacy {
        muted: true,
        physical_seq: Some(1),
    });
    assert!(
        rig.diagnostics
            .iter()
            .any(|audio| matches!(audio, DiagnosticAudio::Ended(EndCause::Muted)))
    );
    assert!(!rig.voice.diagnostic_active());
    rig.silence(300);
    assert_eq!(diagnostic_opens(&rig), 1, "nothing opens while muted");
    rig.handle(Input::Privacy {
        muted: false,
        physical_seq: Some(2),
    });
    rig.handle(Input::Capture(Some(echomuse_core::types::WorkScope::new(
        echomuse_core::types::WorkIdentity {
            device: echomuse_core::types::DeviceId("G090LF0000000001".to_owned()),
            runtime: echomuse_core::types::RuntimeId(1),
            session: echomuse_core::types::SessionId(1),
            turn: None,
        },
        tokio_util::sync::CancellationToken::new(),
    ))));
    rig.silence(300);
    assert_eq!(
        diagnostic_opens(&rig),
        1,
        "not before the new capture epoch"
    );
    // The device announces its new capture epoch after the unmute (the simulated
    // device starts over, so its reference gets a new epoch too).
    rig.device = crate::sim::SimDevice::new(8, 10);
    for body in rig.device.stream_opens() {
        let open =
            echomuse_protocol::streams::StreamOpen::parse(&body).expect("a stream.open body");
        let envelope = rig.envelope(echomuse_protocol::DeviceMessage::StreamOpen(open), 0);
        rig.handle(Input::Control(envelope));
    }
    rig.silence(300);
    assert!(rig.violations.is_empty(), "{:?}", rig.violations);
    assert_eq!(
        diagnostic_opens(&rig),
        2,
        "reopened on the new capture epoch"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_subscription_is_not_asked_for_again_in_that_epoch() {
    let mut rig = Rig::new(Options::default());
    rig.silence(500);
    rig.auto_ack = false;
    rig.handle(Input::Diagnostic(true));
    let id = rig.unacked.pop().expect("the open waits for its ack");
    rig.handle(Input::Ack {
        id,
        outcome: echomuse_link_ack(),
    });
    rig.auto_ack = true;
    rig.silence(1_000);
    assert_eq!(diagnostic_opens(&rig), 1);
    assert!(!rig.voice.diagnostic_active());
}

fn turn_opens(rig: &Rig) -> usize {
    rig.uplink_commands()
        .iter()
        .filter(|command| {
            matches!(command, UplinkCommand::Open(open)
                if serde_json::to_value(open).expect("serializes")["reason"] == "turn")
        })
        .count()
}

#[tokio::test(start_paused = true)]
async fn a_button_turn_is_refused_while_a_recording_mode_runs() {
    // Without a recording mode, a tap starts a button turn.
    let mut rig = Rig::new(Options::default());
    rig.silence(500);
    let tap = rig.device.tap(rig.device.captured(), 1);
    rig.control(echomuse_protocol::MessageType::ButtonAction, tap, 0);
    assert_eq!(turn_opens(&rig), 1, "a tap opens a button turn");

    // With one armed, the same tap is refused: no turn, the error cue instead.
    let mut rig = Rig::new(Options::default());
    rig.silence(500);
    rig.handle(Input::Diagnostic(true));
    let tap = rig.device.tap(rig.device.captured(), 1);
    rig.control(echomuse_protocol::MessageType::ButtonAction, tap, 0);
    assert_eq!(turn_opens(&rig), 0, "no button turn while recording");
    assert!(rig.voice.turn.is_none());
}

fn echomuse_link_ack() -> echomuse_playback::AckOutcome {
    echomuse_playback::AckOutcome::Rejected {
        code: Some("invalid".to_owned()),
    }
}
