//! The alert sound catalog on the dashboard (spec §3.5.2, §4.9; configuration.md
//! "Sounds"): listing, uploads and deletion through presentation's sound service, and
//! previews on a device through its runtime's `preview_sound`.
//!
//! `GET /api/sounds` answers `{sounds: [{id, seconds, shortened}], default_id,
//! unresolved: [{key, scope}]}`: `unresolved` names each `timerSound` or `alarmSound`
//! choice, fleet-wide (`global`) or of a device that overrides the timers section, that
//! the catalog cannot ring, so the device rings its built-in sound instead. A sound a
//! setting selects cannot be deleted.

use std::sync::Arc;

use echomuse_api::{BoxFuture, ServiceError, SoundControl};
use echomuse_core::types::DeviceId;
use echomuse_presentation::sounds::{
    DEFAULT_SOUND_ID, SoundId, UploadFormat,
    catalog::{CatalogSnapshot, DeleteError, SoundSummary, UploadError},
    service::{ServiceError as SoundServiceError, SoundHandle},
};
use echomuse_protocol::alerts::AlertSound;
use echomuse_settings::{DeviceSettings, Section, service::SettingsHandle, values::SoundChoice};
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{app::Directory, device::PreviewError};

/// [`SoundControl`] over the sound service and the device runtimes.
pub struct DashboardSounds {
    pub sounds: SoundHandle,
    /// The catalog's thread, which stops when the last holder drops it.
    pub service: Arc<echomuse_presentation::sounds::service::SoundService>,
    pub settings: SettingsHandle,
    pub directory: watch::Receiver<Directory>,
}

fn summary(sound: &SoundSummary) -> Value {
    json!({
        "id": sound.id.as_str(),
        "seconds": sound.seconds(),
        "shortened": sound.shortened(),
    })
}

/// Every stored `timerSound` and `alarmSound` choice, with the scope that holds it.
fn choices(settings: &SettingsHandle) -> Vec<(&'static str, String, SoundChoice)> {
    let snapshot = settings.snapshot();
    let of = |scope: String, settings: &Arc<DeviceSettings>| {
        [
            (
                "timerSound",
                scope.clone(),
                settings.timers.timer_sound.0.clone(),
            ),
            ("alarmSound", scope, settings.timers.alarm_sound.0.clone()),
        ]
    };
    let mut all: Vec<_> = of("global".to_owned(), snapshot.fleet())
        .into_iter()
        .collect();
    for (device, entry) in snapshot.devices() {
        if entry.overridden().contains(&Section::Timers) {
            all.extend(of(device.0.clone(), entry.settings()));
        }
    }
    all
}

fn catalog_json(catalog: &CatalogSnapshot, settings: &SettingsHandle) -> Value {
    let unresolved: Vec<Value> = choices(settings)
        .into_iter()
        .filter(|(_, _, choice)| catalog.resolve(choice).unresolved)
        .map(|(key, scope, _)| json!({ "key": key, "scope": scope }))
        .collect();
    json!({
        "sounds": catalog.sounds().map(summary).collect::<Vec<_>>(),
        "default_id": DEFAULT_SOUND_ID,
        "unresolved": unresolved,
    })
}

fn service_error<E: std::fmt::Display>(error: SoundServiceError<E>) -> ServiceError {
    match error {
        SoundServiceError::Busy => {
            ServiceError::Unavailable("The sound service is busy; try again".to_owned())
        }
        SoundServiceError::Stopped => {
            ServiceError::Unavailable("The sound service has stopped".to_owned())
        }
        SoundServiceError::Refused(error) => ServiceError::Failed(error.to_string()),
    }
}

fn preview_error(error: PreviewError) -> ServiceError {
    match error {
        PreviewError::NotConnected => ServiceError::NotConnected,
        PreviewError::Unsupported => {
            ServiceError::Unsupported("The device has no local playback".to_owned())
        }
        PreviewError::Rejected(code) => ServiceError::Upstream(format!(
            "The device refused the preview ({})",
            code.as_deref().unwrap_or("no reason given")
        )),
        PreviewError::Unknown => {
            ServiceError::Unknown("The device did not say whether the preview started".to_owned())
        }
        PreviewError::Runtime(error) => ServiceError::Unavailable(error.to_string()),
    }
}

impl DashboardSounds {
    fn handle(&self, device: &DeviceId) -> Result<crate::device::DeviceHandle, ServiceError> {
        self.directory
            .borrow()
            .get(device)
            .cloned()
            .ok_or(ServiceError::UnknownDevice)
    }

    /// What the dashboard's sound id plays: an empty id is the "Default" choice.
    fn preview_sound(&self, id: &str) -> Result<AlertSound, ServiceError> {
        let catalog = self.sounds.snapshot();
        if id.is_empty() {
            return Ok(catalog.resolve(&SoundChoice::Default).sound);
        }
        let sound_id = SoundId::new(id)
            .ok_or_else(|| ServiceError::NotFound("No sound has this id".to_owned()))?;
        let summary = catalog
            .get(&sound_id)
            .ok_or_else(|| ServiceError::NotFound("No sound has this id".to_owned()))?;
        let sha = summary.sha256().ok_or_else(|| {
            ServiceError::Unsupported(
                "This sound could not be turned into an alert sound".to_owned(),
            )
        })?;
        Ok(AlertSound::Asset(sha.clone()))
    }

    async fn upload_now(
        &self,
        id: String,
        file_name: String,
        bytes: Vec<u8>,
    ) -> Result<Value, ServiceError> {
        let id = SoundId::new(&id).ok_or_else(|| {
            ServiceError::Invalid(
                "A sound id is letters, digits, '_', '.' and '-', at most 64".to_owned(),
            )
        })?;
        let format = UploadFormat::from_file_name(&file_name).ok_or_else(|| {
            ServiceError::Invalid("Sounds may be .mp3, .wav, .flac, .ogg or .m4a".to_owned())
        })?;
        match self.sounds.upload(id, format, bytes).await {
            Ok(sound) => Ok(summary(&sound)),
            Err(SoundServiceError::Refused(UploadError::Size(n))) => {
                Err(ServiceError::Invalid(UploadError::Size(n).to_string()))
            }
            Err(SoundServiceError::Refused(UploadError::Export(error))) => {
                Err(ServiceError::Invalid(error.to_string()))
            }
            Err(error) => Err(service_error(error)),
        }
    }

    async fn delete_now(&self, id: String) -> Result<(), ServiceError> {
        let id = SoundId::new(&id)
            .ok_or_else(|| ServiceError::NotFound("No sound has this id".to_owned()))?;
        let selected = choices(&self.settings)
            .into_iter()
            .map(|(_, _, choice)| choice)
            .collect();
        match self.sounds.delete(id, selected).await {
            Ok(()) => Ok(()),
            Err(SoundServiceError::Refused(DeleteError::NotFound)) => {
                Err(ServiceError::NotFound("No sound has this id".to_owned()))
            }
            Err(SoundServiceError::Refused(DeleteError::InUse)) => {
                Err(ServiceError::InUse(DeleteError::InUse.to_string()))
            }
            Err(error) => Err(service_error(error)),
        }
    }
}

impl SoundControl for DashboardSounds {
    fn catalog(&self) -> BoxFuture<'_, Result<Value, ServiceError>> {
        let catalog = catalog_json(&self.sounds.snapshot(), &self.settings);
        Box::pin(async move { Ok(catalog) })
    }

    fn upload(
        &self,
        id: String,
        file_name: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'_, Result<Value, ServiceError>> {
        Box::pin(self.upload_now(id, file_name, bytes))
    }

    fn delete(&self, id: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(self.delete_now(id))
    }

    fn preview(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let sound = self.preview_sound(&id)?;
            let handle = self.handle(&device)?;
            handle.preview_sound(sound).await.map_err(preview_error)
        })
    }

    fn stop_preview(&self, device: DeviceId) -> BoxFuture<'_, Result<bool, ServiceError>> {
        Box::pin(async move {
            let handle = self.handle(&device)?;
            handle.stop_sound_preview().await.map_err(preview_error)
        })
    }
}
