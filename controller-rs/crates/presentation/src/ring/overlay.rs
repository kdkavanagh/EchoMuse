//! The device's own overlays on top of the controller's layer, for a still preview of
//! what the ring shows (`device/internal/server/ring.go` `paintLocked`, `showVolume`).
//! The device paints these itself and keeps them without a controller; the controller
//! never sends them and no scene changes their colours. The alert and link layers, which
//! also outrank the controller, are not modelled here.

use super::wire::{Frame, LedCommand, RING_LEDS, Rgb};

/// The privacy-mute ring: solid red.
pub const MUTE_RED: Rgb = Rgb(180, 0, 0);
/// The volume arc's colour.
pub const VOLUME_CYAN: Rgb = Rgb(0, 200, 200);

/// The volume scale's top (codec unity).
pub const VOLUME_MAX: u8 = 127;
/// The volume buttons' lowest level (−40 dB); the arc's scale starts here.
pub const VOLUME_BUTTON_FLOOR: u8 = 47;

/// The device's local overlays at one moment.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceOverlays {
    /// The privacy mute is on (`privacy.changed`).
    pub muted: bool,
    /// The volume arc is showing at this level (0 to 127), for the 2 s after a volume
    /// button press.
    pub volume_arc: Option<u8>,
}

impl DeviceOverlays {
    /// What the ring shows over `controller`, the controller layer's current update (none
    /// when the layer is clear): the volume arc first, then the mute ring, then the
    /// controller layer's still frame.
    pub fn visible(&self, controller: Option<&LedCommand>) -> Frame {
        if let Some(level) = self.volume_arc {
            return volume_arc(level);
        }
        if self.muted {
            return [MUTE_RED; RING_LEDS];
        }
        controller.map_or([Rgb::OFF; RING_LEDS], LedCommand::still_frame)
    }
}

/// The arc for `level`: LEDs from LED 0 in proportion to the level across the button range,
/// all 12 at the top, at least 1 above level 0, none at 0.
pub fn volume_arc(level: u8) -> Frame {
    let level = i32::from(level.min(VOLUME_MAX));
    let span = i32::from(VOLUME_MAX - VOLUME_BUTTON_FLOOR);
    let mut lit = (level - i32::from(VOLUME_BUTTON_FLOOR)) * RING_LEDS as i32 / span;
    if lit < 1 && level > 0 {
        lit = 1;
    }
    let lit = usize::try_from(lit.clamp(0, RING_LEDS as i32)).unwrap_or(0);
    let mut frame = [Rgb::OFF; RING_LEDS];
    for slot in frame.iter_mut().take(lit) {
        *slot = VOLUME_CYAN;
    }
    frame
}
