//! The alert sound catalog (spec §3.5.2, §4.9; `docs/configuration.md` "Sounds").
//!
//! Users upload sounds by name; each speaker chooses a timer sound and an alarm sound from
//! them. The device rings an alert sound from its own cache, so every upload is exported
//! as a fixed asset: 48 kHz mono PCM16 WAV, cut to its first 10 s, with a 50 ms fade-out,
//! named by the SHA-256 of the whole file. Calendar events name assets by hash and may
//! outlive the upload, so assets are never deleted.
//!
//! The files beside the database, kept exactly as they are:
//!
//! ```text
//! sounds/<id>.<mp3|wav|flac|ogg|m4a>   the upload, named after the sound
//! sounds/<id>.alert.json               its export record (below)
//! sounds/assets/<sha256>.wav           exported assets, never deleted
//! sounds/<id>.pcm, <id>.pcm.stamp      an older decode cache: ignored, removed with its upload
//! ```
//!
//! An export record is one line of JSON in this key order and layout:
//! `{"stamp": "<upload file name>:<mtime ns>:<size>", "sha256": "<hex>"|null,
//! "seconds": 2.0|null, "shortened": false, "error": null|"<why>"}`. A record whose stamp
//! no longer matches its upload, or whose asset is missing, is exported again.
//!
//! [`catalog::SoundCatalog`] does the blocking work (directory scans, `ffmpeg`, file
//! writes); [`service::SoundService`] runs it on its own thread and publishes
//! [`catalog::CatalogSnapshot`]s, which resolve a speaker's choices to the asset hashes
//! the alerts service sends.

pub mod catalog;
pub mod export;
pub mod service;

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use echomuse_protocol::Sha256;
use echomuse_settings::values::SoundChoice;
use serde_json::Value;

/// The catalog id the "Default" choice rings when it exists.
pub const DEFAULT_SOUND_ID: &str = "default";
/// The largest upload accepted.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

const RECORD_SUFFIX: &str = ".alert.json";
const LEGACY_PCM_SUFFIX: &str = ".pcm";
const LEGACY_STAMP_SUFFIX: &str = ".pcm.stamp";
/// Records and assets are private to the controller, as the existing ones are.
const PRIVATE_MODE: u32 = 0o600;
/// Uploads are readable, as the existing ones are.
const UPLOAD_MODE: u32 = 0o644;

/// A sound's catalog id: the name it was uploaded under, and the value stored in
/// `timerSound` and `alarmSound`. Letters, digits, `_`, `.` and `-`, at most 64, not
/// starting with `.`.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SoundId(String);

impl SoundId {
    /// The id, when `text` is a valid one.
    pub fn new(text: &str) -> Option<Self> {
        let named = matches!(SoundChoice::new(text), Ok(SoundChoice::Named(_)));
        (named && !text.starts_with('.')).then(|| Self(text.to_owned()))
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SoundId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An upload's container, from its file name's extension.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum UploadFormat {
    /// `.mp3`
    Mp3,
    /// `.wav`
    Wav,
    /// `.flac`
    Flac,
    /// `.ogg`
    Ogg,
    /// `.m4a`
    M4a,
}

impl UploadFormat {
    /// Every format, in the order a duplicate upload is chosen.
    const ALL: [Self; 5] = [Self::Mp3, Self::Wav, Self::Flac, Self::Ogg, Self::M4a];

    /// The extension, lowercase, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Ogg => "ogg",
            Self::M4a => "m4a",
        }
    }

    /// The format of an extension, in any case.
    fn from_extension(extension: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|f| f.extension().eq_ignore_ascii_case(extension))
    }

    /// The format of an uploaded file's name (`chime.MP3`).
    pub fn from_file_name(name: &str) -> Option<Self> {
        let (_, extension) = name.rsplit_once('.')?;
        Self::from_extension(extension)
    }
}

/// The `sounds/` directory beside the database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SoundsDir {
    root: PathBuf,
}

impl SoundsDir {
    /// `sounds/` under the directory that holds the database.
    pub fn beside_database(database_dir: &Path) -> Self {
        Self::at(database_dir.join("sounds"))
    }

    /// A sounds directory at `root`.
    fn at(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory.
    fn root(&self) -> &Path {
        &self.root
    }

    /// The assets directory.
    fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }

    /// An exported asset's file name: its hash.
    fn asset_name(sha256: &Sha256) -> String {
        format!("{sha256}.wav")
    }

    /// Where an exported asset lives; the asset socket serves it under its hash.
    pub fn asset_path(&self, sha256: &Sha256) -> PathBuf {
        self.assets().join(Self::asset_name(sha256))
    }

    /// An upload's file name.
    fn upload_name(id: &SoundId, format: UploadFormat) -> String {
        format!("{id}.{}", format.extension())
    }

    /// An upload's path.
    fn upload_path(&self, id: &SoundId, format: UploadFormat) -> PathBuf {
        self.root.join(Self::upload_name(id, format))
    }

    /// An export record's file name.
    fn record_name(id: &SoundId) -> String {
        format!("{id}{RECORD_SUFFIX}")
    }

    /// An export record's path.
    fn record_path(&self, id: &SoundId) -> PathBuf {
        self.root.join(Self::record_name(id))
    }

    /// The files an upload's id owns besides the upload: its record and the older
    /// decode cache.
    fn derived_files(&self, id: &SoundId) -> [PathBuf; 3] {
        [
            self.record_path(id),
            self.root.join(format!("{id}{LEGACY_PCM_SUFFIX}")),
            self.root.join(format!("{id}{LEGACY_STAMP_SUFFIX}")),
        ]
    }

    /// Creates the directory and its assets directory.
    fn create(&self) -> io::Result<()> {
        fs::create_dir_all(self.assets())
    }
}

/// What a directory entry's name says it is.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Entry {
    Upload(SoundId, UploadFormat),
    Record(SoundId),
    Other,
}

fn classify(name: &str) -> Entry {
    if name.starts_with('.') {
        return Entry::Other;
    }
    if let Some(stem) = name.strip_suffix(RECORD_SUFFIX) {
        return SoundId::new(stem).map_or(Entry::Other, Entry::Record);
    }
    if name.ends_with(LEGACY_PCM_SUFFIX) || name.ends_with(LEGACY_STAMP_SUFFIX) {
        return Entry::Other;
    }
    match name.rsplit_once('.') {
        Some((stem, extension)) => {
            match (SoundId::new(stem), UploadFormat::from_extension(extension)) {
                (Some(id), Some(format)) => Entry::Upload(id, format),
                _ => Entry::Other,
            }
        }
        None => Entry::Other,
    }
}

/// An upload's stamp: its file name, modification time in ns and size. An export record
/// holds the stamp of the upload it was made from.
fn upload_stamp(path: &Path) -> io::Result<String> {
    let meta = fs::metadata(path)?;
    let mtime_ns = meta
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(format!("{name}:{mtime_ns}:{}", meta.len()))
}

/// An `.alert.json` export record.
#[derive(Clone, Debug, PartialEq)]
struct ExportRecord {
    /// The stamp of the upload it was made from.
    pub stamp: String,
    /// The asset's hash; `None` when the export failed.
    pub sha256: Option<Sha256>,
    /// The asset's length in seconds, to 0.01 s; `None` when the export failed.
    pub seconds: Option<f64>,
    /// The upload was longer than 10 s and was cut.
    pub shortened: bool,
    /// Why the export failed.
    pub error: Option<String>,
}

impl ExportRecord {
    /// Reads a record; `None` when it is not one.
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let stamp = object.get("stamp")?.as_str()?.to_owned();
        let sha256 = match object.get("sha256") {
            None | Some(Value::Null) => None,
            Some(v) => Some(Sha256::parse(v.as_str()?)?),
        };
        let seconds = match object.get("seconds") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_f64()?),
        };
        let shortened = match object.get("shortened") {
            None | Some(Value::Null) => false,
            Some(v) => v.as_bool()?,
        };
        let error = match object.get("error") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_str()?.to_owned()),
        };
        Some(Self {
            stamp,
            sha256,
            seconds,
            shortened,
            error,
        })
    }

    /// The record's text, laid out as the existing records are.
    fn to_text(&self) -> String {
        let string = |s: &str| serde_json::to_string(s).expect("strings serialize");
        let sha = self
            .sha256
            .as_ref()
            .map_or("null".to_owned(), |s| string(s.as_str()));
        // `{:?}` writes a float's shortest round-trip digits with a `.0` when whole (`2.0`).
        let seconds = self.seconds.map_or("null".to_owned(), |s| format!("{s:?}"));
        let error = self.error.as_deref().map_or("null".to_owned(), string);
        format!(
            "{{\"stamp\": {}, \"sha256\": {sha}, \"seconds\": {seconds}, \"shortened\": {}, \"error\": {error}}}",
            string(&self.stamp),
            self.shortened,
        )
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `dir/name` through a hidden temporary file and a rename, so a reader
/// sees the old file or the whole new one, never part of it.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8], mode: u32) -> io::Result<()> {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(".{name}.{}.{n}.tmp", std::process::id()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(name))?;
        File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written
}

/// Removes `path`; a file already gone is not an error.
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_text_matches_the_existing_layout() {
        let real = r#"{"stamp": "system_alerts_24_legacy_short.wav:1789940065842172679:384132", "sha256": "8459a6b79da84019897763ca1eae81c9867c13e85fa24ec6e938ee5fab839a3f", "seconds": 2.0, "shortened": false, "error": null}"#;
        let record = ExportRecord::parse(real).unwrap();
        assert_eq!(record.seconds, Some(2.0));
        assert!(!record.shortened);
        assert_eq!(record.to_text(), real);

        let failed = ExportRecord {
            stamp: "x.mp3:1:2".into(),
            sha256: None,
            seconds: None,
            shortened: false,
            error: Some("ffmpeg: \"bad\"".into()),
        };
        assert_eq!(
            failed.to_text(),
            r#"{"stamp": "x.mp3:1:2", "sha256": null, "seconds": null, "shortened": false, "error": "ffmpeg: \"bad\""}"#
        );
        assert_eq!(ExportRecord::parse(&failed.to_text()), Some(failed));
        assert_eq!(ExportRecord::parse("{}"), None);
        assert_eq!(
            ExportRecord::parse(r#"{"stamp": "a", "sha256": "nothex"}"#),
            None
        );
    }

    #[test]
    fn names_are_sorted_into_uploads_records_and_the_rest() {
        let id = |s: &str| SoundId::new(s).unwrap();
        assert_eq!(
            classify("system_alerts_24_legacy_short.wav"),
            Entry::Upload(id("system_alerts_24_legacy_short"), UploadFormat::Wav)
        );
        assert_eq!(
            classify("Chime.v2.MP3"),
            Entry::Upload(id("Chime.v2"), UploadFormat::Mp3)
        );
        assert_eq!(classify("chime.alert.json"), Entry::Record(id("chime")));
        assert_eq!(classify("chime.pcm"), Entry::Other);
        assert_eq!(classify("chime.pcm.stamp"), Entry::Other);
        assert_eq!(classify(".chime.wav.1.0.tmp"), Entry::Other);
        assert_eq!(classify("notes.txt"), Entry::Other);
        assert_eq!(classify("has space.wav"), Entry::Other);
        assert_eq!(SoundId::new(".hidden"), None);
        assert_eq!(SoundId::new(""), None);
        assert_eq!(SoundId::new(&"a".repeat(65)), None);
        assert_eq!(
            UploadFormat::from_file_name("song.M4A"),
            Some(UploadFormat::M4a)
        );
        assert_eq!(UploadFormat::from_file_name("song.aiff"), None);
    }
}
