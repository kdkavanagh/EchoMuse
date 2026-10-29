//! The database commands of the settings: reading the fleet and device documents, and
//! writing one of them. Each runs through the database service's
//! [`DbHandle`](echomuse_db::DbHandle); a write's reply arrives after its transaction
//! committed. Columns not named here are never written.

use std::fmt;

use echomuse_core::types::DeviceId;
use echomuse_db::{ReadQuery, WriteCommand};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// The `system_config` key of the fleet document.
pub const FLEET_CONFIG_KEY: &str = "global_device_config";

/// Why a settings command failed. A failed write rolled back.
#[derive(Debug)]
pub enum StoreError {
    /// SQLite failed.
    Sqlite(rusqlite::Error),
    /// The device has no row.
    UnknownDevice(DeviceId),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "database: {e}"),
            Self::UnknownDevice(d) => write!(f, "no device {d}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// One device's settings columns as stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceConfigRow {
    /// `device_id`.
    pub device: DeviceId,
    /// `config`.
    pub config: String,
    /// `config_sections`.
    pub config_sections: String,
    /// `use_global_config`, stored 0 or 1.
    pub use_global_config: bool,
}

/// The fleet document and every device's settings columns, read in one snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredRows {
    /// `global_device_config`; `None` when there is no row or its value is NULL.
    pub fleet: Option<String>,
    /// Every device row.
    pub devices: Vec<DeviceConfigRow>,
}

const DEVICE_COLUMNS: &str = "device_id, config, config_sections, use_global_config";

fn device_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceConfigRow> {
    Ok(DeviceConfigRow {
        device: DeviceId(row.get(0)?),
        config: row.get(1)?,
        config_sections: row.get(2)?,
        use_global_config: row.get::<_, i64>(3)? != 0,
    })
}

/// Read the fleet document and every device's settings columns.
#[derive(Clone, Copy, Debug)]
pub struct ReadAllSettings;

impl ReadQuery for ReadAllSettings {
    type Output = StoredRows;
    type Error = StoreError;

    fn read(self, conn: &Connection) -> Result<StoredRows, StoreError> {
        let fleet = conn
            .prepare_cached("SELECT value FROM system_config WHERE key = ?1")?
            .query_row([FLEET_CONFIG_KEY], |row| row.get::<_, Option<String>>(0))
            .optional()?
            .flatten();
        let devices = conn
            .prepare_cached(&format!(
                "SELECT {DEVICE_COLUMNS} FROM devices ORDER BY device_id"
            ))?
            .query_map([], device_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StoredRows { fleet, devices })
    }
}

/// Read one device's settings columns; `None` when it has no row.
#[derive(Clone, Debug)]
pub struct ReadDeviceSettings {
    /// The device.
    pub device: DeviceId,
}

impl ReadQuery for ReadDeviceSettings {
    type Output = Option<DeviceConfigRow>;
    type Error = StoreError;

    fn read(self, conn: &Connection) -> Result<Option<DeviceConfigRow>, StoreError> {
        Ok(conn
            .prepare_cached(&format!(
                "SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = ?1"
            ))?
            .query_row([&self.device.0], device_row)
            .optional()?)
    }
}

/// Store the fleet document, creating its row on a fresh installation.
#[derive(Clone, Debug)]
pub struct WriteFleetConfig {
    /// The document text.
    pub text: String,
}

impl WriteCommand for WriteFleetConfig {
    type Output = ();
    type Error = StoreError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        tx.execute(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![FLEET_CONFIG_KEY, self.text],
        )?;
        Ok(())
    }
}

/// Store a device's settings columns.
#[derive(Clone, Debug)]
pub struct WriteDeviceConfig {
    /// The device.
    pub device: DeviceId,
    /// The new `config` text; `None` leaves it as stored.
    pub config: Option<String>,
    /// The `config_sections` text.
    pub config_sections: String,
    /// `use_global_config`.
    pub use_global_config: bool,
}

impl WriteCommand for WriteDeviceConfig {
    type Output = ();
    type Error = StoreError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        let changed = tx.execute(
            "UPDATE devices SET config = coalesce(?2, config), config_sections = ?3,
                     use_global_config = ?4
                 WHERE device_id = ?1",
            params![
                self.device.0,
                self.config,
                self.config_sections,
                i64::from(self.use_global_config)
            ],
        )?;
        if changed == 0 {
            return Err(StoreError::UnknownDevice(self.device));
        }
        Ok(())
    }
}
