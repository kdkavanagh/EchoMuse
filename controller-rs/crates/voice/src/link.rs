//! The device link as the playback crate sees it, for a machine that performs no I/O:
//! every command becomes an [`Effect`] the runtime sends, and the command's
//! correlation id is chosen here so the runtime can quote it back in [`Input::Ack`].
//!
//! [`Input::Ack`]: crate::io::Input::Ack

use echomuse_playback::{
    CommandLink, LinkClosed, RenderFrame, RenderLink, wire::Command as PlaybackCommand,
};
use echomuse_protocol::MessageId;

use crate::io::{Effect, Outbound};

/// Collects what the playback state machines send. Once the session has ended every
/// send fails with [`LinkClosed`], as the real link would.
#[derive(Debug, Default)]
pub(crate) struct EffectLink {
    pub(crate) effects: Vec<Effect>,
    pub(crate) closed: bool,
}

impl EffectLink {
    /// Queues a message the device does not acknowledge.
    pub(crate) fn send(&mut self, body: Outbound, generation: u32) {
        if !self.closed {
            self.effects.push(Effect::Send { body, generation });
        }
    }

    /// Queues a message the device acknowledges; returns the id its answer quotes.
    pub(crate) fn request(&mut self, body: Outbound, generation: u32) -> Option<MessageId> {
        if self.closed {
            return None;
        }
        let id = MessageId::generate();
        self.effects.push(Effect::Request {
            id: id.clone(),
            body,
            generation,
        });
        Some(id)
    }
}

impl CommandLink for EffectLink {
    fn request(
        &mut self,
        command: &PlaybackCommand,
        generation: u32,
    ) -> Result<MessageId, LinkClosed> {
        EffectLink::request(self, Outbound::Playback(command.clone()), generation).ok_or(LinkClosed)
    }
}

impl RenderLink for EffectLink {
    /// Always takes the frame: the runtime's writer queues it and waits for room. The
    /// render client's own window (at most 258,304 frames ahead of the device's progress)
    /// bounds how many can wait.
    fn queue_frame(&mut self, frame: RenderFrame) -> Result<(), echomuse_playback::FrameRefused> {
        if self.closed {
            return Err(echomuse_playback::FrameRefused::Closed);
        }
        self.effects.push(Effect::RenderFrame(frame));
        Ok(())
    }
}
