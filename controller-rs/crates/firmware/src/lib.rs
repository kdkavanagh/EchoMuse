//! Firmware delivery and provisioning (spec §3.2, §4.10): the bundled firmware, file
//! transfer through the device shell, update, rollback, the install queue, debloat, the
//! secure link and the files the setup wizard installs.
//!
//! Devices already in the field can only be updated through their shell, so this crate
//! speaks to mksh and toybox: [`DeviceShell`] runs commands in a pipe shell and
//! [`transfer`] moves files as base64 heredocs, checked by `md5sum` and placed with `mv`.
//!
//! # How the integrator wires it
//!
//! 1. At startup, load [`BundledFirmware::load`] from `FIRMWARE_DIR` (refuse to start on
//!    an error) and point [`DevicePayloads::new`] at the payload directory (the image's
//!    `device_payloads/`).
//! 2. Implement [`DeviceAccess`] over the live sessions: `open_shell` can call
//!    [`open_pipe_shell`] with the link server's
//!    [`ShellBroker`](echomuse_link::ShellBroker) and the device's
//!    [`SessionLink`](echomuse_link::SessionLink); `disconnect` closes that session.
//!    Implement [`FirmwareEvents`] to turn [`FirmwareEvent`]s into dashboard state and
//!    live events. [`FirmwareStore`] is implemented for
//!    [`DbHandle`](echomuse_db::DbHandle).
//! 3. Build a [`FirmwareService`]. For every session the link admits, call
//!    [`FirmwareService::observe_hello`], then
//!    [`FirmwareService::queued_update_on_connect`] and spawn any job it returns.
//! 4. Routes call [`FirmwareService::prepare_update`] / [`prepare_rollback`] /
//!    [`prepare_debloat`] (answering at once, spawning the job's `run` under
//!    supervision), [`queue_update`] / [`cancel_queued_update`], [`secure_link`], and
//!    the `provision_*` responders in [`provision`].
//!
//! [`prepare_rollback`]: FirmwareService::prepare_rollback
//! [`prepare_debloat`]: FirmwareService::prepare_debloat
//! [`queue_update`]: FirmwareService::queue_update
//! [`cancel_queued_update`]: FirmwareService::cancel_queued_update
//! [`secure_link`]: FirmwareService::secure_link
//!
//! # The update, step by step
//!
//! The firmware lives in `/data/local/bin/server_a` and `server_b`; `server` is a
//! relative symlink to the active one, and `start_server.sh` runs and supervises it. An
//! update writes the bundled binary over the inactive slot (never deleting it first),
//! syncs `start_server.sh` and, on a Fire OS Echo Show, the package lists by md5, then
//! runs `ln -sf <slot> /data/local/bin/server && kill $PPID`: the shell's parent is the
//! firmware, so the supervisor restarts it on the new slot. The next hello says how it
//! went: the bundled version means updated; any other version means the new build
//! failed three fast starts and `start_server.sh` switched back, which is reported with
//! the tail of its supervisor log. A rollback switches the symlink the same way.
//!
//! `firmware_previous` follows what the rollback slot holds: cleared once the new build
//! is written there (an untested build), set to the old version once the new one is
//! running, and left cleared after an automatic rollback (the slot holds a build that
//! failed to start).

#![warn(missing_docs)]

mod bundled;
pub mod debloat;
mod hub;
pub mod layout;
mod payloads;
pub mod provision;
mod service;
mod shell;
mod store;
pub mod transfer;
mod types;

#[cfg(test)]
mod tests;

pub use bundled::{
    BundledFirmware, BundledFirmwareError, FirmwareImage, FirmwareInfo, SERVER_FILE, VERSION_FILE,
};
pub use debloat::{DebloatReport, HideResult, MicrophoneResult, parse_debloat_output};
pub use hub::DeviceHello;
pub use payloads::{DevicePayloads, Payload, PayloadError, START_SCRIPT_FILE, list_packages};
pub use service::{
    DebloatJob, DeviceAccess, DeviceTarget, FirmwareEvent, FirmwareEvents, FirmwareFailure,
    FirmwareService, FirmwareServiceConfig, FirmwareTimings, Operation, RollbackJob,
    RollbackOutcome, SUPERVISOR_LOG_TAIL_BYTES, Stage, SupervisorLog, UpdateJob, UpdateOutcome,
};
pub use shell::{
    CommandLimits, CommandOutput, DeviceShell, Ending, LinkShell, ShellClosed, ShellFailure,
    ShellIo, ShellOpenError, open_pipe_shell,
};
pub use store::{FirmwareStore, StoreError};
pub use types::{FirmwareVersion, LinkToken, Md5};
