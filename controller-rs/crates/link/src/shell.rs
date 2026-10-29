//! The device shell (spec §3.1.9, §3.2): after `shell_open`, the device dials back to
//! `/shell/{device_id}`, with `?pty=1` when it attached a pseudo-terminal.
//!
//! A dial-back is accepted only while someone is waiting for that device's shell
//! ([`ShellBroker::expect`]); anything else is refused with 403 before the upgrade.
//! Each device has at most one shell: a new dial-back closes the previous one. Shells
//! are independent of control sessions, as on the device, so a shell survives a
//! control reconnect.
//!
//! Framing: in pipe mode both directions carry raw bytes. In PTY mode a
//! controller-to-device frame is `0x00` followed by input bytes, or `0x01` followed by
//! columns and rows as big-endian u16; device output is raw bytes.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::{BufMut, Bytes, BytesMut};
use echomuse_core::types::DeviceId;
use echomuse_protocol::{ControlBody, MessageType};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::control::{Ws, WsSink, WsStream};

/// `shell_open`: asks the device to dial back with a shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ShellOpen {
    /// Ask for a pseudo-terminal. The device falls back to a pipe when it cannot
    /// allocate one, and says which it attached when it dials back.
    pub pty: bool,
}

impl ControlBody for ShellOpen {
    const MESSAGE_TYPE: MessageType = MessageType::ShellOpen;
}

/// `shell_close`: asks the device to end its shell.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ShellClose {}

impl ControlBody for ShellClose {
    const MESSAGE_TYPE: MessageType = MessageType::ShellClose;
}

/// How the device attached its shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellMode {
    /// Plain pipes; raw bytes both ways.
    Pipe,
    /// A pseudo-terminal; framed input, resizable.
    Pty,
}

/// One input for the device's shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellInput {
    /// Bytes for the shell's standard input.
    Data(Bytes),
    /// A terminal size change (PTY mode only).
    Resize {
        /// Columns.
        cols: u16,
        /// Rows.
        rows: u16,
    },
}

impl ShellInput {
    /// The frame for `mode`.
    pub fn encode(&self, mode: ShellMode) -> Result<Bytes, ShellError> {
        match (self, mode) {
            (Self::Data(data), ShellMode::Pipe) => Ok(data.clone()),
            (Self::Data(data), ShellMode::Pty) => {
                let mut frame = BytesMut::with_capacity(1 + data.len());
                frame.put_u8(0x00);
                frame.put_slice(data);
                Ok(frame.freeze())
            }
            (Self::Resize { cols, rows }, ShellMode::Pty) => {
                let mut frame = BytesMut::with_capacity(5);
                frame.put_u8(0x01);
                frame.put_u16(*cols);
                frame.put_u16(*rows);
                Ok(frame.freeze())
            }
            (Self::Resize { .. }, ShellMode::Pipe) => Err(ShellError::ResizeInPipeMode),
        }
    }
}

/// Why a shell operation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellError {
    /// A newer expectation or dial-back replaced this one, or the server shut down.
    Superseded,
    /// The shell's socket closed.
    Closed,
    /// A pipe shell has no terminal to resize.
    ResizeInPipeMode,
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Superseded => "replaced by a newer shell",
            Self::Closed => "the shell closed",
            Self::ResizeInPipeMode => "a pipe shell cannot be resized",
        })
    }
}

impl std::error::Error for ShellError {}

#[derive(Default)]
struct Slot {
    waiting: Option<oneshot::Sender<ShellConnection>>,
    live: Option<CancellationToken>,
}

/// The per-device shell routing table. Cheap to clone.
///
/// Like the session registry, this is routing for HTTP upgrades that must be answered
/// before upgrading; the lock is held only for map operations.
#[derive(Clone)]
pub struct ShellBroker {
    slots: Arc<Mutex<HashMap<DeviceId, Slot>>>,
    shutdown: CancellationToken,
}

impl fmt::Debug for ShellBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellBroker").finish_non_exhaustive()
    }
}

impl ShellBroker {
    pub(crate) fn new(shutdown: CancellationToken) -> Self {
        Self {
            slots: Arc::default(),
            shutdown,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<DeviceId, Slot>> {
        self.slots.lock().expect("shell broker lock poisoned")
    }

    /// Waits for `device`'s next dial-back. Call it before sending [`ShellOpen`], so
    /// the dial-back cannot arrive first. An earlier expectation for the same device
    /// fails with [`ShellError::Superseded`]. Dropping the returned value withdraws the
    /// expectation.
    pub fn expect(&self, device: DeviceId) -> ShellPending {
        let (deliver, receiver) = oneshot::channel();
        self.lock().entry(device).or_default().waiting = Some(deliver);
        ShellPending { receiver }
    }

    /// Whether a dial-back from `device` would be taken.
    pub(crate) fn is_expected(&self, device: &DeviceId) -> bool {
        self.lock()
            .get(device)
            .and_then(|slot| slot.waiting.as_ref())
            .is_some_and(|deliver| !deliver.is_closed())
    }

    /// Hands an upgraded dial-back to whoever expects it, closing the device's previous
    /// shell.
    pub(crate) fn deliver(&self, device: &DeviceId, ws: Ws, mode: ShellMode) {
        let stop = self.shutdown.child_token();
        let deliver = {
            let mut slots = self.lock();
            let slot = slots.entry(device.clone()).or_default();
            let Some(deliver) = slot.waiting.take() else {
                return;
            };
            if let Some(previous) = slot.live.replace(stop.clone()) {
                previous.cancel();
            }
            deliver
        };
        let (sink, stream) = ws.split();
        let connection = ShellConnection {
            mode,
            writer: ShellWriter {
                mode,
                sink,
                stop: stop.clone(),
            },
            reader: ShellReader { stream, stop },
        };
        if let Err(connection) = deliver.send(connection) {
            connection.reader.stop.cancel();
        }
    }
}

/// A shell dial-back being waited for.
#[derive(Debug)]
pub struct ShellPending {
    receiver: oneshot::Receiver<ShellConnection>,
}

impl ShellPending {
    /// The connected shell. Bound it with a timeout: the device may never dial back.
    pub async fn connected(self) -> Result<ShellConnection, ShellError> {
        self.receiver.await.map_err(|_| ShellError::Superseded)
    }
}

/// A connected device shell.
pub struct ShellConnection {
    mode: ShellMode,
    writer: ShellWriter,
    reader: ShellReader,
}

impl fmt::Debug for ShellConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellConnection")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl ShellConnection {
    /// How the device attached the shell.
    pub fn mode(&self) -> ShellMode {
        self.mode
    }

    /// Sends one input.
    pub async fn send(&mut self, input: ShellInput) -> Result<(), ShellError> {
        self.writer.send(input).await
    }

    /// The next piece of shell output; `None` once the shell has closed.
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.reader.recv().await
    }

    /// Splits into independently usable halves.
    pub fn split(self) -> (ShellWriter, ShellReader) {
        (self.writer, self.reader)
    }

    /// Bridges the shell to a caller's byte stream, such as a dashboard terminal:
    /// inputs from `input` go to the device and its output goes to `output`, until
    /// either side ends. The shell's socket is closed on return.
    pub async fn bridge<I, O>(self, mut input: I, mut output: O) -> Result<(), ShellError>
    where
        I: Stream<Item = ShellInput> + Unpin,
        O: Sink<Bytes> + Unpin,
    {
        let (mut writer, mut reader) = self.split();
        let result = loop {
            tokio::select! {
                next = input.next() => match next {
                    Some(item) => {
                        if let Err(error) = writer.send(item).await {
                            break Err(error);
                        }
                    }
                    None => break Ok(()),
                },
                out = reader.recv() => match out {
                    Some(bytes) => {
                        if output.send(bytes).await.is_err() {
                            break Ok(());
                        }
                    }
                    None => break Ok(()),
                },
            }
        };
        writer.close().await;
        result
    }
}

/// The input half of a shell.
pub struct ShellWriter {
    mode: ShellMode,
    sink: WsSink,
    stop: CancellationToken,
}

impl fmt::Debug for ShellWriter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellWriter")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl ShellWriter {
    /// Sends one input, framed for the shell's mode.
    pub async fn send(&mut self, input: ShellInput) -> Result<(), ShellError> {
        let frame = input.encode(self.mode)?;
        if self.stop.is_cancelled() {
            return Err(ShellError::Superseded);
        }
        tokio::select! {
            biased;
            _ = self.stop.cancelled() => Err(ShellError::Superseded),
            sent = self.sink.send(Message::Binary(frame)) => sent.map_err(|_| ShellError::Closed),
        }
    }

    /// Closes the shell's socket.
    pub async fn close(&mut self) {
        self.stop.cancel();
        let _ = self.sink.send(Message::Close(None)).await;
    }
}

/// The output half of a shell.
pub struct ShellReader {
    stream: WsStream,
    stop: CancellationToken,
}

impl fmt::Debug for ShellReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShellReader").finish_non_exhaustive()
    }
}

impl ShellReader {
    /// The next piece of shell output; `None` once the shell has closed or been
    /// replaced.
    pub async fn recv(&mut self) -> Option<Bytes> {
        loop {
            let message = tokio::select! {
                biased;
                _ = self.stop.cancelled() => return None,
                message = self.stream.next() => message,
            };
            match message? {
                Ok(Message::Binary(bytes)) => return Some(bytes),
                Ok(Message::Text(text)) => return Some(Bytes::from(text)),
                Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
                Ok(Message::Close(_)) | Err(_) => return None,
            }
        }
    }
}
