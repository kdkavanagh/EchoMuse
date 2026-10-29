//! Timer requests the controller answers itself: "cancel the timer" and "cancel all
//! timers", decided against HA's `HassTimerStatus` and carried out with HA's cancel
//! intents; every other timer request goes to HA. Alarm and alert lines are the deployed
//! controller's.

use echomuse_core::services::{
    DirectIntent, EffectOutcome, HaDeviceId, HaError, HaTimer, MediaUrl, NotDispatched,
    TimerSelector, TimerStatus, Transcript, UnknownCause,
};
use echomuse_records::vocab::{Continuation, TerminalReason, TurnOutcome};

use super::rig::{Options, Rig};
use super::{command, wake};
use crate::{CallId, HaReply, HaRequest};

const HERE: &str = "ha-dev-1";

fn timer(id: &str, name: Option<&str>, minutes: u32, device: &str) -> HaTimer {
    HaTimer {
        id: id.to_owned(),
        name: name.map(str::to_owned),
        ha_device: Some(HaDeviceId(device.to_owned())),
        start_hours: 0,
        start_minutes: minutes,
        start_seconds: 0,
        is_active: true,
    }
}

/// A wake, then a command whose transcript HA gives as `text`.
fn heard(rig: &mut Rig, text: &str) {
    wake(rig, "c1");
    let stt = command(rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: format!("Ophelia, {text}"),
        })),
    );
}

/// A reply in the open window, transcribed as `text`.
fn replied(rig: &mut Rig, text: &str) {
    rig.silence(400);
    let stt = command(rig, &["turn", "on", "the", "kitchen", "light"]);
    rig.reply(
        stt,
        HaReply::Stt(Ok(Transcript {
            text: text.to_owned(),
        })),
    );
}

fn status_call(rig: &mut Rig) -> CallId {
    rig.take_ha(
        "timer status",
        |r| matches!(r, HaRequest::TimerStatus { ha_device } if *ha_device == Some(HaDeviceId(HERE.to_owned()))),
    )
}

fn list(rig: &mut Rig, timers: Vec<HaTimer>) {
    let call = status_call(rig);
    rig.reply(
        call,
        HaReply::TimerStatus(Ok(TimerStatus {
            timers,
            response_type: "action_done".to_owned(),
        })),
    );
}

/// The next cancel intent sent to HA.
fn cancel(rig: &mut Rig) -> (CallId, DirectIntent) {
    let index = rig
        .ha
        .iter()
        .position(|(_, r)| matches!(r, HaRequest::CallIntent { .. }))
        .unwrap_or_else(|| panic!("no cancel; log:\n{}", rig.log.join("\n")));
    match rig.ha.remove(index) {
        (call, HaRequest::CallIntent { intent, ha_device }) => {
            assert_eq!(ha_device, Some(HaDeviceId(HERE.to_owned())));
            (call, intent)
        }
        _ => unreachable!(),
    }
}

/// The line the controller says next, played to its end.
fn said(rig: &mut Rig) -> String {
    let index = rig
        .ha
        .iter()
        .position(|(_, r)| matches!(r, HaRequest::Tts { .. }))
        .unwrap_or_else(|| panic!("nothing said; log:\n{}", rig.log.join("\n")));
    let (call, request) = rig.ha.remove(index);
    let HaRequest::Tts { text, .. } = request else {
        unreachable!()
    };
    rig.reply(
        call,
        HaReply::Tts(Ok(MediaUrl("http://ha/api/tts_proxy/line.wav".to_owned()))),
    );
    rig.play_out(1.0);
    text
}

fn minutes(n: u32) -> DirectIntent {
    DirectIntent::CancelTimer(TimerSelector {
        start_minutes: Some(n),
        ..TimerSelector::default()
    })
}

fn no_more_ha(rig: &Rig) {
    assert!(
        !rig.ha.iter().any(|(_, r)| matches!(
            r,
            HaRequest::CallIntent { .. }
                | HaRequest::IntentTts { .. }
                | HaRequest::TimerStatus { .. }
        )),
        "{:?}",
        rig.ha
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_the_timer_cancels_this_speakers_only_timer_and_names_it() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer.");
    // Another speaker's timer is left alone.
    list(
        &mut rig,
        vec![timer("o", None, 5, "office"), timer("a", None, 2, HERE)],
    );
    let (call, intent) = cancel(&mut rig);
    assert_eq!(intent, minutes(2));
    rig.reply(call, HaReply::CallIntent(EffectOutcome::Applied(())));
    assert_eq!(said(&mut rig), "2 minute timer cancelled.");
    no_more_ha(&rig);
    let rows = rig.turn_rows();
    let row = rows.last().expect("persisted");
    assert_eq!(row.outcome, Some(TurnOutcome::Timer));
    assert_eq!(row.terminal_reason, TerminalReason::Completed);
    assert_eq!(
        row.response_text.as_deref(),
        Some("2 minute timer cancelled.")
    );
    // intent_ms is HA's timer list; intent_local is not reported for the controller's own
    // answer.
    let intent = row.intent.expect("the timer list's time");
    assert_eq!(intent.local, None);
    assert_eq!(
        row.response_type.as_ref().map(|t| t.as_str()),
        Some("action_done")
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_the_timer_with_none_running_says_so() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    list(&mut rig, vec![timer("o", None, 5, "office")]);
    assert_eq!(said(&mut rig), "You don't have any timers.");
    no_more_ha(&rig);
    assert_eq!(
        rig.turn_rows().last().unwrap().outcome,
        Some(TurnOutcome::Timer)
    );
}

#[tokio::test(start_paused = true)]
async fn several_timers_ask_which_and_the_answer_cancels_that_one() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    let five = timer("a", None, 5, HERE);
    let pasta = timer("b", Some("pasta"), 10, HERE);
    list(&mut rig, vec![five.clone(), pasta.clone()]);
    assert_eq!(
        said(&mut rig),
        "Which one? Your 5 minute timer or your 10 minute pasta timer?"
    );
    no_more_ha(&rig);
    let rows = rig.turn_rows();
    let question = rows.last().unwrap();
    assert_eq!(question.outcome, Some(TurnOutcome::Clarification));
    assert_eq!(question.continuation, Some(Continuation::Pending));
    let question = question_uuid(question);

    // The answer, without the wake word: the timers are listed again, and that one goes.
    replied(&mut rig, "The pasta one.");
    list(&mut rig, vec![five, pasta]);
    let (call, intent) = cancel(&mut rig);
    assert_eq!(
        intent,
        DirectIntent::CancelTimer(TimerSelector {
            name: Some("pasta".to_owned()),
            start_minutes: Some(10),
            ..TimerSelector::default()
        })
    );
    rig.reply(call, HaReply::CallIntent(EffectOutcome::Applied(())));
    assert_eq!(said(&mut rig), "10 minute pasta timer cancelled.");
    let rows = rig.turn_rows();
    let answer = rows.last().unwrap();
    assert_eq!(answer.outcome, Some(TurnOutcome::Timer));
    assert_eq!(answer.reply_to, Some(question));
}

fn question_uuid(row: &echomuse_records::TurnRecord) -> uuid::Uuid {
    match row.key {
        echomuse_records::TurnKey::Turn(uuid) => uuid,
        _ => panic!("not a turn"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_unclear_answer_is_asked_again_once_then_left() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    list(
        &mut rig,
        vec![timer("a", None, 5, HERE), timer("b", None, 10, HERE)],
    );
    said(&mut rig);
    replied(&mut rig, "the purple one");
    assert_eq!(
        said(&mut rig),
        "Which timer? Say its length, like the 5 minute timer, or say all of them."
    );
    replied(&mut rig, "the purple one");
    assert_eq!(said(&mut rig), "Okay, I left it.");
    no_more_ha(&rig);
    assert_eq!(
        rig.turn_rows().last().unwrap().outcome,
        Some(TurnOutcome::Clarification)
    );
}

#[tokio::test(start_paused = true)]
async fn a_chosen_timer_that_finished_meanwhile_is_not_cancelled() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    let five = timer("a", None, 5, HERE);
    let ten = timer("b", None, 10, HERE);
    list(&mut rig, vec![five, ten.clone()]);
    said(&mut rig);
    replied(&mut rig, "the first one");
    list(&mut rig, vec![ten]);
    assert_eq!(said(&mut rig), "Your 5 minute timer already finished.");
    no_more_ha(&rig);
}

#[tokio::test(start_paused = true)]
async fn two_timers_alike_cannot_be_told_apart() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    let a = timer("a", None, 10, HERE);
    let b = timer("b", None, 10, HERE);
    list(&mut rig, vec![a.clone(), b.clone()]);
    said(&mut rig);
    replied(&mut rig, "the second one");
    list(&mut rig, vec![a, b]);
    assert_eq!(
        said(&mut rig),
        "You have 2 10 minute timers, and I can't tell them apart. Say cancel all timers to \
         cancel them."
    );
    no_more_ha(&rig);
}

#[tokio::test(start_paused = true)]
async fn cancel_all_timers_when_every_timer_is_this_speakers_is_one_request() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel all timers");
    list(
        &mut rig,
        vec![timer("a", None, 5, HERE), timer("b", None, 10, HERE)],
    );
    let (call, intent) = cancel(&mut rig);
    assert_eq!(intent, DirectIntent::CancelAllTimers);
    rig.reply(call, HaReply::CallIntent(EffectOutcome::Applied(())));
    assert_eq!(said(&mut rig), "5 minute and 10 minute timers cancelled.");
    no_more_ha(&rig);
}

#[tokio::test(start_paused = true)]
async fn cancel_all_beside_another_speakers_timer_goes_one_at_a_time() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel all my timers");
    list(
        &mut rig,
        vec![
            timer("a", None, 5, HERE),
            timer("o", None, 7, "office"),
            timer("b", None, 10, HERE),
        ],
    );
    let (call, intent) = cancel(&mut rig);
    assert_eq!(intent, minutes(5));
    rig.reply(call, HaReply::CallIntent(EffectOutcome::Applied(())));
    let (call, intent) = cancel(&mut rig);
    assert_eq!(intent, minutes(10));
    // HA refuses the second (it just finished): it is counted, not re-sent.
    rig.reply(
        call,
        HaReply::CallIntent(EffectOutcome::Rejected {
            code: "no_match".to_owned(),
            message: "Timer not found".to_owned(),
        }),
    );
    assert_eq!(
        said(&mut rig),
        "5 minute timer cancelled. 1 could not be cancelled."
    );
    no_more_ha(&rig);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_whose_outcome_is_unknown_is_never_resent() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel all timers");
    list(
        &mut rig,
        vec![
            timer("a", None, 5, HERE),
            timer("o", None, 7, "office"),
            timer("b", None, 10, HERE),
        ],
    );
    let (call, _) = cancel(&mut rig);
    rig.reply(
        call,
        HaReply::CallIntent(EffectOutcome::Unknown(UnknownCause::Timeout)),
    );
    assert_eq!(said(&mut rig), "I'm not sure that worked.");
    no_more_ha(&rig);
    let rows = rig.turn_rows();
    assert_eq!(
        rows.last().unwrap().terminal_reason,
        TerminalReason::OutcomeUnknown
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_single_cancel_says_it_could_not() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer");
    list(&mut rig, vec![timer("a", None, 5, HERE)]);
    let (call, _) = cancel(&mut rig);
    rig.reply(
        call,
        HaReply::CallIntent(EffectOutcome::NotDispatched(NotDispatched::Unavailable)),
    );
    assert_eq!(said(&mut rig), "I couldn't cancel that timer.");
}

#[tokio::test(start_paused = true)]
async fn when_ha_cannot_list_its_timers_the_cancel_goes_to_ha() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "cancel the timer.");
    let call = status_call(&mut rig);
    rig.reply(
        call,
        HaReply::TimerStatus(Err(HaError::BadResponse("no slots".to_owned()))),
    );
    rig.take_ha(
        "the intent run",
        |r| matches!(r, HaRequest::IntentTts { text, .. } if text == "cancel the timer."),
    );
}

#[tokio::test(start_paused = true)]
async fn every_other_timer_request_goes_to_ha() {
    for text in [
        "how much time is left",
        "set a ten second timer.",
        "cancel the pasta timer",
        "pause the timer",
    ] {
        let mut rig = Rig::new(Options::default());
        heard(&mut rig, text);
        rig.take_ha(
            "the intent run",
            |r| matches!(r, HaRequest::IntentTts { text: sent, .. } if sent == text),
        );
        assert!(
            !rig.ha
                .iter()
                .any(|(_, r)| matches!(r, HaRequest::TimerStatus { .. })),
            "{text}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn alarm_questions_that_cannot_be_answered_say_so() {
    let mut rig = Rig::new(Options::default());
    heard(&mut rig, "what alarms do I have");
    let index = rig
        .alerts
        .iter()
        .position(|(_, r)| matches!(r, crate::AlertRequest::ListAlarms))
        .expect("the alarm list was asked for");
    let (call, _) = rig.alerts.remove(index);
    rig.handle(crate::Input::Alert {
        call,
        reply: crate::AlertReply::ListAlarms(Err(echomuse_alerts::ServiceStopped)),
    });
    assert_eq!(said(&mut rig), "I couldn't check your alarms.");
}
