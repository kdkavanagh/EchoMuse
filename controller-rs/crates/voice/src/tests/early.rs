//! The early answer: speech that starts while the controller's own question still plays
//! cuts the question off and becomes the reply turn at once.

use echomuse_core::services::{MediaUrl, Transcript};
use echomuse_playback::Command;
use echomuse_protocol::MessageType;
use echomuse_records::vocab::{Continuation, TerminalReason, TriggerType};

use super::rig::{Options, Rig};
use super::{command, wake};
use crate::{DecodeCommand, HaReply, HaRequest, Input, Outbound};

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

#[tokio::test(start_paused = true)]
async fn an_answer_during_the_question_cuts_it_off_and_becomes_the_reply() {
    let mut rig = Rig::new(Options::default());
    ask_and_start_playing(&mut rig);
    assert!(
        rig.voice.question_watch.is_some(),
        "the question is watched"
    );
    rig.silence(400);
    let cancelled_before = cancels(&rig);
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
    // The reply turn is open on the watch's subscription, from 300 ms before the onset.
    let turn = rig.voice.turn.as_mut().expect("the reply turn is open");
    assert_eq!(turn.trigger, TriggerType::Reply);
    assert_eq!(turn.reply_to, Some(question_uuid));
    let policy = turn.policy.as_ref().expect("the reply is listening");
    let (start, trigger) = (policy.trace().start.0, policy.trace().trigger.0);
    assert_eq!(policy.trace().guard_until, None, "no opening guard");
    // The onset is the first cell of the qualifying run, inside the answer: early enough
    // that 480 ms of it followed before the answer stopped.
    assert!(
        trigger >= answer_at && trigger + 480 * 16 <= answer_at + 1_200 * 16,
        "the trigger is the answer's onset ({trigger} vs answer at {answer_at})"
    );
    assert_eq!(start, (trigger - 4_800) / 512 * 512);
    // The reply runs to a commit like any reply.
    rig.silence(2_000);
    assert!(
        rig.ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::Pipeline { .. } | HaRequest::Stt { .. })),
        "the reply committed; log:\n{}",
        rig.log.join("\n")
    );
}

#[tokio::test(start_paused = true)]
async fn quiet_while_the_question_plays_leaves_it_playing() {
    let mut rig = Rig::new(Options::default());
    ask_and_start_playing(&mut rig);
    let cancelled_before = cancels(&rig);
    rig.silence(1_500);
    assert_eq!(cancels(&rig), cancelled_before, "the question plays on");
    assert!(rig.voice.question_watch.is_some());
    assert!(rig.turn_rows().is_empty() || rig.voice.turn.is_some());
}
