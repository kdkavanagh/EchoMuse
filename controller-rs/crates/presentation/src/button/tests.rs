use echomuse_settings::values::MultiTapMs;
use serde_json::json;

use super::*;

const IDLE: ButtonContext = ButtonContext {
    activity: TurnActivity::Idle,
    recording: false,
    speech_available: true,
};

/// A release as the firmware writes it, `mono_ms` on the device's clock.
fn release(seq: u64, held_ms: u64, mono_ms: i64) -> ButtonAction {
    let body = json!({
        "click_type": 138, "button": "Dot", "down": false, "held_ms": held_ms, "muted": false,
        "mono_ns": (mono_ms * 1_000_000).to_string(), "capture_epoch": "7",
        "capture_sample": "160000", "physical_seq": seq, "occurrence_id": null, "handled": null
    });
    ButtonAction::parse(body.as_object().unwrap()).unwrap()
}

fn events(window_ms: u16) -> ButtonSettings {
    ButtonSettings {
        button_single_tap_event: true,
        button_multi_tap_ms: MultiTapMs::new(window_ms).unwrap(),
    }
}

fn act(decision: ButtonDecision) -> Vec<ButtonOutput> {
    match decision {
        ButtonDecision::Act(outputs) => outputs,
        ButtonDecision::Ignored(why) => panic!("ignored: {why:?}"),
    }
}

fn event(e: ButtonEvent) -> ButtonOutput {
    ButtonOutput::Event(e)
}

#[test]
fn parses_the_firmware_body() {
    let body = json!({
        "click_type": 138, "button": "Dot", "down": true, "held_ms": 0, "muted": true,
        "mono_ns": "123456789012", "capture_epoch": "18446744073709551615",
        "capture_sample": "4800", "physical_seq": 13,
        "occurrence_id": "0b9a3c9e-3a55-4d3e-9d6c-5d2d3c1b2a10", "handled": "alert_stopped"
    });
    let action = ButtonAction::parse(body.as_object().unwrap()).unwrap();
    assert_eq!(action.click, ClickType::Dot);
    assert_eq!(action.button, "Dot");
    assert!(action.down && action.muted);
    assert_eq!(action.mono_ns, 123_456_789_012);
    assert_eq!(
        action.capture,
        Some(CapturePosition {
            epoch: u64::MAX,
            sample: 4_800
        })
    );
    assert_eq!(action.physical_seq, 13);
    assert_eq!(action.handled, Some(Reported::Known(Handled::AlertStopped)));

    let volume = json!({
        "click_type": 115, "button": "Volume", "down": false, "held_ms": 0, "muted": false,
        "mono_ns": "5", "capture_epoch": null, "capture_sample": null, "physical_seq": 2,
        "occurrence_id": null, "handled": null
    });
    let volume = ButtonAction::parse(volume.as_object().unwrap()).unwrap();
    assert_eq!(volume.click, ClickType::VolumeUp);
    assert_eq!(volume.capture, None);
    assert_eq!(volume.handled, None);

    for (key, value) in [
        ("capture_epoch", json!(7)),
        ("mono_ns", json!(5)),
        ("physical_seq", json!(-1)),
        ("held_ms", json!("120")),
        ("click_type", json!(70_000)),
        ("handled", json!(true)),
    ] {
        let mut body = body.clone();
        body[key] = value;
        let error = ButtonAction::parse(body.as_object().unwrap()).unwrap_err();
        assert_eq!(error.code, ProtocolErrorCode::MalformedMessage, "{key}");
    }
}

#[test]
fn a_hold_is_750_ms_or_more() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let tap = act(policy.on_action(&release(1, 749, 0), IDLE, now));
    assert_eq!(
        tap,
        [ButtonOutput::StartTurn(ButtonTurn {
            epoch: 7,
            press_sample: 160_000,
            audio_from: 160_000 - 4_800,
            keeps_reply: false
        })]
    );
    let hold = act(policy.on_action(&release(2, 750, 2_000), IDLE, now));
    assert_eq!(hold, [event(ButtonEvent::Long)]);
}

#[test]
fn a_hold_fires_long_even_while_muted_and_during_a_turn() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let mut muted = release(1, 2_000, 0);
    muted.muted = true;
    assert_eq!(
        act(policy.on_action(&muted, IDLE, now)),
        [event(ButtonEvent::Long)]
    );
    let busy = ButtonContext {
        activity: TurnActivity::Turn,
        ..IDLE
    };
    assert_eq!(
        act(policy.on_action(&release(2, 900, 3_000), busy, now)),
        [event(ButtonEvent::Long)]
    );
}

#[test]
fn without_button_hold_every_release_is_a_tap() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(false, events(0));
    let outputs = act(policy.on_action(&release(1, 5_000, 0), IDLE, now));
    assert!(
        matches!(outputs[..], [ButtonOutput::StartTurn(_)]),
        "{outputs:?}"
    );
}

#[test]
fn a_tap_while_muted_is_refused() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, events(0));
    let mut tap = release(1, 100, 0);
    tap.muted = true;
    assert_eq!(
        act(policy.on_action(&tap, IDLE, now)),
        [ButtonOutput::Refused(ButtonRefusal::Muted)]
    );
}

#[test]
fn a_tap_during_a_turn_cancels_it() {
    let now = Instant::now();
    let busy = ButtonContext {
        activity: TurnActivity::Turn,
        ..IDLE
    };
    for settings in [ButtonSettings::default(), events(300)] {
        let mut policy = ButtonPolicy::new(true, settings);
        assert_eq!(
            act(policy.on_action(&release(1, 100, 0), busy, now)),
            [ButtonOutput::CancelTurn]
        );
        assert_eq!(policy.next_deadline(), None);
    }
}

#[test]
fn a_tap_in_a_silent_reply_window_starts_a_turn_that_keeps_the_question() {
    let now = Instant::now();
    let window = ButtonContext {
        activity: TurnActivity::ReplyWindow,
        ..IDLE
    };
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let outputs = act(policy.on_action(&release(1, 100, 0), window, now));
    assert!(
        matches!(
            outputs[..],
            [ButtonOutput::StartTurn(ButtonTurn {
                keeps_reply: true,
                ..
            })]
        ),
        "{outputs:?}"
    );
    let mut eventful = ButtonPolicy::new(true, events(0));
    assert_eq!(
        act(eventful.on_action(&release(1, 100, 0), window, now)),
        [ButtonOutput::CancelTurn]
    );
}

#[test]
fn a_tap_that_cannot_start_a_turn_is_refused() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let recording = ButtonContext {
        recording: true,
        ..IDLE
    };
    assert_eq!(
        act(policy.on_action(&release(1, 100, 0), recording, now)),
        [ButtonOutput::Refused(ButtonRefusal::Recording)]
    );
    let no_speech = ButtonContext {
        speech_available: false,
        ..IDLE
    };
    assert_eq!(
        act(policy.on_action(&release(2, 100, 0), no_speech, now)),
        [ButtonOutput::Refused(ButtonRefusal::SpeechUnavailable)]
    );
    let mut uncaptured = release(3, 100, 0);
    uncaptured.capture = None;
    assert_eq!(
        act(policy.on_action(&uncaptured, IDLE, now)),
        [ButtonOutput::Refused(ButtonRefusal::NoCapture)]
    );
    // The preroll never reaches before the start of the epoch.
    let mut early = release(4, 100, 0);
    early.capture = Some(CapturePosition {
        epoch: 1,
        sample: 1_000,
    });
    assert!(matches!(
        act(policy.on_action(&early, IDLE, now))[..],
        [ButtonOutput::StartTurn(ButtonTurn { audio_from: 0, .. })]
    ));
}

#[test]
fn a_press_that_stopped_an_alert_is_ignored() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let mut stopped = release(1, 100, 0);
    stopped.occurrence_id = Some("0b9a3c9e-3a55-4d3e-9d6c-5d2d3c1b2a10".into());
    stopped.handled = Some(Reported::Known(Handled::AlertStopped));
    assert_eq!(
        policy.on_action(&stopped, IDLE, now),
        ButtonDecision::Ignored(Ignored::HandledByDevice)
    );
    // A hold that stopped one fires nothing either.
    let mut held = release(2, 2_000, 0);
    held.handled = Some(Reported::Unrecognized("alert_snoozed".into()));
    assert_eq!(
        policy.on_action(&held, IDLE, now),
        ButtonDecision::Ignored(Ignored::HandledByDevice)
    );
}

#[test]
fn duplicate_and_older_physical_seq_are_dropped() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    let mut press = release(5, 0, 0);
    press.down = true;
    assert_eq!(
        policy.on_action(&press, IDLE, now),
        ButtonDecision::Ignored(Ignored::Press)
    );
    assert_eq!(
        policy.on_action(&press, IDLE, now),
        ButtonDecision::Ignored(Ignored::Duplicate)
    );
    assert_eq!(
        policy.on_action(&release(4, 100, 0), IDLE, now),
        ButtonDecision::Ignored(Ignored::Duplicate)
    );
    // A privacy change takes a number from the same sequence.
    assert!(policy.accept_physical_seq(6));
    assert!(!policy.accept_physical_seq(6));
    assert_eq!(
        policy.on_action(&release(6, 100, 0), IDLE, now),
        ButtonDecision::Ignored(Ignored::Duplicate)
    );
    assert!(matches!(
        policy.on_action(&release(7, 100, 0), IDLE, now),
        ButtonDecision::Act(_)
    ));
}

#[test]
fn volume_and_mute_buttons_are_the_device_s() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, ButtonSettings::default());
    for (seq, code) in [(1, 115), (2, 114), (3, 113)] {
        let mut action = release(seq, 0, 0);
        action.click = ClickType::from_code(code);
        assert_eq!(
            policy.on_action(&action, IDLE, now),
            ButtonDecision::Ignored(Ignored::LocalButton)
        );
    }
}

#[test]
fn single_tap_event_without_a_window_fires_at_once() {
    let now = Instant::now();
    let mut policy = ButtonPolicy::new(true, events(0));
    assert_eq!(
        act(policy.on_action(&release(1, 100, 0), IDLE, now)),
        [event(ButtonEvent::Single)]
    );
    assert_eq!(policy.next_deadline(), None);
}

#[tokio::test(start_paused = true)]
async fn taps_inside_the_window_are_grouped() {
    let t0 = Instant::now();
    let window = Duration::from_millis(300);
    let mut policy = ButtonPolicy::new(true, events(300));

    // Two taps exactly the window apart on the device's clock: a double.
    assert_eq!(act(policy.on_action(&release(1, 80, 1_000), IDLE, t0)), []);
    assert_eq!(policy.next_deadline(), Some(t0 + window));
    let t1 = t0 + Duration::from_millis(250);
    assert_eq!(act(policy.on_action(&release(2, 80, 1_300), IDLE, t1)), []);
    assert_eq!(policy.next_deadline(), Some(t1 + window));
    assert_eq!(
        policy.on_timer(t1 + window - Duration::from_millis(1)),
        None
    );
    assert_eq!(
        policy.on_timer(t1 + window),
        Some(event(ButtonEvent::Double))
    );
    assert_eq!(policy.next_deadline(), None);

    // One millisecond past the window: the first tap is its own single.
    let t2 = t1 + Duration::from_secs(5);
    act(policy.on_action(&release(3, 80, 10_000), IDLE, t2));
    assert_eq!(
        act(policy.on_action(
            &release(4, 80, 10_301),
            IDLE,
            t2 + Duration::from_millis(200)
        )),
        [event(ButtonEvent::Single)]
    );
    assert_eq!(
        policy.on_timer(t2 + Duration::from_millis(500)),
        Some(event(ButtonEvent::Single))
    );

    // Three taps fire triple at the third, without waiting.
    let t3 = t2 + Duration::from_secs(5);
    act(policy.on_action(&release(5, 80, 20_000), IDLE, t3));
    act(policy.on_action(
        &release(6, 80, 20_200),
        IDLE,
        t3 + Duration::from_millis(200),
    ));
    assert_eq!(
        act(policy.on_action(
            &release(7, 80, 20_400),
            IDLE,
            t3 + Duration::from_millis(400)
        )),
        [event(ButtonEvent::Triple)]
    );
    assert_eq!(policy.next_deadline(), None);
}

#[tokio::test(start_paused = true)]
async fn a_tap_after_the_window_closed_starts_a_new_group() {
    let t0 = Instant::now();
    let mut policy = ButtonPolicy::new(true, events(300));
    act(policy.on_action(&release(1, 80, 0), IDLE, t0));
    // The controller's window passes first, so the group is a single even though the next
    // tap was within the window on the device's clock (it arrived late).
    tokio::time::advance(Duration::from_millis(300)).await;
    assert_eq!(
        policy.on_timer(Instant::now()),
        Some(event(ButtonEvent::Single))
    );
    assert_eq!(
        act(policy.on_action(&release(2, 80, 250), IDLE, Instant::now())),
        []
    );
    // A hold closes a pending group first.
    assert_eq!(
        act(policy.on_action(&release(3, 1_000, 400), IDLE, Instant::now())),
        [event(ButtonEvent::Single), event(ButtonEvent::Long)]
    );
}
