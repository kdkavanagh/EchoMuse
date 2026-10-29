//! A simulated device link for tests: records every command and frame in
//! order, models the audio writer's queue, and lets a test mark the packet
//! being written.

use std::collections::VecDeque;

use echomuse_protocol::MessageId;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::link::{CommandLink, FramePurge, FrameRefused, LinkClosed, RenderFrame, RenderLink};
use crate::wire::{AckOutcome, Command, RenderReport};

/// One thing the client sent, in order.
#[derive(Clone, Debug)]
pub enum Sent {
    Command {
        id: MessageId,
        command: Command,
        generation: u32,
    },
    Frame(RenderFrame),
}

pub struct SimLink {
    pub sent: Vec<Sent>,
    /// Frames queued and not yet written.
    pub queue: VecDeque<RenderFrame>,
    /// The frame being written, whose delivery is uncertain.
    pub writing: Option<RenderFrame>,
    pub capacity: usize,
    pub closed: bool,
    /// Whether `withdraw_frames` reports exactly.
    pub exact_purge: bool,
}

impl SimLink {
    pub fn new() -> Self {
        Self {
            sent: Vec::new(),
            queue: VecDeque::new(),
            writing: None,
            capacity: 1_000,
            closed: false,
            exact_purge: true,
        }
    }

    pub fn commands(&self) -> Vec<(MessageId, Command, u32)> {
        self.sent
            .iter()
            .filter_map(|sent| match sent {
                Sent::Command {
                    id,
                    command,
                    generation,
                } => Some((id.clone(), command.clone(), *generation)),
                Sent::Frame(_) => None,
            })
            .collect()
    }

    pub fn last_command(&self) -> (MessageId, Command, u32) {
        self.commands().pop().expect("a command was sent")
    }

    pub fn frames(&self) -> Vec<RenderFrame> {
        self.sent
            .iter()
            .filter_map(|sent| match sent {
                Sent::Frame(frame) => Some(frame.clone()),
                Sent::Command { .. } => None,
            })
            .collect()
    }

    /// The writer takes the head of the queue and starts writing it.
    pub fn begin_write(&mut self) {
        self.writing = self.queue.pop_front();
    }

    /// The writer drains everything queued.
    pub fn write_all(&mut self) {
        self.writing = None;
        self.queue.clear();
    }
}

impl CommandLink for SimLink {
    fn request(&mut self, command: &Command, generation: u32) -> Result<MessageId, LinkClosed> {
        if self.closed {
            return Err(LinkClosed);
        }
        let id = MessageId::generate();
        self.sent.push(Sent::Command {
            id: id.clone(),
            command: command.clone(),
            generation,
        });
        Ok(id)
    }
}

impl RenderLink for SimLink {
    fn queue_frame(&mut self, frame: RenderFrame) -> Result<(), FrameRefused> {
        if self.closed {
            return Err(FrameRefused::Closed);
        }
        if self.queue.len() >= self.capacity {
            return Err(FrameRefused::Full(frame));
        }
        self.sent.push(Sent::Frame(frame.clone()));
        self.queue.push_back(frame);
        Ok(())
    }

    fn withdraw_frames(&mut self, playback: &CancellationToken) -> FramePurge {
        let before = self.queue.len();
        self.queue.retain(|frame| !frame.playback.is_cancelled());
        let withdrawn = (before - self.queue.len()) as u64;
        let in_write = self
            .writing
            .take_if(|frame| frame.playback.is_cancelled())
            .map(|frame| frame.sequence);
        let _ = playback;
        if self.exact_purge {
            FramePurge::Exact {
                withdrawn,
                in_write,
            }
        } else {
            FramePurge::Unknown
        }
    }
}

pub fn accepted() -> AckOutcome {
    AckOutcome::Accepted
}

pub fn rejected(code: &str) -> AckOutcome {
    AckOutcome::Rejected {
        code: Some(code.to_owned()),
    }
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

/// A `render.progress` body as the firmware writes it.
pub fn progress(playback_id: &str, event: &str, completed: u64) -> RenderReport {
    let body = object(json!({
        "playback_id": playback_id, "event": event,
        "submitted_frames": completed.to_string(), "completed_frames": completed.to_string(),
        "mono_ns": "1000", "uncertainty_us": 42700, "timing_quality": "estimated",
        "reference_coverage": "full"
    }));
    RenderReport::Progress(crate::wire::RenderProgress::parse(&body).unwrap())
}

/// A `render.finished` body as the firmware writes it.
pub fn finished(playback_id: &str, reason: &str, last: u64) -> RenderReport {
    let body = object(json!({
        "playback_id": playback_id, "last_completed_frame": last.to_string(),
        "reason": reason, "timing_quality": "estimated"
    }));
    RenderReport::Finished(crate::wire::RenderFinished::parse(&body).unwrap())
}
