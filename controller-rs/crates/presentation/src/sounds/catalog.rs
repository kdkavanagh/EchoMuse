//! The catalog itself: what is uploaded, what each upload exported to, and how a speaker's
//! choice resolves to the sound an alert rings. [`SoundCatalog`] blocks on the file system
//! and `ffmpeg`; [`CatalogSnapshot`] is the immutable view everyone else reads.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use echomuse_protocol::Sha256;
use echomuse_protocol::alerts::AlertSound;
use echomuse_settings::settings::TimerSettings;
use echomuse_settings::values::SoundChoice;

use super::export::{self, ExportError};
use super::{
    DEFAULT_SOUND_ID, Entry, ExportRecord, MAX_UPLOAD_BYTES, PRIVATE_MODE, SoundId, SoundsDir,
    UPLOAD_MODE, UploadFormat, classify, remove_if_present, upload_stamp, write_atomic,
};

/// Where a sound's export stands.
#[derive(Clone, Debug, PartialEq)]
pub enum ExportState {
    /// The asset exists.
    Ready {
        /// Its hash.
        sha256: Sha256,
        /// Its length in seconds.
        seconds: f64,
        /// The upload was cut to 10 s.
        shortened: bool,
    },
    /// The export failed; why.
    Failed(String),
    /// Not exported yet, or the upload changed since.
    Pending,
}

/// One sound as the dashboard lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct SoundSummary {
    /// The catalog id.
    pub id: SoundId,
    /// Its export.
    pub state: ExportState,
}

impl SoundSummary {
    /// The asset's hash, when exported.
    pub fn sha256(&self) -> Option<&Sha256> {
        match &self.state {
            ExportState::Ready { sha256, .. } => Some(sha256),
            _ => None,
        }
    }

    /// The length in seconds, when exported; the dashboard shows "duration unavailable"
    /// otherwise.
    pub fn seconds(&self) -> Option<f64> {
        match self.state {
            ExportState::Ready { seconds, .. } => Some(seconds),
            _ => None,
        }
    }

    /// Whether it was cut to 10 s.
    pub fn shortened(&self) -> bool {
        matches!(
            self.state,
            ExportState::Ready {
                shortened: true,
                ..
            }
        )
    }
}

/// A choice resolved to what rings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolved {
    /// The sound the alert names.
    pub sound: AlertSound,
    /// The choice names a sound the catalog cannot ring, so the built-in fallback rings
    /// instead; the dashboard says so beside the picker.
    pub unresolved: bool,
}

/// A speaker's effective alert sounds, for the alerts service's endpoint configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EndpointSounds {
    /// New alarms and alarms made in Home Assistant's calendar.
    pub alarm: Resolved,
    /// Finished timers.
    pub timer: Resolved,
}

/// The catalog at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CatalogSnapshot {
    sounds: BTreeMap<SoundId, SoundSummary>,
}

impl CatalogSnapshot {
    /// Every sound, by id.
    pub fn sounds(&self) -> impl Iterator<Item = &SoundSummary> {
        self.sounds.values()
    }

    /// One sound.
    pub fn get(&self, id: &SoundId) -> Option<&SoundSummary> {
        self.sounds.get(id)
    }

    /// What `choice` rings. A named sound that is missing or not exported rings the
    /// built-in fallback and is unresolved. The default choice rings the catalog's
    /// `default` sound when there is one, otherwise the built-in fallback; a `default`
    /// upload that cannot ring is unresolved.
    pub fn resolve(&self, choice: &SoundChoice) -> Resolved {
        let (id, required) = match choice {
            SoundChoice::Default => (SoundId::new(DEFAULT_SOUND_ID), false),
            SoundChoice::Named(name) => (SoundId::new(name), true),
        };
        let entry = id.as_ref().and_then(|id| self.sounds.get(id));
        match entry.and_then(SoundSummary::sha256) {
            Some(sha) => Resolved {
                sound: AlertSound::Asset(sha.clone()),
                unresolved: false,
            },
            None => Resolved {
                sound: AlertSound::Fallback,
                unresolved: required || entry.is_some(),
            },
        }
    }

    /// A speaker's timer and alarm sounds.
    pub fn endpoint_sounds(&self, timers: &TimerSettings) -> EndpointSounds {
        EndpointSounds {
            alarm: self.resolve(&timers.alarm_sound.0),
            timer: self.resolve(&timers.timer_sound.0),
        }
    }
}

/// Why an upload was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UploadError {
    /// Empty, or larger than [`MAX_UPLOAD_BYTES`].
    Size(usize),
    /// It could not be turned into a sound; the upload was not kept.
    Export(ExportError),
    /// It could not be stored.
    Io(String),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Size(0) => f.write_str("the file is empty"),
            Self::Size(n) => write!(f, "the file is {n} bytes; sounds may be at most 10 MiB"),
            Self::Export(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "the sound could not be stored: {e}"),
        }
    }
}

impl std::error::Error for UploadError {}

/// Why a deletion was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeleteError {
    /// No such sound.
    NotFound,
    /// The fleet or a device selects it.
    InUse,
    /// A file could not be removed.
    Io(String),
}

impl fmt::Display for DeleteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such sound"),
            Self::InUse => f.write_str("Sound is still in use"),
            Self::Io(e) => write!(f, "the sound could not be deleted: {e}"),
        }
    }
}

impl std::error::Error for DeleteError {}

#[derive(Clone, Debug)]
struct Upload {
    format: UploadFormat,
    state: ExportState,
}

/// The catalog's owner. Every method blocks on the file system, and exports run `ffmpeg`.
#[derive(Debug)]
pub struct SoundCatalog {
    dir: SoundsDir,
    ffmpeg: PathBuf,
    uploads: BTreeMap<SoundId, Upload>,
}

impl SoundCatalog {
    /// Reads the directory, creating it when missing. Nothing is exported yet: an upload
    /// whose record is missing, stale or names a missing asset is [`ExportState::Pending`]
    /// until [`SoundCatalog::export`].
    pub fn open(dir: SoundsDir, ffmpeg: PathBuf) -> io::Result<Self> {
        dir.create()?;
        let mut found: BTreeMap<SoundId, UploadFormat> = BTreeMap::new();
        for entry in fs::read_dir(dir.root())? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Entry::Upload(id, format) = classify(&entry.file_name().to_string_lossy()) {
                found
                    .entry(id)
                    .and_modify(|f| *f = (*f).min(format))
                    .or_insert(format);
            }
        }
        let uploads = found
            .into_iter()
            .map(|(id, format)| {
                let state = recorded_state(&dir, &id, format);
                (id, Upload { format, state })
            })
            .collect();
        Ok(Self {
            dir,
            ffmpeg,
            uploads,
        })
    }

    /// The directory.
    pub fn dir(&self) -> &SoundsDir {
        &self.dir
    }

    /// The catalog now.
    pub fn snapshot(&self) -> CatalogSnapshot {
        let sounds = self
            .uploads
            .iter()
            .map(|(id, upload)| {
                let summary = SoundSummary {
                    id: id.clone(),
                    state: upload.state.clone(),
                };
                (id.clone(), summary)
            })
            .collect();
        CatalogSnapshot { sounds }
    }

    /// The ids still waiting for an export.
    pub fn pending(&self) -> Vec<SoundId> {
        self.uploads
            .iter()
            .filter(|(_, u)| u.state == ExportState::Pending)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Exports one pending upload, writing its asset (unless one with that hash already
    /// exists) and its record. A failed decode is recorded so it is not retried until the
    /// upload changes; an interrupted one or a file error is not recorded and stays
    /// pending until the next start.
    pub fn export(&mut self, id: &SoundId, stop: &AtomicBool) -> Result<(), ExportError> {
        let Some(upload) = self.uploads.get(id) else {
            return Ok(());
        };
        let path = self.dir.upload_path(id, upload.format);
        let state = export_upload(&self.dir, &self.ffmpeg, id, &path, stop)?;
        if let Some(upload) = self.uploads.get_mut(id) {
            upload.state = state;
        }
        Ok(())
    }

    /// Stores an upload under `id`, replacing any upload of that id, and exports it. An
    /// upload that cannot be decoded is removed again and refused.
    pub fn upload(
        &mut self,
        id: SoundId,
        format: UploadFormat,
        bytes: &[u8],
        stop: &AtomicBool,
    ) -> Result<SoundSummary, UploadError> {
        if bytes.is_empty() || bytes.len() > MAX_UPLOAD_BYTES {
            return Err(UploadError::Size(bytes.len()));
        }
        let io = |e: io::Error| UploadError::Io(e.to_string());
        let root = self.dir.root().to_owned();
        write_atomic(
            &root,
            &SoundsDir::upload_name(&id, format),
            bytes,
            UPLOAD_MODE,
        )
        .map_err(io)?;
        self.remove_files(&id, Some(format)).map_err(io)?;
        let path = self.dir.upload_path(&id, format);
        let stamp = upload_stamp(&path).map_err(io)?;
        let state = match store_asset(&self.dir, &self.ffmpeg, &path, stop) {
            Ok(state) => state,
            Err(e) if e.is_permanent() => {
                self.remove_files(&id, None).map_err(io)?;
                self.uploads.remove(&id);
                return Err(UploadError::Export(e));
            }
            Err(e) => {
                let pending = Upload {
                    format,
                    state: ExportState::Pending,
                };
                self.uploads.insert(id, pending);
                return Err(UploadError::Export(e));
            }
        };
        let upload = Upload {
            format,
            state: state.clone(),
        };
        self.uploads.insert(id.clone(), upload);
        write_record(&self.dir, &id, stamp, &state).map_err(io)?;
        Ok(SoundSummary { id, state })
    }

    /// Deletes a sound: its upload, its record and the older decode cache. Its asset stays,
    /// since alarms already made may name it. `selected` is every stored `timerSound` and
    /// `alarmSound`, fleet and per device; a sound one of them names is refused.
    pub fn delete(&mut self, id: &SoundId, selected: &[SoundChoice]) -> Result<(), DeleteError> {
        if !self.uploads.contains_key(id) {
            return Err(DeleteError::NotFound);
        }
        let named = |c: &SoundChoice| matches!(c, SoundChoice::Named(n) if n == id.as_str());
        if selected.iter().any(named) {
            return Err(DeleteError::InUse);
        }
        self.remove_files(id, None)
            .map_err(|e| DeleteError::Io(e.to_string()))?;
        self.uploads.remove(id);
        Ok(())
    }

    /// Removes an id's uploads in every format but `keep`, its record and the older
    /// decode cache.
    fn remove_files(&self, id: &SoundId, keep: Option<UploadFormat>) -> io::Result<()> {
        for format in UploadFormat::ALL {
            if Some(format) != keep {
                remove_if_present(&self.dir.upload_path(id, format))?;
            }
        }
        for path in self.dir.derived_files(id) {
            remove_if_present(&path)?;
        }
        Ok(())
    }
}

/// The state an upload's record says, when the record is current.
fn recorded_state(dir: &SoundsDir, id: &SoundId, format: UploadFormat) -> ExportState {
    let current = || -> Option<ExportState> {
        let stamp = upload_stamp(&dir.upload_path(id, format)).ok()?;
        let text = fs::read_to_string(dir.root().join(SoundsDir::record_name(id))).ok()?;
        let record = ExportRecord::parse(&text)?;
        if record.stamp != stamp {
            return None;
        }
        if let Some(error) = record.error {
            return Some(ExportState::Failed(error));
        }
        let sha256 = record.sha256?;
        if !dir.asset_path(&sha256).is_file() {
            return None;
        }
        Some(ExportState::Ready {
            sha256,
            seconds: record.seconds?,
            shortened: record.shortened,
        })
    };
    current().unwrap_or(ExportState::Pending)
}

/// Decodes and exports one upload and writes its record. A decode that fails for good is
/// recorded as failed; other failures write nothing.
fn export_upload(
    dir: &SoundsDir,
    ffmpeg: &Path,
    id: &SoundId,
    path: &Path,
    stop: &AtomicBool,
) -> Result<ExportState, ExportError> {
    let io = |e: io::Error| ExportError::Io(e.to_string());
    let stamp = upload_stamp(path).map_err(io)?;
    let state = match store_asset(dir, ffmpeg, path, stop) {
        Ok(state) => state,
        Err(e) if e.is_permanent() => ExportState::Failed(e.to_string()),
        Err(e) => return Err(e),
    };
    write_record(dir, id, stamp, &state).map_err(io)?;
    Ok(state)
}

/// Decodes an upload and stores its asset, unless an asset with that hash already exists,
/// which is left exactly as it is.
fn store_asset(
    dir: &SoundsDir,
    ffmpeg: &Path,
    path: &Path,
    stop: &AtomicBool,
) -> Result<ExportState, ExportError> {
    let asset = export::finish(export::decode(ffmpeg, path, dir.root(), stop)?)?;
    if !dir.asset_path(&asset.sha256).is_file() {
        let name = format!("{}.wav", asset.sha256);
        write_atomic(&dir.assets(), &name, &asset.wav, PRIVATE_MODE)
            .map_err(|e| ExportError::Io(e.to_string()))?;
    }
    Ok(ExportState::Ready {
        seconds: asset.seconds(),
        shortened: asset.shortened,
        sha256: asset.sha256,
    })
}

/// Writes the record of an export made from the upload stamped `stamp`.
fn write_record(
    dir: &SoundsDir,
    id: &SoundId,
    stamp: String,
    state: &ExportState,
) -> io::Result<()> {
    let record = match state {
        ExportState::Ready {
            sha256,
            seconds,
            shortened,
        } => ExportRecord {
            stamp,
            sha256: Some(sha256.clone()),
            seconds: Some(*seconds),
            shortened: *shortened,
            error: None,
        },
        ExportState::Failed(error) => ExportRecord {
            stamp,
            sha256: None,
            seconds: None,
            shortened: false,
            error: Some(error.clone()),
        },
        ExportState::Pending => return Ok(()),
    };
    let name = SoundsDir::record_name(id);
    write_atomic(dir.root(), &name, record.to_text().as_bytes(), PRIVATE_MODE)
}

#[cfg(test)]
mod tests;
