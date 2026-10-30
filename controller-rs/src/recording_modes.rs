//! The dashboard's recording modes (spec §4.11; configuration.md "Recording tools";
//! playback-capture.md): sample collection and ambient recording.
//!
//! - **State.** Whether each mode is armed is stored in `devices.collect_mode` and
//!   `devices.ambient_mode`, so an armed mode survives a reconnect and a restart; the two
//!   exclude each other. Capture windows are not here.
//! - **Audio.** While a mode is armed and the device is connected and unmuted, its
//!   runtime keeps one `diagnostic` uplink subscription open (mic live, plus AFE records
//!   in an `afe_metadata` session) and hands its audio to a recorder for that
//!   session, on a blocking thread. While the subscription is live the device takes no
//!   wake and the ring pulses magenta.
//! - **Samples** (`samples/<device>/<unix ms>.wav`): the stream is cut at its silences.
//!   A clip opens when an 80 ms block is [`OPEN_ABOVE_FLOOR_DB`] louder than the room's
//!   floor, keeps [`PREROLL`] before that and runs until [`HANGOVER`] of quiet; clips
//!   with less than [`MIN_SPEECH`] of loud audio are dropped and clips are cut at
//!   [`MAX_CLIP`]. The newest [`SAMPLES_KEPT`] clips are kept.
//! - **Ambient** (`ambient/<device>/<unix ms>.wav`): everything, written as
//!   `<unix ms>.wav.part` and renamed when the subscription ends or after
//!   [`AMBIENT_ROLL`], when the next file starts. The newest [`AMBIENT_KEPT`] are kept.
//! - **AFE sidecar** (`<unix ms>.afe.json`), in an `afe_metadata` session: one entry per
//!   80 ms period of the recording, from the AFE record whose capture period holds the
//!   period's middle sample, on the recording's own timeline (afe-metadata.md).
//! - **Privacy.** On a mute the subscription ends as muted: the clip being cut and the
//!   ambient file being written are discarded, never saved. Any other end saves what was
//!   recorded. A recorder whose queue overflowed discards its open file and the runtime
//!   reopens the subscription.
//!
//! Decisions where the sources are silent: the cutting thresholds above. The only
//! sources are the dashboard's "a clip starts when the room gets ~12 dB louder than its
//! own floor" and the length of the 242 deployed sample clips (0.8 s to 1.92 s, median
//! 1.12 s, in 160 ms steps), which these values reproduce for a spoken wake word.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use echomuse_api::{
    AmbientListing, AmbientLive, ApiEvent, ArchiveFile, BoxFuture, ClipEntry, ClipName, ClipPart,
    CollectLive, DeviceStatePatch, EventPublisher, RecordingMode, RecordingModes, SamplesListing,
    ServiceError,
};
use echomuse_core::types::{DeviceId, SessionId};
use echomuse_db::{DbHandle, ReadQuery, WriteCommand};
use echomuse_protocol::{
    StreamId,
    ema::{AfeFlags, AfeRecord},
};
use echomuse_records::recordings::{RecordingsLayout, file_safe, wav_bytes, write_atomically};
use echomuse_uplink::{ChunkBody, ChunkData, EndCause};
use echomuse_voice::DiagnosticAudio;
use rusqlite::{Connection, Transaction, params};
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, watch};

use crate::{app::Directory, supervision::TaskSupervisor};

/// Capture samples per second.
const RATE: u64 = 16_000;
/// Samples per AFE period and per analysis block (80 ms).
const BLOCK: usize = 1_280;
/// How much louder than the floor a block opens a clip.
pub const OPEN_ABOVE_FLOOR_DB: f64 = 12.0;
/// How much louder than the floor a block still counts as loud inside a clip.
pub const KEEP_ABOVE_FLOOR_DB: f64 = 6.0;
/// Audio kept before the block that opened a clip.
pub const PREROLL: Duration = Duration::from_millis(240);
/// Quiet that ends a clip (kept at its end).
pub const HANGOVER: Duration = Duration::from_millis(400);
/// Loud audio a clip needs to be kept.
pub const MIN_SPEECH: Duration = Duration::from_millis(160);
/// The longest clip.
pub const MAX_CLIP: Duration = Duration::from_secs(3);
/// Sample clips kept per device.
pub const SAMPLES_KEPT: usize = 2_000;
/// How long one ambient file runs.
pub const AMBIENT_ROLL: Duration = Duration::from_secs(30 * 60);
/// Ambient files kept per device.
pub const AMBIENT_KEPT: usize = 6;
/// Audio deliveries queued for a recorder.
pub const RECORDER_QUEUE: usize = 1_024;
/// How long deleting a device waits for its recorders to finish.
pub const FORGET_WAIT: Duration = Duration::from_secs(10);
/// The quietest floor assumed, dBFS.
const FLOOR_MIN_DB: f64 = -90.0;

fn samples_of(duration: Duration) -> usize {
    (duration.as_millis() as u64 * RATE / 1_000) as usize
}

fn blocks_of(duration: Duration) -> usize {
    samples_of(duration).div_ceil(BLOCK)
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The level of one block, dBFS.
fn level_db(block: &[i16]) -> f64 {
    if block.is_empty() {
        return FLOOR_MIN_DB;
    }
    let energy: f64 = block.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    let rms = (energy / block.len() as f64).sqrt() / 32_768.0;
    if rms <= 0.0 {
        FLOOR_MIN_DB
    } else {
        (20.0 * rms.log10()).max(FLOOR_MIN_DB)
    }
}

// ─── Stored mode state ──────────────────────────────────────────────────────────────

/// Which modes a device has armed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modes {
    pub collect: bool,
    pub ambient: bool,
}

impl Modes {
    fn armed(self) -> bool {
        self.collect || self.ambient
    }
}

/// Every approved device's stored modes.
struct ReadModes;

impl ReadQuery for ReadModes {
    type Output = Vec<(DeviceId, Modes)>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Vec<(DeviceId, Modes)>> {
        let mut statement = conn.prepare_cached(
            "SELECT device_id, collect_mode, ambient_mode FROM devices WHERE approved = 1",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                DeviceId(row.get(0)?),
                Modes {
                    collect: row.get::<_, i64>(1)? != 0,
                    ambient: row.get::<_, i64>(2)? != 0,
                },
            ))
        })?;
        rows.collect()
    }
}

/// Stores a device's modes. False when no row has the id.
struct WriteModes {
    device: DeviceId,
    modes: Modes,
}

impl WriteCommand for WriteModes {
    type Output = bool;
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<bool> {
        let changed = tx
            .prepare_cached(
                "UPDATE devices SET collect_mode = ?2, ambient_mode = ?3 WHERE device_id = ?1",
            )?
            .execute(params![
                self.device.0,
                i64::from(self.modes.collect),
                i64::from(self.modes.ambient)
            ])?;
        Ok(changed > 0)
    }
}

// ─── Files ──────────────────────────────────────────────────────────────────────────

fn mode_dir(root: &Path, mode: RecordingMode, device: &DeviceId) -> PathBuf {
    root.join(mode.directory()).join(&device.0)
}

/// The finished recordings of a directory, newest first.
fn list_clips(dir: &Path) -> io::Result<Vec<(ClipName, ClipEntry)>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut clips = Vec::new();
    for entry in entries {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str().and_then(ClipName::new) else {
            continue;
        };
        let bytes = entry.metadata()?.len();
        let afe = dir.join(name.sidecar()).exists();
        let ms = bytes.saturating_sub(44) * 1_000 / (RATE * 2);
        let ts = name.unix_ms().unwrap_or(0) as f64 / 1_000.0;
        clips.push((
            name.clone(),
            ClipEntry {
                name: name.as_str().to_owned(),
                ts,
                ms,
                bytes,
                afe,
            },
        ));
    }
    clips.sort_by_key(|clip| std::cmp::Reverse(clip.0.unix_ms()));
    Ok(clips)
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Removes a recording and its sidecar. Returns whether the recording existed.
fn remove_clip(dir: &Path, name: &ClipName) -> io::Result<bool> {
    let removed = remove_if_present(&dir.join(name.as_str()))?;
    remove_if_present(&dir.join(name.sidecar()))?;
    Ok(removed)
}

/// Keeps a directory's newest `keep` recordings.
fn retain(dir: &Path, keep: usize) -> io::Result<usize> {
    let mut removed = 0;
    for (name, _) in list_clips(dir)?.into_iter().skip(keep) {
        if remove_clip(dir, &name)? {
            removed += 1;
        }
    }
    Ok(removed)
}

// ─── AFE series ─────────────────────────────────────────────────────────────────────

/// AFE records by the capture sample their period starts at.
#[derive(Default)]
struct AfeTrack {
    records: BTreeMap<u64, AfeRecord>,
}

const AFE_FLAGS: [(AfeFlags, &str); 6] = [
    (AfeFlags::GAP, "gap"),
    (AfeFlags::SYNC, "sync"),
    (AfeFlags::OUTPUT_CLIPPED, "output_clipped"),
    (AfeFlags::MIC_CLIPPED, "mic_clipped"),
    (AfeFlags::AEC_DIVERGED, "aec_diverged"),
    (AfeFlags::DEVICE_MUTE, "device_mute"),
];

impl AfeTrack {
    fn add(&mut self, first: u64, records: &[AfeRecord]) {
        for (index, record) in records.iter().enumerate() {
            self.records.insert(first + (index * BLOCK) as u64, *record);
        }
    }

    /// Forgets records that end before `sample`.
    fn forget_before(&mut self, sample: u64) {
        let keep = self.records.split_off(&sample.saturating_sub(BLOCK as u64));
        self.records = keep;
    }

    /// The series for the recording of `len` samples starting at capture sample
    /// `start`: one entry per period, `null` where no record covers it.
    fn series(&self, start: u64, len: usize) -> Value {
        let periods = len.div_ceil(BLOCK);
        let mut columns: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
        let mut flags: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        for period in 0..periods {
            let middle = start + (period * BLOCK + BLOCK / 2) as u64;
            let record = self
                .records
                .range(..=middle)
                .next_back()
                .filter(|(first, _)| middle < *first + BLOCK as u64)
                .map(|(_, record)| *record);
            let mut push = |key: &'static str, value: Value| {
                columns.entry(key).or_default().push(value);
            };
            match record {
                None => {
                    for key in [
                        "frames",
                        "lost",
                        "playback",
                        "erle_max",
                        "erle_mean",
                        "erle_frames",
                        "dtd_max",
                        "dtd_frames",
                        "rms_max_db",
                        "rms_mean_db",
                        "vad_max",
                        "vad_frames",
                        "volume",
                    ] {
                        push(key, Value::Null);
                    }
                }
                Some(r) => {
                    let data = r.frames > 0;
                    let erle = data && r.erle_frames > 0;
                    let rms = |raw: u8| (data && raw > 0).then(|| i64::from(raw) - 256);
                    push("frames", json!(r.frames));
                    push("lost", json!(r.lost));
                    push("playback", json!(r.playback));
                    push("erle_max", json!(erle.then_some(r.erle_max)));
                    push("erle_mean", json!(erle.then_some(r.erle_mean)));
                    push("erle_frames", json!(r.erle_frames));
                    push(
                        "dtd_max",
                        json!(data.then(|| f64::from(r.dtd_max) / f64::from(AfeRecord::DTD_MAX))),
                    );
                    push("dtd_frames", json!(r.dtd_frames));
                    push("rms_max_db", json!(rms(r.rms_max)));
                    push("rms_mean_db", json!(rms(r.rms_mean)));
                    push("vad_max", json!(data.then(|| f64::from(r.vad_max) * 0.25)));
                    push("vad_frames", json!(r.vad_frames));
                    push("volume", json!(data.then_some(r.volume)));
                    for (flag, name) in AFE_FLAGS {
                        if r.flags.contains(flag) {
                            flags.entry(name).or_default().push(period);
                        }
                    }
                }
            }
        }
        let mut series = Map::new();
        series.insert("start".into(), json!(0));
        series.insert("period".into(), json!(BLOCK));
        series.insert("support".into(), Value::Null);
        series.insert("utterance".into(), Value::Null);
        for (key, values) in columns {
            series.insert(key.into(), Value::Array(values));
        }
        series.insert("flags".into(), json!(flags));
        series.insert("wake_clip".into(), Value::Null);
        series.insert("audio_clip".into(), Value::Null);
        series.insert("recording".into(), json!([0, len]));
        series.insert("stages".into(), json!({}));
        Value::Object(series)
    }
}

// ─── Sample segmenter ───────────────────────────────────────────────────────────────

/// What sample collection reports while it runs.
#[derive(Clone, Debug, Default)]
struct CollectStats {
    floor_db: Option<f64>,
    frames: u64,
    dropped_short: u64,
    truncated: u64,
}

/// A clip the segmenter finished: its samples, from capture sample `start`.
struct CutClip {
    start: u64,
    pcm: Vec<i16>,
}

/// Cuts a contiguous mic stream at its silences.
#[derive(Default)]
struct Segmenter {
    /// Recent blocks, for the preroll, while no clip is open.
    history: Vec<i16>,
    /// The capture sample after the last one pushed.
    next: Option<u64>,
    open: Option<OpenClip>,
    pending: Vec<i16>,
    stats: CollectStats,
}

struct OpenClip {
    start: u64,
    pcm: Vec<i16>,
    loud_blocks: usize,
    quiet_run: usize,
}

impl Segmenter {
    /// Adds samples starting at capture sample `first`; returns finished clips.
    fn push(&mut self, first: u64, samples: &[i16]) -> Vec<CutClip> {
        if self
            .next
            .is_some_and(|next| next + self.pending.len() as u64 != first)
        {
            // A jump in the timeline (a new epoch): nothing straddles it.
            self.discard();
        }
        if self.next.is_none() {
            self.next = Some(first);
        }
        self.pending.extend_from_slice(samples);
        let mut done = Vec::new();
        while self.pending.len() >= BLOCK {
            let block: Vec<i16> = self.pending.drain(..BLOCK).collect();
            let at = self.next.unwrap_or(first);
            self.next = Some(at + BLOCK as u64);
            if let Some(clip) = self.block(at, &block) {
                done.push(clip);
            }
        }
        done
    }

    fn block(&mut self, at: u64, block: &[i16]) -> Option<CutClip> {
        self.stats.frames += 1;
        let level = level_db(block);
        let floor = *self.stats.floor_db.get_or_insert(level);
        if let Some(open) = &mut self.open {
            open.pcm.extend_from_slice(block);
            if level >= floor + KEEP_ABOVE_FLOOR_DB {
                open.loud_blocks += 1;
                open.quiet_run = 0;
            } else {
                open.quiet_run += 1;
            }
            if open.pcm.len() + BLOCK > samples_of(MAX_CLIP) {
                self.stats.truncated += 1;
                return self.close();
            }
            if open.quiet_run >= blocks_of(HANGOVER) {
                return self.close();
            }
            return None;
        }
        if level >= floor + OPEN_ABOVE_FLOOR_DB {
            let preroll = samples_of(PREROLL).min(self.history.len());
            let mut pcm = self.history[self.history.len() - preroll..].to_vec();
            pcm.extend_from_slice(block);
            self.open = Some(OpenClip {
                start: at - preroll as u64,
                pcm,
                loud_blocks: 1,
                quiet_run: 0,
            });
            self.history.clear();
            return None;
        }
        // The floor follows the quiet room: down at once, up slowly.
        let floor = if level < floor {
            level
        } else {
            floor + (level - floor) * 0.05
        };
        self.stats.floor_db = Some(floor.max(FLOOR_MIN_DB));
        self.history.extend_from_slice(block);
        let keep = samples_of(PREROLL);
        if self.history.len() > keep {
            self.history.drain(..self.history.len() - keep);
        }
        None
    }

    fn close(&mut self) -> Option<CutClip> {
        let open = self.open.take()?;
        if open.loud_blocks < blocks_of(MIN_SPEECH) {
            self.stats.dropped_short += 1;
            return None;
        }
        Some(CutClip {
            start: open.start,
            pcm: open.pcm,
        })
    }

    /// Forgets the open clip and the timeline (a mute, a gap).
    fn discard(&mut self) {
        self.open = None;
        self.history.clear();
        self.pending.clear();
        self.next = None;
    }

    /// The open clip at the end of the recording, when it is worth keeping.
    fn finish(&mut self) -> Option<CutClip> {
        let clip = self.close();
        self.discard();
        clip
    }

    fn live(&self) -> CollectLive {
        CollectLive {
            floor_db: self.stats.floor_db,
            open_db: self.stats.floor_db.map(|f| f + OPEN_ABOVE_FLOOR_DB),
            frames: self.stats.frames,
            dropped_short: self.stats.dropped_short,
            truncated: self.stats.truncated,
        }
    }
}

// ─── Ambient writer ─────────────────────────────────────────────────────────────────

/// An ambient file being written: `<unix ms>.wav.part`.
struct AmbientFile {
    name: ClipName,
    part: PathBuf,
    file: File,
    start: u64,
    samples: usize,
}

impl AmbientFile {
    fn create(dir: &Path, start: u64) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let mut ms = wall_ms();
        loop {
            let name = ClipName::new(&format!("{ms}.wav")).expect("digits name a clip");
            let part = dir.join(format!("{}.part", name.as_str()));
            if !dir.join(name.as_str()).exists() && !part.exists() {
                let mut file = File::create(&part)?;
                file.write_all(&wav_bytes(&[]))?;
                return Ok(Self {
                    name,
                    part,
                    file,
                    start,
                    samples: 0,
                });
            }
            ms += 1;
        }
    }

    fn append(&mut self, pcm: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.samples += pcm.len();
        Ok(())
    }

    /// Completes the header, writes the sidecar, and renames the file into place.
    fn finish(mut self, dir: &Path, afe: Option<&AfeTrack>) -> io::Result<ClipName> {
        let header = wav_bytes(&vec![0; self.samples]);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&header[..44])?;
        self.file.sync_all()?;
        drop(self.file);
        if let Some(afe) = afe {
            let series = afe.series(self.start, self.samples).to_string();
            write_atomically(dir, &self.name.sidecar(), series.as_bytes())?;
        }
        fs::rename(&self.part, dir.join(self.name.as_str()))?;
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
        Ok(self.name)
    }

    fn discard(self) -> io::Result<()> {
        drop(self.file);
        remove_if_present(&self.part).map(drop)
    }
}

// ─── The hub ────────────────────────────────────────────────────────────────────────

/// Live counters the dashboard shows.
#[derive(Clone, Debug, Default)]
struct LiveState {
    collect: Option<CollectLive>,
    collect_clips: u64,
    collect_last_ms: Option<u64>,
    ambient_ms: Option<u64>,
    ambient_session: u32,
}

/// Every device's recording modes and live recorders. Cheap to share.
pub struct RecordingHub {
    db: DbHandle,
    root: PathBuf,
    modes: Mutex<HashMap<DeviceId, Modes>>,
    /// Held through a mode edit and while a deleted device's modes are dropped.
    edits: tokio::sync::Mutex<()>,
    live: Mutex<HashMap<DeviceId, LiveState>>,
    /// Recorders still running, per device (a recorder outlives its sender while it
    /// saves what it holds).
    running: Mutex<HashMap<DeviceId, usize>>,
    /// Wakes [`RecordingHub::delete_device`] when a recorder stops.
    recorder_stopped: tokio::sync::Notify,
    directory: watch::Receiver<Directory>,
    publisher: Arc<OnceLock<EventPublisher>>,
    tasks: TaskSupervisor,
}

/// Counts one running recorder of a device until it is dropped, panic or not.
struct RunningRecorder {
    hub: Arc<RecordingHub>,
    device: DeviceId,
}

impl RunningRecorder {
    fn new(hub: Arc<RecordingHub>, device: DeviceId) -> Self {
        *hub.running
            .lock()
            .expect("running lock")
            .entry(device.clone())
            .or_default() += 1;
        Self { hub, device }
    }
}

impl Drop for RunningRecorder {
    fn drop(&mut self) {
        let mut running = self.hub.running.lock().expect("running lock");
        if let Some(count) = running.get_mut(&self.device) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                running.remove(&self.device);
            }
        }
        drop(running);
        self.hub.recorder_stopped.notify_waiters();
    }
}

impl RecordingHub {
    /// The hub over the stored modes of every approved device and the runtimes'
    /// `directory`.
    pub async fn load(
        db: DbHandle,
        layout: &RecordingsLayout,
        publisher: Arc<OnceLock<EventPublisher>>,
        directory: watch::Receiver<Directory>,
        tasks: TaskSupervisor,
    ) -> Result<Arc<Self>, String> {
        let modes = db
            .read(ReadModes)
            .await
            .map_err(|error| format!("reading the recording modes: {error}"))?;
        Ok(Arc::new(Self {
            db,
            root: layout.root().to_path_buf(),
            modes: Mutex::new(modes.into_iter().collect()),
            edits: tokio::sync::Mutex::new(()),
            live: Mutex::default(),
            running: Mutex::default(),
            recorder_stopped: tokio::sync::Notify::new(),
            directory,
            publisher,
            tasks,
        }))
    }

    /// A device's armed modes.
    pub fn modes(&self, device: &DeviceId) -> Modes {
        self.modes
            .lock()
            .expect("modes lock")
            .get(device)
            .copied()
            .unwrap_or_default()
    }

    fn publish(&self, device: &DeviceId, patch: Map<String, Value>) {
        if let Some(publisher) = self.publisher.get() {
            publisher.publish(&ApiEvent::DeviceUpdate {
                device_id: device.clone(),
                state: DeviceStatePatch(patch),
            });
        }
    }

    fn live_patch(&self, device: &DeviceId) -> Map<String, Value> {
        let live = self
            .live
            .lock()
            .expect("live lock")
            .get(device)
            .cloned()
            .unwrap_or_default();
        let modes = self.modes(device);
        Map::from_iter([
            (
                "collectClips".to_owned(),
                json!(live.collect.is_some().then_some(live.collect_clips)),
            ),
            ("collectLastMs".to_owned(), json!(live.collect_last_ms)),
            (
                "ambientMs".to_owned(),
                json!(modes.ambient.then_some(live.ambient_ms).flatten()),
            ),
        ])
    }

    fn update_live(&self, device: &DeviceId, change: impl FnOnce(&mut LiveState)) {
        change(
            self.live
                .lock()
                .expect("live lock")
                .entry(device.clone())
                .or_default(),
        );
    }

    /// A recorder for a session of `device` when a mode is armed: the runtime arms the
    /// voice path's diagnostic subscription with the returned sender.
    pub fn sink(
        self: &Arc<Self>,
        device: &DeviceId,
        session: SessionId,
        afe_metadata: bool,
    ) -> Option<mpsc::Sender<DiagnosticAudio>> {
        let modes = self.modes(device);
        if !modes.armed() || !file_safe(device) {
            return None;
        }
        let (sender, receiver) = mpsc::channel(RECORDER_QUEUE);
        let recorder = ModeRecorder {
            hub: Arc::clone(self),
            device: device.clone(),
            modes,
            afe_metadata,
            segmenter: Segmenter::default(),
            ambient: None,
            afe: AfeTrack::default(),
        };
        let hub = Arc::clone(self);
        let device = device.clone();
        let running = RunningRecorder::new(Arc::clone(self), device.clone());
        self.tasks.spawn("recording mode", async move {
            let ended = tokio::task::spawn_blocking(move || recorder.run(receiver)).await;
            drop(running);
            match ended {
                Ok(RecorderEnd::Overflowed) => {
                    tracing::warn!(%device, ?session, "the recording fell behind; reopening it");
                    hub.recording_changed(&device);
                }
                Ok(RecorderEnd::Ended) => {}
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            }
        });
        Some(sender)
    }

    /// Tells the device's runtime to arm or disarm according to the stored modes.
    fn recording_changed(&self, device: &DeviceId) {
        let handle = self.directory.borrow().get(device).cloned();
        if let Some(handle) = handle
            && let Err(error) = handle.recording_changed()
        {
            tracing::warn!(%device, %error, "the runtime did not take the recording change");
        }
    }

    async fn set_mode(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        enabled: bool,
    ) -> Result<(), ServiceError> {
        if !file_safe(&device) {
            return Err(ServiceError::Unsupported(
                "This device's id cannot name files".to_owned(),
            ));
        }
        // One edit at a time: the two modes exclude each other, so the check, the stored
        // row and the hub's copy must not interleave with another edit.
        let _edit = self.edits.lock().await;
        let mut modes = self.modes(&device);
        match mode {
            RecordingMode::Samples => {
                if enabled && modes.ambient {
                    return Err(ServiceError::Busy(
                        "Ambient recording is on; stop it first".to_owned(),
                    ));
                }
                modes.collect = enabled;
            }
            RecordingMode::Ambient => {
                if enabled && modes.collect {
                    return Err(ServiceError::Busy(
                        "Sample collection is on; stop it first".to_owned(),
                    ));
                }
                modes.ambient = enabled;
            }
        }
        let stored = self
            .db
            .write(WriteModes {
                device: device.clone(),
                modes,
            })
            .await
            .map_err(|error| {
                ServiceError::Failed(format!("storing the recording mode: {error}"))
            })?;
        if !stored {
            return Err(ServiceError::UnknownDevice);
        }
        self.modes
            .lock()
            .expect("modes lock")
            .insert(device.clone(), modes);
        let mut patch = Map::from_iter([
            ("collectMode".to_owned(), json!(modes.collect)),
            ("ambientMode".to_owned(), json!(modes.ambient)),
        ]);
        patch.extend(self.live_patch(&device));
        self.publish(&device, patch);
        self.recording_changed(&device);
        Ok(())
    }

    fn dir(&self, mode: RecordingMode, device: &DeviceId) -> Result<PathBuf, ServiceError> {
        if !file_safe(device) {
            return Err(ServiceError::NotFound("No such recording".to_owned()));
        }
        Ok(mode_dir(&self.root, mode, device))
    }

    /// Deletes a deleted device's recording modes, once its runtime has stopped: waits (at
    /// most [`FORGET_WAIT`]) for its recorders to save or discard what they hold, so no file
    /// is written after its files go, drops its modes and counters, then removes its
    /// sample and ambient directories. Mode edits wait until the returned hold is dropped:
    /// keep it until the device's row is deleted, so an edit queued meanwhile finds no
    /// device instead of arming modes again.
    pub async fn delete_device(&self, device: &DeviceId) -> io::Result<ModeEditsHeld<'_>> {
        let deadline = tokio::time::Instant::now() + FORGET_WAIT;
        loop {
            let stopped = self.recorder_stopped.notified();
            tokio::pin!(stopped);
            stopped.as_mut().enable();
            if !self
                .running
                .lock()
                .expect("running lock")
                .contains_key(device)
            {
                break;
            }
            if tokio::time::timeout_at(deadline, stopped).await.is_err() {
                tracing::warn!(%device, "a deleted device's recorder did not stop in time");
                break;
            }
        }
        let held = ModeEditsHeld {
            _edits: self.edits.lock().await,
        };
        self.modes.lock().expect("modes lock").remove(device);
        self.live.lock().expect("live lock").remove(device);
        if !file_safe(device) {
            // Its id cannot name a directory, so no recorder ever wrote one.
            return Ok(held);
        }
        let dirs = [RecordingMode::Samples, RecordingMode::Ambient]
            .map(|mode| mode_dir(&self.root, mode, device));
        tokio::task::spawn_blocking(move || {
            for dir in dirs {
                match fs::remove_dir_all(dir) {
                    Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                    _ => {}
                }
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)??;
        Ok(held)
    }
}

/// Recording-mode edits held back while a device's deletion finishes; dropping it lets
/// them run again.
#[must_use = "mode edits resume as soon as this is dropped"]
pub struct ModeEditsHeld<'a> {
    _edits: tokio::sync::MutexGuard<'a, ()>,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, ServiceError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ServiceError::Failed("a file task failed".to_owned()))?
        .map_err(|error| ServiceError::Failed(error.to_string()))
}

/// [`RecordingModes`] over the hub.
pub struct RecordingRoutes(pub Arc<RecordingHub>);

impl RecordingModes for RecordingRoutes {
    fn set_mode(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        enabled: bool,
    ) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(self.0.set_mode(device, mode, enabled))
    }

    fn samples(&self, device: DeviceId) -> BoxFuture<'_, Result<SamplesListing, ServiceError>> {
        Box::pin(async move {
            let dir = self.0.dir(RecordingMode::Samples, &device)?;
            let clips = blocking(move || list_clips(&dir)).await?;
            let live = self
                .0
                .live
                .lock()
                .expect("live lock")
                .get(&device)
                .and_then(|l| l.collect.clone());
            Ok(SamplesListing {
                count: clips.len(),
                bytes: clips.iter().map(|(_, c)| c.bytes).sum(),
                ms: clips.iter().map(|(_, c)| c.ms).sum(),
                keep: SAMPLES_KEPT,
                clips: clips.into_iter().map(|(_, c)| c).collect(),
                live,
            })
        })
    }

    fn ambient(&self, device: DeviceId) -> BoxFuture<'_, Result<AmbientListing, ServiceError>> {
        Box::pin(async move {
            let dir = self.0.dir(RecordingMode::Ambient, &device)?;
            let clips = blocking(move || list_clips(&dir)).await?;
            let live = self
                .0
                .live
                .lock()
                .expect("live lock")
                .get(&device)
                .cloned()
                .unwrap_or_default();
            Ok(AmbientListing {
                count: clips.len(),
                bytes: clips.iter().map(|(_, c)| c.bytes).sum(),
                keep: AMBIENT_KEPT,
                max_ms: AMBIENT_ROLL.as_millis() as u64,
                clips: clips.into_iter().map(|(_, c)| c).collect(),
                live: live.ambient_ms.map(|ms| AmbientLive { ms }),
                session: live.ambient_ms.map(|_| live.ambient_session),
            })
        })
    }

    fn file(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        name: ClipName,
        part: ClipPart,
    ) -> BoxFuture<'_, Result<PathBuf, ServiceError>> {
        Box::pin(async move {
            let dir = self.0.dir(mode, &device)?;
            let path = match part {
                ClipPart::Audio => dir.join(name.as_str()),
                ClipPart::Afe => dir.join(name.sidecar()),
            };
            if tokio::fs::metadata(&path).await.is_err() {
                return Err(ServiceError::NotFound("No such recording".to_owned()));
            }
            Ok(path)
        })
    }

    fn delete(
        &self,
        device: DeviceId,
        mode: RecordingMode,
        name: Option<ClipName>,
    ) -> BoxFuture<'_, Result<usize, ServiceError>> {
        Box::pin(async move {
            let dir = self.0.dir(mode, &device)?;
            let removed = blocking(move || match name {
                Some(name) => remove_clip(&dir, &name).map(usize::from),
                None => retain(&dir, 0),
            })
            .await?;
            if removed == 0 {
                return Err(ServiceError::NotFound("No such recording".to_owned()));
            }
            Ok(removed)
        })
    }

    fn archive_files(
        &self,
        device: DeviceId,
        mode: RecordingMode,
    ) -> BoxFuture<'_, Result<Vec<ArchiveFile>, ServiceError>> {
        Box::pin(async move {
            let dir = self.0.dir(mode, &device)?;
            blocking(move || {
                let mut files = Vec::new();
                for (name, _) in list_clips(&dir)? {
                    let unix_secs = (name.unix_ms().unwrap_or(0) / 1_000) as i64;
                    let sidecar = name.sidecar();
                    if dir.join(&sidecar).exists() {
                        files.push(ArchiveFile {
                            path: dir.join(&sidecar),
                            name: sidecar,
                            unix_secs,
                        });
                    }
                    files.push(ArchiveFile {
                        name: name.as_str().to_owned(),
                        path: dir.join(name.as_str()),
                        unix_secs,
                    });
                }
                Ok(files)
            })
            .await
        })
    }

    fn fields(&self, device: &DeviceId) -> Map<String, Value> {
        self.0.live_patch(device)
    }
}

// ─── One session's recorder ─────────────────────────────────────────────────────────

/// How a recorder stopped.
enum RecorderEnd {
    /// The subscription ended (or the runtime disarmed it).
    Ended,
    /// Its queue overflowed and the runtime dropped it; the open file was discarded.
    Overflowed,
}

/// Writes one session's recording-mode audio. Runs on a blocking thread.
struct ModeRecorder {
    hub: Arc<RecordingHub>,
    device: DeviceId,
    modes: Modes,
    afe_metadata: bool,
    segmenter: Segmenter,
    ambient: Option<AmbientFile>,
    afe: AfeTrack,
}

impl ModeRecorder {
    /// Takes the armed mode's audio across the session's subscriptions. Each end saves
    /// (or, on a mute, discards) what that subscription recorded; the recorder stops when
    /// the runtime drops its sender. A sender dropped in the middle of a subscription
    /// means the queue overflowed: the open file is discarded.
    fn run(mut self, mut audio: mpsc::Receiver<DiagnosticAudio>) -> RecorderEnd {
        self.started();
        let mut mid_subscription = false;
        let end = loop {
            match audio.blocking_recv() {
                Some(DiagnosticAudio::Chunk(chunk)) => {
                    mid_subscription = true;
                    match (chunk.stream, chunk.body) {
                        (
                            StreamId::Mic,
                            ChunkBody::Data {
                                data: ChunkData::Pcm(pcm),
                                ..
                            },
                        ) => {
                            let samples: Vec<i16> = pcm.samples().collect();
                            self.mic(chunk.span.start, &samples);
                        }
                        (StreamId::Mic, ChunkBody::Missing(_)) => {
                            // Nothing will fill it: the recording keeps its timeline with
                            // silence, and no clip straddles it.
                            let len = chunk.span.end.saturating_sub(chunk.span.start) as usize;
                            self.segmenter.discard();
                            self.ambient_append(&vec![0; len]);
                        }
                        (
                            StreamId::Afe,
                            ChunkBody::Data {
                                data: ChunkData::Afe(records),
                                ..
                            },
                        ) => self.afe.add(chunk.span.start, &records),
                        _ => {}
                    }
                }
                Some(DiagnosticAudio::Ended(EndCause::Muted)) => {
                    mid_subscription = false;
                    self.discard();
                }
                Some(DiagnosticAudio::Ended(_)) => {
                    mid_subscription = false;
                    self.save();
                }
                None if mid_subscription => {
                    self.discard();
                    break RecorderEnd::Overflowed;
                }
                None => break RecorderEnd::Ended,
            }
        };
        self.stopped();
        end
    }

    fn dir(&self, mode: RecordingMode) -> PathBuf {
        mode_dir(&self.hub.root, mode, &self.device)
    }

    fn started(&mut self) {
        let collect = self.modes.collect;
        self.hub.update_live(&self.device, |live| {
            if collect {
                live.collect = Some(CollectLive {
                    floor_db: None,
                    open_db: None,
                    frames: 0,
                    dropped_short: 0,
                    truncated: 0,
                });
            }
        });
        self.publish();
    }

    fn stopped(&mut self) {
        self.hub.update_live(&self.device, |live| {
            live.collect = None;
            live.ambient_ms = None;
        });
        self.publish();
    }

    fn publish(&self) {
        self.hub
            .publish(&self.device, self.hub.live_patch(&self.device));
    }

    fn mic(&mut self, first: u64, samples: &[i16]) {
        if self.modes.collect {
            for clip in self.segmenter.push(first, samples) {
                self.save_clip(clip);
            }
            let live = self.segmenter.live();
            self.hub
                .update_live(&self.device, |state| state.collect = Some(live));
        }
        if self.modes.ambient {
            if self.ambient.is_none() {
                let dir = self.dir(RecordingMode::Ambient);
                match AmbientFile::create(&dir, first) {
                    Ok(file) => self.ambient = Some(file),
                    Err(error) => {
                        tracing::warn!(device = %self.device, %error, "an ambient file could not be created");
                        return;
                    }
                }
            }
            self.ambient_append(samples);
        }
        // AFE records are kept only for what an open file may still need.
        let keep_from = match (&self.ambient, &self.segmenter.open) {
            (Some(file), _) => file.start,
            (None, Some(open)) => open.start,
            (None, None) => first.saturating_sub(samples_of(PREROLL) as u64),
        };
        self.afe.forget_before(keep_from);
    }

    fn ambient_append(&mut self, samples: &[i16]) {
        let Some(file) = self.ambient.as_mut() else {
            return;
        };
        if let Err(error) = file.append(samples) {
            tracing::warn!(device = %self.device, %error, "an ambient file could not be written");
            if let Some(file) = self.ambient.take() {
                let _ = file.discard();
            }
            return;
        }
        let ms = file.samples as u64 * 1_000 / RATE;
        let roll = file.samples >= samples_of(AMBIENT_ROLL);
        self.hub
            .update_live(&self.device, |live| live.ambient_ms = Some(ms));
        if roll {
            self.finish_ambient();
        }
    }

    fn finish_ambient(&mut self) {
        let Some(file) = self.ambient.take() else {
            return;
        };
        let dir = self.dir(RecordingMode::Ambient);
        let afe = self.afe_metadata.then_some(&self.afe);
        match file.finish(&dir, afe) {
            Ok(name) => {
                tracing::info!(device = %self.device, file = name.as_str(), "ambient file saved");
                if let Err(error) = retain(&dir, AMBIENT_KEPT) {
                    tracing::warn!(device = %self.device, %error, "ambient retention failed");
                }
                self.hub.update_live(&self.device, |live| {
                    live.ambient_session += 1;
                    live.ambient_ms = Some(0);
                });
                self.publish();
            }
            Err(error) => {
                tracing::warn!(device = %self.device, %error, "an ambient file could not be saved");
            }
        }
    }

    fn save_clip(&mut self, clip: CutClip) {
        let dir = self.dir(RecordingMode::Samples);
        let len = clip.pcm.len();
        let sidecar = self
            .afe_metadata
            .then(|| self.afe.series(clip.start, len).to_string());
        let lag_ms = (self
            .segmenter
            .next
            .unwrap_or(clip.start)
            .saturating_sub(clip.start))
            * 1_000
            / RATE;
        let mut ms = wall_ms().saturating_sub(lag_ms);
        let written = loop {
            let name = ClipName::new(&format!("{ms}.wav")).expect("digits name a clip");
            if dir.join(name.as_str()).exists() {
                ms += 1;
                continue;
            }
            let result = (|| -> io::Result<()> {
                if let Some(sidecar) = &sidecar {
                    write_atomically(&dir, &name.sidecar(), sidecar.as_bytes())?;
                }
                write_atomically(&dir, name.as_str(), &wav_bytes(&clip.pcm))?;
                Ok(())
            })();
            break result;
        };
        match written {
            Ok(()) => {
                if let Err(error) = retain(&dir, SAMPLES_KEPT) {
                    tracing::warn!(device = %self.device, %error, "sample retention failed");
                }
                let clip_ms = len as u64 * 1_000 / RATE;
                self.hub.update_live(&self.device, |live| {
                    live.collect_clips += 1;
                    live.collect_last_ms = Some(clip_ms);
                });
                self.publish();
            }
            Err(error) => {
                tracing::warn!(device = %self.device, %error, "a sample clip could not be saved");
            }
        }
    }

    /// The subscription ended for a reason other than a mute: what was recorded is
    /// saved.
    fn save(&mut self) {
        if let Some(clip) = self.segmenter.finish() {
            self.save_clip(clip);
        }
        self.finish_ambient();
    }

    /// A mute or an overflow: nothing unsaved is kept.
    fn discard(&mut self) {
        self.segmenter.discard();
        if let Some(file) = self.ambient.take()
            && let Err(error) = file.discard()
        {
            tracing::warn!(device = %self.device, %error, "an ambient part file could not be removed");
        }
        self.afe = AfeTrack::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(len: usize, amplitude: i16) -> Vec<i16> {
        (0..len)
            .map(|i| if i % 16 < 8 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn a_word_in_a_quiet_room_becomes_one_clip() {
        let mut segmenter = Segmenter::default();
        let mut clips = Vec::new();
        let mut at = 0u64;
        let mut feed = |segmenter: &mut Segmenter, pcm: Vec<i16>| {
            let out = segmenter.push(at, &pcm);
            at += pcm.len() as u64;
            out
        };
        clips.extend(feed(&mut segmenter, tone(16_000, 30)));
        clips.extend(feed(&mut segmenter, tone(8_000, 8_000)));
        clips.extend(feed(&mut segmenter, tone(16_000, 30)));
        assert_eq!(clips.len(), 1);
        let ms = clips[0].pcm.len() as u64 * 1_000 / RATE;
        assert!((800..=1_300).contains(&ms), "{ms} ms");
        // The loud block holding sample 16,000 opens it, with the preroll before it.
        assert_eq!(
            clips[0].start,
            16_000 / BLOCK as u64 * BLOCK as u64 - samples_of(PREROLL) as u64
        );
    }

    #[test]
    fn short_clicks_are_dropped_and_long_noise_is_cut() {
        let mut segmenter = Segmenter::default();
        let mut at = 0u64;
        let mut clips = Vec::new();
        for pcm in [
            tone(16_000, 30),
            tone(BLOCK / 2, 8_000),
            tone(16_000, 30),
            tone(16_000 * 5, 8_000),
        ] {
            clips.extend(segmenter.push(at, &pcm));
            at += pcm.len() as u64;
        }
        assert_eq!(segmenter.stats.dropped_short, 1);
        assert!(segmenter.stats.truncated >= 1);
        assert!(clips.iter().all(|c| c.pcm.len() <= samples_of(MAX_CLIP)));
    }

    #[test]
    fn the_afe_series_takes_the_record_holding_each_period_middle() {
        let mut track = AfeTrack::default();
        let record = AfeRecord {
            frames: 10,
            rms_max: 175,
            rms_mean: 172,
            volume: 70,
            ..AfeRecord::default()
        };
        track.add(100_000, &[record, record]);
        let series = track.series(100_640, 3 * BLOCK);
        assert_eq!(series["recording"], json!([0, 3 * BLOCK]));
        assert_eq!(series["frames"], json!([10, null, null]));
        assert_eq!(series["rms_max_db"][0], json!(-81));
        assert_eq!(series["erle_max"][0], Value::Null);
        assert_eq!(series["period"], json!(BLOCK));
    }

    #[test]
    fn ambient_files_are_parts_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = AmbientFile::create(dir.path(), 0).unwrap();
        file.append(&tone(32_000, 100)).unwrap();
        assert!(
            list_clips(dir.path()).unwrap().is_empty(),
            "a part is not listed"
        );
        let name = file.finish(dir.path(), None).unwrap();
        let clips = list_clips(dir.path()).unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].1.ms, 2_000);
        assert_eq!(clips[0].0, name);
        let bytes = fs::read(dir.path().join(name.as_str())).unwrap();
        assert_eq!(&bytes[40..44], &(64_000u32).to_le_bytes());
    }
}
