//! A simulated Home Assistant on a local port, driven step by step by each test.
//!
//! WebSocket connections (`/api/websocket`) go through HA's authentication exchange
//! (`websocket_api/auth.py`: `auth_required`, then `auth_ok` or `auth_invalid` and a
//! close). After that every frame the client sends reaches the test, and the test writes
//! HA's replies itself. REST requests reach the test the same way; dropping one without a
//! reply closes its connection without an answer.

#![allow(dead_code)]

use std::{
    future::Future,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

pub const TOKEN: &str = "test-token";
pub const HA_VERSION: &str = "2026.8.1";
const PATIENCE: Duration = Duration::from_secs(5);

/// Awaits `future`, failing the test when it takes unreasonably long.
pub async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, future)
        .await
        .expect("the simulated exchange stalled")
}

#[derive(Debug)]
pub enum Inbound {
    Json(Value),
    Binary(Vec<u8>),
    Closed,
}

enum Outbound {
    Json(Value),
    Close,
    /// Stop reading and writing while keeping the socket open, like a host that vanished.
    Freeze,
}

/// One authenticated WebSocket connection, seen from HA's side.
pub struct FakeConnection {
    inbound: mpsc::UnboundedReceiver<Inbound>,
    outbound: mpsc::UnboundedSender<Outbound>,
    /// The client's `auth` message.
    pub auth: Value,
}

impl FakeConnection {
    pub async fn receive(&mut self) -> Inbound {
        within(self.inbound.recv()).await.unwrap_or(Inbound::Closed)
    }

    /// The next text message, which must be a JSON request.
    pub async fn request(&mut self) -> Value {
        match self.receive().await {
            Inbound::Json(value) => value,
            other => panic!("expected a JSON request, got {other:?}"),
        }
    }

    pub async fn binary(&mut self) -> Vec<u8> {
        match self.receive().await {
            Inbound::Binary(frame) => frame,
            other => panic!("expected a binary frame, got {other:?}"),
        }
    }

    /// Asserts the client sends nothing for `period`.
    pub async fn assert_quiet(&mut self, period: Duration) {
        if let Ok(Some(frame)) = tokio::time::timeout(period, self.inbound.recv()).await {
            panic!("expected nothing, got {frame:?}");
        }
    }

    pub fn send(&self, message: Value) {
        let _ = self.outbound.send(Outbound::Json(message));
    }

    /// `result` with success, as `connection.send_result` writes it.
    pub fn result(&self, id: &Value, result: Value) {
        self.send(json!({"id": id, "type": "result", "success": true, "result": result}));
    }

    /// `result` with an error, as `connection.send_error` writes it.
    pub fn error(&self, id: &Value, code: &str, message: &str) {
        self.send(json!({
            "id": id, "type": "result", "success": false,
            "error": {"code": code, "message": message}
        }));
    }

    /// One `event` message for the subscription or run opened by `id`.
    pub fn event(&self, id: &Value, event: Value) {
        self.send(json!({"id": id, "type": "event", "event": event}));
    }

    /// One assist pipeline event, as `PipelineEvent.as_dict` writes it.
    pub fn pipeline_event(&self, id: &Value, kind: &str, data: Value) {
        self.event(
            id,
            json!({"type": kind, "data": data, "timestamp": "2026-10-28T09:00:00.000000+00:00"}),
        );
    }

    pub fn close(&self) {
        let _ = self.outbound.send(Outbound::Close);
    }

    pub fn freeze(&self) {
        let _ = self.outbound.send(Outbound::Freeze);
    }
}

/// One REST request, seen from HA's side.
#[derive(Debug)]
pub struct RestCall {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Value,
    reply: oneshot::Sender<(u16, String)>,
}

impl RestCall {
    pub fn respond(self, status: u16, body: Value) {
        let _ = self.reply.send((status, body.to_string()));
    }

    pub fn respond_text(self, status: u16, body: &str) {
        let _ = self.reply.send((status, body.to_owned()));
    }
}

pub struct FakeHa {
    address: SocketAddr,
    connections: mpsc::UnboundedReceiver<FakeConnection>,
    rest: mpsc::UnboundedReceiver<RestCall>,
    attempts: Arc<AtomicUsize>,
    server: JoinHandle<()>,
}

impl Drop for FakeHa {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeHa {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (connection_sender, connections) = mpsc::unbounded_channel();
        let (rest_sender, rest) = mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(
                    stream,
                    connection_sender.clone(),
                    rest_sender.clone(),
                    counter.clone(),
                ));
            }
        });
        Self {
            address,
            connections,
            rest,
            attempts,
            server,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    /// The next connection that authenticated.
    pub async fn connection(&mut self) -> FakeConnection {
        within(self.connections.recv())
            .await
            .expect("the simulated HA stopped")
    }

    pub async fn rest_call(&mut self) -> RestCall {
        within(self.rest.recv())
            .await
            .expect("the simulated HA stopped")
    }

    pub fn no_rest_call_waiting(&mut self) -> bool {
        self.rest.try_recv().is_err()
    }

    /// WebSocket connections opened so far, authenticated or not.
    pub fn websocket_attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

async fn serve(
    stream: TcpStream,
    connections: mpsc::UnboundedSender<FakeConnection>,
    rest: mpsc::UnboundedSender<RestCall>,
    attempts: Arc<AtomicUsize>,
) {
    let mut head = [0u8; 1024];
    let path = loop {
        let Ok(read) = stream.peek(&mut head).await else {
            return;
        };
        if read == 0 {
            return;
        }
        let text = String::from_utf8_lossy(&head[..read]);
        if let Some(line) = text.split("\r\n").next().filter(|_| text.contains("\r\n")) {
            break line.split(' ').nth(1).unwrap_or_default().to_owned();
        }
        tokio::task::yield_now().await;
    };
    if path.starts_with("/api/websocket") {
        websocket(stream, connections, attempts).await;
    } else {
        http(stream, rest).await;
    }
}

async fn websocket(
    stream: TcpStream,
    connections: mpsc::UnboundedSender<FakeConnection>,
    attempts: Arc<AtomicUsize>,
) {
    let Ok(mut socket) = accept_async(stream).await else {
        return;
    };
    attempts.fetch_add(1, Ordering::SeqCst);
    let text = |value: Value| Message::text(value.to_string());
    if socket
        .send(text(
            json!({"type": "auth_required", "ha_version": HA_VERSION}),
        ))
        .await
        .is_err()
    {
        return;
    }
    let auth = loop {
        match socket.next().await {
            Some(Ok(Message::Text(frame))) => break serde_json::from_str::<Value>(&frame).unwrap(),
            Some(Ok(_)) => {}
            _ => return,
        }
    };
    if auth["type"] != "auth" || auth["access_token"] != TOKEN {
        let _ = socket
            .send(text(
                json!({"type": "auth_invalid", "message": "Invalid access token or password"}),
            ))
            .await;
        let _ = socket.close(None).await;
        return;
    }
    if socket
        .send(text(json!({"type": "auth_ok", "ha_version": HA_VERSION})))
        .await
        .is_err()
    {
        return;
    }
    let (inbound_sender, inbound) = mpsc::unbounded_channel();
    let (outbound, mut outbound_receiver) = mpsc::unbounded_channel();
    let _ = connections.send(FakeConnection {
        inbound,
        outbound,
        auth,
    });
    loop {
        tokio::select! {
            frame = socket.next() => match frame {
                Some(Ok(Message::Text(frame))) => {
                    let _ = inbound_sender.send(Inbound::Json(serde_json::from_str(&frame).unwrap()));
                }
                Some(Ok(Message::Binary(frame))) => {
                    let _ = inbound_sender.send(Inbound::Binary(frame.to_vec()));
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    let _ = inbound_sender.send(Inbound::Closed);
                    return;
                }
                Some(Ok(_)) => {}
            },
            message = outbound_receiver.recv() => match message {
                Some(Outbound::Json(value)) => {
                    if socket.send(text(value)).await.is_err() {
                        return;
                    }
                }
                Some(Outbound::Freeze) => {
                    // Hold the socket without reading it, so not even pings are answered.
                    let _ = outbound_receiver.recv().await;
                    return;
                }
                Some(Outbound::Close) | None => {
                    let _ = socket.close(None).await;
                    let _ = inbound_sender.send(Inbound::Closed);
                    return;
                }
            },
        }
    }
}

async fn http(stream: TcpStream, rest: mpsc::UnboundedSender<RestCall>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut length = 0;
    let mut authorization = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.is_err() {
            return;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    let mut body = vec![0; length];
    if reader.read_exact(&mut body).await.is_err() {
        return;
    }
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    let (reply, answer) = oneshot::channel();
    let _ = rest.send(RestCall {
        method,
        path,
        authorization,
        body,
        reply,
    });
    let Ok((status, body)) = answer.await else {
        // Dropped without a reply: the connection closes with no response.
        return;
    };
    let response = format!(
        "HTTP/1.1 {status} Simulated\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
