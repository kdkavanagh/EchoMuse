//! Clip-window scoring (spec §4.11, §5.1 "Background threads"): the wake model's windows
//! over every saved wake clip and near-miss recording, scored once, in the background,
//! for the archives' crops and the dashboard's window chart.
//!
//! - **What is scored.** A turn whose wake clip was saved (`wake_file` set) and a near
//!   miss whose recording was saved (`recording = 'saved'`), while its windows are NULL
//!   and its file is still on disk, newest first (retention removes the oldest first).
//! - **With which graph.** The one that scored the clip on the device
//!   (`wake_model_sha256`), while the registry still holds it; otherwise the model the
//!   device selects now. `graph_sha256` in the stored windows always names the graph that
//!   scored them. A clip no usable graph can score is left NULL and not retried until
//!   restart.
//! - **Windows.** 22,400 samples (1.4 s) every 2,560 (160 ms) from the clip's start,
//!   plus one ending at the clip's last sample when the hops do not reach it, as every
//!   stored row lays them out (537 of 537 deployed rows). A clip shorter than one window
//!   is scored once, with silence before it. A window below the graph's RMS floor is
//!   silence and is stored as 0, as the deployed rows store it (each lists every window).
//!   Scores are rounded to four decimals, as stored.
//! - **Where it runs.** One thread of its own (`clip-scoring`) owns the wake scorers and
//!   reads the files, so no device's speech workers or runtime thread and no shared I/O
//!   task waits on it. A task spawned through the [`TaskSupervisor`] finds the clips,
//!   hands them to that thread one at a time and stores the results through the records
//!   service. It rescans after each new turn and every minute.
//! - **On demand.** A cropped archive scores the clips it needs at once
//!   ([`ClipScoring::score_now`]).
//! - **Count.** [`ClipScoring::pending`] is the clips still to score, from the latest
//!   scan; `None` before the first scan and while the speech models are not loaded.

use std::{
    collections::HashSet,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc as std_mpsc},
    time::Duration,
};

use echomuse_api::{BoxFuture, ClipKind, ClipRef, ClipScoring, ServiceError};
use echomuse_core::types::{DeviceId, RuntimeId};
use echomuse_db::{DbHandle, ReadQuery};
use echomuse_protocol::Sha256;
use echomuse_records::{
    ClipWindows, NearMissRowId, RecordingKind, RecordingsLayout, RecordsService, TurnRowId,
};
use echomuse_settings::service::SettingsHandle;
use echomuse_speech::{
    BcresnetModelFiles, MonoWav, PinnedFile, Sha256Digest, WindowScore,
    bcresnet::WAKE_WINDOW_SAMPLES,
};
use echomuse_voice::speech::models::{BoxedWakeScorer, ModelSet};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{Notify, oneshot, watch};

use crate::{
    speech_models::SpeechStatus,
    speech_workers::SpeechWorkerKind,
    supervision::{SpeechPathState, TaskSupervisor, Watchdog, WorkerOwner},
};

/// Samples between window starts (160 ms).
const HOP: usize = 2_560;
/// How long the scorer waits between scans when nothing new arrives.
const RESCAN: Duration = Duration::from_secs(60);
/// Clips scored before the next scan, so new clips are not kept waiting behind a backlog.
const BATCH: usize = 32;
/// The capture rate every clip is recorded at.
const RATE: u32 = 16_000;

/// Each window's start sample in a clip of `samples`.
fn window_starts(samples: usize) -> Vec<usize> {
    if samples <= WAKE_WINDOW_SAMPLES {
        return vec![0];
    }
    let last = samples - WAKE_WINDOW_SAMPLES;
    let mut starts: Vec<usize> = (0..=last).step_by(HOP).collect();
    if starts.last() != Some(&last) {
        starts.push(last);
    }
    starts
}

fn rounded(score: f64) -> f64 {
    (score * 10_000.0).round() / 10_000.0
}

/// Scores every window of `pcm`.
fn score_clip(
    scorer: &mut BoxedWakeScorer,
    pcm: &[i16],
) -> Result<Vec<(u64, f64)>, echomuse_speech::SpeechError> {
    let padded;
    let pcm = if pcm.len() < WAKE_WINDOW_SAMPLES {
        let mut audio = vec![0i16; WAKE_WINDOW_SAMPLES - pcm.len()];
        audio.extend_from_slice(pcm);
        padded = audio;
        &padded[..]
    } else {
        pcm
    };
    window_starts(pcm.len())
        .into_iter()
        .map(|start| {
            let score = match scorer.score_window(&pcm[start..start + WAKE_WINDOW_SAMPLES])? {
                WindowScore::BelowRmsFloor => 0.0,
                WindowScore::Scored { probability } => rounded(probability),
            };
            Ok((start as u64, score))
        })
        .collect()
}

// ─── The scoring thread ─────────────────────────────────────────────────────────────

struct Job {
    models: Arc<dyn ModelSet>,
    device: DeviceId,
    graph: Sha256,
    files: BcresnetModelFiles,
    path: PathBuf,
    reply: oneshot::Sender<Result<ClipWindows, String>>,
}

/// What the scoring thread reports its model calls to.
struct Watched {
    watchdog: Watchdog,
    /// The scorer's own speech-path state: a call past the deadline marks only the
    /// scorer, never a device's wakes.
    path: Arc<watch::Sender<SpeechPathState>>,
}

fn run_jobs(jobs: std_mpsc::Receiver<Job>, watched: Watched) {
    let mut scorer: Option<(Sha256, BoxedWakeScorer)> = None;
    for job in jobs {
        // Runtime 0 is no runtime: the call belongs to the clip scorer, scoring one of the
        // device's clips with the BCResNet graph reference scoring also runs.
        let owner = WorkerOwner {
            device: job.device.clone(),
            runtime: RuntimeId(0),
        };
        let guard =
            watched
                .watchdog
                .begin(&owner, SpeechWorkerKind::ReferenceScoring, &watched.path);
        let outcome = catch_unwind(AssertUnwindSafe(|| score_job(&mut scorer, &job)));
        let late = guard.finish();
        let result = match outcome {
            Ok(_) if late => {
                // A call past its deadline is not trusted, and neither is the scorer.
                scorer = None;
                Err("the wake scorer ran past its deadline".to_owned())
            }
            Ok(result) => result,
            Err(_) => {
                // The scorer's state after an interrupted call is unknown: drop it.
                scorer = None;
                Err("the wake scorer panicked".to_owned())
            }
        };
        let _ = job.reply.send(result);
    }
}

fn score_job(
    scorer: &mut Option<(Sha256, BoxedWakeScorer)>,
    job: &Job,
) -> Result<ClipWindows, String> {
    let wav = MonoWav::read(&job.path).map_err(|error| error.to_string())?;
    if wav.sample_rate != RATE {
        return Err(format!(
            "{} is {} Hz, not {RATE}",
            job.path.display(),
            wav.sample_rate
        ));
    }
    if scorer.as_ref().is_none_or(|(graph, _)| *graph != job.graph) {
        *scorer = None;
        let loaded = job
            .models
            .wake_scorer(&job.files)
            .map_err(|error| format!("the wake graph did not load: {error}"))?;
        *scorer = Some((job.graph.clone(), loaded));
    }
    let (_, loaded) = scorer.as_mut().expect("loaded just above");
    let scores = score_clip(loaded, &wav.samples)
        .map_err(|error| format!("the wake graph failed: {error}"))?;
    Ok(ClipWindows {
        graph_sha256: job.graph.clone(),
        samples: wav.samples.len() as u64,
        window: WAKE_WINDOW_SAMPLES as u64,
        scores,
    })
}

// ─── Reads ──────────────────────────────────────────────────────────────────────────

/// A saved clip still to score, with the graph its row names.
#[derive(Clone, Debug)]
struct Unscored {
    clip: ClipRef,
    model: Option<String>,
}

fn table(kind: ClipKind) -> (&'static str, &'static str) {
    match kind {
        ClipKind::WakeClip => ("turns", "wake_file IS NOT NULL AND wake_windows IS NULL"),
        ClipKind::NearMiss => ("near_misses", "recording = 'saved' AND windows IS NULL"),
    }
}

/// Every saved clip without windows, newest first within each kind.
struct AllUnscored;

impl ReadQuery for AllUnscored {
    type Output = Vec<Unscored>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Vec<Unscored>> {
        let mut out = Vec::new();
        for kind in [ClipKind::WakeClip, ClipKind::NearMiss] {
            let (table, saved) = table(kind);
            let sql = format!(
                "SELECT id, device_id, wake_model_sha256 FROM {table} WHERE {saved} ORDER BY id DESC"
            );
            let rows = conn
                .prepare_cached(&sql)?
                .query_map([], |row| {
                    Ok(Unscored {
                        clip: ClipRef {
                            kind,
                            id: row.get(0)?,
                            device: DeviceId(row.get(1)?),
                        },
                        model: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            out.extend(rows);
        }
        Ok(out)
    }
}

/// Those of `clips` still to score.
struct UnscoredAmong(Vec<ClipRef>);

impl ReadQuery for UnscoredAmong {
    type Output = Vec<Unscored>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Vec<Unscored>> {
        let mut out = Vec::new();
        for clip in self.0 {
            let (table, saved) = table(clip.kind);
            let sql = format!(
                "SELECT wake_model_sha256 FROM {table} WHERE id = ?1 AND device_id = ?2 AND {saved}"
            );
            let model: Option<Option<String>> = conn
                .prepare_cached(&sql)?
                .query_row(rusqlite::params![clip.id, clip.device.0], |row| row.get(0))
                .optional()?;
            if let Some(model) = model {
                out.push(Unscored { clip, model });
            }
        }
        Ok(out)
    }
}

// ─── The service ────────────────────────────────────────────────────────────────────

struct Inner {
    db: DbHandle,
    records: RecordsService,
    settings: SettingsHandle,
    speech: watch::Receiver<SpeechStatus>,
    jobs: Mutex<std_mpsc::Sender<Job>>,
    pending: Mutex<Option<u64>>,
    unscorable: Mutex<HashSet<ClipRef>>,
    rescan: Notify,
}

/// Clip-window scoring. Cheap to clone.
#[derive(Clone)]
pub struct ClipScorer {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for ClipScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipScorer")
            .field("pending", &self.pending())
            .finish_non_exhaustive()
    }
}

fn path_of(layout: &RecordingsLayout, clip: &ClipRef) -> Option<PathBuf> {
    let kind = match clip.kind {
        ClipKind::WakeClip => RecordingKind::WakeClip,
        ClipKind::NearMiss => RecordingKind::NearMiss,
    };
    echomuse_records::recordings::file_safe(&clip.device)
        .then(|| layout.path(kind, &clip.device, clip.id))
}

impl ClipScorer {
    /// Starts the scoring thread and the scanning task. `speech` says when the models are
    /// loaded; each new turn the records store asks for a rescan. Every model call is
    /// reported to `watchdog`, so one that hangs past the recovery deadline ends the
    /// process as a device's would (§5.5), and a result that came back late is not stored.
    pub fn start(
        db: DbHandle,
        records: RecordsService,
        settings: SettingsHandle,
        speech: watch::Receiver<SpeechStatus>,
        watchdog: Watchdog,
        tasks: &TaskSupervisor,
    ) -> std::io::Result<Self> {
        let (jobs, receiver) = std_mpsc::channel();
        let watched = Watched {
            watchdog,
            path: Arc::new(watch::Sender::new(SpeechPathState::default())),
        };
        std::thread::Builder::new()
            .name("clip-scoring".to_owned())
            .spawn(move || run_jobs(receiver, watched))?;
        let scorer = Self {
            inner: Arc::new(Inner {
                db,
                records,
                settings,
                speech,
                jobs: Mutex::new(jobs),
                pending: Mutex::new(None),
                unscorable: Mutex::new(HashSet::new()),
                rescan: Notify::new(),
            }),
        };
        let background = scorer.clone();
        drop(tasks.spawn("clip window scoring", async move {
            background.run().await;
        }));
        let turns = scorer.clone();
        let mut stored = turns.inner.records.subscribe_turns();
        drop(tasks.spawn("clip window rescans", async move {
            loop {
                match stored.recv().await {
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        turns.inner.rescan.notify_one();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        }));
        Ok(scorer)
    }

    fn models(&self) -> Result<Arc<dyn ModelSet>, String> {
        match &*self.inner.speech.borrow() {
            SpeechStatus::Ready(models) => Ok(Arc::clone(models)),
            SpeechStatus::Loading => Err("the speech models are still loading".to_owned()),
            SpeechStatus::Unavailable(reason) => {
                Err(format!("the speech models are unavailable: {reason}"))
            }
        }
    }

    fn set_pending(&self, pending: Option<u64>) {
        *self.inner.pending.lock().expect("pending lock") = pending;
    }

    /// The graph files to score `clip` with, and their graph's hash.
    fn graph_for(&self, clip: &Unscored) -> Option<(Sha256, BcresnetModelFiles)> {
        let snapshot = self.inner.settings.snapshot();
        let registry = snapshot.registry();
        let recorded = clip
            .model
            .as_deref()
            .and_then(Sha256::parse)
            .and_then(|sha| registry.model(&sha))
            .filter(|model| model.is_usable());
        let model = match recorded {
            Some(model) => model,
            None => snapshot
                .selected_model(&clip.clip.device)
                .ok()
                .filter(|model| model.is_usable())?,
        };
        let pinned = |file: &str, sha: &Sha256| {
            Sha256Digest::parse_hex(sha.as_str())
                .ok()
                .map(|digest| PinnedFile::new(registry.dir().join(file), digest))
        };
        let files = BcresnetModelFiles {
            graph: pinned(model.graph_file(), model.graph_sha256())?,
            sidecar: pinned(model.sidecar_file(), model.sidecar_sha256())?,
        };
        Some((model.graph_sha256().clone(), files))
    }

    /// Scores and stores one clip; `false` when it cannot be scored (it is then not
    /// tried again until restart).
    async fn score_one(&self, models: &Arc<dyn ModelSet>, clip: Unscored) -> bool {
        let Some(path) = path_of(self.inner.records.layout(), &clip.clip) else {
            return self.unscorable(clip.clip, "its device id cannot name files");
        };
        let Some((graph, files)) = self.graph_for(&clip) else {
            return self.unscorable(clip.clip, "no usable wake model can score it");
        };
        let (reply, answer) = oneshot::channel();
        let job = Job {
            models: Arc::clone(models),
            device: clip.clip.device.clone(),
            graph,
            files,
            path,
            reply,
        };
        if self
            .inner
            .jobs
            .lock()
            .expect("jobs lock")
            .send(job)
            .is_err()
        {
            return self.unscorable(clip.clip, "the scoring thread has stopped");
        }
        let windows = match answer.await {
            Ok(Ok(windows)) => windows,
            Ok(Err(reason)) => return self.unscorable(clip.clip, &reason),
            Err(_) => return self.unscorable(clip.clip, "the scoring thread has stopped"),
        };
        let device = &clip.clip.device;
        let stored = match clip.clip.kind {
            ClipKind::WakeClip => {
                self.inner
                    .records
                    .set_turn_wake_windows(device, TurnRowId(clip.clip.id), windows)
                    .await
            }
            ClipKind::NearMiss => {
                self.inner
                    .records
                    .set_near_miss_windows(device, NearMissRowId(clip.clip.id), windows)
                    .await
            }
        };
        // A clip whose windows cannot be stored is set aside like one that cannot be
        // scored; otherwise every scan would score it again.
        match stored {
            Ok(_) => true,
            Err(error) => {
                self.unscorable(clip.clip, &format!("its windows were not stored: {error}"))
            }
        }
    }

    fn unscorable(&self, clip: ClipRef, why: &str) -> bool {
        tracing::info!(device = %clip.device, id = clip.id, kind = ?clip.kind, why, "a saved clip cannot be scored");
        self.inner
            .unscorable
            .lock()
            .expect("unscorable lock")
            .insert(clip);
        false
    }

    /// The saved clips still to score whose file is on disk, newest first.
    async fn scan(&self) -> Result<Vec<Unscored>, String> {
        let all = self
            .inner
            .db
            .read(AllUnscored)
            .await
            .map_err(|error| error.to_string())?;
        let skip = self
            .inner
            .unscorable
            .lock()
            .expect("unscorable lock")
            .clone();
        let layout = self.inner.records.layout().clone();
        tokio::task::spawn_blocking(move || {
            all.into_iter()
                .filter(|clip| !skip.contains(&clip.clip))
                .filter(|clip| path_of(&layout, &clip.clip).is_some_and(|path| path.is_file()))
                .collect()
        })
        .await
        .map_err(|_| "checking the clip files failed".to_owned())
    }

    async fn run(&self) {
        let mut speech = self.inner.speech.clone();
        loop {
            let models = match self.models() {
                Ok(models) => models,
                Err(_) => {
                    self.set_pending(None);
                    if speech.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
            };
            let clips = match self.scan().await {
                Ok(clips) => clips,
                Err(error) => {
                    tracing::warn!(%error, "the clips to score could not be listed");
                    tokio::time::sleep(RESCAN).await;
                    continue;
                }
            };
            let mut pending = clips.len() as u64;
            self.set_pending(Some(pending));
            if clips.is_empty() {
                tokio::select! {
                    () = self.inner.rescan.notified() => {}
                    () = tokio::time::sleep(RESCAN) => {}
                }
                continue;
            }
            for clip in clips.into_iter().take(BATCH) {
                self.score_one(&models, clip).await;
                pending = pending.saturating_sub(1);
                self.set_pending(Some(pending));
            }
        }
    }
}

impl ClipScoring for ClipScorer {
    fn pending(&self) -> Option<u64> {
        *self.inner.pending.lock().expect("pending lock")
    }

    fn score_now(&self, clips: Vec<ClipRef>) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let models = self.models().map_err(|why| {
                ServiceError::Unavailable(format!("Clips cannot be scored now: {why}"))
            })?;
            let unscored = self
                .inner
                .db
                .read(UnscoredAmong(clips))
                .await
                .map_err(|error| ServiceError::Failed(format!("reading the clips: {error}")))?;
            let skip = self
                .inner
                .unscorable
                .lock()
                .expect("unscorable lock")
                .clone();
            for clip in unscored {
                if !skip.contains(&clip.clip) {
                    self.score_one(&models, clip).await;
                }
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_laid_out_as_the_stored_rows() {
        // Deployed rows: 32,000 samples → 0, 2,560, 5,120, 7,680, 9,600; 42,688 → …,
        // 17,920, 20,288; 23,040 → 0, 640.
        assert_eq!(window_starts(32_000), [0, 2_560, 5_120, 7_680, 9_600]);
        let long = window_starts(42_688);
        assert_eq!(long.len(), 9);
        assert_eq!(&long[7..], [17_920, 20_288]);
        assert_eq!(window_starts(23_040), [0, 640]);
        assert_eq!(window_starts(22_400), [0]);
        assert_eq!(window_starts(100), [0]);
        assert_eq!(window_starts(22_400 + 2_560), [0, 2_560]);
    }

    #[test]
    fn scores_are_kept_to_four_decimals() {
        assert_eq!(rounded(0.406_449), 0.4064);
        assert_eq!(rounded(0.000_04), 0.0);
    }
}
