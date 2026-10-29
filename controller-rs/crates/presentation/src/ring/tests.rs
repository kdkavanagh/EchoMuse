use echomuse_alerts::wire::{TimerView, TimerViewEntry};
use echomuse_protocol::MessageType;
use echomuse_settings::values::{
    LedColor, LedScene, ListenColor, MeterAttack, MeterCurve, MeterDecay, MeterFloor, MeterGamma,
    MeterRef, ThinkColor,
};
use serde_json::{Value, json};

use super::overlay::{DeviceOverlays, MUTE_RED, VOLUME_CYAN, volume_arc};
use super::scene::PRIDE_RAINBOW;
use super::*;

const SCENES: [LedScene; 5] = [
    LedScene::Standard,
    LedScene::Airy,
    LedScene::Malevolent,
    LedScene::Pride,
    LedScene::Custom,
];

fn caps(names: &[&str]) -> Capabilities {
    Capabilities::from_names(names.iter().copied())
}

fn dot() -> Capabilities {
    caps(&["leds", "led_anim", "buttons", "button_hold"])
}

fn scene(scene: LedScene) -> RingSettings {
    RingSettings {
        led_scene: scene,
        ..RingSettings::default()
    }
}

fn timers(entries: &[(&str, u64, u64)]) -> TimerView {
    TimerView {
        timers: entries
            .iter()
            .map(|&(id, remaining_ms, total_ms)| TimerViewEntry {
                id: id.to_owned(),
                name: None,
                total_ms,
                remaining_ms,
                active: true,
            })
            .collect(),
    }
}

/// What the firmware's decoder and ring accept (`retained`, `AnimSpec`, `resolveMeter`),
/// with the controller's own rules on top: all 12 LEDs named, palettes the patterns use,
/// meter values inside the firmware's clamp ranges, and a TTL on every animation but `off`.
fn firmware_accepts(command: &LedCommand) -> Result<(), String> {
    let body = command.body_json();
    let object = body.as_object().ok_or("body is not an object")?;
    let byte = |v: &Value, what: &str| -> Result<u8, String> {
        v.as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .ok_or(format!("{what} {v} is not 0..255"))
    };
    match command {
        LedCommand::Frame(_) => {
            if object.len() != 1 {
                return Err(format!("extra keys in {body}"));
            }
            let leds = object["leds"].as_array().ok_or("leds is not an array")?;
            if leds.len() != RING_LEDS {
                return Err(format!("{} LEDs", leds.len()));
            }
            let mut seen = [false; RING_LEDS];
            for led in leds {
                let led = led.as_object().ok_or("an LED is not an object")?;
                if led.len() != 4 {
                    return Err(format!("LED keys {led:?}"));
                }
                let id = usize::from(byte(&led["id"], "id")?);
                if id >= RING_LEDS || seen[id] {
                    return Err(format!("LED id {id}"));
                }
                seen[id] = true;
                for channel in ["r", "g", "b"] {
                    byte(&led[channel], channel)?;
                }
            }
        }
        LedCommand::Anim(_) => {
            let anim = object["anim"].as_object().ok_or("anim is not an object")?;
            let pattern = anim["pattern"].as_str().ok_or("pattern is not a string")?;
            let colors = anim["colors"].as_array().ok_or("colors is not an array")?;
            for color in colors {
                let rgb = color.as_array().ok_or("a colour is not an array")?;
                if rgb.len() != 3 {
                    return Err(format!("colour {color}"));
                }
                for c in rgb {
                    byte(c, "channel")?;
                }
            }
            let int = |key: &str| {
                anim[key]
                    .as_u64()
                    .filter(|&n| n <= i32::MAX as u64)
                    .ok_or(format!("{key} is not a non-negative int"))
            };
            let period = int("periodMs")?;
            let ttl = int("ttlSec")?;
            let n = colors.len();
            let palette_ok = |n: usize| n == 1 || n == RING_LEDS;
            match pattern {
                "off" => {}
                "solid" | "pulse" | "meter" if palette_ok(n) => {}
                "spin" if n == 2 => {}
                "rotate" if (1..=RING_LEDS).contains(&n) => {}
                "solid" | "pulse" | "meter" | "spin" | "rotate" => {
                    return Err(format!("{pattern} with {n} colours"));
                }
                other => return Err(format!("unknown pattern {other}")),
            }
            if pattern != "off" && ttl == 0 {
                return Err(format!("{pattern} without a TTL"));
            }
            if pattern == "pulse" && period == 0 {
                return Err("pulse without a cycle".into());
            }
            let ranges = [
                ("attack", 0.05, 1.0),
                ("decay", 0.02, 1.0),
                ("floor", 0.0, 0.6),
                ("gamma", 1.0, 3.5),
                ("ref", 0.02, 1.0),
                ("curve", 0.3, 2.0),
            ];
            for (key, lo, hi) in ranges {
                match (pattern, anim.get(key)) {
                    ("meter", Some(v)) => {
                        let v = v.as_f64().ok_or(format!("{key} is not a number"))?;
                        if !(lo..=hi).contains(&v) {
                            return Err(format!("{key} {v} outside {lo}..{hi}"));
                        }
                    }
                    ("meter", None) => return Err(format!("meter without {key}")),
                    (_, Some(_)) => return Err(format!("{pattern} with {key}")),
                    (_, None) => {}
                }
            }
        }
    }
    Ok(())
}

fn anim_of(command: &LedCommand) -> &AnimSpec {
    match command {
        LedCommand::Anim(body) => &body.anim,
        LedCommand::Frame(_) => panic!("expected an animation, got {command:?}"),
    }
}

fn frame_of(command: &LedCommand) -> Frame {
    match command {
        LedCommand::Frame(body) => body.frame(),
        LedCommand::Anim(_) => panic!("expected a frame, got {command:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn every_phase_of_every_scene_is_a_body_the_firmware_accepts() {
    for s in SCENES {
        let now = Instant::now();
        let mut ring = LedProjector::new(&dot(), &scene(s));
        let mut sent = vec![ring.session_started(now).unwrap()];
        for phase in [
            Phase::Listening,
            Phase::Thinking,
            Phase::Speaking,
            Phase::Idle,
        ] {
            sent.push(ring.set_phase(phase, now).unwrap());
        }
        sent.extend(ring.cue(Cue::NoSpeech, now));
        sent.extend(ring.cue(Cue::Error, now));
        sent.extend(ring.on_timer(now + CUE_DURATION));
        sent.extend(ring.set_timers(&timers(&[("pasta", 90_000, 300_000)]), now));
        sent.extend(ring.set_recording(true, now));
        let patterns: Vec<_> = sent
            .iter()
            .map(|c| match c {
                LedCommand::Anim(b) => format!("{:?}", b.anim.pattern),
                LedCommand::Frame(_) => "frame".into(),
            })
            .collect();
        let think = if s == LedScene::Pride {
            "Rotate"
        } else {
            "Spin"
        };
        assert_eq!(
            patterns,
            [
                "Off", "Solid", think, "Meter", "Off", "Pulse", "Pulse", "Off", "frame", "Pulse"
            ],
            "{s:?}"
        );
        for command in &sent {
            firmware_accepts(command).unwrap_or_else(|e| panic!("{s:?} {command:?}: {e}"));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn standard_scene_bodies_are_exact() {
    let now = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    let listening = ring.set_phase(Phase::Listening, now).unwrap();
    assert_eq!(
        listening.body_json(),
        json!({"anim": {"pattern": "solid", "colors": [[0, 180, 0]], "periodMs": 0, "ttlSec": 30}})
    );
    assert_eq!(listening.message_type(), MessageType::LedAnim);
    let thinking = ring.set_phase(Phase::Thinking, now).unwrap();
    assert_eq!(
        thinking.body_json(),
        json!({"anim": {"pattern": "spin", "colors": [[0, 200, 0], [0, 60, 0]], "periodMs": 80, "ttlSec": 135}})
    );
    let speaking = ring.set_phase(Phase::Speaking, now).unwrap();
    assert_eq!(
        speaking.body_json(),
        json!({"anim": {"pattern": "meter", "colors": [[0, 200, 0]], "periodMs": 0, "ttlSec": 20,
            "attack": 0.6, "decay": 0.3, "floor": 0.06, "gamma": 2.2, "ref": 0.22, "curve": 0.7}})
    );
    let recording = ring.set_recording(true, now).unwrap();
    assert_eq!(
        recording.body_json(),
        json!({"anim": {"pattern": "pulse", "colors": [[180, 0, 200]], "periodMs": 2600, "ttlSec": 20}})
    );
    ring.set_recording(false, now);
    let idle = ring.set_phase(Phase::Idle, now).unwrap();
    assert_eq!(
        idle.body_json(),
        json!({"anim": {"pattern": "off", "colors": [], "periodMs": 0, "ttlSec": 0}})
    );
}

#[test]
fn scene_palettes_match_the_ring_document() {
    let standard = ScenePalette::from_settings(&scene(LedScene::Standard));
    assert_eq!(standard.listening, [Rgb(0, 180, 0)]);
    let airy = ScenePalette::from_settings(&scene(LedScene::Airy));
    assert_eq!(airy.listening, [Rgb(80, 150, 200)]);
    assert_eq!(
        airy.thinking,
        Thinking::Spin {
            head: Rgb(150, 205, 255),
            trail: Rgb(25, 45, 70)
        }
    );
    let malevolent = ScenePalette::from_settings(&scene(LedScene::Malevolent));
    assert_eq!(malevolent.listening, [Rgb(110, 0, 45)]);
    assert_eq!(malevolent.meter, [Rgb(210, 45, 0)]);
    let pride = ScenePalette::from_settings(&scene(LedScene::Pride));
    assert_eq!(pride.listening, PRIDE_RAINBOW);
    assert_eq!(pride.thinking, Thinking::Rotate(PRIDE_RAINBOW.to_vec()));
    assert_eq!(pride.meter, PRIDE_RAINBOW);
    // Twelve distinct hues, none of them the mute ring's red.
    let mut hues = PRIDE_RAINBOW.to_vec();
    hues.dedup();
    assert_eq!(hues.len(), RING_LEDS);
    assert!(!PRIDE_RAINBOW.contains(&MUTE_RED));

    let custom = RingSettings {
        led_scene: LedScene::Custom,
        led_listen_color: ListenColor(LedColor::new("#102030").unwrap()),
        led_think_color: ThinkColor(LedColor::new("#ff9600").unwrap()),
        ..RingSettings::default()
    };
    let custom = ScenePalette::from_settings(&custom);
    assert_eq!(custom.listening, [Rgb(16, 32, 48)]);
    assert_eq!(
        custom.thinking,
        Thinking::Spin {
            head: Rgb(255, 150, 0),
            trail: Rgb(85, 50, 0)
        }
    );
    assert_eq!(custom.meter, [Rgb(255, 150, 0)]);
    // The custom scene's defaults are the standard listening and spinner head colours.
    let defaults = ScenePalette::from_settings(&scene(LedScene::Custom));
    assert_eq!(defaults.listening, [Rgb(0, 180, 0)]);
    assert_eq!(defaults.meter, [Rgb(0, 200, 0)]);
}

#[tokio::test(start_paused = true)]
async fn pride_listens_in_twelve_hues_and_thinks_by_turning_them() {
    let now = Instant::now();
    let mut ring = LedProjector::new(&dot(), &scene(LedScene::Pride));
    let listening = ring.set_phase(Phase::Listening, now).unwrap();
    assert_eq!(anim_of(&listening).colors, PRIDE_RAINBOW);
    assert_eq!(listening.still_frame(), PRIDE_RAINBOW);
    let thinking = ring.set_phase(Phase::Thinking, now).unwrap();
    assert_eq!(anim_of(&thinking).pattern, AnimPattern::Rotate);
    assert_eq!(anim_of(&thinking).period_ms, THINKING_STEP_MS);
}

#[tokio::test(start_paused = true)]
async fn meter_carries_the_meter_settings() {
    let now = Instant::now();
    let ring_settings = RingSettings {
        meter_attack: MeterAttack::new(1.0).unwrap(),
        meter_decay: MeterDecay::new(0.02).unwrap(),
        meter_floor: MeterFloor::new(0.0).unwrap(),
        meter_gamma: MeterGamma::new(3.5).unwrap(),
        meter_ref: MeterRef::new(0.02).unwrap(),
        meter_curve: MeterCurve::new(2.0).unwrap(),
        ..RingSettings::default()
    };
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    ring.set_phase(Phase::Speaking, now);
    let tuned = ring.set_settings(&ring_settings, now).unwrap();
    assert_eq!(
        anim_of(&tuned).meter,
        Some(MeterTuning {
            attack: 1.0,
            decay: 0.02,
            floor: 0.0,
            gamma: 3.5,
            reference: 0.02,
            curve: 2.0
        })
    );
    firmware_accepts(&tuned).unwrap();
    // A settings change that leaves the projection alone sends nothing.
    assert_eq!(ring.set_settings(&ring_settings, now), None);
}

#[tokio::test(start_paused = true)]
async fn cues_play_for_a_second_then_the_ring_returns_to_its_state() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    ring.set_timers(&timers(&[("pasta", 150_000, 300_000)]), t0);
    ring.set_phase(Phase::Listening, t0);
    ring.set_phase(Phase::Idle, t0);
    let cue = ring.cue(Cue::NoSpeech, t0).unwrap();
    assert_eq!(
        cue.body_json(),
        json!({"anim": {"pattern": "pulse", "colors": [[0, 200, 0]], "periodMs": 900, "ttlSec": 1}})
    );
    assert_eq!(ring.next_deadline(), Some(t0 + CUE_DURATION));
    assert_eq!(ring.on_timer(t0 + Duration::from_millis(999)), None);
    let back = ring.on_timer(t0 + CUE_DURATION).unwrap();
    assert_eq!(frame_of(&back).iter().filter(|c| !c.is_dark()).count(), 6);

    let error = ring.cue(Cue::Error, t0 + Duration::from_secs(2)).unwrap();
    assert_eq!(anim_of(&error).period_ms, ERROR_PERIOD_MS);
    // A new turn replaces the cue at once.
    let listening = ring
        .set_phase(Phase::Listening, t0 + Duration::from_millis(2_100))
        .unwrap();
    assert_eq!(anim_of(&listening).pattern, AnimPattern::Solid);
}

#[test]
fn cue_choice_follows_the_terminal_reason() {
    use TerminalReason as R;
    assert_eq!(
        Cue::for_turn_end(R::NoInput, TriggerType::Wake),
        Some(Cue::NoSpeech)
    );
    assert_eq!(
        Cue::for_turn_end(R::NoInput, TriggerType::Button),
        Some(Cue::NoSpeech)
    );
    assert_eq!(Cue::for_turn_end(R::NoInput, TriggerType::Reply), None);
    assert_eq!(Cue::for_turn_end(R::NoInput, TriggerType::HaReply), None);
    for reason in [
        R::Interrupted,
        R::AudioOverrun,
        R::SpeechUnavailable,
        R::HaTimeout,
        R::ResponseTimeout,
        R::SttFailed,
        R::HaError,
        R::DiagnosticRecording,
    ] {
        assert_eq!(
            Cue::for_turn_end(reason, TriggerType::Wake),
            Some(Cue::Error),
            "{reason:?}"
        );
    }
    for reason in [
        R::Completed,
        R::Superseded,
        R::Muted,
        R::SessionLost,
        R::ArbitrationLost,
        R::TooLong,
        R::Retry,
        R::EmptyTranscript,
        R::OutcomeUnknown,
    ] {
        assert_eq!(
            Cue::for_turn_end(reason, TriggerType::Wake),
            None,
            "{reason:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn timer_arc_shows_the_first_timer_learned() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    let lit = |c: &LedCommand| frame_of(c).iter().take_while(|c| !c.is_dark()).count();

    let half = ring
        .set_timers(&timers(&[("pasta", 150_000, 300_000)]), t0)
        .unwrap();
    assert_eq!(lit(&half), 6);
    assert_eq!(frame_of(&half)[0], Rgb(0, 180, 0));
    firmware_accepts(&half).unwrap();
    // A later, shorter timer sorts first in the view but the first one learned stays shown.
    let both = timers(&[("eggs", 10_000, 60_000), ("pasta", 149_000, 300_000)]);
    assert_eq!(ring.set_timers(&both, t0), None);
    // One step down: ceil(12 × 124 999 / 300 000) = 5.
    let lower = ring
        .set_timers(
            &timers(&[("eggs", 9_000, 60_000), ("pasta", 124_999, 300_000)]),
            t0,
        )
        .unwrap();
    assert_eq!(lit(&lower), 5);
    // The last millisecond still lights one LED.
    let last = ring
        .set_timers(
            &timers(&[("eggs", 9_000, 60_000), ("pasta", 1, 300_000)]),
            t0,
        )
        .unwrap();
    assert_eq!(lit(&last), 1);
    // When it leaves, the next one learned is shown.
    let eggs = ring
        .set_timers(&timers(&[("eggs", 9_000, 60_000)]), t0)
        .unwrap();
    assert_eq!(lit(&eggs), 2);
    // At zero awaiting HA's finish, the arc goes dark and the layer is cleared.
    let zero = ring
        .set_timers(&timers(&[("eggs", 0, 60_000)]), t0)
        .unwrap();
    assert_eq!(anim_of(&zero).pattern, AnimPattern::Off);

    assert_eq!(timer_leds(300_000, 300_000), 12);
    assert_eq!(timer_leds(275_001, 300_000), 12);
    assert_eq!(timer_leds(275_000, 300_000), 11);
    assert_eq!(timer_leds(400_000, 300_000), 12);
    assert_eq!(timer_leds(5, 0), 12);
}

#[tokio::test(start_paused = true)]
async fn pride_timer_arc_lights_its_hues_led_by_led() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &scene(LedScene::Pride));
    let arc = ring
        .set_timers(&timers(&[("t", 100_000, 400_000)]), t0)
        .unwrap();
    let frame = frame_of(&arc);
    assert_eq!(frame[..3], PRIDE_RAINBOW[..3]);
    assert!(frame[3..].iter().all(|c| c.is_dark()));
}

#[tokio::test(start_paused = true)]
async fn the_timer_arc_waits_for_the_conversation_and_recordings() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    ring.set_phase(Phase::Listening, t0);
    assert_eq!(
        ring.set_timers(&timers(&[("t", 1_000, 2_000)]), t0),
        None,
        "listening outranks the arc"
    );
    ring.set_recording(true, t0);
    ring.set_phase(Phase::Idle, t0);
    assert_eq!(anim_of(ring.current().unwrap()).pattern, AnimPattern::Pulse);
    let arc = ring.set_recording(false, t0).unwrap();
    assert_eq!(frame_of(&arc).iter().filter(|c| !c.is_dark()).count(), 6);
}

#[tokio::test(start_paused = true)]
async fn standing_updates_are_renewed_after_ten_seconds() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    let listening = ring.set_phase(Phase::Listening, t0).unwrap();
    assert_eq!(
        ring.set_phase(Phase::Listening, t0),
        None,
        "no change, nothing sent"
    );
    assert_eq!(ring.next_deadline(), Some(t0 + REFRESH_AFTER));
    assert_eq!(ring.on_timer(t0 + Duration::from_millis(9_999)), None);
    let renewed = ring.on_timer(t0 + REFRESH_AFTER).unwrap();
    assert_eq!(renewed, listening);
    assert_eq!(ring.next_deadline(), Some(t0 + REFRESH_AFTER * 2));

    // The spinner is not renewed; its TTL bounds the thinking phase.
    ring.set_phase(Phase::Thinking, t0 + REFRESH_AFTER);
    assert_eq!(ring.next_deadline(), None);
    // Nor is `off`.
    ring.set_phase(Phase::Idle, t0 + REFRESH_AFTER);
    assert_eq!(ring.next_deadline(), None);
    // A lit timer frame is.
    ring.set_timers(&timers(&[("t", 1_000, 2_000)]), t0 + REFRESH_AFTER);
    assert_eq!(ring.next_deadline(), Some(t0 + REFRESH_AFTER * 2));
}

#[tokio::test(start_paused = true)]
async fn a_new_session_gets_the_projection_again() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&dot(), &RingSettings::default());
    let listening = ring.set_phase(Phase::Listening, t0).unwrap();
    assert_eq!(ring.session_started(t0), Some(listening.clone()));
    ring.cue(Cue::Error, t0);
    ring.session_lost();
    assert_eq!(ring.current(), None);
    assert_eq!(ring.next_deadline(), None);
    assert_eq!(ring.session_started(t0), Some(listening));
}

#[tokio::test(start_paused = true)]
async fn a_device_without_a_ring_gets_nothing() {
    let t0 = Instant::now();
    let fire_os_show = caps(&["screen_wake"]);
    let mut ring = LedProjector::new(&fire_os_show, &RingSettings::default());
    assert_eq!(ring.session_started(t0), None);
    assert_eq!(ring.set_phase(Phase::Listening, t0), None);
    assert_eq!(ring.set_recording(true, t0), None);
    assert_eq!(ring.cue(Cue::Error, t0), None);
    assert_eq!(ring.set_timers(&timers(&[("t", 1, 2)]), t0), None);
    assert_eq!(ring.next_deadline(), None);
    // `led_anim` alone means nothing without `leds`.
    let mut anim_only = LedProjector::new(&caps(&["led_anim"]), &RingSettings::default());
    assert_eq!(anim_only.set_phase(Phase::Thinking, t0), None);
}

#[tokio::test(start_paused = true)]
async fn a_ring_without_animations_gets_fixed_frames() {
    let t0 = Instant::now();
    let mut ring = LedProjector::new(&caps(&["leds"]), &scene(LedScene::Airy));
    let idle = ring.session_started(t0).unwrap();
    assert_eq!(frame_of(&idle), [Rgb::OFF; RING_LEDS]);
    for phase in [Phase::Listening, Phase::Thinking, Phase::Speaking] {
        let command = ring.set_phase(phase, t0);
        if phase == Phase::Listening {
            let command = command.unwrap();
            assert_eq!(frame_of(&command), [Rgb(80, 150, 200); RING_LEDS]);
            firmware_accepts(&command).unwrap();
        } else {
            assert_eq!(command, None, "{phase:?} keeps the listening frame");
        }
    }
    let recording = ring.set_recording(true, t0).unwrap();
    assert_eq!(frame_of(&recording), [RECORDING_COLOR; RING_LEDS]);
    ring.set_recording(false, t0);
    ring.set_phase(Phase::Idle, t0);
    assert_eq!(ring.cue(Cue::Error, t0), None);
    let arc = ring
        .set_timers(&timers(&[("t", 1_000, 4_000)]), t0)
        .unwrap();
    assert_eq!(frame_of(&arc).iter().filter(|c| !c.is_dark()).count(), 3);
    firmware_accepts(&arc).unwrap();
    assert_eq!(arc.message_type(), MessageType::Leds);
    assert!(matches!(ring.current(), Some(LedCommand::Frame(_))));
}

#[test]
fn device_overlays_cover_the_controller_layer() {
    let listening = LedCommand::Anim(LedAnimBody {
        anim: AnimSpec {
            pattern: AnimPattern::Solid,
            colors: vec![Rgb(0, 180, 0)],
            period_ms: 0,
            ttl_sec: 30,
            meter: None,
        },
    });
    let none = DeviceOverlays::default();
    assert_eq!(none.visible(Some(&listening)), [Rgb(0, 180, 0); RING_LEDS]);
    assert_eq!(none.visible(None), [Rgb::OFF; RING_LEDS]);
    let muted = DeviceOverlays {
        muted: true,
        volume_arc: None,
    };
    assert_eq!(muted.visible(Some(&listening)), [MUTE_RED; RING_LEDS]);
    let arc_over_mute = DeviceOverlays {
        muted: true,
        volume_arc: Some(127),
    };
    assert_eq!(
        arc_over_mute.visible(Some(&listening)),
        [VOLUME_CYAN; RING_LEDS]
    );

    let lit = |level| {
        volume_arc(level)
            .iter()
            .filter(|c| **c == VOLUME_CYAN)
            .count()
    };
    assert_eq!(lit(0), 0);
    assert_eq!(lit(1), 1);
    assert_eq!(lit(47), 1);
    assert_eq!(lit(53), 1);
    assert_eq!(lit(54), 1);
    assert_eq!(lit(87), 6);
    assert_eq!(lit(126), 11);
    assert_eq!(lit(127), 12);
    assert_eq!(lit(255), 12);
    // The arc fills from LED 0, and both overlays are frames the firmware accepts.
    assert_eq!(volume_arc(87)[..6], [VOLUME_CYAN; 6]);
    for frame in [volume_arc(87), [MUTE_RED; RING_LEDS]] {
        firmware_accepts(&LedCommand::Frame(LedsBody::from_frame(&frame))).unwrap();
    }
}

#[test]
fn still_frames_follow_the_firmware_first_frame() {
    let spin = LedCommand::Anim(LedAnimBody {
        anim: AnimSpec {
            pattern: AnimPattern::Spin,
            colors: vec![Rgb(0, 200, 0), Rgb(0, 60, 0)],
            period_ms: 80,
            ttl_sec: 135,
            meter: None,
        },
    });
    let frame = spin.still_frame();
    assert_eq!(frame[0], Rgb(0, 200, 0));
    assert_eq!(frame[11], Rgb(0, 60, 0));
    assert!(frame[1..11].iter().all(|c| c.is_dark()));
    let off = LedCommand::Anim(LedAnimBody {
        anim: AnimSpec::off(),
    });
    assert_eq!(off.still_frame(), [Rgb::OFF; RING_LEDS]);
}

#[test]
fn the_firmware_check_refuses_out_of_range_bodies() {
    let mut bad = LedsBody::from_frame(&[Rgb::OFF; RING_LEDS]);
    bad.leds[3].id = 12;
    assert!(firmware_accepts(&LedCommand::Frame(bad)).is_err());
    let short = LedsBody {
        leds: LedsBody::from_frame(&[Rgb::OFF; RING_LEDS]).leds[..11].to_vec(),
    };
    assert!(firmware_accepts(&LedCommand::Frame(short)).is_err());
    let no_ttl = LedCommand::Anim(LedAnimBody {
        anim: AnimSpec {
            pattern: AnimPattern::Solid,
            colors: vec![Rgb(1, 2, 3)],
            period_ms: 0,
            ttl_sec: 0,
            meter: None,
        },
    });
    assert!(firmware_accepts(&no_ttl).is_err());
    let loud_meter = LedCommand::Anim(LedAnimBody {
        anim: AnimSpec {
            pattern: AnimPattern::Meter,
            colors: vec![Rgb(1, 2, 3)],
            period_ms: 0,
            ttl_sec: 20,
            meter: Some(MeterTuning {
                attack: 0.6,
                decay: 0.3,
                floor: 0.9,
                gamma: 2.2,
                reference: 0.22,
                curve: 0.7,
            }),
        },
    });
    assert!(firmware_accepts(&loud_meter).is_err());
}
