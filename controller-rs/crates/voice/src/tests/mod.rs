//! The voice machine's behaviour over synthetic evidence: a simulated device uploading
//! real frames, the real speech-worker code over tone stand-in models, and service
//! answers given by hand.

mod afe;
mod correction;
mod diagnostic;
mod early;
mod evidence;
mod features;
mod latency;
mod pause_server;
pub(crate) mod rig;
mod timers;

use std::time::Duration;

use echomuse_core::services::{
    ConversationId, IntentEnd, MediaUrl, PipelineId, RunEvent, Transcript,
};
use echomuse_playback::Command;
use echomuse_protocol::{MessageType, vocab::AckStatus};
use echomuse_records::vocab::{TerminalReason, TriggerType, TurnOutcome};
use echomuse_uplink::UplinkCommand;

use rig::{Options, Rig};

use crate::{HaReply, HaRequest, Input, Outbound};

const SUPPORT_START: u64 = 9_600;
/// "ophelia" is heard from 1.0 s to 1.4 s; its candidate opens at the end of the gap.
const OPEN: u64 = 26_400;

/// One second of quiet, then the wake word; returns once the candidate is open.
fn wake(rig: &mut Rig, id: &str) {
    rig.silence(1_000);
    let word = rig.words(&["ophelia"]);
    rig.hear(&word);
    assert_eq!(rig.device.captured(), OPEN);
    rig.wake(id, SUPPORT_START, OPEN, false);
}

/// Answers the recognizer's questions (if any) as "complete".
fn answer_recognizer(rig: &mut Rig) {
    while let Some(index) = rig
        .ha
        .iter()
        .position(|(_, r)| matches!(r, HaRequest::Recognize { .. }))
    {
        let (call, _) = rig.ha.remove(index);
        rig.reply(
            call,
            HaReply::Recognize(Ok(vec![Some(
                echomuse_core::services::SentenceRecognition {
                    matched: true,
                    unfilled_slots: false,
                    source: Some("builtin".to_owned()),
                    template: String::new(),
                    intent: Some("HassTurnOn".to_owned()),
                    details: Default::default(),
                    unmatched: Default::default(),
                    targets: Vec::new(),
                },
            )])),
        );
    }
}

/// Says the command and waits for the commit; returns the STT request's call after the
/// pipeline was named.
fn command(rig: &mut Rig, words: &[&str]) -> crate::CallId {
    let speech = rig.words(words);
    rig.hear(&speech);
    for _ in 0..40 {
        answer_recognizer(rig);
        if rig
            .ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. }))
        {
            break;
        }
        rig.silence(80);
    }
    let pipeline = rig.take_ha("pipeline", |r| matches!(r, HaRequest::Pipeline { .. }));
    rig.reply(
        pipeline,
        HaReply::Pipeline(Ok(PipelineId("pipeline-1".to_owned()))),
    );
    rig.take_ha("stt", |r| matches!(r, HaRequest::Stt { .. }))
}

fn intent_end(speech: &str, continue_conversation: bool) -> RunEvent {
    RunEvent::IntentEnded(IntentEnd {
        speech: speech.to_owned(),
        conversation: Some(ConversationId("conversation-1".to_owned())),
        continue_conversation,
        response_type: Some("action_done".to_owned()),
        processed_locally: Some(true),
    })
}

#[tokio::test(start_paused = true)]
async fn an_idle_wake_becomes_a_turn_that_ha_answers() {
    let mut rig = Rig::new(Options::default());
    wake(&mut rig, "c1");
    // The candidate was accepted at once (the device was quiet) and converted to a turn.
    assert!(rig.sent.iter().any(|(body, _)| matches!(body,
        Outbound::CandidateAck(ack) if ack.status == AckStatus::Accepted)));
    assert!(
        rig.uplink_commands().iter().any(|c| matches!(c,
            UplinkCommand::Renew(renew) if renew.conversion.is_some())),
        "the candidate became a turn subscription"
    );
    let stt = command(&mut rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: "Ophelia, turn on the kitchen light.".to_owned(),
        })),
    );
    let intent = rig.take_ha(
        "intent",
        |r| matches!(r, HaRequest::IntentTts { text, .. } if text == "turn on the kitchen light."),
    );
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(
        intent,
        HaReply::IntentEvent(intent_end("Turned on the light.", false)),
    );
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/a.wav".to_owned()),
            streamed: false,
        }),
    );
    // render.start goes out first; frames follow only after the device accepted it.
    let start = rig
        .requests
        .iter()
        .position(|(_, body, _)| matches!(body, Outbound::Playback(Command::RenderStart(_))))
        .expect("render.start sent");
    assert_eq!(rig.frames, 0);
    rig.play_out(1.0);
    assert!(rig.frames > 0, "frames after render.start ({start})");
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
    let rows = rig.turn_rows();
    let row = rows.last().expect("the turn was persisted");
    assert_eq!(row.terminal_reason, TerminalReason::Completed);
    assert_eq!(row.trigger, TriggerType::Wake);
    assert_eq!(row.outcome, Some(TurnOutcome::Ha));
    assert_eq!(row.stt_text.as_deref(), Some("turn on the kitchen light."));
    assert!(row.decision_trace.is_some());
    assert!(row.commit.is_some());
    // Exactly one intent run was sent.
    assert!(
        !rig.ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::IntentTts { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn a_wake_while_muted_is_refused_and_recorded() {
    let mut rig = Rig::new(Options::default());
    rig.silence(1_000);
    rig.handle(Input::Privacy {
        muted: true,
        physical_seq: Some(1),
    });
    rig.hear(&rig.words(&["ophelia"]));
    rig.wake("c1", SUPPORT_START, OPEN, false);
    assert!(rig.sent.iter().any(|(body, _)| matches!(body,
        Outbound::CandidateAck(ack)
            if ack.status == AckStatus::Rejected && ack.error.as_deref() == Some("muted"))));
    let rows = rig.turn_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].terminal_reason, TerminalReason::Muted);
    let _ = MessageType::WakeCandidate;
    let _ = Duration::ZERO;
}

#[tokio::test(start_paused = true)]
async fn a_wake_word_the_device_played_itself_is_refused_as_self_output() {
    let mut rig = Rig::new(Options::default());
    // A device that has been running: the reference reaches back further than the
    // windows the judgement reads (the stream's first window ends 1.4 s in).
    rig.silence(3_000);
    let word = rig.words(&["ophelia"]);
    rig.play(&word);
    // The word's tone starts at 48,000; support and open as for the idle wake above.
    rig.wake("c1", 48_000 - 6_400, 48_000 + 10_400, true);
    rig.silence(1_000);
    let rows = rig.turn_rows();
    assert_eq!(rows.len(), 1, "log:\n{}", rig.log.join("\n"));
    assert_eq!(
        rows[0].terminal_reason,
        TerminalReason::SelfOutput,
        "{:?}\nlog:\n{}",
        rows[0]
            .decision_trace
            .as_ref()
            .map(|t| t.as_str().to_owned()),
        rig.log.join("\n")
    );
    assert!(rig.uplink_commands().iter().any(|c| matches!(c,
        UplinkCommand::Close(close) if close.reason == echomuse_uplink::CloseReason::Rejected)));
}
