//! One device runtime: a dedicated OS thread running a current-thread Tokio runtime
//! that owns everything live about one approved device (spec §5.1, §5.2, §5.7).
//!
//! The actor on that thread is the only code that changes the device's state. It
//! serializes session admission, installs each session's privacy state before capture
//! is allowed, and publishes an immutable [`DeviceSnapshot`] whenever something changes.
//! Other threads talk to it through bounded command queues (ordinary and urgent),
//! cancellation tokens and that snapshot.
//!
//! A device link session handed over with [`DeviceHandle::attach_link`] is read here,
//! urgent events first: privacy changes and the session's end, then the ordered ingress
//! (`stream.open`, `stream.end` and audio packets, checked against the session's own
//! stream registry), then telemetry. Ingress this pass does not act on yet (wake
//! candidates, subscriptions, renders, alerts, buttons) is counted per message type in
//! the snapshot, never dropped silently.

use std::{
    collections::BTreeMap,
    net::IpAddr,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use echomuse_esphome::{entities::VoiceEntities, voice::SatelliteRequest};
use echomuse_link::{
    Ingress, IngressItem, IngressReceiver, LinkSession, LinkStatus, SessionLink, Telemetry,
    TelemetryKind, TelemetryReceiver, Transport, UrgentEvent, UrgentReceiver,
};
use echomuse_protocol::{
    Capabilities, Capability, DeviceMessage, LinkSessionId, MessageType, Platform, StreamId,
    session::{HelloAlerts, SessionHello, VolumeLevel},
};
use serde_json::{Map, Value};
use tokio::{
    runtime::Builder,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};
use tokio_util::sync::{CancellationToken, DropGuard};

use echomuse_core::types::{
    DeviceId, QueueLimits, RuntimeError, RuntimeId, SessionId, WorkIdentity, WorkScope,
};

use crate::device_events::{DeviceEvent, DeviceEvents, ReadySession, is_alert_message};
use crate::esphome_node::{
    EsphomeNodes, EsphomeServices, EsphomeState, Prepared, Wanted, decode_adverts,
};
use crate::speech_models::SpeechStatus;
use crate::speech_workers::{
    SpeechWorkerHandle, SpeechWorkerKind, SpeechWorkers, Work, WorkerLimits, WorkerWatch,
};
use crate::supervision::{
    Availability, DeviceHealth, RuntimeTasks, SpeechPathState, Supervision, SupervisionOptions,
    WorkerOwner,
};
use crate::voice_session::{
    VoiceMessage, VoiceServices, VoiceSession, VoiceStart, VoiceWatch, WatchChange,
};
use echomuse_esphome::entities::DeviceVolume;
pub use echomuse_playback::AlertPreviewSound;
use echomuse_voice::{Input, PreviewOutcome};

/// Device messages the voice path handles (besides the stream messages).
fn voice_message(message_type: MessageType) -> bool {
    matches!(
        message_type,
        MessageType::WakeCandidate
            | MessageType::WakeCandidateEnd
            | MessageType::WakeNearMiss
            | MessageType::RenderProgress
            | MessageType::RenderFinished
            | MessageType::ButtonAction
    )
}

// Identity allocation only; this is not device state.
static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(1);
/// Session ids are unique in the process, not per runtime, so a session of a runtime
/// that failed can never be mistaken for one of its replacement (§5.7 step 2).
static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// How long counters may lag behind in the published snapshot.
const COUNTER_PUBLISH_INTERVAL: Duration = Duration::from_secs(1);
/// How long a stopping runtime waits for its ESPHome nodes to close.
const NODE_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Starting,
    Ready,
    Attached,
    Draining,
    Stopped,
    Failed,
}

/// The device's volume as the device reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Volume {
    /// The level, 0–127.
    Level(u8),
    /// The device has no volume control this run.
    Unavailable,
}

/// What the runtime knows about the attached device link session.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkState {
    /// The `session_id` `session.ready` carried.
    pub session_id: LinkSessionId,
    pub transport: Transport,
    /// The control socket's peer address.
    pub ip: IpAddr,
    pub platform: Platform,
    pub capabilities: Capabilities,
    pub firmware_version: Option<String>,
    pub os_version: Option<String>,
    /// `None` until the device reports it.
    pub volume: Option<Volume>,
    /// The latest `ambient_light` reading in lux; `None` before the first.
    pub ambient_lux: Option<i64>,
    /// The latest `stats` body, as reported; `None` before the first.
    pub stats: Option<Arc<Map<String, Value>>>,
    /// Audio packets handed to the voice path, per stream (it checks them against the
    /// session's stream registry).
    pub audio_accepted: BTreeMap<StreamId, u64>,
    /// Whether `session.ready` granted capture (wake candidates are handled).
    pub capture_permitted: bool,
    /// Why capture was not granted; `None` when it was.
    pub capture_refusal: Option<String>,
    /// The voice path's state.
    pub voice: Option<echomuse_voice::VoiceStatus>,
    /// Messages received that this controller does not act on yet, by type.
    pub unhandled: BTreeMap<&'static str, u64>,
    /// `log` and `ble_adverts` messages the link dropped because its queue was full.
    pub telemetry_dropped: u64,
    /// Reports whose body did not decode, by type.
    pub malformed: BTreeMap<&'static str, u64>,
    /// The SHA-256 of every asset the hello listed as installed; `None` when it sent no
    /// list.
    pub installed_assets: Option<Vec<echomuse_protocol::Sha256>>,
}

#[derive(Clone, Debug)]
pub struct DeviceSnapshot {
    pub device: DeviceId,
    pub runtime: RuntimeId,
    pub lifecycle: Lifecycle,
    pub session: Option<SessionId>,
    /// The privacy mute as last reported; `None` while detached or not reported.
    pub muted: Option<bool>,
    /// Worker threads are available. This does not claim that speech models are loaded.
    pub workers_ready: bool,
    pub failure: Option<String>,
    /// The current capture scope while a session is attached. A mute cancels it at
    /// once; an unmute replaces it with a fresh one. Holders may watch or cancel it;
    /// only the runtime issues a new one.
    pub capture: Option<WorkScope>,
    /// The attached device link session; `None` while no link session is attached.
    pub link: Option<Arc<LinkState>>,
    /// The attached session's send side, for the shared services that address the
    /// device (alert delivery, firmware delivery); `None` while none is attached.
    pub session_link: Option<SessionLink>,
    /// The device's ESPHome nodes.
    pub esphome: EsphomeState,
    /// The device's availability and error window. It belongs to the device, not to one
    /// runtime: it carries over when the runtime is rebuilt (§5.5).
    pub health: DeviceHealth,
}

impl DeviceSnapshot {
    /// Why admission must refuse the device now; `None` when its runtime can take a
    /// session. A runtime that failed or is stopping cannot.
    pub fn admission_refusal(&self) -> Option<String> {
        if let Availability::Restarting { reason, retry_in } = &self.health.availability {
            return Some(format!(
                "the device runtime is restarting ({reason}); it is rebuilt after {} s",
                retry_in.as_secs_f64()
            ));
        }
        match self.lifecycle {
            Lifecycle::Ready | Lifecycle::Attached => None,
            Lifecycle::Starting => Some("the device runtime is starting".to_owned()),
            Lifecycle::Draining | Lifecycle::Stopped | Lifecycle::Failed => {
                Some("the device runtime is not running".to_owned())
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DeviceRuntimeConfig {
    pub ingress: QueueLimits,
    pub workers: WorkerLimits,
    pub shutdown_timeout: Duration,
}

impl Default for DeviceRuntimeConfig {
    fn default() -> Self {
        Self {
            ingress: QueueLimits::default(),
            workers: WorkerLimits::default(),
            shutdown_timeout: Duration::from_secs(30),
        }
    }
}

/// A device link session for [`DeviceHandle::attach_link`], with what admission learned.
pub struct LinkAttach {
    /// The session; the runtime owns it from now on.
    pub session: LinkSession,
    pub hello: SessionHello,
    pub platform: Platform,
    pub peer: IpAddr,
    /// The device's label, or `None` when it has none.
    pub label: Option<String>,
    /// Whether `session.ready` asked for the `afe` stream.
    pub afe_metadata: bool,
    /// The device announced `ble_proxy` and its `bleProxyEnabled` setting is on.
    pub bluetooth_proxy: bool,
    /// Whether `session.ready` granted capture, and why not when it did not.
    pub capture_permitted: bool,
    pub capture_refusal: Option<String>,
    /// The Kiosk Satellite token stored in the device row; `None` while none is stored.
    pub kiosk_token: Option<String>,
}

#[derive(Clone)]
struct CapturePermission {
    muted: bool,
    scope: WorkScope,
}

struct SessionState {
    id: SessionId,
    stop: CancellationToken,
    capture: watch::Receiver<CapturePermission>,
}

/// A link session's receivers and voice path, owned by the actor.
struct LinkBinding {
    link: SessionLink,
    urgent: UrgentReceiver,
    /// `None` once it reported the end; the urgent end follows.
    ingress: Option<IngressReceiver>,
    telemetry: Option<TelemetryReceiver>,
    /// The voice machine and its stream registry.
    voice: VoiceSession,
    /// The last `physical_seq` of a privacy change; older ones are dropped.
    physical_seq: Option<u64>,
    state: LinkState,
    /// The link's status until the session is known to be ready; `None` after that,
    /// or once the link has ended.
    status: Option<watch::Receiver<LinkStatus>>,
    /// The session was reported ready ([`DeviceEvent::SessionReady`]).
    announced: bool,
    /// The hello's alert cache position, for the alert service.
    hello_alerts: Option<HelloAlerts>,
    /// The device services' taps for this session (logs, Wi-Fi, recorder, recording
    /// modes); `None` for a runtime without them.
    taps: Option<crate::taps::SessionTaps>,
}

/// The shared services a runtime serves its device with. A runtime without them runs
/// sessions, capture and speech workers only.
#[derive(Clone, Default)]
pub struct RuntimeServices {
    /// Port assignment, binding and advertising of the device's ESPHome nodes.
    pub esphome: Option<Arc<EsphomeServices>>,
    /// Where session readiness, alert messages and Home Assistant requests go.
    pub events: Option<DeviceEvents>,
    /// What the voice path uses. A runtime without it cannot attach link sessions.
    pub voice: Option<Arc<VoiceServices>>,
    /// The native-call watchdog, the task supervisor and the restart policy. A runtime
    /// started without them starts the production ones.
    pub supervision: Option<Supervision>,
    /// The device services this runtime reports its session's traffic to.
    pub taps: Option<crate::taps::DeviceTaps>,
}

// Only run_actor owns and mutates this state. Handles receive snapshots and tokens.
struct ActiveSession {
    state: Arc<SessionState>,
    capture: watch::Sender<CapturePermission>,
    link: Option<LinkBinding>,
}

struct SessionGrant {
    state: Arc<SessionState>,
    // Cancels the session if its attach reply or last transport handle is dropped.
    lease: Arc<DropGuard>,
}

enum Command {
    Attach {
        muted: bool,
        reply: oneshot::Sender<Result<SessionGrant, RuntimeError>>,
    },
    AttachLink {
        attach: Box<LinkAttach>,
        reply: oneshot::Sender<Result<SessionId, RuntimeError>>,
    },
    Privacy {
        session: SessionId,
        muted: bool,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Detach {
        session: SessionId,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Renegotiate {
        reply: oneshot::Sender<Result<bool, RuntimeError>>,
    },
    /// A request for the attached session's voice path.
    Voice(VoiceCommand),
    /// The stored recording modes changed: arm or disarm the voice path's recording.
    RecordingChanged,
}

/// What the dashboard asks of a device's voice path.
enum VoiceCommand {
    Preview {
        sound: AlertPreviewSound,
        reply: oneshot::Sender<PreviewOutcome>,
    },
    StopPreview {
        reply: oneshot::Sender<PreviewOutcome>,
    },
    /// The device row's Kiosk Satellite token changed.
    KioskToken(Option<String>),
}

/// Why a sound preview did not start or stop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewError {
    /// No device session is attached.
    NotConnected,
    /// The device has no local playback (`render_progress`).
    Unsupported,
    /// The device refused the `render.start`, with its code when it gave one.
    Rejected(Option<String>),
    /// No acknowledgement came back, or the session ended while waiting: the sound may
    /// or may not be playing.
    Unknown,
    /// The runtime is stopped or its command queue is full.
    Runtime(RuntimeError),
}

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("device offline"),
            Self::Unsupported => f.write_str("this device cannot play sound previews"),
            Self::Rejected(Some(code)) => write!(f, "the device refused the preview: {code}"),
            Self::Rejected(None) => f.write_str("the device refused the preview"),
            Self::Unknown => f.write_str("the device did not confirm the preview"),
            Self::Runtime(error) => write!(f, "the device runtime is unavailable: {error}"),
        }
    }
}

impl std::error::Error for PreviewError {}

fn preview_result(outcome: PreviewOutcome) -> Result<bool, PreviewError> {
    match outcome {
        PreviewOutcome::Started => Ok(true),
        PreviewOutcome::Stopped { was_playing } => Ok(was_playing),
        PreviewOutcome::Unsupported => Err(PreviewError::Unsupported),
        PreviewOutcome::Rejected(code) => Err(PreviewError::Rejected(code)),
        PreviewOutcome::Unknown => Err(PreviewError::Unknown),
        PreviewOutcome::SessionEnded => Err(PreviewError::NotConnected),
    }
}

struct Envelope {
    command: Command,
    queued: Instant,
    _bytes: OwnedSemaphorePermit,
}

#[derive(Clone)]
struct CommandQueue {
    sender: mpsc::Sender<Envelope>,
    credits: Arc<Semaphore>,
}

impl CommandQueue {
    fn new(limits: QueueLimits) -> (Self, mpsc::Receiver<Envelope>) {
        let (sender, receiver) = mpsc::channel(limits.messages);
        (
            Self {
                sender,
                credits: Arc::new(Semaphore::new(limits.bytes as usize)),
            },
            receiver,
        )
    }

    fn send(&self, command: Command) -> Result<(), RuntimeError> {
        let bytes = Arc::clone(&self.credits)
            .try_acquire_many_owned(std::mem::size_of::<Envelope>() as u32)
            .map_err(|_| RuntimeError::Overloaded)?;
        let envelope = Envelope {
            command,
            queued: Instant::now(),
            _bytes: bytes,
        };
        self.sender.try_send(envelope).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => RuntimeError::Overloaded,
            mpsc::error::TrySendError::Closed(_) => RuntimeError::Closed,
        })
    }
}

#[derive(Clone)]
pub struct DeviceHandle {
    device: DeviceId,
    runtime: RuntimeId,
    ordinary: CommandQueue,
    urgent: CommandQueue,
    stop: CancellationToken,
    workers: [SpeechWorkerHandle; 4],
    snapshot: watch::Receiver<DeviceSnapshot>,
}

impl DeviceHandle {
    pub fn id(&self) -> &DeviceId {
        &self.device
    }

    /// The identity of the runtime this handle commands. A rebuilt runtime has a new
    /// one; this handle then only reaches the stopped runtime.
    pub fn runtime(&self) -> RuntimeId {
        self.runtime
    }

    /// The device's latest state. Its snapshot follows the device across runtime
    /// rebuilds, so it may describe a newer runtime than [`DeviceHandle::runtime`].
    pub fn snapshot(&self) -> DeviceSnapshot {
        self.snapshot.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<DeviceSnapshot> {
        self.snapshot.clone()
    }

    /// Revokes this runtime's identity at once: its sessions and work are cancelled
    /// and it stops. Its supervisor decides whether a new one is built.
    pub(crate) fn revoke(&self) {
        self.stop.cancel();
    }

    /// The actor serializes admission, revokes the old session, then installs the new one.
    pub async fn attach(&self, muted: bool) -> Result<SessionHandle, RuntimeError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Attach { muted, reply }, false)?;
        let grant = self.wait(received).await?;
        if grant.state.stop.is_cancelled() {
            return Err(RuntimeError::StaleSession);
        }
        Ok(SessionHandle {
            device: self.clone(),
            state: grant.state,
            _lease: grant.lease,
        })
    }

    /// Attaches an admitted device link session. The runtime ends the previous session,
    /// installs the hello's privacy state, publishes the capture scope to the link and
    /// then owns the session: it ends when the link reports its end, when the runtime
    /// replaces it, or when the runtime stops. A hello that does not say whether the
    /// privacy mute is off attaches muted, so capture needs a positive report.
    pub async fn attach_link(&self, attach: LinkAttach) -> Result<SessionId, RuntimeError> {
        let (reply, received) = oneshot::channel();
        self.send(
            Command::AttachLink {
                attach: Box::new(attach),
                reply,
            },
            false,
        )?;
        self.wait(received).await
    }

    /// Ends the attached device link session so the device dials again and receives a
    /// fresh `session.ready` (spec §3.1.4: a change to the wake graph, rules or
    /// thresholds takes effect only through a new one). Returns whether a link session
    /// was attached.
    pub async fn renegotiate(&self) -> Result<bool, RuntimeError> {
        let (reply, received) = oneshot::channel();
        self.send(Command::Renegotiate { reply }, false)?;
        self.wait(received).await
    }

    fn send(&self, command: Command, urgent: bool) -> Result<(), RuntimeError> {
        if self.stop.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let queue = if urgent { &self.urgent } else { &self.ordinary };
        queue.send(command).inspect_err(|error| {
            // Required lifecycle events are never silently discarded.
            eprintln!("device {} command queue failed: {error}", self.device);
            self.stop.cancel();
        })
    }

    async fn wait<T>(
        &self,
        received: oneshot::Receiver<Result<T, RuntimeError>>,
    ) -> Result<T, RuntimeError> {
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(RuntimeError::Closed),
            result = received => result.map_err(|_| RuntimeError::Closed)?,
        }
    }

    /// Plays an alert sound's preview on the attached device (`render.start`
    /// `alert_preview`, a local source naming the sound's SHA-256 or the built-in
    /// fallback). Resolves once the device accepted or refused it.
    pub async fn preview_sound(&self, sound: AlertPreviewSound) -> Result<(), PreviewError> {
        let (reply, answer) = oneshot::channel();
        self.voice_command(VoiceCommand::Preview { sound, reply })?;
        let outcome = self.voice_answer(answer).await?;
        preview_result(outcome).map(|_| ())
    }

    /// Stops the attached device's sound preview; `Ok(true)` when one was playing.
    pub async fn stop_sound_preview(&self) -> Result<bool, PreviewError> {
        let (reply, answer) = oneshot::channel();
        self.voice_command(VoiceCommand::StopPreview { reply })?;
        preview_result(self.voice_answer(answer).await?)
    }

    /// The device row's Kiosk Satellite token changed (`None`: cleared). The runtime
    /// keeps it for later sessions and sends the attached `kiosk_plugin` device a new
    /// `config`.
    pub fn set_kiosk_token(&self, token: Option<String>) -> Result<(), RuntimeError> {
        if self.stop.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        self.ordinary
            .send(Command::Voice(VoiceCommand::KioskToken(token)))
    }

    /// The device's stored recording modes changed; the runtime arms or disarms the
    /// attached session's recording to match.
    pub fn recording_changed(&self) -> Result<(), RuntimeError> {
        if self.stop.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        self.ordinary.send(Command::RecordingChanged)
    }

    /// Queues a dashboard request. A full queue refuses it without stopping the runtime:
    /// unlike the lifecycle commands, a dashboard request may be dropped.
    fn voice_command(&self, command: VoiceCommand) -> Result<(), PreviewError> {
        if self.stop.is_cancelled() {
            return Err(PreviewError::Runtime(RuntimeError::Closed));
        }
        self.ordinary
            .send(Command::Voice(command))
            .map_err(PreviewError::Runtime)
    }

    async fn voice_answer(
        &self,
        answer: oneshot::Receiver<PreviewOutcome>,
    ) -> Result<PreviewOutcome, PreviewError> {
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(PreviewError::Runtime(RuntimeError::Closed)),
            // The session ended with the request still unanswered.
            outcome = answer => Ok(outcome.unwrap_or(PreviewOutcome::Unknown)),
        }
    }
}

/// A transport holds this session-specific handle. An old handle cannot affect a replacement.
/// Dropping the final clone ends the session, even if no explicit detach is sent.
#[derive(Clone)]
pub struct SessionHandle {
    device: DeviceHandle,
    state: Arc<SessionState>,
    _lease: Arc<DropGuard>,
}

impl SessionHandle {
    pub fn id(&self) -> SessionId {
        self.state.id
    }

    /// Unmute publishes a new capture scope; it never revives an old scope.
    pub fn scope(&self) -> Result<WorkScope, RuntimeError> {
        if self.state.stop.is_cancelled() {
            return Err(RuntimeError::StaleSession);
        }
        let permission = self.state.capture.borrow();
        if permission.muted {
            return Err(RuntimeError::Muted);
        }
        if permission.scope.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        Ok(permission.scope.clone())
    }

    pub fn submit<T, F>(
        &self,
        kind: SpeechWorkerKind,
        retained_bytes: usize,
        work: F,
    ) -> Result<Work<T>, RuntimeError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.submit_scoped(self.scope()?, kind, retained_bytes, work)
    }

    /// A turn can derive its own cancellable scope with WorkScope::for_turn.
    pub fn submit_scoped<T, F>(
        &self,
        scope: WorkScope,
        kind: SpeechWorkerKind,
        retained_bytes: usize,
        work: F,
    ) -> Result<Work<T>, RuntimeError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.scope()?;
        let identity = scope.identity();
        if identity.device != self.device.device
            || identity.runtime != self.device.runtime
            || identity.session != self.state.id
        {
            return Err(RuntimeError::StaleSession);
        }
        let index = SpeechWorkerKind::ALL
            .iter()
            .position(|entry| *entry == kind)
            .expect("all speech worker kinds are listed");
        let result = self.device.workers[index].submit(scope, retained_bytes, work);
        if matches!(result, Err(RuntimeError::Overloaded)) {
            self.state.stop.cancel();
        }
        result
    }

    pub async fn set_muted(&self, muted: bool) -> Result<(), RuntimeError> {
        if self.state.stop.is_cancelled() {
            return Err(RuntimeError::StaleSession);
        }
        // Revocation may bypass ordinary work. Only the actor can grant fresh permission.
        if muted {
            self.state.capture.borrow().scope.cancel();
        }
        let (reply, received) = oneshot::channel();
        self.device.send(
            Command::Privacy {
                session: self.id(),
                muted,
                reply,
            },
            true,
        )?;
        self.device.wait(received).await
    }

    pub async fn detach(&self) -> Result<(), RuntimeError> {
        if self.state.stop.is_cancelled() {
            return Err(RuntimeError::StaleSession);
        }
        self.state.stop.cancel();
        let (reply, received) = oneshot::channel();
        self.device.send(
            Command::Detach {
                session: self.id(),
                reply,
            },
            true,
        )?;
        self.device.wait(received).await
    }
}

/// One device runtime: its thread, and the handle that commands it.
pub struct DeviceRuntime {
    handle: DeviceHandle,
    /// The device's snapshot channel, which outlives this runtime when it is rebuilt.
    snapshots: Arc<watch::Sender<DeviceSnapshot>>,
    exited: oneshot::Receiver<Result<(), RuntimeError>>,
    /// How the thread ended, once it has.
    outcome: Option<Result<(), RuntimeError>>,
    thread: Option<thread::JoinHandle<()>>,
    shutdown_timeout: Duration,
}

impl DeviceRuntime {
    /// A runtime without ESPHome nodes: sessions, capture and speech workers only.
    pub async fn start(
        device: DeviceId,
        config: DeviceRuntimeConfig,
    ) -> Result<Self, RuntimeError> {
        Self::start_with(device, config, RuntimeServices::default()).await
    }

    /// A runtime with the shared services it serves its device with: the device's
    /// ESPHome nodes while a link session is attached, and the device event channel.
    pub async fn start_with(
        device: DeviceId,
        config: DeviceRuntimeConfig,
        services: RuntimeServices,
    ) -> Result<Self, RuntimeError> {
        let snapshots = Arc::new(watch::Sender::new(DeviceSnapshot {
            device: device.clone(),
            runtime: RuntimeId(0),
            lifecycle: Lifecycle::Starting,
            session: None,
            muted: None,
            workers_ready: false,
            failure: None,
            capture: None,
            link: None,
            session_link: None,
            esphome: EsphomeState::default(),
            health: DeviceHealth::default(),
        }));
        Self::start_in(device, config, services, snapshots).await
    }

    /// Starts a runtime that publishes on the device's existing snapshot channel: a
    /// rebuild after a failure. The device's health carries over; everything else in
    /// the snapshot describes the new runtime.
    pub(crate) async fn start_in(
        device: DeviceId,
        config: DeviceRuntimeConfig,
        services: RuntimeServices,
        snapshots: Arc<watch::Sender<DeviceSnapshot>>,
    ) -> Result<Self, RuntimeError> {
        config.ingress.validate()?;
        config.workers.validate()?;
        if config.shutdown_timeout.is_zero() {
            return Err(RuntimeError::InvalidLimits);
        }
        let supervision = match &services.supervision {
            Some(supervision) => supervision.clone(),
            None => Supervision::start(SupervisionOptions::default())?,
        };
        let runtime = RuntimeId(
            NEXT_RUNTIME
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| RuntimeError::Closed)?,
        );
        let stop = CancellationToken::new();
        let startup_guard = stop.clone().drop_guard();
        let (ordinary, ordinary_rx) = CommandQueue::new(config.ingress);
        let (urgent, urgent_rx) = CommandQueue::new(config.ingress);
        snapshots.send_modify(|state| {
            state.runtime = runtime;
            state.lifecycle = Lifecycle::Starting;
            state.session = None;
            state.muted = None;
            state.workers_ready = false;
            state.capture = None;
            state.link = None;
            state.session_link = None;
            state.esphome = EsphomeState::default();
        });
        let snapshot = snapshots.subscribe();
        let snapshot_tx = Arc::clone(&snapshots);
        let (ready_tx, ready_rx) = oneshot::channel();
        let (exit_tx, exited) = oneshot::channel();
        let child_stop = stop.clone();
        let child_device = device.clone();
        let ending_events = services.events.clone();
        let thread = thread::Builder::new()
            .name(format!("device-{}", runtime.0))
            .spawn(move || {
                let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), RuntimeError> {
                    let executor = Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| RuntimeError::ThreadStart(error.to_string()))?;
                    executor.block_on(async {
                        // Fresh workers: no model session or stream state of a
                        // runtime that failed is used again (§5.5).
                        let workers = SpeechWorkers::start(
                            &format!("device-{}", runtime.0),
                            config.workers,
                            child_stop.clone(),
                            WorkerWatch {
                                watchdog: supervision.watchdog.clone(),
                                owner: WorkerOwner {
                                    device: child_device.clone(),
                                    runtime,
                                },
                            },
                        )?;
                        let handles = std::array::from_fn(|index| {
                            workers.handle(SpeechWorkerKind::ALL[index])
                        });
                        snapshot_tx.send_modify(|state| {
                            state.lifecycle = Lifecycle::Ready;
                            state.workers_ready = true;
                            state.failure = None;
                        });
                        if ready_tx.send(handles).is_err() {
                            child_stop.cancel();
                        }
                        let (voice_tx, voice_rx) =
                            mpsc::channel(crate::voice_session::MESSAGE_CAPACITY);
                        let (node_tasks, task_failures) = RuntimeTasks::channel();
                        let (voice_services, speech) = match services.voice {
                            Some(shared) => {
                                let (voice, feed) = SpeechFeed::for_device(&shared);
                                (Some(voice), Some(feed))
                            }
                            None => (None, None),
                        };
                        let actor = Actor {
                            snapshots: &snapshot_tx,
                            stop: &child_stop,
                            max_age: config.ingress.max_age,
                            device: child_device.clone(),
                            runtime,
                            current: None,
                            next_session: 0,
                            nodes: EsphomeNodes::new(child_device, services.esphome, node_tasks),
                            events: services.events,
                            tasks: JoinSet::new(),
                            counters_due: None,
                            workers: std::array::from_fn(|index| {
                                workers.handle(SpeechWorkerKind::ALL[index])
                            }),
                            voice_services,
                            taps: services.taps,
                            voice_tx,
                            voice_rx,
                            kiosk_token: None,
                            speech_path: workers.speech_path(),
                            speech,
                            task_failures,
                        };
                        let result = actor.run(ordinary_rx, urgent_rx).await;
                        snapshot_tx.send_modify(|state| state.lifecycle = Lifecycle::Draining);
                        let failure = workers.failure();
                        let shutdown = workers.shutdown(config.shutdown_timeout).await;
                        if shutdown == Err(RuntimeError::ShutdownTimeout) {
                            // A worker is stuck in a call; that outranks the failure
                            // that stopped the runtime.
                            return shutdown;
                        }
                        if let Some(error) = failure.borrow().clone() {
                            return Err(error);
                        }
                        result.and(shutdown)
                    })
                }))
                .unwrap_or(Err(RuntimeError::WorkerPanicked));
                // Revokes every scope of the runtime, including a session the actor
                // could not end because it panicked.
                child_stop.cancel();
                let mut abandoned = None;
                snapshot_tx.send_modify(|state| {
                    if let Some(link) = state.session_link.take() {
                        // The actor did not end this session: close it here so the
                        // device dials again.
                        link.close();
                        abandoned = state.session;
                    }
                    state.lifecycle = if outcome.is_ok() {
                        Lifecycle::Stopped
                    } else {
                        Lifecycle::Failed
                    };
                    state.session = None;
                    state.muted = None;
                    state.capture = None;
                    state.link = None;
                    state.esphome.voice_listening = None;
                    state.esphome.proxy_listening = None;
                    state.workers_ready = false;
                    state.failure = outcome.as_ref().err().map(ToString::to_string);
                });
                if let (Some(session), Some(events)) = (abandoned, &ending_events)
                    && !events.try_send(DeviceEvent::SessionEnded {
                        device: snapshot_tx.borrow().device.clone(),
                        session,
                    })
                {
                    tracing::warn!(
                        "the device event queue is full; a session end was not reported"
                    );
                }
                let _ = exit_tx.send(outcome);
            })
            .map_err(|error| RuntimeError::ThreadStart(error.to_string()))?;
        let workers = match ready_rx.await {
            Ok(workers) => workers,
            Err(_) => {
                stop.cancel();
                let result = exited.await.unwrap_or(Err(RuntimeError::WorkerPanicked));
                let _ = thread.join();
                return Err(result.err().unwrap_or(RuntimeError::Closed));
            }
        };
        let handle = DeviceHandle {
            device,
            runtime,
            ordinary,
            urgent,
            stop,
            workers,
            snapshot,
        };
        let _ = startup_guard.disarm();
        Ok(Self {
            handle,
            snapshots,
            exited,
            outcome: None,
            thread: Some(thread),
            shutdown_timeout: config.shutdown_timeout,
        })
    }

    pub fn handle(&self) -> DeviceHandle {
        self.handle.clone()
    }

    /// The device's snapshot channel, for a runtime that replaces this one.
    pub(crate) fn snapshots(&self) -> Arc<watch::Sender<DeviceSnapshot>> {
        Arc::clone(&self.snapshots)
    }

    pub fn request_shutdown(&self) {
        self.handle.stop.cancel();
    }

    /// Waits until the runtime's thread has ended, by request or by failure, and
    /// returns how. Cancel-safe; once it has returned it returns the same at once.
    pub async fn exited(&mut self) -> Result<(), RuntimeError> {
        if let Some(outcome) = &self.outcome {
            return outcome.clone();
        }
        let outcome = (&mut self.exited)
            .await
            .unwrap_or(Err(RuntimeError::WorkerPanicked));
        self.outcome = Some(outcome.clone());
        outcome
    }

    /// Stops the runtime and joins its thread. A thread that has not ended by the
    /// shutdown deadline is [`RuntimeError::ShutdownTimeout`].
    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        self.request_shutdown();
        let timeout = self.shutdown_timeout;
        let result = tokio::time::timeout(timeout, self.exited())
            .await
            .map_err(|_| RuntimeError::ShutdownTimeout)?;
        if let Some(thread) = self.thread.take() {
            thread.join().map_err(|_| RuntimeError::WorkerPanicked)?;
        }
        result
    }
}

impl Drop for DeviceRuntime {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

/// What a runtime's voice path reads speech readiness from: the controller's speech
/// status, unless this device's speech path is down (§5.5). Then its sessions see
/// [`SpeechStatus::Unavailable`] with the reason and refuse wakes, while every other
/// device keeps working.
struct SpeechFeed {
    /// The controller's status; `None` once its sender is gone (loading settled).
    global: Option<watch::Receiver<SpeechStatus>>,
    /// The status this runtime's sessions read.
    local: watch::Sender<SpeechStatus>,
    /// The latest controller status, kept after its sender is gone.
    latest: SpeechStatus,
}

impl SpeechFeed {
    /// The voice services this runtime's sessions use: the shared ones, reading speech
    /// readiness from this feed.
    fn for_device(shared: &Arc<VoiceServices>) -> (Arc<VoiceServices>, Self) {
        let global = shared.speech.clone();
        let latest = global.borrow().clone();
        let (local, speech) = watch::channel(latest.clone());
        let voice = Arc::new(VoiceServices {
            speech,
            ..VoiceServices::clone(shared)
        });
        (
            voice,
            Self {
                global: Some(global),
                local,
                latest,
            },
        )
    }

    /// Publishes the status the sessions should see, with `path` this device's speech
    /// path.
    fn publish(&self, path: &SpeechPathState) {
        let status = match path.unavailable() {
            Some(reason) => SpeechStatus::Unavailable(reason),
            None => self.latest.clone(),
        };
        self.local.send_replace(status);
    }
}

/// The controller's next speech status; pending forever once its sender is gone.
async fn speech_changed(feed: Option<&mut SpeechFeed>) -> Option<SpeechStatus> {
    let Some(global) = feed.and_then(|feed| feed.global.as_mut()) else {
        return std::future::pending().await;
    };
    match global.changed().await {
        Ok(()) => Some(global.borrow_and_update().clone()),
        Err(_) => None,
    }
}

/// Results of work the actor started on its own thread. The ESPHome preparation, once
/// per session, is boxed so the far more frequent voice task results stay small.
pub(crate) enum Internal {
    Esphome(Box<Prepared>),
    /// A voice-path task finished; its answer, if any, came as a [`VoiceMessage`].
    VoiceTask,
}

/// One thing the actor reacts to.
enum Event {
    Command(Envelope),
    LinkUrgent(Option<UrgentEvent>),
    SessionStopped,
    LinkStatus(Option<bool>),
    Ingress(Option<IngressItem>),
    Telemetry(Option<Telemetry>),
    Satellite(Option<SatelliteRequest>),
    Task(Result<Internal, tokio::task::JoinError>),
    PublishCounters,
    Voice(VoiceMessage),
    VoiceTimer,
    VoiceChange(WatchChange),
    /// A task the runtime spawned outside its join set panicked.
    TaskPanicked(String),
    /// Which of this device's speech workers are stuck changed.
    SpeechPath(SpeechPathState),
    /// The controller's speech status changed; `None` once it no longer can.
    Speech(Option<SpeechStatus>),
}

struct Actor<'a> {
    snapshots: &'a watch::Sender<DeviceSnapshot>,
    stop: &'a CancellationToken,
    max_age: Duration,
    device: DeviceId,
    runtime: RuntimeId,
    current: Option<ActiveSession>,
    next_session: u64,
    nodes: EsphomeNodes,
    tasks: JoinSet<Internal>,
    /// Where readiness, alert messages and Home Assistant requests go.
    events: Option<DeviceEvents>,
    /// Counters changed and are published at this time.
    counters_due: Option<tokio::time::Instant>,
    workers: [SpeechWorkerHandle; 4],
    voice_services: Option<Arc<VoiceServices>>,
    /// The device services' taps; `None` for a runtime without them.
    taps: Option<crate::taps::DeviceTaps>,
    /// Where voice-path tasks post their answers.
    voice_tx: mpsc::Sender<VoiceMessage>,
    voice_rx: mpsc::Receiver<VoiceMessage>,
    /// The device row's Kiosk Satellite token, as the latest attach or dashboard change
    /// said.
    kiosk_token: Option<String>,
    /// Which speech workers the watchdog sees stuck past the call deadline.
    speech_path: watch::Receiver<SpeechPathState>,
    /// The speech status this runtime's sessions read; `None` without voice services.
    speech: Option<SpeechFeed>,
    /// Panics of the tasks the ESPHome nodes spawned.
    task_failures: mpsc::UnboundedReceiver<String>,
}

/// The next state of the device's speech path; pending once the workers are gone.
async fn path_changed(path: &mut watch::Receiver<SpeechPathState>) -> SpeechPathState {
    if path.changed().await.is_err() {
        return std::future::pending().await;
    }
    path.borrow_and_update().clone()
}

/// The link's next status, `Some(audio_joined)`; `None` once the link has ended.
async fn recv_status(receiver: Option<&mut watch::Receiver<LinkStatus>>) -> Option<bool> {
    match receiver {
        Some(receiver) => match receiver.changed().await {
            Ok(()) => Some(receiver.borrow_and_update().audio_joined),
            Err(_) => None,
        },
        None => std::future::pending().await,
    }
}

async fn recv_urgent(receiver: Option<&mut UrgentReceiver>) -> Option<UrgentEvent> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_ingress(receiver: Option<&mut IngressReceiver>) -> Option<IngressItem> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_telemetry(receiver: Option<&mut TelemetryReceiver>) -> Option<Telemetry> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_satellite(
    receiver: Option<&mut mpsc::Receiver<SatelliteRequest>>,
) -> Option<SatelliteRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(due: Option<tokio::time::Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due).await,
        None => std::future::pending().await,
    }
}

async fn watch_change(watch: Option<&mut VoiceWatch>) -> WatchChange {
    match watch {
        Some(watch) => watch.changed().await,
        None => std::future::pending().await,
    }
}

impl Actor<'_> {
    async fn run(
        mut self,
        mut ordinary: mpsc::Receiver<Envelope>,
        mut urgent: mpsc::Receiver<Envelope>,
    ) -> Result<(), RuntimeError> {
        let never = CancellationToken::new();
        let result = loop {
            let attached = self.current.is_some();
            let current_stop = self
                .current
                .as_ref()
                .map(|session| session.state.stop.clone())
                .unwrap_or_else(|| never.clone());
            let (link_urgent, link_ingress, link_telemetry, link_status, voice_watch, voice_due) =
                match self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    Some(LinkBinding {
                        urgent,
                        ingress,
                        telemetry,
                        status,
                        voice,
                        ..
                    }) => {
                        let due = voice.deadline();
                        (
                            Some(urgent),
                            ingress.as_mut(),
                            telemetry.as_mut(),
                            status.as_mut(),
                            Some(&mut voice.watch),
                            due,
                        )
                    }
                    None => (None, None, None, None, None, None),
                };
            let satellite = self.nodes.requests();
            let has_tasks = !self.tasks.is_empty();
            let event = tokio::select! {
                biased;
                _ = self.stop.cancelled() => break Ok(()),
                Some(reason) = self.task_failures.recv() => Event::TaskPanicked(reason),
                state = path_changed(&mut self.speech_path) => Event::SpeechPath(state),
                status = speech_changed(self.speech.as_mut()) => Event::Speech(status),
                message = urgent.recv() => match message {
                    Some(message) => Event::Command(message),
                    None => break Ok(()),
                },
                event = recv_urgent(link_urgent) => Event::LinkUrgent(event),
                _ = current_stop.cancelled(), if attached => Event::SessionStopped,
                changed = recv_status(link_status) => Event::LinkStatus(changed),
                message = ordinary.recv() => match message {
                    Some(message) => Event::Command(message),
                    None => break Ok(()),
                },
                item = recv_ingress(link_ingress) => Event::Ingress(item),
                report = recv_telemetry(link_telemetry) => Event::Telemetry(report),
                request = recv_satellite(satellite) => Event::Satellite(request),
                Some(done) = self.tasks.join_next(), if has_tasks => Event::Task(done),
                _ = sleep_until(self.counters_due) => Event::PublishCounters,
                Some(message) = self.voice_rx.recv() => Event::Voice(message),
                _ = sleep_until(voice_due) => Event::VoiceTimer,
                change = watch_change(voice_watch) => Event::VoiceChange(change),
            };
            if let Err(error) = self.handle(event) {
                break Err(error);
            }
            self.flush_evidence();
        };
        self.end_session("the device runtime stopped");
        // Records the ending session queued are still written, within a deadline.
        let _ = tokio::time::timeout(NODE_SHUTDOWN_DEADLINE, async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        self.tasks.abort_all();
        self.nodes.shutdown(NODE_SHUTDOWN_DEADLINE).await;
        result
    }

    fn handle(&mut self, event: Event) -> Result<(), RuntimeError> {
        match event {
            Event::Command(envelope) => {
                // A command that waited past the queue's age limit means this thread
                // was slow (a loaded host), not that the runtime is broken: failing it
                // would rebuild the runtime and drop the device's session for nothing.
                // A late attach is refused, so admission declines and the device dials
                // again; everything else, revocations above all, is still applied.
                let late = envelope.queued.elapsed() > self.max_age;
                match envelope.command {
                    Command::Attach { reply, .. } if late => {
                        tracing::warn!(device = %self.device, "a late attach was refused");
                        let _ = reply.send(Err(RuntimeError::Expired));
                    }
                    Command::AttachLink { reply, .. } if late => {
                        // Dropping the session here ends it; the device dials again.
                        tracing::warn!(device = %self.device, "a late session attach was refused");
                        let _ = reply.send(Err(RuntimeError::Expired));
                    }
                    command => self.command(command)?,
                }
            }
            Event::TaskPanicked(reason) => {
                // A node task that panicked left its state unknown: the runtime fails
                // and is rebuilt (§5.5).
                tracing::error!(device = %self.device, %reason, "a device runtime task panicked");
                self.stop.cancel();
                return Err(RuntimeError::WorkerPanicked);
            }
            Event::SpeechPath(path) => self.speech_path_changed(&path),
            Event::Speech(status) => {
                if let Some(feed) = self.speech.as_mut() {
                    match status {
                        Some(status) => {
                            feed.latest = status;
                            feed.publish(&self.speech_path.borrow());
                        }
                        None => feed.global = None,
                    }
                }
            }
            Event::LinkStatus(Some(audio_joined)) => {
                if audio_joined {
                    self.announce_ready();
                }
            }
            Event::LinkStatus(None) => {
                if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    binding.status = None;
                }
            }
            Event::LinkUrgent(Some(UrgentEvent::Privacy { change, .. })) => {
                let binding = self.current.as_mut().and_then(|s| s.link.as_mut());
                let Some(binding) = binding else {
                    return Ok(());
                };
                // Button and mute events share one number that only rises (§3.1.5).
                if let Some(seq) = change.physical_seq {
                    if binding.physical_seq.is_some_and(|last| seq <= last) {
                        return Ok(());
                    }
                    binding.physical_seq = Some(seq);
                }
                self.apply_privacy(change.muted);
                if let Some(taps) = self
                    .current
                    .as_mut()
                    .and_then(|s| s.link.as_mut())
                    .and_then(|b| b.taps.as_mut())
                {
                    taps.privacy(change.muted);
                }
                self.voice_privacy(change.muted, change.physical_seq);
            }
            Event::LinkUrgent(Some(UrgentEvent::Ended(reason))) => {
                tracing::info!(device = %self.device, %reason, "device session ended");
                self.end_session("the device session ended");
            }
            Event::LinkUrgent(None) => self.end_session("the device session ended"),
            Event::SessionStopped => self.end_session("the session was revoked"),
            Event::Ingress(Some(item)) => self.ingress(item),
            Event::Ingress(None) => {
                if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    binding.ingress = None;
                }
            }
            Event::Telemetry(Some(report)) => self.telemetry(report),
            Event::Telemetry(None) => {
                if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    binding.telemetry = None;
                }
            }
            Event::Satellite(Some(request)) => self.satellite_request(request),
            Event::Satellite(None) => {
                self.snapshots
                    .send_modify(|state| self.nodes.voice_ended(&mut state.esphome));
            }
            Event::Task(Ok(Internal::Esphome(prepared))) => {
                let attached = self
                    .current
                    .as_ref()
                    .filter(|s| s.link.is_some())
                    .map(|s| s.state.id);
                self.snapshots.send_modify(|state| {
                    self.nodes.install(*prepared, attached, &mut state.esphome);
                });
            }
            Event::Task(Err(error)) => {
                // A panicked helper task is a runtime failure (§5.5), not a lost result.
                if error.is_panic() {
                    self.stop.cancel();
                    return Err(RuntimeError::WorkerPanicked);
                }
            }
            Event::PublishCounters => self.publish_link(),
            Event::Task(Ok(Internal::VoiceTask)) => {}
            Event::Voice(message) => {
                let session = match &message {
                    VoiceMessage::Input { session, .. } | VoiceMessage::Decoded { session, .. } => {
                        *session
                    }
                };
                let binding = self
                    .current
                    .as_mut()
                    .filter(|active| active.state.id == session)
                    .and_then(|active| active.link.as_mut());
                match binding {
                    Some(binding) => binding.voice.message(message, &self.nodes, &mut self.tasks),
                    // A late answer for a session that ended: what asked for it is gone.
                    None => {
                        if let VoiceMessage::Decoded { stream, .. } = message {
                            stream.cancel();
                        }
                    }
                }
                self.publish_voice();
            }
            Event::VoiceTimer => {
                if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    binding
                        .voice
                        .handle(Input::Timer, &self.nodes, &mut self.tasks);
                }
                self.publish_voice();
            }
            Event::VoiceChange(change) => {
                if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
                    binding.voice.changed(change, &self.nodes, &mut self.tasks);
                }
                self.publish_voice();
            }
        }
        Ok(())
    }

    fn command(&mut self, command: Command) -> Result<(), RuntimeError> {
        match command {
            Command::Attach { muted, reply } => {
                if reply.is_closed() {
                    return Ok(());
                }
                let state = self.install(Some(muted), None)?;
                let grant = SessionGrant {
                    state: Arc::clone(&state),
                    lease: Arc::new(state.stop.clone().drop_guard()),
                };
                let _ = reply.send(Ok(grant));
            }
            Command::AttachLink { attach, reply } => {
                if reply.is_closed() {
                    // Admission gave up; dropping the session ends it.
                    return Ok(());
                }
                let attach = *attach;
                let wanted = Wanted {
                    label: attach
                        .label
                        .clone()
                        .unwrap_or_else(|| self.device.0.clone()),
                    platform: attach.platform,
                    entities: VoiceEntities::from_capabilities(&attach.hello.capabilities),
                    bluetooth_proxy: attach.bluetooth_proxy
                        && attach.hello.capabilities.contains(Capability::BleProxy),
                };
                let muted = attach.hello.privacy.as_ref().and_then(|p| p.muted);
                let state = self.install(muted, Some(attach))?;
                let id = state.id;
                let _ = reply.send(Ok(id));
                let mut preparation = None;
                self.snapshots.send_modify(|snapshot| {
                    preparation = self.nodes.on_attach(id, wanted, &mut snapshot.esphome);
                });
                if let Some(preparation) = preparation {
                    self.tasks
                        .spawn(async move { Internal::Esphome(Box::new(preparation.await)) });
                }
            }
            Command::Privacy {
                session,
                muted,
                reply,
            } => {
                let live = self
                    .current
                    .as_ref()
                    .is_some_and(|s| s.state.id == session && !s.state.stop.is_cancelled());
                if !live {
                    let _ = reply.send(Err(RuntimeError::StaleSession));
                    return Ok(());
                }
                self.apply_privacy(muted);
                let _ = reply.send(Ok(()));
            }
            Command::Detach { session, reply } => {
                if self.current.as_ref().map(|active| active.state.id) == Some(session) {
                    self.end_session("the session detached");
                }
                let result = if session.0 == self.next_session {
                    Ok(())
                } else {
                    Err(RuntimeError::StaleSession)
                };
                let _ = reply.send(result);
            }
            Command::Renegotiate { reply } => {
                let attached = self.current.as_ref().is_some_and(|s| s.link.is_some());
                if attached {
                    self.end_session("the session is renegotiated");
                }
                let _ = reply.send(Ok(attached));
            }
            Command::Voice(command) => self.voice_command(command),
            Command::RecordingChanged => self.refresh_recording(),
        }
        Ok(())
    }

    /// Arms or disarms the attached session's recording-mode subscription to match the
    /// stored modes.
    fn refresh_recording(&mut self) {
        let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) else {
            return;
        };
        let Some(taps) = binding.taps.as_mut() else {
            return;
        };
        let sink = match taps.recording() {
            crate::taps::SinkChange::Keep => return,
            crate::taps::SinkChange::Arm(sink) => Some(sink),
            crate::taps::SinkChange::Disarm => None,
        };
        binding
            .voice
            .set_diagnostic(sink, &self.nodes, &mut self.tasks);
        binding.state.voice = Some(binding.voice.status());
    }

    /// A dashboard request for the voice path, answered by the attached session's
    /// machine, or at once when no session is attached.
    fn voice_command(&mut self, command: VoiceCommand) {
        if let VoiceCommand::KioskToken(token) = &command {
            self.kiosk_token = token.clone();
        }
        let binding = self
            .current
            .as_mut()
            .and_then(|s| s.link.as_mut())
            .filter(|binding| !binding.voice.is_lost());
        let Some(binding) = binding else {
            match command {
                VoiceCommand::Preview { reply, .. } => {
                    let _ = reply.send(PreviewOutcome::SessionEnded);
                }
                VoiceCommand::StopPreview { reply } => {
                    let _ = reply.send(PreviewOutcome::Stopped { was_playing: false });
                }
                // Kept for the next session's attach.
                VoiceCommand::KioskToken(_) => {}
            }
            return;
        };
        match command {
            VoiceCommand::Preview { sound, reply } => {
                binding
                    .voice
                    .preview(sound, reply, &self.nodes, &mut self.tasks);
            }
            VoiceCommand::StopPreview { reply } => {
                binding
                    .voice
                    .stop_preview(reply, &self.nodes, &mut self.tasks);
            }
            VoiceCommand::KioskToken(token) => {
                binding
                    .voice
                    .handle(Input::KioskToken(token), &self.nodes, &mut self.tasks);
            }
        }
        self.publish_voice();
    }

    /// Ends the current session, then installs a new one with `muted` as reported.
    fn install(
        &mut self,
        muted: Option<bool>,
        attach: Option<LinkAttach>,
    ) -> Result<Arc<SessionState>, RuntimeError> {
        if let Some(old) = self.current.take() {
            old.state.stop.cancel();
            if let Some(binding) = old.link {
                self.retire_link(old.state.id, binding);
            }
        }
        self.next_session = NEXT_SESSION
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| RuntimeError::Closed)?;
        let id = SessionId(self.next_session);
        let session_stop = self.stop.child_token();
        let scope = WorkScope::new(
            WorkIdentity {
                device: self.device.clone(),
                runtime: self.runtime,
                session: id,
                turn: None,
            },
            session_stop.child_token(),
        );
        // A session that starts muted (or without a privacy report) has no live capture.
        let capture_muted = muted != Some(false);
        if capture_muted {
            scope.cancel();
        }
        let link = match attach {
            None => None,
            Some(attach) => {
                let Some(services) = self.voice_services.clone() else {
                    tracing::warn!(device = %self.device, "a runtime without voice services cannot attach a device link");
                    return Err(RuntimeError::Closed);
                };
                let LinkAttach {
                    session,
                    hello,
                    platform,
                    peer,
                    label: _,
                    afe_metadata,
                    bluetooth_proxy: _,
                    capture_permitted,
                    capture_refusal,
                    kiosk_token,
                } = attach;
                // The link cancels this scope itself on a mute report and at the end.
                session.link.publish_capture_scope(scope.clone());
                let volume =
                    hello
                        .volume
                        .and_then(|volume| volume.level)
                        .map(|level| match level {
                            VolumeLevel::Level(level) => Volume::Level(level),
                            VolumeLevel::Unavailable => Volume::Unavailable,
                        });
                let status = session.link.watch_status();
                let voice = VoiceSession::start(
                    VoiceStart {
                        services,
                        device: self.device.clone(),
                        session: id,
                        link: session.link.clone(),
                        capabilities: hello.capabilities.clone(),
                        afe_metadata,
                        muted: capture_muted,
                        volume: match volume {
                            Some(Volume::Level(level)) => DeviceVolume::new(level),
                            _ => None,
                        },
                        session_scope: WorkScope::new(
                            scope.identity().clone(),
                            session_stop.child_token(),
                        ),
                        capture: (!capture_muted).then(|| scope.clone()),
                        capture_permitted,
                        kiosk_token: {
                            self.kiosk_token = kiosk_token;
                            self.kiosk_token.clone()
                        },
                        workers: self.workers.clone(),
                        messages: self.voice_tx.clone(),
                    },
                    &mut self.tasks,
                );
                let taps = self.taps.as_ref().map(|taps| {
                    taps.session(crate::taps::SessionStart {
                        device: &self.device,
                        runtime: self.runtime,
                        session: id,
                        link: &session.link,
                        firmware: hello.firmware_version.clone(),
                        afe_metadata,
                        muted: capture_muted,
                        capture_permitted,
                    })
                });
                Some(LinkBinding {
                    state: LinkState {
                        session_id: session.link.session_id().clone(),
                        transport: session.link.transport(),
                        ip: peer,
                        platform,
                        capabilities: hello.capabilities,
                        firmware_version: hello.firmware_version,
                        os_version: hello.os_version,
                        volume,
                        ambient_lux: None,
                        stats: None,
                        audio_accepted: BTreeMap::new(),
                        capture_permitted,
                        capture_refusal,
                        voice: Some(voice.status()),
                        unhandled: BTreeMap::new(),
                        telemetry_dropped: 0,
                        malformed: BTreeMap::new(),
                        installed_assets: hello.assets,
                    },
                    link: session.link,
                    urgent: session.urgent,
                    ingress: Some(session.ingress),
                    telemetry: Some(session.telemetry),
                    voice,
                    physical_seq: None,
                    status: Some(status),
                    announced: false,
                    hello_alerts: hello.alerts,
                    taps,
                })
            }
        };
        let capture_scope = scope.clone();
        let permission = CapturePermission {
            muted: capture_muted,
            scope,
        };
        let (capture, capture_rx) = watch::channel(permission);
        let state = Arc::new(SessionState {
            id,
            stop: session_stop,
            capture: capture_rx,
        });
        let link_state = link.as_ref().map(|binding| Arc::new(binding.state.clone()));
        let session_link = link.as_ref().map(|binding| binding.link.clone());
        self.snapshots.send_modify(|snapshot| {
            snapshot.lifecycle = Lifecycle::Attached;
            snapshot.session = Some(id);
            snapshot.muted = muted;
            snapshot.capture = Some(capture_scope);
            snapshot.link = link_state;
            snapshot.session_link = session_link;
        });
        self.counters_due = None;
        self.current = Some(ActiveSession {
            state: Arc::clone(&state),
            capture,
            link,
        });
        self.refresh_recording();
        Ok(state)
    }

    /// Ends the current session, if any: its work is revoked, its link closed and its
    /// ESPHome listeners stopped. The ESPHome records stay published.
    fn end_session(&mut self, why: &'static str) {
        let Some(active) = self.current.take() else {
            return;
        };
        active.state.stop.cancel();
        if let Some(binding) = active.link {
            tracing::debug!(device = %self.device, why, "detaching the device session");
            self.retire_link(active.state.id, binding);
        }
        self.counters_due = None;
        self.snapshots.send_modify(|state| {
            state.lifecycle = Lifecycle::Ready;
            state.session = None;
            state.muted = None;
            state.capture = None;
            state.link = None;
            state.session_link = None;
            self.nodes.on_detach(&mut state.esphome);
        });
    }

    /// Ends a session's voice path (its final records are still written), closes its
    /// link and, when it was reported ready, reports its end.
    fn retire_link(&mut self, session: SessionId, mut binding: LinkBinding) {
        binding.voice.end(&self.nodes, &mut self.tasks);
        if let Some(mut taps) = binding.taps.take() {
            // The session's last decisions go into its recording before it ends.
            let (origin, evidence) = binding.voice.take_evidence();
            taps.evidence(origin, evidence);
            taps.ended(echomuse_recorder::DisconnectReason::LinkLost);
        }
        binding.link.close();
        if binding.announced
            && let Some(events) = &self.events
            && !events.try_send(DeviceEvent::SessionEnded {
                device: self.device.clone(),
                session,
            })
        {
            // The link is closed either way; the services learn of the end from the
            // session no longer being current.
            tracing::warn!(device = %self.device, "the device event queue is full; a session end was not reported");
        }
    }

    /// Hands the voice path's decision evidence to the session recorder, and tells the
    /// voice path whether one is recording (only then does it queue evidence).
    fn flush_evidence(&mut self) {
        let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) else {
            return;
        };
        let Some(taps) = binding.taps.as_mut() else {
            return;
        };
        let (origin, evidence) = binding.voice.take_evidence();
        taps.evidence(origin, evidence);
        binding.voice.set_recording(taps.session_recording());
    }

    /// Reports the attached link session as ready, once: the device has read
    /// `session.ready`, so the shared services may now address it. A full event queue
    /// closes the session rather than leaving it half known (§5.2).
    fn announce_ready(&mut self) {
        let Some(active) = self.current.as_mut() else {
            return;
        };
        let session = active.state.id;
        let Some(binding) = active.link.as_mut() else {
            return;
        };
        if binding.announced {
            return;
        }
        binding.status = None;
        let Some(events) = &self.events else {
            return;
        };
        let ready = ReadySession {
            device: self.device.clone(),
            session,
            link: binding.link.clone(),
            platform: binding.state.platform,
            capabilities: binding.state.capabilities.clone(),
            alerts: binding.hello_alerts.clone(),
            firmware_version: binding.state.firmware_version.clone(),
        };
        if events.try_send(DeviceEvent::SessionReady(Box::new(ready))) {
            binding.announced = true;
        } else {
            tracing::warn!(device = %self.device, "the device event queue is full; closing the session");
            self.end_session("the device event queue is full");
        }
    }

    /// Installs a privacy change: a mute revokes capture at once and leaves it revoked;
    /// an unmute grants a fresh scope. Only this actor grants capture.
    fn apply_privacy(&mut self, muted: bool) {
        let Some(active) = self.current.as_ref() else {
            return;
        };
        let previous = active.capture.borrow().clone();
        if muted {
            previous.scope.cancel();
            if !previous.muted {
                active.capture.send_replace(CapturePermission {
                    muted,
                    scope: previous.scope,
                });
            }
        } else if previous.muted || previous.scope.is_cancelled() {
            let scope = WorkScope::new(
                previous.scope.identity().clone(),
                active.state.stop.child_token(),
            );
            if let Some(binding) = &active.link {
                binding.link.publish_capture_scope(scope.clone());
            }
            active.capture.send_replace(CapturePermission {
                muted,
                scope: scope.clone(),
            });
            self.snapshots
                .send_modify(|state| state.capture = Some(scope));
        }
        self.snapshots
            .send_modify(|state| state.muted = Some(muted));
    }

    fn ingress(&mut self, item: IngressItem) {
        let Some(active) = self.current.as_mut() else {
            return;
        };
        let session = active.state.id;
        let Some(binding) = active.link.as_mut() else {
            return;
        };
        if binding.voice.is_lost() {
            // The session broke the stream rules and is closing.
            return;
        }
        let mut alert = None;
        if let Some(taps) = binding.taps.as_mut() {
            taps.ingress(item.ordinal, &item.ingress);
        }
        let input = match item.ingress {
            Ingress::Audio(packet) => {
                if let Some(stream) = StreamId::of_kind(packet.header().kind) {
                    *binding.state.audio_accepted.entry(stream).or_default() += 1;
                }
                Some(Input::Audio(packet))
            }
            Ingress::Control(envelope) => match &envelope.message {
                DeviceMessage::StreamOpen(_)
                | DeviceMessage::StreamEnd(_)
                | DeviceMessage::UplinkEnded(_) => Some(Input::Control(envelope)),
                DeviceMessage::Other { message_type, .. }
                    if self.events.is_some() && is_alert_message(*message_type) =>
                {
                    if let DeviceMessage::Other { message_type, body } = envelope.message {
                        alert = Some((message_type, body));
                    }
                    None
                }
                DeviceMessage::Other { message_type, .. } if voice_message(*message_type) => {
                    Some(Input::Control(envelope))
                }
                DeviceMessage::Other { message_type, body }
                    if binding
                        .taps
                        .as_mut()
                        .is_some_and(|taps| taps.control(*message_type, body)) =>
                {
                    None
                }
                other => {
                    *binding
                        .state
                        .unhandled
                        .entry(other.message_type().as_str())
                        .or_default() += 1;
                    None
                }
            },
        };
        if let Some(input) = input {
            binding.voice.handle(input, &self.nodes, &mut self.tasks);
            binding.state.voice = Some(binding.voice.status());
        }
        self.counters_changed();
        if let Some((message_type, body)) = alert {
            self.forward_alert(session, message_type, body);
        }
    }

    /// A privacy change for the voice path, after the capture scope changed: the
    /// machine revokes on a mute and takes the fresh scope on an unmute.
    fn voice_privacy(&mut self, muted: bool, physical_seq: Option<u64>) {
        let Some(active) = self.current.as_mut() else {
            return;
        };
        let permission = active.capture.borrow().clone();
        let Some(binding) = active.link.as_mut() else {
            return;
        };
        binding.voice.handle(
            Input::Privacy {
                muted,
                physical_seq,
            },
            &self.nodes,
            &mut self.tasks,
        );
        let scope =
            (!permission.muted && !permission.scope.is_cancelled()).then_some(permission.scope);
        binding
            .voice
            .handle(Input::Capture(scope), &self.nodes, &mut self.tasks);
        self.publish_voice();
    }

    /// The watchdog marked a speech worker of this device stuck, or one came back.
    /// While any is stuck the device is unavailable with the reason and its sessions
    /// read speech as unavailable, so they refuse wakes; no other device is affected.
    fn speech_path_changed(&mut self, path: &SpeechPathState) {
        let reason = path.unavailable();
        self.snapshots.send_modify(|state| {
            let health = &mut state.health;
            match reason.clone() {
                Some(reason) => {
                    if !matches!(health.availability, Availability::SpeechUnavailable { .. }) {
                        health.errors.record(&reason);
                    }
                    tracing::warn!(device = %self.device, %reason, "the device refuses wakes");
                    health.availability = Availability::SpeechUnavailable { reason };
                }
                None => {
                    if matches!(health.availability, Availability::SpeechUnavailable { .. }) {
                        tracing::info!(device = %self.device, "the device's speech path works again");
                        health.availability = Availability::Available;
                    }
                }
            }
        });
        if let Some(feed) = &self.speech {
            feed.publish(path);
        }
    }

    /// Publishes the voice path's state with the link's.
    fn publish_voice(&mut self) {
        if let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) {
            binding.state.voice = Some(binding.voice.status());
        }
        self.counters_changed();
    }

    /// Passes an alert message to the alert service, in ingress order. The device sends
    /// one only after reading `session.ready`, so the session is ready by then if it was
    /// not reported so already.
    fn forward_alert(
        &mut self,
        session: SessionId,
        message_type: MessageType,
        body: Map<String, Value>,
    ) {
        self.announce_ready();
        let still_current = self
            .current
            .as_ref()
            .is_some_and(|s| s.state.id == session && s.link.as_ref().is_some_and(|b| b.announced));
        let Some(events) = self.events.as_ref().filter(|_| still_current) else {
            return;
        };
        let sent = events.try_send(DeviceEvent::Alert {
            device: self.device.clone(),
            session,
            message_type,
            body,
        });
        if !sent {
            // An alert message is required: losing one would desynchronise the cache.
            tracing::warn!(device = %self.device, "the device event queue is full; closing the session");
            self.end_session("the device event queue is full");
        }
    }

    fn telemetry(&mut self, report: Telemetry) {
        let Some(binding) = self.current.as_mut().and_then(|s| s.link.as_mut()) else {
            return;
        };
        let state = &mut binding.state;
        state.telemetry_dropped = state
            .telemetry_dropped
            .saturating_add(report.dropped_before);
        let message_type = report.envelope.message.message_type();
        let DeviceMessage::Other { body, .. } = report.envelope.message else {
            *state.malformed.entry(message_type.as_str()).or_default() += 1;
            self.publish_link();
            return;
        };
        let mut lux = None;
        let mut volume = None;
        match report.kind {
            TelemetryKind::Stats => state.stats = Some(Arc::new(body)),
            TelemetryKind::VolumeState => match decode_volume(&body) {
                Some(level) => {
                    state.volume = Some(Volume::Level(level));
                    volume = Some(DeviceVolume::new(level));
                }
                None => *state.malformed.entry(message_type.as_str()).or_default() += 1,
            },
            TelemetryKind::AmbientLight => match body.get("lux").and_then(Value::as_i64) {
                Some(reading) => {
                    state.ambient_lux = Some(reading);
                    lux = Some(reading);
                }
                None => *state.malformed.entry(message_type.as_str()).or_default() += 1,
            },
            TelemetryKind::BleAdverts => {
                let (adverts, skipped) = decode_adverts(&body);
                if skipped > 0 {
                    *state.malformed.entry(message_type.as_str()).or_default() += skipped as u64;
                }
                match self.nodes.forward_adverts(adverts) {
                    Some(true) => {}
                    Some(false) => {
                        self.snapshots
                            .send_modify(|snapshot| snapshot.esphome.adverts_dropped += 1);
                    }
                    None => *state.unhandled.entry(message_type.as_str()).or_default() += 1,
                }
            }
            TelemetryKind::Log if binding.taps.is_some() => {
                if let Some(taps) = &binding.taps {
                    taps.log(&body);
                }
            }
            TelemetryKind::WakeStats | TelemetryKind::Log => {
                *state.unhandled.entry(message_type.as_str()).or_default() += 1;
            }
        }
        if let Some(volume) = volume {
            binding
                .voice
                .handle(Input::Volume(volume), &self.nodes, &mut self.tasks);
        }
        if let Some(lux) = lux {
            // The sensor entity takes a float; lux readings are small integers.
            self.nodes.set_ambient_light(Some(lux as f32));
        }
        self.publish_link();
    }

    fn satellite_request(&mut self, request: SatelliteRequest) {
        // Timers and the stop button go to the alert service; media, announcements and
        // the runs HA starts go to the voice path of the attached session.
        let forwarded = match (&self.events, request) {
            (Some(events), SatelliteRequest::Timer(event)) => events
                .try_send(DeviceEvent::Timer {
                    device: self.device.clone(),
                    event,
                })
                .then_some(())
                .ok_or("timer"),
            (Some(events), SatelliteRequest::StopAlert) => events
                .try_send(DeviceEvent::StopAlert {
                    device: self.device.clone(),
                })
                .then_some(())
                .ok_or("stop_alert"),
            (None, SatelliteRequest::StopAlert) => Err("stop_alert"),
            (None, SatelliteRequest::Timer(_)) => Err("timer"),
            (_, request) => match self.current.as_mut().and_then(|s| s.link.as_mut()) {
                Some(binding) => {
                    binding
                        .voice
                        .handle(Input::Satellite(request), &self.nodes, &mut self.tasks);
                    Ok(())
                }
                None => {
                    if let SatelliteRequest::Announce(announcement) = request {
                        let refusal = echomuse_voice::SatelliteCommand::FinishAnnouncement {
                            id: announcement.id,
                            success: false,
                        };
                        self.nodes.satellite(refusal, None);
                    }
                    Err("no_session")
                }
            },
        };
        if let Err(kind) = forwarded {
            tracing::info!(device = %self.device, kind, "Home Assistant request refused: not available");
            self.snapshots
                .send_modify(|state| *state.esphome.refused.entry(kind).or_default() += 1);
        }
    }

    fn counters_changed(&mut self) {
        if self.counters_due.is_none() {
            self.counters_due = Some(tokio::time::Instant::now() + COUNTER_PUBLISH_INTERVAL);
        }
    }

    fn publish_link(&mut self) {
        self.counters_due = None;
        let Some(binding) = self.current.as_ref().and_then(|s| s.link.as_ref()) else {
            return;
        };
        let state = Arc::new(binding.state.clone());
        self.snapshots
            .send_modify(|snapshot| snapshot.link = Some(state));
    }
}

/// `volume_state`'s `{"level": 0..127}`.
fn decode_volume(body: &Map<String, Value>) -> Option<u8> {
    body.get("level")
        .and_then(Value::as_u64)
        .and_then(|level| u8::try_from(level).ok())
        .filter(|level| *level <= 127)
}
