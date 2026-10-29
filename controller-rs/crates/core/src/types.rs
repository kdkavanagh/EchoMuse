use std::{fmt, time::Duration};

use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct DeviceId(pub String);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct RuntimeId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct SessionId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct TurnId(pub u64);

/// These are controller identities, not device-protocol UUIDs or generations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkIdentity {
    pub device: DeviceId,
    pub runtime: RuntimeId,
    pub session: SessionId,
    pub turn: Option<TurnId>,
}

/// A result may only affect the operation that issued it while this scope is live.
#[derive(Clone, Debug)]
pub struct WorkScope {
    identity: WorkIdentity,
    cancellation: CancellationToken,
}

impl WorkScope {
    /// Creates a root scope. The device runtime issues capture scopes with this;
    /// other code derives narrower scopes with [`WorkScope::for_turn`] so session
    /// revocation and mute still cancel them.
    pub fn new(identity: WorkIdentity, cancellation: CancellationToken) -> Self {
        Self {
            identity,
            cancellation,
        }
    }

    pub fn identity(&self) -> &WorkIdentity {
        &self.identity
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }

    pub fn for_turn(&self, turn: TurnId) -> Self {
        let mut identity = self.identity.clone();
        identity.turn = Some(turn);
        Self::new(identity, self.cancellation.child_token())
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QueueLimits {
    pub messages: usize,
    pub bytes: u32,
    pub max_age: Duration,
}

impl Default for QueueLimits {
    fn default() -> Self {
        Self {
            messages: 64,
            bytes: 256 * 1024,
            max_age: Duration::from_millis(400),
        }
    }
}

impl QueueLimits {
    pub fn validate(self) -> Result<Self, RuntimeError> {
        if self.messages == 0
            || self.bytes == 0
            || self.max_age.is_zero()
            || self.messages > tokio::sync::Semaphore::MAX_PERMITS
            || self.bytes as usize > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(RuntimeError::InvalidLimits);
        }
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    InvalidLimits,
    Closed,
    Cancelled,
    Overloaded,
    Expired,
    /// A speech worker's call ran past its deadline; its late result was rejected.
    Overdue,
    StaleSession,
    Muted,
    DuplicateDevice,
    UnknownDevice,
    WorkerPanicked,
    ShutdownTimeout,
    ThreadStart(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("queue limits must be positive"),
            Self::Closed => f.write_str("runtime or channel is closed"),
            Self::Cancelled => f.write_str("operation was cancelled"),
            Self::Overloaded => f.write_str("queue capacity was exceeded"),
            Self::Expired => f.write_str("queued operation expired"),
            Self::Overdue => f.write_str("the call ran past its deadline; its result was rejected"),
            Self::StaleSession => f.write_str("session is no longer current"),
            Self::Muted => f.write_str("capture is muted"),
            Self::DuplicateDevice => f.write_str("device already has a runtime"),
            Self::UnknownDevice => f.write_str("device runtime does not exist"),
            Self::WorkerPanicked => f.write_str("worker thread panicked"),
            Self::ShutdownTimeout => {
                f.write_str("worker did not stop before the shutdown deadline")
            }
            Self::ThreadStart(detail) => write!(f, "could not start thread: {detail}"),
        }
    }
}

impl std::error::Error for RuntimeError {}
