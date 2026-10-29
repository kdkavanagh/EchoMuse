//! Firmware delivery and provisioning routes (spec §3.2, §4.10), over the integrator's
//! [`FirmwareControl`].
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/firmware` | signed in | the bundled firmware: `{version, size, sha256}` |
//! | `POST /api/firmware/deploy` | admin | `{version, started, skipped}` |
//! | `POST /api/devices/{id}/update` | admin | `{version}`; the install runs in the background |
//! | `POST /api/devices/{id}/rollback` | admin | `{started: true}`; it runs in the background |
//! | `POST /api/devices/{id}/update/queue` | admin | `{update_queued_at}` |
//! | `DELETE /api/devices/{id}/update/queue` | admin | `{cancelled}` |
//! | `POST /api/devices/{id}/debloat` | admin | `{started: true}`; it runs in the background |
//! | `POST /api/devices/{id}/secure_link` | admin | `{installed: true}` once the credentials are written |
//! | `GET /api/provision/start_script` | admin | `start_server.sh` as text |
//! | `GET /api/provision/crown_lists` | admin | `{dir, tiers_file, hide, mic_deny}` |
//! | `GET /api/provision/firmware` | admin | the binary, with `X-Firmware-Version` and `X-Firmware-Sha256` |
//! | `POST /api/provision/tls_credentials` `{device_id}` | admin | `{ca_pem, token, dir}` |
//! | `POST /api/provision/diagnostics` | admin | the upload with the controller's side added |
//!
//! Shapes follow dashboard.jsx (`doUpdate`, `doRollback`, `QueuedInstall`,
//! `DeployAllModal`, the setup wizard's `fetchProvision`). The device object carries
//! `update_in_progress` and `update_error` from [`FirmwareControl::status`]. The
//! provisioning routes are admin-only: the TLS answer holds a device's link token.

use std::{future::Future, pin::Pin};

use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use echomuse_core::types::DeviceId;
use echomuse_firmware::{
    FirmwareInfo,
    provision::{
        CrownLists, DiagnosticsUpload, FirmwareDownload, ProvisionDiagnostics, ProvisionError,
        StartScriptFile, TlsCredentials, TlsCredentialsRequest,
    },
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    auth::{AdminUser, AuthenticatedUser},
    body::JsonBody,
    error::{ApiError, ErrorCode},
    params::DevicePath,
    state::ApiState,
};

/// A device's firmware operation, as its device object shows it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UpdateStatus {
    /// An update or rollback is running.
    pub in_progress: bool,
    /// Why the latest update or rollback failed; `None` when it did not, or none ran.
    pub error: Option<String>,
}

/// Why a firmware operation did not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FirmwareRefusal {
    /// The device is not registered.
    UnknownDevice,
    /// The device has no live session.
    NotConnected,
    /// Another operation runs on the device.
    Busy,
    /// The device already runs the bundled firmware.
    AlreadyCurrent,
    /// The operation does not apply; the text says why.
    NotApplicable(String),
    /// The controller has no TLS listener.
    TlsUnavailable,
    /// The controller is shutting down.
    Unavailable,
    /// It failed; the text goes to the log and the client.
    Failed(String),
}

/// Why `deploy` skipped a device (dashboard.jsx `SKIP_REASONS`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeploySkipReason {
    /// The device is not connected.
    Offline,
    /// The device is not approved.
    NotApproved,
    /// It already runs the bundled firmware.
    AlreadyCurrent,
    /// An update already runs on it.
    UpdateInProgress,
}

/// One device `deploy` did not start.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeploySkip {
    /// The device.
    pub device_id: DeviceId,
    /// Why.
    pub reason: DeploySkipReason,
}

/// What `POST /api/firmware/deploy` started.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DeployReport {
    /// The bundled version being installed.
    pub version: String,
    /// The devices whose install started.
    pub started: Vec<DeviceId>,
    /// The devices left alone, and why.
    pub skipped: Vec<DeploySkip>,
}

/// Firmware delivery as the routes use it. The integrator implements it over its
/// firmware service and device runtimes.
pub trait FirmwareControl: Send + Sync + 'static {
    /// The bundled firmware.
    fn bundled(&self) -> FirmwareInfo;

    /// The device's firmware operation state. Answers without waiting.
    fn status(&self, device: &DeviceId) -> UpdateStatus;

    /// Starts installing the bundled firmware on a connected device; returns the
    /// version being installed.
    fn update(
        &self,
        device: DeviceId,
    ) -> impl Future<Output = Result<String, FirmwareRefusal>> + Send;

    /// Starts switching a connected device back to its other slot.
    fn rollback(
        &self,
        device: DeviceId,
    ) -> impl Future<Output = Result<(), FirmwareRefusal>> + Send;

    /// Queues an install for the device's next connection; returns its stored time in
    /// seconds since the Unix epoch.
    fn queue_update(
        &self,
        device: DeviceId,
    ) -> impl Future<Output = Result<i64, FirmwareRefusal>> + Send;

    /// Cancels a queued install; returns whether one was queued.
    fn cancel_queued_update(
        &self,
        device: DeviceId,
    ) -> impl Future<Output = Result<bool, FirmwareRefusal>> + Send;

    /// Starts re-applying the debloat on a connected device.
    fn debloat(&self, device: DeviceId)
    -> impl Future<Output = Result<(), FirmwareRefusal>> + Send;

    /// Installs link credentials on a connected device, which then redials over TLS.
    fn secure_link(
        &self,
        device: DeviceId,
    ) -> impl Future<Output = Result<(), FirmwareRefusal>> + Send;

    /// Starts an install on every connected, approved device not running the bundled
    /// firmware.
    fn deploy(&self) -> impl Future<Output = DeployReport> + Send;

    /// `start_server.sh`.
    fn provision_start_script(
        &self,
    ) -> impl Future<Output = Result<StartScriptFile, ProvisionError>> + Send;

    /// The Echo Show package lists.
    fn provision_crown_lists(
        &self,
    ) -> impl Future<Output = Result<CrownLists, ProvisionError>> + Send;

    /// The bundled firmware for the wizard to push.
    fn provision_firmware(
        &self,
    ) -> impl Future<Output = Result<FirmwareDownload, ProvisionError>> + Send;

    /// The CA and the device's link token.
    fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> impl Future<Output = Result<TlsCredentials, ProvisionError>> + Send;

    /// The wizard's diagnostics with the controller's side added.
    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError>;
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`FirmwareControl`] behind a pointer.
pub(crate) trait Firmware: Send + Sync + 'static {
    fn bundled(&self) -> FirmwareInfo;
    fn status(&self, device: &DeviceId) -> UpdateStatus;
    fn update(&self, device: DeviceId) -> BoxFuture<'_, Result<String, FirmwareRefusal>>;
    fn rollback(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>>;
    fn queue_update(&self, device: DeviceId) -> BoxFuture<'_, Result<i64, FirmwareRefusal>>;
    fn cancel_queued_update(
        &self,
        device: DeviceId,
    ) -> BoxFuture<'_, Result<bool, FirmwareRefusal>>;
    fn debloat(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>>;
    fn secure_link(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>>;
    fn deploy(&self) -> BoxFuture<'_, DeployReport>;
    fn provision_start_script(&self) -> BoxFuture<'_, Result<StartScriptFile, ProvisionError>>;
    fn provision_crown_lists(&self) -> BoxFuture<'_, Result<CrownLists, ProvisionError>>;
    fn provision_firmware(&self) -> BoxFuture<'_, Result<FirmwareDownload, ProvisionError>>;
    fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> BoxFuture<'_, Result<TlsCredentials, ProvisionError>>;
    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError>;
}

impl<F: FirmwareControl> Firmware for F {
    fn bundled(&self) -> FirmwareInfo {
        FirmwareControl::bundled(self)
    }
    fn status(&self, device: &DeviceId) -> UpdateStatus {
        FirmwareControl::status(self, device)
    }
    fn update(&self, device: DeviceId) -> BoxFuture<'_, Result<String, FirmwareRefusal>> {
        Box::pin(FirmwareControl::update(self, device))
    }
    fn rollback(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(FirmwareControl::rollback(self, device))
    }
    fn queue_update(&self, device: DeviceId) -> BoxFuture<'_, Result<i64, FirmwareRefusal>> {
        Box::pin(FirmwareControl::queue_update(self, device))
    }
    fn cancel_queued_update(
        &self,
        device: DeviceId,
    ) -> BoxFuture<'_, Result<bool, FirmwareRefusal>> {
        Box::pin(FirmwareControl::cancel_queued_update(self, device))
    }
    fn debloat(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(FirmwareControl::debloat(self, device))
    }
    fn secure_link(&self, device: DeviceId) -> BoxFuture<'_, Result<(), FirmwareRefusal>> {
        Box::pin(FirmwareControl::secure_link(self, device))
    }
    fn deploy(&self) -> BoxFuture<'_, DeployReport> {
        Box::pin(FirmwareControl::deploy(self))
    }
    fn provision_start_script(&self) -> BoxFuture<'_, Result<StartScriptFile, ProvisionError>> {
        Box::pin(FirmwareControl::provision_start_script(self))
    }
    fn provision_crown_lists(&self) -> BoxFuture<'_, Result<CrownLists, ProvisionError>> {
        Box::pin(FirmwareControl::provision_crown_lists(self))
    }
    fn provision_firmware(&self) -> BoxFuture<'_, Result<FirmwareDownload, ProvisionError>> {
        Box::pin(FirmwareControl::provision_firmware(self))
    }
    fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> BoxFuture<'_, Result<TlsCredentials, ProvisionError>> {
        Box::pin(FirmwareControl::provision_tls_credentials(self, request))
    }
    fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError> {
        FirmwareControl::provision_diagnostics(self, upload)
    }
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/firmware", get(bundled))
        .route("/api/firmware/deploy", post(deploy))
        .route("/api/devices/{id}/update", post(update))
        .route("/api/devices/{id}/rollback", post(rollback))
        .route(
            "/api/devices/{id}/update/queue",
            post(queue_update).delete(cancel_queued_update),
        )
        .route("/api/devices/{id}/debloat", post(debloat))
        .route("/api/devices/{id}/secure_link", post(secure_link))
        .route("/api/provision/start_script", get(start_script))
        .route("/api/provision/crown_lists", get(crown_lists))
        .route("/api/provision/firmware", get(firmware_download))
        .route("/api/provision/tls_credentials", post(tls_credentials))
        .route("/api/provision/diagnostics", post(diagnostics))
}

fn refused(refusal: FirmwareRefusal, doing: &'static str) -> ApiError {
    match refusal {
        FirmwareRefusal::UnknownDevice => ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "No device has this id",
        ),
        FirmwareRefusal::NotConnected => ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::NotConnected,
            "The device is not connected",
        ),
        FirmwareRefusal::Busy => ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::Busy,
            "Another firmware operation is running on the device",
        ),
        FirmwareRefusal::AlreadyCurrent => ApiError::new(
            StatusCode::CONFLICT,
            ErrorCode::AlreadyCurrent,
            "The device already runs the bundled firmware",
        ),
        FirmwareRefusal::NotApplicable(reason) => {
            ApiError::new(StatusCode::CONFLICT, ErrorCode::NotApplicable, reason)
        }
        FirmwareRefusal::TlsUnavailable => {
            ApiError::unavailable("This controller has no TLS listener")
        }
        FirmwareRefusal::Unavailable => ApiError::unavailable("Firmware delivery is shutting down"),
        FirmwareRefusal::Failed(reason) => {
            eprintln!("api: {doing} failed: {reason}");
            ApiError::internal(format!("The controller could not {doing}: {reason}"))
        }
    }
}

fn provision_error(error: ProvisionError) -> ApiError {
    let status = error.status();
    let code = match &error {
        ProvisionError::InvalidRequest(_) => ErrorCode::BadRequest,
        ProvisionError::TlsUnavailable => ErrorCode::Unavailable,
        ProvisionError::Firmware(_) | ProvisionError::Payload(_) | ProvisionError::Store(_) => {
            eprintln!("api: a provisioning request failed: {error}");
            ErrorCode::Internal
        }
    };
    ApiError::new(status, code, error.to_string())
}

async fn bundled(State(state): State<ApiState>, _user: AuthenticatedUser) -> Json<FirmwareInfo> {
    Json(state.firmware().bundled())
}

async fn deploy(State(state): State<ApiState>, AdminUser(_): AdminUser) -> Json<DeployReport> {
    Json(state.firmware().deploy().await)
}

async fn update(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    let version = state
        .firmware()
        .update(device)
        .await
        .map_err(|r| refused(r, "start the update"))?;
    Ok(Json(json!({"version": version})))
}

async fn rollback(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    state
        .firmware()
        .rollback(device)
        .await
        .map_err(|r| refused(r, "start the rollback"))?;
    Ok(Json(json!({"started": true})))
}

async fn queue_update(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    let at = state
        .firmware()
        .queue_update(device)
        .await
        .map_err(|r| refused(r, "queue the install"))?;
    Ok(Json(json!({"update_queued_at": at})))
}

async fn cancel_queued_update(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    let cancelled = state
        .firmware()
        .cancel_queued_update(device)
        .await
        .map_err(|r| refused(r, "cancel the queued install"))?;
    Ok(Json(json!({"cancelled": cancelled})))
}

async fn debloat(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    state
        .firmware()
        .debloat(device)
        .await
        .map_err(|r| refused(r, "start the debloat"))?;
    Ok(Json(json!({"started": true})))
}

async fn secure_link(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    state
        .firmware()
        .secure_link(device)
        .await
        .map_err(|r| refused(r, "install the link credentials"))?;
    Ok(Json(json!({"installed": true})))
}

async fn start_script(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
) -> Result<Response, ApiError> {
    let file = state
        .firmware()
        .provision_start_script()
        .await
        .map_err(provision_error)?;
    Ok((
        [(header::CONTENT_TYPE, StartScriptFile::CONTENT_TYPE)],
        Body::from(file.bytes),
    )
        .into_response())
}

async fn crown_lists(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
) -> Result<Json<CrownLists>, ApiError> {
    Ok(Json(
        state
            .firmware()
            .provision_crown_lists()
            .await
            .map_err(provision_error)?,
    ))
}

async fn firmware_download(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
) -> Result<Response, ApiError> {
    let download = state
        .firmware()
        .provision_firmware()
        .await
        .map_err(provision_error)?;
    Ok((download.headers(), Body::from(download.bytes)).into_response())
}

async fn tls_credentials(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    JsonBody(request): JsonBody<TlsCredentialsRequest>,
) -> Result<Json<TlsCredentials>, ApiError> {
    Ok(Json(
        state
            .firmware()
            .provision_tls_credentials(request)
            .await
            .map_err(provision_error)?,
    ))
}

async fn diagnostics(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    JsonBody(upload): JsonBody<DiagnosticsUpload>,
) -> Result<Json<ProvisionDiagnostics>, ApiError> {
    Ok(Json(
        state
            .firmware()
            .provision_diagnostics(upload)
            .map_err(provision_error)?,
    ))
}
