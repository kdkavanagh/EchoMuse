//! The assembled controller on loopback ports, with a temporary data directory (a
//! database built from the contract schema holding one synthetic approved device, and
//! TLS credentials generated on first start), a simulated device speaking the real
//! wire through tokio-tungstenite, and a simulated Home Assistant that authenticates
//! the controller. Every row and every byte the device sends is synthetic.

use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use echomuse_config::{ControllerConfig, Environment};
use echomuse_controller::{
    control_plane::{ControlPlane, ControlPlaneOptions, SpeechSource},
    device::{DeviceRuntimeConfig, DeviceSnapshot, Lifecycle},
    supervision::Availability,
};
use echomuse_core::types::{DeviceId, QueueLimits};
use echomuse_db::OpenedDatabase;
use echomuse_esphome::{
    api::{self, ApiMessage},
    frame::{self, FrameDecoder},
};
use echomuse_firmware::{FirmwareTimings, Md5};
use echomuse_protocol::{
    REQUIRED_CAPABILITIES, Sha256, StreamId,
    ema::{self, Flags, Header, Kind, SourceMask},
};
use echomuse_voice::speech::testing::ToneModels;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

const WAIT: Duration = Duration::from_secs(10);
const DEVICE: &str = "G0SYNTH00A1B2C3D";
const PENDING: &str = "G0SYNTH00PEND1NG";
const DEVICE_TOKEN: &str = "synthetic-device-token-0000000000000000000";
const HA_TOKEN: &str = "synthetic-ha-token";
const RUNTIME_BYTES: &[u8] = b"synthetic armeabi-v7a libonnxruntime.so";
const BUNDLED_BYTES: &[u8] = b"\x7fELF synthetic bundled firmware";
const BUNDLED_VERSION: &str = "fw-synthetic-bundled";
const START_SCRIPT_BYTES: &[u8] = b"#!/system/bin/sh\n# synthetic start_server.sh\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

#[path = "support/ports.rs"]
mod ports;
use ports::{free_port, free_voice_port};

async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future).await.expect("in time")
}

/// A Home Assistant that accepts the controller's token and otherwise stays quiet.
struct FakeHa {
    address: SocketAddr,
    task: JoinHandle<()>,
}

impl FakeHa {
    async fn start() -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_ha(stream));
            }
        });
        Self { address, task }
    }
}

impl Drop for FakeHa {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_ha(stream: TcpStream) {
    let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let text = |value: Value| Message::text(value.to_string());
    if socket
        .send(text(
            json!({"type": "auth_required", "ha_version": "2026.8.1"}),
        ))
        .await
        .is_err()
    {
        return;
    }
    let Some(Ok(Message::Text(auth))) = socket.next().await else {
        return;
    };
    let auth: Value = serde_json::from_str(&auth).unwrap();
    if auth["access_token"] != HA_TOKEN {
        let _ = socket
            .send(text(
                json!({"type": "auth_invalid", "message": "bad token"}),
            ))
            .await;
        return;
    }
    if socket
        .send(text(json!({"type": "auth_ok", "ha_version": "2026.8.1"})))
        .await
        .is_err()
    {
        return;
    }
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Text(frame) = message {
            let request: Value = serde_json::from_str(&frame).unwrap();
            if request["type"] == "ping" {
                let pong = json!({"id": request["id"], "type": "pong"});
                if socket.send(text(pong)).await.is_err() {
                    return;
                }
            }
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    env: Environment,
    esphome_port: u16,
    _ha: FakeHa,
}

impl Fixture {
    async fn new() -> Self {
        Self::with_proxy(false).await
    }

    /// `bluetooth_proxy` switches the device's `bleProxyEnabled` on, as its own
    /// Bluetooth section.
    async fn with_proxy(bluetooth_proxy: bool) -> Self {
        // The controller's errors (a runtime failure, say) show with a failing test;
        // `RUST_LOG` widens them.
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("echomuse.db");
        drop(OpenedDatabase::open(&db_path).unwrap());
        let esphome_port = free_voice_port();
        let (config, sections) = if bluetooth_proxy {
            (r#"{"bleProxyEnabled": true}"#, r#"["bluetooth"]"#)
        } else {
            ("{}", "[]")
        };
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, label, approved, ip, firmware_ver, first_seen, \
             last_seen, token, esphome_api_port, config, config_sections) \
             VALUES (?1, 'Synthetic kitchen', 1, '192.0.2.10', 'fw-synthetic', 1600000000, \
             1600000000, ?2, ?3, ?4, ?5)",
            rusqlite::params![DEVICE, DEVICE_TOKEN, esphome_port, config, sections],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, approved, ip, first_seen, last_seen) \
             VALUES (?1, 0, '192.0.2.11', 1600000000, 1600000000)",
            [PENDING],
        )
        .unwrap();
        drop(conn);
        std::fs::write(dir.path().join("libonnxruntime.so"), RUNTIME_BYTES).unwrap();
        std::fs::create_dir_all(dir.path().join("static")).unwrap();
        let firmware = dir.path().join("firmware");
        std::fs::create_dir_all(&firmware).unwrap();
        std::fs::write(firmware.join("server"), BUNDLED_BYTES).unwrap();
        std::fs::write(firmware.join("version"), BUNDLED_VERSION).unwrap();
        let payloads = dir.path().join("device_payloads");
        std::fs::create_dir_all(&payloads).unwrap();
        std::fs::write(payloads.join("start_server.sh"), START_SCRIPT_BYTES).unwrap();

        let ha = FakeHa::start().await;
        let path = |name: &str| dir.path().join(name).display().to_string();
        let env: Environment = [
            ("SERVER_HOST", "127.0.0.1".to_owned()),
            ("SERVER_PORT", free_port().to_string()),
            ("SERVER_TLS_PORT", free_port().to_string()),
            ("API_PORT", free_port().to_string()),
            ("SERVER_IP", "127.0.0.1".to_owned()),
            ("MDNS_NAME", format!("echomuse-test-{}", std::process::id())),
            ("HA_URL", format!("http://{}", ha.address)),
            ("HA_TOKEN", HA_TOKEN.to_owned()),
            ("DB_PATH", path("echomuse.db")),
            ("ORT_ANDROID_LIB", path("libonnxruntime.so")),
            ("ESPHOME_PROJECT_VERSION", "2.0.0-test".to_owned()),
            // No such file: wake-model uploads are refused unless a test names one.
            ("ONNXRUNTIME_LIB", path("no-onnxruntime.so")),
            ("FIRMWARE_DIR", path("firmware")),
            ("DEVICE_PAYLOADS_DIR", path("device_payloads")),
            ("DEVICE_APPROVAL", "strict".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        Self {
            dir,
            env,
            esphome_port,
            _ha: ha,
        }
    }

    async fn start(&self) -> ControlPlane {
        self.start_with(DeviceRuntimeConfig::default()).await
    }

    async fn start_with(&self, runtime: DeviceRuntimeConfig) -> ControlPlane {
        let config = ControllerConfig::from_env(&self.env, Some("test")).unwrap();
        let options = ControlPlaneOptions {
            config,
            static_dir: self.dir.path().join("static"),
            wake_seed_graph: repo_root().join("bcresnet_audio.onnx"),
            wake_seed_sidecar: repo_root().join("bcresnet_audio.json"),
            qualifier_program: PathBuf::from(env!("CARGO_BIN_EXE_echomuse-controller")),
            runtime,
            firmware_timings: FirmwareTimings {
                shell_open: WAIT,
                command: WAIT,
                chunk: WAIT,
                verify: WAIT,
                reconnect: WAIT,
                debloat_idle: WAIT,
                debloat_total: WAIT,
            },
            speech: SpeechSource::Provided(Arc::new(ToneModels::new(&["ophelia"]))),
            ffmpeg: PathBuf::from("/usr/bin/ffmpeg"),
            supervision: Default::default(),
        };
        within(ControlPlane::start(options)).await.unwrap()
    }
}

fn device_snapshots(plane: &ControlPlane) -> watch::Receiver<DeviceSnapshot> {
    plane
        .registry()
        .directory()
        .get(&DeviceId(DEVICE.to_owned()))
        .expect("the approved device has a runtime")
        .subscribe()
}

async fn wait_for(
    snapshots: &mut watch::Receiver<DeviceSnapshot>,
    what: &str,
    check: impl FnMut(&DeviceSnapshot) -> bool,
) -> DeviceSnapshot {
    let waited = tokio::time::timeout(WAIT, snapshots.wait_for(check))
        .await
        .map(|changed| changed.map(|state| state.clone()));
    match waited {
        Ok(changed) => {
            changed.unwrap_or_else(|_| panic!("the device's snapshots ended waiting for {what}"))
        }
        Err(_) => {
            let state = snapshots.borrow();
            panic!(
                "timed out waiting for {what}; the device is at runtime {:?}, {:?}, session \
                 {:?}, failure {:?}, health {:?}, link {:?}",
                state.runtime,
                state.lifecycle,
                state.session,
                state.failure,
                state.health,
                state.link.as_ref().map(|link| (
                    &link.audio_accepted,
                    link.ambient_lux,
                    &link.unhandled,
                    link.volume
                )),
            )
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The simulated device's control socket. A background task writes everything it
/// sends, heartbeats included, so reading never stalls the heartbeat.
struct Control {
    outgoing: mpsc::UnboundedSender<Message>,
    incoming: futures_util::stream::SplitStream<Socket>,
    session: String,
    ready: Value,
    heartbeat: JoinHandle<()>,
}

impl Drop for Control {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

fn envelope(message_type: &str, session: Option<&str>, body: Value) -> String {
    json!({
        "protocol": 1,
        "type": message_type,
        "session_id": session,
        "message_id": uuid::Uuid::new_v4().to_string(),
        "device_id": DEVICE,
        "generation": 0,
        "body": body,
    })
    .to_string()
}

fn hello(extra: &[&str]) -> Value {
    hello_running(extra, "fw-synthetic-2")
}

fn hello_running(extra: &[&str], version: &str) -> Value {
    let mut capabilities: Vec<&str> = REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
    capabilities.extend(["ambient_light", "button_hold", "a_future_capability"]);
    capabilities.extend(extra);
    json!({
        "capabilities": capabilities,
        "platform": "fireos6",
        "protocols": [1],
        "firmware_version": version,
        "os_version": "Fire OS 6 (synthetic)",
        "privacy": {"muted": false, "capture_epoch": "7"},
        "volume": {"level": 64, "seeded": true},
    })
}

async fn dial(address: SocketAddr, path: &str, session: Option<&str>) -> Socket {
    let mut request = format!("ws://{address}{path}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("X-EM-Token", HeaderValue::from_static(DEVICE_TOKEN));
    if let Some(session) = session {
        request
            .headers_mut()
            .insert("X-EM-Session", HeaderValue::from_str(session).unwrap());
    }
    within(connect_async(request)).await.unwrap().0
}

impl Control {
    async fn connect(address: SocketAddr) -> Self {
        Self::connect_with(address, &[]).await
    }

    /// Connects with `extra` capabilities announced besides the usual ones.
    async fn connect_with(address: SocketAddr, extra: &[&str]) -> Self {
        Self::connect_running(address, extra, "fw-synthetic-2").await
    }

    /// Connects as firmware `version` with `extra` capabilities.
    async fn connect_running(address: SocketAddr, extra: &[&str], version: &str) -> Self {
        Self::connect_hello(address, hello_running(extra, version)).await
    }

    /// Connects with the hello `body`.
    async fn connect_hello(address: SocketAddr, body: Value) -> Self {
        let socket = dial(address, "/device/v1/control", None).await;
        let (mut sink, mut incoming) = socket.split();
        sink.send(Message::text(envelope("session.hello", None, body)))
            .await
            .unwrap();
        let ready = within(next_json(&mut incoming)).await.expect("a reply");
        assert_eq!(ready["type"], "session.ready", "{ready}");
        let session = ready["body"]["session_id"].as_str().unwrap().to_owned();
        let (outgoing, mut queue) = mpsc::unbounded_channel::<Message>();
        let writer_session = session.clone();
        let beats = outgoing.clone();
        let heartbeat = tokio::spawn(async move {
            let writer = tokio::spawn(async move {
                while let Some(message) = queue.recv().await {
                    if sink.send(message).await.is_err() {
                        return;
                    }
                }
                let _ = sink.close().await;
            });
            let mut mono = 1_u64;
            loop {
                let beat = envelope(
                    "heartbeat",
                    Some(&writer_session),
                    json!({"mono_ns": mono.to_string()}),
                );
                if beats.send(Message::text(beat)).is_err() {
                    break;
                }
                mono += 500_000_000;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            writer.abort();
        });
        Self {
            outgoing,
            incoming,
            session,
            ready,
            heartbeat,
        }
    }

    fn send(&self, message_type: &str, body: Value) {
        let text = envelope(message_type, Some(&self.session), body);
        self.outgoing.send(Message::text(text)).unwrap();
    }

    /// The next controller message that is not a heartbeat; `None` once closed.
    async fn next_message(&mut self) -> Option<Value> {
        loop {
            let message = next_json(&mut self.incoming).await?;
            if message["type"] != "heartbeat" {
                return Some(message);
            }
        }
    }

    /// The next controller message of `kind`, skipping others.
    async fn next_of(&mut self, kind: &str) -> Value {
        within(async {
            loop {
                let message = self.next_message().await.expect("the socket closed");
                if message["type"] == kind {
                    return message;
                }
            }
        })
        .await
    }

    /// Sends `clock.request` and waits for its reply. Everything the device sent on
    /// this socket before it has then been queued in order.
    async fn sync(&mut self) {
        let nonce = uuid::Uuid::new_v4().to_string();
        self.send("clock.request", json!({"nonce": nonce}));
        loop {
            let message = within(self.next_message()).await.expect("socket open");
            if message["type"] == "clock.reply" && message["body"]["nonce"] == nonce {
                return;
            }
        }
    }

    /// Whether the controller closes the socket within the wait.
    async fn closed(&mut self) -> bool {
        tokio::time::timeout(WAIT, async { while self.next_message().await.is_some() {} })
            .await
            .is_ok()
    }
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

fn mic_packet(epoch: u64, sequence: u64, first_sample: u64) -> Bytes {
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

/// A Home Assistant client of the ESPHome satellite.
struct EsphomeClient {
    stream: TcpStream,
    decoder: FrameDecoder,
}

impl EsphomeClient {
    async fn connect(address: SocketAddr) -> Self {
        Self {
            stream: within(TcpStream::connect(address)).await.unwrap(),
            decoder: FrameDecoder::new(),
        }
    }

    async fn send<M: ApiMessage>(&mut self, message: &M) {
        self.stream
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
    }

    async fn recv<M: ApiMessage>(&mut self) -> M {
        within(async {
            loop {
                if let Some(frame) = self.decoder.next_frame().unwrap() {
                    if frame.message_type == M::ID {
                        return frame.decode::<M>().unwrap();
                    }
                    continue;
                }
                let read = self
                    .stream
                    .read_buf(self.decoder.buffer_mut())
                    .await
                    .unwrap();
                assert_ne!(read, 0, "the node closed the connection");
            }
        })
        .await
    }
}

fn sha256_file(path: &Path) -> String {
    Sha256::of(&std::fs::read(path).unwrap())
        .as_str()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_attaches_streams_mutes_reconnects_and_detaches() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let mut ha = plane.home_assistant();
    within(ha.wait_for(|status| status.generation.is_some()))
        .await
        .unwrap();
    let mut snapshots = device_snapshots(&plane);
    assert_eq!(snapshots.borrow().lifecycle, Lifecycle::Ready);

    // Connect: session.ready names the runtime, graph and sidecar the device loads.
    let mut control = Control::connect(plane.device_address()).await;
    let body = &control.ready["body"];
    assert_eq!(body["protocol"], 1);
    assert_eq!(
        body["assets"]["runtime_sha256"],
        Sha256::of(RUNTIME_BYTES).as_str()
    );
    assert_eq!(
        body["assets"]["graph_sha256"],
        sha256_file(&repo_root().join("bcresnet_audio.onnx"))
    );
    assert_eq!(
        body["assets"]["sidecar_sha256"],
        sha256_file(&repo_root().join("bcresnet_audio.json"))
    );
    let detector = &body["detector"];
    assert_eq!(detector["hop_blocks"], 2);
    assert_eq!(detector["smoothing"], 3);
    assert_eq!(detector["clear_after_unscored"], 6);
    assert_eq!(detector["provisional_duck"]["max_per_window"], 2);
    assert_eq!(detector["provisional_duck"]["window_ms"], 5000);
    for threshold in ["idle", "playback", "near_miss"] {
        let value = detector["thresholds"][threshold].as_f64().unwrap();
        assert!(value > 0.0 && value <= 1.0, "{threshold} {value}");
    }
    // Rules go only to open_rules_v1 devices, afe_metadata only to afe_metadata_v1 ones.
    assert!(detector.get("open_rules").is_none());
    assert!(body.get("afe_metadata").is_none());
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    let attached = wait_for(&mut snapshots, "the session to attach", |s| {
        s.lifecycle == Lifecycle::Attached && s.link.is_some()
    })
    .await;
    let link = attached.link.as_ref().unwrap();
    assert_eq!(link.session_id.as_str(), control.session);
    assert_eq!(attached.muted, Some(false));
    assert_eq!(link.firmware_version.as_deref(), Some("fw-synthetic-2"));
    let first_session = attached.session.unwrap();

    // The voice satellite listens on the device's port and answers as the device.
    let listening = wait_for(&mut snapshots, "the satellite to listen", |s| {
        s.esphome.voice_listening.is_some() && s.esphome.voice_advertised
    })
    .await;
    assert_eq!(listening.esphome.voice_port, Some(fixture.esphome_port));
    let mut ha_client = EsphomeClient::connect(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        fixture.esphome_port,
    )))
    .await;
    ha_client
        .send(&api::HelloRequest {
            client_info: "Home Assistant 2026.8.1".to_owned(),
            api_version_major: 1,
            api_version_minor: 14,
        })
        .await;
    let hello = ha_client.recv::<api::HelloResponse>().await;
    // `echomuse-` and the last 12 characters of the device id, lowercased.
    assert_eq!(hello.name, "echomuse-nth00a1b2c3d");
    // An announcement goes to the attached session's voice path, which cannot play
    // audio from a host that does not resolve: Home Assistant is told it failed.
    ha_client
        .send(&api::VoiceAssistantAnnounceRequest {
            media_id: "http://ha.invalid/api/tts_proxy/synthetic.flac".to_owned(),
            text: "Synthetic".to_owned(),
            preannounce_media_id: String::new(),
            start_conversation: false,
        })
        .await;
    assert_eq!(
        ha_client
            .recv::<api::VoiceAssistantAnnounceFinished>()
            .await,
        api::VoiceAssistantAnnounceFinished { success: false }
    );
    // It was attempted, not refused as unavailable.
    assert_eq!(snapshots.borrow().esphome.refused.get("announce"), None);
    drop(ha_client);

    // stream.open, then packets in sequence are accepted.
    let mut audio = _audio;
    control.send(
        "stream.open",
        json!({"stream_id": "mic", "epoch": "9", "kind": 1, "sample_rate": 16000,
               "format": 1, "reason": "start"}),
    );
    control.sync().await;
    audio
        .send(Message::Binary(mic_packet(9, 0, 0)))
        .await
        .unwrap();
    audio
        .send(Message::Binary(mic_packet(9, 1, 160)))
        .await
        .unwrap();
    // Telemetry lands in the snapshot; a message type no part of the controller acts on
    // is counted. (Wake messages go to the voice path, Wi-Fi reports to the Wi-Fi
    // service.)
    control.send("volume_state", json!({"level": 100}));
    control.send("ambient_light", json!({"lux": 12}));
    control.send("pong", json!({}));
    let streamed = wait_for(&mut snapshots, "the packets to be counted", |s| {
        s.link.as_ref().is_some_and(|link| {
            link.audio_accepted.get(&StreamId::Mic) == Some(&2)
                && link.ambient_lux == Some(12)
                && link.unhandled.get("pong") == Some(&1)
        })
    })
    .await;
    assert_eq!(
        streamed.link.as_ref().unwrap().volume,
        Some(echomuse_controller::device::Volume::Level(100))
    );

    // Mute travels the urgent path: the capture scope is revoked; unmute grants a
    // fresh one and never revives the old one.
    let scope = streamed.capture.clone().expect("a capture scope");
    assert!(!scope.is_cancelled());
    control.send(
        "privacy.changed",
        json!({"muted": true, "capture_epoch": null, "physical_seq": 1}),
    );
    let muted = wait_for(&mut snapshots, "the mute", |s| s.muted == Some(true)).await;
    assert!(scope.is_cancelled());
    // While muted there is no live capture scope.
    assert!(muted.capture.as_ref().unwrap().is_cancelled());
    // An event whose physical number is not above the last one is dropped.
    control.send(
        "privacy.changed",
        json!({"muted": false, "capture_epoch": "8", "physical_seq": 1}),
    );
    control.sync().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(snapshots.borrow().muted, Some(true));
    control.send(
        "privacy.changed",
        json!({"muted": false, "capture_epoch": "8", "physical_seq": 2}),
    );
    let unmuted = wait_for(&mut snapshots, "the unmute", |s| s.muted == Some(false)).await;
    let fresh = unmuted.capture.clone().unwrap();
    assert!(!fresh.is_cancelled());
    assert!(scope.is_cancelled());

    // A new hello replaces the session; the old socket is closed, and its end
    // cannot detach the new session.
    let mut second = Control::connect(plane.device_address()).await;
    assert_ne!(second.session, control.session);
    assert!(
        control.closed().await,
        "the replaced session was not closed"
    );
    let mut audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&second.session),
    )
    .await;
    let replaced = wait_for(&mut snapshots, "the replacement", |s| {
        s.link
            .as_ref()
            .is_some_and(|link| link.session_id.as_str() == second.session)
    })
    .await;
    assert_ne!(replaced.session, Some(first_session));
    assert!(fresh.is_cancelled());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(snapshots.borrow().lifecycle, Lifecycle::Attached);
    assert!(snapshots.borrow().esphome.voice_listening.is_some());

    // Audio for an epoch never opened in this session ends it with unknown_epoch.
    audio
        .send(Message::Binary(mic_packet(9, 2, 320)))
        .await
        .unwrap();
    let error = loop {
        let message = within(second.next_message())
            .await
            .expect("a protocol.error");
        if message["type"] == "protocol.error" {
            break message;
        }
    };
    assert_eq!(error["body"]["code"], "unknown_epoch", "{error}");
    assert!(second.closed().await);

    // Detached: the satellite stops listening, its record stays published.
    let detached = wait_for(&mut snapshots, "the detach", |s| {
        s.lifecycle == Lifecycle::Ready && s.esphome.voice_listening.is_none()
    })
    .await;
    assert!(detached.link.is_none());
    assert!(detached.capture.is_none());
    assert!(detached.esphome.voice_advertised);
    assert_eq!(detached.esphome.voice_port, Some(fixture.esphome_port));
    let refused = within(async {
        loop {
            if TcpStream::connect((Ipv4Addr::LOCALHOST, fixture.esphome_port))
                .await
                .is_err()
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(refused);

    // Shutdown completes in order and within its deadline.
    let started = Instant::now();
    let report = plane.shutdown().await;
    assert!(report.is_clean(), "{:?}", report.problems);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hello_that_does_not_report_unmuted_grants_no_capture_until_the_device_unmutes() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let mut snapshots = device_snapshots(&plane);

    // A muted hello attaches with no live capture scope.
    let mut muted_hello = hello(&[]);
    muted_hello["privacy"] = json!({"muted": true, "capture_epoch": null});
    let control = Control::connect_hello(plane.device_address(), muted_hello).await;
    let attached = wait_for(&mut snapshots, "the muted session", |s| {
        s.link
            .as_ref()
            .is_some_and(|link| link.session_id.as_str() == control.session)
    })
    .await;
    assert_eq!(attached.muted, Some(true));
    let refused = attached.capture.clone().expect("a capture scope");
    assert!(refused.is_cancelled());

    // The device's unmute grants a fresh scope; the refused one stays revoked.
    control.send(
        "privacy.changed",
        json!({"muted": false, "capture_epoch": "8", "physical_seq": 1}),
    );
    let unmuted = wait_for(&mut snapshots, "the unmute", |s| s.muted == Some(false)).await;
    assert!(!unmuted.capture.as_ref().unwrap().is_cancelled());
    assert!(refused.is_cancelled());

    // A hello that does not say whether the mute is off attaches muted too.
    let mut silent_hello = hello(&[]);
    silent_hello.as_object_mut().unwrap().remove("privacy");
    let replacement = Control::connect_hello(plane.device_address(), silent_hello).await;
    let attached = wait_for(
        &mut snapshots,
        "the session without a privacy report",
        |s| {
            s.link
                .as_ref()
                .is_some_and(|link| link.session_id.as_str() == replacement.session)
        },
    )
    .await;
    assert_eq!(attached.muted, None);
    assert!(attached.capture.as_ref().unwrap().is_cancelled());
    drop((control, replacement));
    assert!(plane.shutdown().await.is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_attach_the_runtime_takes_up_late_is_declined_and_the_runtime_keeps_running() {
    let fixture = Fixture::new().await;
    // Every command waits past its age limit, as on a starved host.
    let plane = fixture
        .start_with(DeviceRuntimeConfig {
            ingress: QueueLimits {
                max_age: Duration::from_nanos(1),
                ..QueueLimits::default()
            },
            ..DeviceRuntimeConfig::default()
        })
        .await;
    let snapshots = device_snapshots(&plane);
    let before = snapshots.borrow().clone();

    let socket = dial(plane.device_address(), "/device/v1/control", None).await;
    let (mut sink, mut incoming) = socket.split();
    sink.send(Message::text(envelope("session.hello", None, hello(&[]))))
        .await
        .unwrap();
    // The controller declines by closing the socket; the device dials again later.
    assert_eq!(within(next_json(&mut incoming)).await, None);

    let after = snapshots.borrow().clone();
    assert_eq!(after.runtime, before.runtime, "the runtime was not rebuilt");
    assert_eq!(after.lifecycle, Lifecycle::Ready);
    assert_eq!(after.failure, None);
    assert_eq!(after.session, None);
    assert_eq!(after.health.availability, Availability::Available);
    assert!(plane.shutdown().await.is_clean());
}

/// One HTTP/1.1 request with an optional bearer token and JSON body; returns the status
/// and the JSON response body (`null` when empty).
async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let body = body.map(Value::to_string).unwrap_or_default();
    let authorization = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    within(stream.read_to_end(&mut response)).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").expect("a whole response");
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(body).unwrap_or_else(|_| panic!("{head}\n{body}"))
    };
    (status, body)
}

/// Runs dashboard setup with the printed token and returns the session token.
async fn sign_in(plane: &ControlPlane) -> String {
    let token = plane.setup_token().expect("no user exists yet").to_owned();
    let (status, setup) = http(
        plane.api_address(),
        "POST",
        "/api/setup",
        None,
        Some(&json!({"token": token, "username": "admin", "password": "synthetic password"})),
    )
    .await;
    assert_eq!((status, &setup["role"]), (200, &json!("admin")), "{setup}");
    setup["token"].as_str().unwrap().to_owned()
}

type EventSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn events(plane: &ControlPlane, session: &str) -> EventSocket {
    connect_async(format!(
        "ws://{}/api/events?token={session}",
        plane.api_address()
    ))
    .await
    .unwrap()
    .0
}

/// The next event of `kind`, with every event passed on the way.
async fn next_event(socket: &mut EventSocket, kind: &str) -> (Value, Vec<Value>) {
    let mut seen = Vec::new();
    within(async {
        loop {
            if let Some(Ok(Message::Text(text))) = socket.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["type"] == kind {
                    return (value, seen);
                }
                seen.push(value);
            }
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dashboard_sees_the_fleet_and_approval_starts_a_runtime() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let session = sign_in(&plane).await;

    let mut snapshots = device_snapshots(&plane);
    let control = Control::connect(plane.device_address()).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    wait_for(&mut snapshots, "the session to attach", |s| {
        s.link.is_some() && s.esphome.voice_listening.is_some()
    })
    .await;

    let mut socket = events(&plane, &session).await;
    let (snapshot, _) = next_event(&mut socket, "snapshot").await;
    let devices: BTreeMap<String, Value> = snapshot["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|device| {
            (
                device["device_id"].as_str().unwrap().to_owned(),
                device.clone(),
            )
        })
        .collect();
    let device = &devices[DEVICE];
    assert_eq!(device["connected"], true, "{device}");
    assert_eq!(device["approved"], true);
    assert_eq!(device["label"], "Synthetic kitchen");
    assert_eq!(device["platform"], "fireos6");
    assert_eq!(device["muted"], false);
    assert_eq!(device["linkTls"], false);
    assert_eq!(device["firmware_ver"], "fw-synthetic-2");
    assert_eq!(device["esphome_port"], fixture.esphome_port);
    assert!(
        device["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("a_future_capability"))
    );
    let pending = &devices[PENDING];
    assert_eq!(pending["approved"], false);
    assert_eq!(pending["connected"], false);
    assert_eq!(pending["muted"], Value::Null);

    // GET /api/devices joins the row, the settings and the runtime.
    let (status, listed) = http(
        plane.api_address(),
        "GET",
        "/api/devices",
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, 200, "{listed}");
    let listed = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == DEVICE)
        .unwrap()
        .clone();
    assert_eq!(listed["connected"], true, "{listed}");
    assert!(listed["config"].is_object(), "{listed}");
    // A Dot that did not announce the Dot-only capabilities has gaps for them.
    assert!(listed["capability_gaps"].is_object(), "{listed}");
    assert!(
        listed["capability_gaps"].get("wifi_control").is_some(),
        "{listed}"
    );

    // Approving the pending device through the dashboard starts its runtime and is
    // announced once.
    let (status, approved) = http(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{PENDING}/approve"),
        Some(&session),
        Some(&json!({"label": "Synthetic office"})),
    )
    .await;
    assert_eq!(status, 200, "{approved}");
    assert_eq!(approved["approved"], true);
    assert!(
        plane
            .registry()
            .directory()
            .contains_key(&DeviceId(PENDING.to_owned()))
    );
    let (announced, _) = next_event(&mut socket, "device_approved").await;
    assert_eq!(announced["device_id"], PENDING);

    // The device's disconnect reaches the dashboard, and nobody announced the approval
    // a second time on the way.
    drop(control);
    drop(_audio);
    let (disconnected, between) = next_event(&mut socket, "device_disconnected").await;
    assert_eq!(disconnected["device_id"], DEVICE);
    assert!(
        !between.iter().any(|e| e["type"] == "device_approved"),
        "{between:?}"
    );

    let report = plane.shutdown().await;
    assert!(report.is_clean(), "{:?}", report.problems);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_setting_that_needs_a_new_session_renegotiates_it() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let session = sign_in(&plane).await;
    let mut snapshots = device_snapshots(&plane);
    let mut control = Control::connect(plane.device_address()).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    wait_for(&mut snapshots, "the session to attach", |s| {
        s.link.is_some()
    })
    .await;
    assert_eq!(
        control.ready["body"]["detector"]["thresholds"]["near_miss"],
        0.17
    );

    // The near-miss floor reaches a device only through a new session.ready.
    let (status, body) = http(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{DEVICE}/config"),
        Some(&session),
        Some(&json!({"config_sections": ["wakeword"], "nearMissThreshold": 0.3})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert!(control.closed().await, "the session was not renegotiated");
    let again = Control::connect(plane.device_address()).await;
    assert_eq!(
        again.ready["body"]["detector"]["thresholds"]["near_miss"],
        0.3
    );
    assert!(plane.shutdown().await.is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_device_stops_it_and_removes_what_is_kept_for_it() {
    let fixture = Fixture::new().await;
    let data = fixture.dir.path();
    let db_path = data.join("echomuse.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for (device, ts) in [(DEVICE, 1), (PENDING, 2)] {
            conn.execute(
                "INSERT INTO device_logs (device_id, ts, level, source, message) \
                 VALUES (?1, ?2, 'info', 'device', 'synthetic')",
                rusqlite::params![device, ts],
            )
            .unwrap();
        }
    }
    let files = [
        format!("recordings/{DEVICE}_1.wav"),
        format!("turn_recordings/{DEVICE}_2.wav"),
        format!("nearmiss_recordings/{DEVICE}_3.wav"),
        format!("wakes/{DEVICE}/4.wav"),
        format!("samples/{DEVICE}/a.wav"),
        format!("ambient/{DEVICE}/b.wav"),
    ];
    let others = [
        format!("recordings/{PENDING}_1.wav"),
        format!("wakes/{PENDING}/4.wav"),
    ];
    // Written once the controller runs: at startup the records service removes every
    // recording without a row, and these have none.
    let plane = fixture.start().await;
    for file in files.iter().chain(&others) {
        let path = data.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"RIFF synthetic").unwrap();
    }
    let session = sign_in(&plane).await;
    let mut snapshots = device_snapshots(&plane);
    let mut control = Control::connect(plane.device_address()).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    wait_for(&mut snapshots, "the satellite to listen", |s| {
        s.esphome.voice_listening.is_some()
    })
    .await;

    let (status, body) = http(
        plane.api_address(),
        "DELETE",
        &format!("/api/devices/{DEVICE}"),
        Some(&session),
        None,
    )
    .await;
    assert_eq!((status, &body["deleted"]), (200, &json!(DEVICE)), "{body}");
    assert!(control.closed().await, "the session was not closed");
    assert!(
        !plane
            .registry()
            .directory()
            .contains_key(&DeviceId(DEVICE.to_owned()))
    );
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, fixture.esphome_port))
            .await
            .is_err()
    );
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count = |sql: &str, device: &str| -> i64 {
        conn.query_row(sql, [device], |row| row.get(0)).unwrap()
    };
    assert_eq!(
        count("SELECT count(*) FROM devices WHERE device_id = ?1", DEVICE),
        0
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM device_logs WHERE device_id = ?1",
            DEVICE
        ),
        0
    );
    assert_eq!(
        count("SELECT count(*) FROM devices WHERE device_id = ?1", PENDING),
        1
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM device_logs WHERE device_id = ?1",
            PENDING
        ),
        1
    );
    for file in &files {
        assert!(!data.join(file).exists(), "{file} survived");
    }
    for file in &others {
        assert!(data.join(file).exists(), "{file} was deleted");
    }
    let (status, _) = http(
        plane.api_address(),
        "GET",
        &format!("/api/devices/{DEVICE}"),
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, 404);

    // The deleted device may come back, as a new pending device.
    let socket = dial(plane.device_address(), "/device/v1/control", None).await;
    let (mut sink, mut incoming) = socket.split();
    sink.send(Message::text(envelope("session.hello", None, hello(&[]))))
        .await
        .unwrap();
    let reply = within(next_json(&mut incoming)).await.expect("a reply");
    assert_eq!(reply["body"]["reason"], "pending_approval", "{reply}");
    assert!(plane.shutdown().await.is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_upload_without_a_usable_onnx_runtime_is_refused_with_the_reason() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let refused = plane
        .notes()
        .wake_uploads_refused
        .clone()
        .expect("ONNXRUNTIME_LIB points at no file");
    assert!(refused.contains("ONNXRUNTIME_LIB"), "{refused}");
    let session = sign_in(&plane).await;
    let (status, body) = upload_model(&plane, &session).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["code"], "model_rejected");
    assert!(
        body["error"].as_str().unwrap().contains("ONNXRUNTIME_LIB"),
        "{body}"
    );
    assert!(plane.shutdown().await.is_clean());
}

/// Qualifies a real upload in the two child processes. Needs the official ONNX Runtime
/// 1.20.1 library, named by `ECHOMUSE_ORT_LIB`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs ECHOMUSE_ORT_LIB"]
async fn an_uploaded_model_is_qualified_in_child_processes() {
    let library = std::env::var("ECHOMUSE_ORT_LIB").expect("ECHOMUSE_ORT_LIB");
    let mut fixture = Fixture::new().await;
    fixture.env.insert("ONNXRUNTIME_LIB".to_owned(), library);
    let plane = fixture.start().await;
    assert_eq!(plane.notes().wake_uploads_refused, None);
    let session = sign_in(&plane).await;
    let (status, body) = upload_model(&plane, &session).await;
    // The deployed graph seeds the registry, so the same bytes come back unchanged;
    // a copy with a different sidecar is a new model and runs the children.
    assert_eq!(status, 200, "{body}");
    let probe = &body["model"]["probe"];
    for score in ["silence", "noise", "tone"] {
        let value = probe[score].as_f64().unwrap_or(-1.0);
        assert!((0.0..=1.0).contains(&value), "{score}: {probe}");
    }
    assert!(plane.shutdown().await.is_clean());
}

/// Uploads the deployed graph with one extra protobuf field that ONNX Runtime ignores
/// (field 1000, varint 1), so the registry, which names a model by its graph's hash,
/// sees a new model that still runs.
async fn upload_model(plane: &ControlPlane, session: &str) -> (u16, Value) {
    let mut graph = std::fs::read(repo_root().join("bcresnet_audio.onnx")).unwrap();
    graph.extend_from_slice(&[0xC0, 0x3E, 0x01]);
    let sidecar = std::fs::read(repo_root().join("bcresnet_audio.json")).unwrap();
    let boundary = "synthetic-boundary-7f3a";
    let mut body = Vec::new();
    let mut file = |name: &str, bytes: &[u8]| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; \
                 filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    };
    file("graph", &graph);
    file("sidecar", &sidecar);
    for (name, value) in [
        ("idle", "0.9"),
        ("playback", "0.65"),
        ("reference", "0.6"),
        ("wake_phrase", "alexa"),
        ("verify_core", "alexa"),
    ] {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let address = plane.api_address();
    let head = format!(
        "POST /api/wake_models/upload HTTP/1.1\r\nHost: {address}\r\n\
         Authorization: Bearer {session}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut stream = within(TcpStream::connect(address)).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(&body).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(150), stream.read_to_end(&mut response))
        .await
        .expect("an answer")
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_keeps_the_tls_credentials_and_port() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    assert!(plane.device_tls_address().is_some());
    let tls = plane.notes().tls.clone().unwrap();
    assert!(tls.created_ca);
    let ca = std::fs::read(fixture.dir.path().join("tls/ca.pem")).unwrap();
    assert!(plane.shutdown().await.is_clean());

    let plane = fixture.start().await;
    assert!(!plane.notes().tls.clone().unwrap().created_ca);
    assert_eq!(
        std::fs::read(fixture.dir.path().join("tls/ca.pem")).unwrap(),
        ca
    );
    // A user now exists only if setup ran; it did not, so the token is offered again.
    assert!(plane.setup_token().is_some());
    assert!(plane.shutdown().await.is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dot_with_its_proxy_on_forwards_advertisements_to_home_assistant() {
    let fixture = Fixture::with_proxy(true).await;
    let plane = fixture.start().await;
    let mut snapshots = device_snapshots(&plane);
    let control = Control::connect_with(plane.device_address(), &["ble_proxy"]).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    let proxy_port = fixture.esphome_port + 1000;
    let running = wait_for(&mut snapshots, "the proxy to listen", |s| {
        s.esphome.proxy_listening.is_some() && s.esphome.proxy_advertised
    })
    .await;
    assert_eq!(running.esphome.proxy_port, Some(proxy_port));

    let mut ha_client =
        EsphomeClient::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, proxy_port))).await;
    ha_client
        .send(&api::HelloRequest {
            client_info: "Home Assistant 2026.8.1".to_owned(),
            api_version_major: 1,
            api_version_minor: 14,
        })
        .await;
    assert_eq!(
        ha_client.recv::<api::HelloResponse>().await.name,
        "echomuse-nth00a1b2c3d-bt"
    );
    ha_client
        .send(&api::SubscribeBluetoothLeAdvertisementsRequest { flags: 1 })
        .await;
    // The subscription lands asynchronously; the device keeps reporting until Home
    // Assistant receives one.
    let adverts = json!({"adverts": [
        {"addr": "AA:BB:CC:DD:EE:FF", "addrType": 1, "rssi": -60, "data": "AgE="}
    ]});
    let reporter = {
        let outgoing = control.outgoing.clone();
        let session = control.session.clone();
        tokio::spawn(async move {
            loop {
                let text = envelope("ble_adverts", Some(&session), adverts.clone());
                if outgoing.send(Message::text(text)).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };
    let forwarded = ha_client
        .recv::<api::BluetoothLeRawAdvertisementsResponse>()
        .await;
    reporter.abort();
    let advert = &forwarded.advertisements[0];
    assert_eq!(advert.address, 0xAABB_CCDD_EEFF);
    assert_eq!(advert.rssi, -60);
    assert_eq!(advert.address_type, 1);
    assert_eq!(advert.data.as_ref(), &[2, 1]);

    drop(control);
    drop(_audio);
    let detached = wait_for(&mut snapshots, "the detach", |s| {
        s.lifecycle == Lifecycle::Ready && s.esphome.proxy_listening.is_none()
    })
    .await;
    assert!(detached.esphome.proxy_advertised);
    assert!(plane.shutdown().await.is_clean());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_finished_timer_rings_on_the_speaker() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let mut snapshots = device_snapshots(&plane);
    let mut control = Control::connect_with(plane.device_address(), &["timer_view_v1"]).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    // The alert service attached the session once it posts the (empty) timer view.
    let view = control.next_of("timer.view").await;
    assert_eq!(view["body"]["timers"], json!([]), "{view}");
    wait_for(&mut snapshots, "the satellite to listen", |s| {
        s.esphome.voice_listening.is_some()
    })
    .await;

    let mut ha_client = EsphomeClient::connect(SocketAddr::from((
        Ipv4Addr::LOCALHOST,
        fixture.esphome_port,
    )))
    .await;
    ha_client
        .send(&api::HelloRequest {
            client_info: "Home Assistant 2026.8.1".to_owned(),
            api_version_major: 1,
            api_version_minor: 14,
        })
        .await;
    ha_client.recv::<api::HelloResponse>().await;
    for event in [
        api::VoiceAssistantTimerEvent::VoiceAssistantTimerStarted,
        api::VoiceAssistantTimerEvent::VoiceAssistantTimerFinished,
    ] {
        ha_client
            .send(&api::VoiceAssistantTimerEventResponse {
                event_type: event as i32,
                timer_id: "01JSYNTHTIMER".to_owned(),
                name: "pasta".to_owned(),
                total_seconds: 600,
                seconds_left: 0,
                is_active: true,
            })
            .await;
    }
    let ring = control.next_of("alert.ring").await;
    let body = &ring["body"];
    assert_eq!(body["ring_id"], "01JSYNTHTIMER", "{ring}");
    assert_eq!(body["name"], "pasta");
    assert_eq!(body["sound"], "builtin:fallback");
    assert!(body["loop_gap_ms"].as_u64().is_some(), "{ring}");
    assert!(body["max_ring_ms"].as_u64().unwrap() > 0, "{ring}");
    control.send(
        "command.ack",
        json!({"message_id": ring["message_id"], "status": "accepted", "error": null}),
    );
    control.sync().await;
    assert!(plane.shutdown().await.is_clean());
}

/// A simulated device's `/data/local/bin`, for the shell commands firmware delivery
/// sends, answered as mksh with toybox answers them.
struct DeviceFiles {
    files: BTreeMap<String, (Vec<u8>, u16)>,
    link: String,
    unknown: Vec<String>,
}

impl DeviceFiles {
    fn new() -> Self {
        let mut files = BTreeMap::new();
        files.insert(
            "/data/local/bin/server_a".to_owned(),
            (b"\x7fELF synthetic running build".to_vec(), 0o755),
        );
        Self {
            files,
            link: "server_a".to_owned(),
            unknown: Vec::new(),
        }
    }
}

fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let len = text[from..].find(end)?;
    Some(&text[from..from + len])
}

/// What a pipe shell session ended with.
enum ShellEnd {
    /// The firmware was killed after `server` was pointed at this slot.
    Switched(String),
    /// The controller closed the shell.
    Closed,
}

/// Serves one pipe shell over the dial-back socket until the controller closes it or
/// kills the firmware.
async fn serve_shell(mut socket: Socket, files: &mut DeviceFiles) -> ShellEnd {
    use base64::Engine;
    let mut input = Vec::new();
    let mut status = 0;
    let mut heredoc: Option<(String, bool, String)> = None;
    loop {
        let bytes = match within(socket.next()).await {
            Some(Ok(Message::Binary(bytes))) => bytes,
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return ShellEnd::Closed,
            Some(Ok(_)) => continue,
        };
        input.extend_from_slice(&bytes);
        while let Some(end) = input.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = input.drain(..=end).collect();
            let line = String::from_utf8(line[..line.len() - 1].to_vec()).unwrap();
            let mut out = String::new();
            if let Some((path, append, body)) = heredoc.as_mut() {
                if line == "__END_B64_42__" {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(body.as_bytes())
                        .unwrap();
                    let entry = files
                        .files
                        .entry(path.clone())
                        .or_insert((Vec::new(), 0o600));
                    if *append {
                        entry.0.extend_from_slice(&data);
                    } else {
                        entry.0 = data;
                    }
                    heredoc = None;
                    status = 0;
                    out.push_str("TRANSFER_OK\n");
                } else {
                    body.push_str(&line);
                }
            } else if let Some(marker) = line
                .strip_prefix("echo \"__EM_")
                .and_then(|rest| rest.strip_suffix("$?\""))
            {
                out = format!("__EM_{marker}{status}\n");
            } else if line == "echo ZWNob211c2U= | base64 -d 2>&1" {
                out.push_str("echomuse");
                status = 0;
            } else if line == "echo echomuse | md5sum 2>&1" {
                out = format!("{}  -\n", Md5::of(b"echomuse\n"));
                status = 0;
            } else if let Some(rest) = line.strip_prefix("(umask 077; base64 -d ") {
                let target = rest
                    .strip_suffix(") <<'__END_B64_42__' && echo TRANSFER_OK")
                    .unwrap_or_else(|| panic!("{line}"));
                let (append, path) = match target.strip_prefix(">> ") {
                    Some(path) => (true, path),
                    None => (false, target.strip_prefix("> ").unwrap()),
                };
                heredoc = Some((path.to_owned(), append, String::new()));
            } else if let Some(part) =
                between(&line, "M=$(md5sum ", " 2>&1); M=${M%% *}; if [ \"$M\" = \"")
            {
                let expected = between(&line, "if [ \"$M\" = \"", "\" ]; then").unwrap();
                let mode = between(&line, "then chmod ", " ").unwrap();
                let dest = between(&line, &format!("mv -f {part} "), " && echo VERIFY_OK").unwrap();
                let held = files.files.remove(part);
                let reported = held.as_ref().map(|(data, _)| Md5::of(data).to_string());
                if reported.as_deref() == Some(expected) {
                    let (data, _) = held.unwrap();
                    let mode = u16::from_str_radix(mode, 8).unwrap();
                    files.files.insert(dest.to_owned(), (data, mode));
                    out.push_str("VERIFY_OK\n");
                } else {
                    out = format!("VERIFY_BAD:{}\n", reported.unwrap_or_default());
                }
                status = 0;
            } else if let Some(path) =
                between(&line, "M=$(md5sum ", " 2>/dev/null); echo \"MD5:${M%% *}\"")
            {
                let reported = files
                    .files
                    .get(path)
                    .map(|(data, _)| Md5::of(data).to_string())
                    .unwrap_or_default();
                out = format!("MD5:{reported}\n");
                status = 0;
            } else if line == "readlink /data/local/bin/server 2>/dev/null" {
                out = format!("{}\n", files.link);
                status = 0;
            } else if line.starts_with("mkdir -p /data/local/etc/echomuse") {
                status = 0;
            } else if let Some(slot) = between(
                &line,
                "ln -sf ",
                " /data/local/bin/server 2>&1 && kill $PPID",
            ) {
                files.link = slot.to_owned();
                // The firmware dies, and with it this shell.
                let _ = socket.close(None).await;
                return ShellEnd::Switched(slot.to_owned());
            } else {
                files.unknown.push(line.clone());
                out = format!("/system/bin/sh: {line}: not found\n");
                status = 127;
            }
            if !out.is_empty() {
                socket
                    .send(Message::Binary(Bytes::from(out)))
                    .await
                    .unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dashboard_update_installs_the_bundled_firmware() {
    let fixture = Fixture::new().await;
    let plane = fixture.start().await;
    let session = sign_in(&plane).await;
    let mut events = events(&plane, &session).await;
    next_event(&mut events, "snapshot").await;
    let mut snapshots = device_snapshots(&plane);
    let mut control = Control::connect(plane.device_address()).await;
    let audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    wait_for(&mut snapshots, "the session to attach", |s| {
        s.link.is_some()
    })
    .await;

    let (status, firmware) = http(
        plane.api_address(),
        "GET",
        "/api/firmware",
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, 200, "{firmware}");
    assert_eq!(firmware["version"], BUNDLED_VERSION);
    assert_eq!(firmware["sha256"], Sha256::of(BUNDLED_BYTES).as_str());

    let (status, started) = http(
        plane.api_address(),
        "POST",
        &format!("/api/devices/{DEVICE}/update"),
        Some(&session),
        Some(&json!({})),
    )
    .await;
    assert_eq!(
        (status, &started["version"]),
        (200, &json!(BUNDLED_VERSION)),
        "{started}"
    );

    // The controller asks for a shell; the device dials back without a pseudo-terminal.
    let open = control.next_of("shell_open").await;
    assert_eq!(open["body"]["pty"], false, "{open}");
    let shell = dial(plane.device_address(), &format!("/shell/{DEVICE}"), None).await;
    let mut files = DeviceFiles::new();
    let ShellEnd::Switched(slot) = serve_shell(shell, &mut files).await else {
        panic!("the shell closed before the switch: {:?}", files.unknown);
    };
    assert!(files.unknown.is_empty(), "{:?}", files.unknown);
    assert_eq!(slot, "server_b");
    let (binary, mode) = files.files["/data/local/bin/server_b"].clone();
    assert_eq!((binary.as_slice(), mode), (BUNDLED_BYTES, 0o755));
    assert_eq!(
        files.files["/data/local/bin/start_server.sh"].0,
        START_SCRIPT_BYTES
    );

    // The firmware restarts on the new slot and says hello as the bundled version.
    drop(control);
    drop(audio);
    let control = Control::connect_running(plane.device_address(), &[], BUNDLED_VERSION).await;
    let _audio = dial(
        plane.device_address(),
        "/device/v1/audio",
        Some(&control.session),
    )
    .await;
    let (updated, _) = next_event(&mut events, "device_updated").await;
    assert_eq!(updated["device_id"], DEVICE);
    let (_, device) = http(
        plane.api_address(),
        "GET",
        &format!("/api/devices/{DEVICE}"),
        Some(&session),
        None,
    )
    .await;
    assert_eq!(device["update_in_progress"], false, "{device}");
    assert_eq!(device["update_error"], Value::Null, "{device}");
    assert_eq!(device["firmware_ver"], BUNDLED_VERSION, "{device}");
    assert_eq!(device["firmware_previous"], "fw-synthetic-2", "{device}");
    assert!(plane.shutdown().await.is_clean());
}
