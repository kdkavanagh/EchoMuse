//! SHA-256 digests and files pinned by them.
//!
//! Every model file and native library this crate loads is named together with the SHA-256 its
//! bytes must have. [`PinnedFile::read_verified`] reads a file and checks it in one pass, so a
//! model handed to ONNX Runtime is exactly the bytes that were hashed.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::error::SpeechError;

/// A SHA-256 digest, written and parsed as 64 lowercase hexadecimal digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest([u8; 32]);

/// Why a string is not a SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DigestParseError {
    /// The string is not 64 characters long.
    #[error("a SHA-256 digest has 64 hex digits, got {0} characters")]
    Length(usize),
    /// A character is not a lowercase hex digit.
    #[error("character {index} of the digest is not a lowercase hex digit")]
    Digit {
        /// Zero-based position of the offending character.
        index: usize,
    },
}

const fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

const fn parse_hex(s: &str) -> Result<[u8; 32], DigestParseError> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return Err(DigestParseError::Length(bytes.len()));
    }
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        let hi = match hex_value(bytes[2 * i]) {
            Some(v) => v,
            None => return Err(DigestParseError::Digit { index: 2 * i }),
        };
        let lo = match hex_value(bytes[2 * i + 1]) {
            Some(v) => v,
            None => return Err(DigestParseError::Digit { index: 2 * i + 1 }),
        };
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Ok(out)
}

impl Sha256Digest {
    /// Parses a digest in a constant, for pins compiled into the crate. An invalid literal is a
    /// compile-time error when used in a `const`.
    pub const fn from_hex_const(s: &str) -> Self {
        match parse_hex(s) {
            Ok(b) => Self(b),
            Err(_) => panic!("invalid SHA-256 literal"),
        }
    }

    /// Parses 64 lowercase hexadecimal digits.
    pub fn parse_hex(s: &str) -> Result<Self, DigestParseError> {
        parse_hex(s).map(Self)
    }

    /// The digest of `bytes`.
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The digest of the file at `path`, read in pieces so a large file is not held in memory.
    pub fn of_file(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(Self(hasher.finalize().into()))
    }

    /// The raw 32 bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The digest as 64 lowercase hexadecimal digits.
    pub fn to_hex(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha256Digest({self})")
    }
}

impl std::str::FromStr for Sha256Digest {
    type Err = DigestParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(s)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// A file path together with the SHA-256 its contents must have.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedFile {
    /// Where the file is.
    pub path: PathBuf,
    /// The digest its bytes must have.
    pub sha256: Sha256Digest,
}

impl PinnedFile {
    /// Pairs a path with its pin.
    pub fn new(path: impl Into<PathBuf>, sha256: Sha256Digest) -> Self {
        Self {
            path: path.into(),
            sha256,
        }
    }

    /// Hashes the file without keeping its bytes, and fails unless the digest matches.
    pub fn verify(&self) -> Result<(), SpeechError> {
        let actual = Sha256Digest::of_file(&self.path).map_err(|source| SpeechError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.check(actual)
    }

    /// Reads the whole file and fails unless its digest matches; the returned bytes are the
    /// ones that were hashed.
    pub fn read_verified(&self) -> Result<Vec<u8>, SpeechError> {
        let bytes = std::fs::read(&self.path).map_err(|source| SpeechError::Io {
            path: self.path.clone(),
            source,
        })?;
        self.check(Sha256Digest::of_bytes(&bytes))?;
        Ok(bytes)
    }

    fn check(&self, actual: Sha256Digest) -> Result<(), SpeechError> {
        if actual == self.sha256 {
            Ok(())
        } else {
            Err(SpeechError::HashMismatch {
                path: self.path.clone(),
                expected: self.sha256,
                actual,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_round_trips_through_hex() {
        let hex = "6b99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396";
        let d = Sha256Digest::parse_hex(hex).unwrap();
        assert_eq!(d.to_hex(), hex);
        assert_eq!(Sha256Digest::from_hex_const(hex), d);
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{hex}\""));
        assert_eq!(serde_json::from_str::<Sha256Digest>(&json).unwrap(), d);
    }

    #[test]
    fn digest_rejects_bad_text() {
        assert_eq!(
            Sha256Digest::parse_hex("abc"),
            Err(DigestParseError::Length(3))
        );
        let upper = "6B99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396";
        assert_eq!(
            Sha256Digest::parse_hex(upper),
            Err(DigestParseError::Digit { index: 1 })
        );
        assert!(serde_json::from_str::<Sha256Digest>("\"zz\"").is_err());
    }

    #[test]
    fn digest_of_empty_input_is_the_known_value() {
        assert_eq!(
            Sha256Digest::of_bytes(b"").to_hex(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn pinned_file_checks_contents() {
        let dir =
            std::env::temp_dir().join(format!("echomuse-speech-digest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        std::fs::write(&path, b"hello").unwrap();
        let good = PinnedFile::new(&path, Sha256Digest::of_bytes(b"hello"));
        assert_eq!(good.read_verified().unwrap(), b"hello");
        good.verify().unwrap();
        let bad = PinnedFile::new(&path, Sha256Digest::of_bytes(b"other"));
        match bad.read_verified() {
            Err(SpeechError::HashMismatch { actual, .. }) => {
                assert_eq!(actual, Sha256Digest::of_bytes(b"hello"))
            }
            other => panic!("expected a hash mismatch, got {other:?}"),
        }
        let missing = PinnedFile::new(dir.join("missing"), Sha256Digest::of_bytes(b""));
        assert!(matches!(missing.verify(), Err(SpeechError::Io { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
