//! What both ESPHome nodes share: the listener, one task per Home Assistant
//! connection, and the parts of the protocol that do not depend on the node.
//!
//! Each node runs one actor task that owns everything mutable about it. The
//! actor accepts connections and spawns a connection task for each. A
//! connection task owns its socket: it decodes frames, answers the hello,
//! device-info, entity-list, ping and disconnect messages itself from
//! pre-encoded replies, runs the keepalive, and forwards every other message
//! to the actor. The actor writes to a connection through a bounded queue and
//! never waits on one: a connection whose queue is full is closed.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiMessage, AuthenticationRequest, DeviceInfoRequest, DisconnectRequest, DisconnectResponse,
    GetTimeResponse, HelloRequest, ListEntitiesRequest, PingRequest, PingResponse,
    SubscribeHomeAssistantStatesRequest, SubscribeHomeassistantServicesRequest,
    SubscribeLogsRequest, SubscribeStatesRequest,
};
use crate::frame::{self, BAD_INDICATOR_REPLY, Frame, FrameDecoder, FrameError};

/// How long a client has from connecting to sending its hello.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// Silence after which the node pings the client.
pub(crate) const KEEPALIVE_PING_AFTER: Duration = Duration::from_secs(60);
/// Silence after which the node gives up on the client (2.5 × the ping time).
pub(crate) const KEEPALIVE_CLOSE_AFTER: Duration = Duration::from_secs(150);
/// The longest one socket write may take.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a closing node waits for the client to acknowledge its
/// disconnect request.
const DISCONNECT_GRACE: Duration = Duration::from_secs(1);
/// How long shutdown waits for connection tasks before aborting them.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(3);
/// Connections served at once. Home Assistant needs one; a second covers its
/// reconnect while a dead connection has not timed out yet, and the rest
/// leave room for a diagnostic client.
const MAX_CONNECTIONS: usize = 4;
/// Frames queued for one connection before it counts as too slow.
const OUTBOUND_CAPACITY: usize = 512;
/// Messages queued from all connections to the actor.
const INBOUND_CAPACITY: usize = 256;

/// Why a Home Assistant connection ended.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CloseReason {
    /// The client sent a disconnect request.
    ClientDisconnected,
    /// The socket closed or failed.
    ConnectionLost,
    /// The first byte of a frame was not the plaintext indicator, as when an
    /// encrypting (Noise) client connects.
    BadIndicator,
    /// A frame declared a payload over 65,535 bytes.
    FrameTooLarge,
    /// A frame header or message did not decode.
    Malformed,
    /// The client sent something other than hello, ping or disconnect first.
    MessageBeforeHello,
    /// No hello within the handshake time.
    HandshakeTimeout,
    /// The client stopped answering pings.
    KeepaliveTimeout,
    /// The client did not read fast enough and its queue filled.
    SlowClient,
    /// A socket write did not finish in time.
    WriteTimeout,
    /// The node already had its maximum number of connections.
    TooManyConnections,
    /// The device runtime was not taking requests and its queue filled.
    RequestsBacklogged,
    /// The node is shutting down.
    ServerShutdown,
}

/// A snapshot of a node's connections, published whenever it changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ServerStatus {
    /// Open connections that have completed their hello or are still
    /// handshaking.
    pub connections: usize,
    /// Connections subscribed to entity states.
    pub state_subscribers: usize,
    /// Whether a client holds the node's main subscription: the voice
    /// assistant on a voice satellite, raw advertisements on a Bluetooth
    /// proxy.
    pub subscribed: bool,
    /// Why the most recent connection ended; `None` before any has.
    pub last_close: Option<CloseReason>,
}

/// The node could not be started.
#[derive(Debug, Error)]
pub enum StartError {
    /// The port could not be bound.
    #[error("binding {address}: {source}")]
    Bind {
        /// The address that was asked for.
        address: SocketAddr,
        /// The operating system's error.
        source: io::Error,
    },
    /// A reply the node sends unchanged on every connection is too large for
    /// one frame (an absurdly long label, for example).
    #[error(transparent)]
    ReplyTooLarge(#[from] frame::FrameTooLarge),
}

/// The node is no longer running.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the ESPHome node has stopped")]
pub struct ServerStopped;

/// Identifies one connection within one node.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ConnId(u64);

/// What the actor sends a connection task.
#[derive(Debug)]
pub(crate) enum Outbound {
    Frame(Bytes),
    Close(CloseReason),
}

/// What connection tasks tell the actor.
#[derive(Debug)]
enum Inbound {
    Message(ConnId, Frame),
    Closed(ConnId, CloseReason),
}

/// What [`Core::next`] hands the node.
#[derive(Debug)]
pub(crate) enum CoreEvent {
    /// A node-specific message from a client past its hello.
    Message(ConnId, Frame),
    /// A client subscribed to states; the node sends it every current state.
    SubscribedStates(ConnId),
    /// A connection ended (its reason is in the status). The core has
    /// already forgotten it.
    Closed(ConnId),
}

/// The replies every connection gets unchanged, encoded once.
#[derive(Debug)]
pub(crate) struct FixedReplies {
    pub(crate) hello: Bytes,
    pub(crate) device_info: Bytes,
    /// Every entity description followed by the end-of-list message.
    pub(crate) entities: Vec<Bytes>,
}

struct ConnRecord {
    /// `None` once the queue has been dropped to force the task to end.
    out: Option<mpsc::Sender<Outbound>>,
    states: bool,
    /// Set when the node closed the connection, so the reported reason is
    /// the node's rather than the task's view of it.
    closing: Option<CloseReason>,
}

/// The listener and connections of one node.
pub(crate) struct Core {
    listener: TcpListener,
    replies: Arc<FixedReplies>,
    connections: HashMap<ConnId, ConnRecord>,
    next_conn: u64,
    inbound_tx: mpsc::Sender<Inbound>,
    inbound_rx: mpsc::Receiver<Inbound>,
    tasks: JoinSet<()>,
    shutdown: CancellationToken,
    status: watch::Sender<ServerStatus>,
}

/// Binds `address` with address reuse, so a restarted node can take its
/// port back while old connections linger in TIME_WAIT.
pub(crate) fn bind(address: SocketAddr) -> Result<TcpListener, StartError> {
    let bind = || -> io::Result<TcpListener> {
        let socket = if address.is_ipv4() {
            TcpSocket::new_v4()?
        } else {
            TcpSocket::new_v6()?
        };
        socket.set_reuseaddr(true)?;
        socket.bind(address)?;
        socket.listen(16)
    };
    bind().map_err(|source| StartError::Bind { address, source })
}

impl Core {
    pub(crate) fn new(
        listener: TcpListener,
        replies: FixedReplies,
        shutdown: CancellationToken,
        status: watch::Sender<ServerStatus>,
    ) -> Self {
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        Self {
            listener,
            replies: Arc::new(replies),
            connections: HashMap::new(),
            next_conn: 0,
            inbound_tx,
            inbound_rx,
            tasks: JoinSet::new(),
            shutdown,
            status,
        }
    }

    /// Waits for the next thing the node must act on, accepting connections
    /// along the way. Cancel-safe: it can sit in a `select!` with the node's
    /// other inputs.
    pub(crate) async fn next(&mut self) -> CoreEvent {
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _peer)) => self.admit(stream),
                    // Transient failures (an aborted handshake, a full file
                    // table) are retried after a short pause rather than in a
                    // tight loop.
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                },
                Some(inbound) = self.inbound_rx.recv() => match inbound {
                    Inbound::Message(conn, frame) => {
                        if !self.is_open(conn) {
                            continue;
                        }
                        if frame.message_type == SubscribeStatesRequest::ID {
                            if let Some(record) = self.connections.get_mut(&conn) {
                                record.states = true;
                            }
                            self.publish_counts();
                            return CoreEvent::SubscribedStates(conn);
                        }
                        return CoreEvent::Message(conn, frame);
                    }
                    Inbound::Closed(conn, reason) => {
                        if let Some(record) = self.connections.remove(&conn) {
                            let reason = record.closing.unwrap_or(reason);
                            self.status.send_modify(|status| status.last_close = Some(reason));
                            self.publish_counts();
                            return CoreEvent::Closed(conn);
                        }
                    }
                },
                // Reap finished connection tasks so the set does not grow.
                Some(_) = self.tasks.join_next(), if !self.tasks.is_empty() => {}
            }
        }
    }

    fn admit(&mut self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        if self.connections.len() >= MAX_CONNECTIONS {
            self.status
                .send_modify(|status| status.last_close = Some(CloseReason::TooManyConnections));
            // Dropping the stream closes it.
            return;
        }
        let conn = ConnId(self.next_conn);
        self.next_conn += 1;
        let (out_tx, out_rx) = mpsc::channel(OUTBOUND_CAPACITY);
        self.connections.insert(
            conn,
            ConnRecord {
                out: Some(out_tx),
                states: false,
                closing: None,
            },
        );
        let task = ConnectionTask {
            conn,
            replies: Arc::clone(&self.replies),
            inbound: self.inbound_tx.clone(),
            outbound: out_rx,
            shutdown: self.shutdown.child_token(),
        };
        self.tasks.spawn(task.run(stream));
        self.publish_counts();
    }

    fn publish_counts(&self) {
        let connections = self.connections.len();
        let state_subscribers = self.connections.values().filter(|c| c.states).count();
        self.status.send_if_modified(|status| {
            let changed =
                status.connections != connections || status.state_subscribers != state_subscribers;
            status.connections = connections;
            status.state_subscribers = state_subscribers;
            changed
        });
    }

    /// Records whether the node's main subscription is held.
    pub(crate) fn set_subscribed(&self, subscribed: bool) {
        self.status.send_if_modified(|status| {
            let changed = status.subscribed != subscribed;
            status.subscribed = subscribed;
            changed
        });
    }

    /// Whether `conn` is open and not being closed.
    pub(crate) fn is_open(&self, conn: ConnId) -> bool {
        self.connections
            .get(&conn)
            .is_some_and(|record| record.closing.is_none())
    }

    /// Queues `frame` for `conn`. A connection whose queue is full is closed
    /// as too slow. Returns whether the frame was queued.
    pub(crate) fn send(&mut self, conn: ConnId, frame: Bytes) -> bool {
        let Some(record) = self.connections.get(&conn) else {
            return false;
        };
        let Some(out) = record.out.as_ref().filter(|_| record.closing.is_none()) else {
            return false;
        };
        match out.try_send(Outbound::Frame(frame)) {
            Ok(()) => true,
            Err(_) => {
                self.close(conn, CloseReason::SlowClient);
                false
            }
        }
    }

    /// Queues `frame` for every connection subscribed to states.
    pub(crate) fn send_to_state_subscribers(&mut self, frame: &Bytes) {
        let subscribers: Vec<ConnId> = self
            .connections
            .iter()
            .filter(|(_, record)| record.states)
            .map(|(conn, _)| *conn)
            .collect();
        for conn in subscribers {
            self.send(conn, frame.clone());
        }
    }

    /// Asks `conn`'s task to close. The core forgets the connection when the
    /// task reports that it has; frames already queued are still written.
    pub(crate) fn close(&mut self, conn: ConnId, reason: CloseReason) {
        let Some(record) = self.connections.get_mut(&conn) else {
            return;
        };
        if record.closing.is_some() {
            return;
        }
        record.closing = Some(reason);
        let requested = record
            .out
            .as_ref()
            .is_some_and(|out| out.try_send(Outbound::Close(reason)).is_ok());
        if !requested {
            // A full queue cannot take the request; dropping the sender ends
            // the task once it has drained what is queued.
            record.out = None;
        }
    }

    /// Stops listening, asks every client to disconnect and waits for the
    /// connection tasks, aborting any that outlast the deadline.
    pub(crate) async fn shutdown(mut self) {
        self.shutdown.cancel();
        drop(self.listener);
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        loop {
            tokio::select! {
                joined = self.tasks.join_next() => {
                    if joined.is_none() {
                        break;
                    }
                }
                // Keep draining so no task blocks on a full queue.
                Some(_) = self.inbound_rx.recv() => {}
                () = sleep_until(deadline) => {
                    self.tasks.abort_all();
                    while self.tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
        self.status.send_modify(|status| {
            status.connections = 0;
            status.state_subscribers = 0;
            status.subscribed = false;
            if !self.connections.is_empty() {
                status.last_close = Some(CloseReason::ServerShutdown);
            }
        });
    }
}

/// One Home Assistant connection.
struct ConnectionTask {
    conn: ConnId,
    replies: Arc<FixedReplies>,
    inbound: mpsc::Sender<Inbound>,
    outbound: mpsc::Receiver<Outbound>,
    shutdown: CancellationToken,
}

/// What a received frame asks the connection to do next.
enum Step {
    Continue,
    Close(CloseReason),
}

/// The fixed one-message frames the connection writes itself.
fn empty_frame<M: ApiMessage>() -> Bytes {
    frame::encode(&M::default()).expect("an empty message fits a frame")
}

impl ConnectionTask {
    async fn run(mut self, mut stream: TcpStream) {
        let reason = self.serve(&mut stream).await;
        let _ = stream.shutdown().await;
        let _ = self.inbound.send(Inbound::Closed(self.conn, reason)).await;
    }

    async fn serve(&mut self, stream: &mut TcpStream) -> CloseReason {
        let connected_at = Instant::now();
        let mut decoder = FrameDecoder::new();
        let mut said_hello = false;
        let mut last_received = connected_at;
        let mut pinged = false;
        loop {
            let deadline = if !said_hello {
                connected_at + HANDSHAKE_TIMEOUT
            } else if !pinged {
                last_received + KEEPALIVE_PING_AFTER
            } else {
                last_received + KEEPALIVE_CLOSE_AFTER
            };
            tokio::select! {
                read = stream.read_buf(decoder.buffer_mut()) => {
                    match read {
                        Ok(0) | Err(_) => return CloseReason::ConnectionLost,
                        Ok(_) => {}
                    }
                    loop {
                        let frame = match decoder.next_frame() {
                            Ok(Some(frame)) => frame,
                            Ok(None) => break,
                            Err(error) => return self.reject(stream, error).await,
                        };
                        match self.on_frame(stream, frame, &mut said_hello).await {
                            Step::Continue => {}
                            Step::Close(reason) => return reason,
                        }
                        // Traffic counts for the keepalive from the hello on,
                        // so a client cannot hold a slot by pinging forever
                        // without saying hello.
                        if said_hello {
                            last_received = Instant::now();
                            pinged = false;
                        }
                    }
                }
                outbound = self.outbound.recv() => match outbound {
                    Some(Outbound::Frame(bytes)) => {
                        if let Err(reason) = write(stream, &bytes).await {
                            return reason;
                        }
                    }
                    Some(Outbound::Close(reason)) => return reason,
                    None => return CloseReason::ServerShutdown,
                },
                () = sleep_until(deadline) => {
                    if !said_hello {
                        return CloseReason::HandshakeTimeout;
                    }
                    if pinged {
                        return CloseReason::KeepaliveTimeout;
                    }
                    if let Err(reason) = write(stream, &empty_frame::<PingRequest>()).await {
                        return reason;
                    }
                    pinged = true;
                }
                () = self.shutdown.cancelled() => {
                    return self.say_goodbye(stream, &mut decoder).await;
                }
            }
        }
    }

    /// Answers a frame error the way an ESPHome node does, then closes.
    async fn reject(&self, stream: &mut TcpStream, error: FrameError) -> CloseReason {
        match error {
            FrameError::BadIndicator(_) => {
                let _ = write(stream, BAD_INDICATOR_REPLY).await;
                CloseReason::BadIndicator
            }
            FrameError::TooLarge(_) => CloseReason::FrameTooLarge,
            FrameError::VarintTooLong | FrameError::TypeOutOfRange(_) => CloseReason::Malformed,
        }
    }

    async fn on_frame(
        &mut self,
        stream: &mut TcpStream,
        frame: Frame,
        said_hello: &mut bool,
    ) -> Step {
        let reply = match frame.message_type {
            HelloRequest::ID => {
                if frame.decode::<HelloRequest>().is_err() {
                    return Step::Close(CloseReason::Malformed);
                }
                *said_hello = true;
                Some(self.replies.hello.clone())
            }
            DisconnectRequest::ID => {
                let _ = write(stream, &empty_frame::<DisconnectResponse>()).await;
                return Step::Close(CloseReason::ClientDisconnected);
            }
            // Only sent in answer to the node's own disconnect request.
            DisconnectResponse::ID => return Step::Close(CloseReason::ServerShutdown),
            PingRequest::ID => Some(empty_frame::<PingResponse>()),
            PingResponse::ID => None,
            _ if !*said_hello => return Step::Close(CloseReason::MessageBeforeHello),
            DeviceInfoRequest::ID => Some(self.replies.device_info.clone()),
            ListEntitiesRequest::ID => {
                for entity in &self.replies.entities {
                    if let Err(reason) = write(stream, entity).await {
                        return Step::Close(reason);
                    }
                }
                None
            }
            // No password is ever asked for, and a node without one sends
            // nothing back (the client does not wait for an answer).
            AuthenticationRequest::ID => None,
            // The node has no log stream, imports no Home Assistant states,
            // calls no Home Assistant actions and never asks for the time,
            // so these subscriptions and replies have nothing to act on.
            SubscribeLogsRequest::ID
            | SubscribeHomeassistantServicesRequest::ID
            | SubscribeHomeAssistantStatesRequest::ID
            | GetTimeResponse::ID => None,
            _ => {
                if self
                    .inbound
                    .send(Inbound::Message(self.conn, frame))
                    .await
                    .is_err()
                {
                    return Step::Close(CloseReason::ServerShutdown);
                }
                None
            }
        };
        if let Some(reply) = reply
            && let Err(reason) = write(stream, &reply).await
        {
            return Step::Close(reason);
        }
        Step::Continue
    }

    /// Sends a disconnect request and waits briefly for the acknowledgement,
    /// as an ESPHome node does when it shuts down.
    async fn say_goodbye(
        &mut self,
        stream: &mut TcpStream,
        decoder: &mut FrameDecoder,
    ) -> CloseReason {
        if write(stream, &empty_frame::<DisconnectRequest>())
            .await
            .is_err()
        {
            return CloseReason::ServerShutdown;
        }
        let _ = timeout(DISCONNECT_GRACE, async {
            loop {
                match stream.read_buf(decoder.buffer_mut()).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                while let Ok(Some(frame)) = decoder.next_frame() {
                    if frame.message_type == DisconnectResponse::ID {
                        return;
                    }
                }
            }
        })
        .await;
        CloseReason::ServerShutdown
    }
}

async fn write(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), CloseReason> {
    match timeout(WRITE_TIMEOUT, stream.write_all(bytes)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(CloseReason::ConnectionLost),
        Err(_) => Err(CloseReason::WriteTimeout),
    }
}
