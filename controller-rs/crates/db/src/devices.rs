//! The `devices` table as device admission uses it (§3.1.2, §3.1.4, §3.3.1, §4.10), and the
//! approval and ESPHome port settings in `system_config`.
//!
//! Each command is a struct run through [`DbHandle`](crate::DbHandle): queries with
//! [`read`](crate::DbHandle::read), changes with [`write`](crate::DbHandle::write).
//! Columns these commands do not name are never written, so their stored values stay.

use std::{
    fmt,
    net::IpAddr,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use echomuse_core::types::DeviceId;
use rusqlite::{Connection, OptionalExtension, Row, Transaction};

use crate::{
    contract::SystemConfigKey,
    service::{ReadQuery, WriteCommand},
};

/// The first ESPHome voice satellite port (§3.3.1).
pub(crate) const ESPHOME_FIRST_PORT: u16 = 16001;
/// A device's Bluetooth proxy listens on its voice port plus this (§3.3.2).
const BLE_PROXY_PORT_OFFSET: u16 = 1000;
/// Random bytes in a link token; URL-safe base64 without padding makes 43 characters,
/// the length of the tokens already stored.
const TOKEN_BYTES: usize = 32;

/// Whole seconds since the Unix epoch, as `first_seen`, `last_seen` and
/// `update_queued_at` store them.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixSeconds(pub i64);

impl UnixSeconds {
    /// The time rounded down to a whole second; None before the epoch or past `i64`.
    pub fn from_system_time(time: SystemTime) -> Option<Self> {
        let seconds = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
        i64::try_from(seconds).ok().map(Self)
    }
}

/// A stored credential. Its `Debug` output never shows the value.
#[derive(Clone, Eq, PartialEq)]
pub struct Secret(String);

impl Secret {
    /// The value, for comparing or sending. Compare a presented link token in constant
    /// time (§3.1.2).
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// How a device that has never been approved is admitted (§4.10): the
/// `device_approval` system setting, else the deployment's `DEVICE_APPROVAL`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApprovalMode {
    /// An administrator approves each new device.
    Strict,
    /// New devices are approved as they first connect.
    Auto,
}

impl ApprovalMode {
    /// The value exactly as stored and configured.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Auto => "auto",
        }
    }

    /// `strict` or `auto`; None for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "strict" => Some(Self::Strict),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// One `devices` row, every column.
///
/// `config` and `config_sections` are the stored JSON text (an object of setting keys,
/// and an array of section ids); the configuration code parses them. Integer flags are
/// read as nonzero = true.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRow {
    pub device_id: DeviceId,
    /// None: never labelled (a pending device).
    pub label: Option<String>,
    pub approved: bool,
    /// The address of the latest connection attempt; None before the first one.
    pub ip: Option<String>,
    /// `firmware_ver`: the firmware version the latest attempt reported.
    pub firmware_version: Option<String>,
    pub firmware_previous: Option<String>,
    /// None for a row created by a credential push before the device ever connected.
    pub first_seen: Option<UnixSeconds>,
    pub last_seen: Option<UnixSeconds>,
    pub config: String,
    /// None: no voice satellite port assigned yet.
    pub esphome_api_port: Option<u16>,
    pub esphome_noise_psk: Option<Secret>,
    pub use_global_config: bool,
    pub ble_proxy_port: Option<u16>,
    /// The link token; None when none was issued or it was revoked. An empty stored
    /// value reads as None, since it can never authenticate.
    pub token: Option<Secret>,
    pub config_sections: String,
    pub collect_mode: bool,
    pub ambient_mode: bool,
    pub os_version: Option<String>,
    pub update_queued_at: Option<UnixSeconds>,
    pub kiosk_token: Option<Secret>,
}

const DEVICE_COLUMNS: &str = "device_id, label, approved, ip, firmware_ver, firmware_previous, \
     first_seen, last_seen, config, esphome_api_port, esphome_noise_psk, use_global_config, \
     ble_proxy_port, token, config_sections, collect_mode, ambient_mode, os_version, \
     update_queued_at, kiosk_token";

fn device_from_row(row: &Row<'_>) -> rusqlite::Result<DeviceRow> {
    let flag = |index: usize| row.get::<_, i64>(index).map(|v| v != 0);
    let secret = |index: usize| row.get::<_, Option<String>>(index).map(|v| v.map(Secret));
    let seconds = |index: usize| row.get::<_, Option<i64>>(index).map(|v| v.map(UnixSeconds));
    Ok(DeviceRow {
        device_id: DeviceId(row.get(0)?),
        label: row.get(1)?,
        approved: flag(2)?,
        ip: row.get(3)?,
        firmware_version: row.get(4)?,
        firmware_previous: row.get(5)?,
        first_seen: seconds(6)?,
        last_seen: seconds(7)?,
        config: row.get(8)?,
        esphome_api_port: row.get(9)?,
        esphome_noise_psk: secret(10)?,
        use_global_config: flag(11)?,
        ble_proxy_port: row.get(12)?,
        token: row
            .get::<_, Option<String>>(13)?
            .filter(|t| !t.is_empty())
            .map(Secret),
        config_sections: row.get(14)?,
        collect_mode: flag(15)?,
        ambient_mode: flag(16)?,
        os_version: row.get(17)?,
        update_queued_at: seconds(18)?,
        kiosk_token: secret(19)?,
    })
}

fn select_device(conn: &Connection, device: &DeviceId) -> rusqlite::Result<Option<DeviceRow>> {
    conn.prepare_cached(&format!(
        "SELECT {DEVICE_COLUMNS} FROM devices WHERE device_id = ?1"
    ))?
    .query_row([&device.0], device_from_row)
    .optional()
}

/// Why a device command failed. Its transaction rolled back.
#[derive(Debug)]
pub enum DeviceError {
    /// No `devices` row has this id.
    UnknownDevice(DeviceId),
    /// No voice port is left whose Bluetooth proxy port would still be valid.
    PortsExhausted,
    /// A `system_config` value this command needs is not what it can read.
    InvalidSetting {
        key: SystemConfigKey,
        value: Option<String>,
    },
    /// The clock is before 1970, so no time can be stored.
    ClockBeforeEpoch,
    /// The operating system's random source failed.
    Random(getrandom::Error),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice(device) => write!(f, "device {device} is not registered"),
            Self::PortsExhausted => f.write_str("no ESPHome port is left to assign"),
            Self::InvalidSetting { key, value } => {
                write!(f, "system_config {key} holds an unreadable value {value:?}")
            }
            Self::ClockBeforeEpoch => f.write_str("the system clock is before 1970"),
            Self::Random(e) => write!(f, "could not generate a token: {e}"),
            Self::Sqlite(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for DeviceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for DeviceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// The device's row, or None when it is not registered.
#[derive(Clone, Debug)]
pub struct GetDevice {
    pub device: DeviceId,
}

impl ReadQuery for GetDevice {
    type Output = Option<DeviceRow>;
    type Error = DeviceError;

    fn read(self, conn: &Connection) -> Result<Self::Output, Self::Error> {
        Ok(select_device(conn, &self.device)?)
    }
}

/// The device's link token; None when the device is unknown or holds none.
#[derive(Clone, Debug)]
pub struct GetDeviceToken {
    pub device: DeviceId,
}

impl ReadQuery for GetDeviceToken {
    type Output = Option<Secret>;
    type Error = DeviceError;

    fn read(self, conn: &Connection) -> Result<Self::Output, Self::Error> {
        let token: Option<Option<String>> = conn
            .prepare_cached("SELECT token FROM devices WHERE device_id = ?1")?
            .query_row([&self.device.0], |row| row.get(0))
            .optional()?;
        Ok(token.flatten().filter(|t| !t.is_empty()).map(Secret))
    }
}

/// The approval mode: the `device_approval` setting, or `fallback` (the deployment's
/// `DEVICE_APPROVAL`) when the setting is absent, NULL or empty. A stored value other
/// than `strict` or `auto` reads as strict, so an unreadable setting never admits
/// devices on its own.
#[derive(Clone, Copy, Debug)]
pub struct ReadApprovalMode {
    pub fallback: ApprovalMode,
}

impl ReadQuery for ReadApprovalMode {
    type Output = ApprovalMode;
    type Error = DeviceError;

    fn read(self, conn: &Connection) -> Result<Self::Output, Self::Error> {
        let stored = setting(conn, SystemConfigKey::DeviceApproval)?;
        Ok(match stored.as_deref() {
            None | Some("") => self.fallback,
            Some(text) => ApprovalMode::parse(text).unwrap_or(ApprovalMode::Strict),
        })
    }
}

fn setting(conn: &Connection, key: SystemConfigKey) -> rusqlite::Result<Option<String>> {
    let value: Option<Option<String>> = conn
        .prepare_cached("SELECT value FROM system_config WHERE key = ?1")?
        .query_row([key.as_str()], |row| row.get(0))
        .optional()?;
    Ok(value.flatten())
}

/// Record one connection attempt, accepted or not (§3.1.4): the peer address, the
/// hello's firmware and OS versions (None when it sent none) and the time. An unknown
/// device is registered as pending, unlabelled, with the column defaults for everything
/// else (configuration `{}`: it overrides no setting).
#[derive(Clone, Debug)]
pub struct RecordConnectionAttempt {
    pub device: DeviceId,
    pub ip: IpAddr,
    pub firmware_version: Option<String>,
    pub os_version: Option<String>,
    pub at: SystemTime,
}

/// The row after a [`RecordConnectionAttempt`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionAttempt {
    /// True when this attempt created the row.
    pub newly_registered: bool,
    pub device: DeviceRow,
}

impl WriteCommand for RecordConnectionAttempt {
    type Output = ConnectionAttempt;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Self::Output, Self::Error> {
        let at = UnixSeconds::from_system_time(self.at).ok_or(DeviceError::ClockBeforeEpoch)?;
        let existed = select_device(tx, &self.device)?.is_some();
        // A row made by a credential push has no first_seen until the device connects.
        tx.prepare_cached(
            "INSERT INTO devices (device_id, approved, ip, firmware_ver, os_version, first_seen, last_seen)
             VALUES (?1, 0, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT (device_id) DO UPDATE SET
                 ip = excluded.ip,
                 firmware_ver = excluded.firmware_ver,
                 os_version = excluded.os_version,
                 last_seen = excluded.last_seen,
                 first_seen = coalesce(devices.first_seen, excluded.first_seen)",
        )?
        .execute(rusqlite::params![
            self.device.0,
            self.ip.to_string(),
            self.firmware_version,
            self.os_version,
            at.0,
        ])?;
        let device = select_device(tx, &self.device)?
            .ok_or_else(|| DeviceError::UnknownDevice(self.device.clone()))?;
        Ok(ConnectionAttempt {
            newly_registered: !existed,
            device,
        })
    }
}

/// Approve a registered device under `label`. Its configuration is unchanged.
#[derive(Clone, Debug)]
pub struct ApproveDevice {
    pub device: DeviceId,
    pub label: String,
}

impl WriteCommand for ApproveDevice {
    type Output = ();
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), DeviceError> {
        let changed = tx
            .prepare_cached("UPDATE devices SET approved = 1, label = ?2 WHERE device_id = ?1")?
            .execute([&self.device.0, &self.label])?;
        if changed == 0 {
            return Err(DeviceError::UnknownDevice(self.device));
        }
        Ok(())
    }
}

/// The device's link token, minting one if it has none. Provisioning pushes credentials
/// before a device first connects (§3.2), so an unknown device gets a pending row with no
/// address and no seen times. Approval is untouched.
#[derive(Clone, Debug)]
pub struct EnsureDeviceToken {
    pub device: DeviceId,
}

impl WriteCommand for EnsureDeviceToken {
    type Output = Secret;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Secret, DeviceError> {
        tx.prepare_cached(
            "INSERT INTO devices (device_id) VALUES (?1) ON CONFLICT (device_id) DO NOTHING",
        )?
        .execute([&self.device.0])?;
        let existing = GetDeviceToken {
            device: self.device.clone(),
        }
        .read(tx)?;
        if let Some(token) = existing {
            return Ok(token);
        }
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(DeviceError::Random)?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        tx.prepare_cached("UPDATE devices SET token = ?2 WHERE device_id = ?1")?
            .execute([&self.device.0, &token])?;
        Ok(Secret(token))
    }
}

/// The device's ESPHome voice satellite port, assigning one if it has none (§3.3.1).
///
/// Ports are handed out in increasing order from `next_esphome_port` and never reused,
/// so HA's config entry for a removed device can never reach a different one. The new
/// port is also above every port already assigned, should the setting lag behind. Ports
/// stop where the Bluetooth proxy port would no longer fit.
#[derive(Clone, Debug)]
pub struct EnsureEsphomePort {
    pub device: DeviceId,
}

impl WriteCommand for EnsureEsphomePort {
    type Output = u16;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<u16, DeviceError> {
        let assigned: Option<Option<u16>> = tx
            .prepare_cached("SELECT esphome_api_port FROM devices WHERE device_id = ?1")?
            .query_row([&self.device.0], |row| row.get(0))
            .optional()?;
        match assigned {
            None => return Err(DeviceError::UnknownDevice(self.device)),
            Some(Some(port)) => return Ok(port),
            Some(None) => {}
        }

        let key = SystemConfigKey::NextEsphomePort;
        let next = match setting(tx, key)? {
            None => u32::from(ESPHOME_FIRST_PORT),
            Some(text) => text
                .trim()
                .parse::<u32>()
                .map_err(|_| DeviceError::InvalidSetting {
                    key,
                    value: Some(text.clone()),
                })?,
        };
        let highest: Option<i64> =
            tx.query_row("SELECT max(esphome_api_port) FROM devices", [], |row| {
                row.get(0)
            })?;
        let above_highest = highest.map_or(0, |port| port.saturating_add(1));
        let port = i64::from(next)
            .max(above_highest)
            .max(i64::from(ESPHOME_FIRST_PORT));
        let port = u16::try_from(port)
            .ok()
            .filter(|port| *port <= u16::MAX - BLE_PROXY_PORT_OFFSET)
            .ok_or(DeviceError::PortsExhausted)?;
        tx.prepare_cached("UPDATE devices SET esphome_api_port = ?2 WHERE device_id = ?1")?
            .execute(rusqlite::params![self.device.0, port])?;
        tx.prepare_cached(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?
        .execute([key.as_str(), &(u32::from(port) + 1).to_string()])?;
        Ok(port)
    }
}

/// The device's Bluetooth proxy port, its voice port plus 1000,
/// stored on first use (§3.3.2). None while the device has no voice port.
#[derive(Clone, Debug)]
pub struct EnsureBleProxyPort {
    pub device: DeviceId,
}

impl WriteCommand for EnsureBleProxyPort {
    type Output = Option<u16>;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Option<u16>, DeviceError> {
        let ports: Option<(Option<u16>, Option<u16>)> = tx
            .prepare_cached(
                "SELECT esphome_api_port, ble_proxy_port FROM devices WHERE device_id = ?1",
            )?
            .query_row([&self.device.0], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let Some((voice, stored)) = ports else {
            return Err(DeviceError::UnknownDevice(self.device));
        };
        let Some(voice) = voice else {
            return Ok(None);
        };
        let port = voice
            .checked_add(BLE_PROXY_PORT_OFFSET)
            .ok_or(DeviceError::PortsExhausted)?;
        if stored != Some(port) {
            tx.prepare_cached("UPDATE devices SET ble_proxy_port = ?2 WHERE device_id = ?1")?
                .execute(rusqlite::params![self.device.0, port])?;
        }
        Ok(Some(port))
    }
}
