//! Arbitration between devices that hear one wake (§4.2, §5.4).
//!
//! The first accepted claim wins. Other devices claiming within their configured
//! suppression window stand down without waiting. This orders controller claims,
//! not acoustic arrival times. A zero window means that caller never stands down.
//!
//! `claim` and `release` are synchronous and hold one `std::sync::Mutex` only for a few
//! comparisons, so a device runtime may call them from its event loop without awaiting.
//! A release names the exact grant it returns, so a late release (a superseded turn of
//! the same device, or a turn that ended after another device's claim) can never clear
//! a newer claim. A grant whose scope has been cancelled no longer suppresses anyone,
//! so a runtime that lost its session or failed without releasing cannot hold the room.

use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

use echomuse_core::types::{DeviceId, RuntimeError, WorkIdentity, WorkScope};

/// Identifies one won claim. Grants are never reused within one arbiter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GrantId(u64);

/// A won claim. Hand it back to [`WakeArbiter::release`] when the turn it opened ends.
/// It is deliberately neither `Clone` nor `Copy`, so each grant is released at most once.
#[derive(Debug, Eq, PartialEq)]
#[must_use = "a won claim must be released when its turn ends"]
pub struct WakeGrant {
    id: GrantId,
    claimed_at: Instant,
}

impl WakeGrant {
    pub fn id(&self) -> GrantId {
        self.id
    }

    /// The controller monotonic time the claim was made at (the caller's `now`).
    pub fn claimed_at(&self) -> Instant {
        self.claimed_at
    }
}

#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub enum ClaimDecision {
    /// This device takes the utterance and should answer now.
    Won(WakeGrant),
    /// Another device claimed this utterance within the claimer's window; stand down.
    Lost { winner: DeviceId },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    /// The grant was current, and the arbiter is free again.
    Released,
    /// A newer claim had already replaced this grant; nothing changed.
    Superseded,
}

/// The current holder. Keeping the claimant's scope lets a cancelled holder stop
/// suppressing other devices without any release call.
struct Held {
    id: GrantId,
    scope: WorkScope,
    claimed_at: Instant,
}

struct State {
    next_grant: u64,
    held: Option<Held>,
}

pub struct WakeArbiter {
    state: Mutex<State>,
}

impl Default for WakeArbiter {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeArbiter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                next_grant: 0,
                held: None,
            }),
        }
    }

    /// Decide the claim of `scope`'s device on the current utterance at controller
    /// monotonic time `now`. Returns at once on both paths.
    ///
    /// The claim loses only when a different device holds a live claim made less than
    /// `window` before `now`. Otherwise it wins and re-arms the window from `now`: nothing
    /// is claimed, the claim has expired, its holder's scope was cancelled, or the holder
    /// is this same device waking again (a genuinely new utterance in the room that
    /// already answered). A zero `window` therefore always wins. A `now` earlier than the
    /// holder's claim (two threads reading the clock before taking the lock) counts as no
    /// time elapsed.
    ///
    /// A cancelled `scope` is refused with [`RuntimeError::Cancelled`] and leaves the
    /// current claim untouched.
    pub fn claim(
        &self,
        scope: &WorkScope,
        window: Duration,
        now: Instant,
    ) -> Result<ClaimDecision, RuntimeError> {
        let mut state = self.lock();
        if scope.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        if let Some(held) = &state.held {
            let holder = &held.scope.identity().device;
            if *holder != scope.identity().device
                && !held.scope.is_cancelled()
                && now.saturating_duration_since(held.claimed_at) < window
            {
                return Ok(ClaimDecision::Lost {
                    winner: holder.clone(),
                });
            }
        }
        state.next_grant = state
            .next_grant
            .checked_add(1)
            .ok_or(RuntimeError::Closed)?;
        let id = GrantId(state.next_grant);
        state.held = Some(Held {
            id,
            scope: scope.clone(),
            claimed_at: now,
        });
        Ok(ClaimDecision::Won(WakeGrant {
            id,
            claimed_at: now,
        }))
    }

    /// Drop `grant` if it is still the current claim, so an immediate follow-up wake on
    /// another device is not suppressed by a finished turn. Works whether or not the
    /// grant's scope has been cancelled: revoking a session first and releasing its
    /// grants afterwards is the normal order (§5.7).
    pub fn release(&self, grant: WakeGrant) -> ReleaseOutcome {
        let mut state = self.lock();
        match &state.held {
            Some(held) if held.id == grant.id => {
                state.held = None;
                ReleaseOutcome::Released
            }
            _ => ReleaseOutcome::Superseded,
        }
    }

    /// The identity holding the current claim, if any, live or not. For status and
    /// diagnostics only; decisions go through `claim`.
    pub fn holder(&self) -> Option<WorkIdentity> {
        self.lock()
            .held
            .as_ref()
            .map(|held| held.scope.identity().clone())
    }

    /// A poisoned shared lock stops the process rather than reusing uncertain state.
    fn lock(&self) -> MutexGuard<'_, State> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(_) => {
                eprintln!("wake arbiter lock poisoned; stopping the controller");
                std::process::abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Barrier},
        thread,
    };

    use tokio_util::sync::CancellationToken;

    use super::*;
    use echomuse_core::types::{RuntimeId, SessionId, TurnId};

    const WINDOW: Duration = Duration::from_millis(700);

    fn scope(device: &str, session: u64) -> WorkScope {
        WorkScope::new(
            WorkIdentity {
                device: DeviceId(device.to_owned()),
                runtime: RuntimeId(1),
                session: SessionId(session),
                turn: Some(TurnId(session)),
            },
            CancellationToken::new(),
        )
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn won(decision: Result<ClaimDecision, RuntimeError>) -> WakeGrant {
        match decision {
            Ok(ClaimDecision::Won(grant)) => grant,
            other => panic!("expected a win, got {other:?}"),
        }
    }

    fn lost_to(decision: Result<ClaimDecision, RuntimeError>) -> String {
        match decision {
            Ok(ClaimDecision::Lost { winner }) => winner.0,
            other => panic!("expected a loss, got {other:?}"),
        }
    }

    #[test]
    fn first_detector_wins_and_others_in_window_lose() {
        // The 2026-07-20 field case: three devices within 184 ms.
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        assert_eq!(
            lost_to(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + ms(90))),
            "office"
        );
        assert_eq!(
            lost_to(arbiter.claim(&scope("kitchen", 3), WINDOW, t0 + ms(184))),
            "office"
        );
    }

    #[test]
    fn claim_expires_after_the_claimers_window() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        assert_eq!(
            lost_to(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + ms(699))),
            "office"
        );
        let _lounge = won(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + WINDOW));
        // The window is the claimer's: a shorter one expires the same claim sooner.
        let arbiter = WakeArbiter::new();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        let _lounge = won(arbiter.claim(&scope("lounge", 2), ms(200), t0 + ms(200)));
    }

    #[test]
    fn zero_window_never_stands_down_but_its_win_still_counts() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        let _lounge = won(arbiter.claim(&scope("lounge", 2), Duration::ZERO, t0 + ms(1)));
        // Arbitration is disabled for the lounge only; its claim still suppresses a
        // device that arbitrates, as in the Python controller.
        assert_eq!(
            lost_to(arbiter.claim(&scope("office", 3), WINDOW, t0 + ms(2))),
            "lounge"
        );
    }

    #[test]
    fn release_frees_the_claim_early() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        assert_eq!(arbiter.release(office), ReleaseOutcome::Released);
        assert_eq!(arbiter.holder(), None);
        let _lounge = won(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + ms(10)));
    }

    #[test]
    fn late_release_of_a_superseded_turn_keeps_the_same_devices_newer_claim() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let first = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        // The same device wakes again: a new utterance, the window re-arms.
        let second = won(arbiter.claim(&scope("office", 2), WINDOW, t0 + ms(600)));
        assert_ne!(first.id(), second.id());
        assert_eq!(arbiter.release(first), ReleaseOutcome::Superseded);
        assert_eq!(
            lost_to(arbiter.claim(&scope("lounge", 3), WINDOW, t0 + ms(1000))),
            "office"
        );
        assert_eq!(
            arbiter.holder().map(|identity| identity.session),
            Some(SessionId(2))
        );
        assert_eq!(arbiter.release(second), ReleaseOutcome::Released);
    }

    #[test]
    fn late_release_cannot_clear_another_devices_newer_claim() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        let _lounge = won(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + ms(800)));
        assert_eq!(arbiter.release(office), ReleaseOutcome::Superseded);
        assert_eq!(
            lost_to(arbiter.claim(&scope("kitchen", 3), WINDOW, t0 + ms(900))),
            "lounge"
        );
    }

    #[test]
    fn cancelled_claimant_is_refused_and_leaves_the_holder() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0));
        let stale = scope("lounge", 2);
        stale.cancel();
        assert_eq!(
            arbiter.claim(&stale, WINDOW, t0 + ms(800)),
            Err(RuntimeError::Cancelled)
        );
        assert_eq!(
            arbiter.holder().map(|identity| identity.device.0),
            Some("office".to_owned())
        );
        // A cancelled turn scope of a live session is refused too.
        let session = scope("lounge", 3);
        let turn = session.for_turn(TurnId(30));
        turn.cancel();
        assert_eq!(
            arbiter.claim(&turn, WINDOW, t0 + ms(800)),
            Err(RuntimeError::Cancelled)
        );
        let _lounge = won(arbiter.claim(&session, WINDOW, t0 + ms(800)));
    }

    #[test]
    fn cancelled_holder_stops_suppressing_and_its_release_is_stale() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let office_scope = scope("office", 1);
        let office = won(arbiter.claim(&office_scope, WINDOW, t0));
        office_scope.cancel();
        let lounge = won(arbiter.claim(&scope("lounge", 2), WINDOW, t0 + ms(50)));
        assert_eq!(arbiter.release(office), ReleaseOutcome::Superseded);
        assert_eq!(arbiter.release(lounge), ReleaseOutcome::Released);
    }

    #[test]
    fn cancelled_holder_can_still_release_its_own_grant() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let office_scope = scope("office", 1);
        let office = won(arbiter.claim(&office_scope, WINDOW, t0));
        office_scope.cancel();
        assert_eq!(arbiter.release(office), ReleaseOutcome::Released);
        assert_eq!(arbiter.holder(), None);
    }

    #[test]
    fn a_clock_read_before_the_holders_claim_counts_as_no_time_elapsed() {
        let arbiter = WakeArbiter::new();
        let t0 = Instant::now();
        let _office = won(arbiter.claim(&scope("office", 1), WINDOW, t0 + ms(5)));
        assert_eq!(
            lost_to(arbiter.claim(&scope("lounge", 2), WINDOW, t0)),
            "office"
        );
    }

    #[test]
    fn simultaneous_claims_from_many_threads_produce_one_winner() {
        const DEVICES: usize = 8;
        let arbiter = WakeArbiter::new();
        let barrier = Arc::new(Barrier::new(DEVICES));
        let now = Instant::now();
        let decisions: Vec<ClaimDecision> = thread::scope(|threads| {
            let handles: Vec<_> = (0..DEVICES)
                .map(|index| {
                    let (arbiter, barrier) = (&arbiter, Arc::clone(&barrier));
                    threads.spawn(move || {
                        let claimant = scope(&format!("device-{index}"), index as u64);
                        barrier.wait();
                        arbiter.claim(&claimant, WINDOW, now).expect("live scope")
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("claim thread"))
                .collect()
        });
        let winners: Vec<&WakeGrant> = decisions
            .iter()
            .filter_map(|decision| match decision {
                ClaimDecision::Won(grant) => Some(grant),
                ClaimDecision::Lost { .. } => None,
            })
            .collect();
        assert_eq!(winners.len(), 1);
        let holder = arbiter.holder().expect("one holder").device;
        for decision in &decisions {
            if let ClaimDecision::Lost { winner } = decision {
                assert_eq!(*winner, holder);
            }
        }
    }
}
