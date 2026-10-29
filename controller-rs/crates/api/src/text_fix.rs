//! Transcript correction's tools in the settings panel (§4.7; dashboard.jsx,
//! `TextFixTester` and `TextFixPronunciationEditor`).
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `POST /api/text_fix/trial` `{text, wake, device_id?}` | signed in | [`TextFixTrial`] |
//! | `GET /api/text_fix/unknown_words?device_id=` | signed in | `{words: [{word, count}]}` |
//!
//! A trial changes nothing: only Home Assistant's non-executing sentence matcher is asked.
//! `text` is what speech to text heard, 1 to [`MAX_TRIAL_CHARS`] characters; `device_id`
//! picks the speaker whose settings and satellite apply, and without it the fleet's
//! settings and HA's preferred pipeline are used. While the correction tables are being
//! rebuilt (or HA is reconnecting) both routes answer 503 with code `not_ready`, which
//! the dashboard retries.

use axum::{Json, Router, extract::State, http::StatusCode, routing::get, routing::post};
use echomuse_core::types::DeviceId;
use serde::{Deserialize, Serialize};

use crate::{
    activity::known_device,
    auth::AuthenticatedUser,
    body::JsonBody,
    device_services::service_error,
    diagnostics::{TextFixError, TextFixTrial, TextFixTrialRequest, UnknownWord},
    error::{ApiError, ErrorCode},
    params::QueryParams,
    state::ApiState,
};

/// The longest text a trial takes (the dashboard's field allows as much).
pub const MAX_TRIAL_CHARS: usize = 300;

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/text_fix/trial", post(trial))
        .route("/api/text_fix/unknown_words", get(unknown_words))
}

fn text_fix_error(error: TextFixError) -> ApiError {
    match error {
        TextFixError::NotReady(why) => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::NotReady, why)
        }
        TextFixError::Service(error) => service_error(error),
    }
}

#[derive(Deserialize)]
struct TrialBody {
    text: String,
    #[serde(default)]
    wake: bool,
    device_id: Option<String>,
}

async fn device_of(state: &ApiState, id: Option<String>) -> Result<Option<DeviceId>, ApiError> {
    match id.filter(|id| !id.is_empty()) {
        None => Ok(None),
        Some(id) => {
            let device = DeviceId(id);
            known_device(state, &device).await?;
            Ok(Some(device))
        }
    }
}

async fn trial(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    JsonBody(body): JsonBody<TrialBody>,
) -> Result<Json<TextFixTrial>, ApiError> {
    let text = body.text.trim();
    if text.is_empty() {
        return Err(ApiError::bad_request("Type a transcript to try"));
    }
    if text.chars().count() > MAX_TRIAL_CHARS {
        return Err(ApiError::bad_request(format!(
            "A transcript to try is at most {MAX_TRIAL_CHARS} characters"
        )));
    }
    let device = device_of(&state, body.device_id).await?;
    state
        .diagnostics()
        .text_fix
        .trial(TextFixTrialRequest {
            text: text.to_owned(),
            wake: body.wake,
            device,
        })
        .await
        .map(Json)
        .map_err(text_fix_error)
}

#[derive(Deserialize)]
struct WordsQuery {
    device_id: Option<String>,
}

#[derive(Serialize)]
struct Words {
    words: Vec<UnknownWord>,
}

async fn unknown_words(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    QueryParams(query): QueryParams<WordsQuery>,
) -> Result<Json<Words>, ApiError> {
    let device = device_of(&state, query.device_id).await?;
    state
        .diagnostics()
        .text_fix
        .unknown_words(device)
        .await
        .map(|words| Json(Words { words }))
        .map_err(text_fix_error)
}
