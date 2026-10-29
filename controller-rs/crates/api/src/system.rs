//! The fleet configuration, the controller-wide settings, and what the controller and
//! Home Assistant report about themselves (§3.4, §4.10).
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/global/config` | signed in | the fleet's value of every setting |
//! | `POST /api/global/config` | admin | `{pushed_to}`: the devices the change was sent to |
//! | `GET /api/system/config` | admin | `{device_approval, session_expiry_days}` |
//! | `PATCH /api/system/config` | admin | the same, after storing the change |
//! | `GET /api/system/status` | signed in | `{controller_version}` |
//! | `GET /api/ha/status` | signed in | `{features, calendar_url}` |
//!
//! The system settings are the ones docs/configuration.md ("System settings") lists that
//! this controller acts on: the approval policy and the sign-in lifetime. The other two
//! it lists, `update_check_interval` and `github_repo`, belong to the controller release
//! check, which this controller does not run; they are refused like any key it does
//! not serve, and their stored rows are left as they are.

use std::collections::BTreeMap;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use echomuse_core::types::DeviceId;
use echomuse_db::{
    ReadQuery, WriteCommand,
    contract::SystemConfigKey,
    devices::{ApprovalMode, DeviceError, ReadApprovalMode},
};
use echomuse_settings::edit::SettingsPatch;
use rusqlite::{Connection, Transaction, params};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use crate::{
    auth::{
        AdminUser, AuthenticatedUser,
        store::{MAX_SESSION_DAYS, SESSION_EXPIRY_KEY, session_days},
    },
    body::JsonBody,
    devices::{edit_error, hide_secrets},
    error::{ApiError, ErrorCode},
    events::FeatureStatus,
    runtime::SettingsApplied,
    state::ApiState,
};

/// What Home Assistant provisioning reports, as `GET /api/ha/status` answers and the
/// `ha_status` event repeats. The integrator publishes it through a
/// `tokio::sync::watch` channel; each new value is sent to every open dashboard.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct HaStatus {
    /// Each provisioning step's state, by name.
    pub features: BTreeMap<String, FeatureStatus>,
    /// Home Assistant's calendar editor, which the alert panel links to; `None` when
    /// unknown.
    pub calendar_url: Option<String>,
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/global/config", get(fleet).post(edit_fleet))
        .route(
            "/api/system/config",
            get(system_config).patch(edit_system_config),
        )
        .route("/api/system/status", get(system_status))
        .route("/api/ha/status", get(ha_status))
}

// ─── The fleet configuration ────────────────────────────────────────────────────

async fn fleet(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
) -> Result<Json<Map<String, Value>>, ApiError> {
    let snapshot = state.settings().snapshot();
    let Value::Object(mut config) = serde_json::to_value(&**snapshot.fleet())
        .map_err(|_| ApiError::internal("The fleet configuration could not be shown"))?
    else {
        return Err(ApiError::internal(
            "The fleet configuration could not be shown",
        ));
    };
    hide_secrets(&mut config, user.role());
    Ok(Json(config))
}

#[derive(Serialize)]
struct Pushed {
    pushed_to: Vec<DeviceId>,
}

/// The dashboard sends every setting (dashboard.jsx, `saveGlobalConfig`).
async fn edit_fleet(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    JsonBody(body): JsonBody<Value>,
) -> Result<Json<Pushed>, ApiError> {
    let patch = SettingsPatch::from_value(&body).map_err(edit_error)?;
    let outcome = state
        .settings()
        .apply_fleet_edit(patch)
        .await
        .map_err(edit_error)?;
    let pushed_to = state
        .runtimes()
        .settings_applied(SettingsApplied {
            changed: outcome.changed_devices,
            new_session: outcome.new_session_needed,
        })
        .await;
    Ok(Json(Pushed { pushed_to }))
}

// ─── System settings ────────────────────────────────────────────────────────────

/// The controller-wide settings in effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SystemSettings {
    #[serde(serialize_with = "approval_text")]
    device_approval: ApprovalMode,
    session_expiry_days: i64,
}

fn approval_text<S: Serializer>(mode: &ApprovalMode, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(mode.as_str())
}

#[derive(Debug)]
enum SystemError {
    Device(DeviceError),
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for SystemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => error.fmt(f),
            Self::Sqlite(error) => error.fmt(f),
        }
    }
}

impl From<rusqlite::Error> for SystemError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// The settings in effect: the stored approval policy, else the deployment's
/// `DEVICE_APPROVAL`; the stored sign-in lifetime, else its default.
struct ReadSystemSettings {
    fallback: ApprovalMode,
}

impl ReadQuery for ReadSystemSettings {
    type Output = SystemSettings;
    type Error = SystemError;

    fn read(self, conn: &Connection) -> Result<SystemSettings, SystemError> {
        let device_approval = ReadApprovalMode {
            fallback: self.fallback,
        }
        .read(conn)
        .map_err(SystemError::Device)?;
        Ok(SystemSettings {
            device_approval,
            session_expiry_days: session_days(conn)?,
        })
    }
}

/// Store system settings as decimal or plain text, creating rows that are missing.
struct WriteSystemSettings(Vec<(&'static str, String)>);

impl WriteCommand for WriteSystemSettings {
    type Output = ();
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        let mut statement = tx.prepare_cached(
            "INSERT INTO system_config (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?;
        for (key, value) in self.0 {
            statement.execute(params![key, value])?;
        }
        Ok(())
    }
}

pub(crate) async fn read_system(state: &ApiState) -> Result<SystemSettings, ApiError> {
    Ok(state
        .db()
        .read(ReadSystemSettings {
            fallback: state.approval_fallback(),
        })
        .await?)
}

async fn system_config(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
) -> Result<Json<SystemSettings>, ApiError> {
    Ok(Json(read_system(&state).await?))
}

fn invalid(message: String) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidSetting, message)
}

/// Parse a system settings change: each key it names, checked, as stored text.
fn system_changes(body: &Value) -> Result<Vec<(&'static str, String)>, ApiError> {
    let Value::Object(object) = body else {
        return Err(ApiError::bad_request(
            "The system settings must be a JSON object",
        ));
    };
    let approval_key = SystemConfigKey::DeviceApproval.as_str();
    object
        .iter()
        .map(|(key, value)| {
            if key == approval_key {
                let mode = value.as_str().and_then(ApprovalMode::parse).ok_or_else(|| {
                    invalid(format!("{approval_key}: must be \"strict\" or \"auto\""))
                })?;
                Ok((approval_key, mode.as_str().to_owned()))
            } else if key == SESSION_EXPIRY_KEY {
                let days = value
                    .as_i64()
                    .filter(|days| (1..=MAX_SESSION_DAYS).contains(days))
                    .ok_or_else(|| {
                        invalid(format!(
                            "{SESSION_EXPIRY_KEY}: must be a whole number of days from 1 to {MAX_SESSION_DAYS}"
                        ))
                    })?;
                Ok((SESSION_EXPIRY_KEY, days.to_string()))
            } else {
                Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    ErrorCode::UnknownSetting,
                    format!("{key}: not a system setting this controller serves"),
                ))
            }
        })
        .collect()
}

async fn edit_system_config(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    JsonBody(body): JsonBody<Value>,
) -> Result<Json<SystemSettings>, ApiError> {
    let changes = system_changes(&body)?;
    if !changes.is_empty() {
        state.db().write(WriteSystemSettings(changes)).await?;
    }
    Ok(Json(read_system(&state).await?))
}

// ─── Status ─────────────────────────────────────────────────────────────────────

/// `GET /api/system/status`; dashboard.jsx reads `controller_version`.
#[derive(Serialize)]
struct SystemStatus {
    controller_version: Option<String>,
}

async fn system_status(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
) -> Json<SystemStatus> {
    Json(SystemStatus {
        controller_version: state.controller_version().map(str::to_owned),
    })
}

async fn ha_status(State(state): State<ApiState>, _user: AuthenticatedUser) -> Json<HaStatus> {
    Json(state.ha_status())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn system_changes_are_checked_key_by_key() {
        assert_eq!(
            system_changes(&json!({"device_approval": "auto", "session_expiry_days": 7})).unwrap(),
            [
                ("device_approval", "auto".to_owned()),
                ("session_expiry_days", "7".to_owned())
            ]
        );
        let error = system_changes(&json!({"github_repo": "someone/else"})).unwrap_err();
        assert_eq!(error.code(), ErrorCode::UnknownSetting);
        let error = system_changes(&json!({"device_approval": "open"})).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidSetting);
        let error = system_changes(&json!({"session_expiry_days": 0})).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidSetting);
        let error = system_changes(&json!({"session_expiry_days": 2.5})).unwrap_err();
        assert_eq!(error.code(), ErrorCode::InvalidSetting);
    }
}
