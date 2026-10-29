//! `POST /api/setup`, `GET /api/system/setup-state` and `/api/auth/*`, as the landing
//! page (index.html) and the dashboard call them.

use std::net::{IpAddr, SocketAddr};

use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
    http::StatusCode,
    routing::{get, post},
};
use echomuse_db::ServiceError;
use serde::{Deserialize, Serialize};

use super::{
    AuthenticatedUser, Role,
    password::NewPassword,
    store::{
        AnyUser, ChangePassword, CreateFirstAdmin, CreateSession, DeleteSession, StoreError,
        UserById, UserByName,
    },
    token::SessionToken,
};
use crate::{
    body::JsonBody,
    clock::unix_now,
    error::{ApiError, ErrorCode},
    events::Revocation,
    state::ApiState,
};

/// Longest username, in characters.
const MAX_USERNAME_CHARS: usize = 64;

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/setup", post(setup))
        .route("/api/system/setup-state", get(setup_state))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/change-password", post(change_password))
}

/// `POST /api/setup` and `POST /api/auth/login` answer with the new session.
#[derive(Serialize)]
struct SessionResponse {
    token: String,
    role: Role,
}

/// `{ok: true}` for actions with nothing else to report.
#[derive(Serialize)]
struct Done {
    ok: bool,
}

const DONE: Json<Done> = Json(Done { ok: true });

fn client_address(peer: SocketAddr) -> IpAddr {
    peer.ip().to_canonical()
}

async fn throttle_check(state: &ApiState, address: IpAddr) -> Result<(), ApiError> {
    state
        .throttle()
        .check(address)
        .await
        .map_err(ApiError::too_many_attempts)
}

fn parse_username(username: &str) -> Result<String, ApiError> {
    let username = username.trim();
    let length = username.chars().count();
    if length == 0 || length > MAX_USERNAME_CHARS || username.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidUsername,
            "Username must be 1 to 64 characters, without control characters",
        ));
    }
    Ok(username.to_owned())
}

fn setup_complete() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        ErrorCode::SetupComplete,
        "Setup is already complete; sign in instead",
    )
}

/// The body index.html's setup form sends.
#[derive(Deserialize)]
struct SetupRequest {
    token: String,
    username: String,
    password: String,
}

/// Create the first administrator with the setup token printed at startup, and sign
/// them in. Refused once any user exists; the token works once.
async fn setup(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    JsonBody(request): JsonBody<SetupRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let address = client_address(peer);
    throttle_check(&state, address).await?;
    // Held to the end, so two setups cannot both pass the checks.
    let mut bootstrap = state.bootstrap().lock().await;
    if state.db().read(AnyUser).await? {
        return Err(setup_complete());
    }
    let Some(expected) = bootstrap.as_ref() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::SetupUnavailable,
            "No setup token is active; restart the controller to print a new one",
        ));
    };
    if !expected.accepts(request.token.trim()) {
        state.throttle().failed(address).await;
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::InvalidSetupToken,
            "That setup token is not the one in the controller's log",
        ));
    }
    let username = parse_username(&request.username)?;
    let password = NewPassword::parse(request.password)?;
    let password_hash = state.passwords().hash(password).await?;
    let token = SessionToken::generate()?;
    let now = unix_now(state.clock())?;
    let created = state
        .db()
        .write(CreateFirstAdmin {
            username,
            password_hash,
            token: token.clone(),
            now,
        })
        .await;
    match created {
        Ok(()) => {}
        Err(ServiceError::Failed(StoreError::UsersExist)) => return Err(setup_complete()),
        Err(error) => return Err(error.into()),
    }
    *bootstrap = None;
    state.throttle().succeeded(address).await;
    Ok(Json(SessionResponse {
        token: token.expose().to_owned(),
        role: Role::Admin,
    }))
}

#[derive(Serialize)]
struct SetupState {
    needs_setup: bool,
}

/// Whether the landing page should show the setup form. Needs no session.
async fn setup_state(State(state): State<ApiState>) -> Result<Json<SetupState>, ApiError> {
    let any_user = state.db().read(AnyUser).await?;
    Ok(Json(SetupState {
        needs_setup: !any_user,
    }))
}

/// The body index.html's sign-in form sends.
#[derive(Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

/// Check a username and password and start a session.
async fn login(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    JsonBody(request): JsonBody<LoginRequest>,
) -> Result<Json<SessionResponse>, ApiError> {
    let address = client_address(peer);
    throttle_check(&state, address).await?;
    let user = state
        .db()
        .read(UserByName {
            username: request.username.trim().to_owned(),
        })
        .await?;
    let verified = match user {
        Some(user) => {
            let hash = user.password_hash.clone();
            if state.passwords().verify(request.password, hash).await? {
                Some(user)
            } else {
                None
            }
        }
        None => {
            state.passwords().verify_decoy(request.password).await?;
            None
        }
    };
    let Some(user) = verified else {
        state.throttle().failed(address).await;
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::InvalidCredentials,
            "Invalid username or password",
        ));
    };
    state.throttle().succeeded(address).await;
    let token = SessionToken::generate()?;
    let now = unix_now(state.clock())?;
    state
        .db()
        .write(CreateSession {
            user: user.id,
            token: token.clone(),
            now,
        })
        .await?;
    Ok(Json(SessionResponse {
        token: token.expose().to_owned(),
        role: user.role,
    }))
}

/// End the session this request used, and close event sockets opened with it.
async fn logout(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
) -> Result<Json<Done>, ApiError> {
    let token = user.token().clone();
    state
        .db()
        .write(DeleteSession {
            token: token.clone(),
        })
        .await?;
    state.events().revoke(Revocation::Session(token));
    Ok(DONE)
}

#[derive(Serialize)]
struct Me {
    username: String,
    role: Role,
}

/// Who is signed in. The landing page uses it to check a stored session.
async fn me(user: AuthenticatedUser) -> Json<Me> {
    Json(Me {
        role: user.role(),
        username: user.username().to_owned(),
    })
}

/// The body the dashboard's Account tab sends.
#[derive(Deserialize)]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// Change the signed-in user's password. The session that asked stays signed in; every
/// other session of that user ends, and their event sockets close.
async fn change_password(
    State(state): State<ApiState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    user: AuthenticatedUser,
    JsonBody(request): JsonBody<ChangePasswordRequest>,
) -> Result<Json<Done>, ApiError> {
    let address = client_address(peer);
    throttle_check(&state, address).await?;
    let new_password = NewPassword::parse(request.new_password)?;
    let stored = state
        .db()
        .read(UserById { id: user.user_id() })
        .await?
        .ok_or_else(ApiError::not_authenticated)?;
    let checked_hash = stored.password_hash.clone();
    if !state
        .passwords()
        .verify(request.current_password, stored.password_hash)
        .await?
    {
        state.throttle().failed(address).await;
        // Not 401: the session is fine, and the dashboard treats 401 as signed out.
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            ErrorCode::InvalidCredentials,
            "Current password is incorrect",
        ));
    }
    state.throttle().succeeded(address).await;
    let new_hash = state.passwords().hash(new_password).await?;
    let changed = state
        .db()
        .write(ChangePassword {
            user: user.user_id(),
            checked_hash,
            new_hash,
            keep: user.token().clone(),
        })
        .await;
    match changed {
        Ok(()) => {}
        Err(ServiceError::Failed(StoreError::PasswordChanged)) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                "The password was changed by another request; try again",
            ));
        }
        Err(error) => return Err(error.into()),
    }
    state.events().revoke(Revocation::OtherSessions {
        user: user.user_id(),
        keep: user.token().clone(),
    });
    Ok(DONE)
}
