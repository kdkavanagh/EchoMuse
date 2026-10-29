//! Dialog focus leases (spec §3.1.5, `docs/protocol-v1.md` §4.3).
//!
//! While a turn listens or speaks, the controller holds a `dialog_input` or
//! `dialog_output` lease on the device. The device ducks content and
//! backgrounds the foreground alert while any lease lives, and on expiry
//! cancels the dialog output its owner started. A lease lives
//! [`FOCUS_TTL`] from the device's receipt of its latest `focus.acquire` or
//! `focus.renew`, and the controller renews it every [`FOCUS_RENEW_INTERVAL`].
//!
//! [`FocusLease`] is an owned value the device runtime drives: it sends through
//! a [`CommandLink`], takes acknowledgements back with
//! [`FocusLease::on_ack`], and says when it next needs
//! [`FocusLease::poll`]. The device starts a lease's time to live when it
//! receives the message, which is never before the controller sent it, so the
//! lease is known to be held until [`FOCUS_TTL`] after the send time of the
//! latest acknowledged message. Past that without a newer acknowledgement the
//! lease is reported expired: the device may already have dropped it.

use std::time::Duration;

use echomuse_protocol::{MessageId, Reported};
use tokio::time::Instant;

use crate::link::{CommandLink, LinkClosed};
use crate::wire::{
    AckFailure, AckOutcome, Command, FocusAcquire, FocusKind, FocusOwner, FocusRejection,
    FocusRelease, FocusRenew, LeaseId,
};

/// A lease's time to live on the device.
pub const FOCUS_TTL: Duration = Duration::from_secs(3);

/// How often a held lease is renewed.
pub const FOCUS_RENEW_INTERVAL: Duration = Duration::from_secs(1);

/// What happened to a lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FocusEvent {
    /// No acknowledged acquire or renew keeps the lease alive any more; the
    /// device may have dropped it, cancelling its owner's dialog output.
    Expired,
    /// The device refused the lease; `None` when it gave no code.
    Rejected(Option<Reported<FocusRejection>>),
    /// The session ended.
    LinkClosed,
    /// The device acknowledged the release.
    Released,
}

/// Why a lease's generation was not changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum GenerationError {
    /// Owner generations only grow.
    #[error("owner generation {requested} is below the lease's {current}")]
    Lower {
        /// The lease's generation.
        current: u32,
        /// The refused one.
        requested: u32,
    },
    /// The lease is no longer held.
    #[error("the lease is not held")]
    NotHeld,
    /// The session has ended.
    #[error(transparent)]
    LinkClosed(#[from] LinkClosed),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Held,
    Releasing,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Op {
    Keep,
    Release,
}

/// One dialog focus lease. See the module documentation.
#[derive(Debug)]
pub struct FocusLease {
    id: LeaseId,
    kind: FocusKind,
    owner: FocusOwner,
    generation: u32,
    state: State,
    next_renewal: Instant,
    /// The lease is known to be held until this instant.
    held_until: Instant,
    pending: Vec<(MessageId, Op, Instant)>,
}

impl FocusLease {
    /// Sends `focus.acquire` for a new lease at the owner's generation.
    pub fn acquire<L: CommandLink>(
        link: &mut L,
        now: Instant,
        kind: FocusKind,
        owner: FocusOwner,
        generation: u32,
    ) -> Result<Self, LinkClosed> {
        let id = LeaseId::generate();
        let body = FocusAcquire {
            lease_id: id.clone(),
            owner: owner.clone(),
            focus: kind,
            ttl_ms: FOCUS_TTL.as_millis() as u64,
        };
        let message = link.request(&Command::FocusAcquire(body), generation)?;
        Ok(Self {
            id,
            kind,
            owner,
            generation,
            state: State::Held,
            next_renewal: now + FOCUS_RENEW_INTERVAL,
            // Until the acquire is acknowledged nothing is known; its
            // acknowledgement must arrive within the time to live.
            held_until: now + FOCUS_TTL,
            pending: vec![(message, Op::Keep, now)],
        })
    }

    /// The lease's id.
    pub fn id(&self) -> &LeaseId {
        &self.id
    }

    /// Its class.
    pub fn kind(&self) -> FocusKind {
        self.kind
    }

    /// Its owner.
    pub fn owner(&self) -> &FocusOwner {
        &self.owner
    }

    /// The owner generation its messages carry.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Whether the lease is still held: not released, expired or refused.
    pub fn is_held(&self) -> bool {
        self.state == State::Held
    }

    /// Moves the lease to a newer owner generation, renewing it at once so the
    /// device learns it.
    pub fn set_generation<L: CommandLink>(
        &mut self,
        link: &mut L,
        now: Instant,
        generation: u32,
    ) -> Result<(), GenerationError> {
        if self.state != State::Held {
            return Err(GenerationError::NotHeld);
        }
        if generation < self.generation {
            return Err(GenerationError::Lower {
                current: self.generation,
                requested: generation,
            });
        }
        self.generation = generation;
        self.renew(link, now)
            .inspect_err(|_| self.state = State::Ended)?;
        Ok(())
    }

    /// Sends `focus.release`. The lease stops renewing at once; the device's
    /// acknowledgement completes it.
    pub fn release<L: CommandLink>(
        &mut self,
        link: &mut L,
        now: Instant,
    ) -> Result<(), LinkClosed> {
        if self.state != State::Held {
            return Ok(());
        }
        let body = FocusRelease {
            lease_id: self.id.clone(),
        };
        match link.request(&Command::FocusRelease(body), self.generation) {
            Ok(message) => {
                self.pending.push((message, Op::Release, now));
                self.state = State::Releasing;
                Ok(())
            }
            Err(LinkClosed) => {
                self.state = State::Ended;
                Err(LinkClosed)
            }
        }
    }

    /// Renews the lease when due and reports its expiry. Call it at
    /// [`next_deadline`](Self::next_deadline).
    pub fn poll<L: CommandLink>(&mut self, link: &mut L, now: Instant) -> Option<FocusEvent> {
        match self.state {
            State::Ended => None,
            State::Releasing => {
                // Without an acknowledgement the device lets the lease lapse
                // on its own; either way it is gone.
                (now >= self.held_until).then(|| {
                    self.state = State::Ended;
                    FocusEvent::Released
                })
            }
            State::Held => {
                if now >= self.held_until {
                    self.state = State::Ended;
                    return Some(FocusEvent::Expired);
                }
                if now >= self.next_renewal && self.renew(link, now).is_err() {
                    self.state = State::Ended;
                    return Some(FocusEvent::LinkClosed);
                }
                None
            }
        }
    }

    /// The next instant [`poll`](Self::poll) has work, if any.
    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            State::Held => Some(self.next_renewal.min(self.held_until)),
            State::Releasing => Some(self.held_until),
            State::Ended => None,
        }
    }

    /// Hands the lease a `command.ack` outcome. `Err` when the command was
    /// not this lease's; otherwise any resulting event.
    pub fn on_ack(
        &mut self,
        message_id: &MessageId,
        outcome: AckOutcome,
    ) -> Result<Option<FocusEvent>, NotThisLease> {
        let index = self
            .pending
            .iter()
            .position(|(id, _, _)| id == message_id)
            .ok_or(NotThisLease)?;
        let (_, op, sent) = self.pending.remove(index);
        if self.state == State::Ended {
            return Ok(None);
        }
        Ok(match (op, outcome) {
            (Op::Keep, AckOutcome::Accepted) => {
                self.held_until = self.held_until.max(sent + FOCUS_TTL);
                None
            }
            (Op::Release, AckOutcome::Accepted) => {
                self.state = State::Ended;
                Some(FocusEvent::Released)
            }
            (_, AckOutcome::Rejected { code }) => {
                self.state = State::Ended;
                Some(FocusEvent::Rejected(code.map(
                    |code| match FocusRejection::from_wire(&code) {
                        Some(known) => Reported::Known(known),
                        None => Reported::Unrecognized(code),
                    },
                )))
            }
            (_, AckOutcome::Failed(AckFailure::SessionEnded)) => {
                self.state = State::Ended;
                Some(FocusEvent::LinkClosed)
            }
            // A lost acknowledgement proves nothing either way; the lease
            // stays held until its known time runs out.
            (_, AckOutcome::Failed(AckFailure::TimedOut)) => None,
        })
    }

    fn renew<L: CommandLink>(&mut self, link: &mut L, now: Instant) -> Result<(), LinkClosed> {
        let body = FocusRenew {
            lease_id: self.id.clone(),
            ttl_ms: FOCUS_TTL.as_millis() as u64,
        };
        let message = link.request(&Command::FocusRenew(body), self.generation)?;
        self.pending.push((message, Op::Keep, now));
        // Renewals keep a steady cadence; a late poll does not cause a burst.
        self.next_renewal =
            (self.next_renewal + FOCUS_RENEW_INTERVAL).max(now + FOCUS_RENEW_INTERVAL / 2);
        Ok(())
    }
}

/// The acknowledged command was not one of this lease's.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("not this lease's command")]
pub struct NotThisLease;

#[cfg(test)]
mod tests {
    use tokio::time;

    use super::*;
    use crate::sim::{SimLink, accepted, rejected};

    fn owner() -> FocusOwner {
        FocusOwner::new("6f9619ff-8b86-d011-b42d-00cf4fc964ff")
    }

    /// Advances paused time to the lease's next deadline and polls.
    async fn step(lease: &mut FocusLease, link: &mut SimLink) -> (Instant, Option<FocusEvent>) {
        time::sleep_until(lease.next_deadline().unwrap()).await;
        let now = Instant::now();
        (now, lease.poll(link, now))
    }

    #[tokio::test(start_paused = true)]
    async fn renews_every_second_while_acknowledged() {
        let start = Instant::now();
        let mut link = SimLink::new();
        let mut lease =
            FocusLease::acquire(&mut link, start, FocusKind::DialogOutput, owner(), 4).unwrap();
        let (id, command, generation) = link.last_command();
        assert_eq!(generation, 4);
        let body = command.body_json();
        assert_eq!(body["focus"], "dialog_output");
        assert_eq!(body["ttl_ms"], 3_000);
        assert_eq!(body["owner"], owner().as_str());
        assert_eq!(lease.on_ack(&id, accepted()), Ok(None));

        let mut renewals = Vec::new();
        for _ in 0..10 {
            let (now, event) = step(&mut lease, &mut link).await;
            assert_eq!(event, None);
            let (id, command, generation) = link.last_command();
            assert!(matches!(command, Command::FocusRenew(_)));
            assert_eq!(command.body_json()["lease_id"], lease.id().as_str());
            assert_eq!(generation, 4);
            renewals.push(now - start);
            assert_eq!(lease.on_ack(&id, accepted()), Ok(None));
        }
        let expected: Vec<Duration> = (1..=10).map(Duration::from_secs).collect();
        assert_eq!(renewals, expected);
        assert!(lease.is_held());
    }

    #[tokio::test(start_paused = true)]
    async fn expires_three_seconds_after_the_last_acknowledged_send() {
        let start = Instant::now();
        let mut link = SimLink::new();
        let mut lease =
            FocusLease::acquire(&mut link, start, FocusKind::DialogInput, owner(), 1).unwrap();
        let (id, _, _) = link.last_command();
        lease.on_ack(&id, accepted()).unwrap();
        // The renewal at 1 s is acknowledged; later ones never are.
        let (_, _) = step(&mut lease, &mut link).await;
        let (renewed, _, _) = link.last_command();
        lease.on_ack(&renewed, accepted()).unwrap();
        let mut event = None;
        let mut fired = start;
        while event.is_none() {
            (fired, event) = step(&mut lease, &mut link).await;
        }
        assert_eq!(event, Some(FocusEvent::Expired));
        assert_eq!(fired - start, Duration::from_secs(4));
        // Renewals went out at 2 and 3 s before it lapsed, then stopped.
        let renews = link
            .commands()
            .iter()
            .filter(|(_, c, _)| matches!(c, Command::FocusRenew(_)))
            .count();
        assert_eq!(renews, 3);
        assert!(!lease.is_held());
        assert_eq!(lease.next_deadline(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unacknowledged_acquire_expires_after_the_ttl() {
        let start = Instant::now();
        let mut link = SimLink::new();
        let mut lease =
            FocusLease::acquire(&mut link, start, FocusKind::DialogInput, owner(), 1).unwrap();
        let mut event = None;
        let mut fired = start;
        while event.is_none() {
            (fired, event) = step(&mut lease, &mut link).await;
        }
        assert_eq!(event, Some(FocusEvent::Expired));
        assert_eq!(fired - start, FOCUS_TTL);
    }

    #[tokio::test(start_paused = true)]
    async fn rejection_release_and_generation() {
        let start = Instant::now();
        let mut link = SimLink::new();
        let mut lease =
            FocusLease::acquire(&mut link, start, FocusKind::DialogInput, owner(), 2).unwrap();
        assert_eq!(
            lease.set_generation(&mut link, start, 1),
            Err(GenerationError::Lower {
                current: 2,
                requested: 1
            })
        );
        lease.set_generation(&mut link, start, 3).unwrap();
        let (_, command, generation) = link.last_command();
        assert!(matches!(command, Command::FocusRenew(_)));
        assert_eq!(generation, 3);

        lease.release(&mut link, start).unwrap();
        let (release, command, generation) = link.last_command();
        assert!(matches!(command, Command::FocusRelease(_)));
        assert_eq!(generation, 3);
        assert_eq!(
            lease.on_ack(&release, accepted()),
            Ok(Some(FocusEvent::Released))
        );
        assert_eq!(
            lease.on_ack(&MessageId::generate(), accepted()),
            Err(NotThisLease)
        );

        let mut other =
            FocusLease::acquire(&mut link, start, FocusKind::DialogOutput, owner(), 1).unwrap();
        let (id, _, _) = link.last_command();
        assert_eq!(
            other.on_ack(&id, rejected("stale_generation")),
            Ok(Some(FocusEvent::Rejected(Some(Reported::Known(
                FocusRejection::StaleGeneration
            )))))
        );
        assert!(!other.is_held());
    }
}
