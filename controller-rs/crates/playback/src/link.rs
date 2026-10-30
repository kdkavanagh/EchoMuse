//! The narrow view of the device link this crate needs.
//!
//! The device link crate owns the sockets, envelopes and acknowledgement
//! correlation. Playback needs two things from it: queue a command the device
//! acknowledges, and queue a kind-3 render frame without waiting, which the
//! link's writer drops unwritten once its playback is cancelled. Reports travel the other
//! way through the device runtime: it receives `command.ack`,
//! `render.progress` and `render.finished` in ingress order and hands them to
//! [`RenderClient`](crate::RenderClient) and [`FocusLease`](crate::FocusLease)
//! as [`AckOutcome`](crate::AckOutcome) and
//! [`RenderReport`](crate::RenderReport) values. Nothing here waits.

use std::ops::Range;

use bytes::Bytes;
use echomuse_protocol::MessageId;
use tokio_util::sync::CancellationToken;

use crate::wire::Command;

/// The session has ended; nothing more can be sent on it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("the device session has ended")]
pub struct LinkClosed;

/// Sends commands the device acknowledges.
pub trait CommandLink {
    /// Queues `command` in an envelope carrying `generation` and returns its
    /// `message_id`. The device's `command.ack`, or the reason none will
    /// come, is later handed back with that id. Never waits.
    fn request(&mut self, command: &Command, generation: u32) -> Result<MessageId, LinkClosed>;
}

/// One kind-3 render frame for the audio socket.
#[derive(Clone, Debug)]
pub struct RenderFrame {
    /// The whole EMA1 frame.
    pub bytes: Bytes,
    /// The playback's token. A frame whose token is cancelled before it is
    /// written must not be written.
    pub playback: CancellationToken,
    /// The packet's sequence number within its epoch, from 0.
    pub sequence: u64,
    /// The packet's first source frame.
    pub first_frame: u64,
    /// The number of frames it carries.
    pub frames: u32,
}

impl RenderFrame {
    /// The source frames it carries.
    pub fn frame_range(&self) -> Range<u64> {
        self.first_frame..self.first_frame + u64::from(self.frames)
    }
}

/// Why a frame was not queued.
#[derive(Debug)]
pub enum FrameRefused {
    /// The audio writer queue is full; the frame is handed back to retry.
    Full(RenderFrame),
    /// The session has ended.
    Closed,
}

/// Sends render frames as well as commands.
pub trait RenderLink: CommandLink {
    /// Queues one frame without waiting.
    fn queue_frame(&mut self, frame: RenderFrame) -> Result<(), FrameRefused>;
}
