-- EchoMuse database schema at schema_version 40, read from a deployed database.
-- This is the persisted-data contract (docs/rust-controller-evaluation.md §3.5.1).
-- Schema only: no rows. Regenerate only by reading a real database, never by hand.

CREATE TABLE alert_delivery (
    endpoint_id TEXT PRIMARY KEY,
    calendar_entry_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    delivery_epoch TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    acked_sequence INTEGER NOT NULL,
    CHECK (acked_sequence <= sequence)
);

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

CREATE TABLE alert_scripts (
    object_id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    revision INTEGER NOT NULL
);

CREATE TABLE device_logs (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id  TEXT    NOT NULL,
        ts         INTEGER NOT NULL,
        level      TEXT    NOT NULL,
        source     TEXT    NOT NULL,
        message    TEXT    NOT NULL,
        FOREIGN KEY (device_id) REFERENCES devices(device_id)
    );

CREATE TABLE device_metrics (
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
        rssi_min         REAL, link_speed_last  INTEGER, link_speed_min   INTEGER, wifi_freq_last   INTEGER, wifi_bssid_last  TEXT, tx_bytes_sum     INTEGER NOT NULL DEFAULT 0, rx_bytes_sum     INTEGER NOT NULL DEFAULT 0, tx_errors_sum    INTEGER NOT NULL DEFAULT 0, tx_dropped_sum   INTEGER NOT NULL DEFAULT 0, rx_crc_sum       INTEGER NOT NULL DEFAULT 0, rtt_sum_ms          INTEGER NOT NULL DEFAULT 0, rtt_samples         INTEGER NOT NULL DEFAULT 0, rtt_min_ms          INTEGER, rtt_max_ms          INTEGER, rtt_excursions      INTEGER NOT NULL DEFAULT 0, rtt_excursions_idle INTEGER NOT NULL DEFAULT 0, rtt_samples_idle INTEGER NOT NULL DEFAULT 0, cpu_temp_sum     REAL    NOT NULL DEFAULT 0, cpu_temp_samples INTEGER NOT NULL DEFAULT 0, cpu_temp_max     REAL, max_temp_max     REAL, cores_online_last INTEGER, cores_online_min  INTEGER, cores_total       INTEGER, thermal_limit_min INTEGER,
        PRIMARY KEY (device_id, hour_ts)
    );

CREATE TABLE devices (
        device_id         TEXT    PRIMARY KEY,
        label             TEXT,
        approved          INTEGER NOT NULL DEFAULT 0,
        ip                TEXT,
        firmware_ver      TEXT,
        firmware_previous TEXT,
        first_seen        INTEGER,
        last_seen         INTEGER,
        config            TEXT    NOT NULL DEFAULT '{}'
    , esphome_api_port INTEGER, esphome_noise_psk TEXT, use_global_config INTEGER NOT NULL DEFAULT 1, ble_proxy_port INTEGER, token TEXT, config_sections TEXT NOT NULL DEFAULT '[]', collect_mode INTEGER NOT NULL DEFAULT 0, ambient_mode INTEGER NOT NULL DEFAULT 0, os_version TEXT, update_queued_at INTEGER, kiosk_token TEXT);

CREATE TABLE near_misses (
        id                INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id         TEXT    NOT NULL,
        ts                REAL    NOT NULL,
        near_miss_uuid    TEXT    NOT NULL,
        profile           TEXT    NOT NULL,
        floor             REAL    NOT NULL,
        peak              REAL    NOT NULL,
        wake_threshold    REAL,
        wake_model_sha256 TEXT,
        duration_ms       INTEGER NOT NULL,
        end_reason        TEXT    NOT NULL,
        recording         TEXT    NOT NULL,
        audio_ms          INTEGER
    , afe_series TEXT, verdict TEXT, windows TEXT);

CREATE TABLE sessions (
        token      TEXT    PRIMARY KEY,
        user_id    INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        FOREIGN KEY (user_id) REFERENCES users(id)
    );

CREATE TABLE system_config (
        key   TEXT PRIMARY KEY,
        value TEXT
    );

CREATE TABLE turns (
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
    , min_depth     INTEGER, prime_wait_ms INTEGER, recv_span_ms  INTEGER, max_gap_ms    INTEGER, bytes_recv    INTEGER, send_ms       INTEGER, delivery_ms   INTEGER, eq_ms         INTEGER, audio_file TEXT, dev_wake_score REAL, dev_wake_delta_ms INTEGER, dev_shadow INTEGER, dev_threshold REAL, ctrl_wake_score REAL, ctrl_wake_delta_ms INTEGER, wake_file TEXT, wake_model_sha256 TEXT, policy_hash TEXT, wake_attribution TEXT, reference_coverage REAL, commit_route TEXT, terminal_reason TEXT, commit_id TEXT, asr_text TEXT, endpoint_class TEXT, endpoint_ms INTEGER, stt_raw TEXT, intent_ms INTEGER, intent_local INTEGER, response_type TEXT, response_text TEXT, turn_uuid TEXT, conversation_id TEXT, reply_to TEXT, continuation TEXT, playback_reason TEXT, first_audio_ms INTEGER, decision_trace TEXT, afe_evidence TEXT, afe_series TEXT, response_latency_ms INTEGER, stt_fix TEXT, wake_windows TEXT);

CREATE TABLE users (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        username      TEXT    UNIQUE NOT NULL,
        password_hash TEXT    NOT NULL,
        role          TEXT    NOT NULL DEFAULT 'readonly',
        created_at    INTEGER NOT NULL
    );

CREATE TABLE wake_counters (
        device_id     TEXT    NOT NULL,
        hour_ts       INTEGER NOT NULL,
        near_misses   INTEGER NOT NULL DEFAULT 0,
        near_miss_max REAL    NOT NULL DEFAULT 0,
        underruns     INTEGER NOT NULL DEFAULT 0, dev_frames    INTEGER NOT NULL DEFAULT 0, dev_drops     INTEGER NOT NULL DEFAULT 0, dev_crossings INTEGER NOT NULL DEFAULT 0, dev_max_score REAL    NOT NULL DEFAULT 0, dev_max_infer_ms INTEGER NOT NULL DEFAULT 0, dev_max_gap_ms   INTEGER NOT NULL DEFAULT 0, dev_hops INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (device_id, hour_ts)
    );

CREATE TABLE wake_shadow (
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

CREATE TABLE wake_shadow_events (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        device_id TEXT    NOT NULL,
        ts        REAL    NOT NULL,
        rule_key  TEXT    NOT NULL,
        kind      TEXT    NOT NULL,
        peak_raw  REAL    NOT NULL,
        raws      TEXT    NOT NULL
    );

CREATE INDEX alert_ops_pending ON alert_ops(endpoint_id, state, seq);

CREATE INDEX alert_ops_terminal ON alert_ops(schedule_id, occurrence_key);

CREATE INDEX idx_device_logs_device_ts
        ON device_logs(device_id, ts DESC);

CREATE INDEX idx_near_misses_device_ts ON near_misses (device_id, ts);

CREATE INDEX idx_near_misses_ts ON near_misses (ts);

CREATE INDEX idx_turns_device_ts ON turns(device_id, ts DESC);

CREATE INDEX idx_turns_ts ON turns (ts);

CREATE INDEX idx_wake_shadow_events_device_ts
        ON wake_shadow_events (device_id, ts DESC);
