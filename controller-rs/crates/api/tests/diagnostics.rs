//! The diagnostic and speech routes over real HTTP: hourly activity, speech assets, the
//! controller release, the Wyoming check (against `echomuse_wyoming`'s simulated server),
//! transcript correction's tools, the support bundle, cropped clip archives with the
//! `clip_windows` count, and the refused wake-clip deletion.

mod common;

use std::{
    collections::HashMap,
    io::Cursor,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use common::{Options, PASSWORD, START, TestServer};
use echomuse_api::{
    BoxFuture, ClipKind, ClipRef, ClipScoring, Diagnostics, LiveDevice, LiveSession, LocalRoute,
    ServiceError, SoundedWord, TextFixError, TextFixService, TextFixTrial, TextFixTrialRequest,
    UnknownWord,
};
use echomuse_core::types::DeviceId;
use echomuse_protocol::{Capabilities, Platform, Sha256};
use echomuse_wyoming::sim::{Script, SimServer};
use serde_json::{Value, json};

const KITCHEN: &str = "G0SYNTHETIC00001";
const RUNTIME_SHA: &str = "174233cf0000000000000000000000000000000000000000000000000000beef";

// ─── Fakes ──────────────────────────────────────────────────────────────────────────

/// Transcript correction that answers from a script and keeps what it was asked.
#[derive(Default)]
struct FakeTextFix {
    ready: Mutex<bool>,
    asked: Mutex<Vec<TextFixTrialRequest>>,
    words_for: Mutex<Vec<Option<DeviceId>>>,
}

impl TextFixService for FakeTextFix {
    fn trial(
        &self,
        request: TextFixTrialRequest,
    ) -> BoxFuture<'_, Result<TextFixTrial, TextFixError>> {
        Box::pin(async move {
            if !*self.ready.lock().unwrap() {
                return Err(TextFixError::NotReady("tables are being built".to_owned()));
            }
            self.asked.lock().unwrap().push(request.clone());
            Ok(TextFixTrial {
                text: request.text.clone(),
                mode: "shadow",
                route: Some(LocalRoute::Alarm),
                record: Some(
                    json!({"reason": "applied", "original": request.text, "sent": "remove alarm", "applied": true}),
                ),
                heard_words: vec![SoundedWord {
                    word: "rmv".to_owned(),
                    phonemes: vec!["AA R EH M V IY".to_owned()],
                    source: Some("letters"),
                }],
                proposed_words: None,
                satellite: request.device.as_ref().map(|_| true),
                language: Some("en".to_owned()),
                wake_phrase: request.wake.then(|| "ophelia".to_owned()),
            })
        })
    }

    fn unknown_words(
        &self,
        device: Option<DeviceId>,
    ) -> BoxFuture<'_, Result<Vec<UnknownWord>, TextFixError>> {
        Box::pin(async move {
            if !*self.ready.lock().unwrap() {
                return Err(TextFixError::NotReady("tables are being built".to_owned()));
            }
            self.words_for.lock().unwrap().push(device);
            Ok(vec![UnknownWord {
                word: "youtubetv".to_owned(),
                count: 3,
            }])
        })
    }
}

/// Clip scoring that "scores" each asked clip by storing `windows` in its row, as the
/// controller's scorer stores them.
#[derive(Default)]
struct FakeScoring {
    db: Mutex<Option<PathBuf>>,
    pending: Mutex<Option<u64>>,
    windows: Mutex<HashMap<(ClipKind, i64), Value>>,
    asked: Mutex<Vec<ClipRef>>,
}

impl ClipScoring for FakeScoring {
    fn pending(&self) -> Option<u64> {
        *self.pending.lock().unwrap()
    }

    fn score_now(&self, clips: Vec<ClipRef>) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let path = self.db.lock().unwrap().clone().unwrap();
            let conn = rusqlite::Connection::open(path).unwrap();
            for clip in clips {
                if let Some(windows) = self.windows.lock().unwrap().get(&(clip.kind, clip.id)) {
                    let sql = match clip.kind {
                        ClipKind::WakeClip => "UPDATE turns SET wake_windows = ?1 WHERE id = ?2",
                        ClipKind::NearMiss => "UPDATE near_misses SET windows = ?1 WHERE id = ?2",
                    };
                    conn.execute(sql, rusqlite::params![windows.to_string(), clip.id])
                        .unwrap();
                }
                self.asked.lock().unwrap().push(clip);
            }
            Ok(())
        })
    }
}

struct Fixture {
    server: TestServer,
    admin: String,
    viewer: String,
    text_fix: Arc<FakeTextFix>,
    scoring: Arc<FakeScoring>,
}

async fn fixture() -> Fixture {
    let text_fix = Arc::new(FakeTextFix::default());
    let scoring = Arc::new(FakeScoring::default());
    let server = TestServer::start(Options {
        diagnostics: Some(Diagnostics {
            text_fix: text_fix.clone(),
            clip_scoring: scoring.clone(),
            runtime_asset: Some(Sha256::parse(RUNTIME_SHA).unwrap()),
        }),
        ..Options::default()
    })
    .await;
    *scoring.db.lock().unwrap() = Some(server.db_path.clone());
    server.add_device(KITCHEN, Some("Kitchen Echo"), true).await;
    server.add_user("admin", "admin");
    server.add_user("viewer", "readonly");
    let admin = server.login("admin", PASSWORD).await;
    let viewer = server.login("viewer", PASSWORD).await;
    Fixture {
        server,
        admin,
        viewer,
        text_fix,
        scoring,
    }
}

fn live(installed: Option<Vec<Sha256>>) -> LiveDevice {
    LiveDevice {
        esphome_port: Some(16001),
        session: Some(LiveSession {
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30)),
            platform: Platform::FireOs6,
            capabilities: Capabilities::from_names(["audio_timeline_v1"]),
            tls: true,
            firmware_version: Some("fw-live".to_owned()),
            os_version: None,
            muted: Some(false),
            volume: None,
            stats: None,
            installed_assets: installed,
        }),
        health: Some(echomuse_api::LiveHealth::default()),
    }
}

/// A 44-byte-header 16 kHz mono WAV whose sample `i` is `i`.
fn wav(samples: usize) -> Vec<u8> {
    let data = (samples * 2) as u32;
    let mut bytes = Vec::new();
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
        bytes.extend_from_slice(&(i as i16).to_le_bytes());
    }
    bytes
}

fn samples_of(wav: &[u8]) -> Vec<i16> {
    wav[44..]
        .chunks(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

async fn get_raw(server: &TestServer, path: &str, token: &str) -> (u16, Vec<u8>) {
    let response = server
        .http
        .get(server.url(path))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    (
        response.status().as_u16(),
        response.bytes().await.unwrap().to_vec(),
    )
}

fn entries(bytes: Vec<u8>) -> HashMap<String, Vec<u8>> {
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

// ─── Hourly activity ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn activity_summarises_the_hourly_rollups_without_inventing_zeros() {
    let f = fixture().await;
    let s = &f.server;
    let hour = START.div_euclid(3_600) * 3_600;
    let conn = s.sqlite();
    conn.execute(
        "INSERT INTO device_metrics (device_id, hour_ts, samples, cpu_sum, cpu_max, \
         rssi_sum, rssi_samples, rssi_min, rtt_sum_ms, rtt_samples, rtt_max_ms, tx_bytes_sum) \
         VALUES (?1, ?2, 4, 40.0, 20.0, -220.0, 4, -60.0, 60, 3, 30, 1000)",
        rusqlite::params![KITCHEN, hour],
    )
    .unwrap();
    // An hour with a row but no report: averages stay unknown.
    conn.execute(
        "INSERT INTO device_metrics (device_id, hour_ts) VALUES (?1, ?2)",
        rusqlite::params![KITCHEN, hour - 3_600],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO wake_counters (device_id, hour_ts, near_misses, near_miss_max, dev_hops, \
         dev_crossings, dev_max_score, dev_max_infer_ms) VALUES (?1, ?2, 2, 0.4, 180, 1, 0.9, 95)",
        rusqlite::params![KITCHEN, hour],
    )
    .unwrap();
    // Outside one day: not shown.
    conn.execute(
        "INSERT INTO device_metrics (device_id, hour_ts, samples) VALUES (?1, ?2, 1)",
        rusqlite::params![KITCHEN, hour - 30 * 3_600],
    )
    .unwrap();

    let (status, body) = s
        .get(
            &format!("/api/devices/{KITCHEN}/activity?days=1"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["days"], 1);
    assert_eq!(body["since"], hour - 23 * 3_600);
    let hours = body["hours"].as_array().unwrap();
    assert_eq!(hours.len(), 2);
    let quiet = &hours[0];
    assert_eq!(quiet["hour_ts"], hour - 3_600);
    assert_eq!(quiet["metrics"]["reports"], 0);
    assert_eq!(quiet["metrics"]["cpu_avg"], Value::Null);
    assert_eq!(quiet["metrics"]["rssi_avg_dbm"], Value::Null);
    assert_eq!(quiet["wake"], Value::Null);
    let busy = &hours[1];
    assert_eq!(busy["metrics"]["cpu_avg"], 10.0);
    assert_eq!(busy["metrics"]["rssi_avg_dbm"], -55.0);
    assert_eq!(busy["metrics"]["rtt_avg_ms"], 20.0);
    assert_eq!(busy["wake"]["near_misses"], 2);
    assert_eq!(busy["wake"]["max_score"], 0.9);
    assert_eq!(body["totals"]["metrics"]["reports"], 4);
    assert_eq!(body["totals"]["metrics"]["tx_bytes"], 1000);
    assert_eq!(body["totals"]["wake"]["hops"], 180);

    let (status, body) = s
        .get(&format!("/api/devices/{KITCHEN}/activity"), Some(&f.viewer))
        .await;
    assert_eq!((status, body["days"].clone()), (200, json!(7)));
    assert_eq!(body["hours"].as_array().unwrap().len(), 3);
    for bad in ["days=0", "days=91", "days=x"] {
        let (status, _) = s
            .get(
                &format!("/api/devices/{KITCHEN}/activity?{bad}"),
                Some(&f.viewer),
            )
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, _) = s
        .get("/api/devices/G0NOSUCHDEVICE01/activity", Some(&f.viewer))
        .await;
    assert_eq!(status, 404);
    let (status, _) = s
        .get(&format!("/api/devices/{KITCHEN}/activity"), None)
        .await;
    assert_eq!(status, 401);
}

// ─── Speech assets ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn speech_assets_say_which_of_the_ready_assets_the_device_holds() {
    let f = fixture().await;
    let s = &f.server;
    let path = format!("/api/devices/{KITCHEN}/speech_assets");
    let (status, body) = s.get(&path, Some(&f.viewer)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["connected"], false);
    assert_eq!(body["error"], Value::Null);
    let assets = body["assets"].as_array().unwrap();
    let kinds: Vec<&str> = assets.iter().map(|a| a["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["runtime", "graph", "sidecar"]);
    assert_eq!(assets[0]["sha256"], RUNTIME_SHA);
    assert!(assets.iter().all(|a| a["installed"].is_null()));
    let graph = assets[1]["sha256"].as_str().unwrap().to_owned();
    assert_eq!(body["wake_model"]["graph_sha256"], graph);

    // Connected, with the runtime and the graph installed but not the sidecar.
    s.runtimes.set_live(
        KITCHEN,
        live(Some(vec![
            Sha256::parse(RUNTIME_SHA).unwrap(),
            Sha256::parse(&graph).unwrap(),
        ])),
    );
    let (_, body) = s.get(&path, Some(&f.viewer)).await;
    assert_eq!(body["connected"], true);
    let installed: Vec<Value> = body["assets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["installed"].clone())
        .collect();
    assert_eq!(installed, [json!(true), json!(true), json!(false)]);

    // A hello without an asset list leaves it unknown.
    s.runtimes.set_live(KITCHEN, live(None));
    let (_, body) = s.get(&path, Some(&f.viewer)).await;
    assert_eq!(body["connected"], true);
    assert!(body["assets"][0]["installed"].is_null());

    let (status, _) = s
        .get(
            "/api/devices/G0NOSUCHDEVICE01/speech_assets",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
}

// ─── Controller release ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_controller_release_is_the_running_version_with_no_check_made() {
    let f = fixture().await;
    let (status, body) = f
        .server
        .get("/api/releases/controller", Some(&f.viewer))
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"available": false, "version": null, "current": "v0.0.0-synthetic",
               "notes": null, "release_url": null, "checked": false})
    );
    let (status, _) = f.server.get("/api/releases/controller", None).await;
    assert_eq!(status, 401);
}

// ─── Wyoming ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_wyoming_check_lists_what_the_server_offers() {
    let f = fixture().await;
    let s = &f.server;
    let wyoming = SimServer::start(Script::whisper(&["tiny-int8", "base-int8"], &[]))
        .await
        .unwrap();
    let port = wyoming.address().port();
    let path = format!("/api/speech/wyoming?host=127.0.0.1&port={port}");
    let (status, body) = s.get(&path, Some(&f.admin)).await;
    assert_eq!(status, 200, "{body}");
    // PauseAsrControls reads programs[].name, .version and .models[].name.
    assert_eq!(body["programs"][0]["name"], "faster-whisper");
    assert_eq!(body["programs"][0]["version"], "2.5.0");
    let models: Vec<&str> = body["programs"][0]["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["tiny-int8", "base-int8"]);

    // Admin only: the controller connects where it is told.
    let (status, _) = s.get(&path, Some(&f.viewer)).await;
    assert_eq!(status, 403);
    // Nothing listening: 502 with the reason the dashboard shows.
    drop(wyoming);
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_port = closed.local_addr().unwrap().port();
    drop(closed);
    let (status, body) = s
        .get(
            &format!("/api/speech/wyoming?host=127.0.0.1&port={closed_port}"),
            Some(&f.admin),
        )
        .await;
    assert_eq!(status, 502, "{body}");
    assert!(
        body["error"].as_str().unwrap().contains("connect"),
        "{body}"
    );
    for bad in [
        "host=&port=10300",
        "host=a%20b&port=10300",
        "host=x&port=0",
        "host=x&port=70000",
    ] {
        let (status, _) = s
            .get(&format!("/api/speech/wyoming?{bad}"), Some(&f.admin))
            .await;
        assert_eq!(status, 400, "{bad}");
    }
}

// ─── Transcript correction ──────────────────────────────────────────────────────────

#[tokio::test]
async fn text_fix_tools_pass_requests_through_and_say_when_not_ready() {
    let f = fixture().await;
    let s = &f.server;
    let trial = json!({"text": "  RMV alarm ", "wake": true, "device_id": KITCHEN});
    let (status, body) = s
        .post("/api/text_fix/trial", trial.clone(), Some(&f.viewer))
        .await;
    // The dashboard retries on not_ready.
    assert_eq!(
        (status, &body["code"]),
        (503, &json!("not_ready")),
        "{body}"
    );
    let (status, body) = s.get("/api/text_fix/unknown_words", Some(&f.viewer)).await;
    assert_eq!((status, &body["code"]), (503, &json!("not_ready")));

    *f.text_fix.ready.lock().unwrap() = true;
    let (status, body) = s.post("/api/text_fix/trial", trial, Some(&f.viewer)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["text"], "RMV alarm");
    assert_eq!(body["route"], "alarm");
    assert_eq!(body["record"]["reason"], "applied");
    assert_eq!(body["heard_words"][0]["source"], "letters");
    assert_eq!(body["satellite"], true);
    assert_eq!(body["wake_phrase"], "ophelia");
    assert_eq!(body["mode"], "shadow");
    let asked = f.text_fix.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [TextFixTrialRequest {
            text: "RMV alarm".to_owned(),
            wake: true,
            device: Some(DeviceId(KITCHEN.to_owned())),
        }]
    );

    let (status, body) = s
        .get(
            &format!("/api/text_fix/unknown_words?device_id={KITCHEN}"),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"words": [{"word": "youtubetv", "count": 3}]}));
    let (_, _) = s.get("/api/text_fix/unknown_words", Some(&f.viewer)).await;
    assert_eq!(
        *f.text_fix.words_for.lock().unwrap(),
        [Some(DeviceId(KITCHEN.to_owned())), None]
    );

    for bad in [
        json!({"text": "   ", "wake": false}),
        json!({"text": "x".repeat(301), "wake": false}),
        json!({"wake": false}),
    ] {
        let (status, _) = s
            .post("/api/text_fix/trial", bad.clone(), Some(&f.viewer))
            .await;
        assert_eq!(status, 400, "{bad}");
    }
    let (status, _) = s
        .post(
            "/api/text_fix/trial",
            json!({"text": "hi", "device_id": "G0NOSUCHDEVICE01"}),
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
    let (status, _) = s
        .get(
            "/api/text_fix/unknown_words?device_id=G0NOSUCHDEVICE01",
            Some(&f.viewer),
        )
        .await;
    assert_eq!(status, 404);
}

// ─── Support bundle ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_support_bundle_never_carries_a_secret() {
    let f = fixture().await;
    let s = &f.server;
    // Secrets beside the bundle's data: the device and kiosk tokens (add_device), password
    // hashes and sessions (users), and secrets a device may have printed in its log.
    let conn = s.sqlite();
    conn.execute(
        "UPDATE devices SET esphome_noise_psk = 'synthetic-noise-psk' WHERE device_id = ?1",
        [KITCHEN],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO device_logs (device_id, ts, level, source, message) VALUES \
         (?1, ?2, 'info', 'wifi', 'joined with psk=wifi-secret-1 token: tok-secret-2'), \
         (?1, ?3, 'info', 'kiosk', 'GET http://admin:kiosk-secret-3@192.0.2.9:2323/x')",
        rusqlite::params![KITCHEN, START * 1000, START * 1000 + 1],
    )
    .unwrap();
    let hash: String = conn
        .query_row("SELECT password_hash FROM users LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();

    let (status, bytes) = get_raw(s, "/api/support/bundle", &f.admin).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    let text = String::from_utf8(bytes).unwrap();
    let bundle: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(bundle["controller_version"], "v0.0.0-synthetic");
    assert_eq!(bundle["devices"][0]["device_id"], KITCHEN);
    assert_eq!(bundle["devices"][0]["kioskTokenSet"], true);
    assert_eq!(bundle["device_logs"][KITCHEN].as_array().unwrap().len(), 2);
    assert!(bundle["fleet_config"].is_object());
    assert!(bundle["wake_models"]["models"].is_array());
    for secret in [
        "synthetic-link-token",
        "synthetic-kiosk-token",
        "synthetic-noise-psk",
        hash.as_str(),
        "$2b$",
        f.admin.as_str(),
        f.viewer.as_str(),
        "PRIVATE KEY",
        "wifi-secret-1",
        "tok-secret-2",
        "kiosk-secret-3",
        "synthetic-ha-token",
    ] {
        assert!(!text.contains(secret), "{secret} is in the bundle");
    }
    assert!(text.contains("[redacted]"));

    let (status, _) = get_raw(s, "/api/support/bundle", &f.viewer).await;
    assert_eq!(status, 403);
}

// ─── Cropped archives ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn cropped_archives_cut_around_the_scored_windows() {
    let f = fixture().await;
    let s = &f.server;
    let at = 1_793_057_103.0; // 2026-10-26T23:25:03Z
    let conn = s.sqlite();
    conn.execute(
        "INSERT INTO turns (device_id, ts, trigger_type, terminal_reason, policy_hash, \
         total_ms, wake_file, wake_score) VALUES (?1, ?2, 'wake', 'completed', 'p', 2400, 'x.wav', 0.95)",
        rusqlite::params![KITCHEN, at],
    )
    .unwrap();
    let turn = conn.last_insert_rowid();
    let clip = wav(32_000);
    let dir = s.dir.path().join(format!("wakes/{KITCHEN}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{turn}.wav")), &clip).unwrap();
    *f.scoring.pending.lock().unwrap() = Some(1);

    let (status, body) = s.get("/api/clip_windows", Some(&f.viewer)).await;
    assert_eq!((status, body), (200, json!({"pending": 1})));

    // Not scored yet: the archive asks for it, and the scorer's windows are used.
    f.scoring.windows.lock().unwrap().insert(
        (ClipKind::WakeClip, turn),
        json!({"graph_sha256": "4e".repeat(32), "samples": 32000, "window": 22400,
               "scores": [[0, 0.01], [2560, 0.2], [5120, 0.35], [7680, 0.41], [9600, 0.26]]}),
    );
    let (status, bytes) = get_raw(
        s,
        "/api/wakeclips.zip?min=0.00&max=1.00&crop=1.40",
        &f.viewer,
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
    assert_eq!(
        *f.scoring.asked.lock().unwrap(),
        [ClipRef {
            kind: ClipKind::WakeClip,
            device: DeviceId(KITCHEN.to_owned()),
            id: turn,
        }]
    );
    let archive = entries(bytes);
    let crop = &archive["kitchen-echo-2026-10-26232503-wake.wav"];
    let samples = samples_of(crop);
    // 1.4 s around the best window (7,680): exactly that window.
    assert_eq!(samples.len(), 22_400);
    assert_eq!(samples[0], 7_680);

    // Every window at or above 0.30, each named by its start.
    let (status, bytes) = get_raw(
        s,
        "/api/wakeclips.zip?min=0.00&max=1.00&crop=0.50&window_min=0.30",
        &f.viewer,
    )
    .await;
    assert_eq!(status, 200);
    let archive = entries(bytes);
    let mut names: Vec<&String> = archive.keys().collect();
    names.sort();
    assert_eq!(
        names,
        [
            "kitchen-echo-2026-10-26232503-wake-320ms.wav",
            "kitchen-echo-2026-10-26232503-wake-480ms.wav"
        ]
    );
    let first = samples_of(&archive["kitchen-echo-2026-10-26232503-wake-320ms.wav"]);
    assert_eq!(first.len(), 8_000);
    assert_eq!(first[0], (5_120 + 11_200 - 4_000) as i16);

    // No window that high: the 404 says so.
    let (status, bytes) = get_raw(
        s,
        "/api/wakeclips.zip?min=0.00&max=1.00&crop=0.50&window_min=0.90",
        &f.viewer,
    )
    .await;
    assert_eq!(status, 404);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"].as_str().unwrap().contains("0.90"), "{body}");

    // A near miss the scorer cannot score is left out, and the 404 says none could be.
    conn.execute(
        "INSERT INTO near_misses (device_id, ts, near_miss_uuid, profile, floor, peak, \
         duration_ms, end_reason, recording) VALUES (?1, ?2, 'nm-1', 'idle', 0.17, 0.4, 640, 'below', 'saved')",
        rusqlite::params![KITCHEN, at],
    )
    .unwrap();
    let near = conn.last_insert_rowid();
    std::fs::create_dir_all(s.dir.path().join("nearmiss_recordings")).unwrap();
    std::fs::write(
        s.dir
            .path()
            .join(format!("nearmiss_recordings/{KITCHEN}_{near}.wav")),
        wav(16_000),
    )
    .unwrap();
    let (status, bytes) = get_raw(s, "/api/near_misses.zip?min=0&max=1&crop=1.0", &f.viewer).await;
    assert_eq!(status, 404);
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["error"].as_str().unwrap().contains("scored"), "{body}");
    // Whole clips still come whole.
    let (status, bytes) = get_raw(s, "/api/near_misses.zip?min=0&max=1", &f.viewer).await;
    assert_eq!(status, 200);
    assert_eq!(entries(bytes).len(), 1);
}

// ─── Wake clip deletion ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn deleting_wake_clips_is_refused_with_the_reason() {
    let f = fixture().await;
    let s = &f.server;
    let path = format!("/api/devices/{KITCHEN}/wakeclips");
    let (status, body) = s.delete(&path, Some(&f.admin)).await;
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["code"], "not_applicable");
    assert!(body["error"].as_str().unwrap().contains("retention"));
    let (status, _) = s.delete(&path, Some(&f.viewer)).await;
    assert_eq!(status, 403);
    let (status, _) = s
        .delete("/api/devices/G0NOSUCHDEVICE01/wakeclips", Some(&f.admin))
        .await;
    assert_eq!(status, 404);
}
