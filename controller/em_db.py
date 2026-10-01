"""
db.py — EchoMuse Controller persistence layer
==============================================

SQLite-backed storage for device registry, per-device config, logs,
users, sessions, system configuration, and activity stats (voice turns,
hourly wake/near-miss counters, hourly hardware metrics).

All public functions are synchronous — they are intended to be called
from asyncio handlers via loop.run_in_executor() when called on the
hot path, or directly for startup/shutdown operations.

Usage:
    import db

    db.init("echomuse.db")          # call once at startup

    device = db.get_device(device_id)
    db.upsert_device_seen(device_id, ip, version)
    db.log_device(device_id, db.LogLevel.INFO, db.LogSource.DEVICE, "Connected")
"""

import json
import logging
import os
import sqlite3
import threading
import time
from collections.abc import Iterable, Iterator, Mapping, Sequence
from contextlib import contextmanager
from dataclasses import dataclass, fields
from enum import StrEnum
from typing import Callable, Final, Optional, Self

import em_ambient
import em_config_sections
import em_recordings
import em_samples
import em_sounds
import em_wakeclips
import em_wake_rules

from em_config_sections import DeviceConfig

log = logging.getLogger("echomuse.db")


class SystemConfigKey(StrEnum):
    """`system_config` keys (get_config/set_config)."""
    SCHEMA_VERSION = "schema_version"
    GLOBAL_DEVICE_CONFIG = "global_device_config"
    NEXT_ESPHOME_PORT = "next_esphome_port"
    DEVICE_APPROVAL = "device_approval"
    SESSION_EXPIRY_DAYS = "session_expiry_days"
    UPDATE_CHECK_INTERVAL = "update_check_interval"
    GITHUB_REPO = "github_repo"
    LATEST_VERSION = "latest_version"
    LATEST_BINARY_URL = "latest_binary_url"
    LATEST_NOTES = "latest_notes"
    LATEST_RELEASE_URL = "latest_release_url"
    LATEST_PUBLISHED_AT = "latest_published_at"
    LAST_UPDATE_CHECK = "last_update_check"
    LATEST_CONTROLLER_VERSION = "latest_controller_version"
    LATEST_CONTROLLER_NOTES = "latest_controller_notes"
    LATEST_CONTROLLER_PUBLISHED_AT = "latest_controller_published_at"


class LogLevel(StrEnum):
    """`device_logs.level`."""
    INFO = "info"
    WARN = "warn"
    ERROR = "error"


class LogSource(StrEnum):
    """`device_logs.source`: who wrote the entry."""
    CONTROLLER = "controller"
    DEVICE = "device"


# ─── Row records ──────────────────────────────────────────────────────────────
#
# What the public getters return instead of sqlite3.Row. Fields follow the
# table's `SELECT *` column order (CREATE, then each ALTER in migration order),
# so asdict() keeps the key order the rows serialised with. from_row reads
# every column by name: a column a later migration adds is ignored until a
# field names it.

@dataclass(frozen=True, slots=True)
class DeviceRow:
    """`devices`. INTEGER flags stay ints; config/config_sections are the
    stored JSON text."""
    device_id: str
    label: Optional[str]
    approved: int
    ip: Optional[str]
    firmware_ver: Optional[str]
    firmware_previous: Optional[str]
    first_seen: Optional[int]
    last_seen: Optional[int]
    config: str
    esphome_api_port: Optional[int]
    esphome_noise_psk: Optional[str]
    use_global_config: int
    ble_proxy_port: Optional[int]
    token: Optional[str]
    config_sections: str
    collect_mode: int
    ambient_mode: int

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> Self:
        return cls(**{f.name: row[f.name] for f in fields(cls)})


@dataclass(frozen=True, slots=True)
class DeviceLogRow:
    """`device_logs`."""
    id: int
    device_id: str
    ts: int
    level: str
    source: str
    message: str

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> Self:
        return cls(**{f.name: row[f.name] for f in fields(cls)})


@dataclass(frozen=True, slots=True)
class WakeCounterRow:
    """`wake_counters`: one device-hour."""
    device_id: str
    hour_ts: int
    near_misses: int
    near_miss_max: float
    underruns: int
    dev_frames: int
    dev_drops: int
    dev_crossings: int
    dev_max_score: float
    dev_max_infer_ms: int
    dev_max_gap_ms: int
    dev_hops: int

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> Self:
        return cls(**{f.name: row[f.name] for f in fields(cls)})


@dataclass(frozen=True, slots=True)
class UserRow:
    """`users`."""
    id: int
    username: str
    password_hash: str
    role: str
    created_at: int

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> Self:
        return cls(**{f.name: row[f.name] for f in fields(cls)})


@dataclass(frozen=True, slots=True)
class SessionRow:
    """`sessions`."""
    token: str
    user_id: int
    created_at: int
    expires_at: int

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> Self:
        return cls(**{f.name: row[f.name] for f in fields(cls)})


# ─── Default device config ────────────────────────────────────────────────────

# The deployed BCResNet graph (SPEC §5.1 [D1]); migration 22 makes it the
# fleet's wakeModel and the controller seeds the registry with it.
DEPLOYED_WAKE_MODEL = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f"

# Every key a config scope may hold (SPEC §18.4). Keys ride the device
# `config` message only where the device uses them; the rest are controller-
# side. Sections: em_config_sections.SECTIONS.
DEFAULT_DEVICE_CONFIG: DeviceConfig = {
    # Confirmation chime on a wake, played by the device as an `earcon`
    # source (§11.2): at candidate open on `local_wake_chime` firmware for
    # idle wakes, otherwise on acceptance. Off by default: an audible change
    # on every device it reaches is a decision, not an upgrade side effect.
    "wakeSound":        False,
    # Device state, not a setting (em_config_sections.STATE_KEYS).
    "startupVolume":    85,
    # True: a tap fires "single" on the HA event entity instead of starting a
    # turn. Hold is unaffected.
    "buttonSingleTapEvent": False,
    # Window (ms) for coalescing taps into double/triple; 0 disables. Delays
    # every tap, which is why it needs buttonSingleTapEvent.
    "buttonMultiTapMs": 0,
    # Dialog ducks content by this much (§6.2); also the provisional-duck depth
    # of a wake candidate over content (§16.1).
    "duckDb": -18.0,
    # Active wake graph: a WakeRegistry SHA-256 (§5.1). Thresholds belong to
    # the registry entry.
    "wakeModel":        DEPLOYED_WAKE_MODEL,
    # Multi-device wake suppression window (ms): the first claimant answers,
    # others within the window stand down (em_arbiter, §5.3). 0 disables.
    "wakeArbitrationMs": 700,
    # Extra live wake open rules (em_wake_rules, §5.2), each
    # {profile, windows, combine, threshold}: a candidate opens when the
    # model's 3-window baseline OR any of these fires. Empty: today's rule.
    "wakeOpenRules":    [],
    # Shadow rules an open_rules_v1 device evaluates without acting on them,
    # reporting how each would have compared with the live rules. Collecting
    # from the first deploy changes no behaviour.
    "wakeShadowRules":  [
        {"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.90},
        {"profile": "idle", "windows": 2, "combine": "mean", "threshold": 0.95},
        {"profile": "idle", "windows": 2, "combine": "all",  "threshold": 0.90},
        {"profile": "idle", "windows": 2, "combine": "all",  "threshold": 0.95},
        {"profile": "idle", "windows": 1, "combine": "mean", "threshold": 0.97},
    ],
    # DTLN noise suppression on the STT copy only (em_ns, §16.7).
    "nsAsr":            False,
    # Keep recent utterances (the uploaded STT copy) as WAVs. Off by default:
    # the only feature that writes recognisable speech to disk.
    "saveUtterances":   False,
    # Keep each accepted candidate's wake clip (support −300 ms … support_end).
    "saveWakeClips":    False,
    # Utterance cap: false → 15 s, true → 30 s (§16.6).
    "extendedUtterances": False,
    # Alert sounds are em_sounds catalog IDs. Empty = the fleet "default"
    # upload if present, else the device's built-in fallback tone (§16.5).
    # timerSound rings HA timers; alarmSound rings alarm events without their
    # own `echomuse:` sound and is the sound of new alarms (§16.7).
    "timerSound":       "",
    "alarmSound":       "",
    # Timer ring limit (s): HA discards a timer as it fires, so nothing else
    # would stop an unattended ring (§10.8).
    "timerRingSeconds": 900,
    # Silence between repeats of the timer sound (s).
    "timerRingGapSeconds": 2.0,
    # BLE proxy over the raw HCI transport (em_ble_proxy). Enabling durably
    # disables the Android Bluetooth stack on the device.
    "bleProxyEnabled":  False,
    # EQ applies to controller-rendered content and dialog, not to device-
    # executed alerts (§18.4).
    "eqBands":          [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    "eqLoudness":       False,
    # LED ring scene (em_scenes). Colours apply only to the "custom" scene.
    "ledScene":         "standard",
    "ledListenColor":   "#00b400",
    "ledThinkColor":    "#00c800",
    # Playback meter ring response; mirrors animator.go meterDefaults.
    "meterAttack":      0.6,   # envelope rise per 40 ms tick
    "meterDecay":       0.30,  # fall per tick
    "meterFloor":       0.06,  # perceptual brightness at silence
    "meterGamma":       2.2,   # >1 expands the dark end
    "meterRef":         0.22,  # speaker RMS mapped to full brightness
    "meterCurve":       0.7,   # <1 lifts quiet consonants
}

# Keys deleted from every config scope; the API rejects them by name.
# v22: the pre-cutover wake/endpoint keys (SPEC §18.4).
_V22_REMOVED_KEYS: frozenset[str] = frozenset({
    "owwModel", "owwThreshold", "bargeInThreshold", "nearMissThreshold",
    "owwOnDevice", "owwSpeexNs", "bargeInEnabled",
    "endpointRelative", "endpointLowPerMil", "endpointSilenceMs", "endpointBackporchMs",
    "maxSpeechMs", "vadThreshold", "vadSpeechMs", "vadSilenceMs",
    "timerRingBurstSeconds",
})
# v23: microphone-processing keys. The native AFE owns gain, AGC, echo
# cancellation and beamforming (§4.1); nothing reads these.
_V23_REMOVED_KEYS: frozenset[str] = frozenset({
    "micGainDb", "adcDigitalGain", "adcMicpga", "agcEnabled",
    "aecEnabled", "aecDelayMs", "aecTailMs", "beamformingEnabled", "beamAngle",
})
REMOVED_CONFIG_KEYS: frozenset[str] = _V22_REMOVED_KEYS | _V23_REMOVED_KEYS

# Maximum log rows retained per device. Older rows are pruned on insert.
LOG_RETENTION = 10_000

# Maximum voice-turn rows retained per device (pruned on insert). At even
# 100 turns/day this is many months of history.
TURN_RETENTION = 20_000

# Voice-turn rows per device that keep their §11.3 decision_trace (3–6 KB
# each); insert_turn NULLs it on older rows in the same transaction.
TRACE_RETENTION = 1_000

# Hourly wake_counters rows older than this are pruned on upsert.
WAKE_COUNTER_RETENTION_DAYS = 180

# Newest wake_shadow_events rows kept per device (pruned on insert).
WAKE_SHADOW_EVENT_RETENTION = 2_000

# ─── Migrations ───────────────────────────────────────────────────────────────
#
# Rules:
#   - Append-only. Never edit an existing entry.
#   - Each migration must update schema_version as its final statement.
#   - Use CREATE TABLE IF NOT EXISTS / INSERT OR IGNORE for idempotency.
#   - SQLite ALTER TABLE only supports ADD COLUMN. Renaming or dropping
#     columns requires a create-copy-drop migration.
#   - Nothing a migration executes may come from another module: a later
#     edit there would silently change an applied migration.

# §16.3 alert journal DDL verbatim, plus the script-installation record
# (§16.7): em_alerts' tables, created by migration 22 and frozen with it.
ALERT_SCHEMA_SQL: Final[str] = """\
CREATE TABLE alert_ops (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id TEXT NOT NULL UNIQUE,
    endpoint_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN
      ('create','update','dismiss','snooze','expire','cancel')),
    schedule_id TEXT NOT NULL,
    occurrence_key TEXT,
    source TEXT NOT NULL CHECK (source IN ('voice','llm','device','dashboard','engine')),
    payload_json TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','applied','rejected')),
    step INTEGER NOT NULL DEFAULT 0,
    result_json TEXT,
    created_ms INTEGER NOT NULL,
    applied_ms INTEGER,
    CHECK ((state = 'applied') = (applied_ms IS NOT NULL)),
    CHECK (action NOT IN ('dismiss','snooze','expire') OR occurrence_key IS NOT NULL)
);
CREATE INDEX alert_ops_pending ON alert_ops(endpoint_id, state, seq);
CREATE INDEX alert_ops_terminal ON alert_ops(schedule_id, occurrence_key);
CREATE TABLE alert_delivery (
    endpoint_id TEXT PRIMARY KEY,
    calendar_entry_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    delivery_epoch TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    acked_sequence INTEGER NOT NULL,
    CHECK (acked_sequence <= sequence)
);
CREATE TABLE alert_scripts (
    object_id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    revision INTEGER NOT NULL
);
"""

MIGRATIONS: list[str] = [
    # ── v1 — initial schema ──────────────────────────────────────────────────
    """
    CREATE TABLE IF NOT EXISTS devices (
        device_id         TEXT    PRIMARY KEY,
        label             TEXT,
        approved          INTEGER NOT NULL DEFAULT 0,
        ip                TEXT,
        firmware_ver      TEXT,
        firmware_previous TEXT,
        first_seen        INTEGER,
        last_seen         INTEGER,
        config            TEXT    NOT NULL DEFAULT '{}'
    );

    CREATE TABLE IF NOT EXISTS device_logs (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id  TEXT    NOT NULL,
        ts         INTEGER NOT NULL,
        level      TEXT    NOT NULL,
        source     TEXT    NOT NULL,
        message    TEXT    NOT NULL,
        FOREIGN KEY (device_id) REFERENCES devices(device_id)
    );
    CREATE INDEX IF NOT EXISTS idx_device_logs_device_ts
        ON device_logs(device_id, ts DESC);

    CREATE TABLE IF NOT EXISTS users (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        username      TEXT    UNIQUE NOT NULL,
        password_hash TEXT    NOT NULL,
        role          TEXT    NOT NULL DEFAULT 'readonly',
        created_at    INTEGER NOT NULL
    );

    CREATE TABLE IF NOT EXISTS sessions (
        token      TEXT    PRIMARY KEY,
        user_id    INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        FOREIGN KEY (user_id) REFERENCES users(id)
    );

    CREATE TABLE IF NOT EXISTS system_config (
        key   TEXT PRIMARY KEY,
        value TEXT
    );

    INSERT OR IGNORE INTO system_config VALUES ('schema_version',        '1');
    INSERT OR IGNORE INTO system_config VALUES ('device_approval',       'strict');
    INSERT OR IGNORE INTO system_config VALUES ('session_expiry_days',   '30');
    INSERT OR IGNORE INTO system_config VALUES ('update_check_interval', '3600');
    INSERT OR IGNORE INTO system_config VALUES ('github_repo',           'wilbowes/EchoMuse');
    INSERT OR IGNORE INTO system_config VALUES ('latest_version',        NULL);
    INSERT OR IGNORE INTO system_config VALUES ('latest_binary_url',     NULL);
    INSERT OR IGNORE INTO system_config VALUES ('last_update_check',     NULL);
    """,

    # ── v2 — ESPHome native API integration ─────────────────────────────────
    """
    ALTER TABLE devices ADD COLUMN esphome_api_port INTEGER;
    ALTER TABLE devices ADD COLUMN esphome_noise_psk TEXT;

    INSERT OR IGNORE INTO system_config VALUES ('next_esphome_port', '16001');

    UPDATE system_config SET value = '2' WHERE key = 'schema_version';
    """,

    # ── v3 — Global device config and per-device override flag ───────────────
    #
    # use_global_config=1 (default): device inherits fleet-wide defaults.
    # use_global_config=0: device has its own config stored in the config column.
    #
    # global_device_config: JSON blob in system_config, same shape as
    # DEFAULT_DEVICE_CONFIG. Seeded from DEFAULT_DEVICE_CONFIG at migration time
    # so existing installs get the same values they were already using.
    f"""
    ALTER TABLE devices ADD COLUMN use_global_config INTEGER NOT NULL DEFAULT 1;

    INSERT OR IGNORE INTO system_config VALUES ('global_device_config', '{json.dumps(DEFAULT_DEVICE_CONFIG)}');

    UPDATE system_config SET value = '3' WHERE key = 'schema_version';
    """,

    # ── v4 — BLE proxy (second ESPHome device per Echo) ──────────────────────
    #
    # ble_proxy_port: TCP port of the device's bluetooth_proxy ESPHome
    # listener. Allocated lazily on first enable from the same
    # next_esphome_port counter as the voice satellite (same never-reuse
    # invariant), so voice and BT ports share one sparse range.
    """
    ALTER TABLE devices ADD COLUMN ble_proxy_port INTEGER;

    UPDATE system_config SET value = '4' WHERE key = 'schema_version';
    """,

    # ── v5 — persistent activity stats ────────────────────────────────────────
    #
    # turns: one row per voice turn (wake/button/barge-in/continuation),
    # written at turn completion from the TurnTrace in em_esphome. Survives
    # controller and device restarts — the in-memory Device.turn_history is
    # hydrated from here on connect. trigger_type not "trigger": TRIGGER is
    # an SQLite keyword. underruns/playback_periods arrive asynchronously
    # (device playback_stats message, firmware >= v2.9) and are NULL until
    # then — NULL means "not reported", 0 means "clean playback".
    #
    # wake_counters: hourly per-device rollups for signals too frequent to
    # store per-event — near-misses (wake score > 0.05 but below threshold)
    # and underruns from non-turn playback (announcements). One UPSERT at
    # most every ~2s per device (piggybacks the existing rate-limited
    # near-miss log path), so the instrumentation cost is negligible.
    #
    # device_metrics: hourly rollup of the device's ~30s hardware stats
    # report (CPU, RAM, storage, WiFi RSSI) for historic trend review.
    # Sums + extremes are upserted in place per report (averages computed
    # at read time), so an hour is one row however many samples land in it.
    # cpu_max keeps short spikes visible through the hourly average;
    # rssi_min keeps marginal-WiFi dips visible the same way.
    """
    CREATE TABLE IF NOT EXISTS turns (
        id               INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id        TEXT    NOT NULL,
        ts               REAL    NOT NULL,
        trigger_type     TEXT,
        wake_model       TEXT,
        wake_score       REAL,
        wake_threshold   REAL,
        noise_floor      REAL,
        outcome          TEXT,
        stt_text         TEXT,
        total_ms         INTEGER,
        vad_end_ms       INTEGER,
        stt_ms           INTEGER,
        tts_url_ms       INTEGER,
        tts_fetch_ms     INTEGER,
        playback_ms      INTEGER,
        audio_ms         INTEGER,
        tts_bytes        INTEGER,
        underruns        INTEGER,
        playback_periods INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_turns_device_ts ON turns(device_id, ts DESC);

    CREATE TABLE IF NOT EXISTS wake_counters (
        device_id     TEXT    NOT NULL,
        hour_ts       INTEGER NOT NULL,
        near_misses   INTEGER NOT NULL DEFAULT 0,
        near_miss_max REAL    NOT NULL DEFAULT 0,
        underruns     INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (device_id, hour_ts)
    );

    CREATE TABLE IF NOT EXISTS device_metrics (
        device_id        TEXT    NOT NULL,
        hour_ts          INTEGER NOT NULL,
        samples          INTEGER NOT NULL DEFAULT 0,
        cpu_sum          REAL    NOT NULL DEFAULT 0,
        cpu_max          REAL    NOT NULL DEFAULT 0,
        mem_used_sum     REAL    NOT NULL DEFAULT 0,
        mem_total_mb     REAL,
        storage_used_mb  REAL,
        storage_total_mb REAL,
        rssi_sum         REAL    NOT NULL DEFAULT 0,
        rssi_samples     INTEGER NOT NULL DEFAULT 0,
        rssi_min         REAL,
        PRIMARY KEY (device_id, hour_ts)
    );

    UPDATE system_config SET value = '5' WHERE key = 'schema_version';
    """,

    # ── v6 — device-link auth token (TLS rollout) ─────────────────────────────
    #
    # token: shared secret the device presents in the X-EM-Token header on
    # all three WebSocket planes (/control, /data, /shell). Minted by
    # ensure_device_token() when credentials are first pushed (provisioning
    # wizard or the dashboard "Secure link" action) and stored on the device
    # at /data/local/etc/echomuse/token. NULL = no credentials issued yet —
    # such devices connect unauthenticated (legacy posture) until
    # REQUIRE_DEVICE_TLS=1 flips the controller to enforcing.
    """
    ALTER TABLE devices ADD COLUMN token TEXT;

    UPDATE system_config SET value = '6' WHERE key = 'schema_version';
    """,

    # ── v7 — delivery instrumentation (playback stall diagnosis) ─────────────
    #
    # Added 2026-07-20 after playback underruns appeared with no identifiable
    # cause: every metric available at the time measured the wrong thing (the
    # controller's "Streaming took Xs" times a socket write, which completes
    # instantly regardless of how slowly the device is actually fed).
    #
    # Device-reported per stream (firmware >= v2.9.6, NULL on older):
    #   min_depth      fewest periods left in the device buffer mid-stream.
    #                  The margin metric — 0 means it starved, 2 means it
    #                  nearly did. Underruns are rare and binary; this is
    #                  continuous and shows degradation before it is audible.
    #   prime_wait_ms  first frame arriving -> first frame played.
    #   recv_span_ms   first -> last frame arrival. Compare against audio
    #                  duration: longer means delivery was slower than
    #                  realtime, i.e. the wire could not keep up.
    #   max_gap_ms     worst single inter-arrival gap (brief stall vs
    #                  uniformly slow link).
    #   bytes_recv     wire bytes the device actually received.
    #
    # Controller-measured:
    #   send_ms        time to write every frame into the socket. Kept for
    #                  contrast with delivery_ms — the gap between them is
    #                  exactly the illusion that misled the 07-20 analysis.
    #   delivery_ms    first frame sent -> device's playback_stats arrival.
    #                  The true end-to-end delivery window.
    #   eq_ms          EQ compute time (executor), to rule the controller in
    #                  or out as the source of a late start.
    """
    ALTER TABLE turns ADD COLUMN min_depth     INTEGER;
    ALTER TABLE turns ADD COLUMN prime_wait_ms INTEGER;
    ALTER TABLE turns ADD COLUMN recv_span_ms  INTEGER;
    ALTER TABLE turns ADD COLUMN max_gap_ms    INTEGER;
    ALTER TABLE turns ADD COLUMN bytes_recv    INTEGER;
    ALTER TABLE turns ADD COLUMN send_ms       INTEGER;
    ALTER TABLE turns ADD COLUMN delivery_ms   INTEGER;
    ALTER TABLE turns ADD COLUMN eq_ms         INTEGER;

    ALTER TABLE device_metrics ADD COLUMN link_speed_last  INTEGER;
    ALTER TABLE device_metrics ADD COLUMN link_speed_min   INTEGER;
    ALTER TABLE device_metrics ADD COLUMN wifi_freq_last   INTEGER;
    ALTER TABLE device_metrics ADD COLUMN wifi_bssid_last  TEXT;
    ALTER TABLE device_metrics ADD COLUMN tx_bytes_sum     INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN rx_bytes_sum     INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN tx_errors_sum    INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN tx_dropped_sum   INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN rx_crc_sum       INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '7' WHERE key = 'schema_version';
    """,

    # ── v8 — per-section fleet/device config scoping ─────────────────────────
    #
    # use_global_config was one boolean for the whole config, so overriding a
    # single setting on one device forked every other setting too and froze
    # them against future fleet changes. config_sections replaces it with the
    # set of sections a device overrides (ids from em_config_sections).
    #
    # The backfill is lossless in both directions:
    #   use_global_config = 1  ->  '[]'    (inherits everything, as before)
    #   use_global_config = 0  ->  all ids (overrides everything, as before)
    # so every device's effective config is byte-identical across the upgrade.
    #
    # use_global_config is KEPT rather than dropped: SQLite cannot drop a
    # column without a table rebuild, and it stays useful as the compat view
    # (no sections overridden == following the fleet), which older API
    # consumers and the migration itself both rely on.
    """
    ALTER TABLE devices ADD COLUMN config_sections TEXT NOT NULL DEFAULT '[]';

    UPDATE devices
       SET config_sections = '["playback","wakeword","microphones","ring","advanced","bluetooth"]'
     WHERE use_global_config = 0;

    UPDATE system_config SET value = '8' WHERE key = 'schema_version';
    """,

    # ── v9 — control-plane RTT ───────────────────────────────────────────────
    #
    # The RF layer is opaque on this hardware: the MTK driver leaves
    # retry/discard/missed-beacon at zero in /proc/net/wireless and reports
    # NOISE=9999, so tx_errors/tx_dropped/rx_crc (added in v7) are
    # STRUCTURALLY zero here and prove nothing either way. RTT measures the
    # latency that actually degrades the experience, needs no driver support,
    # and separates the hypotheses: contention makes latency track load,
    # power-save makes it spike when idle. Hence excursions are split by
    # whether the device was busy when the probe went out.
    """
    ALTER TABLE device_metrics ADD COLUMN rtt_sum_ms          INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN rtt_samples         INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN rtt_min_ms          INTEGER;
    ALTER TABLE device_metrics ADD COLUMN rtt_max_ms          INTEGER;
    ALTER TABLE device_metrics ADD COLUMN rtt_excursions      INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN rtt_excursions_idle INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '9' WHERE key = 'schema_version';
    """,

    # ── v10 — RTT idle-sample denominator ───────────────────────────────────
    #
    # Added immediately after v9 because v9 shipped without it and the
    # excursion split is meaningless without a denominator: "every excursion
    # happened while idle" is vacuous when almost every SAMPLE is idle.
    #
    # This is its own migration rather than an edit to v9 for the reason
    # stated at the top of this list — migrations are APPEND-ONLY. Editing v9
    # after a DB had already reached schema_version 9 meant the column was
    # never created, and every stats report then failed with "no column named
    # rtt_samples_idle", disconnecting all three devices in a loop
    # (2026-07-25, caught within a minute by the post-deploy error check).
    """
    ALTER TABLE device_metrics ADD COLUMN rtt_samples_idle INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '10' WHERE key = 'schema_version';
    """,

    # ── v11 — prune configs to match their scoping ──────────────────────────
    #
    # v8 backfilled config_sections but left the config column alone, so a
    # device that had been fully inheriting still stored a value for every
    # key. Harmless to the DEVICE (get_effective_device_config only reads
    # in-scope keys) but not to the dashboard, which merged the stored dict
    # over the fleet config and so displayed a device's stale settings while
    # every section claimed to be following the fleet — Office showed
    # hey_rhasspy/standard while actually running hey_mycroft/malevolent.
    #
    # The display bug is fixed properly in dashboard.jsx (filter, don't
    # merge). This makes the stored state honest as well, so the invariant
    # asserted in set_device_config_sections holds for migrated rows too.
    # Marked as a no-op SQL statement; the real work runs in Python below,
    # because pruning needs the section map.
    """
    UPDATE system_config SET value = '11' WHERE key = 'schema_version';
    """,

    # ── v12 — utterance recordings ──────────────────────────────────────────
    #
    # Filename (not a blob) of the saved mic audio for this turn. The WAV
    # itself lives in recordings/ beside this DB and is retained by file
    # count per device (em_recordings.KEEP_PER_DEVICE), which is a much
    # shorter window than TURN_RETENTION — so a non-NULL audio_file on an
    # older row is a claim to CHECK, not to trust. Every reader resolves
    # through em_recordings and treats a missing file as "no recording".
    """
    ALTER TABLE turns ADD COLUMN audio_file TEXT;

    UPDATE system_config SET value = '12' WHERE key = 'schema_version';
    """,

    # ── v13 — on-device wake word shadow mode ───────────────────────────────
    #
    # Two levels, because they answer different questions.
    #
    # Per turn (dev_wake_*): when the CONTROLLER woke, did the device agree,
    # and how far apart were they? This is the comparison that decides whether
    # on-device detection is good enough to trust. NULL means one of three
    # things and they are not the same: shadow mode was off, the device's
    # firmware predates it, or the device genuinely did not cross the threshold
    # for this utterance. The third is a finding; the first two are absence of
    # data. dev_shadow distinguishes them — 1 when the device was known to be
    # scoring at the time, so a NULL score alongside it is a real miss.
    #
    # Per hour (wake_counters.dev_*): what did the device see when the
    # controller did NOT wake? Crossings with no corresponding turn are the
    # false-accept side of the comparison, which per-turn rows structurally
    # cannot show. dev_frames is the denominator that makes the rest legible,
    # and dev_drops says whether the device kept up at all — a shadow run that
    # dropped half its frames is not evidence about detection quality.
    """
    ALTER TABLE turns ADD COLUMN dev_wake_score REAL;
    ALTER TABLE turns ADD COLUMN dev_wake_delta_ms INTEGER;
    ALTER TABLE turns ADD COLUMN dev_shadow INTEGER;

    ALTER TABLE wake_counters ADD COLUMN dev_frames    INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE wake_counters ADD COLUMN dev_drops     INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE wake_counters ADD COLUMN dev_crossings INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE wake_counters ADD COLUMN dev_max_score REAL    NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '13' WHERE key = 'schema_version';
    """,

    # ── v14 — thermals and CPU topology ─────────────────────────────────────
    #
    # cpu_temp_* / max_temp_max: this SoC has 11 thermal zones; mtktscpu is the
    # CPU and the max-of-all catches a PMIC or board sensor running hotter than
    # the CPU, which is where trouble shows up first.
    #
    # cores_online_* exists because cpu_pct is NOT self-describing: it comes
    # from the aggregate /proc/stat line, so it is a share of ONLINE capacity,
    # and MTK parks 3 of 4 cores when idle. The same absolute work reads as half
    # the percentage once a second core comes up — so a cpu_pct series without
    # the core count beside it can show a "drop" that is purely a change of
    # divisor. cores_online_min is the interesting end: it is the tightest the
    # device ever was.
    #
    # thermal_limit_min < cores_total means the thermal governor capped capacity
    # at some point in the hour, which is the throttling signal that matters
    # more than any single temperature.
    """
    ALTER TABLE device_metrics ADD COLUMN cpu_temp_sum     REAL    NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN cpu_temp_samples INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE device_metrics ADD COLUMN cpu_temp_max     REAL;
    ALTER TABLE device_metrics ADD COLUMN max_temp_max     REAL;
    ALTER TABLE device_metrics ADD COLUMN cores_online_last INTEGER;
    ALTER TABLE device_metrics ADD COLUMN cores_online_min  INTEGER;
    ALTER TABLE device_metrics ADD COLUMN cores_total       INTEGER;
    ALTER TABLE device_metrics ADD COLUMN thermal_limit_min INTEGER;

    UPDATE system_config SET value = '14' WHERE key = 'schema_version';
    """,

    # ── v15 — the device's own wake threshold, per turn ─────────────────────
    #
    # Needed to tell a real on-device miss from a comparison that was never
    # valid. The controller lowers its wake bar to bargeInThreshold while the
    # speaker is playing (echo at the mic is ~25dB louder than the person), so
    # a turn that fired at 0.055 was not something a device scoring against
    # 0.5 could have caught. Before this, those were counted as misses and the
    # agreement figure was pessimistic.
    #
    # NULL means the device did not report a threshold (firmware predating it),
    # in which case the turn is counted as neither agreed nor missed rather
    # than guessed at.
    """
    ALTER TABLE turns ADD COLUMN dev_threshold REAL;

    UPDATE system_config SET value = '15' WHERE key = 'schema_version';
    """,

    # ── v16 — what the shadow scorer's stalls actually were ─────────────────
    #
    # dev_drops says the scorer's 8-frame (640ms) queue overflowed. It does
    # not say WHY, and three explanations have already been falsified by
    # measurement (turn bursts, core hotplug, controller redeploys), so the
    # counter on its own has stopped being able to answer the question.
    #
    # A drop has exactly two causes and they call for opposite fixes:
    # dev_max_infer_ms is the slowest single inference in the window (the
    # CONSUMER stalling), dev_max_gap_ms the longest gap between frames
    # arriving (the PRODUCER bursting — the mic pipeline handing over its
    # 160ms batches late and in clumps).
    #
    # Maxima rather than averages, because a stall IS the tail: a 700ms event
    # averaged over a 30s window of 375 normal frames disappears entirely.
    # Default 0 reads as "not reported" for firmware predating this, which is
    # honest here — unlike a measurement, an absent maximum cannot be mistaken
    # for a good one, since 0 is also what a perfectly healthy window reports.
    """
    ALTER TABLE wake_counters ADD COLUMN dev_max_infer_ms INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE wake_counters ADD COLUMN dev_max_gap_ms   INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '16' WHERE key = 'schema_version';
    """,

    # ── v17 — the comparison, from the other side ───────────────────────────
    #
    # With owwOnDevice="on" the DEVICE triggers the turn, so dev_wake_score is
    # no longer the side that can be missing — this controller's is. These
    # record whether it agreed, matched the same way and against the same
    # clock, so the agreement figure that justified shipping on-device wake
    # keeps being answerable once the roles are swapped.
    #
    # Without them, turning a device "on" would silently end the measurement:
    # every turn would show a device score and nothing to compare it against,
    # which reads as perfect agreement rather than as no data.
    #
    # NULL means the controller did not detect this utterance within the match
    # window — a controller miss, which is the interesting direction and the
    # one that argues on-device wake is worth having. NULL on a
    # controller-triggered turn just means the column does not apply.
    """
    ALTER TABLE turns ADD COLUMN ctrl_wake_score REAL;
    ALTER TABLE turns ADD COLUMN ctrl_wake_delta_ms INTEGER;

    UPDATE system_config SET value = '17' WHERE key = 'schema_version';
    """,

    # ── v18 — wake-word sample collection mode ──────────────────────────────
    #
    # A device in collect mode streams its wake audio to disk as training
    # clips (em_samples) and starts no voice turns at all. A COLUMN rather
    # than a config key, deliberately: config is section-scoped and
    # fleet-inherited by default, so a key here would let one toggle in the
    # fleet panel silence every Echo in the house at once. This is a mode
    # someone puts ONE device into for an afternoon.
    #
    # Persisted rather than held in memory because the mode outlives the
    # process that was asked for it: someone walks the house saying the wake
    # word for twenty minutes, and a controller restart in the middle must
    # not silently turn collection off — it would look like it was still
    # running and record nothing.
    """
    ALTER TABLE devices ADD COLUMN collect_mode INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '18' WHERE key = 'schema_version';
    """,

    # ── v19 — ambient recording mode ────────────────────────────────────────
    #
    # The other half of a training set: collect mode captures the wake word
    # and cuts at the silences, this one holds the mic open and hands back a
    # single WAV of the whole session (em_ambient) — room noise, which has no
    # onsets to cut on and is worthless once it has been chopped up.
    #
    # A column for both of v18's reasons, which apply unchanged: it is a
    # per-device mode rather than fleet-inherited config, and it outlives the
    # process. The restart case is stronger here, not weaker — someone leaves
    # a room recording for an hour, and a controller that came back with the
    # mode silently off would show a device that is recording nothing while
    # its ring says otherwise. On reconnect the mode is re-armed and a NEW
    # file is started; the interrupted one is recovered from its `.part` by
    # em_ambient.recover rather than lost.
    """
    ALTER TABLE devices ADD COLUMN ambient_mode INTEGER NOT NULL DEFAULT 0;

    UPDATE system_config SET value = '19' WHERE key = 'schema_version';
    """,

    # ── v20 — wake clips ────────────────────────────────────────────────────
    #
    # The audio that crossed the wake threshold, kept per turn (em_wakeclips)
    # so a false positive can be listened to, and collected as BCResNet
    # training material (trained outside this repo, in ~/git/bcresnet).
    # Neither the turn row nor its utterance recording contains the sound
    # that triggered the wake.
    #
    # A COLUMN on turns rather than a device column, unlike v18/v19: this is
    # not a mode, it is an artefact of one turn, and the row that shows the
    # false positive is the row that should link to its own audio. Retention
    # is by file count per device (em_wakeclips.KEEP_PER_DEVICE), a shorter
    # window than TURN_RETENTION, so a non-NULL wake_file on an older row is
    # a claim to CHECK and every reader resolves it through em_wakeclips —
    # exactly as v12's audio_file is treated.
    """
    ALTER TABLE turns ADD COLUMN wake_file TEXT;

    UPDATE system_config SET value = '20' WHERE key = 'schema_version';
    """,

    # ── v21 — timer rings use the ordinary barge-in path ────────────────────
    #
    # ringBargeIn used to decide whether the wake detector ran while a timer
    # chime was audible. That made the default alarm impossible to stop by
    # voice except during a configured quiet gap. Ring audio is playback and
    # now always uses bargeInThreshold, so the separate switch has no valid
    # meaning. Remove it from fleet and per-device JSON rather than leaving a
    # dead setting that APIs keep round-tripping forever.
    """
    UPDATE system_config SET value = '21' WHERE key = 'schema_version';
    """,

    # ── v22 — post-AFE cutover (SPEC §18.4) ─────────────────────────────────
    #
    # Decision-trace columns on turns (§11.3), the device-reported hop count
    # in wake_counters, and the alert journal/delivery tables (§16.3). The
    # shadow/device-wake turn columns keep their history and are no longer
    # written. Config rewrites and the alert-sound export run in _fixup_v22,
    # inside this migration's transaction.
    """
    ALTER TABLE turns ADD COLUMN wake_model_sha256 TEXT;
    ALTER TABLE turns ADD COLUMN policy_hash TEXT;
    ALTER TABLE turns ADD COLUMN wake_attribution TEXT;
    ALTER TABLE turns ADD COLUMN reference_coverage REAL;
    ALTER TABLE turns ADD COLUMN commit_route TEXT;
    ALTER TABLE turns ADD COLUMN terminal_reason TEXT;
    ALTER TABLE turns ADD COLUMN commit_id TEXT;

    ALTER TABLE wake_counters ADD COLUMN dev_hops INTEGER NOT NULL DEFAULT 0;
    """
    + ALERT_SCHEMA_SQL
    + """
    UPDATE system_config SET value = '22' WHERE key = 'schema_version';
    """,

    # ── v23 — drop microphone-processing config keys ────────────────────────
    #
    # Nine keys the native AFE made meaningless were still stored in fleet
    # and device config. The API rejects keys it does not know, so every
    # dashboard save that round-tripped them failed. _fixup_v23 deletes them.
    """
    UPDATE system_config SET value = '23' WHERE key = 'schema_version';
    """,

    # ── v24 — per-stage turn detail for the Activity page ────────────────────
    #
    # What the controller's streaming ASR heard (the text that ended the
    # utterance), how the endpoint decided (grammar class, silence waited),
    # HA's transcript before wake-phrase removal, and the intent result: the
    # spoken answer, HA's response type, whether HA's built-in agent answered,
    # and the intent time. stt_text keeps meaning "the text that was routed".
    """
    ALTER TABLE turns ADD COLUMN asr_text TEXT;
    ALTER TABLE turns ADD COLUMN endpoint_class TEXT;
    ALTER TABLE turns ADD COLUMN endpoint_ms INTEGER;
    ALTER TABLE turns ADD COLUMN stt_raw TEXT;
    ALTER TABLE turns ADD COLUMN intent_ms INTEGER;
    ALTER TABLE turns ADD COLUMN intent_local INTEGER;
    ALTER TABLE turns ADD COLUMN response_type TEXT;
    ALTER TABLE turns ADD COLUMN response_text TEXT;
    UPDATE system_config SET value = '24' WHERE key = 'schema_version';
    """,

    # ── v25 — follow-up questions on the Activity page ───────────────────────
    #
    # A turn whose answer asked a question (HA continue_conversation, or an
    # EchoMuse clarification) records what became of it in `continuation`:
    # `pending` with the row, then one final value written when the reply
    # window resolves. `playback_reason` is how the spoken answer ended
    # (drained, cancelled, ...), `turn_uuid` the actor's turn id that trace
    # logs name, and `reply_to` the turn_uuid of the question a turn answered.
    """
    ALTER TABLE turns ADD COLUMN turn_uuid TEXT;
    ALTER TABLE turns ADD COLUMN conversation_id TEXT;
    ALTER TABLE turns ADD COLUMN reply_to TEXT;
    ALTER TABLE turns ADD COLUMN continuation TEXT;
    ALTER TABLE turns ADD COLUMN playback_reason TEXT;
    UPDATE system_config SET value = '25' WHERE key = 'schema_version';
    """,

    # ── v26 — decision trace and time-to-first-audio on the turn row ─────────
    #
    # `decision_trace` is the §11.3 trace JSON the actor logs with every turn,
    # kept for the newest TRACE_RETENTION rows per device (older rows have it
    # NULLed on insert) and read by sqlite analysis, never by get_turns.
    # `first_audio_ms` is the endpoint commit → the response becoming audible
    # (NULL: nothing became audible).
    """
    ALTER TABLE turns ADD COLUMN first_audio_ms INTEGER;
    ALTER TABLE turns ADD COLUMN decision_trace TEXT;
    UPDATE system_config SET value = '26' WHERE key = 'schema_version';
    """,

    # ── v27 — wake shadow rules (SPEC §5.2) ──────────────────────────────────
    #
    # An `open_rules_v1` device evaluates shadow open rules it never acts on
    # and reports, per rule and 30 s window, how each would have compared
    # with the live rules (wake.stats `shadow`). wake_shadow adds those
    # counters per device-hour and rule (rule_key = OpenRule.key); lead_hist
    # is a JSON array of 7 counts (lead −3..+3 hops). wake_shadow_events keeps
    # the newest WAKE_SHADOW_EVENT_RETENTION unmatched/retried episodes per
    # device; raws is a JSON array. No rows are written for firmware without
    # the report, so an absent device reads as no data, never zeros.
    """
    CREATE TABLE IF NOT EXISTS wake_shadow (
        device_id      TEXT    NOT NULL,
        hour_ts        INTEGER NOT NULL,
        rule_key       TEXT    NOT NULL,
        hops           INTEGER NOT NULL DEFAULT 0,
        opens          INTEGER NOT NULL DEFAULT 0,
        matched        INTEGER NOT NULL DEFAULT 0,
        lead_hist      TEXT    NOT NULL DEFAULT '[0,0,0,0,0,0,0]',
        unmatched      INTEGER NOT NULL DEFAULT 0,
        retried        INTEGER NOT NULL DEFAULT 0,
        live_only      INTEGER NOT NULL DEFAULT 0,
        events_dropped INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (device_id, hour_ts, rule_key)
    );
    CREATE TABLE IF NOT EXISTS wake_shadow_events (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id TEXT    NOT NULL,
        ts        REAL    NOT NULL,
        rule_key  TEXT    NOT NULL,
        kind      TEXT    NOT NULL,
        peak_raw  REAL    NOT NULL,
        raws      TEXT    NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_wake_shadow_events_device_ts
        ON wake_shadow_events (device_id, ts DESC);
    UPDATE system_config SET value = '27' WHERE key = 'schema_version';
    """,
]

# Post-migration fixups that need Python rather than SQL. Keyed by the schema
# version they belong to; run once, immediately after that migration applies.
def _fixup_v11(conn: sqlite3.Connection) -> None:
    rows = conn.execute("SELECT device_id, config, config_sections FROM devices").fetchall()
    for row in rows:
        try:
            cfg  = json.loads(row["config"] or "{}") or {}
            secs = em_config_sections.normalise(json.loads(row["config_sections"] or "[]"))
        except (json.JSONDecodeError, TypeError):
            continue
        keep = em_config_sections.keys_for(secs) | em_config_sections.STATE_KEYS
        pruned = {k: v for k, v in cfg.items() if k in keep}
        if len(pruned) != len(cfg):
            conn.execute(
                "UPDATE devices SET config = ? WHERE device_id = ?",
                (json.dumps(pruned), row["device_id"]),
            )
            log.info(
                f"[db] v11: pruned {len(cfg) - len(pruned)} out-of-scope config "
                f"key(s) from {row['device_id']}"
            )


def _fixup_v21(conn: sqlite3.Connection) -> None:
    removed = "ringBargeIn"
    changed = 0

    row = conn.execute(
        "SELECT value FROM system_config WHERE key = 'global_device_config'"
    ).fetchone()
    if row is not None:
        try:
            cfg = json.loads(row["value"] or "{}") or {}
        except (json.JSONDecodeError, TypeError):
            cfg = None
        if cfg is not None and removed in cfg:
            cfg.pop(removed)
            conn.execute(
                "UPDATE system_config SET value = ? "
                "WHERE key = 'global_device_config'",
                (json.dumps(cfg),),
            )
            changed += 1

    for row in conn.execute("SELECT device_id, config FROM devices").fetchall():
        try:
            cfg = json.loads(row["config"] or "{}") or {}
        except (json.JSONDecodeError, TypeError):
            continue
        if removed not in cfg:
            continue
        cfg.pop(removed)
        conn.execute(
            "UPDATE devices SET config = ? WHERE device_id = ?",
            (json.dumps(cfg), row["device_id"]),
        )
        changed += 1

    if changed:
        log.info(f"[db] v21: removed ringBargeIn from {changed} config scope(s)")


# Values schema 21 fell back to when a scope did not store the key; migration
# 22 reads the OLD effective value, so it must not see today's defaults.
_V21_MAX_SPEECH_MS = 12_000
_V21_TIMER_RING_SECONDS = 60
# §18.4: an old cap of 0 (none) or above this becomes extendedUtterances=true.
_EXTENDED_ABOVE_MS = 15_000


def _json_dict(text: Optional[str]) -> Optional[DeviceConfig]:
    """Parsed JSON object, {} for empty, None for anything unparseable."""
    try:
        value = json.loads(text or "{}") or {}
    except (json.JSONDecodeError, TypeError):
        return None
    return value if isinstance(value, dict) else None


def _extended_utterances(max_speech_ms: object) -> bool:
    try:
        ms = int(max_speech_ms) if isinstance(max_speech_ms, (int, float, str)) else _V21_MAX_SPEECH_MS
    except ValueError:
        ms = _V21_MAX_SPEECH_MS
    return ms == 0 or ms > _EXTENDED_ABOVE_MS


def _rewrite_scope_v22(cfg: DeviceConfig) -> DeviceConfig:
    """One scope's stored config without the v22 removed keys, and with the old
    default ring limit moved to the new default (§18.4 steps 1–2)."""
    out = {k: v for k, v in cfg.items() if k not in _V22_REMOVED_KEYS}
    if out.get("timerRingSeconds") == _V21_TIMER_RING_SECONDS:
        out["timerRingSeconds"] = DEFAULT_DEVICE_CONFIG["timerRingSeconds"]
    return out


def _fixup_v22(conn: sqlite3.Connection) -> None:
    """
    SPEC §18.4 config rewrite and alert-sound export.

    Fleet: removed keys deleted, `wakeModel` = the deployed graph,
    `extendedUtterances` from the old effective `maxSpeechMs`, `alarmSound`
    initialised from `timerSound`. Devices: removed keys deleted; a device
    that overrides the microphones section gets `extendedUtterances` from its
    own old effective cap. Then every effective timerSound/alarmSound ID plus
    `default` is exported as an alert asset before the transaction commits;
    a missing or undecodable ID stays stored and resolves to the fallback.
    """
    row = conn.execute(
        "SELECT value FROM system_config WHERE key = 'global_device_config'"
    ).fetchone()
    fleet = _json_dict(row["value"] if row is not None else None)
    if fleet is None:
        log.warning("[db] v22: fleet config JSON unreadable — rewriting from defaults")
        fleet = {}
    fleet_max_speech = fleet.get("maxSpeechMs", _V21_MAX_SPEECH_MS)

    new_fleet = _rewrite_scope_v22(fleet)
    new_fleet["wakeModel"] = DEPLOYED_WAKE_MODEL
    new_fleet["extendedUtterances"] = _extended_utterances(fleet_max_speech)
    new_fleet["alarmSound"] = fleet.get("timerSound", DEFAULT_DEVICE_CONFIG["timerSound"])
    conn.execute(
        "INSERT OR REPLACE INTO system_config (key, value) "
        "VALUES ('global_device_config', ?)",
        (json.dumps(new_fleet),),
    )
    fleet_effective = {**DEFAULT_DEVICE_CONFIG, **new_fleet}

    sound_ids = {em_sounds.DEFAULT_ID}
    for row in conn.execute(
        "SELECT device_id, config, config_sections FROM devices"
    ).fetchall():
        cfg = _json_dict(row["config"])
        if cfg is None:
            log.warning(f"[db] v22: config JSON unreadable for {row['device_id']} — left as is")
            continue
        try:
            sections = em_config_sections.normalise(json.loads(row["config_sections"] or "[]"))
        except (json.JSONDecodeError, TypeError):
            sections = []
        new = _rewrite_scope_v22(cfg)
        if "microphones" in sections:
            new["extendedUtterances"] = _extended_utterances(
                cfg.get("maxSpeechMs", fleet_max_speech))
        if new != cfg:
            conn.execute(
                "UPDATE devices SET config = ? WHERE device_id = ?",
                (json.dumps(new), row["device_id"]),
            )
        effective = em_config_sections.merge(fleet_effective, new, sections)
        sound_ids.update((effective["timerSound"], effective["alarmSound"]))
    sound_ids.update((fleet_effective["timerSound"], fleet_effective["alarmSound"]))
    sound_ids.discard("")

    directory = em_sounds.sounds_dir(_db_path)
    for sound_id in sorted(sound_ids):
        try:
            exported = em_sounds.export(sound_id, directory)
        except em_sounds.ExportError as e:
            if sound_id != em_sounds.DEFAULT_ID or em_sounds.source_path(sound_id, directory):
                log.warning(f"[db] v22: sound {sound_id!r} not exported ({e}) — "
                            f"rings with {em_sounds.FALLBACK}")
            continue
        if exported.shortened:
            log.warning(f"[db] v22: sound {sound_id!r} is longer than "
                        f"{em_sounds.ALERT_MAX_SECONDS} s — its alert asset is cut")


def _fixup_v23(conn: sqlite3.Connection) -> None:
    """Delete _V23_REMOVED_KEYS from the fleet config and every device config.
    Unreadable JSON is left as is: there is nothing to strip safely."""
    row = conn.execute(
        "SELECT value FROM system_config WHERE key = 'global_device_config'"
    ).fetchone()
    fleet = _json_dict(row["value"] if row is not None else None)
    if fleet is not None and _V23_REMOVED_KEYS & fleet.keys():
        conn.execute(
            "UPDATE system_config SET value = ? WHERE key = 'global_device_config'",
            (json.dumps({k: v for k, v in fleet.items() if k not in _V23_REMOVED_KEYS}),),
        )
    for row in conn.execute("SELECT device_id, config FROM devices").fetchall():
        cfg = _json_dict(row["config"])
        if cfg is not None and _V23_REMOVED_KEYS & cfg.keys():
            conn.execute(
                "UPDATE devices SET config = ? WHERE device_id = ?",
                (json.dumps({k: v for k, v in cfg.items() if k not in _V23_REMOVED_KEYS}),
                 row["device_id"]),
            )


_MIGRATION_FIXUPS: dict[int, Callable[[sqlite3.Connection], None]] = {11: _fixup_v11, 21: _fixup_v21, 22: _fixup_v22, 23: _fixup_v23}

# ─── Connection management ────────────────────────────────────────────────────

_db_path: str = ""
_conn: Optional[sqlite3.Connection] = None

# ONE sqlite3.Connection shared across the run_in_executor pool
# (check_same_thread=False). Two concurrent operations on one connection
# object are a misuse SQLite rejects (SQLITE_MISUSE), so every access — reads
# (_q/_q1) and write transactions (_tx) — serialises through this lock.
_db_lock = threading.Lock()

# em_alerts.AlertEngine's journal runs on its own connection to the same file
# (connect_alerts), serialised by its own lock; WAL lets the two connections
# read concurrently and SQLite's busy timeout orders their writes.
alerts_lock = threading.Lock()


def connect_alerts() -> sqlite3.Connection:
    """A second connection to the initialised database for AlertEngine
    (`AlertEngine(conn, ..., db_lock=alerts_lock)`). Call after init()."""
    assert _db_path, "db.init() has not been called"
    conn = sqlite3.connect(_db_path, check_same_thread=False, timeout=10.0)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys=ON")
    return conn


def init(path: str = "echomuse.db") -> None:
    """
    Initialise the database. Must be called once at startup before any
    other db function is used.

    Opens a single persistent connection, enables WAL mode and foreign
    key enforcement, then runs any pending migrations.
    """
    global _db_path, _conn
    _db_path = path
    log.info(f"Opening database: {os.path.abspath(path)}")
    _conn = sqlite3.connect(path, check_same_thread=False)
    _conn.row_factory = sqlite3.Row
    _conn.execute("PRAGMA journal_mode=WAL")
    _conn.execute("PRAGMA foreign_keys=ON")
    _conn.commit()
    _migrate(_conn)
    log.info("Database ready")


@contextmanager
def _tx() -> Iterator[sqlite3.Connection]:
    """
    Context manager for a write transaction.

    Commits on clean exit, rolls back on any exception and re-raises.
    Holds _db_lock for the whole transaction so no read or other write can
    touch the shared connection mid-transaction (see _db_lock).
    """
    assert _conn is not None, "db.init() has not been called"
    with _db_lock:
        try:
            yield _conn
            _conn.commit()
        except Exception:
            _conn.rollback()
            raise


def _q(sql: str, params: Sequence[object] = ()) -> list[sqlite3.Row]:
    """Execute a read query and return all rows."""
    assert _conn is not None, "db.init() has not been called"
    with _db_lock:
        return _conn.execute(sql, params).fetchall()


def _q1(sql: str, params: Sequence[object] = ()) -> Optional[sqlite3.Row]:
    """Execute a read query and return at most one row."""
    assert _conn is not None, "db.init() has not been called"
    with _db_lock:
        return _conn.execute(sql, params).fetchone()


# ─── Migrations ───────────────────────────────────────────────────────────────

def _backup_before_migrating(conn: sqlite3.Connection, current: int) -> None:
    """
    Copy the database before any migration touches it.

    Migrations run in place and every one so far is additive, which is about
    as safe as schema change gets — but a controller can be several versions
    behind (the version is just an index into an append-only list, so a v11
    database migrates to v16 in one startup), and the first migration that
    rewrites or drops data has no undo without this.

    Uses sqlite3's own backup API rather than copying the file: it is
    consistent against an open connection and correct under WAL, where the
    .db file alone is not the whole database.

    Named for the version being LEFT, not the time — a retry after a failed
    migration starts from the same state, so it should overwrite rather than
    accumulate a backup per attempt. One file per starting version is enough
    to get back to where you were, and cannot fill a disk on its own.

    A backup that cannot be written is a refusal to migrate, not a warning.
    Disk-full is precisely when you want the schema left alone, and a warning
    logged at startup is a warning nobody reads until afterwards. Set
    EM_SKIP_DB_BACKUP=1 to proceed anyway.
    """
    if current == 0:
        return  # fresh database — nothing to preserve

    if os.environ.get("EM_SKIP_DB_BACKUP") == "1":
        log.warning("[db] EM_SKIP_DB_BACKUP=1 — migrating without a backup")
        return

    dest = f"{_db_path}.pre-v{current}.bak"
    try:
        with sqlite3.connect(dest) as bck:
            conn.backup(bck)
        size = os.path.getsize(dest)
        log.info(f"[db] Backed up v{current} schema to {dest} ({size/1024:.0f}KB)")
    except Exception as e:
        raise RuntimeError(
            f"Could not back up the database before migrating from v{current} "
            f"({e}). Refusing to migrate: free some space or fix permissions "
            f"on {os.path.dirname(_db_path) or '.'}, or set EM_SKIP_DB_BACKUP=1 "
            f"to proceed without one."
        ) from e


def _migrate(conn: sqlite3.Connection) -> None:
    """Run any pending migrations in order."""
    # Determine current schema version. On a brand-new database the
    # system_config table doesn't exist yet, so we catch that case.
    try:
        row = conn.execute(
            "SELECT value FROM system_config WHERE key = ?", (SystemConfigKey.SCHEMA_VERSION,)
        ).fetchone()
        current = int(row[0]) if row else 0
    except sqlite3.OperationalError:
        current = 0  # fresh database — system_config doesn't exist yet

    # ── Downgrade guard ──────────────────────────────────────────────────────
    #
    # A database from a NEWER controller. MIGRATIONS[current:] is empty here,
    # so without this the old build starts silently and runs against a schema
    # it does not know. That mostly works — the newer schema is a superset —
    # but "mostly" is the problem: the failure surfaces as odd behaviour
    # somewhere else rather than as an error here, and it happens exactly when
    # someone has rolled an image back and is already troubleshooting.
    #
    # Refusing costs a clear message; proceeding costs an afternoon.
    if current > len(MIGRATIONS):
        raise RuntimeError(
            f"Database is at schema v{current} but this controller only knows "
            f"v{len(MIGRATIONS)} — it was created by a newer version. Use a "
            f"controller at least that new, or restore a backup taken before "
            f"the upgrade (see {os.path.basename(_db_path)}.pre-v*.bak beside "
            f"the database)."
        )

    pending = MIGRATIONS[current:]
    if not pending:
        log.debug(f"Schema is current at v{current}")
        return

    _backup_before_migrating(conn, current)

    log.info(f"Running {len(pending)} migration(s) from v{current}")
    for i, sql in enumerate(pending):
        version = current + i + 1
        log.info(f"Applying migration v{version}")
        try:
            # executescript() commits any open transaction and then runs in
            # autocommit mode, so the explicit BEGIN is what makes the DDL,
            # the schema_version bump and the Python fixup one transaction:
            # a failure anywhere rolls all three back and the next start
            # resumes from the last committed version.
            conn.executescript("BEGIN;\n" + sql)
            fixup = _MIGRATION_FIXUPS.get(version)
            if fixup is not None:
                fixup(conn)
            conn.commit()
        except Exception as e:
            conn.rollback()
            log.error(f"Migration v{version} failed: {e}")
            raise RuntimeError(f"Database migration v{version} failed — cannot start") from e

    new_version = current + len(pending)
    log.info(f"Schema migrated to v{new_version}")


# ─── Device registry ──────────────────────────────────────────────────────────

def get_device(device_id: str) -> Optional[DeviceRow]:
    """Return the device row for device_id, or None if not registered."""
    row = _q1("SELECT * FROM devices WHERE device_id = ?", (device_id,))
    return DeviceRow.from_row(row) if row is not None else None


def get_all_devices() -> list[DeviceRow]:
    """Return all device rows ordered by first_seen."""
    return [DeviceRow.from_row(r) for r in _q("SELECT * FROM devices ORDER BY first_seen ASC")]


def get_pending_devices() -> list[DeviceRow]:
    """Return devices that have connected but not yet been approved."""
    return [DeviceRow.from_row(r) for r in _q(
        "SELECT * FROM devices WHERE approved = 0 ORDER BY first_seen ASC"
    )]


def register_new_device(device_id: str, ip: str, version: Optional[str]) -> None:
    """
    Insert a new device row with approved=0 (pending).

    Called when an unknown device_id connects for the first time.
    Config is seeded from DEFAULT_DEVICE_CONFIG.
    """
    now = _now()
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO devices
                (device_id, label, approved, ip, firmware_ver, first_seen, last_seen, config)
            VALUES (?, NULL, 0, ?, ?, ?, ?, ?)
            """,
            (
                device_id,
                ip,
                version,
                now,
                now,
                json.dumps(DEFAULT_DEVICE_CONFIG),
            ),
        )
    log.info(f"[db] New device registered (pending): {device_id}")


def approve_device(device_id: str, label: str, config: Optional[Mapping[str, object]] = None) -> None:
    """
    Approve a pending device and optionally set label and config.

    If config is not supplied the existing seeded config is kept.
    Raises ValueError if the device is not found.
    """
    device = get_device(device_id)
    if device is None:
        raise ValueError(f"Device not found: {device_id}")

    effective_config = json.dumps(config) if config is not None else device.config

    with _tx() as conn:
        conn.execute(
            """
            UPDATE devices
            SET approved = 1,
                label    = ?,
                config   = ?
            WHERE device_id = ?
            """,
            (label, effective_config, device_id),
        )
    log.info(f"[db] Device approved: {device_id} label={label!r}")


def upsert_device_seen(
    device_id: str,
    ip: str,
    version: Optional[str],
) -> None:
    """
    Update ip, firmware_ver, and last_seen for a known device on each connection.

    Does not touch approval status, label, or config.
    """
    with _tx() as conn:
        conn.execute(
            """
            UPDATE devices
            SET ip           = ?,
                firmware_ver = ?,
                last_seen    = ?
            WHERE device_id = ?
            """,
            (ip, version, _now(), device_id),
        )


def touch_device_seen(device_id: str) -> None:
    """
    Refresh last_seen only — the device is alive right now.

    upsert_device_seen fires on connect, which made last_seen mean "last
    CONNECTED": a device online and healthy for hours still displayed a
    last_seen from whenever it last reconnected (observed 2026-07-25:
    all three devices reading 88-91 minutes stale while streaming audio
    fine). That is precisely backwards from what the field is for —
    knowing when a device went away.

    Called from the ~30s stats report, so last_seen is at most one report
    stale while connected and lands within ~30s of the truth when a device
    drops. It rides the same executor hop as record_device_stats, so it
    adds no write the loop wasn't already making.
    """
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET last_seen = ? WHERE device_id = ?",
            (_now(), device_id),
        )


def get_device_token(device_id: str) -> Optional[str]:
    """Return the device's link-auth token, or None if never issued."""
    row = _q1("SELECT token FROM devices WHERE device_id = ?", (device_id,))
    return row["token"] if row and row["token"] else None


def ensure_device_token(device_id: str) -> str:
    """
    Return the device's link-auth token, minting one if absent.

    Creates a pending device row first if the device has never connected —
    the provisioning wizard pushes credentials before first contact, so the
    row may not exist yet. Approval state is untouched either way.
    """
    import secrets

    existing = get_device_token(device_id)
    if existing:
        return existing

    token = secrets.token_urlsafe(32)
    now = _now()
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO devices
                (device_id, label, approved, ip, firmware_ver, first_seen, last_seen, config)
            VALUES (?, NULL, 0, NULL, NULL, ?, ?, ?)
            ON CONFLICT(device_id) DO NOTHING
            """,
            (device_id, now, now, json.dumps(DEFAULT_DEVICE_CONFIG)),
        )
        conn.execute(
            "UPDATE devices SET token = ? WHERE device_id = ?",
            (token, device_id),
        )
    log.info(f"[db] Link token minted for {device_id}")
    return token


def clear_device_token(device_id: str) -> None:
    """Revoke a device's link token (next credential push mints a new one)."""
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET token = NULL WHERE device_id = ?", (device_id,)
        )


def set_device_label(device_id: str, label: str) -> None:
    """Update the human-readable label for a device."""
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET label = ? WHERE device_id = ?",
            (label, device_id),
        )


def get_collect_mode(device_id: str) -> bool:
    """
    Whether this device is collecting wake-word training samples.

    A device in collect mode answers nothing — see the v18 migration for why
    this is a column rather than a config key.
    """
    row = _q1("SELECT collect_mode FROM devices WHERE device_id = ?", (device_id,))
    return bool(row["collect_mode"]) if row is not None else False


def set_collect_mode(device_id: str, enabled: bool) -> None:
    """Arm or disarm sample collection for a device."""
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET collect_mode = ? WHERE device_id = ?",
            (1 if enabled else 0, device_id),
        )


def get_ambient_mode(device_id: str) -> bool:
    """
    Whether this device is recording ambient room audio.

    Answers nothing while it is on, exactly like collect mode — see the v19
    migration for why this is a column too.
    """
    row = _q1("SELECT ambient_mode FROM devices WHERE device_id = ?", (device_id,))
    return bool(row["ambient_mode"]) if row is not None else False


def set_ambient_mode(device_id: str, enabled: bool) -> None:
    """Arm or disarm ambient recording for a device."""
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET ambient_mode = ? WHERE device_id = ?",
            (1 if enabled else 0, device_id),
        )


def set_device_config(device_id: str, config: Mapping[str, object]) -> None:
    """
    Persist updated config for a device.

    The caller is responsible for immediately pushing the config to the
    live device over the control WebSocket if it is currently connected.
    """
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET config = ? WHERE device_id = ?",
            (json.dumps(config), device_id),
        )


def get_device_config(device_id: str) -> DeviceConfig:
    """
    Return the config dict for a device.

    Falls back to DEFAULT_DEVICE_CONFIG if the device is not found or
    config is empty/invalid — this should not normally happen.
    """
    row = _q1("SELECT config FROM devices WHERE device_id = ?", (device_id,))
    if row is None:
        return dict(DEFAULT_DEVICE_CONFIG)
    try:
        return json.loads(row["config"]) or dict(DEFAULT_DEVICE_CONFIG)
    except (json.JSONDecodeError, TypeError):
        log.warning(f"[db] Invalid config JSON for {device_id} — using defaults")
        return dict(DEFAULT_DEVICE_CONFIG)


def get_global_device_config() -> DeviceConfig:
    """
    Return the fleet-wide default device config.

    Falls back to DEFAULT_DEVICE_CONFIG if the key is missing or unparseable
    (should only occur on a fresh DB before migration v3 has run).
    """
    row = _q1("SELECT value FROM system_config WHERE key = ?", (SystemConfigKey.GLOBAL_DEVICE_CONFIG,))
    if row is None or not row["value"]:
        return dict(DEFAULT_DEVICE_CONFIG)
    try:
        stored = json.loads(row["value"])
    except (json.JSONDecodeError, TypeError):
        log.warning("[db] Invalid global_device_config JSON — using defaults")
        return dict(DEFAULT_DEVICE_CONFIG)
    if not stored:
        return dict(DEFAULT_DEVICE_CONFIG)
    # Underlay defaults so keys added after the stored config was last saved
    # are still pushed with their default value
    # instead of silently falling back to whatever the device binary's
    # env default happens to be.
    return {**DEFAULT_DEVICE_CONFIG, **stored}


def get_global_device_config_raw() -> DeviceConfig:
    """
    The stored fleet config with defaults NOT underlaid — i.e. exactly the
    keys an operator has persisted.

    Used by the config-clobber guard (em_api._dropped_keys). The guard must
    compare against what is really stored: if it compared against the
    defaults-underlaid view, a controller upgrade that introduces a new
    default key would make every save from an already-open dashboard tab
    look like it was deleting that key, and refuse a perfectly legitimate
    write. Returns {} when nothing has been saved yet.
    """
    row = _q1("SELECT value FROM system_config WHERE key = ?", (SystemConfigKey.GLOBAL_DEVICE_CONFIG,))
    if row is None or not row["value"]:
        return {}
    try:
        return json.loads(row["value"]) or {}
    except (json.JSONDecodeError, TypeError):
        return {}


def set_global_device_config(config: Mapping[str, object]) -> None:
    """Persist updated fleet-wide default device config."""
    with _tx() as conn:
        conn.execute(
            "INSERT OR REPLACE INTO system_config (key, value) VALUES (?, ?)",
            (SystemConfigKey.GLOBAL_DEVICE_CONFIG, json.dumps(config)),
        )


def get_device_config_sections(device_id: str) -> list[em_config_sections.SectionId]:
    """The section ids this device overrides. Empty list = follows the fleet."""
    row = _q1("SELECT config_sections FROM devices WHERE device_id = ?", (device_id,))
    if row is None:
        return []
    try:
        return em_config_sections.normalise(json.loads(row["config_sections"]) or [])
    except (json.JSONDecodeError, TypeError):
        log.warning(f"[db] Invalid config_sections for {device_id} — treating as fleet")
        return []


def set_device_config_sections(device_id: str, section_ids: Iterable[object]) -> list[em_config_sections.SectionId]:
    """
    Set which sections this device overrides, and drop the stored values for
    any section it no longer does.

    Discarding on revert is deliberate and matches the pre-v8 use-global
    toggle (which reset the whole config to a copy of global):
    a section that follows the fleet should hold no stale shadow values that
    silently reappear if it is toggled back months later.

    use_global_config is kept in step as the compat view for older readers.
    """
    sections = em_config_sections.normalise(section_ids)
    kept = em_config_sections.keys_for(sections) | em_config_sections.STATE_KEYS
    with _tx() as conn:
        # Read inside the write transaction: a config write landing between
        # a separate read and this UPDATE would otherwise be lost.
        row = conn.execute("SELECT config FROM devices WHERE device_id = ?", (device_id,)).fetchone()
        stored = _json_dict(row["config"] if row is not None else None) or dict(DEFAULT_DEVICE_CONFIG)
        pruned = {k: v for k, v in stored.items() if k in kept}
        conn.execute(
            """
            UPDATE devices
               SET config_sections   = ?,
                   config            = ?,
                   use_global_config = ?
             WHERE device_id = ?
            """,
            (json.dumps(sections), json.dumps(pruned),
             0 if sections else 1, device_id),
        )
    return sections


def get_effective_device_config(device_id: str) -> DeviceConfig:
    """
    Return the config that should be pushed to a device.

    Fleet config, with this device's own values layered over it for whichever
    sections it overrides (see em_config_sections). No overridden sections =
    pure fleet config. Every section overridden = fully device-specific, which
    is what the pre-v8 use_global_config=0 meant.

    STATE_KEYS (startupVolume) always come from the device when present,
    regardless of scoping — that is this device's own hardware state, and
    inheriting it from the fleet would bring a device back at another room's
    volume.

    This is the authoritative source for what config a device should
    actually run — use it in device_connected() and any config-push path.
    """
    row = _q1(
        "SELECT config_sections, config FROM devices WHERE device_id = ?",
        (device_id,),
    )
    if row is None:
        return get_global_device_config()

    try:
        per_device = json.loads(row["config"]) or {}
    except (json.JSONDecodeError, TypeError):
        log.warning(f"[db] Invalid config JSON for {device_id} — using global")
        per_device = {}

    try:
        sections = em_config_sections.normalise(
            json.loads(row["config_sections"]) or []
        )
    except (json.JSONDecodeError, TypeError):
        log.warning(f"[db] Invalid config_sections for {device_id} — using global")
        sections = []

    return em_config_sections.merge(
        get_global_device_config(), per_device, sections
    )


def set_firmware_previous(device_id: str, version: Optional[str]) -> None:
    """
    Record the version that server.old holds after an OTA update.

    Set to the old running version before the update is applied.
    Set to None once server.old is pruned or a rollback completes.
    """
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET firmware_previous = ? WHERE device_id = ?",
            (version, device_id),
        )


def delete_device(device_id: str) -> None:
    """
    Remove a device and all its logs from the registry.

    This is a hard delete — use with care. Logs are removed first to
    satisfy the foreign key constraint.

    Saved utterance recordings, collected wake-word samples, ambient
    recordings and wake clips live on disk rather than in the DB, so no
    cascade reaches them — they are unlinked explicitly here. Leaving a
    deleted device's speech behind on the volume is the one leftover that
    actually matters.
    """
    with _tx() as conn:
        conn.execute("DELETE FROM device_logs WHERE device_id = ?", (device_id,))
        conn.execute("DELETE FROM devices WHERE device_id = ?", (device_id,))
    try:
        removed = em_recordings.delete_device(device_id)
        if removed:
            log.info(f"[db] Removed {removed} recording(s) for {device_id}")
    except Exception as e:
        log.warning(f"[db] Recording cleanup failed for {device_id}: {e}")
    try:
        removed = em_samples.delete_device(device_id)
        if removed:
            log.info(f"[db] Removed {removed} collected sample(s) for {device_id}")
    except Exception as e:
        log.warning(f"[db] Sample cleanup failed for {device_id}: {e}")
    try:
        removed = em_ambient.delete_device(device_id)
        if removed:
            log.info(f"[db] Removed {removed} ambient recording(s) for {device_id}")
    except Exception as e:
        log.warning(f"[db] Ambient cleanup failed for {device_id}: {e}")
    try:
        removed = em_wakeclips.delete_device(device_id)
        if removed:
            log.info(f"[db] Removed {removed} wake clip(s) for {device_id}")
    except Exception as e:
        log.warning(f"[db] Wake clip cleanup failed for {device_id}: {e}")
    log.info(f"[db] Device deleted: {device_id}")


# ─── ESPHome port allocation ──────────────────────────────────────────────────

def get_esphome_port(device_id: str) -> Optional[int]:
    """
    Return the ESPHome API port assigned to this device, or None if unassigned.

    A None return means the device has never been assigned a port in esphome
    mode — call assign_esphome_port() to allocate one.
    """
    row = _q1("SELECT esphome_api_port FROM devices WHERE device_id = ?", (device_id,))
    if row is None:
        return None
    return row["esphome_api_port"]  # may be None (unassigned)


def assign_esphome_port(device_id: str) -> int:
    """
    Allocate and persist an ESPHome API port for this device.

    Takes the next available port from next_esphome_port in system_config,
    increments the counter, persists both atomically, and returns the
    allocated port.

    Port allocation is monotonically increasing and never reuses freed ports
    (see ESPHOME_SPEC.md §2.2 for the rationale — sparse range is intentional
    to prevent silent misrouting if HA still holds a stale config entry for a
    deprovisioned device's old port number).

    Raises ValueError if the device is not found.
    Raises RuntimeError if a port is already assigned — caller should use
    get_esphome_port() first to check.
    """
    with _tx() as conn:
        row = conn.execute(
            "SELECT esphome_api_port FROM devices WHERE device_id = ?", (device_id,)
        ).fetchone()
        if row is None:
            raise ValueError(f"Device not found: {device_id}")
        if row["esphome_api_port"] is not None:
            raise RuntimeError(
                f"Device {device_id} already has ESPHome port {row['esphome_api_port']} — "
                f"use get_esphome_port() to retrieve it"
            )

        next_row = conn.execute(
            "SELECT value FROM system_config WHERE key = ?", (SystemConfigKey.NEXT_ESPHOME_PORT,)
        ).fetchone()
        port = int(next_row["value"])

        conn.execute(
            "UPDATE devices SET esphome_api_port = ? WHERE device_id = ?",
            (port, device_id),
        )
        conn.execute(
            "UPDATE system_config SET value = ? WHERE key = ?",
            (str(port + 1), SystemConfigKey.NEXT_ESPHOME_PORT),
        )

    log.info(f"[db] ESPHome port assigned: {device_id} → {port}")
    return port


def free_esphome_port(device_id: str) -> None:
    """
    Clear the ESPHome API port assignment for a device.

    Called on device deprovisioning. The freed port number is NOT returned
    to the pool — next_esphome_port only ever increments (see assign_esphome_port).
    """
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET esphome_api_port = NULL WHERE device_id = ?",
            (device_id,),
        )
    log.info(f"[db] ESPHome port freed: {device_id}")


# ─── BLE proxy port allocation ────────────────────────────────────────────────

# BLE proxy ports are aligned to the voice satellite port, not drawn from a
# separate pool: ble_proxy_port = esphome_api_port + BLE_PORT_OFFSET. This
# keeps the two ports for one device visibly paired (16001 voice / 17001 BT)
# and needs no allocator. The offset comfortably exceeds any realistic device
# count, so the voice range (16001+) and BT range (17001+) never overlap.
BLE_PORT_OFFSET = 1000


def get_ble_proxy_port(device_id: str) -> Optional[int]:
    """Return the BLE proxy port for this device, or None if unassigned."""
    row = _q1("SELECT ble_proxy_port FROM devices WHERE device_id = ?", (device_id,))
    if row is None:
        return None
    return row["ble_proxy_port"]  # may be None (unassigned)


def ensure_ble_proxy_port(device_id: str) -> Optional[int]:
    """
    Return this device's BLE proxy port (esphome_api_port + BLE_PORT_OFFSET),
    persisting it on first call. Returns None if the device has no voice port
    yet (BLE proxy can't exist without the voice satellite it's paired to).
    """
    with _tx() as conn:
        row = conn.execute(
            "SELECT esphome_api_port, ble_proxy_port FROM devices WHERE device_id = ?",
            (device_id,),
        ).fetchone()
        if row is None or row["esphome_api_port"] is None:
            return None
        port = int(row["esphome_api_port"]) + BLE_PORT_OFFSET
        if row["ble_proxy_port"] != port:
            conn.execute(
                "UPDATE devices SET ble_proxy_port = ? WHERE device_id = ?",
                (port, device_id),
            )
            log.info(f"[db] BLE proxy port set: {device_id} → {port}")
    return port


def free_ble_proxy_port(device_id: str) -> None:
    """Clear the BLE proxy port assignment (deprovisioning)."""
    with _tx() as conn:
        conn.execute(
            "UPDATE devices SET ble_proxy_port = NULL WHERE device_id = ?",
            (device_id,),
        )
    log.info(f"[db] BLE proxy port freed: {device_id}")


# ─── Device logs ──────────────────────────────────────────────────────────────

def log_device(
    device_id: str,
    level: LogLevel,
    source: LogSource,
    message: str,
) -> None:
    """
    Append a log entry for a device and prune old entries.

    Pruning keeps the most recent LOG_RETENTION rows per device.
    The extra DELETE is cheap on SQLite at this row count.
    """
    now_ms = int(time.time() * 1000)
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO device_logs (device_id, ts, level, source, message)
            VALUES (?, ?, ?, ?, ?)
            """,
            (device_id, now_ms, level, source, message),
        )
        # Prune: delete all but the most recent LOG_RETENTION rows for this device.
        conn.execute(
            """
            DELETE FROM device_logs
            WHERE device_id = ?
              AND id NOT IN (
                  SELECT id FROM device_logs
                  WHERE device_id = ?
                  ORDER BY ts DESC
                  LIMIT ?
              )
            """,
            (device_id, device_id, LOG_RETENTION),
        )


def get_device_logs(
    device_id: str,
    limit: int = 100,
    before_ts: Optional[int] = None,
) -> list[DeviceLogRow]:
    """
    Return log entries for a device in reverse-chronological order.

    limit:     maximum rows to return (capped at 1000)
    before_ts: if set, return only entries with ts < before_ts
               (cursor-based pagination — pass the ts of the last row
               from the previous page)
    """
    limit = min(limit, 1000)
    if before_ts is not None:
        rows = _q(
            """
            SELECT * FROM device_logs
            WHERE device_id = ? AND ts < ?
            ORDER BY ts DESC
            LIMIT ?
            """,
            (device_id, before_ts, limit),
        )
    else:
        rows = _q(
            """
            SELECT * FROM device_logs
            WHERE device_id = ?
            ORDER BY ts DESC
            LIMIT ?
            """,
            (device_id, limit),
        )
    return [DeviceLogRow.from_row(r) for r in rows]


# ─── Activity stats ───────────────────────────────────────────────────────────
#
# Persistent per-turn records + hourly rollups (near-misses, hardware
# metrics). Written from the voice pipeline via run_in_executor; read by the
# /api/devices/{id}/turns and /api/devices/{id}/activity endpoints.

# Turn dict keys ↔ column names written by insert_turn. "trigger" is stored
# as trigger_type because TRIGGER is an SQLite keyword. The §11.3 decision
# trace arrived in schema 22, the per-stage Activity detail in schema 24, the
# follow-up question detail in schema 25, the stored trace JSON and
# time-to-first-audio in schema 26.
_TURN_COLUMNS = {
    "trigger":            "trigger_type",
    "wake_model":         "wake_model",
    "wake_score":         "wake_score",
    "wake_threshold":     "wake_threshold",
    "outcome":            "outcome",
    "stt_text":           "stt_text",
    "total_ms":           "total_ms",
    "stt_ms":             "stt_ms",
    "tts_url_ms":         "tts_url_ms",
    "tts_fetch_ms":       "tts_fetch_ms",
    "playback_ms":        "playback_ms",
    "audio_ms":           "audio_ms",
    "wake_model_sha256":  "wake_model_sha256",
    "policy_hash":        "policy_hash",
    "wake_attribution":   "wake_attribution",
    "reference_coverage": "reference_coverage",
    "commit_route":       "commit_route",
    "terminal_reason":    "terminal_reason",
    "commit_id":          "commit_id",
    "asr_text":           "asr_text",        # controller streaming transcript, wake word included
    "endpoint_class":     "endpoint_class",  # grammar class of the committed text
    "endpoint_ms":        "endpoint_ms",     # silence waited after the last command speech
    "stt_raw":            "stt_raw",         # HA transcript before wake-phrase removal
    "intent_ms":          "intent_ms",       # dispatch → intent-end
    "intent_local":       "intent_local",    # 1: HA's built-in agent answered; 0: the conversation agent
    "response_type":      "response_type",   # HA intent response_type
    "response_text":      "response_text",   # the spoken answer
    "playback_reason":    "playback_reason", # how the spoken answer ended: drained | cancelled | failed | …
    "turn_uuid":          "turn_uuid",       # the actor's turn id (the key of the logged trace)
    "conversation_id":    "conversation_id", # HA conversation the turn ran in
    "reply_to":           "reply_to",        # turn_uuid of the question this turn answered
    "continuation":       "continuation",    # fate of the question this turn asked (em_session FOLLOWUP_*)
    "first_audio_ms":     "first_audio_ms",  # endpoint commit → response audible
    "decision_trace":     "decision_trace",  # §11.3 trace JSON; newest TRACE_RETENTION rows only
}

# Written but not returned by get_turns (nor pushed as turn_complete): the
# Activity list never reads the trace, and at 3–6 KB a row it would dominate
# every turns response.
TURN_WRITE_ONLY_COLUMNS = frozenset({"decision_trace"})

# Columns get_turns returns but nothing writes any more (SPEC §18.4 step 3):
# the legacy data-plane delivery measurements and the shadow/device-wake
# comparison. They keep their history. audio_file/wake_file are written after
# the insert by set_turn_audio/set_turn_wake (their names embed the rowid).
_TURN_READ_ONLY_COLUMNS = (
    "noise_floor", "vad_end_ms", "tts_bytes", "underruns", "playback_periods",
    "min_depth", "prime_wait_ms", "recv_span_ms", "max_gap_ms", "bytes_recv",
    "send_ms", "delivery_ms", "eq_ms",
    "dev_wake_score", "dev_wake_delta_ms", "dev_shadow", "dev_threshold",
    "ctrl_wake_score", "ctrl_wake_delta_ms",
    "audio_file", "wake_file",
)


def _py(v: object) -> object:
    """Numpy scalars to Python natives: sqlite3 stores a numpy float32 as a
    BLOB, which breaks JSON serialisation and SQL MAX()."""
    return v.item() if hasattr(v, "item") else v


def insert_turn(device_id: str, rec: Mapping[str, object]) -> int:
    """
    Persist one completed voice turn. `rec` keys are those of _TURN_COLUMNS
    plus optional `ts` (epoch s); missing keys are NULL, other keys ignored.
    Returns the rowid (set_turn_audio/set_turn_wake attach files by it).
    Prunes to TURN_RETENTION rows per device, and NULLs decision_trace on
    all but the newest TRACE_RETENTION of them.
    """
    cols   = ["device_id", "ts"] + list(_TURN_COLUMNS.values())
    values = [device_id, _py(rec.get("ts", time.time()))] + [
        _py(rec.get(k)) for k in _TURN_COLUMNS
    ]
    with _tx() as conn:
        cur = conn.execute(
            f"INSERT INTO turns ({', '.join(cols)}) "
            f"VALUES ({', '.join('?' * len(cols))})",
            values,
        )
        conn.execute(
            """
            DELETE FROM turns
            WHERE device_id = ?
              AND id NOT IN (
                  SELECT id FROM turns
                  WHERE device_id = ?
                  ORDER BY ts DESC
                  LIMIT ?
              )
            """,
            (device_id, device_id, TURN_RETENTION),
        )
        conn.execute(
            """
            UPDATE turns SET decision_trace = NULL
            WHERE device_id = ?
              AND decision_trace IS NOT NULL
              AND id NOT IN (
                  SELECT id FROM turns
                  WHERE device_id = ?
                  ORDER BY ts DESC
                  LIMIT ?
              )
            """,
            (device_id, device_id, TRACE_RETENTION),
        )
        return _rowid(cur)


def set_turn_audio(turn_id: int, audio_file: Optional[str]) -> None:
    """Attach a saved utterance recording (named by rowid) to a turn."""
    with _tx() as conn:
        conn.execute(
            "UPDATE turns SET audio_file = ? WHERE id = ?",
            (audio_file, turn_id),
        )


def set_turn_wake(turn_id: int, wake_file: Optional[str]) -> None:
    """Attach a saved wake clip (named by rowid) to a turn."""
    with _tx() as conn:
        conn.execute(
            "UPDATE turns SET wake_file = ? WHERE id = ?",
            (wake_file, turn_id),
        )


def set_turn_continuation(turn_id: int, continuation: str) -> None:
    """Replace a turn's `pending` follow-up with its final outcome."""
    with _tx() as conn:
        conn.execute(
            "UPDATE turns SET continuation = ? WHERE id = ?",
            (continuation, turn_id),
        )


def get_turns(
    device_id: str,
    limit: int = 50,
    since: Optional[float] = None,
) -> list[dict[str, object]]:
    """
    Recent turns for a device, oldest first: {turn_id, ts} plus every
    _TURN_COLUMNS key except TURN_WRITE_ONLY_COLUMNS, and every
    _TURN_READ_ONLY_COLUMNS column (NULL = not recorded). since: optional
    epoch-seconds lower bound.
    """
    if since is not None:
        rows = _q(
            "SELECT * FROM turns WHERE device_id = ? AND ts >= ? "
            "ORDER BY ts DESC LIMIT ?",
            (device_id, since, limit),
        )
    else:
        rows = _q(
            "SELECT * FROM turns WHERE device_id = ? ORDER BY ts DESC LIMIT ?",
            (device_id, limit),
        )
    out = []
    for row in reversed(rows):
        rec: dict[str, object] = {"turn_id": row["id"], "ts": row["ts"]}
        for key, col in _TURN_COLUMNS.items():
            if key not in TURN_WRITE_ONLY_COLUMNS:
                rec[key] = row[col]
        for col in _TURN_READ_ONLY_COLUMNS:
            rec[col] = row[col]
        out.append(rec)
    return out


def bump_wake_counters(
    device_id: str,
    *,
    near_misses: int = 0,
    near_miss_max: Optional[float] = None,
    dev_hops: int = 0,
    dev_drops: int = 0,
    dev_crossings: int = 0,
    dev_max_score: Optional[float] = None,
    dev_max_infer_ms: Optional[float] = None,
) -> None:
    """
    Accumulate one device `wake.stats` window into the current hour's
    wake_counters row (SPEC §18.4 step 4): near_misses/near_miss_max from its
    near-miss episodes, dev_hops = hops_scored, dev_drops = hops_dropped
    (wake_overrun), dev_crossings = candidates_opened, dev_max_score =
    peak_smoothed, dev_max_infer_ms = infer_max_ms. Counts add; maxima take the
    max, and None (not measured this window) leaves the stored maximum alone.
    """
    def _max(v: object) -> object:
        return 0 if v is None else _py(v)

    hour_ts = int(time.time()) // 3600 * 3600
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO wake_counters (device_id, hour_ts, near_misses, near_miss_max,
                                       dev_hops, dev_drops, dev_crossings,
                                       dev_max_score, dev_max_infer_ms)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (device_id, hour_ts) DO UPDATE SET
                near_misses      = near_misses + excluded.near_misses,
                near_miss_max    = MAX(near_miss_max, excluded.near_miss_max),
                dev_hops         = dev_hops + excluded.dev_hops,
                dev_drops        = dev_drops + excluded.dev_drops,
                dev_crossings    = dev_crossings + excluded.dev_crossings,
                dev_max_score    = MAX(dev_max_score, excluded.dev_max_score),
                dev_max_infer_ms = MAX(dev_max_infer_ms, excluded.dev_max_infer_ms)
            """,
            (device_id, hour_ts, _py(near_misses), _max(near_miss_max),
             _py(dev_hops), _py(dev_drops), _py(dev_crossings),
             _max(dev_max_score),
             _max(None if dev_max_infer_ms is None else round(float(dev_max_infer_ms)))),
        )
        conn.execute(
            "DELETE FROM wake_counters WHERE hour_ts < ?",
            (hour_ts - WAKE_COUNTER_RETENTION_DAYS * 86400,),
        )


def get_wake_counters(device_id: str, since: float) -> list[WakeCounterRow]:
    """Hourly wake counters for a device from `since` (epoch s), oldest first."""
    return [WakeCounterRow.from_row(r) for r in _q(
        "SELECT * FROM wake_counters WHERE device_id = ? AND hour_ts >= ? "
        "ORDER BY hour_ts",
        (device_id, since),
    )]


def _lead_hist(text: object) -> list[int]:
    """A stored lead_hist JSON array; malformed reads as zeros."""
    try:
        values = json.loads(text) if isinstance(text, str) else None
    except json.JSONDecodeError:
        values = None
    if (not isinstance(values, list) or len(values) != em_wake_rules.LEAD_BUCKETS
            or not all(isinstance(v, int) for v in values)):
        return [0] * em_wake_rules.LEAD_BUCKETS
    return values


def record_wake_shadow(device_id: str, reports: Sequence[em_wake_rules.ShadowReport],
                       wall: Callable[[int], float]) -> None:
    """
    Add one wake.stats `shadow` report into the current hour's wake_shadow
    rows (one per rule key; counters and lead_hist add) and insert its events
    with `wall(event.mono_ns)` as their time, keeping the newest
    WAKE_SHADOW_EVENT_RETENTION per device. Hourly rows age out with
    WAKE_COUNTER_RETENTION_DAYS.
    """
    hour_ts = int(time.time()) // 3600 * 3600
    with _tx() as conn:
        for r in reports:
            key = r.rule.key
            row = conn.execute(
                "SELECT lead_hist FROM wake_shadow WHERE device_id = ? AND hour_ts = ? AND rule_key = ?",
                (device_id, hour_ts, key),
            ).fetchone()
            hist = _lead_hist(row["lead_hist"]) if row is not None else [0] * em_wake_rules.LEAD_BUCKETS
            hist = [a + b for a, b in zip(hist, r.lead_hist)]
            conn.execute(
                """
                INSERT INTO wake_shadow (device_id, hour_ts, rule_key, hops, opens, matched,
                                         lead_hist, unmatched, retried, live_only, events_dropped)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT (device_id, hour_ts, rule_key) DO UPDATE SET
                    hops           = hops + excluded.hops,
                    opens          = opens + excluded.opens,
                    matched        = matched + excluded.matched,
                    lead_hist      = excluded.lead_hist,
                    unmatched      = unmatched + excluded.unmatched,
                    retried        = retried + excluded.retried,
                    live_only      = live_only + excluded.live_only,
                    events_dropped = events_dropped + excluded.events_dropped
                """,
                (device_id, hour_ts, key, r.hops, r.opens, r.matched, json.dumps(hist),
                 r.unmatched, r.retried, r.live_only, r.events_dropped),
            )
            for e in r.events:
                conn.execute(
                    "INSERT INTO wake_shadow_events (device_id, ts, rule_key, kind, peak_raw, raws) "
                    "VALUES (?, ?, ?, ?, ?, ?)",
                    (device_id, wall(e.mono_ns), key, str(e.kind), e.peak_raw, json.dumps(list(e.raws))),
                )
        conn.execute(
            """
            DELETE FROM wake_shadow_events WHERE device_id = ? AND id NOT IN (
                SELECT id FROM wake_shadow_events WHERE device_id = ?
                ORDER BY ts DESC, id DESC LIMIT ?)
            """,
            (device_id, device_id, WAKE_SHADOW_EVENT_RETENTION),
        )
        conn.execute("DELETE FROM wake_shadow WHERE hour_ts < ?",
                     (hour_ts - WAKE_COUNTER_RETENTION_DAYS * 86400,))


def get_wake_shadow(device_id: str, since: float) -> list[em_wake_rules.ShadowTotals]:
    """A device's wake_shadow counters per rule key, summed over the hours from `since`."""
    totals: dict[str, em_wake_rules.ShadowTotals] = {}
    for r in _q("SELECT * FROM wake_shadow WHERE device_id = ? AND hour_ts >= ? ORDER BY hour_ts",
                (device_id, since)):
        prev = totals.get(r["rule_key"]) or em_wake_rules.ShadowTotals.empty(r["rule_key"])
        totals[r["rule_key"]] = em_wake_rules.ShadowTotals(
            rule_key=r["rule_key"],
            hops=prev.hops + r["hops"],
            opens=prev.opens + r["opens"],
            matched=prev.matched + r["matched"],
            lead_hist=tuple(a + b for a, b in zip(prev.lead_hist, _lead_hist(r["lead_hist"]))),
            unmatched=prev.unmatched + r["unmatched"],
            retried=prev.retried + r["retried"],
            live_only=prev.live_only + r["live_only"],
            events_dropped=prev.events_dropped + r["events_dropped"],
        )
    return list(totals.values())


def get_wake_shadow_events(device_id: str, since: float, limit: int) -> list[em_wake_rules.ShadowEventRecord]:
    """A device's newest shadow events from `since`, newest first."""
    out: list[em_wake_rules.ShadowEventRecord] = []
    for r in _q("SELECT ts, rule_key, kind, peak_raw, raws FROM wake_shadow_events "
                "WHERE device_id = ? AND ts >= ? ORDER BY ts DESC, id DESC LIMIT ?",
                (device_id, since, limit)):
        try:
            raws = json.loads(r["raws"])
        except json.JSONDecodeError:
            raws = []
        out.append(em_wake_rules.ShadowEventRecord(
            ts=r["ts"], rule_key=r["rule_key"], kind=r["kind"], peak_raw=r["peak_raw"],
            raws=tuple(float(v) for v in raws if isinstance(v, (int, float))) if isinstance(raws, list) else ()))
    return out


def record_device_stats(device_id: str, stats: Mapping[str, object]) -> None:
    """
    Fold one ~30s hardware stats report into the current hour's
    device_metrics rollup. Missing/None fields are skipped. Averages are
    computed at read time from the sums; gauges (storage, mem total) keep
    the latest value. Prunes rows older than WAKE_COUNTER_RETENTION_DAYS.
    """
    hour_ts = int(time.time()) // 3600 * 3600
    cpu     = _number(stats.get("cpuPct"))
    mem     = _number(stats.get("memUsedMb"))
    rssi    = _number(stats.get("wifiRssi"))
    link_speed = _number(stats.get("linkSpeedMbps"))
    cpu_temp   = _number(stats.get("cpuTempC"))
    max_temp   = _number(stats.get("maxTempC"))
    cores_on   = _number(stats.get("coresOnline"))
    cores_tot  = _number(stats.get("coresTotal"))
    therm_lim  = _number(stats.get("thermalCoreLimit"))
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO device_metrics (
                device_id, hour_ts, samples, cpu_sum, cpu_max, mem_used_sum,
                mem_total_mb, storage_used_mb, storage_total_mb,
                rssi_sum, rssi_samples, rssi_min,
                link_speed_last, link_speed_min, wifi_freq_last,
                wifi_bssid_last, tx_bytes_sum, rx_bytes_sum,
                tx_errors_sum, tx_dropped_sum, rx_crc_sum,
                rtt_sum_ms, rtt_samples, rtt_min_ms, rtt_max_ms,
                rtt_excursions, rtt_excursions_idle, rtt_samples_idle,
                cpu_temp_sum, cpu_temp_samples, cpu_temp_max, max_temp_max,
                cores_online_last, cores_online_min, cores_total,
                thermal_limit_min
            ) VALUES (?, ?, 1, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                      ?, ?, ?, ?, ?, ?, ?, ?, ?,
                      ?, ?, ?, ?, ?, ?, ?,
                      ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (device_id, hour_ts) DO UPDATE SET
                samples          = samples + 1,
                cpu_sum          = cpu_sum + excluded.cpu_sum,
                cpu_max          = MAX(cpu_max, excluded.cpu_max),
                mem_used_sum     = mem_used_sum + excluded.mem_used_sum,
                mem_total_mb     = COALESCE(excluded.mem_total_mb, mem_total_mb),
                storage_used_mb  = COALESCE(excluded.storage_used_mb, storage_used_mb),
                storage_total_mb = COALESCE(excluded.storage_total_mb, storage_total_mb),
                rssi_sum         = rssi_sum + excluded.rssi_sum,
                rssi_samples     = rssi_samples + excluded.rssi_samples,
                rssi_min         = CASE
                    WHEN excluded.rssi_min IS NULL THEN rssi_min
                    ELSE MIN(COALESCE(rssi_min, excluded.rssi_min), excluded.rssi_min)
                END,
                -- Link identity keeps the latest value (a band/AP change
                -- mid-hour should be visible as the new one); link_speed_min
                -- keeps the worst PHY rate seen, which is the number that
                -- matters when hunting a throughput collapse.
                -- Thermals: sum/samples for a mean that ignores reports where
                -- the sensor was unreadable, plus the peak. cores_online_min is
                -- the tightest the device ever ran; thermal_limit_min below
                -- cores_total means throttling happened this hour.
                cpu_temp_sum     = cpu_temp_sum + excluded.cpu_temp_sum,
                cpu_temp_samples = cpu_temp_samples + excluded.cpu_temp_samples,
                cpu_temp_max     = MAX(COALESCE(cpu_temp_max, excluded.cpu_temp_max),
                                       COALESCE(excluded.cpu_temp_max, cpu_temp_max)),
                max_temp_max     = MAX(COALESCE(max_temp_max, excluded.max_temp_max),
                                       COALESCE(excluded.max_temp_max, max_temp_max)),
                cores_online_last = COALESCE(excluded.cores_online_last, cores_online_last),
                cores_online_min  = CASE
                    WHEN excluded.cores_online_min IS NULL THEN cores_online_min
                    ELSE MIN(COALESCE(cores_online_min, excluded.cores_online_min),
                             excluded.cores_online_min)
                END,
                cores_total       = COALESCE(excluded.cores_total, cores_total),
                thermal_limit_min = CASE
                    WHEN excluded.thermal_limit_min IS NULL THEN thermal_limit_min
                    ELSE MIN(COALESCE(thermal_limit_min, excluded.thermal_limit_min),
                             excluded.thermal_limit_min)
                END,
                link_speed_last  = COALESCE(excluded.link_speed_last, link_speed_last),
                link_speed_min   = CASE
                    WHEN excluded.link_speed_min IS NULL THEN link_speed_min
                    ELSE MIN(COALESCE(link_speed_min, excluded.link_speed_min),
                             excluded.link_speed_min)
                END,
                wifi_freq_last   = COALESCE(excluded.wifi_freq_last, wifi_freq_last),
                wifi_bssid_last  = COALESCE(excluded.wifi_bssid_last, wifi_bssid_last),
                tx_bytes_sum     = tx_bytes_sum   + excluded.tx_bytes_sum,
                rx_bytes_sum     = rx_bytes_sum   + excluded.rx_bytes_sum,
                tx_errors_sum    = tx_errors_sum  + excluded.tx_errors_sum,
                tx_dropped_sum   = tx_dropped_sum + excluded.tx_dropped_sum,
                rx_crc_sum       = rx_crc_sum     + excluded.rx_crc_sum,
                -- RTT arrives pre-aggregated over the window between stats
                -- reports, so sums accumulate and the extremes take the
                -- better/worse of the two. NULL means no samples landed in
                -- that window and must not poison the running min.
                rtt_sum_ms       = rtt_sum_ms  + excluded.rtt_sum_ms,
                rtt_samples      = rtt_samples + excluded.rtt_samples,
                rtt_min_ms       = CASE
                    WHEN excluded.rtt_min_ms IS NULL THEN rtt_min_ms
                    ELSE MIN(COALESCE(rtt_min_ms, excluded.rtt_min_ms), excluded.rtt_min_ms)
                END,
                rtt_max_ms       = CASE
                    WHEN excluded.rtt_max_ms IS NULL THEN rtt_max_ms
                    ELSE MAX(COALESCE(rtt_max_ms, excluded.rtt_max_ms), excluded.rtt_max_ms)
                END,
                rtt_excursions      = rtt_excursions      + excluded.rtt_excursions,
                rtt_excursions_idle = rtt_excursions_idle + excluded.rtt_excursions_idle,
                rtt_samples_idle    = rtt_samples_idle    + excluded.rtt_samples_idle
            """,
            (
                device_id, hour_ts,
                float(cpu or 0.0), float(cpu or 0.0), float(mem or 0.0),
                _number(stats.get("memTotalMb")),
                _number(stats.get("storageUsedMb")), _number(stats.get("storageTotalMb")),
                float(rssi) if rssi is not None else 0.0,
                1 if rssi is not None else 0,
                float(rssi) if rssi is not None else None,
                # linkSpeedMbps is omitempty device-side and refreshed on a
                # slower cadence, so 0/absent means "not sampled this tick"
                # and must not poison the running minimum.
                link_speed or None, link_speed or None,
                _number(stats.get("wifiFreqMhz")) or None,
                stats.get("wifiBssid") or None,
                int(_number(stats.get("txBytes")) or 0), int(_number(stats.get("rxBytes")) or 0),
                int(_number(stats.get("txErrors")) or 0), int(_number(stats.get("txDropped")) or 0),
                int(_number(stats.get("rxCrcErrors")) or 0),
                # RTT is controller-measured and folded into the same report
                # (see Device.drain_rtt). Absent when no probe completed in
                # the window — NULL rather than 0 so the min stays honest.
                int(_number(stats.get("rttSumMs")) or 0), int(_number(stats.get("rttSamples")) or 0),
                _number(stats.get("rttMinMs")), _number(stats.get("rttMaxMs")),
                int(_number(stats.get("rttExcursions")) or 0),
                int(_number(stats.get("rttExcursionsIdle")) or 0),
                int(_number(stats.get("rttSamplesIdle")) or 0),
                # Thermals. NULL, never 0, when a sensor was unreadable — a
                # zeroed temperature would drag a mean down and read as a cool
                # device, which is the wrong direction for a safety metric.
                float(cpu_temp) if cpu_temp is not None else 0.0,
                1 if cpu_temp is not None else 0,
                float(cpu_temp) if cpu_temp is not None else None,
                float(max_temp) if max_temp is not None else None,
                int(cores_on) if cores_on else None,
                int(cores_on) if cores_on else None,
                int(cores_tot) if cores_tot else None,
                int(therm_lim) if therm_lim else None,
            ),
        )
        conn.execute(
            "DELETE FROM device_metrics WHERE hour_ts < ?",
            (hour_ts - WAKE_COUNTER_RETENTION_DAYS * 86400,),
        )


def _number(value: object) -> int | float | None:
    """A numeric stats field; None when absent or not a number."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def get_device_metrics(device_id: str, since: float) -> list[dict[str, object]]:
    """
    Hourly hardware metrics for a device from `since` (epoch s), oldest
    first, with averages resolved from the stored sums.
    """
    rows = _q(
        "SELECT * FROM device_metrics WHERE device_id = ? AND hour_ts >= ? "
        "ORDER BY hour_ts",
        (device_id, since),
    )
    out = []
    for r in rows:
        n = r["samples"] or 1
        out.append({
            "hour_ts":          r["hour_ts"],
            "samples":          r["samples"],
            "cpu_avg":          round(r["cpu_sum"] / n, 1),
            "cpu_max":          r["cpu_max"],
            "mem_used_avg":     round(r["mem_used_sum"] / n, 1),
            "mem_total_mb":     r["mem_total_mb"],
            "storage_used_mb":  r["storage_used_mb"],
            "storage_total_mb": r["storage_total_mb"],
            "rssi_avg":         round(r["rssi_sum"] / r["rssi_samples"], 1) if r["rssi_samples"] else None,
            "rssi_min":         r["rssi_min"],
            # Link identity + throughput (v7). These were persisted but never
            # surfaced here, so the activity API could not answer "was the
            # link different when this went wrong?".
            # Thermals + topology (v14). cpu_temp_avg divides by its OWN
            # sample count, not `samples`: a report where the sensor was
            # unreadable must not dilute the mean.
            "cpu_temp_avg":      round(r["cpu_temp_sum"] / r["cpu_temp_samples"], 1) if r["cpu_temp_samples"] else None,
            "cpu_temp_max":      r["cpu_temp_max"],
            "max_temp_max":      r["max_temp_max"],
            # cores_online is what makes cpu_avg legible — that percentage is a
            # share of ONLINE capacity, so a series can appear to halve when
            # only the divisor changed.
            "cores_online_last": r["cores_online_last"],
            "cores_online_min":  r["cores_online_min"],
            "cores_total":       r["cores_total"],
            # Below cores_total means the thermal governor capped capacity.
            "thermal_limit_min": r["thermal_limit_min"],
            "link_speed_last":  r["link_speed_last"],
            "link_speed_min":   r["link_speed_min"],
            "wifi_freq_last":   r["wifi_freq_last"],
            "wifi_bssid_last":  r["wifi_bssid_last"],
            "tx_bytes":         r["tx_bytes_sum"],
            "rx_bytes":         r["rx_bytes_sum"],
            # NOTE: tx_errors/tx_dropped/rx_crc are deliberately NOT exposed.
            # The MTK driver leaves every one of them at zero regardless of
            # link quality (/proc/net/wireless reports no retries or missed
            # beacons, signal_poll reports NOISE=9999), so surfacing them
            # would invite reading "0 errors" as "healthy link" — which is
            # exactly the mistake they caused on 2026-07-25.
            #
            # Control-plane RTT (v9) is the latency signal that actually
            # works on this hardware. excursions_idle vs excursions is the
            # discriminator: contention makes latency track load, power-save
            # makes it spike when the device is doing nothing.
            "rtt_avg_ms":       round(r["rtt_sum_ms"] / r["rtt_samples"], 1) if r["rtt_samples"] else None,
            "rtt_min_ms":       r["rtt_min_ms"],
            "rtt_max_ms":       r["rtt_max_ms"],
            "rtt_samples":      r["rtt_samples"],
            "rtt_excursions":      r["rtt_excursions"],
            "rtt_excursions_idle": r["rtt_excursions_idle"],
            "rtt_samples_idle":    r["rtt_samples_idle"],
            # Excursion RATE per state — the actual discriminator. Comparing
            # raw counts is vacuous when almost every sample is idle.
            "rtt_excursion_pct_idle": (
                round(100.0 * r["rtt_excursions_idle"] / r["rtt_samples_idle"], 1)
                if r["rtt_samples_idle"] else None),
            "rtt_excursion_pct_busy": (
                round(100.0 * (r["rtt_excursions"] - r["rtt_excursions_idle"])
                      / (r["rtt_samples"] - r["rtt_samples_idle"]), 1)
                if (r["rtt_samples"] - r["rtt_samples_idle"]) else None),
        })
    return out


# ─── Users ────────────────────────────────────────────────────────────────────

def get_user_by_username(username: str) -> Optional[UserRow]:
    """Return a user row by username, or None."""
    row = _q1("SELECT * FROM users WHERE username = ?", (username,))
    return UserRow.from_row(row) if row is not None else None


def get_user_by_id(user_id: int) -> Optional[UserRow]:
    """Return a user row by id, or None."""
    row = _q1("SELECT * FROM users WHERE id = ?", (user_id,))
    return UserRow.from_row(row) if row is not None else None


def create_user(username: str, password_hash: str, role: str = "readonly") -> int:
    """
    Insert a new user row and return the new user id.

    password_hash must already be bcrypt-hashed — this function does
    not hash passwords itself.
    Raises sqlite3.IntegrityError if username is already taken.
    """
    now = _now()
    with _tx() as conn:
        cur = conn.execute(
            """
            INSERT INTO users (username, password_hash, role, created_at)
            VALUES (?, ?, ?, ?)
            """,
            (username, password_hash, role, now),
        )
        return _rowid(cur)


def update_user_password(user_id: int, new_hash: str) -> None:
    """
    Update the password hash for a user.

    new_hash must already be bcrypt-hashed — this function does not hash
    passwords itself. Raises ValueError if the user is not found.
    """
    with _tx() as conn:
        cur = conn.execute(
            "UPDATE users SET password_hash = ? WHERE id = ?",
            (new_hash, user_id),
        )
        if cur.rowcount == 0:
            raise ValueError(f"User not found: {user_id}")
    log.info(f"[db] Password updated for user id={user_id}")


def get_all_users() -> list[UserRow]:
    """Return all users (password_hash excluded in the API layer, not here)."""
    return [UserRow.from_row(r) for r in _q("SELECT * FROM users ORDER BY created_at ASC")]


def user_count() -> int:
    """Return the total number of users. Used for first-run bootstrap check."""
    row = _q1("SELECT COUNT(*) AS n FROM users")
    return row["n"] if row else 0


# ─── Sessions ─────────────────────────────────────────────────────────────────

def create_session(token: str, user_id: int, expiry_days: int = 30) -> None:
    """Insert a new session row."""
    now = _now()
    expires = now + expiry_days * 86400
    with _tx() as conn:
        conn.execute(
            """
            INSERT INTO sessions (token, user_id, created_at, expires_at)
            VALUES (?, ?, ?, ?)
            """,
            (token, user_id, now, expires),
        )


def get_session(token: str) -> Optional[SessionRow]:
    """
    Return a valid (non-expired) session row, or None.

    Expired sessions are not automatically deleted here — call
    prune_sessions() periodically from the controller.
    """
    row = _q1(
        "SELECT * FROM sessions WHERE token = ? AND expires_at > ?",
        (token, _now()),
    )
    return SessionRow.from_row(row) if row is not None else None


def delete_session(token: str) -> None:
    """Delete a session (logout)."""
    with _tx() as conn:
        conn.execute("DELETE FROM sessions WHERE token = ?", (token,))


def prune_sessions() -> int:
    """
    Delete all expired sessions. Returns the number of rows deleted.

    Call from a periodic background task (e.g. once per hour).
    """
    now = _now()
    with _tx() as conn:
        cur = conn.execute("DELETE FROM sessions WHERE expires_at <= ?", (now,))
        deleted = cur.rowcount
    if deleted:
        log.debug(f"[db] Pruned {deleted} expired session(s)")
    return deleted


# ─── System config ────────────────────────────────────────────────────────────

def get_config(key: SystemConfigKey, default: Optional[str] = None) -> Optional[str]:
    """Return a system_config value by key, or default if not set."""
    row = _q1("SELECT value FROM system_config WHERE key = ?", (key,))
    if row is None:
        return default
    return row["value"]


def set_config(key: SystemConfigKey, value: Optional[str]) -> None:
    """Insert or update a system_config key."""
    with _tx() as conn:
        conn.execute(
            "INSERT OR REPLACE INTO system_config (key, value) VALUES (?, ?)",
            (key, value),
        )


def get_all_config() -> dict[str, Optional[str]]:
    """Return the full system_config as a plain dict."""
    rows = _q("SELECT key, value FROM system_config")
    return {row["key"]: row["value"] for row in rows}


# ─── Helpers ──────────────────────────────────────────────────────────────────

def _rowid(cur: sqlite3.Cursor) -> int:
    """The rowid an INSERT just assigned."""
    if cur.lastrowid is None:
        raise RuntimeError("INSERT assigned no rowid")
    return cur.lastrowid


def _now() -> int:
    """Current time as a Unix timestamp (integer seconds)."""
    return int(time.time())
