//! Synthetic databases in the contract's layout. No row comes from a real installation.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use echomuse_db::{DatabaseService, DbHandle, OpenedDatabase, ServiceConfig, contract};
use rusqlite::Connection;

pub const APPROVED: &str = "G0SYNTHETIC00001";
pub const PENDING: &str = "G6SYNTHETIC00002";

pub fn db_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("echomuse.db")
}

/// An installation the way an earlier controller left it: contract schema, rollback
/// journal (not yet WAL), settings and a few rows.
pub fn existing_installation(path: &Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(contract::SCHEMA_SQL).unwrap();
    conn.execute_batch(
        "INSERT INTO system_config (key, value) VALUES
             ('schema_version', '40'),
             ('device_approval', 'strict'),
             ('next_esphome_port', '16003'),
             ('session_expiry_days', '30');
         INSERT INTO devices (device_id, label, approved, ip, firmware_ver, firmware_previous,
                 first_seen, last_seen, config, esphome_api_port, esphome_noise_psk,
                 use_global_config, ble_proxy_port, token, config_sections, collect_mode,
                 ambient_mode, os_version, update_queued_at, kiosk_token)
             VALUES ('G0SYNTHETIC00001', 'Kitchen', 1, '192.0.2.10', 'fw-000000000001',
                 'fw-000000000000', 1700000000, 1700000500, '{\"wakeSound\": true}', 16001,
                 NULL, 1, NULL, 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA', '[]', 0, 0,
                 'Fire OS 0.0.0 (TEST)', NULL, NULL);
         INSERT INTO devices (device_id, approved, ip, firmware_ver, os_version, first_seen,
                 last_seen)
             VALUES ('G6SYNTHETIC00002', 0, '192.0.2.11', 'fw-000000000002',
                 'LineageOS 0.0-TEST', 1700001000, 1700001000);
         INSERT INTO device_logs (id, device_id, ts, level, source, message)
             VALUES (41, 'G0SYNTHETIC00001', 1700000100, 'info', 'controller', 'synthetic');",
    )
    .unwrap();
    conn
}

pub fn journal_mode(path: &Path) -> String {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap()
}

pub fn start(path: &Path) -> (DatabaseService, DbHandle) {
    start_with(path, ServiceConfig::default())
}

pub fn start_with(path: &Path, config: ServiceConfig) -> (DatabaseService, DbHandle) {
    let opened = OpenedDatabase::open(path).unwrap();
    let service = DatabaseService::start(opened, config).unwrap();
    let handle = service.handle();
    (service, handle)
}
