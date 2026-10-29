//! The wake-model registry routes (§4.11; configuration.md "Wake model").
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/wake_models` | signed in | `{active, models}` |
//! | `POST /api/wake_models/upload` (multipart) | admin | `{model, added}` |
//! | `DELETE /api/wake_models/{graph_sha256}` | admin | `{deleted}`; refused while selected |
//!
//! An upload carries the form dashboard.jsx's `WakeModelPicker` sends: the files `graph`
//! and `sidecar`, the thresholds `idle`, `playback` and `reference`, and `wake_phrase`
//! and `verify_core`. Its sizes, sidecar, thresholds and spoken forms are checked here;
//! then the caller-supplied [`ModelQualifier`] runs the model (in a child process,
//! §5.5) and returns its probe scores, and the settings service adds it to the registry,
//! which refuses a probe score at or above the reference threshold. A graph already
//! registered is returned unchanged without running it again. A new model is not
//! selected anywhere.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State, multipart::Field},
    http::StatusCode,
    routing::{delete, get, post},
};
use echomuse_protocol::Sha256;
use echomuse_settings::{
    registry::{
        AddOutcome, MAX_GRAPH_BYTES, MAX_SIDECAR_BYTES, ModelThresholds, NewModel, ProbeResult,
        RegistryError, Sidecar, SpokenForm, WakeModel,
    },
    service::{ModelError, Selector, SettingsSnapshot},
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    auth::{AdminUser, AuthenticatedUser},
    error::{ApiError, ErrorCode},
    params::TextPath,
    state::ApiState,
};

/// Room for the form's text fields and multipart framing beside the two files.
const FORM_OVERHEAD_BYTES: usize = 64 * 1024;
/// The longest text field accepted.
const MAX_TEXT_FIELD_BYTES: usize = 1024;

/// A model to qualify: the uploaded graph and sidecar, already checked for size and
/// sidecar validity, with their hashes.
#[derive(Clone)]
pub struct ModelCandidate {
    /// The ONNX graph.
    pub graph: Arc<[u8]>,
    /// The sidecar.
    pub sidecar: Arc<[u8]>,
    /// The graph's SHA-256.
    pub graph_sha256: Sha256,
    /// The sidecar's SHA-256.
    pub sidecar_sha256: Sha256,
}

impl fmt::Debug for ModelCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelCandidate")
            .field("graph_sha256", &self.graph_sha256)
            .field("graph_bytes", &self.graph.len())
            .field("sidecar_sha256", &self.sidecar_sha256)
            .finish()
    }
}

/// Why qualification refused a model, in a sentence the dashboard shows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelRefused(pub String);

impl fmt::Display for ModelRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ModelRefused {}

/// Runs an uploaded model before the controller accepts it. The integrator implements
/// it with the speech crate's child-process qualification: check the graph's inputs
/// and outputs against the sidecar, and score silence, noise and a tone.
pub trait ModelQualifier: Send + Sync + 'static {
    /// The probe scores, or why the model is refused. A crash, a hang or an unreadable
    /// report of the qualification child is a refusal too.
    fn qualify(
        &self,
        candidate: ModelCandidate,
    ) -> impl Future<Output = Result<ProbeResult, ModelRefused>> + Send;
}

type QualifyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProbeResult, ModelRefused>> + Send + 'a>>;

/// [`ModelQualifier`] behind a pointer.
pub(crate) trait Qualifier: Send + Sync + 'static {
    fn qualify(&self, candidate: ModelCandidate) -> QualifyFuture<'_>;
}

impl<Q: ModelQualifier> Qualifier for Q {
    fn qualify(&self, candidate: ModelCandidate) -> QualifyFuture<'_> {
        Box::pin(ModelQualifier::qualify(self, candidate))
    }
}

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/wake_models", get(list))
        .route(
            "/api/wake_models/upload",
            post(upload).layer(DefaultBodyLimit::max(
                MAX_GRAPH_BYTES + MAX_SIDECAR_BYTES + FORM_OVERHEAD_BYTES,
            )),
        )
        .route("/api/wake_models/{graph_sha256}", delete(remove))
}

// ─── Listing ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
enum Selection {
    Fleet,
    Device { device_id: String },
}

/// One model as the picker reads it: its identity, spoken forms, thresholds and probe,
/// whether the fleet selects it (`active`) and everywhere it is selected (`in_use_by`).
pub(crate) fn model_json(model: &WakeModel, snapshot: &SettingsSnapshot) -> Value {
    let thresholds = model.thresholds();
    let in_use_by: Vec<Selection> = snapshot
        .selections(model.graph_sha256())
        .into_iter()
        .map(|selector| match selector {
            Selector::Fleet => Selection::Fleet,
            Selector::Device(device) => Selection::Device {
                device_id: device.0,
            },
        })
        .collect();
    json!({
        "graph_sha256": model.graph_sha256().as_str(),
        "sidecar_sha256": model.sidecar_sha256().as_str(),
        "wake_phrase": model.wake_phrase().as_str(),
        "verify_core": model.verify_core().as_str(),
        "thresholds": {
            "idle": thresholds.idle(),
            "playback": thresholds.playback(),
            "reference": thresholds.reference(),
        },
        "probe": model.probe().map(|p| json!({
            "silence": p.silence(),
            "noise": p.noise(),
            "tone": p.tone(),
        })),
        "active": snapshot.fleet().wakeword.wake_model.0 == *model.graph_sha256(),
        "in_use_by": in_use_by,
    })
}

/// The models a device can select (both files verified), in registry order, and the
/// fleet's selection.
async fn list(State(state): State<ApiState>, _user: AuthenticatedUser) -> Json<Value> {
    let snapshot = state.settings().snapshot();
    let models: Vec<Value> = snapshot
        .registry()
        .models()
        .filter(|model| model.is_usable())
        .map(|model| model_json(model, &snapshot))
        .collect();
    Json(json!({
        "active": snapshot.fleet().wakeword.wake_model.0.as_str(),
        "models": models,
    }))
}

// ─── Upload ─────────────────────────────────────────────────────────────────────

fn rejected(message: impl Into<std::borrow::Cow<'static, str>>) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::ModelRejected,
        message,
    )
}

fn too_large(what: &str, max: usize) -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        ErrorCode::TooLarge,
        format!("The {what} must be at most {max} bytes"),
    )
}

/// The form's fields as sent.
#[derive(Default)]
struct UploadForm {
    graph: Option<Vec<u8>>,
    sidecar: Option<Vec<u8>>,
    idle: Option<String>,
    playback: Option<String>,
    reference: Option<String>,
    wake_phrase: Option<String>,
    verify_core: Option<String>,
}

/// Read one field, refusing it once it passes `max` bytes.
async fn field_bytes(mut field: Field<'_>, what: &str, max: usize) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if bytes.len() + chunk.len() > max {
            return Err(too_large(what, max));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn multipart_error(error: axum::extract::multipart::MultipartError) -> ApiError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large(
            "upload",
            MAX_GRAPH_BYTES + MAX_SIDECAR_BYTES + FORM_OVERHEAD_BYTES,
        )
    } else {
        ApiError::bad_request("The upload is not a readable multipart form")
    }
}

async fn read_form(mut multipart: Multipart) -> Result<UploadForm, ApiError> {
    let mut form = UploadForm::default();
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_owned();
        let (slot, max) = match name.as_str() {
            "graph" => {
                let bytes = field_bytes(field, "graph", MAX_GRAPH_BYTES).await?;
                set_once(&mut form.graph, bytes, &name)?;
                continue;
            }
            "sidecar" => {
                let bytes = field_bytes(field, "sidecar", MAX_SIDECAR_BYTES).await?;
                set_once(&mut form.sidecar, bytes, &name)?;
                continue;
            }
            "idle" => (&mut form.idle, MAX_TEXT_FIELD_BYTES),
            "playback" => (&mut form.playback, MAX_TEXT_FIELD_BYTES),
            "reference" => (&mut form.reference, MAX_TEXT_FIELD_BYTES),
            "wake_phrase" => (&mut form.wake_phrase, MAX_TEXT_FIELD_BYTES),
            "verify_core" => (&mut form.verify_core, MAX_TEXT_FIELD_BYTES),
            _ => {
                return Err(ApiError::bad_request(format!(
                    "The upload has an unknown field {name:?}"
                )));
            }
        };
        let bytes = field_bytes(field, &name, max).await?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ApiError::bad_request(format!("{name} is not text")))?;
        set_once(slot, text, &name)?;
    }
    Ok(form)
}

fn set_once<T>(slot: &mut Option<T>, value: T, name: &str) -> Result<(), ApiError> {
    if slot.replace(value).is_some() {
        return Err(ApiError::bad_request(format!("The upload repeats {name}")));
    }
    Ok(())
}

fn required<T>(value: Option<T>, name: &str) -> Result<T, ApiError> {
    value.ok_or_else(|| ApiError::bad_request(format!("The upload is missing {name}")))
}

fn threshold(text: Option<String>, name: &str) -> Result<f64, ApiError> {
    required(text, name)?
        .trim()
        .parse::<f64>()
        .map_err(|_| rejected(format!("{name} is not a number")))
}

/// A checked upload, ready to qualify.
struct Upload {
    candidate: ModelCandidate,
    thresholds: ModelThresholds,
    wake_phrase: SpokenForm,
    verify_core: SpokenForm,
}

fn check(form: UploadForm) -> Result<Upload, ApiError> {
    let graph = required(form.graph, "graph")?;
    let sidecar = required(form.sidecar, "sidecar")?;
    if graph.is_empty() {
        return Err(rejected("The graph is empty"));
    }
    Sidecar::parse(&sidecar).map_err(|e| rejected(format!("sidecar: {e}")))?;
    let thresholds = ModelThresholds::new(
        threshold(form.idle, "idle")?,
        threshold(form.playback, "playback")?,
        threshold(form.reference, "reference")?,
    )
    .map_err(|e| rejected(format!("thresholds: {e}")))?;
    let spoken = |text: Option<String>, name: &str| {
        let text = required(text, name)?;
        SpokenForm::new(&text).map_err(|_| {
            rejected(format!(
                "{name} must be 1 to {} lowercase letters",
                SpokenForm::MAX_CHARS
            ))
        })
    };
    Ok(Upload {
        candidate: ModelCandidate {
            graph_sha256: Sha256::of(&graph),
            sidecar_sha256: Sha256::of(&sidecar),
            graph: graph.into(),
            sidecar: sidecar.into(),
        },
        thresholds,
        wake_phrase: spoken(form.wake_phrase, "wake_phrase")?,
        verify_core: spoken(form.verify_core, "verify_core")?,
    })
}

fn model_error(error: ModelError) -> ApiError {
    match error {
        ModelError::Registry(
            error @ (RegistryError::Size { .. }
            | RegistryError::Sidecar(_)
            | RegistryError::ProbeTooHigh { .. }),
        ) => rejected(error.to_string()),
        ModelError::Registry(RegistryError::UnknownModel(_)) => ApiError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "No registry model has this graph",
        ),
        ModelError::InUse(selectors) => {
            let names: Vec<String> = selectors
                .iter()
                .map(|selector| match selector {
                    Selector::Fleet => "the fleet".to_owned(),
                    Selector::Device(device) => format!("device {device}"),
                })
                .collect();
            ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::InUse,
                format!("The model is still selected by {}", names.join(", ")),
            )
        }
        ModelError::Closed => ApiError::unavailable("The settings service has stopped"),
        error @ (ModelError::Registry(_) | ModelError::Interrupted) => {
            eprintln!("api: a wake-model registry change failed: {error}");
            ApiError::internal("The wake-model registry could not be changed")
        }
    }
}

async fn upload(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    multipart: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> Result<Json<Value>, ApiError> {
    let multipart = multipart.map_err(|_| {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorCode::UnsupportedMediaType,
            "The upload must be a multipart form (Content-Type: multipart/form-data)",
        )
    })?;
    let upload = check(read_form(multipart).await?)?;
    let snapshot = state.settings().snapshot();
    if let Some(existing) = snapshot.registry().model(&upload.candidate.graph_sha256) {
        return Ok(Json(json!({
            "model": model_json(existing, &snapshot),
            "added": false,
        })));
    }
    let probe = state
        .qualifier()
        .qualify(upload.candidate.clone())
        .await
        .map_err(|refused| rejected(format!("The model failed validation: {refused}")))?;
    let outcome = state
        .settings()
        .add_wake_model(NewModel {
            graph: upload.candidate.graph.to_vec(),
            sidecar: upload.candidate.sidecar.to_vec(),
            thresholds: upload.thresholds,
            wake_phrase: upload.wake_phrase,
            verify_core: upload.verify_core,
            probe,
        })
        .await
        .map_err(model_error)?;
    let (model, added) = match outcome {
        AddOutcome::Added(model) => (model, true),
        AddOutcome::AlreadyRegistered(model) => (model, false),
    };
    let snapshot = state.settings().snapshot();
    Ok(Json(json!({
        "model": model_json(&model, &snapshot),
        "added": added,
    })))
}

// ─── Deletion ───────────────────────────────────────────────────────────────────

async fn remove(
    State(state): State<ApiState>,
    AdminUser(_): AdminUser,
    TextPath(text): TextPath,
) -> Result<Json<Value>, ApiError> {
    let sha = Sha256::parse(&text)
        .ok_or_else(|| ApiError::bad_request("A model is named by its graph's SHA-256"))?;
    state
        .settings()
        .delete_wake_model(sha.clone())
        .await
        .map_err(model_error)?;
    Ok(Json(json!({"deleted": sha.as_str()})))
}
