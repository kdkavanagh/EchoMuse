//! The device runtimes as the dashboard's device routes use them (§5.4): live state read
//! from each runtime's published snapshot, and the named commands approve, delete and
//! settings changes.
//!
//! Deleting a device follows §5.7 step 6: its runtime is retired first (admission and
//! routing revoked, the runtime and its listeners stopped), its ESPHome records are
//! withdrawn, then the device services drop its pending work (a Wi-Fi scan still waiting
//! fails, its Wi-Fi state and log allowance go, its recording-mode recorders finish what
//! they hold, and its capture window ends), and only then are its rows, its recordings
//! and recording-mode files and its session recordings (with its recorder grant) deleted.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use echomuse_api::{DeviceLabel, DeviceRuntimes, LiveDevice, RuntimeError, SettingsApplied};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, WriteCommand, devices::DeviceError};
use echomuse_discovery::{DiscoveryHandle, EsphomeNodeKind};
use rusqlite::{Transaction, params};

use crate::{
    app::{ApproveError, Registry},
    dashboard::live_device,
    device_events::{DeviceEvent, DeviceEvents},
    taps::DeviceTaps,
};

/// Tables that keep rows for a device, and the column naming it. `devices` goes last:
/// `device_logs` refers to it.
const DEVICE_TABLES: [(&str, &str); 10] = [
    ("device_logs", "device_id"),
    ("device_metrics", "device_id"),
    ("near_misses", "device_id"),
    ("turns", "device_id"),
    ("wake_counters", "device_id"),
    ("wake_shadow", "device_id"),
    ("wake_shadow_events", "device_id"),
    // The alert journal and delivery cursor name the speaker as `endpoint_id`.
    ("alert_ops", "endpoint_id"),
    ("alert_delivery", "endpoint_id"),
    ("devices", "device_id"),
];

/// Directories beside the database holding files named `<device>_<row id>…`.
const RECORDING_DIRS: [&str; 3] = ["recordings", "turn_recordings", "nearmiss_recordings"];
/// Directories beside the database holding one subdirectory per device.
const DEVICE_DIRS: [&str; 3] = ["wakes", "samples", "ambient"];

/// [`DeviceRuntimes`] over the registry.
pub struct ControllerRuntimes {
    registry: Registry,
    db: DbHandle,
    discovery: DiscoveryHandle,
    data_dir: PathBuf,
    events: DeviceEvents,
    /// The device services holding per-device state.
    taps: DeviceTaps,
}

impl ControllerRuntimes {
    pub fn new(
        registry: Registry,
        db: DbHandle,
        discovery: DiscoveryHandle,
        data_dir: PathBuf,
        events: DeviceEvents,
        taps: DeviceTaps,
    ) -> Self {
        Self {
            registry,
            db,
            discovery,
            data_dir,
            events,
            taps,
        }
    }

    async fn delete_retired(&self, device: &DeviceId) -> Result<(), RuntimeError> {
        if let Err(error) = self.registry.retire(device.clone()).await {
            if error == echomuse_core::types::RuntimeError::Closed {
                return Err(RuntimeError::Unavailable);
            }
            // The runtime is out of the directory and stopped as far as it could be.
            tracing::warn!(%device, %error, "the deleted device's runtime did not stop cleanly");
        }
        for kind in [
            EsphomeNodeKind::VoiceSatellite,
            EsphomeNodeKind::BluetoothProxy,
        ] {
            if let Err(error) = self
                .discovery
                .unregister_esphome(device.clone(), kind)
                .await
            {
                return Err(RuntimeError::Failed(format!(
                    "the ESPHome record of {device} was not withdrawn: {error}"
                )));
            }
        }
        // Pending work is dropped before retained data goes.
        self.taps.wifi.forget(device);
        self.taps.logs.forget(device);
        self.taps.recording.forget(device).await;
        self.taps.capture.forget(device);
        let found = self
            .db
            .write(DeleteDevice {
                device: device.clone(),
            })
            .await
            .map_err(|error| RuntimeError::Failed(format!("deleting {device}: {error}")))?;
        if !found {
            return Err(RuntimeError::UnknownDevice);
        }
        let data_dir = self.data_dir.clone();
        let target = device.clone();
        tokio::task::spawn_blocking(move || delete_files(&data_dir, &target))
            .await
            .map_err(|error| RuntimeError::Failed(format!("deleting files: {error}")))?
            .map_err(|error| {
                RuntimeError::Failed(format!("deleting the files of {device}: {error}"))
            })?;
        self.taps.recorder.forget(device).await;
        Ok(())
    }
}

impl DeviceRuntimes for ControllerRuntimes {
    fn live(&self, device: &DeviceId) -> Option<LiveDevice> {
        self.registry
            .directory()
            .get(device)
            .map(|handle| live_device(&handle.snapshot()))
    }

    async fn approve(&self, device: DeviceId, label: DeviceLabel) -> Result<(), RuntimeError> {
        match self
            .registry
            .approve(device.clone(), label.into_string())
            .await
        {
            Ok(_) => {
                // A relabel of an approved device changes its alert endpoint.
                self.events.send(DeviceEvent::Changed(device)).await;
                Ok(())
            }
            Err(ApproveError::UnknownDevice) => Err(RuntimeError::UnknownDevice),
            Err(ApproveError::Runtime(echomuse_core::types::RuntimeError::Closed)) => {
                Err(RuntimeError::Unavailable)
            }
            Err(error) => Err(RuntimeError::Failed(error.to_string())),
        }
    }

    /// §5.7 step 6: admission and routing are revoked first, then the runtime and its
    /// listeners stop and the records are withdrawn, and only then is retained data
    /// deleted, so a late write or reconnect cannot recreate the device meanwhile.
    async fn delete(&self, device: DeviceId) -> Result<(), RuntimeError> {
        let result = self.delete_retired(&device).await;
        self.registry.release(device).await;
        result
    }

    /// The firmware's `config` message is not composed by this controller yet, so a
    /// changed setting reaches a device only through a new session: each connected
    /// device in `new_session` is disconnected and dials again, receiving a fresh
    /// `session.ready`. Changes outside `new_session` reach no device.
    async fn settings_applied(&self, change: SettingsApplied) -> Vec<DeviceId> {
        // The alert endpoints take the timer ring settings.
        for device in &change.changed {
            self.events.send(DeviceEvent::Changed(device.clone())).await;
        }
        let directory = self.registry.directory();
        let mut reached = Vec::new();
        for device in change.new_session {
            let Some(handle) = directory.get(&device) else {
                continue;
            };
            match handle.renegotiate().await {
                Ok(true) => reached.push(device),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(%device, %error, "the session could not be renegotiated");
                }
            }
        }
        reached
    }

    async fn relabeled(&self, device: DeviceId, _label: String) {
        // The alert endpoint reads the stored label; ESPHome nodes take the new one at
        // the device's next session.
        self.events.send(DeviceEvent::Changed(device)).await;
    }
}

/// Deletes a device's row and every row kept for it, in one transaction. `false` when
/// there was no such device.
struct DeleteDevice {
    device: DeviceId,
}

impl WriteCommand for DeleteDevice {
    type Output = bool;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, DeviceError> {
        let mut removed_device = false;
        for (table, column) in DEVICE_TABLES {
            let removed = tx.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                params![self.device.0],
            )?;
            if table == "devices" {
                removed_device = removed > 0;
            }
        }
        Ok(removed_device)
    }
}

/// Whether `name` is one of the device's recordings: `<device>_` then the row id's
/// digits, then an extension (`.wav`, or a sidecar or partial file beside it).
fn is_recording_of(name: &str, device: &DeviceId) -> bool {
    name.strip_prefix(device.0.as_str())
        .and_then(|rest| rest.strip_prefix('_'))
        .and_then(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0).then(|| &rest[digits..])
        })
        .is_some_and(|rest| rest.starts_with('.'))
}

/// A device id that names exactly one directory entry.
fn is_path_component(device: &DeviceId) -> bool {
    let id = device.0.as_str();
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\', '\0'])
}

fn delete_files(data_dir: &Path, device: &DeviceId) -> io::Result<()> {
    for dir in RECORDING_DIRS {
        let entries = match fs::read_dir(data_dir.join(dir)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|name| is_recording_of(name, device))
            {
                fs::remove_file(entry.path())?;
            }
        }
    }
    if !is_path_component(device) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the device id is not a single path component; its directories were left",
        ));
    }
    for dir in DEVICE_DIRS {
        match fs::remove_dir_all(data_dir.join(dir).join(&device.0)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_devices_recordings_match() {
        let device = DeviceId("G0SYNTH01".to_owned());
        assert!(is_recording_of("G0SYNTH01_12.wav", &device));
        assert!(is_recording_of("G0SYNTH01_7.wav.part", &device));
        assert!(!is_recording_of("G0SYNTH01_.wav", &device));
        assert!(!is_recording_of("G0SYNTH012_7.wav", &device));
        assert!(!is_recording_of("G0SYNTH01_7x.wav", &device));
        assert!(!is_recording_of("G0SYNTH01_7", &device));
        assert!(!is_recording_of("other_7.wav", &device));
    }

    #[test]
    fn device_ids_that_leave_their_directory_are_refused() {
        for id in ["", ".", "..", "a/b", "..\\x"] {
            assert!(!is_path_component(&DeviceId(id.to_owned())), "{id:?}");
        }
        assert!(is_path_component(&DeviceId("G0SYNTH01".to_owned())));
    }
}
