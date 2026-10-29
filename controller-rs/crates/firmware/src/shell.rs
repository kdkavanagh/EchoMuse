//! Running commands in a device's pipe shell (spec §3.1.9, §3.2).
//!
//! In pipe mode the device runs `/system/bin/sh` (mksh) with standard error joined to
//! standard output and no terminal: no prompt and no echo, just the commands' output.
//! [`DeviceShell`] sends one script at a time followed by
//! `echo "__EM_<nonce>_<n>__:$?"`, and takes everything before that marker as the
//! script's output and the number after it as its exit status. The nonce is random per
//! shell, so nothing a command prints can be mistaken for the end of another.

use std::fmt;
use std::time::Duration;

use bytes::Bytes;
use echomuse_link::{
    SessionLink, ShellBroker, ShellConnection, ShellInput, ShellMode, ShellOpen, ShellReader,
    ShellWriter,
};
use tokio::time::Instant;

/// Bytes sent to the device in one shell frame at most.
const MAX_INPUT_FRAME: usize = 64 * 1024;
/// Output kept for one command at most; more fails the command.
const MAX_OUTPUT: usize = 1024 * 1024;

/// A connected pipe shell on a device: raw bytes both ways.
///
/// [`LinkShell`] implements it over the link's shell dial-back; tests implement it with
/// a simulated device.
pub trait ShellIo: Send {
    /// Sends bytes to the shell's standard input.
    fn send(&mut self, bytes: Bytes) -> impl Future<Output = Result<(), ShellClosed>> + Send;
    /// The next piece of output; None once the shell has closed.
    fn recv(&mut self) -> impl Future<Output = Option<Bytes>> + Send;
    /// Ends the shell.
    fn close(&mut self) -> impl Future<Output = ()> + Send;
}

/// The shell's connection closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShellClosed;

/// A pipe shell over the device link.
#[derive(Debug)]
pub struct LinkShell {
    writer: ShellWriter,
    reader: ShellReader,
}

impl ShellIo for LinkShell {
    async fn send(&mut self, bytes: Bytes) -> Result<(), ShellClosed> {
        self.writer
            .send(ShellInput::Data(bytes))
            .await
            .map_err(|_| ShellClosed)
    }

    async fn recv(&mut self) -> Option<Bytes> {
        self.reader.recv().await
    }

    async fn close(&mut self) {
        self.writer.close().await;
    }
}

/// Why a device shell could not be opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellOpenError {
    /// The device has no live session to send `shell_open` on.
    NotConnected,
    /// The device did not dial back in time.
    NoDialBack {
        /// How long the controller waited.
        waited: Duration,
    },
    /// Another shell for the device replaced this one before it connected.
    Superseded,
    /// The device attached a pseudo-terminal although a pipe was asked for.
    NotPipe,
}

impl fmt::Display for ShellOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => f.write_str("the device is not connected"),
            Self::NoDialBack { waited } => write!(
                f,
                "the device did not open its shell within {} s",
                waited.as_secs()
            ),
            Self::Superseded => f.write_str("another shell to the device replaced this one"),
            Self::NotPipe => f.write_str("the device opened a terminal instead of a pipe"),
        }
    }
}

impl std::error::Error for ShellOpenError {}

/// Opens a pipe shell over the link: registers for the dial-back, sends
/// `shell_open {pty: false}` on `link`, and waits for the device. It does not time out
/// by itself; [`FirmwareService`](crate::FirmwareService) bounds every open.
///
/// A [`DeviceAccess`](crate::DeviceAccess) implementation can answer
/// [`open_shell`](crate::DeviceAccess::open_shell) with this, given the device's live
/// session and the link server's [`ShellBroker`].
pub async fn open_pipe_shell(
    broker: &ShellBroker,
    link: &SessionLink,
) -> Result<LinkShell, ShellOpenError> {
    let pending = broker.expect(link.device().clone());
    link.send(&ShellOpen { pty: false }, 0)
        .map_err(|_| ShellOpenError::NotConnected)?;
    let connection = pending
        .connected()
        .await
        .map_err(|_| ShellOpenError::Superseded)?;
    pipe(connection)
}

fn pipe(connection: ShellConnection) -> Result<LinkShell, ShellOpenError> {
    if connection.mode() != ShellMode::Pipe {
        return Err(ShellOpenError::NotPipe);
    }
    let (writer, reader) = connection.split();
    Ok(LinkShell { writer, reader })
}

/// How long one command may take.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandLimits {
    /// The longest silence before the command's end.
    pub idle: Duration,
    /// The longest the whole command may take.
    pub total: Duration,
}

impl CommandLimits {
    /// The same bound for silence and in total.
    pub const fn within(limit: Duration) -> Self {
        Self {
            idle: limit,
            total: limit,
        }
    }
}

/// Why a shell command has no result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellFailure {
    /// The shell closed before the command finished.
    Closed {
        /// What the command printed before that.
        output: String,
    },
    /// The command printed nothing for too long, or ran too long.
    TimedOut {
        /// How long the controller waited.
        waited: Duration,
        /// What the command printed before that.
        output: String,
    },
    /// The command printed more than the controller keeps.
    OutputTooLarge,
}

impl fmt::Display for ShellFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed { .. } => f.write_str("the device shell closed"),
            Self::TimedOut { waited, .. } => {
                write!(f, "the device did not answer within {} s", waited.as_secs())
            }
            Self::OutputTooLarge => f.write_str("the device printed more than expected"),
        }
    }
}

impl std::error::Error for ShellFailure {}

/// One finished command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Everything it printed, standard error included.
    pub text: String,
    /// Its exit status (`$?`).
    pub status: i32,
}

impl CommandOutput {
    /// The output's lines, without trailing whitespace.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.text.lines().map(str::trim_end)
    }

    /// Whether some line is exactly `line`.
    pub fn has_line(&self, line: &str) -> bool {
        self.lines().any(|l| l == line)
    }
}

/// How a command that may end the shell finished.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Ending {
    /// It finished and reported its status.
    Finished(CommandOutput),
    /// The shell closed, as it does when the command ends the firmware.
    Closed {
        /// What was printed before.
        output: String,
    },
}

/// A device shell running one command at a time.
#[derive(Debug)]
pub struct DeviceShell<S> {
    io: S,
    pending: Vec<u8>,
    marker: String,
    sequence: u64,
    closed: bool,
}

impl<S: ShellIo> DeviceShell<S> {
    /// Wraps an open pipe shell.
    pub fn new(io: S) -> Self {
        let mut nonce = [0u8; 8];
        // A fixed nonce only weakens the marker against output that imitates it; the
        // shell still works.
        let _ = getrandom::fill(&mut nonce);
        Self {
            io,
            pending: Vec::new(),
            marker: format!("__EM_{:016x}_", u64::from_le_bytes(nonce)),
            sequence: 0,
            closed: false,
        }
    }

    /// Runs `script` (one or more lines, without a trailing newline) and waits for its
    /// status.
    pub async fn run(
        &mut self,
        script: &str,
        limits: CommandLimits,
    ) -> Result<CommandOutput, ShellFailure> {
        match self.run_until_end(script, limits).await? {
            Ending::Finished(output) => Ok(output),
            Ending::Closed { output } => Err(ShellFailure::Closed { output }),
        }
    }

    /// Runs `script` where the shell closing is an expected ending.
    pub async fn run_until_end(
        &mut self,
        script: &str,
        limits: CommandLimits,
    ) -> Result<Ending, ShellFailure> {
        if self.closed {
            return Ok(Ending::Closed {
                output: String::new(),
            });
        }
        self.sequence += 1;
        let end = format!("{}{}__:", self.marker, self.sequence);
        let input = format!("{script}\necho \"{end}$?\"\n");
        let started = Instant::now();
        for frame in input.as_bytes().chunks(MAX_INPUT_FRAME) {
            if self.io.send(Bytes::copy_from_slice(frame)).await.is_err() {
                self.closed = true;
                return Ok(Ending::Closed {
                    output: self.take_all(),
                });
            }
        }
        let deadline = started + limits.total;
        loop {
            if let Some(output) = self.take_finished(&end) {
                return Ok(Ending::Finished(output));
            }
            if self.pending.len() > MAX_OUTPUT {
                return Err(ShellFailure::OutputTooLarge);
            }
            let wake = deadline.min(Instant::now() + limits.idle);
            match tokio::time::timeout_at(wake, self.io.recv()).await {
                Ok(Some(bytes)) => self.pending.extend_from_slice(&bytes),
                Ok(None) => {
                    self.closed = true;
                    return Ok(Ending::Closed {
                        output: self.take_all(),
                    });
                }
                Err(_) => {
                    return Err(ShellFailure::TimedOut {
                        waited: started.elapsed(),
                        output: self.take_all(),
                    });
                }
            }
        }
    }

    /// Closes the shell.
    pub async fn close(mut self) {
        if !self.closed {
            self.io.close().await;
        }
    }

    fn take_all(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.pending)).into_owned()
    }

    fn take_finished(&mut self, end: &str) -> Option<CommandOutput> {
        let at = find(&self.pending, end.as_bytes())?;
        let after = at + end.len();
        let newline = after + self.pending[after..].iter().position(|&b| b == b'\n')?;
        let status = std::str::from_utf8(&self.pending[after..newline])
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(-1);
        let text = String::from_utf8_lossy(&self.pending[..at]).into_owned();
        self.pending.drain(..=newline);
        Some(CommandOutput { text, status })
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A shell that answers each command with scripted pieces of output, possibly split
    /// across frames, and ends each with the marker it was sent.
    struct Scripted {
        replies: VecDeque<Vec<&'static str>>,
        out: VecDeque<Bytes>,
    }

    impl ShellIo for Scripted {
        async fn send(&mut self, bytes: Bytes) -> Result<(), ShellClosed> {
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let marker_line = text.lines().last().unwrap();
            let marker = marker_line
                .strip_prefix("echo \"")
                .unwrap()
                .strip_suffix("$?\"")
                .unwrap();
            let Some(pieces) = self.replies.pop_front() else {
                return Ok(());
            };
            for piece in pieces {
                self.out.push_back(Bytes::from(piece));
            }
            // The status arrives split from its marker.
            let (a, b) = marker.split_at(5);
            self.out.push_back(Bytes::from(a.to_owned()));
            self.out.push_back(Bytes::from(format!("{b}3\n")));
            Ok(())
        }

        async fn recv(&mut self) -> Option<Bytes> {
            match self.out.pop_front() {
                Some(bytes) => Some(bytes),
                None => std::future::pending().await,
            }
        }

        async fn close(&mut self) {}
    }

    #[tokio::test(start_paused = true)]
    async fn output_and_status_are_split_at_the_marker() {
        let mut shell = DeviceShell::new(Scripted {
            replies: VecDeque::from([vec!["hel", "lo\nwor", "ld"], vec![]]),
            out: VecDeque::new(),
        });
        let limits = CommandLimits::within(Duration::from_secs(5));
        let out = shell
            .run("echo hello; echo -n world", limits)
            .await
            .unwrap();
        assert_eq!(out.text, "hello\nworld");
        assert_eq!(out.status, 3);
        assert!(out.has_line("hello"));
        let out = shell.run(":", limits).await.unwrap();
        assert_eq!(out.text, "");
        // No reply at all: the command times out with its bound.
        let failure = shell.run(":", limits).await.unwrap_err();
        assert_eq!(
            failure,
            ShellFailure::TimedOut {
                waited: Duration::from_secs(5),
                output: String::new()
            }
        );
    }
}
