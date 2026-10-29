//! The controller's side of device link admission (spec §3.1.4, §5.7 step 1) and the
//! asset socket's content (§3.1.9).

use std::{io, path::PathBuf};

use bytes::Bytes;
use echomuse_link::{Admission, AdmissionDecision, AdmissionRequest, AssetSource, LinkSession};
use echomuse_protocol::{Capability, Sha256};
use echomuse_settings::service::SettingsHandle;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::speech_models::SpeechStatus;

use crate::{app::Registry, device::LinkAttach};

/// The runtime asset every device loads: the Android ONNX Runtime library, hashed once
/// at startup.
#[derive(Clone, Debug)]
pub struct RuntimeAsset {
    pub path: PathBuf,
    pub sha256: Sha256,
}

/// Admits sessions of approved devices into their runtimes.
pub struct ControllerAdmission {
    registry: Registry,
    settings: SettingsHandle,
    runtime_asset: Sha256,
    closing: CancellationToken,
    speech: watch::Receiver<SpeechStatus>,
}

impl ControllerAdmission {
    /// `closing` stops admission when cancelled: every later hello is declined.
    /// `speech` decides whether a session's `session.ready` grants capture.
    pub fn new(
        registry: Registry,
        settings: SettingsHandle,
        runtime_asset: Sha256,
        closing: CancellationToken,
        speech: watch::Receiver<SpeechStatus>,
    ) -> Self {
        Self {
            registry,
            settings,
            runtime_asset,
            closing,
            speech,
        }
    }

    async fn decide(
        &self,
        request: AdmissionRequest,
        session: LinkSession,
    ) -> Result<AdmissionDecision, String> {
        if self.closing.is_cancelled() {
            return Err("the controller is shutting down".to_owned());
        }
        if !request.device_row.approved {
            return Err("the device is not approved".to_owned());
        }
        if self.registry.is_retiring(&request.device) {
            return Err("the device is being deleted".to_owned());
        }
        let device = request.device.clone();
        // A device approved since startup (or just now, by automatic approval) has no
        // runtime yet; this creates it.
        let handle = self
            .registry
            .ensure(device.clone())
            .await
            .map_err(|error| format!("the device runtime is unavailable: {error}"))?;
        // A runtime that failed is rebuilt after its backoff; until then the device is
        // refused with the reason (§5.7 step 5), and dials again later.
        if let Some(refusal) = handle.snapshot().admission_refusal() {
            return Err(refusal);
        }
        // The row may have just been created or approved.
        self.settings
            .refresh_device(&device)
            .await
            .map_err(|error| format!("the device settings could not be read: {error}"))?;
        let hello = &request.hello;
        // `capture_permitted` says whether the controller handles wake candidates
        // (docs/protocol-v1.md §4.1). It does once the speech models are loaded; until
        // then the device keeps capturing and every candidate is refused as
        // `speech_unavailable`, and the session is renegotiated when they load.
        let (capture_permitted, capture_refusal) = match &*self.speech.borrow() {
            SpeechStatus::Ready(_) => (true, None),
            SpeechStatus::Loading => (false, Some("the speech models are loading".to_owned())),
            SpeechStatus::Unavailable(reason) => {
                (false, Some(format!("speech is unavailable: {reason}")))
            }
        };
        let afe_capable = hello.capabilities.contains(Capability::AfeMetadata);
        let grant = self
            .settings
            .snapshot()
            .ready_grant(
                &device,
                self.runtime_asset.clone(),
                capture_permitted,
                afe_capable,
            )
            .map_err(|error| format!("session.ready could not be composed: {error}"))?;
        let bluetooth_proxy = self.settings.effective(&device).bluetooth.ble_proxy_enabled;
        let kiosk_token = request
            .device_row
            .kiosk_token
            .as_ref()
            .map(|token| token.expose().to_owned());
        handle
            .attach_link(LinkAttach {
                session,
                hello: request.hello,
                platform: request.platform,
                peer: request.peer.ip(),
                label: request.device_row.label,
                afe_metadata: grant.afe_metadata && afe_capable,
                bluetooth_proxy,
                capture_permitted,
                capture_refusal,
                kiosk_token,
            })
            .await
            .map_err(|error| format!("the device runtime did not attach the session: {error}"))?;
        Ok(AdmissionDecision::Ready(grant))
    }
}

impl Admission for ControllerAdmission {
    async fn admit(&self, request: AdmissionRequest, session: LinkSession) -> AdmissionDecision {
        let device = request.device.clone();
        match self.decide(request, session).await {
            Ok(decision) => decision,
            Err(reason) => {
                tracing::warn!(%device, %reason, "device session declined");
                AdmissionDecision::Decline(reason)
            }
        }
    }
}

/// Serves the asset socket: the runtime library, the wake-model files of the registry,
/// and alert sounds (`sounds/assets/<sha256>.wav` beside the database). The link checks
/// that the bytes hash to the name before serving any of them.
pub struct ControllerAssets {
    runtime: RuntimeAsset,
    settings: SettingsHandle,
    sound_assets: PathBuf,
}

impl ControllerAssets {
    pub fn new(runtime: RuntimeAsset, settings: SettingsHandle, sound_assets: PathBuf) -> Self {
        Self {
            runtime,
            settings,
            sound_assets,
        }
    }

    /// Where the asset named `sha256` would be stored.
    fn path(&self, sha256: &Sha256) -> PathBuf {
        if *sha256 == self.runtime.sha256 {
            return self.runtime.path.clone();
        }
        if let Some(path) = self.settings.snapshot().registry().asset_path(sha256) {
            return path;
        }
        self.sound_assets.join(format!("{}.wav", sha256.as_str()))
    }
}

impl AssetSource for ControllerAssets {
    async fn load(&self, sha256: Sha256) -> io::Result<Option<Bytes>> {
        let path = self.path(&sha256);
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(Some(Bytes::from(bytes))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}
