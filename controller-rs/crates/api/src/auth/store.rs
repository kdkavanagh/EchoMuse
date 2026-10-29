//! The `users`, `sessions` and `session_expiry_days` rows, as commands for the database
//! service.
//!
//! Column shapes, as real installations hold them:
//! - `users.created_at`, `sessions.created_at` and `sessions.expires_at` are whole
//!   seconds since the Unix epoch;
//! - `users.role` is `admin` or `readonly`;
//! - `users.password_hash` is a 60-character bcrypt `$2b$` hash;
//! - `sessions.token` is the session token itself (see [`super::token`]);
//! - `system_config.session_expiry_days` is a decimal count of days.
//!
//! A session lasts `session_expiry_days` days from sign-in, read when the session is
//! created. When the setting is missing, or is not a whole number from 1 to
//! [`MAX_SESSION_DAYS`], it lasts [`DEFAULT_SESSION_DAYS`], the default the controller's
//! documentation gives (docs/configuration.md, "System settings"). It does not slide:
//! using a session does not extend it.

use std::fmt;

use echomuse_db::{ReadQuery, WriteCommand, devices::UnixSeconds};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{Role, UserId, password::PasswordHash, token::SessionToken};

/// How long a session lasts when `session_expiry_days` is missing or unusable.
pub const DEFAULT_SESSION_DAYS: i64 = 30;
/// The longest session lifetime honoured, in days. Larger settings use the default.
pub const MAX_SESSION_DAYS: i64 = 36_500;
/// The `system_config` key of the session lifetime.
pub(crate) const SESSION_EXPIRY_KEY: &str = "session_expiry_days";
const SECONDS_PER_DAY: i64 = 86_400;

/// Why a users or sessions command failed.
#[derive(Debug)]
pub(crate) enum StoreError {
    Sqlite(rusqlite::Error),
    /// A `users.role` value this controller does not know.
    UnknownRole,
    /// `POST /api/setup` found a user already.
    UsersExist,
    /// The password changed, or the user went away, between reading and writing.
    PasswordChanged,
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite: {error}"),
            Self::UnknownRole => f.write_str("a user's stored role is neither admin nor readonly"),
            Self::UsersExist => f.write_str("a user already exists"),
            Self::PasswordChanged => f.write_str("the password changed meanwhile"),
        }
    }
}

fn parse_role(text: &str) -> Result<Role, StoreError> {
    Role::from_stored(text).ok_or(StoreError::UnknownRole)
}

/// The session lifetime in days, from the setting or the default.
pub(crate) fn session_days(conn: &Connection) -> rusqlite::Result<i64> {
    let stored: Option<Option<String>> = conn
        .query_row(
            "SELECT value FROM system_config WHERE key = ?1",
            [SESSION_EXPIRY_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(stored
        .flatten()
        .and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|days| (1..=MAX_SESSION_DAYS).contains(days))
        .unwrap_or(DEFAULT_SESSION_DAYS))
}

/// The session lifetime in seconds, from the setting or the default.
fn session_lifetime(conn: &Connection) -> rusqlite::Result<i64> {
    Ok(session_days(conn)? * SECONDS_PER_DAY)
}

/// Insert a session for `user`, first deleting every session that has expired, so the
/// table only grows by live sessions. Returns its expiry.
fn insert_session(
    tx: &Transaction<'_>,
    user: UserId,
    token: &SessionToken,
    now: UnixSeconds,
) -> rusqlite::Result<UnixSeconds> {
    let expires_at = UnixSeconds(now.0.saturating_add(session_lifetime(tx)?));
    tx.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now.0])?;
    tx.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![token.expose(), user.0, now.0, expires_at.0],
    )?;
    Ok(expires_at)
}

/// Whether any user exists.
pub(crate) struct AnyUser;

impl ReadQuery for AnyUser {
    type Output = bool;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<bool> {
        conn.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |row| row.get(0))
    }
}

/// A user's row, for checking a password.
#[derive(Debug)]
pub(crate) struct StoredUser {
    pub(crate) id: UserId,
    pub(crate) role: Role,
    pub(crate) password_hash: PasswordHash,
}

/// The user with exactly this username.
pub(crate) struct UserByName {
    pub(crate) username: String,
}

impl ReadQuery for UserByName {
    type Output = Option<StoredUser>;
    type Error = StoreError;

    fn read(self, conn: &Connection) -> Result<Option<StoredUser>, StoreError> {
        let row: Option<(i64, String, String)> = conn
            .query_row(
                "SELECT id, password_hash, role FROM users WHERE username = ?1",
                [&self.username],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        row.map(|(id, hash, role)| {
            Ok(StoredUser {
                id: UserId(id),
                role: parse_role(&role)?,
                password_hash: PasswordHash::from_stored(hash),
            })
        })
        .transpose()
    }
}

/// The user with this id.
pub(crate) struct UserById {
    pub(crate) id: UserId,
}

impl ReadQuery for UserById {
    type Output = Option<StoredUser>;
    type Error = StoreError;

    fn read(self, conn: &Connection) -> Result<Option<StoredUser>, StoreError> {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT password_hash, role FROM users WHERE id = ?1",
                [self.id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        row.map(|(hash, role)| {
            Ok(StoredUser {
                id: self.id,
                role: parse_role(&role)?,
                password_hash: PasswordHash::from_stored(hash),
            })
        })
        .transpose()
    }
}

/// A live session and its user.
#[derive(Debug)]
pub(crate) struct SessionUser {
    pub(crate) user_id: UserId,
    pub(crate) username: String,
    pub(crate) role: Role,
    pub(crate) expires_at: UnixSeconds,
}

/// The session with this token, if it has not expired by `now`.
pub(crate) struct LiveSession {
    pub(crate) token: SessionToken,
    pub(crate) now: UnixSeconds,
}

impl ReadQuery for LiveSession {
    type Output = Option<SessionUser>;
    type Error = StoreError;

    fn read(self, conn: &Connection) -> Result<Option<SessionUser>, StoreError> {
        let row: Option<(i64, String, String, i64)> = conn
            .query_row(
                "SELECT users.id, users.username, users.role, sessions.expires_at
                   FROM sessions JOIN users ON users.id = sessions.user_id
                  WHERE sessions.token = ?1 AND sessions.expires_at > ?2",
                params![self.token.expose(), self.now.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        row.map(|(id, username, role, expires_at)| {
            Ok(SessionUser {
                user_id: UserId(id),
                username,
                role: parse_role(&role)?,
                expires_at: UnixSeconds(expires_at),
            })
        })
        .transpose()
    }
}

/// Create the first administrator and a session for them, only while no user exists.
pub(crate) struct CreateFirstAdmin {
    pub(crate) username: String,
    pub(crate) password_hash: PasswordHash,
    pub(crate) token: SessionToken,
    pub(crate) now: UnixSeconds,
}

impl WriteCommand for CreateFirstAdmin {
    type Output = ();
    type Error = StoreError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        if AnyUser.read(tx)? {
            return Err(StoreError::UsersExist);
        }
        tx.execute(
            "INSERT INTO users (username, password_hash, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                self.username,
                self.password_hash.as_str(),
                Role::Admin.as_str(),
                self.now.0
            ],
        )?;
        let user = UserId(tx.last_insert_rowid());
        insert_session(tx, user, &self.token, self.now)?;
        Ok(())
    }
}

/// Create a session for a user who just proved their password.
pub(crate) struct CreateSession {
    pub(crate) user: UserId,
    pub(crate) token: SessionToken,
    pub(crate) now: UnixSeconds,
}

impl WriteCommand for CreateSession {
    type Output = UnixSeconds;
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<UnixSeconds> {
        insert_session(tx, self.user, &self.token, self.now)
    }
}

/// End one session. Returns whether it existed.
pub(crate) struct DeleteSession {
    pub(crate) token: SessionToken,
}

impl WriteCommand for DeleteSession {
    type Output = bool;
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<bool> {
        let deleted = tx.execute(
            "DELETE FROM sessions WHERE token = ?1",
            [self.token.expose()],
        )?;
        Ok(deleted > 0)
    }
}

/// Replace a user's password hash, but only if it is still the one the current password
/// was checked against, and end every other session of that user.
pub(crate) struct ChangePassword {
    pub(crate) user: UserId,
    pub(crate) checked_hash: PasswordHash,
    pub(crate) new_hash: PasswordHash,
    pub(crate) keep: SessionToken,
}

impl WriteCommand for ChangePassword {
    type Output = ();
    type Error = StoreError;

    fn apply(self, tx: &Transaction<'_>) -> Result<(), StoreError> {
        let updated = tx.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2 AND password_hash = ?3",
            params![
                self.new_hash.as_str(),
                self.user.0,
                self.checked_hash.as_str()
            ],
        )?;
        if updated == 0 {
            return Err(StoreError::PasswordChanged);
        }
        tx.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token <> ?2",
            params![self.user.0, self.keep.expose()],
        )?;
        Ok(())
    }
}
