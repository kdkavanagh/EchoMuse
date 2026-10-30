//! What every handler shares.

use std::{path::Path, sync::Arc};

use echomuse_db::{DbHandle, devices::ApprovalMode};
use echomuse_records::RecordsService;
use echomuse_settings::service::SettingsHandle;
use tokio::sync::{Mutex, watch};

use crate::{
    auth::{password::Passwords, throttle::Throttle, token::BootstrapToken},
    clock::WallClock,
    events::EventHub,
    firmware::FirmwareControl,
    ingress::Ingress,
    runtime::DeviceRuntimes,
    system::HaStatus,
    wake_models::ModelQualifier,
};

/// The server's shared state. Cheap to clone.
#[derive(Clone)]
pub(crate) struct ApiState(Arc<Inner>);

pub(crate) struct Inner {
    pub(crate) db: DbHandle,
    pub(crate) clock: Box<dyn WallClock>,
    pub(crate) passwords: Passwords,
    pub(crate) throttle: Throttle,
    /// The setup token until setup succeeds. The lock also keeps two setups apart.
    pub(crate) bootstrap: Mutex<Option<BootstrapToken>>,
    pub(crate) ingress: Ingress,
    pub(crate) static_dir: Box<Path>,
    pub(crate) events: EventHub,
    pub(crate) settings: SettingsHandle,
    pub(crate) runtimes: Box<dyn DeviceRuntimes>,
    pub(crate) qualifier: Box<dyn ModelQualifier>,
    pub(crate) firmware: Box<dyn FirmwareControl>,
    pub(crate) records: RecordsService,
    pub(crate) services: crate::services::DeviceServices,
    pub(crate) capture: std::sync::Arc<dyn crate::capture::CaptureControl>,
    pub(crate) diagnostics: crate::diagnostics::Diagnostics,
    pub(crate) ha_status: watch::Receiver<HaStatus>,
    pub(crate) approval_fallback: ApprovalMode,
    pub(crate) controller_version: Option<String>,
}

impl ApiState {
    pub(crate) fn new(inner: Inner) -> Self {
        Self(Arc::new(inner))
    }

    pub(crate) fn diagnostics(&self) -> &crate::diagnostics::Diagnostics {
        &self.0.diagnostics
    }

    pub(crate) fn db(&self) -> &DbHandle {
        &self.0.db
    }

    pub(crate) fn clock(&self) -> &dyn WallClock {
        &*self.0.clock
    }

    pub(crate) fn passwords(&self) -> &Passwords {
        &self.0.passwords
    }

    pub(crate) fn throttle(&self) -> &Throttle {
        &self.0.throttle
    }

    pub(crate) fn bootstrap(&self) -> &Mutex<Option<BootstrapToken>> {
        &self.0.bootstrap
    }

    pub(crate) fn ingress(&self) -> Ingress {
        self.0.ingress
    }

    pub(crate) fn static_dir(&self) -> &Path {
        &self.0.static_dir
    }

    pub(crate) fn events(&self) -> &EventHub {
        &self.0.events
    }

    pub(crate) fn settings(&self) -> &SettingsHandle {
        &self.0.settings
    }

    pub(crate) fn runtimes(&self) -> &dyn DeviceRuntimes {
        &*self.0.runtimes
    }

    pub(crate) fn qualifier(&self) -> &dyn ModelQualifier {
        &*self.0.qualifier
    }

    pub(crate) fn firmware(&self) -> &dyn FirmwareControl {
        &*self.0.firmware
    }

    pub(crate) fn records(&self) -> &RecordsService {
        &self.0.records
    }

    pub(crate) fn services(&self) -> &crate::services::DeviceServices {
        &self.0.services
    }

    pub(crate) fn capture(&self) -> &dyn crate::capture::CaptureControl {
        &*self.0.capture
    }

    /// The latest Home Assistant status.
    pub(crate) fn ha_status(&self) -> HaStatus {
        self.0.ha_status.borrow().clone()
    }

    /// The deployment's `DEVICE_APPROVAL`, used while the database holds no policy.
    pub(crate) fn approval_fallback(&self) -> ApprovalMode {
        self.0.approval_fallback
    }

    pub(crate) fn controller_version(&self) -> Option<&str> {
        self.0.controller_version.as_deref()
    }
}
