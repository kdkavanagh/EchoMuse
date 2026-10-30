//! [`RecordsService`]: the one way the runtime writes durable records, so publication,
//! retention and device deletion are coordinated (spec §3.5.2, §5.7 steps 4 and 6).
//!
//! # Coordination
//!
//! Each device has a gate. Every write for a device (a row, a file, a mark) runs while
//! holding the gate shared; retention and deletion hold it alone. So retention never sees a
//! half-published recording, and once [`RecordsService::delete_device`] has closed the gate
//! no late writer of that device can add a row or a file: its write fails with
//! [`RecordsError::Closed`] until [`RecordsService::reopen_device`]. A deletion also ends
//! the device's records lifetime: a turn or continuation submitted before it is refused
//! even after the reopen. A mark that finds its row gone removes the file it just wrote.
//!
//! # Publishing a recording
//!
//! 1. Commit the row (unsaved: a NULL file column, or a near miss's `failed`) and take its id.
//! 2. On a blocking thread, write the WAV under a temporary name, sync it, rename it into
//!    place and sync the directory.
//! 3. Mark the row saved.
//! 4. Apply that kind's retention for the device.
//!
//! # Startup ([`RecordsService::start_up`])
//!
//! Run before any device runtime starts. For every recordings directory:
//!
//! - a temporary file is an interrupted write and is removed;
//! - a recording whose row is saved is kept;
//! - a complete recording whose row is not marked saved is adopted (the controller stopped
//!   between the rename and the mark), a cut-short one removed;
//! - a recording without a row of its device, or whose row says it was never recorded, is an
//!   orphan and removed;
//! - a name this crate does not write is left alone and reported.
//!
//! A row whose file never arrived simply stays unsaved, and `pending` continuations become
//! `session_lost`. Then every device's retention runs once.
//!
//! # Telling the dashboard
//!
//! [`RecordsService::subscribe_turns`] hears of each new `turns` row once its recordings are
//! published, so the dashboard's Activity lists can re-read it.
//!
//! # Writes that outlive their session
//!
//! [`RecordsService::submit_turn`] hands a turn's row, its recordings (the turn recording
//! included, which is named by the row's id) and, optionally, the transcript-correction
//! record still being computed for it to the service, which writes them on its own tasks
//! (on the runtime the service was made on): the device session or runtime that asked can
//! end at once without losing any of it. A row waiting for its correction record is held
//! until the record arrives (its sender bounds the wait); a reply's continuation for a held
//! row goes into that row, and one arriving while it is written is applied right after.
//! [`RecordsService::shutdown`] first drains these writes: it waits a bounded time for the
//! held rows' records, then writes the rest without them.

use std::{
    collections::HashMap,
    fmt, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use echomuse_core::types::DeviceId;
use echomuse_db::{DbHandle, ReadQuery, ServiceError, WriteCommand};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, broadcast, oneshot};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use uuid::Uuid;

use crate::{
    error::RecordError,
    logs::{DEVICE_LOG_RETENTION, DeviceLogEntry, InsertDeviceLog},
    near_misses::{
        InsertNearMiss, MarkNearMissSaved, NEAR_MISS_ROW_RETENTION, NEAR_MISS_SERIES_RETENTION,
        NearMissInsert, NearMissRecord, NearMissRowId, PruneNearMisses, SetNearMissVerdict,
        SetNearMissWindows, VerdictUpdate,
    },
    recordings::{
        AdoptRecording, DeleteDeviceRecords, DeletedRows, DevicesWithRecordings, EntryName,
        GetRowState, MarkTurnFileSaved, NEAR_MISS_RECORDING_RETENTION, RecordingKind,
        RecordingsLayout, RetainedIds, RowState, TURN_RECORDING_RETENTION, UTTERANCE_RETENTION,
        WAKE_CLIP_RETENTION, file_safe, remove_if_present, samples_duration, scan, wav_bytes,
        wav_samples, write_atomically,
    },
    rollups::{RecordRtt, RecordStats, RecordWakeStats, RttSample, StatsReport, WakeStatsReport},
    turns::{
        ContinuationUpdate, InsertTurn, RecordContinuation, ResolveStaleContinuations,
        SetTurnWakeWindows, TRACE_RETENTION, TurnInsert, TurnKey, TurnRecord, TurnRowId,
    },
    values::{ClipWindows, Pcm16, SttFix, WallSeconds},
    vocab::{Continuation, ContinuationEnd, NearMissRecording, NearMissVerdict},
};

/// The retention limits, per device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionLimits {
    /// Utterance recordings.
    pub utterances: u32,
    /// Turn recordings.
    pub turn_recordings: u32,
    /// Wake clips.
    pub wake_clips: u32,
    /// Recordings of unmarked near misses.
    pub near_miss_recordings: u32,
    /// Unmarked near-miss rows.
    pub near_miss_rows: u32,
    /// Turns keeping `decision_trace` and `afe_series`.
    pub turn_traces: u32,
    /// Near misses keeping `afe_series`.
    pub near_miss_series: u32,
    /// Device log lines.
    pub device_logs: u32,
}

impl Default for RetentionLimits {
    fn default() -> Self {
        Self {
            utterances: UTTERANCE_RETENTION,
            turn_recordings: TURN_RECORDING_RETENTION,
            wake_clips: WAKE_CLIP_RETENTION,
            near_miss_recordings: NEAR_MISS_RECORDING_RETENTION,
            near_miss_rows: NEAR_MISS_ROW_RETENTION,
            turn_traces: TRACE_RETENTION,
            near_miss_series: NEAR_MISS_SERIES_RETENTION,
            device_logs: DEVICE_LOG_RETENTION,
        }
    }
}

impl RetentionLimits {
    fn files(&self, kind: RecordingKind) -> u32 {
        match kind {
            RecordingKind::Utterance => self.utterances,
            RecordingKind::TurnRecording => self.turn_recordings,
            RecordingKind::WakeClip => self.wake_clips,
            RecordingKind::NearMiss => self.near_miss_recordings,
        }
    }
}

/// Why a records operation failed.
#[derive(Debug)]
pub enum RecordsError {
    /// The database service refused or failed the command.
    Db(ServiceError<RecordError>),
    /// A file operation failed.
    Io(io::Error),
    /// The device was deleted or the service is shutting down: nothing was written.
    Closed(DeviceId),
    /// The device id cannot name files.
    UnsafeDeviceId(DeviceId),
    /// A record task ended without reporting its outcome.
    TaskFailed,
}

impl fmt::Display for RecordsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(e) => write!(f, "database: {e}"),
            Self::Io(e) => write!(f, "files: {e}"),
            Self::Closed(d) => write!(f, "records of device {d} are closed"),
            Self::UnsafeDeviceId(d) => write!(f, "device id {d:?} cannot name files"),
            Self::TaskFailed => f.write_str("a record task ended without reporting its outcome"),
        }
    }
}

impl std::error::Error for RecordsError {}

impl From<ServiceError<RecordError>> for RecordsError {
    fn from(error: ServiceError<RecordError>) -> Self {
        Self::Db(error)
    }
}

impl From<io::Error> for RecordsError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// What became of one recording's publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Publication {
    /// The file exists and its row says so.
    Saved,
    /// Writing the file failed; the row stays unsaved.
    Failed(String),
    /// The row was deleted while the file was written; the file was removed again.
    RowGone,
}

/// The audio a turn keeps, each only when its setting is on.
#[derive(Clone, Debug, Default)]
pub struct TurnAudio {
    /// What STT received (`saveUtterances`).
    pub utterance: Option<Pcm16>,
    /// The wake word (`saveWakeClips`).
    pub wake_clip: Option<Pcm16>,
    /// The turn recording placed on the AFE chart (`turn_recordings/<device>_<row>.wav`),
    /// published once the row has its id.
    pub turn_recording: Option<Pcm16>,
}

/// The result of [`RecordsService::persist_turn`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedTurn {
    /// The row, and whether this call inserted it.
    pub insert: TurnInsert,
    /// The utterance recording, when one was given and the row is new.
    pub utterance: Option<Publication>,
    /// The wake clip, when one was given and the row is new.
    pub wake_clip: Option<Publication>,
    /// The turn recording, when one was given and the row is new.
    pub turn_recording: Option<Publication>,
}

/// The transcript-correction record (`turns.stt_fix`) a submitted turn's row waits for.
/// Its sender is dropped, or sends `None`, when there is no record: the row is then
/// written without one.
pub type FixFollows = oneshot::Receiver<Option<SttFix>>;

/// How long [`RecordsService::shutdown`] waits for held rows' correction records before
/// writing them without, and then for the writes themselves.
pub const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// A near miss's audio, or why there is none.
#[derive(Clone, Debug)]
pub enum NearMissAudio {
    /// The recording to save.
    Recorded(Pcm16),
    /// Not recorded: `off`, `not_permitted` or `failed`.
    NotRecorded(NearMissRecording),
}

/// The result of [`RecordsService::persist_near_miss`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedNearMiss {
    /// The row, whether this call inserted it, and the rows retention deleted.
    pub insert: NearMissInsert,
    /// The recording, when one was given and the row is new.
    pub recording: Option<Publication>,
}

/// What [`RecordsService::start_up`] found and did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReconcileReport {
    /// Interrupted writes removed.
    pub temporary_removed: Vec<PathBuf>,
    /// Recordings without a row (or whose row says they were never recorded) removed.
    pub orphans_removed: Vec<PathBuf>,
    /// Complete recordings whose rows were marked saved.
    pub adopted: Vec<PathBuf>,
    /// Cut-short recordings of unsaved rows removed.
    pub incomplete_removed: Vec<PathBuf>,
    /// Files in the recordings directories this crate does not name, left alone.
    pub unrecognized: Vec<PathBuf>,
    /// `pending` continuations resolved to `session_lost`.
    pub stale_continuations: usize,
    /// Files removed by the startup retention pass.
    pub retention_removed: usize,
}

/// What [`RecordsService::delete_device`] removed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeletedRecords {
    /// The rows.
    pub rows: DeletedRows,
    /// The recording files.
    pub files: usize,
}

/// A new `turns` row (a turn or a refused wake candidate), committed with the recordings it
/// was given.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnStored {
    /// The turn's device.
    pub device: DeviceId,
    /// The row.
    pub id: TurnRowId,
}

/// New-turn notices kept for a subscriber that has not looked yet. One that falls further
/// behind misses some; the dashboard also re-reads its lists every 10 s.
const TURN_NOTICES: usize = 64;

type Gate = Arc<RwLock<bool>>;

/// One lifetime of a device's records: a deletion ends it, and the reopen that follows
/// starts the next. A write captures the lifetime it was submitted (or queued) in and is
/// refused in any later one, so nothing from before a deletion lands in the next
/// approval's records. Taken with [`RecordsService::lifetime`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordsLifetime(u64);

#[derive(Default)]
struct Gates {
    closed_all: bool,
    /// Each device's gate and current lifetime. The lifetime changes only while its gate
    /// is held alone, so a writer holding the gate shared reads it stably.
    by_device: HashMap<DeviceId, (Gate, RecordsLifetime)>,
}

struct Inner {
    db: DbHandle,
    layout: RecordingsLayout,
    limits: RetentionLimits,
    gates: Mutex<Gates>,
    turns_stored: broadcast::Sender<TurnStored>,
    /// Submitted turns whose rows are not committed yet, by device and turn.
    held: Mutex<HashMap<(DeviceId, Uuid), Held>>,
    /// The submitted writes, which [`RecordsService::shutdown`] drains.
    tasks: TaskTracker,
    /// Where submitted writes run: the runtime the service was made on.
    runtime: tokio::runtime::Handle,
    /// Cancelled when the drain stops waiting for correction records.
    flush: CancellationToken,
}

/// Continuation ends that arrived for a submitted turn before its row was committed, each
/// with whoever waits for its result.
#[derive(Default)]
struct Held {
    ends: Vec<(
        ContinuationEnd,
        oneshot::Sender<Result<ContinuationUpdate, RecordsError>>,
    )>,
    /// The row's insert was sent: ends arriving now are applied after it commits.
    writing: bool,
}

/// The records service. Cheap to clone; every clone shares the gates.
#[derive(Clone)]
pub struct RecordsService {
    inner: Arc<Inner>,
}

impl fmt::Debug for RecordsService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordsService")
            .field("layout", &self.inner.layout)
            .field("limits", &self.inner.limits)
            .finish_non_exhaustive()
    }
}

/// The failure of a turn's row, said again for a reply end folded into it; `None` when the
/// vocabulary cannot say it again (a refused command's own error). Nothing here claims the
/// row is absent: an unknown outcome stays unknown.
fn same_failure(error: &RecordsError) -> Option<RecordsError> {
    Some(match error {
        RecordsError::Closed(device) => RecordsError::Closed(device.clone()),
        RecordsError::UnsafeDeviceId(device) => RecordsError::UnsafeDeviceId(device.clone()),
        RecordsError::Db(ServiceError::Closed) => RecordsError::Db(ServiceError::Closed),
        RecordsError::Db(ServiceError::Aborted) => RecordsError::Db(ServiceError::Aborted),
        RecordsError::Db(ServiceError::Unknown) => RecordsError::Db(ServiceError::Unknown),
        RecordsError::Io(error) => {
            RecordsError::Io(io::Error::new(error.kind(), error.to_string()))
        }
        RecordsError::TaskFailed => RecordsError::TaskFailed,
        RecordsError::Db(ServiceError::Failed(_)) => return None,
    })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, RecordsError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| RecordsError::TaskFailed)?
        .map_err(RecordsError::Io)
}

impl RecordsService {
    /// A service writing through `db`, with files under `layout`. Call it on the runtime
    /// that should run submitted writes (the shared one): they must outlive any device
    /// runtime that submits them.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime.
    pub fn new(db: DbHandle, layout: RecordingsLayout, limits: RetentionLimits) -> Self {
        Self {
            inner: Arc::new(Inner {
                db,
                layout,
                limits,
                gates: Mutex::new(Gates::default()),
                turns_stored: broadcast::Sender::new(TURN_NOTICES),
                held: Mutex::new(HashMap::new()),
                tasks: TaskTracker::new(),
                runtime: tokio::runtime::Handle::current(),
                flush: CancellationToken::new(),
            }),
        }
    }

    /// The file layout.
    pub fn layout(&self) -> &RecordingsLayout {
        &self.inner.layout
    }

    /// Hears of every new `turns` row from now on, once its recordings are published.
    pub fn subscribe_turns(&self) -> broadcast::Receiver<TurnStored> {
        self.inner.turns_stored.subscribe()
    }

    /// The device's gate and its current lifetime.
    fn gate(&self, device: &DeviceId) -> (Gate, RecordsLifetime) {
        let mut gates = self.inner.gates.lock().expect("records gates lock");
        let open = !gates.closed_all;
        let (gate, lifetime) = gates
            .by_device
            .entry(device.clone())
            .or_insert_with(|| (Arc::new(RwLock::new(open)), RecordsLifetime(0)));
        (Arc::clone(gate), *lifetime)
    }

    /// The device's current records lifetime, for a write queued before it reaches the
    /// service ([`RecordsService::log`]).
    pub fn lifetime(&self, device: &DeviceId) -> RecordsLifetime {
        self.gate(device).1
    }

    /// Ends the device's current lifetime. Call it holding the gate alone.
    fn end_lifetime(&self, device: &DeviceId) {
        let mut gates = self.inner.gates.lock().expect("records gates lock");
        if let Some((_, lifetime)) = gates.by_device.get_mut(device) {
            lifetime.0 += 1;
        }
    }

    async fn enter(&self, device: &DeviceId) -> Result<OwnedRwLockReadGuard<bool>, RecordsError> {
        let lifetime = self.gate(device).1;
        self.enter_in(device, lifetime).await
    }

    /// Holds the device's gate shared for a write submitted in `lifetime`.
    async fn enter_in(
        &self,
        device: &DeviceId,
        lifetime: RecordsLifetime,
    ) -> Result<OwnedRwLockReadGuard<bool>, RecordsError> {
        let guard = self.gate(device).0.read_owned().await;
        if *guard && self.gate(device).1 == lifetime {
            Ok(guard)
        } else {
            Err(RecordsError::Closed(device.clone()))
        }
    }

    async fn exclusive(
        &self,
        device: &DeviceId,
    ) -> Result<OwnedRwLockWriteGuard<bool>, RecordsError> {
        let guard = self.gate(device).0.write_owned().await;
        if *guard {
            Ok(guard)
        } else {
            Err(RecordsError::Closed(device.clone()))
        }
    }

    async fn write<C: WriteCommand<Error = RecordError>>(
        &self,
        command: C,
    ) -> Result<C::Output, RecordsError> {
        Ok(self.inner.db.write(command).await?)
    }

    async fn read<Q: ReadQuery<Error = RecordError>>(
        &self,
        query: Q,
    ) -> Result<Q::Output, RecordsError> {
        Ok(self.inner.db.read(query).await?)
    }

    fn check_device(device: &DeviceId) -> Result<(), RecordsError> {
        if file_safe(device) {
            Ok(())
        } else {
            Err(RecordsError::UnsafeDeviceId(device.clone()))
        }
    }

    /// Writes one recording of a committed row and marks it saved. The caller holds the
    /// device's gate shared.
    async fn publish(
        &self,
        kind: RecordingKind,
        device: &DeviceId,
        id: i64,
        pcm: Pcm16,
    ) -> Result<Publication, RecordsError> {
        let dir = self.inner.layout.directory(kind, device);
        let name = RecordingsLayout::file_name(kind, device, id);
        let samples = pcm.len() as u64;
        let written = blocking(move || write_atomically(&dir, &name, &wav_bytes(&pcm))).await;
        let path = match written {
            Ok(path) => path,
            Err(RecordsError::Io(e)) => return Ok(Publication::Failed(e.to_string())),
            Err(e) => return Err(e),
        };
        let marked = match kind {
            RecordingKind::TurnRecording => true,
            RecordingKind::NearMiss => {
                self.write(MarkNearMissSaved {
                    id: NearMissRowId(id),
                    audio: samples_duration(samples),
                })
                .await?
            }
            RecordingKind::Utterance | RecordingKind::WakeClip => {
                self.write(MarkTurnFileSaved {
                    device: device.clone(),
                    id,
                    kind,
                })
                .await?
            }
        };
        if marked {
            Ok(Publication::Saved)
        } else {
            blocking(move || remove_if_present(&path)).await?;
            Ok(Publication::RowGone)
        }
    }

    /// Writes a turn's row once per its key and, for a new row, publishes its recordings and
    /// applies their retention. A repeated completion returns the existing row and
    /// publishes nothing. The same as [`RecordsService::submit_turn`] without a correction
    /// record to wait for, awaiting its result.
    pub async fn persist_turn(
        &self,
        record: TurnRecord,
        audio: TurnAudio,
    ) -> Result<PersistedTurn, RecordsError> {
        self.submit_turn(record, audio, None)
            .await
            .unwrap_or(Err(RecordsError::TaskFailed))
    }

    /// Hands a turn's row and recordings to the service, which writes them on its own task
    /// whatever becomes of the caller; the receiver hears the result. With `fix`, the row
    /// is held until the transcript-correction record arrives (or its sender goes, or the
    /// shutdown drain stops waiting) and written with it. Continuations recorded for the
    /// turn meanwhile go into the held row. Registration happens before this returns, so a
    /// continuation submitted afterwards is always applied to this row. A deletion of the
    /// device meanwhile refuses the row, even if the device is approved again first.
    pub fn submit_turn(
        &self,
        record: TurnRecord,
        audio: TurnAudio,
        fix: Option<FixFollows>,
    ) -> oneshot::Receiver<Result<PersistedTurn, RecordsError>> {
        let (reply, result) = oneshot::channel();
        let key = match record.key {
            TurnKey::Turn(uuid) => Some((record.device.clone(), uuid)),
            _ => None,
        };
        if let Some(key) = &key {
            self.held().entry(key.clone()).or_default();
        }
        let lifetime = self.gate(&record.device).1;
        let service = self.clone();
        self.inner.tasks.spawn_on(
            async move {
                let written = service
                    .write_held_turn(key, lifetime, record, audio, fix)
                    .await;
                // Nobody waiting is not an error.
                drop(reply.send(written));
            },
            &self.inner.runtime,
        );
        result
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<(DeviceId, Uuid), Held>> {
        self.inner.held.lock().expect("records held lock")
    }

    async fn write_held_turn(
        &self,
        key: Option<(DeviceId, Uuid)>,
        lifetime: RecordsLifetime,
        mut record: TurnRecord,
        audio: TurnAudio,
        fix: Option<FixFollows>,
    ) -> Result<PersistedTurn, RecordsError> {
        if let Some(fix) = fix {
            tokio::select! {
                biased;
                received = fix => {
                    if let Ok(Some(found)) = received {
                        record.stt_fix = Some(found);
                    }
                }
                () = self.inner.flush.cancelled() => {}
            }
        }
        let Some(key) = key else {
            return self.write_turn(record, audio, lifetime).await;
        };
        // The ends that arrived while the row was held go into it: the first is final.
        let early = {
            let mut held = self.held();
            let entry = held.entry(key.clone()).or_default();
            entry.writing = true;
            std::mem::take(&mut entry.ends)
        };
        let mut folded = Vec::with_capacity(early.len());
        for (end, reply) in early {
            let update = match &record.continuation {
                Some(Continuation::Pending) => {
                    record.continuation = Some(Continuation::Resolved(end));
                    ContinuationUpdate::Resolved
                }
                Some(Continuation::Resolved(first)) => ContinuationUpdate::AlreadyFinal(*first),
                None => ContinuationUpdate::NotAsked,
            };
            folded.push((end, reply, update));
        }
        let device = record.device.clone();
        let inserted = self.insert_turn(record, &audio, lifetime).await;
        // A repeat publishes nothing, so its gate goes now: its ends enter the gate
        // themselves.
        let (gate, insert) = match inserted {
            Ok((gate, insert)) if insert.inserted => (Some(gate), Ok(insert)),
            Ok((_, insert)) => (None, Ok(insert)),
            Err(error) => (None, Err(error)),
        };
        // A folded end is answered once its row's insert is settled: it was committed
        // with the row, or shares the row's failure.
        for (end, reply, update) in folded {
            let outcome = match &insert {
                Ok(insert) if insert.inserted => Ok(update),
                // A repeated completion inserted nothing: the end applies to the row
                // already stored.
                Ok(_) => self.write_continuation(&key.0, key.1, end, lifetime).await,
                Err(error) => match same_failure(error) {
                    Some(error) => Err(error),
                    // No answer: the reply ends without success.
                    None => continue,
                },
            };
            drop(reply.send(outcome));
        }
        let written = match (gate, insert) {
            (Some(gate), Ok(insert)) => self.publish_turn(gate, &device, insert, audio).await,
            (_, Ok(insert)) => Ok(PersistedTurn {
                insert,
                utterance: None,
                wake_clip: None,
                turn_recording: None,
            }),
            (_, Err(error)) => Err(error),
        };
        // Ends that arrived while the row was written apply to it now, in order; the
        // entry goes only when none is left, so a later end cannot overtake them.
        loop {
            let late = {
                let mut held = self.held();
                let ends = held
                    .get_mut(&key)
                    .map(|entry| std::mem::take(&mut entry.ends))
                    .unwrap_or_default();
                if ends.is_empty() {
                    held.remove(&key);
                    break;
                }
                ends
            };
            for (end, reply) in late {
                let update = self.write_continuation(&key.0, key.1, end, lifetime).await;
                drop(reply.send(update));
            }
        }
        written
    }

    async fn write_turn(
        &self,
        record: TurnRecord,
        audio: TurnAudio,
        lifetime: RecordsLifetime,
    ) -> Result<PersistedTurn, RecordsError> {
        let device = record.device.clone();
        let (gate, insert) = self.insert_turn(record, &audio, lifetime).await?;
        self.publish_turn(gate, &device, insert, audio).await
    }

    /// Step 1 of publishing a turn: commits its row (once per key) and keeps the device's
    /// gate held for its recordings.
    async fn insert_turn(
        &self,
        record: TurnRecord,
        audio: &TurnAudio,
        lifetime: RecordsLifetime,
    ) -> Result<(OwnedRwLockReadGuard<bool>, TurnInsert), RecordsError> {
        let has_audio = audio.utterance.is_some()
            || audio.wake_clip.is_some()
            || audio.turn_recording.is_some();
        if has_audio {
            Self::check_device(&record.device)?;
        }
        let gate = self.enter_in(&record.device, lifetime).await?;
        let insert = self
            .write(InsertTurn {
                record,
                trace_retention: self.inner.limits.turn_traces,
            })
            .await?;
        Ok((gate, insert))
    }

    /// The rest of publishing a committed turn: its recordings, the notice to the
    /// dashboard, then retention. A repeat publishes nothing.
    async fn publish_turn(
        &self,
        gate: OwnedRwLockReadGuard<bool>,
        device: &DeviceId,
        insert: TurnInsert,
        audio: TurnAudio,
    ) -> Result<PersistedTurn, RecordsError> {
        let mut result = PersistedTurn {
            insert,
            utterance: None,
            wake_clip: None,
            turn_recording: None,
        };
        if !insert.inserted {
            return Ok(result);
        }
        let published = [
            (RecordingKind::Utterance, audio.utterance),
            (RecordingKind::WakeClip, audio.wake_clip),
            (RecordingKind::TurnRecording, audio.turn_recording),
        ];
        for (kind, pcm) in published {
            let Some(pcm) = pcm else { continue };
            let outcome = Some(self.publish(kind, device, insert.id.0, pcm).await?);
            match kind {
                RecordingKind::Utterance => result.utterance = outcome,
                RecordingKind::WakeClip => result.wake_clip = outcome,
                RecordingKind::TurnRecording => result.turn_recording = outcome,
                RecordingKind::NearMiss => {}
            }
        }
        drop(gate);
        // Nobody listening is not an error.
        drop(self.inner.turns_stored.send(TurnStored {
            device: device.clone(),
            id: insert.id,
        }));
        for (kind, outcome) in [
            (RecordingKind::Utterance, &result.utterance),
            (RecordingKind::WakeClip, &result.wake_clip),
            (RecordingKind::TurnRecording, &result.turn_recording),
        ] {
            if *outcome == Some(Publication::Saved) {
                self.retain_after_write(device, kind).await?;
            }
        }
        Ok(result)
    }

    /// Writes a near miss's row once per its UUID and, for a new row with a recording,
    /// publishes it; then applies retention.
    pub async fn persist_near_miss(
        &self,
        record: NearMissRecord,
        audio: NearMissAudio,
    ) -> Result<PersistedNearMiss, RecordsError> {
        let device = record.device.clone();
        let (initial, pcm) = match audio {
            NearMissAudio::Recorded(pcm) => {
                Self::check_device(&device)?;
                (NearMissRecording::Failed, Some(pcm))
            }
            NearMissAudio::NotRecorded(reason) => (reason, None),
        };
        let gate = self.enter(&device).await?;
        let insert = self
            .write(InsertNearMiss {
                record,
                recording: initial,
                row_retention: self.inner.limits.near_miss_rows,
                series_retention: self.inner.limits.near_miss_series,
            })
            .await?;
        let recording = match pcm {
            Some(pcm) if insert.inserted => Some(
                self.publish(RecordingKind::NearMiss, &device, insert.id.0, pcm)
                    .await?,
            ),
            _ => None,
        };
        drop(gate);
        if recording == Some(Publication::Saved) || !insert.pruned.is_empty() {
            self.retain_after_write(&device, RecordingKind::NearMiss)
                .await?;
        }
        Ok(PersistedNearMiss { insert, recording })
    }

    /// Moves an asking turn's `continuation` from `pending` to its final value. A turn
    /// submitted but not yet written takes it into its row (or right after it commits).
    pub async fn record_continuation(
        &self,
        device: &DeviceId,
        turn_uuid: Uuid,
        end: ContinuationEnd,
    ) -> Result<ContinuationUpdate, RecordsError> {
        self.submit_continuation(device, turn_uuid, end)
            .await
            .unwrap_or(Err(RecordsError::TaskFailed))
    }

    /// [`RecordsService::record_continuation`] on the service's own task, so it is applied
    /// whatever becomes of the caller; the receiver hears the result. Like a submitted
    /// turn, it is refused once the device has been deleted.
    pub fn submit_continuation(
        &self,
        device: &DeviceId,
        turn_uuid: Uuid,
        end: ContinuationEnd,
    ) -> oneshot::Receiver<Result<ContinuationUpdate, RecordsError>> {
        let (reply, result) = oneshot::channel();
        {
            let mut held = self.held();
            if let Some(entry) = held.get_mut(&(device.clone(), turn_uuid)) {
                entry.ends.push((end, reply));
                return result;
            }
        }
        let lifetime = self.gate(device).1;
        let service = self.clone();
        let device = device.clone();
        self.inner.tasks.spawn_on(
            async move {
                let update = service
                    .write_continuation(&device, turn_uuid, end, lifetime)
                    .await;
                drop(reply.send(update));
            },
            &self.inner.runtime,
        );
        result
    }

    async fn write_continuation(
        &self,
        device: &DeviceId,
        turn_uuid: Uuid,
        end: ContinuationEnd,
        lifetime: RecordsLifetime,
    ) -> Result<ContinuationUpdate, RecordsError> {
        let _gate = self.enter_in(device, lifetime).await?;
        self.write(RecordContinuation {
            device: device.clone(),
            turn_uuid,
            end,
        })
        .await
    }

    /// Stores a scored wake clip's windows on its turn.
    pub async fn set_turn_wake_windows(
        &self,
        device: &DeviceId,
        turn: TurnRowId,
        windows: ClipWindows,
    ) -> Result<bool, RecordsError> {
        let _gate = self.enter(device).await?;
        self.write(SetTurnWakeWindows { id: turn, windows }).await
    }

    /// Stores a scored near-miss recording's windows.
    pub async fn set_near_miss_windows(
        &self,
        device: &DeviceId,
        near_miss: NearMissRowId,
        windows: ClipWindows,
    ) -> Result<bool, RecordsError> {
        let _gate = self.enter(device).await?;
        self.write(SetNearMissWindows {
            id: near_miss,
            windows,
        })
        .await
    }

    /// Marks a near miss, or clears its mark; a cleared mark puts its recording back under
    /// the retention limits at once.
    pub async fn set_near_miss_verdict(
        &self,
        device: &DeviceId,
        near_miss: NearMissRowId,
        verdict: Option<NearMissVerdict>,
    ) -> Result<VerdictUpdate, RecordsError> {
        let gate = self.enter(device).await?;
        let update = self
            .write(SetNearMissVerdict {
                device: device.clone(),
                id: near_miss,
                verdict,
            })
            .await?;
        drop(gate);
        if update == VerdictUpdate::Stored && verdict.is_none() {
            let gate = self.enter(device).await?;
            self.write(PruneNearMisses {
                device: device.clone(),
                row_retention: self.inner.limits.near_miss_rows,
                series_retention: self.inner.limits.near_miss_series,
            })
            .await?;
            drop(gate);
            self.retain_after_write(device, RecordingKind::NearMiss)
                .await?;
        }
        Ok(update)
    }

    /// Appends a device log line queued in `lifetime` (see [`RecordsService::lifetime`]),
    /// keeping the device's newest lines.
    pub async fn log(
        &self,
        entry: DeviceLogEntry,
        lifetime: RecordsLifetime,
    ) -> Result<i64, RecordsError> {
        let _gate = self.enter_in(&entry.device, lifetime).await?;
        self.write(InsertDeviceLog {
            entry,
            retention: self.inner.limits.device_logs,
        })
        .await
    }

    /// Adds a `stats` report to its hour.
    pub async fn record_stats(
        &self,
        device: &DeviceId,
        at: WallSeconds,
        report: StatsReport,
    ) -> Result<(), RecordsError> {
        let _gate = self.enter(device).await?;
        self.write(RecordStats {
            device: device.clone(),
            at,
            report,
        })
        .await
        .map(drop)
    }

    /// Adds a link round trip to its hour.
    pub async fn record_rtt(
        &self,
        device: &DeviceId,
        at: WallSeconds,
        sample: RttSample,
    ) -> Result<(), RecordsError> {
        let _gate = self.enter(device).await?;
        self.write(RecordRtt {
            device: device.clone(),
            at,
            sample,
        })
        .await
        .map(drop)
    }

    /// Adds a `wake.stats` report to its hour and writes its shadow events.
    pub async fn record_wake_stats(
        &self,
        device: &DeviceId,
        at: WallSeconds,
        report: WakeStatsReport,
    ) -> Result<usize, RecordsError> {
        let _gate = self.enter(device).await?;
        self.write(RecordWakeStats {
            device: device.clone(),
            at,
            report,
        })
        .await
    }

    /// Applies one kind's file retention to a device: keeps its newest saved recordings
    /// (and marked near misses) and removes every other file of the kind for the device.
    /// Returns the number of files removed.
    pub async fn enforce_retention(
        &self,
        device: &DeviceId,
        kind: RecordingKind,
    ) -> Result<usize, RecordsError> {
        Self::check_device(device)?;
        let _gate = self.exclusive(device).await?;
        self.retain_files(device, kind).await
    }

    /// Retention after a write that already committed. A device deleted in between has no
    /// files left to retain, and one whose id cannot name files has none at all; neither is
    /// the write's failure.
    async fn retain_after_write(
        &self,
        device: &DeviceId,
        kind: RecordingKind,
    ) -> Result<(), RecordsError> {
        if !file_safe(device) {
            return Ok(());
        }
        match self.enforce_retention(device, kind).await {
            Ok(_) | Err(RecordsError::Closed(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn retain_files(
        &self,
        device: &DeviceId,
        kind: RecordingKind,
    ) -> Result<usize, RecordsError> {
        let keep = self.inner.limits.files(kind);
        let retained = match kind {
            RecordingKind::TurnRecording => None,
            kind => Some(
                self.read(RetainedIds {
                    kind,
                    device: device.clone(),
                    keep,
                })
                .await?,
            ),
        };
        let layout = self.inner.layout.clone();
        let device = device.clone();
        blocking(move || {
            let mut recordings: Vec<(i64, PathBuf)> = scan(&layout, kind, Some(&device))?
                .into_iter()
                .filter_map(|f| match f.name {
                    EntryName::Recording { id, .. } => Some((id, f.path)),
                    _ => None,
                })
                .collect();
            recordings.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
            let mut removed = 0;
            for (index, (id, path)) in recordings.into_iter().enumerate() {
                let kept = match &retained {
                    Some(ids) => ids.contains(&id),
                    None => index < keep as usize,
                };
                if !kept && remove_if_present(&path)? {
                    removed += 1;
                }
            }
            Ok(removed)
        })
        .await
    }

    /// Reconciles files with rows, resolves stale continuations and applies every
    /// device's retention once. Run it before any device runtime starts.
    pub async fn start_up(&self) -> Result<ReconcileReport, RecordsError> {
        let mut report = ReconcileReport::default();
        for kind in RecordingKind::ALL {
            let layout = self.inner.layout.clone();
            let found = blocking(move || scan(&layout, kind, None)).await?;
            for entry in found {
                match entry.name {
                    // Only a deletion of its device removes an earlier controller's
                    // partial file.
                    EntryName::Unrecognized | EntryName::LegacyPartial { .. } => {
                        report.unrecognized.push(entry.path);
                    }
                    EntryName::Temporary => {
                        let path = entry.path.clone();
                        blocking(move || remove_if_present(&path)).await?;
                        report.temporary_removed.push(entry.path);
                    }
                    EntryName::Recording { device, id } => {
                        self.reconcile_file(kind, device, id, entry.path, &mut report)
                            .await?;
                    }
                }
            }
        }
        report.stale_continuations = self.write(ResolveStaleContinuations).await?;
        for device in self.read(DevicesWithRecordings).await? {
            self.write(PruneNearMisses {
                device: device.clone(),
                row_retention: self.inner.limits.near_miss_rows,
                series_retention: self.inner.limits.near_miss_series,
            })
            .await?;
            if !file_safe(&device) {
                continue;
            }
            for kind in RecordingKind::ALL {
                report.retention_removed += self.enforce_retention(&device, kind).await?;
            }
        }
        Ok(report)
    }

    async fn reconcile_file(
        &self,
        kind: RecordingKind,
        device: DeviceId,
        id: i64,
        path: PathBuf,
        report: &mut ReconcileReport,
    ) -> Result<(), RecordsError> {
        let state = self
            .read(GetRowState {
                kind,
                device: device.clone(),
                id,
            })
            .await?;
        match state {
            RowState::Saved => {}
            RowState::Missing | RowState::NotRecorded => {
                let p = path.clone();
                blocking(move || remove_if_present(&p)).await?;
                report.orphans_removed.push(path);
            }
            RowState::Unsaved => {
                let p = path.clone();
                let samples = blocking(move || wav_samples(&p)).await?;
                let adopted = match samples {
                    Some(samples) => {
                        self.write(AdoptRecording {
                            kind,
                            device,
                            id,
                            samples,
                        })
                        .await?
                    }
                    None => false,
                };
                if adopted {
                    report.adopted.push(path);
                } else {
                    let p = path.clone();
                    blocking(move || remove_if_present(&p)).await?;
                    report.incomplete_removed.push(path);
                }
            }
        }
        Ok(())
    }

    /// Deletes every record of a device: closes its gate and ends its lifetime (so late
    /// writers fail, even after a reopen), waits for writes in progress, deletes its rows
    /// and then its recordings; its own wake-clip directory goes whole. The gate stays
    /// closed until [`RecordsService::reopen_device`]. Delete the `devices` row afterwards.
    pub async fn delete_device(&self, device: &DeviceId) -> Result<DeletedRecords, RecordsError> {
        let mut gate = self.gate(device).0.write_owned().await;
        *gate = false;
        self.end_lifetime(device);
        let rows = self.write(DeleteDeviceRecords(device.clone())).await?;
        let mut files = 0;
        if file_safe(device) {
            let layout = self.inner.layout.clone();
            let d = device.clone();
            files = blocking(move || {
                let mut removed = 0;
                for kind in RecordingKind::ALL {
                    for found in scan(&layout, kind, Some(&d))? {
                        if !matches!(found.name, EntryName::Unrecognized)
                            && remove_if_present(&found.path)?
                        {
                            removed += 1;
                        }
                    }
                }
                match std::fs::remove_dir_all(layout.directory(RecordingKind::WakeClip, &d)) {
                    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
                Ok(removed)
            })
            .await?;
        }
        drop(gate);
        Ok(DeletedRecords { rows, files })
    }

    /// Opens a deleted device's records again, before a runtime starts for it anew. A
    /// closed gate it opens starts a new lifetime, so a write queued while the device was
    /// deleted is refused too. Opening a gate that is open changes nothing.
    pub async fn reopen_device(&self, device: &DeviceId) {
        let mut gate = self.gate(device).0.write_owned().await;
        if *gate {
            return;
        }
        let closed_all = self
            .inner
            .gates
            .lock()
            .expect("records gates lock")
            .closed_all;
        if !closed_all {
            self.end_lifetime(device);
            *gate = true;
        }
    }

    /// Drains the submitted writes ([`RecordsService::drain`] with [`DRAIN_WAIT`]), then
    /// refuses new writes for every device and waits for those in progress, for shutdown.
    pub async fn shutdown(&self) {
        self.drain(DRAIN_WAIT).await;
        let gates: Vec<Gate> = {
            let mut gates = self.inner.gates.lock().expect("records gates lock");
            gates.closed_all = true;
            gates
                .by_device
                .values()
                .map(|(gate, _)| Arc::clone(gate))
                .collect()
        };
        for gate in gates {
            *gate.write_owned().await = false;
        }
    }

    /// Waits up to `wait` for the submitted writes to finish; rows still held for their
    /// correction records are then written without them, and those writes get `wait`
    /// again. Returns whether every submitted write finished.
    pub async fn drain(&self, wait: Duration) -> bool {
        let tasks = &self.inner.tasks;
        tasks.close();
        let mut done = tokio::time::timeout(wait, tasks.wait()).await.is_ok();
        if !done {
            self.inner.flush.cancel();
            done = tokio::time::timeout(wait, tasks.wait()).await.is_ok();
        }
        tasks.reopen();
        done
    }
}
