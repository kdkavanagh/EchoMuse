//! Password hashing and checking.
//!
//! Stored hashes are bcrypt in the `$2b$` form, and existing ones must keep verifying
//! (§3.4). New hashes are `$2b$` too, so the previous controller can still check them
//! after a rollback (§3.5.1).
//!
//! Cost: [`BcryptCost::DEFAULT`] is 12, the cost of the hashes real installations
//! already hold, and above the commonly recommended minimum of 10. On the small machines
//! the add-on runs on, a cost-12 check takes a fraction of a second to about a second, a
//! price paid once per sign-in. Hashing runs on Tokio's blocking pool, at most
//! [`CONCURRENT_HASHES`] at a time, so a burst of sign-ins cannot occupy every core or
//! stall the shared runtime.

use std::{fmt, sync::Arc};

use bcrypt::Version;
use tokio::sync::{OnceCell, Semaphore};

use crate::error::{ApiError, ErrorCode};

/// bcrypt hashes computed at the same time, at most.
pub const CONCURRENT_HASHES: usize = 2;
/// Shortest new password, in characters, matching the dashboard's own check.
pub const MIN_PASSWORD_CHARS: usize = 8;
/// Longest new password, in bytes. bcrypt reads only the first 72 bytes, so a longer
/// password would silently be cut; it is refused instead.
pub const MAX_PASSWORD_BYTES: usize = 72;

/// The work factor of new hashes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BcryptCost(u32);

impl BcryptCost {
    /// The cost of new hashes in production; see the module notes.
    pub const DEFAULT: Self = Self(12);

    /// A cost bcrypt accepts (4 to 31). Tests use small costs to stay fast.
    pub fn new(cost: u32) -> Option<Self> {
        (4..=31).contains(&cost).then_some(Self(cost))
    }

    /// The cost as bcrypt counts it (log2 of the rounds).
    pub fn get(self) -> u32 {
        self.0
    }
}

impl Default for BcryptCost {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A bcrypt hash as `users.password_hash` stores it. `Debug` never shows it.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PasswordHash(String);

impl PasswordHash {
    pub(crate) fn from_stored(text: String) -> Self {
        Self(text)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash(..)")
    }
}

/// A password a user is setting, checked against the length rules. `Debug` never shows
/// it.
pub(crate) struct NewPassword(String);

impl NewPassword {
    pub(crate) fn parse(password: String) -> Result<Self, ApiError> {
        if password.chars().count() < MIN_PASSWORD_CHARS {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                ErrorCode::InvalidPassword,
                "Password must be at least 8 characters",
            ));
        }
        if password.len() > MAX_PASSWORD_BYTES {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                ErrorCode::InvalidPassword,
                "Password must be at most 72 bytes",
            ));
        }
        Ok(Self(password))
    }
}

impl fmt::Debug for NewPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("NewPassword(..)")
    }
}

/// Runs bcrypt off the shared runtime, a few at a time.
pub(crate) struct Passwords {
    cost: BcryptCost,
    permits: Arc<Semaphore>,
    /// A hash of a random password, checked when the username is unknown so that the
    /// answer takes as long as for a known user.
    decoy: OnceCell<PasswordHash>,
}

impl Passwords {
    pub(crate) fn new(cost: BcryptCost) -> Self {
        Self {
            cost,
            permits: Arc::new(Semaphore::new(CONCURRENT_HASHES)),
            decoy: OnceCell::new(),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| ApiError::internal("Password checking is unavailable"))?;
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|_| ApiError::internal("Password checking failed"))
    }

    /// A new `$2b$` hash with a random salt.
    pub(crate) async fn hash(&self, password: NewPassword) -> Result<PasswordHash, ApiError> {
        let cost = self.cost.get();
        self.run(move || {
            let mut salt = [0u8; 16];
            getrandom::fill(&mut salt).map_err(|error| {
                tracing::error!(%error, "no randomness for a password salt");
                ApiError::internal("The password could not be stored")
            })?;
            bcrypt::non_truncating_hash_with_salt(password.0.as_bytes(), cost, salt)
                .map(|parts| PasswordHash(parts.format_for_version(Version::TwoB)))
                .map_err(|error| {
                    tracing::error!(%error, "bcrypt refused to hash a password");
                    ApiError::internal("The password could not be stored")
                })
        })
        .await?
    }

    /// Whether `password` matches `hash`. A stored hash bcrypt cannot read never matches;
    /// that is logged, since the user cannot sign in until an administrator resets it.
    pub(crate) async fn verify(
        &self,
        password: String,
        hash: PasswordHash,
    ) -> Result<bool, ApiError> {
        self.run(
            move || match bcrypt::verify(password.as_bytes(), hash.as_str()) {
                Ok(matches) => matches,
                Err(error) => {
                    tracing::warn!(%error, "a stored password hash is not readable bcrypt");
                    false
                }
            },
        )
        .await
    }

    /// Spend the time a real check would take, for a username that does not exist.
    pub(crate) async fn verify_decoy(&self, password: String) -> Result<(), ApiError> {
        let decoy = self
            .decoy
            .get_or_try_init(|| async {
                let mut random = [0u8; 18];
                getrandom::fill(&mut random)
                    .map_err(|_| ApiError::internal("Password checking is unavailable"))?;
                let decoy: String = random.iter().map(|b| char::from(b'a' + b % 26)).collect();
                self.hash(NewPassword(decoy)).await
            })
            .await?
            .clone();
        self.verify(password, decoy).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_hashes_are_2b_and_verify() {
        let passwords = Passwords::new(BcryptCost::new(4).unwrap());
        let hash = passwords
            .hash(NewPassword::parse("a long enough secret".to_owned()).unwrap())
            .await
            .unwrap();
        assert!(hash.as_str().starts_with("$2b$04$"), "{hash:?}");
        assert_eq!(hash.as_str().len(), 60);
        assert!(
            passwords
                .verify("a long enough secret".to_owned(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!passwords.verify("wrong".to_owned(), hash).await.unwrap());
    }

    #[test]
    fn length_rules() {
        assert!(NewPassword::parse("short".to_owned()).is_err());
        assert!(NewPassword::parse("x".repeat(73)).is_err());
        assert!(NewPassword::parse("x".repeat(72)).is_ok());
        assert!(NewPassword::parse("eight ch".to_owned()).is_ok());
    }

    #[tokio::test]
    async fn unreadable_hash_never_matches() {
        let passwords = Passwords::new(BcryptCost::new(4).unwrap());
        let hash = PasswordHash::from_stored("not a hash".to_owned());
        assert!(!passwords.verify("anything".to_owned(), hash).await.unwrap());
    }
}
