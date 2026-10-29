//! The device services end to end: the assembled controller on loopback ports, simulated
//! devices speaking the real wire over their control, audio and shell sockets, a
//! simulated Home Assistant (WebSocket and REST, with Local Calendar), and a simulated
//! Kiosk Satellite, all driven through the dashboard's real HTTP and WebSocket routes.
//!
//! Covered here: device logs (rate bound, retention, the dashboard's read); Wi-Fi scan
//! and change with `wifi_commit` across the reconnect the change causes, and the refusal
//! with its reason on devices without `wifi_control`; sample collection and ambient
//! recording (arm, files, mute, reconnect, disarm, `samples.zip`) and button turns refused
//! meanwhile; the session recorder (grant, private files, mute, no grant); the browser
//! shell in PTY and pipe mode; dashboard alarms against the simulated calendar; the sound
//! catalog (upload, list, in-use refusal, preview and stop, the chosen sounds reaching the
//! alarms and `alert.prefetch`); Kiosk Satellite login; and what deleting a device clears.
//!
//! `RUST_LOG=echomuse=debug` shows the controller's log for a failing test.

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use echomuse_config::{ControllerConfig, Environment};
use echomuse_controller::{
    control_plane::{ControlPlane, ControlPlaneOptions, SpeechSource},
    device::DeviceRuntimeConfig,
    supervision::SupervisionOptions,
};
use echomuse_db::OpenedDatabase;
use echomuse_firmware::FirmwareTimings;
use echomuse_protocol::REQUIRED_CAPABILITIES;
use echomuse_recorder::{Event, Recording};
use echomuse_voice::{
    sim::{BLOCK, SimDevice},
    speech::testing::ToneModels,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

/// How long any one expected thing may take.
const WAIT: Duration = Duration::from_secs(30);
const HA_TOKEN: &str = "synthetic-ha-token";
const KITCHEN: &str = "G0SYNTH00C3D4E5F";
const HALL: &str = "G0SYNTH00D4E5F6A";
const SHOW: &str = "G0SYNTH00E5F6A7B";
const PIPELINE: &str = "01hmd0v9ffh9ze4dbrz9mw0gks";
const KIOSK_PASSWORD: &str = "synthetic kiosk password";
const KIOSK_TOKEN: &str = "synthetic-kiosk-token-1";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn log_init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let _ = rustls::crypto::ring::default_provider().install_default();
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

/// A square wave of `ms` milliseconds at `amplitude`: loud speech or a quiet room.
fn tone(ms: u64, amplitude: i16) -> Vec<i16> {
    (0..ms * 16)
        .map(|i| if i % 16 < 8 { amplitude } else { -amplitude })
        .collect()
}

/// 16 kHz mono PCM16 WAV of a 440 Hz tone: an uploaded sound.
fn sound_wav(ms: u64) -> Vec<u8> {
    let samples: Vec<i16> = (0..ms * 16)
        .map(|n| ((n as f64 / 16_000.0 * 440.0 * std::f64::consts::TAU).sin() * 9_000.0) as i16)
        .collect();
    let data = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + data as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    wav
}

// ── A plain HTTP/1.1 server, for the simulated Home Assistant and Kiosk Satellite ────

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn read_request(reader: &mut BufReader<TcpStream>) -> Option<HttpRequest> {
    let mut line = String::new();
    reader.read_line(&mut line).await.ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).await.ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    Some(HttpRequest { method, path, body })
}

async fn respond(reader: BufReader<TcpStream>, status: u16, body: &Value) {
    let payload = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} Simulated\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(payload.as_bytes()).await;
    let _ = stream.shutdown().await;
}

// ── The simulated Home Assistant ─────────────────────────────────────────────────────

#[derive(Default)]
struct HaState {
    /// Every WebSocket request, in order.
    requests: Vec<Value>,
    /// Local Calendar config entries: (entry id, title).
    entries: Vec<(String, String)>,
    /// Calendar entities: (entity id, config entry id).
    entities: Vec<(String, String)>,
    /// Each calendar's events, as `CalendarEvent.as_dict` writes them.
    events: HashMap<String, Vec<Value>>,
    next_id: u32,
}

impl HaState {
    fn next(&mut self) -> u32 {
        self.next_id += 1;
        self.next_id
    }

    fn calendar_writes(&self, kind: &str) -> Vec<Value> {
        self.requests
            .iter()
            .filter(|r| r["type"] == kind)
            .cloned()
            .collect()
    }
}

struct SimHa {
    address: SocketAddr,
    state: Arc<Mutex<HaState>>,
    task: JoinHandle<()>,
}

impl Drop for SimHa {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SimHa {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(HaState::default()));
        let shared = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_ha(stream, Arc::clone(&shared)));
            }
        });
        Self {
            address,
            state,
            task,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HaState> {
        self.state.lock().unwrap()
    }
}

async fn serve_ha(stream: TcpStream, state: Arc<Mutex<HaState>>) {
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
        ha_websocket(stream, state).await;
    } else {
        ha_http(stream, state).await;
    }
}

type HaSocket = WebSocketStream<TcpStream>;

async fn send_json(socket: &mut HaSocket, value: Value) -> bool {
    socket.send(Message::text(value.to_string())).await.is_ok()
}

async fn result(socket: &mut HaSocket, id: &Value, result: Value) -> bool {
    send_json(
        socket,
        json!({"id": id, "type": "result", "success": true, "result": result}),
    )
    .await
}

/// The push of a `calendar/event/subscribe`: every event of the calendar.
async fn calendar_push(
    socket: &mut HaSocket,
    state: &Arc<Mutex<HaState>>,
    subscription: &Value,
    entity: &str,
) -> bool {
    let events = state
        .lock()
        .unwrap()
        .events
        .get(entity)
        .cloned()
        .unwrap_or_default();
    send_json(
        socket,
        json!({"id": subscription, "type": "event", "event": {"events": events}}),
    )
    .await
}

async fn ha_websocket(stream: TcpStream, state: Arc<Mutex<HaState>>) {
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
    if auth["access_token"] != HA_TOKEN {
        let _ = send_json(
            &mut socket,
            json!({"type": "auth_invalid", "message": "Invalid access token or password"}),
        )
        .await;
        return;
    }
    if !send_json(
        &mut socket,
        json!({"type": "auth_ok", "ha_version": "2026.8.1"}),
    )
    .await
    {
        return;
    }
    // This connection's calendar subscriptions: (subscription id, entity).
    let mut subscriptions: Vec<(Value, String)> = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        let request = match message {
            Message::Text(text) => serde_json::from_str::<Value>(&text).unwrap(),
            Message::Close(_) => return,
            _ => continue,
        };
        state.lock().unwrap().requests.push(request.clone());
        let id = request["id"].clone();
        let kind = request["type"].as_str().unwrap_or_default().to_owned();
        let ok = match kind.as_str() {
            "ping" => send_json(&mut socket, json!({"id": id, "type": "pong"})).await,
            "get_config" => {
                result(
                    &mut socket,
                    &id,
                    json!({"time_zone": "UTC", "location_name": "Home",
                           "version": "2026.8.1", "language": "en"}),
                )
                .await
            }
            "config/entity_registry/list" => {
                let entities: Vec<Value> = state
                    .lock()
                    .unwrap()
                    .entities
                    .iter()
                    .map(|(entity, entry)| {
                        json!({"entity_id": entity, "config_entry_id": entry,
                               "platform": "local_calendar", "device_id": null})
                    })
                    .collect();
                result(&mut socket, &id, Value::Array(entities)).await
            }
            "config_entries/get" => {
                let entries: Vec<Value> = if request["domain"] == "local_calendar" {
                    state
                        .lock()
                        .unwrap()
                        .entries
                        .iter()
                        .map(|(entry, title)| {
                            json!({"entry_id": entry, "title": title,
                                   "domain": "local_calendar", "state": "loaded"})
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                result(&mut socket, &id, Value::Array(entries)).await
            }
            "calendar/event/subscribe" => {
                let entity = request["entity_id"].as_str().unwrap_or_default().to_owned();
                subscriptions.push((id.clone(), entity.clone()));
                result(&mut socket, &id, Value::Null).await
                    && calendar_push(&mut socket, &state, &id, &entity).await
            }
            "unsubscribe_events" => {
                subscriptions.retain(|(sub, _)| *sub != request["subscription"]);
                result(&mut socket, &id, Value::Null).await
            }
            "calendar/event/create" | "calendar/event/delete" => {
                let entity = request["entity_id"].as_str().unwrap_or_default().to_owned();
                {
                    let mut s = state.lock().unwrap();
                    if kind == "calendar/event/create" {
                        let uid = format!("synthetic-uid-{}", s.next());
                        let event = &request["event"];
                        let mut stored = json!({
                            "start": event["dtstart"], "end": event["dtend"],
                            "summary": event["summary"], "uid": uid, "all_day": false,
                        });
                        for key in ["description", "rrule"] {
                            if let Some(value) = event.get(key) {
                                stored[key] = value.clone();
                            }
                        }
                        s.events.entry(entity.clone()).or_default().push(stored);
                    } else {
                        let uid = request["uid"].clone();
                        if let Some(events) = s.events.get_mut(&entity) {
                            events.retain(|e| e["uid"] != uid);
                        }
                    }
                }
                let mut ok = result(&mut socket, &id, Value::Null).await;
                for (subscription, subscribed) in subscriptions.clone() {
                    if subscribed == entity {
                        ok &= calendar_push(&mut socket, &state, &subscription, &entity).await;
                    }
                }
                ok
            }
            "assist_pipeline/pipeline/list" => {
                result(
                    &mut socket,
                    &id,
                    json!({"pipelines": [{"id": PIPELINE, "name": "Home Assistant",
                        "language": "en", "conversation_engine": "conversation.home_assistant",
                        "conversation_language": "en", "stt_engine": "stt.faster_whisper",
                        "stt_language": "en", "tts_engine": "tts.piper", "tts_language": "en",
                        "tts_voice": "amy", "wake_word_entity": null, "wake_word_id": null,
                        "prefer_local_intents": false}],
                        "preferred_pipeline": PIPELINE}),
                )
                .await
            }
            "assist_pipeline/pipeline/get" => {
                result(
                    &mut socket,
                    &id,
                    json!({"id": PIPELINE, "name": "Home Assistant", "language": "en",
                           "conversation_language": "en",
                           "conversation_engine": "conversation.home_assistant"}),
                )
                .await
            }
            "config/device_registry/list"
            | "config/area_registry/list"
            | "config/floor_registry/list"
            | "get_states" => result(&mut socket, &id, json!([])).await,
            "homeassistant/expose_entity/list" => {
                result(&mut socket, &id, json!({"exposed_entities": {}})).await
            }
            "config/entity_registry/get_entries" => result(&mut socket, &id, json!({})).await,
            "conversation/agent/homeassistant/debug" => {
                result(&mut socket, &id, json!({"results": []})).await
            }
            "subscribe_events" | "homeassistant/expose_entity" | "fire_event" => {
                result(&mut socket, &id, Value::Null).await
            }
            "call_service" => {
                result(
                    &mut socket,
                    &id,
                    json!({"context": {"id": "01K8CONTEXT", "parent_id": null,
                                       "user_id": null}}),
                )
                .await
            }
            _ => {
                send_json(
                    &mut socket,
                    json!({"id": id, "type": "result", "success": false,
                           "error": {"code": "unknown_command", "message": "Unknown command."}}),
                )
                .await
            }
        };
        if !ok {
            return;
        }
    }
}

/// HA's REST side: Local Calendar's config flow, calendar reads (nothing before the
/// subscription window) and the intent endpoint; everything else is not found.
async fn ha_http(stream: TcpStream, state: Arc<Mutex<HaState>>) {
    let mut reader = BufReader::new(stream);
    let Some(request) = read_request(&mut reader).await else {
        return;
    };
    let path = request
        .path
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let (status, body) = match (request.method.as_str(), path.as_str()) {
        ("POST", "/api/config/config_entries/flow") => {
            let flow = state.lock().unwrap().next();
            (
                200,
                json!({"type": "form", "flow_id": format!("flow-{flow}"),
                       "handler": "local_calendar", "step_id": "user",
                       "data_schema": [], "errors": {}}),
            )
        }
        ("POST", flow) if flow.starts_with("/api/config/config_entries/flow/") => {
            let asked: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
            let name = asked["calendar_name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let mut s = state.lock().unwrap();
            let entry = format!("entry-{}", s.next());
            let slug: String = name
                .to_ascii_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            s.entries.push((entry.clone(), name.clone()));
            s.entities.push((format!("calendar.{slug}"), entry.clone()));
            (
                200,
                json!({"type": "create_entry", "flow_id": flow.rsplit('/').next(),
                       "handler": "local_calendar", "title": name,
                       "result": {"entry_id": entry, "title": name,
                                  "domain": "local_calendar"}}),
            )
        }
        ("GET", calendar) if calendar.starts_with("/api/calendars/") => (200, json!([])),
        ("POST", "/api/intent/handle") => (
            200,
            json!({"speech": {}, "card": {}, "language": "en",
                   "response_type": "action_done", "data": {"success": [], "failed": []},
                   "speech_slots": {"timers": []}}),
        ),
        _ => (404, json!({"message": "Resource not found"})),
    };
    respond(reader, status, &body).await;
}

// ── The simulated Kiosk Satellite ────────────────────────────────────────────────────

/// Kiosk Satellite's remote API login on `port`: the password it accepts gives
/// [`KIOSK_TOKEN`]; every request's body is kept.
async fn kiosk_satellite(port: u16) -> (Arc<Mutex<Vec<Value>>>, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let logins = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&logins);
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stream);
                let Some(request) = read_request(&mut reader).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
                seen.lock().unwrap().push(json!({"method": request.method,
                    "path": request.path, "body": body.clone()}));
                let (status, answer) = if request.method == "POST"
                    && request.path == "/api/login"
                    && body["password"] == KIOSK_PASSWORD
                {
                    (200, json!({"token": KIOSK_TOKEN}))
                } else {
                    (401, json!({"error": "invalid password"}))
                };
                respond(reader, status, &answer).await;
            });
        }
    });
    (logins, task)
}

// ── The controller ───────────────────────────────────────────────────────────────────

/// One approved device of the fixture.
struct DeviceRow {
    id: &'static str,
    label: &'static str,
    config: String,
    sections: &'static str,
}

impl DeviceRow {
    fn plain(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            config: "{}".to_owned(),
            sections: "[]",
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    env: Environment,
    ha: SimHa,
}

impl Fixture {
    async fn new(devices: &[DeviceRow]) -> Self {
        log_init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("echomuse.db");
        drop(OpenedDatabase::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for (n, device) in devices.iter().enumerate() {
            conn.execute(
                "INSERT INTO devices (device_id, label, approved, ip, firmware_ver, first_seen, \
                 last_seen, token, esphome_api_port, config, config_sections) \
                 VALUES (?1, ?2, 1, ?3, 'fw-synthetic', 1600000000, 1600000000, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    device.id,
                    device.label,
                    format!("192.0.2.{}", 10 + n),
                    token(device.id),
                    free_voice_port(),
                    &device.config,
                    device.sections
                ],
            )
            .unwrap();
        }
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

        let ha = SimHa::start().await;
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
                    "echomuse-services-test-{}-{}",
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
        Self { dir, env, ha }
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
            speech: SpeechSource::Provided(Arc::new(ToneModels::new(&["ophelia", "turn", "on"]))),
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

    fn data(&self) -> &Path {
        self.dir.path()
    }

    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("echomuse.db")).unwrap()
    }
}

async fn shut_down(plane: ControlPlane) {
    let report = plane.shutdown().await;
    assert!(report.is_clean(), "{:?}", report.problems);
}

// ── The dashboard ────────────────────────────────────────────────────────────────────

/// A signed-in administrator on the dashboard API.
struct Dashboard {
    address: SocketAddr,
    session: String,
    http: reqwest::Client,
}

struct Answer {
    status: u16,
    content_type: String,
    bytes: Bytes,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
}

impl Dashboard {
    /// Runs dashboard setup with the printed token.
    async fn sign_in(plane: &ControlPlane) -> Self {
        let http = reqwest::Client::builder().build().unwrap();
        let mut dashboard = Self {
            address: plane.api_address(),
            session: String::new(),
            http,
        };
        let token = plane.setup_token().expect("no user exists yet").to_owned();
        let setup = dashboard
            .call(
                "POST",
                "/api/setup",
                Some(&json!({"token": token, "username": "admin",
                             "password": "synthetic password"})),
            )
            .await;
        assert_eq!(setup.0, 200, "{}", setup.1);
        dashboard.session = setup.1["token"].as_str().unwrap().to_owned();
        dashboard
    }

    async fn send(&self, method: &str, path: &str, content_type: &str, body: Vec<u8>) -> Answer {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = self
            .http
            .request(method, format!("http://{}{path}", self.address))
            .timeout(WAIT);
        if !self.session.is_empty() {
            request = request.bearer_auth(&self.session);
        }
        if !body.is_empty() {
            request = request.header("content-type", content_type).body(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let bytes = response.bytes().await.unwrap();
        Answer {
            status,
            content_type,
            bytes,
        }
    }

    async fn call(&self, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let body = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
        let answer = self.send(method, path, "application/json", body).await;
        (answer.status, answer.json())
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        self.call("GET", path, None).await
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.call("POST", path, Some(body)).await
    }

    /// Polls `path` until `check` takes its answer.
    async fn until<T>(
        &self,
        what: &str,
        path: &str,
        mut check: impl FnMut(u16, &Value) -> Option<T>,
    ) -> T {
        within(what, async {
            loop {
                let (status, body) = self.get(path).await;
                if let Some(value) = check(status, &body) {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
    }

    /// The device object the dashboard lists.
    async fn device(&self, device: &str) -> Value {
        let (status, body) = self.get(&format!("/api/devices/{device}")).await;
        assert_eq!(status, 200, "{body}");
        body
    }

    /// Uploads a sound under `id` (`POST /api/sounds/upload`, multipart as the
    /// dashboard's form sends it).
    async fn upload_sound(&self, id: &str, file_name: &str, bytes: &[u8]) -> (u16, Value) {
        let boundary = "----echomuse-synthetic-boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"id\"\r\n\r\n{id}\r\n\
                 --{boundary}\r\nContent-Disposition: form-data; name=\"sound\"; \
                 filename=\"{file_name}\"\r\nContent-Type: audio/wav\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let answer = self
            .send(
                "POST",
                "/api/sounds/upload",
                &format!("multipart/form-data; boundary={boundary}"),
                body,
            )
            .await;
        (answer.status, answer.json())
    }
}

// ── The simulated device ─────────────────────────────────────────────────────────────

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One thing in a device's capture script.
enum Item {
    /// The room says this.
    Hear(Vec<i16>),
    /// A device message sent at this point of the capture.
    Send { kind: &'static str, body: Value },
    /// The device stops capturing (a mute).
    StopCapture,
}

/// The device's alert cache, as the firmware keeps it.
#[derive(Default)]
struct AlertCache {
    epoch: Option<String>,
    applied: u64,
    /// Live occurrence objects by occurrence id.
    objects: BTreeMap<String, Value>,
}

impl AlertCache {
    fn apply(&mut self, objects: &[Value]) {
        for object in objects {
            let id = object["occurrence_id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            if object.get("tombstone").is_some() {
                self.objects.remove(&id);
            } else {
                self.objects.insert(id, object.clone());
            }
        }
    }
}

/// What the device's shell received.
#[derive(Default)]
struct ShellLog {
    /// Dial-backs made, with `?pty=1` or not.
    dials: Vec<bool>,
    /// Input bytes, after the PTY framing.
    input: Vec<u8>,
    /// Resizes: (columns, rows).
    resizes: Vec<(u16, u16)>,
}

#[derive(Default)]
struct Shared {
    sim: Option<SimDevice>,
    script: VecDeque<Item>,
    capturing: bool,
    /// Every controller message except heartbeats and acknowledgements, in order.
    received: Vec<Value>,
    /// The live audio subscriptions, by lease id.
    subscriptions: HashSet<String>,
    alerts: AlertCache,
    shell: ShellLog,
    /// The shell answers `shell_open` with a PTY.
    shell_pty: bool,
    physical_seq: u64,
}

impl Shared {
    fn sim(&mut self) -> &mut SimDevice {
        self.sim.as_mut().expect("capture started")
    }
}

/// How a simulated device connects.
#[derive(Clone)]
struct Hello {
    device: &'static str,
    platform: &'static str,
    extra: Vec<&'static str>,
}

impl Hello {
    fn dot(device: &'static str, extra: &[&'static str]) -> Self {
        let mut caps = vec!["leds", "led_anim", "buttons", "button_hold", "privacy_mute"];
        caps.extend_from_slice(extra);
        Self {
            device,
            platform: "fireos6",
            extra: caps,
        }
    }

    fn lineage_show(device: &'static str, extra: &[&'static str]) -> Self {
        let mut caps = vec!["leds", "led_anim", "buttons", "button_hold", "screen_wake"];
        caps.extend_from_slice(extra);
        Self {
            device,
            platform: "lineage18_crown",
            extra: caps,
        }
    }
}

struct Speaker {
    hello: Hello,
    session: String,
    address: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    control: mpsc::UnboundedSender<Message>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Speaker {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn envelope(
    device: &str,
    session: Option<&str>,
    message_type: &str,
    body: Value,
) -> (String, String) {
    let message_id = uuid::Uuid::new_v4().to_string();
    let text = json!({
        "protocol": 1, "type": message_type, "session_id": session,
        "message_id": message_id, "device_id": device, "generation": 0, "body": body,
    })
    .to_string();
    (text, message_id)
}

async fn dial(address: SocketAddr, device: &str, path: &str, session: Option<&str>) -> Socket {
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("X-EM-Token", HeaderValue::from_str(&token(device)).unwrap());
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

/// Everything a device's tasks share for sending.
#[derive(Clone)]
struct Wire {
    device: &'static str,
    session: String,
    address: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    control: mpsc::UnboundedSender<Message>,
    audio: mpsc::UnboundedSender<Message>,
}

impl Wire {
    fn send(&self, kind: &str, body: Value) {
        let (text, _) = envelope(self.device, Some(&self.session), kind, body);
        let _ = self.control.send(Message::text(text));
    }

    fn ack(&self, message_id: &Value) {
        self.send(
            "command.ack",
            json!({"message_id": message_id, "status": "accepted", "error": null}),
        );
    }

    fn alert_ack(&self) {
        let (epoch, applied) = {
            let shared = self.shared.lock().unwrap();
            (shared.alerts.epoch.clone(), shared.alerts.applied)
        };
        self.send(
            "alert.ack",
            json!({"delivery_epoch": epoch, "applied_through": applied, "durable": true,
                   "need_snapshot": false}),
        );
    }

    fn control_message(&self, message: Value) {
        let kind = message["type"].as_str().unwrap_or_default().to_owned();
        let id = message["message_id"].clone();
        let body = message["body"].clone();
        self.shared.lock().unwrap().received.push(message);
        match kind.as_str() {
            "uplink.open" => {
                self.ack(&id);
                let mut shared = self.shared.lock().unwrap();
                let mut packets = Vec::new();
                for stream in ["mic", "cells", "reference", "afe"] {
                    let from = body["streams"][stream]
                        .as_str()
                        .and_then(|s| s.parse::<u64>().ok());
                    if let Some(from) = from {
                        packets.extend(shared.sim().backfill(stream, from));
                    }
                }
                for packet in packets {
                    let _ = self.audio.send(Message::Binary(packet));
                }
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
            "alert.snapshot" => {
                let last = {
                    let mut shared = self.shared.lock().unwrap();
                    if body["page_index"] == 0 {
                        shared.alerts.objects.clear();
                    }
                    shared.alerts.epoch = body["delivery_epoch"].as_str().map(str::to_owned);
                    let objects = body["objects"].as_array().cloned().unwrap_or_default();
                    shared.alerts.apply(&objects);
                    let last =
                        body["page_index"].as_u64().map(|i| i + 1) == body["page_count"].as_u64();
                    if last {
                        shared.alerts.applied = body["high_water_mark"].as_u64().unwrap_or(0);
                    }
                    last
                };
                if last {
                    self.alert_ack();
                }
            }
            "alert.delta" => {
                {
                    let mut shared = self.shared.lock().unwrap();
                    let objects = body["objects"].as_array().cloned().unwrap_or_default();
                    shared.alerts.apply(&objects);
                    shared.alerts.applied = body["sequence"].as_u64().unwrap_or(0);
                }
                self.alert_ack();
            }
            "wifi_scan" => {
                self.ack(&id);
                self.send(
                    "wifi_scan_result",
                    json!({"error": "", "networks": [{"ssid": "Home", "signal": -41},
                                                     {"ssid": "Attic", "signal": -67}]}),
                );
            }
            "shell_open" => {
                self.ack(&id);
                let wire = self.clone();
                tokio::spawn(async move { wire.shell().await });
            }
            "protocol.error" | "timer.view" | "turn.view" | "alert.op_result" => {}
            _ => self.ack(&id),
        }
    }

    /// The device's shell dial-back: it echoes every input line back as `out:<line>`.
    async fn shell(self) {
        let pty = self.shared.lock().unwrap().shell_pty;
        let path = if pty {
            format!("/shell/{}?pty=1", self.device)
        } else {
            format!("/shell/{}", self.device)
        };
        self.shared.lock().unwrap().shell.dials.push(pty);
        let socket = dial(self.address, self.device, &path, None).await;
        let (mut sink, mut stream) = socket.split();
        while let Some(Ok(message)) = stream.next().await {
            let Message::Binary(frame) = message else {
                continue;
            };
            let input = if pty {
                match frame.first() {
                    Some(0x00) => frame.slice(1..),
                    Some(0x01) if frame.len() >= 5 => {
                        let cols = u16::from_be_bytes([frame[1], frame[2]]);
                        let rows = u16::from_be_bytes([frame[3], frame[4]]);
                        self.shared.lock().unwrap().shell.resizes.push((cols, rows));
                        continue;
                    }
                    _ => continue,
                }
            } else {
                frame
            };
            self.shared
                .lock()
                .unwrap()
                .shell
                .input
                .extend_from_slice(&input);
            let mut out = b"out:".to_vec();
            out.extend_from_slice(&input);
            if sink.send(Message::Binary(out.into())).await.is_err() {
                return;
            }
        }
    }
}

/// Each connection captures under its own epochs, as a restarted capture does.
fn next_epochs() -> (u64, u64) {
    static NEXT: AtomicU64 = AtomicU64::new(10);
    let base = NEXT.fetch_add(2, Ordering::Relaxed);
    (base, base + 1)
}

impl Speaker {
    async fn connect(address: SocketAddr, hello: Hello) -> Self {
        let device = hello.device;
        let socket = dial(address, device, "/device/v1/control", None).await;
        let (mut sink, mut incoming) = socket.split();
        let mut capabilities: Vec<&str> =
            REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
        capabilities.extend(hello.extra.iter().copied());
        let (capture_epoch, reference_epoch) = next_epochs();
        let body = json!({
            "capabilities": capabilities, "platform": hello.platform, "protocols": [1],
            "firmware_version": "fw-synthetic-2", "os_version": "synthetic",
            "privacy": {"muted": false, "capture_epoch": capture_epoch.to_string()},
            "volume": {"level": 64, "seeded": true},
        });
        let (text, _) = envelope(device, None, "session.hello", body);
        sink.send(Message::text(text)).await.unwrap();
        let ready = within("session.ready", next_json(&mut incoming))
            .await
            .expect("a reply");
        assert_eq!(ready["type"], "session.ready", "{ready}");
        let session = ready["body"]["session_id"].as_str().unwrap().to_owned();
        let audio_socket = dial(address, device, "/device/v1/audio", Some(&session)).await;
        let (mut audio_sink, _audio_incoming) = audio_socket.split();

        let shared = Arc::new(Mutex::new(Shared {
            physical_seq: 1,
            shell_pty: true,
            ..Shared::default()
        }));
        let (control, mut control_queue) = mpsc::unbounded_channel::<Message>();
        let (audio, mut audio_queue) = mpsc::unbounded_channel::<Message>();
        let wire = Wire {
            device,
            session: session.clone(),
            address,
            shared: Arc::clone(&shared),
            control: control.clone(),
            audio,
        };
        let mut tasks = Vec::new();
        tasks.push(tokio::spawn(async move {
            while let Some(message) = control_queue.recv().await {
                if sink.send(message).await.is_err() {
                    return;
                }
            }
        }));
        tasks.push(tokio::spawn(async move {
            // Keeps the audio socket's read side alive with its write side.
            let _incoming = _audio_incoming;
            while let Some(message) = audio_queue.recv().await {
                if audio_sink.send(message).await.is_err() {
                    return;
                }
            }
        }));
        let beats = wire.clone();
        tasks.push(tokio::spawn(async move {
            let mut mono = 1_u64;
            loop {
                beats.send("heartbeat", json!({"mono_ns": mono.to_string()}));
                mono += 500_000_000;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }));
        let reader = wire.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(message) = next_json(&mut incoming).await {
                match message["type"].as_str() {
                    Some("heartbeat") | Some("command.ack") => {}
                    _ => reader.control_message(message),
                }
            }
        }));
        {
            let mut guard = shared.lock().unwrap();
            let sim = SimDevice::new(capture_epoch, reference_epoch);
            for body in sim.stream_opens() {
                wire.send("stream.open", Value::Object(body));
            }
            guard.sim = Some(sim);
        }
        wire.send(
            "alert.state",
            json!({"active": null, "queue": [], "clock_trusted": true, "wakelock": "held",
                   "store": "ok"}),
        );
        let mut speaker = Self {
            hello,
            session,
            address,
            shared,
            control,
            tasks,
        };
        speaker.sync().await;
        speaker.shared().capturing = true;
        speaker.tasks.push(tokio::spawn(capture_loop(wire)));
        speaker
    }

    /// Drops the link and dials again, as the firmware does after a network change.
    async fn reconnect(self) -> Self {
        let (address, hello) = (self.address, self.hello.clone());
        drop(self);
        // The controller notices the lost link before the new hello replaces it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Self::connect(address, hello).await
    }

    fn send(&self, kind: &str, body: Value) {
        let (text, _) = envelope(self.hello.device, Some(&self.session), kind, body);
        self.control.send(Message::text(text)).unwrap();
    }

    /// Sends `clock.request` and waits for its reply: everything sent before it on the
    /// control socket has then been taken in order.
    async fn sync(&self) {
        let nonce = uuid::Uuid::new_v4().to_string();
        self.send("clock.request", json!({"nonce": nonce}));
        self.wait_message("the clock reply", |m| {
            m["type"] == "clock.reply" && m["body"]["nonce"] == nonce
        })
        .await;
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap()
    }

    fn say(&self, pcm: Vec<i16>) {
        self.shared().script.push_back(Item::Hear(pcm));
    }

    fn send_at(&self, kind: &'static str, body: Value) {
        self.shared().script.push_back(Item::Send { kind, body });
    }

    /// The mute button: the firmware reports it and stops capturing.
    fn mute(&self) {
        let seq = self.next_physical_seq();
        self.send_at(
            "privacy.changed",
            json!({"muted": true, "capture_epoch": null, "physical_seq": seq}),
        );
        self.shared().script.push_back(Item::StopCapture);
    }

    fn next_physical_seq(&self) -> u64 {
        let mut shared = self.shared();
        shared.physical_seq += 1;
        shared.physical_seq
    }

    /// Waits until everything queued has been captured.
    async fn drained(&self) {
        eventually("the capture script to play out", || {
            self.shared().script.is_empty().then_some(())
        })
        .await;
    }

    fn messages(&self, kind: &str) -> Vec<Value> {
        self.shared()
            .received
            .iter()
            .filter(|m| m["type"] == kind)
            .cloned()
            .collect()
    }

    async fn wait_message(&self, what: &str, check: impl Fn(&Value) -> bool) -> Value {
        eventually(what, || {
            self.shared().received.iter().find(|m| check(m)).cloned()
        })
        .await
    }

    /// Waits for the recording modes' subscription and returns its lease id.
    async fn diagnostic_opened(&self, after: usize) -> String {
        let open = eventually("the recording subscription", || {
            self.messages("uplink.open")
                .into_iter()
                .skip(after)
                .find(|m| m["body"]["reason"] == "diagnostic")
        })
        .await;
        open["body"]["lease_id"].as_str().unwrap().to_owned()
    }

    fn subscribed(&self, lease: &str) -> bool {
        self.shared().subscriptions.contains(lease)
    }
}

/// The device's capture: one 80 ms packet per tick, the script's audio or else quiet,
/// with the script's messages sent where they stand in it.
async fn capture_loop(wire: Wire) {
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let mut markers = Vec::new();
        {
            let mut shared = wire.shared.lock().unwrap();
            if !shared.capturing {
                continue;
            }
            let block = match shared.script.front_mut() {
                Some(Item::Hear(pcm)) => {
                    let n = pcm.len().min(BLOCK as usize);
                    Some(pcm.drain(..n).collect::<Vec<i16>>())
                }
                Some(_) => None,
                None => Some(vec![0; BLOCK as usize]),
            };
            if let Some(block) = block {
                if matches!(shared.script.front(), Some(Item::Hear(pcm)) if pcm.is_empty()) {
                    shared.script.pop_front();
                }
                shared.sim().hear(&block);
                if shared.subscriptions.is_empty() {
                    shared.sim().skip_live();
                } else {
                    for packet in shared.sim().live() {
                        let _ = wire.audio.send(Message::Binary(packet));
                    }
                }
            }
            while matches!(
                shared.script.front(),
                Some(Item::Send { .. } | Item::StopCapture)
            ) {
                markers.push(shared.script.pop_front().expect("present"));
            }
        }
        for marker in markers {
            match marker {
                Item::Send { kind, body } => wire.send(kind, body),
                Item::StopCapture => wire.shared.lock().unwrap().capturing = false,
                Item::Hear(_) => {}
            }
        }
    }
}

// ── Device logs ──────────────────────────────────────────────────────────────────────

/// The device's lines are stored with the controller's receive time, at most 20 at once
/// and then one per 3 s; refused lines are counted in one controller line; long lines are
/// cut; the device keeps its newest 10,000 lines; the dashboard reads them newest first.
#[tokio::test(flavor = "multi_thread")]
async fn device_logs_are_rate_bound_kept_to_their_retention_and_read_by_the_dashboard() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    {
        // A full log of old lines.
        let mut conn = fixture.db();
        let tx = conn.transaction().unwrap();
        for ts in 1..=10_000_i64 {
            tx.execute(
                "INSERT INTO device_logs (device_id, ts, level, source, message) \
                 VALUES (?1, ?2, 'info', 'device', 'old line')",
                rusqlite::params![KITCHEN, ts],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;

    let long = "x".repeat(3_000);
    speaker.send("log", json!({"level": "info", "message": long}));
    for n in 1..=30 {
        speaker.send(
            "log",
            json!({"level": "info", "message": format!("line {n}")}),
        );
    }
    speaker.sync().await;
    let path = format!("/api/devices/{KITCHEN}/logs?limit=40");
    let newest = dashboard
        .until("twenty new lines", &path, |status, body| {
            assert_eq!(status, 200, "{body}");
            let fresh: Vec<Value> = body
                .as_array()?
                .iter()
                .filter(|e| e["message"] != "old line")
                .cloned()
                .collect();
            (fresh.len() >= 20).then_some(fresh)
        })
        .await;
    // Newest first: the long line was the first accepted, then "line 1" to "line 19".
    assert_eq!(newest.len(), 20, "{newest:#?}");
    assert_eq!(newest[0]["message"], "line 19");
    assert_eq!(newest[0]["source"], "device");
    assert_eq!(newest[0]["level"], "info");
    assert!(newest[0]["ts"].as_i64().unwrap() > 1_600_000_000_000);
    let cut = newest[19]["message"].as_str().unwrap();
    assert!(
        cut.len() <= 2_000 + "…".len() && cut.ends_with('…'),
        "{cut}"
    );

    // Once the allowance refills, the refused lines are counted, then the line stored.
    tokio::time::sleep(Duration::from_millis(3_200)).await;
    speaker.send(
        "log",
        json!({"level": "warn", "message": "after the pause"}),
    );
    let latest = dashboard
        .until("the line after the pause", &path, |_, body| {
            let entries = body.as_array()?;
            (entries.first()?["message"] == "after the pause").then(|| entries.clone())
        })
        .await;
    assert_eq!(latest[0]["level"], "warn");
    assert_eq!(latest[1]["source"], "controller", "{:#?}", &latest[..2]);
    assert_eq!(
        latest[1]["message"],
        "11 device log lines were not stored (rate limit)"
    );

    // The retention keeps 10,000 lines: the oldest went.
    let conn = fixture.db();
    let (count, oldest): (i64, i64) = conn
        .query_row(
            "SELECT count(*), min(ts) FROM device_logs WHERE device_id = ?1",
            [KITCHEN],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(count, 10_000);
    assert_eq!(oldest, 23);
    drop(speaker);
    shut_down(plane).await;
}

// ── Wi-Fi ────────────────────────────────────────────────────────────────────────────

/// A Dot with `wifi_control` scans and changes networks: the change is pending until the
/// device, back on its new network after the reconnect the change causes, reports it; the
/// controller answers every report with `wifi_commit`. Elsewhere the routes are refused
/// with the reason the dashboard shows.
#[tokio::test(flavor = "multi_thread")]
async fn wifi_scan_and_change_reach_a_wifi_control_device_and_are_refused_elsewhere() {
    let fixture = Fixture::new(&[
        DeviceRow::plain(KITCHEN, "Kitchen"),
        DeviceRow::plain(HALL, "Hall"),
        DeviceRow::plain(SHOW, "Show"),
    ])
    .await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let dot = Speaker::connect(
        plane.device_address(),
        Hello::dot(KITCHEN, &["wifi_control"]),
    )
    .await;
    let older_dot = Speaker::connect(plane.device_address(), Hello::dot(HALL, &[])).await;
    let show = Speaker::connect(plane.device_address(), Hello::lineage_show(SHOW, &[])).await;

    let (status, body) = dashboard
        .post(&format!("/api/devices/{KITCHEN}/wifi/scan"), &json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["networks"],
        json!([{"ssid": "Home", "signal": -41}, {"ssid": "Attic", "signal": -67}])
    );

    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/wifi"),
            &json!({"ssid": "Attic", "psk": "correct horse"}),
        )
        .await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["pending"]["ssid"], "Attic");
    let change = dot
        .wait_message("wifi_change", |m| m["type"] == "wifi_change")
        .await;
    assert_eq!(
        change["body"],
        json!({"ssid": "Attic", "psk": "correct horse"})
    );
    assert_eq!(
        dashboard.device(KITCHEN).await["wifi"],
        json!({"pending": {"ssid": "Attic"}, "last_result": null})
    );
    // One change at a time.
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/wifi"),
            &json!({"ssid": "Cellar", "psk": ""}),
        )
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("busy")), "{body}");

    // The device rejoins on the new network and reports; the state survives the reconnect.
    let dot = dot.reconnect().await;
    let report = json!({"error": "", "ok": true, "ssid": "Attic"});
    dot.send("wifi_result", report.clone());
    dot.wait_message("wifi_commit", |m| m["type"] == "wifi_commit")
        .await;
    assert_eq!(
        dashboard.device(KITCHEN).await["wifi"],
        json!({"pending": null, "last_result": {"ok": true, "ssid": "Attic", "error": ""}})
    );
    // The firmware repeats its report until it is committed; each repeat is answered.
    dot.send("wifi_result", report);
    eventually("a second wifi_commit", || {
        (dot.messages("wifi_commit").len() == 2).then_some(())
    })
    .await;

    // A Dot whose firmware predates Wi-Fi control, and an Echo Show, are refused.
    for (device, reason) in [
        (HALL, "firmware update available"),
        (SHOW, "managed by the device"),
    ] {
        let (status, body) = dashboard
            .post(&format!("/api/devices/{device}/wifi/scan"), &json!({}))
            .await;
        assert_eq!(
            (status, &body["code"]),
            (409, &json!("not_applicable")),
            "{body}"
        );
        assert_eq!(body["error"], reason);
        let (status, _) = dashboard
            .post(
                &format!("/api/devices/{device}/wifi"),
                &json!({"ssid": "Attic", "psk": "correct horse"}),
            )
            .await;
        assert_eq!(status, 409);
    }
    assert!(older_dot.messages("wifi_scan").is_empty());
    assert!(show.messages("wifi_change").is_empty());
    drop((dot, older_dot, show));
    shut_down(plane).await;
}

// ── Recording modes ──────────────────────────────────────────────────────────────────

/// The sample clips listed for the device.
async fn sample_names(dashboard: &Dashboard, device: &str) -> Vec<String> {
    let (status, body) = dashboard
        .get(&format!("/api/devices/{device}/samples"))
        .await;
    assert_eq!(status, 200, "{body}");
    body["clips"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Sample collection: arming opens the recording subscription and words become clips;
/// the device takes no button turn meanwhile; a mute discards the clip being cut; the
/// mode survives a reconnect; disarming closes the subscription; the clips download one
/// by one and as `samples.zip`.
#[tokio::test(flavor = "multi_thread")]
async fn sample_collection_cuts_clips_refuses_button_turns_and_survives_a_reconnect() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;

    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/collect"),
            &json!({"enabled": true}),
        )
        .await;
    assert_eq!((status, &body), (200, &json!({"collectMode": true})));
    assert_eq!(dashboard.device(KITCHEN).await["collectMode"], true);
    let lease = speaker.diagnostic_opened(0).await;

    // A word in a quiet room.
    speaker.say(tone(1_000, 30));
    speaker.say(tone(600, 8_000));
    speaker.say(tone(1_000, 30));
    let clips = dashboard
        .until(
            "a sample clip",
            &format!("/api/devices/{KITCHEN}/samples"),
            |_, b| (b["count"].as_u64()? >= 1).then(|| b.clone()),
        )
        .await;
    assert_eq!(clips["count"], 1, "{clips}");
    let clip = clips["clips"][0].clone();
    // About 1.2 s (the word, the preroll and the hangover) on an idle machine; a gap the
    // uplink reports under load cuts a clip short, so only the cutter's bounds are held
    // here (its lengths are unit-tested in recording_modes).
    let ms = clip["ms"].as_u64().unwrap();
    assert!((160..=3_000).contains(&ms), "{clip}");
    assert_eq!(clip["afe"], false);
    let name = clip["name"].as_str().unwrap().to_owned();
    let audio = dashboard
        .send(
            "GET",
            &format!("/api/devices/{KITCHEN}/samples/{name}"),
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(audio.status, 200);
    assert_eq!(audio.content_type, "audio/wav");
    assert_eq!(&audio.bytes[..4], b"RIFF");
    assert_eq!(audio.bytes.len() as u64, clip["bytes"].as_u64().unwrap());

    // The button's tap is refused while the mode runs: no turn opens.
    let opens = speaker.messages("uplink.open").len();
    let seq = speaker.next_physical_seq();
    let tap = {
        let mut shared = speaker.shared();
        let at = shared.sim().captured();
        shared.sim().tap(at, seq)
    };
    speaker.send("button.action", Value::Object(tap));
    speaker.sync().await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(speaker.messages("uplink.open").len(), opens);
    assert!(speaker.subscribed(&lease));
    let turns: i64 = fixture
        .db()
        .query_row("SELECT count(*) FROM turns", [], |row| row.get(0))
        .unwrap();
    assert_eq!(turns, 0);

    // A mute in the middle of a word: what was being cut is discarded.
    speaker.say(tone(400, 8_000));
    speaker.mute();
    speaker.drained().await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(sample_names(&dashboard, KITCHEN).await, vec![name]);

    // After a reconnect (unmuted) the mode resumes and the next word is a clip.
    let speaker = speaker.reconnect().await;
    let lease = speaker.diagnostic_opened(0).await;
    speaker.say(tone(1_000, 30));
    speaker.say(tone(600, 8_000));
    speaker.say(tone(1_000, 30));
    let names = dashboard
        .until(
            "the second clip",
            &format!("/api/devices/{KITCHEN}/samples"),
            |_, b| (b["count"].as_u64()? >= 2).then(|| b["clips"].clone()),
        )
        .await;
    assert_eq!(names.as_array().unwrap().len(), 2, "{names}");

    // Disarming closes the subscription.
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/collect"),
            &json!({"enabled": false}),
        )
        .await;
    assert_eq!((status, &body), (200, &json!({"collectMode": false})));
    speaker
        .wait_message("the recording subscription's close", |m| {
            m["type"] == "uplink.close" && m["body"]["lease_id"] == lease.as_str()
        })
        .await;

    // Every clip in one archive.
    let zip = dashboard
        .send(
            "GET",
            &format!("/api/devices/{KITCHEN}/samples.zip"),
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(zip.status, 200);
    assert_eq!(zip.content_type, "application/zip");
    assert_eq!(&zip.bytes[..4], b"PK\x03\x04");
    let text = String::from_utf8_lossy(&zip.bytes);
    for clip in sample_names(&dashboard, KITCHEN).await {
        assert!(text.contains(&clip), "{clip} is not in the archive");
    }
    drop(speaker);
    shut_down(plane).await;
}

/// The ambient files of the device on disk: (finished, parts).
fn ambient_files(data: &Path, device: &str) -> (Vec<String>, Vec<String>) {
    let mut finished = Vec::new();
    let mut parts = Vec::new();
    if let Ok(entries) = std::fs::read_dir(data.join("ambient").join(device)) {
        for entry in entries {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if name.ends_with(".part") {
                parts.push(name);
            } else if name.ends_with(".wav") {
                finished.push(name);
            }
        }
    }
    (finished, parts)
}

/// Ambient recording writes a part file while it runs and saves it when disarmed; a mute
/// discards the part; armed, it survives a reconnect.
#[tokio::test(flavor = "multi_thread")]
async fn ambient_recording_saves_on_disarm_discards_on_mute_and_survives_a_reconnect() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;
    let ambient = format!("/api/devices/{KITCHEN}/ambient");

    // Collection and ambient recording exclude each other.
    let (status, body) = dashboard.post(&ambient, &json!({"enabled": true})).await;
    assert_eq!((status, &body), (200, &json!({"ambientMode": true})));
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/collect"),
            &json!({"enabled": true}),
        )
        .await;
    assert_eq!((status, &body["code"]), (409, &json!("busy")), "{body}");
    speaker.diagnostic_opened(0).await;
    speaker.say(tone(2_000, 300));
    speaker.drained().await;
    let live = dashboard
        .until("the live ambient length", &ambient, |_, b| {
            (b["live"]["ms"].as_u64()? >= 1_000).then(|| b.clone())
        })
        .await;
    assert_eq!(live["count"], 0, "a part is not listed: {live}");
    assert_eq!(ambient_files(fixture.data(), KITCHEN).1.len(), 1);

    // Disarming saves it.
    dashboard.post(&ambient, &json!({"enabled": false})).await;
    let listing = dashboard
        .until("the saved ambient file", &ambient, |_, b| {
            (b["count"].as_u64()? == 1).then(|| b.clone())
        })
        .await;
    // The two seconds of sound, give or take the packet the subscription opened in.
    assert!(
        listing["clips"][0]["ms"].as_u64().unwrap() >= 1_800,
        "{listing}"
    );
    assert_eq!(
        ambient_files(fixture.data(), KITCHEN),
        (
            vec![listing["clips"][0]["name"].as_str().unwrap().to_owned()],
            vec![]
        )
    );

    // Armed again, then muted: the part is discarded.
    let opened = speaker.messages("uplink.open").len();
    dashboard.post(&ambient, &json!({"enabled": true})).await;
    speaker.diagnostic_opened(opened).await;
    eventually("a new part file", || {
        (ambient_files(fixture.data(), KITCHEN).1.len() == 1).then_some(())
    })
    .await;
    speaker.say(tone(500, 300));
    speaker.mute();
    speaker.drained().await;
    eventually("the part to be discarded", || {
        ambient_files(fixture.data(), KITCHEN)
            .1
            .is_empty()
            .then_some(())
    })
    .await;
    assert_eq!(ambient_files(fixture.data(), KITCHEN).0.len(), 1);

    // The mode is still armed: a reconnect resumes it, and disarming saves the new file.
    let speaker = speaker.reconnect().await;
    speaker.diagnostic_opened(0).await;
    speaker.say(tone(1_000, 300));
    speaker.drained().await;
    dashboard.post(&ambient, &json!({"enabled": false})).await;
    let listing = dashboard
        .until("the second ambient file", &ambient, |_, b| {
            (b["count"].as_u64()? == 2).then(|| b.clone())
        })
        .await;
    let name = listing["clips"][0]["name"].as_str().unwrap();
    let audio = dashboard
        .send(
            "GET",
            &format!("/api/devices/{KITCHEN}/ambient/{name}"),
            "",
            Vec::new(),
        )
        .await;
    assert_eq!((audio.status, &audio.bytes[..4]), (200, &b"RIFF"[..]));
    drop(speaker);
    shut_down(plane).await;
}

// ── The session recorder ─────────────────────────────────────────────────────────────

/// Session recordings of the device on disk.
fn session_recordings(data: &Path, device: &str) -> Vec<PathBuf> {
    match std::fs::read_dir(data.join("session_recordings").join(device)) {
        Ok(entries) => entries.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

/// A granted device's session is recorded into a private file that marks the mute and
/// keeps no frame after it; a device without a grant is not recorded at all.
#[tokio::test(flavor = "multi_thread")]
async fn the_session_recorder_records_only_under_a_grant_and_honours_a_mute() {
    let fixture = Fixture::new(&[
        DeviceRow::plain(KITCHEN, "Kitchen"),
        DeviceRow::plain(HALL, "Hall"),
    ])
    .await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let (status, grant) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/session_recorder"),
            &json!({"minutes": 5, "streams": {"mic": true}}),
        )
        .await;
    assert_eq!(status, 200, "{grant}");
    let granted = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;
    let ungranted = Speaker::connect(plane.device_address(), Hello::dot(HALL, &[])).await;
    // Ambient recording makes the devices upload audio.
    for device in [KITCHEN, HALL] {
        dashboard
            .post(
                &format!("/api/devices/{device}/ambient"),
                &json!({"enabled": true}),
            )
            .await;
    }
    for speaker in [&granted, &ungranted] {
        speaker.diagnostic_opened(0).await;
        speaker.say(tone(1_500, 300));
    }
    granted.drained().await;
    ungranted.drained().await;
    granted.mute();
    granted.drained().await;
    granted.sync().await;
    drop(granted);

    let recordings = dashboard
        .until(
            "the finished recording",
            &format!("/api/devices/{KITCHEN}/session_recorder"),
            |_, b| {
                let first = b["recordings"].as_array()?.first()?.clone();
                (first["finished"] == true).then_some(b.clone())
            },
        )
        .await;
    let id = recordings["recordings"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let files = session_recordings(fixture.data(), KITCHEN);
    assert_eq!(files.len(), 1, "{files:?}");
    let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&files[0]), 0o600);
    assert_eq!(mode(files[0].parent().unwrap()), 0o700);
    let download = dashboard
        .send(
            "GET",
            &format!("/api/devices/{KITCHEN}/session_recorder/{id}"),
            "",
            Vec::new(),
        )
        .await;
    assert_eq!(download.status, 200);
    let recording = Recording::from_bytes(download.bytes).unwrap();
    let frames = recording.frames().count();
    assert!(frames > 0, "no frame was recorded");
    let mute_seq = recording
        .events()
        .find(|e| matches!(&e.event, Event::Privacy { state, .. } if state.muted))
        .map(|e| e.seq)
        .expect("the mute is recorded");
    assert!(
        recording.frames().all(|f| f.seq < mute_seq),
        "a frame was kept after the mute"
    );

    // Nothing for the device without a grant.
    let (status, body) = dashboard
        .get(&format!("/api/devices/{HALL}/session_recorder"))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["recordings"], json!([]));
    assert!(session_recordings(fixture.data(), HALL).is_empty());
    drop(ungranted);
    shut_down(plane).await;
}

// ── The browser shell ────────────────────────────────────────────────────────────────

type Browser = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn open_shell(dashboard: &Dashboard, device: &str) -> Browser {
    let mut request = format!("ws://{}/api/devices/{device}/shell", dashboard.address)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {}", dashboard.session)).unwrap(),
    );
    within("the shell to open", connect_async(request))
        .await
        .unwrap()
        .0
}

async fn next_frame(browser: &mut Browser) -> Message {
    within("a shell frame", async {
        loop {
            match browser.next().await {
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(message)) => return message,
                other => panic!("the shell ended: {other:?}"),
            }
        }
    })
    .await
}

/// The browser terminal reaches the device's shell dial-back: in PTY mode with the
/// dashboard's framing (input and resizes), in pipe mode with raw bytes.
#[tokio::test(flavor = "multi_thread")]
async fn the_browser_shell_bridges_pty_and_pipe_shells() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;

    let mut browser = open_shell(&dashboard, KITCHEN).await;
    let meta = next_frame(&mut browser).await;
    let Message::Text(meta) = meta else {
        panic!("{meta:?}")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&meta).unwrap(),
        json!({"type": "shell_meta", "pty": true})
    );
    let open = speaker
        .wait_message("shell_open", |m| m["type"] == "shell_open")
        .await;
    assert_eq!(open["body"]["pty"], true);
    browser
        .send(Message::Binary(vec![0x01, 0, 120, 0, 40].into()))
        .await
        .unwrap();
    browser
        .send(Message::Binary(b"\x00uptime\n".to_vec().into()))
        .await
        .unwrap();
    let out = next_frame(&mut browser).await;
    assert_eq!(out, Message::Binary(b"out:uptime\n".to_vec().into()));
    assert_eq!(speaker.shared().shell.resizes, [(120, 40)]);
    browser.close(None).await.unwrap();
    drop(browser);

    // A device that could not get a pseudo-terminal dials back without one.
    speaker.shared().shell_pty = false;
    let mut browser = open_shell(&dashboard, KITCHEN).await;
    let Message::Text(meta) = next_frame(&mut browser).await else {
        panic!("no shell_meta")
    };
    assert_eq!(
        serde_json::from_str::<Value>(&meta).unwrap(),
        json!({"type": "shell_meta", "pty": false})
    );
    browser.send(Message::text("ls /data\n")).await.unwrap();
    let out = next_frame(&mut browser).await;
    assert_eq!(out, Message::Binary(b"out:ls /data\n".to_vec().into()));
    assert_eq!(speaker.shared().shell.dials, [true, false]);
    assert_eq!(speaker.shared().shell.input, b"uptime\nls /data\n");
    drop(browser);
    drop(speaker);
    shut_down(plane).await;
}

// ── Alarms and sounds ────────────────────────────────────────────────────────────────

/// The device's live alarm objects.
fn cached_alarms(speaker: &Speaker) -> Vec<Value> {
    speaker
        .shared()
        .alerts
        .objects
        .values()
        .filter(|o| o["kind"] == "alarm")
        .cloned()
        .collect()
}

/// Tomorrow's date in UTC, the simulated Home Assistant's time zone.
fn tomorrow() -> String {
    let today = jiff::Timestamp::now()
        .to_zoned(jiff::tz::TimeZone::UTC)
        .date();
    today.tomorrow().unwrap().to_string()
}

/// An alarm set on the dashboard is stored in the speaker's Local Calendar and armed on
/// the device; the alert panel lists it; cancelling deletes the event and disarms it.
#[tokio::test(flavor = "multi_thread")]
async fn dashboard_alarms_are_set_listed_and_cancelled_through_the_calendar() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;
    let alerts = format!("/api/devices/{KITCHEN}/alerts");
    dashboard
        .until("the speaker's calendar", &alerts, |_, b| {
            (b["status"]["calendar_live"] == true && b["status"]["online"] == true).then_some(())
        })
        .await;
    assert_eq!(
        fixture.ha.state().entries[0].1,
        "EchoMuse Kitchen",
        "the speaker's Local Calendar"
    );

    let date = tomorrow();
    let (status, set) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/alarms"),
            &json!({"time": "07:30", "days": [], "name": "Wake up", "date": date}),
        )
        .await;
    assert_eq!(status, 200, "{set}");
    assert_eq!(set["ok"], true, "{set}");
    let created = fixture.ha.state().calendar_writes("calendar/event/create");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(created[0]["event"]["summary"], "Wake up");
    assert_eq!(
        created[0]["event"]["dtstart"],
        format!("{date}T07:30:00+00:00")
    );
    let armed = eventually("the alarm on the device", || {
        cached_alarms(&speaker).into_iter().next()
    })
    .await;
    assert_eq!(armed["label"], "Wake up");
    assert_eq!(armed["sound"], "builtin:fallback");
    assert_eq!(armed["due_local"], format!("{date}T07:30:00+00:00"));
    let listed = dashboard
        .until("the panel's armed alarm", &alerts, |_, b| {
            let o = b["status"]["occurrences"].as_array()?.first()?.clone();
            (o["armed_on_endpoint"] == true).then_some(o)
        })
        .await;
    assert_eq!(listed["label"], "Wake up");
    assert_eq!(listed["kind"], "alarm");
    let ops = dashboard.get(&alerts).await.1["status"]["operations"].clone();
    assert_eq!(ops[0]["source"], "dashboard", "{ops}");

    // A cancellation that names nothing is refused; the right one deletes the event.
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/alarms/cancel"),
            &json!({"name": "Wake up", "time": "08:00"}),
        )
        .await;
    assert_eq!((status, &body["ok"]), (200, &json!(false)), "{body}");
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/alarms/cancel"),
            &json!({"name": "wake up", "time": "07:30"}),
        )
        .await;
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
    assert_eq!(
        fixture
            .ha
            .state()
            .calendar_writes("calendar/event/delete")
            .len(),
        1
    );
    eventually("the alarm to leave the device", || {
        cached_alarms(&speaker).is_empty().then_some(())
    })
    .await;
    dashboard
        .until("the panel without it", &alerts, |_, b| {
            b["status"]["occurrences"]
                .as_array()?
                .is_empty()
                .then_some(())
        })
        .await;
    drop(speaker);
    shut_down(plane).await;
}

/// The SHA-256 the catalog exported `id` to, from its export record.
fn exported_sha(data: &Path, id: &str) -> Option<String> {
    let record =
        std::fs::read_to_string(data.join("sounds").join(format!("{id}.alert.json"))).ok()?;
    serde_json::from_str::<Value>(&record).ok()?["sha256"]
        .as_str()
        .map(str::to_owned)
}

/// The catalog: an upload is listed and resolves the speaker's choices; the chosen timer
/// sound is prefetched by an `alert_prefetch` device and new alarms ring the chosen alarm
/// sound; a chosen sound cannot be deleted; previews start and stop on the device; a
/// settings change re-resolves the speaker's sounds.
#[tokio::test(flavor = "multi_thread")]
async fn chosen_sounds_reach_the_alarms_and_the_device_and_cannot_be_deleted() {
    let fixture = Fixture::new(&[DeviceRow {
        id: KITCHEN,
        label: "Kitchen",
        config: r#"{"alarmSound": "bell", "timerSound": "bell"}"#.to_owned(),
        sections: r#"["timers"]"#,
    }])
    .await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let speaker = Speaker::connect(
        plane.device_address(),
        Hello::dot(KITCHEN, &["alert_prefetch"]),
    )
    .await;
    let alerts = format!("/api/devices/{KITCHEN}/alerts");
    dashboard
        .until("the speaker's calendar", &alerts, |_, b| {
            (b["status"]["calendar_live"] == true && b["status"]["online"] == true).then_some(())
        })
        .await;

    // Before the upload, the choice cannot ring and the dashboard says so.
    let (status, catalog) = dashboard.get("/api/sounds").await;
    assert_eq!(status, 200, "{catalog}");
    assert_eq!(catalog["sounds"], json!([]));
    assert_eq!(
        catalog["unresolved"],
        json!([{"key": "timerSound", "scope": KITCHEN},
               {"key": "alarmSound", "scope": KITCHEN}])
    );
    assert!(speaker.messages("alert.prefetch").is_empty());

    let (status, uploaded) = dashboard
        .upload_sound("bell", "bell.wav", &sound_wav(1_500))
        .await;
    assert_eq!(status, 200, "{uploaded}");
    assert_eq!(uploaded["sound"]["id"], "bell");
    let catalog = dashboard
        .until("the exported sound", "/api/sounds", |_, b| {
            (b["sounds"][0]["seconds"].as_f64().is_some()).then(|| b.clone())
        })
        .await;
    assert_eq!(catalog["unresolved"], json!([]), "{catalog}");
    let sha = exported_sha(fixture.data(), "bell").expect("an exported asset");
    assert!(
        fixture
            .data()
            .join("sounds/assets")
            .join(format!("{sha}.wav"))
            .exists()
    );

    // The catalog change reaches the alert service: the timer sound is prefetched.
    let prefetch = speaker
        .wait_message("alert.prefetch", |m| m["type"] == "alert.prefetch")
        .await;
    assert_eq!(prefetch["body"], json!({"sounds": [sha]}));

    // A new alarm rings the chosen sound.
    let (status, set) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/alarms"),
            &json!({"time": "06:45", "name": "Bell", "date": tomorrow()}),
        )
        .await;
    assert_eq!((status, &set["ok"]), (200, &json!(true)), "{set}");
    let armed = eventually("the alarm on the device", || {
        cached_alarms(&speaker).into_iter().next()
    })
    .await;
    assert_eq!(armed["sound"], sha.as_str());

    // A chosen sound cannot be deleted.
    let deleted = dashboard
        .send("DELETE", "/api/sounds/bell", "", Vec::new())
        .await;
    assert_eq!(deleted.status, 409);
    assert_eq!(deleted.json()["code"], "in_use");

    // Previews start and stop on the device.
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/sounds/preview"),
            &json!({"sound_id": "bell"}),
        )
        .await;
    assert_eq!((status, &body), (200, &json!({"ok": true})));
    let start = speaker
        .wait_message("the preview's render.start", |m| {
            m["type"] == "render.start" && m["body"]["source_class"] == "alert_preview"
        })
        .await;
    assert_eq!(start["body"]["local_asset"], sha.as_str());
    let (status, body) = dashboard
        .post(&format!("/api/devices/{KITCHEN}/sounds/stop"), &json!({}))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    // An unknown sound is not previewed.
    let (status, _) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/sounds/preview"),
            &json!({"sound_id": "nonesuch"}),
        )
        .await;
    assert_eq!(status, 404);

    // A settings change back to the default releases the sound: it can be deleted, and
    // the next alarm rings the built-in sound.
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/config"),
            &json!({"config_sections": ["timers"], "alarmSound": "", "timerSound": ""}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let deleted = dashboard
        .send("DELETE", "/api/sounds/bell", "", Vec::new())
        .await;
    assert_eq!(deleted.status, 200, "{:?}", deleted.json());
    assert_eq!(dashboard.get("/api/sounds").await.1["sounds"], json!([]));
    let (status, set) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/alarms"),
            &json!({"time": "06:50", "name": "Plain", "date": tomorrow()}),
        )
        .await;
    assert_eq!((status, &set["ok"]), (200, &json!(true)), "{set}");
    let plain = eventually("the plain alarm on the device", || {
        cached_alarms(&speaker)
            .into_iter()
            .find(|o| o["label"] == "Plain")
    })
    .await;
    assert_eq!(plain["sound"], "builtin:fallback");
    drop(speaker);
    shut_down(plane).await;
}

// ── Kiosk Satellite ──────────────────────────────────────────────────────────────────

/// Logging in at an Echo Show's Kiosk Satellite stores only the token it answers and
/// sends it to the device in `config`; a wrong password is refused; a Dot is refused with
/// the reason.
#[tokio::test(flavor = "multi_thread")]
async fn kiosk_login_stores_the_token_and_sends_it_to_the_show() {
    let kiosk_port = free_port();
    let config = format!(r#"{{"kioskPort": {kiosk_port}}}"#);
    let fixture = Fixture::new(&[
        DeviceRow {
            id: SHOW,
            label: "Show",
            config,
            sections: r#"["screen"]"#,
        },
        DeviceRow::plain(KITCHEN, "Kitchen"),
    ])
    .await;
    let (logins, kiosk) = kiosk_satellite(kiosk_port).await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let show = Speaker::connect(
        plane.device_address(),
        Hello::lineage_show(SHOW, &["screen_overlay", "kiosk_plugin"]),
    )
    .await;
    let dot = Speaker::connect(plane.device_address(), Hello::dot(KITCHEN, &[])).await;

    let login = format!("/api/devices/{SHOW}/kiosk_login");
    let (status, body) = dashboard.post(&login, &json!({"password": "wrong"})).await;
    assert_eq!(
        (status, &body["code"]),
        (400, &json!("bad_request")),
        "{body}"
    );
    let (status, body) = dashboard
        .post(&login, &json!({"password": KIOSK_PASSWORD}))
        .await;
    assert_eq!((status, &body), (200, &json!({"ok": true})));
    assert_eq!(
        logins.lock().unwrap().last().unwrap()["body"],
        json!({"password": KIOSK_PASSWORD, "ttl_days": 3650})
    );
    let stored: Option<String> = fixture
        .db()
        .query_row(
            "SELECT kiosk_token FROM devices WHERE device_id = ?1",
            [SHOW],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored.as_deref(), Some(KIOSK_TOKEN));
    let config = show
        .wait_message("config with the token", |m| {
            m["type"] == "config" && m["body"]["kioskToken"] == KIOSK_TOKEN
        })
        .await;
    assert_eq!(config["body"]["kioskPort"], kiosk_port);
    let row: String = fixture
        .db()
        .query_row(
            "SELECT config FROM devices WHERE device_id = ?1",
            [SHOW],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!row.contains(KIOSK_PASSWORD), "the password was stored");

    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/kiosk_login"),
            &json!({"password": KIOSK_PASSWORD}),
        )
        .await;
    assert_eq!(
        (status, &body["code"]),
        (409, &json!("not_applicable")),
        "{body}"
    );
    assert_eq!(body["error"], "no screen on this device");
    assert_eq!(logins.lock().unwrap().len(), 2);
    drop((show, dot));
    kiosk.abort();
    shut_down(plane).await;
}

// ── Deleting a device ────────────────────────────────────────────────────────────────

/// Deleting a device clears what the device services keep for it: its Wi-Fi state, its
/// recording modes and files, its session recordings and grant, and its sound choices.
/// When it comes back and is approved again it starts clean.
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_device_clears_its_wifi_recordings_and_sound_choices() {
    let fixture = Fixture::new(&[
        DeviceRow {
            id: KITCHEN,
            label: "Kitchen",
            config: r#"{"alarmSound": "bell"}"#.to_owned(),
            sections: r#"["timers"]"#,
        },
        DeviceRow::plain(HALL, "Hall"),
    ])
    .await;
    let plane = fixture.start().await;
    let dashboard = Dashboard::sign_in(&plane).await;
    let (status, _) = dashboard
        .upload_sound("bell", "bell.wav", &sound_wav(800))
        .await;
    assert_eq!(status, 200);
    let speaker = Speaker::connect(
        plane.device_address(),
        Hello::dot(KITCHEN, &["wifi_control"]),
    )
    .await;
    let other = Speaker::connect(plane.device_address(), Hello::dot(HALL, &[])).await;

    // Wi-Fi state, a session recording and ambient files for the kitchen; ambient files
    // for the hall too.
    dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/session_recorder"),
            &json!({"minutes": 5}),
        )
        .await;
    let (status, _) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/wifi"),
            &json!({"ssid": "Attic", "psk": "correct horse"}),
        )
        .await;
    assert_eq!(status, 202);
    for (device, speaker) in [(KITCHEN, &speaker), (HALL, &other)] {
        dashboard
            .post(
                &format!("/api/devices/{device}/ambient"),
                &json!({"enabled": true}),
            )
            .await;
        speaker.diagnostic_opened(0).await;
        speaker.say(tone(800, 300));
    }
    speaker.drained().await;
    other.drained().await;
    // The hall's file is saved; the kitchen's ends with the device.
    dashboard
        .post(
            &format!("/api/devices/{HALL}/ambient"),
            &json!({"enabled": false}),
        )
        .await;
    eventually("the hall's ambient file", || {
        (ambient_files(fixture.data(), HALL).0.len() == 1).then_some(())
    })
    .await;
    eventually("the kitchen's session recording", || {
        (!session_recordings(fixture.data(), KITCHEN).is_empty()).then_some(())
    })
    .await;
    let deleted = dashboard
        .send("DELETE", "/api/sounds/bell", "", Vec::new())
        .await;
    assert_eq!(deleted.status, 409, "the kitchen chose it");

    let (status, body) = dashboard
        .call("DELETE", &format!("/api/devices/{KITCHEN}"), None)
        .await;
    assert_eq!((status, &body["deleted"]), (200, &json!(KITCHEN)), "{body}");
    assert!(!fixture.data().join("ambient").join(KITCHEN).exists());
    assert!(session_recordings(fixture.data(), KITCHEN).is_empty());
    assert_eq!(ambient_files(fixture.data(), HALL).0.len(), 1);
    // The deleted device's choice no longer holds the sound.
    let deleted = dashboard
        .send("DELETE", "/api/sounds/bell", "", Vec::new())
        .await;
    assert_eq!(deleted.status, 200, "{:?}", deleted.json());
    // Its recordings stay gone: nothing wrote them again after the deletion.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!fixture.data().join("ambient").join(KITCHEN).exists());

    // It comes back as a new pending device, and once approved starts clean.
    drop(speaker);
    let socket = dial(plane.device_address(), KITCHEN, "/device/v1/control", None).await;
    let (mut sink, mut incoming) = socket.split();
    let mut capabilities: Vec<&str> = REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
    capabilities.push("wifi_control");
    let (text, _) = envelope(
        KITCHEN,
        None,
        "session.hello",
        json!({"capabilities": capabilities, "platform": "fireos6", "protocols": [1],
               "firmware_version": "fw-synthetic-2", "os_version": "synthetic",
               "privacy": {"muted": false, "capture_epoch": "3"},
               "volume": {"level": 64, "seeded": true}}),
    );
    sink.send(Message::text(text)).await.unwrap();
    let reply = within("the pending reply", next_json(&mut incoming))
        .await
        .expect("a reply");
    assert_eq!(reply["body"]["reason"], "pending_approval", "{reply}");
    drop((sink, incoming));
    let (status, body) = dashboard
        .post(
            &format!("/api/devices/{KITCHEN}/approve"),
            &json!({"label": "Kitchen again"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let device = dashboard.device(KITCHEN).await;
    assert_eq!(
        device["wifi"],
        json!({"pending": null, "last_result": null})
    );
    assert_eq!(device["collectMode"], false);
    assert_eq!(device["ambientMode"], false);
    let (status, recorder) = dashboard
        .get(&format!("/api/devices/{KITCHEN}/session_recorder"))
        .await;
    assert_eq!(status, 200, "{recorder}");
    assert_eq!(recorder["recordings"], json!([]));
    assert_eq!(recorder["status"]["state"], "off", "{recorder}");
    drop(other);
    shut_down(plane).await;
}
