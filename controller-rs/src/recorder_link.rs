//! The diagnostic session recorder in the running controller (spec §6.2; the recorder
//! crate's documentation holds its consent, privacy and file rules).
//!
//! - **Consent.** An administrator grants one device a bounded interval (at most 8 hours)
//!   and names what may be kept, with `POST /api/devices/{id}/session_recorder {minutes,
//!   streams?}`; `DELETE` withdraws it, which discards everything not yet written;
//!   `GET` shows the grant and the device's recordings. There is no dashboard control
//!   yet. Without a live grant nothing is recorded.
//! - **What is recorded here.** The device runtime's ingress tap: every accepted EMA1
//!   uplink frame with its ingress ordinal, `stream.open` and `stream.end`, privacy
//!   changes and the session's end. A session already attached when a grant is given
//!   starts recording at its next ingress item, with the streams already open recorded
//!   as opened then. Wake, turn, echo and Home Assistant decision evidence is not fed
//!   yet, so a recording here cannot prove decision-replay equivalence.
//! - **Privacy.** While the device is muted or capture is not permitted no frame is
//!   kept, and a mute discards what is not yet written (the recorder enforces both).
//!   Shell traffic and HTTP bodies are never offered to it.
//! - **Files** are private (`0600` in `0700` directories) under
//!   `<data dir>/session_recordings/`, carry a 7-day retention and are swept hourly;
//!   deleting a device deletes them.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use echomuse_api::{BoxFuture, RecorderGrant, ServiceError, SessionRecorders};
use echomuse_core::types::{DeviceId, RuntimeId, SessionId};
use echomuse_link::{Ingress, IngressOrdinal as LinkOrdinal};
use echomuse_protocol::{DeviceMessage, LinkSessionId, StreamId, ema};
use echomuse_recorder::{
    AdministratorId, DisconnectReason, GrantBook, GrantError, GrantRequest, IngressOrdinal,
    PrivacyState, RecorderConfig, RecorderState, RecordingId, RecordingStore, Revisions,
    SessionIdentity, SessionRecorder, StreamScope,
};
use echomuse_turn::{Epoch, MonoInstant};
use serde_json::{Value, json};

use crate::supervision::TaskSupervisor;

/// How often expired grants end and old recordings are swept.
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// The grant book and the recordings directory. Cheap to share.
pub struct RecorderHub {
    book: GrantBook,
    store: RecordingStore,
    origin: Instant,
    controller_version: String,
}

impl RecorderHub {
    /// Opens (creating) the recordings directory and starts the hourly sweep.
    pub fn open(
        directory: PathBuf,
        controller_version: String,
        tasks: &TaskSupervisor,
    ) -> std::io::Result<Arc<Self>> {
        let store = RecordingStore::open(RecorderConfig::new(directory))?;
        let hub = Arc::new(Self {
            book: GrantBook::new(),
            store,
            origin: Instant::now(),
            controller_version,
        });
        let sweeper = Arc::downgrade(&hub);
        tasks.spawn("session recorder sweep", async move {
            let mut every = tokio::time::interval(SWEEP_EVERY);
            loop {
                every.tick().await;
                let Some(hub) = sweeper.upgrade() else {
                    return;
                };
                hub.book.expire(hub.now());
                let store = hub.store.clone();
                match tokio::task::spawn_blocking(move || store.sweep(SystemTime::now())).await {
                    Ok(Ok(removed)) if !removed.is_empty() => {
                        tracing::info!(count = removed.len(), "expired session recordings removed");
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(%error, "the session recordings sweep failed"),
                    Err(error) => std::panic::resume_unwind(error.into_panic()),
                }
            }
        });
        Ok(hub)
    }

    /// The controller's monotonic time on the recorder's clock.
    pub fn now(&self) -> MonoInstant {
        MonoInstant(u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    /// A tap for one device link session.
    pub fn tap(
        self: &Arc<Self>,
        identity: SessionIdentity,
        firmware: Option<String>,
        privacy: PrivacyState,
    ) -> RecorderTap {
        RecorderTap {
            hub: Arc::clone(self),
            identity,
            revisions: Revisions {
                controller: self.controller_version.clone(),
                firmware,
                device_config: None,
                models: Vec::new(),
            },
            privacy,
            open: BTreeMap::new(),
            recorder: None,
        }
    }

    /// Deletes what is kept for a deleted device: its grant is withdrawn and its
    /// recordings deleted.
    pub async fn delete_device(&self, device: &DeviceId) -> std::io::Result<()> {
        self.book.forget(device, self.now());
        let store = self.store.clone();
        let device = device.clone();
        match tokio::task::spawn_blocking(move || store.delete_device(&device)).await {
            Ok(result) => result.map(drop),
            Err(error) => std::panic::resume_unwind(error.into_panic()),
        }
    }

    fn recordings(&self, device: &DeviceId) -> Result<Vec<Value>, ServiceError> {
        let listed = self
            .store
            .list()
            .map_err(|error| ServiceError::Failed(format!("listing recordings: {error}")))?;
        let mut out: Vec<Value> = listed
            .into_iter()
            .filter_map(|stored| {
                let header = stored.header?;
                (header.session.device == device.0).then(|| {
                    json!({
                        "id": header.recording.as_str(),
                        "finished": stored.finished,
                        "writing": stored.writing,
                        "bytes": stored.bytes,
                        "header": header,
                    })
                })
            })
            .collect();
        out.sort_by(|a, b| {
            b["header"]["anchor"]
                .as_u64()
                .cmp(&a["header"]["anchor"].as_u64())
        });
        Ok(out)
    }

    fn path_of(&self, device: &DeviceId, id: &str) -> Result<PathBuf, ServiceError> {
        let missing = || ServiceError::NotFound("No such recording".to_owned());
        let id = RecordingId::parse(id).ok_or_else(missing)?;
        let listed = self
            .store
            .list()
            .map_err(|error| ServiceError::Failed(format!("listing recordings: {error}")))?;
        listed
            .into_iter()
            .find(|stored| {
                stored
                    .header
                    .as_ref()
                    .is_some_and(|h| h.recording == id && h.session.device == device.0)
            })
            .map(|stored| stored.path)
            .ok_or_else(missing)
    }
}

fn grant_error(error: GrantError) -> ServiceError {
    match error {
        GrantError::EmptyInterval | GrantError::TooLong { .. } => {
            ServiceError::Invalid(error.to_string())
        }
        GrantError::AlreadyActive { .. } => {
            ServiceError::Busy("The device already has a live recording grant".to_owned())
        }
        GrantError::NotActive => {
            ServiceError::NotFound("The device has no live recording grant".to_owned())
        }
    }
}

fn administrator(name: String) -> Result<AdministratorId, ServiceError> {
    AdministratorId::new(name).ok_or_else(|| ServiceError::Invalid("No administrator".to_owned()))
}

/// [`SessionRecorders`] over the hub.
pub struct RecorderRoutes(pub Arc<RecorderHub>);

impl SessionRecorders for RecorderRoutes {
    fn status(&self, device: DeviceId) -> BoxFuture<'_, Result<Value, ServiceError>> {
        Box::pin(async move {
            let status = self.0.book.status(&device, self.0.now());
            let hub = Arc::clone(&self.0);
            let recordings = tokio::task::spawn_blocking(move || hub.recordings(&device))
                .await
                .map_err(|_| ServiceError::Failed("listing recordings failed".to_owned()))??;
            Ok(json!({ "status": status, "recordings": recordings }))
        })
    }

    fn grant(
        &self,
        device: DeviceId,
        grant: RecorderGrant,
    ) -> BoxFuture<'_, Result<Value, ServiceError>> {
        Box::pin(async move {
            let request = GrantRequest {
                administrator: administrator(grant.administrator)?,
                interval: Duration::from_secs(grant.minutes.saturating_mul(60)),
                streams: StreamScope {
                    mic: grant.scope.mic,
                    reference: grant.scope.reference,
                    cells: grant.scope.cells,
                    afe: grant.scope.afe,
                    transcripts: grant.scope.transcripts,
                },
            };
            let status = self
                .0
                .book
                .grant(&device, request, self.0.now(), SystemTime::now())
                .map_err(grant_error)?;
            tracing::info!(%device, "session recording granted");
            Ok(json!(status))
        })
    }

    fn withdraw(&self, device: DeviceId, by: String) -> BoxFuture<'_, Result<Value, ServiceError>> {
        Box::pin(async move {
            let status = self
                .0
                .book
                .withdraw(&device, administrator(by)?, self.0.now(), SystemTime::now())
                .map_err(grant_error)?;
            tracing::info!(%device, "session recording withdrawn");
            Ok(json!(status))
        })
    }

    fn file(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<PathBuf, ServiceError>> {
        Box::pin(async move {
            let hub = Arc::clone(&self.0);
            tokio::task::spawn_blocking(move || hub.path_of(&device, &id))
                .await
                .map_err(|_| ServiceError::Failed("finding the recording failed".to_owned()))?
        })
    }

    fn delete(&self, device: DeviceId, id: String) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let hub = Arc::clone(&self.0);
            tokio::task::spawn_blocking(move || {
                hub.path_of(&device, &id)?;
                let id = RecordingId::parse(&id)
                    .ok_or_else(|| ServiceError::NotFound("No such recording".to_owned()))?;
                hub.store
                    .delete(&id)
                    .map_err(|error| ServiceError::Failed(format!("deleting: {error}")))?
                    .then_some(())
                    .ok_or_else(|| ServiceError::NotFound("No such recording".to_owned()))
            })
            .await
            .map_err(|_| ServiceError::Failed("deleting the recording failed".to_owned()))?
        })
    }
}

/// The session recorder's view of one device link session, held by its runtime.
pub struct RecorderTap {
    hub: Arc<RecorderHub>,
    identity: SessionIdentity,
    revisions: Revisions,
    privacy: PrivacyState,
    /// Streams open now, with their epochs.
    open: BTreeMap<StreamId, Epoch>,
    recorder: Option<SessionRecorder>,
}

impl RecorderTap {
    /// Starts recording when a live grant allows and none runs; ends a recording whose
    /// grant ended.
    fn ensure(&mut self, ordinal: IngressOrdinal) {
        let now = self.hub.now();
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.poll(now);
            if matches!(recorder.state(), RecorderState::Recording { .. }) {
                return;
            }
            self.recorder = None;
        }
        let Some(pass) = self.hub.book.pass(&self.identity.device, now) else {
            return;
        };
        match SessionRecorder::start(
            pass,
            &self.hub.store,
            self.identity.clone(),
            self.revisions.clone(),
            self.privacy,
            now,
            SystemTime::now(),
        ) {
            Ok(mut recorder) => {
                for (stream, epoch) in &self.open {
                    recorder.stream_opened(ordinal, *stream, *epoch, now);
                }
                tracing::info!(device = %self.identity.device, recording = recorder.id().as_str(), "session recording started");
                self.recorder = Some(recorder);
            }
            Err(error) => {
                tracing::warn!(device = %self.identity.device, %error, "session recording did not start");
            }
        }
    }

    /// One ordered ingress item, after the runtime accepted it.
    pub fn ingress(&mut self, ordinal: LinkOrdinal, ingress: &Ingress) {
        let ordinal = IngressOrdinal(ordinal.0);
        match ingress {
            Ingress::Control(envelope) => match &envelope.message {
                DeviceMessage::StreamOpen(open) => {
                    let Some(epoch) = Epoch::new(open.epoch.get()) else {
                        return;
                    };
                    self.open.insert(open.stream_id, epoch);
                    self.ensure(ordinal);
                    if let Some(recorder) = self.recorder.as_mut() {
                        recorder.stream_opened(ordinal, open.stream_id, epoch, self.hub.now());
                    }
                }
                DeviceMessage::StreamEnd(end) => {
                    let Some(epoch) = Epoch::new(end.epoch) else {
                        return;
                    };
                    if self.open.get(&end.stream_id) == Some(&epoch) {
                        self.open.remove(&end.stream_id);
                    }
                    self.ensure(ordinal);
                    if let Some(recorder) = self.recorder.as_mut() {
                        recorder.stream_ended(ordinal, end.stream_id, epoch, self.hub.now());
                    }
                }
                _ => {}
            },
            Ingress::Audio(packet) => {
                self.ensure(ordinal);
                let Some(recorder) = self.recorder.as_mut() else {
                    return;
                };
                // The frame exactly as received: its header re-encoded around the
                // payload slice the packet kept.
                if let Ok(frame) = ema::encode(packet.header(), packet.payload()) {
                    recorder.frame(ordinal, &frame, self.hub.now());
                }
            }
        }
    }

    /// The device's mute or the runtime's capture permission changed.
    pub fn privacy(&mut self, muted: bool, capture_permitted: bool) {
        self.privacy = PrivacyState {
            muted,
            capture_permitted,
        };
        if let Some(recorder) = self.recorder.as_mut() {
            recorder.privacy(None, self.privacy, self.hub.now());
        }
    }

    /// The session ended.
    pub fn ended(mut self, reason: DisconnectReason) {
        if let Some(recorder) = self.recorder.take() {
            let _finishing = recorder.disconnected(reason, self.hub.now());
        }
    }

    /// Whether a recording is being written for this session.
    pub fn recording(&self) -> bool {
        self.recorder.is_some()
    }

    /// The voice machine's decision evidence, in the order it was queued, behind the
    /// ingress that led to it. `origin` is where the machine's clock starts; its times
    /// are moved onto the recorder's clock, so the recorded intervals are the ones the
    /// machine's deciders and policies ran with.
    pub fn evidence(
        &mut self,
        origin: tokio::time::Instant,
        items: Vec<(MonoInstant, echomuse_voice::evidence::Evidence)>,
    ) {
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        let base = origin.into_std();
        let hub = self.hub.origin;
        let clock = |at: MonoInstant| {
            let instant = base + Duration::from_nanos(at.0);
            MonoInstant(
                u64::try_from(instant.saturating_duration_since(hub).as_nanos())
                    .unwrap_or(u64::MAX),
            )
        };
        for (at, evidence) in items {
            evidence.record(recorder, at, clock);
        }
    }
}

/// The identity a tap records under.
pub fn identity(
    device: DeviceId,
    runtime: RuntimeId,
    session: SessionId,
    link_session: LinkSessionId,
) -> SessionIdentity {
    SessionIdentity {
        device,
        runtime,
        session,
        link_session,
    }
}
