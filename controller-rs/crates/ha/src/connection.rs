//! The one WebSocket connection to Home Assistant: authentication, request routing by
//! message id, and the task that keeps it connected.
//!
//! Each authenticated connection is a [`Link`] with its own generation. Requests register
//! a route under their message id before the request is written, so a reply can never
//! arrive before its route exists. Writes go through one async mutex, which assigns the
//! id and writes the frame in the same critical section: HA refuses an id that is not
//! larger than the previous one on the connection. The reading task never waits on a
//! consumer; it hands each message to its route without blocking.
//!
//! When a connection ends, its link is marked lost and every route on it is failed at
//! once. HA forgets all subscriptions and runs of a closed connection, so nothing is
//! carried over to the next link.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use echomuse_core::{
    services::{HaConnectionGeneration, HaError, NotDispatched},
    types::WorkScope,
};
use futures_util::{SinkExt, StreamExt, stream::SplitSink, stream::SplitStream};
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
    time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

use crate::{client::Shared, pipeline::IntentRoute};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type SocketWriter = SplitSink<Socket, Message>;
type SocketReader = SplitStream<Socket>;

/// HA's registry listings grow with the installation and HA puts no limit on what it
/// sends, so the controller accepts large messages. A frame larger than this is treated
/// as a broken connection.
const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

// ── Public status ────────────────────────────────────────────────────────────

/// Why the connection is down or the last attempt failed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionFault {
    /// The WebSocket could not be opened (network, TLS, HTTP upgrade).
    Connect(String),
    /// HA answered `auth_invalid`; its message is kept. The session stops reconnecting.
    AuthRejected(String),
    /// HA did not follow the authentication exchange.
    Handshake(String),
    /// An authenticated connection ended.
    Lost(String),
}

impl fmt::Display for ConnectionFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(detail) => write!(f, "could not connect: {detail}"),
            Self::AuthRejected(detail) => write!(f, "Home Assistant refused the token: {detail}"),
            Self::Handshake(detail) => write!(f, "authentication exchange failed: {detail}"),
            Self::Lost(detail) => write!(f, "connection lost: {detail}"),
        }
    }
}

/// The connection as the rest of the controller sees it, published whole on every change.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HaStatus {
    /// The generation of the live connection; None while disconnected.
    pub generation: Option<HaConnectionGeneration>,
    /// The version HA reported when the latest connection authenticated; None until one has.
    pub ha_version: Option<String>,
    /// Why the latest connection or attempt failed; None after a successful connection.
    pub last_fault: Option<ConnectionFault>,
}

/// How the session task ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEnd {
    /// [`crate::HaClient::stop`] was called.
    Stopped,
    /// HA refused the token. Retrying the same token would only count as further failed
    /// logins, which HA's login ban feature punishes, so the session stops; restart it with
    /// a corrected token.
    AuthRejected(String),
}

/// One event of a `subscribe_events` subscription.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct HaEvent {
    /// HA's event type, for example `automation_reloaded`.
    pub event_type: String,
    /// The event's payload. Its shape belongs to the event type, so the subscriber parses it.
    pub data: Value,
}

// ── Routing ──────────────────────────────────────────────────────────────────

/// HA's answer that it refused a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Refusal {
    pub code: String,
    pub message: String,
}

impl From<Refusal> for HaError {
    fn from(refusal: Refusal) -> Self {
        HaError::Rejected {
            code: refusal.code,
            message: refusal.message,
        }
    }
}

/// The reply to a command.
pub(crate) enum Reply {
    Done(Value),
    Refused(Refusal),
    Lost,
}

/// What a pipeline run's owner receives, in arrival order.
pub(crate) enum RunItem {
    /// HA validated the run and started it.
    Started,
    /// HA refused to start the run.
    Refused(Refusal),
    /// One run event, as received.
    Event(Value),
    /// The connection ended.
    Lost,
}

pub(crate) struct EventRoute {
    pub confirm: Option<oneshot::Sender<Reply>>,
    pub events: mpsc::Sender<HaEvent>,
}

/// Where the messages carrying one id go.
pub(crate) enum Route {
    Reply(Option<oneshot::Sender<Reply>>),
    Run(mpsc::UnboundedSender<RunItem>),
    Intent(Box<IntentRoute>),
    Events(EventRoute),
}

impl Route {
    /// Handles the `result` message; true when the route is finished.
    fn on_result(&mut self, outcome: Result<Value, Refusal>) -> bool {
        match self {
            Self::Reply(sender) => {
                if let Some(sender) = sender.take() {
                    let _ = sender.send(match outcome {
                        Ok(value) => Reply::Done(value),
                        Err(refusal) => Reply::Refused(refusal),
                    });
                }
                true
            }
            Self::Run(items) => {
                let refused = outcome.is_err();
                let item = match outcome {
                    Ok(_) => RunItem::Started,
                    Err(refusal) => RunItem::Refused(refusal),
                };
                items.send(item).is_err() || refused
            }
            Self::Intent(run) => run.on_result(outcome),
            Self::Events(route) => {
                let refused = outcome.is_err();
                if let Some(confirm) = route.confirm.take() {
                    let _ = confirm.send(match outcome {
                        Ok(value) => Reply::Done(value),
                        Err(refusal) => Reply::Refused(refusal),
                    });
                }
                refused
            }
        }
    }

    /// Handles one `event` message; true when the route is finished.
    fn on_event(&mut self, event: Value) -> bool {
        match self {
            Self::Reply(_) => false,
            Self::Run(items) => items.send(RunItem::Event(event)).is_err(),
            Self::Intent(run) => run.on_event(event),
            Self::Events(route) => match serde_json::from_value::<HaEvent>(event) {
                // A subscriber that falls a full channel behind loses the subscription
                // rather than silently missing events in the middle: its receiver ends.
                Ok(event) => route.events.try_send(event).is_err(),
                Err(_) => false,
            },
        }
    }

    /// The connection ended while the route was open.
    fn lose(self) {
        match self {
            Self::Reply(sender) => {
                if let Some(sender) = sender {
                    let _ = sender.send(Reply::Lost);
                }
            }
            Self::Run(items) => {
                let _ = items.send(RunItem::Lost);
            }
            Self::Intent(run) => run.lose(),
            Self::Events(route) => {
                if let Some(confirm) = route.confirm {
                    let _ = confirm.send(Reply::Lost);
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum IncomingKind {
    Result,
    Event,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct WireError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct Incoming {
    id: Option<u64>,
    #[serde(rename = "type")]
    kind: IncomingKind,
    success: Option<bool>,
    result: Option<Value>,
    error: Option<WireError>,
    event: Option<Value>,
}

impl From<Option<WireError>> for Refusal {
    fn from(error: Option<WireError>) -> Self {
        let error = error.unwrap_or(WireError {
            code: None,
            message: None,
        });
        Self {
            // HA's own code for an error it gives no specific code to.
            code: error.code.unwrap_or_else(|| "unknown_error".to_owned()),
            message: error.message.unwrap_or_default(),
        }
    }
}

// ── Link ─────────────────────────────────────────────────────────────────────

struct Writer {
    socket: SocketWriter,
    last_id: u64,
}

/// One authenticated connection.
pub(crate) struct Link {
    pub generation: HaConnectionGeneration,
    writer: tokio::sync::Mutex<Writer>,
    /// None once the link is lost: nothing more can be registered on it.
    routes: std::sync::Mutex<Option<HashMap<u64, Route>>>,
    lost: CancellationToken,
}

impl Link {
    fn new(generation: HaConnectionGeneration, socket: SocketWriter) -> Self {
        Self {
            generation,
            writer: tokio::sync::Mutex::new(Writer { socket, last_id: 0 }),
            routes: std::sync::Mutex::new(Some(HashMap::new())),
            lost: CancellationToken::new(),
        }
    }

    pub fn is_lost(&self) -> bool {
        self.lost.is_cancelled()
    }

    /// Writes one command with the next message id after registering `route` under it.
    /// `scope` is checked inside the write lock, immediately before the write; a cancelled
    /// scope sends nothing. A failed write sends no complete frame, so it also counts as
    /// not dispatched, and it ends the link.
    ///
    /// Dropping the returned future while it writes may still let the frame go out.
    pub async fn dispatch(
        &self,
        scope: Option<&WorkScope>,
        mut body: Map<String, Value>,
        route: Route,
    ) -> Result<u64, NotDispatched> {
        let mut writer = self.writer.lock().await;
        if scope.is_some_and(WorkScope::is_cancelled) {
            return Err(NotDispatched::Cancelled);
        }
        let id = writer.last_id + 1;
        body.insert("id".to_owned(), Value::from(id));
        let text = Value::Object(body).to_string();
        match self.lock_routes().as_mut() {
            Some(routes) => routes.insert(id, route),
            None => return Err(NotDispatched::Unavailable),
        };
        writer.last_id = id;
        if writer.socket.send(Message::text(text)).await.is_err() {
            self.remove(id);
            self.lost.cancel();
            return Err(NotDispatched::Unavailable);
        }
        Ok(id)
    }

    /// Writes one binary frame. Fails when the link is lost or the write fails.
    pub async fn send_binary(&self, frame: Vec<u8>) -> Result<(), ()> {
        let mut writer = self.writer.lock().await;
        if self.is_lost() {
            return Err(());
        }
        if writer.socket.send(Message::binary(frame)).await.is_err() {
            self.lost.cancel();
            return Err(());
        }
        Ok(())
    }

    /// Drops the route under `id`; later messages with that id are ignored.
    pub fn remove(&self, id: u64) {
        if let Some(routes) = self.lock_routes().as_mut() {
            routes.remove(&id);
        }
    }

    fn lock_routes(&self) -> std::sync::MutexGuard<'_, Option<HashMap<u64, Route>>> {
        // A poisoned route table means a routing panic; §5.5 stops the process for that.
        self.routes.lock().expect("HA route table poisoned")
    }

    fn deliver(&self, text: &str) {
        let Ok(message) = serde_json::from_str::<Incoming>(text) else {
            return;
        };
        let Some(id) = message.id else {
            return;
        };
        let mut routes = self.lock_routes();
        let Some(routes) = routes.as_mut() else {
            return;
        };
        let Some(route) = routes.get_mut(&id) else {
            return;
        };
        let finished = match message.kind {
            IncomingKind::Result => route.on_result(if message.success == Some(true) {
                Ok(message.result.unwrap_or(Value::Null))
            } else {
                Err(Refusal::from(message.error))
            }),
            IncomingKind::Event => match message.event {
                Some(event) => route.on_event(event),
                None => false,
            },
            IncomingKind::Other => false,
        };
        if finished {
            routes.remove(&id);
        }
    }

    /// Marks the link lost and fails every open route.
    fn shut(&self) {
        self.lost.cancel();
        let routes = self.lock_routes().take();
        for (_, route) in routes.into_iter().flatten() {
            route.lose();
        }
    }
}

/// Removes a route when the request that owns it finishes or is dropped.
pub(crate) struct RouteGuard<'a> {
    link: &'a Link,
    id: u64,
}

impl<'a> RouteGuard<'a> {
    pub fn new(link: &'a Link, id: u64) -> Self {
        Self { link, id }
    }

    /// Leaves the route registered after this guard goes away.
    pub fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for RouteGuard<'_> {
    fn drop(&mut self) {
        self.link.remove(self.id);
    }
}

// ── Session task ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AuthKind {
    AuthRequired,
    AuthOk,
    AuthInvalid,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    kind: AuthKind,
    ha_version: Option<String>,
    message: Option<String>,
}

enum Attempt {
    Connected {
        writer: SocketWriter,
        reader: SocketReader,
        ha_version: Option<String>,
    },
    Failed(ConnectionFault),
}

/// Keeps one authenticated connection open until stopped or refused.
pub(crate) async fn run_session(
    shared: Arc<Shared>,
    links: watch::Sender<Option<Arc<Link>>>,
) -> SessionEnd {
    let options = &shared.options;
    let mut delay = options.reconnect_initial;
    let mut generation = 0;
    loop {
        let attempt = tokio::select! {
            () = shared.stop.cancelled() => return SessionEnd::Stopped,
            attempt = connect(&shared) => attempt,
        };
        match attempt {
            Attempt::Failed(fault) => {
                let refused = match &fault {
                    ConnectionFault::AuthRejected(message) => Some(message.clone()),
                    _ => None,
                };
                shared
                    .status
                    .send_modify(|status| status.last_fault = Some(fault));
                if let Some(message) = refused {
                    return SessionEnd::AuthRejected(message);
                }
            }
            Attempt::Connected {
                writer,
                reader,
                ha_version,
            } => {
                generation += 1;
                delay = options.reconnect_initial;
                let link = Arc::new(Link::new(HaConnectionGeneration(generation), writer));
                // The link is published before the status, so anyone who sees the new
                // generation in the status can already send on it.
                links.send_replace(Some(link.clone()));
                shared.status.send_replace(HaStatus {
                    generation: Some(link.generation),
                    ha_version,
                    last_fault: None,
                });
                let fault = read_until_lost(&shared, &link, reader).await;
                links.send_replace(None);
                link.shut();
                shared.status.send_modify(|status| {
                    status.generation = None;
                    status.last_fault = fault.clone();
                });
                close_politely(&link, options.command_timeout).await;
                if fault.is_none() {
                    return SessionEnd::Stopped;
                }
            }
        }
        // Backoff follows the spec's restart rule (§5.7): 1 s doubling to 30 s by default,
        // back to the start after a successful connection.
        tokio::select! {
            () = shared.stop.cancelled() => return SessionEnd::Stopped,
            () = tokio::time::sleep(delay) => {}
        }
        delay = (delay * 2).min(options.reconnect_max);
    }
}

async fn connect(shared: &Shared) -> Attempt {
    let timeout = shared.options.command_timeout;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_MESSAGE_BYTES));
    let connector = if shared.endpoint.websocket_url().starts_with("wss://") {
        Connector::Rustls(shared.tls.clone())
    } else {
        Connector::Plain
    };
    let opened = tokio::time::timeout(
        timeout,
        connect_async_tls_with_config(
            shared.endpoint.websocket_url(),
            Some(config),
            true,
            Some(connector),
        ),
    )
    .await;
    let socket = match opened {
        Err(_) => return Attempt::Failed(ConnectionFault::Connect("timed out".to_owned())),
        Ok(Err(err)) => return Attempt::Failed(ConnectionFault::Connect(err.to_string())),
        Ok(Ok((socket, _response))) => socket,
    };
    let (mut writer, mut reader) = socket.split();

    match next_auth_message(&mut reader, timeout).await {
        Ok(AuthMessage {
            kind: AuthKind::AuthRequired,
            ..
        }) => {}
        Ok(_) => return handshake_failure("expected auth_required"),
        Err(fault) => return Attempt::Failed(fault),
    }
    let auth = serde_json::json!({"type": "auth", "access_token": shared.endpoint.token()});
    match tokio::time::timeout(timeout, writer.send(Message::text(auth.to_string()))).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return handshake_failure(&err.to_string()),
        Err(_) => return handshake_failure("timed out sending auth"),
    }
    match next_auth_message(&mut reader, timeout).await {
        Ok(AuthMessage {
            kind: AuthKind::AuthOk,
            ha_version,
            ..
        }) => Attempt::Connected {
            writer,
            reader,
            ha_version,
        },
        Ok(AuthMessage {
            kind: AuthKind::AuthInvalid,
            message,
            ..
        }) => Attempt::Failed(ConnectionFault::AuthRejected(message.unwrap_or_default())),
        Ok(_) => handshake_failure("expected auth_ok or auth_invalid"),
        Err(fault) => Attempt::Failed(fault),
    }
}

fn handshake_failure(detail: &str) -> Attempt {
    Attempt::Failed(ConnectionFault::Handshake(detail.to_owned()))
}

async fn next_auth_message(
    reader: &mut SocketReader,
    timeout: Duration,
) -> Result<AuthMessage, ConnectionFault> {
    let deadline = Instant::now() + timeout;
    loop {
        let frame = tokio::time::timeout_at(deadline, reader.next())
            .await
            .map_err(|_| ConnectionFault::Handshake("timed out".to_owned()))?;
        match frame {
            Some(Ok(Message::Text(text))) => {
                return serde_json::from_str(&text).map_err(|err| {
                    ConnectionFault::Handshake(format!("unreadable message: {err}"))
                });
            }
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(_)) => {
                return Err(ConnectionFault::Handshake(
                    "unexpected frame during authentication".to_owned(),
                ));
            }
            Some(Err(err)) => return Err(ConnectionFault::Handshake(err.to_string())),
            None => return Err(ConnectionFault::Handshake("closed".to_owned())),
        }
    }
}

/// Routes incoming messages until the connection ends. None when the session was stopped.
///
/// A WebSocket ping goes out every heartbeat period; when nothing at all has arrived by the
/// next period, the socket is treated as dead, so a silently vanished HA is noticed within
/// two periods.
async fn read_until_lost(
    shared: &Shared,
    link: &Link,
    mut reader: SocketReader,
) -> Option<ConnectionFault> {
    let period = shared.options.heartbeat;
    let mut heartbeat = tokio::time::interval_at(Instant::now() + period, period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ping_outstanding = false;
    loop {
        tokio::select! {
            biased;
            () = shared.stop.cancelled() => return None,
            () = link.lost.cancelled() => {
                return Some(ConnectionFault::Lost("a write failed".to_owned()));
            }
            frame = reader.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    ping_outstanding = false;
                    link.deliver(&text);
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Some(ConnectionFault::Lost("closed by Home Assistant".to_owned()));
                }
                Some(Ok(_)) => ping_outstanding = false,
                Some(Err(err)) => return Some(ConnectionFault::Lost(err.to_string())),
            },
            _ = heartbeat.tick() => {
                if ping_outstanding {
                    return Some(ConnectionFault::Lost("no answer to ping".to_owned()));
                }
                ping_outstanding = true;
                let mut writer = link.writer.lock().await;
                if writer.socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Some(ConnectionFault::Lost("ping write failed".to_owned()));
                }
            }
        }
    }
}

async fn close_politely(link: &Link, timeout: Duration) {
    if let Ok(mut writer) = link.writer.try_lock() {
        let _ = tokio::time::timeout(timeout, writer.socket.close()).await;
    }
}
