//! The settings: their sections, their keys, the typed [`DeviceSettings`], and how the
//! fleet's values and a device's overridden sections combine.
//!
//! The 9 sections and their 39 keys are the dashboard's `CONFIG_SECTIONS`
//! (`controller/static/dashboard.jsx`); the 40th key, `startupVolume`, is device state,
//! the dashboard's `STATE_KEYS`: always the device's own value, whatever its sections
//! say (configuration.md "Volume").

use std::collections::BTreeSet;
use std::fmt;

use echomuse_protocol::{Capabilities, Capability};

use crate::json::Json;
use crate::values::*;

/// A settings section: the unit a device overrides the fleet by.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Section {
    /// EQ, speech boost and duck depth.
    Playback,
    /// The wake model, near-miss floor, wake clips, arbitration, chime and rules.
    Wakeword,
    /// The speech copy sent to Home Assistant ("Speech" on the dashboard).
    Microphones,
    /// The LED ring.
    Ring,
    /// The action button ("Button" on the dashboard).
    Advanced,
    /// The Bluetooth proxy.
    Bluetooth,
    /// Timer and alarm rings.
    Timers,
    /// Transcript correction.
    Transcript,
    /// The Echo Show's screen.
    Screen,
}

impl Section {
    /// Every section, in the dashboard's order.
    pub const ALL: [Section; 9] = [
        Self::Playback,
        Self::Wakeword,
        Self::Microphones,
        Self::Ring,
        Self::Advanced,
        Self::Bluetooth,
        Self::Timers,
        Self::Transcript,
        Self::Screen,
    ];

    /// The section id as `config_sections` stores it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Playback => "playback",
            Self::Wakeword => "wakeword",
            Self::Microphones => "microphones",
            Self::Ring => "ring",
            Self::Advanced => "advanced",
            Self::Bluetooth => "bluetooth",
            Self::Timers => "timers",
            Self::Transcript => "transcript",
            Self::Screen => "screen",
        }
    }

    /// The section with this stored id.
    pub fn from_wire(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// The keys the section owns, in the dashboard's order.
    pub fn keys(self) -> impl Iterator<Item = SettingKey> {
        SettingKey::ALL
            .iter()
            .copied()
            .filter(move |k| k.scope() == Scope::Section(self))
    }
}

impl fmt::Display for Section {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The sections a device overrides.
pub type SectionSet = BTreeSet<Section>;

/// Where a key's effective value comes from.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Scope {
    /// The fleet's value, or the device's own while it overrides this section.
    Section(Section),
    /// Device state: the device's own value when it has one, whatever its sections.
    State,
}

/// Keys removed from the configuration (post-afe-audio-architecture.md §18.4). An edit
/// naming one is refused as removed rather than as unknown; a stored document that
/// still carries one is reported.
pub const REMOVED_KEYS: &[&str] = &[
    "owwModel",
    "owwThreshold",
    "bargeInThreshold",
    "owwOnDevice",
    "owwSpeexNs",
    "bargeInEnabled",
    "endpointRelative",
    "endpointLowPerMil",
    "endpointSilenceMs",
    "endpointBackporchMs",
    "maxSpeechMs",
    "vadThreshold",
    "vadSpeechMs",
    "vadSilenceMs",
    "timerRingBurstSeconds",
    "replyOnset",
];

/// A capability condition under which a setting has an effect on a device. The reason
/// shown for an unmet one is the caller's (it depends on the platform, §4.10).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Requirement {
    /// The device must announce this capability.
    Has(Capability),
    /// The setting goes unused on a device announcing this capability (a LineageOS
    /// Show's own app lights the screen, so the kiosk API settings do nothing there).
    Lacks(Capability),
}

impl Requirement {
    /// Whether `capabilities` meet it.
    pub fn met_by(self, capabilities: &Capabilities) -> bool {
        match self {
            Self::Has(c) => capabilities.contains(c),
            Self::Lacks(c) => !capabilities.contains(c),
        }
    }
}

macro_rules! settings_table {
    ($( $(#[$doc:meta])* $variant:ident => $wire:literal, $scope:expr, $group:ident . $field:ident : $ty:ty; )+) => {
        /// One of the 40 setting keys.
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum SettingKey {
            $( $(#[$doc])* $variant, )+
        }

        impl SettingKey {
            /// Every key: by section in the dashboard's order, then `startupVolume`.
            pub const ALL: &'static [SettingKey] = &[$(Self::$variant,)+];

            /// The key as stored and sent by the dashboard.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire,)+
                }
            }

            /// The key with this stored name.
            pub fn from_wire(text: &str) -> Option<Self> {
                match text {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// Its section, or device state.
            pub const fn scope(self) -> Scope {
                match self {
                    $(Self::$variant => $scope,)+
                }
            }
        }

        /// One setting with its typed value.
        #[derive(Clone, Debug, PartialEq)]
        pub enum SettingValue {
            $( $(#[$doc])* $variant($ty), )+
        }

        impl SettingValue {
            /// Its key.
            pub const fn key(&self) -> SettingKey {
                match self {
                    $(Self::$variant(_) => SettingKey::$variant,)+
                }
            }

            /// Parse and validate the JSON value of `key`.
            pub fn parse(key: SettingKey, value: &Json) -> Result<Self, ValueError> {
                match key {
                    $(SettingKey::$variant => <$ty as SettingType>::from_json(value).map(Self::$variant),)+
                }
            }

            /// The stored JSON form.
            pub fn to_json(&self) -> Json {
                match self {
                    $(Self::$variant(v) => SettingType::to_json(v),)+
                }
            }
        }

        impl DeviceSettings {
            /// The value of `key`.
            pub fn get(&self, key: SettingKey) -> SettingValue {
                match key {
                    $(SettingKey::$variant => SettingValue::$variant(self.$group.$field.clone()),)+
                }
            }

            /// Replace one value.
            pub fn set(&mut self, value: SettingValue) {
                match value {
                    $(SettingValue::$variant(v) => self.$group.$field = v,)+
                }
            }
        }
    };
}

use Section::*;

settings_table! {
    /// `eqBands`.
    EqBands => "eqBands", Scope::Section(Playback), playback.eq_bands: EqBands;
    /// `eqLoudness`.
    EqLoudness => "eqLoudness", Scope::Section(Playback), playback.eq_loudness: bool;
    /// `duckDb`.
    DuckDb => "duckDb", Scope::Section(Playback), playback.duck_db: DuckDb;
    /// `wakeModel`.
    WakeModel => "wakeModel", Scope::Section(Wakeword), wakeword.wake_model: WakeModelChoice;
    /// `nearMissThreshold`.
    NearMissThreshold => "nearMissThreshold", Scope::Section(Wakeword), wakeword.near_miss_threshold: NearMissFloor;
    /// `saveWakeClips`.
    SaveWakeClips => "saveWakeClips", Scope::Section(Wakeword), wakeword.save_wake_clips: bool;
    /// `wakeArbitrationMs`.
    WakeArbitrationMs => "wakeArbitrationMs", Scope::Section(Wakeword), wakeword.wake_arbitration_ms: ArbitrationWindowMs;
    /// `wakeSound`.
    WakeSound => "wakeSound", Scope::Section(Wakeword), wakeword.wake_sound: bool;
    /// `wakeOpenRules`.
    WakeOpenRules => "wakeOpenRules", Scope::Section(Wakeword), wakeword.wake_open_rules: ExtraOpenRules;
    /// `wakeShadowRules`.
    WakeShadowRules => "wakeShadowRules", Scope::Section(Wakeword), wakeword.wake_shadow_rules: ShadowRules;
    /// `nsAsr`.
    NsAsr => "nsAsr", Scope::Section(Microphones), microphones.ns_asr: bool;
    /// `saveUtterances`.
    SaveUtterances => "saveUtterances", Scope::Section(Microphones), microphones.save_utterances: bool;
    /// `extendedUtterances`.
    ExtendedUtterances => "extendedUtterances", Scope::Section(Microphones), microphones.extended_utterances: bool;
    /// `pauseAsr`.
    PauseAsr => "pauseAsr", Scope::Section(Microphones), microphones.pause_asr: PauseAsr;
    /// `ledScene`.
    LedScene => "ledScene", Scope::Section(Ring), ring.led_scene: LedScene;
    /// `ledListenColor`.
    LedListenColor => "ledListenColor", Scope::Section(Ring), ring.led_listen_color: ListenColor;
    /// `ledThinkColor`.
    LedThinkColor => "ledThinkColor", Scope::Section(Ring), ring.led_think_color: ThinkColor;
    /// `meterAttack`.
    MeterAttack => "meterAttack", Scope::Section(Ring), ring.meter_attack: MeterAttack;
    /// `meterDecay`.
    MeterDecay => "meterDecay", Scope::Section(Ring), ring.meter_decay: MeterDecay;
    /// `meterFloor`.
    MeterFloor => "meterFloor", Scope::Section(Ring), ring.meter_floor: MeterFloor;
    /// `meterGamma`.
    MeterGamma => "meterGamma", Scope::Section(Ring), ring.meter_gamma: MeterGamma;
    /// `meterRef`.
    MeterRef => "meterRef", Scope::Section(Ring), ring.meter_ref: MeterRef;
    /// `meterCurve`.
    MeterCurve => "meterCurve", Scope::Section(Ring), ring.meter_curve: MeterCurve;
    /// `buttonSingleTapEvent`.
    ButtonSingleTapEvent => "buttonSingleTapEvent", Scope::Section(Advanced), advanced.button_single_tap_event: bool;
    /// `buttonMultiTapMs`.
    ButtonMultiTapMs => "buttonMultiTapMs", Scope::Section(Advanced), advanced.button_multi_tap_ms: MultiTapMs;
    /// `bleProxyEnabled`.
    BleProxyEnabled => "bleProxyEnabled", Scope::Section(Bluetooth), bluetooth.ble_proxy_enabled: bool;
    /// `timerSound`.
    TimerSound => "timerSound", Scope::Section(Timers), timers.timer_sound: TimerSound;
    /// `timerRingSeconds`.
    TimerRingSeconds => "timerRingSeconds", Scope::Section(Timers), timers.timer_ring_seconds: TimerRingSeconds;
    /// `timerRingGapSeconds`.
    TimerRingGapSeconds => "timerRingGapSeconds", Scope::Section(Timers), timers.timer_ring_gap_seconds: TimerRingGapSeconds;
    /// `alarmSound`.
    AlarmSound => "alarmSound", Scope::Section(Timers), timers.alarm_sound: AlarmSound;
    /// `transcriptFix`.
    TranscriptFix => "transcriptFix", Scope::Section(Transcript), transcript.transcript_fix: TranscriptFixMode;
    /// `transcriptFixTerms`.
    TranscriptFixTerms => "transcriptFixTerms", Scope::Section(Transcript), transcript.transcript_fix_terms: FixTerms;
    /// `transcriptFixPhrases`.
    TranscriptFixPhrases => "transcriptFixPhrases", Scope::Section(Transcript), transcript.transcript_fix_phrases: FixPhrases;
    /// `transcriptFixPronunciations`.
    TranscriptFixPronunciations => "transcriptFixPronunciations", Scope::Section(Transcript), transcript.transcript_fix_pronunciations: Pronunciations;
    /// `screenWake`.
    ScreenWake => "screenWake", Scope::Section(Screen), screen.screen_wake: bool;
    /// `screenWakeUrl`.
    ScreenWakeUrl => "screenWakeUrl", Scope::Section(Screen), screen.screen_wake_url: KioskUrl;
    /// `screenWakePassword`.
    ScreenWakePassword => "screenWakePassword", Scope::Section(Screen), screen.screen_wake_password: KioskPassword;
    /// `kioskPort`.
    KioskPort => "kioskPort", Scope::Section(Screen), screen.kiosk_port: KioskPort;
    /// `turnViewLinger`.
    TurnViewLinger => "turnViewLinger", Scope::Section(Screen), screen.turn_view_linger: TurnViewLinger;
    /// `startupVolume`.
    StartupVolume => "startupVolume", Scope::State, state.startup_volume: StartupVolume;
}

impl SettingKey {
    /// Its section; `None` for device state.
    pub const fn section(self) -> Option<Section> {
        match self.scope() {
            Scope::Section(s) => Some(s),
            Scope::State => None,
        }
    }

    /// The capability conditions under which this setting has an effect, as the
    /// dashboard gates its controls. Empty when it applies on every device.
    pub const fn requirements(self) -> &'static [Requirement] {
        use Capability as C;
        use Requirement::{Has, Lacks};
        match self {
            Self::DuckDb => &[Has(C::FocusLeases)],
            Self::WakeModel => &[Has(C::DeviceWake)],
            Self::WakeSound => &[Has(C::RenderProgress)],
            Self::WakeOpenRules | Self::WakeShadowRules => &[Has(C::OpenRules)],
            Self::LedScene | Self::LedListenColor => &[Has(C::Leds)],
            Self::LedThinkColor
            | Self::MeterAttack
            | Self::MeterDecay
            | Self::MeterFloor
            | Self::MeterGamma
            | Self::MeterRef
            | Self::MeterCurve => &[Has(C::Leds), Has(C::LedAnim)],
            Self::ButtonSingleTapEvent | Self::ButtonMultiTapMs => &[Has(C::ButtonHold)],
            Self::BleProxyEnabled => &[Has(C::BleProxy)],
            Self::ScreenWake => &[Has(C::ScreenWake)],
            Self::ScreenWakeUrl | Self::ScreenWakePassword => {
                &[Has(C::ScreenWake), Lacks(C::ScreenOverlay)]
            }
            Self::KioskPort => &[Has(C::KioskPlugin)],
            Self::TurnViewLinger => &[Has(C::TurnView)],
            _ => &[],
        }
    }

    /// The conditions `capabilities` do not meet, in [`requirements`](Self::requirements)
    /// order; empty when the setting applies on this device.
    pub fn unmet(self, capabilities: &Capabilities) -> Vec<Requirement> {
        self.requirements()
            .iter()
            .copied()
            .filter(|r| !r.met_by(capabilities))
            .collect()
    }

    /// Whether changing this setting needs the device's next `session.ready`: the wake
    /// graph, the open or shadow rules and the near-miss floor (spec §3.1.4).
    pub const fn needs_new_session(self) -> bool {
        matches!(
            self,
            Self::WakeModel | Self::WakeOpenRules | Self::WakeShadowRules | Self::NearMissThreshold
        )
    }
}

impl fmt::Display for SettingKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `playback`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackSettings {
    /// `eqBands`.
    pub eq_bands: EqBands,
    /// `eqLoudness`: a +5 dB presence boost at 2.5 kHz. Default off.
    pub eq_loudness: bool,
    /// `duckDb`.
    pub duck_db: DuckDb,
}

/// `wakeword`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WakewordSettings {
    /// `wakeModel`.
    pub wake_model: WakeModelChoice,
    /// `nearMissThreshold`.
    pub near_miss_threshold: NearMissFloor,
    /// `saveWakeClips`: keep wake clips and near-miss recordings. Default off.
    pub save_wake_clips: bool,
    /// `wakeArbitrationMs`.
    pub wake_arbitration_ms: ArbitrationWindowMs,
    /// `wakeSound`: the wake chime. Default off.
    pub wake_sound: bool,
    /// `wakeOpenRules`.
    pub wake_open_rules: ExtraOpenRules,
    /// `wakeShadowRules`.
    pub wake_shadow_rules: ShadowRules,
}

/// `microphones` ("Speech").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeechSettings {
    /// `nsAsr`: DTLN noise suppression on the speech-to-text copy. Default off.
    pub ns_asr: bool,
    /// `saveUtterances`: keep each turn's audio. Default off.
    pub save_utterances: bool,
    /// `extendedUtterances`: 30 s requests instead of 15 s. Default off.
    pub extended_utterances: bool,
    /// `pauseAsr`.
    pub pause_asr: PauseAsr,
}

/// `ring`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RingSettings {
    /// `ledScene`.
    pub led_scene: LedScene,
    /// `ledListenColor`.
    pub led_listen_color: ListenColor,
    /// `ledThinkColor`.
    pub led_think_color: ThinkColor,
    /// `meterAttack`.
    pub meter_attack: MeterAttack,
    /// `meterDecay`.
    pub meter_decay: MeterDecay,
    /// `meterFloor`.
    pub meter_floor: MeterFloor,
    /// `meterGamma`.
    pub meter_gamma: MeterGamma,
    /// `meterRef`.
    pub meter_ref: MeterRef,
    /// `meterCurve`.
    pub meter_curve: MeterCurve,
}

/// `advanced` ("Button").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ButtonSettings {
    /// `buttonSingleTapEvent`: a tap fires the `single` event instead of starting a
    /// conversation. Default off.
    pub button_single_tap_event: bool,
    /// `buttonMultiTapMs`.
    pub button_multi_tap_ms: MultiTapMs,
}

/// `bluetooth`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BluetoothSettings {
    /// `bleProxyEnabled`: the Home Assistant Bluetooth proxy. Default off.
    pub ble_proxy_enabled: bool,
}

/// `timers` ("Timers & alarms").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimerSettings {
    /// `timerSound`.
    pub timer_sound: TimerSound,
    /// `timerRingSeconds`.
    pub timer_ring_seconds: TimerRingSeconds,
    /// `timerRingGapSeconds`.
    pub timer_ring_gap_seconds: TimerRingGapSeconds,
    /// `alarmSound`.
    pub alarm_sound: AlarmSound,
}

/// `transcript` ("Transcript correction").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptSettings {
    /// `transcriptFix`.
    pub transcript_fix: TranscriptFixMode,
    /// `transcriptFixTerms`.
    pub transcript_fix_terms: FixTerms,
    /// `transcriptFixPhrases`.
    pub transcript_fix_phrases: FixPhrases,
    /// `transcriptFixPronunciations`.
    pub transcript_fix_pronunciations: Pronunciations,
}

/// `screen`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScreenSettings {
    /// `screenWake`: light the kiosk screen on wake. Default off.
    pub screen_wake: bool,
    /// `screenWakeUrl`.
    pub screen_wake_url: KioskUrl,
    /// `screenWakePassword`.
    pub screen_wake_password: KioskPassword,
    /// `kioskPort`.
    pub kiosk_port: KioskPort,
    /// `turnViewLinger`.
    pub turn_view_linger: TurnViewLinger,
}

/// Device state kept with the settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceState {
    /// `startupVolume`.
    pub startup_volume: StartupVolume,
}

/// Every setting, typed. As a device's effective settings it is the fleet's values with
/// the device's overridden sections and its own state layered over them; as the fleet's
/// it is the fleet document over the defaults. [`Default`] is the product defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceSettings {
    /// Section `playback`.
    pub playback: PlaybackSettings,
    /// Section `wakeword`.
    pub wakeword: WakewordSettings,
    /// Section `microphones`.
    pub microphones: SpeechSettings,
    /// Section `ring`.
    pub ring: RingSettings,
    /// Section `advanced`.
    pub advanced: ButtonSettings,
    /// Section `bluetooth`.
    pub bluetooth: BluetoothSettings,
    /// Section `timers`.
    pub timers: TimerSettings,
    /// Section `transcript`.
    pub transcript: TranscriptSettings,
    /// Section `screen`.
    pub screen: ScreenSettings,
    /// Device state.
    pub state: DeviceState,
}

impl DeviceSettings {
    /// Every value as one JSON object, keys in [`SettingKey::ALL`] order: the shape the
    /// dashboard reads. It includes the kiosk password, which the dashboard edits.
    pub fn to_json(&self) -> Json {
        Json::Object(
            SettingKey::ALL
                .iter()
                .map(|k| (k.as_str().to_owned(), self.get(*k).to_json()))
                .collect(),
        )
    }

    /// The keys whose values differ from `other`'s.
    pub fn differences(&self, other: &DeviceSettings) -> Vec<SettingKey> {
        SettingKey::ALL
            .iter()
            .copied()
            .filter(|k| self.get(*k) != other.get(*k))
            .collect()
    }
}

impl serde::Serialize for DeviceSettings {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_json().serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forty_keys_in_nine_sections() {
        assert_eq!(SettingKey::ALL.len(), 40);
        let per_section: Vec<usize> = Section::ALL.iter().map(|s| s.keys().count()).collect();
        assert_eq!(per_section, [3, 7, 4, 9, 2, 1, 4, 4, 5]);
        for key in SettingKey::ALL {
            assert_eq!(SettingKey::from_wire(key.as_str()), Some(*key));
            assert!(!REMOVED_KEYS.contains(&key.as_str()));
        }
        assert_eq!(SettingKey::StartupVolume.scope(), Scope::State);
        for section in Section::ALL {
            assert_eq!(Section::from_wire(section.as_str()), Some(section));
        }
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let d = DeviceSettings::default();
        let text = d.to_json().to_python_text();
        assert_eq!(
            text,
            concat!(
                r#"{"eqBands": [0, 0, 0, 0, 0, 0, 0, 0], "eqLoudness": false, "duckDb": -18, "#,
                r#""wakeModel": "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f", "#,
                r#""nearMissThreshold": 0.17, "saveWakeClips": false, "wakeArbitrationMs": 700, "#,
                r#""wakeSound": false, "wakeOpenRules": [], "wakeShadowRules": ["#,
                r#"{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.9}, "#,
                r#"{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.95}, "#,
                r#"{"profile": "idle", "windows": 2, "combine": "all", "threshold": 0.9}, "#,
                r#"{"profile": "idle", "windows": 2, "combine": "all", "threshold": 0.95}, "#,
                r#"{"profile": "idle", "windows": 1, "combine": "mean", "threshold": 0.97}], "#,
                r#""nsAsr": false, "saveUtterances": false, "extendedUtterances": false, "#,
                r#""pauseAsr": {"engine": "kroko", "host": "", "port": 10300, "model": "", "language": "en"}, "#,
                r##""ledScene": "standard", "ledListenColor": "#00b400", "ledThinkColor": "#00c800", "##,
                r#""meterAttack": 0.6, "meterDecay": 0.3, "meterFloor": 0.06, "meterGamma": 2.2, "#,
                r#""meterRef": 0.22, "meterCurve": 0.7, "buttonSingleTapEvent": false, "#,
                r#""buttonMultiTapMs": 0, "bleProxyEnabled": false, "timerSound": "", "#,
                r#""timerRingSeconds": 900, "timerRingGapSeconds": 2, "alarmSound": "", "#,
                r#""transcriptFix": "shadow", "transcriptFixTerms": [], "transcriptFixPhrases": [], "#,
                r#""transcriptFixPronunciations": {}, "screenWake": false, "#,
                r#""screenWakeUrl": "http://127.0.0.1:2323", "screenWakePassword": "", "#,
                r#""kioskPort": 2324, "turnViewLinger": 6, "startupVolume": 85}"#
            )
        );
        // Every default parses back as itself.
        for key in SettingKey::ALL {
            let value = d.get(*key);
            assert_eq!(
                SettingValue::parse(*key, &value.to_json()).unwrap(),
                value,
                "{key}"
            );
        }
    }

    #[test]
    fn requirements_follow_capabilities() {
        let show = Capabilities::from_names(["screen_wake", "screen_overlay", "leds"]);
        assert!(SettingKey::ScreenWake.unmet(&show).is_empty());
        assert_eq!(
            SettingKey::ScreenWakeUrl.unmet(&show),
            [Requirement::Lacks(Capability::ScreenOverlay)]
        );
        assert_eq!(
            SettingKey::MeterDecay.unmet(&show),
            [Requirement::Has(Capability::LedAnim)]
        );
        assert!(
            SettingKey::EqBands
                .unmet(&Capabilities::default())
                .is_empty()
        );
    }
}
