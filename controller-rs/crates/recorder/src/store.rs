//! Where recordings live: one configured directory, one subdirectory per device, one file per
//! recording, all readable by the controller's user only, each deleted when its retention ends
//! or on request.
//!
//! ```text
//! <directory>/                      0700
//!   <device>/                       0700  (the device id, with unsafe bytes escaped)
//!     <recording id>.emrec.part     0600  while it is written, or after a crash
//!     <recording id>.emrec          0600  once it ended in order
//! ```
//!
//! Deletion coordinates with recordings still being written: the writer is told first, stops
//! and removes its own file, and cannot recreate it afterwards.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use echomuse_core::types::DeviceId;

use crate::grant::{UnixMillis, lock};
use crate::queue::{Queue, QueueBounds};
use crate::reader::read_header;
use crate::record::{Header, RecordingId};

/// A finished recording's file extension.
pub const EXTENSION: &str = "emrec";
/// The extension of a recording still being written, or cut short.
pub const PART_EXTENSION: &str = "emrec.part";
/// How long recordings are kept unless configured otherwise.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The recorder's configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecorderConfig {
    /// The directory recordings are written under. It is created `0700` if missing, and
    /// tightened to `0700` if it exists with wider permissions.
    pub directory: PathBuf,
    /// How long each recording is kept after it starts.
    pub retention: Duration,
    /// The write queue's bounds per recording.
    pub queue: QueueBounds,
}

impl RecorderConfig {
    /// The defaults under `directory`.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            retention: DEFAULT_RETENTION,
            queue: QueueBounds::default(),
        }
    }
}

#[derive(Debug)]
struct Active {
    device: String,
    queue: Weak<Queue>,
}

#[derive(Debug)]
pub(crate) struct StoreShared {
    pub(crate) config: RecorderConfig,
    active: Mutex<HashMap<RecordingId, Active>>,
}

/// One recording on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRecording {
    /// Its file.
    pub path: PathBuf,
    /// Its header, or `None` when the file has none readable.
    pub header: Option<Header>,
    /// Whether it ended in order (`.emrec`) rather than being written or cut short
    /// (`.emrec.part`).
    pub finished: bool,
    /// Whether a writer is still writing it.
    pub writing: bool,
    /// Its size in bytes.
    pub bytes: u64,
}

/// The recordings directory. Cheap to clone; every clone is the same store.
#[derive(Clone, Debug)]
pub struct RecordingStore {
    pub(crate) shared: Arc<StoreShared>,
}

/// Creates `dir` as `0700`, or tightens an existing one.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let perms = fs::metadata(dir)?.permissions();
    if perms.mode() & 0o077 != 0 {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The device's directory name: ASCII letters, digits, `-` and `_` as they are, every other
/// byte as `%XX`, so any id is one safe path component.
pub(crate) fn device_dir_name(device: &str) -> String {
    let mut out = String::with_capacity(device.len());
    for b in device.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.is_empty() {
        out.push('%');
    }
    out
}

impl RecordingStore {
    /// Opens (creating) the directory.
    pub fn open(config: RecorderConfig) -> io::Result<Self> {
        private_dir(&config.directory)?;
        Ok(Self {
            shared: Arc::new(StoreShared {
                config,
                active: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &RecorderConfig {
        &self.shared.config
    }

    /// The directory a device's recordings live in.
    pub fn device_dir(&self, device: &DeviceId) -> PathBuf {
        self.shared
            .config
            .directory
            .join(device_dir_name(&device.0))
    }

    pub(crate) fn paths(&self, device: &str, id: &RecordingId) -> (PathBuf, PathBuf) {
        let dir = self.shared.config.directory.join(device_dir_name(device));
        (
            dir.join(format!("{}.{PART_EXTENSION}", id.as_str())),
            dir.join(format!("{}.{EXTENSION}", id.as_str())),
        )
    }

    pub(crate) fn register(&self, id: &RecordingId, device: &str, queue: &Arc<Queue>) {
        lock(&self.shared.active).insert(
            id.clone(),
            Active {
                device: device.to_owned(),
                queue: Arc::downgrade(queue),
            },
        );
    }

    pub(crate) fn unregister(&self, id: &RecordingId) {
        lock(&self.shared.active).remove(id);
    }

    fn writing(&self, id: &RecordingId) -> Option<Arc<Queue>> {
        lock(&self.shared.active)
            .get(id)
            .and_then(|a| a.queue.upgrade())
    }

    /// Every recording on disk, unordered.
    pub fn list(&self) -> io::Result<Vec<StoredRecording>> {
        let mut out = Vec::new();
        let root = &self.shared.config.directory;
        for dev in fs::read_dir(root)? {
            let dev = dev?;
            if !dev.file_type()?.is_dir() {
                continue;
            }
            for f in fs::read_dir(dev.path())? {
                let f = f?;
                let path = f.path();
                let name = f.file_name();
                let Some(name) = name.to_str() else { continue };
                let (stem, finished) =
                    if let Some(s) = name.strip_suffix(&format!(".{PART_EXTENSION}")) {
                        (s, false)
                    } else if let Some(s) = name.strip_suffix(&format!(".{EXTENSION}")) {
                        (s, true)
                    } else {
                        continue;
                    };
                let writing = RecordingId::parse(stem)
                    .and_then(|id| self.writing(&id))
                    .is_some();
                out.push(StoredRecording {
                    header: read_header(&path).ok(),
                    bytes: f.metadata()?.len(),
                    path,
                    finished,
                    writing,
                });
            }
        }
        Ok(out)
    }

    /// Deletes one recording, stopping its writer first if it is still being written. Returns
    /// whether anything was deleted.
    pub fn delete(&self, id: &RecordingId) -> io::Result<bool> {
        let device = lock(&self.shared.active).get(id).map(|a| a.device.clone());
        if let Some(q) = self.writing(id) {
            q.abort();
        }
        let mut deleted = false;
        let devices: Vec<String> = match device {
            Some(d) => vec![device_dir_name(&d)],
            None => self.device_dirs()?,
        };
        for d in devices {
            let dir = self.shared.config.directory.join(d);
            for name in [
                format!("{}.{PART_EXTENSION}", id.as_str()),
                format!("{}.{EXTENSION}", id.as_str()),
            ] {
                deleted |= remove_if_present(&dir.join(name))?;
            }
        }
        Ok(deleted)
    }

    fn device_dirs(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.shared.config.directory)? {
            let e = e?;
            if e.file_type()?.is_dir()
                && let Some(n) = e.file_name().to_str()
            {
                out.push(n.to_owned());
            }
        }
        Ok(out)
    }

    /// Deletes every recording of a device, stopping its writers first. Returns how many files
    /// were removed.
    pub fn delete_device(&self, device: &DeviceId) -> io::Result<usize> {
        let writing: Vec<Arc<Queue>> = lock(&self.shared.active)
            .values()
            .filter(|a| a.device == device.0)
            .filter_map(|a| a.queue.upgrade())
            .collect();
        for q in writing {
            q.abort();
        }
        let dir = self.device_dir(device);
        let mut n = 0;
        match fs::read_dir(&dir) {
            Ok(entries) => {
                for e in entries {
                    let path = e?.path();
                    if remove_if_present(&path)? {
                        n += 1;
                    }
                }
                match fs::remove_dir(&dir) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(n)
    }

    /// Deletes every recording whose retention has ended by `now`. A file without a readable
    /// header is judged by its modification time. A recording still being written past its
    /// retention is stopped and deleted too. Returns the deleted files.
    pub fn sweep(&self, now: SystemTime) -> io::Result<Vec<PathBuf>> {
        let now_ms = UnixMillis::of(now);
        let retention = self.shared.config.retention;
        let mut deleted = Vec::new();
        for r in self.list()? {
            let due = match &r.header {
                Some(h) => h.retain_until() <= now_ms,
                None => fs::metadata(&r.path)
                    .and_then(|m| m.modified())
                    .map(|m| UnixMillis::of(m).after(retention) <= now_ms)
                    .unwrap_or(false),
            };
            if !due {
                continue;
            }
            if let Some(h) = &r.header
                && let Some(q) = self.writing(&h.recording)
            {
                q.abort();
            }
            if remove_if_present(&r.path)? {
                deleted.push(r.path);
            }
        }
        Ok(deleted)
    }
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
