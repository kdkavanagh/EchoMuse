//! The voice satellite's entities and their states (spec §3.3.1), and the
//! Bluetooth proxy's one diagnostic sensor (§3.3.2).
//!
//! Home Assistant 2026.8.1 keys an ESPHome entity on the MAC, the entity
//! type and the entity *name* (`{mac}/0/{type}/{name}`), and migrates older
//! entries keyed on the `object_id`. The keys route states and commands.
//! All three are therefore fixed. The names carry the device label, as the
//! entities already registered in Home Assistant do.

use bytes::Bytes;
use echomuse_protocol::{Capabilities, Capability};

use crate::api::{
    BinarySensorStateResponse, EntityCategory, EventResponse, ListEntitiesBinarySensorResponse,
    ListEntitiesButtonResponse, ListEntitiesDoneResponse, ListEntitiesEventResponse,
    ListEntitiesMediaPlayerResponse, ListEntitiesSensorResponse, ListEntitiesTextSensorResponse,
    MediaPlayerFormatPurpose, MediaPlayerState, MediaPlayerStateResponse,
    MediaPlayerSupportedFormat, SensorStateClass, SensorStateResponse, TextSensorStateResponse,
};
use crate::frame::{self, FrameTooLarge};

/// Key of the `media_player` entity.
pub const MEDIA_PLAYER_KEY: u32 = 1;
/// Key of the `action_button` event entity.
pub const ACTION_BUTTON_KEY: u32 = 2;
/// Key of the `ambient_light` sensor.
pub const AMBIENT_LIGHT_KEY: u32 = 3;
/// Key of the `alert_ringing` binary sensor.
pub const ALERT_RINGING_KEY: u32 = 4;
/// Key of the `stop_alert` button.
pub const STOP_ALERT_KEY: u32 = 5;
/// Key of the `voice_state` text sensor.
pub const VOICE_STATE_KEY: u32 = 6;
/// Key of the Bluetooth proxy's `ble_advertisements` sensor.
pub const BLE_ADVERTISEMENTS_KEY: u32 = 1;

/// The media player's feature flags in Home Assistant's numbering: pause (1),
/// volume set (4), mute (8), play media (512), stop (4096), play (16384),
/// browse (131072) and announce (1048576). Clients below API 1.11 derive the
/// same set from `supports_pause`.
pub const MEDIA_PLAYER_FEATURE_FLAGS: u32 = 1 | 4 | 8 | 512 | 4096 | 16384 | 131_072 | 1_048_576;

/// Which optional entities the satellite has.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VoiceEntities {
    /// The `action_button` event; only for a device announcing `button_hold`.
    pub action_button: bool,
    /// The `ambient_light` sensor; only for a device announcing
    /// `ambient_light` (its sensor is readable).
    pub ambient_light: bool,
}

impl VoiceEntities {
    /// The entities a device's announced capabilities call for.
    pub fn from_capabilities(capabilities: &Capabilities) -> Self {
        Self {
            action_button: capabilities.contains(Capability::ButtonHold),
            ambient_light: capabilities.contains(Capability::AmbientLight),
        }
    }
}

/// What the media player is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Playback {
    /// Nothing playing.
    Idle,
    /// Media playing.
    Playing,
    /// Media paused.
    Paused,
    /// An announcement playing. Reported to Home Assistant as playing, as
    /// ESPHome does: Home Assistant's media player has no announcing state.
    Announcing,
}

/// A device volume step, 0 to 127.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceVolume(u8);

impl DeviceVolume {
    /// The loudest step.
    pub const MAX: Self = Self(127);

    /// `step` if it is within 0 to 127.
    pub fn new(step: u8) -> Option<Self> {
        (step <= Self::MAX.0).then_some(Self(step))
    }

    /// The step.
    pub fn get(self) -> u8 {
        self.0
    }

    /// The step nearest Home Assistant's 0 to 1 level (`round(level·127)`),
    /// clamped to the range; `None` for a level that is not a number.
    pub fn from_home_assistant(level: f32) -> Option<Self> {
        if !level.is_finite() {
            return None;
        }
        let step = (level.clamp(0.0, 1.0) * 127.0).round();
        // The clamp keeps `step` within 0..=127.
        Some(Self(step as u8))
    }

    /// Home Assistant's 0 to 1 level for this step.
    pub fn to_home_assistant(self) -> f32 {
        f32::from(self.0) / 127.0
    }
}

/// The media player's state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaState {
    /// What is playing.
    pub playback: Playback,
    /// The volume.
    pub volume: DeviceVolume,
    /// Whether output is muted.
    pub muted: bool,
}

/// The `voice_state` text sensor's values.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum VoiceState {
    /// Waiting for a wake.
    Idle,
    /// Capturing a request.
    Listening,
    /// Waiting for the answer.
    Thinking,
    /// Speaking the answer.
    Speaking,
}

impl VoiceState {
    /// The text the sensor shows.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::Thinking => "thinking",
            Self::Speaking => "speaking",
        }
    }
}

/// The `action_button` event types.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ButtonEvent {
    /// A hold.
    Long,
    /// One tap.
    Single,
    /// Two taps.
    Double,
    /// Three taps.
    Triple,
}

impl ButtonEvent {
    /// Every type, in the order the entity lists them.
    pub const ALL: [Self; 4] = [Self::Long, Self::Single, Self::Double, Self::Triple];

    /// The event type's name in Home Assistant.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Single => "single",
            Self::Double => "double",
            Self::Triple => "triple",
        }
    }
}

/// The satellite's entity states. `None` is unknown: Home Assistant shows
/// the entity without a value until the runtime reports one.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VoiceStates {
    /// The media player.
    pub media: Option<MediaState>,
    /// Ambient light in lux; also `None` while the sensor has no reading.
    pub ambient_light_lux: Option<f32>,
    /// Whether an alarm or timer is ringing.
    pub alert_ringing: Option<bool>,
    /// The voice state.
    pub voice_state: Option<VoiceState>,
}

fn encode<M: crate::api::ApiMessage>(message: &M) -> Result<Bytes, FrameTooLarge> {
    frame::encode(message)
}

/// The voice satellite's entity list, in key order, ending with the
/// end-of-list message.
pub(crate) fn voice_entity_frames(
    label: &str,
    entities: VoiceEntities,
) -> Result<Vec<Bytes>, FrameTooLarge> {
    let format = |purpose: MediaPlayerFormatPurpose| MediaPlayerSupportedFormat {
        format: "flac".to_owned(),
        sample_rate: 48_000,
        num_channels: 1,
        purpose: purpose as i32,
        sample_bytes: 2,
    };
    let mut frames = vec![encode(&ListEntitiesMediaPlayerResponse {
        object_id: "media_player".to_owned(),
        key: MEDIA_PLAYER_KEY,
        name: label.to_owned(),
        supports_pause: true,
        supported_formats: vec![
            format(MediaPlayerFormatPurpose::Default),
            format(MediaPlayerFormatPurpose::Announcement),
        ],
        feature_flags: MEDIA_PLAYER_FEATURE_FLAGS,
        ..Default::default()
    })?];
    if entities.action_button {
        frames.push(encode(&ListEntitiesEventResponse {
            object_id: "action_button".to_owned(),
            key: ACTION_BUTTON_KEY,
            name: format!("{label} Action Button"),
            device_class: "button".to_owned(),
            event_types: ButtonEvent::ALL
                .iter()
                .map(|e| e.as_str().to_owned())
                .collect(),
            ..Default::default()
        })?);
    }
    if entities.ambient_light {
        frames.push(encode(&ListEntitiesSensorResponse {
            object_id: "ambient_light".to_owned(),
            key: AMBIENT_LIGHT_KEY,
            name: format!("{label} Ambient Light"),
            unit_of_measurement: "lx".to_owned(),
            accuracy_decimals: 0,
            device_class: "illuminance".to_owned(),
            state_class: SensorStateClass::StateClassMeasurement as i32,
            ..Default::default()
        })?);
    }
    frames.push(encode(&ListEntitiesBinarySensorResponse {
        object_id: "alert_ringing".to_owned(),
        key: ALERT_RINGING_KEY,
        name: format!("{label} Alert ringing"),
        device_class: "sound".to_owned(),
        ..Default::default()
    })?);
    frames.push(encode(&ListEntitiesButtonResponse {
        object_id: "stop_alert".to_owned(),
        key: STOP_ALERT_KEY,
        name: format!("{label} Stop alert"),
        device_class: "restart".to_owned(),
        ..Default::default()
    })?);
    frames.push(encode(&ListEntitiesTextSensorResponse {
        object_id: "voice_state".to_owned(),
        key: VOICE_STATE_KEY,
        name: format!("{label} Voice state"),
        icon: "mdi:account-voice".to_owned(),
        ..Default::default()
    })?);
    frames.push(encode(&ListEntitiesDoneResponse {})?);
    Ok(frames)
}

/// The Bluetooth proxy's entity list.
pub(crate) fn bluetooth_entity_frames(label: &str) -> Result<Vec<Bytes>, FrameTooLarge> {
    Ok(vec![
        encode(&ListEntitiesSensorResponse {
            object_id: "ble_advertisements".to_owned(),
            key: BLE_ADVERTISEMENTS_KEY,
            name: format!("{label} BLE advertisements"),
            icon: "mdi:bluetooth".to_owned(),
            accuracy_decimals: 0,
            state_class: SensorStateClass::StateClassTotalIncreasing as i32,
            entity_category: EntityCategory::Diagnostic as i32,
            ..Default::default()
        })?,
        encode(&ListEntitiesDoneResponse {})?,
    ])
}

/// The media player's state message; `None` while unknown, because the
/// message has no way to say "no state" and Home Assistant shows the entity
/// as unknown until the first one arrives.
pub(crate) fn media_frame(media: Option<MediaState>) -> Option<Bytes> {
    let media = media?;
    let state = match media.playback {
        Playback::Idle => MediaPlayerState::Idle,
        Playback::Playing | Playback::Announcing => MediaPlayerState::Playing,
        Playback::Paused => MediaPlayerState::Paused,
    };
    Some(
        encode(&MediaPlayerStateResponse {
            key: MEDIA_PLAYER_KEY,
            state: state as i32,
            volume: media.volume.to_home_assistant(),
            muted: media.muted,
            device_id: 0,
        })
        .expect("a fixed-size state fits a frame"),
    )
}

/// A sensor state, missing when `value` is `None` or not a number.
pub(crate) fn sensor_frame(key: u32, value: Option<f32>) -> Bytes {
    let value = value.filter(|v| v.is_finite());
    encode(&SensorStateResponse {
        key,
        state: value.unwrap_or_default(),
        missing_state: value.is_none(),
        device_id: 0,
    })
    .expect("a fixed-size state fits a frame")
}

pub(crate) fn alert_ringing_frame(ringing: Option<bool>) -> Bytes {
    encode(&BinarySensorStateResponse {
        key: ALERT_RINGING_KEY,
        state: ringing.unwrap_or_default(),
        missing_state: ringing.is_none(),
        device_id: 0,
    })
    .expect("a fixed-size state fits a frame")
}

pub(crate) fn voice_state_frame(state: Option<VoiceState>) -> Bytes {
    encode(&TextSensorStateResponse {
        key: VOICE_STATE_KEY,
        state: state.map(VoiceState::as_str).unwrap_or_default().to_owned(),
        missing_state: state.is_none(),
        device_id: 0,
    })
    .expect("a fixed-size state fits a frame")
}

pub(crate) fn button_event_frame(event: ButtonEvent) -> Bytes {
    encode(&EventResponse {
        key: ACTION_BUTTON_KEY,
        event_type: event.as_str().to_owned(),
        device_id: 0,
    })
    .expect("a fixed-size event fits a frame")
}

/// Every state the satellite reports, for a client that just subscribed.
pub(crate) fn all_voice_state_frames(entities: VoiceEntities, states: &VoiceStates) -> Vec<Bytes> {
    let mut frames = Vec::new();
    frames.extend(media_frame(states.media));
    if entities.ambient_light {
        frames.push(sensor_frame(AMBIENT_LIGHT_KEY, states.ambient_light_lux));
    }
    frames.push(alert_ringing_frame(states.alert_ringing));
    frames.push(voice_state_frame(states.voice_state));
    frames
}

/// The frames for the states that differ between `before` and `after`.
pub(crate) fn changed_voice_state_frames(
    entities: VoiceEntities,
    before: &VoiceStates,
    after: &VoiceStates,
) -> Vec<Bytes> {
    let mut frames = Vec::new();
    if before.media != after.media {
        frames.extend(media_frame(after.media));
    }
    if entities.ambient_light && before.ambient_light_lux != after.ambient_light_lux {
        frames.push(sensor_frame(AMBIENT_LIGHT_KEY, after.ambient_light_lux));
    }
    if before.alert_ringing != after.alert_ringing {
        frames.push(alert_ringing_frame(after.alert_ringing));
    }
    if before.voice_state != after.voice_state {
        frames.push(voice_state_frame(after.voice_state));
    }
    frames
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_crosses_as_round_level_times_127() {
        assert_eq!(DeviceVolume::from_home_assistant(0.5).unwrap().get(), 64);
        assert_eq!(DeviceVolume::from_home_assistant(1.0).unwrap().get(), 127);
        assert_eq!(DeviceVolume::from_home_assistant(0.0).unwrap().get(), 0);
        assert_eq!(DeviceVolume::from_home_assistant(1.5).unwrap().get(), 127);
        assert_eq!(DeviceVolume::from_home_assistant(-0.2).unwrap().get(), 0);
        assert_eq!(DeviceVolume::from_home_assistant(f32::NAN), None);
        assert_eq!(DeviceVolume::new(128), None);
        assert_eq!(DeviceVolume::MAX.to_home_assistant(), 1.0);
        for step in 0..=127 {
            let volume = DeviceVolume::new(step).unwrap();
            assert_eq!(
                DeviceVolume::from_home_assistant(volume.to_home_assistant()),
                Some(volume)
            );
        }
    }

    #[test]
    fn the_media_feature_flags_match_home_assistants_derivation() {
        // What Home Assistant computes from supports_pause below API 1.11.
        assert_eq!(MEDIA_PLAYER_FEATURE_FLAGS, 1_200_653);
    }

    #[test]
    fn capabilities_choose_the_optional_entities() {
        let both = Capabilities::from_names(["button_hold", "ambient_light", "leds"]);
        assert_eq!(
            VoiceEntities::from_capabilities(&both),
            VoiceEntities {
                action_button: true,
                ambient_light: true
            }
        );
        let neither = Capabilities::from_names(["buttons", "leds"]);
        assert_eq!(
            VoiceEntities::from_capabilities(&neither),
            VoiceEntities::default()
        );
    }

    #[test]
    fn only_changed_states_are_sent() {
        let entities = VoiceEntities::default();
        let before = VoiceStates::default();
        let after = VoiceStates {
            alert_ringing: Some(true),
            // Not an entity here, so not sent.
            ambient_light_lux: Some(3.0),
            ..VoiceStates::default()
        };
        let frames = changed_voice_state_frames(entities, &before, &after);
        assert_eq!(frames, vec![alert_ringing_frame(Some(true))]);
    }
}
