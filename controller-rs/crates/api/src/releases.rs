//! `GET /api/releases/controller` (signed in): what is known about a newer controller
//! release.
//!
//! The controller's update channel is its image, which the person running it pulls with
//! their own container tooling (dashboard.jsx: "There is deliberately no update
//! button"); the spec names no release check (§3.4, §3.6), and this controller runs
//! none. So the answer is the running version and no newer release:
//! `{available: false, version: null, current, notes: null, release_url: null,
//! checked: false}`. `checked: false` says that `available` comes from no check, not
//! from one that found nothing. The dashboard shows its update notice only when
//! `available` and `version` are both set, so it shows none. The `controller_update`
//! event is never sent.

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use crate::{auth::AuthenticatedUser, state::ApiState};

pub(crate) fn routes() -> Router<ApiState> {
    Router::new().route("/api/releases/controller", get(controller_release))
}

/// The answer, in the `controller_update` shape plus `checked`.
#[derive(Serialize)]
struct Release {
    available: bool,
    version: Option<String>,
    current: Option<String>,
    notes: Option<String>,
    release_url: Option<String>,
    checked: bool,
}

async fn controller_release(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
) -> Json<Release> {
    Json(Release {
        available: false,
        version: None,
        current: state.controller_version().map(str::to_owned),
        notes: None,
        release_url: None,
        checked: false,
    })
}
