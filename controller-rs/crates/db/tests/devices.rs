//! Device admission against a contract-shaped database holding synthetic rows.

mod common;

use std::{
    net::{IpAddr, Ipv4Addr},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use common::{APPROVED, PENDING, db_path, existing_installation, start};
use echomuse_core::types::DeviceId;
use echomuse_db::{
    DbHandle, ServiceError,
    contract::SystemConfigKey,
    devices::{
        ApprovalMode, ApproveDevice, DeviceError, EnsureBleProxyPort, EnsureDeviceToken,
        EnsureEsphomePort, GetDevice, GetDeviceToken, ReadApprovalMode, RecordConnectionAttempt,
        UnixSeconds,
    },
};
use rusqlite::{Connection, types::Value};

const NEW: &str = "G0SYNTHETIC00003";

fn id(device: &str) -> DeviceId {
    DeviceId(device.to_owned())
}

fn at(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_millis(700)
}

fn ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
}

/// The stored values of one row, read outside the service, with SQLite's own types.
fn raw_row(path: &Path, sql: &str) -> Vec<Value> {
    let conn = Connection::open(path).unwrap();
    let mut statement = conn.prepare(sql).unwrap();
    let columns = statement.column_count();
    statement
        .query_row([], |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

async fn setup() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    echomuse_db::DatabaseService,
    DbHandle,
) {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    let (service, db) = start(&path);
    (dir, path, service, db)
}

#[tokio::test]
async fn a_device_row_reads_every_column_with_nulls_kept() {
    let (_dir, _path, service, db) = setup().await;

    let row = db
        .read(GetDevice {
            device: id(APPROVED),
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.device_id, id(APPROVED));
    assert_eq!(row.label.as_deref(), Some("Kitchen"));
    assert!(row.approved);
    assert_eq!(row.ip.as_deref(), Some("192.0.2.10"));
    assert_eq!(row.firmware_version.as_deref(), Some("fw-000000000001"));
    assert_eq!(row.firmware_previous.as_deref(), Some("fw-000000000000"));
    assert_eq!(row.first_seen, Some(UnixSeconds(1_700_000_000)));
    assert_eq!(row.last_seen, Some(UnixSeconds(1_700_000_500)));
    assert_eq!(row.config, r#"{"wakeSound": true}"#);
    assert_eq!(row.esphome_api_port, Some(16001));
    assert_eq!(row.esphome_noise_psk, None);
    assert!(row.use_global_config);
    assert_eq!(row.ble_proxy_port, None);
    assert_eq!(row.token.as_ref().map(|t| t.expose().len()), Some(43));
    assert_eq!(row.config_sections, "[]");
    assert!(!row.collect_mode && !row.ambient_mode);
    assert_eq!(row.os_version.as_deref(), Some("Fire OS 0.0.0 (TEST)"));
    assert_eq!(row.update_queued_at, None);
    assert_eq!(row.kiosk_token, None);
    assert!(
        !format!("{row:?}").contains("AAAAAAAA"),
        "Debug hides the token"
    );

    assert_eq!(db.read(GetDevice { device: id(NEW) }).await.unwrap(), None);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn the_first_attempt_registers_a_pending_device() {
    let (_dir, path, service, db) = setup().await;

    let attempt = db
        .write(RecordConnectionAttempt {
            device: id(NEW),
            ip: ip(12),
            firmware_version: Some("fw-000000000003".into()),
            os_version: None,
            at: at(1_700_002_000),
        })
        .await
        .unwrap();
    assert!(attempt.newly_registered);
    let row = attempt.device;
    assert!(!row.approved);
    assert_eq!(row.label, None);
    assert_eq!(row.first_seen, Some(UnixSeconds(1_700_002_000)));
    assert_eq!(row.last_seen, row.first_seen);
    assert_eq!(row.token, None);
    assert_eq!(row.esphome_api_port, None);

    assert_eq!(
        raw_row(
            &path,
            "SELECT label, approved, ip, firmware_ver, os_version, first_seen, last_seen, config, \
             config_sections, use_global_config, collect_mode, ambient_mode, token \
             FROM devices WHERE device_id = 'G0SYNTHETIC00003'"
        ),
        [
            Value::Null,
            Value::Integer(0),
            text("192.0.2.12"),
            text("fw-000000000003"),
            Value::Null,
            Value::Integer(1_700_002_000),
            Value::Integer(1_700_002_000),
            text("{}"),
            text("[]"),
            Value::Integer(1),
            Value::Integer(0),
            Value::Integer(0),
            Value::Null,
        ]
    );
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn every_attempt_records_address_versions_and_time_only() {
    let (_dir, path, service, db) = setup().await;
    let before = raw_row(
        &path,
        "SELECT label, approved, first_seen, config, esphome_api_port, token, firmware_previous \
         FROM devices WHERE device_id = 'G0SYNTHETIC00001'",
    );

    let attempt = db
        .write(RecordConnectionAttempt {
            device: id(APPROVED),
            ip: ip(99),
            firmware_version: None,
            os_version: Some("Fire OS 0.0.1 (TEST)".into()),
            at: at(1_700_009_000),
        })
        .await
        .unwrap();
    assert!(!attempt.newly_registered);
    assert_eq!(
        raw_row(
            &path,
            "SELECT ip, firmware_ver, os_version, last_seen FROM devices \
             WHERE device_id = 'G0SYNTHETIC00001'"
        ),
        [
            text("192.0.2.99"),
            Value::Null,
            text("Fire OS 0.0.1 (TEST)"),
            Value::Integer(1_700_009_000),
        ],
        "a hello without a firmware version stores NULL, not an old or empty value"
    );
    assert_eq!(
        raw_row(
            &path,
            "SELECT label, approved, first_seen, config, esphome_api_port, token, \
             firmware_previous FROM devices WHERE device_id = 'G0SYNTHETIC00001'"
        ),
        before
    );
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn approval_labels_a_device_and_needs_its_row() {
    let (_dir, path, service, db) = setup().await;

    db.write(ApproveDevice {
        device: id(PENDING),
        label: "Unknown G6SYNTHE".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        raw_row(
            &path,
            "SELECT approved, label, config FROM devices WHERE device_id = 'G6SYNTHETIC00002'"
        ),
        [Value::Integer(1), text("Unknown G6SYNTHE"), text("{}")]
    );
    assert!(matches!(
        db.write(ApproveDevice {
            device: id(NEW),
            label: "x".into()
        })
        .await,
        Err(ServiceError::Failed(DeviceError::UnknownDevice(d))) if d == id(NEW)
    ));
    assert_eq!(db.read(GetDevice { device: id(NEW) }).await.unwrap(), None);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn a_token_is_minted_once_and_kept() {
    let (_dir, path, service, db) = setup().await;

    let existing = db
        .write(EnsureDeviceToken {
            device: id(APPROVED),
        })
        .await
        .unwrap();
    assert_eq!(existing.expose(), "A".repeat(43));

    // Provisioning pushes credentials before the device ever connected.
    let minted = db
        .write(EnsureDeviceToken { device: id(NEW) })
        .await
        .unwrap();
    let token = minted.expose();
    assert_eq!(token.len(), 43);
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert_eq!(
        raw_row(
            &path,
            "SELECT approved, label, ip, first_seen, last_seen FROM devices \
             WHERE device_id = 'G0SYNTHETIC00003'"
        ),
        [
            Value::Integer(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null
        ],
        "a device that never connected has no address and no seen times"
    );
    let again = db
        .write(EnsureDeviceToken { device: id(NEW) })
        .await
        .unwrap();
    assert_eq!(again, minted);
    assert_eq!(
        db.read(GetDeviceToken { device: id(NEW) }).await.unwrap(),
        Some(minted)
    );

    // Its first connection then sets first_seen.
    let attempt = db
        .write(RecordConnectionAttempt {
            device: id(NEW),
            ip: ip(12),
            firmware_version: None,
            os_version: None,
            at: at(1_700_003_000),
        })
        .await
        .unwrap();
    assert!(!attempt.newly_registered);
    assert_eq!(attempt.device.first_seen, Some(UnixSeconds(1_700_003_000)));

    // An empty stored token cannot authenticate, so it counts as none.
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE devices SET token = '' WHERE device_id = 'G6SYNTHETIC00002'",
            [],
        )
        .unwrap();
    assert_eq!(
        db.read(GetDeviceToken {
            device: id(PENDING)
        })
        .await
        .unwrap(),
        None
    );
    let replaced = db
        .write(EnsureDeviceToken {
            device: id(PENDING),
        })
        .await
        .unwrap();
    assert_eq!(replaced.expose().len(), 43);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

fn set_setting(path: &Path, key: SystemConfigKey, value: Option<&str>) {
    Connection::open(path)
        .unwrap()
        .execute(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key.as_str(), value],
        )
        .unwrap();
}

#[tokio::test]
async fn the_stored_approval_mode_wins_over_the_fallback() {
    let (_dir, path, service, db) = setup().await;
    let mode = |fallback| db.read(ReadApprovalMode { fallback });

    assert_eq!(
        mode(ApprovalMode::Auto).await.unwrap(),
        ApprovalMode::Strict
    );
    set_setting(&path, SystemConfigKey::DeviceApproval, Some("auto"));
    assert_eq!(
        mode(ApprovalMode::Strict).await.unwrap(),
        ApprovalMode::Auto
    );
    for absent in [None, Some("")] {
        set_setting(&path, SystemConfigKey::DeviceApproval, absent);
        assert_eq!(mode(ApprovalMode::Auto).await.unwrap(), ApprovalMode::Auto);
        assert_eq!(
            mode(ApprovalMode::Strict).await.unwrap(),
            ApprovalMode::Strict
        );
    }
    set_setting(&path, SystemConfigKey::DeviceApproval, Some("sometimes"));
    assert_eq!(
        mode(ApprovalMode::Auto).await.unwrap(),
        ApprovalMode::Strict,
        "an unreadable setting never admits devices"
    );
    Connection::open(&path)
        .unwrap()
        .execute(
            "DELETE FROM system_config WHERE key = 'device_approval'",
            [],
        )
        .unwrap();
    assert_eq!(mode(ApprovalMode::Auto).await.unwrap(), ApprovalMode::Auto);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

fn next_port(path: &Path) -> Vec<Value> {
    raw_row(
        path,
        "SELECT value FROM system_config WHERE key = 'next_esphome_port'",
    )
}

#[tokio::test]
async fn esphome_ports_increase_and_are_never_reused() {
    let (_dir, path, service, db) = setup().await;
    let ensure = |device: &str| db.write(EnsureEsphomePort { device: id(device) });

    assert_eq!(
        ensure(APPROVED).await.unwrap(),
        16001,
        "a device keeps its port"
    );
    assert_eq!(next_port(&path), [text("16003")]);

    assert_eq!(ensure(PENDING).await.unwrap(), 16003);
    assert_eq!(
        next_port(&path),
        [text("16004")],
        "stored as text, like the rest"
    );
    assert_eq!(ensure(PENDING).await.unwrap(), 16003);
    assert_eq!(next_port(&path), [text("16004")]);

    // The device that held 16003 is removed; its port is not handed out again.
    Connection::open(&path)
        .unwrap()
        .execute(
            "DELETE FROM devices WHERE device_id = 'G6SYNTHETIC00002'",
            [],
        )
        .unwrap();
    db.write(EnsureDeviceToken { device: id(NEW) })
        .await
        .unwrap();
    assert_eq!(ensure(NEW).await.unwrap(), 16004);

    assert!(matches!(
        ensure("G0NOSUCHDEVICE00").await,
        Err(ServiceError::Failed(DeviceError::UnknownDevice(_)))
    ));
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn a_lagging_or_missing_port_counter_still_never_reuses_a_port() {
    let (_dir, path, service, db) = setup().await;

    set_setting(&path, SystemConfigKey::NextEsphomePort, Some("16000"));
    assert_eq!(
        db.write(EnsureEsphomePort {
            device: id(PENDING)
        })
        .await
        .unwrap(),
        16002,
        "above the highest port already assigned"
    );

    Connection::open(&path)
        .unwrap()
        .execute(
            "DELETE FROM system_config WHERE key = 'next_esphome_port'",
            [],
        )
        .unwrap();
    db.write(EnsureDeviceToken { device: id(NEW) })
        .await
        .unwrap();
    assert_eq!(
        db.write(EnsureEsphomePort { device: id(NEW) })
            .await
            .unwrap(),
        16003
    );
    assert_eq!(next_port(&path), [text("16004")]);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn an_unreadable_or_exhausted_counter_assigns_nothing() {
    let (_dir, path, service, db) = setup().await;
    let assigned = || {
        raw_row(
            &path,
            "SELECT esphome_api_port FROM devices WHERE device_id = 'G6SYNTHETIC00002'",
        )
    };

    set_setting(&path, SystemConfigKey::NextEsphomePort, Some("lots"));
    assert!(matches!(
        db.write(EnsureEsphomePort { device: id(PENDING) }).await,
        Err(ServiceError::Failed(DeviceError::InvalidSetting {
            key: SystemConfigKey::NextEsphomePort,
            value: Some(v),
        })) if v == "lots"
    ));
    assert_eq!(assigned(), [Value::Null]);

    // The last voice port whose Bluetooth port (+1000) still fits is 64535.
    set_setting(&path, SystemConfigKey::NextEsphomePort, Some("64535"));
    assert_eq!(
        db.write(EnsureEsphomePort {
            device: id(PENDING)
        })
        .await
        .unwrap(),
        64535
    );
    db.write(EnsureDeviceToken { device: id(NEW) })
        .await
        .unwrap();
    assert!(matches!(
        db.write(EnsureEsphomePort { device: id(NEW) }).await,
        Err(ServiceError::Failed(DeviceError::PortsExhausted))
    ));
    assert_eq!(next_port(&path), [text("64536")]);
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}

#[tokio::test]
async fn the_bluetooth_port_pairs_with_the_voice_port() {
    let (_dir, path, service, db) = setup().await;
    let ble = |device: &str| db.write(EnsureBleProxyPort { device: id(device) });

    assert_eq!(ble(PENDING).await.unwrap(), None, "no voice port yet");
    assert_eq!(
        raw_row(
            &path,
            "SELECT ble_proxy_port FROM devices WHERE device_id = 'G6SYNTHETIC00002'"
        ),
        [Value::Null]
    );
    assert_eq!(ble(APPROVED).await.unwrap(), Some(17001));
    assert_eq!(ble(APPROVED).await.unwrap(), Some(17001));
    assert_eq!(
        raw_row(
            &path,
            "SELECT ble_proxy_port FROM devices WHERE device_id = 'G0SYNTHETIC00001'"
        ),
        [Value::Integer(17001)]
    );
    assert!(matches!(
        ble(NEW).await,
        Err(ServiceError::Failed(DeviceError::UnknownDevice(_)))
    ));
    service.shutdown(Duration::from_secs(10)).await.unwrap();
}
