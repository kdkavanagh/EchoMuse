//! The dashboard's pages and assets.
//!
//! The static directory holds the compiled dashboard: `index.html` (the landing page:
//! sign-in and first-run setup), `dashboard.html`, and the files they load by relative
//! URL under `static/` (`dashboard.js`, `favicon.svg`, `vendor/…`). They are served as:
//!
//! - `GET /` → `index.html`;
//! - `GET /dashboard` → `dashboard.html` (index.html navigates to `dashboard`);
//! - `GET /static/{path}` → that file under the directory, typed by its extension.
//!
//! The two pages get a `<base href>` under ingress ([`crate::ingress`]). Asset paths
//! are resolved by `tower_http`'s `ServeDir`, which refuses `..` and absolute
//! components, so nothing outside the directory is reachable.

use std::path::Path;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::get,
};
use tower_http::services::ServeDir;

use crate::{
    error::ApiError,
    ingress::{base_href, with_base},
    state::ApiState,
};

pub(crate) fn routes(static_dir: &Path) -> Router<ApiState> {
    let assets = ServeDir::new(static_dir).append_index_html_on_directories(false);
    Router::new()
        .route("/", get(index))
        .route("/dashboard", get(dashboard))
        .nest_service("/static", assets)
}

async fn index(State(state): State<ApiState>, headers: HeaderMap) -> Result<Response, ApiError> {
    page(&state, &headers, "index.html").await
}

async fn dashboard(
    State(state): State<ApiState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    page(&state, &headers, "dashboard.html").await
}

async fn page(state: &ApiState, headers: &HeaderMap, name: &str) -> Result<Response, ApiError> {
    let base = base_href(state.ingress(), headers)?;
    let path = state.static_dir().join(name);
    let html = match tokio::fs::read_to_string(&path).await {
        Ok(html) => html,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!("the dashboard page {} is missing", path.display());
            return Err(ApiError::not_found());
        }
        Err(error) => {
            tracing::error!(%error, "cannot read {}", path.display());
            return Err(ApiError::internal("The dashboard page could not be read"));
        }
    };
    let html = match base {
        Some(href) => with_base(&html, &href)
            .ok_or_else(|| ApiError::internal("The dashboard page has no <head>"))?,
        None => html,
    };
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            // The base differs per ingress session, so a cached copy is checked first.
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        html,
    )
        .into_response())
}
