//! Handshake and lifecycle bodies (docs/protocol-v1.md §4.1, spec §3.1.4):
//! `session.hello` and the admission check, `session.ready` with its
//! detector policy, `session.rejected`, `heartbeat`, `clock.request` and
//! `clock.reply`, `command.ack` and the device's `protocol.error`.

use std::fmt;

use serde::Serialize;
use serde::ser::SerializeStruct;
use serde_json::Value;

use crate::envelope::{ControlBody, LinkSessionId, MessageId, PROTOCOL_VERSION};
use crate::error::{ProtocolError, ProtocolErrorCode};
use crate::json::{Fields, Object, u64_string};
use crate::vocab::{
    AckStatus, Capabilities, Capability, MessageType, Platform, RejectReason, Reported, Sha256,
};

/// `session.hello`, narrowed at the link boundary.
///
/// `protocols` and `capabilities` decide whether the session can exist, so
/// a missing or mistyped one fails the hello. Every other field is state or
/// description: it may be absent (older firmware, or a value the device
/// could not read), which reads as `None`, meaning unknown; a present field
/// of the wrong JSON type is `malformed_message`. `boot_id`, `ip` and `clock`
/// are sent but unused (spec §3.1.4) and are not kept.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionHello {
    /// The announced capabilities, unknown names included.
    pub capabilities: Capabilities,
    /// The system image, or what the device sent instead of a supported one.
    pub platform: HelloPlatform,
    /// The EchoMuse firmware version.
    pub firmware_version: Option<String>,
    /// The system image's own build name (`ro.build.version.name`).
    pub os_version: Option<String>,
    /// The privacy mute and capture epoch.
    pub privacy: Option<HelloPrivacy>,
    /// The alert cache's delivery position.
    pub alerts: Option<HelloAlerts>,
    /// The SHA-256 of every installed speech asset.
    pub assets: Option<Vec<Sha256>>,
    /// The device's volume.
    pub volume: Option<HelloVolume>,
    /// The legacy ambient light sensor report.
    pub ambient_light_status: Option<AmbientLightStatus>,
}

/// `session.hello.platform`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HelloPlatform {
    /// A supported image.
    Supported(Platform),
    /// Absent (`sent` is `None`) or anything else; `sent` is the value's JSON
    /// text for the refusal log. Admission refuses such a hello.
    Unsupported {
        /// The JSON text of the value sent, `None` when the field was absent.
        sent: Option<String>,
    },
}

/// `session.hello.privacy`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelloPrivacy {
    /// Whether the privacy mute is on.
    pub muted: Option<bool>,
    /// The current capture epoch; `None` while there is none (muted).
    pub capture_epoch: Option<u64>,
}

wire_enum! {
    /// `session.hello.alerts.wakeup`.
    pub enum AlarmWakeup {
        /// The device can wake itself for an alarm.
        Ok => "ok",
        /// It cannot hold the wakelock; alarms may not wake it.
        Unavailable => "alarm_wakeup_unavailable",
    }
}

/// `session.hello.alerts`: where the device's alert cache stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelloAlerts {
    /// The delivery epoch; `None` before any snapshot or after a corrupt store.
    pub delivery_epoch: Option<String>,
    /// The last delivery sequence the device applied in that epoch.
    pub acked_sequence: Option<u64>,
    /// Whether alarms can wake the device.
    pub wakeup: Option<Reported<AlarmWakeup>>,
}

/// `session.hello.volume.level`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VolumeLevel {
    /// The level, 0–127.
    Level(u8),
    /// JSON null: the device has no volume control this run.
    Unavailable,
}

/// `session.hello.volume`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloVolume {
    /// The level, or that the device cannot control volume; `None` when the
    /// device did not report it.
    pub level: Option<VolumeLevel>,
    /// Whether the level is already the device's own (a startup volume is
    /// then not applied).
    pub seeded: Option<bool>,
}

wire_enum! {
    /// The ambient light sensor's status code.
    pub enum AmbientLightCode {
        /// Found and readable.
        Ok => "ok",
        /// The bus lists no sensor.
        NoChip => "no_chip",
        /// The sensor is listed but has no reading.
        NoAttribute => "no_attribute",
        /// Not determined.
        Unknown => "unknown",
    }
}

/// `session.hello.ambient_light_status`, the firmware's legacy sensor report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AmbientLightStatus {
    /// The status code.
    pub code: Option<Reported<AmbientLightCode>>,
    /// A sentence for a support bundle.
    pub detail: Option<String>,
    /// Every i2c device name on the bus.
    pub seen: Vec<String>,
    /// Where the sensor was found.
    pub path: Option<String>,
}

impl SessionHello {
    /// Decodes a hello body. `protocols` must be an integer array that
    /// includes 1 (`unsupported_protocol`); `capabilities` must be a string
    /// array (`malformed_message`).
    pub fn parse(body: &Object) -> Result<SessionHello, ProtocolError> {
        let f = Fields::new(body, "session.hello");
        let protocols_ok = f
            .raw("protocols")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().all(|p| p.as_i64().is_some())
                    && items.iter().any(|p| p.as_u64() == Some(PROTOCOL_VERSION))
            });
        if !protocols_ok {
            let sent = f
                .raw("protocols")
                .map_or_else(|| "absent".to_owned(), Value::to_string);
            return Err(ProtocolError::new(
                ProtocolErrorCode::UnsupportedProtocol,
                format!("protocols {sent}"),
            ));
        }
        let names = f.array("capabilities")?;
        let names: Option<Vec<&str>> = names.iter().map(Value::as_str).collect();
        let capabilities = Capabilities::from_names(
            names.ok_or_else(|| f.error("capabilities", "must be a string array"))?,
        );
        let platform = match f.raw("platform") {
            Some(Value::String(text)) => match Platform::from_wire(text) {
                Some(platform) => HelloPlatform::Supported(platform),
                None => HelloPlatform::Unsupported {
                    sent: Some(Value::String(text.clone()).to_string()),
                },
            },
            Some(other) => HelloPlatform::Unsupported {
                sent: Some(other.to_string()),
            },
            None => HelloPlatform::Unsupported { sent: None },
        };
        Ok(SessionHello {
            capabilities,
            platform,
            firmware_version: f.optional_text("firmware_version")?,
            os_version: f.optional_text("os_version")?,
            privacy: f
                .optional_object("privacy")?
                .map(parse_privacy)
                .transpose()?,
            alerts: f.optional_object("alerts")?.map(parse_alerts).transpose()?,
            assets: f
                .optional_array("assets")?
                .map(|items| parse_assets(&f, items))
                .transpose()?,
            volume: f.optional_object("volume")?.map(parse_volume).transpose()?,
            ambient_light_status: f
                .optional_object("ambient_light_status")?
                .map(parse_ambient)
                .transpose()?,
        })
    }
}

fn parse_privacy(object: &Object) -> Result<HelloPrivacy, ProtocolError> {
    let f = Fields::new(object, "session.hello.privacy");
    Ok(HelloPrivacy {
        muted: f.optional_bool("muted")?,
        capture_epoch: f.optional_u64_string("capture_epoch")?,
    })
}

fn parse_alerts(object: &Object) -> Result<HelloAlerts, ProtocolError> {
    let f = Fields::new(object, "session.hello.alerts");
    Ok(HelloAlerts {
        delivery_epoch: f.optional_string("delivery_epoch")?.map(str::to_owned),
        acked_sequence: f.optional_count("acked_sequence")?,
        wakeup: f
            .optional_string("wakeup")?
            .map(|text| match AlarmWakeup::from_wire(text) {
                Some(known) => Reported::Known(known),
                None => Reported::Unrecognized(text.to_owned()),
            }),
    })
}

fn parse_assets(f: &Fields<'_>, items: &[Value]) -> Result<Vec<Sha256>, ProtocolError> {
    items
        .iter()
        .map(|item| {
            item.as_str()
                .and_then(Sha256::parse)
                .ok_or_else(|| f.error("assets", "must hold SHA-256 hex strings"))
        })
        .collect()
}

fn parse_volume(object: &Object) -> Result<HelloVolume, ProtocolError> {
    let f = Fields::new(object, "session.hello.volume");
    let level = match f.raw("level") {
        None => None,
        Some(Value::Null) => Some(VolumeLevel::Unavailable),
        Some(value) => match value.as_u64().filter(|l| *l <= 127) {
            Some(level) => Some(VolumeLevel::Level(level as u8)),
            None => return Err(f.error("level", "must be an integer 0..127 or null")),
        },
    };
    Ok(HelloVolume {
        level,
        seeded: f.optional_bool("seeded")?,
    })
}

fn parse_ambient(object: &Object) -> Result<AmbientLightStatus, ProtocolError> {
    let f = Fields::new(object, "session.hello.ambient_light_status");
    let seen = match f.optional_array("seen")? {
        None => Vec::new(),
        Some(items) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| f.error("seen", "must be a string array"))?,
    };
    Ok(AmbientLightStatus {
        code: f
            .optional_string("code")?
            .map(|text| match AmbientLightCode::from_wire(text) {
                Some(known) => Reported::Known(known),
                None => Reported::Unrecognized(text.to_owned()),
            }),
        detail: f.optional_text("detail")?,
        seen,
        path: f.optional_text("path")?,
    })
}

/// Why admission refuses a hello. Every refusal is answered with
/// `session.rejected` reason `protocol`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdmissionRefusal {
    /// Required capabilities the device did not announce, in the order of
    /// [`crate::REQUIRED_CAPABILITIES`].
    MissingCapabilities(Vec<Capability>),
    /// The platform was absent or names an image the controller does not
    /// support; `sent` is its JSON text.
    UnsupportedPlatform {
        /// The JSON text of the value sent, `None` when absent.
        sent: Option<String>,
    },
}

impl AdmissionRefusal {
    /// The `session.rejected` reason: always `protocol`.
    pub const fn reason(&self) -> RejectReason {
        RejectReason::Protocol
    }
}

impl fmt::Display for AdmissionRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdmissionRefusal::MissingCapabilities(missing) => {
                f.write_str("missing required capabilities:")?;
                for capability in missing {
                    write!(f, " {capability}")?;
                }
                Ok(())
            }
            AdmissionRefusal::UnsupportedPlatform { sent: Some(sent) } => {
                write!(f, "unsupported platform {sent}")
            }
            AdmissionRefusal::UnsupportedPlatform { sent: None } => f.write_str("no platform"),
        }
    }
}

/// The protocol part of admission (spec §3.1.4): all eight required
/// capabilities, checked first, and a supported platform. Token and approval
/// checks belong to the caller.
pub fn check_admission(hello: &SessionHello) -> Result<Platform, AdmissionRefusal> {
    let missing = hello.capabilities.missing_required();
    if !missing.is_empty() {
        return Err(AdmissionRefusal::MissingCapabilities(missing));
    }
    match &hello.platform {
        HelloPlatform::Supported(platform) => Ok(*platform),
        HelloPlatform::Unsupported { sent } => {
            Err(AdmissionRefusal::UnsupportedPlatform { sent: sent.clone() })
        }
    }
}

/// `session.ready.assets`: the speech assets the device must load.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SpeechAssets {
    /// The ONNX Runtime library.
    pub runtime_sha256: Sha256,
    /// The wake graph.
    pub graph_sha256: Sha256,
    /// The wake graph's sidecar.
    pub sidecar_sha256: Sha256,
}

/// Why a detector policy is one the device would refuse.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PolicyError {
    /// A threshold outside (0, 1].
    Threshold {
        /// Which threshold.
        name: &'static str,
        /// Its value.
        value: f64,
    },
    /// A provisional duck that is not a finite number of dB.
    DuckDb(f64),
    /// A rule's windows outside 1–3.
    RuleWindows(u8),
    /// A rule's threshold outside (0, 1].
    RuleThreshold(f64),
    /// More than six live rules.
    TooManyOpenRules(usize),
    /// More than eight shadow rules.
    TooManyShadowRules(usize),
    /// Live rules without one for this profile; an empty list has none for
    /// either.
    NoRuleForProfile(RuleProfile),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyError::Threshold { name, value } => {
                write!(f, "{name} threshold {value} outside (0, 1]")
            }
            PolicyError::DuckDb(v) => write!(f, "provisional duck {v} dB is not finite"),
            PolicyError::RuleWindows(w) => write!(f, "rule windows {w} outside 1..3"),
            PolicyError::RuleThreshold(t) => write!(f, "rule threshold {t} outside (0, 1]"),
            PolicyError::TooManyOpenRules(n) => {
                write!(f, "{n} open rules, at most {MAX_OPEN_RULES}")
            }
            PolicyError::TooManyShadowRules(n) => {
                write!(f, "{n} shadow rules, at most {MAX_SHADOW_RULES}")
            }
            PolicyError::NoRuleForProfile(p) => write!(f, "open rules need at least one {p} rule"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// The most live rules the device accepts.
pub const MAX_OPEN_RULES: usize = 6;
/// The most shadow rules the device accepts.
pub const MAX_SHADOW_RULES: usize = 8;

fn unit_interval(value: f64) -> bool {
    value > 0.0 && value <= 1.0
}

/// `detector.thresholds`, each in (0, 1].
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Thresholds {
    idle: f64,
    playback: f64,
    near_miss: f64,
}

impl Thresholds {
    /// The wake model's idle and playback thresholds and the near-miss floor.
    pub fn new(idle: f64, playback: f64, near_miss: f64) -> Result<Self, PolicyError> {
        for (name, value) in [
            ("idle", idle),
            ("playback", playback),
            ("near_miss", near_miss),
        ] {
            if !unit_interval(value) {
                return Err(PolicyError::Threshold { name, value });
            }
        }
        Ok(Self {
            idle,
            playback,
            near_miss,
        })
    }

    /// The idle threshold.
    pub fn idle(&self) -> f64 {
        self.idle
    }

    /// The playback threshold.
    pub fn playback(&self) -> f64 {
        self.playback
    }

    /// The near-miss floor.
    pub fn near_miss(&self) -> f64 {
        self.near_miss
    }
}

wire_enum! {
    /// The threshold profile a rule applies at.
    pub enum RuleProfile {
        /// Hops scored while the device plays nothing.
        Idle => "idle",
        /// Hops scored while the device plays audio.
        Playback => "playback",
    }
}

wire_enum! {
    /// How a rule combines its windows.
    pub enum RuleCombine {
        /// The mean reaches the threshold.
        Mean => "mean",
        /// Every window reaches the threshold.
        All => "all",
    }
}

/// One `open_rules` or `shadow_rules` entry.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct OpenRule {
    profile: RuleProfile,
    windows: u8,
    combine: RuleCombine,
    threshold: f64,
}

impl OpenRule {
    /// A rule over the last `windows` (1–3) raw scores of `profile` hops,
    /// firing when their mean or every one reaches `threshold` (0, 1].
    pub fn new(
        profile: RuleProfile,
        windows: u8,
        combine: RuleCombine,
        threshold: f64,
    ) -> Result<Self, PolicyError> {
        if !(1..=3).contains(&windows) {
            return Err(PolicyError::RuleWindows(windows));
        }
        if !unit_interval(threshold) {
            return Err(PolicyError::RuleThreshold(threshold));
        }
        Ok(Self {
            profile,
            windows,
            combine,
            threshold,
        })
    }

    /// The rule's profile.
    pub fn profile(&self) -> RuleProfile {
        self.profile
    }

    /// The rule's window count.
    pub fn windows(&self) -> u8 {
        self.windows
    }

    /// How it combines its windows.
    pub fn combine(&self) -> RuleCombine {
        self.combine
    }

    /// Its threshold.
    pub fn threshold(&self) -> f64 {
        self.threshold
    }
}

/// `session.ready.detector`. The hop, smoothing and clearing values are fixed
/// at 2, 3 and 6 and the provisional duck at two per 5,000 ms window: the
/// device refuses to run without them.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectorPolicy {
    thresholds: Thresholds,
    duck_db: f64,
    open_rules: Option<Vec<OpenRule>>,
    shadow_rules: Option<Vec<OpenRule>>,
}

impl DetectorPolicy {
    /// The detector hop in 80 ms capture blocks.
    pub const HOP_BLOCKS: u32 = 2;
    /// Scores in the smoothing mean.
    pub const SMOOTHING: u32 = 3;
    /// Unscored hops after which the score history clears.
    pub const CLEAR_AFTER_UNSCORED: u32 = 6;
    /// Provisional ducks allowed per window.
    pub const DUCK_MAX_PER_WINDOW: u32 = 2;
    /// The provisional duck window in ms.
    pub const DUCK_WINDOW_MS: u32 = 5_000;

    /// A policy the device accepts (`device/internal/wakeword/detector`
    /// `CheckRules`): at most six live rules with at least one per profile,
    /// so an explicit empty list is refused, and at most eight shadow rules.
    /// `None` leaves a list off the wire: the device then derives the two
    /// baseline live rules from the thresholds, and runs no shadow rules.
    pub fn new(
        thresholds: Thresholds,
        duck_db: f64,
        open_rules: Option<Vec<OpenRule>>,
        shadow_rules: Option<Vec<OpenRule>>,
    ) -> Result<Self, PolicyError> {
        if !duck_db.is_finite() {
            return Err(PolicyError::DuckDb(duck_db));
        }
        if let Some(rules) = &open_rules {
            if rules.len() > MAX_OPEN_RULES {
                return Err(PolicyError::TooManyOpenRules(rules.len()));
            }
            for profile in RuleProfile::ALL {
                if !rules.iter().any(|r| r.profile == *profile) {
                    return Err(PolicyError::NoRuleForProfile(*profile));
                }
            }
        }
        if let Some(rules) = &shadow_rules
            && rules.len() > MAX_SHADOW_RULES
        {
            return Err(PolicyError::TooManyShadowRules(rules.len()));
        }
        Ok(Self {
            thresholds,
            duck_db,
            open_rules,
            shadow_rules,
        })
    }

    /// The thresholds.
    pub fn thresholds(&self) -> Thresholds {
        self.thresholds
    }

    /// The provisional duck in dB.
    pub fn duck_db(&self) -> f64 {
        self.duck_db
    }

    /// The live rules, `None` when left to the device's baseline.
    pub fn open_rules(&self) -> Option<&[OpenRule]> {
        self.open_rules.as_deref()
    }

    /// The shadow rules, `None` when none are sent.
    pub fn shadow_rules(&self) -> Option<&[OpenRule]> {
        self.shadow_rules.as_deref()
    }

    fn without_rules(&self) -> Self {
        Self {
            open_rules: None,
            shadow_rules: None,
            ..self.clone()
        }
    }
}

#[derive(Serialize)]
struct ProvisionalDuckWire {
    duck_db: f64,
    max_per_window: u32,
    window_ms: u32,
}

impl Serialize for DetectorPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let fields =
            5 + usize::from(self.open_rules.is_some()) + usize::from(self.shadow_rules.is_some());
        let mut s = serializer.serialize_struct("DetectorPolicy", fields)?;
        s.serialize_field("thresholds", &self.thresholds)?;
        s.serialize_field("hop_blocks", &Self::HOP_BLOCKS)?;
        s.serialize_field("smoothing", &Self::SMOOTHING)?;
        s.serialize_field("clear_after_unscored", &Self::CLEAR_AFTER_UNSCORED)?;
        let duck = ProvisionalDuckWire {
            duck_db: self.duck_db,
            max_per_window: Self::DUCK_MAX_PER_WINDOW,
            window_ms: Self::DUCK_WINDOW_MS,
        };
        s.serialize_field("provisional_duck", &duck)?;
        if let Some(rules) = &self.open_rules {
            s.serialize_field("open_rules", rules)?;
        }
        if let Some(rules) = &self.shadow_rules {
            s.serialize_field("shadow_rules", rules)?;
        }
        s.end()
    }
}

/// What admission grants a session: the parts of `session.ready` that the
/// application decides.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadyGrant {
    /// Whether the controller will accept wake candidates.
    pub capture_permitted: bool,
    /// The speech assets to load.
    pub assets: SpeechAssets,
    /// The detector policy.
    pub detector: DetectorPolicy,
    /// Whether to ask for the `afe` stream.
    pub afe_metadata: bool,
}

/// `session.ready`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionReady {
    protocol: u64,
    session_id: LinkSessionId,
    server_boot_id: uuid::Uuid,
    capture_permitted: bool,
    assets: SpeechAssets,
    detector: DetectorPolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    afe_metadata: Option<bool>,
    #[serde(serialize_with = "u64_string")]
    utc_ms: u64,
}

impl SessionReady {
    /// The `session.ready` for a device with `capabilities`. The rule lists
    /// go only to a device announcing `open_rules_v1`, and `afe_metadata:
    /// true` only when the grant asks for it and the device announced
    /// `afe_metadata_v1`; otherwise the fields are left off the wire, which
    /// older firmware requires (spec §3.1.4).
    pub fn new(
        grant: &ReadyGrant,
        capabilities: &Capabilities,
        session_id: LinkSessionId,
        server_boot_id: uuid::Uuid,
        utc_ms: u64,
    ) -> Self {
        let detector = if capabilities.contains(Capability::OpenRules) {
            grant.detector.clone()
        } else {
            grant.detector.without_rules()
        };
        let afe = grant.afe_metadata && capabilities.contains(Capability::AfeMetadata);
        Self {
            protocol: PROTOCOL_VERSION,
            session_id,
            server_boot_id,
            capture_permitted: grant.capture_permitted,
            assets: grant.assets.clone(),
            detector,
            afe_metadata: afe.then_some(true),
            utc_ms,
        }
    }

    /// The session this message admits.
    pub fn session_id(&self) -> &LinkSessionId {
        &self.session_id
    }

    /// Whether the session carries the `afe` stream.
    pub fn afe_metadata(&self) -> bool {
        self.afe_metadata == Some(true)
    }
}

impl ControlBody for SessionReady {
    const MESSAGE_TYPE: MessageType = MessageType::SessionReady;
}

/// `session.rejected`, sent with a null `session_id` before the close.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct SessionRejected {
    /// Why.
    pub reason: RejectReason,
}

impl ControlBody for SessionRejected {
    const MESSAGE_TYPE: MessageType = MessageType::SessionRejected;
}

/// `heartbeat`, both directions: the sender's monotonic clock.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct Heartbeat {
    /// The sender's `CLOCK_MONOTONIC` in ns.
    #[serde(serialize_with = "u64_string")]
    pub mono_ns: u64,
}

impl Heartbeat {
    /// Decodes a device heartbeat; `mono_ns` is required.
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        Ok(Self {
            mono_ns: Fields::new(body, "heartbeat").u64_string("mono_ns")?,
        })
    }
}

impl ControlBody for Heartbeat {
    const MESSAGE_TYPE: MessageType = MessageType::Heartbeat;
}

/// `clock.request`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClockRequest {
    /// The device's id for this request, echoed in the reply.
    pub nonce: String,
}

impl ClockRequest {
    /// Decodes a clock request; `nonce` is a required non-empty string.
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        Ok(Self {
            nonce: Fields::new(body, "clock.request")
                .nonempty_string("nonce")?
                .to_owned(),
        })
    }
}

/// `clock.reply`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ClockReply {
    /// The request's nonce.
    pub nonce: String,
    /// The controller's UTC time in ms.
    #[serde(serialize_with = "u64_string")]
    pub utc_ms: u64,
}

impl ClockReply {
    /// The reply to `request` at `utc_ms`.
    pub fn to(request: ClockRequest, utc_ms: u64) -> Self {
        Self {
            nonce: request.nonce,
            utc_ms,
        }
    }
}

impl ControlBody for ClockReply {
    const MESSAGE_TYPE: MessageType = MessageType::ClockReply;
}

/// `command.ack`, both directions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommandAck {
    /// The acknowledged message's `message_id`.
    pub message_id: MessageId,
    /// The outcome.
    pub status: AckStatus,
    /// The rejection code; `null` on the wire when there is none.
    pub error: Option<String>,
}

impl CommandAck {
    /// Decodes a device acknowledgement: a non-empty `message_id`, a known
    /// `status`, and `error` a string, null or absent.
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        let f = Fields::new(body, "command.ack");
        let message_id = MessageId::from(f.nonempty_string("message_id")?.to_owned());
        let status = f
            .string("status")
            .ok()
            .and_then(AckStatus::from_wire)
            .ok_or_else(|| f.error("status", "must be accepted, applied, durable or rejected"))?;
        let error = f.optional_string("error")?.map(str::to_owned);
        Ok(Self {
            message_id,
            status,
            error,
        })
    }
}

impl ControlBody for CommandAck {
    const MESSAGE_TYPE: MessageType = MessageType::CommandAck;
}

/// A `protocol.error` the device sent before closing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolErrorReport {
    /// The code; the firmware sends `malformed_message` and `malformed_frame`.
    pub code: Reported<ProtocolErrorCode>,
    /// The device's sentence.
    pub detail: String,
}

impl ProtocolErrorReport {
    /// Decodes a device `protocol.error`: `code` and `detail` are strings.
    pub fn parse(body: &Object) -> Result<Self, ProtocolError> {
        let f = Fields::new(body, "protocol.error");
        let code = f.nonempty_string("code")?;
        let code = match ProtocolErrorCode::from_wire(code) {
            Some(known) => Reported::Known(known),
            None => Reported::Unrecognized(code.to_owned()),
        };
        Ok(Self {
            code,
            detail: f.string("detail")?.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::vocab::REQUIRED_CAPABILITIES;

    fn object(value: Value) -> Object {
        match value {
            Value::Object(object) => object,
            _ => panic!("object"),
        }
    }

    /// The hello example of docs/protocol-v1.md §4.1, as the Dot sends it.
    fn dot_hello() -> Value {
        json!({
            "capabilities": ["audio_timeline_v1","uplink_leases_v1","device_wake_v1",
              "render_reference_v1","render_progress_v1","focus_leases_v1","alert_cache_v1",
              "turn_protocol_v1","leds","led_anim","buttons","button_hold","alert_prefetch","local_wake_chime",
              "no_input_chime","announcement_chime","open_rules_v1","near_miss_v1","ambient_light","afe_metadata_v1",
              "wifi_control","ble_proxy","privacy_mute"],
            "platform": "fireos6",
            "firmware_version": "v3.0.0", "os_version": "Fire OS 6.5.6.9 (NS6569/6009)",
            "boot_id": "b0", "protocols": [1],
            "ip": "10.0.0.5", "ambient_light_status": {"code": "ok", "path": "/sys/bus/i2c/devices/2-0039/als_lux"},
            "privacy": {"muted": false, "capture_epoch": "123"},
            "clock": {"trusted": true, "mono_ns": "1", "utc_ms": null},
            "alerts": {"delivery_epoch": null, "acked_sequence": 0, "wakeup": "ok"},
            "assets": ["174233cf0000000000000000000000000000000000000000000000000000beef"],
            "volume": {"level": 120, "seeded": false}
        })
    }

    #[test]
    fn the_documented_dot_hello_narrows_to_typed_fields() {
        let hello = SessionHello::parse(&object(dot_hello())).unwrap();
        assert_eq!(hello.platform, HelloPlatform::Supported(Platform::FireOs6));
        assert!(
            REQUIRED_CAPABILITIES
                .iter()
                .all(|c| hello.capabilities.contains(*c))
        );
        assert!(hello.capabilities.contains(Capability::PrivacyMute));
        assert_eq!(hello.firmware_version.as_deref(), Some("v3.0.0"));
        assert_eq!(
            hello.privacy,
            Some(HelloPrivacy {
                muted: Some(false),
                capture_epoch: Some(123)
            })
        );
        assert_eq!(
            hello.alerts,
            Some(HelloAlerts {
                delivery_epoch: None,
                acked_sequence: Some(0),
                wakeup: Some(Reported::Known(AlarmWakeup::Ok))
            })
        );
        assert_eq!(
            hello.volume,
            Some(HelloVolume {
                level: Some(VolumeLevel::Level(120)),
                seeded: Some(false)
            })
        );
        assert_eq!(hello.assets.as_ref().map(Vec::len), Some(1));
        let als = hello.ambient_light_status.unwrap();
        assert_eq!(als.code, Some(Reported::Known(AmbientLightCode::Ok)));
        assert_eq!(als.detail, None);
        assert!(als.seen.is_empty());
        assert_eq!(
            check_admission(&SessionHello::parse(&object(dot_hello())).unwrap()),
            Ok(Platform::FireOs6)
        );
    }

    #[test]
    fn a_null_volume_level_means_no_volume_control_not_level_zero() {
        let mut body = dot_hello();
        body["volume"] = json!({"level": null, "seeded": false});
        let hello = SessionHello::parse(&object(body)).unwrap();
        assert_eq!(hello.volume.unwrap().level, Some(VolumeLevel::Unavailable));
        let mut body = dot_hello();
        body["volume"] = json!({"seeded": true});
        assert_eq!(
            SessionHello::parse(&object(body))
                .unwrap()
                .volume
                .unwrap()
                .level,
            None
        );
        for level in [json!(128), json!(-1), json!(1.5), json!("120")] {
            let mut body = dot_hello();
            body["volume"] = json!({"level": level, "seeded": false});
            let err = SessionHello::parse(&object(body)).unwrap_err();
            assert_eq!(err.code, ProtocolErrorCode::MalformedMessage, "{level}");
        }
    }

    #[test]
    fn absent_descriptive_fields_are_unknown_and_empty_strings_too() {
        let mut body = object(
            json!({"capabilities": [], "protocols": [2, 1], "firmware_version": "", "os_version": null}),
        );
        let hello = SessionHello::parse(&body).unwrap();
        assert_eq!(hello.firmware_version, None);
        assert_eq!(hello.os_version, None);
        assert_eq!(
            (hello.privacy, hello.alerts, hello.assets, hello.volume),
            (None, None, None, None)
        );
        assert_eq!(hello.platform, HelloPlatform::Unsupported { sent: None });
        body.insert(
            "privacy".into(),
            json!({"muted": true, "capture_epoch": null}),
        );
        let privacy = SessionHello::parse(&body).unwrap().privacy.unwrap();
        assert_eq!(
            privacy,
            HelloPrivacy {
                muted: Some(true),
                capture_epoch: None
            }
        );
    }

    #[test]
    fn unknown_capabilities_and_report_values_are_kept_not_refused() {
        let mut body = dot_hello();
        body["capabilities"]
            .as_array_mut()
            .unwrap()
            .push(json!("hologram_v1"));
        body["alerts"]["wakeup"] = json!("sleepy");
        body["ambient_light_status"]["code"] = json!("dazzled");
        let hello = SessionHello::parse(&object(body)).unwrap();
        assert_eq!(
            hello.capabilities.unknown().collect::<Vec<_>>(),
            ["hologram_v1"]
        );
        assert_eq!(
            hello.alerts.unwrap().wakeup,
            Some(Reported::Unrecognized("sleepy".into()))
        );
        assert_eq!(
            hello.ambient_light_status.unwrap().code,
            Some(Reported::Unrecognized("dazzled".into()))
        );
    }

    #[test]
    fn protocols_without_one_are_unsupported() {
        for protocols in [
            json!([2]),
            json!([]),
            json!("1"),
            json!([1.5, 1]),
            json!(["1"]),
            json!(null),
        ] {
            let mut body = dot_hello();
            body["protocols"] = protocols.clone();
            let err = SessionHello::parse(&object(body)).unwrap_err();
            assert_eq!(
                err.code,
                ProtocolErrorCode::UnsupportedProtocol,
                "{protocols}"
            );
        }
        let mut body = object(dot_hello());
        body.remove("protocols");
        assert_eq!(
            SessionHello::parse(&body).unwrap_err().code,
            ProtocolErrorCode::UnsupportedProtocol
        );
    }

    #[test]
    fn mistyped_hello_fields_are_malformed() {
        let cases = [
            ("capabilities", json!("leds")),
            ("capabilities", json!(["leds", 3])),
            ("privacy", json!(true)),
            ("privacy", json!({"muted": "no"})),
            ("privacy", json!({"capture_epoch": 123})),
            ("alerts", json!({"acked_sequence": -1})),
            ("alerts", json!({"delivery_epoch": 5})),
            ("assets", json!(["not-a-sha"])),
            ("assets", json!("174233cf")),
            ("firmware_version", json!(3)),
            ("volume", json!({"level": 3, "seeded": "yes"})),
            ("ambient_light_status", json!({"seen": [1]})),
        ];
        for (key, value) in cases {
            let mut body = dot_hello();
            body[key] = value.clone();
            let err = SessionHello::parse(&object(body)).unwrap_err();
            assert_eq!(
                err.code,
                ProtocolErrorCode::MalformedMessage,
                "{key} = {value}"
            );
        }
    }

    #[test]
    fn admission_requires_all_eight_capabilities_then_a_supported_platform() {
        for platform in Platform::ALL {
            let mut body = dot_hello();
            body["platform"] = json!(platform.as_str());
            assert_eq!(
                check_admission(&SessionHello::parse(&object(body)).unwrap()),
                Ok(*platform)
            );
        }
        let mut body = dot_hello();
        body["capabilities"] = json!(["audio_timeline_v1", "leds", "alert_cache_v1"]);
        body["platform"] = json!("fireos5");
        let refusal = check_admission(&SessionHello::parse(&object(body)).unwrap()).unwrap_err();
        let AdmissionRefusal::MissingCapabilities(missing) = &refusal else {
            panic!("{refusal:?}")
        };
        assert_eq!(
            missing,
            &[
                Capability::UplinkSubscriptions,
                Capability::DeviceWake,
                Capability::RenderReference,
                Capability::RenderProgress,
                Capability::FocusLeases,
                Capability::TurnProtocol
            ]
        );
        assert_eq!(refusal.reason(), RejectReason::Protocol);

        for (platform, sent) in [
            (Some(json!("fireos5")), Some("\"fireos5\"")),
            (Some(json!(6)), Some("6")),
            (Some(json!(null)), Some("null")),
            (None, None),
        ] {
            let mut body = object(dot_hello());
            match platform {
                Some(value) => body.insert("platform".into(), value),
                None => body.remove("platform"),
            };
            let refusal = check_admission(&SessionHello::parse(&body).unwrap()).unwrap_err();
            assert_eq!(
                refusal,
                AdmissionRefusal::UnsupportedPlatform {
                    sent: sent.map(str::to_owned)
                }
            );
            assert_eq!(refusal.reason(), RejectReason::Protocol);
        }
    }

    fn sha(c: char) -> Sha256 {
        Sha256::parse(&c.to_string().repeat(64)).unwrap()
    }

    fn policy() -> DetectorPolicy {
        let rules = vec![
            OpenRule::new(RuleProfile::Idle, 3, RuleCombine::Mean, 0.90).unwrap(),
            OpenRule::new(RuleProfile::Playback, 3, RuleCombine::Mean, 0.65).unwrap(),
        ];
        let shadow = vec![OpenRule::new(RuleProfile::Idle, 2, RuleCombine::Mean, 0.95).unwrap()];
        DetectorPolicy::new(
            Thresholds::new(0.90, 0.65, 0.17).unwrap(),
            -18.0,
            Some(rules),
            Some(shadow),
        )
        .unwrap()
    }

    fn grant(afe_metadata: bool) -> ReadyGrant {
        ReadyGrant {
            capture_permitted: true,
            assets: SpeechAssets {
                runtime_sha256: sha('1'),
                graph_sha256: sha('2'),
                sidecar_sha256: sha('3'),
            },
            detector: policy(),
            afe_metadata,
        }
    }

    fn ready_json(grant: &ReadyGrant, caps: &[&str]) -> Value {
        let boot = uuid::Uuid::parse_str("8e0a8f2c-4c1e-4d5b-9c1a-2b3c4d5e6f70").unwrap();
        let ready = SessionReady::new(
            grant,
            &Capabilities::from_names(caps.iter().copied()),
            LinkSessionId::from("s".to_owned()),
            boot,
            1_700_000_000_123,
        );
        serde_json::to_value(&ready).unwrap()
    }

    #[test]
    fn session_ready_matches_the_documented_shape_for_a_capable_device() {
        let value = ready_json(&grant(true), &["open_rules_v1", "afe_metadata_v1"]);
        assert_eq!(
            value,
            json!({"protocol": 1, "session_id": "s", "server_boot_id": "8e0a8f2c-4c1e-4d5b-9c1a-2b3c4d5e6f70",
                   "capture_permitted": true,
                   "assets": {"runtime_sha256": "1".repeat(64), "graph_sha256": "2".repeat(64), "sidecar_sha256": "3".repeat(64)},
                   "detector": {"thresholds": {"idle": 0.90, "playback": 0.65, "near_miss": 0.17},
                     "hop_blocks": 2, "smoothing": 3, "clear_after_unscored": 6,
                     "open_rules": [{"profile": "idle", "windows": 3, "combine": "mean", "threshold": 0.90},
                                    {"profile": "playback", "windows": 3, "combine": "mean", "threshold": 0.65}],
                     "shadow_rules": [{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.95}],
                     "provisional_duck": {"duck_db": -18.0, "max_per_window": 2, "window_ms": 5000}},
                   "afe_metadata": true, "utc_ms": "1700000000123"})
        );
    }

    #[test]
    fn rule_lists_and_afe_metadata_go_only_to_devices_announcing_them() {
        let value = ready_json(&grant(true), &[]);
        let detector = value["detector"].as_object().unwrap();
        assert!(!detector.contains_key("open_rules") && !detector.contains_key("shadow_rules"));
        assert!(value.get("afe_metadata").is_none());
        let value = ready_json(&grant(false), &["afe_metadata_v1"]);
        assert!(value.get("afe_metadata").is_none(), "not asked for");
        let only_live = DetectorPolicy::new(
            Thresholds::new(0.9, 0.65, 0.17).unwrap(),
            -18.0,
            policy().open_rules().map(<[_]>::to_vec),
            None,
        )
        .unwrap();
        let value = ready_json(
            &ReadyGrant {
                detector: only_live,
                ..grant(false)
            },
            &["open_rules_v1"],
        );
        assert!(value["detector"].get("open_rules").is_some());
        assert!(
            value["detector"].get("shadow_rules").is_none(),
            "absent shadow rules mean none"
        );
    }

    #[test]
    fn a_policy_the_device_would_refuse_cannot_be_built() {
        let th = Thresholds::new(0.9, 0.65, 0.17).unwrap();
        let idle = OpenRule::new(RuleProfile::Idle, 3, RuleCombine::Mean, 0.9).unwrap();
        let playback = OpenRule::new(RuleProfile::Playback, 1, RuleCombine::All, 1.0).unwrap();
        assert_eq!(
            Thresholds::new(0.0, 0.5, 0.5),
            Err(PolicyError::Threshold {
                name: "idle",
                value: 0.0
            })
        );
        assert_eq!(
            Thresholds::new(0.5, 1.01, 0.5),
            Err(PolicyError::Threshold {
                name: "playback",
                value: 1.01
            })
        );
        assert!(Thresholds::new(0.5, 0.5, f64::NAN).is_err());
        assert_eq!(
            OpenRule::new(RuleProfile::Idle, 0, RuleCombine::Mean, 0.5),
            Err(PolicyError::RuleWindows(0))
        );
        assert_eq!(
            OpenRule::new(RuleProfile::Idle, 4, RuleCombine::Mean, 0.5),
            Err(PolicyError::RuleWindows(4))
        );
        assert_eq!(
            OpenRule::new(RuleProfile::Idle, 2, RuleCombine::Mean, 0.0),
            Err(PolicyError::RuleThreshold(0.0))
        );
        assert_eq!(
            DetectorPolicy::new(th, f64::INFINITY, None, None),
            Err(PolicyError::DuckDb(f64::INFINITY))
        );
        assert_eq!(
            DetectorPolicy::new(th, -18.0, Some(vec![]), None),
            Err(PolicyError::NoRuleForProfile(RuleProfile::Idle))
        );
        assert_eq!(
            DetectorPolicy::new(th, -18.0, Some(vec![idle]), None),
            Err(PolicyError::NoRuleForProfile(RuleProfile::Playback))
        );
        let seven = vec![idle, playback, idle, idle, idle, idle, idle];
        assert_eq!(
            DetectorPolicy::new(th, -18.0, Some(seven), None),
            Err(PolicyError::TooManyOpenRules(7))
        );
        let six = vec![idle, playback, idle, idle, idle, idle];
        assert!(DetectorPolicy::new(th, -18.0, Some(six), Some(vec![idle; 8])).is_ok());
        assert_eq!(
            DetectorPolicy::new(th, -18.0, None, Some(vec![idle; 9])),
            Err(PolicyError::TooManyShadowRules(9))
        );
    }

    #[test]
    fn command_ack_error_is_null_on_the_wire_when_absent() {
        let ack = CommandAck {
            message_id: MessageId::from("m".to_owned()),
            status: AckStatus::Accepted,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&ack).unwrap(),
            json!({"message_id": "m", "status": "accepted", "error": null})
        );
        let parsed = CommandAck::parse(&object(
            json!({"message_id": "m", "status": "rejected", "error": "invalid"}),
        ))
        .unwrap();
        assert_eq!(parsed.error.as_deref(), Some("invalid"));
        assert_eq!(
            CommandAck::parse(&object(json!({"message_id": "m", "status": "accepted"})))
                .unwrap()
                .error,
            None
        );
    }

    #[test]
    fn clock_reply_echoes_the_nonce_with_utc_as_a_decimal_string() {
        let request = ClockRequest::parse(&object(json!({"nonce": "n-1"}))).unwrap();
        let reply = ClockReply::to(request, 1_700_000_000_000);
        assert_eq!(
            serde_json::to_value(&reply).unwrap(),
            json!({"nonce": "n-1", "utc_ms": "1700000000000"})
        );
    }

    #[test]
    fn device_protocol_errors_keep_unrecognized_codes() {
        let known =
            ProtocolErrorReport::parse(&object(json!({"code": "malformed_frame", "detail": "x"})))
                .unwrap();
        assert_eq!(
            known.code,
            Reported::Known(ProtocolErrorCode::MalformedFrame)
        );
        let other =
            ProtocolErrorReport::parse(&object(json!({"code": "melted", "detail": ""}))).unwrap();
        assert_eq!(other.code, Reported::Unrecognized("melted".into()));
        assert!(ProtocolErrorReport::parse(&object(json!({"code": "melted"}))).is_err());
    }
}
