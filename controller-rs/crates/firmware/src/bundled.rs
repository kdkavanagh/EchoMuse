//! The firmware bundled with this controller (spec §3.2 "Bundled firmware").
//!
//! `FIRMWARE_DIR` holds `server`, the binary, and `version`, its version text. Both are
//! read and hashed once at startup. Every later read hashes the files again and refuses
//! to hand out bytes that differ from what startup recorded: the image's firmware is the
//! only one this controller installs, and a binary that changed underneath a running
//! controller is not the one it vouched for.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use echomuse_protocol::Sha256;
use serde::Serialize;

use crate::types::{FirmwareVersion, Md5};

/// The binary's file name inside `FIRMWARE_DIR`.
pub const SERVER_FILE: &str = "server";
/// The version file's name inside `FIRMWARE_DIR`.
pub const VERSION_FILE: &str = "version";

/// What `GET /api/firmware` reports: `{version, size, sha256}`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FirmwareInfo {
    /// The bundled version.
    pub version: FirmwareVersion,
    /// The binary's size in bytes.
    pub size: u64,
    /// The binary's SHA-256.
    pub sha256: Sha256,
}

/// The bundled firmware as recorded at startup. Cheap to clone.
#[derive(Clone, Debug)]
pub struct BundledFirmware {
    inner: Arc<Recorded>,
}

#[derive(Debug)]
struct Recorded {
    dir: PathBuf,
    info: FirmwareInfo,
    md5: Md5,
    version_text: Bytes,
}

/// The bundled firmware's bytes, checked against what startup recorded.
#[derive(Clone)]
pub struct FirmwareImage {
    /// The binary.
    pub bytes: Bytes,
    /// Its version, size and SHA-256.
    pub info: FirmwareInfo,
    /// Its MD5, which the device checks the transfer against.
    pub md5: Md5,
}

impl fmt::Debug for FirmwareImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FirmwareImage")
            .field("info", &self.info)
            .field("md5", &self.md5)
            .finish_non_exhaustive()
    }
}

/// Why the bundled firmware cannot be used.
#[derive(Debug)]
pub enum BundledFirmwareError {
    /// A file could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// The error.
        error: io::Error,
    },
    /// The binary is empty.
    EmptyBinary {
        /// The binary's path.
        path: PathBuf,
    },
    /// The version file does not hold one version.
    InvalidVersion {
        /// The version file's path.
        path: PathBuf,
    },
    /// The binary or the version file changed after startup recorded them.
    Changed {
        /// The file that changed.
        path: PathBuf,
    },
    /// The task reading the files stopped before finishing.
    Interrupted,
}

impl fmt::Display for BundledFirmwareError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, error } => write!(f, "cannot read {}: {error}", path.display()),
            Self::EmptyBinary { path } => write!(f, "the firmware {} is empty", path.display()),
            Self::InvalidVersion { path } => {
                write!(f, "{} does not hold a firmware version", path.display())
            }
            Self::Changed { path } => write!(
                f,
                "{} changed since the controller started; restart the controller to use it",
                path.display()
            ),
            Self::Interrupted => f.write_str("reading the bundled firmware was interrupted"),
        }
    }
}

impl std::error::Error for BundledFirmwareError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { error, .. } => Some(error),
            _ => None,
        }
    }
}

struct Files {
    binary: Vec<u8>,
    version_text: Vec<u8>,
}

fn read_files(dir: &Path) -> Result<Files, BundledFirmwareError> {
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read(&path).map_err(|error| BundledFirmwareError::Read { path, error })
    };
    Ok(Files {
        binary: read(SERVER_FILE)?,
        version_text: read(VERSION_FILE)?,
    })
}

impl BundledFirmware {
    /// Reads, checks and hashes `FIRMWARE_DIR`'s `server` and `version`. Blocking; call it
    /// during startup, before serving.
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self, BundledFirmwareError> {
        let dir = dir.into();
        let files = read_files(&dir)?;
        if files.binary.is_empty() {
            return Err(BundledFirmwareError::EmptyBinary {
                path: dir.join(SERVER_FILE),
            });
        }
        let version = std::str::from_utf8(&files.version_text)
            .ok()
            .and_then(FirmwareVersion::new)
            .ok_or_else(|| BundledFirmwareError::InvalidVersion {
                path: dir.join(VERSION_FILE),
            })?;
        let info = FirmwareInfo {
            version,
            size: files.binary.len() as u64,
            sha256: Sha256::of(&files.binary),
        };
        Ok(Self {
            inner: Arc::new(Recorded {
                info,
                md5: Md5::of(&files.binary),
                version_text: Bytes::from(files.version_text),
                dir,
            }),
        })
    }

    /// The version, size and SHA-256 recorded at startup.
    pub fn info(&self) -> &FirmwareInfo {
        &self.inner.info
    }

    /// The bundled version.
    pub fn version(&self) -> &FirmwareVersion {
        &self.inner.info.version
    }

    /// Reads the binary for serving, off the async runtime. Fails with
    /// [`BundledFirmwareError::Changed`] when the binary or its version file no longer
    /// match what startup recorded.
    pub async fn read_verified(&self) -> Result<FirmwareImage, BundledFirmwareError> {
        let recorded = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || recorded.reread())
            .await
            .map_err(|_| BundledFirmwareError::Interrupted)?
    }
}

impl Recorded {
    fn reread(&self) -> Result<FirmwareImage, BundledFirmwareError> {
        let files = read_files(&self.dir)?;
        if files.binary.len() as u64 != self.info.size
            || Sha256::of(&files.binary) != self.info.sha256
        {
            return Err(BundledFirmwareError::Changed {
                path: self.dir.join(SERVER_FILE),
            });
        }
        if files.version_text != self.version_text {
            return Err(BundledFirmwareError::Changed {
                path: self.dir.join(VERSION_FILE),
            });
        }
        Ok(FirmwareImage {
            bytes: Bytes::from(files.binary),
            info: self.info.clone(),
            md5: self.md5,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(binary: &[u8], version: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SERVER_FILE), binary).unwrap();
        std::fs::write(dir.path().join(VERSION_FILE), version).unwrap();
        dir
    }

    #[tokio::test]
    async fn records_the_binary_and_serves_it_unchanged() {
        let dir = bundle(b"\x7fELF firmware", "fw-0123456789ab\n");
        let firmware = BundledFirmware::load(dir.path()).unwrap();
        assert_eq!(firmware.version().as_str(), "fw-0123456789ab");
        assert_eq!(firmware.info().size, 13);
        assert_eq!(firmware.info().sha256, Sha256::of(b"\x7fELF firmware"));
        let image = firmware.read_verified().await.unwrap();
        assert_eq!(&image.bytes[..], b"\x7fELF firmware");
        assert_eq!(image.md5, Md5::of(b"\x7fELF firmware"));
        let json = serde_json::to_value(firmware.info()).unwrap();
        assert_eq!(json["version"], "fw-0123456789ab");
        assert_eq!(json["size"], 13);
        assert_eq!(json["sha256"], Sha256::of(b"\x7fELF firmware").as_str());
    }

    #[tokio::test]
    async fn refuses_a_binary_that_changed_since_startup() {
        let dir = bundle(b"\x7fELF firmware", "fw-0123456789ab\n");
        let firmware = BundledFirmware::load(dir.path()).unwrap();
        // Same length, different bytes.
        std::fs::write(dir.path().join(SERVER_FILE), b"\x7fELF FIRMWARE").unwrap();
        assert!(matches!(
            firmware.read_verified().await,
            Err(BundledFirmwareError::Changed { path }) if path.ends_with(SERVER_FILE)
        ));
        std::fs::write(dir.path().join(SERVER_FILE), b"\x7fELF firmware").unwrap();
        assert!(firmware.read_verified().await.is_ok());
        std::fs::write(dir.path().join(VERSION_FILE), "fw-ffffffffffff\n").unwrap();
        assert!(matches!(
            firmware.read_verified().await,
            Err(BundledFirmwareError::Changed { path }) if path.ends_with(VERSION_FILE)
        ));
        std::fs::remove_file(dir.path().join(SERVER_FILE)).unwrap();
        assert!(matches!(
            firmware.read_verified().await,
            Err(BundledFirmwareError::Read { .. })
        ));
    }

    #[test]
    fn refuses_to_start_without_a_usable_bundle() {
        let dir = bundle(b"", "fw-0123456789ab\n");
        assert!(matches!(
            BundledFirmware::load(dir.path()),
            Err(BundledFirmwareError::EmptyBinary { .. })
        ));
        let dir = bundle(b"bin", "\n");
        assert!(matches!(
            BundledFirmware::load(dir.path()),
            Err(BundledFirmwareError::InvalidVersion { .. })
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            BundledFirmware::load(dir.path()),
            Err(BundledFirmwareError::Read { .. })
        ));
    }
}
