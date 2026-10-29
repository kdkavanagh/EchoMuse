//! Device services for the API tests: every request is refused as unavailable, since
//! these tests exercise other routes. The device-service routes are tested end to end
//! with simulated devices in the controller's own tests.

use std::{path::PathBuf, sync::Arc};

use echomuse_api::{
    AlarmCancel, AlarmReply, AlarmRequest, AlertControl, AmbientListing, ArchiveFile, BoxFuture,
    CaptureArm, CaptureControl, CaptureError, CaptureRecording, CaptureState, CaptureTag, ClipName,
    ClipPart, DeviceServices, KioskLogin, RecorderGrant, RecordingMode, RecordingModes,
    RecordingRequest, SamplesListing, ServiceError, SessionRecorders, ShellAccess, ShellChannels,
    SoundControl, WifiControl, WifiCredentials, WifiNetwork, WindowId, WindowMs, WindowView,
};
use echomuse_core::types::DeviceId;
use serde_json::{Map, Value};

/// Refuses every device-service request.
pub struct Refusing;

fn refused<T: Send + 'static>() -> BoxFuture<'static, Result<T, ServiceError>> {
    Box::pin(async {
        Err(ServiceError::Unavailable(
            "device services are not part of this test".to_owned(),
        ))
    })
}

impl WifiControl for Refusing {
    fn scan(&self, _: DeviceId) -> BoxFuture<'_, Result<Vec<WifiNetwork>, ServiceError>> {
        refused()
    }
    fn change(&self, _: DeviceId, _: WifiCredentials) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
    fn fields(&self, _: &DeviceId) -> Map<String, Value> {
        Map::new()
    }
}

impl RecordingModes for Refusing {
    fn set_mode(
        &self,
        _: DeviceId,
        _: RecordingMode,
        _: bool,
    ) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
    fn samples(&self, _: DeviceId) -> BoxFuture<'_, Result<SamplesListing, ServiceError>> {
        refused()
    }
    fn ambient(&self, _: DeviceId) -> BoxFuture<'_, Result<AmbientListing, ServiceError>> {
        refused()
    }
    fn file(
        &self,
        _: DeviceId,
        _: RecordingMode,
        _: ClipName,
        _: ClipPart,
    ) -> BoxFuture<'_, Result<PathBuf, ServiceError>> {
        refused()
    }
    fn delete(
        &self,
        _: DeviceId,
        _: RecordingMode,
        _: Option<ClipName>,
    ) -> BoxFuture<'_, Result<usize, ServiceError>> {
        refused()
    }
    fn archive_files(
        &self,
        _: DeviceId,
        _: RecordingMode,
    ) -> BoxFuture<'_, Result<Vec<ArchiveFile>, ServiceError>> {
        refused()
    }
    fn fields(&self, _: &DeviceId) -> Map<String, Value> {
        Map::new()
    }
}

impl SessionRecorders for Refusing {
    fn status(&self, _: DeviceId) -> BoxFuture<'_, Result<Value, ServiceError>> {
        refused()
    }
    fn grant(&self, _: DeviceId, _: RecorderGrant) -> BoxFuture<'_, Result<Value, ServiceError>> {
        refused()
    }
    fn withdraw(&self, _: DeviceId, _: String) -> BoxFuture<'_, Result<Value, ServiceError>> {
        refused()
    }
    fn file(&self, _: DeviceId, _: String) -> BoxFuture<'_, Result<PathBuf, ServiceError>> {
        refused()
    }
    fn delete(&self, _: DeviceId, _: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
}

impl ShellAccess for Refusing {
    fn open(&self, _: DeviceId) -> BoxFuture<'_, Result<ShellChannels, ServiceError>> {
        refused()
    }
}

impl AlertControl for Refusing {
    fn status(&self, _: DeviceId) -> BoxFuture<'_, Result<Option<Value>, ServiceError>> {
        refused()
    }
    fn set_alarm(
        &self,
        _: DeviceId,
        _: AlarmRequest,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>> {
        refused()
    }
    fn cancel_alarm(
        &self,
        _: DeviceId,
        _: AlarmCancel,
    ) -> BoxFuture<'_, Result<AlarmReply, ServiceError>> {
        refused()
    }
}

impl SoundControl for Refusing {
    fn catalog(&self) -> BoxFuture<'_, Result<Value, ServiceError>> {
        refused()
    }
    fn upload(
        &self,
        _: String,
        _: String,
        _: Vec<u8>,
    ) -> BoxFuture<'_, Result<Value, ServiceError>> {
        refused()
    }
    fn delete(&self, _: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
    fn preview(&self, _: DeviceId, _: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
    fn stop_preview(&self, _: DeviceId) -> BoxFuture<'_, Result<bool, ServiceError>> {
        refused()
    }
}

impl KioskLogin for Refusing {
    fn login(&self, _: DeviceId, _: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
}

fn capture_refused() -> CaptureError {
    CaptureError::Unavailable("capture is not part of this test".into())
}

impl CaptureControl for Refusing {
    fn arm(&self, _: &DeviceId, _: CaptureArm) -> Result<CaptureState, CaptureError> {
        Err(capture_refused())
    }
    fn disarm(&self, _: &DeviceId) -> Result<CaptureState, CaptureError> {
        Err(capture_refused())
    }
    fn state(&self, _: &DeviceId) -> Result<CaptureState, CaptureError> {
        Err(capture_refused())
    }
    fn open_window(
        &self,
        _: &DeviceId,
        _: CaptureTag,
        _: WindowMs,
    ) -> Result<WindowView, CaptureError> {
        Err(capture_refused())
    }
    fn stop_window(&self, _: &DeviceId, _: &WindowId) -> Result<WindowView, CaptureError> {
        Err(capture_refused())
    }
    fn recording(
        &self,
        _: DeviceId,
        _: RecordingRequest,
    ) -> BoxFuture<'_, Result<CaptureRecording, CaptureError>> {
        Box::pin(async { Err(capture_refused()) })
    }
    fn fields(&self, _: &DeviceId) -> Map<String, Value> {
        Map::new()
    }
}

/// Capture that refuses everything.
pub fn refusing_capture() -> Arc<dyn CaptureControl> {
    Arc::new(Refusing)
}

/// Device services that refuse everything.
pub fn refusing() -> DeviceServices {
    let all = Arc::new(Refusing);
    DeviceServices {
        wifi: all.clone(),
        recording: all.clone(),
        recorder: all.clone(),
        shell: all.clone(),
        alerts: all.clone(),
        sounds: all.clone(),
        kiosk: all,
    }
}

/// Refuses transcript correction as not ready and cannot score clips.
pub struct NoDiagnostics;

impl echomuse_api::TextFixService for NoDiagnostics {
    fn trial(
        &self,
        _: echomuse_api::TextFixTrialRequest,
    ) -> BoxFuture<'_, Result<echomuse_api::TextFixTrial, echomuse_api::TextFixError>> {
        Box::pin(async {
            Err(echomuse_api::TextFixError::NotReady(
                "correction is not part of this test".to_owned(),
            ))
        })
    }

    fn unknown_words(
        &self,
        _: Option<DeviceId>,
    ) -> BoxFuture<'_, Result<Vec<echomuse_api::UnknownWord>, echomuse_api::TextFixError>> {
        Box::pin(async {
            Err(echomuse_api::TextFixError::NotReady(
                "correction is not part of this test".to_owned(),
            ))
        })
    }
}

impl echomuse_api::ClipScoring for NoDiagnostics {
    fn pending(&self) -> Option<u64> {
        None
    }

    fn score_now(&self, _: Vec<echomuse_api::ClipRef>) -> BoxFuture<'_, Result<(), ServiceError>> {
        refused()
    }
}

/// Diagnostics that refuse everything, with no runtime asset known.
pub fn refusing_diagnostics() -> echomuse_api::Diagnostics {
    let none = Arc::new(NoDiagnostics);
    echomuse_api::Diagnostics {
        text_fix: none.clone(),
        clip_scoring: none,
        runtime_asset: None,
    }
}
