//! The dispatcher: one task on the shared runtime that hands device events
//! ([`DeviceEvent`]) to the alert service and firmware delivery, and the alert
//! service's notices to the dashboard.
//!
//! Every approved device is an alert endpoint. Its [`EndpointConfig`] is built from its
//! row and settings:
//!
//! - **label**: the row's label, or the device id while it has none;
//! - **area**: `None`. The satellite's Home Assistant area comes from HA's device
//!   registry, which this controller does not read yet, so the relay's speaker matching
//!   by area has nothing to match;
//! - **alarm and timer sounds**: the device's effective `alarmSound` and `timerSound`
//!   resolved through the sound catalog ([`CatalogSnapshot::endpoint_sounds`]). A choice
//!   the catalog cannot ring (a missing or unexported sound) rings the firmware's
//!   built-in tone, [`AlertSound::Fallback`](echomuse_protocol::alerts::AlertSound)
//!   (configuration.md "Sounds"). A settings change reaches the service as
//!   [`DeviceEvent::Changed`]; a catalog change (an upload finishing its export, a
//!   deletion) updates every registered speaker whose sounds it changes;
//! - **timer loop gap and ring limit**: the device's effective `timerRingGapSeconds`
//!   and `timerRingSeconds`.

use std::{collections::HashMap, num::NonZeroU64, sync::Arc};

use echomuse_alerts::{
    Act, AlertHandle, DeviceAttach, EndpointConfig, OpId, RingRef,
    wire::{ActAction, ActSource, AlertInbound},
};
use echomuse_api::{AlertNotice as DashboardNotice, ApiEvent, EventPublisher};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, devices::GetDevice};
use echomuse_firmware::{DeviceHello, FirmwareVersion};
use echomuse_presentation::sounds::{catalog::CatalogSnapshot, service::SoundHandle};
use echomuse_settings::service::SettingsHandle;
use tokio::sync::{broadcast, mpsc, watch};

use crate::{
    app::Directory,
    device_events::{DeviceEvent, ReadySession},
    firmware_control::{Firmware, device_target},
    supervision::TaskSupervisor,
};

/// What the dispatcher serves events with.
pub struct Dispatcher {
    pub alerts: AlertHandle,
    pub firmware: Firmware,
    pub db: DbHandle,
    pub settings: SettingsHandle,
    /// The sound catalog the alarm and timer choices are resolved in.
    pub sounds: SoundHandle,
    pub directory: watch::Receiver<Directory>,
    /// Spawns the work the dispatcher starts; a panic in it stops the process.
    pub tasks: TaskSupervisor,
}

/// The alert endpoint of a device, as the module documentation describes.
async fn endpoint(
    db: &DbHandle,
    settings: &SettingsHandle,
    catalog: &CatalogSnapshot,
    device: &DeviceId,
) -> Option<EndpointConfig> {
    let row = match db
        .read(GetDevice {
            device: device.clone(),
        })
        .await
    {
        Ok(Some(row)) if row.approved => row,
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(%device, %error, "the device's alert endpoint could not be read");
            return None;
        }
    };
    let effective = settings.effective(device);
    let timers = &effective.timers;
    let gap_ms = (timers.timer_ring_gap_seconds.get() * 1000.0).round() as u64;
    let max_ring_ms = u64::from(timers.timer_ring_seconds.get()) * 1000;
    let sounds = catalog.endpoint_sounds(timers);
    Some(EndpointConfig {
        device: device.clone(),
        label: row.label.unwrap_or_else(|| device.0.clone()),
        area: None,
        alarm_sound: sounds.alarm.sound,
        timer_sound: sounds.timer.sound,
        timer_loop_gap_ms: gap_ms,
        timer_max_ring_ms: NonZeroU64::new(max_ring_ms)?,
    })
}

impl Dispatcher {
    /// Serves events until every sender is gone or the alert service stops.
    pub async fn run(self, mut events: mpsc::Receiver<DeviceEvent>) {
        // The configuration each registered speaker was last given.
        let mut registered = HashMap::new();
        let mut catalog = self.sounds.subscribe();
        let mut catalog_open = true;
        loop {
            let served = tokio::select! {
                event = events.recv() => match event {
                    Some(event) => self.handle(event, &mut registered).await,
                    None => return,
                },
                changed = catalog.changed(), if catalog_open => match changed {
                    Ok(()) => {
                        let snapshot = catalog.borrow_and_update().clone();
                        self.catalog_changed(&snapshot, &mut registered).await
                    }
                    Err(_) => {
                        // The catalog stopped with the controller; nothing changes now.
                        catalog_open = false;
                        Ok(())
                    }
                },
            };
            if served.is_err() {
                tracing::warn!("the alert service stopped; device events are no longer served");
                return;
            }
        }
    }

    async fn configure(&self, device: &DeviceId) -> Option<EndpointConfig> {
        let catalog = self.sounds.snapshot();
        endpoint(&self.db, &self.settings, &catalog, device).await
    }

    /// Updates every registered speaker whose sounds the new catalog resolves
    /// differently.
    async fn catalog_changed(
        &self,
        catalog: &Arc<CatalogSnapshot>,
        registered: &mut HashMap<DeviceId, EndpointConfig>,
    ) -> Result<(), echomuse_alerts::ServiceStopped> {
        let devices: Vec<DeviceId> = registered.keys().cloned().collect();
        for device in devices {
            let Some(config) = endpoint(&self.db, &self.settings, catalog, &device).await else {
                continue;
            };
            if registered.get(&device) != Some(&config) {
                tracing::info!(%device, alarm = ?config.alarm_sound, timer = ?config.timer_sound,
                    "the sound catalog changed the speaker's alert sounds");
                registered.insert(device, config.clone());
                self.alerts.update(config)?;
            }
        }
        Ok(())
    }

    async fn handle(
        &self,
        event: DeviceEvent,
        registered: &mut HashMap<DeviceId, EndpointConfig>,
    ) -> Result<(), echomuse_alerts::ServiceStopped> {
        match event {
            DeviceEvent::Registered(device) => {
                if let Some(config) = self.configure(&device).await {
                    registered.insert(device, config.clone());
                    self.alerts.register(config)?;
                }
            }
            DeviceEvent::Changed(device) => {
                if let Some(config) = self.configure(&device).await {
                    if let Some(known) = registered.get_mut(&device) {
                        *known = config.clone();
                    }
                    self.alerts.update(config)?;
                }
            }
            DeviceEvent::Deregistered(device) => {
                registered.remove(&device);
                self.alerts.remove(device)?;
            }
            DeviceEvent::SessionReady(ready) => {
                let ReadySession {
                    device,
                    session,
                    link: _,
                    platform,
                    capabilities,
                    alerts,
                    firmware_version,
                } = *ready;
                self.alerts.attach(
                    device.clone(),
                    DeviceAttach {
                        session,
                        capabilities,
                        alerts,
                    },
                )?;
                self.firmware.observe_hello(&DeviceHello {
                    device: device.clone(),
                    platform,
                    firmware_version: firmware_version.as_deref().and_then(FirmwareVersion::new),
                });
                self.queued_update(device);
            }
            DeviceEvent::SessionEnded { device, session } => self.alerts.detach(device, session)?,
            DeviceEvent::Alert {
                device,
                session,
                message_type,
                body,
            } => match AlertInbound::decode(message_type, body) {
                Some(Ok(message)) => self.alerts.device_message(device, session, message)?,
                Some(Err(error)) => {
                    tracing::warn!(%device, %error, "a malformed alert message was ignored");
                }
                None => {}
            },
            DeviceEvent::Timer { device, event } => {
                self.alerts.timer_event(device, event)?;
            }
            DeviceEvent::StopAlert { device } => self.stop_alert(device),
        }
        Ok(())
    }

    /// A queued install runs once the device is back, after its hello was observed.
    fn queued_update(&self, device: DeviceId) {
        let firmware = self.firmware.clone();
        let directory = self.directory.clone();
        let db = self.db.clone();
        let tasks = self.tasks.clone();
        drop(self.tasks.spawn("queued update check", async move {
            let target = match device_target(&directory, &db, &device).await {
                Ok(target) => target,
                Err(error) => {
                    tracing::debug!(%device, ?error, "no queued install check for this session");
                    return;
                }
            };
            match firmware.queued_update_on_connect(target).await {
                Ok(Some(job)) => drop(tasks.spawn("queued update", job.run())),
                Ok(None) => {}
                Err(failure) => {
                    tracing::warn!(%device, %failure, "the queued install did not start")
                }
            }
        }));
    }

    /// Home Assistant's `stop_alert` button: stop whatever rings on this speaker.
    fn stop_alert(&self, device: DeviceId) {
        let ringing = self
            .alerts
            .rings()
            .borrow()
            .devices
            .get(&device.0)
            .and_then(|rings| rings.active.as_ref().map(|ring| ring.id.clone()));
        let Some(ring_id) = ringing else {
            tracing::info!(%device, "stop_alert pressed while nothing rings");
            return;
        };
        let alerts = self.alerts.clone();
        drop(self.tasks.spawn("stop_alert", async move {
            let outcome = alerts
                .act(Act {
                    op_id: OpId::dashboard(),
                    target: RingRef {
                        device: device.clone(),
                        ring_id,
                    },
                    action: ActAction::Dismiss,
                    source: ActSource::Entity,
                })
                .await;
            tracing::info!(%device, ?outcome, "stop_alert from Home Assistant");
        }));
    }
}

/// Logs the alert service's notices and tells the dashboard which speakers' alerts
/// changed (`alerts` with kind `changed`: its alert panel reads them again). A change
/// of what rings on a speaker is one too.
pub async fn publish_alert_notices(alerts: AlertHandle, publisher: EventPublisher) {
    let mut notices = alerts.notices();
    let mut rings = alerts.rings();
    let mut previous = rings.borrow_and_update().clone();
    loop {
        tokio::select! {
            notice = notices.recv() => match notice {
                Ok(notice) => {
                    tracing::info!(?notice, "alert notice");
                    if let Some(device) = notice.device() {
                        publisher.publish(&ApiEvent::Alerts {
                            device_id: device.clone(),
                            notice: DashboardNotice::Changed,
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "alert notices were missed");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            changed = rings.changed() => {
                if changed.is_err() {
                    return;
                }
                let current = rings.borrow_and_update().clone();
                let changed: std::collections::BTreeSet<&String> = previous
                    .devices
                    .keys()
                    .chain(current.devices.keys())
                    .filter(|id| previous.devices.get(*id) != current.devices.get(*id))
                    .collect();
                for id in changed {
                    publisher.publish(&ApiEvent::Alerts {
                        device_id: DeviceId(id.clone()),
                        notice: DashboardNotice::Changed,
                    });
                }
                previous = current;
            }
        }
    }
}
