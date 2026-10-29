//! The cutover backup, `<DB_PATH>.pre-rust.bak` (§3.5.1 rule 5): written through SQLite's
//! online backup API before this controller first writes to an existing installation,
//! published atomically, and never replaced once it exists.

use std::{
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use rusqlite::{
    Connection,
    backup::{Backup, StepResult},
};

const CUTOVER_SUFFIX: &str = ".pre-rust.bak";
/// Where an unfinished backup is written; it never carries the final name.
const PARTIAL_SUFFIX: &str = ".partial";
/// How often, and how long apart, a backup step that found the source locked is retried.
/// Nothing else should be writing at startup, so this only rides out a brief lock.
const LOCKED_RETRIES: u32 = 200;
const LOCKED_PAUSE: Duration = Duration::from_millis(50);

/// The outcome of making sure the cutover backup exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CutoverBackup {
    /// This start wrote the backup.
    Written(PathBuf),
    /// An earlier start wrote it; it was left exactly as it was.
    AlreadyPresent(PathBuf),
}

impl CutoverBackup {
    /// Where the backup is.
    pub fn path(&self) -> &Path {
        match self {
            Self::Written(path) | Self::AlreadyPresent(path) => path,
        }
    }
}

/// Why the cutover backup could not be written. Startup stops: this backup cannot be
/// skipped.
#[derive(Debug)]
pub struct BackupError {
    /// The backup file that was being written or checked.
    pub path: PathBuf,
    pub cause: BackupFailure,
}

#[derive(Debug)]
pub enum BackupFailure {
    /// Reading the database or writing the copy failed in SQLite.
    Sqlite(rusqlite::Error),
    /// Creating, syncing or publishing the file failed.
    Io(io::Error),
    /// Something other than a regular file already has the backup's name.
    NotAFile,
    /// The database stayed locked by another connection.
    Locked,
}

impl fmt::Display for BackupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.path.display();
        match &self.cause {
            BackupFailure::Sqlite(e) => write!(f, "could not write the backup {path}: {e}")?,
            BackupFailure::Io(e) => write!(f, "could not write the backup {path}: {e}")?,
            BackupFailure::NotAFile => write!(f, "{path} exists but is not a regular file")?,
            BackupFailure::Locked => write!(
                f,
                "could not write the backup {path}: another program kept the database locked"
            )?,
        }
        let directory = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        write!(
            f,
            ". The controller writes nothing to the database until this backup exists. \
             Free some space or fix the permissions in {}, and stop any other program using \
             the database. This backup cannot be skipped.",
            directory.display()
        )
    }
}

impl std::error::Error for BackupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.cause {
            BackupFailure::Sqlite(e) => Some(e),
            BackupFailure::Io(e) => Some(e),
            BackupFailure::NotAFile | BackupFailure::Locked => None,
        }
    }
}

/// `<database>.pre-rust.bak`: the database's own file name with the suffix added.
pub fn cutover_backup_path(database: &Path) -> PathBuf {
    with_suffix(database, CUTOVER_SUFFIX)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = OsString::from(path.as_os_str());
    name.push(suffix);
    PathBuf::from(name)
}

/// Write the cutover backup unless it already exists.
pub(crate) fn ensure_cutover_backup(
    source: &Connection,
    database: &Path,
) -> Result<CutoverBackup, BackupError> {
    let path = cutover_backup_path(database);
    let fail = |cause| BackupError {
        path: path.clone(),
        cause,
    };
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => return Ok(CutoverBackup::AlreadyPresent(path)),
        Ok(_) => return Err(fail(BackupFailure::NotAFile)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(fail(BackupFailure::Io(e))),
    }

    let partial = with_suffix(&path, PARTIAL_SUFFIX);
    // A crash during an earlier attempt leaves the copy and possibly its rollback journal.
    for leftover in [partial.clone(), with_suffix(&partial, "-journal")] {
        match fs::remove_file(&leftover) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail(BackupFailure::Io(e))),
        }
    }

    copy_database(source, &partial).map_err(fail)?;
    publish(&partial, &path, database).map_err(fail)
}

fn copy_database(source: &Connection, partial: &Path) -> Result<(), BackupFailure> {
    let mut copy = Connection::open(partial).map_err(BackupFailure::Sqlite)?;
    {
        let backup = Backup::new(source, &mut copy).map_err(BackupFailure::Sqlite)?;
        let mut locked = 0;
        loop {
            match backup.step(-1).map_err(BackupFailure::Sqlite)? {
                StepResult::Done => break,
                StepResult::More => {}
                _ if locked < LOCKED_RETRIES => {
                    locked += 1;
                    thread::sleep(LOCKED_PAUSE);
                }
                _ => return Err(BackupFailure::Locked),
            }
        }
    }
    copy.close().map_err(|(_, e)| BackupFailure::Sqlite(e))?;
    fs::File::open(partial)
        .and_then(|file| file.sync_all())
        .map_err(BackupFailure::Io)
}

/// Give the finished copy its final name without ever replacing an existing file, then
/// make the rename durable.
fn publish(partial: &Path, path: &Path, database: &Path) -> Result<CutoverBackup, BackupFailure> {
    // The backup holds the same secrets as the database, so it is no more readable.
    let permissions = fs::metadata(database)
        .map_err(BackupFailure::Io)?
        .permissions();
    fs::set_permissions(partial, permissions).map_err(BackupFailure::Io)?;

    // A hard link fails when the name is taken, which a rename would silently replace.
    let outcome = match fs::hard_link(partial, path) {
        Ok(()) => {
            fs::remove_file(partial).map_err(BackupFailure::Io)?;
            CutoverBackup::Written(path.to_owned())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            fs::remove_file(partial).map_err(BackupFailure::Io)?;
            CutoverBackup::AlreadyPresent(path.to_owned())
        }
        // Some filesystems have no hard links. The name was free a moment ago and only
        // one controller runs, so a rename is the fallback.
        Err(_) => {
            if fs::symlink_metadata(path).is_ok() {
                fs::remove_file(partial).map_err(BackupFailure::Io)?;
                return Ok(CutoverBackup::AlreadyPresent(path.to_owned()));
            }
            fs::rename(partial, path).map_err(BackupFailure::Io)?;
            CutoverBackup::Written(path.to_owned())
        }
    };
    if let Some(directory) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::File::open(directory)
            .and_then(|dir| dir.sync_all())
            .map_err(BackupFailure::Io)?;
    }
    Ok(outcome)
}
