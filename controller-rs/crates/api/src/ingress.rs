//! Home Assistant add-on ingress (§3.4).
//!
//! With ingress on (`ECHOMUSE_HOME_ASSISTANT_INGRESS=true`), the controller serves only
//! requests that arrive from the Supervisor's ingress proxy, `172.30.32.2`; everything
//! else is refused with 403 before routing. The proxy mounts the dashboard below a
//! generated path and says which in `X-Ingress-Path`; the served pages get that path as
//! their `<base href>` so their relative URLs resolve below it. With ingress off the
//! header is ignored, so nobody can rewrite the page's base from outside.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::{
    error::{ApiError, ErrorCode},
    state::ApiState,
};

/// The Home Assistant Supervisor's ingress proxy address.
pub const SUPERVISOR_INGRESS_PROXY: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 32, 2));

/// The header carrying the path the proxy mounts the dashboard at.
pub const INGRESS_PATH_HEADER: &str = "x-ingress-path";

/// Who may reach the API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ingress {
    /// Any client on the network; `X-Ingress-Path` is ignored.
    Off,
    /// Only `proxy`, which is [`SUPERVISOR_INGRESS_PROXY`] in the add-on.
    Only {
        /// The one address served.
        proxy: IpAddr,
    },
}

impl Ingress {
    /// The add-on's setting: [`Ingress::Only`] the Supervisor's proxy when `ingress_only`
    /// (`ControllerConfig::ingress_only`), else [`Ingress::Off`].
    pub fn from_setting(ingress_only: bool) -> Self {
        if ingress_only {
            Self::Only {
                proxy: SUPERVISOR_INGRESS_PROXY,
            }
        } else {
            Self::Off
        }
    }
}

/// Refuse requests from anyone but the proxy while ingress is on.
pub(crate) async fn admit(State(state): State<ApiState>, request: Request, next: Next) -> Response {
    if let Ingress::Only { proxy } = state.ingress() {
        let peer = request
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| peer.ip().to_canonical());
        if peer != Some(proxy.to_canonical()) {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                ErrorCode::Forbidden,
                "This controller is served only through Home Assistant",
            )
            .into_response();
        }
    }
    next.run(request).await
}

/// The `<base href>` for a served page: the ingress path with a trailing slash, or
/// `None` outside ingress or without the header. A path with characters a Supervisor
/// path never has is refused, which also keeps it from breaking out of the attribute.
pub(crate) fn base_href(ingress: Ingress, headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    if ingress == Ingress::Off {
        return Ok(None);
    }
    let Some(value) = headers.get(INGRESS_PATH_HEADER) else {
        return Ok(None);
    };
    let path = value
        .to_str()
        .ok()
        .filter(|path| {
            path.starts_with('/')
                && path
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-_.~".contains(&b))
        })
        .ok_or_else(|| ApiError::bad_request("X-Ingress-Path is not a plain path"))?;
    Ok(Some(if path.ends_with('/') {
        path.to_owned()
    } else {
        format!("{path}/")
    }))
}

/// `html` with `<base href="{href}">` as the first element of its `<head>`, ahead of
/// every relative URL, or `None` when it has no `<head>`.
pub(crate) fn with_base(html: &str, href: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    let head_end = loop {
        let start = from + lower[from..].find("<head")?;
        let after = start + "<head".len();
        // `<header>` also starts with `<head`.
        match lower.as_bytes().get(after) {
            Some(b'>') => break after + 1,
            Some(b) if b.is_ascii_whitespace() => break after + lower[after..].find('>')? + 1,
            _ => from = after,
        }
    };
    let base = format!("\n  <base href=\"{href}\" />");
    let mut page = String::with_capacity(html.len() + base.len());
    page.push_str(&html[..head_end]);
    page.push_str(&base);
    page.push_str(&html[head_end..]);
    Some(page)
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    #[test]
    fn base_goes_first_in_head() {
        let html = "<html>\n<head>\n  <link href=\"static/a.css\">\n</head><body><header></header></body></html>";
        let page = with_base(html, "/api/hassio_ingress/abc/").unwrap();
        assert_eq!(
            page,
            "<html>\n<head>\n  <base href=\"/api/hassio_ingress/abc/\" />\n  <link href=\"static/a.css\">\n</head><body><header></header></body></html>"
        );
        let attributes = with_base("<HEAD lang=\"en\"><title>", "/x/").unwrap();
        assert_eq!(
            attributes,
            "<HEAD lang=\"en\">\n  <base href=\"/x/\" /><title>"
        );
        assert!(with_base("<header>no head</header>", "/x/").is_none());
    }

    #[test]
    fn ingress_path_rules() {
        let only = Ingress::from_setting(true);
        let mut headers = HeaderMap::new();
        assert_eq!(base_href(only, &headers).unwrap(), None);
        headers.insert(
            INGRESS_PATH_HEADER,
            HeaderValue::from_static("/api/hassio_ingress/Tok-en_1"),
        );
        assert_eq!(
            base_href(only, &headers).unwrap().as_deref(),
            Some("/api/hassio_ingress/Tok-en_1/")
        );
        assert_eq!(base_href(Ingress::Off, &headers).unwrap(), None);
        headers.insert(
            INGRESS_PATH_HEADER,
            HeaderValue::from_static("/x\"><script>"),
        );
        assert!(base_href(only, &headers).is_err());
    }
}
