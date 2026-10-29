//! Closed vocabularies of the device link: message types, capabilities,
//! platforms, rejection reasons, acknowledgement statuses and stream names,
//! plus the SHA-256 text form the link uses for assets and digests.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Serialize, Serializer};

use crate::ema::Kind;

wire_enum! {
    /// An envelope `type` (docs/protocol-v1.md §4, `device/internal/proto`).
    pub enum MessageType {
        /// D→C, the first message of every session.
        SessionHello => "session.hello",
        /// C→D, admits the session.
        SessionReady => "session.ready",
        /// C→D, refuses the session.
        SessionRejected => "session.rejected",
        /// Both directions, every second.
        Heartbeat => "heartbeat",
        /// D→C, asks for the controller's UTC time.
        ClockRequest => "clock.request",
        /// C→D, answers `clock.request`.
        ClockReply => "clock.reply",
        /// Both directions, acknowledges a command by its `message_id`.
        CommandAck => "command.ack",
        /// Both directions, reports a violation before closing.
        ProtocolError => "protocol.error",
        /// D→C, announces an uplink stream epoch.
        StreamOpen => "stream.open",
        /// D→C, ends an uplink stream epoch.
        StreamEnd => "stream.end",
        /// C→D, starts a playback.
        RenderStart => "render.start",
        /// C→D, marks where a network playback's audio ends.
        RenderEnd => "render.end",
        /// C→D, cancels a playback.
        RenderCancel => "render.cancel",
        /// D→C, playback progress.
        RenderProgress => "render.progress",
        /// D→C, a playback finished.
        RenderFinished => "render.finished",
        /// C→D, takes dialog focus.
        FocusAcquire => "focus.acquire",
        /// C→D, renews dialog focus.
        FocusRenew => "focus.renew",
        /// C→D, releases dialog focus.
        FocusRelease => "focus.release",
        /// D→C, the device heard its wake word.
        WakeCandidate => "wake.candidate",
        /// D→C, a wake candidate closed.
        WakeCandidateEnd => "wake.candidate_end",
        /// D→C, wake detector statistics.
        WakeStats => "wake.stats",
        /// D→C, a near miss that opened no candidate (`near_miss_v1`).
        WakeNearMiss => "wake.near_miss",
        /// C→D, opens an audio subscription.
        UplinkOpen => "uplink.open",
        /// C→D, renews or converts an audio subscription.
        UplinkRenew => "uplink.renew",
        /// C→D, closes an audio subscription.
        UplinkClose => "uplink.close",
        /// D→C, an audio subscription ended.
        UplinkEnded => "uplink.ended",
        /// D→C, the privacy mute changed.
        PrivacyChanged => "privacy.changed",
        /// D→C, a button press.
        ButtonAction => "button.action",
        /// C→D, one page of the alert cache.
        AlertSnapshot => "alert.snapshot",
        /// C→D, changes to the alert cache.
        AlertDelta => "alert.delta",
        /// D→C, acknowledges alert deliveries.
        AlertAck => "alert.ack",
        /// D→C, a local dismiss, snooze or expiry.
        AlertLocalOperation => "alert.local_operation",
        /// C→D, answers `alert.local_operation`.
        AlertOpResult => "alert.op_result",
        /// C→D, dismisses or snoozes a ringing alert.
        AlertAct => "alert.act",
        /// C→D, rings a timer.
        AlertRing => "alert.ring",
        /// C→D, names alert sounds to install (`alert_prefetch`).
        AlertPrefetch => "alert.prefetch",
        /// D→C, a ring ended.
        AlertRingEnded => "alert.ring_ended",
        /// D→C, the device's alert state.
        AlertState => "alert.state",
        /// C→D, the turn view (`turn_view_v1`).
        TurnView => "turn.view",
        /// C→D, the timer view (`timer_view_v1`).
        TimerView => "timer.view",
        /// C→D, LED colours.
        Leds => "leds",
        /// C→D, an LED animation.
        LedAnim => "led_anim",
        /// C→D, sets the volume.
        VolumeSet => "volume_set",
        /// C→D, device configuration.
        Config => "config",
        /// C→D, asks the device to dial back a shell.
        ShellOpen => "shell_open",
        /// C→D, closes the shell.
        ShellClose => "shell_close",
        /// C→D, changes Wi-Fi (`wifi_control`).
        WifiChange => "wifi_change",
        /// C→D, keeps a Wi-Fi change (`wifi_control`).
        WifiCommit => "wifi_commit",
        /// C→D, scans Wi-Fi (`wifi_control`).
        WifiScan => "wifi_scan",
        /// C→D, round-trip probe.
        Ping => "ping",
        /// D→C, the device's volume.
        VolumeState => "volume_state",
        /// D→C, an ambient light reading.
        AmbientLight => "ambient_light",
        /// D→C, a device log line.
        Log => "log",
        /// D→C, the outcome of `wifi_change`.
        WifiResult => "wifi_result",
        /// D→C, the outcome of `wifi_scan`.
        WifiScanResult => "wifi_scan_result",
        /// D→C, BLE advertisements (`ble_proxy`).
        BleAdverts => "ble_adverts",
        /// D→C, device statistics.
        Stats => "stats",
        /// D→C, answers `ping`.
        Pong => "pong",
    }
}

impl MessageType {
    /// Whether the device sends this type.
    pub const fn sent_by_device(self) -> bool {
        use MessageType::*;
        matches!(
            self,
            SessionHello
                | Heartbeat
                | ClockRequest
                | CommandAck
                | ProtocolError
                | StreamOpen
                | StreamEnd
                | RenderProgress
                | RenderFinished
                | WakeCandidate
                | WakeCandidateEnd
                | WakeStats
                | WakeNearMiss
                | UplinkEnded
                | PrivacyChanged
                | ButtonAction
                | AlertAck
                | AlertLocalOperation
                | AlertRingEnded
                | AlertState
                | VolumeState
                | AmbientLight
                | Log
                | WifiResult
                | WifiScanResult
                | BleAdverts
                | Stats
                | Pong
        )
    }

    /// Whether the controller sends this type.
    pub const fn sent_by_controller(self) -> bool {
        use MessageType::*;
        matches!(self, Heartbeat | CommandAck | ProtocolError) || !self.sent_by_device()
    }
}

wire_enum! {
    /// A `session.hello` capability name the controller knows
    /// (spec §3.1.4 and §3.1.9).
    pub enum Capability {
        /// Required: EMA1 audio timeline.
        AudioTimeline => "audio_timeline_v1",
        /// Required: audio subscriptions (the wire keeps the older name).
        UplinkSubscriptions => "uplink_leases_v1",
        /// Required: wake detection on the device.
        DeviceWake => "device_wake_v1",
        /// Required: the final-mix reference stream.
        RenderReference => "render_reference_v1",
        /// Required: `render.progress`.
        RenderProgress => "render_progress_v1",
        /// Required: dialog focus leases.
        FocusLeases => "focus_leases_v1",
        /// Required: the durable alert cache.
        AlertCache => "alert_cache_v1",
        /// Required: the turn protocol.
        TurnProtocol => "turn_protocol_v1",
        /// A LED ring or its on-screen equivalent.
        Leds => "leds",
        /// LED animations.
        LedAnim => "led_anim",
        /// Buttons.
        Buttons => "buttons",
        /// Button holds.
        ButtonHold => "button_hold",
        /// A readable ambient light sensor.
        AmbientLight => "ambient_light",
        /// Installs the sounds `alert.prefetch` names.
        AlertPrefetch => "alert_prefetch",
        /// Plays the wake chime itself.
        LocalWakeChime => "local_wake_chime",
        /// Accepts the `builtin:no_input_chime` earcon.
        NoInputChime => "no_input_chime",
        /// Accepts the `builtin:announcement_chime` earcon.
        AnnouncementChime => "announcement_chime",
        /// Opens candidates on `detector.open_rules`.
        OpenRules => "open_rules_v1",
        /// Reports near misses and takes `near_miss` subscriptions.
        NearMiss => "near_miss_v1",
        /// Serves the `afe` stream when `session.ready` asks for it.
        AfeMetadata => "afe_metadata_v1",
        /// Dot only: handles the Wi-Fi messages.
        WifiControl => "wifi_control",
        /// Dot only: sends BLE advertisements.
        BleProxy => "ble_proxy",
        /// Dot only: a hardware privacy mute.
        PrivacyMute => "privacy_mute",
        /// Echo Show: lights the screen on wake.
        ScreenWake => "screen_wake",
        /// LineageOS Show: the companion app's light bar and touch zones.
        ScreenOverlay => "screen_overlay",
        /// LineageOS Show: accepts `turn.view`.
        TurnView => "turn_view_v1",
        /// LineageOS Show: accepts `timer.view`.
        TimerView => "timer_view_v1",
        /// LineageOS Show: installs the Kiosk Satellite plugin.
        KioskPlugin => "kiosk_plugin",
    }
}

/// The eight protocol capabilities a device must announce to be admitted
/// (spec §3.1.4).
pub const REQUIRED_CAPABILITIES: [Capability; 8] = [
    Capability::AudioTimeline,
    Capability::UplinkSubscriptions,
    Capability::DeviceWake,
    Capability::RenderReference,
    Capability::RenderProgress,
    Capability::FocusLeases,
    Capability::AlertCache,
    Capability::TurnProtocol,
];

/// The capabilities one hello announced. Names this controller does not
/// know are kept, apart, and otherwise ignored (spec §3.1.3).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    known: BTreeSet<Capability>,
    unknown: BTreeSet<String>,
}

impl Capabilities {
    /// Sorts announced names into known capabilities and unknown names.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::default();
        for name in names {
            match Capability::from_wire(name) {
                Some(capability) => {
                    out.known.insert(capability);
                }
                None => {
                    out.unknown.insert(name.to_owned());
                }
            }
        }
        out
    }

    /// Whether the device announced `capability`.
    pub fn contains(&self, capability: Capability) -> bool {
        self.known.contains(&capability)
    }

    /// The known capabilities, in declaration order.
    pub fn known(&self) -> impl Iterator<Item = Capability> + '_ {
        self.known.iter().copied()
    }

    /// The announced names this controller does not know, sorted.
    pub fn unknown(&self) -> impl Iterator<Item = &str> + '_ {
        self.unknown.iter().map(String::as_str)
    }

    /// The required capabilities the device did not announce, in the order
    /// of [`REQUIRED_CAPABILITIES`].
    pub fn missing_required(&self) -> Vec<Capability> {
        REQUIRED_CAPABILITIES
            .into_iter()
            .filter(|c| !self.contains(*c))
            .collect()
    }
}

wire_enum! {
    /// `session.hello.platform`: the system image the firmware runs on.
    pub enum Platform {
        /// Echo Dot 2nd gen on Fire OS 6.
        FireOs6 => "fireos6",
        /// Echo Show 8 1st gen on Fire OS 6.
        FireOs6Crown => "fireos6_crown",
        /// The same Echo Show 8 on LineageOS 18.1.
        Lineage18Crown => "lineage18_crown",
    }
}

wire_enum! {
    /// `session.rejected.reason`.
    pub enum RejectReason {
        /// The device is known but not approved yet.
        PendingApproval => "pending_approval",
        /// The device token does not match.
        Unauthorized => "unauthorized",
        /// A required capability is missing, the platform is unknown, or the
        /// hello is malformed.
        Protocol => "protocol",
    }
}

wire_enum! {
    /// `command.ack.status`.
    pub enum AckStatus {
        /// Received and taken on.
        Accepted => "accepted",
        /// Carried out.
        Applied => "applied",
        /// Carried out and journaled to storage.
        Durable => "durable",
        /// Refused; `error` names why.
        Rejected => "rejected",
    }
}

wire_enum! {
    /// An uplink stream (`stream.open.stream_id`, audio subscription keys).
    pub enum StreamId {
        /// Capture audio, kind 1.
        Mic => "mic",
        /// The final-mix reference, kind 2, on its own epochs.
        Reference => "reference",
        /// Loudness cells, kind 4, on the capture epoch.
        Cells => "cells",
        /// AFE period records, kind 5, on the capture epoch; only in a session
        /// whose `session.ready` carried `afe_metadata`.
        Afe => "afe",
    }
}

impl StreamId {
    /// The EMA1 kind this stream's packets carry.
    pub const fn kind(self) -> Kind {
        match self {
            StreamId::Mic => Kind::Mic,
            StreamId::Reference => Kind::Reference,
            StreamId::Cells => Kind::Cells,
            StreamId::Afe => Kind::Afe,
        }
    }

    /// The stream an uplink packet of `kind` belongs to; `None` for render.
    pub const fn of_kind(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Mic => Some(StreamId::Mic),
            Kind::Reference => Some(StreamId::Reference),
            Kind::Cells => Some(StreamId::Cells),
            Kind::Afe => Some(StreamId::Afe),
            Kind::Render => None,
        }
    }
}

/// A value of a vocabulary the device reports for information only. A value
/// this controller does not know is kept as sent rather than failing the
/// message, so newer firmware can add one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Reported<T> {
    /// A value the controller knows.
    Known(T),
    /// The text the device sent.
    Unrecognized(String),
}

/// A SHA-256 digest in the link's text form: 64 lowercase hex digits.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Sha256(String);

impl Sha256 {
    /// Accepts exactly 64 lowercase hex digits.
    pub fn parse(text: &str) -> Option<Self> {
        let valid =
            text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        valid.then(|| Self(text.to_owned()))
    }

    /// The text form of a raw digest.
    pub fn from_digest(digest: &[u8; 32]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(64);
        for byte in digest {
            text.push(HEX[usize::from(byte >> 4)] as char);
            text.push(HEX[usize::from(byte & 0xf)] as char);
        }
        Self(text)
    }

    /// The SHA-256 of `data`.
    pub fn of(data: &[u8]) -> Self {
        use sha2::Digest;
        let digest: [u8; 32] = sha2::Sha256::digest(data).into();
        Self::from_digest(&digest)
    }

    /// The 64 hex digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Sha256 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_type_has_a_direction_and_round_trips() {
        for &t in MessageType::ALL {
            assert!(t.sent_by_device() || t.sent_by_controller(), "{t}");
            assert_eq!(MessageType::from_wire(t.as_str()), Some(t));
        }
        for t in [
            MessageType::Heartbeat,
            MessageType::CommandAck,
            MessageType::ProtocolError,
        ] {
            assert!(t.sent_by_device() && t.sent_by_controller());
        }
        assert!(!MessageType::SessionReady.sent_by_device());
        assert!(!MessageType::SessionHello.sent_by_controller());
    }

    #[test]
    fn capabilities_keep_unknown_names_apart() {
        let caps = Capabilities::from_names(["leds", "teleport_v9", "audio_timeline_v1", "leds"]);
        assert!(caps.contains(Capability::Leds));
        assert!(caps.contains(Capability::AudioTimeline));
        assert_eq!(caps.unknown().collect::<Vec<_>>(), ["teleport_v9"]);
        assert_eq!(caps.missing_required().len(), 7);
        assert!(!caps.missing_required().contains(&Capability::AudioTimeline));
    }

    #[test]
    fn sha256_text_form_is_64_lowercase_hex_digits() {
        let empty = Sha256::of(b"");
        assert_eq!(
            empty.as_str(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(Sha256::parse(empty.as_str()), Some(empty.clone()));
        assert_eq!(Sha256::parse(&empty.as_str().to_uppercase()), None);
        assert_eq!(Sha256::parse(&empty.as_str()[1..]), None);
        assert_eq!(Sha256::parse(&format!("{}g", &empty.as_str()[1..])), None);
    }
}
