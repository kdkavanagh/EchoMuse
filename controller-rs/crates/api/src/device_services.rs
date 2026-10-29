//! The device-service routes (§3.4): Wi-Fi, recording modes and their files, the
//! diagnostic session recorder, the browser shell, alarms, sounds and Kiosk Satellite
//! login. The work is done by the integrator's [`crate::DeviceServices`]; these handlers check
//! the caller, the request and the device's capabilities first, and shape the answers as
//! dashboard.jsx reads them.
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `POST /api/devices/{id}/wifi/scan` | admin | `{networks: [{ssid, signal}]}` |
//! | `POST /api/devices/{id}/wifi` `{ssid, psk}` | admin | 202 `{pending: {ssid}}`; the outcome arrives as the device's `wifi` |
//! | `POST /api/devices/{id}/collect` `{enabled}` | admin | `{collectMode}` |
//! | `POST /api/devices/{id}/ambient` `{enabled}` | admin | `{ambientMode}` |
//! | `GET /api/devices/{id}/samples`, `…/ambient` | admin | the listing |
//! | `GET …/samples/{name}`, `…/ambient/{name}` | admin | the WAV |
//! | `GET …/samples/{name}/afe`, `…/ambient/{name}/afe` | admin | the AFE series; 404 when none |
//! | `DELETE …/samples[/{name}]`, `…/ambient[/{name}]` | admin | `{deleted}` |
//! | `GET /api/devices/{id}/samples.zip` | admin | every sample clip and sidecar |
//! | `GET /api/devices/{id}/session_recorder` | admin | `{status, recordings}` |
//! | `POST /api/devices/{id}/session_recorder` `{minutes, streams?}` | admin | the grant status |
//! | `DELETE /api/devices/{id}/session_recorder` | admin | the grant status after withdrawal |
//! | `GET`/`DELETE /api/devices/{id}/session_recorder/{recording}` | admin | the `.emrec` file / `{deleted}` |
//! | `WS /api/devices/{id}/shell` | admin | `shell_meta`, then the shell |
//! | `GET /api/devices/{id}/alerts` | signed in | `{status, ha}` |
//! | `POST /api/devices/{id}/alarms` `{time, days, name, date}` | admin | `{ok, …}` |
//! | `POST /api/devices/{id}/alarms/cancel` `{name, time, all}` | admin | `{ok, …}` |
//! | `GET /api/sounds` | signed in | `{sounds, default_id, unresolved}` |
//! | `POST /api/sounds/upload` (multipart `sound`, `id`) | admin | `{sound}` |
//! | `DELETE /api/sounds/{id}` | admin | `{deleted}` |
//! | `POST /api/devices/{id}/sounds/preview` `{sound_id}` | admin | `{ok: true}` |
//! | `POST /api/devices/{id}/sounds/stop` | admin | `{ok: true, stopped}` |
//! | `POST /api/devices/{id}/kiosk_login` `{password}` | admin | `{ok: true}` |
//!
//! The session recorder has no dashboard control yet; its routes are for administrators'
//! scripts. A grant is bounded (at most 8 hours) and names what may be kept; withdrawing
//! it discards everything not yet written.

use std::path::PathBuf;

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{
        DefaultBodyLimit, Multipart, State, WebSocketUpgrade,
        ws::{Message, WebSocket, rejection::WebSocketUpgradeRejection},
    },
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use echomuse_core::types::DeviceId;
use echomuse_protocol::Capability;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    auth::{AdminUser, AuthenticatedUser},
    body::JsonBody,
    error::{ApiError, ErrorCode},
    gaps::capability_gaps,
    params::{DevicePath, TextPath},
    runtime::LiveSession,
    services::{
        AlarmCancel, AlarmRequest, ClipName, ClipPart, RecorderGrant, RecorderScope, RecordingMode,
        ServiceError, ShellFrame, WifiCredentials,
    },
    state::ApiState,
    zip_writer::{DosTime, ZipEntry, stream_zip},
};

/// The largest sound upload accepted, with room for the multipart framing.
const SOUND_UPLOAD_LIMIT: usize = 11 * 1024 * 1024;

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/devices/{id}/wifi/scan", post(wifi_scan))
        .route("/api/devices/{id}/wifi", post(wifi_change))
        .route("/api/devices/{id}/collect", post(collect))
        .route(
            "/api/devices/{id}/ambient",
            get(ambient_list).post(ambient).delete(ambient_delete_all),
        )
        .route(
            "/api/devices/{id}/samples",
            get(samples_list).delete(samples_delete_all),
        )
        .route("/api/devices/{id}/samples.zip", get(samples_zip))
        .route(
            "/api/devices/{id}/samples/{name}",
            get(sample_audio).delete(sample_delete),
        )
        .route("/api/devices/{id}/samples/{name}/afe", get(sample_afe))
        .route(
            "/api/devices/{id}/ambient/{name}",
            get(ambient_audio).delete(ambient_delete),
        )
        .route("/api/devices/{id}/ambient/{name}/afe", get(ambient_afe))
        .route(
            "/api/devices/{id}/session_recorder",
            get(recorder_status)
                .post(recorder_grant)
                .delete(recorder_withdraw),
        )
        .route(
            "/api/devices/{id}/session_recorder/{recording}",
            get(recorder_file).delete(recorder_delete),
        )
        .route("/api/devices/{id}/shell", get(shell))
        .route("/api/devices/{id}/alerts", get(alerts))
        .route("/api/devices/{id}/alarms", post(set_alarm))
        .route("/api/devices/{id}/alarms/cancel", post(cancel_alarm))
        .route("/api/sounds", get(sounds))
        .route(
            "/api/sounds/upload",
            post(upload_sound).layer(DefaultBodyLimit::max(SOUND_UPLOAD_LIMIT)),
        )
        .route("/api/sounds/{sound}", axum::routing::delete(delete_sound))
        .route("/api/devices/{id}/sounds/preview", post(preview_sound))
        .route("/api/devices/{id}/sounds/stop", post(stop_sound))
        .route("/api/devices/{id}/kiosk_login", post(kiosk_login))
}

// ─── Shared checks ──────────────────────────────────────────────────────────────────

/// The answer for a service's refusal.
pub(crate) fn service_error(error: ServiceError) -> ApiError {
    let message = error.to_string();
    match error {
        ServiceError::UnknownDevice => {
            ApiError::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
        }
        ServiceError::NotConnected => {
            ApiError::new(StatusCode::CONFLICT, ErrorCode::NotConnected, message)
        }
        ServiceError::Unsupported(_) => {
            ApiError::new(StatusCode::CONFLICT, ErrorCode::NotApplicable, message)
        }
        ServiceError::Busy(_) => ApiError::new(StatusCode::CONFLICT, ErrorCode::Busy, message),
        ServiceError::Invalid(_) => ApiError::bad_request(message),
        ServiceError::NotFound(_) => {
            ApiError::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, message)
        }
        ServiceError::InUse(_) => ApiError::new(StatusCode::CONFLICT, ErrorCode::InUse, message),
        ServiceError::Upstream(_) => {
            ApiError::new(StatusCode::BAD_GATEWAY, ErrorCode::Unavailable, message)
        }
        ServiceError::Unknown(_) => {
            ApiError::new(StatusCode::GATEWAY_TIMEOUT, ErrorCode::Unavailable, message)
        }
        ServiceError::Unavailable(_) => ApiError::unavailable(message),
        ServiceError::Failed(reason) => {
            eprintln!("api: a device service failed: {reason}");
            ApiError::internal("The controller could not do that")
        }
    }
}

fn unknown_device() -> ApiError {
    service_error(ServiceError::UnknownDevice)
}

/// The device's live session, when it is connected and announces `capability`. A device
/// that lacks it is refused with the reason the dashboard shows beside the control.
fn require(
    state: &ApiState,
    device: &DeviceId,
    capability: Capability,
) -> Result<LiveSession, ApiError> {
    let live = state.runtimes().live(device).ok_or_else(unknown_device)?;
    let session = live
        .session
        .ok_or_else(|| service_error(ServiceError::NotConnected))?;
    if !session.capabilities.contains(capability) {
        let reason = capability_gaps(session.platform, &session.capabilities)
            .get(&capability)
            .map(|gap| gap.text().to_owned())
            .unwrap_or_else(|| format!("the device does not announce {}", capability.as_str()));
        return Err(service_error(ServiceError::Unsupported(reason)));
    }
    Ok(session)
}

/// The device has a runtime (it is approved).
fn known(state: &ApiState, device: &DeviceId) -> Result<(), ApiError> {
    state
        .runtimes()
        .live(device)
        .map(drop)
        .ok_or_else(unknown_device)
}

async fn send_file(path: PathBuf, content_type: &'static str) -> Result<Response, ApiError> {
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found());
        }
        Err(error) => {
            eprintln!("api: could not open {}: {error}", path.display());
            return Err(ApiError::internal("The file could not be read"));
        }
    };
    let length = file.metadata().await.ok().map(|m| m.len());
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut response = (
        [(header::CONTENT_TYPE, HeaderValue::from_static(content_type))],
        body,
    )
        .into_response();
    if let Some(length) = length {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    Ok(response)
}

// ─── Wi-Fi ──────────────────────────────────────────────────────────────────────────

async fn wifi_scan(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    require(&state, &device, Capability::WifiControl)?;
    let networks = state
        .services()
        .wifi
        .scan(device)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "networks": networks })))
}

#[derive(Deserialize)]
struct WifiBody {
    ssid: String,
    #[serde(default)]
    psk: String,
}

async fn wifi_change(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<WifiBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let credentials = WifiCredentials::new(&body.ssid, &body.psk).map_err(ApiError::bad_request)?;
    require(&state, &device, Capability::WifiControl)?;
    let ssid = credentials.ssid().to_owned();
    state
        .services()
        .wifi
        .change(device, credentials)
        .await
        .map_err(service_error)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "pending": { "ssid": ssid } })),
    ))
}

// ─── Recording modes ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

async fn set_mode(
    state: &ApiState,
    device: DeviceId,
    mode: RecordingMode,
    enabled: bool,
) -> Result<(), ApiError> {
    known(state, &device)?;
    state
        .services()
        .recording
        .set_mode(device, mode, enabled)
        .await
        .map_err(service_error)
}

async fn collect(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    set_mode(&state, device, RecordingMode::Samples, body.enabled).await?;
    Ok(Json(json!({ "collectMode": body.enabled })))
}

async fn ambient(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    set_mode(&state, device, RecordingMode::Ambient, body.enabled).await?;
    Ok(Json(json!({ "ambientMode": body.enabled })))
}

async fn samples_list(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let listing = state
        .services()
        .recording
        .samples(device)
        .await
        .map_err(service_error)?;
    Ok(Json(serde_json::to_value(listing).unwrap_or(Value::Null)))
}

async fn ambient_list(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let listing = state
        .services()
        .recording
        .ambient(device)
        .await
        .map_err(service_error)?;
    Ok(Json(serde_json::to_value(listing).unwrap_or(Value::Null)))
}

fn clip_name(text: &str) -> Result<ClipName, ApiError> {
    ClipName::new(text).ok_or_else(ApiError::not_found)
}

async fn clip(
    state: &ApiState,
    device: DeviceId,
    mode: RecordingMode,
    name: &str,
    part: ClipPart,
) -> Result<Response, ApiError> {
    known(state, &device)?;
    let name = clip_name(name)?;
    let path = state
        .services()
        .recording
        .file(device, mode, name, part)
        .await
        .map_err(service_error)?;
    let content_type = match part {
        ClipPart::Audio => "audio/wav",
        ClipPart::Afe => "application/json",
    };
    send_file(path, content_type).await
}

async fn delete_clips(
    state: &ApiState,
    device: DeviceId,
    mode: RecordingMode,
    name: Option<&str>,
) -> Result<Json<Value>, ApiError> {
    known(state, &device)?;
    let name = name.map(clip_name).transpose()?;
    let deleted = state
        .services()
        .recording
        .delete(device, mode, name)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "deleted": deleted })))
}

/// `/api/devices/{id}/{mode}/{name}`'s two segments.
#[derive(Deserialize)]
struct ClipPath {
    id: String,
    name: String,
}

fn clip_path(path: axum::extract::Path<ClipPath>) -> (DeviceId, String) {
    (DeviceId(path.0.id), path.0.name)
}

async fn sample_audio(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Response, ApiError> {
    let (device, name) = clip_path(path);
    clip(
        &state,
        device,
        RecordingMode::Samples,
        &name,
        ClipPart::Audio,
    )
    .await
}

async fn sample_afe(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Response, ApiError> {
    let (device, name) = clip_path(path);
    clip(&state, device, RecordingMode::Samples, &name, ClipPart::Afe).await
}

async fn ambient_audio(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Response, ApiError> {
    let (device, name) = clip_path(path);
    clip(
        &state,
        device,
        RecordingMode::Ambient,
        &name,
        ClipPart::Audio,
    )
    .await
}

async fn ambient_afe(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Response, ApiError> {
    let (device, name) = clip_path(path);
    clip(&state, device, RecordingMode::Ambient, &name, ClipPart::Afe).await
}

async fn sample_delete(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Json<Value>, ApiError> {
    let (device, name) = clip_path(path);
    delete_clips(&state, device, RecordingMode::Samples, Some(&name)).await
}

async fn ambient_delete(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    path: axum::extract::Path<ClipPath>,
) -> Result<Json<Value>, ApiError> {
    let (device, name) = clip_path(path);
    delete_clips(&state, device, RecordingMode::Ambient, Some(&name)).await
}

async fn samples_delete_all(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    delete_clips(&state, device, RecordingMode::Samples, None).await
}

async fn ambient_delete_all(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    delete_clips(&state, device, RecordingMode::Ambient, None).await
}

async fn samples_zip(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Response, ApiError> {
    known(&state, &device)?;
    let files = state
        .services()
        .recording
        .archive_files(device.clone(), RecordingMode::Samples)
        .await
        .map_err(service_error)?;
    let entries = files
        .into_iter()
        .map(|file| ZipEntry {
            name: file.name,
            path: file.path,
            time: DosTime::from_unix(file.unix_secs),
        })
        .collect();
    let disposition = format!("attachment; filename=\"{}-samples.zip\"", device.0);
    let mut response = (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/zip"),
        )],
        stream_zip(entries),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

// ─── Session recorder ───────────────────────────────────────────────────────────────

/// `streams` of a grant request: what may be kept. Absent keys are not kept.
#[derive(Deserialize, Default)]
#[serde(default)]
struct ScopeBody {
    mic: bool,
    reference: bool,
    cells: bool,
    afe: bool,
    transcripts: bool,
}

#[derive(Deserialize)]
struct GrantBody {
    minutes: u64,
    /// What may be kept; absent keeps every stream and transcripts.
    streams: Option<ScopeBody>,
}

async fn recorder_status(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    state
        .services()
        .recorder
        .status(device)
        .await
        .map(Json)
        .map_err(service_error)
}

async fn recorder_grant(
    State(state): State<ApiState>,
    AdminUser(user): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<GrantBody>,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let scope = match body.streams {
        Some(s) => RecorderScope {
            mic: s.mic,
            reference: s.reference,
            cells: s.cells,
            afe: s.afe,
            transcripts: s.transcripts,
        },
        None => RecorderScope {
            mic: true,
            reference: true,
            cells: true,
            afe: true,
            transcripts: true,
        },
    };
    let grant = RecorderGrant {
        administrator: user.username().to_owned(),
        minutes: body.minutes,
        scope,
    };
    state
        .services()
        .recorder
        .grant(device, grant)
        .await
        .map(Json)
        .map_err(service_error)
}

async fn recorder_withdraw(
    State(state): State<ApiState>,
    AdminUser(user): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    state
        .services()
        .recorder
        .withdraw(device, user.username().to_owned())
        .await
        .map(Json)
        .map_err(service_error)
}

#[derive(Deserialize)]
struct RecordingPath {
    id: String,
    recording: String,
}

async fn recorder_file(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    axum::extract::Path(path): axum::extract::Path<RecordingPath>,
) -> Result<Response, ApiError> {
    let device = DeviceId(path.id);
    known(&state, &device)?;
    let file = state
        .services()
        .recorder
        .file(device, path.recording)
        .await
        .map_err(service_error)?;
    send_file(file, "application/octet-stream").await
}

async fn recorder_delete(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    axum::extract::Path(path): axum::extract::Path<RecordingPath>,
) -> Result<Json<Value>, ApiError> {
    let device = DeviceId(path.id);
    known(&state, &device)?;
    state
        .services()
        .recorder
        .delete(device, path.recording.clone())
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "deleted": path.recording })))
}

// ─── Shell ──────────────────────────────────────────────────────────────────────────

async fn shell(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Result<Response, ApiError> {
    let upgrade = upgrade.map_err(|rejection| {
        ApiError::new(
            rejection.status(),
            ErrorCode::BadRequest,
            "This path takes a WebSocket",
        )
    })?;
    known(&state, &device)?;
    // The shell opens before the upgrade, so a refusal answers with its reason.
    let channels = state
        .services()
        .shell
        .open(device)
        .await
        .map_err(service_error)?;
    Ok(upgrade.on_upgrade(move |socket| bridge_shell(socket, channels)))
}

/// The dashboard's framing: `0x00` + input, or `0x01` + columns and rows (big-endian
/// u16) in PTY mode; raw text in pipe mode.
fn shell_frame(pty: bool, message: Message) -> Option<ShellFrame> {
    match message {
        Message::Binary(bytes) if pty => match bytes.first() {
            Some(0x00) => Some(ShellFrame::Data(bytes.slice(1..))),
            Some(0x01) if bytes.len() >= 5 => Some(ShellFrame::Resize {
                cols: u16::from_be_bytes([bytes[1], bytes[2]]),
                rows: u16::from_be_bytes([bytes[3], bytes[4]]),
            }),
            _ => None,
        },
        Message::Binary(bytes) => Some(ShellFrame::Data(bytes)),
        Message::Text(text) => Some(ShellFrame::Data(Bytes::copy_from_slice(
            text.as_str().as_bytes(),
        ))),
        _ => None,
    }
}

async fn bridge_shell(socket: WebSocket, channels: crate::services::ShellChannels) {
    let crate::services::ShellChannels {
        pty,
        input,
        mut output,
    } = channels;
    let (mut sink, mut stream) = socket.split();
    let meta = json!({ "type": "shell_meta", "pty": pty }).to_string();
    if sink.send(Message::Text(meta.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(message)) => {
                    if let Some(frame) = shell_frame(pty, message)
                        && input.send(frame).await.is_err()
                    {
                        break;
                    }
                }
            },
            out = output.recv() => match out {
                Some(bytes) => {
                    if sink.send(Message::Binary(bytes)).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    drop(input);
    let _ = sink.send(Message::Close(None)).await;
}

// ─── Alarms ─────────────────────────────────────────────────────────────────────────

async fn alerts(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let status = state
        .services()
        .alerts
        .status(device)
        .await
        .map_err(service_error)?;
    let ha = state.ha_status().features;
    let mut body = serde_json::Map::new();
    if let Some(status) = status {
        body.insert("status".into(), status);
    }
    body.insert("ha".into(), json!(ha));
    Ok(Json(Value::Object(body)))
}

#[derive(Deserialize)]
struct AlarmBody {
    time: String,
    #[serde(default)]
    days: Vec<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    date: Option<String>,
}

async fn set_alarm(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<AlarmBody>,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let reply = state
        .services()
        .alerts
        .set_alarm(
            device,
            AlarmRequest {
                time: body.time,
                days: body.days,
                name: body.name,
                date: body.date.filter(|d| !d.is_empty()),
            },
        )
        .await
        .map_err(service_error)?;
    Ok(Json(serde_json::to_value(reply).unwrap_or(Value::Null)))
}

#[derive(Deserialize)]
struct CancelBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    time: String,
    #[serde(default)]
    all: bool,
}

async fn cancel_alarm(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<CancelBody>,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let reply = state
        .services()
        .alerts
        .cancel_alarm(
            device,
            AlarmCancel {
                name: body.name,
                time: body.time,
                all: body.all,
            },
        )
        .await
        .map_err(service_error)?;
    Ok(Json(serde_json::to_value(reply).unwrap_or(Value::Null)))
}

// ─── Sounds ─────────────────────────────────────────────────────────────────────────

async fn sounds(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
) -> Result<Json<Value>, ApiError> {
    state
        .services()
        .sounds
        .catalog()
        .await
        .map(Json)
        .map_err(service_error)
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::TooLarge,
            "Sounds may be at most 10 MiB",
        )
    } else {
        ApiError::bad_request("The upload could not be read")
    }
}

async fn upload_sound(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Json<Value>, ApiError> {
    let mut multipart =
        multipart.map_err(|_| ApiError::bad_request("The upload must be multipart form data"))?;
    let mut id = None;
    let mut file = None;
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        match field.name().unwrap_or_default() {
            "id" => id = Some(field.text().await.map_err(multipart_error)?),
            "sound" => {
                let name = field.file_name().unwrap_or_default().to_owned();
                let bytes = field.bytes().await.map_err(multipart_error)?;
                file = Some((name, bytes.to_vec()));
            }
            _ => {}
        }
    }
    let (file_name, bytes) = file.ok_or_else(|| ApiError::bad_request("No sound file was sent"))?;
    let id = id.ok_or_else(|| ApiError::bad_request("No sound id was sent"))?;
    let sound = state
        .services()
        .sounds
        .upload(id, file_name, bytes)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "sound": sound })))
}

async fn delete_sound(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    TextPath(sound): TextPath,
) -> Result<Json<Value>, ApiError> {
    state
        .services()
        .sounds
        .delete(sound.clone())
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "deleted": sound })))
}

#[derive(Deserialize)]
struct PreviewBody {
    sound_id: String,
}

async fn preview_sound(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<PreviewBody>,
) -> Result<Json<Value>, ApiError> {
    require(&state, &device, Capability::RenderProgress)?;
    state
        .services()
        .sounds
        .preview(device, body.sound_id)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "ok": true })))
}

async fn stop_sound(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known(&state, &device)?;
    let stopped = state
        .services()
        .sounds
        .stop_preview(device)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "ok": true, "stopped": stopped })))
}

// ─── Kiosk Satellite ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct KioskBody {
    password: String,
}

/// Accepted while the device lacks `kiosk_plugin` only because its firmware predates it,
/// so the token is stored before the update that brings the plugin
/// (echo-show-companion-plan.md §11).
async fn kiosk_login(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    DevicePath(device): DevicePath,
    JsonBody(body): JsonBody<KioskBody>,
) -> Result<Json<Value>, ApiError> {
    if body.password.is_empty() {
        return Err(ApiError::bad_request("Enter the Kiosk Satellite password"));
    }
    match require(&state, &device, Capability::KioskPlugin) {
        Ok(_) => {}
        Err(refusal) => {
            let live = state.runtimes().live(&device).and_then(|l| l.session);
            let update_pending = live.is_some_and(|session| {
                capability_gaps(session.platform, &session.capabilities)
                    .get(&Capability::KioskPlugin)
                    == Some(&crate::gaps::GapReason::FirmwareUpdate)
            });
            if !update_pending {
                return Err(refusal);
            }
        }
    }
    state
        .services()
        .kiosk
        .login(device, body.password)
        .await
        .map_err(service_error)?;
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_frames_follow_the_dashboard() {
        let data = shell_frame(true, Message::Binary(Bytes::from_static(b"\x00ls\r")));
        assert_eq!(data, Some(ShellFrame::Data(Bytes::from_static(b"ls\r"))));
        let resize = shell_frame(
            true,
            Message::Binary(Bytes::from_static(&[1, 0, 80, 0, 24])),
        );
        assert_eq!(resize, Some(ShellFrame::Resize { cols: 80, rows: 24 }));
        assert_eq!(
            shell_frame(true, Message::Binary(Bytes::from_static(&[1, 0]))),
            None
        );
        let pipe = shell_frame(false, Message::Text("ls\n".into()));
        assert_eq!(pipe, Some(ShellFrame::Data(Bytes::from_static(b"ls\n"))));
    }
}
