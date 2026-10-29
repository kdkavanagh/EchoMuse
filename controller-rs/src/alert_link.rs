//! The device link as the alert service uses it ([`AlertLink`]): bodies go on the
//! session the service addressed, and only while that session is still the device's
//! attached one.

use std::time::Duration;

use echomuse_alerts::{
    AlertLink, CommandOutcome, LinkRefusal, SessionRef,
    wire::{CommandBody, PostBody},
};
use echomuse_core::services::UnknownCause;
use echomuse_link::{CommandFailure, SendError, SessionLink};
use tokio::sync::watch;

use crate::app::Directory;

/// How long the device has to acknowledge an alert command.
const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// [`AlertLink`] over the runtimes' published sessions.
pub struct DeviceAlertLink {
    directory: watch::Receiver<Directory>,
}

impl DeviceAlertLink {
    pub fn new(directory: watch::Receiver<Directory>) -> Self {
        Self { directory }
    }

    /// The session's link, when `to` is still the device's attached session.
    fn current(&self, to: &SessionRef) -> Option<SessionLink> {
        let snapshot = self.directory.borrow().get(&to.device)?.snapshot();
        (snapshot.session == Some(to.session))
            .then_some(snapshot.session_link)
            .flatten()
    }
}

fn refusal(error: SendError) -> LinkRefusal {
    match error {
        SendError::SessionEnded(_) => LinkRefusal::NotCurrent,
        SendError::Overloaded => LinkRefusal::Overloaded,
        // A body the alert crate built always encodes; treat a failure as unsendable on
        // this session rather than losing it silently.
        SendError::Encode(error) => {
            tracing::error!(%error, "an alert body did not encode");
            LinkRefusal::NotCurrent
        }
    }
}

impl AlertLink for DeviceAlertLink {
    fn post(&self, to: &SessionRef, body: PostBody) -> Result<(), LinkRefusal> {
        let link = self.current(to).ok_or(LinkRefusal::NotCurrent)?;
        let sent = match &body {
            PostBody::Snapshot(page) => link.send(page, 0),
            PostBody::Delta(delta) => link.send(delta, 0),
            PostBody::OpResult(result) => link.send(result, 0),
            PostBody::TimerView(view) => link.send(view, 0),
        };
        sent.map(|_| ()).map_err(refusal)
    }

    async fn command(&self, to: &SessionRef, body: CommandBody) -> CommandOutcome {
        let Some(link) = self.current(to) else {
            return CommandOutcome::NotSent(LinkRefusal::NotCurrent);
        };
        let pending = match &body {
            CommandBody::Act(act) => link.request(act, 0, ACK_TIMEOUT),
            CommandBody::Ring(ring) => link.request(ring, 0, ACK_TIMEOUT),
            CommandBody::Prefetch(prefetch) => link.request(prefetch, 0, ACK_TIMEOUT),
        };
        let pending = match pending {
            Ok(pending) => pending,
            Err(error) => return CommandOutcome::NotSent(refusal(error)),
        };
        match pending.acknowledged().await {
            Ok(ack) => CommandOutcome::Acked {
                status: ack.status,
                error: ack.error,
            },
            // Never written: nothing reached the device.
            Err(CommandFailure::Timeout { written: false })
            | Err(CommandFailure::SessionEnded { written: false, .. }) => {
                CommandOutcome::NotSent(LinkRefusal::NotCurrent)
            }
            Err(CommandFailure::Timeout { written: true }) => {
                CommandOutcome::Unknown(UnknownCause::Timeout)
            }
            Err(CommandFailure::SessionEnded { written: true, .. }) => {
                CommandOutcome::Unknown(UnknownCause::ConnectionLost)
            }
        }
    }
}
