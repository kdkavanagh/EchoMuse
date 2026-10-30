//! Recording files beside the database (spec §3.5.2) and the commands that tie them to rows.
//!
//! # Names
//!
//! | Kind | File | Row | Saved when |
//! |---|---|---|---|
//! | [`RecordingKind::Utterance`] | `recordings/<device>_<id>.wav` | `turns` | `audio_file` = `<device>_<id>.wav` |
//! | [`RecordingKind::TurnRecording`] | `turn_recordings/<device>_<id>.wav` | `turns` | the file exists (no column) |
//! | [`RecordingKind::WakeClip`] | `wakes/<device>/<id>.wav` | `turns` | `wake_file` = `<id>.wav` |
//! | [`RecordingKind::NearMiss`] | `nearmiss_recordings/<device>_<id>.wav` | `near_misses` | `recording` = `saved` |
//!
//! Each is a 44-byte-header RIFF WAV of 16 kHz mono PCM16, as the existing files are. A
//! file is first written as `.<name>.<nonce>.tmp` in its final directory, synced, renamed
//! into place, and the directory synced; only then is its row marked saved.
//!
//! # Retention (spec §4.11, configuration.md "Save utterances" and "Save wake clips")
//!
//! Per device: the newest 300 utterance recordings, 100 turn recordings, 1,000 wake clips and
//! 1,000 near-miss recordings, the last counting only unmarked near misses; a marked near
//! miss keeps its recording. Retention removes files and leaves the rows' file columns as
//! they are: a saved file that is gone reads as "no longer kept", as it does today.

use std::{
    collections::HashSet,
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use echomuse_core::types::DeviceId;
use echomuse_db::{ReadQuery, WriteCommand};
use rusqlite::{Connection, Transaction, params};

use crate::{error::RecordError, values::SAMPLE_RATE_HZ};

/// Utterance recordings kept per device.
pub const UTTERANCE_RETENTION: u32 = 300;
/// Turn recordings kept per device.
pub const TURN_RECORDING_RETENTION: u32 = 100;
/// Wake clips kept per device.
pub const WAKE_CLIP_RETENTION: u32 = 1_000;
/// Recordings of unmarked near misses kept per device.
pub const NEAR_MISS_RECORDING_RETENTION: u32 = 1_000;

/// The length of the WAV header every recording starts with.
pub const WAV_HEADER_BYTES: usize = 44;

/// A recording named by a row id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecordingKind {
    /// What STT received: `recordings/`.
    Utterance,
    /// The mic from before the wake through the answer: `turn_recordings/`.
    TurnRecording,
    /// The wake word: `wakes/<device>/`.
    WakeClip,
    /// A near miss: `nearmiss_recordings/`.
    NearMiss,
}

impl RecordingKind {
    /// Every kind.
    pub const ALL: [Self; 4] = [
        Self::Utterance,
        Self::TurnRecording,
        Self::WakeClip,
        Self::NearMiss,
    ];

    /// The directory beside the database.
    pub const fn directory(self) -> &'static str {
        match self {
            Self::Utterance => "recordings",
            Self::TurnRecording => "turn_recordings",
            Self::WakeClip => "wakes",
            Self::NearMiss => "nearmiss_recordings",
        }
    }

    const fn per_device_directory(self) -> bool {
        matches!(self, Self::WakeClip)
    }
}

/// Whether a device id can name files: non-empty ASCII letters, digits, `-` and `_`. Every
/// real device serial is letters and digits.
pub fn file_safe(device: &DeviceId) -> bool {
    !device.0.is_empty()
        && device
            .0
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Where the files beside the database live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingsLayout {
    root: PathBuf,
}

impl RecordingsLayout {
    /// Files under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Files in the database's directory; a bare file name puts them in the working
    /// directory.
    pub fn beside_database(db_path: &Path) -> Self {
        match db_path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => Self::new(parent),
            _ => Self::new("."),
        }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding a device's files of `kind`.
    pub fn directory(&self, kind: RecordingKind, device: &DeviceId) -> PathBuf {
        let dir = self.root.join(kind.directory());
        if kind.per_device_directory() {
            dir.join(&device.0)
        } else {
            dir
        }
    }

    /// The file name of row `id`'s recording, as the file columns store it.
    pub fn file_name(kind: RecordingKind, device: &DeviceId, id: i64) -> String {
        if kind.per_device_directory() {
            format!("{id}.wav")
        } else {
            format!("{}_{id}.wav", device.0)
        }
    }

    /// The path of row `id`'s recording.
    pub fn path(&self, kind: RecordingKind, device: &DeviceId, id: i64) -> PathBuf {
        self.directory(kind, device)
            .join(Self::file_name(kind, device, id))
    }
}

/// What a directory entry is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EntryName {
    /// A recording: its device and row id.
    Recording { device: DeviceId, id: i64 },
    /// A temporary file this crate writes before a rename.
    Temporary,
    /// A recording an earlier controller was still writing (`<name>.wav.part`): left
    /// alone at startup, deleted with its device.
    LegacyPartial { device: DeviceId },
    /// Anything else, which is never touched.
    Unrecognized,
}

/// Reads a file name in a `kind` directory. For a wake clip, `dir_device` is the directory's
/// device.
pub fn parse_entry(kind: RecordingKind, name: &str, dir_device: Option<&DeviceId>) -> EntryName {
    if name.starts_with('.') && name.ends_with(".tmp") && name.contains(".wav.") {
        return EntryName::Temporary;
    }
    let (stem, partial) = match name.strip_suffix(".wav.part") {
        Some(stem) => (stem, true),
        None => match name.strip_suffix(".wav") {
            Some(stem) => (stem, false),
            None => return EntryName::Unrecognized,
        },
    };
    let (device, id) = match (kind.per_device_directory(), dir_device) {
        (true, Some(device)) => (device.clone(), stem),
        (true, None) => return EntryName::Unrecognized,
        (false, _) => match stem.rsplit_once('_') {
            Some((device, id)) => (DeviceId(device.to_owned()), id),
            None => return EntryName::Unrecognized,
        },
    };
    let numeric = !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit());
    match id.parse::<i64>() {
        Ok(_) if partial && numeric && file_safe(&device) => EntryName::LegacyPartial { device },
        Ok(id) if numeric && id > 0 && file_safe(&device) => EntryName::Recording { device, id },
        _ => EntryName::Unrecognized,
    }
}

/// The bytes of a 16 kHz mono PCM16 WAV of `pcm`.
pub fn wav_bytes(pcm: &[i16]) -> Vec<u8> {
    let data_len = u32::try_from(pcm.len() * 2).expect("a recording under 4 GiB");
    let mut out = Vec::with_capacity(WAV_HEADER_BYTES + pcm.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&SAMPLE_RATE_HZ.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE_HZ * 2).to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in pcm {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// The sample count of a complete 16 kHz mono PCM16 WAV in this layout; None for any other
/// file, including one cut short.
pub fn wav_samples(path: &Path) -> io::Result<Option<u64>> {
    let mut file = File::open(path)?;
    let mut header = [0u8; WAV_HEADER_BYTES];
    if file.read_exact(&mut header).is_err() {
        return Ok(None);
    }
    let len = file.metadata()?.len();
    let u16_at = |i: usize| u16::from_le_bytes([header[i], header[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(header[i..i + 4].try_into().expect("4 bytes"));
    let data_len = u64::from(u32_at(40));
    let valid = &header[0..4] == b"RIFF"
        && &header[8..16] == b"WAVEfmt "
        && u32_at(16) == 16
        && u16_at(20) == 1
        && u16_at(22) == 1
        && u32_at(24) == SAMPLE_RATE_HZ
        && u16_at(34) == 16
        && &header[36..40] == b"data"
        && data_len % 2 == 0
        && len == WAV_HEADER_BYTES as u64 + data_len;
    Ok(valid.then_some(data_len / 2))
}

/// The length of `samples` samples at 16 kHz, rounded down to a millisecond.
pub fn samples_duration(samples: u64) -> Duration {
    Duration::from_millis(samples * 1_000 / u64::from(SAMPLE_RATE_HZ))
}

static NONCE: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `dir/name` so that the name never holds a partial file: a temporary
/// file in the same directory, synced, renamed over the name, and the directory synced.
/// Blocking: run it off the async runtime.
pub fn write_atomically(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{name}.{}-{nonce}.tmp", std::process::id()));
    let target = dir.join(name);
    let written = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)?;
        File::open(dir)?.sync_all()
    })();
    if let Err(error) = written {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(target)
}

/// Removes a file; one already gone counts as removed.
pub(crate) fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// One entry found in a recordings directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Found {
    pub path: PathBuf,
    pub name: EntryName,
}

/// Lists a kind's files: all devices, or one. Missing directories are empty.
pub(crate) fn scan(
    layout: &RecordingsLayout,
    kind: RecordingKind,
    only: Option<&DeviceId>,
) -> io::Result<Vec<Found>> {
    let mut found = Vec::new();
    let mut list = |dir: &Path, dir_device: Option<&DeviceId>| -> io::Result<()> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let Some(text) = file_name.to_str() else {
                found.push(Found {
                    path: entry.path(),
                    name: EntryName::Unrecognized,
                });
                continue;
            };
            let name = parse_entry(kind, text, dir_device);
            let mine = match (&name, only) {
                (_, None) => true,
                (
                    EntryName::Recording { device, .. } | EntryName::LegacyPartial { device },
                    Some(d),
                ) => device == d,
                (EntryName::Temporary, Some(d)) => {
                    dir_device.is_some() || text.starts_with(&format!(".{}_", d.0))
                }
                (EntryName::Unrecognized, Some(_)) => false,
            };
            if mine {
                found.push(Found {
                    path: entry.path(),
                    name,
                });
            }
        }
        Ok(())
    };
    if kind.per_device_directory() {
        match only {
            Some(device) => list(&layout.directory(kind, device), Some(device))?,
            None => {
                let base = layout.root().join(kind.directory());
                let devices = match fs::read_dir(&base) {
                    Ok(entries) => entries,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(found),
                    Err(e) => return Err(e),
                };
                for entry in devices {
                    let entry = entry?;
                    if !entry.file_type()?.is_dir() {
                        continue;
                    }
                    let device = entry.file_name().to_str().map(|d| DeviceId(d.to_owned()));
                    match device.filter(file_safe) {
                        Some(device) => list(&entry.path(), Some(&device))?,
                        None => continue,
                    }
                }
            }
        }
    } else {
        list(&layout.root().join(kind.directory()), None)?;
    }
    Ok(found)
}

/// Records a turn file as saved: `audio_file` or `wake_file`. False when the row is gone.
#[derive(Clone, Debug)]
pub struct MarkTurnFileSaved {
    /// The row's device.
    pub device: DeviceId,
    /// The row.
    pub id: i64,
    /// [`RecordingKind::Utterance`] or [`RecordingKind::WakeClip`].
    pub kind: RecordingKind,
}

impl WriteCommand for MarkTurnFileSaved {
    type Output = bool;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, RecordError> {
        let column = match self.kind {
            RecordingKind::Utterance => "audio_file",
            RecordingKind::WakeClip => "wake_file",
            _ => {
                return Err(RecordError::Invalid(
                    "only utterances and wake clips have a column",
                ));
            }
        };
        let name = RecordingsLayout::file_name(self.kind, &self.device, self.id);
        Ok(tx.execute(
            &format!("UPDATE turns SET {column} = ?3 WHERE id = ?1 AND device_id = ?2"),
            params![self.id, self.device.0, name],
        )? > 0)
    }
}

/// A row's recording state, for reconciling a file with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowState {
    /// No row of this device has the id.
    Missing,
    /// The row says its file is saved (or, for a turn recording, the row exists).
    Saved,
    /// The row's file is not marked saved: it may adopt a complete file.
    Unsaved,
    /// The row says it was never recorded (`off`, `not_permitted`).
    NotRecorded,
}

/// The recording state of one row of a kind.
#[derive(Clone, Debug)]
pub struct GetRowState {
    /// The kind.
    pub kind: RecordingKind,
    /// The device the file names.
    pub device: DeviceId,
    /// The row id the file names.
    pub id: i64,
}

impl ReadQuery for GetRowState {
    type Output = RowState;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<RowState, RecordError> {
        row_state(conn, self.kind, &self.device, self.id)
    }
}

fn row_state(
    conn: &Connection,
    kind: RecordingKind,
    device: &DeviceId,
    id: i64,
) -> Result<RowState, RecordError> {
    use rusqlite::OptionalExtension;
    let (table, column) = match kind {
        RecordingKind::Utterance => ("turns", "audio_file"),
        RecordingKind::WakeClip => ("turns", "wake_file"),
        RecordingKind::TurnRecording => ("turns", "id"),
        RecordingKind::NearMiss => ("near_misses", "recording"),
    };
    let value: Option<Option<rusqlite::types::Value>> = conn
        .query_row(
            &format!("SELECT {column} FROM {table} WHERE id = ?1 AND device_id = ?2"),
            params![id, device.0],
            |r| r.get(0),
        )
        .optional()?;
    use rusqlite::types::Value;
    Ok(match (kind, value) {
        (_, None) => RowState::Missing,
        (RecordingKind::TurnRecording, Some(_)) => RowState::Saved,
        (RecordingKind::NearMiss, Some(Some(Value::Text(text)))) => match text.as_str() {
            "saved" => RowState::Saved,
            "failed" => RowState::Unsaved,
            _ => RowState::NotRecorded,
        },
        (RecordingKind::NearMiss, Some(_)) => {
            return Err(RecordError::BadStoredValue { table, column, id });
        }
        (_, Some(None)) => RowState::Unsaved,
        (_, Some(Some(_))) => RowState::Saved,
    })
}

/// Marks an unsaved row saved for a complete file found at startup: the controller stopped
/// after renaming the file into place and before marking its row. False when the row is no
/// longer unsaved.
#[derive(Clone, Debug)]
pub struct AdoptRecording {
    /// The kind (not [`RecordingKind::TurnRecording`], which has no mark).
    pub kind: RecordingKind,
    /// The device.
    pub device: DeviceId,
    /// The row.
    pub id: i64,
    /// The file's sample count.
    pub samples: u64,
}

impl WriteCommand for AdoptRecording {
    type Output = bool;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, RecordError> {
        if row_state(tx, self.kind, &self.device, self.id)? != RowState::Unsaved {
            return Ok(false);
        }
        match self.kind {
            RecordingKind::NearMiss => crate::near_misses::MarkNearMissSaved {
                id: crate::near_misses::NearMissRowId(self.id),
                audio: samples_duration(self.samples),
            }
            .apply(tx),
            kind => MarkTurnFileSaved {
                device: self.device,
                id: self.id,
                kind,
            }
            .apply(tx),
        }
    }
}

/// The ids of a device's recordings retention keeps for one kind; every other file of the
/// kind for the device is removed. Not used for turn recordings, which are counted by file.
#[derive(Clone, Debug)]
pub struct RetainedIds {
    /// The kind.
    pub kind: RecordingKind,
    /// The device.
    pub device: DeviceId,
    /// How many to keep (marked near misses are kept on top).
    pub keep: u32,
}

impl ReadQuery for RetainedIds {
    type Output = HashSet<i64>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<HashSet<i64>, RecordError> {
        let sql = match self.kind {
            RecordingKind::Utterance => {
                "SELECT id FROM turns WHERE device_id = ?1 AND audio_file IS NOT NULL \
                 ORDER BY id DESC LIMIT ?2"
            }
            RecordingKind::WakeClip => {
                "SELECT id FROM turns WHERE device_id = ?1 AND wake_file IS NOT NULL \
                 ORDER BY id DESC LIMIT ?2"
            }
            RecordingKind::NearMiss => {
                "SELECT id FROM (SELECT id FROM near_misses WHERE device_id = ?1 \
                 AND recording = 'saved' AND verdict IS NULL ORDER BY id DESC LIMIT ?2) \
                 UNION SELECT id FROM near_misses WHERE device_id = ?1 \
                 AND recording = 'saved' AND verdict IS NOT NULL"
            }
            RecordingKind::TurnRecording => {
                return Err(RecordError::Invalid("turn recordings are counted by file"));
            }
        };
        let mut statement = conn.prepare(sql)?;
        let ids = statement
            .query_map(params![self.device.0, i64::from(self.keep)], |r| r.get(0))?
            .collect::<rusqlite::Result<HashSet<i64>>>()?;
        Ok(ids)
    }
}

/// The rows a device's records span, for deleting it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeletedRows {
    /// `turns` rows.
    pub turns: usize,
    /// `near_misses` rows.
    pub near_misses: usize,
    /// `device_logs` rows.
    pub device_logs: usize,
    /// `device_metrics` hours.
    pub device_metrics: usize,
    /// `wake_counters` hours.
    pub wake_counters: usize,
    /// `wake_shadow` rows.
    pub wake_shadow: usize,
    /// `wake_shadow_events` rows.
    pub wake_shadow_events: usize,
}

/// Deletes every record row of a device, in one transaction. Its `devices` row is not
/// touched: delete that afterwards (`device_logs` refers to it).
#[derive(Clone, Debug)]
pub struct DeleteDeviceRecords(pub DeviceId);

impl WriteCommand for DeleteDeviceRecords {
    type Output = DeletedRows;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<DeletedRows, RecordError> {
        let delete = |table: &str| {
            tx.execute(
                &format!("DELETE FROM {table} WHERE device_id = ?1"),
                [&self.0.0],
            )
        };
        Ok(DeletedRows {
            turns: delete("turns")?,
            near_misses: delete("near_misses")?,
            device_logs: delete("device_logs")?,
            device_metrics: delete("device_metrics")?,
            wake_counters: delete("wake_counters")?,
            wake_shadow: delete("wake_shadow")?,
            wake_shadow_events: delete("wake_shadow_events")?,
        })
    }
}

/// Every device that has a turn or near-miss row.
#[derive(Clone, Copy, Debug, Default)]
pub struct DevicesWithRecordings;

impl ReadQuery for DevicesWithRecordings {
    type Output = Vec<DeviceId>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<DeviceId>, RecordError> {
        let mut statement = conn.prepare(
            "SELECT device_id FROM turns UNION SELECT device_id FROM near_misses \
             ORDER BY device_id",
        )?;
        let devices = statement
            .query_map([], |r| r.get(0).map(DeviceId))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(devices)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str) -> DeviceId {
        DeviceId(id.to_owned())
    }

    #[test]
    fn names_follow_the_existing_layout() {
        let layout = RecordingsLayout::new("/data");
        let d = dev("G090LF1072461EEG");
        assert_eq!(
            layout.path(RecordingKind::Utterance, &d, 966),
            PathBuf::from("/data/recordings/G090LF1072461EEG_966.wav")
        );
        assert_eq!(
            layout.path(RecordingKind::TurnRecording, &d, 966),
            PathBuf::from("/data/turn_recordings/G090LF1072461EEG_966.wav")
        );
        assert_eq!(
            layout.path(RecordingKind::WakeClip, &d, 966),
            PathBuf::from("/data/wakes/G090LF1072461EEG/966.wav")
        );
        assert_eq!(
            layout.path(RecordingKind::NearMiss, &d, 562),
            PathBuf::from("/data/nearmiss_recordings/G090LF1072461EEG_562.wav")
        );
        assert_eq!(
            RecordingsLayout::beside_database(Path::new("/data/echomuse.db")),
            layout
        );
        assert_eq!(
            RecordingsLayout::beside_database(Path::new("echomuse.db")).root(),
            Path::new(".")
        );
    }

    #[test]
    fn entries_parse_by_kind() {
        let d = dev("G0A");
        assert_eq!(
            parse_entry(RecordingKind::Utterance, "G0A_12.wav", None),
            EntryName::Recording {
                device: d.clone(),
                id: 12
            }
        );
        assert_eq!(
            parse_entry(RecordingKind::WakeClip, "12.wav", Some(&d)),
            EntryName::Recording {
                device: d.clone(),
                id: 12
            }
        );
        assert_eq!(
            parse_entry(RecordingKind::Utterance, ".G0A_12.wav.7-1.tmp", None),
            EntryName::Temporary
        );
        assert_eq!(
            parse_entry(RecordingKind::NearMiss, "G0A_12.wav.part", None),
            EntryName::LegacyPartial { device: d.clone() }
        );
        for odd in [
            "G0A_12.mp3",
            "G0A_x.wav",
            "12.wav",
            "G0A_+1.wav",
            "G0A_0.wav",
            "G0A_x.wav.part",
            "G0A_12.part",
        ] {
            assert_eq!(
                parse_entry(RecordingKind::NearMiss, odd, None),
                EntryName::Unrecognized,
                "{odd}"
            );
        }
    }

    #[test]
    fn wav_round_trips_and_rejects_short_files() {
        let dir = tempfile::tempdir().unwrap();
        let pcm: Vec<i16> = (0..1_600).map(|i| i as i16 - 800).collect();
        let path = write_atomically(dir.path(), "a.wav", &wav_bytes(&pcm)).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(
            &bytes[20..36],
            &[1, 0, 1, 0, 0x80, 0x3e, 0, 0, 0, 0x7d, 0, 0, 2, 0, 16, 0]
        );
        assert_eq!(wav_samples(&path).unwrap(), Some(1_600));
        assert_eq!(samples_duration(1_600), Duration::from_millis(100));
        fs::write(&path, &bytes[..bytes.len() - 2]).unwrap();
        assert_eq!(wav_samples(&path).unwrap(), None);
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file stays behind");
    }
}
