//! Signing in: users, roles, sessions, and the extractors routes use to require them
//! (§3.4).
//!
//! - HTTP requests present a session as `Authorization: Bearer <token>`.
//! - WebSocket upgrades may instead put it in the `token` query parameter (what the
//!   dashboard does) or a `session` cookie. A cookie is sent by the browser on its own,
//!   so a cookie-authenticated upgrade must come from a page on the controller itself:
//!   when it carries an `Origin`, that origin's host must equal the request's `Host`.
//!   Tokens are never read from the query string of an ordinary request.

mod extract;
pub(crate) mod password;
pub(crate) mod routes;
pub(crate) mod store;
pub(crate) mod throttle;
pub(crate) mod token;

use serde::Serialize;

pub use extract::{AdminUser, AuthenticatedUser};

/// A user's role, as `users.role` stores it and the dashboard reads it (`em_role`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// May change anything.
    Admin,
    /// May look but not change the fleet.
    Readonly,
}

impl Role {
    /// The role exactly as stored and sent.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Readonly => "readonly",
        }
    }

    /// A stored role, or `None` for a value this controller does not know.
    pub fn from_stored(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Self::Admin),
            "readonly" => Some(Self::Readonly),
            _ => None,
        }
    }
}

/// `users.id`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UserId(pub i64);
