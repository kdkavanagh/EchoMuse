//! The narrow view of the device link this crate needs.
//!
//! The device link crate owns the sockets, envelopes and acknowledgement
//! correlation. Playback needs three things from it: queue a command the device
//! acknowledges, queue a kind-3 render frame without waiting, and withdraw a
//! cancelled playback's frames that are still queued. Reports travel the other
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

/// What withdrawing a cancelled playback's frames achieved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramePurge {
    /// The link knows exactly: the newest `withdrawn` frames it had queued
    /// for the playback were dropped unwritten, and `in_write` is the
    /// sequence number of the packet that was being written, whose delivery
    /// is uncertain.
    Exact {
        /// Packets dropped unwritten.
        withdrawn: u64,
        /// The packet being written at the moment of cancellation.
        in_write: Option<u64>,
    },
    /// The link drops the playback's queued frames but cannot say which had
    /// already been written.
    Unknown,
}

/// Sends render frames as well as commands.
pub trait RenderLink: CommandLink {
    /// Queues one frame without waiting.
    fn queue_frame(&mut self, frame: RenderFrame) -> Result<(), FrameRefused>;

    /// Called after `playback` has been cancelled: drops that playback's
    /// frames that are still queued and reports what happened. A link whose
    /// writer already skips frames with a cancelled token, and cannot tell
    /// which were written, keeps this default.
    fn withdraw_frames(&mut self, playback: &CancellationToken) -> FramePurge {
        debug_assert!(playback.is_cancelled());
        FramePurge::Unknown
    }
}
