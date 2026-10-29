//! What the provisioning wizard fetches and posts (spec §3.2 "Wizard endpoints"), as
//! typed responders the API routes call:
//!
//! | Route | Responder |
//! |---|---|
//! | `GET /api/provision/start_script` | [`FirmwareService::provision_start_script`] |
//! | `GET /api/provision/crown_lists` | [`FirmwareService::provision_crown_lists`] |
//! | `GET /api/provision/firmware` | [`FirmwareService::provision_firmware`] |
//! | `POST /api/provision/tls_credentials` | [`FirmwareService::provision_tls_credentials`] |
//! | `POST /api/provision/diagnostics` | [`FirmwareService::provision_diagnostics`] |
//!
//! The wizard runs in the dashboard over WebUSB adb and pushes these files itself; a
//! device mid-wizard has no link session, which is why they are served over HTTP.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use http::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use http::{HeaderName, StatusCode};
use serde::{Deserialize, Serialize};

use crate::bundled::{BundledFirmwareError, FirmwareInfo};
use crate::layout::{CROWN_TIERS, CrownList, ETC_DIR};
use crate::payloads::{PayloadError, list_packages};
use crate::service::{DeviceAccess, FirmwareEvents, FirmwareService};
use crate::store::{FirmwareStore, StoreError};
use crate::types::LinkToken;

/// The firmware download's version header.
pub const HEADER_FIRMWARE_VERSION: HeaderName = HeaderName::from_static("x-firmware-version");
/// The firmware download's SHA-256 header (lowercase hex).
pub const HEADER_FIRMWARE_SHA256: HeaderName = HeaderName::from_static("x-firmware-sha256");

/// The longest device id (`ro.serialno`) accepted.
const MAX_DEVICE_ID: usize = 64;
const MAX_STEP: usize = 64;
const MAX_ERROR: usize = 16 * 1024;
const MAX_PROBES: usize = 64;
const MAX_PROBE_NAME: usize = 64;
const MAX_PROBE_OUTPUT: usize = 64 * 1024;
const MAX_TRANSCRIPT_LINES: usize = 5000;
const MAX_TRANSCRIPT_LINE: usize = 16 * 1024;
const MAX_SSID: usize = 64;

/// Why a provisioning request has no answer.
#[derive(Clone, Debug)]
pub enum ProvisionError {
    /// The request is malformed; the text says how.
    InvalidRequest(String),
    /// The controller has no TLS listener (the wizard then provisions a plain link).
    TlsUnavailable,
    /// The bundled firmware cannot be served.
    Firmware(Arc<BundledFirmwareError>),
    /// A payload file cannot be served.
    Payload(Arc<PayloadError>),
    /// The database failed.
    Store(StoreError),
}

impl ProvisionError {
    /// The HTTP status to answer with.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            Self::TlsUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Firmware(_) | Self::Payload(_) | Self::Store(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(reason) => f.write_str(reason),
            Self::TlsUnavailable => f.write_str("this controller has no TLS listener"),
            Self::Firmware(error) => error.fmt(f),
            Self::Payload(error) => error.fmt(f),
            Self::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ProvisionError {}

impl From<PayloadError> for ProvisionError {
    fn from(error: PayloadError) -> Self {
        Self::Payload(Arc::new(error))
    }
}

/// `start_server.sh`, served as text exactly as stored.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartScriptFile {
    /// The bytes.
    pub bytes: Bytes,
}

impl StartScriptFile {
    /// The response's `Content-Type`.
    pub const CONTENT_TYPE: &'static str = "text/plain; charset=utf-8";
}

/// One package list as the wizard pushes it: `{file, text, packages}`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PackageList {
    /// The file name, the same on the device.
    pub file: &'static str,
    /// The file's text, exactly as stored.
    pub text: String,
    /// The packages it names.
    pub packages: Vec<String>,
}

/// A hide list with its tier: `{tier, file, text, packages}`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HideList {
    /// `"1"` or `"2"`, as `crown_tiers` names it.
    pub tier: &'static str,
    /// The list.
    #[serde(flatten)]
    pub list: PackageList,
}

/// `GET /api/provision/crown_lists`: `{dir, tiers_file, hide, mic_deny}`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CrownLists {
    /// Where the lists go on the device.
    pub dir: &'static str,
    /// The file recording the applied tiers.
    pub tiers_file: &'static str,
    /// The two hide lists, tier 1 first.
    pub hide: Vec<HideList>,
    /// The microphone-denial list.
    pub mic_deny: PackageList,
}

/// `GET /api/provision/firmware`: the bundled binary and its headers.
#[derive(Clone)]
pub struct FirmwareDownload {
    /// The binary, checked against what startup recorded.
    pub bytes: Bytes,
    /// Its version, size and SHA-256.
    pub info: FirmwareInfo,
}

impl fmt::Debug for FirmwareDownload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirmwareDownload")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl FirmwareDownload {
    /// `Content-Type: application/octet-stream`, `X-Firmware-Version` and
    /// `X-Firmware-Sha256`.
    pub fn headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            HEADER_FIRMWARE_VERSION,
            HeaderValue::from_str(self.info.version.as_str())
                .expect("a firmware version is a valid header value"),
        );
        headers.insert(
            HEADER_FIRMWARE_SHA256,
            HeaderValue::from_str(self.info.sha256.as_str())
                .expect("hex digits are a valid header value"),
        );
        headers
    }
}

/// `POST /api/provision/tls_credentials` body: `{device_id}`, the device's
/// `ro.serialno`.
#[derive(Clone, Debug, Deserialize)]
pub struct TlsCredentialsRequest {
    /// The device id.
    pub device_id: String,
}

/// `POST /api/provision/tls_credentials` answer: `{ca_pem, token, dir}`. The token is
/// in the body by design; `Debug` never shows it.
#[derive(Clone, Serialize)]
pub struct TlsCredentials {
    /// The CA the device pins.
    pub ca_pem: String,
    /// The device's link token.
    #[serde(serialize_with = "expose_token")]
    pub token: LinkToken,
    /// Where `ca.pem` and `token` go on the device.
    pub dir: &'static str,
}

fn expose_token<S: serde::Serializer>(token: &LinkToken, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(token.expose())
}

impl fmt::Debug for TlsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TlsCredentials")
            .field("token", &self.token)
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// `POST /api/provision/diagnostics` body: what the wizard collected after a failed step.
#[derive(Clone, Debug, Deserialize)]
pub struct DiagnosticsUpload {
    /// The failed step's id.
    pub step: String,
    /// The failure's message.
    #[serde(default)]
    pub error: String,
    /// Each probe's name and output.
    #[serde(default)]
    pub probes: BTreeMap<String, String>,
    /// The wizard's log lines.
    #[serde(default)]
    pub transcript: Vec<String>,
    /// The Wi-Fi network chosen in the wizard, if any.
    #[serde(default)]
    pub selected_ssid: Option<String>,
}

/// `POST /api/provision/diagnostics` answer: the upload with the controller's side added.
/// The wizard offers it to the operator as a download.
#[derive(Clone, Debug, Serialize)]
pub struct ProvisionDiagnostics {
    /// When the controller received it, in milliseconds since the Unix epoch; None when
    /// the clock reads before 1970.
    pub received_utc_ms: Option<u64>,
    /// The controller's version; None when unknown.
    pub controller_version: Option<String>,
    /// The firmware the wizard installs.
    pub bundled_firmware: FirmwareInfo,
    /// The failed step's id.
    pub step: String,
    /// The failure's message.
    pub error: String,
    /// The Wi-Fi network chosen in the wizard, if any.
    pub selected_ssid: Option<String>,
    /// Each probe's name and output.
    pub probes: BTreeMap<String, String>,
    /// The wizard's log lines.
    pub transcript: Vec<String>,
}

fn invalid(reason: impl Into<String>) -> ProvisionError {
    ProvisionError::InvalidRequest(reason.into())
}

/// A device id as `ro.serialno` gives it: 1 to 64 visible ASCII characters.
fn parse_device_id(text: &str) -> Result<DeviceId, ProvisionError> {
    let valid = !text.is_empty()
        && text.len() <= MAX_DEVICE_ID
        && text.bytes().all(|b| b.is_ascii_graphic());
    if !valid {
        return Err(invalid(
            "device_id must be 1 to 64 visible ASCII characters",
        ));
    }
    Ok(DeviceId(text.to_owned()))
}

impl DiagnosticsUpload {
    fn check(&self) -> Result<(), ProvisionError> {
        if self.step.is_empty() || self.step.len() > MAX_STEP {
            return Err(invalid("step must be 1 to 64 characters"));
        }
        if self.error.len() > MAX_ERROR {
            return Err(invalid("error is too long"));
        }
        if self.probes.len() > MAX_PROBES {
            return Err(invalid("too many probes"));
        }
        if self
            .probes
            .iter()
            .any(|(name, output)| name.len() > MAX_PROBE_NAME || output.len() > MAX_PROBE_OUTPUT)
        {
            return Err(invalid("a probe's name or output is too long"));
        }
        if self.transcript.len() > MAX_TRANSCRIPT_LINES
            || self
                .transcript
                .iter()
                .any(|l| l.len() > MAX_TRANSCRIPT_LINE)
        {
            return Err(invalid("the transcript is too long"));
        }
        if self
            .selected_ssid
            .as_ref()
            .is_some_and(|s| s.len() > MAX_SSID)
        {
            return Err(invalid("selected_ssid is too long"));
        }
        Ok(())
    }
}

impl<D: DeviceAccess, S: FirmwareStore, E: FirmwareEvents> FirmwareService<D, S, E> {
    /// `start_server.sh`, read from the payload directory.
    pub async fn provision_start_script(&self) -> Result<StartScriptFile, ProvisionError> {
        let payload = self.inner().config.payloads.start_script().await?;
        Ok(StartScriptFile {
            bytes: payload.bytes,
        })
    }

    /// The three Echo Show package lists.
    pub async fn provision_crown_lists(&self) -> Result<CrownLists, ProvisionError> {
        let payloads = &self.inner().config.payloads;
        let mut hide = Vec::new();
        let mut mic_deny = None;
        for list in CrownList::ALL {
            let payload = payloads.crown_list(list).await?;
            let text = payload.text()?.to_owned();
            let entry = PackageList {
                file: list.file_name(),
                packages: list_packages(&text),
                text,
            };
            match list.tier() {
                Some(tier) => hide.push(HideList { tier, list: entry }),
                None => mic_deny = Some(entry),
            }
        }
        Ok(CrownLists {
            dir: ETC_DIR,
            tiers_file: CROWN_TIERS,
            hide,
            mic_deny: mic_deny.expect("CrownList::ALL has the microphone list"),
        })
    }

    /// The bundled firmware, refused if it changed since startup.
    pub async fn provision_firmware(&self) -> Result<FirmwareDownload, ProvisionError> {
        let image = self
            .bundled()
            .read_verified()
            .await
            .map_err(|error| ProvisionError::Firmware(Arc::new(error)))?;
        Ok(FirmwareDownload {
            bytes: image.bytes,
            info: image.info,
        })
    }

    /// The CA and the device's link token (minted, with a pending row, if it has none).
    /// Approval is unchanged.
    pub async fn provision_tls_credentials(
        &self,
        request: TlsCredentialsRequest,
    ) -> Result<TlsCredentials, ProvisionError> {
        let device = parse_device_id(&request.device_id)?;
        let ca_pem = self
            .inner()
            .config
            .link_ca_pem
            .clone()
            .ok_or(ProvisionError::TlsUnavailable)?;
        let token = self
            .inner()
            .store
            .ensure_link_token(&device)
            .await
            .map_err(ProvisionError::Store)?;
        tracing::info!(%device, "issued link credentials to the provisioning wizard");
        Ok(TlsCredentials {
            ca_pem,
            token,
            dir: ETC_DIR,
        })
    }

    /// Checks a diagnostics upload and returns it with the controller's side added.
    pub fn provision_diagnostics(
        &self,
        upload: DiagnosticsUpload,
    ) -> Result<ProvisionDiagnostics, ProvisionError> {
        upload.check()?;
        let config = &self.inner().config;
        let received_utc_ms = config
            .clock
            .now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|since| u64::try_from(since.as_millis()).ok());
        tracing::warn!(
            step = %upload.step,
            error = %upload.error.lines().next().unwrap_or(""),
            probes = upload.probes.len(),
            "a provisioning step failed; the wizard sent diagnostics"
        );
        Ok(ProvisionDiagnostics {
            received_utc_ms,
            controller_version: config.controller_version.clone(),
            bundled_firmware: config.bundled.info().clone(),
            step: upload.step,
            error: upload.error,
            selected_ssid: upload.selected_ssid,
            probes: upload.probes,
            transcript: upload.transcript,
        })
    }
}
