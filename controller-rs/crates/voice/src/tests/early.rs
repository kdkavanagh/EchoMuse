//! The early answer: speech that starts while the controller's own question still plays
//! cuts the question off and becomes the reply turn at once.

use echomuse_core::services::{MediaUrl, Transcript};
use echomuse_playback::Command;
use echomuse_protocol::MessageType;
use echomuse_records::vocab::{Continuation, TerminalReason, TriggerType};

use super::rig::{Options, Rig};
use super::{command, wake};
use crate::{DecodeCommand, HaReply, HaRequest, Input, Outbound, evidence::Evidence};
use echomuse_presentation::phase::ActorState;
use echomuse_uplink::UplinkCommand;

/// Asks "Sorry, I didn't catch that." (an empty transcript) and lets the device start
/// playing it, without finishing it.
fn ask_and_start_playing(rig: &mut Rig) {
    wake(rig, "c1");
    let stt = command(rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: String::new(),
        })),
    );
    let tts = rig.take_ha("tts", |r| matches!(r, HaRequest::Tts { .. }));
    rig.reply(
        tts,
        HaReply::Tts(Ok(MediaUrl("http://ha/api/tts_proxy/q.wav".to_owned()))),
    );
    let Some(DecodeCommand::Start { call, .. }) = rig
        .decodes
        .iter()
        .rev()
        .find(|c| matches!(c, DecodeCommand::Start { .. }))
        .cloned()
    else {
        panic!("the question was not fetched; log:\n{}", rig.log.join("\n"));
    };
    rig.handle(Input::Decode {
        call,
        event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Opened {
            seekable: echomuse_playback::Seekability::Seekable,
        }),
    });
    rig.handle(Input::Decode {
        call,
        event: crate::DecodeReport::Event(echomuse_playback::DecodeEvent::Audio(vec![800; 3_840])),
    });
    let (playback_id, generation) = rig.last_render_start();
    let progress = serde_json::json!({
        "playback_id": playback_id, "event": "start", "submitted_frames": "3840",
        "completed_frames": "0", "mono_ns": "1", "uncertainty_us": 0,
        "timing_quality": "estimated", "reference_coverage": "full",
    });
    rig.control(
        MessageType::RenderProgress,
        progress.as_object().expect("an object").clone(),
        generation,
    );
}

fn cancels(rig: &Rig) -> usize {
    rig.requests
        .iter()
        .filter(|(_, body, _)| matches!(body, Outbound::Playback(Command::RenderCancel(_))))
        .count()
}

/// The reply subscriptions opened so far (reply windows and early-answer watches).
fn reply_opens(rig: &Rig) -> usize {
    rig.uplink_commands()
        .iter()
        .filter(|command| {
            matches!(command, UplinkCommand::Open(open)
                if serde_json::to_value(open).expect("serializes")["reason"] == "reply")
        })
        .count()
}

/// The reply turns' policies as the session recorder saw them open (reopenings aside):
/// each one's start, trigger and reply rules.
fn reply_openings(rig: &Rig) -> Vec<echomuse_turn::TurnSpec> {
    rig.evidence
        .iter()
        .filter_map(|(_, evidence)| match evidence {
            Evidence::TurnOpened {
                cause:
                    echomuse_recorder::TurnCause::Reply {
                        reopened_from: None,
                    },
                spec,
                ..
            } => Some(spec.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_answer_during_the_question_cuts_it_off_and_becomes_the_reply() {
    let mut rig = Rig::new(Options::default());
    rig.voice.set_recording(true);
    ask_and_start_playing(&mut rig);
    let question_playback = rig.last_render_start();
    rig.silence(400);
    let cancelled_before = cancels(&rig);
    let windows_before = reply_opens(&rig);
    // The user answers while the question plays: one long word, louder than the room.
    let answer_at = rig.device.captured();
    let index = rig.models.index("stop").expect("in the vocabulary");
    rig.hear(&crate::speech::testing::tone(index, 1_200, 6_000));

    assert!(
        cancels(&rig) > cancelled_before,
        "the question was cancelled; log:\n{}",
        rig.log.join("\n")
    );
    // The question's row: its own ending, with the reply still to come.
    let rows = rig.turn_rows();
    let question = rows.last().expect("the question's turn was persisted");
    assert_eq!(question.terminal_reason, TerminalReason::EmptyTranscript);
    assert_eq!(question.continuation, Some(Continuation::Pending));
    let echomuse_records::TurnKey::Turn(question_uuid) = question.key else {
        panic!("the question is a turn");
    };
    // The reply turn opened at once, from 300 ms before the onset.
    let openings = reply_openings(&rig);
    let [spec] = openings.as_slice() else {
        panic!("one reply turn opened: {openings:?}");
    };
    let (start, trigger) = (spec.start.0, spec.trigger.0);
    let echomuse_turn::TurnKind::Reply(reply) = spec.kind else {
        panic!("a reply turn: {spec:?}");
    };
    assert_eq!(reply.guard_until, None, "no opening guard");
    // The onset is the first cell of the qualifying run, inside the answer: early enough
    // that 480 ms of it followed before the answer stopped.
    assert!(
        trigger >= answer_at && trigger + 480 * 16 <= answer_at + 1_200 * 16,
        "the trigger is the answer's onset ({trigger} vs answer at {answer_at})"
    );
    assert_eq!(start, (trigger - 4_800) / 512 * 512);
    // The question's playback reports its end late, as drained: it names the question's
    // turn, so it neither ends the reply nor opens another window.
    let (playback_id, generation) = question_playback;
    let finished = serde_json::json!({
        "playback_id": playback_id, "last_completed_frame": "3840",
        "reason": "drained", "timing_quality": "estimated",
    });
    rig.control(
        MessageType::RenderFinished,
        finished.as_object().expect("an object").clone(),
        generation,
    );
    assert!(
        matches!(
            rig.voice.status().state,
            ActorState::Listening | ActorState::EndPending
        ),
        "the reply still listens"
    );
    // The reply runs to a commit like any reply.
    rig.silence(2_000);
    assert!(
        rig.ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. } | HaRequest::Stt { .. })),
        "the reply committed; log:\n{}",
        rig.log.join("\n")
    );
    assert_eq!(reply_opens(&rig), windows_before, "no reply window opened");
    // Its row answers the question.
    rig.handle(Input::SessionLost);
    let rows = rig.turn_rows();
    let reply = rows.last().expect("the reply's turn was persisted");
    assert_eq!(reply.trigger, TriggerType::Reply);
    assert_eq!(reply.reply_to, Some(question_uuid));
}

#[tokio::test(start_paused = true)]
async fn quiet_while_the_question_plays_leaves_it_playing() {
    let mut rig = Rig::new(Options::default());
    ask_and_start_playing(&mut rig);
    let cancelled_before = cancels(&rig);
    rig.silence(1_500);
    assert_eq!(cancels(&rig), cancelled_before, "the question plays on");
}
