//! The Activity and diagnostics routes over real HTTP (§3.4, §4.11), on a temporary
//! contract-schema database with synthetic rows and recordings, checked against what the
//! shipped dashboard (controller/static/dashboard.jsx) reads.

mod common;

use std::{collections::HashMap, io::Cursor, path::PathBuf, time::Duration};

use common::{DAY, Options, PASSWORD, START, TestServer};
use futures_util::StreamExt;
use rusqlite::params;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const KITCHEN: &str = "G0SYNTHETIC00001";
const HALLWAY: &str = "G0SYNTHETIC00002";
const GRAPH: &str = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f";
const WAIT: Duration = Duration::from_secs(10);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Fleet {
    server: TestServer,
    admin: String,
    viewer: String,
}

/// Two approved devices, an administrator and a read-only user.
async fn fleet() -> Fleet {
    let server = TestServer::start(Options::default()).await;
    server.add_device(KITCHEN, Some("Kitchen Echo"), true).await;
    server.add_device(HALLWAY, Some("Hallway"), true).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    Fleet {
        server,
        admin,
        viewer,
    }
}

/// A synthetic `turns` row: a completed wake turn at `ts`, with `extra` columns set too.
fn add_turn(server: &TestServer, device: &str, ts: f64, extra: &[(&str, Value)]) -> i64 {
    let mut columns: Vec<(String, Value)> = vec![
        ("device_id".into(), json!(device)),
        ("ts".into(), json!(ts)),
        ("trigger_type".into(), json!("wake")),
        ("terminal_reason".into(), json!("completed")),
        ("policy_hash".into(), json!("synthetic-policy")),
        ("total_ms".into(), json!(2_400)),
    ];
    for (name, value) in extra {
        columns.retain(|(c, _)| c != name);
        columns.push(((*name).to_owned(), value.clone()));
    }
    insert(server, "turns", &columns)
}

/// A synthetic `near_misses` row at `ts`, with `extra` columns set too.
fn add_near_miss(server: &TestServer, device: &str, ts: f64, extra: &[(&str, Value)]) -> i64 {
    let mut columns: Vec<(String, Value)> = vec![
        ("device_id".into(), json!(device)),
        ("ts".into(), json!(ts)),
        ("near_miss_uuid".into(), json!(format!("nm-{device}-{ts}"))),
        ("profile".into(), json!("idle")),
        ("floor".into(), json!(0.17)),
        ("peak".into(), json!(0.42)),
        ("wake_threshold".into(), json!(0.9)),
        ("wake_model_sha256".into(), json!(GRAPH)),
        ("duration_ms".into(), json!(640)),
        ("end_reason".into(), json!("below")),
        ("recording".into(), json!("off")),
    ];
    for (name, value) in extra {
        columns.retain(|(c, _)| c != name);
        columns.push(((*name).to_owned(), value.clone()));
    }
    insert(server, "near_misses", &columns)
}

fn insert(server: &TestServer, table: &str, columns: &[(String, Value)]) -> i64 {
    let names: Vec<&str> = columns.iter().map(|(c, _)| c.as_str()).collect();
    let marks: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();
    let values: Vec<rusqlite::types::Value> = columns
        .iter()
        .map(|(_, v)| match v {
            Value::Null => rusqlite::types::Value::Null,
            Value::Bool(b) => rusqlite::types::Value::Integer(i64::from(*b)),
            Value::Number(n) if n.is_i64() => rusqlite::types::Value::Integer(n.as_i64().unwrap()),
            Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
            Value::String(s) => rusqlite::types::Value::Text(s.clone()),
            other => rusqlite::types::Value::Text(other.to_string()),
        })
        .collect();
    let conn = server.sqlite();
    conn.execute(
        &format!(
            "INSERT INTO {table} ({}) VALUES ({})",
            names.join(", "),
            marks.join(", ")
        ),
        rusqlite::params_from_iter(values),
    )
    .unwrap();
    conn.last_insert_rowid()
}

/// A 44-byte-header WAV of `samples` 16 kHz mono samples, as the recordings are stored.
fn wav(samples: usize, seed: i16) -> Vec<u8> {
    let data = (samples * 2) as u32;
    let mut bytes = Vec::with_capacity(44 + data as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&16_000_u32.to_le_bytes());
    bytes.extend_from_slice(&32_000_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data.to_le_bytes());
    for i in 0..samples {
        bytes.extend_from_slice(&(seed.wrapping_add(i as i16)).to_le_bytes());
    }
    bytes
}

/// Writes a recording beside the test database.
fn put_file(server: &TestServer, relative: &str, bytes: &[u8]) -> PathBuf {
    let path = server.dir.path().join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    path
}

async fn get_bytes(server: &TestServer, path: &str, token: &str) -> (u16, Option<String>, Vec<u8>) {
    let response = server
        .http
        .get(server.url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let kind = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned());
    (status, kind, response.bytes().await.unwrap().to_vec())
}

async fn connect(server: &TestServer, token: &str) -> Socket {
    let (socket, _) = connect_async(server.ws_url(&format!("/api/events?token={token}")))
        .await
        .unwrap();
    socket
}

async fn next_json(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a message in time")
            .expect("the socket is open")
            .expect("no socket error");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

fn ids(list: &Value, key: &str) -> Vec<i64> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|row| row[key].as_i64().unwrap())
        .collect()
}

#[tokio::test]
async fn turn_lists_are_the_newest_rows_oldest_first_in_the_dashboard_shape() {
    let f = fleet().await;
    let s = &f.server;
    let t0 = START as f64;
    let a = add_turn(s, KITCHEN, t0 - 300.0, &[]);
    let b = add_turn(s, HALLWAY, t0 - 200.0, &[]);
    let c = add_turn(
        s,
        KITCHEN,
        t0 - 100.5,
        &[
            ("wake_model", json!("ophelia")),
            ("wake_score", json!(0.97)),
            ("wake_threshold", json!(0.9)),
            ("wake_model_sha256", json!(GRAPH)),
            ("wake_attribution", json!("idle")),
            ("outcome", json!("ha")),
            ("intent_local", json!(1)),
            ("stt_raw", json!("Ophelia, turn on the light.")),
            ("stt_text", json!("turn on the light.")),
            (
                "stt_fix",
                json!(r#"{"mode":"shadow","reason":"original_matched","applied":false}"#),
            ),
            (
                "afe_evidence",
                json!(r#"{"support_start":1600,"playback_onset":null}"#),
            ),
            (
                "wake_windows",
                json!(format!(
                    r#"{{"graph_sha256":"{GRAPH}","samples":42560,"window":22400,"scores":[[0,0.01],[2560,0.98]]}}"#
                )),
            ),
            ("decision_trace", json!(r#"{"turn_id":"x"}"#)),
            ("afe_series", json!(r#"{"start":0,"period":1280}"#)),
            ("response_latency_ms", json!(1_850)),
            ("wake_file", json!("3.wav")),
        ],
    );
    // A row from before the post-AFE cutover: legacy outcome, no terminal reason.
    let d = add_turn(
        s,
        HALLWAY,
        t0 - 50.0,
        &[
            ("terminal_reason", Value::Null),
            ("outcome", json!("ok")),
            ("trigger_type", json!("wakeword(0.102)")),
            ("stt_ms", json!(-1)),
        ],
    );

    // The fleet list (dashboard.jsx ~3114: GET /api/turns?limit=…): every device, the
    // newest `limit`, oldest first (TurnObservability: "the oldest first").
    let (status, all) = s.get("/api/turns?limit=200", Some(&f.viewer)).await;
    assert_eq!(status, 200, "{all}");
    assert_eq!(ids(&all, "turn_id"), [a, b, c, d]);
    let (_, newest) = s.get("/api/turns?limit=2", Some(&f.viewer)).await;
    assert_eq!(ids(&newest, "turn_id"), [c, d]);
    let (_, since) = s
        .get(&format!("/api/turns?since={}", t0 - 200.0), Some(&f.viewer))
        .await;
    assert_eq!(ids(&since, "turn_id"), [b, c, d]);
    // One device's (dashboard.jsx ~4507), each row naming its device_id.
    let (status, kitchen) = s
        .get(
            &format!("/api/devices/{KITCHEN}/turns?limit=50"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(ids(&kitchen, "turn_id"), [a, c]);
    assert!(
        kitchen
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["device_id"] == KITCHEN)
    );

    // Every key TurnDetail and TurnObservability read, with JSON columns parsed.
    let turn = &kitchen[1];
    for key in [
        "afe_evidence",
        "asr_text",
        "audio_file",
        "audio_ms",
        "commit_id",
        "commit_route",
        "continuation",
        "conversation_id",
        "device_id",
        "endpoint_class",
        "endpoint_ms",
        "first_audio_ms",
        "intent_local",
        "intent_ms",
        "outcome",
        "playback_ms",
        "playback_reason",
        "policy_hash",
        "reference_coverage",
        "reply_to",
        "response_latency_ms",
        "response_text",
        "response_type",
        "stt_fix",
        "stt_ms",
        "stt_raw",
        "stt_text",
        "terminal_reason",
        "total_ms",
        "trigger",
        "ts",
        "tts_url_ms",
        "turn_id",
        "turn_uuid",
        "wake_attribution",
        "wake_file",
        "wake_model",
        "wake_model_sha256",
        "wake_score",
        "wake_threshold",
        "wake_windows",
    ] {
        assert!(turn.get(key).is_some(), "{key} missing from {turn}");
    }
    assert_eq!(turn["ts"], json!(t0 - 100.5));
    assert_eq!(turn["trigger"], "wake");
    assert_eq!(turn["intent_local"], true);
    assert_eq!(turn["response_latency_ms"], 1_850);
    // NativeAfeEvidence reads evidence.playback_onset; parseTextFix takes an object;
    // ClipWindowsView reads windows.samples, .window and .scores.
    assert_eq!(turn["afe_evidence"]["support_start"], 1600);
    assert_eq!(turn["stt_fix"]["reason"], "original_matched");
    assert_eq!(
        turn["wake_windows"]["scores"],
        json!([[0, 0.01], [2560, 0.98]])
    );
    // Unknown stays null; the large documents are not in the list.
    assert_eq!(turn["audio_file"], Value::Null);
    assert_eq!(turn["reply_to"], Value::Null);
    assert!(turn.get("decision_trace").is_none() && turn.get("afe_series").is_none());
    // A legacy row keeps its stored words and sentinels (isLegacyTurn, `measured`).
    let legacy = &all[3];
    assert_eq!(legacy["terminal_reason"], Value::Null);
    assert_eq!(legacy["outcome"], "ok");
    assert_eq!(legacy["trigger"], "wakeword(0.102)");
    assert_eq!(legacy["stt_ms"], -1);

    // Limits, roles and unknown devices.
    for bad in ["limit=0", "limit=1001", "limit=x", "since=x"] {
        let (status, body) = s.get(&format!("/api/turns?{bad}"), Some(&f.viewer)).await;
        assert_eq!(status, 400, "{bad}: {body}");
        assert_eq!(body["code"], "bad_request");
    }
    let (_, most) = s.get("/api/turns?limit=1000", Some(&f.viewer)).await;
    assert_eq!(most.as_array().unwrap().len(), 4);
    let (status, _) = s.get("/api/turns", None).await;
    assert_eq!(status, 401);
    let (status, body) = s
        .get("/api/devices/G0NOSUCHDEVICE01/turns", Some(&f.viewer))
        .await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")));
    // Without a limit, the documented default of 50.
    for i in 0..60 {
        add_turn(s, KITCHEN, t0 + f64::from(i), &[]);
    }
    let (_, default) = s.get("/api/turns", Some(&f.viewer)).await;
    assert_eq!(default.as_array().unwrap().len(), 50);
    assert_eq!(default[49]["ts"], json!(t0 + 59.0));
}

#[tokio::test]
async fn near_miss_lists_show_a_pruned_recording_and_parsed_windows() {
    let f = fleet().await;
    let s = &f.server;
    let t0 = START as f64;
    let kept = add_near_miss(
        s,
        KITCHEN,
        t0 - 30.0,
        &[
            ("recording", json!("saved")),
            ("audio_ms", json!(1_240)),
            ("verdict", json!("missed_wake")),
            (
                "windows",
                json!(format!(
                    r#"{{"graph_sha256":"{GRAPH}","samples":19840,"window":22400,"scores":[]}}"#
                )),
            ),
        ],
    );
    put_file(
        s,
        &format!("nearmiss_recordings/{KITCHEN}_{kept}.wav"),
        &wav(100, 1),
    );
    let pruned = add_near_miss(
        s,
        KITCHEN,
        t0 - 20.0,
        &[
            ("recording", json!("saved")),
            ("windows", json!("not json")),
        ],
    );
    let off = add_near_miss(s, HALLWAY, t0 - 10.0, &[]);

    // dashboard.jsx ~3115 and ~4510.
    let (status, all) = s.get("/api/near_misses?limit=200", Some(&f.viewer)).await;
    assert_eq!(status, 200, "{all}");
    assert_eq!(ids(&all, "id"), [kept, pruned, off]);
    let (_, kitchen) = s
        .get(
            &format!("/api/devices/{KITCHEN}/near_misses?limit=50"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(ids(&kitchen, "id"), [kept, pruned]);

    let first = &kitchen[0];
    for key in [
        "audio_ms",
        "device_id",
        "duration_ms",
        "end_reason",
        "floor",
        "id",
        "near_miss_uuid",
        "peak",
        "profile",
        "recording",
        "ts",
        "verdict",
        "wake_model_sha256",
        "wake_threshold",
        "windows",
    ] {
        assert!(first.get(key).is_some(), "{key} missing from {first}");
    }
    assert_eq!(first["recording"], "saved");
    assert_eq!(first["verdict"], "missed_wake");
    assert_eq!(first["windows"]["samples"], 19_840);
    // NEAR_MISS_RECORDING.PRUNED is derived at read time: saved, but the WAV is gone.
    assert_eq!(kitchen[1]["recording"], "pruned");
    // A malformed windows column is not handed to ClipWindowsView.
    assert_eq!(kitchen[1]["windows"], Value::Null);
    assert_eq!(all[2]["recording"], "off");
    assert_eq!(all[2]["verdict"], Value::Null);
}

#[tokio::test]
async fn marking_a_near_miss_needs_an_administrator_and_publishes_turn_complete() {
    let f = fleet().await;
    let s = &f.server;
    let saved = add_near_miss(s, KITCHEN, START as f64, &[("recording", json!("saved"))]);
    put_file(
        s,
        &format!("nearmiss_recordings/{KITCHEN}_{saved}.wav"),
        &wav(10, 0),
    );
    let unrecorded = add_near_miss(s, KITCHEN, START as f64 + 1.0, &[]);
    let path = format!("/api/devices/{KITCHEN}/near_misses/{saved}");
    let verdict = |id: i64| -> Option<String> {
        s.sqlite()
            .query_row("SELECT verdict FROM near_misses WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap()
    };

    // TurnObservability's `markGapOf`: only an administrator marks.
    let (status, body) = s
        .patch(&path, json!({"verdict": "missed_wake"}), Some(&f.viewer))
        .await;
    assert_eq!((status, body["code"].as_str()), (403, Some("forbidden")));
    assert_eq!(verdict(saved), None);

    let mut socket = connect(s, &f.viewer).await;
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");
    // dashboard.jsx ~2710: API.patch(…/near_misses/{id}, { verdict }).
    let (status, body) = s
        .patch(&path, json!({"verdict": "missed_wake"}), Some(&f.admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"id": saved, "device_id": KITCHEN, "verdict": "missed_wake"})
    );
    assert_eq!(verdict(saved).as_deref(), Some("missed_wake"));
    // The event every Activity view re-reads its lists on (dashboard.jsx ~3130).
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "turn_complete", "device_id": KITCHEN})
    );
    let (_, listed) = s
        .get(
            &format!("/api/devices/{KITCHEN}/near_misses"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(listed[0]["verdict"], "missed_wake");

    // Clearing (onMark(null)) is allowed.
    let (status, body) = s
        .patch(&path, json!({"verdict": null}), Some(&f.admin))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(verdict(saved), None);

    // No recording to judge it by, another device's row, an unknown row, a bad word.
    let (status, body) = s
        .patch(
            &format!("/api/devices/{KITCHEN}/near_misses/{unrecorded}"),
            json!({"verdict": "correctly_ignored"}),
            Some(&f.admin),
        )
        .await;
    assert_eq!((status, body["code"].as_str()), (409, Some("conflict")));
    let (status, _) = s
        .patch(
            &format!("/api/devices/{HALLWAY}/near_misses/{saved}"),
            json!({"verdict": "missed_wake"}),
            Some(&f.admin),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = s
        .patch(
            &format!("/api/devices/{KITCHEN}/near_misses/99999"),
            json!({"verdict": "missed_wake"}),
            Some(&f.admin),
        )
        .await;
    assert_eq!(status, 404);
    for body in [
        json!({"verdict": "maybe"}),
        json!({}),
        json!({"verdict": 1}),
    ] {
        let (status, _) = s.patch(&path, body.clone(), Some(&f.admin)).await;
        assert_eq!(status, 400, "{body}");
    }
}

#[tokio::test]
async fn traces_and_afe_series_are_the_stored_objects_or_404() {
    let f = fleet().await;
    let s = &f.server;
    // The AFE series as real rows store it: the AfeSeriesJson the dashboard charts
    // (AfeLanes reads start, period, frames, the per-period arrays, flags, stages and the
    // clip spans).
    let series = json!({
        "start": 1_456_020_480_u64, "period": 1280,
        "support": [1_456_036_480_u64, 1_456_074_240_u64],
        "utterance": [1_456_031_680_u64, 1_456_109_056_u64],
        "frames": [10, 10, 9, 0], "lost": [0, 0, 1, 0], "playback": [0, 0, 3, null],
        "erle_max": [null, null, 12, null], "erle_mean": [null, null, 7.67, null],
        "erle_frames": [0, 0, 3, 0], "dtd_max": [0.0, 0.0, 0.25, null],
        "dtd_frames": [0, 0, 1, 0], "rms_max_db": [-76, -78, -47, null],
        "rms_mean_db": [-79, -81, -60, null], "vad_max": [0.0, 0.25, 0.75, null],
        "vad_frames": [0, 2, 9, 0], "volume": [70, 70, 70, null],
        "flags": {"output_clipped": [2]},
        "wake_clip": [1_456_031_680_u64, 1_456_074_240_u64],
        "audio_clip": [1_456_031_680_u64, 1_456_109_056_u64],
        "recording": [1_456_031_232_u64, 1_456_293_120_u64],
        "stages": {"endpoint": [1_456_105_984_u64, 1_456_109_056_u64]},
        "silero_start": 1_456_020_480_u64, "silero": [0.01, 0.9, null],
    });
    let trace = json!({"turn_id": "t", "verification": {"result": "pass", "text": "ophelia"},
                       "utterance": {"pauses": [], "endpoint": []}});
    let full = add_turn(
        s,
        KITCHEN,
        START as f64,
        &[
            ("decision_trace", json!(trace.to_string())),
            ("afe_series", json!(series.to_string())),
        ],
    );
    let bare = add_turn(s, KITCHEN, START as f64 + 1.0, &[]);
    let near = add_near_miss(
        s,
        KITCHEN,
        START as f64,
        &[("afe_series", json!(series.to_string()))],
    );

    // useTurnTrace (dashboard.jsx ~1964) and useAfeSeries (~1591, ~1850, ~2458).
    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/turns/{full}/trace"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, trace);
    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/turns/{full}/afe"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, series);
    // Numbers keep their stored form: a gap stays null, never 0.
    assert_eq!(body["playback"][3], Value::Null);
    assert_eq!(body["frames"].as_array().unwrap().len(), 4);
    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/near_misses/{near}/afe"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, series);

    // A 404 is "no chart" / "no trace" to the dashboard.
    for path in [
        format!("/api/devices/{KITCHEN}/turns/{bare}/trace"),
        format!("/api/devices/{KITCHEN}/turns/{bare}/afe"),
        format!("/api/devices/{HALLWAY}/turns/{full}/trace"),
        format!("/api/devices/{HALLWAY}/near_misses/{near}/afe"),
        format!("/api/devices/{KITCHEN}/turns/99999/afe"),
        format!("/api/devices/{KITCHEN}/turns/abc/trace"),
    ] {
        let (status, body) = s.get(&path, Some(&f.viewer)).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (404, Some("not_found")),
            "{path}"
        );
    }
    let (status, _) = s
        .get(&format!("/api/devices/{KITCHEN}/turns/{full}/trace"), None)
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn recordings_are_served_as_wav_and_404_once_gone() {
    let f = fleet().await;
    let s = &f.server;
    let turn = add_turn(
        s,
        KITCHEN,
        START as f64,
        &[
            ("audio_file", json!(format!("{KITCHEN}_1.wav"))),
            ("wake_file", json!("1.wav")),
        ],
    );
    let utterance = wav(16_000, 3);
    let clip = wav(4_000, 9);
    let whole = wav(48_000, 5);
    put_file(s, &format!("recordings/{KITCHEN}_{turn}.wav"), &utterance);
    put_file(s, &format!("wakes/{KITCHEN}/{turn}.wav"), &clip);
    let turn_file = put_file(s, &format!("turn_recordings/{KITCHEN}_{turn}.wav"), &whole);
    let near = add_near_miss(s, KITCHEN, START as f64, &[("recording", json!("saved"))]);
    let near_wav = wav(800, 2);
    put_file(
        s,
        &format!("nearmiss_recordings/{KITCHEN}_{near}.wav"),
        &near_wav,
    );

    // TURN_CLIP's kinds (dashboard.jsx ~147) and the near miss's (~2572).
    for (kind, expected) in [
        ("audio", &utterance),
        ("wake", &clip),
        ("recording", &whole),
    ] {
        let (status, content, bytes) = get_bytes(
            s,
            &format!("/api/devices/{KITCHEN}/turns/{turn}/{kind}"),
            &f.viewer,
        )
        .await;
        assert_eq!(status, 200, "{kind}");
        assert_eq!(content.as_deref(), Some("audio/wav"));
        assert_eq!(&bytes, expected, "{kind}");
    }
    let (status, _, bytes) = get_bytes(
        s,
        &format!("/api/devices/{KITCHEN}/near_misses/{near}/recording"),
        &f.viewer,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(bytes, near_wav);

    // Retention removed it: 404, which the dashboard marks gone (`markGone`).
    std::fs::remove_file(turn_file).unwrap();
    let (status, _, _) = get_bytes(
        s,
        &format!("/api/devices/{KITCHEN}/turns/{turn}/recording"),
        &f.viewer,
    )
    .await;
    assert_eq!(status, 404);
    // Never saved (no audio_file): 404 even though a file of that name is put there.
    let unsaved = add_turn(s, KITCHEN, START as f64 + 5.0, &[]);
    put_file(
        s,
        &format!("recordings/{KITCHEN}_{unsaved}.wav"),
        &utterance,
    );
    let (status, _, _) = get_bytes(
        s,
        &format!("/api/devices/{KITCHEN}/turns/{unsaved}/audio"),
        &f.viewer,
    )
    .await;
    assert_eq!(status, 404);
    // Another device's row, and an unknown kind.
    for path in [
        format!("/api/devices/{HALLWAY}/turns/{turn}/wake"),
        format!("/api/devices/{HALLWAY}/near_misses/{near}/recording"),
        format!("/api/devices/{KITCHEN}/turns/{turn}/video"),
    ] {
        let (status, _, _) = get_bytes(s, &path, &f.viewer).await;
        assert_eq!(status, 404, "{path}");
    }
    let (status, _) = s
        .get(&format!("/api/devices/{KITCHEN}/turns/{turn}/wake"), None)
        .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn the_wake_clip_archive_holds_every_kept_clip_named_as_its_download() {
    let f = fleet().await;
    let s = &f.server;
    // 2026-10-26T23:25:03Z, twice in one second (a refused candidate and its retry), and
    // one whose file retention has removed.
    let at = 1_793_057_103.25;
    let mut expected = HashMap::new();
    for (n, ts) in [at, at + 0.5].into_iter().enumerate() {
        let id = add_turn(s, KITCHEN, ts, &[("wake_file", json!("x.wav"))]);
        let bytes = wav(1_000 + n, n as i16);
        put_file(s, &format!("wakes/{KITCHEN}/{id}.wav"), &bytes);
        expected.insert(
            if n == 0 {
                "kitchen-echo-2026-10-26232503-wake.wav".to_owned()
            } else {
                "kitchen-echo-2026-10-26232503-wake (1).wav".to_owned()
            },
            bytes,
        );
    }
    add_turn(s, KITCHEN, at + 60.0, &[("wake_file", json!("y.wav"))]);
    // A file without a saved row is not the device's clip.
    let unsaved = add_turn(s, KITCHEN, at + 70.0, &[]);
    put_file(s, &format!("wakes/{KITCHEN}/{unsaved}.wav"), &wav(10, 0));

    // dashboard.jsx ~2606: API.blob(`/api/devices/${device}/wakeclips.zip`).
    let response = s
        .http
        .get(s.url(&format!("/api/devices/{KITCHEN}/wakeclips.zip")))
        .bearer_auth(&f.viewer)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.headers()["content-type"], "application/zip");
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"kitchen-echo-wakeclips.zip\""
    );
    let archive = response.bytes().await.unwrap().to_vec();
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).unwrap();
    assert_eq!(zip.len(), 2);
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert_eq!(expected.get(entry.name()), Some(&bytes), "{}", entry.name());
    }

    // None kept: 404 with the reason the dashboard shows (`zipError`).
    let (status, body) = s
        .get(
            &format!("/api/devices/{HALLWAY}/wakeclips.zip"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
    assert!(body["error"].as_str().unwrap().contains("wake clip"));
    let (status, _) = s
        .get(
            "/api/devices/G0NOSUCHDEVICE01/wakeclips.zip",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = s
        .get(&format!("/api/devices/{KITCHEN}/wakeclips.zip"), None)
        .await;
    assert_eq!(status, 401);
}

fn add_shadow_hour(
    s: &TestServer,
    device: &str,
    hour: i64,
    key: &str,
    counts: [i64; 7],
    lead: &str,
) {
    s.sqlite()
        .execute(
            "INSERT INTO wake_shadow (device_id, hour_ts, rule_key, hops, opens, matched, \
             lead_hist, unmatched, retried, live_only, events_dropped) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                device, hour, key, counts[0], counts[1], counts[2], lead, counts[3], counts[4],
                counts[5], counts[6]
            ],
        )
        .unwrap();
}

#[tokio::test]
async fn the_shadow_table_sums_the_window_and_lists_configured_and_retired_rules() {
    let f = fleet().await;
    let s = &f.server;
    let hour = START / 3_600 * 3_600;
    // The default shadow rules (configuration.md "Shadow rules") include idle:2:mean:0.90.
    // 22,500 hops are an hour at 160 ms; two hours inside the 7 days, one outside.
    add_shadow_hour(
        s,
        KITCHEN,
        hour - 3_600,
        "idle:2:mean:0.90",
        [22_500, 4, 3, 1, 1, 0, 0],
        "[0, 0, 0, 1, 1, 0, 0]",
    );
    add_shadow_hour(
        s,
        KITCHEN,
        hour,
        "idle:2:mean:0.90",
        [22_500, 2, 1, 0, 0, 2, 1],
        "[0, 0, 0, 0, 0, 0, 1]",
    );
    add_shadow_hour(
        s,
        KITCHEN,
        hour - 8 * DAY,
        "idle:2:mean:0.90",
        [22_500, 9, 9, 9, 9, 9, 9],
        "[9, 9, 9, 9, 9, 9, 9]",
    );
    // A rule no longer configured.
    add_shadow_hour(
        s,
        KITCHEN,
        hour,
        "playback:1:mean:0.80",
        [0, 0, 0, 0, 0, 0, 0],
        "[0, 0, 0, 0, 0, 0, 0]",
    );
    let conn = s.sqlite();
    for (ts, kind, peak) in [
        (START as f64 - 100.0, "unmatched", 0.93),
        (START as f64 - 50.0, "retried", 0.95),
        (START as f64 - 9.0 * DAY as f64, "unmatched", 0.99),
    ] {
        conn.execute(
            "INSERT INTO wake_shadow_events (device_id, ts, rule_key, kind, peak_raw, raws) \
             VALUES (?1, ?2, 'idle:2:mean:0.90', ?3, ?4, '[0.91, 0.93, 0.92]')",
            params![KITCHEN, ts, kind, peak],
        )
        .unwrap();
    }

    // dashboard.jsx ~3954: GET …/wake_shadow?days=7, read by WakeShadowTable.
    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/wake_shadow?days=7"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let rules = body["rules"].as_array().unwrap();
    // The five default rules in their order, then the retired one.
    let keys: Vec<&str> = rules.iter().map(|r| r["key"].as_str().unwrap()).collect();
    assert_eq!(
        keys,
        [
            "idle:2:mean:0.90",
            "idle:2:mean:0.95",
            "idle:2:all:0.90",
            "idle:2:all:0.95",
            "idle:1:mean:0.97",
            "playback:1:mean:0.80",
        ]
    );
    let r = &rules[0];
    assert_eq!(r["active"], true);
    assert_eq!(
        r["rule"],
        json!({"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.9})
    );
    assert!((r["hours"].as_f64().unwrap() - 2.0).abs() < 1e-9);
    assert_eq!(r["matched"], 4);
    // Opened earlier: leads +1 (one) and +3 (one); lead −0… are not earlier.
    assert_eq!(r["matched_earlier"], 2);
    assert!((r["mean_lead_ms"].as_f64().unwrap() - 320.0).abs() < 1e-9);
    assert_eq!(r["lead_hist"], json!([0, 0, 0, 1, 1, 0, 1]));
    assert_eq!(r["unmatched"], 1);
    assert!((r["unmatched_per_hour"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    // One-sided 95% Poisson bound of one event over two hours (§13.3).
    assert!((r["unmatched_per_hour_upper95"].as_f64().unwrap() - 4.743_865 / 2.0).abs() < 1e-5);
    assert_eq!(
        (r["retried"].as_i64(), r["live_only"].as_i64()),
        (Some(1), Some(2))
    );
    // A configured rule with no report has measured nothing: no counts, never zeros.
    let unmeasured = &rules[1];
    assert_eq!(unmeasured["hours"], 0.0);
    assert_eq!(unmeasured["unmatched"], Value::Null);
    assert_eq!(unmeasured["unmatched_per_hour"], Value::Null);
    // A retired rule: inactive, no rule object (the dashboard parses the key).
    let retired = &rules[5];
    assert_eq!(
        (retired["active"].as_bool(), retired["rule"].is_null()),
        (Some(false), true)
    );
    assert_eq!(retired["unmatched_per_hour"], Value::Null);

    // The window's events, newest first, with raws parsed (the row's title reads e.raws).
    let events = body["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["kind"], "retried");
    assert_eq!(events[1]["kind"], "unmatched");
    assert_eq!(events[1]["raws"], json!([0.91, 0.93, 0.92]));
    assert_eq!(events[1]["rule_key"], "idle:2:mean:0.90");

    // 1–180 days (configuration.md).
    for bad in ["days=0", "days=181", "days=x"] {
        let (status, _) = s
            .get(
                &format!("/api/devices/{KITCHEN}/wake_shadow?{bad}"),
                Some(&f.viewer),
            )
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (_, wide) = s
        .get(
            &format!("/api/devices/{KITCHEN}/wake_shadow?days=10"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(wide["rules"][0]["matched"], 13);
    assert_eq!(wide["events"].as_array().unwrap().len(), 3);
    let (status, _) = s
        .get("/api/devices/G0NOSUCHDEVICE01/wake_shadow", Some(&f.viewer))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn response_latency_percentiles_cover_only_measured_turns() {
    let f = fleet().await;
    let s = &f.server;
    let now = START as f64;
    // Inside a day: 1,000…4,000 ms; inside a week: 10,000 more; inside 30 days: nothing new.
    for (age, ms) in [
        (60.0, 1_000),
        (120.0, 2_000),
        (180.0, 3_000),
        (240.0, 4_000),
    ] {
        add_turn(s, KITCHEN, now - age, &[("response_latency_ms", json!(ms))]);
    }
    add_turn(
        s,
        KITCHEN,
        now - 3.0 * DAY as f64,
        &[("response_latency_ms", json!(10_000))],
    );
    // Not measured: no reply, a sentinel, too old, another device.
    add_turn(s, KITCHEN, now - 10.0, &[]);
    add_turn(
        s,
        KITCHEN,
        now - 20.0,
        &[("response_latency_ms", json!(-1))],
    );
    add_turn(
        s,
        KITCHEN,
        now - 40.0 * DAY as f64,
        &[("response_latency_ms", json!(99))],
    );
    add_turn(
        s,
        HALLWAY,
        now - 10.0,
        &[("response_latency_ms", json!(500))],
    );

    // dashboard.jsx ~4037, read by ResponseLatency.
    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/response_latency"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let windows = body["windows"].as_array().unwrap();
    let hours: Vec<i64> = windows
        .iter()
        .map(|w| w["hours"].as_i64().unwrap())
        .collect();
    assert_eq!(hours, [24, 168, 720]);
    assert_eq!(windows[0]["turns"], 4);
    assert_eq!(windows[0]["p50"], 2_500.0);
    assert_eq!(windows[0]["p90"], 3_700.0);
    assert_eq!(windows[1]["turns"], 5);
    assert_eq!(windows[1]["p50"], 3_000.0);
    assert_eq!(windows[2]["turns"], 5);

    // A window without a measured turn reads "—", never zero.
    let (_, empty) = s
        .get(
            &format!("/api/devices/{HALLWAY}/response_latency"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(empty["windows"][0]["turns"], 1);
    s.clock.set(START + 2 * DAY);
    let (_, later) = s
        .get(
            &format!("/api/devices/{HALLWAY}/response_latency"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(later["windows"][0]["turns"], 0);
    for p in ["p50", "p90", "p95", "p99"] {
        assert_eq!(later["windows"][0][p], Value::Null, "{p}");
    }
    assert_eq!(later["windows"][1]["p99"], 500.0);
    let (status, _) = s
        .get(
            "/api/devices/G0NOSUCHDEVICE01/response_latency",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn every_fetch_of_the_activity_page_answers() {
    let f = fleet().await;
    let s = &f.server;
    add_turn(s, KITCHEN, START as f64, &[]);
    // FleetActivity's three fetches for every device (base '/api') and for one device
    // (base '/api/devices/{id}'), dashboard.jsx ~3110–3116.
    for base in ["/api".to_owned(), format!("/api/devices/{KITCHEN}")] {
        for list in ["turns", "near_misses"] {
            let (status, body) = s
                .get(&format!("{base}/{list}?limit=200"), Some(&f.viewer))
                .await;
            assert_eq!(status, 200, "{base}/{list}: {body}");
            assert!(body.is_array());
        }
    }
    // `pending` is null: this controller does not score clip windows, so the count is
    // unknown and the page shows no scoring progress (`windows?.pending ?? null`).
    let (status, body) = s.get("/api/clip_windows", Some(&f.viewer)).await;
    assert_eq!((status, body), (200, json!({"pending": null})));
    let (status, _) = s.get("/api/clip_windows", None).await;
    assert_eq!(status, 401);
}

/// The names in an archive and their bytes.
async fn archive_entries(server: &TestServer, path: &str, token: &str) -> HashMap<String, Vec<u8>> {
    let (status, content, bytes) = get_bytes(server, path, token).await;
    assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&bytes));
    assert_eq!(content.as_deref(), Some("application/zip"));
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    (0..zip.len())
        .map(|i| {
            let mut entry = zip.by_index(i).unwrap();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            (entry.name().to_owned(), bytes)
        })
        .collect()
}

#[tokio::test]
async fn the_activity_archives_hold_the_saved_clips_the_filters_match() {
    let f = fleet().await;
    let s = &f.server;
    let at = 1_793_057_103.0; // 2026-10-26T23:25:03Z
    let clip = |device: &str, ts: f64, score: Value, extra: &[(&str, Value)], seed: i16| {
        let mut columns = vec![("wake_file", json!("x.wav")), ("wake_score", score)];
        columns.extend_from_slice(extra);
        let id = add_turn(s, device, ts, &columns);
        let bytes = wav(500, seed);
        put_file(s, &format!("wakes/{device}/{id}.wav"), &bytes);
        bytes
    };
    let confirmed = clip(KITCHEN, at, json!(0.95), &[], 1);
    // Refused (dashboard.jsx REFUSED_WAKE): never a confirmed wake, whatever its score.
    clip(
        KITCHEN,
        at + 1.0,
        json!(0.97),
        &[("terminal_reason", json!("arbitration_lost"))],
        2,
    );
    let hallway = clip(HALLWAY, at + 2.0, json!(0.5), &[], 3);
    let marked = add_near_miss(
        s,
        KITCHEN,
        at,
        &[
            ("recording", json!("saved")),
            ("verdict", json!("missed_wake")),
            ("peak", json!(0.6)),
        ],
    );
    let marked_wav = wav(300, 4);
    put_file(
        s,
        &format!("nearmiss_recordings/{KITCHEN}_{marked}.wav"),
        &marked_wav,
    );
    let unmarked = add_near_miss(s, KITCHEN, at + 5.0, &[("recording", json!("saved"))]);
    let unmarked_wav = wav(300, 5);
    put_file(
        s,
        &format!("nearmiss_recordings/{KITCHEN}_{unmarked}.wav"),
        &unmarked_wav,
    );

    // FleetActivity's `download` (dashboard.jsx ~3172): min, max, the archive's query and
    // `device` for one device.
    let wakes = archive_entries(s, "/api/wakeclips.zip?min=0.90&max=1.00", &f.viewer).await;
    assert_eq!(
        wakes,
        HashMap::from([(
            "kitchen-echo-2026-10-26232503-wake.wav".to_owned(),
            confirmed
        )])
    );
    let one = archive_entries(
        s,
        &format!("/api/wakeclips.zip?min=0.00&max=1.00&device={HALLWAY}"),
        &f.viewer,
    )
    .await;
    assert_eq!(
        one,
        HashMap::from([("hallway-2026-10-26232505-wake.wav".to_owned(), hallway)])
    );
    let missed = archive_entries(
        s,
        "/api/near_misses.zip?min=0.00&max=1.00&verdict=missed_wake",
        &f.viewer,
    )
    .await;
    assert_eq!(
        missed,
        HashMap::from([(
            format!("kitchen-echo-nearmiss{marked}.wav"),
            marked_wav.clone()
        )])
    );
    let every = archive_entries(s, "/api/near_misses.zip?min=0.00&max=1.00", &f.viewer).await;
    assert_eq!(
        every,
        HashMap::from([
            (format!("kitchen-echo-nearmiss{marked}.wav"), marked_wav),
            (format!("kitchen-echo-nearmiss{unmarked}.wav"), unmarked_wav),
        ])
    );

    // Nothing matches: 404 ("no saved … matches"); a crop is refused with its reason.
    let (status, _) = s
        .get(
            "/api/near_misses.zip?min=0.00&max=1.00&verdict=correctly_ignored",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
    // Cropping needs the clip scorer; without one the archive says it cannot score.
    let (status, _) = s
        .get(
            "/api/wakeclips.zip?min=0.00&max=1.00&crop=1.40",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 503);
    let (status, body) = s
        .get(
            "/api/wakeclips.zip?min=0.00&max=1.00&window_min=0.20",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("crop"), "{body}");
    for bad in ["min=2&max=1", "min=0.5&max=0.4", "min=x"] {
        let (status, _) = s
            .get(&format!("/api/wakeclips.zip?{bad}"), Some(&f.viewer))
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, _) = s
        .get(
            "/api/wakeclips.zip?device=G0NOSUCHDEVICE01",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = s.get("/api/near_misses.zip", None).await;
    assert_eq!(status, 401);
}
