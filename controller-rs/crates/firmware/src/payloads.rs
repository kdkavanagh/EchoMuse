//! Files the controller installs on devices besides the firmware: `start_server.sh` and
//! the three Echo Show package lists (spec §3.2). Their bytes are the contract, so they
//! are served exactly as they sit in the payload directory (the image's
//! `device_payloads/`), and read again on every use so an edited file takes effect
//! without a restart.

use std::fmt;
use std::io;
use std::path::PathBuf;

use bytes::Bytes;

use crate::layout::CrownList;
use crate::types::Md5;

/// The start script's file name in the payload directory.
pub const START_SCRIPT_FILE: &str = "start_server.sh";

/// The directory holding the payload files.
#[derive(Clone, Debug)]
pub struct DevicePayloads {
    dir: PathBuf,
}

/// One payload file's bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payload {
    /// The file name.
    pub file_name: &'static str,
    /// The bytes, exactly as stored.
    pub bytes: Bytes,
    /// Their MD5, which decides whether a device's copy is current.
    pub md5: Md5,
}

impl Payload {
    /// The bytes as text; the payloads are UTF-8 text files.
    pub fn text(&self) -> Result<&str, PayloadError> {
        std::str::from_utf8(&self.bytes).map_err(|_| PayloadError::NotText {
            file_name: self.file_name,
        })
    }
}

/// Why a payload file cannot be used.
#[derive(Debug)]
pub enum PayloadError {
    /// The file could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// The error.
        error: io::Error,
    },
    /// A file served as text is not UTF-8.
    NotText {
        /// The file.
        file_name: &'static str,
    },
}

impl fmt::Display for PayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, error } => write!(f, "cannot read {}: {error}", path.display()),
            Self::NotText { file_name } => write!(f, "{file_name} is not UTF-8 text"),
        }
    }
}

impl std::error::Error for PayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { error, .. } => Some(error),
            Self::NotText { .. } => None,
        }
    }
}

impl DevicePayloads {
    /// Payloads read from `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    async fn read(&self, file_name: &'static str) -> Result<Payload, PayloadError> {
        let path = self.dir.join(file_name);
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|error| PayloadError::Read { path, error })?;
        Ok(Payload {
            file_name,
            md5: Md5::of(&bytes),
            bytes: Bytes::from(bytes),
        })
    }

    /// `start_server.sh`.
    pub async fn start_script(&self) -> Result<Payload, PayloadError> {
        self.read(START_SCRIPT_FILE).await
    }

    /// One Echo Show package list.
    pub async fn crown_list(&self, list: CrownList) -> Result<Payload, PayloadError> {
        self.read(list.file_name()).await
    }
}

/// The packages a list names, in order, read the way `start_server.sh` reads them: the
/// first word of each line, skipping blank lines and lines starting with `#`.
pub fn list_packages(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|word| !word.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packages_are_first_words_without_comments() {
        let text = "# header\n\ncom.amazon.a\n  com.amazon.b  # trailing\n#com.amazon.c\ncom.amazon.d extra\n";
        assert_eq!(
            list_packages(text),
            ["com.amazon.a", "com.amazon.b", "com.amazon.d"]
        );
    }

    #[test]
    fn the_shipped_lists_parse() {
        // The contract files themselves, read as data.
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../controller/device_payloads");
        for list in CrownList::ALL {
            let text = std::fs::read_to_string(dir.join(list.file_name())).unwrap();
            let packages = list_packages(&text);
            assert!(!packages.is_empty(), "{}", list.file_name());
            assert!(packages.iter().all(|p| p.contains('.')), "{packages:?}");
        }
    }

    #[tokio::test]
    async fn reads_each_payload_fresh() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(START_SCRIPT_FILE), "#!/system/bin/sh\n").unwrap();
        let payloads = DevicePayloads::new(dir.path());
        let first = payloads.start_script().await.unwrap();
        assert_eq!(first.md5, Md5::of(b"#!/system/bin/sh\n"));
        std::fs::write(
            dir.path().join(START_SCRIPT_FILE),
            "#!/system/bin/sh\n# v2\n",
        )
        .unwrap();
        let second = payloads.start_script().await.unwrap();
        assert_ne!(first.md5, second.md5);
        assert!(matches!(
            payloads.crown_list(CrownList::Hide).await,
            Err(PayloadError::Read { .. })
        ));
    }
}
