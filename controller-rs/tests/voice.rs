//! The voice path end to end: the assembled controller on loopback ports (stand-in tone
//! models for speech, the real ffmpeg decoder), simulated speakers speaking the real wire
//! over both sockets, and a simulated Home Assistant that runs assist pipelines with HA's
//! own event sequences and serves the spoken answers as WAV audio.
//!
//! Each speaker captures continuously, 80 ms per packet in real time, like the firmware:
//! a test queues what the room says (tones from `echomuse_voice::speech::testing`) and
//! the messages the device sends at given points of its capture. The speaker answers the
//! controller's commands with `command.ack`, uploads a subscription's backfill once it is
//! granted, and reports every playback the controller starts. Results are read back from
//! the `turns` table and from what each speaker received.
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
    device::{DeviceRuntimeConfig, DeviceSnapshot},
    supervision::{Availability, FatalHook, RestartPolicy, SupervisionOptions},
};
use echomuse_core::types::DeviceId;
use echomuse_db::OpenedDatabase;
use echomuse_esphome::{
    api::{self, ApiMessage},
    frame::{self, FrameDecoder},
};
use echomuse_firmware::FirmwareTimings;
use echomuse_protocol::{
    REQUIRED_CAPABILITIES,
    ema::{Kind, Packet},
};
use echomuse_turn::Sample;
use echomuse_uplink::starts::CandidateStarts;
use echomuse_voice::{
    sim::{BLOCK, SimDevice},
    speech::{
        models::ModelSet,
        testing::{ToneModels, silence, utterance},
    },
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, accept_async, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

/// How long any one expected thing may take. Turns run in real time (a wake, a command
/// and the endpointing silence take a few seconds), so this is generous but bounded.
const WAIT: Duration = Duration::from_secs(30);
const HA_TOKEN: &str = "synthetic-ha-token";
const KITCHEN: &str = "G0SYNTH00A1B2C3D";
const HALL: &str = "G0SYNTH00B2C3D4E";
/// The stand-ins' vocabulary; word 0 is the wake word, as the seeded model's phrase.
const WORDS: &[&str] = &[
    "ophelia", "turn", "on", "the", "kitchen", "light", "stop", "what", "time", "is", "it",
];
const PIPELINE: &str = "01hmd0v9ffh9ze4dbrz9mw0gks";
/// The wake word's tone starts this far into a wake script, after a second of quiet.
const WAKE_LEAD: u64 = 16_000;
/// Relative to the word's start, as in the voice crate's rig: the candidate's support
/// starts 400 ms before the word and opens at the end of the gap after it.
const SUPPORT_BEFORE_WORD: u64 = 6_400;
const OPEN_AFTER_WORD: u64 = 10_400;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn log_init() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

fn models() -> ToneModels {
    ToneModels::new(WORDS)
}

fn words(words: &[&str]) -> Vec<i16> {
    utterance(&models(), words, 400, 250, 6_000)
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

/// The copied installation check (tools/data-copy-check.sh), which uses this file's
/// simulated Home Assistant and speaker.
#[path = "data_copy/check.rs"]
mod data_copy_check;

/// A device's token: its stored one when the copied-installation check dials as a real
/// device, the synthetic one otherwise.
fn token(device: &str) -> String {
    data_copy_check::real_token(device)
        .unwrap_or_else(|| format!("synthetic-device-token-{device}-000"))
}

/// 16 kHz mono PCM16 WAV of `ms` milliseconds of a 330 Hz tone: the spoken answers.
fn answer_wav(ms: u64) -> Vec<u8> {
    let samples: Vec<i16> = (0..ms * 16)
        .map(|n| {
            let t = n as f64 / 16_000.0;
            ((t * 330.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16
        })
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

// ── The simulated Home Assistant ─────────────────────────────────────────────────────

/// How HA answers one intent run.
#[derive(Clone, Debug)]
struct IntentPlan {
    speech: String,
    conversation: String,
    continue_conversation: bool,
    /// Close the connection once the run has started, before it ends.
    lose_connection: bool,
}

impl IntentPlan {
    fn answer(speech: &str) -> Self {
        Self {
            speech: speech.to_owned(),
            conversation: "01K8CONVERSATION".to_owned(),
            continue_conversation: false,
            lose_connection: false,
        }
    }
}

#[derive(Default)]
struct HaState {
    /// What each STT run hears, in order; an empty transcript once these run out.
    transcripts: VecDeque<String>,
    /// How each intent run goes, in order; a plain answer once these run out.
    intents: VecDeque<IntentPlan>,
    /// Every WebSocket request, in order.
    requests: Vec<Value>,
    /// Audio frames each STT run received, by handler.
    stt_frames: HashMap<u8, usize>,
    /// Drop new connections at once (Home Assistant is down).
    refuse_connections: bool,
    /// Connections that authenticated.
    connections: usize,
    /// Answer audio served, by path.
    audio_served: Vec<String>,
    /// Enabled automations: their config ids and stored configurations.
    automations: Vec<(String, Value)>,
    /// MACs of the satellites HA's device registry holds, with their device ids.
    satellites: Vec<(String, String)>,
    /// What HA's sentence matcher answers for a sentence; a sentence not listed matches
    /// `turn on [the] {name}` with nothing resolved. `Value::Null`: not recognized.
    recognitions: HashMap<String, Value>,
    /// HA's running timers, as `HassTimerStatus` lists them (every device's).
    timers: Vec<Value>,
    /// Every `POST /api/intent/handle` body, in order.
    intent_calls: Vec<Value>,
    next_handler: u8,
    next_tts: u32,
}

impl HaState {
    /// HA starts a timer the way its timer manager reports it: a unit that was not given
    /// is 0, an absent name an empty string.
    fn start_timer(&mut self, id: &str, name: &str, minutes: u32, device: &str) {
        self.timers.push(json!({
            "id": id, "name": name, "device_id": SimHa::ha_device(device), "language": "en",
            "start_hours": 0, "start_minutes": minutes, "start_seconds": 0, "is_active": true,
            "rounded_hours_left": 0, "rounded_minutes_left": minutes,
            "rounded_seconds_left": 0, "total_seconds_left": minutes * 60,
        }));
    }

    /// `intent/timers.py`: `HassTimerStatus` lists every timer; `HassCancelTimer` finds
    /// one by name, then the start units, then the asking device (`_find_timer`), and
    /// refuses when none or several remain; `HassCancelAllTimers` cancels every timer.
    fn handle_intent(&mut self, body: &Value) -> Value {
        self.intent_calls.push(body.clone());
        let done = |slots: Value| {
            json!({"speech": {}, "card": {}, "language": "en", "response_type": "action_done",
                   "data": {"success": [], "failed": []}, "speech_slots": slots})
        };
        match body["name"].as_str() {
            Some("HassTimerStatus") => done(json!({"timers": self.timers})),
            Some("HassCancelAllTimers") => {
                self.timers.clear();
                done(json!({}))
            }
            Some("HassCancelTimer") => {
                let slots = &body["data"];
                let mut left: Vec<usize> = (0..self.timers.len()).collect();
                let mut found = None;
                if let Some(name) = slots["name"].as_str() {
                    left.retain(|&i| self.timers[i]["name"] == name);
                    if left.len() == 1 {
                        found = Some(left[0]);
                    }
                }
                if found.is_none() {
                    let unit = |slot: &str| slots[slot].as_u64().unwrap_or(0);
                    left.retain(|&i| {
                        ["start_hours", "start_minutes", "start_seconds"]
                            .iter()
                            .all(|slot| self.timers[i][*slot].as_u64() == Some(unit(slot)))
                    });
                    if left.len() == 1 {
                        found = Some(left[0]);
                    } else {
                        left.retain(|&i| self.timers[i]["device_id"] == body["device_id"]);
                        if left.len() == 1 {
                            found = Some(left[0]);
                        }
                    }
                }
                match found {
                    Some(i) => {
                        self.timers.remove(i);
                        done(json!({}))
                    }
                    None => json!({"speech": {"plain": {"speech": "Timer not found"}},
                                   "card": {}, "language": "en", "response_type": "error",
                                   "data": {"code": "no_valid_targets"}}),
                }
            }
            _ => json!({"speech": {"plain": {"speech": "Unknown intent"}}, "card": {},
                        "language": "en", "response_type": "error",
                        "data": {"code": "no_intent_match"}}),
        }
    }
}

impl HaState {
    fn runs(&self, stage: &str) -> Vec<Value> {
        self.requests
            .iter()
            .filter(|r| r["type"] == "assist_pipeline/run" && r["start_stage"] == stage)
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
    async fn start(devices: &[&str]) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(HaState {
            satellites: devices
                .iter()
                .map(|device| {
                    let mac = echomuse_discovery::voice_mac(&DeviceId((*device).to_owned()));
                    (
                        mac.to_string().to_ascii_lowercase(),
                        format!("ha-{}", device.to_ascii_lowercase()),
                    )
                })
                .collect(),
            next_handler: 1,
            ..HaState::default()
        }));
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

    fn hears(&self, transcript: &str) {
        self.state().transcripts.push_back(transcript.to_owned());
    }

    fn answers(&self, plan: IntentPlan) {
        self.state().intents.push_back(plan);
    }

    /// The HA device id the registry gives `device`.
    fn ha_device(device: &str) -> String {
        format!("ha-{}", device.to_ascii_lowercase())
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
        if state.lock().unwrap().refuse_connections {
            return;
        }
        ha_websocket(stream, state).await;
    } else {
        ha_http(stream, state).await;
    }
}

type HaSocket = WebSocketStream<TcpStream>;

async fn send_json(socket: &mut HaSocket, value: Value) -> bool {
    socket.send(Message::text(value.to_string())).await.is_ok()
}

async fn pipeline_event(socket: &mut HaSocket, id: &Value, kind: &str, data: Value) -> bool {
    send_json(
        socket,
        json!({"id": id, "type": "event", "event": {
            "type": kind, "data": data, "timestamp": "2026-10-29T09:00:00.000000+00:00"}}),
    )
    .await
}

fn intent_output(plan: &IntentPlan) -> Value {
    json!({
        "response": {
            "speech": {"plain": {"speech": plan.speech, "extra_data": null}},
            "card": {}, "language": "en", "response_type": "action_done",
            "data": {"targets": [], "success": [], "failed": []}
        },
        "conversation_id": plan.conversation,
        "continue_conversation": plan.continue_conversation
    })
}

fn tts_output(token: &str) -> Value {
    json!({"media_id": format!("media-source://tts/-stream-/{token}"), "token": token,
           "url": format!("/api/tts_proxy/{token}"), "mime_type": "audio/wav"})
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
    state.lock().unwrap().connections += 1;
    // STT runs waiting for their audio to end: handler → (run id, transcript).
    let mut stt: HashMap<u8, (Value, String)> = HashMap::new();
    while let Some(Ok(message)) = socket.next().await {
        let request = match message {
            Message::Text(text) => serde_json::from_str::<Value>(&text).unwrap(),
            Message::Binary(frame) => {
                let Some(&handler) = frame.first() else {
                    continue;
                };
                *state.lock().unwrap().stt_frames.entry(handler).or_default() += 1;
                if frame.len() == 1
                    && let Some((id, text)) = stt.remove(&handler)
                {
                    let ok =
                        pipeline_event(&mut socket, &id, "stt-vad-end", json!({"timestamp": 1000}))
                            .await
                            && pipeline_event(
                                &mut socket,
                                &id,
                                "stt-end",
                                json!({"stt_output": {"text": format!(" {text} ")}}),
                            )
                            .await
                            && pipeline_event(&mut socket, &id, "run-end", Value::Null).await;
                    if !ok {
                        return;
                    }
                }
                continue;
            }
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
                    json!({"time_zone": "Europe/London", "location_name": "Home",
                           "version": "2026.8.1", "language": "en"}),
                )
                .await
            }
            "config/device_registry/list" => {
                let devices: Vec<Value> = state
                    .lock()
                    .unwrap()
                    .satellites
                    .iter()
                    .map(|(mac, id)| {
                        json!({"id": id, "connections": [["mac", mac]], "identifiers": [],
                               "manufacturer": "Espressif", "name": id, "area_id": null})
                    })
                    .collect();
                result(&mut socket, &id, Value::Array(devices)).await
            }
            "config/entity_registry/list" => result(&mut socket, &id, json!([])).await,
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
            "conversation/agent/homeassistant/debug" => {
                let recognitions = state.lock().unwrap().recognitions.clone();
                let results: Vec<Value> = request["sentences"]
                    .as_array()
                    .map(|sentences| {
                        sentences
                            .iter()
                            .map(|sentence| {
                                let text = sentence.as_str().unwrap_or_default();
                                recognitions.get(text).cloned().unwrap_or_else(|| {
                                    json!({"intent": {"name": "HassTurnOn"}, "slots": {},
                                           "details": {}, "targets": {}, "match": true,
                                           "sentence_template": "turn on [the] {name}",
                                           "unmatched_slots": {}, "source": "builtin"})
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                result(&mut socket, &id, json!({"results": results})).await
            }
            // What Assist matches by name: one exposed light in one area.
            "homeassistant/expose_entity/list" => {
                result(
                    &mut socket,
                    &id,
                    json!({"exposed_entities": {"light.kitchen": {"conversation": true}}}),
                )
                .await
            }
            "get_states" => {
                result(
                    &mut socket,
                    &id,
                    json!([{"entity_id": "light.kitchen", "state": "off",
                            "attributes": {"friendly_name": "Kitchen Light"}}]),
                )
                .await
            }
            "config/entity_registry/get_entries" => {
                result(
                    &mut socket,
                    &id,
                    json!({"light.kitchen": {"entity_id": "light.kitchen", "aliases": []}}),
                )
                .await
            }
            "config/area_registry/list" => {
                result(
                    &mut socket,
                    &id,
                    json!([{"area_id": "kitchen", "name": "Kitchen", "aliases": []}]),
                )
                .await
            }
            "config/floor_registry/list" => result(&mut socket, &id, json!([])).await,
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
            "subscribe_events" => result(&mut socket, &id, Value::Null).await,
            "assist_pipeline/run" => match request["start_stage"].as_str() {
                Some("stt") => {
                    let (handler, text) = {
                        let mut s = state.lock().unwrap();
                        let handler = s.next_handler;
                        s.next_handler = s.next_handler.wrapping_add(1).max(1);
                        (handler, s.transcripts.pop_front().unwrap_or_default())
                    };
                    stt.insert(handler, (id.clone(), text));
                    result(&mut socket, &id, Value::Null).await
                        && pipeline_event(
                            &mut socket,
                            &id,
                            "run-start",
                            json!({"pipeline": PIPELINE, "language": "en",
                                   "conversation_id": "01K8STT",
                                   "runner_data": {"stt_binary_handler_id": handler,
                                                   "timeout": 300}}),
                        )
                        .await
                        && pipeline_event(
                            &mut socket,
                            &id,
                            "stt-start",
                            json!({"engine": "stt.faster_whisper",
                                   "metadata": {"language": "en", "format": "wav",
                                                "codec": "pcm", "bit_rate": 16,
                                                "sample_rate": 16000, "channel": 1}}),
                        )
                        .await
                }
                Some("intent") => {
                    let (plan, token) = {
                        let mut s = state.lock().unwrap();
                        s.next_tts += 1;
                        let plan = s
                            .intents
                            .pop_front()
                            .unwrap_or_else(|| IntentPlan::answer("Done."));
                        (plan, format!("answer-{}.wav", s.next_tts))
                    };
                    if !intent_run(&mut socket, &id, &plan, &token).await {
                        return;
                    }
                    if plan.lose_connection {
                        // HA goes away with the run open, and stays away until the
                        // test lets it back.
                        state.lock().unwrap().refuse_connections = true;
                        let _ = socket.close(None).await;
                        return;
                    }
                    true
                }
                Some("tts") => {
                    let token = {
                        let mut s = state.lock().unwrap();
                        s.next_tts += 1;
                        format!("line-{}.wav", s.next_tts)
                    };
                    result(&mut socket, &id, Value::Null).await
                        && pipeline_event(
                            &mut socket,
                            &id,
                            "run-start",
                            json!({"pipeline": PIPELINE, "language": "en",
                                   "conversation_id": "01K8TTS",
                                   "runner_data": {"stt_binary_handler_id": null,
                                                   "timeout": 300},
                                   "tts_output": {"token": token,
                                                  "url": format!("/api/tts_proxy/{token}"),
                                                  "mime_type": "audio/wav",
                                                  "stream_response": false}}),
                        )
                        .await
                        && pipeline_event(
                            &mut socket,
                            &id,
                            "tts-start",
                            json!({"engine": "tts.piper", "language": "en", "voice": "amy",
                                   "tts_input": request["input"]["text"]}),
                        )
                        .await
                        && pipeline_event(
                            &mut socket,
                            &id,
                            "tts-end",
                            json!({"tts_output": tts_output(&token)}),
                        )
                        .await
                        && pipeline_event(&mut socket, &id, "run-end", Value::Null).await
                }
                _ => error(&mut socket, &id, "invalid_format", "Unknown start stage").await,
            },
            _ => error(&mut socket, &id, "unknown_command", "Unknown command.").await,
        };
        if !ok {
            return;
        }
    }
}

/// HA's side of an intent-to-TTS run (`assist_pipeline/pipeline.py`); with
/// `lose_connection` only its start.
async fn intent_run(socket: &mut HaSocket, id: &Value, plan: &IntentPlan, token: &str) -> bool {
    let started = result(socket, id, Value::Null).await
        && pipeline_event(
            socket,
            id,
            "run-start",
            json!({"pipeline": PIPELINE, "language": "en", "conversation_id": plan.conversation,
                   "runner_data": {"stt_binary_handler_id": null, "timeout": 300},
                   "tts_output": {"token": token, "url": format!("/api/tts_proxy/{token}"),
                                  "mime_type": "audio/wav", "stream_response": false}}),
        )
        .await
        && pipeline_event(
            socket,
            id,
            "intent-start",
            json!({"engine": "conversation.home_assistant", "language": "en",
                   "intent_input": "", "conversation_id": plan.conversation,
                   "device_id": null, "prefer_local_intents": false}),
        )
        .await;
    if !started || plan.lose_connection {
        return started;
    }
    pipeline_event(
        socket,
        id,
        "intent-end",
        json!({"processed_locally": true, "intent_output": intent_output(plan)}),
    )
    .await
        && pipeline_event(
            socket,
            id,
            "tts-start",
            json!({"engine": "tts.piper", "language": "en", "voice": "amy",
                   "tts_input": plan.speech}),
        )
        .await
        && pipeline_event(
            socket,
            id,
            "tts-end",
            json!({"tts_output": tts_output(token)}),
        )
        .await
        && pipeline_event(socket, id, "run-end", Value::Null).await
}

async fn result(socket: &mut HaSocket, id: &Value, result: Value) -> bool {
    send_json(
        socket,
        json!({"id": id, "type": "result", "success": true, "result": result}),
    )
    .await
}

async fn error(socket: &mut HaSocket, id: &Value, code: &str, message: &str) -> bool {
    send_json(
        socket,
        json!({"id": id, "type": "result", "success": false,
               "error": {"code": code, "message": message}}),
    )
    .await
}

/// HA's REST side: the answer audio under `/api/tts_proxy/`, the intent endpoint (its
/// timer intents over [`HaState::timers`]), entity states (the enabled automations, nothing
/// else) and the automations' stored configurations.
async fn ha_http(stream: TcpStream, state: Arc<Mutex<HaState>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).await.is_err() {
            return;
        }
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
    if reader.read_exact(&mut body).await.is_err() {
        return;
    }
    let (status, content_type, payload) = if method == "GET" && path.starts_with("/api/tts_proxy/")
    {
        state.lock().unwrap().audio_served.push(path.clone());
        (200, "audio/wav", answer_wav(600))
    } else if method == "POST" && path == "/api/intent/handle" {
        let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let response = state.lock().unwrap().handle_intent(&request);
        (200, "application/json", response.to_string().into_bytes())
    } else if method == "GET" && path == "/api/states" {
        let states: Vec<Value> = state
            .lock()
            .unwrap()
            .automations
            .iter()
            .map(|(id, _)| {
                json!({"entity_id": format!("automation.a{id}"), "state": "on",
                       "attributes": {"id": id}})
            })
            .collect();
        (
            200,
            "application/json",
            Value::Array(states).to_string().into_bytes(),
        )
    } else if let Some(id) = path.strip_prefix("/api/config/automation/config/")
        && let Some((_, config)) = state
            .lock()
            .unwrap()
            .automations
            .iter()
            .find(|(known, _)| known == id)
    {
        (200, "application/json", config.to_string().into_bytes())
    } else {
        (
            404,
            "application/json",
            json!({"message": "Entity not found."})
                .to_string()
                .into_bytes(),
        )
    };
    let head = format!(
        "HTTP/1.1 {status} Simulated\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&payload).await;
    let _ = stream.shutdown().await;
}

// ── The controller ───────────────────────────────────────────────────────────────────

/// One approved speaker of the fixture.
struct DeviceRow {
    id: &'static str,
    label: &'static str,
    config: std::borrow::Cow<'static, str>,
    sections: &'static str,
}

impl DeviceRow {
    fn plain(id: &'static str, label: &'static str) -> Self {
        Self {
            id,
            label,
            config: "{}".into(),
            sections: "[]",
        }
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    env: Environment,
    esphome_ports: HashMap<&'static str, u16>,
    ha: SimHa,
}

impl Fixture {
    async fn new(devices: &[DeviceRow]) -> Self {
        log_init();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("echomuse.db");
        drop(OpenedDatabase::open(&db_path).unwrap());
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut esphome_ports = HashMap::new();
        for (n, device) in devices.iter().enumerate() {
            let port = free_voice_port();
            esphome_ports.insert(device.id, port);
            conn.execute(
                "INSERT INTO devices (device_id, label, approved, ip, firmware_ver, first_seen, \
                 last_seen, token, esphome_api_port, config, config_sections) \
                 VALUES (?1, ?2, 1, ?3, 'fw-synthetic', 1600000000, 1600000000, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    device.id,
                    device.label,
                    format!("192.0.2.{}", 10 + n),
                    token(device.id),
                    port,
                    device.config.as_ref(),
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

        let ids: Vec<&str> = devices.iter().map(|d| d.id).collect();
        let ha = SimHa::start(&ids).await;
        let path = |name: &str| dir.path().join(name).display().to_string();
        let env: Environment = [
            ("SERVER_HOST", "127.0.0.1".to_owned()),
            ("SERVER_PORT", free_port().to_string()),
            ("SERVER_TLS_PORT", free_port().to_string()),
            ("API_PORT", free_port().to_string()),
            ("SERVER_IP", "127.0.0.1".to_owned()),
            (
                "MDNS_NAME",
                format!("echomuse-voice-test-{}-{}", std::process::id(), free_port()),
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
        Self {
            dir,
            env,
            esphome_ports,
            ha,
        }
    }

    async fn start(&self) -> ControlPlane {
        self.start_with(Arc::new(models()), SupervisionOptions::default())
            .await
    }

    /// Starts the controller with `speech` as its models and `supervision` as its
    /// failure handling.
    async fn start_with(
        &self,
        speech: Arc<dyn ModelSet>,
        supervision: SupervisionOptions,
    ) -> ControlPlane {
        let config = ControllerConfig::from_env(&self.env, Some("test")).unwrap();
        let options = ControlPlaneOptions {
            config,
            static_dir: self.dir.path().join("static"),
            wake_seed_graph: repo_root().join("bcresnet_audio.onnx"),
            wake_seed_sidecar: repo_root().join("bcresnet_audio.json"),
            qualifier_program: PathBuf::from(env!("CARGO_BIN_EXE_echomuse-controller")),
            runtime: DeviceRuntimeConfig::default(),
            firmware_timings: FirmwareTimings::default(),
            speech: SpeechSource::Provided(speech),
            ffmpeg: PathBuf::from("/usr/bin/ffmpeg"),
            supervision,
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

    fn db(&self) -> PathBuf {
        self.dir.path().join("echomuse.db")
    }

    /// The device's turn rows, oldest first.
    fn rows(&self, device: &str) -> Vec<TurnRow> {
        let conn = rusqlite::Connection::open(self.db()).unwrap();
        let mut statement = conn
            .prepare(
                "SELECT trigger_type, terminal_reason, outcome, stt_text, continuation, \
                 conversation_id, commit_id, commit_route, response_text, reply_to, turn_uuid, \
                 decision_trace \
                 FROM turns WHERE device_id = ?1 ORDER BY id",
            )
            .unwrap();
        statement
            .query_map([device], |row| {
                Ok(TurnRow {
                    trigger: row.get(0)?,
                    terminal: row.get(1)?,
                    outcome: row.get(2)?,
                    stt_text: row.get(3)?,
                    continuation: row.get(4)?,
                    conversation: row.get(5)?,
                    commit_id: row.get(6)?,
                    commit_route: row.get(7)?,
                    response_text: row.get(8)?,
                    reply_to: row.get(9)?,
                    turn_uuid: row.get(10)?,
                    trace: row.get(11)?,
                })
            })
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Waits until the device has `count` rows and returns them; on timeout the rows
    /// there are show what happened instead.
    async fn wait_rows(&self, device: &str, count: usize) -> Vec<TurnRow> {
        let wait = async {
            loop {
                let rows = self.rows(device);
                if rows.len() >= count {
                    return rows;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        match tokio::time::timeout(WAIT, wait).await {
            Ok(rows) => rows,
            Err(_) => panic!(
                "timed out waiting for {count} turn rows of {device}; it has {:#?}",
                self.rows(device)
            ),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TurnRow {
    trigger: Option<String>,
    terminal: Option<String>,
    outcome: Option<String>,
    stt_text: Option<String>,
    continuation: Option<String>,
    conversation: Option<String>,
    commit_id: Option<String>,
    commit_route: Option<String>,
    response_text: Option<String>,
    reply_to: Option<String>,
    turn_uuid: Option<String>,
    /// The decision trace, shown when an assertion on the row fails.
    trace: Option<String>,
}

fn device_snapshots(plane: &ControlPlane, device: &str) -> watch::Receiver<DeviceSnapshot> {
    plane
        .registry()
        .directory()
        .get(&DeviceId(device.to_owned()))
        .expect("the approved device has a runtime")
        .subscribe()
}

// ── The simulated speaker ────────────────────────────────────────────────────────────

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One thing in a speaker's capture script.
enum Item {
    /// The room says this while the device plays nothing.
    Hear(Vec<i16>),
    /// The device plays this and hears its echo.
    Play(Vec<i16>),
    /// A wake candidate opening here.
    Candidate {
        id: String,
        support_start: u64,
        open: u64,
        producing_sound: bool,
    },
    /// A device message sent at this point of the capture.
    Send { kind: &'static str, body: Value },
    /// The device stops capturing (a mute).
    StopCapture,
}

/// A playback the controller started, as the device follows it.
#[derive(Debug, Default)]
struct Playback {
    generation: u64,
    /// Frames received, from frame 0.
    received: u64,
    end_frame: Option<u64>,
    end_seen: bool,
    started: bool,
    finished: bool,
}

#[derive(Default)]
struct Shared {
    sim: Option<SimDevice>,
    script: VecDeque<Item>,
    /// Samples the script holds that were not captured yet.
    queued: u64,
    capturing: bool,
    /// Every controller message except heartbeats and acknowledgements, in order.
    received: Vec<Value>,
    /// Acknowledgement waiters, by the acknowledged message's id.
    waiters: HashMap<String, oneshot::Sender<Value>>,
    /// The controller's answer to each wake candidate.
    candidate_acks: HashMap<String, String>,
    playbacks: HashMap<String, Playback>,
    /// Render epoch → playback id.
    epochs: HashMap<u64, String>,
    /// Render frames that arrived before their `render.start` was acknowledged.
    frames_before_ack: Vec<String>,
    render_frames: u64,
    /// The alert ringing on the device, as `wake.candidate`'s `active_alert`.
    alert: Option<Value>,
    physical_seq: u64,
    /// The live audio subscriptions, by lease id: the device uploads nothing without one.
    subscriptions: HashSet<String>,
    /// Subscriptions the controller closed.
    closed: HashSet<String>,
    /// The `render.finished` reports the device sent: (playback id, reason).
    finished: Vec<(String, &'static str)>,
    /// Network playbacks keep playing (they start but never drain) while this is set: a
    /// question the user answers before it ends.
    hold_playback: bool,
}

impl Shared {
    fn sim(&mut self) -> &mut SimDevice {
        self.sim.as_mut().expect("capture started")
    }
}

struct Speaker {
    device: &'static str,
    session: String,
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

/// Everything a speaker's tasks share for sending.
#[derive(Clone)]
struct Wire {
    device: &'static str,
    session: String,
    shared: Arc<Mutex<Shared>>,
    control: mpsc::UnboundedSender<Message>,
    audio: mpsc::UnboundedSender<Message>,
}

impl Wire {
    fn send(&self, kind: &str, generation: u64, body: Value) -> String {
        let (text, id) = envelope(self.device, Some(&self.session), kind, generation, body);
        let _ = self.control.send(Message::text(text));
        id
    }

    fn ack(&self, message_id: &Value, status: &str) {
        self.send(
            "command.ack",
            0,
            json!({"message_id": message_id, "status": status, "error": null}),
        );
    }

    /// Sends packets while the caller holds the shared state, so sequence numbers go out
    /// in order.
    fn audio(&self, packets: Vec<Bytes>) {
        for packet in packets {
            let _ = self.audio.send(Message::Binary(packet));
        }
    }

    /// Reports a playback that has played out (`render.finished` `drained`, 150 ms after
    /// the last frame completed).
    fn finish_later(&self, playback_id: String, reason: &'static str) {
        let wire = self.clone();
        tokio::spawn(async move {
            if reason == "drained" {
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let (generation, last) = {
                let mut shared = wire.shared.lock().unwrap();
                let Some(playback) = shared.playbacks.get_mut(&playback_id) else {
                    return;
                };
                if playback.finished {
                    return;
                }
                playback.finished = true;
                let report = (playback.generation, playback.received);
                shared.finished.push((playback_id.clone(), reason));
                report
            };
            wire.send(
                "render.finished",
                generation,
                json!({"playback_id": playback_id, "last_completed_frame": last.to_string(),
                       "reason": reason, "timing_quality": "estimated"}),
            );
        });
    }

    fn progress(&self, playback_id: &str, generation: u64, event: &str, frames: u64) {
        self.send(
            "render.progress",
            generation,
            json!({"playback_id": playback_id, "event": event,
                   "submitted_frames": frames.to_string(), "completed_frames": frames.to_string(),
                   "mono_ns": "1", "uncertainty_us": 0, "timing_quality": "estimated",
                   "reference_coverage": "full"}),
        );
    }

    /// A network playback whose end is known and fully received has drained.
    fn maybe_drained(&self, shared: &mut Shared, playback_id: &str) {
        let hold = shared.hold_playback;
        let Some(playback) = shared.playbacks.get_mut(playback_id) else {
            return;
        };
        if playback.finished || !playback.end_seen {
            return;
        }
        if playback
            .end_frame
            .is_none_or(|end| playback.received >= end)
        {
            if !playback.started {
                playback.started = true;
                self.progress(playback_id, playback.generation, "start", playback.received);
            }
            if !hold {
                self.finish_later(playback_id.to_owned(), "drained");
            }
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
                let playback_id = body["playback_id"].as_str().unwrap_or_default().to_owned();
                let epoch = body["epoch"]
                    .as_str()
                    .and_then(|epoch| epoch.parse::<u64>().ok());
                {
                    let mut shared = self.shared.lock().unwrap();
                    shared.playbacks.insert(
                        playback_id.clone(),
                        Playback {
                            generation,
                            ..Playback::default()
                        },
                    );
                    if let Some(epoch) = epoch {
                        shared.epochs.insert(epoch, playback_id.clone());
                    }
                }
                self.ack(&id, "accepted");
                if epoch.is_none() {
                    // A local sound plays at once and is reported like any other.
                    let wire = self.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        wire.progress(&playback_id, generation, "start", 0);
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        wire.finish_later(playback_id, "drained");
                    });
                }
            }
            "render.end" => {
                self.ack(&id, "accepted");
                let playback_id = body["playback_id"].as_str().unwrap_or_default().to_owned();
                let mut shared = self.shared.lock().unwrap();
                if let Some(playback) = shared.playbacks.get_mut(&playback_id) {
                    playback.end_seen = true;
                    playback.end_frame = body["end_frame"]
                        .as_str()
                        .and_then(|frame| frame.parse().ok());
                }
                self.maybe_drained(&mut shared, &playback_id);
            }
            "render.cancel" => {
                self.ack(&id, "accepted");
                let playback_id = body["playback_id"].as_str().unwrap_or_default().to_owned();
                self.finish_later(playback_id, "cancelled");
            }
            "uplink.open" => {
                self.ack(&id, "accepted");
                // The subscription's backfill runs from each requested start, mic first
                // as the firmware packs it; live audio follows it.
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
                self.ack(&id, "accepted");
                if let Some(lease) = body["lease_id"].as_str() {
                    let mut shared = self.shared.lock().unwrap();
                    shared.subscriptions.remove(lease);
                    shared.closed.insert(lease.to_owned());
                }
            }
            "alert.ring" => {
                self.ack(&id, "accepted");
                let alert = json!({"id": body["ring_id"], "kind": "timer",
                                   "name": body["name"], "foreground": true});
                self.shared.lock().unwrap().alert = Some(alert);
                self.send(
                    "alert.state",
                    0,
                    json!({"active": {"id": body["ring_id"], "kind": "timer",
                                      "name": body["name"], "foreground": true,
                                      "started_mono_ns": "1000",
                                      "deadline_mono_ns": "900000001000"},
                           "queue": [], "clock_trusted": true, "wakelock": "held",
                           "store": "ok"}),
                );
            }
            "alert.act" => {
                // A timer ring stops at once.
                self.ack(&id, "applied");
                self.shared.lock().unwrap().alert = None;
                self.send(
                    "alert.ring_ended",
                    0,
                    json!({"id": body["target_id"], "kind": "timer", "reason": "stopped"}),
                );
                self.send(
                    "alert.state",
                    0,
                    json!({"active": null, "queue": [], "clock_trusted": true,
                           "wakelock": "held", "store": "ok"}),
                );
            }
            "protocol.error" | "timer.view" | "turn.view" | "alert.op_result" => {}
            _ => self.ack(&id, "accepted"),
        }
    }

    /// A render packet on the audio socket.
    fn render_packet(&self, bytes: Bytes) {
        let Ok(packet) = Packet::parse(bytes) else {
            return;
        };
        let header = packet.header();
        if header.kind != Kind::Render {
            return;
        }
        let mut shared = self.shared.lock().unwrap();
        shared.render_frames += 1;
        let Some(playback_id) = shared.epochs.get(&header.epoch).cloned() else {
            shared
                .frames_before_ack
                .push(format!("epoch {} before its render.start", header.epoch));
            return;
        };
        let Some(playback) = shared.playbacks.get_mut(&playback_id) else {
            return;
        };
        // The acknowledgement went out when the start was registered, so a frame of an
        // unregistered epoch is one sent before it.
        let end = header.first_sample + u64::from(header.frame_count);
        playback.received = playback.received.max(end);
        if !playback.started {
            playback.started = true;
            self.progress(&playback_id, playback.generation, "start", 0);
        }
        self.maybe_drained(&mut shared, &playback_id);
    }
}

impl Speaker {
    async fn connect(address: SocketAddr, device: &'static str, extra: &[&str]) -> Self {
        let socket = dial(address, device, "/device/v1/control", None).await;
        let (mut sink, mut incoming) = socket.split();
        let mut capabilities: Vec<&str> =
            REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
        capabilities.extend(["leds", "led_anim", "buttons", "button_hold"]);
        capabilities.extend(extra);
        let hello = json!({
            "capabilities": capabilities, "platform": "fireos6", "protocols": [1],
            "firmware_version": "fw-synthetic-2", "os_version": "Fire OS 6 (synthetic)",
            "privacy": {"muted": false, "capture_epoch": "7"},
            "volume": {"level": 64, "seeded": true},
        });
        let (text, _) = envelope(device, None, "session.hello", 0, hello);
        sink.send(Message::text(text)).await.unwrap();
        let ready = within("session.ready", next_json(&mut incoming))
            .await
            .expect("a reply");
        assert_eq!(ready["type"], "session.ready", "{ready}");
        assert_eq!(ready["body"]["capture_permitted"], true, "{ready}");
        let session = ready["body"]["session_id"].as_str().unwrap().to_owned();
        let audio_socket = dial(address, device, "/device/v1/audio", Some(&session)).await;
        let (mut audio_sink, mut audio_incoming) = audio_socket.split();

        let shared = Arc::new(Mutex::new(Shared {
            physical_seq: 1,
            ..Shared::default()
        }));
        let (control, mut control_queue) = mpsc::unbounded_channel::<Message>();
        let (audio, mut audio_queue) = mpsc::unbounded_channel::<Message>();
        let wire = Wire {
            device,
            session: session.clone(),
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
        let render = wire.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(Ok(message)) = audio_incoming.next().await {
                if let Message::Binary(bytes) = message {
                    render.render_packet(bytes);
                }
            }
        }));

        // The current epochs, as after every session.ready, and the alert state.
        {
            let mut guard = shared.lock().unwrap();
            let sim = if extra.contains(&"afe_metadata_v1") {
                SimDevice::new(7, 9).with_afe()
            } else {
                SimDevice::new(7, 9)
            };
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
        let speaker = Self {
            device,
            session,
            shared,
            control,
            tasks,
        };
        speaker.sync().await;
        let mut speaker = speaker;
        speaker.shared.lock().unwrap().capturing = true;
        let capture = wire;
        speaker.tasks.push(tokio::spawn(capture_loop(capture)));
        speaker
    }

    /// Sends `clock.request` and waits for its reply: everything sent before it on the
    /// control socket has then been taken in order.
    async fn sync(&self) {
        let nonce = uuid::Uuid::new_v4().to_string();
        let (text, _) = envelope(
            self.device,
            Some(&self.session),
            "clock.request",
            0,
            json!({"nonce": nonce}),
        );
        self.control.send(Message::text(text)).unwrap();
        self.wait_message("the clock reply", |m| {
            m["type"] == "clock.reply" && m["body"]["nonce"] == nonce
        })
        .await;
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
        if let Item::Hear(pcm) | Item::Play(pcm) = &item {
            shared.queued += pcm.len() as u64;
        }
        shared.script.push_back(item);
    }

    fn say(&self, pcm: Vec<i16>) {
        self.push(Item::Hear(pcm));
    }

    /// A second of quiet, then the wake word (heard, or played by the device itself),
    /// then the candidate opens. Returns the candidate id.
    fn wake(&self, id: &str, played: bool) -> String {
        let word_start = self.position() + WAKE_LEAD;
        self.say(silence(1_000));
        let word = words(&["ophelia"]);
        assert_eq!(word.len() as u64, OPEN_AFTER_WORD);
        self.push(if played {
            Item::Play(word)
        } else {
            Item::Hear(word)
        });
        self.push(Item::Candidate {
            id: id.to_owned(),
            support_start: word_start - SUPPORT_BEFORE_WORD,
            open: word_start + OPEN_AFTER_WORD,
            producing_sound: played,
        });
        id.to_owned()
    }

    fn send_at(&self, kind: &'static str, body: Value) {
        self.push(Item::Send { kind, body });
    }

    fn next_physical_seq(&self) -> u64 {
        let mut shared = self.shared();
        shared.physical_seq += 1;
        shared.physical_seq
    }

    /// The controller's answer to candidate `id`.
    async fn candidate_ack(&self, id: &str) -> String {
        eventually(&format!("the answer to candidate {id}"), || {
            self.shared().candidate_acks.get(id).cloned()
        })
        .await
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

    fn frames_before_ack(&self) -> Vec<String> {
        self.shared().frames_before_ack.clone()
    }

    fn render_frames(&self) -> u64 {
        self.shared().render_frames
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
                Some(Item::Hear(pcm)) | Some(Item::Play(pcm)) => {
                    let n = pcm.len().min(BLOCK as usize);
                    Some(pcm.drain(..n).collect::<Vec<i16>>())
                }
                Some(_) => None,
                None => Some(vec![0; BLOCK as usize]),
            };
            if let Some(block) = block {
                let played = matches!(shared.script.front(), Some(Item::Play(_)));
                if !shared.script.is_empty() {
                    shared.queued -= block.len() as u64;
                }
                if matches!(shared.script.front(), Some(Item::Hear(pcm) | Item::Play(pcm)) if pcm.is_empty())
                {
                    shared.script.pop_front();
                }
                if played {
                    shared.sim().play(&block);
                } else {
                    shared.sim().hear(&block);
                }
                // Audio goes up only while a subscription wants it.
                if shared.subscriptions.is_empty() {
                    shared.sim().skip_live();
                } else {
                    let packets = shared.sim().live();
                    wire.audio(packets);
                }
            }
            while matches!(
                shared.script.front(),
                Some(Item::Candidate { .. } | Item::Send { .. } | Item::StopCapture)
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
                    producing_sound,
                } => candidate(&wire, &id, support_start, open, producing_sound).await,
                Item::Send { kind, body } => {
                    wire.send(kind, 0, body);
                }
                Item::StopCapture => wire.shared.lock().unwrap().capturing = false,
                Item::Hear(_) | Item::Play(_) => {}
            }
        }
    }
}

/// Sends a wake candidate and, once the controller accepts it, uploads the candidate
/// subscription's backfill, as the firmware does. Capture waits meanwhile.
async fn candidate(wire: &Wire, id: &str, support_start: u64, open: u64, producing_sound: bool) {
    let (body, alert) = {
        let mut shared = wire.shared.lock().unwrap();
        let alert = shared.alert.clone();
        let body = shared.sim().candidate(
            id,
            &format!("lease-{id}"),
            support_start,
            open,
            producing_sound,
        );
        (body, alert)
    };
    let mut body = Value::Object(body);
    if let Some(alert) = alert {
        body["active_alert"] = alert;
    }
    let (sent, answer) = oneshot::channel();
    let (text, message_id) = envelope(wire.device, Some(&wire.session), "wake.candidate", 1, body);
    wire.shared.lock().unwrap().waiters.insert(message_id, sent);
    let _ = wire.control.send(Message::text(text));
    let status = match tokio::time::timeout(Duration::from_secs(1), answer).await {
        Ok(Ok(ack)) => ack["status"].as_str().unwrap_or_default().to_owned(),
        _ => "ttl".to_owned(),
    };
    let mut shared = wire.shared.lock().unwrap();
    shared.candidate_acks.insert(id.to_owned(), status.clone());
    let lease = format!("lease-{id}");
    if status == "accepted" && !shared.closed.contains(&lease) {
        let afe = shared.sim().serves_afe();
        let starts = CandidateStarts::new(Sample(support_start), afe);
        let sim = shared.sim();
        let mut packets = sim.backfill("mic", starts.mic.0);
        packets.extend(sim.backfill("reference", starts.reference.0));
        packets.extend(sim.backfill("cells", starts.cells.0));
        if let Some(afe) = starts.afe {
            packets.extend(sim.backfill("afe", afe.0));
        }
        wire.audio(packets);
        shared.subscriptions.insert(lease);
    }
}

/// A Home Assistant client of a speaker's ESPHome satellite.
struct EsphomeClient {
    stream: TcpStream,
    decoder: FrameDecoder,
}

impl EsphomeClient {
    async fn connect(port: u16) -> Self {
        let stream = within(
            "the satellite connection",
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
        )
        .await
        .unwrap();
        let mut client = Self {
            stream,
            decoder: FrameDecoder::new(),
        };
        client
            .send(&api::HelloRequest {
                client_info: "Home Assistant 2026.8.1".to_owned(),
                api_version_major: 1,
                api_version_minor: 14,
            })
            .await;
        client.recv::<api::HelloResponse>().await;
        client
    }

    async fn send<M: ApiMessage>(&mut self, message: &M) {
        self.stream
            .write_all(&frame::encode(message).unwrap())
            .await
            .unwrap();
    }

    async fn recv<M: ApiMessage>(&mut self) -> M {
        within("a satellite message", async {
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

/// Waits until the speaker's satellite listens for Home Assistant.
async fn satellite_listening(plane: &ControlPlane, device: &str) {
    let mut snapshots = device_snapshots(plane, device);
    within(
        "the satellite to listen",
        snapshots.wait_for(|s| s.esphome.voice_listening.is_some()),
    )
    .await
    .unwrap();
}

fn intent_texts(ha: &SimHa) -> Vec<String> {
    ha.state()
        .runs("intent")
        .iter()
        .map(|run| run["input"]["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn uplink_closes(speaker: &Speaker, lease: &str) -> Vec<String> {
    speaker
        .messages("uplink.close")
        .iter()
        .filter(|m| m["body"]["lease_id"] == lease)
        .map(|m| m["body"]["reason"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn dialog_starts(speaker: &Speaker) -> Vec<Value> {
    speaker
        .messages("render.start")
        .into_iter()
        .filter(|m| m["body"]["source_class"] == "dialog_output")
        .collect()
}

async fn shut_down(plane: ControlPlane) {
    let report = plane.shutdown().await;
    assert!(report.is_clean(), "{:?}", report.problems);
}

// ── The cases ────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_wake_is_answered_by_home_assistant_and_recorded() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture
        .ha
        .answers(IntentPlan::answer("Turned on the light."));

    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    // The candidate subscription became the turn's.
    let renew = speaker
        .wait_message("the conversion to a turn", |m| {
            m["type"] == "uplink.renew" && m["body"]["lease_id"] == "lease-c1"
        })
        .await;
    assert_eq!(renew["body"]["reason"], "turn", "{renew}");

    let rows = fixture.wait_rows(KITCHEN, 1).await;
    let row = &rows[0];
    assert_eq!(row.trigger.as_deref(), Some("wake"), "{row:?}");
    assert_eq!(row.terminal.as_deref(), Some("completed"), "{row:?}");
    assert_eq!(row.outcome.as_deref(), Some("ha"), "{row:?}");
    assert_eq!(
        row.stt_text.as_deref(),
        Some("turn on the kitchen light."),
        "{row:?}"
    );
    assert!(row.commit_id.is_some(), "{row:?}");
    assert_eq!(row.response_text.as_deref(), Some("Turned on the light."));

    // HA heard the command without the wake phrase, exactly once, for this satellite.
    assert_eq!(intent_texts(&fixture.ha), ["turn on the kitchen light."]);
    let stt = fixture.ha.state().runs("stt");
    assert_eq!(stt.len(), 1);
    assert_eq!(stt[0]["device_id"], SimHa::ha_device(KITCHEN));
    assert_eq!(stt[0]["pipeline"], PIPELINE);
    assert!(fixture.ha.state().stt_frames.values().sum::<usize>() >= 2);

    // The answer was fetched and decoded, and the device acknowledged its start before
    // any of its frames arrived.
    assert_eq!(fixture.ha.state().audio_served.len(), 1);
    let starts = dialog_starts(&speaker);
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!(starts[0]["body"]["epoch"].is_string(), "{}", starts[0]);
    assert!(speaker.render_frames() > 0);
    assert_eq!(speaker.frames_before_ack(), Vec::<String>::new());
    shut_down(plane).await;
}

/// A plain HTTP/1.1 GET on the dashboard API with a session; returns the status and body.
async fn api_get(address: SocketAddr, path: &str, session: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {session}\r\n\
         Connection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8(response).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_turn_reaches_the_dashboard_as_it_lands() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    // A signed-in dashboard user, as the sessions table holds one.
    let session = "a".repeat(64);
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "INSERT INTO users (id, username, password_hash, role, created_at) \
         VALUES (1, 'viewer', '$2b$04$synthetic', 'readonly', 1600000000)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) \
         VALUES (?1, 1, 1600000000, 4000000000)",
        [&session],
    )
    .unwrap();
    drop(conn);
    let plane = fixture.start().await;
    let (mut events, _) = connect_async(format!(
        "ws://{}/api/events?token={session}",
        plane.api_address()
    ))
    .await
    .unwrap();
    let mut next_event = async || loop {
        match within("an event", events.next()).await.unwrap().unwrap() {
            Message::Text(text) => return serde_json::from_str::<Value>(&text).unwrap(),
            _ => continue,
        }
    };
    assert_eq!(next_event().await["type"], "snapshot");

    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture
        .ha
        .answers(IntentPlan::answer("Turned on the light."));
    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");

    // dashboard.jsx's Activity views re-read their lists on turn_complete of their device.
    let landed = loop {
        let event = next_event().await;
        if event["type"] == "turn_complete" {
            break event;
        }
    };
    assert_eq!(
        landed,
        json!({"type": "turn_complete", "device_id": KITCHEN})
    );
    let (status, turns) = api_get(
        plane.api_address(),
        &format!("/api/devices/{KITCHEN}/turns?limit=50"),
        &session,
    )
    .await;
    assert_eq!(status, 200, "{turns}");
    let turn = &turns[0];
    assert_eq!(turn["device_id"], KITCHEN);
    assert_eq!(turn["trigger"], "wake");
    assert_eq!(turn["terminal_reason"], "completed");
    assert_eq!(turn["response_text"], "Turned on the light.");
    let (status, trace) = api_get(
        plane.api_address(),
        &format!("/api/devices/{KITCHEN}/turns/{}/trace", turn["turn_id"]),
        &session,
    )
    .await;
    assert_eq!(status, 200, "{trace}");
    assert!(trace.is_object());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_word_the_device_played_itself_is_refused_as_self_output() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;

    // A device that has been running a while (the reference judgement reads windows
    // reaching back further than the stream's first, which ends 1.4 s in), then plays
    // the wake word and hears its own echo.
    speaker.say(silence(3_000));
    let candidate = speaker.wake("c1", true);
    // The subscription is granted so the evidence can arrive, then closed as refused.
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    eventually("the candidate's subscription to close", || {
        (!uplink_closes(&speaker, "lease-c1").is_empty()).then_some(())
    })
    .await;
    assert_eq!(uplink_closes(&speaker, "lease-c1"), ["rejected"]);
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].trigger.as_deref(), Some("wake"), "{rows:?}");
    assert_eq!(rows[0].terminal.as_deref(), Some("self_output"), "{rows:?}");
    assert!(fixture.ha.state().runs("stt").is_empty());
    assert!(speaker.messages("uplink.renew").is_empty());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_speakers_hearing_one_wake_answer_once() {
    let fixture = Fixture::new(&[
        DeviceRow::plain(KITCHEN, "Kitchen"),
        DeviceRow::plain(HALL, "Hall"),
    ])
    .await;
    let plane = fixture.start().await;
    let kitchen = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    let hall = Speaker::connect(plane.device_address(), HALL, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture
        .ha
        .answers(IntentPlan::answer("Turned on the light."));

    // One person speaks in a room both hear: the scripts start together.
    for speaker in [&kitchen, &hall] {
        speaker.wake("c1", false);
        speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    }
    assert_eq!(kitchen.candidate_ack("c1").await, "accepted");
    assert_eq!(hall.candidate_ack("c1").await, "accepted");
    let (winner, loser) = eventually("one speaker to stand down", || {
        let kitchen_lost =
            uplink_closes(&kitchen, "lease-c1").contains(&"arbitration_lost".to_owned());
        let hall_lost = uplink_closes(&hall, "lease-c1").contains(&"arbitration_lost".to_owned());
        match (kitchen_lost, hall_lost) {
            (true, false) => Some((&hall, &kitchen)),
            (false, true) => Some((&kitchen, &hall)),
            (false, false) => None,
            (true, true) => panic!("both speakers stood down"),
        }
    })
    .await;
    // The loser's candidate never became a turn; the winner's did.
    assert!(loser.messages("uplink.renew").is_empty());
    let loser_rows = fixture.wait_rows(loser.device, 1).await;
    assert_eq!(loser_rows.len(), 1, "{loser_rows:?}");
    assert_eq!(loser_rows[0].trigger.as_deref(), Some("wake"));
    assert_eq!(
        loser_rows[0].terminal.as_deref(),
        Some("arbitration_lost"),
        "{loser_rows:?}"
    );
    let winner_rows = fixture.wait_rows(winner.device, 1).await;
    assert_eq!(
        winner_rows[0].terminal.as_deref(),
        Some("completed"),
        "{winner_rows:?}"
    );
    assert_eq!(winner_rows[0].outcome.as_deref(), Some("ha"));
    assert_eq!(intent_texts(&fixture.ha), ["turn on the kitchen light."]);
    let stt = fixture.ha.state().runs("stt");
    assert_eq!(stt.len(), 1);
    assert_eq!(stt[0]["device_id"], SimHa::ha_device(winner.device));
    assert_eq!(dialog_starts(winner).len(), 1);
    assert!(dialog_starts(loser).is_empty());
    assert_eq!(fixture.rows(loser.device).len(), 1);
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mute_during_the_command_ends_the_turn_without_asking_home_assistant() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");

    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on"]));
    // The mute button, mid-command: the firmware stops capturing.
    let seq = speaker.next_physical_seq();
    speaker.send_at(
        "privacy.changed",
        json!({"muted": true, "capture_epoch": null, "physical_seq": seq}),
    );
    speaker.push(Item::StopCapture);
    assert_eq!(speaker.candidate_ack("c1").await, "accepted");

    let rows = fixture.wait_rows(KITCHEN, 1).await;
    assert_eq!(rows[0].trigger.as_deref(), Some("wake"), "{rows:?}");
    assert_eq!(rows[0].terminal.as_deref(), Some("muted"), "{rows:?}");
    assert_eq!(rows[0].outcome, None, "{rows:?}");
    // Nothing reached Home Assistant for it, then or later.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(fixture.ha.state().runs("intent").is_empty());
    assert!(fixture.ha.state().runs("stt").is_empty());
    assert_eq!(fixture.rows(KITCHEN).len(), 1);
    shut_down(plane).await;
}

/// HA goes away once the intent run has started. The run may or may not have acted, so
/// it is never sent again and the turn ends `outcome_unknown`. The controller then says
/// "I'm not sure that worked." through HA's TTS, which needs HA: while HA stays away that
/// request fails at once (nothing is dispatched), the device hears no spoken line, and
/// no reconnection later resends either the intent or the line.
#[tokio::test(flavor = "multi_thread")]
async fn losing_home_assistant_after_dispatch_leaves_the_outcome_unknown() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture.ha.answers(IntentPlan {
        lose_connection: true,
        ..IntentPlan::answer("Turned on the light.")
    });

    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    let row = &rows[0];
    assert_eq!(row.terminal.as_deref(), Some("outcome_unknown"), "{row:?}");
    assert_eq!(row.stt_text.as_deref(), Some("turn on the kitchen light."));
    assert_eq!(intent_texts(&fixture.ha), ["turn on the kitchen light."]);
    // HA was down when the line was to be said: no TTS run reached it, nothing played.
    assert!(fixture.ha.state().runs("tts").is_empty());
    assert!(dialog_starts(&speaker).is_empty());

    // HA comes back; the controller reconnects and sends nothing for the lost turn.
    fixture.ha.state().refuse_connections = false;
    eventually("the controller to reconnect", || {
        (fixture.ha.state().connections >= 2).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(fixture.ha.state().runs("intent").len(), 1);
    assert!(fixture.ha.state().runs("tts").is_empty());
    assert!(dialog_starts(&speaker).is_empty());
    assert_eq!(fixture.rows(KITCHEN).len(), 1);
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_while_a_timer_rings_dismisses_it_on_the_controller() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    satellite_listening(&plane, KITCHEN).await;

    // HA's timer finishes on the satellite; the controller rings it on the speaker.
    let mut satellite = EsphomeClient::connect(fixture.esphome_ports[KITCHEN]).await;
    for event in [
        api::VoiceAssistantTimerEvent::VoiceAssistantTimerStarted,
        api::VoiceAssistantTimerEvent::VoiceAssistantTimerFinished,
    ] {
        satellite
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
    speaker
        .wait_message("the ring", |m| m["type"] == "alert.ring")
        .await;
    // The alert state that reports the ring has been taken before the wake.
    speaker.sync().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Between two bursts of the ring the user says "Ophelia, stop".
    speaker.wake("c1", false);
    speaker.say(words(&["stop"]));
    assert_eq!(speaker.candidate_ack("c1").await, "accepted");
    let act = speaker
        .wait_message("the dismissal", |m| m["type"] == "alert.act")
        .await;
    assert_eq!(act["body"]["action"], "dismiss", "{act}");
    assert_eq!(act["body"]["target_id"], "01JSYNTHTIMER", "{act}");
    assert_eq!(act["body"]["source"], "voice", "{act}");

    let rows = fixture.wait_rows(KITCHEN, 1).await;
    assert_eq!(rows[0].trigger.as_deref(), Some("wake"), "{rows:?}");
    assert_eq!(rows[0].terminal.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(
        rows[0].outcome.as_deref(),
        Some("local_command"),
        "{rows:?}"
    );
    // Handled before STT: Home Assistant transcribed nothing and ran no intent. The ring
    // stopping is the answer: nothing is said.
    assert!(fixture.ha.state().runs("stt").is_empty());
    assert!(fixture.ha.state().runs("intent").is_empty());
    assert!(fixture.ha.state().runs("tts").is_empty());
    assert_eq!(rows[0].response_text, None, "{rows:?}");
    shut_down(plane).await;
}

/// The controller's own spoken lines, in order (HA's TTS-only runs).
fn lines_said(ha: &SimHa) -> Vec<String> {
    ha.state()
        .runs("tts")
        .iter()
        .map(|run| run["input"]["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// The cancel intents the controller ran through `/api/intent/handle`.
fn timer_cancels(ha: &SimHa) -> Vec<Value> {
    ha.state()
        .intent_calls
        .iter()
        .filter(|call| call["name"] != "HassTimerStatus")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn timer_questions_are_answered_by_ha_whatever_timers_run() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    // HA answers "how much time is left" itself, with the custom sentences in its
    // configuration (the field's turns 454, 711, 921 and 924); the controller only sends
    // it, with the satellite's device.
    let cases = [
        (
            vec![("01KTIMERA", "", 2)],
            "Your 2 minute timer has 1 minute and 51 seconds left.",
        ),
        (
            vec![("01KTIMERA", "", 5), ("01KTIMERB", "pasta", 10)],
            "You have 2 timers: a 5 minute timer with 3 minutes left and a 10 minute pasta \
             timer with 8 minutes left.",
        ),
        (vec![], "You don't have any timers."),
    ];
    for (n, (timers, answer)) in cases.iter().enumerate() {
        {
            let mut ha = fixture.ha.state();
            ha.timers.clear();
            for (id, name, minutes) in timers {
                ha.start_timer(id, name, *minutes, KITCHEN);
            }
        }
        fixture.ha.hears("Ophelia, how much time is left?");
        fixture.ha.answers(IntentPlan::answer(answer));
        speaker.wake(&format!("c{n}"), false);
        speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
        let rows = fixture.wait_rows(KITCHEN, n + 1).await;
        let row = &rows[n];
        assert_eq!(row.terminal.as_deref(), Some("completed"), "{row:?}");
        assert_eq!(row.outcome.as_deref(), Some("ha"), "{row:?}");
        assert_eq!(row.response_text.as_deref(), Some(*answer), "{row:?}");
        speaker.say(silence(1_500));
    }
    let intents = fixture.ha.state().runs("intent");
    assert_eq!(intents.len(), 3, "{intents:?}");
    for run in &intents {
        assert_eq!(run["input"]["text"], "how much time is left?", "{run}");
        assert_eq!(run["device_id"], SimHa::ha_device(KITCHEN), "{run}");
    }
    // Nothing was cancelled, and the controller said nothing of its own.
    assert!(timer_cancels(&fixture.ha).is_empty());
    assert!(lines_said(&fixture.ha).is_empty());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_a_timer_by_name_goes_to_ha() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture
        .ha
        .state()
        .start_timer("01KTIMERB", "pasta", 10, KITCHEN);
    fixture.ha.hears("Ophelia, cancel the pasta timer.");
    fixture
        .ha
        .answers(IntentPlan::answer("10 minute pasta timer cancelled."));
    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    assert_eq!(rows[0].outcome.as_deref(), Some("ha"), "{rows:?}");
    assert_eq!(intent_texts(&fixture.ha), ["cancel the pasta timer."]);
    assert!(timer_cancels(&fixture.ha).is_empty());
    assert!(lines_said(&fixture.ha).is_empty());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_the_timer_cancels_this_speakers_only_timer_and_names_it() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    {
        let mut ha = fixture.ha.state();
        ha.start_timer("01KTIMERA", "", 2, KITCHEN);
        ha.start_timer("01KTIMERO", "", 2, "G090LF0000000099");
    }
    fixture.ha.hears("Ophelia, cancel the timer.");
    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    let row = &rows[0];
    assert_eq!(row.terminal.as_deref(), Some("completed"), "{row:?}");
    assert_eq!(row.outcome.as_deref(), Some("timer"), "{row:?}");
    assert_eq!(
        row.response_text.as_deref(),
        Some("2 minute timer cancelled."),
        "{row:?}"
    );
    assert_eq!(lines_said(&fixture.ha), ["2 minute timer cancelled."]);
    // One cancel, picking this speaker's timer by its length and the device; the other
    // speaker's two-minute timer runs on.
    let cancels = timer_cancels(&fixture.ha);
    assert_eq!(cancels.len(), 1, "{cancels:?}");
    assert_eq!(
        cancels[0],
        json!({"name": "HassCancelTimer", "data": {"start_minutes": 2},
               "device_id": SimHa::ha_device(KITCHEN)})
    );
    let left = fixture.ha.state().timers.clone();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], "01KTIMERO");
    // HA ran no conversation for it.
    assert!(fixture.ha.state().runs("intent").is_empty());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_the_timer_with_several_asks_which_and_takes_the_answer() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    {
        let mut ha = fixture.ha.state();
        ha.start_timer("01KTIMERA", "", 5, KITCHEN);
        ha.start_timer("01KTIMERB", "pasta", 10, KITCHEN);
    }
    fixture.ha.hears("Ophelia, cancel the timer.");
    fixture.ha.hears("The pasta one.");
    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    speaker
        .wait_message("the reply window", |m| {
            m["type"] == "uplink.open" && m["body"]["reason"] == "reply"
        })
        .await;
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let rows = fixture.wait_rows(KITCHEN, 2).await;
    let (question, answer) = (&rows[0], &rows[1]);
    assert_eq!(
        question.outcome.as_deref(),
        Some("clarification"),
        "{rows:?}"
    );
    assert_eq!(answer.trigger.as_deref(), Some("reply"), "{rows:?}");
    assert_eq!(answer.outcome.as_deref(), Some("timer"), "{rows:?}");
    assert_eq!(answer.reply_to, question.turn_uuid, "{rows:?}");
    assert_eq!(
        lines_said(&fixture.ha),
        [
            "Which one? Your 5 minute timer or your 10 minute pasta timer?",
            "10 minute pasta timer cancelled."
        ]
    );
    let left = fixture.ha.state().timers.clone();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["id"], "01KTIMERA");
    assert!(fixture.ha.state().runs("intent").is_empty());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_button_tap_starts_a_turn() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Turn on the kitchen light.");
    fixture
        .ha
        .answers(IntentPlan::answer("Turned on the light."));

    speaker.say(silence(1_000));
    let press = speaker.position();
    let seq = speaker.next_physical_seq();
    let tap = speaker.shared().sim().tap(press, seq);
    speaker.send_at("button.action", Value::Object(tap));
    let open = speaker
        .wait_message("the button turn's subscription", |m| {
            m["type"] == "uplink.open"
        })
        .await;
    assert_eq!(open["body"]["reason"], "turn", "{open}");
    speaker.say(silence(300));
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));

    let rows = fixture.wait_rows(KITCHEN, 1).await;
    let row = &rows[0];
    assert_eq!(row.trigger.as_deref(), Some("button"), "{row:?}");
    assert_eq!(row.terminal.as_deref(), Some("completed"), "{row:?}");
    assert_eq!(row.outcome.as_deref(), Some("ha"), "{row:?}");
    assert_eq!(
        row.stt_text.as_deref(),
        Some("Turn on the kitchen light."),
        "{row:?}"
    );
    assert_eq!(intent_texts(&fixture.ha), ["Turn on the kitchen light."]);
    assert_eq!(speaker.frames_before_ack(), Vec::<String>::new());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conversation_that_continues_takes_a_reply_without_the_wake_word() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture.ha.hears("What time is it?");
    fixture.ha.answers(IntentPlan {
        continue_conversation: true,
        conversation: "01K8CONTINUED".to_owned(),
        ..IntentPlan::answer("Which light?")
    });
    fixture.ha.answers(IntentPlan {
        conversation: "01K8CONTINUED".to_owned(),
        ..IntentPlan::answer("It is nine.")
    });

    speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    // Once the question has played out, the controller listens for the reply.
    let reply = speaker
        .wait_message("the reply window", |m| {
            m["type"] == "uplink.open" && m["body"]["reason"] == "reply"
        })
        .await;
    // The question's playback was reported drained before the window opened.
    let question = dialog_starts(&speaker)[0]["body"]["playback_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let finished = speaker.shared().finished.clone();
    assert!(
        finished.contains(&(question, "drained")),
        "the window opened before the question drained: {finished:?} {reply}"
    );
    speaker.say(words(&["what", "time", "is", "it"]));

    let rows = fixture.wait_rows(KITCHEN, 2).await;
    let (first, second) = (&rows[0], &rows[1]);
    assert_eq!(first.terminal.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(first.conversation.as_deref(), Some("01K8CONTINUED"));
    assert_eq!(second.trigger.as_deref(), Some("reply"), "{rows:?}");
    assert_eq!(second.terminal.as_deref(), Some("completed"), "{rows:?}");
    assert_eq!(
        second.stt_text.as_deref(),
        Some("What time is it?"),
        "{rows:?}"
    );
    assert_eq!(second.reply_to, first.turn_uuid, "{rows:?}");
    // The reply went to HA in the same conversation.
    let intents = fixture.ha.state().runs("intent");
    assert_eq!(intents.len(), 2, "{intents:?}");
    assert_eq!(intents[1]["input"]["text"], "What time is it?");
    assert_eq!(intents[1]["conversation_id"], "01K8CONTINUED");
    let first = eventually("the question's continuation to resolve", || {
        let rows = fixture.rows(KITCHEN);
        (rows[0].continuation.as_deref() != Some("pending")).then(|| rows[0].clone())
    })
    .await;
    assert_eq!(first.continuation.as_deref(), Some("answered"), "{first:?}");
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wake_without_a_command_ends_with_the_no_input_cue_and_chime() {
    let fixture = Fixture::new(&[DeviceRow {
        config: r#"{"wakeSound": true}"#.into(),
        sections: r#"["wakeword"]"#,
        ..DeviceRow::plain(KITCHEN, "Kitchen")
    }])
    .await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &["no_input_chime"]).await;

    speaker.wake("c1", false);
    assert_eq!(speaker.candidate_ack("c1").await, "accepted");
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    assert_eq!(rows[0].trigger.as_deref(), Some("wake"), "{rows:?}");
    assert_eq!(rows[0].terminal.as_deref(), Some("no_input"), "{rows:?}");
    let chime = speaker
        .wait_message("the no-input chime", |m| {
            m["type"] == "render.start" && m["body"]["local_asset"] == "builtin:no_input_chime"
        })
        .await;
    assert_eq!(chime["body"]["source_class"], "earcon", "{chime}");
    let cue = speaker
        .wait_message("the no-speech cue", |m| {
            m["type"] == "led_anim"
                && m["body"]["anim"]["pattern"] == "pulse"
                && m["body"]["anim"]["periodMs"] == 900
        })
        .await;
    assert_eq!(cue["body"]["anim"]["ttlSec"], 1, "{cue}");
    assert!(fixture.ha.state().runs("stt").is_empty());
    assert!(fixture.ha.state().runs("intent").is_empty());
    shut_down(plane).await;
}

// ── The real binary ──────────────────────────────────────────────────────────────────

/// 16 kHz mono PCM16 samples of a WAV file.
fn read_wav(path: &Path) -> Vec<i16> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&bytes[..4], b"RIFF", "{} is not a WAV file", path.display());
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];
        if id == b"fmt " {
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([body[14], body[15]]);
            assert_eq!(
                (channels, rate, bits),
                (1, 16_000, 16),
                "{}",
                path.display()
            );
        }
        if id == b"data" {
            return body
                .as_chunks::<2>()
                .0
                .iter()
                .map(|s| i16::from_le_bytes(*s))
                .collect();
        }
        at += 8 + size + (size & 1);
    }
    panic!("{} has no data chunk", path.display());
}

fn smoke_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

/// The release binary with the real speech bundle, a simulated Home Assistant and a
/// simulated speaker streaming a real wake recording, read in place. Prints the turn
/// row. Needs:
///
/// - `ECHOMUSE_SMOKE_BIN`: the controller binary;
/// - `SPEECH_BUNDLE_DIR`, `ONNXRUNTIME_LIB`, `NS_MODEL_DIR`, `ORT_ANDROID_LIB`: as for
///   the controller;
/// - `ECHOMUSE_SMOKE_WAV`: a 16 kHz mono recording of the wake word and a command;
/// - `ECHOMUSE_SMOKE_SUPPORT_START`, `ECHOMUSE_SMOKE_OPEN`: where in it the device's
///   candidate was supported from and opened (capture samples from the file's start);
/// - `ECHOMUSE_SMOKE_TRANSCRIPT`: what Home Assistant's STT hears;
/// - `ECHOMUSE_SMOKE_KEEP` (optional): keep the data directory and print its path.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a smoke run of the release binary; needs the ECHOMUSE_SMOKE_* variables"]
async fn the_release_binary_answers_a_recorded_wake() {
    let binary = smoke_env("ECHOMUSE_SMOKE_BIN");
    let wav = PathBuf::from(smoke_env("ECHOMUSE_SMOKE_WAV"));
    let support: u64 = smoke_env("ECHOMUSE_SMOKE_SUPPORT_START").parse().unwrap();
    let open: u64 = smoke_env("ECHOMUSE_SMOKE_OPEN").parse().unwrap();
    let transcript = smoke_env("ECHOMUSE_SMOKE_TRANSCRIPT");
    let recording = read_wav(&wav);
    assert!(open <= recording.len() as u64 && support < open);

    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    // The image's layout relative to the working directory: the dashboard and the
    // deployed wake model, linked rather than copied.
    let work = fixture.dir.path().join("app");
    std::fs::create_dir_all(work.join("static")).unwrap();
    for name in ["bcresnet_audio.onnx", "bcresnet_audio.json"] {
        std::os::unix::fs::symlink(repo_root().join(name), work.join(name)).unwrap();
    }
    let mut env = fixture.env.clone();
    for name in [
        "SPEECH_BUNDLE_DIR",
        "ONNXRUNTIME_LIB",
        "NS_MODEL_DIR",
        "ORT_ANDROID_LIB",
    ] {
        env.insert(name.to_owned(), smoke_env(name));
    }
    let mut child = tokio::process::Command::new(&binary)
        .current_dir(&work)
        .env_clear()
        .envs(&env)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "info")
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("the controller starts");
    let stderr = child.stderr.take().unwrap();
    let (ready_tx, ready) = oneshot::channel::<Result<(), String>>();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut ready_tx = Some(ready_tx);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&raw).trim_end().to_owned();
            eprintln!("controller: {line}");
            let outcome = if line.contains("speech models are ready") {
                Some(Ok(()))
            } else if line.contains("speech is unavailable") {
                Some(Err(line.clone()))
            } else {
                None
            };
            if let Some(outcome) = outcome
                && let Some(tx) = ready_tx.take()
            {
                let _ = tx.send(outcome);
            }
        }
        if let Some(tx) = ready_tx {
            let _ = tx.send(Err(
                "the controller exited before speech was ready".to_owned()
            ));
        }
    });
    // Each bundle model is qualified in a child process first (up to 120 s each).
    tokio::time::timeout(Duration::from_secs(400), ready)
        .await
        .expect("speech to load in time")
        .expect("the log reader to answer")
        .expect("speech is ready");

    let address: SocketAddr = format!("127.0.0.1:{}", env["SERVER_PORT"]).parse().unwrap();
    let speaker = Speaker::connect(address, KITCHEN, &[]).await;
    fixture.ha.hears(&transcript);
    fixture.ha.answers(IntentPlan::answer("Done."));
    // A running device: quiet first, then the recording, the candidate where the device
    // opened it, the rest of the recording and quiet for the endpoint.
    speaker.say(silence(3_000));
    let base = speaker.position();
    speaker.say(recording[..open as usize].to_vec());
    speaker.push(Item::Candidate {
        id: "smoke".to_owned(),
        support_start: base + support,
        open: base + open,
        producing_sound: false,
    });
    speaker.say(recording[open as usize..].to_vec());
    speaker.say(silence(4_000));
    assert_eq!(speaker.candidate_ack("smoke").await, "accepted");
    let rows = fixture.wait_rows(KITCHEN, 1).await;
    eprintln!("turn row: {:#?}", rows[0]);
    eprintln!("HA STT runs: {:?}", fixture.ha.state().runs("stt"));
    eprintln!("HA intent texts: {:?}", intent_texts(&fixture.ha));

    let _ = tokio::process::Command::new("kill")
        .arg(child.id().expect("running").to_string())
        .status()
        .await;
    let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
    if std::env::var_os("ECHOMUSE_SMOKE_KEEP").is_some() {
        let kept = fixture.dir.keep();
        eprintln!("data directory kept: {}", kept.display());
    }
}

// ── Failures (§5.5, §5.7 step 5) ─────────────────────────────────────────────────────

/// Faults the stand-in models inject into the next VAD call of any worker.
#[derive(Default)]
struct Faults {
    /// The next call panics: a speech-worker failure.
    panic_next: std::sync::atomic::AtomicBool,
    /// The next call blocks until released: a native call that does not return.
    block_next: std::sync::atomic::AtomicBool,
    blocked: std::sync::atomic::AtomicBool,
    released: Mutex<bool>,
    wake: std::sync::Condvar,
}

impl Faults {
    fn strike(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        if self.panic_next.swap(false, SeqCst) {
            panic!("injected speech worker panic");
        }
        if self.block_next.swap(false, SeqCst) {
            self.blocked.store(true, SeqCst);
            let mut released = self.released.lock().unwrap();
            while !*released {
                released = self.wake.wait(released).unwrap();
            }
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

/// The tone stand-ins, with [`Faults`] injected into their VAD.
struct FaultyModels {
    inner: ToneModels,
    faults: Arc<Faults>,
}

struct FaultyVad {
    inner: Box<dyn echomuse_voice::speech::models::Vad>,
    faults: Arc<Faults>,
}

impl echomuse_voice::speech::models::Vad for FaultyVad {
    fn process_cell(&mut self, cell: &[f32; 512]) -> Result<f32, echomuse_speech::SpeechError> {
        self.faults.strike();
        self.inner.process_cell(cell)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

impl echomuse_voice::speech::models::ModelSet for FaultyModels {
    fn asr_stream(
        &self,
    ) -> Result<Box<dyn echomuse_voice::speech::models::AsrStream>, echomuse_speech::SpeechError>
    {
        self.inner.asr_stream()
    }

    fn vad(
        &self,
    ) -> Result<Box<dyn echomuse_voice::speech::models::Vad>, echomuse_speech::SpeechError> {
        Ok(Box::new(FaultyVad {
            inner: self.inner.vad()?,
            faults: Arc::clone(&self.faults),
        }))
    }

    fn denoiser(
        &self,
    ) -> Result<Box<dyn echomuse_voice::speech::models::Denoise>, echomuse_speech::SpeechError>
    {
        self.inner.denoiser()
    }

    fn wake_scorer(
        &self,
        files: &echomuse_speech::BcresnetModelFiles,
    ) -> Result<echomuse_voice::speech::models::BoxedWakeScorer, echomuse_speech::SpeechError> {
        self.inner.wake_scorer(files)
    }
}

/// What the fatal hook was asked to do, instead of ending the test process.
fn recording_hook() -> (FatalHook, Arc<Mutex<Vec<String>>>) {
    let reasons = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&reasons);
    let hook = FatalHook::new(move |reason| recorded.lock().unwrap().push(reason.to_owned()));
    (hook, reasons)
}

/// Sends a hello as `device` and returns the controller's reply; `None` when it closed
/// the socket instead, as it does when it declines the session.
async fn hello_reply(address: SocketAddr, device: &str) -> Option<Value> {
    let socket = dial(address, device, "/device/v1/control", None).await;
    let (mut sink, mut incoming) = socket.split();
    let capabilities: Vec<&str> = REQUIRED_CAPABILITIES.iter().map(|c| c.as_str()).collect();
    let hello = json!({
        "capabilities": capabilities, "platform": "fireos6", "protocols": [1],
        "firmware_version": "fw-synthetic-2", "os_version": "Fire OS 6 (synthetic)",
        "privacy": {"muted": false, "capture_epoch": "7"},
    });
    let (text, _) = envelope(device, None, "session.hello", 0, hello);
    sink.send(Message::text(text)).await.unwrap();
    within("the reply to a hello", next_json(&mut incoming)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_speech_worker_panic_rebuilds_only_that_device_and_its_session_returns() {
    let fixture = Fixture::new(&[
        DeviceRow::plain(KITCHEN, "Kitchen"),
        DeviceRow::plain(HALL, "Hall"),
    ])
    .await;
    let faults = Arc::new(Faults::default());
    let (fatal, fatal_reasons) = recording_hook();
    let plane = fixture
        .start_with(
            Arc::new(FaultyModels {
                inner: models(),
                faults: Arc::clone(&faults),
            }),
            SupervisionOptions {
                fatal,
                restart: RestartPolicy {
                    initial: Duration::from_secs(2),
                    max: Duration::from_secs(8),
                    stable: Duration::from_secs(60),
                },
                ..SupervisionOptions::default()
            },
        )
        .await;
    let kitchen = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    let hall = Speaker::connect(plane.device_address(), HALL, &[]).await;
    let mut kitchen_state = device_snapshots(&plane, KITCHEN);
    let hall_before = device_snapshots(&plane, HALL).borrow().clone();
    let first_runtime = kitchen_state.borrow().runtime;

    // The kitchen hears a wake; analysing it panics on the kitchen's worker.
    faults
        .panic_next
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _candidate = kitchen.wake("c1", false);
    kitchen.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let failed = within(
        "the kitchen's runtime to fail",
        kitchen_state
            .wait_for(|state| matches!(state.health.availability, Availability::Restarting { .. })),
    )
    .await
    .unwrap()
    .clone();
    let Availability::Restarting { reason, retry_in } = &failed.health.availability else {
        unreachable!("waited for it");
    };
    assert!(reason.contains("worker thread panicked"), "{reason}");
    assert_eq!(*retry_in, Duration::from_secs(2), "the first backoff");
    assert_eq!(failed.runtime, first_runtime);
    assert_eq!(failed.session, None, "its session was closed");
    assert_eq!(failed.health.errors.failures, 1);
    // The dashboard sees it unavailable, with the reason.
    let shown = echomuse_controller::dashboard::live_device(&failed).fields()["health"].clone();
    assert_eq!(shown["available"], false, "{shown}");
    assert_eq!(shown["restarting"], true, "{shown}");
    assert_eq!(shown["recent_failures"], 1, "{shown}");
    assert!(
        shown["reason"]
            .as_str()
            .unwrap()
            .contains("worker thread panicked")
    );

    // While it waits to be rebuilt, the kitchen's sessions are declined.
    assert_eq!(hello_reply(plane.device_address(), KITCHEN).await, None);

    // The hall never noticed.
    let hall_now = device_snapshots(&plane, HALL).borrow().clone();
    assert_eq!(hall_now.runtime, hall_before.runtime);
    assert_eq!(hall_now.session, hall_before.session);
    assert_eq!(hall_now.health.availability, Availability::Available);
    assert_eq!(hall_now.health.errors.failures, 0);

    // After the backoff a fresh runtime takes the kitchen, and its device is admitted.
    let rebuilt = within(
        "the kitchen's runtime to be rebuilt",
        kitchen_state.wait_for(|state| {
            state.runtime != first_runtime && state.health.availability == Availability::Available
        }),
    )
    .await
    .unwrap()
    .clone();
    assert_eq!(
        rebuilt.health.errors.failures, 1,
        "the error window remembers"
    );
    assert_eq!(
        plane
            .registry()
            .directory()
            .get(&DeviceId(KITCHEN.to_owned()))
            .map(|handle| handle.runtime()),
        Some(rebuilt.runtime)
    );
    drop(kitchen);
    let kitchen = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    let attached = within(
        "the kitchen's new session",
        kitchen_state.wait_for(|state| state.session.is_some()),
    )
    .await
    .unwrap()
    .clone();
    assert_eq!(attached.runtime, rebuilt.runtime);
    assert_eq!(
        device_snapshots(&plane, HALL).borrow().runtime,
        hall_before.runtime
    );
    assert!(fatal_reasons.lock().unwrap().is_empty());
    drop((kitchen, hall));
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stuck_model_call_refuses_that_devices_wakes_and_then_ends_the_process() {
    let fixture = Fixture::new(&[
        DeviceRow::plain(KITCHEN, "Kitchen"),
        DeviceRow::plain(HALL, "Hall"),
    ])
    .await;
    let faults = Arc::new(Faults::default());
    let (fatal, fatal_reasons) = recording_hook();
    let plane = fixture
        .start_with(
            Arc::new(FaultyModels {
                inner: models(),
                faults: Arc::clone(&faults),
            }),
            SupervisionOptions {
                fatal,
                watchdog: echomuse_controller::supervision::WatchdogConfig {
                    call_deadline: Duration::from_millis(300),
                    recovery_deadline: Duration::from_secs(4),
                },
                ..SupervisionOptions::default()
            },
        )
        .await;
    let kitchen = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    let hall = Speaker::connect(plane.device_address(), HALL, &[]).await;
    let mut kitchen_state = device_snapshots(&plane, KITCHEN);

    // The kitchen's microphone worker gets stuck inside a model call.
    faults
        .block_next
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _stuck = kitchen.wake("c1", false);
    kitchen.say(words(&["turn", "on", "the", "kitchen", "light"]));
    let unavailable = within(
        "the kitchen's speech path to be marked stuck",
        kitchen_state.wait_for(|state| {
            matches!(
                state.health.availability,
                Availability::SpeechUnavailable { .. }
            )
        }),
    )
    .await
    .unwrap()
    .clone();
    assert_eq!(
        unavailable.health.availability.reason(),
        Some("the microphone analysis speech worker is stuck in a model call past its deadline")
    );
    assert!(faults.blocked.load(std::sync::atomic::Ordering::SeqCst));

    // Its next wake is refused as speech_unavailable; the hall's is answered.
    kitchen.say(silence(1_000));
    let _refused = kitchen.wake("c2", false);
    kitchen.say(words(&["what", "time", "is", "it"]));
    // The first, stuck candidate ends on its own; the second is the refused one.
    let refusal = eventually("the kitchen's wake to be refused", || {
        fixture
            .rows(KITCHEN)
            .into_iter()
            .find(|row| row.terminal.as_deref() == Some("speech_unavailable"))
    })
    .await;
    assert_eq!(refusal.trigger.as_deref(), Some("wake"), "{refusal:?}");
    assert_eq!(
        device_snapshots(&plane, HALL).borrow().health.availability,
        Availability::Available
    );
    // Both kitchen candidates have ended (the stuck one ends on its own), so no claim
    // of the kitchen's can stand in the hall's way.
    fixture.wait_rows(KITCHEN, 2).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    fixture
        .ha
        .answers(IntentPlan::answer("Turned on the light."));
    let hall_wake = hall.wake("h1", false);
    hall.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(hall.candidate_ack(&hall_wake).await, "accepted");
    let hall_rows = fixture.wait_rows(HALL, 1).await;
    assert_eq!(
        hall_rows[0].terminal.as_deref(),
        Some("completed"),
        "{hall_rows:?}"
    );

    // Past the recovery deadline the process would be ended (the hook records it here).
    let reason = eventually("the watchdog to end the process", || {
        fatal_reasons.lock().unwrap().first().cloned()
    })
    .await;
    assert!(reason.contains(&format!("device {KITCHEN}")), "{reason}");
    assert!(reason.contains("microphone analysis"), "{reason}");

    // In this test the call does return; the late result is thrown away and the speech
    // path works again.
    faults.release();
    within(
        "the kitchen's speech path to work again",
        kitchen_state.wait_for(|state| state.health.availability == Availability::Available),
    )
    .await
    .unwrap();
    assert_eq!(kitchen_state.borrow().health.errors.failures, 1);
    drop((kitchen, hall));
    shut_down(plane).await;
}

// ── Correction, early answers, AFE charts, recordings, config, timers, previews ───────

/// A device row whose sections override the fleet with `config`.
fn configured(
    id: &'static str,
    config: impl Into<std::borrow::Cow<'static, str>>,
    sections: &'static str,
) -> DeviceRow {
    DeviceRow {
        id,
        label: "Kitchen",
        config: config.into(),
        sections,
    }
}

/// The columns of the device's turn rows these tests read, oldest first:
/// (id, turn_uuid, stt_fix, afe_series, continuation, reply_to, trigger_type).
#[allow(clippy::type_complexity)]
fn feature_rows(
    fixture: &Fixture,
    device: &str,
) -> Vec<(
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
)> {
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    let mut statement = conn
        .prepare(
            "SELECT id, turn_uuid, stt_fix, afe_series, continuation, reply_to, trigger_type \
             FROM turns WHERE device_id = ?1 ORDER BY id",
        )
        .unwrap();
    statement
        .query_map([device], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

/// One answered wake turn: "turn on the kitchen light".
async fn answered_wake(fixture: &Fixture, speaker: &Speaker) {
    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    fixture.wait_rows(KITCHEN, 1).await;
}

/// An administrator's dashboard session, inserted as the sessions table holds one.
fn admin_session(fixture: &Fixture) -> String {
    let session = "b".repeat(64);
    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    conn.execute(
        "INSERT INTO users (id, username, password_hash, role, created_at) \
         VALUES (2, 'admin', '$2b$04$synthetic', 'admin', 1600000000)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) \
         VALUES (?1, 2, 1600000000, 4000000000)",
        [&session],
    )
    .unwrap();
    session
}

/// A plain HTTP/1.1 POST of `body` on the dashboard API with a session.
async fn api_post(address: SocketAddr, path: &str, session: &str, body: &Value) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).await.unwrap();
    let body = body.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {session}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8(response).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shadow_correction_is_recorded_without_changing_what_ha_hears() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen lite.");
    answered_wake(&fixture, &speaker).await;
    let rows = eventually("the correction record", || {
        let rows = feature_rows(&fixture, KITCHEN);
        rows.first().and_then(|r| r.2.clone()).map(|_| rows)
    })
    .await;
    let fix: Value = serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
    assert_eq!(fix["mode"], "shadow", "{fix}");
    assert_eq!(fix["applied"], false, "{fix}");
    // Shadow mode never changes the text.
    assert_eq!(intent_texts(&fixture.ha), ["turn on the kitchen lite."]);
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_on_correction_sends_what_ha_understands_and_records_it() {
    let fixture = Fixture::new(&[configured(
        KITCHEN,
        r#"{"transcriptFix": "on"}"#,
        r#"["transcript"]"#,
    )])
    .await;
    {
        let mut state = fixture.ha.state();
        // HA's matcher does not understand what was heard, and understands the light.
        state
            .recognitions
            .insert("turn on the kitchen lite.".to_owned(), Value::Null);
        state.recognitions.insert(
            "turn on the Kitchen Light".to_owned(),
            json!({"intent": {"name": "HassTurnOn"},
                   "slots": {"name": "Kitchen Light"},
                   "details": {"name": {"name": "name", "value": "Kitchen Light",
                                        "text": "kitchen light"}},
                   "targets": {"light.kitchen": {"matched": true}}, "match": true,
                   "sentence_template": "turn on [the] {name}",
                   "unmatched_slots": {}, "source": "builtin"}),
        );
    }
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen lite.");
    answered_wake(&fixture, &speaker).await;
    let rows = feature_rows(&fixture, KITCHEN);
    let fix: Value = serde_json::from_str(rows[0].2.as_deref().expect("recorded")).unwrap();
    assert_eq!(fix["mode"], "on", "{fix}");
    // The tables were built from HA's names; the name rule proposed the light, HA's
    // matcher understood it, and it was sent in place of what was heard.
    assert_eq!(fix["applied"], true, "{fix}");
    assert_eq!(fix["reason"], "applied", "{fix}");
    assert_eq!(fix["original"], "turn on the kitchen lite.", "{fix}");
    assert_eq!(fix["sent"], "turn on the Kitchen Light", "{fix}");
    assert_eq!(fix["deadline_met"], true, "{fix}");
    assert_eq!(intent_texts(&fixture.ha), ["turn on the Kitchen Light"]);
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_automation_trigger_is_a_known_phrase_for_correction() {
    let fixture = Fixture::new(&[configured(
        KITCHEN,
        r#"{"transcriptFix": "on"}"#,
        r#"["transcript"]"#,
    )])
    .await;
    {
        let mut state = fixture.ha.state();
        state.automations.push((
            "1700".to_owned(),
            json!({"alias": "Movies", "triggers": [
                {"trigger": "conversation", "command": ["watch (plex|youtube tv)"]}]}),
        ));
        // HA's matcher does not understand what was heard; it matches the trigger.
        state
            .recognitions
            .insert("watch flex.".to_owned(), Value::Null);
        state.recognitions.insert(
            "watch plex".to_owned(),
            json!({"match": true, "source": "trigger",
                   "sentence_template": "watch (plex|youtube tv)",
                   "unmatched_slots": {}}),
        );
    }
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    // The tables are rebuilt with the trigger once HA's automations were read.
    tokio::time::sleep(Duration::from_millis(500)).await;
    fixture.ha.hears("Ophelia, watch flex.");
    answered_wake(&fixture, &speaker).await;
    let rows = feature_rows(&fixture, KITCHEN);
    let fix: Value = serde_json::from_str(rows[0].2.as_deref().expect("recorded")).unwrap();
    assert_eq!(fix["applied"], true, "{fix}");
    assert_eq!(fix["sent"], "watch plex", "{fix}");
    assert_eq!(intent_texts(&fixture.ha), ["watch plex"]);
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_while_the_question_plays_cuts_it_off_and_is_the_reply() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    // Nothing usable is heard: the controller asks "Sorry, I didn't catch that." The
    // answer comes while that still plays.
    fixture.ha.hears("");
    fixture.ha.hears("turn on the kitchen light.");
    speaker.shared().hold_playback = true;
    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    speaker
        .wait_message("the question's playback", |m| {
            m["type"] == "render.start" && m["body"]["source_class"] == "dialog_output"
        })
        .await;
    speaker
        .wait_message("the early-answer watch", |m| {
            m["type"] == "uplink.open"
                && m["body"]["reason"] == "reply"
                && m["body"]["owner"]
                    .as_str()
                    .is_some_and(|o| o.starts_with("early-"))
        })
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let index = models().index("turn").unwrap();
    speaker.say(echomuse_voice::speech::testing::tone(index, 1_200, 6_000));
    speaker
        .wait_message("the question cut off", |m| m["type"] == "render.cancel")
        .await;
    speaker.shared().hold_playback = false;
    speaker.say(silence(1_500));
    let rows = fixture.wait_rows(KITCHEN, 2).await;
    let features = feature_rows(&fixture, KITCHEN);
    assert_eq!(
        rows[0].terminal.as_deref(),
        Some("empty_transcript"),
        "{rows:?}"
    );
    assert_eq!(features[1].6.as_deref(), Some("reply"), "{rows:?}");
    assert_eq!(
        features[1].5, features[0].1,
        "the reply answers the question"
    );
    assert_eq!(features[0].4.as_deref(), Some("answered"), "{rows:?}");
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_afe_turn_stores_its_chart_and_publishes_its_recordings() {
    let fixture = Fixture::new(&[configured(
        KITCHEN,
        r#"{"saveUtterances": true}"#,
        r#"["microphones"]"#,
    )])
    .await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &["afe_metadata_v1"]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    answered_wake(&fixture, &speaker).await;
    let open = speaker
        .wait_message("the response subscription", |m| {
            m["type"] == "uplink.open"
                && m["body"]["owner"]
                    .as_str()
                    .is_some_and(|o| o.starts_with("response-"))
        })
        .await;
    assert!(open["body"]["streams"]["afe"].is_string(), "{open}");
    let rows = feature_rows(&fixture, KITCHEN);
    let series: Value = serde_json::from_str(rows[0].3.as_deref().expect("a chart")).unwrap();
    assert_eq!(series["period"], 1_280, "{series}");
    assert!(series["frames"].as_array().is_some_and(|f| f.len() > 20));
    let recording = series["recording"].as_array().expect("a recording span");
    let span = recording[1].as_u64().unwrap() - recording[0].as_u64().unwrap();
    // Both files exist once published: the utterance, and the turn recording.
    let id = rows[0].0;
    let dir = fixture.db().parent().unwrap().to_path_buf();
    let turn_file = dir.join(format!("turn_recordings/{KITCHEN}_{id}.wav"));
    eventually("the turn recording", || turn_file.exists().then_some(())).await;
    assert_eq!(read_wav(&turn_file).len() as u64, span);
    assert!(dir.join(format!("recordings/{KITCHEN}_{id}.wav")).exists());
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_whose_session_ends_keeps_its_recording_and_correction_record() {
    let fixture = Fixture::new(&[configured(
        KITCHEN,
        r#"{"saveUtterances": true}"#,
        r#"["microphones"]"#,
    )])
    .await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &["afe_metadata_v1"]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen lite.");
    speaker.shared().hold_playback = true;
    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["turn", "on", "the", "kitchen", "light"]));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    speaker
        .wait_message("the answer's playback", |m| {
            m["type"] == "render.start" && m["body"]["source_class"] == "dialog_output"
        })
        .await;
    // The device goes away while the answer plays: the turn ends with its session,
    // and nothing the session held may be lost.
    drop(speaker);
    fixture.wait_rows(KITCHEN, 1).await;
    let rows = eventually("the correction record", || {
        let rows = feature_rows(&fixture, KITCHEN);
        rows.first().and_then(|r| r.2.clone()).map(|_| rows)
    })
    .await;
    let fix: Value = serde_json::from_str(rows[0].2.as_deref().unwrap()).unwrap();
    assert_eq!(fix["mode"], "shadow", "{fix}");
    assert!(rows[0].3.is_some(), "the chart was stored");
    let id = rows[0].0;
    let dir = fixture.db().parent().unwrap().to_path_buf();
    let turn_file = dir.join(format!("turn_recordings/{KITCHEN}_{id}.wav"));
    eventually("the turn recording", || turn_file.exists().then_some(())).await;
    shut_down(plane).await;
}

/// Every finished session recording under the data directory.
fn session_recording_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.join("session_recordings")];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|e| e == "emrec") {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_session_replays_to_the_same_wake_and_turn_outcomes() {
    use echomuse_recorder::{Equivalence, Recording, ReplayOptions, replay};
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let session = admin_session(&fixture);
    let plane = fixture.start().await;
    let (status, body) = api_post(
        plane.api_address(),
        &format!("/api/devices/{KITCHEN}/session_recorder"),
        &session,
        &json!({"minutes": 10}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, turn on the kitchen light.");
    answered_wake(&fixture, &speaker).await;
    drop(speaker);
    let dir = fixture.db().parent().unwrap().to_path_buf();
    let file = eventually("the finished session recording", || {
        session_recording_files(&dir).pop()
    })
    .await;
    let recording = Recording::read(&file).unwrap();
    let report = replay(&recording, &ReplayOptions::default());
    assert_eq!(report.wakes.len(), 1, "{report:#?}");
    assert!(report.wakes[0].matches(), "{report:#?}");
    assert_eq!(report.turns.len(), 1, "{report:#?}");
    assert!(report.turns[0].recorded.committed.is_some(), "{report:#?}");
    assert!(report.divergences.is_empty(), "{:#?}", report.divergences);
    assert_eq!(report.equivalence(), Equivalence::Proven, "{report:#?}");

    // A replay under a policy that waits far longer never commits: not equivalent.
    let mut slow = echomuse_turn::TurnConfig::default();
    slow.endpoint.pause_complete = echomuse_turn::Samples(16_000 * 20);
    slow.endpoint.pause_extendable = echomuse_turn::Samples(16_000 * 20);
    slow.endpoint.pause_needs_more = echomuse_turn::Samples(16_000 * 20);
    let slow = replay(
        &recording,
        &ReplayOptions {
            turn_config: Some(slow),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(slow.equivalence(), Equivalence::Differs, "{slow:#?}");
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn config_goes_to_the_device_at_attach_and_after_a_settings_change() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let session = admin_session(&fixture);
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &["local_wake_chime"]).await;
    let first = speaker
        .wait_message("config at attach", |m| m["type"] == "config")
        .await;
    assert_eq!(first["body"]["wakeSound"], false, "{first}");
    assert_eq!(first["body"]["startupVolume"], 85, "{first}");
    let (status, body) = api_post(
        plane.api_address(),
        &format!("/api/devices/{KITCHEN}/config"),
        &session,
        &json!({"config_sections": ["wakeword"], "wakeSound": true}),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let second = speaker
        .wait_message("config after the change", |m| {
            m["type"] == "config" && m["body"]["wakeSound"] == true
        })
        .await;
    assert!(second["body"].get("kioskToken").is_none(), "{second}");
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_timer_draws_the_arc_and_reaches_a_timer_view_every_second() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &["timer_view_v1"]).await;
    satellite_listening(&plane, KITCHEN).await;
    let mut satellite = EsphomeClient::connect(fixture.esphome_ports[KITCHEN]).await;
    let leds_before = speaker.messages("leds").len();
    satellite
        .send(&api::VoiceAssistantTimerEventResponse {
            event_type: api::VoiceAssistantTimerEvent::VoiceAssistantTimerStarted as i32,
            timer_id: "01JSYNTHTIMER".to_owned(),
            name: "pasta".to_owned(),
            total_seconds: 600,
            seconds_left: 600,
            is_active: true,
        })
        .await;
    let view = speaker
        .wait_message("a timer view", |m| {
            m["type"] == "timer.view" && m["body"]["timers"][0]["id"] == "01JSYNTHTIMER"
        })
        .await;
    assert_eq!(view["body"]["timers"][0]["total_ms"], 600_000, "{view}");
    // Resent each second while the timer runs.
    eventually("a second timer view", || {
        (speaker.messages("timer.view").len() >= 3).then_some(())
    })
    .await;
    // The arc: the ring shows the timer.
    eventually("the timer arc", || {
        (speaker.messages("leds").len() > leds_before).then_some(())
    })
    .await;
    shut_down(plane).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sound_preview_starts_on_the_device_and_stops_on_request() {
    let fixture = Fixture::new(&[DeviceRow::plain(KITCHEN, "Kitchen")]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    let handle = plane
        .registry()
        .directory()
        .get(&DeviceId(KITCHEN.to_owned()))
        .expect("a runtime")
        .clone();
    let sha = "ab".repeat(32);
    let sound = echomuse_controller::device::AlertPreviewSound::Sound(
        echomuse_protocol::Sha256::parse(&sha).unwrap(),
    );
    within("the preview to start", handle.preview_sound(sound))
        .await
        .expect("started");
    let start = speaker
        .wait_message("the preview's render.start", |m| {
            m["type"] == "render.start" && m["body"]["source_class"] == "alert_preview"
        })
        .await;
    assert_eq!(start["body"]["local_asset"], sha.as_str(), "{start}");
    assert!(start["body"]["epoch"].is_null(), "{start}");
    let stopped = within("the preview to stop", handle.stop_sound_preview())
        .await
        .expect("stopped");
    assert!(stopped);
    speaker
        .wait_message("the preview's render.cancel", |m| {
            m["type"] == "render.cancel"
        })
        .await;
    shut_down(plane).await;
}

/// Pause transcription by a Wyoming server (§4.7) through the whole controller: a command
/// the grammar cannot judge, a pause, and the simulated server's complete words end the
/// turn; HA still transcribes the audio, and the row records the server's words.
#[tokio::test(flavor = "multi_thread")]
async fn the_pause_servers_words_end_the_turn_and_the_row_records_them() {
    use echomuse_wyoming::sim::{Script, SimServer};
    let server = SimServer::start(Script::whisper(
        &["base-int8"],
        &["Ophelia, what is the time?"; 4],
    ))
    .await
    .unwrap();
    let config = format!(
        r#"{{"pauseAsr": {{"engine": "wyoming", "host": "127.0.0.1", "port": {}, "model": "", "language": "en"}}}}"#,
        server.address().port()
    );
    let fixture = Fixture::new(&[configured(KITCHEN, config, r#"["microphones"]"#)]).await;
    let plane = fixture.start().await;
    let speaker = Speaker::connect(plane.device_address(), KITCHEN, &[]).await;
    fixture.ha.hears("Ophelia, what is the time?");
    fixture.ha.answers(IntentPlan::answer("It is noon."));

    let candidate = speaker.wake("c1", false);
    speaker.say(words(&["kitchen", "light", "time"]));
    speaker.say(silence(3_000));
    assert_eq!(speaker.candidate_ack(&candidate).await, "accepted");
    fixture.wait_rows(KITCHEN, 1).await;

    // The server heard the utterance so far, at +20 dB, in the configured language.
    let heard = server.heard();
    assert!(!heard.is_empty());
    assert_eq!(heard[0].language.as_deref(), Some("en"));
    assert_eq!(heard[0].model, None);
    assert_eq!(heard[0].format, Some((16_000, 2, 1)));
    assert!(
        heard[0].pcm.contains(&i16::MAX),
        "the evidence gain saturates loud words"
    );
    // HA heard its own transcript of the audio.
    assert_eq!(intent_texts(&fixture.ha), ["what is the time?"]);

    let conn = rusqlite::Connection::open(fixture.db()).unwrap();
    let (policy, trace): (String, String) = conn
        .query_row(
            "SELECT policy_hash, decision_trace FROM turns WHERE device_id = ?1",
            [KITCHEN],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(policy, "post_afe_10+wyoming:default");
    let trace: Value = serde_json::from_str(&trace).unwrap();
    let u = &trace["utterance"];
    assert_eq!(u["stable_prefix"], "what is the time", "{u}");
    assert_eq!(u["heard"], "Ophelia, what is the time?");
    let pause = &u["pauses"][0];
    assert_eq!(
        pause["server"]["text"], "Ophelia, what is the time?",
        "{pause}"
    );
    assert_eq!(pause["server"]["judged"], "what is the time");
    assert!(pause["server"]["ms"].is_u64(), "{pause}");
    let commit = u["endpoint"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["event"] == "commit")
        .unwrap();
    assert_eq!(commit["source"], "server", "{commit}");
    shut_down(plane).await;
}
