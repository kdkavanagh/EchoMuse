//! What firmware delivery keeps in the `devices` table (spec §3.5.1, §4.10):
//! `update_queued_at`, `firmware_previous` and the link token.
//!
//! [`FirmwareStore`] is implemented here for the database handle, through the commands
//! below. Only the named columns are written; every other column keeps its value.

use std::fmt;

use echomuse_core::types::DeviceId;
use echomuse_db::devices::{DeviceError, EnsureDeviceToken, UnixSeconds};
use echomuse_db::{DbHandle, ServiceError, WriteCommand};
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::types::{FirmwareVersion, LinkToken};

/// Why the store could not answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    /// No `devices` row has this id.
    UnknownDevice(DeviceId),
    /// The database failed or is closed; the text says how.
    Unavailable(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice(device) => write!(f, "device {device} is not registered"),
            Self::Unavailable(reason) => write!(f, "the database is unavailable: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The firmware-related device state the service reads and writes. Implemented for
/// [`DbHandle`]; tests may implement it in memory.
pub trait FirmwareStore: Send + Sync + 'static {
    /// Queues an install of the bundled firmware for the device's next connection. A
    /// queue already set keeps its time. Returns the stored time.
    fn queue_update(
        &self,
        device: &DeviceId,
        at: UnixSeconds,
    ) -> impl Future<Output = Result<UnixSeconds, StoreError>> + Send;

    /// Clears the device's queued install and returns when it was queued, in one step, so
    /// two connections cannot both start it.
    fn take_queued_update(
        &self,
        device: &DeviceId,
    ) -> impl Future<Output = Result<Option<UnixSeconds>, StoreError>> + Send;

    /// Records the version in the device's rollback slot; None when it is unknown or holds
    /// no usable build.
    fn set_firmware_previous(
        &self,
        device: &DeviceId,
        version: Option<&FirmwareVersion>,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The device's link token, minting one (and a pending row) if it has none.
    fn ensure_link_token(
        &self,
        device: &DeviceId,
    ) -> impl Future<Output = Result<LinkToken, StoreError>> + Send;
}

/// A command's failure.
#[derive(Debug)]
pub enum FirmwareRowError {
    /// No `devices` row has this id.
    UnknownDevice(DeviceId),
    /// SQLite failed.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for FirmwareRowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice(device) => write!(f, "device {device} is not registered"),
            Self::Sqlite(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FirmwareRowError {}

impl From<rusqlite::Error> for FirmwareRowError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

fn queued_at(
    conn: &Connection,
    device: &DeviceId,
) -> Result<Option<UnixSeconds>, FirmwareRowError> {
    let stored: Option<Option<i64>> = conn
        .prepare_cached("SELECT update_queued_at FROM devices WHERE device_id = ?1")?
        .query_row([&device.0], |row| row.get(0))
        .optional()?;
    let stored = stored.ok_or_else(|| FirmwareRowError::UnknownDevice(device.clone()))?;
    Ok(stored.map(UnixSeconds))
}

/// Sets `update_queued_at` unless already set; outputs the stored value.
#[derive(Clone, Debug)]
pub struct QueueUpdate {
    /// The device.
    pub device: DeviceId,
    /// When the install was queued.
    pub at: UnixSeconds,
}

impl WriteCommand for QueueUpdate {
    type Output = UnixSeconds;
    type Error = FirmwareRowError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, Self::Error> {
        tx.prepare_cached(
            "UPDATE devices SET update_queued_at = coalesce(update_queued_at, ?2) WHERE device_id = ?1",
        )?
        .execute(rusqlite::params![self.device.0, self.at.0])?;
        Ok(queued_at(tx, &self.device)?.unwrap_or(self.at))
    }
}

/// Clears `update_queued_at`; outputs the value it held.
#[derive(Clone, Debug)]
pub struct TakeQueuedUpdate {
    /// The device.
    pub device: DeviceId,
}

impl WriteCommand for TakeQueuedUpdate {
    type Output = Option<UnixSeconds>;
    type Error = FirmwareRowError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, Self::Error> {
        let held = queued_at(tx, &self.device)?;
        if held.is_some() {
            tx.prepare_cached("UPDATE devices SET update_queued_at = NULL WHERE device_id = ?1")?
                .execute([&self.device.0])?;
        }
        Ok(held)
    }
}

/// Sets `firmware_previous`.
#[derive(Clone, Debug)]
pub struct SetFirmwarePrevious {
    /// The device.
    pub device: DeviceId,
    /// The rollback slot's version; None when unknown or unusable.
    pub version: Option<FirmwareVersion>,
}

impl WriteCommand for SetFirmwarePrevious {
    type Output = ();
    type Error = FirmwareRowError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), Self::Error> {
        let changed = tx
            .prepare_cached("UPDATE devices SET firmware_previous = ?2 WHERE device_id = ?1")?
            .execute(rusqlite::params![
                self.device.0,
                self.version.as_ref().map(FirmwareVersion::as_str)
            ])?;
        if changed == 0 {
            return Err(FirmwareRowError::UnknownDevice(self.device));
        }
        Ok(())
    }
}

fn row_error(error: ServiceError<FirmwareRowError>) -> StoreError {
    match error {
        ServiceError::Failed(FirmwareRowError::UnknownDevice(device)) => {
            StoreError::UnknownDevice(device)
        }
        other => StoreError::Unavailable(other.to_string()),
    }
}

impl FirmwareStore for DbHandle {
    async fn queue_update(
        &self,
        device: &DeviceId,
        at: UnixSeconds,
    ) -> Result<UnixSeconds, StoreError> {
        self.write(QueueUpdate {
            device: device.clone(),
            at,
        })
        .await
        .map_err(row_error)
    }

    async fn take_queued_update(
        &self,
        device: &DeviceId,
    ) -> Result<Option<UnixSeconds>, StoreError> {
        self.write(TakeQueuedUpdate {
            device: device.clone(),
        })
        .await
        .map_err(row_error)
    }

    async fn set_firmware_previous(
        &self,
        device: &DeviceId,
        version: Option<&FirmwareVersion>,
    ) -> Result<(), StoreError> {
        self.write(SetFirmwarePrevious {
            device: device.clone(),
            version: version.cloned(),
        })
        .await
        .map_err(row_error)
    }

    async fn ensure_link_token(&self, device: &DeviceId) -> Result<LinkToken, StoreError> {
        self.write(EnsureDeviceToken {
            device: device.clone(),
        })
        .await
        .map(|secret| LinkToken::new(secret.expose().to_owned()))
        .map_err(|error| match error {
            ServiceError::Failed(DeviceError::UnknownDevice(device)) => {
                StoreError::UnknownDevice(device)
            }
            other => StoreError::Unavailable(other.to_string()),
        })
    }
}
