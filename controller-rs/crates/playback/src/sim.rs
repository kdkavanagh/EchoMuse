//! A simulated device link for tests: records every command and frame in
//! order and models the audio writer's queue.

use std::collections::VecDeque;

use echomuse_protocol::MessageId;
use serde_json::{Map, Value, json};

use crate::link::{CommandLink, FrameRefused, LinkClosed, RenderFrame, RenderLink};
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
    pub capacity: usize,
    pub closed: bool,
}

impl SimLink {
    pub fn new() -> Self {
        Self {
            sent: Vec::new(),
            queue: VecDeque::new(),
            capacity: 1_000,
            closed: false,
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

    /// The writer drains everything queued.
    pub fn write_all(&mut self) {
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
