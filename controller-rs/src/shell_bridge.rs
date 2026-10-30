//! The dashboard's browser terminal (spec §3.1.9, §4.10): `WS /api/devices/{id}/shell`
//! bridged to the device's shell dial-back through the controller's [`ShellBroker`].
//!
//! The controller asks for a pseudo-terminal (`shell_open {pty: true}`) and waits for
//! the dial-back; the device falls back to a pipe when it cannot allocate one, and the
//! route tells the browser which it got (`shell_meta`). The device keeps one shell at a
//! time, so opening one closes any other, including a firmware transfer's; the route
//! refuses while a firmware operation runs on the device. Shell traffic is never
//! recorded or logged.

use std::time::Duration;

use bytes::Bytes;
use echomuse_api::{BoxFuture, ServiceError, ShellAccess, ShellChannels, ShellFrame};
use echomuse_core::types::DeviceId;
use echomuse_link::{ShellBroker, ShellInput, ShellMode, ShellOpen};
use tokio::sync::{mpsc, watch};

use crate::{app::Directory, firmware_control::Firmware, supervision::TaskSupervisor};

/// How long the device may take to dial back.
const DIAL_BACK_TIMEOUT: Duration = Duration::from_secs(10);
/// Frames buffered each way.
const FRAMES: usize = 64;

/// [`ShellAccess`] over the runtimes' sessions and the shell broker.
pub struct ShellBridge {
    pub directory: watch::Receiver<Directory>,
    pub shells: ShellBroker,
    /// The firmware service, whose operations use the device's one shell.
    pub firmware: Firmware,
    pub tasks: TaskSupervisor,
}

impl ShellBridge {
    async fn open_now(&self, device: DeviceId) -> Result<ShellChannels, ServiceError> {
        if self.firmware.is_busy(&device) {
            return Err(ServiceError::Busy(
                "A firmware operation is using the device's shell".to_owned(),
            ));
        }
        let handle = self
            .directory
            .borrow()
            .get(&device)
            .cloned()
            .ok_or(ServiceError::UnknownDevice)?;
        let link = handle
            .snapshot()
            .session_link
            .ok_or(ServiceError::NotConnected)?;
        let pending = self.shells.expect(device.clone());
        link.send(&ShellOpen { pty: true }, 0)
            .map_err(|_| ServiceError::NotConnected)?;
        let connection = match tokio::time::timeout(DIAL_BACK_TIMEOUT, pending.connected()).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(_)) => {
                return Err(ServiceError::Busy(
                    "Another shell to the device replaced this one".to_owned(),
                ));
            }
            Err(_) => {
                return Err(ServiceError::Upstream(
                    "The device did not open its shell".to_owned(),
                ));
            }
        };
        let pty = connection.mode() == ShellMode::Pty;
        let (input_tx, mut input_rx) = mpsc::channel::<ShellFrame>(FRAMES);
        let (output_tx, output_rx) = mpsc::channel::<Bytes>(FRAMES);
        let (mut writer, mut reader) = connection.split();
        self.tasks.spawn("dashboard shell", async move {
            loop {
                tokio::select! {
                    frame = input_rx.recv() => match frame {
                        Some(frame) => {
                            let input = match frame {
                                ShellFrame::Data(data) => ShellInput::Data(data),
                                ShellFrame::Resize { cols, rows } => ShellInput::Resize { cols, rows },
                            };
                            match writer.send(input).await {
                                Ok(()) => {}
                                // A resize means nothing to a pipe shell.
                                Err(echomuse_link::ShellError::ResizeInPipeMode) => {}
                                Err(_) => break,
                            }
                        }
                        None => break,
                    },
                    out = reader.recv() => match out {
                        Some(bytes) => {
                            if output_tx.send(bytes).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            writer.close().await;
        });
        Ok(ShellChannels {
            pty,
            input: input_tx,
            output: output_rx,
        })
    }
}

impl ShellAccess for ShellBridge {
    fn open(&self, device: DeviceId) -> BoxFuture<'_, Result<ShellChannels, ServiceError>> {
        Box::pin(self.open_now(device))
    }
}
