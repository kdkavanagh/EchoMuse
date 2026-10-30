//! Firmware delivery wired to the devices and the dashboard (spec §3.2, §4.10): the
//! firmware service's access to live sessions ([`FirmwareAccess`]), its events as
//! dashboard state and live events ([`FirmwareDashboard`]), and the routes' view of it
//! ([`ControllerFirmware`]).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

use echomuse_api::{
    ApiEvent, BoxFuture, DeployReport, DeploySkip, DeploySkipReason, DeviceStatePatch,
    EventPublisher, FirmwareControl, FirmwareRefusal, UpdateStatus,
};
use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, devices::GetDevice};
use echomuse_firmware::{
    DeviceAccess, DeviceTarget, FirmwareEvent, FirmwareEvents, FirmwareFailure, FirmwareInfo,
    FirmwareService, FirmwareVersion, LinkShell, Operation, RollbackOutcome, ShellOpenError,
    UpdateOutcome, open_pipe_shell,
    provision::{
        CrownLists, DiagnosticsUpload, FirmwareDownload, ProvisionDiagnostics, ProvisionError,
        StartScriptFile, TlsCredentials, TlsCredentialsRequest,
    },
};
use echomuse_link::{SessionLink, ShellBroker};
use serde_json::{Map, json};
use tokio::sync::watch;

use crate::{app::Directory, supervision::TaskSupervisor};

/// The firmware service as this controller builds it.
pub type Firmware = FirmwareService<FirmwareAccess, DbHandle, FirmwareDashboard>;

/// Every device's latest update or rollback state, as the dashboard shows it. Which
/// devices an operation holds is the firmware service's ([`FirmwareService::is_busy`]).
pub type UpdateStates = Arc<Mutex<HashMap<DeviceId, UpdateStatus>>>;

/// A device's live session, from its runtime's latest snapshot.
fn live_link(directory: &watch::Receiver<Directory>, device: &DeviceId) -> Option<SessionLink> {
    directory.borrow().get(device)?.snapshot().session_link
}

/// [`DeviceAccess`] over the runtimes' published sessions and the link server's shell
/// broker.
pub struct FirmwareAccess {
    directory: watch::Receiver<Directory>,
    shells: ShellBroker,
}

impl FirmwareAccess {
    pub fn new(directory: watch::Receiver<Directory>, shells: ShellBroker) -> Self {
        Self { directory, shells }
    }
}

impl DeviceAccess for FirmwareAccess {
    type Shell = LinkShell;

    async fn open_shell(&self, device: &DeviceId) -> Result<LinkShell, ShellOpenError> {
        let link = live_link(&self.directory, device).ok_or(ShellOpenError::NotConnected)?;
        open_pipe_shell(&self.shells, &link).await
    }

    fn disconnect(&self, device: &DeviceId) -> bool {
        match live_link(&self.directory, device) {
            Some(link) => {
                link.close();
                true
            }
            None => false,
        }
    }
}

/// [`FirmwareEvents`] as dashboard state (`update_in_progress`, `update_error`) and the
/// live events dashboard.jsx reads.
pub struct FirmwareDashboard {
    publisher: Arc<OnceLock<EventPublisher>>,
    states: UpdateStates,
}

impl FirmwareDashboard {
    /// The events, the publisher filled in later through the returned slot (the API is
    /// built after the firmware service), and the states it keeps.
    pub fn new() -> (Self, Arc<OnceLock<EventPublisher>>, UpdateStates) {
        let publisher = Arc::new(OnceLock::new());
        let states = UpdateStates::default();
        (
            Self {
                publisher: Arc::clone(&publisher),
                states: Arc::clone(&states),
            },
            publisher,
            states,
        )
    }

    fn publish_event(&self, event: ApiEvent) {
        if let Some(publisher) = self.publisher.get() {
            publisher.publish(&event);
        }
    }

    fn set(&self, device: &DeviceId, status: UpdateStatus) {
        let previous = self
            .states
            .lock()
            .expect("firmware states poisoned")
            .insert(device.clone(), status.clone());
        if previous.unwrap_or_default() != status {
            self.publish_event(ApiEvent::DeviceUpdate {
                device_id: device.clone(),
                state: DeviceStatePatch(Map::from_iter([
                    ("update_in_progress".to_owned(), json!(status.in_progress)),
                    ("update_error".to_owned(), json!(status.error)),
                ])),
            });
        }
    }
}

impl FirmwareEvents for FirmwareDashboard {
    fn publish(&self, event: FirmwareEvent) {
        match event {
            FirmwareEvent::Progress {
                device,
                operation,
                stage,
            } => {
                tracing::info!(%device, ?operation, ?stage, "firmware operation");
                if matches!(operation, Operation::Update | Operation::Rollback) {
                    self.set(
                        &device,
                        UpdateStatus {
                            in_progress: true,
                            error: None,
                        },
                    );
                }
            }
            FirmwareEvent::UpdateFinished { device, outcome } => {
                let (error, event) = match outcome {
                    UpdateOutcome::Updated { .. } => (
                        None,
                        ApiEvent::DeviceUpdated {
                            device_id: device.clone(),
                        },
                    ),
                    UpdateOutcome::AutoRolledBack {
                        attempted, running, ..
                    } => (
                        Some(format!(
                            "{} failed to start; the device went back to {}",
                            attempted.as_str(),
                            running.as_str()
                        )),
                        ApiEvent::DeviceAutoRolledBack {
                            device_id: device.clone(),
                        },
                    ),
                    UpdateOutcome::Failed(failure) => (
                        Some(failure.to_string()),
                        ApiEvent::DeviceUpdateFailed {
                            device_id: device.clone(),
                        },
                    ),
                };
                self.set(
                    &device,
                    UpdateStatus {
                        in_progress: false,
                        error,
                    },
                );
                self.publish_event(event);
            }
            FirmwareEvent::RollbackFinished { device, outcome } => {
                let (error, event) = match outcome {
                    RollbackOutcome::RolledBack { .. } => (
                        None,
                        ApiEvent::DeviceRolledBack {
                            device_id: device.clone(),
                        },
                    ),
                    RollbackOutcome::Reverted { running, .. } => (
                        Some(format!(
                            "the other slot failed to start; the device stayed on {}",
                            running.as_str()
                        )),
                        ApiEvent::DeviceUpdateFailed {
                            device_id: device.clone(),
                        },
                    ),
                    RollbackOutcome::Failed(failure) => (
                        Some(failure.to_string()),
                        ApiEvent::DeviceUpdateFailed {
                            device_id: device.clone(),
                        },
                    ),
                };
                self.set(
                    &device,
                    UpdateStatus {
                        in_progress: false,
                        error,
                    },
                );
                self.publish_event(event);
            }
            FirmwareEvent::QueueChanged { device, queued_at } => {
                self.publish_event(ApiEvent::DeviceUpdateQueue {
                    device_id: device,
                    queued_at: queued_at.map(|at| at.0),
                });
            }
        }
    }
}

/// The device's target for an operation: its live session's platform and version, and
/// the version its row holds for the rollback slot.
pub async fn device_target(
    directory: &watch::Receiver<Directory>,
    db: &DbHandle,
    device: &DeviceId,
) -> Result<DeviceTarget, FirmwareRefusal> {
    let snapshot = directory
        .borrow()
        .get(device)
        .map(|handle| handle.snapshot());
    let row = db
        .read(GetDevice {
            device: device.clone(),
        })
        .await
        .map_err(|error| FirmwareRefusal::Failed(format!("reading the device: {error}")))?
        .ok_or(FirmwareRefusal::UnknownDevice)?;
    let link = snapshot
        .and_then(|snapshot| snapshot.link)
        .ok_or(FirmwareRefusal::NotConnected)?;
    Ok(DeviceTarget {
        device: device.clone(),
        platform: link.platform,
        running: link
            .firmware_version
            .as_deref()
            .and_then(FirmwareVersion::new),
        previous: row
            .firmware_previous
            .as_deref()
            .and_then(FirmwareVersion::new),
    })
}

fn refusal(failure: FirmwareFailure) -> FirmwareRefusal {
    match failure {
        FirmwareFailure::Busy => FirmwareRefusal::Busy,
        FirmwareFailure::AlreadyCurrent => FirmwareRefusal::AlreadyCurrent,
        FirmwareFailure::NothingToDebloat => FirmwareRefusal::NotApplicable(
            "a LineageOS Echo Show runs nothing of Amazon's to debloat".to_owned(),
        ),
        FirmwareFailure::TlsUnavailable => FirmwareRefusal::TlsUnavailable,
        FirmwareFailure::ShellOpen(ShellOpenError::NotConnected) => FirmwareRefusal::NotConnected,
        other => FirmwareRefusal::Failed(other.to_string()),
    }
}

/// [`FirmwareControl`] over the firmware service and the runtimes.
pub struct ControllerFirmware {
    service: Firmware,
    directory: watch::Receiver<Directory>,
    db: DbHandle,
    states: UpdateStates,
    tasks: TaskSupervisor,
}

impl ControllerFirmware {
    pub fn new(
        service: Firmware,
        directory: watch::Receiver<Directory>,
        db: DbHandle,
        states: UpdateStates,
        tasks: TaskSupervisor,
    ) -> Self {
        Self {
            service,
            directory,
            db,
            states,
            tasks,
        }
    }

    async fn start_update(&self, target: DeviceTarget) -> Result<String, FirmwareRefusal> {
        let job = self.service.prepare_update(target).map_err(refusal)?;
        let version = job.version().as_str().to_owned();
        // The supervisor observes the job (a panic stops the process); nothing waits on it.
        drop(self.tasks.spawn("update", job.run()));
        Ok(version)
    }
}

impl FirmwareControl for ControllerFirmware {
    fn bundled(&self) -> FirmwareInfo {
        self.service.bundled().info().clone()
    }

    fn status(&self, device: &DeviceId) -> UpdateStatus {
        self.states
            .lock()
            .expect("firmware states poisoned")
            .get(device)
            .cloned()
            .unwrap_or_default()
    }

    fn update(&self, device: DeviceId) -> BoxFuture<'_, Result<String, FirmwareRefusal>> {
        Box::pin(async move {
            let target = device_target(&self.directory, &self.db, &device).await?;
            self.start_update(target).await
        })
    }

    fn rollback(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move {
            let target = device_target(&self.directory, &self.db, &device).await?;
            let job = self.service.prepare_rollback(target).map_err(refusal)?;
            drop(self.tasks.spawn("rollback", job.run()));
            Ok(())
        })
    }

    fn queue_update(&self, device: DeviceId) -> BoxFuture<'_, Result<i64, FirmwareRefusal>> {
        Box::pin(async move {
            self.service
                .queue_update(&device)
                .await
                .map(|at| at.0)
                .map_err(refusal)
        })
    }

    fn cancel_queued_update(
        &self,
        device: DeviceId,
    ) -> BoxFuture<'_, Result<bool, FirmwareRefusal>> {
        Box::pin(async move {
            self.service
                .cancel_queued_update(&device)
                .await
                .map_err(refusal)
        })
    }

    fn debloat(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move {
            let target = device_target(&self.directory, &self.db, &device).await?;
            let job = self.service.prepare_debloat(target).map_err(refusal)?;
            drop(self.tasks.spawn("debloat", job.run()));
            Ok(())
        })
    }

    fn secure_link(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(async move {
            if live_link(&self.directory, &device).is_none() {
                return Err(FirmwareRefusal::NotConnected);
            }
            self.service.secure_link(&device).await.map_err(refusal)
        })
    }

    fn deploy(&self) -> BoxFuture<'_, DeployReport> {
        Box::pin(async move {
            let devices: Vec<DeviceId> = {
                let directory = self.directory.borrow();
                let mut devices: Vec<DeviceId> = directory.keys().cloned().collect();
                devices.sort_by(|a, b| a.0.cmp(&b.0));
                devices
            };
            let mut report = DeployReport {
                version: self.service.bundled().version().as_str().to_owned(),
                started: Vec::new(),
                skipped: Vec::new(),
            };
            for device in devices {
                let skip = |reason| DeploySkip {
                    device_id: device.clone(),
                    reason,
                };
                let target = match device_target(&self.directory, &self.db, &device).await {
                    Ok(target) => target,
                    Err(FirmwareRefusal::NotConnected) => {
                        report.skipped.push(skip(DeploySkipReason::Offline));
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%device, ?error, "deploy could not read the device");
                        continue;
                    }
                };
                match self.start_update(target).await {
                    Ok(_) => report.started.push(device),
                    Err(FirmwareRefusal::Busy) => {
                        report
                            .skipped
                            .push(skip(DeploySkipReason::UpdateInProgress));
                    }
                    Err(FirmwareRefusal::AlreadyCurrent) => {
                        report.skipped.push(skip(DeploySkipReason::AlreadyCurrent));
                    }
                    Err(error) => {
                        tracing::warn!(%device, ?error, "deploy could not start the update");
                    }
                }
            }
            report
        })
    }

    fn provision_start_script(&self) -> BoxFuture<'_, Result<StartScriptFile, ProvisionError>> {
        Box::pin(self.service.provision_start_script())
    }

    fn provision_crown_lists(&self) -> BoxFuture<'_, Result<CrownLists, ProvisionError>> {
        Box::pin(self.service.provision_crown_lists())
    }

    fn provision_firmware(&self) -> BoxFuture<'_, Result<FirmwareDownload, ProvisionError>> {
        Box::pin(self.service.provision_firmware())
    }

    fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> BoxFuture<'_, Result<TlsCredentials, ProvisionError>> {
        Box::pin(self.service.provision_tls_credentials(request))
    }

    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError> {
        self.service.provision_diagnostics(upload)
    }
}
