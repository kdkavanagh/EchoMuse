//! One admitted device session as the runtime sees it: the [`SessionLink`] it sends
//! with, the receivers it reads from, and why the session ended.

use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use echomuse_core::types::{DeviceId, WorkScope};
use echomuse_protocol::ema::{self, HEADER_BYTES, Kind};
use echomuse_protocol::session::ProtocolErrorReport;
use echomuse_protocol::{
    ControlBody, EncodeError, EncodedEnvelope, LinkSessionId, MessageId, ProtocolError,
    encode_envelope,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::auth::Transport;
use crate::commands::{CommandProgress, PendingAck, Registration};
use crate::ingress::{IngressReceiver, TelemetryReceiver, UrgentReceiver};

/// Which socket of a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    /// `/device/v1/control`.
    Control,
    /// `/device/v1/audio`.
    Audio,
}

/// Which bounded queue could not take a required item.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueKind {
    /// The ordered ingress channel (control messages and audio packets).
    Ingress,
    /// The urgent path (privacy changes).
    Urgent,
    /// The control socket's writer queue.
    ControlEgress,
    /// The acknowledgement waiter registrations.
    AckRegistrations,
}

impl fmt::Display for QueueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ingress => "ingress",
            Self::Urgent => "urgent",
            Self::ControlEgress => "control egress",
            Self::AckRegistrations => "ack registrations",
        })
    }
}

/// Why a session ended. The first reason wins; later ones are ignored.
#[derive(Clone, Debug, PartialEq)]
pub enum CloseReason {
    /// A socket closed, failed, or stalled past the write deadline.
    SocketClosed {
        /// Which socket.
        socket: SocketKind,
        /// What happened, for the log.
        detail: String,
    },
    /// The audio socket did not join before the deadline.
    AudioNotJoined,
    /// Three seconds without a control message.
    HeartbeatLost,
    /// The device broke the wire contract; the controller sent `protocol.error` with
    /// this body before closing.
    ProtocolViolation(ProtocolError),
    /// The device sent `protocol.error` and is closing.
    DeviceProtocolError(ProtocolErrorReport),
    /// A newer hello from the same device replaced this session.
    Replaced,
    /// A required item could not be queued, or waited past its residence limit. This is
    /// the controller's overload diagnostic; nothing about it is sent to the device.
    Overloaded {
        /// The queue.
        queue: QueueKind,
        /// What overflowed, for the log.
        detail: String,
    },
    /// The runtime closed the session, or dropped what it needs to run it (its
    /// [`SessionLink`] clones, its ingress or urgent receiver).
    ClosedByController,
    /// The integrator's admission declined the session; `session.ready` was not sent.
    Declined(String),
    /// The link server is shutting down.
    Shutdown,
    /// One of the session's tasks panicked.
    TaskPanicked(&'static str),
}

impl fmt::Display for CloseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketClosed { socket, detail } => write!(f, "{socket:?} socket: {detail}"),
            Self::AudioNotJoined => f.write_str("the audio socket never joined"),
            Self::HeartbeatLost => f.write_str("no control message for the loss deadline"),
            Self::ProtocolViolation(error) => write!(f, "protocol violation: {error}"),
            Self::DeviceProtocolError(report) => {
                write!(
                    f,
                    "device protocol.error {:?}: {}",
                    report.code, report.detail
                )
            }
            Self::Replaced => f.write_str("replaced by a newer session"),
            Self::Overloaded { queue, detail } => write!(f, "overloaded {queue} queue: {detail}"),
            Self::ClosedByController => f.write_str("closed by the controller"),
            Self::Declined(detail) => write!(f, "admission declined: {detail}"),
            Self::Shutdown => f.write_str("link server shut down"),
            Self::TaskPanicked(task) => write!(f, "the {task} task panicked"),
        }
    }
}

/// The state shared by a session's tasks and handles. Everything here is either
/// immutable, set once (`end`), or a synchronised slot (`capture`).
pub(crate) struct SessionCore {
    pub(crate) device: DeviceId,
    pub(crate) session_id: LinkSessionId,
    pub(crate) transport: Transport,
    pub(crate) stop: CancellationToken,
    end: OnceLock<CloseReason>,
    capture: watch::Sender<Option<WorkScope>>,
}

impl fmt::Debug for SessionCore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCore")
            .field("device", &self.device)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl SessionCore {
    pub(crate) fn new(
        device: DeviceId,
        transport: Transport,
        stop: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            device,
            session_id: LinkSessionId::generate(),
            transport,
            stop,
            end: OnceLock::new(),
            capture: watch::Sender::new(None),
        })
    }

    /// Ends the session: records the first reason, stops every task and handle, and
    /// revokes the published capture scope.
    pub(crate) fn end(&self, reason: CloseReason) {
        if self.end.set(reason.clone()).is_ok() {
            match &reason {
                CloseReason::Overloaded { .. } | CloseReason::TaskPanicked(_) => {
                    tracing::warn!(device = %self.device, session = %self.session_id, %reason, "device session ended");
                }
                _ => {
                    tracing::info!(device = %self.device, session = %self.session_id, %reason, "device session ended");
                }
            }
        }
        self.stop.cancel();
        self.revoke_capture();
    }

    /// The reason, once the session has ended. A session stopped by the server's
    /// shutdown token before any reason was recorded ended for [`CloseReason::Shutdown`].
    pub(crate) fn reason(&self) -> Option<CloseReason> {
        match self.end.get() {
            Some(reason) => Some(reason.clone()),
            None if self.stop.is_cancelled() => Some(CloseReason::Shutdown),
            None => None,
        }
    }

    pub(crate) fn ended_reason(&self) -> CloseReason {
        self.reason().unwrap_or(CloseReason::Shutdown)
    }

    /// `body` in an envelope carrying this session's id and device.
    pub(crate) fn encode<B: ControlBody>(
        &self,
        body: &B,
        generation: u32,
    ) -> Result<EncodedEnvelope, EncodeError> {
        encode_envelope(body, Some(&self.session_id), &self.device, generation)
    }

    pub(crate) fn revoke_capture(&self) {
        if let Some(scope) = self.capture.borrow().as_ref() {
            scope.cancel();
        }
    }

    fn publish_capture(&self, scope: WorkScope) {
        self.capture.send_replace(Some(scope));
        // Either `end` saw the new scope and cancelled it, or this sees the stop.
        if self.stop.is_cancelled() {
            self.revoke_capture();
        }
    }
}

/// One controller heartbeat sample: the device's monotonic clock and when it arrived.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeartbeatSample {
    /// The device's `CLOCK_MONOTONIC` in ns, from `heartbeat.mono_ns`.
    pub device_mono_ns: u64,
    /// When the controller read it, on Tokio's monotonic clock.
    pub received_at: Instant,
}

/// What the link knows about a live session's sockets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinkStatus {
    /// Two seconds without a control message (dashboard only).
    pub degraded: bool,
    /// The latest device heartbeat; `None` before the first.
    pub last_heartbeat: Option<HeartbeatSample>,
    /// Whether the audio socket has joined.
    pub audio_joined: bool,
}

/// Why a send did not queue its frame.
#[derive(Debug)]
pub enum SendError {
    /// The session has ended; nothing more is sent on it.
    SessionEnded(CloseReason),
    /// The control writer queue was full. A required control message is never dropped
    /// silently, so the session was closed as overloaded.
    Overloaded,
    /// The body could not be encoded; the session is unaffected.
    Encode(EncodeError),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionEnded(reason) => write!(f, "session ended: {reason}"),
            Self::Overloaded => f.write_str("control egress full; session closed"),
            Self::Encode(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SendError {}

/// Why a render frame was not queued.
#[derive(Debug)]
pub enum RenderError {
    /// The render queue is full; the frame is handed back. Only
    /// [`SessionLink::try_send_render`] returns this.
    Full(Bytes),
    /// The bytes are not one whole EMA1 kind-3 frame.
    NotRender,
    /// The playback's token was cancelled.
    PlaybackCancelled,
    /// The session has ended.
    SessionEnded(CloseReason),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("render queue full"),
            Self::NotRender => f.write_str("not a whole EMA1 render frame"),
            Self::PlaybackCancelled => f.write_str("playback cancelled"),
            Self::SessionEnded(reason) => write!(f, "session ended: {reason}"),
        }
    }
}

impl std::error::Error for RenderError {}

/// One frame for the control writer.
pub(crate) struct ControlOut {
    pub(crate) text: String,
    pub(crate) progress: Option<Arc<watch::Sender<CommandProgress>>>,
    pub(crate) _credit: OwnedSemaphorePermit,
}

/// The sending side of the control writer queue, bounded by count and bytes.
#[derive(Clone)]
pub(crate) struct ControlEgress {
    tx: mpsc::Sender<ControlOut>,
    credit: Arc<Semaphore>,
}

impl ControlEgress {
    pub(crate) fn new(messages: usize, bytes: u32) -> (Self, mpsc::Receiver<ControlOut>) {
        let (tx, rx) = mpsc::channel(messages);
        let credit = Arc::new(Semaphore::new(bytes as usize));
        (Self { tx, credit }, rx)
    }

    /// Queues one required frame without waiting. A full queue closes the session.
    pub(crate) fn push(
        &self,
        core: &SessionCore,
        text: String,
        progress: Option<Arc<watch::Sender<CommandProgress>>>,
    ) -> Result<(), SendError> {
        if let Some(reason) = core.reason() {
            return Err(SendError::SessionEnded(reason));
        }
        let slot = match self.tx.try_reserve() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err(overload(core, "every control writer slot is taken"));
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(SendError::SessionEnded(core.ended_reason()));
            }
        };
        // Control frames are at most 256 KiB, so the length fits.
        let bytes = u32::try_from(text.len()).unwrap_or(u32::MAX);
        let Ok(credit) = Arc::clone(&self.credit).try_acquire_many_owned(bytes) else {
            return Err(overload(core, "the control writer byte budget is spent"));
        };
        slot.send(ControlOut {
            text,
            progress,
            _credit: credit,
        });
        Ok(())
    }
}

fn overload(core: &SessionCore, detail: &str) -> SendError {
    core.end(CloseReason::Overloaded {
        queue: QueueKind::ControlEgress,
        detail: detail.to_owned(),
    });
    SendError::Overloaded
}

/// One render frame for the audio writer.
pub(crate) struct RenderOut {
    pub(crate) frame: Bytes,
    pub(crate) playback: CancellationToken,
    pub(crate) _credit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct RenderEgress {
    tx: mpsc::Sender<RenderOut>,
    credit: Arc<Semaphore>,
}

impl RenderEgress {
    pub(crate) fn new(messages: usize, bytes: u32) -> (Self, mpsc::Receiver<RenderOut>) {
        let (tx, rx) = mpsc::channel(messages);
        let credit = Arc::new(Semaphore::new(bytes as usize));
        (Self { tx, credit }, rx)
    }
}

/// The runtime's handle on one session: the typed send API, render frames, capture
/// revocation and the session's end. Cheap to clone. Dropping every clone ends the
/// session with [`CloseReason::ClosedByController`].
#[derive(Clone)]
pub struct SessionLink {
    core: Arc<SessionCore>,
    control: ControlEgress,
    registrations: mpsc::Sender<Registration>,
    render: RenderEgress,
    status: watch::Receiver<LinkStatus>,
}

impl fmt::Debug for SessionLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionLink")
            .field("device", &self.core.device)
            .field("session_id", &self.core.session_id)
            .field("transport", &self.core.transport)
            .finish_non_exhaustive()
    }
}

impl SessionLink {
    pub(crate) fn new(
        core: Arc<SessionCore>,
        control: ControlEgress,
        registrations: mpsc::Sender<Registration>,
        render: RenderEgress,
        status: watch::Receiver<LinkStatus>,
    ) -> Self {
        Self {
            core,
            control,
            registrations,
            render,
            status,
        }
    }

    /// The device.
    pub fn device(&self) -> &DeviceId {
        &self.core.device
    }

    /// The `session_id` sent in `session.ready`.
    pub fn session_id(&self) -> &LinkSessionId {
        &self.core.session_id
    }

    /// Which listener the session's sockets use.
    pub fn transport(&self) -> Transport {
        self.core.transport
    }

    /// Queues one controller message, wrapped in an envelope carrying this session's
    /// id and `generation`, and returns its `message_id`. The frame is required: if the
    /// control writer queue is full the session is closed as overloaded rather than the
    /// message dropped. Never waits.
    pub fn send<B: ControlBody>(&self, body: &B, generation: u32) -> Result<MessageId, SendError> {
        let encoded = self
            .core
            .encode(body, generation)
            .map_err(SendError::Encode)?;
        self.control.push(&self.core, encoded.text, None)?;
        Ok(encoded.message_id)
    }

    /// Like [`send`](Self::send), for a message the device acknowledges with
    /// `command.ack`. The returned [`PendingAck`] follows the command from enqueued to
    /// written to acknowledged, or to failed: no acknowledgement within `timeout` of
    /// this call, or the session ended first. Never waits.
    pub fn request<B: ControlBody>(
        &self,
        body: &B,
        generation: u32,
        timeout: Duration,
    ) -> Result<PendingAck, SendError> {
        let encoded = self
            .core
            .encode(body, generation)
            .map_err(SendError::Encode)?;
        if let Some(reason) = self.core.reason() {
            return Err(SendError::SessionEnded(reason));
        }
        let (progress, watcher) = watch::channel(CommandProgress::Enqueued);
        let progress = Arc::new(progress);
        let registration = Registration {
            message_id: encoded.message_id.clone(),
            progress: Arc::clone(&progress),
            deadline: Instant::now() + timeout,
        };
        // The waiter is registered before the frame can be written, so the device's
        // acknowledgement always finds it.
        match self.registrations.try_send(registration) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.core.end(CloseReason::Overloaded {
                    queue: QueueKind::AckRegistrations,
                    detail: "every acknowledgement registration slot is taken".to_owned(),
                });
                return Err(SendError::Overloaded);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                return Err(SendError::SessionEnded(self.core.ended_reason()));
            }
        }
        self.control
            .push(&self.core, encoded.text, Some(progress))?;
        Ok(PendingAck::new(
            encoded.message_id,
            watcher,
            Arc::clone(&self.core),
        ))
    }

    /// Queues one render frame (EMA1 kind 3, as [`ema::encode_render`] produces) for
    /// the audio socket without waiting. Frames whose `playback` token is cancelled
    /// before they are written are discarded, which purges a cancelled playback's
    /// queued packets; a frame already being written may still arrive.
    pub fn try_send_render(
        &self,
        frame: Bytes,
        playback: &CancellationToken,
    ) -> Result<(), RenderError> {
        self.check_render(&frame, playback)?;
        let slot = match self.render.tx.try_reserve() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(())) => return Err(RenderError::Full(frame)),
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(RenderError::SessionEnded(self.core.ended_reason()));
            }
        };
        let bytes = u32::try_from(frame.len()).unwrap_or(u32::MAX);
        let Ok(credit) = Arc::clone(&self.render.credit).try_acquire_many_owned(bytes) else {
            return Err(RenderError::Full(frame));
        };
        slot.send(RenderOut {
            frame,
            playback: playback.clone(),
            _credit: credit,
        });
        Ok(())
    }

    /// Queues one render frame, waiting for room. Gives up when the playback is
    /// cancelled or the session ends.
    pub async fn send_render(
        &self,
        frame: Bytes,
        playback: &CancellationToken,
    ) -> Result<(), RenderError> {
        self.check_render(&frame, playback)?;
        let bytes = u32::try_from(frame.len()).unwrap_or(u32::MAX);
        let ended = || RenderError::SessionEnded(self.core.ended_reason());
        let slot = tokio::select! {
            biased;
            _ = self.core.stop.cancelled() => return Err(ended()),
            _ = playback.cancelled() => return Err(RenderError::PlaybackCancelled),
            slot = self.render.tx.reserve() => slot.map_err(|_| ended())?,
        };
        let credit = tokio::select! {
            biased;
            _ = self.core.stop.cancelled() => return Err(ended()),
            _ = playback.cancelled() => return Err(RenderError::PlaybackCancelled),
            credit = Arc::clone(&self.render.credit).acquire_many_owned(bytes) => {
                credit.map_err(|_| ended())?
            }
        };
        slot.send(RenderOut {
            frame,
            playback: playback.clone(),
            _credit: credit,
        });
        Ok(())
    }

    fn check_render(&self, frame: &Bytes, playback: &CancellationToken) -> Result<(), RenderError> {
        if let Some(reason) = self.core.reason() {
            return Err(RenderError::SessionEnded(reason));
        }
        if playback.is_cancelled() {
            return Err(RenderError::PlaybackCancelled);
        }
        let header = ema::decode_header(frame).map_err(|_| RenderError::NotRender)?;
        let whole = frame.len() as u64 == HEADER_BYTES as u64 + header.payload_bytes();
        if header.kind != Kind::Render || !whole {
            return Err(RenderError::NotRender);
        }
        Ok(())
    }

    /// Publishes the scope of the current capture permission. When the device reports
    /// the privacy mute on, the link cancels this scope before it queues the urgent
    /// event, and it cancels it when the session ends. Only the runtime publishes a new
    /// scope, so unmuting never revives an old one.
    pub fn publish_capture_scope(&self, scope: WorkScope) {
        self.core.publish_capture(scope);
    }

    /// The link's current view of the sockets.
    pub fn status(&self) -> LinkStatus {
        *self.status.borrow()
    }

    /// A receiver that sees every status change.
    pub fn watch_status(&self) -> watch::Receiver<LinkStatus> {
        self.status.clone()
    }

    /// A token cancelled when the session ends. It is a child of the session's own,
    /// so cancelling it does not end the session.
    pub fn cancellation(&self) -> CancellationToken {
        self.core.stop.child_token()
    }

    /// Why the session ended; `None` while it is live.
    pub fn end_reason(&self) -> Option<CloseReason> {
        self.core.reason()
    }

    /// Waits for the session to end and returns why.
    pub async fn ended(&self) -> CloseReason {
        self.core.stop.cancelled().await;
        self.core.ended_reason()
    }

    /// Ends the session: its sockets close and every waiter fails.
    pub fn close(&self) {
        self.core.end(CloseReason::ClosedByController);
    }

    /// Reports a violation the runtime found, such as a stream registry error: the
    /// link sends `protocol.error` with `error` as the last control frame and closes
    /// the session.
    pub fn protocol_violation(&self, error: ProtocolError) {
        self.core.end(CloseReason::ProtocolViolation(error));
    }
}

/// Everything an admitted session hands the runtime. The link fills the receivers
/// from the session's sockets until the session ends; each then reports the end.
#[derive(Debug)]
pub struct LinkSession {
    /// The send side.
    pub link: SessionLink,
    /// Order-sensitive control messages and audio packets, in the order they were
    /// queued.
    pub ingress: IngressReceiver,
    /// Privacy changes, and the session's end.
    pub urgent: UrgentReceiver,
    /// Replaceable and best-effort telemetry.
    pub telemetry: TelemetryReceiver,
}
