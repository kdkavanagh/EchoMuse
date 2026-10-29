//! File transfer through the device shell (spec §3.2 "File transfer").
//!
//! The device has only toybox and mksh (no `printf`, `cut`, `awk` or `head`), so a file
//! travels as base64 text:
//!
//! 1. [`check_tools`] proves `base64 -d` and `md5sum` work.
//! 2. [`transfer_file`] writes `<dest>.part` through base64 heredocs ending at
//!    `__END_B64_42__`, a bounded chunk at a time, each acknowledged by `TRANSFER_OK`.
//!    The part file is created with umask 077, so a secret is never readable by others
//!    on its way.
//! 3. It compares the part file's `md5sum` with the data's, sets the final mode and
//!    `mv`s the part file over the destination, answering `VERIFY_OK` or
//!    `VERIFY_BAD:<md5>`.
//!
//! The destination is never deleted first: it may be the rollback slot, and `mv` replaces
//! it in one step, so an interrupted transfer leaves it as it was.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::layout::{DevicePath, FileMode};
use crate::shell::{CommandLimits, CommandOutput, DeviceShell, ShellFailure, ShellIo};
use crate::types::Md5;

/// The heredoc delimiter, which base64 text can never contain.
pub const HEREDOC_DELIMITER: &str = "__END_B64_42__";
/// What the device prints after each chunk it wrote.
pub const TRANSFER_OK: &str = "TRANSFER_OK";
/// What the device prints when the file is in place.
pub const VERIFY_OK: &str = "VERIFY_OK";
/// What the device prints, followed by the md5 it computed, when the file is corrupt.
pub const VERIFY_BAD: &str = "VERIFY_BAD:";

/// Raw bytes per heredoc. A chunk is 256 KiB of base64 text: small enough for the
/// shell's here-document handling and to acknowledge often, large enough that the
/// per-chunk round trip does not dominate.
pub const CHUNK_BYTES: usize = 192 * 1024;
/// Raw bytes per base64 line (76 characters, as `base64` itself wraps).
const LINE_BYTES: usize = 57;

const PROBE_TEXT: &str = "echomuse";

/// A tool the transfer needs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    /// `base64 -d`.
    Base64,
    /// `md5sum`.
    Md5sum,
}

impl fmt::Display for Tool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Base64 => "base64 -d",
            Self::Md5sum => "md5sum",
        })
    }
}

/// Why a transfer failed. None of these leave the destination changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferError {
    /// The shell failed.
    Shell(ShellFailure),
    /// A tool the transfer needs does not work on the device.
    MissingTool {
        /// The tool.
        tool: Tool,
        /// What the probe printed.
        output: String,
    },
    /// The device could not write a chunk (for example, a full disk).
    WriteFailed {
        /// What it printed.
        output: String,
    },
    /// The file on the device does not match what was sent; the part file was removed.
    Mismatch {
        /// The MD5 of what was sent.
        expected: Md5,
        /// What `md5sum` reported on the device.
        reported: String,
    },
    /// The file arrived intact but could not be moved into place.
    PlaceFailed {
        /// What the device printed.
        output: String,
    },
    /// The device answered a query with something unexpected.
    UnexpectedReply {
        /// What it printed.
        output: String,
    },
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shell(failure) => failure.fmt(f),
            Self::MissingTool { tool, output } => {
                write!(f, "`{tool}` does not work on the device: {}", output.trim())
            }
            Self::WriteFailed { output } => {
                write!(f, "the device could not write the file: {}", output.trim())
            }
            Self::Mismatch { expected, reported } => write!(
                f,
                "the file arrived corrupted (md5 {reported}, expected {expected})"
            ),
            Self::PlaceFailed { output } => write!(
                f,
                "the device could not move the file into place: {}",
                output.trim()
            ),
            Self::UnexpectedReply { output } => {
                write!(f, "unexpected reply from the device: {}", output.trim())
            }
        }
    }
}

impl std::error::Error for TransferError {}

impl From<ShellFailure> for TransferError {
    fn from(failure: ShellFailure) -> Self {
        Self::Shell(failure)
    }
}

/// Proof that [`check_tools`] passed on this shell.
#[derive(Debug)]
pub struct ToolsReady(());

/// Bounds for the commands of a transfer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferLimits {
    /// Short commands: probes, md5 queries.
    pub command: CommandLimits,
    /// Writing one chunk.
    pub chunk: CommandLimits,
    /// Hashing and placing the finished file.
    pub verify: CommandLimits,
}

/// Probes that `base64 -d` decodes and `md5sum` hashes.
pub async fn check_tools<S: ShellIo>(
    shell: &mut DeviceShell<S>,
    limits: &TransferLimits,
) -> Result<ToolsReady, TransferError> {
    let encoded = STANDARD.encode(PROBE_TEXT);
    let out = shell
        .run(&format!("echo {encoded} | base64 -d 2>&1"), limits.command)
        .await?;
    if out.status != 0 || out.text.trim() != PROBE_TEXT {
        return Err(TransferError::MissingTool {
            tool: Tool::Base64,
            output: out.text,
        });
    }
    let out = shell
        .run(&format!("echo {PROBE_TEXT} | md5sum 2>&1"), limits.command)
        .await?;
    let expected = Md5::of(format!("{PROBE_TEXT}\n").as_bytes());
    let reported = out.text.split_whitespace().next().and_then(Md5::parse);
    if out.status != 0 || reported != Some(expected) {
        return Err(TransferError::MissingTool {
            tool: Tool::Md5sum,
            output: out.text,
        });
    }
    Ok(ToolsReady(()))
}

/// The MD5 of a file on the device; None when it does not exist or cannot be read.
pub async fn remote_md5<S: ShellIo>(
    shell: &mut DeviceShell<S>,
    _tools: &ToolsReady,
    path: &DevicePath,
    limits: &TransferLimits,
) -> Result<Option<Md5>, TransferError> {
    let out = shell
        .run(
            &format!("M=$(md5sum {path} 2>/dev/null); echo \"MD5:${{M%% *}}\""),
            limits.command,
        )
        .await?;
    let reported = out
        .lines()
        .find_map(|line| line.strip_prefix("MD5:"))
        .ok_or_else(|| TransferError::UnexpectedReply {
            output: out.text.clone(),
        })?;
    Ok(Md5::parse(reported))
}

/// Writes `data` to `dest` with `mode`, reporting `(sent, total)` bytes after each chunk
/// the device acknowledged. Returns the data's MD5.
pub async fn transfer_file<S: ShellIo>(
    shell: &mut DeviceShell<S>,
    _tools: &ToolsReady,
    data: &[u8],
    dest: &DevicePath,
    mode: FileMode,
    limits: &TransferLimits,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> Result<Md5, TransferError> {
    let part = dest.part();
    let total = data.len() as u64;
    let mut sent = 0u64;
    // An empty file is one empty heredoc.
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(CHUNK_BYTES).collect()
    };
    for (index, chunk) in chunks.into_iter().enumerate() {
        let redirect = if index == 0 { ">" } else { ">>" };
        let script = format!(
            "(umask 077; base64 -d {redirect} {part}) <<'{HEREDOC_DELIMITER}' && echo {TRANSFER_OK}\n{}{HEREDOC_DELIMITER}",
            encode_lines(chunk)
        );
        let out = shell.run(&script, limits.chunk).await?;
        if !out.has_line(TRANSFER_OK) {
            return Err(TransferError::WriteFailed { output: out.text });
        }
        sent += chunk.len() as u64;
        progress(sent, total);
    }
    let md5 = Md5::of(data);
    let out = shell
        .run(
            &format!(
                "M=$(md5sum {part} 2>&1); M=${{M%% *}}; if [ \"$M\" = \"{md5}\" ]; then chmod {mode} {part} && mv -f {part} {dest} && echo {VERIFY_OK}; else rm -f {part}; echo \"{VERIFY_BAD}$M\"; fi"
            ),
            limits.verify,
        )
        .await?;
    verify_result(out, md5)
}

fn verify_result(out: CommandOutput, expected: Md5) -> Result<Md5, TransferError> {
    if out.has_line(VERIFY_OK) {
        return Ok(expected);
    }
    if let Some(reported) = out.lines().find_map(|line| line.strip_prefix(VERIFY_BAD)) {
        return Err(TransferError::Mismatch {
            expected,
            reported: reported.to_owned(),
        });
    }
    Err(TransferError::PlaceFailed { output: out.text })
}

/// Whether [`sync_file`] wrote the file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Synced {
    /// The device already had these bytes.
    Unchanged,
    /// The file was written.
    Written,
}

/// Writes `data` to `dest` unless the device's copy already has the same MD5.
pub async fn sync_file<S: ShellIo>(
    shell: &mut DeviceShell<S>,
    tools: &ToolsReady,
    data: &[u8],
    dest: &DevicePath,
    mode: FileMode,
    limits: &TransferLimits,
) -> Result<Synced, TransferError> {
    if remote_md5(shell, tools, dest, limits).await? == Some(Md5::of(data)) {
        return Ok(Synced::Unchanged);
    }
    transfer_file(shell, tools, data, dest, mode, limits, &mut |_, _| {}).await?;
    Ok(Synced::Written)
}

/// Base64 in 76-character lines, each ending in a newline.
fn encode_lines(data: &[u8]) -> String {
    let mut text = String::with_capacity(data.len().div_ceil(LINE_BYTES) * 77);
    for line in data.chunks(LINE_BYTES) {
        STANDARD.encode_string(line, &mut text);
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_decode_back_to_the_data() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let text = encode_lines(&data);
        assert!(text.lines().all(|line| line.len() <= 76));
        let joined: String = text.lines().collect();
        assert_eq!(STANDARD.decode(joined).unwrap(), data);
        assert_eq!(encode_lines(&[]), "");
        assert!(!text.contains(HEREDOC_DELIMITER));
    }

    #[test]
    fn verify_lines_are_read_into_results() {
        let md5 = Md5::of(b"x");
        let out = |text: &str| CommandOutput {
            text: text.into(),
            status: 0,
        };
        assert_eq!(verify_result(out("VERIFY_OK\n"), md5), Ok(md5));
        assert_eq!(
            verify_result(out("VERIFY_BAD:0123\n"), md5),
            Err(TransferError::Mismatch {
                expected: md5,
                reported: "0123".into()
            })
        );
        assert!(matches!(
            verify_result(out("mv: Permission denied\n"), md5),
            Err(TransferError::PlaceFailed { .. })
        ));
    }
}
