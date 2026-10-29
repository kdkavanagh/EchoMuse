//! Capture windows end to end (playback-capture.md): the assembled controller on
//! loopback ports, a simulated speaker streaming EMA1 mic audio in real time (80 ms per
//! packet, only while a subscription wants it), a simulated Home Assistant that
//! answers the controller's setup requests, and a driver speaking plain HTTP to the
//! dashboard API. Webhook deliveries go to a local test server.
//!
//! `RUST_LOG=echomuse=debug` shows the controller's log for a failing test.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use echomuse_config::{ControllerConfig, Environment};
use echomuse_controller::{
    control_plane::{ControlPlane, ControlPlaneOptions, SpeechSource},
    device::DeviceRuntimeConfig,
    supervision::SupervisionOptions,
};
use echomuse_core::types::DeviceId;
use echomuse_db::OpenedDatabase;
use echomuse_firmware::FirmwareTimings;
use echomuse_protocol::REQUIRED_CAPABILITIES;
use echomuse_voice::{
    sim::{BLOCK, SimDevice},
    speech::testing::{ToneModels, silence, tone, utterance},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

const WAIT: Duration = Duration::from_secs(30);
const HA_TOKEN: &str = "synthetic-ha-token";
const KITCHEN: &str = "G0SYNTH00C4P7R01";
const WORDS: &[&str] = &["ophelia", "turn", "on"];
const PIPELINE: &str = "01hmd0v9ffh9ze4dbrz9mw0gks";
/// The capture headers every delivery carries.
const HEADERS: [&str; 10] = [
    "x-em-device",
    "x-em-session",
    "x-em-tag",
    "x-em-ms",
    "x-em-peak-db",
    "x-em-floor-db",
    "x-em-frames",
    "x-em-dropped",
    "x-em-truncated",
    "x-em-opened-ms",
];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn log_init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

#[path = "support/ports.rs"]
mod ports;
use ports::{free_port, free_voice_port};

async fn within<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Polls `check` every 20 ms until it gives a value.
async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    within(what, async {
        loop {
            if let Some(value) = check() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
}

fn token(device: &str) -> String {
    format!("synthetic-device-token-{device}-000")
}

// ── Home Assistant ───────────────────────────────────────────────────────────────────

/// Answers what the controller asks at startup; nothing here runs a pipeline.
struct SimHa {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for SimHa {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SimHa {
    async fn start(device: &str) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mac = echomuse_discovery::voice_mac(&DeviceId(device.to_owned()))
            .to_string()
            .to_ascii_lowercase();
        let satellite = format!("ha-{}", device.to_ascii_lowercase());
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_ha(stream, mac.clone(), satellite.clone()));
            }
        });
        Self { address, task }
    }
}

async fn serve_ha(stream: TcpStream, mac: String, satellite: String) {
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
        ha_websocket(stream, mac, satellite).await;
    } else {
        let (_, _, _) = read_request(stream, |_, _| (404, Vec::new(), b"{}".to_vec())).await;
    }
}

type HaSocket = WebSocketStream<TcpStream>;

async fn send_json(socket: &mut HaSocket, value: Value) -> bool {
    socket.send(Message::text(value.to_string())).await.is_ok()
}

async fn ha_websocket(stream: TcpStream, mac: String, satellite: String) {
    let Ok(mut socket) = accept_async(stream).await else {
        return;
    };
    if !send_json(
        &mut socket,
        json!({"type": "auth_required", "ha_version": "2026.8.1"}),
    )
    .await
    {
        return;
    }
    let auth = loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => break serde_json::from_str::<Value>(&text).unwrap(),
            Some(Ok(_)) => {}
            _ => return,
        }
    };
    if auth["access_token"] != HA_TOKEN
        || !send_json(
            &mut socket,
            json!({"type": "auth_ok", "ha_version": "2026.8.1"}),
        )
        .await
    {
        return;
    }
    while let Some(Ok(message)) = socket.next().await {
        let request = match message {
            Message::Text(text) => serde_json::from_str::<Value>(&text).unwrap(),
            Message::Close(_) => return,
            _ => continue,
        };
        let id = request["id"].clone();
        let reply = match request["type"].as_str().unwrap_or_default() {
            "ping" => json!({"id": id, "type": "pong"}),
            kind => {
                let result = match kind {
                    "get_config" => json!({"time_zone": "Europe/London", "location_name": "Home",
                                           "version": "2026.8.1", "language": "en"}),
                    "config/device_registry/list" => json!([{"id": satellite,
                        "connections": [["mac", mac]], "identifiers": [],
                        "manufacturer": "Espressif", "name": satellite, "area_id": null}]),
                    "assist_pipeline/pipeline/list" => json!({"pipelines": [{"id": PIPELINE,
                        "name": "Home Assistant", "language": "en",
                        "conversation_engine": "conversation.home_assistant",
                        "conversation_language": "en", "stt_engine": "stt.faster_whisper",
                        "stt_language": "en", "tts_engine": "tts.piper", "tts_language": "en",
                        "tts_voice": "amy", "wake_word_entity": null, "wake_word_id": null,
                        "prefer_local_intents": false}], "preferred_pipeline": PIPELINE}),
                    "assist_pipeline/pipeline/get" => json!({"id": PIPELINE,
                        "name": "Home Assistant", "language": "en",
                        "conversation_language": "en",
                        "conversation_engine": "conversation.home_assistant"}),
                    "homeassistant/expose_entity/list" => json!({"exposed_entities": {}}),
                    "config/entity_registry/get_entries" => json!({}),
                    "config/entity_registry/list"
                    | "config/area_registry/list"
                    | "config/floor_registry/list"
                    | "get_states" => json!([]),
                    "subscribe_events" => Value::Null,
                    _ => {
                        let error = json!({"id": id, "type": "result", "success": false,
                            "error": {"code": "unknown_command", "message": "Unknown command."}});
                        if !send_json(&mut socket, error).await {
                            return;
                        }
                        continue;
                    }
                };
                json!({"id": id, "type": "result", "success": true, "result": result})
            }
        };
        if !send_json(&mut socket, reply).await {
            return;
        }
    }
}

// ── Plain HTTP ───────────────────────────────────────────────────────────────────────

/// One HTTP message as read off a socket.
#[derive(Debug)]
struct Http {
    status: u16,
    /// Lower-case names.
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Http {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("no {name} header in {:?}", self.headers))
    }
}

/// A request on the dashboard API with an administrator's session.
async fn api(address: SocketAddr, method: &str, path: &str, body: Option<Value>) -> Http {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        admin_token(),
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a response head");
    let head = String::from_utf8(response[..split].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Http {
        status,
        headers,
        body: response[split + 4..].to_vec(),
    }
}

/// A request as a test server read it: its path, headers (lower-case names) and body.
type Request = (String, HashMap<String, String>, Vec<u8>);

/// Reads one request off `stream`, answers it with `answer(path, headers)`'s status,
/// extra headers and body, and returns what was asked.
async fn read_request(
    stream: TcpStream,
    answer: impl FnOnce(&str, &HashMap<String, String>) -> (u16, Vec<(String, String)>, Vec<u8>),
) -> Request {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let _ = reader.read_line(&mut line).await;
    let path = line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|l| l.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body).await;
    let (status, extra, payload) = answer(&path, &headers);
    let mut head = format!(
        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        payload.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let mut stream = reader.into_inner();
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&payload).await;
    let _ = stream.shutdown().await;
    (path, headers, body)
}

/// The driver's webhook: `/clip` takes a delivery, `/redirect` answers 302 to
/// `/followed`, which no delivery may ever reach.
struct Webhook {
    address: SocketAddr,
    received: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Drop for Webhook {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Webhook {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let request = read_request(stream, |path, _| match path {
                        "/redirect" => (
                            302,
                            vec![("Location".to_owned(), format!("http://{address}/followed"))],
                            Vec::new(),
                        ),
                        _ => (200, Vec::new(), Vec::new()),
                    })
                    .await;
                    log.lock().unwrap().push(request);
                });
            }
        });
        Self {
            address,
            received,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn requests(&self) -> Vec<Request> {
        self.received.lock().unwrap().clone()
    }
}

/// The samples of a 16 kHz mono PCM16 WAV with the 44-byte header.
fn wav_samples(wav: &[u8]) -> Vec<i16> {
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..16], b"WAVEfmt ");
    assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1, "mono");
    assert_eq!(
        u32::from_le_bytes(wav[24..28].try_into().unwrap()),
        16_000,
        "16 kHz"
    );
    assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16, "PCM16");
    assert_eq!(&wav[36..40], b"data");
    let data = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
    assert_eq!(data, wav.len() - 44);
    wav[44..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|s| i16::from_le_bytes(*s))
        .collect()
}

/// Checks a delivery's headers against its WAV and returns its samples.
fn check_delivery(headers: &HashMap<String, String>, wav: &[u8], tag: &str) -> Vec<i16> {
    for name in HEADERS {
        assert!(
            headers.contains_key(name),
            "{name} missing from {headers:?}"
        );
    }
    assert_eq!(headers["x-em-device"], KITCHEN);
    assert_eq!(headers["x-em-tag"], tag);
    let samples = wav_samples(wav);
    let frames: usize = headers["x-em-frames"].parse().unwrap();
    assert_eq!(frames, samples.len());
    assert_eq!(
        headers["x-em-ms"].parse::<usize>().unwrap(),
        frames * 1_000 / 16_000
    );
    for level in ["x-em-peak-db", "x-em-floor-db"] {
        let db: f64 = headers[level].parse().unwrap();
        assert!((-90.0..=0.0).contains(&db), "{level} {db}");
    }
    assert!(headers["x-em-opened-ms"].parse::<u64>().unwrap() > 1_600_000_000_000);
    samples
}

// ── The controller ───────────────────────────────────────────────────────────────────

fn admin_token() -> String {
    "c".repeat(64)
}

struct Fixture {
    dir: tempfile::TempDir,
    env: Environment,
    _ha: SimHa,
}

impl Fixture {
    async fn new() -> Self {
        log_init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("echomuse.db");
        drop(OpenedDatabase::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, label, approved, ip, firmware_ver, first_seen, \
             last_seen, token, esphome_api_port, config, config_sections) \
             VALUES (?1, 'Kitchen', 1, '192.0.2.10', 'fw-synthetic', 1600000000, 1600000000, \
             ?2, ?3, '{}', '[]')",
            rusqlite::params![KITCHEN, token(KITCHEN), free_voice_port()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO users (id, username, password_hash, role, created_at) \
             VALUES (2, 'admin', '$2b$04$synthetic', 'admin', 1600000000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (token, user_id, created_at, expires_at) \
             VALUES (?1, 2, 1600000000, 4000000000)",
            [admin_token()],
        )
        .unwrap();
        drop(conn);
        std::fs::write(
            dir.path().join("libonnxruntime.so"),
            b"synthetic armeabi-v7a libonnxruntime.so",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("static")).unwrap();
        let firmware = dir.path().join("firmware");
        std::fs::create_dir_all(&firmware).unwrap();
        std::fs::write(
            firmware.join("server"),
            b"\x7fELF synthetic bundled firmware",
        )
        .unwrap();
        std::fs::write(firmware.join("version"), "fw-synthetic-bundled").unwrap();
        let payloads = dir.path().join("device_payloads");
        std::fs::create_dir_all(&payloads).unwrap();
        std::fs::write(payloads.join("start_server.sh"), b"#!/system/bin/sh\n").unwrap();

        let ha = SimHa::start(KITCHEN).await;
        let path = |name: &str| dir.path().join(name).display().to_string();
        let env: Environment = [
            ("SERVER_HOST", "127.0.0.1".to_owned()),
            ("SERVER_PORT", free_port().to_string()),
            ("SERVER_TLS_PORT", free_port().to_string()),
            ("API_PORT", free_port().to_string()),
            ("SERVER_IP", "127.0.0.1".to_owned()),
            (
                "MDNS_NAME",
                format!(
                    "echomuse-capture-test-{}-{}",
                    std::process::id(),
                    free_port()
                ),
            ),
            ("HA_URL", format!("http://{}", ha.address)),
            ("HA_TOKEN", HA_TOKEN.to_owned()),
            ("DB_PATH", path("echomuse.db")),
            ("ORT_ANDROID_LIB", path("libonnxruntime.so")),
            ("ESPHOME_PROJECT_VERSION", "2.0.0-test".to_owned()),
            ("ONNXRUNTIME_LIB", path("no-onnxruntime.so")),
            ("FIRMWARE_DIR", path("firmware")),
            ("DEVICE_PAYLOADS_DIR", path("device_payloads")),
            ("DEVICE_APPROVAL", "strict".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        Self { dir, env, _ha: ha }
    }

    async fn start(&self) -> ControlPlane {
        let config = ControllerConfig::from_env(&self.env, Some("test")).unwrap();
        let options = ControlPlaneOptions {
            config,
            static_dir: self.dir.path().join("static"),
            wake_seed_graph: repo_root().join("bcresnet_audio.onnx"),
            wake_seed_sidecar: repo_root().join("bcresnet_audio.json"),
            qualifier_program: PathBuf::from(env!("CARGO_BIN_EXE_echomuse-controller")),
            runtime: DeviceRuntimeConfig::default(),
            firmware_timings: FirmwareTimings::default(),
            speech: SpeechSource::Provided(Arc::new(ToneModels::new(WORDS))),
            ffmpeg: PathBuf::from("/usr/bin/ffmpeg"),
            supervision: SupervisionOptions::default(),
        };
        let plane = within("the controller to start", ControlPlane::start(options))
            .await
            .unwrap();
        let mut ha = plane.home_assistant();
        within(
            "Home Assistant to connect",
            ha.wait_for(|status| status.generation.is_some()),
        )
        .await
        .unwrap();
        plane
    }

    /// The device's controller log lines.
    fn log_lines(&self) -> Vec<String> {
        let conn = rusqlite::Connection::open(self.dir.path().join("echomuse.db")).unwrap();
        let mut statement = conn
            .prepare("SELECT message FROM device_logs WHERE device_id = ?1 ORDER BY id")
            .unwrap();
        statement
            .query_map([KITCHEN], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }
}

async fn shut_down(plane: ControlPlane) {
    let report = plane.shutdown().await;
    assert!(report.is_clean(), "{:?}", report.problems);
}

// ── The simulated speaker ────────────────────────────────────────────────────────────

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One thing in the speaker's capture script.
enum Item {
    /// The room says this.
    Hear(Vec<i16>),
    /// A wake candidate opening here.
    Candidate {
        id: String,
        support_start: u64,
        open: u64,
    },
    /// A device message sent at this point of the capture.
    Send { kind: &'static str, body: Value },
}

#[derive(Default)]
struct Shared {
    sim: Option<SimDevice>,
    script: VecDeque<Item>,
    /// Samples the script holds that were not captured yet.
    queued: u64,
    /// Every controller message except heartbeats and acknowledgements, in order.
    received: Vec<Value>,
    /// Acknowledgement waiters, by the acknowledged message's id.
    waiters: HashMap<String, oneshot::Sender<Value>>,
    /// The controller's answer to each wake candidate: (status, error).
    candidate_acks: HashMap<String, (String, Option<String>)>,
    /// The live audio subscriptions, by lease id: the device uploads nothing without one.
    subscriptions: HashSet<String>,
    physical_seq: u64,
    /// The device is capturing (it stops on a mute).
    capturing: bool,
}

impl Shared {
    fn sim(&mut self) -> &mut SimDevice {
        self.sim.as_mut().expect("capture started")
    }
}

fn envelope(
    device: &str,
    session: Option<&str>,
    message_type: &str,
    generation: u64,
    body: Value,
) -> (String, String) {
    let message_id = uuid::Uuid::new_v4().to_string();
    let text = json!({
        "protocol": 1, "type": message_type, "session_id": session,
        "message_id": message_id, "device_id": device, "generation": generation, "body": body,
    })
    .to_string();
    (text, message_id)
}

async fn dial(address: SocketAddr, path: &str, session: Option<&str>) -> Socket {
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "X-EM-Token",
        HeaderValue::from_str(&token(KITCHEN)).unwrap(),
    );
    if let Some(session) = session {
        request
            .headers_mut()
            .insert("X-EM-Session", HeaderValue::from_str(session).unwrap());
    }
    within("the socket to open", connect_async(request))
        .await
        .unwrap()
        .0
}

async fn next_json(incoming: &mut futures_util::stream::SplitStream<Socket>) -> Option<Value> {
    loop {
        match incoming.next().await? {
            Ok(Message::Text(text)) => return Some(serde_json::from_str(&text).unwrap()),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

#[derive(Clone)]
struct Wire {
    session: String,
    shared: Arc<Mutex<Shared>>,
    control: mpsc::UnboundedSender<Message>,
    audio: mpsc::UnboundedSender<Message>,
}

impl Wire {
    fn send(&self, kind: &str, generation: u64, body: Value) -> String {
        let (text, id) = envelope(KITCHEN, Some(&self.session), kind, generation, body);
        let _ = self.control.send(Message::text(text));
        id
    }

    fn ack(&self, message_id: &Value) {
        self.send(
            "command.ack",
            0,
            json!({"message_id": message_id, "status": "accepted", "error": null}),
        );
    }

    fn audio(&self, packets: Vec<Bytes>) {
        for packet in packets {
            let _ = self.audio.send(Message::Binary(packet));
        }
    }

    /// A controller message on the control socket.
    fn control_message(&self, message: Value) {
        let kind = message["type"].as_str().unwrap_or_default().to_owned();
        let id = message["message_id"].clone();
        let body = message["body"].clone();
        let generation = message["generation"].as_u64().unwrap_or(0);
        self.shared.lock().unwrap().received.push(message);
        match kind.as_str() {
            "render.start" => {
                // Only local sounds (the error cue) play here: started, then drained.
                self.ack(&id);
                let playback = body["playback_id"].as_str().unwrap_or_default().to_owned();
                let wire = self.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    wire.send(
                        "render.progress",
                        generation,
                        json!({"playback_id": playback, "event": "start",
                               "submitted_frames": "0", "completed_frames": "0",
                               "mono_ns": "1", "uncertainty_us": 0,
                               "timing_quality": "estimated", "reference_coverage": "full"}),
                    );
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    wire.send(
                        "render.finished",
                        generation,
                        json!({"playback_id": playback, "last_completed_frame": "0",
                               "reason": "drained", "timing_quality": "estimated"}),
                    );
                });
            }
            "uplink.open" => {
                self.ack(&id);
                let mut shared = self.shared.lock().unwrap();
                if shared.sim.is_none() {
                    return;
                }
                let mut packets = Vec::new();
                for stream in ["mic", "cells", "reference", "afe"] {
                    let from = body["streams"][stream]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok());
                    if let Some(from) = from {
                        packets.extend(shared.sim().backfill(stream, from));
                    }
                }
                self.audio(packets);
                if let Some(lease) = body["lease_id"].as_str() {
                    shared.subscriptions.insert(lease.to_owned());
                }
            }
            "uplink.close" => {
                self.ack(&id);
                if let Some(lease) = body["lease_id"].as_str() {
                    self.shared.lock().unwrap().subscriptions.remove(lease);
                }
            }
            "protocol.error" | "timer.view" | "turn.view" | "alert.op_result" => {}
            _ => self.ack(&id),
        }
    }
}

struct Speaker {
    shared: Arc<Mutex<Shared>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Speaker {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Speaker {
    async fn connect(address: SocketAddr) -> Self {
        let socket = dial(address, "/device/v1/control", None).await;
        let (mut sink, mut incoming) = socket.split();
        let mut capabilities: Vec<&str> =
            REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
        capabilities.extend(["leds", "led_anim", "buttons", "button_hold"]);
        let hello = json!({
            "capabilities": capabilities, "platform": "fireos6", "protocols": [1],
            "firmware_version": "fw-synthetic-2", "os_version": "Fire OS 6 (synthetic)",
            "privacy": {"muted": false, "capture_epoch": "7"},
            "volume": {"level": 64, "seeded": true},
        });
        let (text, _) = envelope(KITCHEN, None, "session.hello", 0, hello);
        sink.send(Message::text(text)).await.unwrap();
        let ready = within("session.ready", next_json(&mut incoming))
            .await
            .expect("a reply");
        assert_eq!(ready["type"], "session.ready", "{ready}");
        let session = ready["body"]["session_id"].as_str().unwrap().to_owned();
        let audio_socket = dial(address, "/device/v1/audio", Some(&session)).await;
        let (mut audio_sink, _audio_incoming) = audio_socket.split();

        let shared = Arc::new(Mutex::new(Shared {
            physical_seq: 1,
            capturing: true,
            ..Shared::default()
        }));
        let (control, mut control_queue) = mpsc::unbounded_channel::<Message>();
        let (audio, mut audio_queue) = mpsc::unbounded_channel::<Message>();
        let wire = Wire {
            session,
            shared: Arc::clone(&shared),
            control,
            audio,
        };
        let mut tasks = vec![
            tokio::spawn(async move {
                while let Some(message) = control_queue.recv().await {
                    if sink.send(message).await.is_err() {
                        return;
                    }
                }
            }),
            tokio::spawn(async move {
                while let Some(message) = audio_queue.recv().await {
                    if audio_sink.send(message).await.is_err() {
                        return;
                    }
                }
            }),
        ];
        let beats = wire.clone();
        tasks.push(tokio::spawn(async move {
            let mut mono = 1_u64;
            loop {
                beats.send("heartbeat", 0, json!({"mono_ns": mono.to_string()}));
                mono += 500_000_000;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }));
        let reader = wire.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(message) = next_json(&mut incoming).await {
                match message["type"].as_str() {
                    Some("heartbeat") => {}
                    Some("command.ack") => {
                        let acked = message["body"]["message_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned();
                        if let Some(waiter) = reader.shared.lock().unwrap().waiters.remove(&acked) {
                            let _ = waiter.send(message["body"].clone());
                        }
                    }
                    _ => reader.control_message(message),
                }
            }
        }));
        {
            let mut guard = shared.lock().unwrap();
            let sim = SimDevice::new(7, 9);
            for body in sim.stream_opens() {
                wire.send("stream.open", 0, Value::Object(body));
            }
            guard.sim = Some(sim);
        }
        wire.send(
            "alert.state",
            0,
            json!({"active": null, "queue": [], "clock_trusted": true, "wakelock": "held",
                   "store": "ok"}),
        );
        tasks.push(tokio::spawn(capture_loop(wire)));
        Self { shared, tasks }
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    /// Where the next queued item lands in the capture.
    fn position(&self) -> u64 {
        let mut shared = self.shared();
        shared.sim().captured() + shared.queued
    }

    fn push(&self, item: Item) {
        let mut shared = self.shared();
        if let Item::Hear(pcm) = &item {
            shared.queued += pcm.len() as u64;
        }
        shared.script.push_back(item);
    }

    fn say(&self, pcm: Vec<i16>) {
        self.push(Item::Hear(pcm));
    }

    /// Waits until everything queued so far has been captured.
    async fn drained(&self) {
        eventually("the script to play out", || {
            let shared = self.shared();
            (shared.queued == 0 && shared.script.is_empty()).then_some(())
        })
        .await;
    }

    /// Waits for the controller's diagnostic subscription and returns its lease.
    async fn diagnostic_lease(&self) -> String {
        eventually("the diagnostic subscription", || {
            let shared = self.shared();
            shared
                .received
                .iter()
                .rev()
                .find(|m| m["type"] == "uplink.open" && m["body"]["reason"] == "diagnostic")
                .and_then(|m| m["body"]["lease_id"].as_str().map(str::to_owned))
                .filter(|lease| shared.subscriptions.contains(lease))
        })
        .await
    }

    fn closed(&self, lease: &str) -> bool {
        self.shared()
            .received
            .iter()
            .any(|m| m["type"] == "uplink.close" && m["body"]["lease_id"] == lease)
    }

    fn next_physical_seq(&self) -> u64 {
        let mut shared = self.shared();
        shared.physical_seq += 1;
        shared.physical_seq
    }
}

/// The device's capture: one 80 ms packet per tick, the script's audio or else quiet,
/// with the script's messages sent where they stand in it.
async fn capture_loop(wire: Wire) {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if !wire.shared.lock().unwrap().capturing {
            continue;
        }
        let mut markers = Vec::new();
        {
            let mut shared = wire.shared.lock().unwrap();
            let block = match shared.script.front_mut() {
                Some(Item::Hear(pcm)) => {
                    let n = pcm.len().min(BLOCK as usize);
                    Some(pcm.drain(..n).collect::<Vec<i16>>())
                }
                Some(_) => None,
                None => Some(vec![0; BLOCK as usize]),
            };
            if let Some(block) = block {
                if !shared.script.is_empty() {
                    shared.queued -= block.len() as u64;
                }
                if matches!(shared.script.front(), Some(Item::Hear(pcm)) if pcm.is_empty()) {
                    shared.script.pop_front();
                }
                shared.sim().hear(&block);
                if shared.subscriptions.is_empty() {
                    shared.sim().skip_live();
                } else {
                    let packets = shared.sim().live();
                    wire.audio(packets);
                }
            }
            while matches!(
                shared.script.front(),
                Some(Item::Candidate { .. } | Item::Send { .. })
            ) {
                markers.push(shared.script.pop_front().expect("present"));
            }
        }
        for marker in markers {
            match marker {
                Item::Candidate {
                    id,
                    support_start,
                    open,
                } => candidate(&wire, &id, support_start, open).await,
                Item::Send { kind, body } => {
                    // A mute ends every subscription on the device and stops capture.
                    let mute = kind == "privacy.changed" && body["muted"] == true;
                    wire.send(kind, 0, body);
                    if mute {
                        let mut shared = wire.shared.lock().unwrap();
                        shared.subscriptions.clear();
                        shared.capturing = false;
                    }
                }
                Item::Hear(_) => {}
            }
        }
    }
}

/// Sends a wake candidate and records the controller's answer.
async fn candidate(wire: &Wire, id: &str, support_start: u64, open: u64) {
    let body = wire.shared.lock().unwrap().sim().candidate(
        id,
        &format!("lease-{id}"),
        support_start,
        open,
        false,
    );
    let (sent, answer) = oneshot::channel();
    let (text, message_id) = envelope(
        KITCHEN,
        Some(&wire.session),
        "wake.candidate",
        1,
        Value::Object(body),
    );
    wire.shared.lock().unwrap().waiters.insert(message_id, sent);
    let _ = wire.control.send(Message::text(text));
    let ack = match tokio::time::timeout(Duration::from_secs(2), answer).await {
        Ok(Ok(ack)) => (
            ack["status"].as_str().unwrap_or_default().to_owned(),
            ack["error"].as_str().map(str::to_owned),
        ),
        _ => ("ttl".to_owned(), None),
    };
    wire.shared
        .lock()
        .unwrap()
        .candidate_acks
        .insert(id.to_owned(), ack);
}

/// A 1 kHz tone at about −12 dBFS.
fn test_tone(ms: u64) -> Vec<i16> {
    (0..ms * 16)
        .map(|n| {
            let t = n as f64 / 16_000.0;
            ((t * 1_000.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16
        })
        .collect()
}

/// Reads `GET …/capture` every 50 ms until `check` accepts the answer.
async fn state_until(plane: &ControlPlane, what: &str, check: impl Fn(&Http) -> bool) -> Http {
    within(what, async {
        loop {
            let state = capture_state(plane).await;
            if check(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
}

async fn capture_state(plane: &ControlPlane) -> Http {
    api(
        plane.api_address(),
        "GET",
        &format!("/api/devices/{KITCHEN}/capture"),
        None,
    )
    .await
}

async fn arm(plane: &ControlPlane, body: Value) -> Http {
    api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture"),
        Some(body),
    )
    .await
}

async fn open_window(plane: &ControlPlane, tag: &str, max_ms: Option<u64>) -> Http {
    let mut body = json!({ "tag": tag });
    if let Some(max_ms) = max_ms {
        body["max_ms"] = json!(max_ms);
    }
    api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture/window"),
        Some(body),
    )
    .await
}

async fn collect(plane: &ControlPlane, session: &str, wait: u64) -> Http {
    api(
        plane.api_address(),
        "GET",
        &format!("/api/devices/{KITCHEN}/capture/recording?session={session}&wait={wait}"),
        None,
    )
    .await
}

async fn capture_mode_listed(plane: &ControlPlane) -> Value {
    let devices = api(plane.api_address(), "GET", "/api/devices", None).await;
    assert_eq!(devices.status, 200, "{:?}", devices.json());
    devices
        .json()
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == KITCHEN)
        .unwrap()["captureMode"]
        .clone()
}

// ── Tests ────────────────────────────────────────────────────────────────────────────

/// Pull mode: the window holds the tone the device heard after it opened, the stop
/// delivers it with every header, the long poll hands it over once, and a second read
/// finds nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_window_records_the_tone_and_is_collected_once() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    assert_eq!(capture_mode_listed(&plane).await, false);

    let armed = arm(&plane, json!({"enabled": true})).await;
    assert_eq!(armed.status, 200, "{:?}", armed.json());
    let state = armed.json();
    assert_eq!(state["enabled"], true);
    assert_eq!(state["transport"], "pull");
    assert_eq!(state["webhook"], Value::Null);
    assert_eq!(state["idle_s"], 300);
    assert_eq!(state["window"], Value::Null);
    assert_eq!(state["pending"], 0);
    assert_eq!(state["dropped"], 0);
    speaker.diagnostic_lease().await;
    assert_eq!(capture_mode_listed(&plane).await, true);

    let opened = open_window(&plane, "cv/0001.mp3 vol=0.4", None).await;
    assert_eq!(opened.status, 200, "{:?}", opened.json());
    let opened = opened.json();
    assert_eq!(opened["tag"], "cv/0001.mp3 vol=0.4");
    assert_eq!(opened["max_ms"], 12_000);
    let session = opened["session"].as_str().unwrap().to_owned();
    // Only one window at a time.
    let second = open_window(&plane, "again", None).await;
    assert_eq!(second.status, 409);
    assert_eq!(second.json()["code"], "busy");
    let state = capture_state(&plane).await.json();
    assert_eq!(state["window"]["session"], session.as_str());
    assert!(state["window"]["opened_ms"].as_u64().is_some());

    speaker.say(silence(160));
    speaker.say(test_tone(480));
    speaker.say(silence(160));
    speaker.drained().await;
    // A stale session never stops the window.
    let stale = api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture/window/stop"),
        Some(json!({"session": "cw-0-1"})),
    )
    .await;
    assert_eq!(stale.status, 409);
    assert_eq!(stale.json()["code"], "conflict");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped = api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture/window/stop"),
        Some(json!({ "session": session })),
    )
    .await;
    assert_eq!(stopped.status, 200, "{:?}", stopped.json());
    assert_eq!(stopped.json()["session"], session.as_str());

    let recording = collect(&plane, &session, 10).await;
    assert_eq!(recording.status, 200, "{:?}", recording.json());
    assert_eq!(recording.header("content-type"), "audio/wav");
    let samples = check_delivery(&recording.headers, &recording.body, "cv/0001.mp3 vol=0.4");
    assert_eq!(recording.header("x-em-session"), session);
    assert_eq!(recording.header("x-em-truncated"), "false");
    assert_eq!(recording.header("x-em-dropped"), "0");
    // The window holds all of the tone.
    assert!(samples.len() >= 480 * 16, "{} samples", samples.len());
    let loud = samples.iter().filter(|s| s.unsigned_abs() > 4_000).count();
    assert!(loud > 480 * 16 / 3, "{loud} loud samples");
    let peak: f64 = recording.header("x-em-peak-db").parse().unwrap();
    assert!((-13.0..=-11.0).contains(&peak), "peak {peak}");
    let floor: f64 = recording.header("x-em-floor-db").parse().unwrap();
    assert_eq!(floor, -90.0, "the window has blocks of silence");

    // Consumed on read.
    let again = collect(&plane, &session, 0).await;
    assert_eq!(again.status, 404, "{:?}", again.json());
    assert_eq!(again.json()["code"], "not_found");
    let state = capture_state(&plane).await.json();
    assert_eq!(state["pending"], 0);
    assert_eq!(state["window"], Value::Null);
    shut_down(plane).await;
}

/// A window nobody stops closes on audio time at `max_ms`, exactly, as truncated.
#[tokio::test(flavor = "multi_thread")]
async fn max_ms_closes_a_window_on_audio_time_as_truncated() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    assert_eq!(arm(&plane, json!({"enabled": true})).await.status, 200);
    speaker.diagnostic_lease().await;
    // Clamped to 30 s.
    let long = open_window(&plane, "long", Some(90_000)).await;
    assert_eq!(long.json()["max_ms"], 30_000);
    let session = long.json()["session"].as_str().unwrap().to_owned();
    let stopped = api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture/window/stop"),
        Some(json!({ "session": session })),
    )
    .await;
    assert_eq!(stopped.status, 200);

    let opened = open_window(&plane, "short", Some(500)).await;
    assert_eq!(opened.status, 200, "{:?}", opened.json());
    let session = opened.json()["session"].as_str().unwrap().to_owned();
    speaker.say(test_tone(2_000));
    let recording = collect(&plane, &session, 20).await;
    assert_eq!(recording.status, 200, "{:?}", recording.json());
    let samples = check_delivery(&recording.headers, &recording.body, "short");
    assert_eq!(samples.len(), 8_000);
    assert_eq!(recording.header("x-em-ms"), "500");
    assert_eq!(recording.header("x-em-truncated"), "true");
    // Stopping it after it closed names it again; it is not a stale stop.
    let stopped = api(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{KITCHEN}/capture/window/stop"),
        Some(json!({ "session": session })),
    )
    .await;
    assert_eq!(stopped.status, 200, "{:?}", stopped.json());
    shut_down(plane).await;
}

/// Push mode: each window is posted to the webhook with the WAV and its headers; a
/// webhook that answers 302 is not followed, and the window is reported as undelivered.
#[tokio::test(flavor = "multi_thread")]
async fn webhook_mode_posts_each_window_and_never_follows_a_redirect() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    let webhook = Webhook::start().await;

    // The scheme is checked before anything is armed.
    for bad in ["ftp://127.0.0.1/clip", "file:///tmp/clip", "not a url"] {
        let refused = arm(&plane, json!({"enabled": true, "webhook": bad})).await;
        assert_eq!(refused.status, 400, "{bad}: {:?}", refused.json());
    }
    assert_eq!(capture_state(&plane).await.json()["enabled"], false);

    let armed = arm(
        &plane,
        json!({"enabled": true, "webhook": webhook.url("/clip"), "idle_s": 99_999}),
    )
    .await;
    assert_eq!(armed.status, 200, "{:?}", armed.json());
    assert_eq!(armed.json()["transport"], "webhook");
    assert_eq!(armed.json()["webhook"], webhook.url("/clip"));
    assert_eq!(armed.json()["idle_s"], 3_600);
    // Another delivery is refused while this one is armed.
    let other = arm(
        &plane,
        json!({"enabled": true, "webhook": webhook.url("/redirect")}),
    )
    .await;
    assert_eq!(other.status, 409);
    speaker.diagnostic_lease().await;
    // Nothing is held for the long poll in push mode.
    let polled = collect(&plane, "x", 0).await;
    assert_eq!(polled.status, 409);

    let opened = open_window(&plane, "pushed", Some(400)).await;
    let session = opened.json()["session"].as_str().unwrap().to_owned();
    speaker.say(test_tone(800));
    let (path, headers, wav) = eventually("the delivery", || webhook.requests().pop()).await;
    assert_eq!(path, "/clip");
    assert_eq!(headers["content-type"], "audio/wav");
    assert_eq!(headers["x-em-session"], session);
    assert_eq!(headers["x-em-truncated"], "true");
    assert_eq!(check_delivery(&headers, &wav, "pushed").len(), 6_400);

    // Disarm, then arm on a webhook that redirects.
    let off = arm(&plane, json!({"enabled": false})).await;
    assert_eq!(off.status, 200);
    assert_eq!(off.json()["enabled"], false);
    assert_eq!(
        arm(
            &plane,
            json!({"enabled": true, "webhook": webhook.url("/redirect")})
        )
        .await
        .status,
        200
    );
    speaker.diagnostic_lease().await;
    let opened = open_window(&plane, "redirected", Some(240)).await;
    let session = opened.json()["session"].as_str().unwrap().to_owned();
    speaker.say(test_tone(500));
    let failure = state_until(&plane, "the failed delivery", |state| {
        state.json()["last_failure"]["session"] == session.as_str()
    })
    .await
    .json()["last_failure"]
        .clone();
    assert_eq!(failure["reason"], "delivery");
    assert!(
        failure["detail"].as_str().unwrap().contains("302"),
        "{failure}"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    let paths: Vec<String> = webhook.requests().into_iter().map(|r| r.0).collect();
    assert_eq!(paths, ["/clip", "/redirect"]);
    shut_down(plane).await;
}

/// `idle_s` is a dead-man: with no window and no capture request the mode clears
/// itself, the subscription closes (wakes resume) and the device log says why.
#[tokio::test(flavor = "multi_thread")]
async fn the_idle_dead_man_clears_the_mode() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    let armed = arm(&plane, json!({"enabled": true, "idle_s": 2})).await;
    assert_eq!(armed.json()["idle_s"], 2);
    let lease = speaker.diagnostic_lease().await;
    // Read through the device list, which is not a capture request.
    within("the dead-man", async {
        while capture_mode_listed(&plane).await == true {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    eventually("the subscription to close", || {
        speaker.closed(&lease).then_some(())
    })
    .await;
    assert_eq!(capture_state(&plane).await.json()["enabled"], false);
    eventually("the log line", || {
        fixture
            .log_lines()
            .into_iter()
            .find(|line| line.contains("capture mode cleared: no capture window"))
    })
    .await;
    shut_down(plane).await;
}

/// The mode belongs to the session: it is gone once the session is lost, and a
/// reconnected device comes back with it off. An unknown device is 404, a disconnected
/// one 409 `not_connected`.
#[tokio::test(flavor = "multi_thread")]
async fn losing_the_session_clears_the_mode() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    assert_eq!(arm(&plane, json!({"enabled": true})).await.status, 200);
    speaker.diagnostic_lease().await;
    assert_eq!(capture_mode_listed(&plane).await, true);

    drop(speaker);
    within("the session to end", async {
        while capture_state(&plane).await.status != 409 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let state = capture_state(&plane).await;
    assert_eq!(state.json()["code"], "not_connected");
    assert_eq!(open_window(&plane, "x", None).await.status, 409);
    assert_eq!(capture_mode_listed(&plane).await, false);
    let unknown = api(
        plane.api_address(),
        "GET",
        "/api/devices/G0SYNTHUNKNOWN00/capture",
        None,
    )
    .await;
    assert_eq!(unknown.status, 404);

    let speaker = Speaker::connect(plane.device_address()).await;
    let state = state_until(&plane, "the reconnected session", |state| {
        state.status == 200
    })
    .await;
    assert_eq!(state.json()["enabled"], false);
    assert_eq!(capture_mode_listed(&plane).await, false);
    // No recording subscription was opened for the new session.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !speaker
            .shared()
            .received
            .iter()
            .any(|m| m["type"] == "uplink.open")
    );
    shut_down(plane).await;
}

/// While capture is armed the device answers no wake (refusal `diagnostic_recording`),
/// and a mute discards the open window's audio instead of delivering it.
#[tokio::test(flavor = "multi_thread")]
async fn a_wake_is_refused_and_a_mute_discards_the_window() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address()).await;
    assert_eq!(arm(&plane, json!({"enabled": true})).await.status, 200);
    speaker.diagnostic_lease().await;

    let models = ToneModels::new(WORDS);
    let word_start = speaker.position() + 16_000;
    speaker.say(silence(1_000));
    speaker.say(utterance(&models, &["ophelia"], 400, 250, 6_000));
    let open = speaker.position();
    speaker.push(Item::Candidate {
        id: "c1".to_owned(),
        support_start: word_start - 6_400,
        open,
    });
    let ack = eventually("the candidate's answer", || {
        speaker.shared().candidate_acks.get("c1").cloned()
    })
    .await;
    assert_eq!(
        ack,
        (
            "rejected".to_owned(),
            Some("diagnostic_recording".to_owned())
        )
    );

    let opened = open_window(&plane, "muted", None).await;
    let session = opened.json()["session"].as_str().unwrap().to_owned();
    speaker.say(tone(1, 400, 6_000));
    let seq = speaker.next_physical_seq();
    speaker.push(Item::Send {
        kind: "privacy.changed",
        body: json!({"muted": true, "capture_epoch": null, "physical_seq": seq}),
    });
    let failure = state_until(&plane, "the discarded window", |state| {
        state.json()["last_failure"]["session"] == session.as_str()
    })
    .await
    .json()["last_failure"]
        .clone();
    assert_eq!(failure["reason"], "muted");
    let collected = collect(&plane, &session, 0).await;
    assert_eq!(collected.status, 404, "{:?}", collected.json());
    assert_eq!(capture_state(&plane).await.json()["pending"], 0);
    shut_down(plane).await;
}
