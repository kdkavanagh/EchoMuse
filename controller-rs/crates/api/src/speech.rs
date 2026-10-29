//! The speech assets a device loads, and what a Wyoming speech-to-text server offers
//! (§3.1.4, §4.7).
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/devices/{id}/speech_assets` | signed in | `{device_id, connected, wake_model, error, assets}` |
//! | `GET /api/speech/wyoming?host=&port=` | admin | `{host, port, programs}` |
//!
//! **Speech assets.** Every `session.ready` names three assets the device must load: the
//! ONNX Runtime library (`runtime`), the selected wake graph (`graph`) and its sidecar
//! (`sidecar`); the device fetches any it lacks over the asset socket. Each entry is
//! `{kind, name, sha256, installed}`: `installed` is whether the device's last hello listed
//! that SHA-256 among its installed assets, `null` while the device is not connected or
//! its hello sent no list. `sha256` is `null` only when the controller does not know it;
//! `error` then says why. `wake_model` is `{graph_sha256, wake_phrase}` of the model the
//! device's settings select, or `null` when it is not in the registry. The shipped
//! dashboard does not read this route; the shape is this controller's.
//!
//! **Wyoming.** The dashboard's "Check" beside `pauseAsr` (dashboard.jsx,
//! `PauseAsrControls`) reads `programs[].name`, `.version` and `.models[].name`. It is
//! admin only, because the controller connects to the address given. A server that
//! cannot be reached or does not speak Wyoming answers 502 with the reason.

use std::time::Duration;

use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use echomuse_protocol::Sha256;
use echomuse_wyoming::{AsrProgram, ServerAddress, describe};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    activity::known_device,
    auth::{AdminUser, AuthenticatedUser},
    error::{ApiError, ErrorCode},
    params::{DevicePath, QueryParams},
    state::ApiState,
};

/// How long a Wyoming server may take to describe itself.
pub const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
/// The runtime asset's name on the device.
const RUNTIME_NAME: &str = "libonnxruntime.so";

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/devices/{id}/speech_assets", get(speech_assets))
        .route("/api/speech/wyoming", get(wyoming))
}

#[derive(Serialize)]
struct Asset {
    kind: &'static str,
    name: Option<String>,
    sha256: Option<Sha256>,
    installed: Option<bool>,
}

async fn speech_assets(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Json<Value>, ApiError> {
    known_device(&state, &device).await?;
    let session = state.runtimes().live(&device).and_then(|live| live.session);
    let connected = session.is_some();
    let installed = session.and_then(|session| session.installed_assets);
    let has = |sha: &Option<Sha256>| -> Option<bool> {
        let list = installed.as_ref()?;
        Some(sha.as_ref().is_some_and(|sha| list.contains(sha)))
    };
    let snapshot = state.settings().snapshot();
    let selected = snapshot.selected_model(&device);
    let runtime = state.diagnostics().runtime_asset.clone();
    let (graph, sidecar, wake_model, error) = match &selected {
        Ok(model) => (
            Some((model.graph_file().to_owned(), model.graph_sha256().clone())),
            Some((
                model.sidecar_file().to_owned(),
                model.sidecar_sha256().clone(),
            )),
            json!({
                "graph_sha256": model.graph_sha256(),
                "wake_phrase": model.wake_phrase().as_str(),
            }),
            runtime
                .is_none()
                .then(|| "The runtime asset's SHA-256 is not known".to_owned()),
        ),
        Err(error) => (None, None, Value::Null, Some(error.to_string())),
    };
    let runtime_sha = runtime;
    let graph_sha = graph.as_ref().map(|(_, sha)| sha.clone());
    let sidecar_sha = sidecar.as_ref().map(|(_, sha)| sha.clone());
    let assets = vec![
        Asset {
            kind: "runtime",
            name: Some(RUNTIME_NAME.to_owned()),
            installed: has(&runtime_sha),
            sha256: runtime_sha,
        },
        Asset {
            kind: "graph",
            name: graph.map(|(name, _)| name),
            installed: has(&graph_sha),
            sha256: graph_sha,
        },
        Asset {
            kind: "sidecar",
            name: sidecar.map(|(name, _)| name),
            installed: has(&sidecar_sha),
            sha256: sidecar_sha,
        },
    ];
    Ok(Json(json!({
        "device_id": device,
        "connected": connected,
        "wake_model": wake_model,
        "error": error,
        "assets": assets,
    })))
}

#[derive(Deserialize)]
struct WyomingQuery {
    host: Option<String>,
    port: Option<String>,
}

#[derive(Serialize)]
struct WyomingAnswer {
    host: String,
    port: u16,
    programs: Vec<AsrProgram>,
}

async fn wyoming(
    AdminUser(_): AdminUser,
    QueryParams(query): QueryParams<WyomingQuery>,
) -> Result<Json<WyomingAnswer>, ApiError> {
    let host = query.host.unwrap_or_default();
    let port: u16 = match query.port.as_deref() {
        None | Some("") => echomuse_settings::values::PauseAsr::default().port(),
        Some(text) => text
            .parse()
            .ok()
            .filter(|&port: &u16| port != 0)
            .ok_or_else(|| ApiError::bad_request("port must be 1 to 65535"))?,
    };
    let server = ServerAddress::new(&host, port)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    match describe(&server, DESCRIBE_TIMEOUT).await {
        Ok(info) => Ok(Json(WyomingAnswer {
            host: server.host().to_owned(),
            port,
            programs: info.asr,
        })),
        Err(error) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            ErrorCode::Unavailable,
            format!("{server}: {error}"),
        )),
    }
}
