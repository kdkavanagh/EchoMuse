"""
Schema and writer tests for turn traces, wake counters and device metrics,
against a real temporary database migrated from scratch.
"""

import pytest

import em_db as db


@pytest.fixture()
def fresh_db(tmp_path, monkeypatch):
    """A real database migrated from scratch to the current schema."""
    path = tmp_path / "test.db"
    db.init(str(path))
    yield db
    if db._conn is not None:
        db._conn.close()
        db._conn = None


def _cols(table: str) -> set:
    return {r[1] for r in db._conn.execute(f"PRAGMA table_info({table})")}


def test_record_device_stats_accumulates_link_metrics(fresh_db):
    db.register_new_device("dev1", "1.2.3.4", "v2.9.6")
    db.record_device_stats("dev1", {
        "cpuPct": 20.0, "memUsedMb": 180, "wifiRssi": -55,
        "linkSpeedMbps": 135, "wifiFreqMhz": 5805, "wifiBssid": "aa:bb",
        "txBytes": 1000, "rxBytes": 2000, "txErrors": 1,
        "txDropped": 2, "rxCrcErrors": 3,
    })
    db.record_device_stats("dev1", {
        "cpuPct": 22.0, "memUsedMb": 181, "wifiRssi": -57,
        "linkSpeedMbps": 72, "wifiFreqMhz": 2412, "wifiBssid": "aa:cc",
        "txBytes": 500, "rxBytes": 700, "txErrors": 4,
        "txDropped": 0, "rxCrcErrors": 1,
    })
    row = db._q1("SELECT * FROM device_metrics WHERE device_id = 'dev1'")
    assert row["samples"] == 2
    assert row["tx_bytes_sum"] == 1500
    assert row["rx_bytes_sum"] == 2700
    assert row["tx_errors_sum"] == 5
    assert row["rx_crc_sum"] == 4
    # Latest identity wins (a band change should be visible)...
    assert row["link_speed_last"] == 72
    assert row["wifi_freq_last"] == 2412
    assert row["wifi_bssid_last"] == "aa:cc"
    # ...but the worst PHY rate is what a throughput hunt needs.
    assert row["link_speed_min"] == 72


def test_link_speed_absent_does_not_poison_minimum(fresh_db):
    """linkSpeedMbps is omitempty and refreshed on a slower cadence, so a
    tick without it must not record a 0 Mbps minimum."""
    db.register_new_device("dev1", "1.2.3.4", "v2.9.6")
    db.record_device_stats("dev1", {"cpuPct": 5.0, "memUsedMb": 100,
                                    "linkSpeedMbps": 150})
    db.record_device_stats("dev1", {"cpuPct": 5.0, "memUsedMb": 100})
    row = db._q1("SELECT * FROM device_metrics WHERE device_id = 'dev1'")
    assert row["link_speed_min"] == 150
    assert row["link_speed_last"] == 150


def test_migrates_to_head(fresh_db):
    """Head of MIGRATIONS, so appending one without bumping its own
    schema_version statement fails here rather than in production."""
    expected = str(len(db.MIGRATIONS))
    assert db._q1("SELECT value FROM system_config WHERE key='schema_version'")["value"] == expected


def test_rtt_accumulates_and_keeps_extremes(fresh_db):
    db.register_new_device("dev1", "10.0.0.9", "vtest")
    db.record_device_stats("dev1", {
        "cpuPct": 5.0, "memUsedMb": 100,
        "rttSumMs": 300, "rttSamples": 6, "rttMinMs": 30, "rttMaxMs": 90,
        "rttExcursions": 0, "rttExcursionsIdle": 0,
    })
    db.record_device_stats("dev1", {
        "cpuPct": 5.0, "memUsedMb": 100,
        "rttSumMs": 1400, "rttSamples": 6, "rttMinMs": 40, "rttMaxMs": 1100,
        "rttExcursions": 2, "rttExcursionsIdle": 2,
    })
    m = db.get_device_metrics("dev1", 0)[-1]
    assert m["rtt_samples"] == 12
    assert m["rtt_avg_ms"] == round(1700 / 12, 1)
    assert m["rtt_min_ms"] == 30    # best across both windows
    assert m["rtt_max_ms"] == 1100  # worst across both windows
    assert m["rtt_excursions"] == 2
    assert m["rtt_excursions_idle"] == 2


def test_window_without_rtt_samples_does_not_poison_the_minimum(fresh_db):
    """
    A stats report can land with no completed probe in its window. That must
    not reset the running minimum to 0 — the same class of bug as
    link_speed's absent-means-not-sampled.
    """
    db.register_new_device("dev1", "10.0.0.9", "vtest")
    db.record_device_stats("dev1", {
        "cpuPct": 5.0, "memUsedMb": 100,
        "rttSumMs": 120, "rttSamples": 3, "rttMinMs": 35, "rttMaxMs": 50,
    })
    db.record_device_stats("dev1", {"cpuPct": 5.0, "memUsedMb": 100})
    m = db.get_device_metrics("dev1", 0)[-1]
    assert m["rtt_min_ms"] == 35
    assert m["rtt_max_ms"] == 50
    assert m["rtt_samples"] == 3


def test_driver_dead_counters_are_not_surfaced(fresh_db):
    """
    tx_errors/tx_dropped/rx_crc read zero on this hardware regardless of link
    quality, so they must not appear in the read API where a zero would be
    mistaken for health.
    """
    db.register_new_device("dev1", "10.0.0.9", "vtest")
    db.record_device_stats("dev1", {"cpuPct": 5.0, "memUsedMb": 100})
    m = db.get_device_metrics("dev1", 0)[-1]
    for dead in ("tx_errors", "tx_dropped", "rx_crc"):
        assert dead not in m


def test_excursion_rate_not_raw_count_is_the_discriminator(fresh_db):
    """
    "Every excursion happened while idle" is vacuous if almost every SAMPLE
    is idle — which is the normal case, since devices spend most of their
    life outside a turn. The read API must expose per-state RATES so the
    comparison is meaningful.
    """
    db.register_new_device("dev1", "10.0.0.9", "vtest")
    # 100 samples: 90 idle with 9 excursions (10%), 10 busy with 5 (50%).
    db.record_device_stats("dev1", {
        "cpuPct": 5.0, "memUsedMb": 100,
        "rttSumMs": 5000, "rttSamples": 100, "rttMinMs": 5, "rttMaxMs": 900,
        "rttExcursions": 14, "rttExcursionsIdle": 9, "rttSamplesIdle": 90,
    })
    m = db.get_device_metrics("dev1", 0)[-1]
    assert m["rtt_excursion_pct_idle"] == 10.0
    assert m["rtt_excursion_pct_busy"] == 50.0


def test_excursion_rates_are_none_without_samples_in_that_state(fresh_db):
    """No busy samples must read None, not 0% — absence of data is not
    evidence of a clean state."""
    db.register_new_device("dev1", "10.0.0.9", "vtest")
    db.record_device_stats("dev1", {
        "cpuPct": 5.0, "memUsedMb": 100,
        "rttSumMs": 500, "rttSamples": 10, "rttMinMs": 5, "rttMaxMs": 90,
        "rttExcursions": 1, "rttExcursionsIdle": 1, "rttSamplesIdle": 10,
    })
    m = db.get_device_metrics("dev1", 0)[-1]
    assert m["rtt_excursion_pct_idle"] == 10.0
    assert m["rtt_excursion_pct_busy"] is None


def test_device_metrics_has_thermal_columns(fresh_db):
    cols = _cols("device_metrics")
    for c in ("cpu_temp_sum", "cpu_temp_samples", "cpu_temp_max", "max_temp_max",
              "cores_online_last", "cores_online_min", "cores_total",
              "thermal_limit_min"):
        assert c in cols, f"device_metrics.{c} missing"


def test_thermal_stats_relay_and_rollup(fresh_db):
    """The relay guard: a device stat has to be named in DeviceStats (Go), the
    em_controller allowlist AND here, or it is silently dropped. This covers
    the third."""
    db.record_device_stats("dev-t", {
        "cpuPct": 25.0, "memUsedMb": 200, "cpuTempC": 33.0, "maxTempC": 34.2,
        "coresOnline": 2, "coresTotal": 4, "thermalCoreLimit": 4,
    })
    db.record_device_stats("dev-t", {
        "cpuPct": 51.0, "memUsedMb": 205, "cpuTempC": 39.0, "maxTempC": 41.0,
        "coresOnline": 1, "coresTotal": 4, "thermalCoreLimit": 3,
    })
    m = db.get_device_metrics("dev-t", 0)[0]
    assert m["cpu_temp_avg"] == 36.0, "mean of 33 and 39"
    assert m["cpu_temp_max"] == 39.0
    assert m["max_temp_max"] == 41.0
    assert m["cores_online_last"] == 1
    assert m["cores_online_min"] == 1, "the tightest the device ever ran"
    assert m["cores_total"] == 4
    assert m["thermal_limit_min"] == 3, "throttling happened this hour"


def test_unreadable_temp_does_not_dilute_the_mean(fresh_db):
    """A missing sensor reading must be skipped, not counted as 0C — averaging
    a zero in reads as a cool device, which is the wrong direction for a metric
    whose whole job is to warn."""
    db.record_device_stats("dev-u", {"cpuPct": 20.0, "cpuTempC": 40.0})
    db.record_device_stats("dev-u", {"cpuPct": 20.0})  # sensor unreadable
    m = db.get_device_metrics("dev-u", 0)[0]
    assert m["samples"] == 2
    assert m["cpu_temp_avg"] == 40.0, "one temp sample, not 20.0"


def test_metrics_without_thermals_stay_null(fresh_db):
    """Older firmware sends no thermal fields; those must read as absent rather
    than as a 0C device with 0 cores."""
    db.record_device_stats("dev-old", {"cpuPct": 22.0, "memUsedMb": 180})
    m = db.get_device_metrics("dev-old", 0)[0]
    assert m["cpu_temp_avg"] is None
    assert m["cores_online_last"] is None
    assert m["thermal_limit_min"] is None


TRACE = {
    "wake_model_sha256": "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f",
    "policy_hash": "post_afe_1:abc",
    "wake_attribution": "user",
    "reference_coverage": 0.97,
    "commit_route": "A",
    "terminal_reason": "handled",
    "commit_id": "c-1",
}


def test_turn_decision_trace_round_trips(fresh_db):
    """§11.3 trace columns survive insert_turn's key→column mapping and come
    back through get_turns under the same keys."""
    turn_id = db.insert_turn("dev1", {"ts": 1_800_000_000, "trigger": "wake",
                                      "outcome": "ok", **TRACE})
    rec = db.get_turns("dev1")[-1]
    assert rec["turn_id"] == turn_id
    for key, value in TRACE.items():
        assert rec[key] == value


def test_trace_is_null_when_unrecorded_and_history_columns_are_not_written(fresh_db):
    """Missing trace fields read NULL (not measured), and the legacy shadow /
    delivery columns keep history only: insert_turn ignores them (§18.4)."""
    db.insert_turn("dev1", {"ts": 1_800_000_000, "outcome": "no_input",
                            "dev_wake_score": 0.6, "delivery_ms": 900})
    rec = db.get_turns("dev1")[-1]
    assert all(rec[key] is None for key in TRACE)
    assert rec["dev_wake_score"] is None and rec["delivery_ms"] is None


def _stored_traces(device_id: str) -> list:
    return [r[0] for r in db._conn.execute(
        "SELECT decision_trace FROM turns WHERE device_id = ? ORDER BY ts", (device_id,))]


def test_the_decision_trace_is_stored_with_the_row_but_not_returned_by_get_turns(fresh_db):
    """The trace JSON is for sqlite analysis; the Activity list gets
    first_audio_ms but never the 3–6 KB trace."""
    db.insert_turn("dev1", {"ts": 1_800_000_000, "first_audio_ms": 1_430,
                            "decision_trace": '{"turn_id":"t-1"}'})
    rec = db.get_turns("dev1")[-1]
    assert rec["first_audio_ms"] == 1_430
    assert "decision_trace" not in rec
    assert _stored_traces("dev1") == ['{"turn_id":"t-1"}']


def test_only_each_devices_newest_trace_retention_turns_keep_their_trace(fresh_db, monkeypatch):
    monkeypatch.setattr(db, "TRACE_RETENTION", 3)
    db.insert_turn("dev2", {"ts": 1_700_000_000, "decision_trace": "other"})
    for i in range(5):
        db.insert_turn("dev1", {"ts": 1_800_000_000 + i, "first_audio_ms": i,
                                "decision_trace": f"t{i}"})
    assert _stored_traces("dev1") == [None, None, "t2", "t3", "t4"]
    assert [r["first_audio_ms"] for r in db.get_turns("dev1")] == [0, 1, 2, 3, 4]   # rows kept
    assert _stored_traces("dev2") == ["other"]                                     # per device


def test_numpy_scores_are_stored_as_numbers(fresh_db):
    np = pytest.importorskip("numpy")
    db.insert_turn("dev1", {"ts": 1_800_000_000, "wake_score": np.float32(0.9),
                            "reference_coverage": np.float64(0.5)})
    rec = db.get_turns("dev1")[-1]
    assert isinstance(rec["wake_score"], float)
    assert rec["reference_coverage"] == 0.5


def test_wake_stats_windows_accumulate_into_the_hour(fresh_db):
    """§18.4 step 4: counts add, maxima take the maximum, and a window that did
    not measure a maximum (None) leaves the stored one alone."""
    db.bump_wake_counters("dev2", near_misses=2, near_miss_max=0.31, dev_hops=187,
                          dev_drops=1, dev_crossings=1, dev_max_score=0.93,
                          dev_max_infer_ms=131.4)
    db.bump_wake_counters("dev2", near_misses=1, near_miss_max=0.2, dev_hops=188,
                          dev_crossings=0, dev_max_score=0.4, dev_max_infer_ms=90.0)
    db.bump_wake_counters("dev2", dev_hops=5, near_miss_max=None,
                          dev_max_score=None, dev_max_infer_ms=None)
    rows = db.get_wake_counters("dev2", 0)
    assert len(rows) == 1
    r = rows[0]
    assert (r.near_misses, r.dev_hops, r.dev_drops, r.dev_crossings) == (3, 380, 1, 1)
    assert r.near_miss_max == 0.31
    assert r.dev_max_score == 0.93
    assert r.dev_max_infer_ms == 131
