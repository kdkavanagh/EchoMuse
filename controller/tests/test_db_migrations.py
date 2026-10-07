"""Migration safety, including the transactional schema-21 → 22 cutover."""

import json
import os
import sqlite3

import pytest

import em_db

VER = "SELECT value FROM system_config WHERE key = 'schema_version'"


def _legacy_db(tmp_path, upto=11, label="Kitchen"):
    """A database as a controller `upto` migrations ago left it."""
    p = str(tmp_path / "em.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:upto]:
        c.executescript(sql)
    c.execute("INSERT INTO devices (device_id, label, approved) VALUES ('D', ?, 1)",
              (label,))
    c.commit()
    c.close()
    return p


def _v21_db(tmp_path):
    """A schema-21 database with fleet/device values the cutover rewrites."""
    p = str(tmp_path / "v21.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:21]:
        c.executescript(sql)
    fleet = {
        "owwModel": "ophelia-bcresnet", "owwThreshold": 0.77,
        "bargeInEnabled": False, "maxSpeechMs": 0,
        "timerSound": "chime", "timerRingSeconds": 60,
        "timerRingGapSeconds": 3.0,
    }
    c.execute("UPDATE system_config SET value=? WHERE key='global_device_config'",
              (json.dumps(fleet),))
    c.execute(
        "INSERT INTO devices (device_id,label,approved,config,config_sections,use_global_config) "
        "VALUES (?,?,?,?,?,0)",
        ("short", "Short", 1, json.dumps({
            "maxSpeechMs": 15_000, "timerSound": "beep",
            "timerRingSeconds": 60, "owwModel": "old",
        }), json.dumps(["wakeword", "microphones", "timers"])),
    )
    c.execute(
        "INSERT INTO devices (device_id,label,approved,config,config_sections,use_global_config) "
        "VALUES (?,?,?,?,?,0)",
        ("long", "Long", 1, json.dumps({
            "maxSpeechMs": 15_001, "timerRingSeconds": 120,
            "endpointRelative": True,
        }), json.dumps(["microphones", "timers"])),
    )
    c.commit()
    c.close()
    return p


def test_a_multi_version_jump_migrates_and_keeps_data(tmp_path):
    p = _legacy_db(tmp_path)
    em_db.init(p)
    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)
    assert c.execute("SELECT label FROM devices").fetchone()[0] == "Kitchen"


def test_a_backup_is_taken_before_migrating(tmp_path):
    p = _legacy_db(tmp_path)
    em_db.init(p)
    bak = f"{p}.pre-v11.bak"
    assert os.path.exists(bak)
    b = sqlite3.connect(bak)
    assert int(b.execute(VER).fetchone()[0]) == 11
    assert b.execute("SELECT label FROM devices").fetchone()[0] == "Kitchen"


def test_a_fresh_database_is_not_backed_up(tmp_path):
    em_db.init(str(tmp_path / "fresh.db"))
    assert not [f for f in os.listdir(tmp_path) if f.endswith(".bak")]


def test_restarting_current_schema_does_not_rebackup(tmp_path):
    p = _legacy_db(tmp_path)
    em_db.init(p)
    before = sorted(f for f in os.listdir(tmp_path) if f.endswith(".bak"))
    em_db.init(p)
    assert sorted(f for f in os.listdir(tmp_path) if f.endswith(".bak")) == before


def test_a_failed_backup_refuses_to_migrate(tmp_path, monkeypatch):
    p = _legacy_db(tmp_path)
    real = sqlite3.connect

    def boom(path, *args, **kwargs):
        if str(path).endswith(".bak"):
            raise sqlite3.OperationalError("disk I/O error")
        return real(path, *args, **kwargs)

    monkeypatch.setattr(sqlite3, "connect", boom)
    with pytest.raises(RuntimeError, match="back up"):
        em_db.init(p)
    monkeypatch.undo()
    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == 11
    assert c.execute("SELECT label FROM devices").fetchone()[0] == "Kitchen"


def test_backup_can_be_skipped_deliberately(tmp_path, monkeypatch):
    p = _legacy_db(tmp_path)
    real = sqlite3.connect
    monkeypatch.setenv("EM_SKIP_DB_BACKUP", "1")
    monkeypatch.setattr(
        sqlite3, "connect",
        lambda path, *a, **k: (_ for _ in ()).throw(sqlite3.OperationalError("x"))
        if str(path).endswith(".bak") else real(path, *a, **k),
    )
    em_db.init(p)
    monkeypatch.undo()
    assert int(sqlite3.connect(p).execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)


def test_newer_database_refuses_to_start_and_names_versions(tmp_path, monkeypatch):
    p = _legacy_db(tmp_path)
    em_db.init(p)
    latest = len(em_db.MIGRATIONS)
    monkeypatch.setattr(em_db, "MIGRATIONS", em_db.MIGRATIONS[:-2])
    with pytest.raises(RuntimeError, match="newer version") as exc:
        em_db.init(p)
    message = str(exc.value)
    assert f"v{latest}" in message and f"v{latest - 2}" in message
    assert ".bak" in message


def test_v22_fresh_database_has_cutover_schema_and_defaults(tmp_path):
    p = str(tmp_path / "fresh-v22.db")
    em_db.init(p)
    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)
    turn_cols = {r[1] for r in c.execute("PRAGMA table_info(turns)")}
    assert {
        "wake_model_sha256", "policy_hash", "wake_attribution",
        "reference_coverage", "commit_route", "terminal_reason", "commit_id",
    } <= turn_cols
    assert "dev_hops" in {r[1] for r in c.execute("PRAGMA table_info(wake_counters)")}
    assert c.execute("SELECT 1 FROM alert_ops").fetchall() == []
    assert c.execute("SELECT 1 FROM alert_delivery").fetchall() == []
    assert c.execute("SELECT 1 FROM alert_scripts").fetchall() == []
    fleet = json.loads(c.execute(
        "SELECT value FROM system_config WHERE key='global_device_config'"
    ).fetchone()[0])
    assert fleet["wakeModel"] == em_db.DEPLOYED_WAKE_MODEL
    assert fleet["timerRingSeconds"] == 900
    assert fleet["alarmSound"] == ""
    assert fleet["extendedUtterances"] is False
    assert not (em_db.REMOVED_CONFIG_KEYS & fleet.keys())


def test_v22_upgrades_configs_and_exports_effective_sounds(tmp_path, monkeypatch):
    p = _v21_db(tmp_path)
    exported = []

    def fake_export(sound_id, directory):
        exported.append((sound_id, directory))
        return em_db.em_sounds.AlertExport("a" * 64, 1.0, sound_id == "beep")

    monkeypatch.setattr(em_db.em_sounds, "export", fake_export)
    em_db.init(p)

    c = sqlite3.connect(p)
    c.row_factory = sqlite3.Row
    assert int(c.execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)
    fleet = json.loads(c.execute(
        "SELECT value FROM system_config WHERE key='global_device_config'"
    ).fetchone()["value"])
    assert fleet["wakeModel"] == em_db.DEPLOYED_WAKE_MODEL
    assert fleet["extendedUtterances"] is True
    assert fleet["timerRingSeconds"] == 900
    assert fleet["alarmSound"] == "chime"
    assert not (em_db.REMOVED_CONFIG_KEYS & fleet.keys())

    configs = {r["device_id"]: json.loads(r["config"])
               for r in c.execute("SELECT device_id,config FROM devices")}
    assert configs["short"]["extendedUtterances"] is False
    assert configs["short"]["timerRingSeconds"] == 900
    assert configs["long"]["extendedUtterances"] is True
    assert configs["long"]["timerRingSeconds"] == 120
    assert not any(em_db.REMOVED_CONFIG_KEYS & cfg.keys() for cfg in configs.values())
    expected_dir = em_db.em_sounds.sounds_dir(p)
    assert set(exported) == {
        ("default", expected_dir), ("beep", expected_dir), ("chime", expected_dir),
    }


def test_v22_failure_rolls_back_everything_then_resumes(tmp_path, monkeypatch):
    p = _v21_db(tmp_path)
    calls = 0

    def crash_once(_sound_id, _directory):
        nonlocal calls
        calls += 1
        if calls == 2:
            raise RuntimeError("simulated asset disk failure")
        return em_db.em_sounds.AlertExport("a" * 64, 1.0, False)

    monkeypatch.setattr(em_db.em_sounds, "export", crash_once)
    with pytest.raises(RuntimeError, match="migration v22 failed"):
        em_db.init(p)

    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == 21
    assert "wake_model_sha256" not in {r[1] for r in c.execute("PRAGMA table_info(turns)")}
    with pytest.raises(sqlite3.OperationalError, match="no such table"):
        c.execute("SELECT * FROM alert_ops")
    fleet = json.loads(c.execute(
        "SELECT value FROM system_config WHERE key='global_device_config'"
    ).fetchone()[0])
    assert fleet["owwModel"] == "ophelia-bcresnet"
    assert "wakeModel" not in fleet
    c.close()

    monkeypatch.setattr(
        em_db.em_sounds, "export",
        lambda _sid, _dir: em_db.em_sounds.AlertExport("b" * 64, 1.0, False),
    )
    em_db._migrate(em_db._conn)
    assert int(sqlite3.connect(p).execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)


def test_v23_strips_microphone_processing_keys_the_api_would_reject(tmp_path):
    # Stored AFE-era keys made every dashboard save fail: the API rejects keys
    # it does not know, and the dashboard posts back what it was given.
    dead = {"micGainDb": 6, "aecEnabled": True, "beamAngle": 90}
    p = str(tmp_path / "v22.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:22]:
        c.executescript(sql)
    c.execute("UPDATE system_config SET value=? WHERE key='global_device_config'",
              (json.dumps({**dead, "duckDb": -12}),))
    c.execute("INSERT INTO devices (device_id,label,approved,config) VALUES ('D','Kitchen',1,?)",
              (json.dumps({**dead, "nsAsr": True}),))
    c.commit()
    c.close()

    em_db.init(p)
    c = sqlite3.connect(p)
    fleet = json.loads(c.execute(
        "SELECT value FROM system_config WHERE key='global_device_config'").fetchone()[0])
    device = json.loads(c.execute("SELECT config FROM devices").fetchone()[0])
    assert fleet == {"duckDb": -12}
    assert device == {"nsAsr": True}
    assert set(dead) <= em_db.REMOVED_CONFIG_KEYS


def test_v26_adds_first_audio_and_trace_columns_to_existing_turns(tmp_path):
    p = str(tmp_path / "v25.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:25]:
        c.executescript(sql)
    c.execute("INSERT INTO turns (device_id, ts, outcome, turn_uuid) VALUES ('D', 1.0, 'ha', 't-1')")
    c.commit()
    c.close()

    em_db.init(p)
    c = sqlite3.connect(p)
    row = c.execute("SELECT turn_uuid, first_audio_ms, decision_trace FROM turns").fetchone()
    assert row == ("t-1", None, None)                  # history kept; unmeasured reads NULL


def test_v27_adds_the_wake_shadow_tables_to_an_existing_database(tmp_path):
    p = str(tmp_path / "v26.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:26]:
        c.executescript(sql)
    c.execute("INSERT INTO wake_counters (device_id, hour_ts, near_misses) VALUES ('D', 3600, 2)")
    c.commit()
    c.close()

    em_db.init(p)
    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == len(em_db.MIGRATIONS)
    assert c.execute("SELECT near_misses FROM wake_counters").fetchone() == (2,)     # history kept
    assert c.execute("SELECT COUNT(*) FROM wake_shadow").fetchone() == (0,)           # no data, not zeros
    assert c.execute("SELECT COUNT(*) FROM wake_shadow_events").fetchone() == (0,)
    assert "idx_wake_shadow_events_device_ts" in {
        r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE type = 'index'")}


def test_v28_adds_afe_evidence_and_older_turns_read_unavailable(tmp_path):
    p = str(tmp_path / "v27.db")
    c = sqlite3.connect(p)
    for sql in em_db.MIGRATIONS[:27]:
        c.executescript(sql)
    c.execute("INSERT INTO turns (device_id, ts, outcome, turn_uuid) VALUES ('D', 1.0, 'ha', 't-1')")
    c.commit()
    c.close()

    em_db.init(p)
    c = sqlite3.connect(p)
    assert int(c.execute(VER).fetchone()[0]) == 28 == len(em_db.MIGRATIONS)
    # NULL is "unavailable" (a row predating native AFE evidence), never a zero summary.
    assert c.execute("SELECT turn_uuid, afe_evidence FROM turns").fetchone() == ("t-1", None)


def test_connect_alerts_shares_database_but_not_connection(tmp_path):
    p = str(tmp_path / "alerts.db")
    em_db.init(p)
    alerts = em_db.connect_alerts()
    try:
        assert alerts is not em_db._conn
        assert alerts.execute("SELECT COUNT(*) FROM alert_ops").fetchone()[0] == 0
        with em_db.alerts_lock, alerts:
            alerts.execute(
                "INSERT INTO alert_delivery VALUES (?,?,?,?,?,?)",
                ("dev", "entry", "calendar.dev", "epoch", 0, 0),
            )
        assert em_db._q1(
            "SELECT endpoint_id FROM alert_delivery WHERE endpoint_id='dev'"
        )["endpoint_id"] == "dev"
    finally:
        alerts.close()
