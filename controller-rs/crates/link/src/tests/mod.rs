//! Deterministic tests: an in-process simulated device speaks the real wire format
//! through tokio-tungstenite against a real [`LinkServer`] and a temporary database
//! created from the contract schema. Every row is synthetic.

mod admission;
mod assets_shell;
mod ingress;
mod liveness;
mod sockets;
mod tls;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_db::devices::{ApproveDevice, EnsureDeviceToken, RecordConnectionAttempt};
use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig, WriteCommand};
use echomuse_protocol::ema::{self, Flags, Header, Kind, SourceMask};
use echomuse_protocol::session::{DetectorPolicy, ReadyGrant, SpeechAssets, Thresholds};
use echomuse_protocol::{REQUIRED_CAPABILITIES, Sha256};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{self, Message};

use crate::{
    Admission, AdmissionDecision, AdmissionRequest, AssetSource, CloseReason, LinkConfig,
    LinkLimits, LinkServer, LinkSession, LinkTimings, SessionLink, Transport, WallClock,
};

/// The fixed wall time of every test: 2023-11-14T22:13:20Z.
pub(super) const UTC_MS: u64 = 1_700_000_000_000;

struct FixedClock;

impl WallClock for FixedClock {
    fn now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(UTC_MS)
    }
}

/// The grant every admitted test session gets.
pub(super) fn grant() -> ReadyGrant {
    ReadyGrant {
        capture_permitted: true,
        assets: SpeechAssets {
            runtime_sha256: Sha256::of(b"runtime"),
            graph_sha256: Sha256::of(b"graph"),
            sidecar_sha256: Sha256::of(b"sidecar"),
        },
        detector: DetectorPolicy::new(Thresholds::new(0.5, 0.6, 0.3).unwrap(), -12.0, None, None)
            .unwrap(),
        afe_metadata: false,
    }
}

/// One admission the link asked for.
pub(super) struct Admitted {
    pub(super) request: AdmissionRequest,
    pub(super) session: LinkSession,
    /// How every earlier admitted session stood when this admission was asked for.
    pub(super) earlier: Vec<Option<CloseReason>>,
}

#[derive(Clone)]
pub(super) enum Decide {
    Ready,
    Decline,
}

struct TestAdmission {
    admitted: mpsc::UnboundedSender<Admitted>,
    decide: Arc<Mutex<Decide>>,
    earlier: Mutex<Vec<SessionLink>>,
}

impl Admission for TestAdmission {
    async fn admit(&self, request: AdmissionRequest, session: LinkSession) -> AdmissionDecision {
        let earlier = {
            let mut links = self.earlier.lock().unwrap();
            let states = links.iter().map(SessionLink::end_reason).collect();
            links.push(session.link.clone());
            states
        };
        let decision = match *self.decide.lock().unwrap() {
            Decide::Ready => AdmissionDecision::Ready(grant()),
            Decide::Decline => AdmissionDecision::Decline("runtime not ready".to_owned()),
        };
        let _ = self.admitted.send(Admitted {
            request,
            session,
            earlier,
        });
        decision
    }
}

#[derive(Clone, Default)]
pub(super) struct TestAssets {
    pub(super) files: HashMap<Sha256, Bytes>,
}

impl AssetSource for TestAssets {
    async fn load(&self, sha256: Sha256) -> io::Result<Option<Bytes>> {
        Ok(self.files.get(&sha256).cloned())
    }
}

/// Writes one `system_config` value, as an administrator's settings change would.
struct SetSetting {
    key: &'static str,
    value: &'static str,
}

impl WriteCommand for SetSetting {
    type Output = ();
    type Error = rusqlite::Error;

    fn apply(self, tx: &rusqlite::Transaction<'_>) -> Result<(), rusqlite::Error> {
        tx.execute(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [self.key, self.value],
        )?;
        Ok(())
    }
}

/// How a simulated device reaches the server.
#[derive(Clone)]
pub(super) enum Route {
    /// A real TCP connection to the plain listener.
    Tcp,
    /// An in-process pipe served as if the given listener accepted it. Used with paused
    /// time, and to stand in for the TLS listener in token-rule tests.
    Pipe(Transport),
    /// A real TLS connection to the TLS listener.
    Tls(Arc<rustls::ClientConfig>),
}

pub(super) trait IoStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> IoStream for T {}
pub(super) type Client = WebSocketStream<Box<dyn IoStream>>;

pub(super) struct Harness {
    _dir: tempfile::TempDir,
    _service: DatabaseService,
    pub(super) db: DbHandle,
    pub(super) server: LinkServer,
    pub(super) admitted: mpsc::UnboundedReceiver<Admitted>,
    pub(super) decide: Arc<Mutex<Decide>>,
}

pub(super) fn test_config() -> LinkConfig {
    LinkConfig {
        host: "127.0.0.1".to_owned(),
        plain_port: 0,
        tls: None,
        require_device_tls: false,
        approval_fallback: echomuse_db::devices::ApprovalMode::Strict,
        limits: LinkLimits::default(),
        timings: LinkTimings::default(),
        wall_clock: Arc::new(FixedClock),
    }
}

impl Harness {
    pub(super) async fn start() -> Self {
        Self::with(test_config(), TestAssets::default()).await
    }

    pub(super) async fn with(config: LinkConfig, assets: TestAssets) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let opened = OpenedDatabase::open(dir.path().join("echomuse.db")).unwrap();
        let service = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
        let db = service.handle();
        let (admitted_tx, admitted) = mpsc::unbounded_channel();
        let decide = Arc::new(Mutex::new(Decide::Ready));
        let admission = TestAdmission {
            admitted: admitted_tx,
            decide: Arc::clone(&decide),
            earlier: Mutex::new(Vec::new()),
        };
        let server = LinkServer::bind(config, db.clone(), admission, assets)
            .await
            .unwrap();
        Self {
            _dir: dir,
            _service: service,
            db,
            server,
            admitted,
            decide,
        }
    }

    pub(super) async fn set_setting(&self, key: &'static str, value: &'static str) {
        self.db.write(SetSetting { key, value }).await.unwrap();
    }

    /// Registers `id` as a device that connected before.
    pub(super) async fn register(&self, id: &str) {
        self.db
            .write(RecordConnectionAttempt {
                device: DeviceId(id.to_owned()),
                ip: "192.0.2.1".parse().unwrap(),
                firmware_version: Some("fw-old".to_owned()),
                os_version: None,
                at: UNIX_EPOCH + Duration::from_secs(1_600_000_000),
            })
            .await
            .unwrap();
    }

    /// An approved device holding a link token.
    pub(super) async fn approved(&self, id: &str) -> SimDevice {
        let mut device = self.approved_without_token(id).await;
        let token = self
            .db
            .write(EnsureDeviceToken {
                device: DeviceId(id.to_owned()),
            })
            .await
            .unwrap();
        device.token = Some(token.expose().to_owned());
        device
    }

    /// An approved device whose token was never issued (or was revoked).
    pub(super) async fn approved_without_token(&self, id: &str) -> SimDevice {
        self.register(id).await;
        self.db
            .write(ApproveDevice {
                device: DeviceId(id.to_owned()),
                label: format!("Synthetic {id}"),
            })
            .await
            .unwrap();
        SimDevice {
            id: id.to_owned(),
            token: None,
        }
    }

    pub(super) async fn next_admitted(&mut self) -> Admitted {
        tokio::time::timeout(Duration::from_secs(10), self.admitted.recv())
            .await
            .expect("an admission in time")
            .expect("admission channel open")
    }

    async fn io(&self, route: &Route) -> Box<dyn IoStream> {
        match route {
            Route::Tcp => Box::new(TcpStream::connect(self.server.plain_addr()).await.unwrap()),
            Route::Pipe(transport) => {
                let (device_end, server_end) = tokio::io::duplex(1 << 20);
                let peer: SocketAddr = "192.0.2.50:40000".parse().unwrap();
                self.server.serve_io(server_end, *transport, peer);
                Box::new(device_end)
            }
            Route::Tls(config) => {
                let tcp = TcpStream::connect(self.server.tls_addr().unwrap())
                    .await
                    .unwrap();
                let name =
                    rustls::pki_types::ServerName::try_from(echomuse_config::TLS_SERVER_NAME)
                        .unwrap();
                let stream = tokio_rustls::TlsConnector::from(Arc::clone(config))
                    .connect(name, tcp)
                    .await
                    .unwrap();
                Box::new(stream)
            }
        }
    }

    /// Opens a WebSocket at `path` with `headers`.
    pub(super) async fn dial(
        &self,
        route: &Route,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<Client, tungstenite::Error> {
        let io = self.io(route).await;
        let mut request = format!("ws://echomuse.local{path}")
            .into_client_request()
            .unwrap();
        for (name, value) in headers {
            request.headers_mut().insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        tokio_tungstenite::client_async(request, io)
            .await
            .map(|(ws, _)| ws)
    }
}

/// The HTTP status of a refused upgrade.
pub(super) fn refused_status(result: Result<Client, tungstenite::Error>) -> u16 {
    match result {
        Err(tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("expected an HTTP refusal, got {other}"),
        Ok(_) => panic!("expected an HTTP refusal, the upgrade succeeded"),
    }
}

/// A hello body announcing everything admission requires.
pub(super) fn hello_body() -> Value {
    let capabilities: Vec<&str> = REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
    json!({
        "capabilities": capabilities,
        "platform": "fireos6",
        "protocols": [1],
        "firmware_version": "fw-test",
        "os_version": "Fire OS test",
        "privacy": {"muted": false, "capture_epoch": "7"},
    })
}

pub(super) fn envelope(
    message_type: &str,
    session: Option<&str>,
    device: &str,
    generation: u32,
    body: Value,
) -> String {
    json!({
        "protocol": 1,
        "type": message_type,
        "session_id": session,
        "message_id": uuid::Uuid::new_v4().to_string(),
        "device_id": device,
        "generation": generation,
        "body": body,
    })
    .to_string()
}

/// A simulated device's identity.
#[derive(Clone)]
pub(super) struct SimDevice {
    pub(super) id: String,
    pub(super) token: Option<String>,
}

impl SimDevice {
    pub(super) fn unknown(id: &str, token: Option<&str>) -> Self {
        Self {
            id: id.to_owned(),
            token: token.map(str::to_owned),
        }
    }

    fn headers<'a>(&'a self, session: Option<&'a str>) -> Vec<(&'a str, &'a str)> {
        let mut headers = Vec::new();
        if let Some(token) = &self.token {
            headers.push(("X-EM-Token", token.as_str()));
        }
        if let Some(session) = session {
            headers.push(("X-EM-Session", session));
        }
        headers
    }

    /// Dials control, sends a hello with `body`, and returns the socket and the reply.
    pub(super) async fn hello_with(
        &self,
        h: &Harness,
        route: &Route,
        body: Value,
    ) -> (Control, Value) {
        let ws = h
            .dial(route, crate::PATH_CONTROL, &self.headers(None))
            .await
            .unwrap();
        let mut control = Control {
            ws,
            device: self.id.clone(),
            session: None,
        };
        control
            .send_text(envelope("session.hello", None, &self.id, 0, body))
            .await;
        let reply = control.next_json().await.expect("a reply to the hello");
        if reply["type"] == "session.ready" {
            control.session = Some(reply["body"]["session_id"].as_str().unwrap().to_owned());
        }
        (control, reply)
    }

    pub(super) async fn hello(&self, h: &Harness, route: &Route) -> (Control, Value) {
        self.hello_with(h, route, hello_body()).await
    }

    /// Connects and expects `session.ready`; returns the control socket and the
    /// runtime's side of the session.
    pub(super) async fn connect(&self, h: &mut Harness, route: &Route) -> (Control, Admitted) {
        let (control, reply) = self.hello(h, route).await;
        assert_eq!(reply["type"], "session.ready", "{reply}");
        let admitted = h.next_admitted().await;
        (control, admitted)
    }

    pub(super) async fn dial_joined(
        &self,
        h: &Harness,
        route: &Route,
        path: &str,
        session: &str,
    ) -> Result<Client, tungstenite::Error> {
        h.dial(route, path, &self.headers(Some(session))).await
    }
}

/// A simulated device's control socket.
pub(super) struct Control {
    pub(super) ws: Client,
    pub(super) device: String,
    pub(super) session: Option<String>,
}

impl Control {
    pub(super) fn session(&self) -> &str {
        self.session.as_deref().expect("an admitted session")
    }

    pub(super) async fn send_text(&mut self, text: String) {
        self.ws.send(Message::text(text)).await.unwrap();
    }

    /// Sends one envelope on the session and returns its `message_id`.
    pub(super) async fn send(
        &mut self,
        message_type: &str,
        generation: u32,
        body: Value,
    ) -> String {
        let text = envelope(
            message_type,
            self.session.as_deref(),
            &self.device,
            generation,
            body,
        );
        let id = serde_json::from_str::<Value>(&text).unwrap()["message_id"]
            .as_str()
            .unwrap()
            .to_owned();
        self.send_text(text).await;
        id
    }

    /// The next text frame as JSON; `None` once the socket closed.
    pub(super) async fn next_json(&mut self) -> Option<Value> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Text(text)) => {
                    return Some(serde_json::from_str(text.as_str()).unwrap());
                }
                Ok(Message::Binary(_)) => panic!("binary frame on control"),
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Ok(Message::Close(_)) | Err(_) => return None,
            }
        }
    }

    /// The next controller message that is not a heartbeat.
    pub(super) async fn next_message(&mut self) -> Option<Value> {
        loop {
            let message = self.next_json().await?;
            if message["type"] != "heartbeat" {
                return Some(message);
            }
        }
    }

    /// Reads until the socket closes and returns every non-heartbeat message.
    pub(super) async fn drain_until_closed(&mut self) -> Vec<Value> {
        let mut seen = Vec::new();
        while let Some(message) = self.next_message().await {
            seen.push(message);
        }
        seen
    }
}

/// A mic packet of 160 silent samples.
pub(super) fn mic_packet(epoch: u64, sequence: u64, first_sample: u64) -> Bytes {
    let header = Header {
        kind: Kind::Mic,
        flags: Flags::NONE,
        epoch,
        sequence,
        first_sample,
        mono_ns: 0,
        uncertainty_us: None,
        frame_count: 160,
        generation: 0,
        source_mask: SourceMask::NONE,
    };
    ema::encode(&header, &[0u8; 320]).unwrap()
}

/// Reads the next binary frame of a client socket.
pub(super) async fn next_binary(ws: &mut Client) -> Option<Bytes> {
    loop {
        match ws.next().await? {
            Ok(Message::Binary(bytes)) => return Some(bytes),
            Ok(Message::Text(text)) => panic!("unexpected text frame {}", text.as_str()),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Close(_)) | Err(_) => return None,
        }
    }
}

/// Whether a client socket closes (rather than delivering data) within five seconds.
pub(super) async fn closes(ws: &mut Client) -> bool {
    let next = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return true,
                Some(Ok(_)) => return false,
            }
        }
    })
    .await;
    next.unwrap_or(false)
}
