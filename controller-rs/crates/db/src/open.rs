//! Opening the database at `DB_PATH` in place (§3.5.1).
//!
//! An existing installation is only read until the cutover backup exists: the checks of
//! its version and layout come first, then the backup, and only then the first write
//! (switching the journal to WAL if it is not already). A new installation gets the
//! contract schema and its first `system_config` rows in one transaction.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, types::Value};

use crate::{
    backup::{BackupError, CutoverBackup, ensure_cutover_backup},
    contract::{SCHEMA_SQL, SCHEMA_VERSION, SchemaDifference, SystemConfigKey, schema_differences},
    devices::{ApprovalMode, ESPHOME_FIRST_PORT},
};

/// How long a statement waits for another connection's lock before failing. Only this
/// controller should use the database, so this covers brief locks taken by tools such
/// as the `sqlite3` shell or a file backup.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

/// What opening found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Startup {
    /// There was no database: the contract schema was created.
    Created,
    /// An existing installation passed every check; its cutover backup exists.
    Existing { cutover_backup: CutoverBackup },
}

/// Why the controller refuses to start on this database. Nothing was written to it.
#[derive(Debug)]
pub enum OpenError {
    /// SQLite failed while `action` was under way.
    Sqlite {
        action: &'static str,
        source: rusqlite::Error,
    },
    /// The database has tables but no `system_config` `schema_version`.
    NoSchemaVersion,
    /// `schema_version` is not a whole number; the stored text, None when it is NULL.
    UnreadableSchemaVersion(Option<String>),
    /// A newer controller created the database.
    NewerSchema {
        found: u64,
        known: u32,
        database: PathBuf,
    },
    /// An older release created the database and it was never upgraded to this version.
    OlderSchema { found: u64, known: u32 },
    /// The version is right but the layout is not.
    SchemaMismatch(Vec<SchemaDifference>),
    /// SQLite would not switch the database to WAL; the mode it kept.
    WalUnavailable { journal_mode: String },
    /// The cutover backup could not be written.
    CutoverBackup(BackupError),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite { action, source } => write!(f, "database error while {action}: {source}"),
            Self::NoSchemaVersion => f.write_str(
                "the database has tables but no system_config schema_version, so it is not an \
                 EchoMuse database this controller can open",
            ),
            Self::UnreadableSchemaVersion(Some(text)) => {
                write!(f, "the database's schema_version {text:?} is not a number")
            }
            Self::UnreadableSchemaVersion(None) => {
                f.write_str("the database's schema_version is NULL")
            }
            Self::NewerSchema {
                found,
                known,
                database,
            } => {
                let name = database
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| database.display().to_string());
                write!(
                    f,
                    "the database is at schema_version {found}, but this controller knows only \
                     version {known}: a newer controller created it. Run a controller at least \
                     that new, or restore a backup taken before that upgrade (the {name}.pre-* \
                     files beside the database)."
                )
            }
            Self::OlderSchema { found, known } => write!(
                f,
                "the database is at schema_version {found}, and this controller opens only \
                 version {known}. Upgrade it first: start the latest Python controller release \
                 on this database once, then start this controller."
            ),
            Self::SchemaMismatch(differences) => {
                f.write_str(
                    "the database's layout differs from the schema this controller writes, so \
                     nothing was written to it:",
                )?;
                for difference in differences {
                    write!(f, "\n  - {difference}")?;
                }
                Ok(())
            }
            Self::WalUnavailable { journal_mode } => write!(
                f,
                "SQLite kept the database in {journal_mode} journal mode instead of WAL; the \
                 filesystem may not support the shared memory WAL needs"
            ),
            Self::CutoverBackup(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for OpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite { source, .. } => Some(source),
            Self::CutoverBackup(e) => Some(e),
            _ => None,
        }
    }
}

fn sqlite(action: &'static str) -> impl FnOnce(rusqlite::Error) -> OpenError {
    move |source| OpenError::Sqlite { action, source }
}

/// A database that passed every startup check, with its read-write connection. Only
/// [`OpenedDatabase::open`] makes one, so whatever writes through it does so after the
/// cutover backup.
pub struct OpenedDatabase {
    pub(crate) connection: Connection,
    path: PathBuf,
    startup: Startup,
}

impl fmt::Debug for OpenedDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenedDatabase")
            .field("path", &self.path)
            .field("startup", &self.startup)
            .finish_non_exhaustive()
    }
}

impl OpenedDatabase {
    /// Open the database at `path`, creating it when there is none. A new database
    /// starts with strict device approval.
    ///
    /// An existing database must be at [`SCHEMA_VERSION`] with exactly the contract's
    /// tables and indexes. Then its cutover backup is written if missing, and only then is
    /// the journal switched to WAL. Every connection uses `foreign_keys=ON`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OpenError> {
        Self::open_with_initial_approval(path, ApprovalMode::Strict)
    }

    /// Like [`OpenedDatabase::open`], but a new database stores `approval` as its
    /// `device_approval` setting, so the deployment's `DEVICE_APPROVAL` choice holds on a
    /// fresh install. An existing database keeps its stored setting.
    pub fn open_with_initial_approval(
        path: impl AsRef<Path>,
        approval: ApprovalMode,
    ) -> Result<Self, OpenError> {
        let path = path.as_ref().to_owned();
        let mut connection = Connection::open(&path).map_err(sqlite("opening the database"))?;
        connection
            .busy_timeout(BUSY_TIMEOUT)
            .map_err(sqlite("setting the busy timeout"))?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(sqlite("enabling foreign keys"))?;

        let objects: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .map_err(sqlite("reading the schema"))?;
        let startup = if objects == 0 {
            use_wal(&connection)?;
            create_schema(&mut connection, approval)?;
            Startup::Created
        } else {
            check_version(&connection, &path)?;
            let differences =
                schema_differences(&connection).map_err(sqlite("comparing the schema"))?;
            if !differences.is_empty() {
                return Err(OpenError::SchemaMismatch(differences));
            }
            let cutover_backup =
                ensure_cutover_backup(&connection, &path).map_err(OpenError::CutoverBackup)?;
            use_wal(&connection)?;
            Startup::Existing { cutover_backup }
        };
        Ok(Self {
            connection,
            path,
            startup,
        })
    }

    /// The database file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What opening found and did.
    pub fn startup(&self) -> &Startup {
        &self.startup
    }
}

fn check_version(connection: &Connection, path: &Path) -> Result<(), OpenError> {
    let has_config: bool = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'system_config')",
            [],
            |row| row.get(0),
        )
        .map_err(sqlite("reading the schema"))?;
    if !has_config {
        return Err(OpenError::NoSchemaVersion);
    }
    let stored: Option<Value> = connection
        .query_row(
            "SELECT value FROM system_config WHERE key = ?1",
            [SystemConfigKey::SchemaVersion.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(sqlite("reading schema_version"))?;
    let found = match stored {
        None => return Err(OpenError::NoSchemaVersion),
        Some(Value::Integer(n)) => {
            u64::try_from(n).map_err(|_| OpenError::UnreadableSchemaVersion(Some(n.to_string())))?
        }
        Some(Value::Text(text)) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| OpenError::UnreadableSchemaVersion(Some(text.clone())))?,
        Some(Value::Null) => return Err(OpenError::UnreadableSchemaVersion(None)),
        Some(Value::Real(n)) => {
            return Err(OpenError::UnreadableSchemaVersion(Some(n.to_string())));
        }
        Some(Value::Blob(_)) => {
            return Err(OpenError::UnreadableSchemaVersion(Some(
                "<blob>".to_owned(),
            )));
        }
    };
    let known = SCHEMA_VERSION;
    match found.cmp(&u64::from(known)) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(OpenError::NewerSchema {
            found,
            known,
            database: path.to_owned(),
        }),
        std::cmp::Ordering::Less => Err(OpenError::OlderSchema { found, known }),
    }
}

fn use_wal(connection: &Connection) -> Result<(), OpenError> {
    let mode: String = connection
        .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
        .map_err(sqlite("switching to WAL"))?;
    if mode.eq_ignore_ascii_case("wal") {
        Ok(())
    } else {
        Err(OpenError::WalUnavailable { journal_mode: mode })
    }
}

/// The contract schema and the `system_config` rows a new installation starts with, in
/// one transaction: a failure leaves the file empty, and the next start tries again.
fn create_schema(connection: &mut Connection, approval: ApprovalMode) -> Result<(), OpenError> {
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(sqlite("creating the schema"))?;
    tx.execute_batch(SCHEMA_SQL)
        .map_err(sqlite("creating the schema"))?;
    let first_port = ESPHOME_FIRST_PORT.to_string();
    let version = SCHEMA_VERSION.to_string();
    let rows = [
        (SystemConfigKey::SchemaVersion, version.as_str()),
        (SystemConfigKey::DeviceApproval, approval.as_str()),
        (SystemConfigKey::NextEsphomePort, first_port.as_str()),
    ];
    for (key, value) in rows {
        tx.execute(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2)",
            [key.as_str(), value],
        )
        .map_err(sqlite("writing the first settings"))?;
    }
    tx.commit().map_err(sqlite("creating the schema"))
}
