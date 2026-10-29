//! Wall time for persisted timestamps and session expiry (§5.3), supplied as a value so
//! tests can control it.

use std::time::SystemTime;

use echomuse_db::devices::UnixSeconds;

use crate::error::ApiError;

/// The wall clock the API reads for `created_at` and `expires_at`.
pub trait WallClock: Send + Sync + 'static {
    /// The current wall time.
    fn now(&self) -> SystemTime;
}

/// The operating system's clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// The clock's time in whole seconds, as the `users` and `sessions` tables store it.
pub(crate) fn unix_now(clock: &dyn WallClock) -> Result<UnixSeconds, ApiError> {
    UnixSeconds::from_system_time(clock.now())
        .ok_or_else(|| ApiError::internal("The controller's clock is before 1970"))
}
