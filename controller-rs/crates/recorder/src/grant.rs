//! Consent: the administrator's per-device recording grant (§6.2 "Consent and scope").
//!
//! Nothing is recorded without a live grant. An administrator grants one device a bounded
//! interval and names what may be kept: mic audio, reference audio, loudness cells, AFE
//! records and transcripts. The device runtime asks the [`GrantBook`] for a [`GrantPass`],
//! which exists only while the grant is live, and a [`SessionRecorder`](crate::SessionRecorder)
//! can only be started with one. The recorder then checks the grant again on every call, so a
//! recording stops at the grant's end even if the runtime never asks.
//!
//! Withdrawing a grant takes effect at once, from whichever thread withdraws it: every
//! recording under it discards what it has not yet written and ends.
//!
//! Times: the grant's interval runs on the controller's monotonic clock, which every caller
//! passes in. Wall time appears only in [`GrantStatus`], for the dashboard to show.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime};

use echomuse_core::types::DeviceId;
use echomuse_protocol::StreamId;
use echomuse_protocol::ema::Kind;
use echomuse_turn::MonoInstant;
use serde::{Deserialize, Serialize};

use crate::queue::{Purge, Queue};
use crate::record::{EndReason, StreamName};

/// The longest interval one grant may cover.
pub const MAX_GRANT_INTERVAL: Duration = Duration::from_secs(8 * 60 * 60);

/// The administrator who granted or withdrew consent, as the dashboard identifies them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdministratorId(String);

impl AdministratorId {
    /// The administrator named `name`, or `None` for an empty or blank name.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let name = name.into();
        (!name.trim().is_empty()).then_some(Self(name))
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AdministratorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a grant allows the recorder to keep. Decision evidence that is neither audio nor text
/// (wake and endpoint decisions, VAD probabilities, echo labels, timings, stream and epoch
/// identities, mute, cancellation and disconnects) is kept under every grant: it is what a
/// decision replay needs, and it carries no speech.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamScope {
    /// Mic audio (EMA1 kind 1).
    pub mic: bool,
    /// The device's final-mix reference audio (kind 2).
    pub reference: bool,
    /// The device's loudness cells (kind 4), as frames and as endpoint evidence.
    pub cells: bool,
    /// The native AFE records (kind 5).
    pub afe: bool,
    /// Text: streaming ASR tokens, verifier transcripts, the text asked of HA's sentence
    /// matcher, committed text, grammar answers and HA request and reply bodies.
    pub transcripts: bool,
}

impl StreamScope {
    /// Everything.
    pub const ALL: StreamScope = StreamScope {
        mic: true,
        reference: true,
        cells: true,
        afe: true,
        transcripts: true,
    };

    /// Whether frames of `kind` may be kept. Render frames (kind 3, the controller's own output)
    /// are never kept.
    pub const fn allows(&self, kind: Kind) -> bool {
        match kind {
            Kind::Mic => self.mic,
            Kind::Reference => self.reference,
            Kind::Cells => self.cells,
            Kind::Afe => self.afe,
            Kind::Render => false,
        }
    }

    /// The uplink streams this scope leaves out.
    pub fn withheld_streams(&self) -> Vec<StreamName> {
        [
            StreamId::Mic,
            StreamId::Reference,
            StreamId::Cells,
            StreamId::Afe,
        ]
        .into_iter()
        .filter(|s| !self.allows(s.kind()))
        .map(StreamName::from)
        .collect()
    }
}

/// A grant's identity: a fresh UUIDv4 for every grant.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GrantId(String);

impl GrantId {
    fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A wall-clock time in milliseconds since the Unix epoch, for display and file retention only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixMillis(pub u64);

impl UnixMillis {
    /// The time `t`; a time before the epoch is the epoch.
    pub fn of(t: SystemTime) -> Self {
        Self(
            t.duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// The time `d` later.
    pub fn after(self, d: Duration) -> Self {
        Self(
            self.0
                .saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        )
    }
}

/// An administrator's request to record one device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRequest {
    /// Who asked.
    pub administrator: AdministratorId,
    /// How long recording may run, from now: more than zero and at most
    /// [`MAX_GRANT_INTERVAL`].
    pub interval: Duration,
    /// What may be kept.
    pub streams: StreamScope,
}

/// Why a grant could not be given or withdrawn.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    /// The interval was zero.
    #[error("the recording interval must be longer than zero")]
    EmptyInterval,
    /// The interval was longer than [`MAX_GRANT_INTERVAL`].
    #[error("the recording interval may be at most {max_secs} s")]
    TooLong {
        /// The longest interval, in seconds.
        max_secs: u64,
    },
    /// The device already has a live grant; withdraw it first.
    #[error("device already has a live recording grant {grant:?}")]
    AlreadyActive {
        /// The live grant.
        grant: GrantId,
    },
    /// The device has no live grant to withdraw.
    #[error("device has no live recording grant")]
    NotActive,
}

/// What the dashboard shows for one device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GrantStatus {
    /// No grant has been given since the controller started.
    Off,
    /// Recording is allowed.
    Active {
        /// The grant.
        grant: GrantId,
        /// Who gave it.
        administrator: AdministratorId,
        /// What may be kept.
        streams: StreamScope,
        /// When it was given.
        granted_at: UnixMillis,
        /// When it ends, on the wall clock at the time it was given.
        ends_at: UnixMillis,
        /// How long it has left, in milliseconds, on the monotonic clock.
        remaining_ms: u64,
        /// Whether a recording is being written under it now.
        recording: bool,
    },
    /// The interval ran out.
    Expired {
        /// The grant.
        grant: GrantId,
        /// Who gave it.
        administrator: AdministratorId,
        /// When it ended.
        ended_at: UnixMillis,
    },
    /// An administrator withdrew it.
    Withdrawn {
        /// The grant.
        grant: GrantId,
        /// Who withdrew it.
        by: AdministratorId,
        /// When.
        at: UnixMillis,
    },
}

#[derive(Debug)]
struct Withdrawal {
    by: AdministratorId,
    at: UnixMillis,
}

#[derive(Debug, Default)]
struct GrantState {
    withdrawal: Option<Withdrawal>,
    writers: Vec<Weak<Queue>>,
}

/// One grant, shared by the book and every recorder under it.
#[derive(Debug)]
pub(crate) struct GrantShared {
    pub(crate) id: GrantId,
    pub(crate) device: DeviceId,
    pub(crate) administrator: AdministratorId,
    pub(crate) streams: StreamScope,
    pub(crate) granted_at: UnixMillis,
    pub(crate) starts: MonoInstant,
    pub(crate) ends: MonoInstant,
    interval: Duration,
    withdrawn: AtomicBool,
    state: Mutex<GrantState>,
}

impl GrantShared {
    /// Whether recording is still allowed at `now`.
    pub(crate) fn live_at(&self, now: MonoInstant) -> Result<(), EndReason> {
        if self.withdrawn.load(Ordering::Acquire) {
            Err(EndReason::Withdrawn)
        } else if now >= self.ends {
            Err(EndReason::Expired)
        } else {
            Ok(())
        }
    }

    /// Registers a recorder's queue so withdrawal and expiry can reach it. Refused once the
    /// grant has been withdrawn or its device forgotten.
    pub(crate) fn attach(&self, queue: &Arc<Queue>) -> Result<(), EndReason> {
        let mut state = lock(&self.state);
        if self.withdrawn.load(Ordering::Acquire) {
            return Err(EndReason::Withdrawn);
        }
        state.writers.retain(|w| w.strong_count() > 0);
        state.writers.push(Arc::downgrade(queue));
        Ok(())
    }

    fn recording(&self) -> bool {
        lock(&self.state)
            .writers
            .iter()
            .filter_map(Weak::upgrade)
            .any(|q| q.is_open())
    }

    /// Ends every recorder under this grant.
    fn end_writers(&self, reason: EndReason, purge: Purge, now: MonoInstant) {
        let writers = std::mem::take(&mut lock(&self.state).writers);
        for q in writers.iter().filter_map(Weak::upgrade) {
            q.close(reason, purge, now);
        }
    }

    fn status(&self, now: MonoInstant) -> GrantStatus {
        let state = lock(&self.state);
        if let Some(w) = &state.withdrawal {
            return GrantStatus::Withdrawn {
                grant: self.id.clone(),
                by: w.by.clone(),
                at: w.at,
            };
        }
        drop(state);
        if now >= self.ends {
            return GrantStatus::Expired {
                grant: self.id.clone(),
                administrator: self.administrator.clone(),
                ended_at: self.granted_at.after(self.interval),
            };
        }
        GrantStatus::Active {
            grant: self.id.clone(),
            administrator: self.administrator.clone(),
            streams: self.streams,
            granted_at: self.granted_at,
            ends_at: self.granted_at.after(self.interval),
            remaining_ms: u64::try_from(self.ends.since(now).as_millis()).unwrap_or(u64::MAX),
            recording: self.recording(),
        }
    }
}

/// Proof that a device's grant was live when it was taken. It cannot be built outside this
/// crate, so a recorder cannot exist without a grant:
///
/// ```compile_fail
/// let pass = echomuse_recorder::GrantPass {};
/// ```
#[derive(Debug)]
pub struct GrantPass {
    pub(crate) shared: Arc<GrantShared>,
}

/// Every device's grant. One book serves the whole controller: the dashboard API grants and
/// withdraws, and each device runtime takes passes from it. All methods are short and never
/// wait on I/O.
#[derive(Debug, Default)]
pub struct GrantBook {
    grants: Mutex<HashMap<DeviceId, Arc<GrantShared>>>,
}

impl GrantBook {
    /// An empty book: nothing may be recorded.
    pub fn new() -> Self {
        Self::default()
    }

    /// Grants `device` recording from `now` for the request's interval. `wall` is the same
    /// moment on the wall clock, for display.
    pub fn grant(
        &self,
        device: &DeviceId,
        request: GrantRequest,
        now: MonoInstant,
        wall: SystemTime,
    ) -> Result<GrantStatus, GrantError> {
        if request.interval.is_zero() {
            return Err(GrantError::EmptyInterval);
        }
        if request.interval > MAX_GRANT_INTERVAL {
            return Err(GrantError::TooLong {
                max_secs: MAX_GRANT_INTERVAL.as_secs(),
            });
        }
        let mut grants = lock(&self.grants);
        if let Some(g) = grants.get(device)
            && g.live_at(now).is_ok()
        {
            return Err(GrantError::AlreadyActive {
                grant: g.id.clone(),
            });
        }
        if let Some(old) = grants.get(device) {
            // An expired grant's recorders end now if they have not noticed yet.
            old.end_writers(EndReason::Expired, Purge::Nothing, now);
        }
        let shared = Arc::new(GrantShared {
            id: GrantId::generate(),
            device: device.clone(),
            administrator: request.administrator,
            streams: request.streams,
            granted_at: UnixMillis::of(wall),
            starts: now,
            ends: now.after(request.interval),
            interval: request.interval,
            withdrawn: AtomicBool::new(false),
            state: Mutex::new(GrantState::default()),
        });
        let status = shared.status(now);
        grants.insert(device.clone(), shared);
        Ok(status)
    }

    /// Withdraws `device`'s live grant at once: every recording under it discards what it has
    /// not yet written, records the withdrawal and ends. Already written recordings stay until
    /// their retention ends or they are deleted.
    pub fn withdraw(
        &self,
        device: &DeviceId,
        by: AdministratorId,
        now: MonoInstant,
        wall: SystemTime,
    ) -> Result<GrantStatus, GrantError> {
        let grants = lock(&self.grants);
        let Some(g) = grants.get(device).filter(|g| g.live_at(now).is_ok()) else {
            return Err(GrantError::NotActive);
        };
        let g = Arc::clone(g);
        drop(grants);
        {
            let mut state = lock(&g.state);
            state.withdrawal = Some(Withdrawal {
                by,
                at: UnixMillis::of(wall),
            });
            g.withdrawn.store(true, Ordering::Release);
        }
        g.end_writers(EndReason::Withdrawn, Purge::Everything, now);
        Ok(g.status(now))
    }

    /// The device's status at `now`. A grant found expired ends its recorders here too.
    pub fn status(&self, device: &DeviceId, now: MonoInstant) -> GrantStatus {
        let Some(g) = lock(&self.grants).get(device).cloned() else {
            return GrantStatus::Off;
        };
        if g.live_at(now) == Err(EndReason::Expired) {
            g.end_writers(EndReason::Expired, Purge::Nothing, now);
        }
        g.status(now)
    }

    /// A pass for `device`, only while its grant is live at `now`.
    pub fn pass(&self, device: &DeviceId, now: MonoInstant) -> Option<GrantPass> {
        let g = lock(&self.grants).get(device).cloned()?;
        g.live_at(now).ok()?;
        Some(GrantPass { shared: g })
    }

    /// Ends the recorders of every grant that has expired by `now`. Call it from any periodic
    /// task; recorders also stop on their own at their next call after the end.
    pub fn expire(&self, now: MonoInstant) {
        let grants: Vec<_> = lock(&self.grants).values().cloned().collect();
        for g in grants {
            if g.live_at(now) == Err(EndReason::Expired) {
                g.end_writers(EndReason::Expired, Purge::Nothing, now);
            }
        }
    }

    /// Forgets a deleted device, withdrawing its live grant first so no recording continues.
    pub fn forget(&self, device: &DeviceId, now: MonoInstant) {
        if let Some(g) = lock(&self.grants).remove(device) {
            g.withdrawn.store(true, Ordering::Release);
            g.end_writers(EndReason::Withdrawn, Purge::Everything, now);
        }
    }
}

/// Locks a mutex, carrying on with the data if a panicking thread poisoned it: every critical
/// section here leaves the data consistent.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
