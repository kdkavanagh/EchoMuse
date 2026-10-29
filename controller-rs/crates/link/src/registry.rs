//! Which session each device holds, and where an audio or asset upgrade naming a
//! session should go (spec §3.1.1).
//!
//! This is a routing table, not device state: an HTTP upgrade must be accepted or
//! refused with 403 before the upgrade, synchronously with the request, so the lookup
//! cannot wait for a session task to answer a message. The lock is held only for map
//! operations, never across an await, and no session behaviour depends on anything
//! here beyond "which session is current" and "has audio joined".

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use echomuse_core::types::DeviceId;
use echomuse_protocol::LinkSessionId;
use tokio::sync::oneshot;

use crate::control::Ws;
use crate::session::SessionCore;

struct Active {
    core: Arc<SessionCore>,
    audio: Option<oneshot::Sender<Ws>>,
}

#[derive(Default)]
struct State {
    /// The newest admitted hello of each device, whether or not its session is ready.
    claims: HashMap<DeviceId, Arc<SessionCore>>,
    /// Sessions whose `session.ready` is being or has been written.
    active: HashMap<LinkSessionId, Active>,
}

/// Why an audio or asset upgrade cannot join a session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JoinRefusal {
    /// No live session has that id.
    UnknownSession,
    /// The session's audio socket has already joined.
    AudioTaken,
}

#[derive(Default)]
pub(crate) struct Registry {
    state: Mutex<State>,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding this lock leaves routing unknowable; spec §5.5 stops
        // the process for a poisoned shared lock.
        self.state
            .lock()
            .expect("link session registry lock poisoned")
    }

    /// Records `core` as its device's newest admitted session and returns the session
    /// it replaces, which the caller ends before admitting the new one. The replaced
    /// session stops routing at once.
    pub(crate) fn claim(&self, core: Arc<SessionCore>) -> Option<Arc<SessionCore>> {
        let mut state = self.lock();
        let old = state.claims.insert(core.device.clone(), core);
        if let Some(old) = &old {
            state.active.remove(&old.session_id);
        }
        old
    }

    /// Opens `core` to audio and asset upgrades, unless a newer hello from its device
    /// replaced it meanwhile.
    pub(crate) fn activate(&self, core: &Arc<SessionCore>, audio: oneshot::Sender<Ws>) -> bool {
        let mut state = self.lock();
        let current = state
            .claims
            .get(&core.device)
            .is_some_and(|claim| Arc::ptr_eq(claim, core));
        if current {
            let active = Active {
                core: Arc::clone(core),
                audio: Some(audio),
            };
            state.active.insert(core.session_id.clone(), active);
        }
        current
    }

    /// Forgets `core`. A newer session of the same device is untouched.
    pub(crate) fn release(&self, core: &Arc<SessionCore>) {
        let mut state = self.lock();
        state.active.remove(&core.session_id);
        let current = state
            .claims
            .get(&core.device)
            .is_some_and(|claim| Arc::ptr_eq(claim, core));
        if current {
            state.claims.remove(&core.device);
        }
    }

    /// The live session `session_id` names.
    pub(crate) fn lookup(&self, session_id: &LinkSessionId) -> Option<Arc<SessionCore>> {
        let state = self.lock();
        let active = state.active.get(session_id)?;
        (!active.core.stop.is_cancelled()).then(|| Arc::clone(&active.core))
    }

    /// Takes the hand-off for `core`'s audio socket; each session takes one.
    pub(crate) fn take_audio(
        &self,
        core: &Arc<SessionCore>,
    ) -> Result<oneshot::Sender<Ws>, JoinRefusal> {
        let mut state = self.lock();
        let active = state
            .active
            .get_mut(&core.session_id)
            .filter(|active| Arc::ptr_eq(&active.core, core) && !core.stop.is_cancelled())
            .ok_or(JoinRefusal::UnknownSession)?;
        active.audio.take().ok_or(JoinRefusal::AudioTaken)
    }

    /// Whether `core` is still a live, routed session.
    pub(crate) fn is_live(&self, core: &Arc<SessionCore>) -> bool {
        let state = self.lock();
        state
            .active
            .get(&core.session_id)
            .is_some_and(|active| Arc::ptr_eq(&active.core, core))
            && !core.stop.is_cancelled()
    }
}
