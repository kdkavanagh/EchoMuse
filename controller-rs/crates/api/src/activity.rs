//! The Activity and diagnostics routes (§3.4 "Turns and activity", §4.11): voice turns and
//! near misses, a turn's decision trace, the native AFE series, near-miss verdicts, the
//! shadow-rule table and the response-latency percentiles.
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/turns?limit=&since=` | signed in | every device's newest turns, oldest first |
//! | `GET /api/devices/{id}/turns?limit=&since=` | signed in | one device's |
//! | `GET /api/near_misses?limit=&since=` | signed in | every device's newest near misses, oldest first |
//! | `GET /api/devices/{id}/near_misses?limit=&since=` | signed in | one device's |
//! | `PATCH /api/devices/{id}/near_misses/{nm}` `{verdict}` | admin | `{id, device_id, verdict}`; publishes `turn_complete` |
//! | `GET /api/devices/{id}/turns/{turn}/trace` | signed in | the stored decision trace |
//! | `GET /api/devices/{id}/turns/{turn}/afe` | signed in | the turn's AFE series |
//! | `GET /api/devices/{id}/near_misses/{nm}/afe` | signed in | the near miss's AFE series |
//! | `GET /api/devices/{id}/wake_shadow?days=` | signed in | `{days, rules, events}` |
//! | `GET /api/devices/{id}/response_latency` | signed in | `{windows}` |
//! | `GET /api/clip_windows` | signed in | `{pending}` |
//!
//! `limit` is 1 to [`MAX_LIST_LIMIT`] rows, [`DEFAULT_LIST_LIMIT`] when absent; `since` is
//! wall seconds, and a row whose `ts` is at least `since` is listed. The lists return the
//! newest rows, ordered oldest first (dashboard.jsx, `TurnObservability`: "the oldest
//! first"). A turn row carries every key `TurnDetail` and `TurnObservability` read, under
//! the dashboard's names (`turn_id` for the row id, `trigger` for `trigger_type`). JSON
//! columns arrive parsed: `afe_evidence`, `stt_fix` and `wake_windows` (`windows` of a near
//! miss), each `null` when the column is. A near miss's `recording` is the stored word,
//! except that a `saved` recording whose file retention has removed reads `pruned`
//! (dashboard.jsx, `NEAR_MISS_RECORDING`).
//!
//! The decision trace and both AFE series are the stored objects, unchanged (dashboard.jsx,
//! `useTurnTrace` and `useAfeSeries`); 404 means none is kept, which the dashboard shows as
//! "no trace" or "no chart". A row of another device answers 404 too.
//!
//! The dashboard has one live event for activity, `turn_complete`, on which every Activity
//! view and the response-latency panel re-read their lists; new near misses raise none and
//! arrive with the 10 s poll (dashboard.jsx, `FleetActivity`). A verdict change publishes
//! `turn_complete` for its device, so other open dashboards show the mark at once.

use axum::{
    Json, Router,
    extract::{FromRequestParts, Path as PathParams, State},
    http::{StatusCode, request::Parts},
    routing::get,
};
use echomuse_core::types::DeviceId;
use echomuse_db::ReadQuery;
use echomuse_protocol::session::OpenRule;
use echomuse_records::{
    GetNearMiss, GetTurn, ListNearMisses, ListScope, ListTurns, NearMissListing, NearMissRowId,
    RecentShadowEvents, RecordingKind, RecordingsLayout, RecordsError, ResponseLatencies,
    ShadowTotals, ShadowTotalsSince, TurnListing, TurnRowId, VerdictUpdate, recordings::file_safe,
    rollups::rule_key, values::ClipWindows, vocab::NearMissVerdict,
};
use echomuse_settings::values::WakeRule;
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    auth::{AdminUser, AuthenticatedUser},
    body::JsonBody,
    error::{ApiError, ErrorCode},
    events::ApiEvent,
    params::{DevicePath, QueryParams},
    state::ApiState,
};

/// Rows a list returns when the request names no limit (configuration.md, "Activity
/// across every device").
pub const DEFAULT_LIST_LIMIT: u32 = 50;
/// The most rows one list request returns.
pub const MAX_LIST_LIMIT: u32 = 1_000;
/// The shadow table's window when the request names none: the Status tab's seven days.
pub const DEFAULT_SHADOW_DAYS: u32 = 7;
/// The longest shadow window (configuration.md: "1–180 days").
pub const MAX_SHADOW_DAYS: u32 = 180;
/// The newest shadow events the table lists.
pub const SHADOW_EVENTS_SHOWN: u32 = 50;
/// The response-latency windows, in hours: a day, a week and 30 days (dashboard.jsx,
/// `LATENCY_WINDOW_LABEL`).
pub const LATENCY_WINDOWS_HOURS: [u32; 3] = [24, 168, 720];
/// The percentiles each latency window reports.
const PERCENTILES: [(&str, f64); 4] = [("p50", 0.50), ("p90", 0.90), ("p95", 0.95), ("p99", 0.99)];
/// One wake hop: two 80 ms capture blocks, 2,560 samples (device/internal/wakeword/detector,
/// `HopSamples`), so a shadow rule's exposure is its hops × 160 ms (post-AFE architecture
/// §13.3 "Exposure").
const HOP_SECONDS: f64 = 2_560.0 / 16_000.0;
/// A shadow `lead_hist` bucket's lead, in hops: index − 3, clamped to −3..+3.
const LEAD_OFFSET: usize = 3;

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/turns", get(fleet_turns))
        .route("/api/near_misses", get(fleet_near_misses))
        .route("/api/clip_windows", get(clip_windows))
        .route("/api/devices/{id}/turns", get(device_turns))
        .route("/api/devices/{id}/near_misses", get(device_near_misses))
        .route(
            "/api/devices/{id}/near_misses/{row}",
            axum::routing::patch(set_verdict),
        )
        .route("/api/devices/{id}/turns/{row}/trace", get(turn_trace))
        .route("/api/devices/{id}/turns/{row}/afe", get(turn_afe))
        .route(
            "/api/devices/{id}/near_misses/{row}/afe",
            get(near_miss_afe),
        )
        .route("/api/devices/{id}/wake_shadow", get(wake_shadow))
        .route("/api/devices/{id}/response_latency", get(response_latency))
}

// ─── Shared pieces ──────────────────────────────────────────────────────────────

/// A device's `devices` row: whether it exists, and its label.
pub(crate) struct DeviceRow(pub(crate) DeviceId);

/// What [`DeviceRow`] found.
pub(crate) struct KnownDevice {
    pub(crate) label: Option<String>,
}

impl ReadQuery for DeviceRow {
    type Output = Option<KnownDevice>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<Option<KnownDevice>> {
        conn.prepare_cached("SELECT label FROM devices WHERE device_id = ?1")?
            .query_row([&self.0.0], |row| Ok(KnownDevice { label: row.get(0)? }))
            .optional()
    }
}

pub(crate) fn unknown_device() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        ErrorCode::NotFound,
        "No device has this id",
    )
}

/// The device's row, or 404.
pub(crate) async fn known_device(
    state: &ApiState,
    device: &DeviceId,
) -> Result<KnownDevice, ApiError> {
    state
        .db()
        .read(DeviceRow(device.clone()))
        .await?
        .ok_or_else(unknown_device)
}

/// `{id}` and a row id of `/api/devices/{id}/…/{row}…`. A row id that is not a number
/// names no row: 404.
pub(crate) struct RowPath {
    pub(crate) device: DeviceId,
    pub(crate) row: i64,
}

impl<S: Send + Sync> FromRequestParts<S> for RowPath {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let PathParams((device, row)) =
            PathParams::<(String, String)>::from_request_parts(parts, state)
                .await
                .map_err(|_| ApiError::bad_request("The path is not readable"))?;
        let row = row.parse().map_err(|_| ApiError::not_found())?;
        if device.is_empty() {
            return Err(ApiError::not_found());
        }
        Ok(Self {
            device: DeviceId(device),
            row,
        })
    }
}

pub(crate) fn no_such(what: &'static str) -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, what)
}

/// The unparsed text of a JSON column, or `null` when the column is NULL. Text that does
/// not parse is passed on as a string rather than dropped.
fn json_column(text: Option<String>) -> Value {
    match text {
        None => Value::Null,
        Some(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
    }
}

/// A clip-windows column: the stored object when it is in the documented shape, else
/// `null`. The dashboard draws the object without checking it, so a malformed one is not
/// passed on.
fn windows_column(table: &str, id: i64, text: Option<String>) -> Value {
    let Some(text) = text else {
        return Value::Null;
    };
    match ClipWindows::parse(&text).and_then(|_| serde_json::from_str(&text).ok()) {
        Some(value) => value,
        None => {
            eprintln!("api: {table} row {id} holds clip windows that cannot be read");
            Value::Null
        }
    }
}

/// A stored JSON object column served whole; its row must hold an object.
fn stored_object(table: &str, column: &str, id: i64, text: &str) -> Result<Value, ApiError> {
    match serde_json::from_str::<Value>(text) {
        Ok(value @ Value::Object(_)) => Ok(value),
        _ => {
            eprintln!("api: {table}.{column} of row {id} is not a JSON object");
            Err(ApiError::internal(format!(
                "The stored {column} of this row cannot be read"
            )))
        }
    }
}

fn wall_now(state: &ApiState) -> Result<f64, ApiError> {
    state
        .clock()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .map_err(|_| ApiError::internal("The controller's clock is before 1970"))
}

fn records_error(error: RecordsError, doing: &str) -> ApiError {
    match error {
        RecordsError::Closed(_) => {
            ApiError::unavailable("This device's records are closed: it is being deleted")
        }
        error => {
            eprintln!("api: {doing} failed: {error}");
            ApiError::internal(format!("The controller could not {doing}"))
        }
    }
}

// ─── Turn and near-miss lists ───────────────────────────────────────────────────

/// `?limit=&since=` of a list.
#[derive(Deserialize)]
struct ListQuery {
    limit: Option<u32>,
    since: Option<f64>,
}

struct ListBounds {
    limit: u32,
    since: Option<f64>,
}

impl ListQuery {
    fn bounds(self) -> Result<ListBounds, ApiError> {
        let limit = self.limit.unwrap_or(DEFAULT_LIST_LIMIT);
        if !(1..=MAX_LIST_LIMIT).contains(&limit) {
            return Err(ApiError::bad_request(format!(
                "limit must be 1 to {MAX_LIST_LIMIT}"
            )));
        }
        if self.since.is_some_and(|s| !s.is_finite()) {
            return Err(ApiError::bad_request("since must be a number of seconds"));
        }
        Ok(ListBounds {
            limit,
            since: self.since,
        })
    }
}

/// One turn as the dashboard's lists hold it.
fn turn_json(t: TurnListing) -> Value {
    let id = t.id.0;
    json!({
        "turn_id": id,
        "device_id": t.device_id,
        "ts": t.ts,
        "trigger": t.trigger_type,
        "wake_model": t.wake_model,
        "wake_score": t.wake_score,
        "wake_threshold": t.wake_threshold,
        "wake_model_sha256": t.wake_model_sha256,
        "wake_attribution": t.wake_attribution,
        "outcome": t.outcome,
        "asr_text": t.asr_text,
        "reference_coverage": t.reference_coverage,
        "commit_route": t.commit_route,
        "commit_id": t.commit_id,
        "endpoint_class": t.endpoint_class,
        "endpoint_ms": t.endpoint_ms,
        "audio_ms": t.audio_ms,
        "stt_raw": t.stt_raw,
        "stt_text": t.stt_text,
        "stt_ms": t.stt_ms,
        "stt_fix": json_column(t.stt_fix),
        "intent_ms": t.intent_ms,
        "intent_local": t.intent_local,
        "response_type": t.response_type,
        "response_text": t.response_text,
        "conversation_id": t.conversation_id,
        "tts_url_ms": t.tts_url_ms,
        "playback_ms": t.playback_ms,
        "playback_reason": t.playback_reason,
        "first_audio_ms": t.first_audio_ms,
        "response_latency_ms": t.response_latency_ms,
        "total_ms": t.total_ms,
        "policy_hash": t.policy_hash,
        "terminal_reason": t.terminal_reason,
        "turn_uuid": t.turn_uuid,
        "reply_to": t.reply_to,
        "continuation": t.continuation,
        "afe_evidence": json_column(t.afe_evidence),
        "wake_windows": windows_column("turns", id, t.wake_windows),
        "audio_file": t.audio_file,
        "wake_file": t.wake_file,
    })
}

/// The `recording` a near miss shows: `pruned` for a saved file retention removed.
fn shown_recording(layout: &RecordingsLayout, n: &NearMissListing) -> String {
    if n.recording != "saved" {
        return n.recording.clone();
    }
    let device = DeviceId(n.device_id.clone());
    let kept = file_safe(&device)
        && layout
            .path(RecordingKind::NearMiss, &device, n.id.0)
            .is_file();
    if kept {
        n.recording.clone()
    } else {
        "pruned".to_owned()
    }
}

fn near_miss_json(n: NearMissListing, recording: String) -> Value {
    let id = n.id.0;
    json!({
        "id": id,
        "device_id": n.device_id,
        "ts": n.ts,
        "near_miss_uuid": n.near_miss_uuid,
        "profile": n.profile,
        "floor": n.floor,
        "peak": n.peak,
        "wake_threshold": n.wake_threshold,
        "wake_model_sha256": n.wake_model_sha256,
        "duration_ms": n.duration_ms,
        "end_reason": n.end_reason,
        "recording": recording,
        "audio_ms": n.audio_ms,
        "verdict": n.verdict,
        "windows": windows_column("near_misses", id, n.windows),
    })
}

async fn list_turns(
    state: &ApiState,
    scope: ListScope,
    query: ListQuery,
) -> Result<Json<Vec<Value>>, ApiError> {
    let bounds = query.bounds()?;
    let rows = state
        .db()
        .read(ListTurns {
            scope,
            since: bounds.since,
            limit: bounds.limit,
        })
        .await?;
    Ok(Json(rows.into_iter().map(turn_json).collect()))
}

async fn list_near_misses(
    state: &ApiState,
    scope: ListScope,
    query: ListQuery,
) -> Result<Json<Vec<Value>>, ApiError> {
    let bounds = query.bounds()?;
    let rows = state
        .db()
        .read(ListNearMisses {
            scope,
            since: bounds.since,
            limit: bounds.limit,
        })
        .await?;
    // Whether each saved recording is still kept is a file check: off the runtime.
    let layout = state.records().layout().clone();
    let listed = tokio::task::spawn_blocking(move || {
        rows.into_iter()
            .map(|n| {
                let recording = shown_recording(&layout, &n);
                near_miss_json(n, recording)
            })
            .collect()
    })
    .await
    .map_err(|_| ApiError::internal("Reading the near misses failed"))?;
    Ok(Json(listed))
}

async fn fleet_turns(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    QueryParams(query): QueryParams<ListQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    list_turns(&state, ListScope::Fleet, query).await
}

async fn device_turns(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<ListQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    known_device(&state, &device).await?;
    list_turns(&state, ListScope::Device(device), query).await
}

async fn fleet_near_misses(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    QueryParams(query): QueryParams<ListQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    list_near_misses(&state, ListScope::Fleet, query).await
}

async fn device_near_misses(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<ListQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    known_device(&state, &device).await?;
    list_near_misses(&state, ListScope::Device(device), query).await
}

/// `GET /api/clip_windows`: `{pending}`, the saved clips the controller still has to score
/// (dashboard.jsx, `FleetActivity`: "null: unknown"): `null` until the scorer has counted
/// them, or while it cannot run.
async fn clip_windows(State(state): State<ApiState>, _user: AuthenticatedUser) -> Json<Value> {
    Json(json!({ "pending": state.diagnostics().clip_scoring.pending() }))
}

// ─── Verdicts ───────────────────────────────────────────────────────────────────

/// `PATCH …/near_misses/{nm}` with `{verdict}`: `missed_wake`, `correctly_ignored`, or
/// `null` to clear the mark (dashboard.jsx, `TurnObservability`'s `mark`).
async fn set_verdict(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: RowPath,
    JsonBody(body): JsonBody<Map<String, Value>>,
) -> Result<Json<Value>, ApiError> {
    let verdict = match body.get("verdict") {
        Some(Value::Null) => None,
        Some(Value::String(word)) => Some(NearMissVerdict::parse(word).ok_or_else(|| {
            ApiError::bad_request("verdict must be missed_wake, correctly_ignored or null")
        })?),
        _ => {
            return Err(ApiError::bad_request(
                "The body must be {verdict}: missed_wake, correctly_ignored or null",
            ));
        }
    };
    known_device(&state, &path.device).await?;
    let update = state
        .records()
        .set_near_miss_verdict(&path.device, NearMissRowId(path.row), verdict)
        .await
        .map_err(|e| records_error(e, "mark the near miss"))?;
    match update {
        VerdictUpdate::Stored => {}
        VerdictUpdate::NoSuchNearMiss => {
            return Err(no_such("No near miss of this device has this id"));
        }
        VerdictUpdate::NotRecorded => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::Conflict,
                "There is no recording to judge this near miss by",
            ));
        }
    }
    state.events().publisher().publish(&ApiEvent::TurnComplete {
        device_id: path.device.clone(),
    });
    Ok(Json(json!({
        "id": path.row,
        "device_id": path.device,
        "verdict": verdict,
    })))
}

// ─── A turn's trace and the AFE series ──────────────────────────────────────────

async fn turn_of_device(
    state: &ApiState,
    path: &RowPath,
) -> Result<echomuse_records::StoredTurn, ApiError> {
    state
        .db()
        .read(GetTurn(TurnRowId(path.row)))
        .await?
        .filter(|t| t.device_id == path.device.0)
        .ok_or_else(|| no_such("No turn of this device has this id"))
}

async fn turn_trace(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Json<Value>, ApiError> {
    let turn = turn_of_device(&state, &path).await?;
    let text = turn
        .decision_trace
        .ok_or_else(|| no_such("No decision trace is kept for this turn"))?;
    Ok(Json(stored_object(
        "turns",
        "decision_trace",
        path.row,
        &text,
    )?))
}

async fn turn_afe(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Json<Value>, ApiError> {
    let turn = turn_of_device(&state, &path).await?;
    let text = turn
        .afe_series
        .ok_or_else(|| no_such("No AFE series is kept for this turn"))?;
    Ok(Json(stored_object("turns", "afe_series", path.row, &text)?))
}

async fn near_miss_afe(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Json<Value>, ApiError> {
    let near_miss = state
        .db()
        .read(GetNearMiss(NearMissRowId(path.row)))
        .await?
        .filter(|n| n.device_id == path.device.0)
        .ok_or_else(|| no_such("No near miss of this device has this id"))?;
    let text = near_miss
        .afe_series
        .ok_or_else(|| no_such("No AFE series is kept for this near miss"))?;
    Ok(Json(stored_object(
        "near_misses",
        "afe_series",
        path.row,
        &text,
    )?))
}

// ─── Shadow rules ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ShadowQuery {
    days: Option<u32>,
}

/// The one-sided 95% upper bound of a Poisson rate's count, given `k` events: the λ at
/// which seeing `k` or fewer has probability 0.05 (post-AFE architecture §13.3: "with no
/// `unmatched` at all the bound is 2.996/T").
pub fn poisson_upper_95(k: u64) -> f64 {
    let k_f = k as f64;
    // P(X ≤ k; λ), summed in log space so large counts do not underflow.
    let cdf = |lambda: f64| -> f64 {
        let ln_lambda = lambda.ln();
        let mut ln_term = -lambda;
        let mut terms = Vec::with_capacity(k as usize + 1);
        terms.push(ln_term);
        for i in 1..=k {
            ln_term += ln_lambda - (i as f64).ln();
            terms.push(ln_term);
        }
        let top = terms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        top.exp() * terms.iter().map(|t| (t - top).exp()).sum::<f64>()
    };
    let (mut lo, mut hi) = (k_f.max(1e-9), k_f + 10.0 * (k_f + 1.0).sqrt() + 10.0);
    for _ in 0..200 {
        let mid = (lo + hi) / 2.0;
        if cdf(mid) > 0.05 {
            lo = mid;
        } else {
            hi = mid;
        }
        if hi - lo < 1e-9 {
            break;
        }
    }
    (lo + hi) / 2.0
}

fn rule_json(rule: &WakeRule) -> Value {
    json!({
        "profile": rule.profile.as_str(),
        "windows": rule.windows.get(),
        "combine": rule.combine.as_str(),
        "threshold": rule.threshold.get(),
    })
}

/// One row of the shadow table (dashboard.jsx, `WakeShadowTable`; configuration.md "Shadow
/// rules (7 days)"). A rule without a report in the window has measured nothing: `hours` 0
/// and every count `null`.
fn shadow_rule_json(key: String, rule: Option<&WakeRule>, totals: Option<&ShadowTotals>) -> Value {
    let mut row = json!({
        "key": key,
        "rule": rule.map(rule_json),
        "active": rule.is_some(),
    });
    let fields = row.as_object_mut().expect("an object");
    let Some(t) = totals else {
        fields.insert("hours".into(), json!(0.0));
        for name in [
            "hops",
            "opens",
            "matched",
            "matched_earlier",
            "lead_hist",
            "mean_lead_ms",
            "unmatched",
            "unmatched_per_hour",
            "unmatched_per_hour_upper95",
            "retried",
            "live_only",
            "events_dropped",
        ] {
            fields.insert(name.into(), Value::Null);
        }
        return row;
    };
    let hours = t.hops as f64 * HOP_SECONDS / 3_600.0;
    // A positive lead is the shadow rule opening before the live rules: buckets +1..+3,
    // the last holding every lead of three hops or more.
    let earlier: Vec<(u64, u64)> = (LEAD_OFFSET + 1..t.lead_hist.len())
        .map(|i| ((i - LEAD_OFFSET) as u64, t.lead_hist[i]))
        .collect();
    let matched_earlier: u64 = earlier.iter().map(|(_, n)| n).sum();
    let mean_lead_ms = (matched_earlier > 0).then(|| {
        let hops: u64 = earlier.iter().map(|(lead, n)| lead * n).sum();
        hops as f64 / matched_earlier as f64 * HOP_SECONDS * 1_000.0
    });
    let per_hour = |count: f64| (hours > 0.0).then(|| count / hours);
    fields.extend([
        ("hours".into(), json!(hours)),
        ("hops".into(), json!(t.hops)),
        ("opens".into(), json!(t.opens)),
        ("matched".into(), json!(t.matched)),
        ("matched_earlier".into(), json!(matched_earlier)),
        ("lead_hist".into(), json!(t.lead_hist)),
        ("mean_lead_ms".into(), json!(mean_lead_ms)),
        ("unmatched".into(), json!(t.unmatched)),
        (
            "unmatched_per_hour".into(),
            json!(per_hour(t.unmatched as f64)),
        ),
        (
            "unmatched_per_hour_upper95".into(),
            json!(per_hour(poisson_upper_95(t.unmatched))),
        ),
        ("retried".into(), json!(t.retried)),
        ("live_only".into(), json!(t.live_only)),
        ("events_dropped".into(), json!(t.events_dropped)),
    ]);
    row
}

/// `GET …/wake_shadow?days=N`: each shadow rule's counters summed over the last `N` days
/// (the hours from the one `N` days ago on), the device's configured shadow rules first in
/// their order, then rules no longer configured that reported in the window; and the newest
/// [`SHADOW_EVENTS_SHOWN`] shadow events of the window, newest first.
async fn wake_shadow(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
    QueryParams(query): QueryParams<ShadowQuery>,
) -> Result<Json<Value>, ApiError> {
    let days = query.days.unwrap_or(DEFAULT_SHADOW_DAYS);
    if !(1..=MAX_SHADOW_DAYS).contains(&days) {
        return Err(ApiError::bad_request(format!(
            "days must be 1 to {MAX_SHADOW_DAYS}"
        )));
    }
    known_device(&state, &device).await?;
    let since = wall_now(&state)? - f64::from(days) * 86_400.0;
    let since_hour = (since / 3_600.0).floor() as i64 * 3_600;
    let totals = state
        .db()
        .read(ShadowTotalsSince {
            device: device.clone(),
            since_hour,
        })
        .await?;
    let events = state
        .db()
        .read(RecentShadowEvents {
            device: device.clone(),
            since,
            limit: SHADOW_EVENTS_SHOWN,
        })
        .await?;

    let settings = state.settings().effective(&device);
    let mut rules = Vec::new();
    let mut listed = Vec::new();
    for rule in settings.wakeword.wake_shadow_rules.rules() {
        let open = OpenRule::new(
            rule.profile,
            rule.windows.get(),
            rule.combine,
            rule.threshold.get(),
        );
        let key = match open {
            Ok(open) => rule_key(&open),
            Err(error) => {
                eprintln!("api: shadow rule {rule:?} of {device} has no key: {error}");
                continue;
            }
        };
        let found = totals.iter().find(|t| t.rule_key == key);
        rules.push(shadow_rule_json(key.clone(), Some(rule), found));
        listed.push(key);
    }
    for t in &totals {
        if !listed.contains(&t.rule_key) {
            rules.push(shadow_rule_json(t.rule_key.clone(), None, Some(t)));
        }
    }
    let events: Vec<Value> = events
        .into_iter()
        .map(|e| {
            json!({
                "id": e.id,
                "ts": e.ts,
                "rule_key": e.rule_key,
                "kind": e.kind,
                "peak_raw": e.peak_raw,
                "raws": serde_json::from_str::<Value>(&e.raws).unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(Json(
        json!({ "days": days, "rules": rules, "events": events }),
    ))
}

// ─── Response latency ───────────────────────────────────────────────────────────

/// The `p`-quantile of sorted `values`, interpolating linearly between the two closest
/// ranks (the usual definition, R's type 7); `None` without values.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    let last = sorted.len().checked_sub(1)?;
    let h = last as f64 * p;
    let below = h.floor() as usize;
    let above = (below + 1).min(last);
    Some(sorted[below] + (h - below as f64) * (sorted[above] - sorted[below]))
}

/// `GET …/response_latency`: for the last 24 hours, 7 days and 30 days, how many turns
/// measured a response latency and its percentiles in ms; a window without any has `null`
/// percentiles (dashboard.jsx, `ResponseLatency`: "a window without any reads —, never
/// zero").
async fn response_latency(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known_device(&state, &device).await?;
    let now = wall_now(&state)?;
    let longest = LATENCY_WINDOWS_HOURS.iter().copied().max().unwrap_or(0);
    let measured = state
        .db()
        .read(ResponseLatencies {
            device,
            since: now - f64::from(longest) * 3_600.0,
        })
        .await?;
    let windows: Vec<Value> = LATENCY_WINDOWS_HOURS
        .iter()
        .map(|&hours| {
            let since = now - f64::from(hours) * 3_600.0;
            let mut values: Vec<f64> = measured
                .iter()
                .filter(|m| m.ts >= since)
                .map(|m| m.ms as f64)
                .collect();
            values.sort_by(f64::total_cmp);
            let mut window = json!({ "hours": hours, "turns": values.len() });
            let fields = window.as_object_mut().expect("an object");
            for (name, p) in PERCENTILES {
                fields.insert(name.into(), json!(percentile(&values, p)));
            }
            window
        })
        .collect();
    Ok(Json(json!({ "windows": windows })))
}

/// The path a turn's or near miss's recording has, when the device id can name files.
pub(crate) fn recording_path(
    layout: &RecordingsLayout,
    kind: RecordingKind,
    device: &DeviceId,
    id: i64,
) -> Option<std::path::PathBuf> {
    file_safe(device).then(|| layout.path(kind, device, id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_poisson_bound_matches_the_documented_values() {
        // §13.3: 2.996/T with none; about 24 idle hours to qualify at 0.2/h with one.
        assert!((poisson_upper_95(0) - 2.995_732).abs() < 1e-5);
        let one = poisson_upper_95(1);
        assert!((one - 4.743_865).abs() < 1e-5, "{one}");
        assert!((one / 0.2 - 23.7).abs() < 0.1);
        // Large counts stay finite and above the count.
        let many = poisson_upper_95(2_000);
        assert!(many > 2_000.0 && many < 2_100.0, "{many}");
    }

    #[test]
    fn percentiles_interpolate_between_ranks() {
        assert_eq!(percentile(&[], 0.5), None);
        assert_eq!(percentile(&[1_200.0], 0.99), Some(1_200.0));
        let v = [1_000.0, 2_000.0, 3_000.0, 4_000.0];
        assert_eq!(percentile(&v, 0.5), Some(2_500.0));
        assert_eq!(percentile(&v, 0.9), Some(3_700.0));
    }
}
