//! The EchoMuse SQLite database at `DB_PATH`, opened in place (§3.5.1), and the database
//! service that owns it (§5.1).
//!
//! Startup, in order:
//!
//! 1. [`OpenedDatabase::open`] creates a new installation from the contract schema
//!    ([`contract::SCHEMA_SQL`]), or checks an existing one: `schema_version` must be
//!    exactly [`contract::SCHEMA_VERSION`] and its tables and indexes must match the
//!    contract. Nothing is written to a database that fails a check.
//! 2. For an existing installation it then writes `<DB_PATH>.pre-rust.bak` through
//!    SQLite's online backup API, unless an earlier start already did, and only after
//!    that switches the journal to WAL. Every connection enables foreign keys.
//! 3. [`DatabaseService::start`] moves the read-write connection onto the writer thread
//!    and opens the read-only connections. Callers then use a [`DbHandle`] with the
//!    command structs in [`devices`], or their own [`WriteCommand`]/[`ReadQuery`] types.
//!
//! The schema is never changed: this controller carries no migrations.

mod backup;
pub mod contract;
pub mod devices;
mod open;
mod service;

pub use backup::{BackupError, BackupFailure, CutoverBackup, cutover_backup_path};
pub use open::{OpenError, OpenedDatabase, Startup};
pub use service::{
    DatabaseService, DbHandle, PendingWrite, ReadQuery, ServiceConfig, ServiceError, ShutdownError,
    StartError, WriteCommand,
};
