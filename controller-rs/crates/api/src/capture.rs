//! Capture windows (§3.4, §4.11; playback-capture.md): an administrator's script arms a
//! connected device's capture mode, opens and stops recording windows around the
//! playbacks it drives, and gets each window back whole as one WAV, pushed to its
//! webhook or collected with a long poll. The work is done by the integrator's
//! [`CaptureControl`]; these handlers check the caller, the request and whether the
//! device is connected, and shape the answers.
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `POST /api/devices/{id}/capture` `{enabled: true, webhook?, idle_s?}` | admin | the mode's state |
//! | `POST /api/devices/{id}/capture` `{enabled: false}` | admin | the mode's state |
//! | `GET /api/devices/{id}/capture` | admin | the mode's state |
//! | `POST …/capture/window` `{tag, max_ms?}` | admin | `{session, tag, max_ms}` |
//! | `POST …/capture/window/stop` `{session}` | admin | `{session, tag, max_ms}` of the stopped window |
//! | `GET …/capture/recording?session=…&wait=…` | admin | the WAV with its `X-EM-*` headers; 404 when nothing arrived |
//!
//! Every route needs a connected, approved device: an unknown one answers 404, one
//! without a live session 409 `not_connected`. `idle_s` is clamped to 1..=3600
//! (default 300), `max_ms` to 1..=30 000 (default 12 000), and the long poll's `wait`
//! (seconds) to at most 120 (default 30). A webhook must be an `http` or `https` URL
//! with a host; it is checked before anything is armed.
//!
//! Decisions where the documents are silent:
//!
//! - **The state's shape**: `{enabled, transport: "webhook"|"pull"|null, webhook,
//!   idle_s, window: {session, tag, max_ms, opened_ms}|null, pending, dropped,
//!   last_failure: {session, tag, reason, detail}|null}`. `transport`, `webhook` and
//!   `idle_s` are `null` while the mode is off; `pending` counts the finished recordings
//!   not yet collected (pull) or not yet posted (webhook); `dropped` counts the windows
//!   dropped since the mode was armed; `last_failure` is the latest window that ended
//!   without a delivery (`empty`, `muted`, `fell_behind` or `delivery`, the last with the
//!   webhook's error in `detail`).
//! - **Re-arming** an armed mode with the same delivery only renews `idle_s`; a different
//!   delivery is refused (409 `busy`) until the mode is disarmed.
//! - **Stopping** the device's latest window again, or after it closed on its own,
//!   answers that window; any other `session` is refused (409 `conflict`), so a late
//!   stop never lands on a newer window.
//! - **The long poll** with `session` waits for that window only, and answers 404 at
//!   once when that window is not open, not held, or ended without a recording; without
//!   `session` it takes the oldest held recording. It answers 409 `not_applicable` while
//!   the mode is off or delivers to a webhook.
//! - **`tag`** is echoed verbatim in `X-EM-Tag`, so it must be a valid header value:
//!   at most 512 bytes of printable ASCII and spaces (400 otherwise).

use std::{borrow::Cow, fmt, time::Duration};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use echomuse_core::types::DeviceId;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{
    auth::AdminUser,
    body::JsonBody,
    error::{ApiError, ErrorCode},
    params::{DevicePath, QueryParams},
    services::BoxFuture,
    state::ApiState,
};

// ─── Limits ─────────────────────────────────────────────────────────────────────────

/// How long a capture mode may go without a window or a capture request before it
/// clears itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct IdleSeconds(u16);

impl IdleSeconds {
    /// The default, 300 s.
    pub const DEFAULT: Self = Self(300);
    /// The most, 3600 s.
    pub const MAX: Self = Self(3_600);

    /// `seconds`, clamped to 1..=3600.
    pub fn clamped(seconds: u64) -> Self {
        Self(seconds.clamp(1, u64::from(Self::MAX.0)) as u16)
    }

    /// The seconds.
    pub fn seconds(self) -> u16 {
        self.0
    }

    /// As a duration.
    pub fn duration(self) -> Duration {
        Duration::from_secs(u64::from(self.0))
    }
}

/// The longest a window may record before it closes on its own.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WindowMs(u32);

impl WindowMs {
    /// The default, 12 s.
    pub const DEFAULT: Self = Self(12_000);
    /// The most, 30 s (`MAX_WINDOW_MS`).
    pub const MAX: Self = Self(30_000);

    /// `ms`, clamped to 1..=30 000.
    pub fn clamped(ms: u64) -> Self {
        Self(ms.clamp(1, u64::from(Self::MAX.0)) as u32)
    }

    /// The milliseconds.
    pub fn ms(self) -> u32 {
        self.0
    }

    /// As a duration.
    pub fn duration(self) -> Duration {
        Duration::from_millis(u64::from(self.0))
    }
}

/// How long the recording long poll waits by default.
pub const DEFAULT_POLL_WAIT: Duration = Duration::from_secs(30);
/// The longest the recording long poll waits.
pub const MAX_POLL_WAIT: Duration = Duration::from_secs(120);

/// The long poll's `wait` (seconds), defaulted and capped.
pub fn poll_wait(seconds: Option<f64>) -> Result<Duration, String> {
    match seconds {
        None => Ok(DEFAULT_POLL_WAIT),
        Some(seconds) if seconds.is_finite() && seconds >= 0.0 => Ok(Duration::from_secs_f64(
            seconds.min(MAX_POLL_WAIT.as_secs_f64()),
        )),
        Some(_) => Err("wait must be a number of seconds, 0 or more".to_owned()),
    }
}

// ─── Request values ─────────────────────────────────────────────────────────────────

/// Where finished windows are posted: an `http` or `https` URL with a host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureWebhook(url::Url);

impl CaptureWebhook {
    /// Checks a webhook URL before anything is armed: the scheme must be `http` or
    /// `https` and the URL must name a host. This is not authorisation (the caller is an
    /// administrator); it keeps the controller from posting to `file:`, `unix:` or
    /// hostless URLs.
    pub fn parse(text: &str) -> Result<Self, String> {
        let url = url::Url::parse(text.trim())
            .map_err(|error| format!("The webhook is not a URL: {error}"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("The webhook must be an http or https URL".to_owned());
        }
        if url.host_str().is_none_or(str::is_empty) {
            return Err("The webhook URL must name a host".to_owned());
        }
        Ok(Self(url))
    }

    /// The URL.
    pub fn url(&self) -> &url::Url {
        &self.0
    }

    /// The URL as text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// How finished windows reach the caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureTransport {
    /// Each finished window is posted to this webhook.
    Webhook(CaptureWebhook),
    /// The controller holds each finished window until `GET …/capture/recording`.
    Pull,
}

impl CaptureTransport {
    /// Its name in the state.
    pub fn kind(&self) -> TransportKind {
        match self {
            Self::Webhook(_) => TransportKind::Webhook,
            Self::Pull => TransportKind::Pull,
        }
    }
}

/// [`CaptureTransport`]'s name, as the state reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    /// Posted to a webhook.
    Webhook,
    /// Collected with the long poll.
    Pull,
}

/// `POST …/capture` `{enabled: true, …}`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CaptureArm {
    /// How finished windows are delivered.
    pub transport: CaptureTransport,
    /// The dead-man.
    pub idle: IdleSeconds,
}

/// A window's opaque tag, echoed verbatim in `X-EM-Tag`: at most [`CaptureTag::MAX_BYTES`]
/// of printable ASCII and spaces, so it is always a valid header value.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct CaptureTag(String);

impl CaptureTag {
    /// The longest tag.
    pub const MAX_BYTES: usize = 512;

    /// Checks a tag.
    pub fn new(tag: String) -> Result<Self, String> {
        if tag.len() > Self::MAX_BYTES {
            return Err(format!("tag is longer than {} bytes", Self::MAX_BYTES));
        }
        if !tag.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            return Err("tag must be printable ASCII (it is sent back as a header)".to_owned());
        }
        Ok(Self(tag))
    }

    /// The tag.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A window's id, its `session`. Unique within the controller process and across its
/// restarts.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WindowId(pub String);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `GET …/capture/recording`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingRequest {
    /// Only this window's recording; `None`: the oldest held one.
    pub session: Option<WindowId>,
    /// How long to wait for it.
    pub wait: Duration,
}

// ─── Answers ────────────────────────────────────────────────────────────────────────

/// A window as the state and the window routes report it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WindowView {
    /// Its id.
    pub session: WindowId,
    /// Its tag.
    pub tag: CaptureTag,
    /// When it closes on its own.
    pub max_ms: WindowMs,
    /// When it opened, Unix milliseconds; `None` in the open and stop answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_ms: Option<u64>,
}

/// Why a window ended without a delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowFailureReason {
    /// No audio arrived while it was open.
    Empty,
    /// The privacy mute went on; its audio was discarded.
    Muted,
    /// The recording path fell behind the microphone; its audio was discarded.
    FellBehind,
    /// The webhook did not take it.
    Delivery,
}

/// The latest window that ended without a delivery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WindowFailure {
    /// The window.
    pub session: WindowId,
    /// Its tag.
    pub tag: CaptureTag,
    /// Why.
    pub reason: WindowFailureReason,
    /// The webhook's error, for [`WindowFailureReason::Delivery`].
    pub detail: Option<String>,
}

/// `GET …/capture`: the mode's state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CaptureState {
    /// The mode is armed.
    pub enabled: bool,
    /// How windows are delivered; `None` while off.
    pub transport: Option<TransportKind>,
    /// The webhook; `None` while off or pulling.
    pub webhook: Option<String>,
    /// The dead-man, seconds; `None` while off.
    pub idle_s: Option<IdleSeconds>,
    /// The open window.
    pub window: Option<WindowView>,
    /// Finished recordings not yet collected or posted.
    pub pending: usize,
    /// Windows dropped since the mode was armed.
    pub dropped: u64,
    /// The latest window that ended without a delivery since the mode was armed.
    pub last_failure: Option<WindowFailure>,
}

impl CaptureState {
    /// The state of a mode that is off.
    pub fn off() -> Self {
        Self {
            enabled: false,
            transport: None,
            webhook: None,
            idle_s: None,
            window: None,
            pending: 0,
            dropped: 0,
            last_failure: None,
        }
    }
}

/// One finished window: the WAV (16 kHz mono PCM16, 44-byte header) and what the
/// `X-EM-*` headers say about it.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureRecording {
    /// The device.
    pub device: DeviceId,
    /// The window.
    pub session: WindowId,
    /// Its tag.
    pub tag: CaptureTag,
    /// Its length, ms.
    pub ms: u64,
    /// Its loudest sample, dBFS.
    pub peak_db: f64,
    /// Its quietest 80 ms, dBFS.
    pub floor_db: f64,
    /// Its samples (mono: one per frame).
    pub frames: u64,
    /// Windows dropped since the previous delivery.
    pub dropped: u64,
    /// It closed at `max_ms` (on audio time or on the wall clock), not on a stop.
    pub truncated: bool,
    /// When it opened, Unix milliseconds.
    pub opened_ms: u64,
    /// The WAV.
    pub wav: Bytes,
}

impl CaptureRecording {
    /// The `X-EM-*` headers, for the long poll's answer and the webhook's POST alike.
    pub fn headers(&self) -> [(&'static str, String); 10] {
        [
            ("X-EM-Device", self.device.0.clone()),
            ("X-EM-Session", self.session.0.clone()),
            ("X-EM-Tag", self.tag.0.clone()),
            ("X-EM-Ms", self.ms.to_string()),
            ("X-EM-Peak-Db", format!("{:.1}", self.peak_db)),
            ("X-EM-Floor-Db", format!("{:.1}", self.floor_db)),
            ("X-EM-Frames", self.frames.to_string()),
            ("X-EM-Dropped", self.dropped.to_string()),
            ("X-EM-Truncated", self.truncated.to_string()),
            ("X-EM-Opened-Ms", self.opened_ms.to_string()),
        ]
    }
}

/// Why a capture request was not done.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CaptureError {
    /// No approved device has this id (404).
    UnknownDevice,
    /// The device has no live session (409 `not_connected`).
    NotConnected,
    /// The mode is off (409 `not_applicable`).
    NotArmed,
    /// The mode delivers to a webhook, so nothing is held (409 `not_applicable`).
    PushMode,
    /// The mode is armed with another delivery (409 `busy`).
    OtherTransport,
    /// A window is already open (409 `busy`).
    WindowOpen,
    /// The `session` is not the device's latest window (409 `conflict`).
    StaleWindow,
    /// Nothing arrived within the wait, or the window ended without a recording (404).
    NothingArrived(Cow<'static, str>),
    /// A service capture needs is stopping or not part of this build (503).
    Unavailable(Cow<'static, str>),
    /// The controller failed; the text goes to the log (500).
    Failed(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("No device has this id"),
            Self::NotConnected => f.write_str("The device is not connected"),
            Self::NotArmed => f.write_str("Capture mode is not on"),
            Self::PushMode => f.write_str("Capture mode delivers to its webhook"),
            Self::OtherTransport => {
                f.write_str("Capture mode is on with another delivery; turn it off first")
            }
            Self::WindowOpen => f.write_str("A capture window is already open"),
            Self::StaleWindow => f.write_str("That is not the device's latest capture window"),
            Self::NothingArrived(why) | Self::Unavailable(why) => f.write_str(why),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<CaptureError> for ApiError {
    fn from(error: CaptureError) -> Self {
        let message = error.to_string();
        let (status, code) = match error {
            CaptureError::UnknownDevice | CaptureError::NothingArrived(_) => {
                (StatusCode::NOT_FOUND, ErrorCode::NotFound)
            }
            CaptureError::NotConnected => (StatusCode::CONFLICT, ErrorCode::NotConnected),
            CaptureError::NotArmed | CaptureError::PushMode => {
                (StatusCode::CONFLICT, ErrorCode::NotApplicable)
            }
            CaptureError::OtherTransport | CaptureError::WindowOpen => {
                (StatusCode::CONFLICT, ErrorCode::Busy)
            }
            CaptureError::StaleWindow => (StatusCode::CONFLICT, ErrorCode::Conflict),
            CaptureError::Unavailable(why) => return ApiError::unavailable(why),
            CaptureError::Failed(reason) => {
                eprintln!("api: capture failed: {reason}");
                return ApiError::internal("The controller could not do that");
            }
        };
        ApiError::new(status, code, message)
    }
}

/// Capture windows on the device runtimes. Every method answers at once except
/// [`CaptureControl::recording`], which waits at most the request's `wait`; none blocks
/// the caller's runtime. Each call is a sign of life for the mode's `idle_s` dead-man.
pub trait CaptureControl: Send + Sync + 'static {
    /// Arms (or renews) the device's capture mode on its live session.
    fn arm(&self, device: &DeviceId, arm: CaptureArm) -> Result<CaptureState, CaptureError>;

    /// Disarms it, discarding an open window and held recordings.
    fn disarm(&self, device: &DeviceId) -> Result<CaptureState, CaptureError>;

    /// The mode's state.
    fn state(&self, device: &DeviceId) -> Result<CaptureState, CaptureError>;

    /// Opens a window.
    fn open_window(
        &self,
        device: &DeviceId,
        tag: CaptureTag,
        max: WindowMs,
    ) -> Result<WindowView, CaptureError>;

    /// Stops the window `session`, which is delivered.
    fn stop_window(
        &self,
        device: &DeviceId,
        session: &WindowId,
    ) -> Result<WindowView, CaptureError>;

    /// A finished recording (pull), consumed by this read.
    fn recording(
        &self,
        device: DeviceId,
        request: RecordingRequest,
    ) -> BoxFuture<'_, Result<CaptureRecording, CaptureError>>;

    /// The device object's capture field: `captureMode`.
    fn fields(&self, device: &DeviceId) -> Map<String, Value>;
}

// ─── Routes ─────────────────────────────────────────────────────────────────────────

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/devices/{id}/capture", get(state).post(set))
        .route("/api/devices/{id}/capture/window", post(open_window))
        .route("/api/devices/{id}/capture/window/stop", post(stop_window))
        .route("/api/devices/{id}/capture/recording", get(recording))
}

/// The device has a live session.
fn connected(state: &ApiState, device: &DeviceId) -> Result<(), ApiError> {
    let live = state
        .runtimes()
        .live(device)
        .ok_or(CaptureError::UnknownDevice)?;
    live.session.ok_or(CaptureError::NotConnected)?;
    Ok(())
}

#[derive(Deserialize)]
struct SetBody {
    enabled: bool,
    #[serde(default)]
    webhook: Option<String>,
    #[serde(default)]
    idle_s: Option<u64>,
}

async fn set(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<SetBody>,
) -> Result<Json<CaptureState>, ApiError> {
    connected(&state, &device)?;
    if !body.enabled {
        return Ok(Json(state.capture().disarm(&device)?));
    }
    let transport = match body.webhook.as_deref() {
        Some(url) if !url.trim().is_empty() => {
            CaptureTransport::Webhook(CaptureWebhook::parse(url).map_err(ApiError::bad_request)?)
        }
        _ => CaptureTransport::Pull,
    };
    let idle = body
        .idle_s
        .map_or(IdleSeconds::DEFAULT, IdleSeconds::clamped);
    Ok(Json(
        state
            .capture()
            .arm(&device, CaptureArm { transport, idle })?,
    ))
}

async fn state(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<CaptureState>, ApiError> {
    connected(&state, &device)?;
    Ok(Json(state.capture().state(&device)?))
}

#[derive(Deserialize)]
struct WindowBody {
    tag: String,
    #[serde(default)]
    max_ms: Option<u64>,
}

async fn open_window(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<WindowBody>,
) -> Result<Json<WindowView>, ApiError> {
    connected(&state, &device)?;
    let tag = CaptureTag::new(body.tag).map_err(ApiError::bad_request)?;
    let max = body.max_ms.map_or(WindowMs::DEFAULT, WindowMs::clamped);
    Ok(Json(state.capture().open_window(&device, tag, max)?))
}

#[derive(Deserialize)]
struct StopBody {
    session: WindowId,
}

async fn stop_window(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<StopBody>,
) -> Result<Json<WindowView>, ApiError> {
    connected(&state, &device)?;
    Ok(Json(state.capture().stop_window(&device, &body.session)?))
}

#[derive(Deserialize)]
struct RecordingQuery {
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    wait: Option<f64>,
}

async fn recording(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<RecordingQuery>,
) -> Result<Response, ApiError> {
    connected(&state, &device)?;
    let wait = poll_wait(query.wait).map_err(ApiError::bad_request)?;
    let request = RecordingRequest {
        session: query.session.filter(|s| !s.is_empty()).map(WindowId),
        wait,
    };
    let recording = state.capture().recording(device, request).await?;
    let mut response = Body::from(recording.wav.clone()).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(recording.wav.len()),
    );
    for (name, value) in recording.headers() {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(lower(name)), value);
        }
    }
    Ok(response)
}

/// A header name in the lower case `HeaderName::from_static` needs.
fn lower(name: &'static str) -> &'static str {
    match name {
        "X-EM-Device" => "x-em-device",
        "X-EM-Session" => "x-em-session",
        "X-EM-Tag" => "x-em-tag",
        "X-EM-Ms" => "x-em-ms",
        "X-EM-Peak-Db" => "x-em-peak-db",
        "X-EM-Floor-Db" => "x-em-floor-db",
        "X-EM-Frames" => "x-em-frames",
        "X-EM-Dropped" => "x-em-dropped",
        "X-EM-Truncated" => "x-em-truncated",
        "X-EM-Opened-Ms" => "x-em-opened-ms",
        other => unreachable!("not a capture header: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_and_window_lengths_are_clamped() {
        assert_eq!(IdleSeconds::clamped(0).seconds(), 1);
        assert_eq!(IdleSeconds::clamped(300).seconds(), 300);
        assert_eq!(IdleSeconds::clamped(3_601).seconds(), 3_600);
        assert_eq!(IdleSeconds::clamped(u64::MAX).seconds(), 3_600);
        assert_eq!(IdleSeconds::DEFAULT.seconds(), 300);
        assert_eq!(WindowMs::clamped(0).ms(), 1);
        assert_eq!(WindowMs::clamped(12_000).ms(), 12_000);
        assert_eq!(WindowMs::clamped(30_001).ms(), 30_000);
        assert_eq!(WindowMs::clamped(u64::MAX).ms(), 30_000);
        assert_eq!(WindowMs::DEFAULT.ms(), 12_000);
    }

    #[test]
    fn the_poll_wait_defaults_to_30_s_and_is_capped_at_120() {
        assert_eq!(poll_wait(None), Ok(Duration::from_secs(30)));
        assert_eq!(poll_wait(Some(0.0)), Ok(Duration::ZERO));
        assert_eq!(poll_wait(Some(2.5)), Ok(Duration::from_millis(2_500)));
        assert_eq!(poll_wait(Some(600.0)), Ok(Duration::from_secs(120)));
        assert!(poll_wait(Some(-1.0)).is_err());
        assert!(poll_wait(Some(f64::NAN)).is_err());
    }

    #[test]
    fn a_webhook_must_be_http_or_https_with_a_host() {
        for good in [
            "http://192.0.2.5:8080/clip",
            "https://driver.example/clip?x=1",
            "  http://host/  ",
        ] {
            assert!(CaptureWebhook::parse(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "clip",
            "/clip",
            "file:///etc/passwd",
            "ftp://host/clip",
            "unix:/run/socket",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "http://",
            "http://:80/clip",
        ] {
            assert!(CaptureWebhook::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(
            CaptureWebhook::parse("http://host:9/clip")
                .unwrap()
                .as_str(),
            "http://host:9/clip"
        );
    }

    #[test]
    fn a_tag_is_a_header_value() {
        assert!(CaptureTag::new(String::new()).is_ok());
        assert!(CaptureTag::new("cv/clip_0001.mp3 vol=0.4 player=kitchen".into()).is_ok());
        assert!(CaptureTag::new("x".repeat(512)).is_ok());
        assert!(CaptureTag::new("x".repeat(513)).is_err());
        assert!(CaptureTag::new("line\nbreak".into()).is_err());
        assert!(CaptureTag::new("caf\u{e9}".into()).is_err());
    }

    #[test]
    fn every_header_has_its_lower_case_name() {
        let recording = CaptureRecording {
            device: DeviceId("D".into()),
            session: WindowId("s".into()),
            tag: CaptureTag::new("t".into()).unwrap(),
            ms: 1,
            peak_db: -3.04,
            floor_db: -60.0,
            frames: 16,
            dropped: 0,
            truncated: false,
            opened_ms: 5,
            wav: Bytes::new(),
        };
        for (name, _) in recording.headers() {
            assert_eq!(lower(name), name.to_ascii_lowercase());
        }
        assert_eq!(recording.headers()[4].1, "-3.0");
        assert_eq!(recording.headers()[8].1, "false");
    }

    #[test]
    fn refusals_answer_their_status() {
        let status = |error: CaptureError| ApiError::from(error).status();
        assert_eq!(status(CaptureError::UnknownDevice), StatusCode::NOT_FOUND);
        assert_eq!(status(CaptureError::NotConnected), StatusCode::CONFLICT);
        assert_eq!(status(CaptureError::WindowOpen), StatusCode::CONFLICT);
        assert_eq!(status(CaptureError::StaleWindow), StatusCode::CONFLICT);
        assert_eq!(
            status(CaptureError::NothingArrived("nothing".into())),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            ApiError::from(CaptureError::NotConnected).code(),
            ErrorCode::NotConnected
        );
    }
}
