//! `command.ack` correlation (spec §5.2): one waiter table per session, owned by the
//! session's control task, which follows each acknowledged command from enqueued to
//! written to acknowledged or failed.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use echomuse_protocol::MessageId;
use echomuse_protocol::session::CommandAck;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::time::DelayQueue;
use tokio_util::time::delay_queue::Key;

use crate::session::{CloseReason, SessionCore};

/// Where one acknowledged command stands.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandProgress {
    /// Queued for the control socket.
    Enqueued,
    /// Written to the control socket; no acknowledgement yet.
    Written,
    /// The device acknowledged it (`accepted`, `applied`, `durable` or `rejected`).
    Acknowledged(CommandAck),
    /// It will not be acknowledged on this session.
    Failed(CommandFailure),
}

impl CommandProgress {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Acknowledged(_) | Self::Failed(_))
    }
}

/// Why a command will not be acknowledged.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandFailure {
    /// No acknowledgement arrived before the deadline. The device may still have
    /// received and applied the command.
    Timeout {
        /// Whether the frame had been written to the socket.
        written: bool,
    },
    /// The session ended first. If the frame was written, the device may have applied
    /// it.
    SessionEnded {
        /// Why the session ended.
        reason: CloseReason,
        /// Whether the frame had been written to the socket.
        written: bool,
    },
}

impl fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { written } => {
                write!(f, "no acknowledgement in time (written: {written})")
            }
            Self::SessionEnded { reason, written } => {
                write!(f, "session ended: {reason} (written: {written})")
            }
        }
    }
}

impl std::error::Error for CommandFailure {}

/// A command sent with [`SessionLink::request`](crate::SessionLink::request).
#[derive(Debug)]
pub struct PendingAck {
    message_id: MessageId,
    progress: watch::Receiver<CommandProgress>,
    core: Arc<SessionCore>,
}

impl PendingAck {
    pub(crate) fn new(
        message_id: MessageId,
        progress: watch::Receiver<CommandProgress>,
        core: Arc<SessionCore>,
    ) -> Self {
        Self {
            message_id,
            progress,
            core,
        }
    }

    /// The command's `message_id`, which the device's `command.ack` echoes.
    pub fn message_id(&self) -> &MessageId {
        &self.message_id
    }

    /// Where the command stands now.
    pub fn progress(&self) -> CommandProgress {
        self.progress.borrow().clone()
    }

    /// Waits until the frame is written (or already acknowledged).
    pub async fn written(&mut self) -> Result<(), CommandFailure> {
        let core = Arc::clone(&self.core);
        let seen = self
            .progress
            .wait_for(|p| !matches!(p, CommandProgress::Enqueued))
            .await
            .map(|p| p.clone());
        match seen {
            Ok(CommandProgress::Failed(failure)) => Err(failure),
            Ok(_) => Ok(()),
            Err(_) => Err(abandoned(&core, false)),
        }
    }

    /// Waits for the device's acknowledgement.
    pub async fn acknowledged(mut self) -> Result<CommandAck, CommandFailure> {
        let seen = self
            .progress
            .wait_for(CommandProgress::is_terminal)
            .await
            .map(|p| p.clone());
        match seen {
            Ok(CommandProgress::Acknowledged(ack)) => Ok(ack),
            Ok(CommandProgress::Failed(failure)) => Err(failure),
            Ok(_) => unreachable!("wait_for returns a terminal state"),
            Err(_) => {
                let written = matches!(*self.progress.borrow(), CommandProgress::Written);
                Err(abandoned(&self.core, written))
            }
        }
    }
}

/// Every holder of the command's state is gone without recording an outcome, which
/// happens only when the session ended with the frame still queued or in flight.
fn abandoned(core: &SessionCore, written: bool) -> CommandFailure {
    CommandFailure::SessionEnded {
        reason: core.ended_reason(),
        written,
    }
}

/// A waiter, sent to the control task before its frame is queued.
pub(crate) struct Registration {
    pub(crate) message_id: MessageId,
    pub(crate) progress: Arc<watch::Sender<CommandProgress>>,
    pub(crate) deadline: Instant,
}

/// Marks a command written, unless it already has an outcome.
pub(crate) fn mark_written(progress: &watch::Sender<CommandProgress>) {
    progress.send_if_modified(|p| {
        if matches!(p, CommandProgress::Enqueued) {
            *p = CommandProgress::Written;
            true
        } else {
            false
        }
    });
}

fn fail(progress: &watch::Sender<CommandProgress>, failure: impl FnOnce(bool) -> CommandFailure) {
    progress.send_if_modified(|p| {
        if p.is_terminal() {
            return false;
        }
        let written = matches!(p, CommandProgress::Written);
        *p = CommandProgress::Failed(failure(written));
        true
    });
}

struct Waiter {
    progress: Arc<watch::Sender<CommandProgress>>,
    key: Key,
}

/// The session's waiters, owned by its control task.
pub(crate) struct AckTable {
    waiters: HashMap<MessageId, Waiter>,
    deadlines: DelayQueue<MessageId>,
}

impl AckTable {
    pub(crate) fn new() -> Self {
        Self {
            waiters: HashMap::new(),
            deadlines: DelayQueue::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.waiters.is_empty()
    }

    pub(crate) fn insert(&mut self, registration: Registration) {
        let key = self
            .deadlines
            .insert_at(registration.message_id.clone(), registration.deadline);
        let waiter = Waiter {
            progress: registration.progress,
            key,
        };
        if let Some(old) = self.waiters.insert(registration.message_id, waiter) {
            // UUIDv4 message ids do not repeat; keep the table consistent if one did.
            self.deadlines.remove(&old.key);
        }
    }

    /// Resolves the waiter `ack` names. An acknowledgement for no waiter (a command
    /// sent without one, or one that already timed out) is dropped. Registrations still
    /// queued are taken first, since a waiter is always registered before its frame
    /// could reach the device.
    pub(crate) fn resolve(&mut self, ack: CommandAck, pending: &mut mpsc::Receiver<Registration>) {
        if !self.waiters.contains_key(&ack.message_id) {
            while let Ok(registration) = pending.try_recv() {
                self.insert(registration);
            }
        }
        let Some(waiter) = self.waiters.remove(&ack.message_id) else {
            tracing::debug!(message_id = %ack.message_id, "command.ack matches no waiter");
            return;
        };
        self.deadlines.remove(&waiter.key);
        waiter.progress.send_if_modified(|p| {
            if p.is_terminal() {
                return false;
            }
            *p = CommandProgress::Acknowledged(ack);
            true
        });
    }

    /// Waits for the next deadline and fails its waiter. Pending forever while the
    /// table is empty, so guard it with [`is_empty`](Self::is_empty).
    pub(crate) async fn expire_next(&mut self) {
        let Some(expired) = std::future::poll_fn(|cx| self.deadlines.poll_expired(cx)).await else {
            return;
        };
        if let Some(waiter) = self.waiters.remove(expired.get_ref()) {
            fail(&waiter.progress, |written| CommandFailure::Timeout {
                written,
            });
        }
    }

    /// Fails every waiter, including registrations not yet taken: the session ended.
    pub(crate) fn fail_all(
        &mut self,
        reason: &CloseReason,
        pending: &mut mpsc::Receiver<Registration>,
    ) {
        pending.close();
        while let Ok(registration) = pending.try_recv() {
            self.insert(registration);
        }
        for (_, waiter) in self.waiters.drain() {
            fail(&waiter.progress, |written| CommandFailure::SessionEnded {
                reason: reason.clone(),
                written,
            });
        }
        self.deadlines.clear();
    }
}
