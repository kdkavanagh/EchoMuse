//! The firmware delivery service: update, rollback, the install queue, debloat and the
//! secure link (spec §3.2, §4.10).

use std::collections::HashSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use echomuse_core::types::DeviceId;
use echomuse_db::devices::UnixSeconds;
use echomuse_link::WallClock;
use echomuse_protocol::Platform;

use crate::bundled::{BundledFirmware, BundledFirmwareError};
use crate::debloat::{DebloatParseError, DebloatReport, parse_debloat_output};
use crate::hub::{DeviceHello, HelloHub};
use crate::layout::{
    CA_PEM, CrownList, DevicePath, ETC_DIR, FileMode, LINK_TOKEN, SERVER_LINK, START_SCRIPT,
    SUPERVISOR_LOG, Slot,
};
use crate::payloads::{DevicePayloads, Payload, PayloadError};
use crate::shell::{CommandLimits, DeviceShell, Ending, ShellFailure, ShellIo, ShellOpenError};
use crate::store::{FirmwareStore, StoreError};
use crate::transfer::{
    Synced, ToolsReady, TransferError, TransferLimits, check_tools, sync_file, transfer_file,
};
use crate::types::FirmwareVersion;

/// Bytes of the supervisor log reported with a rollback (the log itself is capped at
/// 64 KiB; its newest lines are the ones that explain the rollback).
pub const SUPERVISOR_LOG_TAIL_BYTES: u32 = 8192;

/// Access to devices that only the integrator has: their live sessions.
pub trait DeviceAccess: Send + Sync + 'static {
    /// The device's shell.
    type Shell: ShellIo + 'static;

    /// Opens a pipe shell on the device: `shell_open {pty: false}` on its live session and
    /// the dial-back ([`open_pipe_shell`](crate::open_pipe_shell) does both). The service
    /// bounds the wait.
    fn open_shell(
        &self,
        device: &DeviceId,
    ) -> impl Future<Output = Result<Self::Shell, ShellOpenError>> + Send;

    /// Ends the device's live control session, so it redials (and, holding a CA, does so
    /// over TLS). Returns whether a session was live.
    fn disconnect(&self, device: &DeviceId) -> bool;
}

/// Where the service reports what it is doing.
pub trait FirmwareEvents: Send + Sync + 'static {
    /// One event. Called from the operation's task; must not block.
    fn publish(&self, event: FirmwareEvent);
}

/// Something firmware delivery did.
#[derive(Clone, Debug)]
pub enum FirmwareEvent {
    /// An operation moved on.
    Progress {
        /// The device.
        device: DeviceId,
        /// The operation.
        operation: Operation,
        /// Where it is.
        stage: Stage,
    },
    /// An update finished.
    UpdateFinished {
        /// The device.
        device: DeviceId,
        /// How it ended.
        outcome: UpdateOutcome,
    },
    /// A rollback finished.
    RollbackFinished {
        /// The device.
        device: DeviceId,
        /// How it ended.
        outcome: RollbackOutcome,
    },
    /// A queued install was set or cleared (`update_queued_at`).
    QueueChanged {
        /// The device.
        device: DeviceId,
        /// When it was queued; None when cleared.
        queued_at: Option<UnixSeconds>,
    },
}

/// An operation on a device. One runs per device at a time: they share its one shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Installing the bundled firmware.
    Update,
    /// Switching back to the other slot.
    Rollback,
    /// Re-applying the debloat.
    Debloat,
    /// Installing link credentials.
    SecureLink,
}

/// Where an operation is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stage {
    /// Waiting for the device's shell.
    OpeningShell,
    /// Probing `base64 -d` and `md5sum`.
    CheckingTools,
    /// Reading which slot is active.
    ReadingSlot,
    /// Sending the firmware.
    Uploading {
        /// Bytes the device has acknowledged.
        sent: u64,
        /// Bytes in all.
        total: u64,
    },
    /// Syncing `start_server.sh`.
    SyncingStartScript,
    /// Syncing the Echo Show package lists.
    SyncingCrownLists,
    /// Pointing `server` at a slot and restarting the firmware.
    Switching {
        /// The slot switched to.
        slot: Slot,
    },
    /// Waiting for the device to come back.
    WaitingForReconnect,
    /// Reading the supervisor log after a rollback by the device.
    ReadingSupervisorLog,
    /// Running `start_server.sh debloat`.
    RunningDebloat,
    /// Writing `ca.pem` and `token`.
    WritingCredentials,
}

/// What the supervisor log said, reported with a rollback by the device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisorLog {
    /// The newest part of the log.
    Read(String),
    /// It could not be read; the text says why.
    Unavailable(String),
}

/// How an update ended.
#[derive(Clone, Debug)]
pub enum UpdateOutcome {
    /// The device came back running the bundled firmware.
    Updated {
        /// The version it ran before; None when unknown.
        from: Option<FirmwareVersion>,
        /// The bundled version it runs now.
        to: FirmwareVersion,
    },
    /// The new build failed to start three times and `start_server.sh` switched back.
    AutoRolledBack {
        /// The version that failed.
        attempted: FirmwareVersion,
        /// The version the device came back on.
        running: FirmwareVersion,
        /// The supervisor's log of the failed starts.
        supervisor_log: SupervisorLog,
    },
    /// The update did not happen, or its result is unknown.
    Failed(FirmwareFailure),
}

/// How a rollback ended.
#[derive(Clone, Debug)]
pub enum RollbackOutcome {
    /// The device came back on the other slot.
    RolledBack {
        /// The version it ran before; None when unknown.
        from: Option<FirmwareVersion>,
        /// The version it runs now.
        to: FirmwareVersion,
    },
    /// The other slot failed to start three times and `start_server.sh` switched back.
    Reverted {
        /// The version the device came back on.
        running: FirmwareVersion,
        /// The supervisor's log of the failed starts.
        supervisor_log: SupervisorLog,
    },
    /// The rollback did not happen, or its result is unknown.
    Failed(FirmwareFailure),
}

/// Why an operation was refused or failed.
#[derive(Clone, Debug)]
pub enum FirmwareFailure {
    /// Another operation is running on the device.
    Busy,
    /// The device already runs the bundled firmware.
    AlreadyCurrent,
    /// A LineageOS Echo Show runs nothing of Amazon's, so it has nothing to debloat.
    NothingToDebloat,
    /// The controller has no TLS listener, so there are no credentials to install.
    TlsUnavailable,
    /// The bundled firmware cannot be served.
    BundledFirmware(Arc<BundledFirmwareError>),
    /// A payload file cannot be read.
    Payload(Arc<PayloadError>),
    /// The database failed.
    Store(StoreError),
    /// The device's shell could not be opened.
    ShellOpen(ShellOpenError),
    /// A shell command failed.
    Shell(ShellFailure),
    /// `base64 -d` or `md5sum` does not work on the device.
    Tools(TransferError),
    /// A file transfer failed; the destination is unchanged.
    Transfer {
        /// The destination.
        path: DevicePath,
        /// Why.
        error: TransferError,
    },
    /// A command printed an error.
    CommandFailed {
        /// What was run, in words.
        action: &'static str,
        /// What it printed.
        output: String,
    },
    /// `server` does not link to `server_a` or `server_b`, so the device cannot roll back
    /// on its own and no slot is known to be safe to overwrite.
    UnknownSlot {
        /// What `readlink` printed.
        link: String,
    },
    /// The slot to roll back to is missing or not executable.
    SlotMissing {
        /// The slot.
        slot: Slot,
    },
    /// `ln -sf` failed; the firmware was not restarted.
    SwitchFailed {
        /// What it printed.
        output: String,
    },
    /// The device did not come back in time.
    NoReconnect {
        /// How long the controller waited.
        waited: Duration,
    },
    /// The device came back without reporting a firmware version.
    UnknownVersion,
    /// `start_server.sh debloat` printed no result.
    Debloat(DebloatParseError),
}

impl fmt::Display for FirmwareFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("another firmware operation is running on this device"),
            Self::AlreadyCurrent => f.write_str("the device already runs the bundled firmware"),
            Self::NothingToDebloat => f.write_str("nothing to debloat on LineageOS"),
            Self::TlsUnavailable => f.write_str("this controller has no TLS listener"),
            Self::BundledFirmware(error) => error.fmt(f),
            Self::Payload(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
            Self::ShellOpen(error) => error.fmt(f),
            Self::Shell(error) => error.fmt(f),
            Self::Tools(error) => write!(f, "the device cannot receive files: {error}"),
            Self::Transfer { path, error } => write!(f, "writing {path} failed: {error}"),
            Self::CommandFailed { action, output } => {
                write!(f, "{action} failed: {}", output.trim())
            }
            Self::UnknownSlot { link } => write!(
                f,
                "{SERVER_LINK} links to {:?}, not server_a or server_b",
                link.trim()
            ),
            Self::SlotMissing { slot } => {
                write!(f, "the {slot} slot is missing or not executable")
            }
            Self::SwitchFailed { output } => {
                write!(f, "switching the firmware slot failed: {}", output.trim())
            }
            Self::NoReconnect { waited } => write!(
                f,
                "the device did not reconnect within {} s",
                waited.as_secs()
            ),
            Self::UnknownVersion => {
                f.write_str("the device reconnected without reporting its firmware version")
            }
            Self::Debloat(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FirmwareFailure {}

impl From<ShellOpenError> for FirmwareFailure {
    fn from(error: ShellOpenError) -> Self {
        Self::ShellOpen(error)
    }
}

impl From<ShellFailure> for FirmwareFailure {
    fn from(error: ShellFailure) -> Self {
        Self::Shell(error)
    }
}

impl From<StoreError> for FirmwareFailure {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<PayloadError> for FirmwareFailure {
    fn from(error: PayloadError) -> Self {
        Self::Payload(Arc::new(error))
    }
}

/// How long each step may take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareTimings {
    /// From `shell_open` to the dial-back.
    pub shell_open: Duration,
    /// A short command: a probe, `readlink`, an md5 query, `mkdir`.
    pub command: Duration,
    /// Writing one transfer chunk.
    pub chunk: Duration,
    /// Hashing and placing a transferred file.
    pub verify: Duration,
    /// From restarting the firmware to its hello. Covers an automatic rollback: three
    /// failed starts of up to 15 s with 3 s pauses, then init restarting the script.
    pub reconnect: Duration,
    /// The longest silence from `start_server.sh debloat`.
    pub debloat_idle: Duration,
    /// The longest `start_server.sh debloat` may run.
    pub debloat_total: Duration,
}

impl Default for FirmwareTimings {
    fn default() -> Self {
        Self {
            shell_open: Duration::from_secs(15),
            command: Duration::from_secs(20),
            chunk: Duration::from_secs(60),
            verify: Duration::from_secs(120),
            reconnect: Duration::from_secs(240),
            debloat_idle: Duration::from_secs(60),
            debloat_total: Duration::from_secs(300),
        }
    }
}

impl FirmwareTimings {
    fn transfer(&self) -> TransferLimits {
        TransferLimits {
            command: CommandLimits::within(self.command),
            chunk: CommandLimits::within(self.chunk),
            verify: CommandLimits::within(self.verify),
        }
    }
}

/// What the service needs besides the integrator's traits.
pub struct FirmwareServiceConfig {
    /// The bundled firmware, loaded at startup.
    pub bundled: BundledFirmware,
    /// The payload directory (the image's `device_payloads/`).
    pub payloads: DevicePayloads,
    /// The CA devices pin for the TLS link (`ca.pem`); None when TLS is off.
    pub link_ca_pem: Option<String>,
    /// `EM_CONTROLLER_VERSION`, for provisioning diagnostics; None when unknown.
    pub controller_version: Option<String>,
    /// Step bounds.
    pub timings: FirmwareTimings,
    /// Wall time, for `update_queued_at` and diagnostics.
    pub clock: Arc<dyn WallClock>,
}

impl fmt::Debug for FirmwareServiceConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirmwareServiceConfig")
            .field("bundled", &self.bundled)
            .field("payloads", &self.payloads)
            .field("link_ca", &self.link_ca_pem.is_some())
            .field("controller_version", &self.controller_version)
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

/// The device an operation is for, as the integrator knows it from the live session and
/// the device's row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceTarget {
    /// The device.
    pub device: DeviceId,
    /// The image it runs on (from its hello).
    pub platform: Platform,
    /// The version it runs (from its hello); None when unknown.
    pub running: Option<FirmwareVersion>,
    /// The version in its rollback slot (`firmware_previous`); None when unknown.
    pub previous: Option<FirmwareVersion>,
}

/// Firmware delivery. Cheap to clone.
pub struct FirmwareService<D, S, E> {
    inner: Arc<Inner<D, S, E>>,
}

impl<D, S, E> Clone for FirmwareService<D, S, E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<D, S, E> fmt::Debug for FirmwareService<D, S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirmwareService")
            .field("bundled", self.inner.config.bundled.info())
            .finish_non_exhaustive()
    }
}

pub(crate) struct Inner<D, S, E> {
    pub(crate) access: D,
    pub(crate) store: S,
    pub(crate) events: E,
    pub(crate) config: FirmwareServiceConfig,
    hellos: HelloHub,
    busy: Arc<Mutex<HashSet<DeviceId>>>,
}

/// Holds a device's one operation; releases it when dropped.
#[derive(Debug)]
struct Claim {
    busy: Arc<Mutex<HashSet<DeviceId>>>,
    device: DeviceId,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.busy
            .lock()
            .expect("firmware busy set poisoned")
            .remove(&self.device);
    }
}

/// An update ready to run. Spawn [`run`](Self::run) under supervision.
pub struct UpdateJob<D, S, E> {
    inner: Arc<Inner<D, S, E>>,
    target: DeviceTarget,
    _claim: Claim,
}

impl<D, S, E> fmt::Debug for UpdateJob<D, S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateJob")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A rollback ready to run. Spawn [`run`](Self::run) under supervision.
pub struct RollbackJob<D, S, E> {
    inner: Arc<Inner<D, S, E>>,
    target: DeviceTarget,
    _claim: Claim,
}

impl<D, S, E> fmt::Debug for RollbackJob<D, S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RollbackJob")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A debloat ready to run. Spawn [`run`](Self::run) under supervision.
pub struct DebloatJob<D, S, E> {
    inner: Arc<Inner<D, S, E>>,
    target: DeviceTarget,
    _claim: Claim,
}

impl<D, S, E> fmt::Debug for DebloatJob<D, S, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DebloatJob")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> FirmwareService<D, S, E> {
    /// The service.
    pub fn new(access: D, store: S, events: E, config: FirmwareServiceConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                access,
                store,
                events,
                config,
                hellos: HelloHub::default(),
                busy: Arc::default(),
            }),
        }
    }

    pub(crate) fn inner(&self) -> &Inner<D, S, E> {
        &self.inner
    }

    /// The bundled firmware.
    pub fn bundled(&self) -> &BundledFirmware {
        &self.inner.config.bundled
    }

    /// Reports an admitted hello. Call it for every session the link admits, before
    /// [`queued_update_on_connect`](Self::queued_update_on_connect): an update or rollback
    /// in flight learns from it which build came up.
    pub fn observe_hello(&self, hello: &DeviceHello) {
        self.inner.hellos.publish(hello);
    }

    /// Whether an operation is running on the device.
    pub fn is_busy(&self, device: &DeviceId) -> bool {
        self.inner
            .busy
            .lock()
            .expect("firmware busy set poisoned")
            .contains(device)
    }

    fn claim(&self, device: &DeviceId) -> Result<Claim, FirmwareFailure> {
        let mut busy = self.inner.busy.lock().expect("firmware busy set poisoned");
        if !busy.insert(device.clone()) {
            return Err(FirmwareFailure::Busy);
        }
        Ok(Claim {
            busy: Arc::clone(&self.inner.busy),
            device: device.clone(),
        })
    }

    /// Reserves the device for an update of the bundled firmware. Refused while another
    /// operation runs on it, or when it already runs the bundled version.
    pub fn prepare_update(
        &self,
        target: DeviceTarget,
    ) -> Result<UpdateJob<D, S, E>, FirmwareFailure> {
        if target.running.as_ref() == Some(self.bundled().version()) {
            return Err(FirmwareFailure::AlreadyCurrent);
        }
        let claim = self.claim(&target.device)?;
        Ok(UpdateJob {
            inner: Arc::clone(&self.inner),
            target,
            _claim: claim,
        })
    }

    /// Reserves the device for a rollback to its other slot.
    pub fn prepare_rollback(
        &self,
        target: DeviceTarget,
    ) -> Result<RollbackJob<D, S, E>, FirmwareFailure> {
        let claim = self.claim(&target.device)?;
        Ok(RollbackJob {
            inner: Arc::clone(&self.inner),
            target,
            _claim: claim,
        })
    }

    /// Queues an install of the bundled firmware for the device's next connection. A
    /// queue already set keeps its time. Returns the stored time.
    pub async fn queue_update(&self, device: &DeviceId) -> Result<UnixSeconds, FirmwareFailure> {
        let now =
            UnixSeconds::from_system_time(self.inner.config.clock.now()).ok_or_else(|| {
                FirmwareFailure::Store(StoreError::Unavailable(
                    "the system clock is before 1970".into(),
                ))
            })?;
        let at = self.inner.store.queue_update(device, now).await?;
        self.inner.events.publish(FirmwareEvent::QueueChanged {
            device: device.clone(),
            queued_at: Some(at),
        });
        Ok(at)
    }

    /// Cancels the device's queued install. Returns whether one was queued.
    pub async fn cancel_queued_update(&self, device: &DeviceId) -> Result<bool, FirmwareFailure> {
        let cancelled = self.inner.store.take_queued_update(device).await?.is_some();
        if cancelled {
            self.inner.events.publish(FirmwareEvent::QueueChanged {
                device: device.clone(),
                queued_at: None,
            });
        }
        Ok(cancelled)
    }

    /// Call after [`observe_hello`](Self::observe_hello) for each admitted session. When
    /// an install is queued it is cleared, and an update job is returned unless the device
    /// already runs the bundled firmware. While another operation runs on the device the
    /// queue is left for a later connection.
    pub async fn queued_update_on_connect(
        &self,
        target: DeviceTarget,
    ) -> Result<Option<UpdateJob<D, S, E>>, FirmwareFailure> {
        let device = target.device.clone();
        let job = match self.prepare_update(target) {
            Ok(job) => Some(job),
            Err(FirmwareFailure::AlreadyCurrent) => None,
            Err(FirmwareFailure::Busy) => return Ok(None),
            Err(other) => return Err(other),
        };
        let Some(queued) = self.inner.store.take_queued_update(&device).await? else {
            return Ok(None);
        };
        tracing::info!(%device, queued_at = queued.0, installs = job.is_some(), "taking a queued firmware install");
        self.inner.events.publish(FirmwareEvent::QueueChanged {
            device,
            queued_at: None,
        });
        Ok(job)
    }

    /// Reserves the device for a debloat. Refused on a LineageOS Echo Show, which runs
    /// nothing of Amazon's, and while another operation runs on the device.
    pub fn prepare_debloat(
        &self,
        target: DeviceTarget,
    ) -> Result<DebloatJob<D, S, E>, FirmwareFailure> {
        if target.platform == Platform::Lineage18Crown {
            return Err(FirmwareFailure::NothingToDebloat);
        }
        let claim = self.claim(&target.device)?;
        Ok(DebloatJob {
            inner: Arc::clone(&self.inner),
            target,
            _claim: claim,
        })
    }

    /// Writes the controller's CA (`ca.pem`, 0644) and the device's link token (`token`,
    /// 0600, minted if it has none) into `/data/local/etc/echomuse/`, then ends the
    /// device's session so it redials over TLS.
    pub async fn secure_link(&self, device: &DeviceId) -> Result<(), FirmwareFailure> {
        let ca = self
            .inner
            .config
            .link_ca_pem
            .clone()
            .ok_or(FirmwareFailure::TlsUnavailable)?;
        let _claim = self.claim(device)?;
        let token = self.inner.store.ensure_link_token(device).await?;
        let mut shell = self.inner.open_shell(device, Operation::SecureLink).await?;
        let result: Result<(), FirmwareFailure> = async {
            let tools = self
                .inner
                .tools(&mut shell, device, Operation::SecureLink)
                .await?;
            self.inner
                .progress(device, Operation::SecureLink, Stage::WritingCredentials);
            self.inner.make_etc_dir(&mut shell).await?;
            let limits = self.inner.config.timings.transfer();
            for (path, data, mode) in [
                (CA_PEM, ca.as_bytes(), FileMode::READABLE),
                (LINK_TOKEN, token.expose().as_bytes(), FileMode::PRIVATE),
            ] {
                let path = DevicePath::fixed(path);
                transfer_file(
                    &mut shell,
                    &tools,
                    data,
                    &path,
                    mode,
                    &limits,
                    &mut |_, _| {},
                )
                .await
                .map_err(|error| FirmwareFailure::Transfer { path, error })?;
            }
            Ok(())
        }
        .await;
        shell.close().await;
        result?;
        let was_live = self.inner.access.disconnect(device);
        tracing::info!(%device, was_live, "installed link credentials; the device redials over TLS");
        Ok(())
    }
}

/// The payload files an operation installs, read once at its start.
struct Payloads {
    start_script: Payload,
    crown_lists: Vec<(CrownList, Payload)>,
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> Inner<D, S, E> {
    fn progress(&self, device: &DeviceId, operation: Operation, stage: Stage) {
        self.events.publish(FirmwareEvent::Progress {
            device: device.clone(),
            operation,
            stage,
        });
    }

    async fn open_shell(
        &self,
        device: &DeviceId,
        operation: Operation,
    ) -> Result<DeviceShell<D::Shell>, FirmwareFailure> {
        self.progress(device, operation, Stage::OpeningShell);
        let waited = self.config.timings.shell_open;
        match tokio::time::timeout(waited, self.access.open_shell(device)).await {
            Ok(Ok(io)) => Ok(DeviceShell::new(io)),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(ShellOpenError::NoDialBack { waited }.into()),
        }
    }

    async fn tools(
        &self,
        shell: &mut DeviceShell<D::Shell>,
        device: &DeviceId,
        operation: Operation,
    ) -> Result<ToolsReady, FirmwareFailure> {
        self.progress(device, operation, Stage::CheckingTools);
        check_tools(shell, &self.config.timings.transfer())
            .await
            .map_err(|error| match error {
                TransferError::Shell(failure) => FirmwareFailure::Shell(failure),
                other => FirmwareFailure::Tools(other),
            })
    }

    async fn load_payloads(&self, platform: Platform) -> Result<Payloads, FirmwareFailure> {
        let start_script = self.config.payloads.start_script().await?;
        let mut crown_lists = Vec::new();
        if platform == Platform::FireOs6Crown {
            for list in CrownList::ALL {
                crown_lists.push((list, self.config.payloads.crown_list(list).await?));
            }
        }
        Ok(Payloads {
            start_script,
            crown_lists,
        })
    }

    async fn make_etc_dir(&self, shell: &mut DeviceShell<D::Shell>) -> Result<(), FirmwareFailure> {
        let out = shell
            .run(
                &format!("mkdir -p {ETC_DIR} 2>&1"),
                CommandLimits::within(self.config.timings.command),
            )
            .await?;
        if out.status != 0 {
            return Err(FirmwareFailure::CommandFailed {
                action: "creating /data/local/etc/echomuse",
                output: out.text,
            });
        }
        Ok(())
    }

    /// Syncs the start script and any package lists by md5.
    async fn sync_payloads(
        &self,
        shell: &mut DeviceShell<D::Shell>,
        tools: &ToolsReady,
        device: &DeviceId,
        operation: Operation,
        payloads: &Payloads,
    ) -> Result<(), FirmwareFailure> {
        let limits = self.config.timings.transfer();
        self.progress(device, operation, Stage::SyncingStartScript);
        let path = DevicePath::fixed(START_SCRIPT);
        let synced = sync_file(
            shell,
            tools,
            &payloads.start_script.bytes,
            &path,
            FileMode::EXECUTABLE,
            &limits,
        )
        .await
        .map_err(|error| FirmwareFailure::Transfer { path, error })?;
        if synced == Synced::Written {
            tracing::info!(%device, md5 = %payloads.start_script.md5, "synced start_server.sh");
        }
        if payloads.crown_lists.is_empty() {
            return Ok(());
        }
        self.progress(device, operation, Stage::SyncingCrownLists);
        self.make_etc_dir(shell).await?;
        for (list, payload) in &payloads.crown_lists {
            let path = list.device_path();
            let synced = sync_file(
                shell,
                tools,
                &payload.bytes,
                &path,
                FileMode::READABLE,
                &limits,
            )
            .await
            .map_err(|error| FirmwareFailure::Transfer { path, error })?;
            if synced == Synced::Written {
                tracing::info!(%device, file = list.file_name(), "synced an Echo Show package list");
            }
        }
        Ok(())
    }

    async fn active_slot(
        &self,
        shell: &mut DeviceShell<D::Shell>,
        device: &DeviceId,
        operation: Operation,
    ) -> Result<Slot, FirmwareFailure> {
        self.progress(device, operation, Stage::ReadingSlot);
        let out = shell
            .run(
                &format!("readlink {SERVER_LINK} 2>/dev/null"),
                CommandLimits::within(self.config.timings.command),
            )
            .await?;
        Slot::from_link(out.text.trim()).ok_or(FirmwareFailure::UnknownSlot { link: out.text })
    }

    /// Points `server` at `slot` and kills the firmware (the shell's parent), then waits
    /// for the hello of whichever build `start_server.sh` brings up.
    async fn switch_and_wait(
        &self,
        mut shell: DeviceShell<D::Shell>,
        device: &DeviceId,
        operation: Operation,
        slot: Slot,
    ) -> Result<DeviceHello, FirmwareFailure> {
        let mut hellos = self.hellos.watch(device);
        self.progress(device, operation, Stage::Switching { slot });
        let ending = shell
            .run_until_end(
                &format!("ln -sf {slot} {SERVER_LINK} 2>&1 && kill $PPID"),
                CommandLimits::within(self.config.timings.command),
            )
            .await;
        match ending {
            Ok(Ending::Finished(out)) if out.status != 0 => {
                shell.close().await;
                return Err(FirmwareFailure::SwitchFailed { output: out.text });
            }
            // The firmware is gone, or going: any hello so far came before the restart.
            Ok(_) => hellos.discard_received(),
            // Unknown: the switch may have happened, so wait for what comes back.
            Err(error) => {
                tracing::warn!(%device, %error, "no answer to the slot switch; waiting for a reconnect");
            }
        }
        shell.close().await;
        self.progress(device, operation, Stage::WaitingForReconnect);
        let waited = self.config.timings.reconnect;
        match tokio::time::timeout(waited, hellos.next()).await {
            Ok(Some(hello)) => Ok(hello),
            Ok(None) | Err(_) => Err(FirmwareFailure::NoReconnect { waited }),
        }
    }

    async fn supervisor_log(&self, device: &DeviceId, operation: Operation) -> SupervisorLog {
        self.progress(device, operation, Stage::ReadingSupervisorLog);
        let mut shell = match self.open_shell(device, operation).await {
            Ok(shell) => shell,
            Err(error) => return SupervisorLog::Unavailable(error.to_string()),
        };
        let result = shell
            .run(
                &format!("tail -c {SUPERVISOR_LOG_TAIL_BYTES} {SUPERVISOR_LOG} 2>&1"),
                CommandLimits::within(self.config.timings.command),
            )
            .await;
        shell.close().await;
        match result {
            Ok(out) if out.status == 0 => SupervisorLog::Read(out.text),
            Ok(out) => SupervisorLog::Unavailable(out.text.trim().to_owned()),
            Err(error) => SupervisorLog::Unavailable(error.to_string()),
        }
    }

    async fn record_previous(&self, device: &DeviceId, version: Option<&FirmwareVersion>) {
        if let Err(error) = self.store.set_firmware_previous(device, version).await {
            tracing::warn!(%device, %error, "could not record the rollback slot's version");
        }
    }

    async fn update(&self, target: &DeviceTarget) -> Result<UpdateOutcome, FirmwareFailure> {
        let device = &target.device;
        let op = Operation::Update;
        let image = self
            .config
            .bundled
            .read_verified()
            .await
            .map_err(|error| FirmwareFailure::BundledFirmware(Arc::new(error)))?;
        let payloads = self.load_payloads(target.platform).await?;
        let mut shell = self.open_shell(device, op).await?;
        let prepared = async {
            let tools = self.tools(&mut shell, device, op).await?;
            let active = self.active_slot(&mut shell, device, op).await?;
            let slot = active.other();
            let path = slot.path();
            let limits = self.config.timings.transfer();
            let total = image.bytes.len() as u64;
            self.progress(device, op, Stage::Uploading { sent: 0, total });
            let mut report =
                |sent, total| self.progress(device, op, Stage::Uploading { sent, total });
            let placed = transfer_file(
                &mut shell,
                &tools,
                &image.bytes,
                &path,
                FileMode::EXECUTABLE,
                &limits,
                &mut report,
            )
            .await;
            if let Err(error) = placed {
                return Err(FirmwareFailure::Transfer { path, error });
            }
            // The rollback slot now holds a build that has not run yet.
            self.record_previous(device, None).await;
            self.sync_payloads(&mut shell, &tools, device, op, &payloads)
                .await?;
            Ok(slot)
        }
        .await;
        let slot = match prepared {
            Ok(slot) => slot,
            Err(failure) => {
                shell.close().await;
                return Err(failure);
            }
        };
        let hello = self.switch_and_wait(shell, device, op, slot).await?;
        let to = image.info.version;
        match hello.firmware_version {
            Some(running) if running == to => {
                self.record_previous(device, target.running.as_ref()).await;
                Ok(UpdateOutcome::Updated {
                    from: target.running.clone(),
                    to,
                })
            }
            // The device runs something other than the slot it was switched to, which only
            // start_server.sh's rollback does.
            Some(running) => Ok(UpdateOutcome::AutoRolledBack {
                attempted: to,
                running,
                supervisor_log: self.supervisor_log(device, op).await,
            }),
            None => Err(FirmwareFailure::UnknownVersion),
        }
    }

    async fn rollback(&self, target: &DeviceTarget) -> Result<RollbackOutcome, FirmwareFailure> {
        let device = &target.device;
        let op = Operation::Rollback;
        let mut shell = self.open_shell(device, op).await?;
        let checked = async {
            let active = self.active_slot(&mut shell, device, op).await?;
            let slot = active.other();
            let out = shell
                .run(
                    &format!("[ -x {} ] && echo SLOT_READY", slot.path()),
                    CommandLimits::within(self.config.timings.command),
                )
                .await?;
            if !out.has_line("SLOT_READY") {
                return Err(FirmwareFailure::SlotMissing { slot });
            }
            Ok(slot)
        }
        .await;
        let slot = match checked {
            Ok(slot) => slot,
            Err(failure) => {
                shell.close().await;
                return Err(failure);
            }
        };
        let hello = self.switch_and_wait(shell, device, op, slot).await?;
        let Some(running) = hello.firmware_version else {
            return Err(FirmwareFailure::UnknownVersion);
        };
        // Back on the version it left, and the other slot is not known to hold that same
        // version: start_server.sh switched back.
        let reverted =
            target.running.as_ref() == Some(&running) && target.previous.as_ref() != Some(&running);
        if reverted {
            self.record_previous(device, None).await;
            return Ok(RollbackOutcome::Reverted {
                running,
                supervisor_log: self.supervisor_log(device, op).await,
            });
        }
        self.record_previous(device, target.running.as_ref()).await;
        Ok(RollbackOutcome::RolledBack {
            from: target.running.clone(),
            to: running,
        })
    }

    async fn debloat(&self, target: &DeviceTarget) -> Result<DebloatReport, FirmwareFailure> {
        let device = &target.device;
        let op = Operation::Debloat;
        let payloads = self.load_payloads(target.platform).await?;
        let mut shell = self.open_shell(device, op).await?;
        let result = async {
            let tools = self.tools(&mut shell, device, op).await?;
            self.sync_payloads(&mut shell, &tools, device, op, &payloads)
                .await?;
            self.progress(device, op, Stage::RunningDebloat);
            let out = shell
                .run(
                    &format!("/system/bin/sh {START_SCRIPT} debloat 2>&1"),
                    CommandLimits {
                        idle: self.config.timings.debloat_idle,
                        total: self.config.timings.debloat_total,
                    },
                )
                .await?;
            parse_debloat_output(&out.text).map_err(FirmwareFailure::Debloat)
        }
        .await;
        shell.close().await;
        result
    }
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> UpdateJob<D, S, E> {
    /// The version this update installs.
    pub fn version(&self) -> &FirmwareVersion {
        self.inner.config.bundled.version()
    }

    /// The device.
    pub fn device(&self) -> &DeviceId {
        &self.target.device
    }

    /// Installs the bundled firmware into the inactive slot, syncs the start script (and
    /// on a Fire OS Echo Show the package lists), switches slots and waits for the device
    /// to come back. Publishes [`FirmwareEvent::UpdateFinished`] and returns the outcome.
    pub async fn run(self) -> UpdateOutcome {
        let outcome = match self.inner.update(&self.target).await {
            Ok(outcome) => outcome,
            Err(failure) => UpdateOutcome::Failed(failure),
        };
        let device = &self.target.device;
        match &outcome {
            UpdateOutcome::Updated { from, to } => {
                tracing::info!(%device, from = ?from.as_ref().map(FirmwareVersion::as_str), %to, "firmware updated");
            }
            UpdateOutcome::AutoRolledBack {
                attempted,
                running,
                supervisor_log,
            } => {
                tracing::warn!(%device, %attempted, %running, ?supervisor_log, "the new firmware failed to start; the device rolled back");
            }
            UpdateOutcome::Failed(failure) => {
                tracing::warn!(%device, %failure, "firmware update failed");
            }
        }
        self.inner.events.publish(FirmwareEvent::UpdateFinished {
            device: device.clone(),
            outcome: outcome.clone(),
        });
        outcome
    }
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> RollbackJob<D, S, E> {
    /// The device.
    pub fn device(&self) -> &DeviceId {
        &self.target.device
    }

    /// Switches to the other slot and waits for the device to come back. Publishes
    /// [`FirmwareEvent::RollbackFinished`] and returns the outcome.
    pub async fn run(self) -> RollbackOutcome {
        let outcome = match self.inner.rollback(&self.target).await {
            Ok(outcome) => outcome,
            Err(failure) => RollbackOutcome::Failed(failure),
        };
        let device = &self.target.device;
        match &outcome {
            RollbackOutcome::RolledBack { to, .. } => {
                tracing::info!(%device, %to, "firmware rolled back");
            }
            RollbackOutcome::Reverted { running, .. } => {
                tracing::warn!(%device, %running, "the rollback slot failed to start; the device switched back");
            }
            RollbackOutcome::Failed(failure) => {
                tracing::warn!(%device, %failure, "firmware rollback failed");
            }
        }
        self.inner.events.publish(FirmwareEvent::RollbackFinished {
            device: device.clone(),
            outcome: outcome.clone(),
        });
        outcome
    }
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> DebloatJob<D, S, E> {
    /// Syncs `start_server.sh` (and on a Fire OS Echo Show its package lists), then runs
    /// `start_server.sh debloat` and reads its result.
    pub async fn run(self) -> Result<DebloatReport, FirmwareFailure> {
        let result = self.inner.debloat(&self.target).await;
        let device = &self.target.device;
        match &result {
            Ok(report) => tracing::info!(%device, "{report}"),
            Err(failure) => tracing::warn!(%device, %failure, "debloat failed"),
        }
        result
    }
}
