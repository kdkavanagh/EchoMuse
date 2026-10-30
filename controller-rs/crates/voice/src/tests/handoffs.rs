//! What passes between the wake handling, the conversation and the device's output: an
//! accepted candidate's queued evidence, a reply window's question taken over by a wake,
//! and long commands that keep their whole span.

use echomuse_core::services::{ConversationId, MediaUrl, PipelineId, RunEvent, Transcript};
use echomuse_presentation::phase::ActorState;
use echomuse_records::vocab::{ContinuationEnd, FollowUp, TriggerType};

use super::rig::{Options, Rig};
use super::{OPEN, SUPPORT_START, command, intent_end, wake};
use crate::{HaReply, HaRequest, PersistRequest, evidence::Evidence};

/// Listens in 80 ms of silence until the state is `state`; `false` within `blocks`.
fn until(rig: &mut Rig, state: ActorState, blocks: usize) -> bool {
    for _ in 0..blocks {
        if rig.voice.status().state == state {
            return true;
        }
        rig.silence(80);
    }
    rig.voice.status().state == state
}

#[tokio::test(start_paused = true)]
async fn results_queued_for_a_candidate_reach_the_turn_it_became() {
    let mut rig = Rig::new(Options::default());
    rig.voice.set_recording(true);
    rig.silence(1_000);
    let word = rig.words(&["ophelia"]);
    rig.hear(&word);
    // The device made sound, so the candidate waits for its verifier; the microphone
    // worker falls behind meanwhile and its results queue up for the candidate's stream.
    rig.hold_mic_results = true;
    rig.wake("c1", SUPPORT_START, OPEN, true);
    assert_eq!(rig.voice.status().state, ActorState::Armed);
    assert!(
        until(&mut rig, ActorState::Listening, 20),
        "the candidate became a turn; log:\n{}",
        rig.log.join("\n")
    );
    let stale = rig.voice.status().counters.stale_results;
    rig.release_mic_results();
    // Every queued result was the turn's: none was dropped as stale, and the VAD of the
    // candidate's audio reached the turn's policy.
    assert_eq!(rig.voice.status().counters.stale_results, stale);
    assert!(
        rig.evidence.iter().any(|(_, e)| matches!(e,
            Evidence::Vad { first, .. } if first.start().0 < OPEN)),
        "the candidate's VAD reached the turn"
    );
    // The turn hears its command through to the commit.
    let stt = command(&mut rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: "Ophelia, turn on the kitchen light.".to_owned(),
        })),
    );
    rig.take_ha(
        "intent",
        |r| matches!(r, HaRequest::IntentTts { text, .. } if text == "turn on the kitchen light."),
    );
}

#[tokio::test(start_paused = true)]
async fn a_wake_during_a_reply_window_carries_its_question_once() {
    let mut rig = Rig::new(Options::default());
    wake(&mut rig, "c1");
    let stt = command(&mut rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: "Ophelia, turn on the kitchen light.".to_owned(),
        })),
    );
    let intent = rig.take_ha("intent", |r| matches!(r, HaRequest::IntentTts { .. }));
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(
        intent,
        HaReply::IntentEvent(intent_end("Which room?", true)),
    );
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/q.wav".to_owned()),
            streamed: false,
        }),
    );
    // The room says the wake word while the question plays; the window opens when the
    // question drains, and no live audio has reached it when the candidate arrives.
    let word = rig.words(&["ophelia"]);
    let word_at = rig.device.captured();
    rig.hear(&word);
    let open = rig.device.captured();
    rig.play_out(1.0);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
    let rows = rig.turn_rows();
    let question = rows.last().expect("the question's row");
    let echomuse_records::TurnKey::Turn(question) = question.key else {
        panic!("the question is a turn");
    };
    assert_eq!(
        rig.voice.status().state,
        ActorState::Armed,
        "the window is open"
    );
    rig.wake("c2", word_at - 6_400, open, false);

    let continuations = |rig: &Rig| -> Vec<ContinuationEnd> {
        rig.persisted
            .iter()
            .filter_map(|request| match request {
                PersistRequest::Continuation { turn_uuid, end } if *turn_uuid == question => {
                    Some(*end)
                }
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        continuations(&rig),
        [ContinuationEnd::FollowUp(FollowUp::Wake)]
    );
    // The wake's turn answers the question: HA continues the conversation.
    let stt = command(&mut rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: "Ophelia, the kitchen.".to_owned(),
        })),
    );
    let intent = rig.take_ha("the reply's intent", |r| {
        matches!(r, HaRequest::IntentTts { conversation, .. }
            if *conversation == Some(ConversationId("conversation-1".to_owned())))
    });
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(intent, HaReply::IntentEvent(intent_end("Done.", false)));
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/a.wav".to_owned()),
            streamed: false,
        }),
    );
    rig.play_out(1.0);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
    let rows = rig.turn_rows();
    let reply = rows.last().expect("the wake turn's row");
    assert_eq!(reply.trigger, TriggerType::Wake);
    assert_eq!(reply.reply_to, Some(question));
    // The question was resolved once, by the wake.
    assert_eq!(
        continuations(&rig),
        [ContinuationEnd::FollowUp(FollowUp::Wake)]
    );
}

/// The rig's words cycled for about `ms` of speech, in an order the local grammar
/// cannot judge (so the turn waits out the long pause).
fn long_command(rig: &Rig, ms: usize) -> Vec<i16> {
    let grammar = echomuse_grammar::Grammar::new().unwrap();
    let candidates: [&[&str]; 3] = [
        &["kitchen", "light", "time"],
        &["light", "kitchen", "on"],
        &["the", "light", "it"],
    ];
    let words: Vec<&str> = candidates
        .iter()
        .map(|words| words.iter().copied().cycle().take(ms / 650).collect())
        .find(|words: &Vec<&str>| {
            grammar.classify(&words.join(" ")) == echomuse_grammar::Completeness::Unknown
        })
        .expect("one order is beyond the grammar");
    rig.words(&words)
}

#[tokio::test(start_paused = true)]
async fn long_commands_reach_stt_whole_within_their_budget() {
    // 8 s within the normal 15 s budget; 20 s with extended utterances (30 s). Both are
    // counted from the wake word and include the pause that ends them.
    for (extended, ms) in [(false, 8_000), (true, 20_000)] {
        let mut settings = echomuse_settings::DeviceSettings::default();
        settings.microphones.extended_utterances = extended;
        let mut rig = Rig::new(Options {
            settings,
            ..Options::default()
        });
        wake(&mut rig, "c1");
        let speech = long_command(&rig, ms);
        rig.hear(&speech);
        for _ in 0..80 {
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
        let audio = rig
            .ha
            .iter()
            .find_map(|(_, r)| match r {
                HaRequest::Stt { audio, .. } => Some(audio.len()),
                _ => None,
            })
            .expect("the STT copy went to HA");
        // The copy runs from before the wake word through the end of the command.
        assert!(
            audio >= speech.len(),
            "extended {extended}: {audio} samples sent for {} spoken",
            speech.len()
        );
    }
}
