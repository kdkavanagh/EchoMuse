//! The settings service against a contract-schema database holding synthetic rows shaped
//! like real ones: Python `json.dumps` documents, a fleet document without
//! `nearMissThreshold`, devices carrying full copies of every key.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use echomuse_core::types::DeviceId;
use echomuse_db::{DatabaseService, OpenedDatabase, ServiceConfig};
use echomuse_protocol::Sha256;
use echomuse_settings::edit::{DeviceEdit, EditError, SettingsPatch};
use echomuse_settings::registry::{
    AddOutcome, ModelThresholds, NewModel, ProbeResult, SeedModel, SpokenForm, WakeRegistry,
};
use echomuse_settings::service::{ModelError, Selector, SettingsHandle, SettingsService};
use echomuse_settings::stored::StoredIssue;
use echomuse_settings::values::*;
use echomuse_settings::{DeviceSettings, Section, SectionSet, SettingKey, SettingValue};
use rusqlite::Connection;
use serde_json::json;

const FOLLOWER: &str = "G0SYNTHETIC00001";
const OVERRIDER: &str = "G0SYNTHETIC00002";

const RULES: &str = concat!(
    r#"[{"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.92}, "#,
    r#"{"profile": "idle", "windows": 1, "combine": "all", "threshold": 0.96}]"#
);

/// A fleet document in the stored shape: every section key but `nearMissThreshold`,
/// plus `startupVolume`, in the order an earlier controller wrote them.
fn fleet_doc() -> String {
    format!(
        concat!(
            r#"{{"wakeSound": true, "startupVolume": 80, "buttonSingleTapEvent": false, "#,
            r#""buttonMultiTapMs": 0, "duckDb": -30, "wakeModel": "{model}", "#,
            r#""wakeArbitrationMs": 650, "wakeOpenRules": [], "wakeShadowRules": {rules}, "#,
            r#""nsAsr": false, "saveUtterances": true, "saveWakeClips": true, "#,
            r#""extendedUtterances": false, "pauseAsr": {{"engine": "wyoming", "host": "stt.example", "#,
            r#""port": 10300, "model": "", "language": "en"}}, "transcriptFix": "shadow", "#,
            r#""transcriptFixTerms": ["Caf\u00e9 Radio"], "transcriptFixPhrases": [], "#,
            r#""transcriptFixPronunciations": {{}}, "timerSound": "chime_soft", "#,
            r#""alarmSound": "chime_soft", "timerRingSeconds": 540, "timerRingGapSeconds": 0.5, "#,
            r#""bleProxyEnabled": false, "screenWake": false, "screenWakeUrl": "http://127.0.0.1:2323", "#,
            r#""screenWakePassword": "", "kioskPort": 2324, "turnViewLinger": 4, "#,
            r#""eqBands": [5, 3, 1, 0, -1, -2, 1, 2], "eqLoudness": false, "ledScene": "standard", "#,
            r##""ledListenColor": "#00b400", "ledThinkColor": "#00c800", "meterAttack": 0.6, "##,
            r#""meterDecay": 0.3, "meterFloor": 0.06, "meterGamma": 2.2, "meterRef": 0.22, "#,
            r#""meterCurve": 0.7}}"#
        ),
        model = DEPLOYED_WAKE_GRAPH_SHA256,
        rules = RULES
    )
}

/// A device copy of the fleet document with its own `nearMissThreshold` and volume.
fn follower_doc() -> String {
    fleet_doc()
        .replacen(r#""startupVolume": 80"#, r#""startupVolume": 101"#, 1)
        .replacen(
            r#""wakeArbitrationMs": 650"#,
            r#""nearMissThreshold": 0.2, "wakeArbitrationMs": 650"#,
            1,
        )
}

/// A device overriding Ring and Timers, with a stale wake-word value and a key this
/// controller does not know.
fn overrider_doc() -> String {
    fleet_doc()
        .replacen(r#""ledScene": "standard""#, r#""ledScene": "pride""#, 1)
        .replacen(
            r#""timerRingSeconds": 540"#,
            r#""timerRingSeconds": 600"#,
            1,
        )
        .replacen(
            r#""wakeArbitrationMs": 650"#,
            r#""wakeArbitrationMs": 0, "legacyKey": 7"#,
            1,
        )
}

fn id(device: &str) -> DeviceId {
    DeviceId(device.to_owned())
}

struct Fixture {
    _temp: tempfile::TempDir,
    path: PathBuf,
    models: PathBuf,
    db: DatabaseService,
    service: SettingsService,
    handle: SettingsHandle,
}

impl Fixture {
    fn raw(&self, sql: &str) -> Option<String> {
        Connection::open(&self.path)
            .unwrap()
            .query_row(sql, [], |row| row.get(0))
            .ok()
    }

    fn fleet_text(&self) -> Option<String> {
        self.raw("SELECT value FROM system_config WHERE key = 'global_device_config'")
    }

    fn device_columns(&self, device: &str) -> (String, String, i64) {
        Connection::open(&self.path)
            .unwrap()
            .query_row(
                "SELECT config, config_sections, use_global_config FROM devices WHERE device_id = ?1",
                [device],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    async fn stop(self) {
        self.service.shutdown().await.unwrap();
        self.db.shutdown(Duration::from_secs(5)).await.unwrap();
    }
}

fn repo_model() -> (PathBuf, PathBuf) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    (
        root.join("bcresnet_audio.onnx"),
        root.join("bcresnet_audio.json"),
    )
}

async fn fixture(with_rows: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("echomuse.db");
    let opened = OpenedDatabase::open(&path).unwrap();
    if with_rows {
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO system_config (key, value) VALUES ('global_device_config', ?1)",
            [fleet_doc()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, label, approved, config, use_global_config, config_sections)
                 VALUES (?1, 'Kitchen', 1, ?2, 1, '[]')",
            [FOLLOWER, &follower_doc()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO devices (device_id, label, approved, config, use_global_config, config_sections)
                 VALUES (?1, 'Office', 1, ?2, 0, '[\"ring\", \"timers\"]')",
            [OVERRIDER, &overrider_doc()],
        )
        .unwrap();
    }
    let db = DatabaseService::start(opened, ServiceConfig::default()).unwrap();
    let models = temp.path().join("oww_models");
    let (graph, sidecar) = repo_model();
    let registry = tokio::task::spawn_blocking({
        let models = models.clone();
        move || WakeRegistry::open_or_seed(models, &SeedModel::deployed(graph, sidecar))
    })
    .await
    .unwrap()
    .unwrap();
    let service = SettingsService::start(db.handle(), registry).await.unwrap();
    let handle = service.handle();
    Fixture {
        _temp: temp,
        path,
        models,
        db,
        service,
        handle,
    }
}

fn deployed() -> Sha256 {
    Sha256::parse(DEPLOYED_WAKE_GRAPH_SHA256).unwrap()
}

fn set(key: SettingKey, json: serde_json::Value) -> SettingValue {
    SettingValue::parse(key, &echomuse_settings::json::Json::from_value(&json)).unwrap()
}

#[tokio::test]
async fn a_fresh_installation_uses_the_defaults() {
    let f = fixture(false).await;
    let snapshot = f.handle.snapshot();
    assert_eq!(**snapshot.fleet(), DeviceSettings::default());
    assert_eq!(
        *f.handle.effective(&id("G0UNKNOWN0000001")),
        DeviceSettings::default()
    );
    assert!(snapshot.fleet_issues().is_empty());
    assert_eq!(f.fleet_text(), None);
    // The first fleet edit creates the row with just what it set.
    f.handle
        .apply_fleet_edit(SettingsPatch::from_value(&json!({"duckDb": -24})).unwrap())
        .await
        .unwrap();
    assert_eq!(f.fleet_text().as_deref(), Some(r#"{"duckDb": -24}"#));
    assert_eq!(
        f.handle.effective(&id(FOLLOWER)).playback.duck_db.get(),
        -24.0
    );
    f.stop().await;
}

#[tokio::test]
async fn fleet_values_with_overridden_sections_and_state_layered_over_them() {
    let f = fixture(true).await;
    let snapshot = f.handle.snapshot();
    let fleet = snapshot.fleet();
    assert_eq!(fleet.playback.duck_db.get(), -30.0);
    // The fleet document has no nearMissThreshold: the default applies.
    assert_eq!(fleet.wakeword.near_miss_threshold.get(), 0.17);
    assert_eq!(fleet.wakeword.wake_shadow_rules.rules().len(), 2);
    assert_eq!(fleet.microphones.pause_asr.host(), "stt.example");
    assert_eq!(
        fleet.transcript.transcript_fix_terms.0.entries()[0].as_str(),
        "Café Radio"
    );

    // The follower takes only its state from its own copy.
    let follower = f.handle.effective(&id(FOLLOWER));
    let mut expected = (**fleet).clone();
    expected.state.startup_volume = StartupVolume::new(101).unwrap();
    assert_eq!(*follower, expected);

    // The overrider takes its Ring and Timers values, not its stale wake-word one.
    let overrider = snapshot.device(&id(OVERRIDER)).unwrap();
    assert_eq!(
        *overrider.overridden(),
        SectionSet::from([Section::Ring, Section::Timers])
    );
    let settings = overrider.settings();
    assert_eq!(settings.ring.led_scene, LedScene::Pride);
    assert_eq!(settings.timers.timer_ring_seconds.get(), 600);
    assert_eq!(settings.wakeword.wake_arbitration_ms.get(), 650);
    assert_eq!(settings.state.startup_volume.get(), 80);
    assert_eq!(
        overrider.issues(),
        [StoredIssue::Unrecognised {
            key: "legacyKey".to_owned()
        }]
    );
    f.stop().await;
}

#[tokio::test]
async fn stored_documents_round_trip_unchanged() {
    let f = fixture(true).await;
    let generation = f.handle.snapshot().generation();
    // Saving the fleet's own values (what the dashboard sends) changes nothing.
    let same = serde_json::to_value(&**f.handle.snapshot().fleet()).unwrap();
    let mut same = same.as_object().unwrap().clone();
    same.remove("nearMissThreshold");
    let outcome = f
        .handle
        .apply_fleet_edit(SettingsPatch::from_value(&serde_json::Value::Object(same)).unwrap())
        .await
        .unwrap();
    assert!(outcome.changed_devices.is_empty());
    assert_eq!(f.handle.snapshot().generation(), generation);
    assert_eq!(f.fleet_text(), Some(fleet_doc()));
    // So does a device save of its own effective config with its sections unchanged.
    let effective = serde_json::to_value(&*f.handle.effective(&id(OVERRIDER))).unwrap();
    let mut body = effective.as_object().unwrap().clone();
    body.insert("config_sections".to_owned(), json!(["ring", "timers"]));
    let outcome = f
        .handle
        .apply_device_edit(
            id(OVERRIDER),
            DeviceEdit::from_value(&serde_json::Value::Object(body)).unwrap(),
        )
        .await
        .unwrap();
    assert!(outcome.changed_devices.is_empty());
    // Nothing was written: not even the fleet-following Wake word values the body
    // carried, which differ from the stored copy (`wakeArbitrationMs` 650, stored 0).
    assert_eq!(
        f.device_columns(OVERRIDER),
        (overrider_doc(), r#"["ring", "timers"]"#.to_owned(), 0)
    );

    // One change rewrites only that key, in place, keeping the unknown key.
    let mut patch = SettingsPatch::new();
    patch.set(set(SettingKey::TimerRingSeconds, json!(900)));
    f.handle
        .apply_device_edit(
            id(OVERRIDER),
            DeviceEdit {
                sections: None,
                values: patch,
            },
        )
        .await
        .unwrap();
    let (config, sections, use_global) = f.device_columns(OVERRIDER);
    assert_eq!(
        config,
        overrider_doc().replacen(
            r#""timerRingSeconds": 600"#,
            r#""timerRingSeconds": 900"#,
            1
        )
    );
    assert_eq!(
        (sections.as_str(), use_global),
        (r#"["ring", "timers"]"#, 0)
    );
    f.stop().await;
}

#[tokio::test]
async fn switching_sections_starts_from_the_fleet_and_back() {
    let f = fixture(true).await;
    // The follower's copy has a stale nearMissThreshold 0.2; switching Wake word to
    // the device starts it from the fleet's value instead.
    let edit = DeviceEdit::from_value(&json!({"config_sections": ["wakeword"]})).unwrap();
    let outcome = f
        .handle
        .apply_device_edit(id(FOLLOWER), edit)
        .await
        .unwrap();
    assert!(outcome.changed_devices.is_empty());
    let (config, sections, use_global) = f.device_columns(FOLLOWER);
    assert!(config.contains(r#""nearMissThreshold": 0.17"#));
    assert_eq!((sections.as_str(), use_global), (r#"["wakeword"]"#, 0));
    // Now its own value takes effect, and needs a new session.ready.
    let edit = DeviceEdit::from_value(&json!({"nearMissThreshold": 0.25})).unwrap();
    let outcome = f
        .handle
        .apply_device_edit(id(FOLLOWER), edit)
        .await
        .unwrap();
    assert_eq!(outcome.changed_devices, [id(FOLLOWER)]);
    assert_eq!(outcome.new_session_needed, [id(FOLLOWER)]);
    // Back to the fleet: the stored copy stays, the fleet's value applies again.
    let edit = DeviceEdit::from_value(&json!({"config_sections": []})).unwrap();
    f.handle
        .apply_device_edit(id(FOLLOWER), edit)
        .await
        .unwrap();
    let (config, sections, use_global) = f.device_columns(FOLLOWER);
    assert!(config.contains(r#""nearMissThreshold": 0.25"#));
    assert_eq!((sections.as_str(), use_global), ("[]", 1));
    assert_eq!(
        f.handle
            .effective(&id(FOLLOWER))
            .wakeword
            .near_miss_threshold
            .get(),
        0.17
    );
    f.stop().await;
}

#[tokio::test]
async fn edits_are_refused_by_key_and_change_nothing() {
    let f = fixture(true).await;
    for (body, expected) in [
        (
            json!({"owwModel": "x"}),
            "owwModel: this setting was removed",
        ),
        (json!({"wakeColour": 1}), "wakeColour: not a setting"),
        (
            json!({"kioskPort": 0}),
            "kioskPort: 0 is outside 1 to 65535",
        ),
        (
            json!({"nearMissThreshold": 0.123}),
            "nearMissThreshold: 0.123 has more than two decimals",
        ),
    ] {
        assert_eq!(
            SettingsPatch::from_value(&body).unwrap_err().to_string(),
            expected
        );
    }
    let generation = f.handle.snapshot().generation();
    let missing = "0".repeat(64);
    let err = f
        .handle
        .apply_fleet_edit(SettingsPatch::from_value(&json!({ "wakeModel": missing })).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::UnknownWakeModel(_)));
    let baseline = json!({"wakeOpenRules": [
        {"profile": "playback", "windows": 3, "combine": "mean", "threshold": 0.65}
    ]});
    let err = f
        .handle
        .apply_fleet_edit(SettingsPatch::from_value(&baseline).unwrap())
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::OpenRuleIsBaseline { .. }));
    assert_eq!(f.handle.snapshot().generation(), generation);
    assert_eq!(f.fleet_text(), Some(fleet_doc()));
    f.stop().await;
}

#[tokio::test]
async fn snapshots_are_published_only_after_the_commit() {
    let f = fixture(true).await;
    let mut follower = f.handle.watch_device(id(FOLLOWER));
    let path = f.path.clone();
    // When the follower's settings change, the database already holds the edit.
    let watcher = tokio::spawn(async move {
        follower.changed().await.unwrap();
        Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT value FROM system_config WHERE key = 'global_device_config'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    });
    let before = f.handle.snapshot().generation();
    let outcome = f
        .handle
        .apply_fleet_edit(SettingsPatch::from_value(&json!({"wakeArbitrationMs": 800})).unwrap())
        .await
        .unwrap();
    let stored = watcher.await.unwrap();
    assert_eq!(outcome.snapshot.generation(), before + 1);
    assert!(stored.contains(r#""wakeArbitrationMs": 800"#));
    // The follower changed; the overrider's Wake word follows the fleet too.
    assert_eq!(outcome.changed_devices, [id(FOLLOWER), id(OVERRIDER)]);
    assert!(outcome.new_session_needed.is_empty());

    // A write that fails publishes nothing.
    Connection::open(&f.path)
        .unwrap()
        .execute("DELETE FROM devices WHERE device_id = ?1", [OVERRIDER])
        .unwrap();
    let generation = f.handle.snapshot().generation();
    let err = f
        .handle
        .apply_device_edit(
            id(OVERRIDER),
            DeviceEdit::from_value(&json!({"timerRingSeconds": 60})).unwrap(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, EditError::UnknownDevice(_)));
    assert_eq!(f.handle.snapshot().generation(), generation);
    // Refreshing drops the deleted device.
    let snapshot = f.handle.refresh_device(&id(OVERRIDER)).await.unwrap();
    assert!(snapshot.device(&id(OVERRIDER)).is_none());
    f.stop().await;
}

#[tokio::test]
async fn a_device_watch_wakes_only_for_its_device() {
    let f = fixture(true).await;
    let mut follower = f.handle.watch_device(id(FOLLOWER));
    let mut overrider = f.handle.watch_device(id(OVERRIDER));
    let first = follower.current();
    // Ring is overridden by the overrider, so a fleet Ring change reaches the follower only.
    f.handle
        .apply_fleet_edit(SettingsPatch::from_value(&json!({"ledScene": "airy"})).unwrap())
        .await
        .unwrap();
    let next = follower.changed().await.unwrap();
    assert_eq!(next.ring.led_scene, LedScene::Airy);
    assert!(!Arc::ptr_eq(&first, &next));
    let quiet = tokio::time::timeout(Duration::from_millis(50), overrider.changed()).await;
    assert!(quiet.is_err(), "the overrider's settings did not change");
    f.stop().await;
}

fn synthetic_model(graph: &[u8]) -> NewModel {
    NewModel {
        graph: graph.to_vec(),
        sidecar: br#"{"type": "bcresnet", "sampleRate": 16000, "window": 22400, "labels": ["noise", "tara", "unknown"], "wakeIndex": 1, "normPeak": 0.8}"#.to_vec(),
        thresholds: ModelThresholds::new(0.85, 0.6, 0.25).unwrap(),
        wake_phrase: SpokenForm::new("tara").unwrap(),
        verify_core: SpokenForm::new("tar").unwrap(),
        probe: ProbeResult::new(0.001, 0.02, 0.003).unwrap(),
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn models_are_added_selected_and_deleted_only_when_unused() {
    let f = fixture(true).await;
    let model = synthetic_model(b"synthetic graph");
    let graph = Sha256::of(&model.graph);
    let AddOutcome::Added(added) = f.handle.add_wake_model(model).await.unwrap() else {
        panic!("a new graph is added");
    };
    assert_eq!(*added.graph_sha256(), graph);
    let names = file_names(&f.models);
    assert!(names.contains(&format!("{graph}.onnx")));

    // The overrider takes Wake word and selects the new model.
    let edit = DeviceEdit::from_value(&json!({
        "config_sections": ["ring", "timers", "wakeword"],
        "wakeModel": graph.to_string(),
    }))
    .unwrap();
    let outcome = f
        .handle
        .apply_device_edit(id(OVERRIDER), edit)
        .await
        .unwrap();
    assert_eq!(outcome.new_session_needed, [id(OVERRIDER)]);
    let err = f.handle.delete_wake_model(graph.clone()).await.unwrap_err();
    assert!(
        matches!(&err, ModelError::InUse(users) if *users == [Selector::Device(id(OVERRIDER))])
    );
    let err = f.handle.delete_wake_model(deployed()).await.unwrap_err();
    assert!(
        matches!(&err, ModelError::InUse(users) if users[0] == Selector::Fleet && users.len() == 2)
    );

    // Once nothing selects it, it goes, files and all; nothing else is renamed or removed.
    let edit = DeviceEdit::from_value(&json!({"config_sections": ["ring", "timers"]})).unwrap();
    f.handle
        .apply_device_edit(id(OVERRIDER), edit)
        .await
        .unwrap();
    let snapshot = f.handle.delete_wake_model(graph.clone()).await.unwrap();
    assert!(snapshot.registry().model(&graph).is_none());
    let mut expected = names.clone();
    expected.retain(|n| {
        !n.starts_with(graph.as_str()) && !n.starts_with(added.sidecar_sha256().as_str())
    });
    assert_eq!(file_names(&f.models), expected);
    f.stop().await;
}

#[tokio::test]
async fn ready_grant_carries_the_settings_detector() {
    let f = fixture(true).await;
    let runtime = Sha256::of(b"synthetic runtime");
    // Fleet: two shadow rules, no extra open rules, duck -30, near-miss default.
    let grant = f
        .handle
        .snapshot()
        .ready_grant(&id(FOLLOWER), runtime.clone(), true, true)
        .unwrap();
    assert_eq!(grant.assets.graph_sha256, deployed());
    assert_eq!(
        grant.assets.sidecar_sha256.as_str(),
        echomuse_settings::registry::DEPLOYED_SIDECAR_SHA256
    );
    assert_eq!(grant.assets.runtime_sha256, runtime);
    assert_eq!(
        serde_json::to_value(&grant.detector).unwrap(),
        json!({
            "thresholds": {"idle": 0.9, "playback": 0.65, "near_miss": 0.17},
            "hop_blocks": 2, "smoothing": 3, "clear_after_unscored": 6,
            "provisional_duck": {"duck_db": -30.0, "max_per_window": 2, "window_ms": 5000},
            "shadow_rules": [
                {"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.92},
                {"profile": "idle", "windows": 1, "combine": "all", "threshold": 0.96}
            ]
        })
    );
    // An extra open rule brings the two baseline rules with it.
    let patch = SettingsPatch::from_value(&json!({
        "wakeOpenRules": [{"profile": "playback", "windows": 2, "combine": "mean", "threshold": 0.6}],
        "wakeShadowRules": [],
        "nearMissThreshold": 0.2,
    }))
    .unwrap();
    f.handle.apply_fleet_edit(patch).await.unwrap();
    let grant = f
        .handle
        .snapshot()
        .ready_grant(&id(FOLLOWER), runtime, false, true)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&grant.detector).unwrap(),
        json!({
            "thresholds": {"idle": 0.9, "playback": 0.65, "near_miss": 0.2},
            "hop_blocks": 2, "smoothing": 3, "clear_after_unscored": 6,
            "provisional_duck": {"duck_db": -30.0, "max_per_window": 2, "window_ms": 5000},
            "open_rules": [
                {"profile": "idle", "windows": 3, "combine": "mean", "threshold": 0.9},
                {"profile": "playback", "windows": 3, "combine": "mean", "threshold": 0.65},
                {"profile": "playback", "windows": 2, "combine": "mean", "threshold": 0.6}
            ]
        })
    );
    f.stop().await;
}
