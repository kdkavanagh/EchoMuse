//! The three paths from a session's sockets to its runtime (spec §5.2):
//!
//! - **Ordered ingress**: one bounded channel per session carrying every
//!   order-sensitive control message and every EMA1 packet, from both sockets, in the
//!   order this process queued them. Socket tasks queue without awaiting, so the
//!   channel's order is the observed enqueue order; the receiver numbers items as it
//!   takes them. Count, byte and residence bounds apply, and an item that cannot be
//!   queued closes the session as overloaded instead of being dropped.
//! - **Urgent**: privacy changes, on a small separate bounded channel, followed by the
//!   session's end. The link revokes the published capture scope before queueing a
//!   mute, and revokes the session's token before the end is visible.
//! - **Telemetry**: replaceable reports (`stats`, `wake.stats`, `ambient_light`,
//!   `volume_state`) coalesce to their latest value; `log` and `ble_adverts` go
//!   through their own bounded queue that drops and counts when full. Neither uses
//!   ingress capacity.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use echomuse_protocol::ema::Packet;
use echomuse_protocol::envelope::{DeviceEnvelope, EnvelopeHeader};
use echomuse_protocol::{MessageType, ProtocolError, ProtocolErrorCode};
use serde_json::{Map, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::time::Instant;

use crate::session::{CloseReason, QueueKind, SessionCore};

/// What the ordered ingress channel carries.
#[derive(Clone, Debug, PartialEq)]
pub enum Ingress {
    /// An order-sensitive control message: `stream.open`, `stream.end`,
    /// `uplink.ended`, wake, render, alert, button, Wi-Fi and `pong` messages, and any
    /// other known device message that is not transport, urgent or telemetry.
    Control(DeviceEnvelope),
    /// One EMA1 packet from the audio socket. Its envelope rules were checked
    /// ([`Packet::parse`]); its stream rules are the runtime's to check against the
    /// session's stream registry, in this channel's order.
    Audio(Packet),
}

/// The position of an item in its session's ingress, assigned as the runtime takes it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IngressOrdinal(pub u64);

/// One item taken from the ordered ingress channel.
#[derive(Clone, Debug)]
pub struct IngressItem {
    /// Its position: 0, 1, 2… per session, without gaps.
    pub ordinal: IngressOrdinal,
    /// When the socket task queued it.
    pub enqueued_at: Instant,
    /// The message or packet.
    pub ingress: Ingress,
}

pub(crate) struct Queued {
    ingress: Ingress,
    enqueued_at: Instant,
    _credit: OwnedSemaphorePermit,
}

/// The socket tasks' side of the ordered ingress channel.
#[derive(Clone)]
pub(crate) struct IngressProducer {
    tx: mpsc::Sender<Queued>,
    credit: Arc<Semaphore>,
}

impl IngressProducer {
    /// Queues one item without waiting. `bytes` is its retained size.
    pub(crate) fn push(&self, ingress: Ingress, bytes: usize) -> Result<(), CloseReason> {
        let slot = match self.tx.try_reserve() {
            Ok(slot) => slot,
            Err(mpsc::error::TrySendError::Full(())) => {
                return Err(CloseReason::Overloaded {
                    queue: QueueKind::Ingress,
                    detail: "every ingress slot is taken".to_owned(),
                });
            }
            Err(mpsc::error::TrySendError::Closed(())) => {
                return Err(CloseReason::ClosedByController);
            }
        };
        let permits = u32::try_from(bytes).unwrap_or(u32::MAX);
        let Ok(credit) = Arc::clone(&self.credit).try_acquire_many_owned(permits) else {
            return Err(CloseReason::Overloaded {
                queue: QueueKind::Ingress,
                detail: format!("the ingress byte budget cannot take {bytes} more bytes"),
            });
        };
        slot.send(Queued {
            ingress,
            enqueued_at: Instant::now(),
            _credit: credit,
        });
        Ok(())
    }
}

/// The runtime's end of a session's ordered ingress channel.
pub struct IngressReceiver {
    rx: mpsc::Receiver<Queued>,
    core: Arc<SessionCore>,
    max_age: Duration,
    next: u64,
}

impl fmt::Debug for IngressReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngressReceiver")
            .field("session_id", &self.core.session_id)
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl IngressReceiver {
    /// The next item in queue order, or `None` once the session has ended. Anything
    /// still queued when the session ends is discarded, so it can never reach a
    /// replacement session. An item that waited longer than the ingress residence limit
    /// ends the session as overloaded.
    pub async fn recv(&mut self) -> Option<IngressItem> {
        let queued = tokio::select! {
            biased;
            _ = self.core.stop.cancelled() => {
                self.rx.close();
                return None;
            }
            queued = self.rx.recv() => queued?,
        };
        let waited = queued.enqueued_at.elapsed();
        if waited > self.max_age {
            self.core.end(CloseReason::Overloaded {
                queue: QueueKind::Ingress,
                detail: format!(
                    "an item waited {} ms, over the {} ms residence limit",
                    waited.as_millis(),
                    self.max_age.as_millis()
                ),
            });
            self.rx.close();
            return None;
        }
        let ordinal = IngressOrdinal(self.next);
        self.next += 1;
        Some(IngressItem {
            ordinal,
            enqueued_at: queued.enqueued_at,
            ingress: queued.ingress,
        })
    }
}

pub(crate) fn ingress_channel(
    core: &Arc<SessionCore>,
    messages: usize,
    bytes: u32,
    max_age: Duration,
) -> (IngressProducer, IngressReceiver) {
    let (tx, rx) = mpsc::channel(messages);
    let producer = IngressProducer {
        tx,
        credit: Arc::new(Semaphore::new(bytes as usize)),
    };
    let receiver = IngressReceiver {
        rx,
        core: Arc::clone(core),
        max_age,
        next: 0,
    };
    (producer, receiver)
}

/// `privacy.changed`, decoded.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivacyChange {
    /// Whether the privacy mute is now on.
    pub muted: bool,
    /// The capture epoch now in force; `None` while muted, or when not reported.
    pub capture_epoch: Option<u64>,
    /// The physical event number, shared with `button.action`; `None` when not
    /// reported. The runtime drops events not above the last one it saw.
    pub physical_seq: Option<u64>,
}

impl PrivacyChange {
    /// Decodes `{"muted": bool, "capture_epoch": "…"|null, "physical_seq": n}`.
    /// `muted` is required.
    pub(crate) fn parse(body: &Map<String, Value>) -> Result<Self, ProtocolError> {
        let malformed = |detail: &str| {
            ProtocolError::new(
                ProtocolErrorCode::MalformedMessage,
                format!("privacy.changed.{detail}"),
            )
        };
        let muted = body
            .get("muted")
            .and_then(Value::as_bool)
            .ok_or_else(|| malformed("muted must be a boolean"))?;
        let capture_epoch = match body.get("capture_epoch") {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(
                text.parse::<u64>()
                    .map_err(|_| malformed("capture_epoch must be a uint64 decimal string"))?,
            ),
            Some(_) => return Err(malformed("capture_epoch must be a string or null")),
        };
        let physical_seq = match body.get("physical_seq") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .ok_or_else(|| malformed("physical_seq must be a non-negative integer"))?,
            ),
        };
        Ok(Self {
            muted,
            capture_epoch,
            physical_seq,
        })
    }
}

/// An event on the urgent path.
#[derive(Clone, Debug, PartialEq)]
pub enum UrgentEvent {
    /// The device reported a privacy mute change. When `muted`, the published capture
    /// scope was already cancelled.
    Privacy {
        /// The message's envelope fields.
        header: EnvelopeHeader,
        /// The change.
        change: PrivacyChange,
    },
    /// The session ended; its token is already cancelled. Always the last event.
    Ended(CloseReason),
}

/// The runtime's end of a session's urgent path. Service it before ingress.
pub struct UrgentReceiver {
    rx: mpsc::Receiver<UrgentEvent>,
    core: Arc<SessionCore>,
    ended: bool,
}

impl fmt::Debug for UrgentReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UrgentReceiver")
            .field("session_id", &self.core.session_id)
            .finish_non_exhaustive()
    }
}

impl UrgentReceiver {
    /// The next urgent event. Events queued before the end come first, then exactly one
    /// [`UrgentEvent::Ended`], then `None`.
    pub async fn recv(&mut self) -> Option<UrgentEvent> {
        if self.ended {
            return None;
        }
        tokio::select! {
            biased;
            event = self.rx.recv() => {
                if let Some(event) = event {
                    return Some(event);
                }
            }
            _ = self.core.stop.cancelled() => {}
        }
        self.ended = true;
        self.rx.close();
        Some(UrgentEvent::Ended(self.core.ended_reason()))
    }
}

pub(crate) fn urgent_channel(
    core: &Arc<SessionCore>,
    capacity: usize,
) -> (mpsc::Sender<UrgentEvent>, UrgentReceiver) {
    let (tx, rx) = mpsc::channel(capacity);
    let receiver = UrgentReceiver {
        rx,
        core: Arc::clone(core),
        ended: false,
    };
    (tx, receiver)
}

/// A telemetry message type.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TelemetryKind {
    /// `stats`, replaceable.
    Stats,
    /// `wake.stats`, replaceable.
    WakeStats,
    /// `ambient_light`, replaceable.
    AmbientLight,
    /// `volume_state`, replaceable.
    VolumeState,
    /// `log`, queued best-effort.
    Log,
    /// `ble_adverts`, queued best-effort.
    BleAdverts,
}

impl TelemetryKind {
    pub(crate) fn of(message_type: MessageType) -> Option<Self> {
        Some(match message_type {
            MessageType::Stats => Self::Stats,
            MessageType::WakeStats => Self::WakeStats,
            MessageType::AmbientLight => Self::AmbientLight,
            MessageType::VolumeState => Self::VolumeState,
            MessageType::Log => Self::Log,
            MessageType::BleAdverts => Self::BleAdverts,
            _ => return None,
        })
    }
}

/// One telemetry message.
#[derive(Clone, Debug, PartialEq)]
pub struct Telemetry {
    /// Its type.
    pub kind: TelemetryKind,
    /// The message; its body is decoded by the module that owns it.
    pub envelope: DeviceEnvelope,
    /// For `log` and `ble_adverts`: how many messages of either kind were dropped
    /// because the queue was full since the previous delivered one. Replaceable kinds
    /// coalesce instead and always read 0.
    pub dropped_before: u64,
}

/// The control task's side of the telemetry paths.
pub(crate) struct TelemetryProducer {
    stats: watch::Sender<Option<Telemetry>>,
    wake_stats: watch::Sender<Option<Telemetry>>,
    ambient_light: watch::Sender<Option<Telemetry>>,
    volume_state: watch::Sender<Option<Telemetry>>,
    queued: mpsc::Sender<Telemetry>,
    dropped: u64,
}

impl TelemetryProducer {
    pub(crate) fn push(&mut self, kind: TelemetryKind, envelope: DeviceEnvelope) {
        let mut telemetry = Telemetry {
            kind,
            envelope,
            dropped_before: 0,
        };
        let slot = match kind {
            TelemetryKind::Stats => &self.stats,
            TelemetryKind::WakeStats => &self.wake_stats,
            TelemetryKind::AmbientLight => &self.ambient_light,
            TelemetryKind::VolumeState => &self.volume_state,
            TelemetryKind::Log | TelemetryKind::BleAdverts => {
                telemetry.dropped_before = self.dropped;
                match self.queued.try_send(telemetry) {
                    Ok(()) => self.dropped = 0,
                    Err(_) => self.dropped += 1,
                }
                return;
            }
        };
        slot.send_replace(Some(telemetry));
    }
}

/// The runtime's end of a session's telemetry.
pub struct TelemetryReceiver {
    stats: watch::Receiver<Option<Telemetry>>,
    wake_stats: watch::Receiver<Option<Telemetry>>,
    ambient_light: watch::Receiver<Option<Telemetry>>,
    volume_state: watch::Receiver<Option<Telemetry>>,
    queued: mpsc::Receiver<Telemetry>,
    core: Arc<SessionCore>,
}

impl fmt::Debug for TelemetryReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelemetryReceiver")
            .field("session_id", &self.core.session_id)
            .finish_non_exhaustive()
    }
}

impl TelemetryReceiver {
    /// The next telemetry message: the latest unread value of a replaceable kind, or
    /// the next queued `log`/`ble_adverts`. `None` once the session has ended.
    pub async fn recv(&mut self) -> Option<Telemetry> {
        loop {
            let changed = tokio::select! {
                biased;
                _ = self.core.stop.cancelled() => return None,
                queued = self.queued.recv() => return queued,
                changed = self.stats.changed() => changed.map(|()| TelemetryKind::Stats),
                changed = self.wake_stats.changed() => changed.map(|()| TelemetryKind::WakeStats),
                changed = self.ambient_light.changed() => {
                    changed.map(|()| TelemetryKind::AmbientLight)
                }
                changed = self.volume_state.changed() => {
                    changed.map(|()| TelemetryKind::VolumeState)
                }
            };
            let slot = match changed {
                Ok(TelemetryKind::Stats) => &mut self.stats,
                Ok(TelemetryKind::WakeStats) => &mut self.wake_stats,
                Ok(TelemetryKind::AmbientLight) => &mut self.ambient_light,
                Ok(TelemetryKind::VolumeState) => &mut self.volume_state,
                Ok(TelemetryKind::Log | TelemetryKind::BleAdverts) => {
                    unreachable!("queued kinds have no slot")
                }
                // A slot's sender is dropped only when the control task ends, which
                // the stop token reports.
                Err(_) => {
                    self.core.stop.cancelled().await;
                    return None;
                }
            };
            if let Some(telemetry) = slot.borrow_and_update().clone() {
                return Some(telemetry);
            }
        }
    }
}

pub(crate) fn telemetry_channel(
    core: &Arc<SessionCore>,
    capacity: usize,
) -> (TelemetryProducer, TelemetryReceiver) {
    let (stats, stats_rx) = watch::channel(None);
    let (wake_stats, wake_stats_rx) = watch::channel(None);
    let (ambient_light, ambient_light_rx) = watch::channel(None);
    let (volume_state, volume_state_rx) = watch::channel(None);
    let (queued, queued_rx) = mpsc::channel(capacity);
    let producer = TelemetryProducer {
        stats,
        wake_stats,
        ambient_light,
        volume_state,
        queued,
        dropped: 0,
    };
    let receiver = TelemetryReceiver {
        stats: stats_rx,
        wake_stats: wake_stats_rx,
        ambient_light: ambient_light_rx,
        volume_state: volume_state_rx,
        queued: queued_rx,
        core: Arc::clone(core),
    };
    (producer, receiver)
}
