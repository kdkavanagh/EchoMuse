//! Small value types shared by the rest of the crate.

use std::fmt;

use serde::{Serialize, Serializer};

/// The longest firmware version text accepted.
const MAX_VERSION_LEN: usize = 128;

/// A firmware version as the firmware reports it in `session.hello` and as the bundled
/// `version` file holds it (`fw-` plus a source hash, today). Compared as text, never
/// ordered: the controller negotiates features by capability, not version.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct FirmwareVersion(String);

impl FirmwareVersion {
    /// The version in `text`, without surrounding whitespace. None when that is empty,
    /// longer than 128 bytes, or holds anything but visible ASCII characters (it travels
    /// in an HTTP header).
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        let valid = !text.is_empty()
            && text.len() <= MAX_VERSION_LEN
            && text.bytes().all(|b| b.is_ascii_graphic());
        valid.then(|| Self(text.to_owned()))
    }

    /// The version text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for FirmwareVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// An MD5 digest as `md5sum` prints it: 32 lowercase hex digits. Used only to check that
/// a file arrived intact on the device, where `md5sum` is the hash toybox has.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Md5([u8; 16]);

impl Md5 {
    /// The MD5 of `data`.
    pub fn of(data: &[u8]) -> Self {
        use md5::Digest;
        Self(md5::Md5::digest(data).into())
    }

    /// Accepts exactly 32 hex digits, either case.
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() != 32 {
            return None;
        }
        let mut digest = [0u8; 16];
        let (pairs, _) = text.as_bytes().as_chunks::<2>();
        for (byte, [high, low]) in digest.iter_mut().zip(pairs) {
            let high = hex_value(*high)?;
            let low = hex_value(*low)?;
            *byte = high << 4 | low;
        }
        Some(Self(digest))
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

impl fmt::Display for Md5 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Md5 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Md5({self})")
    }
}

/// A device's link token. Its `Debug` output never shows the value.
#[derive(Clone, Eq, PartialEq)]
pub struct LinkToken(String);

impl LinkToken {
    /// Wraps a token value.
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// The value, for writing to the device or the wizard.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LinkToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_md5sum_and_round_trips() {
        // `echo echomuse | md5sum`
        let digest = Md5::of(b"echomuse\n");
        let text = digest.to_string();
        assert_eq!(text.len(), 32);
        assert_eq!(Md5::parse(&text), Some(digest));
        assert_eq!(Md5::parse(&text.to_uppercase()), Some(digest));
        assert_eq!(Md5::of(b"").to_string(), "d41d8cd98f00b204e9800998ecf8427e");
        assert!(Md5::parse("xyz").is_none());
        assert!(Md5::parse(&"g".repeat(32)).is_none());
    }

    #[test]
    fn versions_are_one_trimmed_word() {
        assert_eq!(
            FirmwareVersion::new("fw-0123456789ab\n").unwrap().as_str(),
            "fw-0123456789ab"
        );
        assert!(FirmwareVersion::new("  ").is_none());
        assert!(FirmwareVersion::new("fw 1").is_none());
        assert!(FirmwareVersion::new(&"x".repeat(129)).is_none());
    }

    #[test]
    fn tokens_stay_out_of_debug_output() {
        let token = LinkToken::new("secret-value".into());
        assert_eq!(format!("{token:?}"), "LinkToken(..)");
    }
}
