//! Slowing down password and setup-token guessing.
//!
//! Decision: failures are counted per client address. The first
//! [`ThrottleConfig::free_failures`] in a row cost nothing more than the bcrypt check;
//! after that each attempt must wait, from the last failure, a delay that starts at
//! [`ThrottleConfig::first_delay`] and doubles per further failure up to
//! [`ThrottleConfig::max_delay`]. An attempt made too early is answered 429 with
//! `Retry-After` before any password is checked, so it costs no bcrypt work and does not
//! extend the wait. A successful sign-in clears the address; an address with no failure
//! for [`ThrottleConfig::forget_after`] is forgotten.
//!
//! Counting per address rather than per username means a stranger on the network cannot
//! lock the administrator out by guessing their name. Behind add-on ingress every request
//! arrives from the Supervisor's proxy, so there the count is shared by everyone, who has
//! already signed in to Home Assistant to get that far. At most
//! [`ThrottleConfig::max_addresses`] addresses are tracked; when full, the address whose
//! last failure is oldest is dropped first.
//!
//! Time is Tokio's monotonic clock, so a change of wall time neither extends nor ends a
//! wait.

use std::{collections::HashMap, net::IpAddr, time::Duration};

use tokio::{sync::Mutex, time::Instant};

/// Sign-in throttling limits; see the module notes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThrottleConfig {
    /// Failures in a row allowed before attempts must wait.
    pub free_failures: u32,
    /// The wait after the first failure beyond the free ones.
    pub first_delay: Duration,
    /// The longest wait.
    pub max_delay: Duration,
    /// An address with no failure for this long starts again from zero.
    pub forget_after: Duration,
    /// Addresses tracked at most.
    pub max_addresses: usize,
}

impl Default for ThrottleConfig {
    fn default() -> Self {
        Self {
            free_failures: 5,
            first_delay: Duration::from_secs(2),
            max_delay: Duration::from_secs(300),
            forget_after: Duration::from_secs(3600),
            max_addresses: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Failures {
    count: u32,
    last: Instant,
}

/// Failure counts per client address.
pub(crate) struct Throttle {
    config: ThrottleConfig,
    failures: Mutex<HashMap<IpAddr, Failures>>,
}

impl Throttle {
    pub(crate) fn new(config: ThrottleConfig) -> Self {
        Self {
            config,
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn delay_after(&self, count: u32) -> Duration {
        let beyond = count.saturating_sub(self.config.free_failures);
        if beyond == 0 {
            return Duration::ZERO;
        }
        let doublings = (beyond - 1).min(31);
        self.config
            .first_delay
            .saturating_mul(1u32 << doublings)
            .min(self.config.max_delay)
    }

    /// `Err(wait)` when `address` must wait before trying again.
    pub(crate) async fn check(&self, address: IpAddr) -> Result<(), Duration> {
        let now = Instant::now();
        let failures = self.failures.lock().await;
        let Some(entry) = failures.get(&address) else {
            return Ok(());
        };
        let ready_at = entry.last + self.delay_after(entry.count);
        if now < ready_at {
            Err(ready_at - now)
        } else {
            Ok(())
        }
    }

    /// Count a failed attempt.
    pub(crate) async fn failed(&self, address: IpAddr) {
        let now = Instant::now();
        let forget_after = self.config.forget_after;
        let mut failures = self.failures.lock().await;
        if !failures.contains_key(&address) && failures.len() >= self.config.max_addresses {
            failures.retain(|_, entry| now.duration_since(entry.last) < forget_after);
            if failures.len() >= self.config.max_addresses
                && let Some(oldest) = failures
                    .iter()
                    .min_by_key(|(_, entry)| entry.last)
                    .map(|(address, _)| *address)
            {
                failures.remove(&oldest);
            }
        }
        let entry = failures.entry(address).or_insert(Failures {
            count: 0,
            last: now,
        });
        if now.duration_since(entry.last) >= forget_after {
            entry.count = 0;
        }
        entry.count = entry.count.saturating_add(1);
        entry.last = now;
    }

    /// Clear `address` after a success.
    pub(crate) async fn succeeded(&self, address: IpAddr) {
        self.failures.lock().await.remove(&address);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn config() -> ThrottleConfig {
        ThrottleConfig {
            free_failures: 2,
            first_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(4),
            forget_after: Duration::from_secs(60),
            max_addresses: 2,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_double_after_the_free_failures() {
        let throttle = Throttle::new(config());
        let a = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        throttle.failed(a).await;
        throttle.failed(a).await;
        assert_eq!(throttle.check(a).await, Ok(()));
        throttle.failed(a).await;
        assert_eq!(throttle.check(a).await, Err(Duration::from_secs(1)));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(throttle.check(a).await, Ok(()));
        throttle.failed(a).await;
        assert_eq!(throttle.check(a).await, Err(Duration::from_secs(2)));
        tokio::time::advance(Duration::from_secs(2)).await;
        throttle.failed(a).await;
        throttle.failed(a).await;
        assert_eq!(throttle.check(a).await, Err(Duration::from_secs(4)));
        throttle.succeeded(a).await;
        assert_eq!(throttle.check(a).await, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn forgets_quiet_addresses_and_stays_bounded() {
        let throttle = Throttle::new(config());
        let [a, b, c] = [1, 2, 3].map(|n| IpAddr::V4(Ipv4Addr::new(192, 0, 2, n)));
        for _ in 0..3 {
            throttle.failed(a).await;
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        throttle.failed(a).await;
        assert_eq!(throttle.check(a).await, Ok(()), "count restarted");
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..3 {
            throttle.failed(b).await;
        }
        throttle.failed(c).await;
        assert_eq!(throttle.failures.lock().await.len(), 2);
        assert_eq!(throttle.check(a).await, Ok(()), "oldest dropped");
        assert!(throttle.check(b).await.is_err());
    }
}
