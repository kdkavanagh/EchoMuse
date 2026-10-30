//! The action button's gesture policy (spec §4.3; `docs/protocol-v1.md` §4.6).
//!
//! The firmware reports both edges of every action-button press in `button.action`: the
//! press with the capture position of the press, and the release with the same position
//! and `held_ms`, the device's own measure of the hold. The controller decides on the
//! release alone and never times the two messages itself, since link delays well past a
//! second would turn noise into gestures. On a LineageOS Show the companion's touch zones
//! report the same messages.
//!
//! On each release:
//!
//! 1. A message whose `physical_seq` is not above the last button or mute event's is
//!    dropped.
//! 2. A press the device used to stop an alert (`handled: "alert_stopped"`, or a `handled`
//!    value this controller does not know) is ignored.
//! 3. A hold of [`HOLD_MS`] or more fires Home Assistant's `long` event, even while muted.
//!    A device without `button_hold` reports no hold time, so every release is a tap.
//! 4. A tap while muted is refused (the error cue, hidden by the red ring).
//! 5. A tap during a turn cancels it. A tap during a reply window that has heard no command
//!    speech starts a button turn that keeps the question, unless taps fire events, in
//!    which case it cancels the window.
//! 6. With `buttonSingleTapEvent` on, a tap fires `single`, or with `buttonMultiTapMs`
//!    above 0, taps whose releases are at most that far apart on the device's clock are
//!    grouped into `single`, `double` or `triple`; a group ends at its third tap, when the
//!    window passes on the controller's clock with no further tap, or when a tap arrives
//!    too far apart.
//! 7. Otherwise a tap starts a turn whose audio begins [`PREROLL_MS`] before the press,
//!    unless a recording mode is on or speech is unavailable (both refused with the error
//!    cue) or the press carries no capture position.
//!
//! Volume and mute buttons are handled on the device; their reports are ignored here.

use echomuse_esphome::entities::ButtonEvent;
use echomuse_protocol::{ProtocolError, ProtocolErrorCode, Reported};
use echomuse_settings::settings::ButtonSettings;
use serde_json::{Map, Value};
use tokio::time::{Duration, Instant};

/// The shortest hold that fires `long`, in ms.
pub const HOLD_MS: u64 = 750;
/// How far before the press a button turn's audio starts, in ms.
pub const PREROLL_MS: u64 = 300;
/// Microphone samples per ms (16 kHz).
const SAMPLES_PER_MS: u64 = 16;

/// `button.action.click_type`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClickType {
    /// The action button (138).
    Dot,
    /// Volume up (115).
    VolumeUp,
    /// Volume down (114).
    VolumeDown,
    /// The mute button (113).
    Mute,
    /// A code this controller does not know.
    Other(u16),
}

impl ClickType {
    /// The click type of a wire code.
    pub fn from_code(code: u16) -> Self {
        match code {
            138 => Self::Dot,
            115 => Self::VolumeUp,
            114 => Self::VolumeDown,
            113 => Self::Mute,
            other => Self::Other(other),
        }
    }
}

/// `button.action.handled`: what the device already did with the press.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Handled {
    /// It stopped a ringing or backgrounded alert.
    AlertStopped,
}

/// Where the microphone timeline stood at the press.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CapturePosition {
    /// The capture epoch.
    pub epoch: u64,
    /// The sample at the press.
    pub sample: u64,
}

/// One decoded `button.action`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ButtonAction {
    /// Which button.
    pub click: ClickType,
    /// `button`: `Dot` or `Volume` as sent.
    pub button: String,
    /// The press edge; false for the release.
    pub down: bool,
    /// How long it was held, on the release; 0 on the press.
    pub held_ms: u64,
    /// The device was privacy-muted at the edge.
    pub muted: bool,
    /// The device's monotonic clock at the edge.
    pub mono_ns: i64,
    /// The press's capture position; `None` while nothing was being captured.
    pub capture: Option<CapturePosition>,
    /// The physical event number shared with `privacy.changed`.
    pub physical_seq: u64,
    /// The alert the device had active, if any.
    pub occurrence_id: Option<String>,
    /// What the device did with the press, if anything.
    pub handled: Option<Reported<Handled>>,
}

impl ButtonAction {
    /// Decodes a `button.action` body as the firmware writes it (`proto.ButtonAction`).
    pub fn parse(body: &Map<String, Value>) -> Result<Self, ProtocolError> {
        let bad = |key: &str, rule: &str| {
            ProtocolError::new(
                ProtocolErrorCode::MalformedMessage,
                format!("button.action.{key} {rule}"),
            )
        };
        let get = |key: &str| body.get(key).unwrap_or(&Value::Null);
        let uint = |key: &str| {
            get(key)
                .as_u64()
                .ok_or_else(|| bad(key, "must be a non-negative integer"))
        };
        let boolean = |key: &str| {
            get(key)
                .as_bool()
                .ok_or_else(|| bad(key, "must be a boolean"))
        };
        let nullable_u64 = |key: &str| match get(key) {
            Value::Null => Ok(None),
            Value::String(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse()
                    .map(Some)
                    .map_err(|_| bad(key, "is out of range"))
            }
            _ => Err(bad(key, "must be a decimal string or null")),
        };
        let code =
            u16::try_from(uint("click_type")?).map_err(|_| bad("click_type", "is out of range"))?;
        let button = get("button")
            .as_str()
            .ok_or_else(|| bad("button", "must be a string"))?
            .to_owned();
        let mono_ns = get("mono_ns")
            .as_str()
            .and_then(|text| text.parse::<i64>().ok())
            .ok_or_else(|| bad("mono_ns", "must be a decimal string"))?;
        let capture = match (
            nullable_u64("capture_epoch")?,
            nullable_u64("capture_sample")?,
        ) {
            (Some(epoch), Some(sample)) => Some(CapturePosition { epoch, sample }),
            _ => None,
        };
        let occurrence_id = match get("occurrence_id") {
            Value::Null => None,
            Value::String(id) => Some(id.clone()),
            _ => return Err(bad("occurrence_id", "must be a string or null")),
        };
        let handled = match get("handled") {
            Value::Null => None,
            Value::String(text) if text == "alert_stopped" => {
                Some(Reported::Known(Handled::AlertStopped))
            }
            Value::String(text) => Some(Reported::Unrecognized(text.clone())),
            _ => return Err(bad("handled", "must be a string or null")),
        };
        Ok(Self {
            click: ClickType::from_code(code),
            button,
            down: boolean("down")?,
            held_ms: uint("held_ms")?,
            muted: boolean("muted")?,
            mono_ns,
            capture,
            physical_seq: uint("physical_seq")?,
            occurrence_id,
            handled,
        })
    }
}

/// The dialog a tap lands in.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TurnActivity {
    /// No turn.
    Idle,
    /// A wake, button or reply turn with command speech, or its answer.
    Turn,
    /// A reply window that has heard no command speech yet.
    ReplyWindow,
}

/// What the device runtime knows when a release arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ButtonContext {
    /// The dialog in progress.
    pub activity: TurnActivity,
    /// A recording mode is on.
    pub recording: bool,
    /// The speech path can take a turn.
    pub speech_available: bool,
}

/// Why a tap did not start a turn.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonRefusal {
    /// The device is privacy-muted.
    Muted,
    /// A recording mode is on.
    Recording,
    /// The speech path is unavailable.
    SpeechUnavailable,
    /// The press carries no capture position.
    NoCapture,
}

/// A button turn to open.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ButtonTurn {
    /// The capture epoch of the press.
    pub epoch: u64,
    /// The trigger: the press's sample.
    pub press_sample: u64,
    /// Where the turn's audio starts: [`PREROLL_MS`] before the press.
    pub audio_from: u64,
    /// It supersedes a reply window and keeps its question.
    pub keeps_reply: bool,
}

/// What to do.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonOutput {
    /// Fire Home Assistant's `action_button` event.
    Event(ButtonEvent),
    /// Cancel the turn in progress.
    CancelTurn,
    /// Open a button turn.
    StartTurn(ButtonTurn),
    /// The tap was refused; play the error cue.
    Refused(ButtonRefusal),
}

/// Why a message led to nothing.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Ignored {
    /// Its `physical_seq` was not above the last one.
    Duplicate,
    /// A volume or mute button, which the device handles.
    LocalButton,
    /// The press edge; the release decides.
    Press,
    /// The device used the press itself (it stopped an alert).
    HandledByDevice,
}

/// The verdict on one message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ButtonDecision {
    /// Nothing to do.
    Ignored(Ignored),
    /// Do these, in order (a pending tap group may be flushed first).
    Act(Vec<ButtonOutput>),
}

#[derive(Clone, Copy, Debug)]
struct TapGroup {
    taps: u8,
    last_release_ns: i64,
    closes_at: Instant,
}

/// One device session's button policy. The device runtime passes every `button.action`
/// to [`ButtonPolicy::on_action`], every `privacy.changed` sequence number to
/// [`ButtonPolicy::accept_physical_seq`], and calls [`ButtonPolicy::on_timer`] at
/// [`ButtonPolicy::next_deadline`]. A new session starts a new policy.
#[derive(Clone, Debug)]
pub struct ButtonPolicy {
    hold_capable: bool,
    settings: ButtonSettings,
    last_seq: Option<u64>,
    group: Option<TapGroup>,
}

impl ButtonPolicy {
    /// A policy for a device; `hold_capable` is whether it announced `button_hold`.
    pub fn new(hold_capable: bool, settings: ButtonSettings) -> Self {
        Self {
            hold_capable,
            settings,
            last_seq: None,
            group: None,
        }
    }

    /// New `advanced` settings; they apply from the next tap.
    pub fn set_settings(&mut self, settings: ButtonSettings) {
        self.settings = settings;
    }

    /// Whether a physical event numbered `seq` is new. Button and mute events share one
    /// rising number per session, so the runtime checks `privacy.changed` here too.
    pub fn accept_physical_seq(&mut self, seq: u64) -> bool {
        if self.last_seq.is_some_and(|last| seq <= last) {
            return false;
        }
        self.last_seq = Some(seq);
        true
    }

    /// Whether taps fire Home Assistant events instead of starting turns.
    fn tap_events(&self) -> bool {
        self.hold_capable && self.settings.button_single_tap_event
    }

    /// Decides one `button.action` received at `now` on the controller's clock.
    pub fn on_action(
        &mut self,
        action: &ButtonAction,
        context: ButtonContext,
        now: Instant,
    ) -> ButtonDecision {
        if !self.accept_physical_seq(action.physical_seq) {
            return ButtonDecision::Ignored(Ignored::Duplicate);
        }
        if action.click != ClickType::Dot {
            return ButtonDecision::Ignored(Ignored::LocalButton);
        }
        if action.down {
            return ButtonDecision::Ignored(Ignored::Press);
        }
        if action.handled.is_some() {
            return ButtonDecision::Ignored(Ignored::HandledByDevice);
        }
        let mut out = Vec::new();
        if self.hold_capable && action.held_ms >= HOLD_MS {
            self.flush(&mut out);
            out.push(ButtonOutput::Event(ButtonEvent::Long));
            return ButtonDecision::Act(out);
        }
        if action.muted {
            self.flush(&mut out);
            out.push(ButtonOutput::Refused(ButtonRefusal::Muted));
            return ButtonDecision::Act(out);
        }
        let events = self.tap_events();
        let cancels = match context.activity {
            TurnActivity::Turn => true,
            TurnActivity::ReplyWindow => events,
            TurnActivity::Idle => false,
        };
        if cancels {
            self.flush(&mut out);
            out.push(ButtonOutput::CancelTurn);
            return ButtonDecision::Act(out);
        }
        if events {
            self.tap(action.mono_ns, now, &mut out);
            return ButtonDecision::Act(out);
        }
        let start = if context.recording {
            ButtonOutput::Refused(ButtonRefusal::Recording)
        } else if !context.speech_available {
            ButtonOutput::Refused(ButtonRefusal::SpeechUnavailable)
        } else if let Some(at) = action.capture {
            ButtonOutput::StartTurn(ButtonTurn {
                epoch: at.epoch,
                press_sample: at.sample,
                audio_from: at.sample.saturating_sub(PREROLL_MS * SAMPLES_PER_MS),
                keeps_reply: context.activity == TurnActivity::ReplyWindow,
            })
        } else {
            ButtonOutput::Refused(ButtonRefusal::NoCapture)
        };
        out.push(start);
        ButtonDecision::Act(out)
    }

    /// When a pending tap group closes.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.group.map(|g| g.closes_at)
    }

    /// Closes a tap group whose window has passed.
    pub fn on_timer(&mut self, now: Instant) -> Option<ButtonOutput> {
        let group = self.group.filter(|g| now >= g.closes_at)?;
        self.group = None;
        Some(ButtonOutput::Event(tap_event(group.taps)))
    }

    fn tap(&mut self, release_ns: i64, now: Instant, out: &mut Vec<ButtonOutput>) {
        let window_ms = self.settings.button_multi_tap_ms.get();
        if window_ms == 0 {
            out.push(ButtonOutput::Event(ButtonEvent::Single));
            return;
        }
        let window_ns = i64::from(window_ms) * 1_000_000;
        let taps = match self.group {
            Some(g) if (0..=window_ns).contains(&release_ns.saturating_sub(g.last_release_ns)) => {
                g.taps + 1
            }
            Some(_) => {
                self.flush(out);
                1
            }
            None => 1,
        };
        if taps >= 3 {
            self.group = None;
            out.push(ButtonOutput::Event(ButtonEvent::Triple));
            return;
        }
        self.group = Some(TapGroup {
            taps,
            last_release_ns: release_ns,
            closes_at: now + Duration::from_millis(u64::from(window_ms)),
        });
    }

    fn flush(&mut self, out: &mut Vec<ButtonOutput>) {
        if let Some(group) = self.group.take() {
            out.push(ButtonOutput::Event(tap_event(group.taps)));
        }
    }
}

fn tap_event(taps: u8) -> ButtonEvent {
    match taps {
        0 | 1 => ButtonEvent::Single,
        2 => ButtonEvent::Double,
        _ => ButtonEvent::Triple,
    }
}

#[cfg(test)]
mod tests;
