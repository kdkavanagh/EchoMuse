//! The device-runtime registry: which approved devices have a runtime, and the immutable
//! directory of their handles that admission and the dashboard read.
//!
//! Every runtime is watched by a supervisor task (§5.5, §5.7 step 5). When a runtime
//! fails without being asked to stop (a speech worker or runtime task panicked, or its
//! queues broke), the supervisor makes sure it is fully stopped (its identity revoked,
//! its session closed, its workers joined), shows the device as restarting with the
//! reason, waits the restart backoff, and starts a fresh runtime: new runtime id, new
//! speech workers and model sessions, settings read again, no unfinished conversation.
//! Admission refuses the device meanwhile. A worker that cannot be stopped fires the
//! fatal hook instead of starting a replacement beside it.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use echomuse_api::{ApiEvent, EventPublisher};
use echomuse_db::{
    DbHandle, ReadQuery, ServiceError,
    devices::{ApproveDevice, DeviceError},
};
use rusqlite::Connection;
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use echomuse_core::types::{DeviceId, RuntimeError};

use echomuse_voice::arbiter::WakeArbiter;

use crate::{
    dashboard,
    device::{DeviceHandle, DeviceRuntime, DeviceRuntimeConfig, DeviceSnapshot, RuntimeServices},
    device_events::{DeviceEvent, DeviceEvents},
    supervision::{Availability, RestartBackoff, Supervision, SupervisionOptions, TaskSupervisor},
};

/// Every runtime's handle, published whole whenever a runtime is added, removed or
/// rebuilt.
pub type Directory = Arc<HashMap<DeviceId, DeviceHandle>>;

/// Owned by the shared Tokio runtime. Device state stays on dedicated threads.
/// Adding a device here creates runtime resources; it is not protocol admission or approval.
pub struct Controller {
    devices: HashMap<DeviceId, Supervised>,
    config: DeviceRuntimeConfig,
    arbiter: Arc<WakeArbiter>,
    services: RuntimeServices,
    supervision: Supervision,
    /// Shared with the supervisors, which put a rebuilt runtime's handle in place.
    directory: Arc<watch::Sender<Directory>>,
}

/// One device's supervisor task.
struct Supervised {
    /// Asks the supervisor to stop the runtime for good.
    stop: CancellationToken,
    /// The supervisor's result: the last runtime's clean stop, or why it was not.
    task: JoinHandle<Result<(), RuntimeError>>,
}

impl Controller {
    /// A registry whose runtimes run no ESPHome nodes and report no events, with the
    /// production failure handling.
    pub fn new(config: DeviceRuntimeConfig) -> Result<Self, RuntimeError> {
        Self::with_services(config, RuntimeServices::default())
    }

    /// A registry whose runtimes serve their devices with `services`. Without
    /// `services.supervision` the production failure handling is started.
    pub fn with_services(
        config: DeviceRuntimeConfig,
        mut services: RuntimeServices,
    ) -> Result<Self, RuntimeError> {
        config.ingress.validate()?;
        config.workers.validate()?;
        if config.shutdown_timeout.is_zero() {
            return Err(RuntimeError::InvalidLimits);
        }
        let supervision = match &services.supervision {
            Some(supervision) => supervision.clone(),
            None => Supervision::start(SupervisionOptions::default())?,
        };
        supervision.restart.validate()?;
        services.supervision = Some(supervision.clone());
        Ok(Self {
            devices: HashMap::new(),
            config,
            arbiter: Arc::new(WakeArbiter::default()),
            services,
            supervision,
            directory: Arc::new(watch::Sender::new(Arc::new(HashMap::new()))),
        })
    }

    /// Where the runtimes report device events.
    pub fn events(&self) -> Option<DeviceEvents> {
        self.services.events.clone()
    }

    pub fn arbiter(&self) -> Arc<WakeArbiter> {
        Arc::clone(&self.arbiter)
    }

    /// Spawns the shared services' tasks; a panic in one stops the process.
    pub fn tasks(&self) -> TaskSupervisor {
        self.supervision.tasks.clone()
    }

    /// Gives every runtime started from now on the voice path's services. Its arbiter
    /// becomes the registry's.
    pub fn set_voice_services(&mut self, voice: Arc<crate::voice_session::VoiceServices>) {
        self.arbiter = Arc::clone(&voice.arbiter);
        self.services.voice = Some(voice);
    }

    /// The directory of runtime handles, updated as runtimes come, go and are rebuilt.
    pub fn directory(&self) -> watch::Receiver<Directory> {
        self.directory.subscribe()
    }

    /// Starts the device's runtime and its supervisor. The handle's snapshot keeps
    /// following the device across rebuilds; its commands reach only this runtime.
    pub async fn add_device(&mut self, id: DeviceId) -> Result<DeviceHandle, RuntimeError> {
        if self.devices.contains_key(&id) {
            return Err(RuntimeError::DuplicateDevice);
        }
        let runtime =
            DeviceRuntime::start_with(id.clone(), self.config, self.services.clone()).await?;
        let handle = runtime.handle();
        self.directory.send_modify(|directory| {
            Arc::make_mut(directory).insert(id.clone(), handle.clone());
        });
        let stop = CancellationToken::new();
        let supervisor = Supervisor {
            device: id.clone(),
            config: self.config,
            services: self.services.clone(),
            supervision: self.supervision.clone(),
            directory: Arc::clone(&self.directory),
            snapshots: runtime.snapshots(),
            stop: stop.clone(),
        };
        let task = self
            .supervision
            .tasks
            .spawn("device runtime supervisor", supervisor.run(runtime));
        self.devices.insert(id, Supervised { stop, task });
        Ok(handle)
    }

    /// The device's current runtime.
    pub fn device(&self, id: &DeviceId) -> Option<DeviceHandle> {
        self.directory.borrow().get(id).cloned()
    }

    pub fn snapshots(&self) -> Vec<DeviceSnapshot> {
        let mut snapshots: Vec<_> = self
            .directory
            .borrow()
            .values()
            .map(DeviceHandle::snapshot)
            .collect();
        snapshots.sort_by(|left, right| left.device.0.cmp(&right.device.0));
        snapshots
    }

    /// Removes the device's runtime from the directory, then stops it and its
    /// supervisor. An error is the last runtime's failure to stop cleanly.
    pub async fn remove_device(&mut self, id: &DeviceId) -> Result<(), RuntimeError> {
        let supervised = self.devices.remove(id).ok_or(RuntimeError::UnknownDevice)?;
        // The supervisor first, so it does not take the revoked runtime's exit for a
        // failure and rebuild it.
        supervised.stop.cancel();
        let mut removed = None;
        self.directory.send_modify(|directory| {
            removed = Arc::make_mut(directory).remove(id);
        });
        if let Some(handle) = removed {
            handle.revoke();
        }
        supervised
            .task
            .await
            .unwrap_or(Err(RuntimeError::WorkerPanicked))
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        // Revoke every device first; no device remains live while another drains. The
        // supervisors are told first, so none rebuilds a runtime revoked here.
        for supervised in self.devices.values() {
            supervised.stop.cancel();
        }
        for handle in self.directory.borrow().values() {
            handle.revoke();
        }
        self.directory.send_replace(Arc::new(HashMap::new()));
        let mut first_error = None;
        for (_, supervised) in self.devices.drain() {
            let result = supervised
                .task
                .await
                .unwrap_or(Err(RuntimeError::WorkerPanicked));
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// Watches one device's runtime and rebuilds it when it fails.
struct Supervisor {
    device: DeviceId,
    config: DeviceRuntimeConfig,
    services: RuntimeServices,
    supervision: Supervision,
    directory: Arc<watch::Sender<Directory>>,
    /// The device's snapshot channel, which every runtime of the device publishes on.
    snapshots: Arc<watch::Sender<DeviceSnapshot>>,
    stop: CancellationToken,
}

impl Supervisor {
    async fn run(self, mut runtime: DeviceRuntime) -> Result<(), RuntimeError> {
        let policy = self.supervision.restart;
        let mut backoff = RestartBackoff::new(policy);
        loop {
            let started = tokio::time::Instant::now();
            let stable = tokio::time::sleep(policy.stable);
            tokio::pin!(stable);
            let mut settled = false;
            let exit = loop {
                tokio::select! {
                    biased;
                    () = self.stop.cancelled() => return runtime.shutdown().await,
                    exit = runtime.exited() => break exit,
                    () = &mut stable, if !settled => {
                        // A stable run: the delays and the error window start over.
                        settled = true;
                        backoff.reset();
                        self.snapshots.send_modify(|state| state.health.errors.close());
                    }
                }
            };
            if self.stop.is_cancelled() {
                // The runtime stopped because it was asked to.
                return runtime.shutdown().await.and(exit);
            }
            let mut reason = match &exit {
                Err(error) => format!("the device runtime failed: {error}"),
                Ok(()) => "the device runtime stopped without being asked to".to_owned(),
            };
            // Its identity is revoked and its session closed by now; this waits for
            // its thread and its speech workers to be gone.
            let stopped = runtime.shutdown().await;
            if exit == Err(RuntimeError::ShutdownTimeout)
                || stopped == Err(RuntimeError::ShutdownTimeout)
            {
                // A worker is stuck in a call: a replacement must not start beside it.
                self.supervision.fatal().fire(&format!(
                    "device {}: the failed runtime's speech workers did not stop within {} s",
                    self.device,
                    self.config.shutdown_timeout.as_secs_f64()
                ));
                return Err(RuntimeError::ShutdownTimeout);
            }
            let mut ran_for = started.elapsed();
            runtime = loop {
                let delay = backoff.failed(ran_for);
                tracing::error!(
                    device = %self.device,
                    %reason,
                    retry_in_ms = delay.as_millis(),
                    "the device runtime failed; it is rebuilt after the backoff"
                );
                self.snapshots.send_modify(|state| {
                    state.health.errors.record(&reason);
                    state.health.availability = Availability::Restarting {
                        reason: reason.clone(),
                        retry_in: delay,
                    };
                });
                tokio::select! {
                    biased;
                    () = self.stop.cancelled() => return Ok(()),
                    () = tokio::time::sleep(delay) => {}
                }
                let started = DeviceRuntime::start_in(
                    self.device.clone(),
                    self.config,
                    self.services.clone(),
                    Arc::clone(&self.snapshots),
                )
                .await;
                match started {
                    Ok(rebuilt) => break rebuilt,
                    Err(error) => {
                        reason = format!("the device runtime did not restart: {error}");
                        ran_for = std::time::Duration::ZERO;
                    }
                }
            };
            if self.stop.is_cancelled() {
                return runtime.shutdown().await;
            }
            let handle = runtime.handle();
            let mut placed = false;
            self.directory.send_modify(|directory| {
                if let Some(entry) = Arc::make_mut(directory).get_mut(&self.device) {
                    *entry = handle.clone();
                    placed = true;
                }
            });
            if !placed {
                // The device was removed while its runtime was rebuilt.
                return runtime.shutdown().await;
            }
            // Admission reads this, so it changes only once the new handle is in place.
            self.snapshots
                .send_modify(|state| state.health.availability = Availability::Available);
            tracing::info!(
                device = %self.device,
                runtime = handle.snapshot().runtime.0,
                "the device runtime was rebuilt"
            );
        }
    }
}

enum RegistryCommand {
    Ensure {
        device: DeviceId,
        /// Publish `device_approved` when this creates the runtime.
        announce: bool,
        reply: oneshot::Sender<Result<DeviceHandle, RuntimeError>>,
    },
    Retire {
        device: DeviceId,
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
    Release {
        device: DeviceId,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), RuntimeError>>,
    },
}

/// Devices being deleted: admission refuses them and no runtime is created for them.
pub type Retiring = Arc<HashSet<DeviceId>>;

/// The registry as a service on the shared runtime: one task owns the [`Controller`],
/// so runtimes are created and removed one at a time and the directory is published
/// after each change. Cheap to clone.
#[derive(Clone)]
pub struct Registry {
    commands: mpsc::Sender<RegistryCommand>,
    directory: watch::Receiver<Directory>,
    retiring: watch::Receiver<Retiring>,
    db: DbHandle,
}

impl Registry {
    /// Starts the service. It serves commands once it is given the dashboard's event
    /// publisher through the returned sender (the API is built with this registry, so
    /// the publisher exists only afterwards); from then on every runtime, already held
    /// or added later, has its state changes published.
    pub fn start(
        controller: Controller,
        db: DbHandle,
    ) -> (Self, JoinHandle<()>, oneshot::Sender<EventPublisher>) {
        let directory = controller.directory();
        let (commands, receiver) = mpsc::channel(64);
        let (retiring_tx, retiring) = watch::channel(Arc::new(HashSet::new()));
        let (publisher_tx, publisher) = oneshot::channel();
        let tasks = controller.tasks();
        let task = tasks.spawn(
            "device registry",
            serve(controller, receiver, retiring_tx, publisher),
        );
        (
            Self {
                commands,
                directory,
                retiring,
                db,
            },
            task,
            publisher_tx,
        )
    }

    /// The current directory; reading it takes no part in the registry's work.
    pub fn directory(&self) -> Directory {
        Arc::clone(&self.directory.borrow())
    }

    /// Whether the device is being deleted.
    pub fn is_retiring(&self, device: &DeviceId) -> bool {
        self.retiring.borrow().contains(device)
    }

    /// The device's runtime, created if it has none. A runtime created here belongs to a
    /// device approved since startup (automatic approval on its first hello), so the
    /// dashboard is told with `device_approved`. The caller is responsible for the
    /// device being approved.
    pub async fn ensure(&self, device: DeviceId) -> Result<DeviceHandle, RuntimeError> {
        if let Some(handle) = self.directory().get(&device) {
            return Ok(handle.clone());
        }
        self.request_runtime(device, true).await
    }

    async fn request_runtime(
        &self,
        device: DeviceId,
        announce: bool,
    ) -> Result<DeviceHandle, RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(RegistryCommand::Ensure {
                device,
                announce,
                reply,
            })
            .await
            .map_err(|_| RuntimeError::Closed)?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    /// Approves a registered device under `label` (or relabels an approved one) and
    /// starts its runtime. The dashboard route that asked publishes `device_approved`.
    pub async fn approve(
        &self,
        device: DeviceId,
        label: String,
    ) -> Result<DeviceHandle, ApproveError> {
        self.db
            .write(ApproveDevice {
                device: device.clone(),
                label,
            })
            .await
            .map_err(|error| match error {
                ServiceError::Failed(DeviceError::UnknownDevice(_)) => ApproveError::UnknownDevice,
                other => ApproveError::Database(other.to_string()),
            })?;
        if let Some(handle) = self.directory().get(&device) {
            return Ok(handle.clone());
        }
        self.request_runtime(device, false)
            .await
            .map_err(ApproveError::Runtime)
    }

    /// Starts deleting a device: from now on admission refuses it and no runtime is
    /// created for it, and its runtime (if any) is removed from the directory and
    /// stopped, which closes its session and its ESPHome listeners. An error is the
    /// runtime's failure to stop cleanly; it is gone from the directory regardless.
    pub async fn retire(&self, device: DeviceId) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(RegistryCommand::Retire { device, reply })
            .await
            .map_err(|_| RuntimeError::Closed)?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }

    /// Ends a deletion: the device may register again, as a new pending device.
    pub async fn release(&self, device: DeviceId) {
        // A closed registry admits nothing anyway.
        let _ = self
            .commands
            .send(RegistryCommand::Release { device })
            .await;
    }

    /// Revokes every runtime at once, then waits for each to stop.
    pub async fn shutdown(&self) -> Result<(), RuntimeError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(RegistryCommand::Shutdown { reply })
            .await
            .map_err(|_| RuntimeError::Closed)?;
        response.await.map_err(|_| RuntimeError::Closed)?
    }
}

/// Why an approval did not produce a runtime.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApproveError {
    /// No such device is registered.
    UnknownDevice,
    /// The approval was not stored.
    Database(String),
    /// The approval was stored but the runtime did not start.
    Runtime(RuntimeError),
}

impl std::fmt::Display for ApproveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("the device is not registered"),
            Self::Database(error) => write!(f, "the approval was not stored: {error}"),
            Self::Runtime(error) => write!(f, "the device runtime did not start: {error}"),
        }
    }
}

impl std::error::Error for ApproveError {}

async fn serve(
    mut controller: Controller,
    mut commands: mpsc::Receiver<RegistryCommand>,
    retiring: watch::Sender<Retiring>,
    publisher: oneshot::Receiver<EventPublisher>,
) {
    let Ok(publisher) = publisher.await else {
        // The API was never built: nothing to serve, so stop the runtimes.
        if let Err(error) = controller.shutdown().await {
            tracing::error!(%error, "device runtimes did not stop cleanly");
        }
        return;
    };
    let tasks = controller.tasks();
    // Each publisher ends when its device's runtime is removed; a panic in one stops
    // the process, so none can fail unseen.
    for handle in controller.directory().borrow().values() {
        tasks.spawn(
            "dashboard device events",
            dashboard::publish_device_changes(handle.subscribe(), publisher.clone()),
        );
    }
    while let Some(command) = commands.recv().await {
        match command {
            RegistryCommand::Ensure {
                device,
                announce,
                reply,
            } => {
                let result = if retiring.borrow().contains(&device) {
                    Err(RuntimeError::Closed)
                } else if let Some(handle) = controller.device(&device) {
                    Ok(handle)
                } else {
                    let added = controller.add_device(device.clone()).await;
                    if let Ok(handle) = &added {
                        tasks.spawn(
                            "dashboard device events",
                            dashboard::publish_device_changes(
                                handle.subscribe(),
                                publisher.clone(),
                            ),
                        );
                        if let Some(events) = controller.events() {
                            events.send(DeviceEvent::Registered(device.clone())).await;
                        }
                        if announce {
                            publisher.publish(&ApiEvent::DeviceApproved { device_id: device });
                        }
                    }
                    added
                };
                let _ = reply.send(result);
            }
            RegistryCommand::Retire { device, reply } => {
                retiring.send_modify(|set| {
                    Arc::make_mut(set).insert(device.clone());
                });
                let result = match controller.remove_device(&device).await {
                    Err(RuntimeError::UnknownDevice) => Ok(()),
                    other => other,
                };
                if let Some(events) = controller.events() {
                    events.send(DeviceEvent::Deregistered(device)).await;
                }
                let _ = reply.send(result);
            }
            RegistryCommand::Release { device } => {
                retiring.send_modify(|set| {
                    Arc::make_mut(set).remove(&device);
                });
            }
            RegistryCommand::Shutdown { reply } => {
                commands.close();
                let _ = reply.send(controller.shutdown().await);
                return;
            }
        }
    }
    // Every handle was dropped without a shutdown: stop the runtimes anyway.
    if let Err(error) = controller.shutdown().await {
        tracing::error!(%error, "device runtimes did not stop cleanly");
    }
}

/// The ids of every approved device, ordered.
#[derive(Clone, Copy, Debug)]
pub struct ApprovedDevices;

impl ReadQuery for ApprovedDevices {
    type Output = Vec<DeviceId>;
    type Error = DeviceError;

    fn read(self, conn: &Connection) -> Result<Self::Output, Self::Error> {
        let mut statement = conn.prepare_cached(
            "SELECT device_id FROM devices WHERE approved != 0 ORDER BY device_id",
        )?;
        let rows = statement.query_map([], |row| Ok(DeviceId(row.get(0)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
