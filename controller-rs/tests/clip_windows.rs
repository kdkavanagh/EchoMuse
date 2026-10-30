//! Clip-window scoring end to end: the controller's scorer, on its own thread with the
//! tone stand-in models, finds a kept wake clip in a contract-schema database, scores its
//! windows with the wake scorer under the native-call watchdog, and stores them where the
//! Activity routes and the cropped archives read them; a near miss is scored on demand.

use std::{path::Path, sync::Arc, time::Duration};

use echomuse_api::{ClipKind, ClipRef, ClipScoring};
use echomuse_controller::{
    clip_windows::ClipScorer,
    speech_models,
    supervision::{Supervision, SupervisionOptions},
};
use echomuse_core::types::DeviceId;
use echomuse_db::{DatabaseService, OpenedDatabase, ServiceConfig};
use echomuse_records::{ClipWindows, RecordingsLayout, RecordsService, RetentionLimits};
use echomuse_settings::{
    registry::{SeedModel, WakeRegistry},
    service::SettingsService,
};
use echomuse_voice::speech::testing::{ToneModels, silence, tone};

const KITCHEN: &str = "G0SYNTH00A1B2C3D";
const WAIT: Duration = Duration::from_secs(20);

/// A 44-byte-header 16 kHz mono WAV of `pcm`.
fn wav(pcm: &[i16]) -> Vec<u8> {
    echomuse_records::recordings::wav_bytes(pcm)
}

/// Two seconds of quiet, the wake word's tone for 700 ms, two seconds of quiet.
fn wake_clip() -> Vec<i16> {
    let mut pcm = silence(2_000);
    pcm.extend(tone(0, 700, 6_000));
    pcm.extend(silence(2_000));
    pcm
}

fn windows_of(conn: &rusqlite::Connection, sql: &str, id: i64) -> Option<ClipWindows> {
    let text: Option<String> = conn.query_row(sql, [id], |row| row.get(0)).unwrap();
    text.as_deref().and_then(ClipWindows::parse)
}

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    tokio::time::timeout(WAIT, async {
        loop {
            if let Some(value) = check() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kept_clips_are_scored_in_the_background_and_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("echomuse.db");
    let service = DatabaseService::start(
        OpenedDatabase::open(&db_path).unwrap(),
        ServiceConfig::default(),
    )
    .unwrap();
    let db = service.handle();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let registry = WakeRegistry::open_or_seed(
        dir.path().join("oww_models"),
        &SeedModel::deployed(
            root.join("bcresnet_audio.onnx"),
            root.join("bcresnet_audio.json"),
        ),
    )
    .unwrap();
    let settings_service = SettingsService::start(db.clone(), registry).await.unwrap();
    let settings = settings_service.handle();
    let graph = settings.snapshot().fleet().wakeword.wake_model.0.clone();
    let records = RecordsService::new(
        db.clone(),
        RecordingsLayout::beside_database(&db_path),
        RetentionLimits::default(),
    );

    // A kept wake clip, one whose file retention already removed, and a saved near miss.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.execute(
        "INSERT INTO devices (device_id, label, approved) VALUES (?1, 'Kitchen', 1)",
        [KITCHEN],
    )
    .unwrap();
    let turn = |file: bool| {
        conn.execute(
            "INSERT INTO turns (device_id, ts, trigger_type, terminal_reason, policy_hash, \
             total_ms, wake_file, wake_model_sha256) \
             VALUES (?1, 1793057103.0, 'wake', 'completed', 'p', 2400, 'x.wav', ?2)",
            rusqlite::params![KITCHEN, graph.as_str()],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        if file {
            let dir = dir.path().join("wakes").join(KITCHEN);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{id}.wav")), wav(&wake_clip())).unwrap();
        }
        id
    };
    let kept = turn(true);
    let gone = turn(false);

    let supervision = Supervision::start(SupervisionOptions::default()).unwrap();
    let models = Arc::new(ToneModels::new(&["ophelia", "turn", "on"]));
    let scorer = ClipScorer::start(
        db.clone(),
        records,
        settings,
        speech_models::provided(models),
        supervision.watchdog.clone(),
        &supervision.tasks,
    )
    .unwrap();

    // The background job scores the kept clip; the one without a file is not pending.
    let turn_sql = "SELECT wake_windows FROM turns WHERE id = ?1";
    let windows = eventually("the kept clip's windows", || {
        windows_of(&conn, turn_sql, kept)
    })
    .await;
    eventually("no clip left to score", || {
        (scorer.pending() == Some(0)).then_some(())
    })
    .await;
    assert!(windows_of(&conn, turn_sql, gone).is_none());
    assert_eq!(windows.graph_sha256, graph);
    assert_eq!(windows.samples, wake_clip().len() as u64);
    assert_eq!(windows.window, 22_400);
    // 75,200 samples: a window every 160 ms from 0 through 51,200, then one ending at the
    // clip's end.
    let starts: Vec<u64> = windows.scores.iter().map(|(start, _)| *start).collect();
    assert_eq!(starts.len(), 22, "{starts:?}");
    assert_eq!(starts[..2], [0, 2_560]);
    assert_eq!(starts[20..], [51_200, 52_800]);
    // Windows holding enough of the tone hear the wake word; silent windows are below the
    // RMS floor and stored as 0, as the deployed rows store them.
    let (tone_start, tone_end) = (32_000_u64, 32_000 + 11_200);
    for (start, score) in &windows.scores {
        let end = start + 22_400;
        let overlap = end.min(tone_end).saturating_sub((*start).max(tone_start));
        if overlap >= 6_400 {
            assert_eq!(*score, 0.95, "window at {start}");
        } else if overlap == 0 {
            assert_eq!(*score, 0.0, "window at {start}");
        }
    }
    assert!(windows.scores.iter().any(|(_, s)| *s == 0.95));
    assert!(windows.scores.iter().any(|(_, s)| *s == 0.0));

    // A near miss saved since is scored at once when an archive asks for it.
    conn.execute(
        "INSERT INTO near_misses (device_id, ts, near_miss_uuid, profile, floor, peak, \
         duration_ms, end_reason, recording) \
         VALUES (?1, 1793057200.0, 'nm-1', 'idle', 0.17, 0.4, 640, 'below', 'saved')",
        [KITCHEN],
    )
    .unwrap();
    let near = conn.last_insert_rowid();
    std::fs::create_dir_all(dir.path().join("nearmiss_recordings")).unwrap();
    let mut short = silence(200);
    short.extend(tone(0, 700, 6_000));
    std::fs::write(
        dir.path()
            .join(format!("nearmiss_recordings/{KITCHEN}_{near}.wav")),
        wav(&short),
    )
    .unwrap();
    scorer
        .score_now(vec![ClipRef {
            kind: ClipKind::NearMiss,
            device: DeviceId(KITCHEN.to_owned()),
            id: near,
        }])
        .await
        .unwrap();
    let windows = windows_of(&conn, "SELECT windows FROM near_misses WHERE id = ?1", near)
        .expect("scored on demand");
    // Shorter than one window: scored once, with silence before it.
    assert_eq!(windows.samples, short.len() as u64);
    assert_eq!(windows.scores, [(0, 0.95)]);
}
