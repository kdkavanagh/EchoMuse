//! The colour scenes of the controller-driven ring (`docs/led-ring-states.md` "Scenes").
//! The device-local colours (red mute, cyan volume arc and alerts) are fixed in firmware
//! and never follow a scene.

use echomuse_settings::settings::RingSettings;
use echomuse_settings::values::LedScene;

use super::wire::{RING_LEDS, Rgb};

/// The `pride` rainbow, one hue per LED: twelve hues 30° apart from red, at full saturation
/// and the brightness of the dashboard's pride swatches (0xbf).
pub const PRIDE_RAINBOW: [Rgb; RING_LEDS] = [
    Rgb(191, 0, 0),
    Rgb(191, 96, 0),
    Rgb(191, 191, 0),
    Rgb(96, 191, 0),
    Rgb(0, 191, 0),
    Rgb(0, 191, 96),
    Rgb(0, 191, 191),
    Rgb(0, 96, 191),
    Rgb(0, 0, 191),
    Rgb(96, 0, 191),
    Rgb(191, 0, 191),
    Rgb(191, 0, 96),
];

/// How the thinking phase moves.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Thinking {
    /// A lit head with a dimmer trail behind it.
    Spin {
        /// The head.
        head: Rgb,
        /// The LED behind it.
        trail: Rgb,
    },
    /// The palette turns around the ring.
    Rotate(Vec<Rgb>),
}

/// One scene's colours.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScenePalette {
    /// The listening ring and the timer arc: one colour, or one per LED.
    pub listening: Vec<Rgb>,
    /// The thinking animation.
    pub thinking: Thinking,
    /// The speaking meter and the outcome cues: one colour, or one per LED.
    pub meter: Vec<Rgb>,
}

impl ScenePalette {
    /// The palette `ring` selects. Custom colours are already valid `#rrggbb` values.
    pub fn from_settings(ring: &RingSettings) -> Self {
        match ring.led_scene {
            LedScene::Standard => Self::fixed(Rgb(0, 180, 0), Rgb(0, 200, 0), Rgb(0, 60, 0)),
            LedScene::Airy => Self::fixed(Rgb(80, 150, 200), Rgb(150, 205, 255), Rgb(25, 45, 70)),
            LedScene::Malevolent => Self::fixed(Rgb(110, 0, 45), Rgb(210, 45, 0), Rgb(55, 8, 0)),
            LedScene::Pride => Self {
                listening: PRIDE_RAINBOW.to_vec(),
                thinking: Thinking::Rotate(PRIDE_RAINBOW.to_vec()),
                meter: PRIDE_RAINBOW.to_vec(),
            },
            LedScene::Custom => {
                let listen = Rgb::from(ring.led_listen_color.0.rgb());
                let think = Rgb::from(ring.led_think_color.0.rgb());
                Self::fixed(listen, think, think.scaled(1.0 / 3.0))
            }
        }
    }

    fn fixed(listen: Rgb, head: Rgb, trail: Rgb) -> Self {
        Self {
            listening: vec![listen],
            thinking: Thinking::Spin { head, trail },
            meter: vec![head],
        }
    }

    /// The listening colour of LED `index`.
    pub fn listening_at(&self, index: usize) -> Rgb {
        match self.listening.as_slice() {
            [one] => *one,
            many => many.get(index).copied().unwrap_or_default(),
        }
    }
}
