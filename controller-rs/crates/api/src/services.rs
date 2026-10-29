//! What the device-service routes need from the integrator: Wi-Fi control, recording
//! modes, the diagnostic session recorder, the browser shell, alarms, the sound catalog
//! and Kiosk Satellite login (§3.4, §4.9–§4.11, §6.2).
//!
//! The integrator implements each trait over its services and device runtimes and hands
//! them over in [`DeviceServices`]. The routes check what they can themselves (the
//! caller's role, the request's shape, whether the device is connected and announces the
//! capability, through [`crate::DeviceRuntimes::live`]) before calling; a trait method
//! answers [`ServiceError`] for everything else. Every method completes once the work is
//! done or refused; none of them blocks the caller's runtime.

use std::{fmt, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use axum::body::Bytes;
use echomuse_core::types::DeviceId;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::mpsc;

/// A boxed future, as the traits here return.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Why a device-service request was not done. Each answers its own status; the text is
/// the sentence the dashboard shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceError {
    /// No device has this id (404).
    UnknownDevice,
    /// The device has no live session (409).
    NotConnected,
    /// The device cannot do this; why (409).
    Unsupported(String),
    /// Something already running on the device rules this out; what (409).
    Busy(String),
    /// The request asks for something that cannot be done as asked (400).
    Invalid(String),
    /// The thing named does not exist (404).
    NotFound(String),
    /// The thing is still in use (409).
    InUse(String),
    /// The device or a service it depends on refused or could not be reached (502).
    Upstream(String),
    /// The request may have reached the device, but no answer came back in time; it is
    /// not repeated (504).
    Unknown(String),
    /// A service this needs is busy or stopping (503).
    Unavailable(String),
    /// The controller failed; the text goes to the log, not to the client (500).
    Failed(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("No device has this id"),
            Self::NotConnected => f.write_str("The device is not connected"),
            Self::Unsupported(why)
            | Self::Busy(why)
            | Self::Invalid(why)
            | Self::NotFound(why)
            | Self::InUse(why)
            | Self::Upstream(why)
            | Self::Unknown(why)
            | Self::Unavailable(why)
            | Self::Failed(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ServiceError {}

// ─── Wi-Fi ──────────────────────────────────────────────────────────────────────────

/// One network a Wi-Fi scan found, strongest first.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WifiNetwork {
    /// Its name.
    pub ssid: String,
    /// Its signal, dBm.
    pub signal: i32,
}

/// A network to join, checked the way the firmware checks it
/// (`device/internal/wifi/wifi.go`): a name, and a passphrase that is empty for an open
/// network or 8 to 63 characters, neither containing `"` or `\`.
#[derive(Clone, Eq, PartialEq)]
pub struct WifiCredentials {
    ssid: String,
    psk: String,
}

impl fmt::Debug for WifiCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WifiCredentials")
            .field("ssid", &self.ssid)
            .finish_non_exhaustive()
    }
}

impl WifiCredentials {
    /// The credentials, or why the firmware would refuse them.
    pub fn new(ssid: &str, psk: &str) -> Result<Self, &'static str> {
        if ssid.is_empty() {
            return Err("The network name is empty");
        }
        if ssid.len() > 32 {
            return Err("A network name is at most 32 bytes");
        }
        let quoted = |text: &str| text.contains('"') || text.contains('\\');
        if quoted(ssid) || quoted(psk) {
            return Err("The network name and password cannot contain \" or \\");
        }
        let chars = psk.chars().count();
        if chars != 0 && !(8..=63).contains(&chars) {
            return Err("A Wi-Fi password is 8 to 63 characters, or empty for an open network");
        }
        Ok(Self {
            ssid: ssid.to_owned(),
            psk: psk.to_owned(),
        })
    }

    /// The network name.
    pub fn ssid(&self) -> &str {
        &self.ssid
    }

    /// The passphrase; empty for an open network.
    pub fn psk(&self) -> &str {
        &self.psk
    }
}

/// Wi-Fi control for devices that announce `wifi_control` (the Echo Dot).
pub trait WifiControl: Send + Sync + 'static {
    /// Asks the device to scan and waits for its answer.
    fn scan(&self, device: DeviceId) -> BoxFuture<'_, Result<Vec<WifiNetwork>, ServiceError>>;

    /// Sends the change; its outcome arrives later as the device object's `wifi`.
    fn change(
        &self,
        device: DeviceId,
        credentials: WifiCredentials,
    ) -> BoxFuture<'_, Result<(), ServiceError>>;

    /// The device object's `wifi` field: `{pending, last_result}`.
    fn fields(&self, device: &DeviceId) -> Map<String, Value>;
}

// ─── Recording modes ────────────────────────────────────────────────────────────────

/// A recording mode that keeps files (§4.11).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecordingMode {
    /// Sample collection: clips cut at the silences, in `samples/<device>/`.
    Samples,
    /// Ambient recording: long files, in `ambient/<device>/`.
    Ambient,
}

impl RecordingMode {
    /// The directory beside the database holding the mode's files.
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Samples => "samples",
            Self::Ambient => "ambient",
        }
    }
}

/// A recording's file name: `<unix ms>.wav`, as every existing recording is named.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ClipName(String);

impl ClipName {
    /// The name, when `text` is one.
    pub fn new(text: &str) -> Option<Self> {
        let stem = text.strip_suffix(".wav")?;
        (!stem.is_empty() && stem.len() <= 20 && stem.bytes().all(|b| b.is_ascii_digit()))
            .then(|| Self(text.to_owned()))
    }

    /// The file name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The time in the name: Unix milliseconds.
    pub fn unix_ms(&self) -> Option<u64> {
        self.0.trim_end_matches(".wav").parse().ok()
    }

    /// The AFE sidecar's file name: `<unix ms>.afe.json`.
    pub fn sidecar(&self) -> String {
        format!("{}.afe.json", self.0.trim_end_matches(".wav"))
    }
}

/// One saved recording as the dashboard lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipEntry {
    /// The file name, used in the clip's URLs.
    pub name: String,
    /// When it started, Unix **seconds**.
    pub ts: f64,
    /// Its length, ms.
    pub ms: u64,
    /// Its size, bytes.
    pub bytes: u64,
    /// Whether it has an AFE sidecar.
    pub afe: bool,
}

/// What sample collection reports while it runs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CollectLive {
    /// The room's floor, dBFS; `None` until measured.
    pub floor_db: Option<f64>,
    /// The level a clip opens at, dBFS; `None` until the floor is measured.
    pub open_db: Option<f64>,
    /// Audio blocks examined this session.
    pub frames: u64,
    /// Clips dropped as too short.
    pub dropped_short: u64,
    /// Clips cut at the length limit.
    pub truncated: u64,
}

/// `GET …/samples`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SamplesListing {
    /// The clips, newest first.
    pub clips: Vec<ClipEntry>,
    /// How many.
    pub count: usize,
    /// Their size, bytes.
    pub bytes: u64,
    /// Their length, ms.
    pub ms: u64,
    /// How many are kept.
    pub keep: usize,
    /// Present while collecting on a connected device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<CollectLive>,
}

/// What ambient recording reports while it runs.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AmbientLive {
    /// The open file's length, ms.
    pub ms: u64,
}

/// `GET …/ambient`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AmbientListing {
    /// The finished files, newest first.
    pub clips: Vec<ClipEntry>,
    /// How many.
    pub count: usize,
    /// Their size, bytes.
    pub bytes: u64,
    /// How many are kept.
    pub keep: usize,
    /// How long one file runs before the next starts, ms.
    #[serde(rename = "maxMs")]
    pub max_ms: u64,
    /// Present while a file is being written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<AmbientLive>,
    /// Files already saved in the running session; present while recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<u32>,
}

/// One file of a recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipPart {
    /// The WAV.
    Audio,
    /// The AFE sidecar (JSON).
    Afe,
}

/// Sample collection and ambient recording (§4.11).
pub trait RecordingModes: Send + Sync + 'static {
    /// Arms or disarms a mode and stores the choice; an armed mode records whenever the
    /// device is connected and unmuted.
    fn set_mode(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        enabled: bool,
    ) -> BoxFuture<'_, Result<(), ServiceError>>;

    /// The device's sample clips.
    fn samples(&self, device: DeviceId) -> BoxFuture<'_, Result<SamplesListing, ServiceError>>;

    /// The device's ambient files.
    fn ambient(&self, device: DeviceId) -> BoxFuture<'_, Result<AmbientListing, ServiceError>>;

    /// Where one file of a recording is.
    fn file(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        name: ClipName,
        part: ClipPart,
    ) -> BoxFuture<'_, Result<PathBuf, ServiceError>>;

    /// Deletes one recording (with its sidecar), or every recording of the mode when
    /// `name` is `None`. Returns how many recordings were removed.
    fn delete(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        name: Option<ClipName>,
    ) -> BoxFuture<'_, Result<usize, ServiceError>>;

    /// Every file of the mode's recordings (WAVs and sidecars), for an archive.
    fn archive_files(
        &self,
        device: DeviceId,
        mode: RecordingMode,
    ) -> BoxFuture<'_, Result<Vec<ArchiveFile>, ServiceError>>;

    /// The device object's live recording fields (`collectClips`, `collectLastMs`,
    /// `ambientMs`); empty while nothing records.
    fn fields(&self, device: &DeviceId) -> Map<String, Value>;
}

/// One file of an archive.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchiveFile {
    /// Its name in the archive.
    pub name: String,
    /// Where it is.
    pub path: PathBuf,
    /// Its time, Unix seconds.
    pub unix_secs: i64,
}

// ─── Session recorder ───────────────────────────────────────────────────────────────

/// What a session recording grant lets the recorder keep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecorderScope {
    /// Mic audio.
    pub mic: bool,
    /// Reference audio.
    pub reference: bool,
    /// Loudness cells.
    pub cells: bool,
    /// AFE records.
    pub afe: bool,
    /// Transcripts.
    pub transcripts: bool,
}

/// An administrator's grant request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecorderGrant {
    /// Who asked: the administrator's username.
    pub administrator: String,
    /// How long, in minutes.
    pub minutes: u64,
    /// What may be kept.
    pub scope: RecorderScope,
}

/// The diagnostic session recorder (§6.2). Values are JSON objects in the recorder's own
/// self-describing shapes.
pub trait SessionRecorders: Send + Sync + 'static {
    /// `{status, recordings}` for the device.
    fn status(&self, device: DeviceId) -> BoxFuture<'_, Result<Value, ServiceError>>;

    /// Grants the device recording for a bounded interval; returns the grant status.
    fn grant(
        &self,
        device: DeviceId,
        grant: RecorderGrant,
    ) -> BoxFuture<'_, Result<Value, ServiceError>>;

    /// Withdraws the device's live grant: what is not yet written is discarded.
    fn withdraw(
        &self,
        device: DeviceId,
        administrator: String,
    ) -> BoxFuture<'_, Result<Value, ServiceError>>;

    /// Where a finished recording of the device is.
    fn file(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<PathBuf, ServiceError>>;

    /// Deletes one recording of the device; a recording still being written stops first.
    fn delete(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<(), ServiceError>>;
}

// ─── Shell ──────────────────────────────────────────────────────────────────────────

/// One input for the device's shell, as the dashboard's terminal sends it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellFrame {
    /// Bytes for its standard input.
    Data(Bytes),
    /// The terminal's size (PTY only).
    Resize {
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
}

/// A connected device shell, as the route bridges it to the browser. Closing `input` (or
/// dropping it) ends the shell; `output` ends when the shell does.
#[derive(Debug)]
pub struct ShellChannels {
    /// Whether the device attached a pseudo-terminal.
    pub pty: bool,
    /// Inputs for the shell.
    pub input: mpsc::Sender<ShellFrame>,
    /// The shell's output.
    pub output: mpsc::Receiver<Bytes>,
}

/// The device's browser shell.
pub trait ShellAccess: Send + Sync + 'static {
    /// Opens a shell on the device (asking for a PTY) and waits for it to dial back.
    fn open(&self, device: DeviceId) -> BoxFuture<'_, Result<ShellChannels, ServiceError>>;
}

// ─── Alarms ─────────────────────────────────────────────────────────────────────────

/// `POST …/alarms`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmRequest {
    /// `HH:MM`.
    pub time: String,
    /// `mon` … `sun`; none for a one-shot alarm.
    pub days: Vec<String>,
    /// Its label; empty is "Alarm".
    pub name: String,
    /// `YYYY-MM-DD` for a one-shot alarm on that day.
    pub date: Option<String>,
}

/// `POST …/alarms/cancel`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmCancel {
    /// The label of the alarms to cancel.
    pub name: String,
    /// Their time of day, `HH:MM` or `HH:MM:SS`.
    pub time: String,
    /// Every alarm of the speaker instead.
    pub all: bool,
}

/// What an alarm request did, as the dashboard reads it: `{ok, error?, pending?,
/// delivery_pending?, armed_on_endpoint?}`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct AlarmReply {
    /// Home Assistant's calendar shows the change.
    pub ok: bool,
    /// Why not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Journaled and still being confirmed under the same id.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
    /// Stored, but the speaker has not cached it yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub delivery_pending: bool,
    /// The speaker has durably cached it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub armed_on_endpoint: bool,
}

/// Alarms from the dashboard, through the alert service.
pub trait AlertControl: Send + Sync + 'static {
    /// The alert panel's `status`; `None` when the speaker's alarm calendar is unavailable.
    fn status(&self, device: DeviceId) -> BoxFuture<'_, Result<Option<Value>, ServiceError>>;

    /// Sets an alarm.
    fn set_alarm(
        &self,
        device: DeviceId,
        request: AlarmRequest,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>>;

    /// Cancels alarms.
    fn cancel_alarm(
        &self,
        device: DeviceId,
        request: AlarmCancel,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>>;
}

// ─── Sounds ─────────────────────────────────────────────────────────────────────────

/// The alert sound catalog and previews on a device.
pub trait SoundControl: Send + Sync + 'static {
    /// `{sounds, default_id, unresolved}`.
    fn catalog(&self) -> BoxFuture<'_, Result<Value, ServiceError>>;

    /// Stores and exports an upload; returns the sound as the catalog lists it.
    fn upload(
        &self,
        id: String,
        file_name: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<Value, ServiceError>>;

    /// Deletes a sound no setting selects.
    fn delete(&self, id: String) -> BoxFuture<'_, Result<(), ServiceError>>;

    /// Plays a catalog sound on the device; resolves once the device accepted it.
    fn preview(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<(), ServiceError>>;

    /// Stops a preview; returns whether one was playing.
    fn stop_preview(&self, device: DeviceId) -> BoxFuture<'_, Result<bool, ServiceError>>;
}

// ─── Kiosk Satellite ────────────────────────────────────────────────────────────────

/// Kiosk Satellite login for devices that announce `kiosk_plugin`.
pub trait KioskLogin: Send + Sync + 'static {
    /// Logs in at the device's Kiosk Satellite with `password`, keeps only the token it
    /// returns, and gives it to the device.
    fn login(&self, device: DeviceId, password: String) -> BoxFuture<'_, Result<(), ServiceError>>;
}

/// Every device service the routes call.
#[derive(Clone)]
pub struct DeviceServices {
    /// Wi-Fi control.
    pub wifi: Arc<dyn WifiControl>,
    /// Sample collection and ambient recording.
    pub recording: Arc<dyn RecordingModes>,
    /// The diagnostic session recorder.
    pub recorder: Arc<dyn SessionRecorders>,
    /// The browser shell.
    pub shell: Arc<dyn ShellAccess>,
    /// Alarms.
    pub alerts: Arc<dyn AlertControl>,
    /// Sounds.
    pub sounds: Arc<dyn SoundControl>,
    /// Kiosk Satellite login.
    pub kiosk: Arc<dyn KioskLogin>,
}

impl DeviceServices {
    /// The device object's fields these services decide.
    pub fn device_fields(&self, device: &DeviceId) -> Map<String, Value> {
        let mut fields = self.wifi.fields(device);
        fields.extend(self.recording.fields(device));
        fields
    }
}

impl fmt::Debug for DeviceServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceServices").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_follow_the_firmware_rules() {
        assert!(WifiCredentials::new("home", "secret123").is_ok());
        assert!(WifiCredentials::new("cafe", "").is_ok());
        assert!(WifiCredentials::new("", "secret123").is_err());
        assert!(WifiCredentials::new("home", "short").is_err());
        assert!(WifiCredentials::new("home", &"x".repeat(64)).is_err());
        assert!(WifiCredentials::new("ho\"me", "secret123").is_err());
        assert!(WifiCredentials::new("home", "sec\\ret123").is_err());
    }

    #[test]
    fn clip_names_are_unix_milliseconds() {
        let name = ClipName::new("1787503434055.wav").unwrap();
        assert_eq!(name.unix_ms(), Some(1_787_503_434_055));
        assert_eq!(name.sidecar(), "1787503434055.afe.json");
        for bad in ["", ".wav", "../x.wav", "12.wav.part", "12a.wav", "12"] {
            assert!(ClipName::new(bad).is_none(), "{bad}");
        }
    }
}
