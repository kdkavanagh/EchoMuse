//! `WS /api/events`: the dashboard's live updates (§3.4).
//!
//! A signed-in client connects (with `?token=`, as the dashboard does, or another way
//! [`crate::auth`] accepts) and first receives a `snapshot` of every device, the same
//! objects `GET /api/devices` returns to that user, then every [`ApiEvent`] published
//! through an [`EventPublisher`] after it subscribed. The subscription starts before the
//! snapshot is taken, so nothing published in between is lost; a change may arrive
//! twice, which the dashboard's merges tolerate.
//!
//! Each event is serialized once and shared by every socket. The hub keeps at most
//! [`EventsConfig::buffer`] unsent events per socket; publishing never waits for a
//! client. A client that falls further behind, or whose socket does not accept a message
//! within [`EventsConfig::write_timeout`], is disconnected, and the dashboard reconnects
//! and starts again from a fresh snapshot. A socket also closes when its session expires,
//! is signed out, or ends because its user changed their password, and when the server
//! shuts down.

use std::{collections::BTreeMap, num::NonZeroUsize, time::Duration};

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{
            CloseFrame, Message, Utf8Bytes, WebSocket, close_code,
            rejection::WebSocketUpgradeRejection,
        },
    },
    response::Response,
    routing::get,
};
use echomuse_core::types::DeviceId;
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

use crate::{
    auth::{AuthenticatedUser, UserId, token::SessionToken},
    clock::unix_now,
    devices,
    error::{ApiError, ErrorCode},
    state::ApiState,
};

/// Revocation notices kept for sockets that have not looked yet. A socket that misses
/// one cannot tell whether it was meant, so it closes.
const REVOCATION_BUFFER: usize = 64;

/// One device as the dashboard's device list holds it: the object `GET /api/devices`
/// returns for it (see [`crate::devices`]), which always includes `device_id`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DeviceView(Map<String, Value>);

impl DeviceView {
    /// The object the devices routes composed.
    pub(crate) fn from_fields(fields: Map<String, Value>) -> Self {
        debug_assert!(fields.get("device_id").is_some_and(Value::is_string));
        Self(fields)
    }

    /// The device's fields.
    pub fn fields(&self) -> &Map<String, Value> {
        &self.0
    }
}

/// Some of a device's fields, which the dashboard merges into the device it holds
/// (`device_update`'s `state`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct DeviceStatePatch(pub Map<String, Value>);

/// One Home Assistant timer as the alert panel lists it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TimerDisplay {
    /// The timer's identity, stable while it runs.
    pub id: String,
    /// Its name; `None` for an unnamed timer, which the dashboard calls "Timer".
    pub name: Option<String>,
    /// Whole seconds left.
    pub remaining_seconds: u64,
    /// Running (`true`) or paused.
    pub active: bool,
}

/// The `kind` and `data` of an `alerts` event.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AlertNotice {
    /// The device's running timers, republished every second while any exists. The
    /// dashboard replaces its list with these.
    Timers {
        /// Every running or paused timer.
        timers: Vec<TimerDisplay>,
    },
    /// Anything else about the device's alerts changed; the dashboard reads them again.
    Changed,
}

/// One entry of `ha_status`'s `features`: whether a Home Assistant provisioning step
/// is ready, and why not.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FeatureStatus {
    /// Ready.
    pub ok: bool,
    /// Why it is not ready; `None` when there is nothing to add.
    pub detail: Option<String>,
}

/// A message on `/api/events`, as dashboard.jsx reads it (its `EVENT_TYPE`).
///
/// `device_log` is not here: the dashboard reads nothing from it but its type, and the
/// controller stores no device log entries yet; `GET /api/devices/{id}/logs` reads them.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApiEvent {
    /// Every device. Sent to each socket when it opens; publishing one replaces every
    /// open dashboard's device list.
    Snapshot {
        /// The devices, as `GET /api/devices` returns them.
        devices: Vec<DeviceView>,
    },
    /// Some of a device's fields changed; the dashboard merges `state` into it.
    DeviceUpdate {
        /// The device.
        device_id: DeviceId,
        /// The changed fields.
        state: DeviceStatePatch,
    },
    /// The device's session came up.
    DeviceConnected {
        /// The device.
        device_id: DeviceId,
    },
    /// The device's session ended; the dashboard clears its activity flags.
    DeviceDisconnected {
        /// The device.
        device_id: DeviceId,
    },
    /// A device is waiting for approval; the dashboard re-reads the fleet.
    DevicePending {
        /// The device.
        device_id: DeviceId,
    },
    /// A device was approved; the dashboard re-reads the fleet.
    DeviceApproved {
        /// The device.
        device_id: DeviceId,
    },
    /// A device was deleted; the dashboard drops it.
    DeviceDeleted {
        /// The device.
        device_id: DeviceId,
    },
    /// A firmware update finished; the dashboard re-reads the fleet.
    DeviceUpdated {
        /// The device.
        device_id: DeviceId,
    },
    /// A firmware update failed; the dashboard re-reads the fleet.
    DeviceUpdateFailed {
        /// The device.
        device_id: DeviceId,
    },
    /// The device went back to its previous firmware on its own; the dashboard re-reads
    /// the fleet.
    DeviceAutoRolledBack {
        /// The device.
        device_id: DeviceId,
    },
    /// The device was rolled back on request; the dashboard re-reads the fleet.
    DeviceRolledBack {
        /// The device.
        device_id: DeviceId,
    },
    /// A queued firmware install was set or cleared.
    DeviceUpdateQueue {
        /// The device.
        device_id: DeviceId,
        /// When it was queued, in whole seconds since the Unix epoch (the device's
        /// `update_queued_at`); `None` when the queue was cleared.
        queued_at: Option<i64>,
    },
    /// A turn was stored, or a near miss's verdict changed; Activity views and the
    /// response-latency panel re-read their lists.
    TurnComplete {
        /// The device the turn was on.
        device_id: DeviceId,
    },
    /// Something about a device's alarms or timers.
    Alerts {
        /// The device.
        device_id: DeviceId,
        /// What changed.
        #[serde(flatten)]
        notice: AlertNotice,
    },
    /// Home Assistant provisioning readiness, by step.
    HaStatus {
        /// Each step's state.
        features: BTreeMap<String, FeatureStatus>,
    },
}

/// Limits of the events hub.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventsConfig {
    /// Events a socket may be behind before it is disconnected.
    pub buffer: NonZeroUsize,
    /// How long one message may take to be accepted by a socket before it is
    /// disconnected.
    pub write_timeout: Duration,
}

impl Default for EventsConfig {
    fn default() -> Self {
        Self {
            buffer: NonZeroUsize::new(256).expect("256 is not zero"),
            write_timeout: Duration::from_secs(10),
        }
    }
}

/// Sessions whose open sockets must close.
#[derive(Clone, Debug)]
pub(crate) enum Revocation {
    /// This session ended.
    Session(SessionToken),
    /// Every session of `user` except `keep` ended.
    OtherSessions { user: UserId, keep: SessionToken },
}

impl Revocation {
    fn ends(&self, user: UserId, token: &SessionToken) -> bool {
        match self {
            Self::Session(ended) => ended.matches(token),
            Self::OtherSessions { user: owner, keep } => *owner == user && !keep.matches(token),
        }
    }
}

/// Publishes events to every open `/api/events` socket. Cheap to clone.
#[derive(Clone, Debug)]
pub struct EventPublisher {
    events: broadcast::Sender<Utf8Bytes>,
}

impl EventPublisher {
    /// Send `event` to every open socket. Never waits: a socket that cannot keep up is
    /// disconnected instead.
    pub fn publish(&self, event: &ApiEvent) {
        match serde_json::to_string(event) {
            // No open socket is not an error.
            Ok(text) => drop(self.events.send(Utf8Bytes::from(text))),
            Err(error) => tracing::error!(%error, "an event could not be serialized"),
        }
    }

    /// Sockets currently open.
    pub fn open_sockets(&self) -> usize {
        self.events.receiver_count()
    }
}

/// The hub inside the server state.
pub(crate) struct EventHub {
    events: broadcast::Sender<Utf8Bytes>,
    revocations: broadcast::Sender<Revocation>,
    write_timeout: Duration,
    shutdown: CancellationToken,
}

impl EventHub {
    pub(crate) fn new(config: EventsConfig, shutdown: CancellationToken) -> Self {
        Self {
            events: broadcast::Sender::new(config.buffer.get()),
            revocations: broadcast::Sender::new(REVOCATION_BUFFER),
            write_timeout: config.write_timeout,
            shutdown,
        }
    }

    pub(crate) fn publisher(&self) -> EventPublisher {
        EventPublisher {
            events: self.events.clone(),
        }
    }

    /// Close the open sockets of sessions that just ended.
    pub(crate) fn revoke(&self, revocation: Revocation) {
        // No open socket is not an error.
        drop(self.revocations.send(revocation));
    }
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new().route("/api/events", get(open))
}

async fn open(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let upgrade = upgrade.map_err(|rejection| {
        ApiError::new(
            rejection.status(),
            ErrorCode::BadRequest,
            "This address takes a WebSocket connection",
        )
    })?;
    Ok(upgrade.on_upgrade(move |socket| serve(state, user, socket)))
}

/// Why the server closed a socket.
#[derive(Clone, Copy, Debug)]
enum Closing {
    ShuttingDown,
    SessionEnded,
    TooSlow,
    NoSnapshot,
}

impl Closing {
    fn frame(self) -> CloseFrame {
        let (code, reason) = match self {
            Self::ShuttingDown => (close_code::AWAY, "the controller is stopping"),
            Self::SessionEnded => (close_code::POLICY, "the session ended"),
            Self::TooSlow => (close_code::AGAIN, "too far behind; reconnect"),
            Self::NoSnapshot => (close_code::ERROR, "the device list is unavailable"),
        };
        CloseFrame {
            code,
            reason: Utf8Bytes::from_static(reason),
        }
    }
}

/// One open events socket and what it listens to.
struct EventSocket {
    sink: SplitSink<WebSocket, Message>,
    incoming: SplitStream<WebSocket>,
    events: broadcast::Receiver<Utf8Bytes>,
    revocations: broadcast::Receiver<Revocation>,
    shutdown: CancellationToken,
    write_timeout: Duration,
}

async fn serve(state: ApiState, user: AuthenticatedUser, socket: WebSocket) {
    let hub = state.events();
    let (sink, incoming) = socket.split();
    // Subscribe before taking the snapshot, so nothing published meanwhile is missed.
    let mut socket = EventSocket {
        sink,
        incoming,
        events: hub.events.subscribe(),
        revocations: hub.revocations.subscribe(),
        shutdown: hub.shutdown.clone(),
        write_timeout: hub.write_timeout,
    };
    if let Some(closing) = socket.run(&state, &user).await {
        socket.send(Message::Close(Some(closing.frame()))).await;
    }
}

impl EventSocket {
    /// Send one message, giving up after the write timeout. False when the socket is
    /// done.
    async fn send(&mut self, message: Message) -> bool {
        let sent = tokio::time::timeout(self.write_timeout, self.sink.send(message)).await;
        matches!(sent, Ok(Ok(())))
    }

    /// Send the snapshot, then events until something ends the socket. Returns why the
    /// server closes it, or `None` when the client already left.
    async fn run(&mut self, state: &ApiState, user: &AuthenticatedUser) -> Option<Closing> {
        let Ok(now) = unix_now(state.clock()) else {
            return Some(Closing::SessionEnded);
        };
        let remaining = u64::try_from(user.expires_at().0.saturating_sub(now.0)).unwrap_or(0);
        let session_end = tokio::time::sleep(Duration::from_secs(remaining));
        tokio::pin!(session_end);

        let devices = tokio::select! {
            _ = self.shutdown.cancelled() => return Some(Closing::ShuttingDown),
            devices = devices::all_views(state, user.role()) => devices,
        };
        let Ok(devices) = devices else {
            return Some(Closing::NoSnapshot);
        };
        let snapshot = match serde_json::to_string(&ApiEvent::Snapshot { devices }) {
            Ok(text) => text,
            Err(error) => {
                tracing::error!(%error, "the device snapshot could not be serialized");
                return Some(Closing::NoSnapshot);
            }
        };
        if !self.send(Message::Text(Utf8Bytes::from(snapshot))).await {
            return None;
        }

        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return Some(Closing::ShuttingDown),
                _ = &mut session_end => return Some(Closing::SessionEnded),
                revocation = self.revocations.recv() => match revocation {
                    Ok(revocation) if revocation.ends(user.user_id(), user.token()) => {
                        return Some(Closing::SessionEnded);
                    }
                    Ok(_) => {}
                    // A missed notice may have been this session's.
                    Err(RecvError::Lagged(_)) => return Some(Closing::SessionEnded),
                    Err(RecvError::Closed) => return Some(Closing::ShuttingDown),
                },
                message = self.incoming.next() => match message {
                    // The dashboard sends nothing; pings are answered by the socket itself.
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
                    Some(Ok(_)) => {}
                },
                event = self.events.recv() => match event {
                    Ok(text) => {
                        if !self.send(Message::Text(text)).await {
                            return None;
                        }
                    }
                    Err(RecvError::Lagged(_)) => return Some(Closing::TooSlow),
                    Err(RecvError::Closed) => return Some(Closing::ShuttingDown),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn device(id: &str) -> DeviceId {
        DeviceId(id.to_owned())
    }

    fn wire(event: &ApiEvent) -> Value {
        serde_json::to_value(event).unwrap()
    }

    #[test]
    fn events_have_the_shapes_the_dashboard_reads() {
        let kitchen = device("G0SYNTHETIC00001");
        let Value::Object(fields) = json!({"device_id": "G0SYNTHETIC00001", "connected": true})
        else {
            unreachable!()
        };
        assert_eq!(
            wire(&ApiEvent::Snapshot {
                devices: vec![DeviceView::from_fields(fields)]
            }),
            json!({"type": "snapshot", "devices": [{"device_id": "G0SYNTHETIC00001", "connected": true}]})
        );
        let Value::Object(state) = json!({"speaking": true}) else {
            unreachable!()
        };
        assert_eq!(
            wire(&ApiEvent::DeviceUpdate {
                device_id: kitchen.clone(),
                state: DeviceStatePatch(state)
            }),
            json!({"type": "device_update", "device_id": "G0SYNTHETIC00001", "state": {"speaking": true}})
        );
        assert_eq!(
            wire(&ApiEvent::DeviceAutoRolledBack {
                device_id: kitchen.clone()
            }),
            json!({"type": "device_auto_rolled_back", "device_id": "G0SYNTHETIC00001"})
        );
        assert_eq!(
            wire(&ApiEvent::DeviceUpdateQueue {
                device_id: kitchen.clone(),
                queued_at: None
            }),
            json!({"type": "device_update_queue", "device_id": "G0SYNTHETIC00001", "queued_at": null})
        );
        assert_eq!(
            wire(&ApiEvent::Alerts {
                device_id: kitchen.clone(),
                notice: AlertNotice::Timers {
                    timers: vec![TimerDisplay {
                        id: "timer-1".to_owned(),
                        name: None,
                        remaining_seconds: 90,
                        active: true,
                    }]
                }
            }),
            json!({"type": "alerts", "device_id": "G0SYNTHETIC00001", "kind": "timers",
                   "data": {"timers": [{"id": "timer-1", "name": null, "remaining_seconds": 90, "active": true}]}})
        );
        assert_eq!(
            wire(&ApiEvent::Alerts {
                device_id: kitchen,
                notice: AlertNotice::Changed
            }),
            json!({"type": "alerts", "device_id": "G0SYNTHETIC00001", "kind": "changed"})
        );
        assert_eq!(
            wire(&ApiEvent::HaStatus {
                features: BTreeMap::from([(
                    "calendar".to_owned(),
                    FeatureStatus {
                        ok: false,
                        detail: Some("not set up".to_owned())
                    }
                )])
            }),
            json!({"type": "ha_status", "features": {"calendar": {"ok": false, "detail": "not set up"}}})
        );
    }

    #[test]
    fn revocations_pick_their_sessions() {
        let mine = SessionToken::from_presented("aaaa").unwrap();
        let other = SessionToken::from_presented("bbbb").unwrap();
        let me = UserId(1);
        assert!(Revocation::Session(mine.clone()).ends(me, &mine));
        assert!(!Revocation::Session(other.clone()).ends(me, &mine));
        let change = Revocation::OtherSessions {
            user: me,
            keep: mine.clone(),
        };
        assert!(!change.ends(me, &mine));
        assert!(change.ends(me, &other));
        assert!(!change.ends(UserId(2), &other));
    }
}
