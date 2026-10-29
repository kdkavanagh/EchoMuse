//! Session tokens and the first-run setup token.
//!
//! A session token is 32 random bytes written as 64 lowercase hexadecimal characters,
//! the form every row of `sessions.token` in real installations has. It is stored as is:
//! the column is the token itself, so the controller finds a session with an indexed
//! lookup by that value rather than comparing tokens in memory. Wherever this crate does
//! compare tokens in memory (the setup token, revoked sessions on open event sockets) it
//! uses a constant-time comparison.

use std::fmt;

use echomuse_db::DbHandle;
use subtle::ConstantTimeEq;

use crate::{auth::store::AnyUser, error::ApiError};

/// Random bytes in a session token.
const SESSION_TOKEN_BYTES: usize = 32;
/// Random bytes in a setup token: 128 bits is far beyond guessing, and with sign-in
/// throttling an attacker gets a few attempts a minute. 32 hexadecimal characters is
/// still easy to copy out of a log.
const SETUP_TOKEN_BYTES: usize = 16;
/// Longest token accepted from a client. Real tokens are 64 characters; anything much
/// longer is not a token this controller issued and is refused before any lookup.
const MAX_PRESENTED_TOKEN: usize = 256;

fn random_hex<const N: usize>() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes)?;
    let mut text = String::with_capacity(N * 2);
    for byte in bytes {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    Ok(text)
}

fn equal_in_constant_time(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// A dashboard session's bearer token. `Debug` never shows it.
#[derive(Clone, Eq, PartialEq)]
pub struct SessionToken(String);

impl SessionToken {
    /// A new random token.
    pub(crate) fn generate() -> Result<Self, ApiError> {
        random_hex::<SESSION_TOKEN_BYTES>()
            .map(Self)
            .map_err(|error| {
                eprintln!("api: no randomness for a session token: {error}");
                ApiError::internal("The controller could not create a session")
            })
    }

    /// A token as a client presented it, if it could be one: non-empty, not absurdly
    /// long, and printable ASCII without spaces.
    pub(crate) fn from_presented(text: &str) -> Option<Self> {
        let plausible = !text.is_empty()
            && text.len() <= MAX_PRESENTED_TOKEN
            && text.bytes().all(|b| b.is_ascii_graphic());
        plausible.then(|| Self(text.to_owned()))
    }

    /// The token's text, for storing or returning to the client that owns it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `other` is the same token, compared in constant time.
    pub(crate) fn matches(&self, other: &SessionToken) -> bool {
        equal_in_constant_time(&self.0, &other.0)
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(..)")
    }
}

/// The one-time token that lets the first administrator be created (`POST /api/setup`).
///
/// The integrator asks for one at startup with [`BootstrapToken::issue_if_needed`], prints
/// it to the controller's log while no user exists, and hands it to
/// [`ApiServer`](crate::ApiServer). It is accepted once: a successful setup consumes
/// it, and setup is refused from then on because a user exists. `Debug` never shows it.
pub struct BootstrapToken(String);

impl BootstrapToken {
    /// A new token if the database has no user yet, else `None`: setup is closed and
    /// nothing should be printed.
    pub async fn issue_if_needed(db: &DbHandle) -> Result<Option<Self>, ApiError> {
        if db.read(AnyUser).await? {
            return Ok(None);
        }
        Self::generate().map(Some)
    }

    /// A new random token, whatever the database holds.
    pub fn generate() -> Result<Self, ApiError> {
        random_hex::<SETUP_TOKEN_BYTES>()
            .map(Self)
            .map_err(|error| {
                eprintln!("api: no randomness for the setup token: {error}");
                ApiError::internal("The controller could not create a setup token")
            })
    }

    /// The token's text, for printing to the controller's log.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether `presented` is this token, compared in constant time.
    pub(crate) fn accepts(&self, presented: &str) -> bool {
        equal_in_constant_time(&self.0, presented)
    }
}

impl fmt::Debug for BootstrapToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BootstrapToken(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tokens_have_the_stored_shape() {
        let token = SessionToken::generate().unwrap();
        assert_eq!(token.expose().len(), 64);
        assert!(
            token
                .expose()
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_ne!(token, SessionToken::generate().unwrap());
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let token = SessionToken::generate().unwrap();
        assert!(!format!("{token:?}").contains(token.expose()));
        let setup = BootstrapToken::generate().unwrap();
        assert!(!format!("{setup:?}").contains(setup.expose()));
        assert_eq!(setup.expose().len(), 32);
    }

    #[test]
    fn presented_tokens_are_screened() {
        assert!(SessionToken::from_presented("").is_none());
        assert!(SessionToken::from_presented("has space").is_none());
        assert!(SessionToken::from_presented(&"a".repeat(257)).is_none());
        assert!(SessionToken::from_presented(&"a".repeat(64)).is_some());
    }
}
