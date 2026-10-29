//! `device_logs`: the device's `log` lines and the controller's lines about the device.
//!
//! | Column | Type | Encoding |
//! |---|---|---|
//! | `id` | INTEGER | row id |
//! | `device_id` | TEXT | the device serial (a `devices` row must exist) |
//! | `ts` | INTEGER | wall **milliseconds** since the epoch |
//! | `level` | TEXT | [`LogLevel`] |
//! | `source` | TEXT | [`LogSource`] |
//! | `message` | TEXT | the line as written, never NULL |
//!
//! Each device keeps its newest 10,000 lines: one device of the deployed database holds
//! exactly 10,000 and every other fewer, the only evidence of the limit.

use echomuse_core::types::DeviceId;
use echomuse_db::WriteCommand;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::{
    error::RecordError,
    values::UnixMillis,
    vocab::{LogLevel, LogSource},
};

/// Lines kept per device.
pub const DEVICE_LOG_RETENTION: u32 = 10_000;

/// One log line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceLogEntry {
    /// `device_id`.
    pub device: DeviceId,
    /// `ts`.
    pub ts: UnixMillis,
    /// `level`.
    pub level: LogLevel,
    /// `source`.
    pub source: LogSource,
    /// `message`.
    pub message: String,
}

/// Appends a line, then keeps the device's newest `retention` lines. Returns the row id.
#[derive(Clone, Debug)]
pub struct InsertDeviceLog {
    /// The line.
    pub entry: DeviceLogEntry,
    /// See [`DEVICE_LOG_RETENTION`].
    pub retention: u32,
}

impl InsertDeviceLog {
    /// Appends with the standard retention.
    pub fn new(entry: DeviceLogEntry) -> Self {
        Self {
            entry,
            retention: DEVICE_LOG_RETENTION,
        }
    }
}

impl WriteCommand for InsertDeviceLog {
    type Output = i64;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<i64, RecordError> {
        let e = &self.entry;
        tx.execute(
            "INSERT INTO device_logs (device_id, ts, level, source, message) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                e.device.0,
                e.ts.0,
                e.level.as_str(),
                e.source.as_str(),
                e.message
            ],
        )?;
        let id = tx.last_insert_rowid();
        // The oldest line still kept, found through the (device_id, ts DESC) index; newest
        // means latest `ts`, then latest id.
        let cutoff: Option<(i64, i64)> = tx
            .query_row(
                "SELECT ts, id FROM device_logs WHERE device_id = ?1 \
                 ORDER BY ts DESC, id DESC LIMIT 1 OFFSET ?2",
                params![e.device.0, i64::from(self.retention.max(1)) - 1],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((ts, oldest)) = cutoff {
            tx.execute(
                "DELETE FROM device_logs WHERE device_id = ?1 \
                 AND (ts < ?2 OR (ts = ?2 AND id < ?3))",
                params![e.device.0, ts, oldest],
            )?;
        }
        Ok(id)
    }
}
