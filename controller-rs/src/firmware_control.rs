//! Firmware delivery wired to the devices and the dashboard (spec §3.2, §4.10): the
//! firmware service's access to live sessions ([`FirmwareAccess`]), its events as
//! dashboard state and live events ([`FirmwareDashboard`]), and the routes' view of it
//! ([`ControllerFirmware`]).

use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use echomuse_api::{
    ApiEvent, DeployReport, DeploySkip, DeploySkipReason, DeviceStatePatch, EventPublisher,
    FirmwareControl, FirmwareRefusal, UpdateStatus,
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
use echomuse_protocol::Platform;
use serde_json::{Map, json};
use tokio::sync::watch;

use crate::{app::Directory, supervision::TaskSupervisor};

/// The firmware service as this controller builds it.
pub type Firmware = FirmwareService<FirmwareAccess, DbHandle, FirmwareDashboard>;

/// Every device's firmware operation state, published whole on each change.
pub type UpdateStates = Arc<HashMap<DeviceId, UpdateStatus>>;

/// A device's live session, from its runtime's latest snapshot.
fn live_link(directory: &watch::Receiver<Directory>, device: &DeviceId) -> Option<SessionLink> {
    directory.borrow().get(device)?.snapshot().session_link
}

/// [`DeviceAccess`] over the runtimes' published sessions and the link server's shell
/// broker. The broker exists only once the device listeners run; until then no device
/// is connected anyway.
pub struct FirmwareAccess {
    directory: watch::Receiver<Directory>,
    shells: Arc<OnceLock<ShellBroker>>,
}

impl FirmwareAccess {
    /// Access with the broker filled in later through the returned slot.
    pub fn new(directory: watch::Receiver<Directory>) -> (Self, Arc<OnceLock<ShellBroker>>) {
        let shells = Arc::new(OnceLock::new());
        (
            Self {
                directory,
                shells: Arc::clone(&shells),
            },
            shells,
        )
    }
}

impl DeviceAccess for FirmwareAccess {
    type Shell = LinkShell;

    async fn open_shell(&self, device: &DeviceId) -> Result<LinkShell, ShellOpenError> {
        let link = live_link(&self.directory, device).ok_or(ShellOpenError::NotConnected)?;
        let broker = self.shells.get().ok_or(ShellOpenError::NotConnected)?;
        open_pipe_shell(broker, &link).await
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
    states: watch::Sender<UpdateStates>,
}

impl FirmwareDashboard {
    /// The events, the publisher filled in later through the returned slot (the API is
    /// built after the firmware service), and the state receiver.
    pub fn new() -> (
        Self,
        Arc<OnceLock<EventPublisher>>,
        watch::Receiver<UpdateStates>,
    ) {
        let publisher = Arc::new(OnceLock::new());
        let (states, receiver) = watch::channel(Arc::new(HashMap::new()));
        (
            Self {
                publisher: Arc::clone(&publisher),
                states,
            },
            publisher,
            receiver,
        )
    }

    fn publish_event(&self, event: ApiEvent) {
        if let Some(publisher) = self.publisher.get() {
            publisher.publish(&event);
        }
    }

    fn set(&self, device: &DeviceId, status: UpdateStatus) {
        let mut changed = false;
        self.states.send_modify(|states| {
            let entry = Arc::make_mut(states).entry(device.clone()).or_default();
            changed = *entry != status;
            *entry = status.clone();
        });
        if changed {
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
                    UpdateOutcome::Updated { from, to } => {
                        tracing::info!(%device, from = ?from.as_ref().map(FirmwareVersion::as_str), to = to.as_str(), "firmware updated");
                        (
                            None,
                            ApiEvent::DeviceUpdated {
                                device_id: device.clone(),
                            },
                        )
                    }
                    UpdateOutcome::AutoRolledBack {
                        attempted,
                        running,
                        supervisor_log,
                    } => {
                        tracing::warn!(%device, attempted = attempted.as_str(), running = running.as_str(), ?supervisor_log, "the new firmware failed to start; the device rolled back");
                        (
                            Some(format!(
                                "{} failed to start; the device went back to {}",
                                attempted.as_str(),
                                running.as_str()
                            )),
                            ApiEvent::DeviceAutoRolledBack {
                                device_id: device.clone(),
                            },
                        )
                    }
                    UpdateOutcome::Failed(failure) => {
                        tracing::warn!(%device, %failure, "firmware update failed");
                        (
                            Some(failure.to_string()),
                            ApiEvent::DeviceUpdateFailed {
                                device_id: device.clone(),
                            },
                        )
                    }
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

/// Runs a firmware job on the shared runtime under the task supervisor, so a panic in
/// it never disappears with an unobserved join handle: it stops the process (§5.5).
pub fn supervise<F>(tasks: &TaskSupervisor, what: &'static str, device: DeviceId, job: F)
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tracing::debug!(%device, what, "firmware job started");
    // The supervisor observes the task; nothing waits on its result.
    drop(tasks.spawn(what, job));
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
    states: watch::Receiver<UpdateStates>,
    tasks: TaskSupervisor,
}

impl ControllerFirmware {
    pub fn new(
        service: Firmware,
        directory: watch::Receiver<Directory>,
        db: DbHandle,
        states: watch::Receiver<UpdateStates>,
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
        let device = job.device().clone();
        supervise(&self.tasks, "update", device, job.run());
        Ok(version)
    }
}

impl FirmwareControl for ControllerFirmware {
    fn bundled(&self) -> FirmwareInfo {
        self.service.bundled().info().clone()
    }

    fn status(&self, device: &DeviceId) -> UpdateStatus {
        self.states
            .borrow()
            .get(device)
            .cloned()
            .unwrap_or_default()
    }

    async fn update(&self, device: DeviceId) -> Result<String, FirmwareRefusal> {
        let target = device_target(&self.directory, &self.db, &device).await?;
        self.start_update(target).await
    }

    async fn rollback(&self, device: DeviceId) -> Result<(), FirmwareRefusal> {
        let target = device_target(&self.directory, &self.db, &device).await?;
        let job = self.service.prepare_rollback(target).map_err(refusal)?;
        supervise(&self.tasks, "rollback", device, job.run());
        Ok(())
    }

    async fn queue_update(&self, device: DeviceId) -> Result<i64, FirmwareRefusal> {
        self.service
            .queue_update(&device)
            .await
            .map(|at| at.0)
            .map_err(refusal)
    }

    async fn cancel_queued_update(&self, device: DeviceId) -> Result<bool, FirmwareRefusal> {
        self.service
            .cancel_queued_update(&device)
            .await
            .map_err(refusal)
    }

    async fn debloat(&self, device: DeviceId) -> Result<(), FirmwareRefusal> {
        let target = device_target(&self.directory, &self.db, &device).await?;
        if target.platform == Platform::Lineage18Crown {
            return Err(refusal(FirmwareFailure::NothingToDebloat));
        }
        if self.service.is_busy(&device) {
            return Err(FirmwareRefusal::Busy);
        }
        let service = self.service.clone();
        supervise(&self.tasks, "debloat", device.clone(), async move {
            match service.debloat(&target).await {
                Ok(report) => tracing::info!(%device, "debloat finished: {report}"),
                Err(failure) => tracing::warn!(%device, %failure, "debloat failed"),
            }
        });
        Ok(())
    }

    async fn secure_link(&self, device: DeviceId) -> Result<(), FirmwareRefusal> {
        if live_link(&self.directory, &device).is_none() {
            return Err(FirmwareRefusal::NotConnected);
        }
        self.service.secure_link(&device).await.map_err(refusal)
    }

    async fn deploy(&self) -> DeployReport {
        let bundled = self.service.bundled().version().clone();
        let devices: Vec<DeviceId> = {
            let directory = self.directory.borrow();
            let mut devices: Vec<DeviceId> = directory.keys().cloned().collect();
            devices.sort_by(|a, b| a.0.cmp(&b.0));
            devices
        };
        let mut report = DeployReport {
            version: bundled.as_str().to_owned(),
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
            if target.running.as_ref() == Some(&bundled) {
                report.skipped.push(skip(DeploySkipReason::AlreadyCurrent));
                continue;
            }
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
                Err(error) => tracing::warn!(%device, ?error, "deploy could not start the update"),
            }
        }
        report
    }

    async fn provision_start_script(&self) -> Result<StartScriptFile, ProvisionError> {
        self.service.provision_start_script().await
    }

    async fn provision_crown_lists(&self) -> Result<CrownLists, ProvisionError> {
        self.service.provision_crown_lists().await
    }

    async fn provision_firmware(&self) -> Result<FirmwareDownload, ProvisionError> {
        self.service.provision_firmware().await
    }

    async fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> Result<TlsCredentials, ProvisionError> {
        self.service.provision_tls_credentials(request).await
    }

    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError> {
        self.service.provision_diagnostics(upload)
    }
}
