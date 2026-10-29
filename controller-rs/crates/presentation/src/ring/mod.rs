//! The controller's projection of a device's state onto its LED ring (spec §4.8;
//! `docs/led-ring-states.md`). On a LineageOS Show the same `leds` and `led_anim` messages
//! drive the companion app's on-screen light bar, which the firmware feeds from the same
//! ring compositor, so nothing here differs by platform.
//!
//! The device composites the ring from layers. The controller owns one layer and paints it
//! from the first matching row:
//!
//! | Condition | Ring | Wire |
//! |---|---|---|
//! | A recording mode is on | magenta `(180,0,200)` pulse, 2.6 s cycle | `led_anim` pulse, TTL 20 s |
//! | Listening | the scene's listening palette | `led_anim` solid, TTL 30 s |
//! | Thinking | the scene's spinner (pride: the rainbow turning), 80 ms per step | `led_anim` spin or rotate, TTL 135 s |
//! | Speaking | the meter in the scene's meter palette, tuned by the `meter*` settings | `led_anim` meter, TTL 20 s |
//! | An outcome cue is playing (1 s) | no-speech: one 0.9 s throb; error: 0.22 s blinks | `led_anim` pulse, TTL 1 s |
//! | A timer is running | `ceil(12 × remaining ÷ total)` LEDs from LED 0 in the listening palette | `leds` |
//! | Otherwise | dark | `led_anim` off |
//!
//! Animations with a TTL of 30 s or less, and lit frames, are sent again after 10 s without
//! an update, so they last as long as their state; the spinner's 135 s TTL is never renewed
//! and bounds one thinking phase. A device without `leds` gets nothing. One with `leds` but
//! not `led_anim` gets fixed frames: the listening palette for every dialog phase, a solid
//! magenta frame for a recording mode, no outcome cues, and a dark frame when idle.
//!
//! The mute ring, the volume arc, ringing alerts and the link state are the device's own
//! layers; [`overlay`] models them for a still preview.

pub mod overlay;
pub mod scene;
pub mod wire;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use echomuse_alerts::wire::TimerView;
use echomuse_protocol::{Capabilities, Capability};
use echomuse_records::vocab::{TerminalReason, TriggerType};
use echomuse_settings::settings::RingSettings;
use tokio::time::{Duration, Instant};

use crate::phase::Phase;
use scene::{ScenePalette, Thinking};
use wire::{
    AnimPattern, AnimSpec, Frame, LedAnimBody, LedCommand, LedsBody, MeterTuning, RING_LEDS, Rgb,
    palette_frame,
};

/// The recording-mode pulse's colour.
pub const RECORDING_COLOR: Rgb = Rgb(180, 0, 200);
/// The recording-mode pulse's cycle.
pub const RECORDING_PERIOD_MS: u32 = 2_600;
/// The thinking animation's step.
pub const THINKING_STEP_MS: u32 = 80;
/// The no-speech cue's pulse cycle.
pub const NO_SPEECH_PERIOD_MS: u32 = 900;
/// The error cue's pulse cycle.
pub const ERROR_PERIOD_MS: u32 = 220;
/// How long an outcome cue shows.
pub const CUE_DURATION: Duration = Duration::from_secs(1);
/// How long a renewable update may stand before it is sent again.
pub const REFRESH_AFTER: Duration = Duration::from_secs(10);

const LISTENING_TTL_SEC: u32 = 30;
const THINKING_TTL_SEC: u32 = 135;
const SPEAKING_TTL_SEC: u32 = 20;
const RECORDING_TTL_SEC: u32 = 20;
const CUE_TTL_SEC: u32 = 1;

/// A one-second outcome cue in the scene's cue colour.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Cue {
    /// Nothing was said after the wake word or button: one slow throb.
    NoSpeech,
    /// Something went wrong: fast blinks.
    Error,
}

impl Cue {
    /// The cue for a turn that ended with `reason`. A reply window that heard nothing ends
    /// without a cue; refusals that are not turn endings (a wake or button refused while
    /// a recording mode is on, a button refused while muted, a spoken line that could not
    /// play) take [`Cue::Error`] directly.
    pub fn for_turn_end(reason: TerminalReason, trigger: TriggerType) -> Option<Cue> {
        use TerminalReason as R;
        match reason {
            R::NoInput => {
                matches!(trigger, TriggerType::Wake | TriggerType::Button).then_some(Cue::NoSpeech)
            }
            R::Interrupted
            | R::AudioOverrun
            | R::SpeechUnavailable
            | R::HaTimeout
            | R::ResponseTimeout
            | R::SttFailed
            | R::HaError
            | R::DiagnosticRecording => Some(Cue::Error),
            _ => None,
        }
    }
}

/// What the device can show.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RingCapabilities {
    /// A ring or light bar (`leds`).
    pub frames: bool,
    /// Animations (`led_anim`).
    pub animations: bool,
}

impl RingCapabilities {
    /// What `capabilities` announce.
    pub fn of(capabilities: &Capabilities) -> Self {
        let frames = capabilities.contains(Capability::Leds);
        Self {
            frames,
            animations: frames && capabilities.contains(Capability::LedAnim),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ActiveCue {
    cue: Cue,
    until: Instant,
}

#[derive(Clone, Debug)]
struct Sent {
    command: LedCommand,
    at: Instant,
    renew: bool,
}

/// One device's ring projection. The device runtime feeds it state changes and calls
/// [`LedProjector::on_timer`] at [`LedProjector::next_deadline`]; every method returns the
/// update to send, if any. It does no I/O and reads no clock.
#[derive(Clone, Debug)]
pub struct LedProjector {
    capabilities: RingCapabilities,
    palette: ScenePalette,
    meter: MeterTuning,
    phase: Phase,
    recording: bool,
    /// Timer ids in the order they were first seen, with their latest `(remaining, total)`.
    timer_order: Vec<String>,
    timers: BTreeMap<String, (u64, u64)>,
    cue: Option<ActiveCue>,
    sent: Option<Sent>,
}

impl LedProjector {
    /// A projection for a device announcing `capabilities`, idle and with nothing sent.
    pub fn new(capabilities: &Capabilities, ring: &RingSettings) -> Self {
        Self {
            capabilities: RingCapabilities::of(capabilities),
            palette: ScenePalette::from_settings(ring),
            meter: meter_tuning(ring),
            phase: Phase::Idle,
            recording: false,
            timer_order: Vec::new(),
            timers: BTreeMap::new(),
            cue: None,
            sent: None,
        }
    }

    /// The device's session became ready: everything is sent again.
    pub fn session_started(&mut self, now: Instant) -> Option<LedCommand> {
        self.emit(now, true)
    }

    /// The session ended; the device clears the controller layer itself.
    pub fn session_lost(&mut self) {
        self.sent = None;
        self.cue = None;
    }

    /// The ring settings changed.
    pub fn set_settings(&mut self, ring: &RingSettings, now: Instant) -> Option<LedCommand> {
        self.palette = ScenePalette::from_settings(ring);
        self.meter = meter_tuning(ring);
        self.emit(now, false)
    }

    /// The dialog phase changed. A new dialog phase ends any cue.
    pub fn set_phase(&mut self, phase: Phase, now: Instant) -> Option<LedCommand> {
        self.phase = phase;
        if phase != Phase::Idle {
            self.cue = None;
        }
        self.emit(now, false)
    }

    /// A recording mode started or ended.
    pub fn set_recording(&mut self, on: bool, now: Instant) -> Option<LedCommand> {
        self.recording = on;
        if on {
            self.cue = None;
        }
        self.emit(now, false)
    }

    /// The speaker's timers, as the alerts service's display copies show them now.
    pub fn set_timers(&mut self, view: &TimerView, now: Instant) -> Option<LedCommand> {
        self.timers = view
            .timers
            .iter()
            .map(|t| (t.id.clone(), (t.remaining_ms, t.total_ms)))
            .collect();
        self.timer_order.retain(|id| self.timers.contains_key(id));
        for timer in &view.timers {
            if !self.timer_order.contains(&timer.id) {
                self.timer_order.push(timer.id.clone());
            }
        }
        self.emit(now, false)
    }

    /// Plays an outcome cue for one second over the idle ring. A device without
    /// animations shows none.
    pub fn cue(&mut self, cue: Cue, now: Instant) -> Option<LedCommand> {
        if !self.capabilities.animations {
            return None;
        }
        self.cue = Some(ActiveCue {
            cue,
            until: now + CUE_DURATION,
        });
        self.emit(now, false)
    }

    /// When [`LedProjector::on_timer`] next has work.
    pub fn next_deadline(&self) -> Option<Instant> {
        let cue_end = self.cue.map(|c| c.until);
        let renew = self
            .sent
            .as_ref()
            .filter(|s| s.renew)
            .map(|s| s.at + REFRESH_AFTER);
        match (cue_end, renew) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Ends a finished cue and renews a standing update.
    pub fn on_timer(&mut self, now: Instant) -> Option<LedCommand> {
        if self.cue.is_some_and(|c| now >= c.until) {
            self.cue = None;
        }
        let renew_due = self
            .sent
            .as_ref()
            .is_some_and(|s| s.renew && now >= s.at + REFRESH_AFTER);
        self.emit(now, renew_due)
    }

    /// The update last sent, if the session has had one.
    pub fn current(&self) -> Option<&LedCommand> {
        self.sent.as_ref().map(|s| &s.command)
    }

    fn emit(&mut self, now: Instant, force: bool) -> Option<LedCommand> {
        let (command, renew) = self.desired()?;
        let changed = self.sent.as_ref().is_none_or(|s| s.command != command);
        if !(changed || force) {
            return None;
        }
        self.sent = Some(Sent {
            command: command.clone(),
            at: now,
            renew,
        });
        Some(command)
    }

    /// The update the current state calls for, and whether it is renewed.
    fn desired(&self) -> Option<(LedCommand, bool)> {
        if !self.capabilities.frames {
            return None;
        }
        if !self.capabilities.animations {
            return Some(self.desired_frame());
        }
        let anim = |pattern, colors: Vec<Rgb>, period_ms, ttl_sec, meter| {
            LedCommand::Anim(LedAnimBody {
                anim: AnimSpec {
                    pattern,
                    colors,
                    period_ms,
                    ttl_sec,
                    meter,
                },
            })
        };
        let p = &self.palette;
        if self.recording {
            let pulse = anim(
                AnimPattern::Pulse,
                vec![RECORDING_COLOR],
                RECORDING_PERIOD_MS,
                RECORDING_TTL_SEC,
                None,
            );
            return Some((pulse, true));
        }
        match self.phase {
            Phase::Listening => {
                let solid = anim(
                    AnimPattern::Solid,
                    p.listening.clone(),
                    0,
                    LISTENING_TTL_SEC,
                    None,
                );
                return Some((solid, true));
            }
            Phase::Thinking => {
                let spinner = match &p.thinking {
                    Thinking::Spin { head, trail } => anim(
                        AnimPattern::Spin,
                        vec![*head, *trail],
                        THINKING_STEP_MS,
                        THINKING_TTL_SEC,
                        None,
                    ),
                    Thinking::Rotate(colors) => anim(
                        AnimPattern::Rotate,
                        colors.clone(),
                        THINKING_STEP_MS,
                        THINKING_TTL_SEC,
                        None,
                    ),
                };
                return Some((spinner, false));
            }
            Phase::Speaking => {
                let meter = anim(
                    AnimPattern::Meter,
                    p.meter.clone(),
                    0,
                    SPEAKING_TTL_SEC,
                    Some(self.meter),
                );
                return Some((meter, true));
            }
            Phase::Idle => {}
        }
        if let Some(active) = self.cue {
            let period = match active.cue {
                Cue::NoSpeech => NO_SPEECH_PERIOD_MS,
                Cue::Error => ERROR_PERIOD_MS,
            };
            let pulse = anim(
                AnimPattern::Pulse,
                p.meter.clone(),
                period,
                CUE_TTL_SEC,
                None,
            );
            return Some((pulse, false));
        }
        if let Some(frame) = self.timer_frame() {
            return Some((LedCommand::Frame(LedsBody::from_frame(&frame)), true));
        }
        Some((
            LedCommand::Anim(LedAnimBody {
                anim: AnimSpec::off(),
            }),
            false,
        ))
    }

    /// The fixed-frame projection for a device without animations.
    fn desired_frame(&self) -> (LedCommand, bool) {
        let frame = if self.recording {
            [RECORDING_COLOR; RING_LEDS]
        } else if self.phase != Phase::Idle {
            palette_frame(&self.palette.listening)
        } else {
            self.timer_frame().unwrap_or([Rgb::OFF; RING_LEDS])
        };
        let lit = frame.iter().any(|c| !c.is_dark());
        (LedCommand::Frame(LedsBody::from_frame(&frame)), lit)
    }

    /// The first-seen timer's remaining fraction, or `None` when no timer has time left.
    fn timer_frame(&self) -> Option<Frame> {
        let first = self.timer_order.first()?;
        let &(remaining, total) = self.timers.get(first)?;
        let lit = timer_leds(remaining, total);
        if lit == 0 {
            return None;
        }
        let mut frame = [Rgb::OFF; RING_LEDS];
        for (i, slot) in frame.iter_mut().enumerate().take(lit) {
            *slot = self.palette.listening_at(i);
        }
        Some(frame)
    }
}

/// `ceil(12 × remaining ÷ total)`, at most 12; at least 1 while any time remains.
pub fn timer_leds(remaining_ms: u64, total_ms: u64) -> usize {
    if remaining_ms == 0 {
        return 0;
    }
    if total_ms == 0 {
        return RING_LEDS;
    }
    let lit = (u128::from(remaining_ms) * RING_LEDS as u128).div_ceil(u128::from(total_ms));
    usize::try_from(lit)
        .unwrap_or(RING_LEDS)
        .clamp(1, RING_LEDS)
}

fn meter_tuning(ring: &RingSettings) -> MeterTuning {
    MeterTuning {
        attack: ring.meter_attack.get(),
        decay: ring.meter_decay.get(),
        floor: ring.meter_floor.get(),
        gamma: ring.meter_gamma.get(),
        reference: ring.meter_ref.get(),
        curve: ring.meter_curve.get(),
    }
}
