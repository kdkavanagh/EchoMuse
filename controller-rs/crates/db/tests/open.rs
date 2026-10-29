//! Opening the database in place: the version and layout checks, the cutover backup and
//! the connection settings (§3.5.1).

mod common;

use std::{collections::BTreeMap, fs};

use common::{db_path, existing_installation, journal_mode};
use echomuse_db::{
    BackupFailure, CutoverBackup, OpenError, OpenedDatabase, Startup,
    contract::{ObjectKind, SchemaDifference},
    cutover_backup_path,
};
use rusqlite::Connection;

fn objects(conn: &Connection, kind: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = ?1 AND name NOT LIKE 'sqlite\\_%' \
             ESCAPE '\\' ORDER BY name",
        )
        .unwrap();
    statement
        .query_map([kind], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn settings(conn: &Connection) -> BTreeMap<String, Option<String>> {
    let mut statement = conn
        .prepare("SELECT key, value FROM system_config")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn open_err(path: &std::path::Path) -> OpenError {
    OpenedDatabase::open(path).expect_err("the database must be refused")
}

#[test]
fn a_new_installation_records_the_deployment_approval_but_an_existing_one_keeps_its_own() {
    use echomuse_db::devices::ApprovalMode;
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);

    drop(OpenedDatabase::open_with_initial_approval(&path, ApprovalMode::Auto).unwrap());
    let approval = |path: &std::path::Path| {
        settings(&Connection::open(path).unwrap())["device_approval"].clone()
    };
    assert_eq!(approval(&path), Some("auto".to_owned()));

    // Reopening with another deployment choice leaves the stored setting alone.
    drop(OpenedDatabase::open_with_initial_approval(&path, ApprovalMode::Strict).unwrap());
    assert_eq!(approval(&path), Some("auto".to_owned()));
}

#[test]
fn a_new_installation_gets_the_contract_schema_and_first_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);

    let opened = OpenedDatabase::open(&path).unwrap();
    assert_eq!(opened.startup(), &Startup::Created);
    drop(opened);
    assert!(
        !cutover_backup_path(&path).exists(),
        "a new installation has nothing to back up"
    );

    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        objects(&conn, "table"),
        [
            "alert_delivery",
            "alert_ops",
            "alert_scripts",
            "device_logs",
            "device_metrics",
            "devices",
            "near_misses",
            "sessions",
            "system_config",
            "turns",
            "users",
            "wake_counters",
            "wake_shadow",
            "wake_shadow_events",
        ]
    );
    assert_eq!(objects(&conn, "index").len(), 8);
    let expected: BTreeMap<String, Option<String>> = [
        ("device_approval", "strict"),
        ("next_esphome_port", "16001"),
        ("schema_version", "40"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), Some(v.to_owned())))
    .collect();
    assert_eq!(settings(&conn), expected);
    let kinds: Vec<String> = conn
        .prepare("SELECT typeof(value) FROM system_config")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        kinds.iter().all(|k| k == "text"),
        "settings are stored as text"
    );
    drop(conn);
    assert_eq!(journal_mode(&path), "wal");

    // Opening it again checks it against the contract like any existing installation.
    let reopened = OpenedDatabase::open(&path).unwrap();
    assert!(matches!(
        reopened.startup(),
        Startup::Existing {
            cutover_backup: CutoverBackup::Written(_)
        }
    ));
}

#[test]
fn an_existing_installation_is_backed_up_before_its_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    assert_eq!(journal_mode(&path), "delete");

    let opened = OpenedDatabase::open(&path).unwrap();
    let backup = cutover_backup_path(&path);
    assert_eq!(
        opened.startup(),
        &Startup::Existing {
            cutover_backup: CutoverBackup::Written(backup.clone())
        }
    );
    assert_eq!(backup.file_name().unwrap(), "echomuse.db.pre-rust.bak");
    drop(opened);

    // The backup was taken before WAL was switched on, which is the first write.
    assert_eq!(journal_mode(&backup), "delete");
    assert_eq!(journal_mode(&path), "wal");
    for file in [&path, &backup] {
        let conn = Connection::open(file).unwrap();
        let log: (i64, String) = conn
            .query_row("SELECT id, message FROM device_logs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(log, (41, "synthetic".to_owned()), "row ids are kept");
        let devices: i64 = conn
            .query_row("SELECT count(*) FROM devices", [], |row| row.get(0))
            .unwrap();
        assert_eq!(devices, 2);
        assert_eq!(
            settings(&conn).get("session_expiry_days"),
            Some(&Some("30".to_owned()))
        );
    }
    assert!(
        !dir.path().join("echomuse.db.pre-rust.bak.partial").exists(),
        "no unfinished copy is left behind"
    );
}

#[test]
fn a_completed_cutover_backup_is_never_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    drop(OpenedDatabase::open(&path).unwrap());
    let backup = cutover_backup_path(&path);
    let first = fs::read(&backup).unwrap();

    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO device_logs (device_id, ts, level, source, message) \
             VALUES ('G0SYNTHETIC00001', 1700000200, 'info', 'controller', 'later')",
            [],
        )
        .unwrap();
    let opened = OpenedDatabase::open(&path).unwrap();
    assert_eq!(
        opened.startup(),
        &Startup::Existing {
            cutover_backup: CutoverBackup::AlreadyPresent(backup.clone())
        }
    );
    assert_eq!(fs::read(&backup).unwrap(), first);
}

#[test]
fn an_unfinished_copy_from_a_crash_is_discarded_and_written_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    let partial = dir.path().join("echomuse.db.pre-rust.bak.partial");
    fs::write(&partial, b"half a copy").unwrap();

    let opened = OpenedDatabase::open(&path).unwrap();
    assert!(matches!(
        opened.startup(),
        Startup::Existing {
            cutover_backup: CutoverBackup::Written(_)
        }
    ));
    assert!(!partial.exists());
    let count: i64 = Connection::open(cutover_backup_path(&path))
        .unwrap()
        .query_row("SELECT count(*) FROM devices", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn a_backup_that_cannot_be_written_stops_startup_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    fs::create_dir(cutover_backup_path(&path)).unwrap();

    match open_err(&path) {
        OpenError::CutoverBackup(e) => {
            assert!(matches!(e.cause, BackupFailure::NotAFile));
            assert!(e.to_string().contains("cannot be skipped"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        journal_mode(&path),
        "delete",
        "the database was not touched"
    );
}

fn set_version(path: &std::path::Path, value: Option<&str>) {
    Connection::open(path)
        .unwrap()
        .execute(
            "UPDATE system_config SET value = ?1 WHERE key = 'schema_version'",
            [value],
        )
        .unwrap();
}

#[test]
fn a_newer_database_is_refused_with_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    set_version(&path, Some("41"));

    let error = open_err(&path);
    assert!(matches!(
        error,
        OpenError::NewerSchema {
            found: 41,
            known: 40,
            ..
        }
    ));
    let message = error.to_string();
    assert!(message.contains("newer controller") && message.contains("echomuse.db.pre-*"));
    assert!(!cutover_backup_path(&path).exists());
    assert_eq!(journal_mode(&path), "delete");
}

#[test]
fn an_older_database_is_refused_with_upgrade_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    drop(existing_installation(&path));
    set_version(&path, Some("39"));

    let error = open_err(&path);
    assert!(matches!(
        error,
        OpenError::OlderSchema {
            found: 39,
            known: 40
        }
    ));
    assert!(
        error
            .to_string()
            .contains("latest Python controller release")
    );
    assert!(!cutover_backup_path(&path).exists());
}

#[test]
fn a_database_without_a_readable_version_is_refused() {
    let dir = tempfile::tempdir().unwrap();

    let foreign = dir.path().join("foreign.db");
    Connection::open(&foreign)
        .unwrap()
        .execute_batch("CREATE TABLE notes (body TEXT)")
        .unwrap();
    assert!(matches!(open_err(&foreign), OpenError::NoSchemaVersion));

    let path = db_path(&dir);
    drop(existing_installation(&path));
    set_version(&path, Some("forty"));
    assert!(matches!(
        open_err(&path),
        OpenError::UnreadableSchemaVersion(Some(text)) if text == "forty"
    ));
    set_version(&path, None);
    assert!(matches!(
        open_err(&path),
        OpenError::UnreadableSchemaVersion(None)
    ));
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM system_config WHERE key = 'schema_version'", [])
        .unwrap();
    assert!(matches!(open_err(&path), OpenError::NoSchemaVersion));
    assert!(!cutover_backup_path(&path).exists());
}

fn mismatch(alteration: &str) -> Vec<SchemaDifference> {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    existing_installation(&path)
        .execute_batch(alteration)
        .unwrap();
    let differences = match open_err(&path) {
        OpenError::SchemaMismatch(differences) => differences,
        other => panic!("unexpected {other:?}"),
    };
    assert!(!cutover_backup_path(&path).exists());
    assert_eq!(journal_mode(&path), "delete");
    differences
}

#[test]
fn a_different_layout_is_refused_with_each_difference() {
    assert_eq!(
        mismatch("ALTER TABLE devices ADD COLUMN nickname TEXT"),
        [SchemaDifference::Columns {
            table: "devices".to_owned(),
            missing: vec![],
            unexpected: vec!["nickname".to_owned()],
            changed: vec![],
        }]
    );
    assert_eq!(
        mismatch("ALTER TABLE turns DROP COLUMN wake_windows"),
        [SchemaDifference::Columns {
            table: "turns".to_owned(),
            missing: vec!["wake_windows".to_owned()],
            unexpected: vec![],
            changed: vec![],
        }]
    );
    assert_eq!(
        mismatch(
            "DROP TABLE alert_scripts;
             CREATE TABLE alert_scripts (object_id TEXT PRIMARY KEY, sha256 TEXT, revision INTEGER NOT NULL)"
        ),
        [SchemaDifference::Columns {
            table: "alert_scripts".to_owned(),
            missing: vec![],
            unexpected: vec![],
            changed: vec!["sha256".to_owned()],
        }]
    );
    assert_eq!(
        mismatch(
            "DROP TABLE alert_scripts;
             CREATE TABLE alert_scripts (object_id TEXT PRIMARY KEY, sha256 TEXT NOT NULL, \
             revision INTEGER NOT NULL, CHECK (revision > 0))"
        ),
        [SchemaDifference::Definition {
            kind: ObjectKind::Table,
            name: "alert_scripts".to_owned(),
        }]
    );
    let differences = mismatch(
        "DROP INDEX idx_turns_ts;
         CREATE TABLE extras (x INTEGER);
         CREATE VIEW recent AS SELECT * FROM turns",
    );
    assert_eq!(
        differences,
        [
            SchemaDifference::Missing {
                kind: ObjectKind::Index,
                name: "idx_turns_ts".to_owned(),
            },
            SchemaDifference::Unexpected {
                kind: ObjectKind::Table,
                name: "extras".to_owned(),
            },
            SchemaDifference::Unexpected {
                kind: ObjectKind::View,
                name: "recent".to_owned(),
            },
        ]
    );
    let report = OpenError::SchemaMismatch(differences).to_string();
    assert!(report.contains("index idx_turns_ts is missing"));
    assert!(report.contains("table extras is not part of the schema"));
}

#[test]
fn layout_whitespace_and_sqlite_statistics_are_not_differences() {
    let dir = tempfile::tempdir().unwrap();
    let path = db_path(&dir);
    existing_installation(&path)
        .execute_batch(
            "DROP TABLE alert_scripts;
             CREATE TABLE alert_scripts(object_id TEXT PRIMARY KEY,sha256 TEXT NOT NULL,
                 revision   INTEGER NOT NULL);
             ANALYZE;",
        )
        .unwrap();
    assert!(OpenedDatabase::open(&path).is_ok());
}
