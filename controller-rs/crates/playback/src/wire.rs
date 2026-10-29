//! The render and focus message bodies, as the Go firmware defines them
//! (`device/internal/proto/proto.go`, `device/internal/render/types.go`,
//! `device/internal/focus/focus.go`) and `docs/protocol-v1.md` §4.2–§4.3
//! describes them.
//!
//! Controller bodies serialize exactly as the firmware decodes them: 64-bit
//! values as decimal strings, a missing epoch or local asset as `null`.
//! Device bodies are parsed from the `body` object the link hands over for
//! `render.progress` and `render.finished`; the playback's generation travels
//! in the envelope, not the body.

use std::fmt;
use std::num::NonZeroU64;

use echomuse_protocol::session::CommandAck;
use echomuse_protocol::vocab::AckStatus;
use echomuse_protocol::{ControlBody, MessageType, Reported, Sha256};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

/// Declares a closed string vocabulary with its exact wire spellings.
macro_rules! vocabulary {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $wire:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// The exact wire spelling.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $wire, )+
                }
            }

            /// Parses the exact wire spelling; anything else is `None`.
            pub fn from_wire(text: &str) -> Option<Self> {
                match text {
                    $( $wire => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

vocabulary! {
    /// A `render.start` source class. Each class has one playback at a time on
    /// the device and its own generation fence.
    pub enum SourceClass {
        /// Media, fed by kind-3 packets.
        Content => "content",
        /// Spoken replies and announcements, fed by kind-3 packets.
        DialogOutput => "dialog_output",
        /// A built-in sound the device holds.
        Earcon => "earcon",
        /// An alert sound played once from the device's sound store.
        AlertPreview => "alert_preview",
    }
}

impl SourceClass {
    /// Whether the class is fed by kind-3 packets rather than a local asset.
    pub const fn is_network(self) -> bool {
        matches!(self, SourceClass::Content | SourceClass::DialogOutput)
    }

    /// The class's slot, matching the firmware's mixer slots.
    pub(crate) const fn slot(self) -> usize {
        match self {
            SourceClass::Content => 0,
            SourceClass::DialogOutput => 1,
            SourceClass::Earcon => 2,
            SourceClass::AlertPreview => 3,
        }
    }
}

vocabulary! {
    /// The two network classes, which the controller streams audio for.
    pub enum NetworkClass {
        /// Media.
        Content => "content",
        /// Spoken replies and announcements.
        DialogOutput => "dialog_output",
    }
}

impl From<NetworkClass> for SourceClass {
    fn from(class: NetworkClass) -> Self {
        match class {
            NetworkClass::Content => SourceClass::Content,
            NetworkClass::DialogOutput => SourceClass::DialogOutput,
        }
    }
}

vocabulary! {
    /// `render.finished.reason`.
    pub enum FinishReason {
        /// Every frame played; declared 150 ms after the last buffer completed.
        Drained => "drained",
        /// Cancelled by `render.cancel`, a newer playback of the class, or the
        /// expiry of the dialog lease that owned it.
        Cancelled => "cancelled",
        /// The device failed it, for example on a source FIFO overflow.
        Failed => "failed",
        /// Two seconds of continuous starvation.
        Underrun => "underrun",
    }
}

vocabulary! {
    /// `render.progress.event`.
    pub enum ProgressEvent {
        /// The periodic report, every 80 ms while playing.
        Progress => "progress",
        /// The first frames became audible.
        Start => "start",
        /// Queued audio was discarded.
        Flush => "flush",
        /// A seek; the firmware has none today.
        Seek => "seek",
        /// The device paused the playback (alert foreground).
        Pause => "pause",
        /// The device resumed it.
        Resume => "resume",
        /// The source starved; `missing_from`/`missing_to` name the gap.
        Underrun => "underrun",
        /// The applied gain changed; `gain_db` is the new gain.
        Gain => "gain",
    }
}

vocabulary! {
    /// `timing_quality` of `render.progress` and `render.finished`.
    pub enum TimingQuality {
        /// Derived from buffer completions.
        Estimated => "estimated",
    }
}

vocabulary! {
    /// `render.progress.reference_coverage`.
    pub enum ReferenceCoverage {
        /// The reference stream carries the whole playback.
        Full => "full",
        /// Some output bypassed the reference.
        Partial => "partial",
    }
}

vocabulary! {
    /// A `command.ack` rejection code the firmware sends for `render.*`.
    pub enum RenderRejection {
        /// The body was malformed.
        Invalid => "invalid",
        /// The source class is not one the device plays.
        InvalidClass => "invalid_class",
        /// The local asset is not installed or not a built-in.
        UnknownAsset => "unknown_asset",
        /// A newer playback of the class has started.
        StaleGeneration => "stale_generation",
        /// `render.end` named a playback the device no longer has.
        UnknownPlayback => "unknown_playback",
    }
}

vocabulary! {
    /// A `focus.acquire` class.
    pub enum FocusKind {
        /// The user is speaking to this device.
        DialogInput => "dialog_input",
        /// This device is speaking an answer.
        DialogOutput => "dialog_output",
    }
}

vocabulary! {
    /// A `command.ack` rejection code the firmware sends for `focus.*`.
    pub enum FocusRejection {
        /// The body was malformed or the class unknown.
        Invalid => "invalid",
        /// The lease is held at a newer generation.
        StaleGeneration => "stale_generation",
        /// The device holds no lease with this id (it expired).
        UnknownLease => "unknown_lease",
    }
}

vocabulary! {
    /// Why the controller cancels a playback. The device ignores the text;
    /// it is kept for the device log.
    pub enum CancelReason {
        /// The user or Home Assistant stopped or paused it.
        Stopped => "stopped",
        /// A newer playback of the same class replaces it.
        Superseded => "superseded",
        /// A wake or button press interrupted it.
        Interrupted => "interrupted",
        /// A response limit was exceeded.
        ResponseTimeout => "response_timeout",
        /// Its audio could not be fetched or decoded.
        SourceFailed => "source_failed",
        /// Its `render.start` was never acknowledged.
        Unconfirmed => "unconfirmed",
    }
}

/// A playback's `playback_id`: a fresh UUIDv4 per playback.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaybackId(String);

impl PlaybackId {
    /// A fresh id.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as sent.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id a device report named.
    pub(crate) fn from_wire(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl fmt::Display for PlaybackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for PlaybackId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A focus lease's `lease_id`: a fresh UUIDv4 per lease.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LeaseId(String);

impl LeaseId {
    /// A fresh id.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as sent.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for LeaseId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A focus lease's `owner`: the turn or announcement it belongs to. On expiry
/// the device cancels dialog output started under the same owner.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct FocusOwner(String);

impl FocusOwner {
    /// Wraps the owner's id. It must be non-empty to mean anything to the
    /// device; an empty owner matches no dialog output.
    pub fn new(owner: impl Into<String>) -> Self {
        Self(owner.into())
    }

    /// The owner as sent.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A kind-3 render epoch: random and never zero.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderEpoch(NonZeroU64);

impl RenderEpoch {
    /// A random nonzero epoch from the operating system's generator.
    ///
    /// # Panics
    ///
    /// Panics if the operating system has no random source, which no
    /// supported platform lacks.
    pub fn random() -> Self {
        loop {
            let value = getrandom::u64().expect("the operating system provides random numbers");
            if let Some(epoch) = NonZeroU64::new(value) {
                return Self(epoch);
            }
        }
    }

    /// The epoch's value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A built-in earcon the device holds (`device/internal/cue`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Earcon {
    /// `builtin:wake_chime`.
    WakeChime,
    /// `builtin:no_input_chime`; only devices announcing `no_input_chime`
    /// have it.
    NoInputChime,
    /// `builtin:announcement_chime`; only devices announcing
    /// `announcement_chime` have it.
    AnnouncementChime,
}

impl Earcon {
    /// The `local_asset` name.
    pub const fn asset(self) -> &'static str {
        match self {
            Earcon::WakeChime => "builtin:wake_chime",
            Earcon::NoInputChime => "builtin:no_input_chime",
            Earcon::AnnouncementChime => "builtin:announcement_chime",
        }
    }
}

/// The sound an `alert_preview` plays. A hash the device has not installed
/// plays the fallback tone while the device fetches it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum AlertPreviewSound {
    /// `builtin:fallback`.
    Fallback,
    /// An uploaded alert sound, named by its SHA-256.
    Sound(Sha256),
}

/// What a local playback plays.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum LocalSource {
    /// An `earcon`.
    Earcon(Earcon),
    /// An `alert_preview`.
    AlertPreview(AlertPreviewSound),
}

impl LocalSource {
    /// The source class.
    pub fn class(&self) -> SourceClass {
        match self {
            LocalSource::Earcon(_) => SourceClass::Earcon,
            LocalSource::AlertPreview(_) => SourceClass::AlertPreview,
        }
    }

    /// The `local_asset` name.
    pub fn asset(&self) -> String {
        match self {
            LocalSource::Earcon(earcon) => earcon.asset().to_owned(),
            LocalSource::AlertPreview(AlertPreviewSound::Fallback) => "builtin:fallback".to_owned(),
            LocalSource::AlertPreview(AlertPreviewSound::Sound(hash)) => hash.as_str().to_owned(),
        }
    }
}

fn u64_string<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(value)
}

fn epoch_string<S: Serializer>(
    value: &Option<RenderEpoch>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(epoch) => serializer.collect_str(&epoch.get()),
        None => serializer.serialize_none(),
    }
}

/// The PCM16 `format` code, the only one `render.start` accepts.
pub const FORMAT_PCM16: u8 = 1;

/// `render.start`, at the playback's generation. The device acknowledges it
/// `accepted` or rejects it with a [`RenderRejection`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RenderStart {
    /// The playback.
    pub playback_id: PlaybackId,
    /// Its class.
    pub source_class: SourceClass,
    /// The kind-3 epoch for a network class; `null` for a local one.
    #[serde(serialize_with = "epoch_string")]
    pub epoch: Option<RenderEpoch>,
    /// The user-selected gain; device ducking is relative to it.
    pub gain_db: f64,
    /// Always [`FORMAT_PCM16`].
    pub format: u8,
    /// The asset a local class plays; `null` for a network class.
    pub local_asset: Option<String>,
    /// Whether this playback is a Home Assistant announcement.
    pub announcement: bool,
}

impl ControlBody for RenderStart {
    const MESSAGE_TYPE: MessageType = MessageType::RenderStart;
}

/// `render.end`: no audio follows `end_frame`, the source frame after the last
/// one sent. Frames before it may still be in flight on the audio socket.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderEnd {
    /// The playback.
    pub playback_id: PlaybackId,
    /// One past the last frame sent.
    #[serde(serialize_with = "u64_string")]
    pub end_frame: u64,
}

impl ControlBody for RenderEnd {
    const MESSAGE_TYPE: MessageType = MessageType::RenderEnd;
}

/// `render.cancel`, idempotent: the device discards the playback's queued
/// audio and reports `render.finished` `cancelled`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RenderCancel {
    /// The playback.
    pub playback_id: PlaybackId,
    /// Why, for the device log.
    pub reason: CancelReason,
}

impl ControlBody for RenderCancel {
    const MESSAGE_TYPE: MessageType = MessageType::RenderCancel;
}

/// `focus.acquire`, at the owner's generation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FocusAcquire {
    /// The lease.
    pub lease_id: LeaseId,
    /// The turn or announcement that owns it.
    pub owner: FocusOwner,
    /// Its class.
    pub focus: FocusKind,
    /// How long the device keeps it without a renewal.
    pub ttl_ms: u64,
}

impl ControlBody for FocusAcquire {
    const MESSAGE_TYPE: MessageType = MessageType::FocusAcquire;
}

/// `focus.renew`, at the owner's generation, which may only grow.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FocusRenew {
    /// The lease.
    pub lease_id: LeaseId,
    /// The new time to live from the device's receipt.
    pub ttl_ms: u64,
}

impl ControlBody for FocusRenew {
    const MESSAGE_TYPE: MessageType = MessageType::FocusRenew;
}

/// `focus.release`, at the owner's generation. Releasing an unknown lease is
/// accepted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FocusRelease {
    /// The lease.
    pub lease_id: LeaseId,
}

impl ControlBody for FocusRelease {
    const MESSAGE_TYPE: MessageType = MessageType::FocusRelease;
}

/// A command this crate sends; the device acknowledges every one with
/// `command.ack`. The link wraps the body in an envelope carrying the
/// generation passed alongside it.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    /// `render.start`.
    RenderStart(RenderStart),
    /// `render.end`.
    RenderEnd(RenderEnd),
    /// `render.cancel`.
    RenderCancel(RenderCancel),
    /// `focus.acquire`.
    FocusAcquire(FocusAcquire),
    /// `focus.renew`.
    FocusRenew(FocusRenew),
    /// `focus.release`.
    FocusRelease(FocusRelease),
}

impl Command {
    /// The envelope `type`.
    pub fn message_type(&self) -> MessageType {
        match self {
            Command::RenderStart(_) => RenderStart::MESSAGE_TYPE,
            Command::RenderEnd(_) => RenderEnd::MESSAGE_TYPE,
            Command::RenderCancel(_) => RenderCancel::MESSAGE_TYPE,
            Command::FocusAcquire(_) => FocusAcquire::MESSAGE_TYPE,
            Command::FocusRenew(_) => FocusRenew::MESSAGE_TYPE,
            Command::FocusRelease(_) => FocusRelease::MESSAGE_TYPE,
        }
    }

    /// The body as a JSON value, as it goes on the wire.
    pub fn body_json(&self) -> Value {
        let value = match self {
            Command::RenderStart(body) => serde_json::to_value(body),
            Command::RenderEnd(body) => serde_json::to_value(body),
            Command::RenderCancel(body) => serde_json::to_value(body),
            Command::FocusAcquire(body) => serde_json::to_value(body),
            Command::FocusRenew(body) => serde_json::to_value(body),
            Command::FocusRelease(body) => serde_json::to_value(body),
        };
        // Every body is a struct of strings, numbers, booleans and nulls.
        value.expect("command bodies always serialize")
    }
}

/// How the device answered a command, or why no answer came.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AckOutcome {
    /// `accepted`, `applied` or `durable`.
    Accepted,
    /// `rejected`, with the device's code when it gave one.
    Rejected {
        /// The `error` field, `None` when the device sent `null`.
        code: Option<String>,
    },
    /// No acknowledgement will arrive.
    Failed(AckFailure),
}

impl AckOutcome {
    /// The outcome a `command.ack` reports.
    pub fn from_ack(ack: &CommandAck) -> Self {
        match ack.status {
            AckStatus::Accepted | AckStatus::Applied | AckStatus::Durable => AckOutcome::Accepted,
            AckStatus::Rejected => AckOutcome::Rejected {
                code: ack.error.clone(),
            },
        }
    }
}

/// Why a command's acknowledgement will never arrive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AckFailure {
    /// The link's acknowledgement deadline passed. The device may or may not
    /// have acted on the command.
    TimedOut,
    /// The session ended first.
    SessionEnded,
}

/// A device body that breaks the firmware's own encoding.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message_type}.{field} {rule}")]
pub struct BodyError {
    /// The message.
    pub message_type: MessageType,
    /// The offending field.
    pub field: &'static str,
    /// What it must be.
    pub rule: &'static str,
}

struct Fields<'a> {
    body: &'a Map<String, Value>,
    message_type: MessageType,
}

impl<'a> Fields<'a> {
    fn error(&self, field: &'static str, rule: &'static str) -> BodyError {
        BodyError {
            message_type: self.message_type,
            field,
            rule,
        }
    }

    fn string(&self, field: &'static str) -> Result<&'a str, BodyError> {
        self.body
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| self.error(field, "must be a string"))
    }

    fn nonempty_string(&self, field: &'static str) -> Result<&'a str, BodyError> {
        self.string(field)
            .ok()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| self.error(field, "must be a non-empty string"))
    }

    fn reported<T>(
        &self,
        field: &'static str,
        parse: fn(&str) -> Option<T>,
    ) -> Result<Reported<T>, BodyError> {
        let text = self.string(field)?;
        Ok(match parse(text) {
            Some(value) => Reported::Known(value),
            None => Reported::Unrecognized(text.to_owned()),
        })
    }

    fn decimal_u64(&self, field: &'static str) -> Result<u64, BodyError> {
        self.body
            .get(field)
            .and_then(Value::as_str)
            .and_then(parse_decimal_u64)
            .ok_or_else(|| self.error(field, "must be a decimal string of a uint64"))
    }

    /// Absent and `null` are both no value, as the firmware's optional
    /// `NullU64` fields are written.
    fn optional_decimal_u64(&self, field: &'static str) -> Result<Option<u64>, BodyError> {
        match self.body.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => parse_decimal_u64(text)
                .map(Some)
                .ok_or_else(|| self.error(field, "must be a decimal string of a uint64 or null")),
            Some(_) => Err(self.error(field, "must be a decimal string of a uint64 or null")),
        }
    }

    fn decimal_i64(&self, field: &'static str) -> Result<i64, BodyError> {
        let parse = |text: &str| {
            let digits = text.strip_prefix('-').unwrap_or(text);
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            text.parse::<i64>().ok()
        };
        self.body
            .get(field)
            .and_then(Value::as_str)
            .and_then(parse)
            .ok_or_else(|| self.error(field, "must be a decimal string of an int64"))
    }

    fn number_u32(&self, field: &'static str) -> Result<u32, BodyError> {
        self.body
            .get(field)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| self.error(field, "must be a uint32"))
    }

    fn optional_f64(&self, field: &'static str) -> Result<Option<f64>, BodyError> {
        match self.body.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value
                .as_f64()
                .map(Some)
                .ok_or_else(|| self.error(field, "must be a number")),
        }
    }
}

fn parse_decimal_u64(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `render.progress`: sent every 80 ms while a playback plays and on every
/// event. Frame counts are this playback's source frames.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderProgress {
    /// The playback.
    pub playback_id: PlaybackId,
    /// What happened.
    pub event: Reported<ProgressEvent>,
    /// Source frames handed to the output.
    pub submitted_frames: u64,
    /// Source frames whose output buffers completed.
    pub completed_frames: u64,
    /// The device's monotonic time of the completed frontier.
    pub mono_ns: i64,
    /// The uncertainty of `mono_ns`.
    pub uncertainty_us: u32,
    /// How the timing was derived.
    pub timing_quality: Reported<TimingQuality>,
    /// How much of the playback the reference stream carries.
    pub reference_coverage: Reported<ReferenceCoverage>,
    /// Present only for a `seek`.
    pub seek_frame: Option<u64>,
    /// Present only for an `underrun`: the gap's first render-epoch frame.
    pub missing_from: Option<u64>,
    /// Present only for an `underrun`: one past the gap's last frame.
    pub missing_to: Option<u64>,
    /// Present only for a `gain` event.
    pub gain_db: Option<f64>,
}

impl RenderProgress {
    /// Decodes the body as the firmware writes it.
    pub fn parse(body: &Map<String, Value>) -> Result<Self, BodyError> {
        let f = Fields {
            body,
            message_type: MessageType::RenderProgress,
        };
        Ok(Self {
            playback_id: PlaybackId::from_wire(f.nonempty_string("playback_id")?),
            event: f.reported("event", ProgressEvent::from_wire)?,
            submitted_frames: f.decimal_u64("submitted_frames")?,
            completed_frames: f.decimal_u64("completed_frames")?,
            mono_ns: f.decimal_i64("mono_ns")?,
            uncertainty_us: f.number_u32("uncertainty_us")?,
            timing_quality: f.reported("timing_quality", TimingQuality::from_wire)?,
            reference_coverage: f.reported("reference_coverage", ReferenceCoverage::from_wire)?,
            seek_frame: f.optional_decimal_u64("seek_frame")?,
            missing_from: f.optional_decimal_u64("missing_from")?,
            missing_to: f.optional_decimal_u64("missing_to")?,
            gain_db: f.optional_f64("gain_db")?,
        })
    }
}

/// `render.finished`: the playback is over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderFinished {
    /// The playback.
    pub playback_id: PlaybackId,
    /// One past the last source frame whose output completed.
    pub last_completed_frame: u64,
    /// Why it ended.
    pub reason: Reported<FinishReason>,
    /// How the timing was derived.
    pub timing_quality: Reported<TimingQuality>,
}

impl RenderFinished {
    /// Decodes the body as the firmware writes it.
    pub fn parse(body: &Map<String, Value>) -> Result<Self, BodyError> {
        let f = Fields {
            body,
            message_type: MessageType::RenderFinished,
        };
        Ok(Self {
            playback_id: PlaybackId::from_wire(f.nonempty_string("playback_id")?),
            last_completed_frame: f.decimal_u64("last_completed_frame")?,
            reason: f.reported("reason", FinishReason::from_wire)?,
            timing_quality: f.reported("timing_quality", TimingQuality::from_wire)?,
        })
    }
}

/// A device report about a playback the controller started.
#[derive(Clone, Debug, PartialEq)]
pub enum RenderReport {
    /// `render.progress`.
    Progress(RenderProgress),
    /// `render.finished`.
    Finished(RenderFinished),
}

impl RenderReport {
    /// Decodes a device message's body if its type is `render.progress` or
    /// `render.finished`; `None` for any other type.
    pub fn parse(
        message_type: MessageType,
        body: &Map<String, Value>,
    ) -> Option<Result<Self, BodyError>> {
        match message_type {
            MessageType::RenderProgress => Some(RenderProgress::parse(body).map(Self::Progress)),
            MessageType::RenderFinished => Some(RenderFinished::parse(body).map(Self::Finished)),
            _ => None,
        }
    }

    /// The playback the report is about.
    pub fn playback_id(&self) -> &PlaybackId {
        match self {
            RenderReport::Progress(progress) => &progress.playback_id,
            RenderReport::Finished(finished) => &finished.playback_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    #[test]
    fn render_start_serializes_as_the_firmware_decodes_it() {
        let network = RenderStart {
            playback_id: PlaybackId::from_wire("p"),
            source_class: SourceClass::DialogOutput,
            epoch: Some(RenderEpoch(
                NonZeroU64::new(18_446_744_073_709_551_615).unwrap(),
            )),
            gain_db: 0.0,
            format: FORMAT_PCM16,
            local_asset: None,
            announcement: true,
        };
        assert_eq!(
            serde_json::to_value(&network).unwrap(),
            json!({"playback_id":"p","source_class":"dialog_output","epoch":"18446744073709551615",
                   "gain_db":0.0,"format":1,"local_asset":null,"announcement":true})
        );
        let local = RenderStart {
            source_class: SourceClass::Earcon,
            epoch: None,
            local_asset: Some(Earcon::NoInputChime.asset().to_owned()),
            announcement: false,
            ..network
        };
        assert_eq!(
            serde_json::to_value(&local).unwrap(),
            json!({"playback_id":"p","source_class":"earcon","epoch":null,"gain_db":0.0,
                   "format":1,"local_asset":"builtin:no_input_chime","announcement":false})
        );
        let end = RenderEnd {
            playback_id: PlaybackId::from_wire("p"),
            end_frame: 7_680,
        };
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            json!({"playback_id":"p","end_frame":"7680"})
        );
        let acquire = FocusAcquire {
            lease_id: LeaseId("l".into()),
            owner: FocusOwner::new("t"),
            focus: FocusKind::DialogOutput,
            ttl_ms: 3_000,
        };
        assert_eq!(
            serde_json::to_value(&acquire).unwrap(),
            json!({"lease_id":"l","owner":"t","focus":"dialog_output","ttl_ms":3000})
        );
    }

    #[test]
    fn progress_parses_per_event_fields_and_keeps_unknown_values() {
        let body = object(json!({
            "playback_id": "p", "event": "underrun", "submitted_frames": "4096",
            "completed_frames": "2048", "mono_ns": "-5", "uncertainty_us": 42700,
            "timing_quality": "estimated", "reference_coverage": "sideways",
            "missing_from": "100", "missing_to": "580"
        }));
        let progress = RenderProgress::parse(&body).unwrap();
        assert_eq!(progress.event, Reported::Known(ProgressEvent::Underrun));
        assert_eq!(progress.completed_frames, 2_048);
        assert_eq!(progress.mono_ns, -5);
        assert_eq!(progress.missing_from, Some(100));
        assert_eq!(progress.seek_frame, None);
        assert_eq!(
            progress.reference_coverage,
            Reported::Unrecognized("sideways".into())
        );

        let mut bad = body.clone();
        bad.insert("completed_frames".into(), json!(2048));
        assert_eq!(
            RenderProgress::parse(&bad).unwrap_err().field,
            "completed_frames"
        );
        let mut signed = body;
        signed.insert("submitted_frames".into(), json!("+1"));
        assert!(RenderProgress::parse(&signed).is_err());
    }

    #[test]
    fn finished_parses() {
        let body = object(json!({
            "playback_id": "p", "last_completed_frame": "96000",
            "reason": "drained", "timing_quality": "estimated"
        }));
        let report = RenderReport::parse(MessageType::RenderFinished, &body)
            .unwrap()
            .unwrap();
        let RenderReport::Finished(finished) = report else {
            panic!("not finished");
        };
        assert_eq!(finished.last_completed_frame, 96_000);
        assert_eq!(finished.reason, Reported::Known(FinishReason::Drained));
        assert!(RenderReport::parse(MessageType::Heartbeat, &body).is_none());
    }
}
