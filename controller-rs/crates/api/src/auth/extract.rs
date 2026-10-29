//! Extractors that admit a request only with a live session, or only an administrator's.

use axum::{
    extract::{FromRequestParts, Query},
    http::{HeaderMap, StatusCode, header, request::Parts},
};
use echomuse_db::devices::UnixSeconds;
use serde::Deserialize;

use super::{Role, UserId, store::LiveSession, token::SessionToken};
use crate::{
    clock::unix_now,
    error::{ApiError, ErrorCode},
    state::ApiState,
};

/// The signed-in user behind a request. Extracting it answers 401 when the request
/// carries no session, or one that is unknown or expired.
#[derive(Clone, Debug)]
pub struct AuthenticatedUser {
    user_id: UserId,
    username: String,
    role: Role,
    token: SessionToken,
    expires_at: UnixSeconds,
}

impl AuthenticatedUser {
    /// `users.id`.
    pub fn user_id(&self) -> UserId {
        self.user_id
    }

    /// `users.username`.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The user's role.
    pub fn role(&self) -> Role {
        self.role
    }

    /// When the session used for this request ends.
    pub fn expires_at(&self) -> UnixSeconds {
        self.expires_at
    }

    /// The session token this request presented.
    pub(crate) fn token(&self) -> &SessionToken {
        &self.token
    }
}

/// A signed-in administrator. Extracting it answers 401 without a session and 403 for
/// a `readonly` user.
#[derive(Clone, Debug)]
pub struct AdminUser(pub AuthenticatedUser);

impl FromRequestParts<ApiState> for AuthenticatedUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let token = presented_token(parts)?.ok_or_else(ApiError::not_authenticated)?;
        let now = unix_now(state.clock())?;
        let session = state
            .db()
            .read(LiveSession {
                token: token.clone(),
                now,
            })
            .await?
            .ok_or_else(ApiError::not_authenticated)?;
        Ok(Self {
            user_id: session.user_id,
            username: session.username,
            role: session.role,
            token,
            expires_at: session.expires_at,
        })
    }
}

impl FromRequestParts<ApiState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &ApiState) -> Result<Self, ApiError> {
        let user = AuthenticatedUser::from_request_parts(parts, state).await?;
        match user.role {
            Role::Admin => Ok(Self(user)),
            Role::Readonly => Err(ApiError::admin_only()),
        }
    }
}

/// The session token a request presents, if any. A credential that is present but
/// malformed is refused rather than ignored.
fn presented_token(parts: &Parts) -> Result<Option<SessionToken>, ApiError> {
    if let Some(value) = parts.headers.get(header::AUTHORIZATION) {
        let text = value.to_str().map_err(|_| ApiError::not_authenticated())?;
        let (scheme, token) = text
            .split_once(' ')
            .ok_or_else(ApiError::not_authenticated)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(ApiError::not_authenticated());
        }
        return SessionToken::from_presented(token.trim())
            .map(Some)
            .ok_or_else(ApiError::not_authenticated);
    }
    if !is_websocket_upgrade(&parts.headers) {
        return Ok(None);
    }

    #[derive(Deserialize)]
    struct TokenQuery {
        token: Option<String>,
    }
    if let Ok(Query(TokenQuery { token: Some(token) })) = Query::try_from_uri(&parts.uri) {
        return SessionToken::from_presented(&token)
            .map(Some)
            .ok_or_else(ApiError::not_authenticated);
    }
    if let Some(cookie) = session_cookie(&parts.headers) {
        if !origin_matches_host(&parts.headers) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "A session cookie is accepted only from the controller's own pages",
            ));
        }
        return SessionToken::from_presented(cookie)
            .map(Some)
            .ok_or_else(ApiError::not_authenticated);
    }
    Ok(None)
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::UPGRADE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|protocol| protocol.trim().eq_ignore_ascii_case("websocket"))
        })
}

/// The value of the `session` cookie.
fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == "session")
        .map(|(_, value)| value.trim_matches('"'))
}

/// True when the request has no `Origin`, or its origin's host and port are the
/// request's `Host`.
fn origin_matches_host(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin
        .to_str()
        .ok()
        .and_then(|origin| origin.split_once("://"))
        .is_some_and(|(_, authority)| authority.eq_ignore_ascii_case(host))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderValue, Request};

    use super::*;

    fn parts(uri: &str, headers: &[(&str, &str)]) -> Parts {
        let mut request = Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(()).unwrap().into_parts().0
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn found(parts: &Parts) -> Option<String> {
        presented_token(parts)
            .unwrap()
            .map(|t| t.expose().to_owned())
    }

    #[test]
    fn bearer_header_on_any_request() {
        let bearer = format!("Bearer {TOKEN}");
        let p = parts("/api/auth/me", &[("authorization", &bearer)]);
        assert_eq!(found(&p).as_deref(), Some(TOKEN));
        let p = parts("/api/auth/me", &[("authorization", "Basic abc")]);
        assert_eq!(
            presented_token(&p).unwrap_err().code(),
            ErrorCode::NotAuthenticated
        );
    }

    #[test]
    fn query_and_cookie_only_on_websocket_upgrades() {
        let uri = format!("/api/events?token={TOKEN}");
        let cookie = format!("theme=dark; session={TOKEN}");
        assert_eq!(found(&parts(&uri, &[])), None);
        assert_eq!(found(&parts("/api/events", &[("cookie", &cookie)])), None);

        let ws = ("upgrade", "websocket");
        assert_eq!(found(&parts(&uri, &[ws])).as_deref(), Some(TOKEN));
        assert_eq!(
            found(&parts("/api/events", &[ws, ("cookie", &cookie)])).as_deref(),
            Some(TOKEN)
        );
    }

    #[test]
    fn cookie_from_another_site_is_refused() {
        let cookie = format!("session={TOKEN}");
        let same = parts(
            "/api/events",
            &[
                ("upgrade", "websocket"),
                ("cookie", &cookie),
                ("host", "echomuse.local:8768"),
                ("origin", "http://echomuse.local:8768"),
            ],
        );
        assert_eq!(found(&same).as_deref(), Some(TOKEN));
        let mut other = same.clone();
        other.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert_eq!(
            presented_token(&other).unwrap_err().code(),
            ErrorCode::Forbidden
        );
    }
}
