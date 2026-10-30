//! The device runtimes as the dashboard's device routes use them (§5.4): live state read
//! from each runtime's published snapshot, and the named commands approve, delete and
//! settings changes.
//!
//! Deleting a device follows §5.7 step 6: its runtime is retired first (admission and
//! routing revoked, the runtime and its listeners stopped), its ESPHome records are
//! withdrawn, then the device services drop its pending work (a Wi-Fi scan still waiting
//! fails, its Wi-Fi state, log allowance and capture window go). Then each owner deletes
//! what it keeps: the records service its rows and recordings (closing the device's
//! records to late writers until a runtime starts for it again), the recording modes their
//! files once their recorders finish, and the session recorder its grant and recordings.
//! Its alert journal and its `devices` row go last, so a deletion that failed part way can
//! be asked for again.

use echomuse_api::{
    BoxFuture, DeviceLabel, DeviceRuntimes, LiveDevice, RuntimeError, SettingsApplied,
};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, WriteCommand, devices::DeviceError};
use echomuse_discovery::{DiscoveryHandle, EsphomeNodeKind};
use echomuse_records::RecordsService;
use rusqlite::Transaction;

use crate::{
    app::{ApproveError, Registry},
    dashboard::live_device,
    device_events::{DeviceEvent, DeviceEvents},
    taps::DeviceTaps,
};

/// [`DeviceRuntimes`] over the registry.
pub struct ControllerRuntimes {
    registry: Registry,
    db: DbHandle,
    discovery: DiscoveryHandle,
    records: RecordsService,
    events: DeviceEvents,
    /// The device services holding per-device state.
    taps: DeviceTaps,
}

impl ControllerRuntimes {
    pub fn new(
        registry: Registry,
        db: DbHandle,
        discovery: DiscoveryHandle,
        records: RecordsService,
        events: DeviceEvents,
        taps: DeviceTaps,
    ) -> Self {
        Self {
            registry,
            db,
            discovery,
            records,
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
        self.taps.capture.forget(device);
        self.records.delete_device(device).await.map_err(|error| {
            RuntimeError::Failed(format!("deleting the records of {device}: {error}"))
        })?;
        // Mode edits wait until the row is gone, so one queued meanwhile finds no device
        // rather than arming modes again in memory.
        let _mode_edits = self
            .taps
            .recording
            .delete_device(device)
            .await
            .map_err(|error| {
                RuntimeError::Failed(format!(
                    "deleting the recording-mode files of {device}: {error}"
                ))
            })?;
        self.taps
            .recorder
            .delete_device(device)
            .await
            .map_err(|error| {
                RuntimeError::Failed(format!(
                    "deleting the session recordings of {device}: {error}"
                ))
            })?;
        // The row goes last, so a deletion that failed above can be asked for again.
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

    fn approve(
        &self,
        device: DeviceId,
        label: DeviceLabel,
    ) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move {
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
        })
    }

    /// §5.7 step 6: admission and routing are revoked first, then the runtime and its
    /// listeners stop and the records are withdrawn, and only then is retained data
    /// deleted, so a late write or reconnect cannot recreate the device meanwhile.
    fn delete(&self, device: DeviceId) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(async move {
            let result = self.delete_retired(&device).await;
            self.registry.release(device).await;
            result
        })
    }

    /// The firmware's `config` message is not composed by this controller yet, so a
    /// changed setting reaches a device only through a new session: each connected
    /// device in `new_session` is disconnected and dials again, receiving a fresh
    /// `session.ready`. Changes outside `new_session` reach no device.
    fn settings_applied(&self, change: SettingsApplied) -> BoxFuture<'_, Vec<DeviceId>> {
        Box::pin(async move {
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
        })
    }

    fn relabeled(&self, device: DeviceId, _label: String) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // The alert endpoint reads the stored label; ESPHome nodes take the new one at
            // the device's next session.
            self.events.send(DeviceEvent::Changed(device)).await;
        })
    }
}

/// Deletes a device's alert journal and delivery cursor (which name the speaker as
/// `endpoint_id`) and then its `devices` row, in one transaction; the records service has
/// already deleted the rows it writes. `false` when there was no such device.
struct DeleteDevice {
    device: DeviceId,
}

impl WriteCommand for DeleteDevice {
    type Output = bool;
    type Error = DeviceError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, DeviceError> {
        let id = [&self.device.0];
        tx.execute("DELETE FROM alert_ops WHERE endpoint_id = ?1", id)?;
        tx.execute("DELETE FROM alert_delivery WHERE endpoint_id = ?1", id)?;
        Ok(tx.execute("DELETE FROM devices WHERE device_id = ?1", id)? > 0)
    }
}
