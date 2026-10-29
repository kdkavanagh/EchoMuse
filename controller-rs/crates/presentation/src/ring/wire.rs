//! The `leds` and `led_anim` bodies exactly as the firmware decodes them
//! (`device/internal/supervisor/session.go` `retained`, `device/internal/server/animator.go`
//! `AnimSpec`, `device/pkg/led/led.go` `Led`).

use echomuse_protocol::{ControlBody, MessageType};
use serde::Serialize;

/// The LEDs in the ring: the Dot's ring and the LineageOS Show's on-screen light bar.
pub const RING_LEDS: usize = 12;

/// One LED colour. On the wire it is `[r, g, b]` inside `led_anim` colour lists.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Dark.
    pub const OFF: Rgb = Rgb(0, 0, 0);

    /// Whether every channel is zero.
    pub fn is_dark(self) -> bool {
        self == Self::OFF
    }

    /// The colour with every channel scaled by `factor` (0 to 1), rounded to nearest.
    pub fn scaled(self, factor: f64) -> Rgb {
        let channel = |c: u8| (f64::from(c) * factor).round().clamp(0.0, 255.0) as u8;
        Rgb(channel(self.0), channel(self.1), channel(self.2))
    }
}

impl From<[u8; 3]> for Rgb {
    fn from([r, g, b]: [u8; 3]) -> Self {
        Rgb(r, g, b)
    }
}

/// A whole ring: LED 0 first, clockwise.
pub type Frame = [Rgb; RING_LEDS];

/// One LED of a `leds` body: `{"id", "r", "g", "b"}`, `id` 0 to 11.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Led {
    /// The LED, 0 to 11.
    pub id: u8,
    /// Red.
    pub r: u8,
    /// Green.
    pub g: u8,
    /// Blue.
    pub b: u8,
}

/// `leds`: a fixed frame on the controller layer, replacing any controller animation.
/// The firmware updates only the LEDs it names, so the controller always names all 12.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LedsBody {
    /// All twelve LEDs, in order.
    pub leds: Vec<Led>,
}

impl LedsBody {
    /// The body that paints `frame`.
    pub fn from_frame(frame: &Frame) -> Self {
        let leds = frame
            .iter()
            .zip(0u8..)
            .map(|(c, id)| Led {
                id,
                r: c.0,
                g: c.1,
                b: c.2,
            })
            .collect();
        Self { leds }
    }

    /// The frame this body paints; LEDs it does not name stay dark.
    pub fn frame(&self) -> Frame {
        let mut frame = [Rgb::OFF; RING_LEDS];
        for led in &self.leds {
            if let Some(slot) = frame.get_mut(usize::from(led.id)) {
                *slot = Rgb(led.r, led.g, led.b);
            }
        }
        frame
    }
}

impl ControlBody for LedsBody {
    const MESSAGE_TYPE: MessageType = MessageType::Leds;
}

/// An `led_anim` pattern.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnimPattern {
    /// Clears the controller layer.
    Off,
    /// One colour fills the ring, or one colour per LED.
    Solid,
    /// `[head, trail]`: a lit LED with a dimmer one behind it, one step per period.
    Spin,
    /// The palette shifted one LED per period.
    Rotate,
    /// The palette at 15 to 100 % brightness on a raised cosine, one cycle per period.
    Pulse,
    /// The palette's brightness follows the speaker's final-mix level.
    Meter,
}

/// The meter's response curve (`docs/led-ring-states.md` "Playback meter"). Each value is
/// already inside the firmware's clamp range, since the settings types bound them.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct MeterTuning {
    /// Rise per 40 ms tick, 0.05 to 1.
    pub attack: f64,
    /// Fall per 40 ms tick, 0.02 to 1.
    pub decay: f64,
    /// Brightness at silence, 0 to 0.6.
    pub floor: f64,
    /// Output gamma, 1 to 3.5.
    pub gamma: f64,
    /// The level shown at full brightness, 0.02 to 1.
    #[serde(rename = "ref")]
    pub reference: f64,
    /// Input exponent, 0.3 to 2.
    pub curve: f64,
}

/// The `anim` object of `led_anim`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AnimSpec {
    /// The pattern.
    pub pattern: AnimPattern,
    /// The palette; its meaning depends on the pattern.
    pub colors: Vec<Rgb>,
    /// Spin and rotate: ms per step. Pulse: ms per cycle. 0 is the firmware's default.
    #[serde(rename = "periodMs")]
    pub period_ms: u32,
    /// Seconds until the device clears the layer unless a newer update replaces it; 0 for
    /// none.
    #[serde(rename = "ttlSec")]
    pub ttl_sec: u32,
    /// The meter's curve, for `meter` only.
    #[serde(flatten)]
    pub meter: Option<MeterTuning>,
}

impl AnimSpec {
    /// `off`: clears the controller layer.
    pub fn off() -> Self {
        Self {
            pattern: AnimPattern::Off,
            colors: Vec::new(),
            period_ms: 0,
            ttl_sec: 0,
            meter: None,
        }
    }
}

/// `led_anim`: an animation the device renders on its own ticker.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LedAnimBody {
    /// The animation.
    pub anim: AnimSpec,
}

impl ControlBody for LedAnimBody {
    const MESSAGE_TYPE: MessageType = MessageType::LedAnim;
}

/// One ring update to send.
#[derive(Clone, Debug, PartialEq)]
pub enum LedCommand {
    /// A fixed frame (`leds`).
    Frame(LedsBody),
    /// An animation (`led_anim`).
    Anim(LedAnimBody),
}

impl LedCommand {
    /// The envelope type it travels in.
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Frame(_) => MessageType::Leds,
            Self::Anim(_) => MessageType::LedAnim,
        }
    }

    /// The body as JSON.
    pub fn body_json(&self) -> serde_json::Value {
        let encoded = match self {
            Self::Frame(body) => serde_json::to_value(body),
            Self::Anim(body) => serde_json::to_value(body),
        };
        encoded.expect("LED bodies always serialize")
    }

    /// One still picture of what the controller layer shows: a frame as sent, an
    /// animation's palette at full brightness at its first step, dark for `off`.
    pub fn still_frame(&self) -> Frame {
        match self {
            Self::Frame(body) => body.frame(),
            Self::Anim(body) => anim_still(&body.anim),
        }
    }
}

/// How the firmware maps a palette onto the ring (`paletteFrame`): one colour fills it,
/// otherwise colours map per LED and the rest stay dark.
pub fn palette_frame(colors: &[Rgb]) -> Frame {
    let mut frame = [Rgb::OFF; RING_LEDS];
    match colors {
        [one] => frame = [*one; RING_LEDS],
        many => {
            for (slot, color) in frame.iter_mut().zip(many) {
                *slot = *color;
            }
        }
    }
    frame
}

fn anim_still(spec: &AnimSpec) -> Frame {
    match spec.pattern {
        AnimPattern::Off => [Rgb::OFF; RING_LEDS],
        AnimPattern::Spin => {
            // The firmware's frame 0: head on LED 0, trail on LED 11.
            let mut frame = [Rgb::OFF; RING_LEDS];
            frame[0] = spec.colors.first().copied().unwrap_or_default();
            frame[RING_LEDS - 1] = spec.colors.get(1).copied().unwrap_or_default();
            frame
        }
        AnimPattern::Rotate => {
            let mut frame = [Rgb::OFF; RING_LEDS];
            if !spec.colors.is_empty() {
                for (i, slot) in frame.iter_mut().enumerate() {
                    *slot = spec.colors[i % spec.colors.len()];
                }
            }
            frame
        }
        AnimPattern::Solid | AnimPattern::Pulse | AnimPattern::Meter => palette_frame(&spec.colors),
    }
}
