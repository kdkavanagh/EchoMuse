//! The one error shape every API route answers with.
//!
//! The dashboard reads a failed response's JSON body as `{error, code}`: `error` is the
//! sentence it shows the user and `code` a stable word it can act on (dashboard.jsx,
//! `API.request`; index.html's `post` shows `error`). A 401 always means "sign in
//! again" to the dashboard, so only a missing, unknown or expired session answers 401.

use std::{borrow::Cow, fmt, time::Duration};

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use echomuse_db::ServiceError;
use serde::Serialize;

/// The `code` field of an error body: what went wrong, as a word the dashboard can test.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No session, or the session is unknown or expired. Always status 401.
    NotAuthenticated,
    /// Signed in, but this needs another role, or the request came from somewhere the
    /// controller does not serve.
    Forbidden,
    /// A username and password, or a current password, that do not match.
    InvalidCredentials,
    /// `POST /api/setup` with a setup token that is not the one printed at startup.
    InvalidSetupToken,
    /// `POST /api/setup` after a user exists.
    SetupComplete,
    /// No user exists, but this controller holds no setup token; restarting prints one.
    SetupUnavailable,
    /// A username that is empty, too long or contains control characters.
    InvalidUsername,
    /// A new password outside the accepted length.
    InvalidPassword,
    /// A request body or parameter that could not be understood.
    BadRequest,
    /// A body that is not `application/json` where JSON is required.
    UnsupportedMediaType,
    /// Too many failed sign-in attempts from this address; retry after `Retry-After`.
    TooManyAttempts,
    /// Nothing is served at this path.
    NotFound,
    /// The path exists but not for this method.
    MethodNotAllowed,
    /// The change raced with another one; read again and retry.
    Conflict,
    /// A configuration key that is not a setting.
    UnknownSetting,
    /// A configuration key the configuration removed.
    RemovedSetting,
    /// A setting value, section list or model choice the setting does not take.
    InvalidSetting,
    /// The stored configuration cannot be read, so a change is not written over it.
    StoredUnreadable,
    /// The thing is still in use (a wake model the fleet or a device selects).
    InUse,
    /// A wake model failed validation or qualification.
    ModelRejected,
    /// A request body larger than this route accepts.
    TooLarge,
    /// The device has no live session, so nothing can be done on it now.
    NotConnected,
    /// Another firmware operation is running on the device.
    Busy,
    /// The device already runs the bundled firmware.
    AlreadyCurrent,
    /// The operation does not apply to this device (debloat on LineageOS, rollback
    /// with nothing in the other slot).
    NotApplicable,
    /// A service this needs (the database, the settings, the device runtimes) is shutting
    /// down or not accepting work.
    Unavailable,
    /// Something this needs is still being prepared (transcript correction's tables);
    /// ask again soon.
    NotReady,
    /// The controller failed; the message says where.
    Internal,
}

/// A failed request: status, code and the sentence shown to the user.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    message: Cow<'static, str>,
    retry_after: Option<Duration>,
}

impl ApiError {
    /// An error with an explicit status.
    pub fn new(status: StatusCode, code: ErrorCode, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    /// 401: no usable session.
    pub fn not_authenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::NotAuthenticated,
            "Sign in to continue",
        )
    }

    /// 403: the signed-in user's role does not allow this.
    pub fn admin_only() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            ErrorCode::Forbidden,
            "Only an administrator can do this",
        )
    }

    /// 400 with the given reason.
    pub fn bad_request(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, message)
    }

    /// 404.
    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, "Not found")
    }

    /// 404 saying what is missing.
    pub(crate) fn no_such(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
    }

    /// 404: no device has this id.
    pub(crate) fn unknown_device() -> Self {
        Self::no_such("No device has this id")
    }

    /// 409: the device has no live session.
    pub(crate) fn not_connected() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            ErrorCode::NotConnected,
            "The device is not connected",
        )
    }

    /// 503: a service this needs is stopping or stopped.
    pub fn unavailable(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            message,
        )
    }

    /// 429 with a `Retry-After` header rounded up to whole seconds.
    pub fn too_many_attempts(retry_after: Duration) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::TooManyAttempts,
            "Too many failed attempts; wait before trying again",
        );
        error.retry_after = Some(retry_after);
        error
    }

    /// 500 naming what failed. The message must not contain a secret.
    pub fn internal(message: impl Into<Cow<'static, str>>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            message,
        )
    }

    /// The response status.
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The body's `code`.
    pub fn code(&self) -> ErrorCode {
        self.code
    }

    /// The body's `error` sentence.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?}): {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    code: ErrorCode,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: &self.message,
            code: self.code,
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some(wait) = self.retry_after {
            // Round up so a client that waits exactly this long is let through.
            let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds.max(1)));
        }
        response
    }
}

/// A database command that did not complete. Only the kind of failure reaches the
/// client; the detail goes to the controller's log.
impl<E: fmt::Display> From<ServiceError<E>> for ApiError {
    fn from(error: ServiceError<E>) -> Self {
        match error {
            ServiceError::Closed => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "The database is not accepting requests",
            ),
            ServiceError::Failed(error) => {
                tracing::error!(%error, "database command failed");
                Self::internal("A database command failed")
            }
            ServiceError::Aborted => {
                tracing::error!("database command panicked and was rolled back");
                Self::internal("A database command failed")
            }
            ServiceError::Unknown => {
                tracing::error!("database thread stopped before reporting a result");
                Self::internal("The database stopped before reporting a result")
            }
        }
    }
}
