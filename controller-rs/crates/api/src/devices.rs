//! The device routes (§3.4, §4.10) and the device object the dashboard holds.
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/devices` | signed in | every device object, ordered by id |
//! | `GET /api/devices/pending` | signed in | the devices waiting for approval |
//! | `GET /api/devices/{id}` | signed in | one device object |
//! | `PATCH /api/devices/{id}` `{label}` | admin | the device object; publishes `device_update` |
//! | `DELETE /api/devices/{id}` | admin | `{deleted}`; publishes `device_deleted` |
//! | `POST /api/devices/{id}/approve` `{label}` | admin | the device object; publishes `device_approved` |
//! | `GET /api/devices/{id}/config` | signed in | `{config, config_sections, use_global_config}` |
//! | `POST /api/devices/{id}/config` | admin | the same, after storing the edit |
//! | `GET /api/devices/{id}/logs?limit=` | signed in | the newest log entries first |
//!
//! A device object joins the `devices` row, the device's settings and what its runtime
//! last published ([`LiveDevice`]). Its keys are the ones dashboard.jsx reads; a value
//! nobody knows is `null`, never a stand-in such as `0` or `false`. `config` holds the
//! device's own values: those of the sections it overrides, plus `startupVolume`, which
//! is always its own (dashboard.jsx, `effectiveConfig`). The kiosk password is a setting
//! the dashboard edits, so administrators see it; anyone else reads `null`. The link and
//! Kiosk Satellite tokens never leave the controller (`kioskTokenSet` says whether one
//! is held).

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use echomuse_core::types::DeviceId;
use echomuse_db::{ReadQuery, WriteCommand};
use echomuse_settings::{
    Scope, SettingKey,
    edit::{DeviceEdit, EditError},
    service::{DeviceEntry, SettingsSnapshot},
};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    auth::{AdminUser, AuthenticatedUser, Role},
    body::JsonBody,
    error::{ApiError, ErrorCode},
    events::{ApiEvent, DeviceStatePatch, DeviceView},
    firmware::UpdateStatus,
    params::{DevicePath, QueryParams},
    runtime::{LiveDevice, RuntimeError, SettingsApplied},
    state::ApiState,
};

/// A device label: what the dashboard shows for a device, and its name in Home
/// Assistant. Trimmed, 1 to [`DeviceLabel::MAX_CHARS`] characters, no control
/// characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceLabel(String);

impl DeviceLabel {
    /// The most characters a label may have.
    pub const MAX_CHARS: usize = 64;

    /// The label `text` trims to, when it is one.
    pub fn new(text: &str) -> Option<Self> {
        let trimmed = text.trim();
        let chars = trimmed.chars().count();
        ((1..=Self::MAX_CHARS).contains(&chars) && !trimmed.chars().any(char::is_control))
            .then(|| Self(trimmed.to_owned()))
    }

    /// The label.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The label, owned.
    pub fn into_string(self) -> String {
        self.0
    }
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/devices", get(list))
        .route("/api/devices/pending", get(pending))
        .route("/api/devices/{id}", get(one).patch(relabel).delete(delete))
        .route("/api/devices/{id}/approve", post(approve))
        .route("/api/devices/{id}/config", get(config).post(edit_config))
        .route("/api/devices/{id}/logs", get(logs))
}

// ─── The `devices` rows the object shows ────────────────────────────────────────

/// The `devices` columns a device object shows. Secrets are not read, only whether
/// a Kiosk Satellite token is held.
#[derive(Clone, Debug)]
struct DeviceRecord {
    device_id: DeviceId,
    label: Option<String>,
    approved: bool,
    ip: Option<String>,
    firmware_ver: Option<String>,
    firmware_previous: Option<String>,
    os_version: Option<String>,
    first_seen: Option<i64>,
    last_seen: Option<i64>,
    update_queued_at: Option<i64>,
    esphome_api_port: Option<u16>,
    collect_mode: bool,
    ambient_mode: bool,
    kiosk_token_set: bool,
}

const RECORD_COLUMNS: &str = "device_id, label, approved, ip, firmware_ver, firmware_previous, \
     os_version, first_seen, last_seen, update_queued_at, esphome_api_port, collect_mode, \
     ambient_mode, coalesce(kiosk_token, '') != ''";

fn record(row: &Row<'_>) -> rusqlite::Result<DeviceRecord> {
    Ok(DeviceRecord {
        device_id: DeviceId(row.get(0)?),
        label: row.get(1)?,
        approved: row.get::<_, i64>(2)? != 0,
        ip: row.get(3)?,
        firmware_ver: row.get(4)?,
        firmware_previous: row.get(5)?,
        os_version: row.get(6)?,
        first_seen: row.get(7)?,
        last_seen: row.get(8)?,
        update_queued_at: row.get(9)?,
        esphome_api_port: row.get(10)?,
        collect_mode: row.get::<_, i64>(11)? != 0,
        ambient_mode: row.get::<_, i64>(12)? != 0,
        kiosk_token_set: row.get(13)?,
    })
}

/// Which rows to read.
#[derive(Clone, Debug)]
enum Which {
    All,
    Pending,
    One(DeviceId),
}

/// Device rows, ordered by id.
struct ReadRecords(Which);

impl ReadQuery for ReadRecords {
    type Output = Vec<DeviceRecord>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Vec<DeviceRecord>> {
        let (filter, id) = match &self.0 {
            Which::All => ("1", None),
            Which::Pending => ("approved = 0", None),
            Which::One(device) => ("device_id = ?1", Some(device.0.as_str())),
        };
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {RECORD_COLUMNS} FROM devices WHERE {filter} ORDER BY device_id"
        ))?;
        let rows = match id {
            Some(id) => statement.query_map([id], record)?,
            None => statement.query_map([], record)?,
        };
        rows.collect()
    }
}

/// Set a device's label. False when no row has the id.
struct WriteLabel {
    device: DeviceId,
    label: DeviceLabel,
}

impl WriteCommand for WriteLabel {
    type Output = bool;
    type Error = rusqlite::Error;

    fn apply(self, tx: &Transaction<'_>) -> rusqlite::Result<bool> {
        let changed = tx
            .prepare_cached("UPDATE devices SET label = ?2 WHERE device_id = ?1")?
            .execute(params![self.device.0, self.label.as_str()])?;
        Ok(changed > 0)
    }
}

// ─── The device object ──────────────────────────────────────────────────────────

/// The device's own values, as `config` carries them: every key of an overridden
/// section, plus the device-state keys.
fn own_config(entry: &DeviceEntry, viewer: Role) -> Map<String, Value> {
    let overridden = entry.overridden();
    let mut config: Map<String, Value> = SettingKey::ALL
        .iter()
        .copied()
        .filter(|key| match key.scope() {
            Scope::Section(section) => overridden.contains(&section),
            Scope::State => true,
        })
        .map(|key| (key.as_str().to_owned(), setting_value(entry, key)))
        .collect();
    hide_secrets(&mut config, viewer);
    config
}

fn setting_value(entry: &DeviceEntry, key: SettingKey) -> Value {
    serde_json::to_value(entry.settings().get(key).to_json()).unwrap_or(Value::Null)
}

/// The kiosk password goes only to administrators, who edit it.
pub(crate) fn hide_secrets(config: &mut Map<String, Value>, viewer: Role) {
    if viewer != Role::Admin
        && let Some(value) = config.get_mut(SettingKey::ScreenWakePassword.as_str())
    {
        *value = Value::Null;
    }
}

/// `config`, `config_sections` and `use_global_config`; `null` while the settings do
/// not know the device yet.
fn config_fields(entry: Option<&DeviceEntry>, viewer: Role) -> Map<String, Value> {
    let mut fields = Map::new();
    match entry {
        Some(entry) => {
            let sections: Vec<&str> = entry.overridden().iter().map(|s| s.as_str()).collect();
            fields.insert("config".into(), Value::Object(own_config(entry, viewer)));
            fields.insert("config_sections".into(), json!(sections));
            fields.insert(
                "use_global_config".into(),
                json!(entry.overridden().is_empty()),
            );
        }
        None => {
            for key in ["config", "config_sections", "use_global_config"] {
                fields.insert(key.into(), Value::Null);
            }
        }
    }
    fields
}

fn view(
    record: &DeviceRecord,
    settings: &SettingsSnapshot,
    live: Option<LiveDevice>,
    update: &UpdateStatus,
    viewer: Role,
) -> DeviceView {
    let mut fields = Map::new();
    fields.extend([
        ("device_id".into(), json!(record.device_id)),
        ("label".into(), json!(record.label)),
        ("approved".into(), json!(record.approved)),
        ("ip".into(), json!(record.ip)),
        ("firmware_ver".into(), json!(record.firmware_ver)),
        ("firmware_previous".into(), json!(record.firmware_previous)),
        ("os_version".into(), json!(record.os_version)),
        ("first_seen".into(), json!(record.first_seen)),
        ("last_seen".into(), json!(record.last_seen)),
        ("update_queued_at".into(), json!(record.update_queued_at)),
        ("esphome_port".into(), json!(record.esphome_api_port)),
        ("collectMode".into(), json!(record.collect_mode)),
        ("ambientMode".into(), json!(record.ambient_mode)),
        ("kioskTokenSet".into(), json!(record.kiosk_token_set)),
        ("update_in_progress".into(), json!(update.in_progress)),
        ("update_error".into(), json!(update.error)),
    ]);
    fields.extend(config_fields(settings.device(&record.device_id), viewer));
    // A device without a runtime (pending) is not connected.
    fields.extend(live.unwrap_or_default().fields());
    DeviceView::from_fields(fields)
}

async fn views(state: &ApiState, which: Which, viewer: Role) -> Result<Vec<DeviceView>, ApiError> {
    let records = state.db().read(ReadRecords(which)).await?;
    let settings = state.settings().snapshot();
    Ok(records
        .iter()
        .map(|record| {
            let live = state.runtimes().live(&record.device_id);
            let update = state.firmware().status(&record.device_id);
            let mut fields = view(record, &settings, live, &update, viewer)
                .fields()
                .clone();
            // Wi-Fi state and live recording counters, from the device services.
            fields.extend(state.services().device_fields(&record.device_id));
            fields.extend(state.capture().fields(&record.device_id));
            DeviceView::from_fields(fields)
        })
        .collect())
}

/// Every device object, for `GET /api/devices` and each events socket's `snapshot`.
pub(crate) async fn all_views(state: &ApiState, viewer: Role) -> Result<Vec<DeviceView>, ApiError> {
    views(state, Which::All, viewer).await
}

async fn one_view(
    state: &ApiState,
    device: &DeviceId,
    viewer: Role,
) -> Result<DeviceView, ApiError> {
    views(state, Which::One(device.clone()), viewer)
        .await?
        .pop()
        .ok_or_else(unknown_device)
}

fn unknown_device() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "No device has this id",
    )
}

fn runtime_error(error: RuntimeError, doing: &'static str) -> ApiError {
    match error {
        RuntimeError::UnknownDevice => unknown_device(),
        RuntimeError::Unavailable => ApiError::unavailable("The device runtimes are shutting down"),
        RuntimeError::Failed(reason) => {
            eprintln!("api: {doing} failed: {reason}");
            ApiError::internal(format!("The controller could not {doing} the device"))
        }
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────────────

async fn list(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<DeviceView>>, ApiError> {
    Ok(Json(all_views(&state, user.role()).await?))
}

async fn pending(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<DeviceView>>, ApiError> {
    Ok(Json(views(&state, Which::Pending, user.role()).await?))
}

async fn one(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Json<DeviceView>, ApiError> {
    Ok(Json(one_view(&state, &device, user.role()).await?))
}

/// `{label}` of a rename or an approval.
#[derive(Deserialize)]
struct LabelBody {
    label: String,
}

fn label(body: &LabelBody) -> Result<DeviceLabel, ApiError> {
    DeviceLabel::new(&body.label).ok_or_else(|| {
        ApiError::bad_request(format!(
            "A label must be 1 to {} characters, without control characters",
            DeviceLabel::MAX_CHARS
        ))
    })
}

async fn relabel(
    State(state): State<ApiState>,
    AdminUser(user): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<LabelBody>,
) -> Result<Json<DeviceView>, ApiError> {
    let label = label(&body)?;
    let text = label.as_str().to_owned();
    let found = state
        .db()
        .write(WriteLabel {
            device: device.clone(),
            label,
        })
        .await?;
    if !found {
        return Err(unknown_device());
    }
    state
        .runtimes()
        .relabeled(device.clone(), text.clone())
        .await;
    state.events().publisher().publish(&ApiEvent::DeviceUpdate {
        device_id: device.clone(),
        state: DeviceStatePatch(Map::from_iter([("label".to_owned(), json!(text))])),
    });
    Ok(Json(one_view(&state, &device, user.role()).await?))
}

async fn approve(
    State(state): State<ApiState>,
    AdminUser(user): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<LabelBody>,
) -> Result<Json<DeviceView>, ApiError> {
    let label = label(&body)?;
    if state
        .db()
        .read(ReadRecords(Which::One(device.clone())))
        .await?
        .is_empty()
    {
        return Err(unknown_device());
    }
    state
        .runtimes()
        .approve(device.clone(), label)
        .await
        .map_err(|e| runtime_error(e, "approve"))?;
    state
        .events()
        .publisher()
        .publish(&ApiEvent::DeviceApproved {
            device_id: device.clone(),
        });
    Ok(Json(one_view(&state, &device, user.role()).await?))
}

#[derive(Serialize)]
struct Deleted {
    deleted: DeviceId,
}

async fn delete(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Deleted>, ApiError> {
    if state
        .db()
        .read(ReadRecords(Which::One(device.clone())))
        .await?
        .is_empty()
    {
        return Err(unknown_device());
    }
    state
        .runtimes()
        .delete(device.clone())
        .await
        .map_err(|e| runtime_error(e, "delete"))?;
    state
        .events()
        .publisher()
        .publish(&ApiEvent::DeviceDeleted {
            device_id: device.clone(),
        });
    // The settings drop the device once they read that its row is gone.
    if let Err(error) = state.settings().refresh_device(&device).await {
        eprintln!("api: the settings did not drop deleted device {device}: {error}");
    }
    Ok(Json(Deleted { deleted: device }))
}

// ─── Configuration ──────────────────────────────────────────────────────────────

/// What a configuration edit's refusal looks like to the dashboard: `{error, code}`,
/// with the sentence it shows (`alert(e.error)`).
pub(crate) fn edit_error(error: EditError) -> ApiError {
    use axum::http::StatusCode;
    let message = error.to_string();
    match error {
        EditError::NotAnObject(_) => ApiError::bad_request(message),
        EditError::UnknownKey(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, ErrorCode::UnknownSetting, message)
        }
        EditError::RemovedKey(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, ErrorCode::RemovedSetting, message)
        }
        EditError::InvalidValue { .. }
        | EditError::UnknownSection(_)
        | EditError::InvalidSections(_)
        | EditError::UnknownWakeModel(_)
        | EditError::OpenRuleIsBaseline { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidSetting, message)
        }
        EditError::UnknownDevice(_) => unknown_device(),
        EditError::StoredUnreadable => {
            ApiError::new(StatusCode::CONFLICT, ErrorCode::StoredUnreadable, message)
        }
        EditError::Database(reason) => {
            eprintln!("api: a configuration edit was not stored: {reason}");
            ApiError::internal("The configuration was not stored")
        }
        EditError::Closed => ApiError::unavailable("The settings service has stopped"),
    }
}

async fn config(
    State(state): State<ApiState>,
    user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Map<String, Value>>, ApiError> {
    let settings = state.settings().snapshot();
    let entry = settings.device(&device).ok_or_else(unknown_device)?;
    Ok(Json(config_fields(Some(entry), user.role())))
}

/// The dashboard sends `config_sections` plus the whole effective configuration; only
/// the overridden sections' values and `startupVolume` are stored (dashboard.jsx,
/// `pushConfig`).
async fn edit_config(
    State(state): State<ApiState>,
    AdminUser(user): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<Value>,
) -> Result<Json<Map<String, Value>>, ApiError> {
    let edit = DeviceEdit::from_value(&body).map_err(edit_error)?;
    let outcome = state
        .settings()
        .apply_device_edit(device.clone(), edit)
        .await
        .map_err(edit_error)?;
    state
        .runtimes()
        .settings_applied(SettingsApplied {
            changed: outcome.changed_devices,
            new_session: outcome.new_session_needed,
        })
        .await;
    let entry = outcome
        .snapshot
        .device(&device)
        .ok_or_else(unknown_device)?;
    Ok(Json(config_fields(Some(entry), user.role())))
}

// ─── Logs ───────────────────────────────────────────────────────────────────────

/// Log entries returned when the request names no limit.
pub const DEFAULT_LOG_LIMIT: u32 = 100;
/// The most log entries one request returns.
pub const MAX_LOG_LIMIT: u32 = 1_000;

#[derive(Deserialize)]
struct LogsQuery {
    limit: Option<u32>,
}

/// One `device_logs` row. `ts` is milliseconds since the Unix epoch, as real rows
/// store it and as the dashboard reads it (`new Date(entry.ts)`).
#[derive(Debug, Serialize)]
struct LogEntry {
    id: i64,
    ts: i64,
    level: String,
    source: String,
    message: String,
}

/// The newest entries of a device's log, newest first; `None` when no device has the
/// id.
struct ReadLogs {
    device: DeviceId,
    limit: u32,
}

impl ReadQuery for ReadLogs {
    type Output = Option<Vec<LogEntry>>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Option<Vec<LogEntry>>> {
        let known = conn
            .prepare_cached("SELECT 1 FROM devices WHERE device_id = ?1")?
            .query_row([&self.device.0], |_| Ok(()))
            .optional()?;
        if known.is_none() {
            return Ok(None);
        }
        let mut statement = conn.prepare_cached(
            "SELECT id, ts, level, source, message FROM device_logs WHERE device_id = ?1 \
             ORDER BY ts DESC, id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![self.device.0, self.limit], |row| {
            Ok(LogEntry {
                id: row.get(0)?,
                ts: row.get(1)?,
                level: row.get(2)?,
                source: row.get(3)?,
                message: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>().map(Some)
    }
}

async fn logs(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<LogsQuery>,
) -> Result<Json<Vec<LogEntry>>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if !(1..=MAX_LOG_LIMIT).contains(&limit) {
        return Err(ApiError::bad_request(format!(
            "limit must be 1 to {MAX_LOG_LIMIT}"
        )));
    }
    state
        .db()
        .read(ReadLogs { device, limit })
        .await?
        .map(Json)
        .ok_or_else(unknown_device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_trimmed_and_bounded() {
        assert_eq!(DeviceLabel::new("  Kitchen ").unwrap().as_str(), "Kitchen");
        assert!(DeviceLabel::new("   ").is_none());
        assert!(DeviceLabel::new("a\u{7}b").is_none());
        assert!(DeviceLabel::new(&"é".repeat(DeviceLabel::MAX_CHARS)).is_some());
        assert!(DeviceLabel::new(&"é".repeat(DeviceLabel::MAX_CHARS + 1)).is_none());
    }
}
