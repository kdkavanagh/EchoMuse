//! Capture windows (spec §3.4, §4.11; playback-capture.md): a driver script arms a
//! connected device's capture mode, opens a recording window around each playback it
//! drives, and gets each window back whole as one WAV.
//!
//! - **Audio.** While the mode is armed the device's runtime keeps the recording modes'
//!   `diagnostic` subscription open, exactly as for sample collection and ambient
//!   recording, so the device takes no wake and the ring pulses. Every armed mode taps
//!   the same mic chunks through the session's [`DiagnosticTee`] (16 kHz mono PCM16, no
//!   gain). A window keeps the mic samples that arrive while it is open; a range the
//!   device never sent is kept as silence, so the recording keeps its timeline.
//! - **State.** Not persisted. The mode belongs to one link session: it is cleared when
//!   that session ends and when the device is deleted, and a driver re-arms after a
//!   reconnect.
//! - **Closing.** A window closes on its stop, on audio time at `max_ms` (truncated),
//!   and on the wall clock at `max_ms` + [`WINDOW_GRACE`] (truncated), so a stalled link
//!   cannot hold it open. An empty window is a failure, never a delivery; a mute discards
//!   the open window's audio (privacy) and fails it.
//! - **Delivery.** Pull: up to [`CAPTURE_QUEUE_MAX`] finished recordings are held for the
//!   long poll, the oldest dropped first. Push: a per-device sender task posts each to
//!   the webhook off a queue of [`CAPTURE_QUEUE_MAX`]; a full queue drops the window.
//!   Either way a drop is counted, logged and reported to the next delivery as
//!   `X-EM-Dropped`. Redirects are never followed. The mic path never waits on HTTP.
//! - **Dead-man.** No window and no capture request for `idle_s` clears the mode, with a
//!   device log line naming the reason. It never fires while a window is open or a long
//!   poll waits.
//! - **Disarm** is synchronous: the watchdog that runs it is never cancelled under it; it
//!   ends on its own when it sees the mode gone.
//!
//! Decisions where the documents are silent (see also [`echomuse_api::capture`]):
//! `X-EM-Peak-Db` is the loudest sample and `X-EM-Floor-Db` the quietest 80 ms block's
//! RMS (the whole window when shorter), both dBFS, rounded to 0.1 and never below
//! −90; `X-EM-Frames` is the WAV's sample count; `X-EM-Opened-Ms` is the Unix time the
//! window opened; a pull store that is full drops its oldest recording, since the newest
//! is the one a live driver is waiting for; a webhook that answers anything but 2xx, or
//! cannot be reached within [`WEBHOOK_TIMEOUT`], fails that window (`delivery`).

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use echomuse_api::{
    ApiEvent, BoxFuture, CaptureArm, CaptureControl, CaptureError, CaptureRecording, CaptureState,
    CaptureTag, CaptureTransport, CaptureWebhook, DeviceStatePatch, EventPublisher, IdleSeconds,
    RecordingRequest, TransportKind, WindowFailure, WindowFailureReason, WindowId, WindowMs,
    WindowView,
};
use echomuse_core::types::{DeviceId, SessionId};
use echomuse_protocol::StreamId;
use echomuse_records::{recordings::wav_bytes, vocab::LogLevel};
use echomuse_uplink::{ChunkBody, ChunkData, CloseReason, EndCause};
use echomuse_voice::DiagnosticAudio;
use serde_json::{Map, Value, json};
use tokio::{
    sync::{mpsc, watch},
    time::Instant,
};

use crate::{
    app::Directory,
    device_logs::LogSink,
    recording_modes::{Modes, RECORDER_QUEUE},
    supervision::TaskSupervisor,
};

/// Capture samples per second.
const RATE: u64 = 16_000;
/// Samples per 80 ms block, the floor's unit.
const BLOCK: usize = 1_280;
/// The quietest level reported, dBFS.
const FLOOR_MIN_DB: f64 = -90.0;
/// Finished windows queued for the webhook, or held for the long poll.
pub const CAPTURE_QUEUE_MAX: usize = 4;
/// How long past `max_ms` the wall clock lets a window stay open.
pub const WINDOW_GRACE: Duration = Duration::from_secs(2);
/// How long one webhook POST may take.
pub const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn dbfs(level: f64) -> f64 {
    if level <= 0.0 {
        return FLOOR_MIN_DB;
    }
    (20.0 * (level / 32_768.0).log10()).max(FLOOR_MIN_DB)
}

/// The loudest sample, dBFS.
fn peak_db(pcm: &[i16]) -> f64 {
    let peak = pcm
        .iter()
        .map(|s| i32::from(*s).unsigned_abs())
        .max()
        .unwrap_or(0);
    dbfs(f64::from(peak))
}

/// The quietest full 80 ms block's RMS, dBFS; the whole window's when shorter.
fn floor_db(pcm: &[i16]) -> f64 {
    let rms = |block: &[i16]| {
        let sum: f64 = block.iter().map(|s| f64::from(*s).powi(2)).sum();
        (sum / block.len().max(1) as f64).sqrt()
    };
    let quietest = pcm
        .as_chunks::<BLOCK>()
        .0
        .iter()
        .map(|block| rms(block))
        .min_by(f64::total_cmp)
        .unwrap_or_else(|| rms(pcm));
    dbfs(quietest)
}

/// `{"captureMode": armed}`.
fn mode_field(armed: bool) -> Map<String, Value> {
    Map::from_iter([("captureMode".to_owned(), json!(armed))])
}

// ─── State ──────────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Inner {
    /// Each device's attached link session, as its runtime's taps report it.
    sessions: HashMap<DeviceId, SessionId>,
    /// Armed modes.
    armed: HashMap<DeviceId, Armed>,
    /// The last arm's generation.
    generation: u64,
    /// The last window's number.
    windows: u64,
}

/// How an armed mode delivers.
enum Delivery {
    Webhook {
        url: CaptureWebhook,
        queue: mpsc::Sender<CaptureRecording>,
    },
    Pull,
}

/// One device's armed mode.
struct Armed {
    /// The link session it was armed on.
    session: SessionId,
    /// This arm's identity: taps and tasks of an earlier arm do not touch it.
    generation: u64,
    delivery: Delivery,
    idle: IdleSeconds,
    /// The latest capture request, or the latest window's close.
    last_call: Instant,
    /// Long polls waiting now.
    polls: usize,
    window: Option<OpenWindow>,
    /// The latest window, open or closed.
    last_window: Option<WindowView>,
    /// Finished recordings waiting for the long poll (pull).
    held: VecDeque<CaptureRecording>,
    /// Recordings queued for the webhook or being posted.
    posting: usize,
    dropped: u64,
    /// Drops not yet reported to a delivery.
    dropped_unreported: u64,
    last_failure: Option<WindowFailure>,
    /// Bumped on every change, for the watchdog and the long polls; dropped with the arm.
    revision: watch::Sender<u64>,
}

struct OpenWindow {
    view: WindowView,
    max_samples: usize,
    /// The wall-clock close.
    deadline: Instant,
    pcm: Vec<i16>,
}

impl Armed {
    fn changed(&self) {
        self.revision.send_modify(|r| *r = r.wrapping_add(1));
    }

    fn touch(&mut self) {
        self.last_call = Instant::now();
        self.changed();
    }

    fn state(&self) -> CaptureState {
        let (transport, webhook) = match &self.delivery {
            Delivery::Webhook { url, .. } => {
                (TransportKind::Webhook, Some(url.as_str().to_owned()))
            }
            Delivery::Pull => (TransportKind::Pull, None),
        };
        CaptureState {
            enabled: true,
            transport: Some(transport),
            webhook,
            idle_s: Some(self.idle),
            window: self.window.as_ref().map(|w| w.view.clone()),
            pending: self.held.len() + self.posting,
            dropped: self.dropped,
            last_failure: self.last_failure.clone(),
        }
    }

    fn same_delivery(&self, transport: &CaptureTransport) -> bool {
        match (&self.delivery, transport) {
            (Delivery::Webhook { url, .. }, CaptureTransport::Webhook(new)) => url == new,
            (Delivery::Pull, CaptureTransport::Pull) => true,
            _ => false,
        }
    }

    fn fail(
        &mut self,
        session: &WindowId,
        tag: &CaptureTag,
        reason: WindowFailureReason,
        detail: Option<String>,
    ) {
        self.last_failure = Some(WindowFailure {
            session: session.clone(),
            tag: tag.clone(),
            reason,
            detail,
        });
    }
}

/// What a closed window left to say once the lock is released.
enum Closed {
    Delivered,
    Failed(String),
    Dropped(String),
}

// ─── The hub ────────────────────────────────────────────────────────────────────────

/// Every device's capture mode. Cheap to share; implements [`CaptureControl`].
pub struct CaptureHub {
    me: Weak<CaptureHub>,
    inner: Mutex<Inner>,
    /// When this controller started, Unix ms: part of every window id, so an id from an
    /// earlier run never names a window of this one.
    started_ms: u64,
    http: reqwest::Client,
    logs: LogSink,
    directory: OnceLock<watch::Receiver<Directory>>,
    publisher: Arc<OnceLock<EventPublisher>>,
    tasks: TaskSupervisor,
}

/// The webhook client: the system's roots for `https`, no redirects, a bounded POST.
fn webhook_client() -> Result<reqwest::Client, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| format!("the capture webhook's TLS: {error}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(WEBHOOK_TIMEOUT)
        .build()
        .map_err(|error| format!("the capture webhook client: {error}"))
}

impl CaptureHub {
    /// The hub, with no device connected yet.
    pub fn new(
        logs: LogSink,
        publisher: Arc<OnceLock<EventPublisher>>,
        tasks: TaskSupervisor,
    ) -> Result<Arc<Self>, String> {
        let http = webhook_client()?;
        Ok(Arc::new_cyclic(|me| Self {
            me: me.clone(),
            inner: Mutex::default(),
            started_ms: unix_ms(),
            http,
            logs,
            directory: OnceLock::new(),
            publisher,
            tasks,
        }))
    }

    /// The device runtimes, for telling a runtime its recording changed.
    pub fn set_directory(&self, directory: watch::Receiver<Directory>) {
        let _ = self.directory.set(directory);
    }

    fn me(&self) -> Arc<Self> {
        self.me
            .upgrade()
            .expect("the hub is alive while it is called")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("capture lock")
    }

    fn publish(&self, device: &DeviceId, armed: bool) {
        if let Some(publisher) = self.publisher.get() {
            publisher.publish(&ApiEvent::DeviceUpdate {
                device_id: device.clone(),
                state: DeviceStatePatch(mode_field(armed)),
            });
        }
    }

    fn log(&self, device: &DeviceId, level: LogLevel, message: String) {
        match level {
            LogLevel::Info => tracing::info!(%device, "{message}"),
            _ => tracing::warn!(%device, "{message}"),
        }
        self.logs.controller_line(device, level, message);
    }

    /// Tells the device's runtime to arm or disarm its recording subscription.
    fn recording_changed(&self, device: &DeviceId) {
        let handle = self
            .directory
            .get()
            .and_then(|d| d.borrow().get(device).cloned());
        if let Some(handle) = handle
            && let Err(error) = handle.recording_changed()
        {
            tracing::warn!(%device, %error, "the runtime did not take the capture change");
        }
    }

    fn known(&self, device: &DeviceId) -> bool {
        self.directory
            .get()
            .is_some_and(|d| d.borrow().contains_key(device))
    }

    /// The device's live session, or why there is none.
    fn session(&self, inner: &Inner, device: &DeviceId) -> Result<SessionId, CaptureError> {
        match inner.sessions.get(device) {
            Some(session) => Ok(*session),
            None if self.known(device) => Err(CaptureError::NotConnected),
            None => Err(CaptureError::UnknownDevice),
        }
    }

    /// The device's mode armed on its live session.
    fn armed<'a>(
        &self,
        inner: &'a mut Inner,
        device: &DeviceId,
    ) -> Result<&'a mut Armed, CaptureError> {
        let session = self.session(inner, device)?;
        inner
            .armed
            .get_mut(device)
            .filter(|armed| armed.session == session)
            .ok_or(CaptureError::NotArmed)
    }

    // ── The device runtime's side ──

    /// A link session of `device` attached.
    pub fn attached(&self, device: &DeviceId, session: SessionId) {
        self.lock().sessions.insert(device.clone(), session);
    }

    /// A link session of `device` ended: a mode armed on it is cleared.
    pub fn detached(&self, device: &DeviceId, session: SessionId) {
        let cleared = {
            let mut inner = self.lock();
            if inner.sessions.get(device) == Some(&session) {
                inner.sessions.remove(device);
            }
            match inner.armed.get(device) {
                Some(armed) if armed.session == session => inner.armed.remove(device),
                _ => None,
            }
        };
        if cleared.is_some() {
            self.log(
                device,
                LogLevel::Info,
                "capture mode cleared: the device's session ended".to_owned(),
            );
            self.publish(device, false);
        }
    }

    /// Forgets a deleted device.
    pub fn forget(&self, device: &DeviceId) {
        let mut inner = self.lock();
        inner.sessions.remove(device);
        inner.armed.remove(device);
    }

    /// Capture's tap on `session` of `device`, while the mode is armed on it.
    pub fn tap(&self, device: &DeviceId, session: SessionId) -> Option<CaptureTap> {
        let inner = self.lock();
        let armed = inner.armed.get(device).filter(|a| a.session == session)?;
        Some(CaptureTap {
            hub: self.me(),
            device: device.clone(),
            generation: armed.generation,
        })
    }

    /// A fan-out for a session's diagnostic subscription, and the sender the voice path
    /// delivers to.
    pub fn tee(&self) -> (DiagnosticTee, mpsc::Sender<DiagnosticAudio>) {
        DiagnosticTee::start(&self.tasks)
    }

    /// One delivery of the diagnostic subscription.
    fn audio(&self, device: &DeviceId, generation: u64, audio: &DiagnosticAudio) {
        let mut said = None;
        {
            let mut inner = self.lock();
            let Some(armed) = inner
                .armed
                .get_mut(device)
                .filter(|a| a.generation == generation)
            else {
                return;
            };
            match audio {
                DiagnosticAudio::Chunk(chunk) if chunk.stream == StreamId::Mic => {
                    let Some(window) = armed.window.as_mut() else {
                        return;
                    };
                    let room = window.max_samples.saturating_sub(window.pcm.len());
                    match &chunk.body {
                        ChunkBody::Data {
                            data: ChunkData::Pcm(pcm),
                            ..
                        } => window.pcm.extend(pcm.samples().take(room)),
                        ChunkBody::Missing(_) => {
                            let len = chunk.span.end.saturating_sub(chunk.span.start) as usize;
                            window.pcm.extend(std::iter::repeat_n(0, len.min(room)));
                        }
                        ChunkBody::Data { .. } => {}
                    }
                    if window.pcm.len() >= window.max_samples {
                        said = Some(self.close(device, armed, true));
                    }
                }
                DiagnosticAudio::Ended(EndCause::Muted) => {
                    if let Some(window) = armed.window.take() {
                        let view = &window.view;
                        armed.fail(&view.session, &view.tag, WindowFailureReason::Muted, None);
                        armed.last_call = Instant::now();
                        armed.changed();
                        said = Some(Closed::Failed(format!(
                            "capture window {} ({}) discarded: the device was muted",
                            window.view.session,
                            window.view.tag.as_str()
                        )));
                    }
                }
                // A window outlives its subscription: the next one (after a new capture
                // epoch, say) carries on, and the wall clock closes it otherwise.
                DiagnosticAudio::Chunk(_) | DiagnosticAudio::Ended(_) => {}
            }
        }
        if let Some(said) = said {
            self.said(device, said);
        }
    }

    /// The tee's channel closed in the middle of a subscription: the recording path fell
    /// behind, so the open window is discarded and the runtime reopens the subscription.
    fn lost(&self, device: &DeviceId, generation: u64) {
        let said = {
            let mut inner = self.lock();
            let Some(armed) = inner
                .armed
                .get_mut(device)
                .filter(|a| a.generation == generation)
            else {
                return;
            };
            armed.window.take().map(|window| {
                let view = &window.view;
                armed.fail(
                    &view.session,
                    &view.tag,
                    WindowFailureReason::FellBehind,
                    None,
                );
                armed.touch();
                Closed::Failed(format!(
                    "capture window {} ({}) discarded: the recording fell behind",
                    window.view.session,
                    window.view.tag.as_str()
                ))
            })
        };
        if let Some(said) = said {
            self.said(device, said);
        }
        self.recording_changed(device);
    }

    fn said(&self, device: &DeviceId, said: Closed) {
        match said {
            Closed::Delivered => {}
            Closed::Failed(message) | Closed::Dropped(message) => {
                self.log(device, LogLevel::Warn, message)
            }
        }
    }

    /// Closes the open window and delivers it (or fails it when empty).
    fn close(&self, device: &DeviceId, armed: &mut Armed, truncated: bool) -> Closed {
        let Some(window) = armed.window.take() else {
            return Closed::Delivered;
        };
        armed.last_window = Some(WindowView {
            opened_ms: None,
            ..window.view.clone()
        });
        armed.touch();
        let view = window.view;
        if window.pcm.is_empty() {
            armed.fail(&view.session, &view.tag, WindowFailureReason::Empty, None);
            return Closed::Failed(format!(
                "capture window {} ({}) recorded nothing",
                view.session,
                view.tag.as_str()
            ));
        }
        let frames = window.pcm.len() as u64;
        let recording = CaptureRecording {
            device: device.clone(),
            session: view.session.clone(),
            tag: view.tag.clone(),
            ms: frames * 1_000 / RATE,
            peak_db: peak_db(&window.pcm),
            floor_db: floor_db(&window.pcm),
            frames,
            dropped: 0,
            truncated,
            opened_ms: view.opened_ms.unwrap_or(0),
            wav: wav_bytes(&window.pcm).into(),
        };
        match &armed.delivery {
            Delivery::Webhook { queue, .. } => {
                let recording = CaptureRecording {
                    dropped: armed.dropped_unreported,
                    ..recording
                };
                match queue.try_send(recording) {
                    Ok(()) => {
                        armed.dropped_unreported = 0;
                        armed.posting += 1;
                        Closed::Delivered
                    }
                    Err(_) => {
                        armed.dropped += 1;
                        armed.dropped_unreported += 1;
                        Closed::Dropped(format!(
                            "capture window {} ({}) dropped: the webhook is {CAPTURE_QUEUE_MAX} windows behind",
                            view.session,
                            view.tag.as_str()
                        ))
                    }
                }
            }
            Delivery::Pull => {
                armed.held.push_back(recording);
                if armed.held.len() > CAPTURE_QUEUE_MAX
                    && let Some(old) = armed.held.pop_front()
                {
                    armed.dropped += 1;
                    armed.dropped_unreported += 1;
                    return Closed::Dropped(format!(
                        "capture window {} ({}) dropped: {CAPTURE_QUEUE_MAX} newer recordings are waiting to be collected",
                        old.session,
                        old.tag.as_str()
                    ));
                }
                Closed::Delivered
            }
        }
    }

    /// Clears the mode, for `reason`. Runs to its end without waiting on anything, so the
    /// watchdog can call it about its own arm.
    fn clear(&self, device: &DeviceId, generation: Option<u64>, reason: &str) -> bool {
        let removed = {
            let mut inner = self.lock();
            let current = inner
                .armed
                .get(device)
                .is_some_and(|a| generation.is_none_or(|g| a.generation == g));
            current.then(|| inner.armed.remove(device)).flatten()
        };
        let Some(removed) = removed else {
            return false;
        };
        // The webhook's sender posts what is already queued, then stops; the watchdog
        // and the long polls see the revision channel close.
        drop(removed);
        self.log(
            device,
            LogLevel::Info,
            format!("capture mode cleared: {reason}"),
        );
        self.publish(device, false);
        self.recording_changed(device);
        true
    }

    // ── Tasks ──

    /// What the watchdog waits for: `None` once the arm is gone; `Some(None)` while only
    /// a change can make anything due.
    fn due(&self, device: &DeviceId, generation: u64) -> Option<Option<Instant>> {
        let inner = self.lock();
        let armed = inner
            .armed
            .get(device)
            .filter(|a| a.generation == generation)?;
        Some(match &armed.window {
            Some(window) => Some(window.deadline),
            None if armed.polls > 0 => None,
            None => Some(armed.last_call + armed.idle.duration()),
        })
    }

    /// The watchdog's deadline passed: the wall clock closes an open window, and the
    /// dead-man clears an idle mode.
    fn expire(&self, device: &DeviceId, generation: u64) {
        let now = Instant::now();
        let said = {
            let mut inner = self.lock();
            let Some(armed) = inner
                .armed
                .get_mut(device)
                .filter(|a| a.generation == generation)
            else {
                return;
            };
            match &armed.window {
                Some(window) if now >= window.deadline => Ok(self.close(device, armed, true)),
                None if armed.polls == 0 && now >= armed.last_call + armed.idle.duration() => {
                    Err(armed.idle)
                }
                _ => return,
            }
        };
        match said {
            Ok(said) => self.said(device, said),
            Err(idle) => {
                self.clear(
                    device,
                    Some(generation),
                    &format!(
                        "no capture window and no capture request in {} s",
                        idle.seconds()
                    ),
                );
            }
        }
    }

    async fn watchdog(self: Arc<Self>, device: DeviceId, generation: u64) {
        let Some(mut revision) = self
            .lock()
            .armed
            .get(&device)
            .filter(|a| a.generation == generation)
            .map(|a| a.revision.subscribe())
        else {
            return;
        };
        loop {
            revision.borrow_and_update();
            let Some(due) = self.due(&device, generation) else {
                return;
            };
            let wait = async {
                match due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = wait => self.expire(&device, generation),
                changed = revision.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    async fn post_windows(
        self: Arc<Self>,
        device: DeviceId,
        generation: u64,
        url: CaptureWebhook,
        mut queue: mpsc::Receiver<CaptureRecording>,
    ) {
        while let Some(recording) = queue.recv().await {
            let mut request = self
                .http
                .post(url.url().clone())
                .header(reqwest::header::CONTENT_TYPE, "audio/wav");
            for (name, value) in recording.headers() {
                request = request.header(name, value);
            }
            let failure = match request.body(recording.wav.clone()).send().await {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => Some(format!("the webhook answered {}", response.status())),
                Err(error) => Some(format!("the webhook could not be reached: {error}")),
            };
            let mut inner = self.lock();
            if let Some(armed) = inner
                .armed
                .get_mut(&device)
                .filter(|a| a.generation == generation)
            {
                armed.posting = armed.posting.saturating_sub(1);
                if let Some(failure) = &failure {
                    armed.fail(
                        &recording.session,
                        &recording.tag,
                        WindowFailureReason::Delivery,
                        Some(failure.clone()),
                    );
                }
                armed.changed();
            }
            drop(inner);
            if let Some(failure) = failure {
                self.log(
                    &device,
                    LogLevel::Warn,
                    format!(
                        "capture window {} ({}) was not delivered: {failure}",
                        recording.session,
                        recording.tag.as_str()
                    ),
                );
            }
        }
    }

    /// The next held recording matching `session`, consumed.
    fn take(armed: &mut Armed, session: Option<&WindowId>) -> Option<CaptureRecording> {
        let at = armed
            .held
            .iter()
            .position(|r| session.is_none_or(|s| &r.session == s))?;
        let mut recording = armed.held.remove(at)?;
        recording.dropped = std::mem::take(&mut armed.dropped_unreported);
        armed.changed();
        Some(recording)
    }

    async fn poll(
        &self,
        device: DeviceId,
        request: RecordingRequest,
    ) -> Result<CaptureRecording, CaptureError> {
        let (generation, mut revision) = {
            let mut inner = self.lock();
            let armed = self.armed(&mut inner, &device)?;
            if !matches!(armed.delivery, Delivery::Pull) {
                return Err(CaptureError::PushMode);
            }
            armed.polls += 1;
            armed.touch();
            (armed.generation, armed.revision.subscribe())
        };
        let _poll = PollGuard {
            hub: self,
            device: &device,
            generation,
        };
        let deadline = Instant::now() + request.wait;
        loop {
            {
                let mut inner = self.lock();
                let Some(armed) = inner
                    .armed
                    .get_mut(&device)
                    .filter(|a| a.generation == generation)
                else {
                    return Err(CaptureError::NotArmed);
                };
                revision.borrow_and_update();
                if let Some(recording) = Self::take(armed, request.session.as_ref()) {
                    return Ok(recording);
                }
                if let Some(session) = &request.session {
                    if let Some(failure) = armed
                        .last_failure
                        .as_ref()
                        .filter(|f| &f.session == session)
                    {
                        return Err(CaptureError::NothingArrived(
                            format!(
                                "That window ended without a recording ({})",
                                serde_json::to_value(failure.reason)
                                    .ok()
                                    .and_then(|v| v.as_str().map(str::to_owned))
                                    .unwrap_or_default()
                            )
                            .into(),
                        ));
                    }
                    if armed
                        .window
                        .as_ref()
                        .is_none_or(|w| &w.view.session != session)
                    {
                        return Err(CaptureError::NothingArrived(
                            "No recording of that window is held".into(),
                        ));
                    }
                }
            }
            match tokio::time::timeout_at(deadline, revision.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => return Err(CaptureError::NotArmed),
                Err(_) => {
                    return Err(CaptureError::NothingArrived(
                        "No recording arrived in time".into(),
                    ));
                }
            }
        }
    }
}

/// Counts a waiting long poll until it ends, answered or abandoned.
struct PollGuard<'a> {
    hub: &'a CaptureHub,
    device: &'a DeviceId,
    generation: u64,
}

impl Drop for PollGuard<'_> {
    fn drop(&mut self) {
        let mut inner = self.hub.lock();
        if let Some(armed) = inner
            .armed
            .get_mut(self.device)
            .filter(|a| a.generation == self.generation)
        {
            armed.polls = armed.polls.saturating_sub(1);
            armed.touch();
        }
    }
}

impl CaptureControl for CaptureHub {
    fn arm(&self, device: &DeviceId, arm: CaptureArm) -> Result<CaptureState, CaptureError> {
        let (state, webhook) = {
            let mut inner = self.lock();
            let session = self.session(&inner, device)?;
            if let Some(armed) = inner.armed.get_mut(device).filter(|a| a.session == session) {
                if !armed.same_delivery(&arm.transport) {
                    return Err(CaptureError::OtherTransport);
                }
                armed.idle = arm.idle;
                armed.touch();
                return Ok(armed.state());
            }
            inner.generation += 1;
            let generation = inner.generation;
            let (revision, _) = watch::channel(0);
            let (delivery, webhook) =
                match arm.transport {
                    CaptureTransport::Webhook(url) => {
                        let (queue, receiver) = mpsc::channel(CAPTURE_QUEUE_MAX);
                        drop(self.tasks.spawn(
                            "capture webhook",
                            self.me().post_windows(
                                device.clone(),
                                generation,
                                url.clone(),
                                receiver,
                            ),
                        ));
                        let text = url.as_str().to_owned();
                        (Delivery::Webhook { url, queue }, Some(text))
                    }
                    CaptureTransport::Pull => (Delivery::Pull, None),
                };
            let armed = Armed {
                session,
                generation,
                delivery,
                idle: arm.idle,
                last_call: Instant::now(),
                polls: 0,
                window: None,
                last_window: None,
                held: VecDeque::new(),
                posting: 0,
                dropped: 0,
                dropped_unreported: 0,
                last_failure: None,
                revision,
            };
            let state = armed.state();
            inner.armed.insert(device.clone(), armed);
            drop(self.tasks.spawn(
                "capture watchdog",
                self.me().watchdog(device.clone(), generation),
            ));
            (state, webhook)
        };
        self.log(
            device,
            LogLevel::Info,
            format!(
                "capture mode on: {}, cleared after {} s idle",
                match webhook {
                    Some(url) => format!("posting to {url}"),
                    None => "held for collection".to_owned(),
                },
                arm.idle.seconds()
            ),
        );
        self.publish(device, true);
        self.recording_changed(device);
        Ok(state)
    }

    fn disarm(&self, device: &DeviceId) -> Result<CaptureState, CaptureError> {
        {
            let inner = self.lock();
            self.session(&inner, device)?;
        }
        self.clear(device, None, "turned off by request");
        Ok(CaptureState::off())
    }

    fn state(&self, device: &DeviceId) -> Result<CaptureState, CaptureError> {
        let mut inner = self.lock();
        match self.armed(&mut inner, device) {
            Ok(armed) => {
                armed.touch();
                Ok(armed.state())
            }
            Err(CaptureError::NotArmed) => Ok(CaptureState::off()),
            Err(error) => Err(error),
        }
    }

    fn open_window(
        &self,
        device: &DeviceId,
        tag: CaptureTag,
        max: WindowMs,
    ) -> Result<WindowView, CaptureError> {
        let mut inner = self.lock();
        inner.windows += 1;
        let number = inner.windows;
        let armed = self.armed(&mut inner, device)?;
        if armed.window.is_some() {
            return Err(CaptureError::WindowOpen);
        }
        let view = WindowView {
            session: WindowId(format!("cw-{}-{number}", self.started_ms)),
            tag,
            max_ms: max,
            opened_ms: Some(unix_ms()),
        };
        armed.window = Some(OpenWindow {
            view: view.clone(),
            max_samples: (u64::from(max.ms()) * RATE / 1_000) as usize,
            deadline: Instant::now() + max.duration() + WINDOW_GRACE,
            pcm: Vec::new(),
        });
        let answer = WindowView {
            opened_ms: None,
            ..view
        };
        armed.last_window = Some(answer.clone());
        armed.touch();
        Ok(answer)
    }

    fn stop_window(
        &self,
        device: &DeviceId,
        session: &WindowId,
    ) -> Result<WindowView, CaptureError> {
        let (view, said) = {
            let mut inner = self.lock();
            let armed = self.armed(&mut inner, device)?;
            armed.touch();
            let latest = armed
                .last_window
                .clone()
                .filter(|w| &w.session == session)
                .ok_or(CaptureError::StaleWindow)?;
            let open = armed
                .window
                .as_ref()
                .is_some_and(|w| &w.view.session == session);
            (latest, open.then(|| self.close(device, armed, false)))
        };
        if let Some(said) = said {
            self.said(device, said);
        }
        Ok(view)
    }

    fn recording(
        &self,
        device: DeviceId,
        request: RecordingRequest,
    ) -> BoxFuture<'_, Result<CaptureRecording, CaptureError>> {
        Box::pin(self.poll(device, request))
    }

    fn fields(&self, device: &DeviceId) -> Map<String, Value> {
        let inner = self.lock();
        let armed = inner
            .armed
            .get(device)
            .is_some_and(|a| inner.sessions.get(device) == Some(&a.session));
        mode_field(armed)
    }
}

// ─── The tee ────────────────────────────────────────────────────────────────────────

/// Capture's tap on one armed mode's audio.
pub struct CaptureTap {
    hub: Arc<CaptureHub>,
    device: DeviceId,
    generation: u64,
}

/// One link session's diagnostic subscription, handed to every armed recording mode:
/// sample collection or ambient recording's recorder, and capture. The voice path holds
/// its sender for as long as any mode is armed, so a mode arming or disarming beside
/// another changes only the tee's targets, not the subscription.
///
/// A recorder that falls behind (its queue is full) is dropped in the middle of a
/// subscription, and discards what it holds, as it would were it the voice path's sink;
/// one retired while the subscription runs gets the subscription's end first, so it
/// saves. When the voice path drops the tee in the middle of a subscription (the tee
/// fell behind), every target is told the same way.
pub struct DiagnosticTee {
    sender: mpsc::WeakSender<DiagnosticAudio>,
    targets: Arc<Mutex<TeeTargets>>,
}

#[derive(Default)]
struct TeeTargets {
    recorder: Option<(Modes, mpsc::Sender<DiagnosticAudio>)>,
    capture: Option<CaptureTap>,
    /// A subscription's chunks went out and its end has not.
    mid_subscription: bool,
}

impl TeeTargets {
    fn retire_recorder(&mut self) {
        if let Some((_, recorder)) = self.recorder.take()
            && self.mid_subscription
        {
            let _ = recorder.try_send(DiagnosticAudio::Ended(EndCause::Closed(
                CloseReason::Closed,
            )));
        }
    }
}

impl DiagnosticTee {
    fn start(tasks: &TaskSupervisor) -> (Self, mpsc::Sender<DiagnosticAudio>) {
        let (sender, receiver) = mpsc::channel(RECORDER_QUEUE);
        let targets = Arc::new(Mutex::new(TeeTargets::default()));
        drop(tasks.spawn(
            "diagnostic audio",
            Self::run(Arc::clone(&targets), receiver),
        ));
        (
            Self {
                sender: sender.downgrade(),
                targets,
            },
            sender,
        )
    }

    async fn run(targets: Arc<Mutex<TeeTargets>>, mut audio: mpsc::Receiver<DiagnosticAudio>) {
        while let Some(audio) = audio.recv().await {
            let mut targets = targets.lock().expect("tee lock");
            targets.mid_subscription = matches!(audio, DiagnosticAudio::Chunk(_));
            if let Some(capture) = &targets.capture {
                capture
                    .hub
                    .audio(&capture.device, capture.generation, &audio);
            }
            if let Some((_, recorder)) = &targets.recorder
                && recorder.try_send(audio).is_err()
            {
                tracing::warn!("the recording fell behind; dropping its recorder");
                targets.recorder = None;
            }
        }
        let mut targets = targets.lock().expect("tee lock");
        targets.recorder = None;
        if targets.mid_subscription
            && let Some(capture) = targets.capture.take()
        {
            capture.hub.lost(&capture.device, capture.generation);
        }
    }

    fn targets(&self) -> std::sync::MutexGuard<'_, TeeTargets> {
        self.targets.lock().expect("tee lock")
    }

    /// Whether the voice path still delivers to it.
    pub fn is_live(&self) -> bool {
        self.sender.strong_count() > 0
    }

    /// The modes its recorder was made for, while that recorder takes audio.
    pub fn recorder_modes(&self) -> Option<Modes> {
        self.targets()
            .recorder
            .as_ref()
            .filter(|(_, recorder)| !recorder.is_closed())
            .map(|(modes, _)| *modes)
    }

    /// Replaces its recorder; the one it had saves what it recorded.
    pub fn set_recorder(&self, recorder: Option<(Modes, mpsc::Sender<DiagnosticAudio>)>) {
        let mut targets = self.targets();
        targets.retire_recorder();
        targets.recorder = recorder;
    }

    /// Points it at capture's tap (`None`: capture is off); the same arm's tap is kept.
    pub fn set_capture(&self, capture: Option<CaptureTap>) {
        let mut targets = self.targets();
        let same = match (&targets.capture, &capture) {
            (Some(old), Some(new)) => old.device == new.device && old.generation == new.generation,
            (None, None) => true,
            _ => false,
        };
        if !same {
            targets.capture = capture;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_dbfs_and_never_below_minus_90() {
        assert_eq!(peak_db(&[]), FLOOR_MIN_DB);
        assert_eq!(peak_db(&[0; 100]), FLOOR_MIN_DB);
        assert!((peak_db(&[i16::MIN]) - 0.0).abs() < 1e-9);
        assert!((peak_db(&[0, 16_384, -3]) - -6.0206).abs() < 1e-3);
        // The floor is the quietest 80 ms block: a loud block and a quiet one.
        let mut pcm = vec![8_000_i16; BLOCK];
        pcm.extend(std::iter::repeat_n(80_i16, BLOCK));
        assert!((floor_db(&pcm) - dbfs(80.0)).abs() < 1e-9);
        // Shorter than a block: the whole window.
        assert!((floor_db(&[100, -100]) - dbfs(100.0)).abs() < 1e-9);
        assert_eq!(floor_db(&[0; 3 * BLOCK]), FLOOR_MIN_DB);
    }
}
