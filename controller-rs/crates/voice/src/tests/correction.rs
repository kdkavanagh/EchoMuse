//! Transcript correction as the machine uses it: the turn waits for the decision, sends
//! what it says, and records the decision in `turns.stt_fix`; a check that runs on beside
//! the turn hands its row to the records service, which waits for the record.

use echomuse_core::services::{MediaUrl, RunEvent, Transcript};
use echomuse_records::{JsonObject, SttFix};

use super::rig::{Options, Rig};
use super::{command, intent_end, wake};
use crate::{Correction, HaReply, HaRequest};

fn fix(text: &str) -> SttFix {
    SttFix(JsonObject::parse(text).expect("an object"))
}

/// A wake turn whose transcript is `heard`; returns the correction request's call.
fn heard(rig: &mut Rig, heard: &str) -> crate::CallId {
    rig.auto_correct = false;
    wake(rig, "c1");
    let stt = command(rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: heard.to_owned(),
        })),
    );
    let (call, request) = rig.corrections.pop().expect("correction was asked");
    assert_eq!(request.heard, heard);
    assert_eq!(request.wake_phrase.as_deref(), Some("ophelia"));
    assert!(!request.contextual);
    assert!(
        !rig.ha
            .iter()
            .any(|(_, r)| matches!(r, HaRequest::IntentTts { .. })),
        "the turn waits for the decision"
    );
    call
}

fn answer(rig: &mut Rig, intent: crate::CallId) {
    answer_asking(rig, intent, false);
}

fn answer_asking(rig: &mut Rig, intent: crate::CallId, asks: bool) {
    rig.reply(intent, HaReply::IntentDispatched(Ok(())));
    rig.reply(
        intent,
        HaReply::IntentEvent(intent_end("Which room?", asks)),
    );
    rig.reply(
        intent,
        HaReply::IntentEvent(RunEvent::TtsReady {
            url: MediaUrl("http://ha/api/tts_proxy/a.wav".to_owned()),
            streamed: false,
        }),
    );
    rig.play_out(0.5);
    rig.reply(intent, HaReply::IntentEvent(RunEvent::Ended));
}

#[tokio::test(start_paused = true)]
async fn an_applied_correction_is_what_ha_receives_and_is_recorded() {
    let mut rig = Rig::new(Options::default());
    let call = heard(&mut rig, "turn on the kitchen lite");
    let record = fix(r#"{"mode":"on","reason":"applied","applied":true}"#);
    rig.handle(crate::Input::Corrected {
        call,
        correction: Correction {
            send: "turn on the kitchen light".to_owned(),
            applied: true,
            record: Some(record.clone()),
            record_follows: false,
        },
    });
    let intent = rig.take_ha(
        "intent",
        |r| matches!(r, HaRequest::IntentTts { text, .. } if text == "turn on the kitchen light"),
    );
    answer(&mut rig, intent);
    let row = rig.turn_rows().pop().expect("persisted").clone();
    assert_eq!(row.stt_text.as_deref(), Some("turn on the kitchen lite"));
    assert_eq!(row.stt_fix, Some(record));
}

/// The fix call the turn row in `rig` was handed over with.
fn fix_follows(rig: &Rig) -> Option<crate::CallId> {
    rig.persisted
        .iter()
        .find_map(|request| match request {
            crate::PersistRequest::Turn { fix_follows, .. } => Some(*fix_follows),
            _ => None,
        })
        .expect("a turn row")
}

#[tokio::test(start_paused = true)]
async fn a_shadow_check_hands_its_row_over_to_wait_for_the_record() {
    let mut rig = Rig::new(Options::default());
    let call = heard(&mut rig, "turn on the kitchen lite");
    rig.handle(crate::Input::Corrected {
        call,
        correction: Correction {
            send: "turn on the kitchen lite".to_owned(),
            applied: false,
            record: None,
            record_follows: true,
        },
    });
    let intent = rig.take_ha(
        "intent",
        |r| matches!(r, HaRequest::IntentTts { text, .. } if text == "turn on the kitchen lite"),
    );
    answer(&mut rig, intent);
    // The row goes to the records service at once, naming the check whose record it
    // waits for: nothing of it stays with the session.
    let row = rig.turn_rows().pop().expect("handed over").clone();
    assert_eq!(row.stt_fix, None);
    assert_eq!(fix_follows(&rig), Some(call));
}

#[tokio::test(start_paused = true)]
async fn a_reply_to_a_question_whose_row_waits_for_its_record_is_sent_as_its_continuation() {
    let mut rig = Rig::new(Options::default());
    let call = heard(&mut rig, "turn on the light");
    rig.handle(crate::Input::Corrected {
        call,
        correction: Correction {
            send: "turn on the light".to_owned(),
            applied: false,
            record: None,
            record_follows: true,
        },
    });
    let intent = rig.take_ha("intent", |r| matches!(r, HaRequest::IntentTts { .. }));
    answer_asking(&mut rig, intent, true);
    let question = rig
        .turn_rows()
        .first()
        .copied()
        .expect("handed over")
        .clone();
    assert_eq!(
        question.continuation,
        Some(echomuse_records::vocab::Continuation::Pending)
    );
    assert_eq!(fix_follows(&rig), Some(call));
    // The answer comes in the reply window; the records service puts its continuation
    // into the held row (tested with the service).
    let reply = rig.words(&["kitchen"]);
    rig.hear(&reply);
    rig.silence(300);
    let question_uuid = match question.key {
        echomuse_records::TurnKey::Turn(uuid) => uuid,
        other => panic!("a turn key: {other:?}"),
    };
    assert!(
        rig.persisted.iter().any(|r| matches!(r,
            crate::PersistRequest::Continuation { turn_uuid, end }
                if *turn_uuid == question_uuid
                    && *end == echomuse_records::vocab::ContinuationEnd::FollowUp(
                        echomuse_records::vocab::FollowUp::Answered))),
        "log:\n{}",
        rig.log.join("\n")
    );
}
