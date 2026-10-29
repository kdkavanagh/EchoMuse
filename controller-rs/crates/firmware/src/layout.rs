//! Where EchoMuse lives on a device (spec §3.1.2, §3.2): the A/B firmware slots, the
//! start script and the `/data/local/etc/echomuse/` files. These paths are what the
//! wizard, `start_server.sh` and the firmware already use, so they are fixed.

use std::fmt;

/// The directory holding the firmware slots and the start script.
pub const BIN_DIR: &str = "/data/local/bin";
/// The symlink `start_server.sh` runs; it names the active slot relative to [`BIN_DIR`].
pub const SERVER_LINK: &str = "/data/local/bin/server";
/// The start script that supervises the firmware and rolls it back.
pub const START_SCRIPT: &str = "/data/local/bin/start_server.sh";
/// EchoMuse's own files on the device: link credentials, the Echo Show package lists
/// and the supervisor log.
pub const ETC_DIR: &str = "/data/local/etc/echomuse";
/// The start script's own log of starts, exits and rollbacks; it survives a reboot.
pub const SUPERVISOR_LOG: &str = "/data/local/etc/echomuse/supervisor.log";
/// The CA the device pins for the TLS link (mode 0644).
pub const CA_PEM: &str = "/data/local/etc/echomuse/ca.pem";
/// The device's link token (mode 0600).
pub const LINK_TOKEN: &str = "/data/local/etc/echomuse/token";
/// The Echo Show hide tiers the wizard applied (`1`, or `1 2`).
pub const CROWN_TIERS: &str = "/data/local/etc/echomuse/crown_tiers";

/// An absolute path on the device that is safe to put in a shell command unquoted:
/// letters, digits, `.`, `_`, `-` and `/` only, with no `..` component.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DevicePath(String);

impl DevicePath {
    /// Accepts an absolute path made of the safe characters.
    pub fn parse(path: &str) -> Option<Self> {
        let safe = path.starts_with('/')
            && path.len() > 1
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
            && !path.split('/').any(|part| part == "..");
        safe.then(|| Self(path.to_owned()))
    }

    /// One of the fixed paths in this module.
    pub(crate) fn fixed(path: &'static str) -> Self {
        Self::parse(path).expect("fixed device paths are valid")
    }

    /// The path text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The temporary file a transfer writes before moving it over this path.
    pub fn part(&self) -> Self {
        Self(format!("{}.part", self.0))
    }
}

impl fmt::Display for DevicePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Unix permission mode for a file the controller writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileMode(u16);

impl FileMode {
    /// Owner read and write only, for secrets.
    pub const PRIVATE: Self = Self(0o600);
    /// Readable by everyone.
    pub const READABLE: Self = Self(0o644);
    /// Readable and executable by everyone, for programs and scripts.
    pub const EXECUTABLE: Self = Self(0o755);

    /// The mode bits.
    pub fn bits(self) -> u16 {
        self.0
    }
}

impl fmt::Display for FileMode {
    /// The octal form `chmod` takes, such as `644`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:o}", self.0)
    }
}

/// One of the two firmware slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Slot {
    /// `server_a`.
    A,
    /// `server_b`.
    B,
}

impl Slot {
    /// The name the `server` symlink holds. It must be this relative name:
    /// `start_server.sh` only knows how to roll back from `server_a` or `server_b`.
    pub fn name(self) -> &'static str {
        match self {
            Self::A => "server_a",
            Self::B => "server_b",
        }
    }

    /// The slot's full path.
    pub fn path(self) -> DevicePath {
        DevicePath::fixed(match self {
            Self::A => "/data/local/bin/server_a",
            Self::B => "/data/local/bin/server_b",
        })
    }

    /// The other slot.
    pub fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// The slot a `readlink` of the symlink names, if it is one of the two.
    pub fn from_link(target: &str) -> Option<Self> {
        match target {
            "server_a" => Some(Self::A),
            "server_b" => Some(Self::B),
            _ => None,
        }
    }
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The three Echo Show package lists `start_server.sh` reads from [`ETC_DIR`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CrownList {
    /// Tier 1 hide list.
    Hide,
    /// Tier 2 hide list, applied only when the operator opted in.
    HideTier2,
    /// Packages whose microphone is denied while Tier 1 is applied.
    MicDeny,
}

impl CrownList {
    /// All three, in the order the wizard installs them.
    pub const ALL: [Self; 3] = [Self::Hide, Self::HideTier2, Self::MicDeny];

    /// The file name, the same in the controller's payload directory and on the device.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Hide => "crown_hide_packages.txt",
            Self::HideTier2 => "crown_hide_packages_tier2.txt",
            Self::MicDeny => "crown_mic_deny_packages.txt",
        }
    }

    /// Where the list goes on the device.
    pub fn device_path(self) -> DevicePath {
        DevicePath::fixed(match self {
            Self::Hide => "/data/local/etc/echomuse/crown_hide_packages.txt",
            Self::HideTier2 => "/data/local/etc/echomuse/crown_hide_packages_tier2.txt",
            Self::MicDeny => "/data/local/etc/echomuse/crown_mic_deny_packages.txt",
        })
    }

    /// The hide tier this list belongs to (`"1"` or `"2"`, as `crown_tiers` names
    /// them); None for the microphone list.
    pub fn tier(self) -> Option<&'static str> {
        match self {
            Self::Hide => Some("1"),
            Self::HideTier2 => Some("2"),
            Self::MicDeny => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths_refuse_anything_a_shell_would_interpret() {
        assert!(DevicePath::parse("/data/local/bin/server_a").is_some());
        for bad in [
            "",
            "/",
            "relative/path",
            "/data/local/bin/a b",
            "/data/$(reboot)",
            "/data/'x'",
            "/data/../system",
            "/data/x;y",
        ] {
            assert!(DevicePath::parse(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            DevicePath::fixed(START_SCRIPT).part().as_str(),
            "/data/local/bin/start_server.sh.part"
        );
    }

    #[test]
    fn slots_flip_and_read_back_from_the_link() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::from_link("server_b"), Some(Slot::B));
        assert_eq!(Slot::from_link("/data/local/bin/server_b"), None);
        assert_eq!(Slot::B.path().as_str(), "/data/local/bin/server_b");
        assert_eq!(FileMode::PRIVATE.to_string(), "600");
        assert_eq!(FileMode::EXECUTABLE.to_string(), "755");
    }
}
