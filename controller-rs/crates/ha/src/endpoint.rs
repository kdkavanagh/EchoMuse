//! Where Home Assistant lives and the token the controller presents to it.

use std::fmt;

use echomuse_core::services::MediaUrl;
use url::Url;

/// The WebSocket API inside an add-on. HA's add-on documentation names this address for
/// the Supervisor's proxy to Home Assistant Core.
const SUPERVISOR_WEBSOCKET_URL: &str = "ws://supervisor/core/websocket";
/// The Supervisor's proxy to Core's REST API; REST paths go under `<this>/api`.
const SUPERVISOR_REST_BASE: &str = "http://supervisor/core";

/// How to reach Home Assistant: its WebSocket API, the base of its REST API, and the
/// long-lived access token sent to both.
#[derive(Clone, Eq, PartialEq)]
pub struct HaEndpoint {
    websocket_url: String,
    /// Has no trailing slash and no `/api` suffix.
    rest_base: String,
    token: String,
}

/// Why the endpoint settings were refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndpointError {
    /// `HA_URL` is not an absolute `http://` or `https://` URL with a host.
    InvalidUrl(String),
    /// A token was given but is empty.
    EmptyToken,
}

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(detail) => write!(f, "HA_URL is not usable: {detail}"),
            Self::EmptyToken => f.write_str("the Home Assistant token is empty"),
        }
    }
}

impl std::error::Error for EndpointError {}

impl HaEndpoint {
    /// Home Assistant at `base_url` (for example `http://homeassistant.local:8123`), the
    /// direct setup used by Docker and bare-metal installs. An `https` base gives a `wss`
    /// WebSocket.
    pub fn direct(base_url: &str, token: &str) -> Result<Self, EndpointError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(EndpointError::EmptyToken);
        }
        let parsed = Url::parse(base_url.trim())
            .map_err(|err| EndpointError::InvalidUrl(err.to_string()))?;
        let websocket_scheme = match parsed.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => {
                return Err(EndpointError::InvalidUrl(format!(
                    "scheme {other:?} is neither http nor https"
                )));
            }
        };
        if parsed.host_str().is_none_or(str::is_empty) {
            return Err(EndpointError::InvalidUrl("no host".to_owned()));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(EndpointError::InvalidUrl(
                "a query or fragment cannot be part of the base URL".to_owned(),
            ));
        }
        let rest_base = parsed.as_str().trim_end_matches('/').to_owned();
        let after_scheme = &rest_base[parsed.scheme().len()..];
        Ok(Self {
            websocket_url: format!("{websocket_scheme}{after_scheme}/api/websocket"),
            rest_base,
            token: token.to_owned(),
        })
    }

    /// Home Assistant behind the Supervisor's proxy, from inside an add-on.
    pub fn supervisor(token: &str) -> Result<Self, EndpointError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(EndpointError::EmptyToken);
        }
        Ok(Self {
            websocket_url: SUPERVISOR_WEBSOCKET_URL.to_owned(),
            rest_base: SUPERVISOR_REST_BASE.to_owned(),
            token: token.to_owned(),
        })
    }

    /// The WebSocket API's URL.
    pub fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    /// The REST API's URL for `path`, which starts with `/` and is relative to `/api`.
    pub fn rest_url(&self, path: &str) -> String {
        format!("{}/api{path}", self.rest_base)
    }

    /// Resolves a URL Home Assistant returned. HA gives media such as TTS audio as a path
    /// from its root (`/api/tts_proxy/<token>`); an absolute URL is kept. None for anything
    /// else.
    pub fn absolute(&self, url: &str) -> Option<MediaUrl> {
        if url.starts_with("http://") || url.starts_with("https://") {
            Some(MediaUrl(url.to_owned()))
        } else if url.starts_with('/') {
            Some(MediaUrl(format!("{}{url}", self.rest_base)))
        } else {
            None
        }
    }

    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn uses_tls(&self) -> bool {
        self.websocket_url.starts_with("wss://") || self.rest_base.starts_with("https://")
    }
}

impl fmt::Debug for HaEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HaEndpoint")
            .field("websocket_url", &self.websocket_url)
            .field("rest_base", &self.rest_base)
            .field("token", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_direct_https_url_gives_a_secure_websocket_and_keeps_its_port() {
        let endpoint = HaEndpoint::direct("https://ha.example:8123/", "secret").unwrap();
        assert_eq!(
            endpoint.websocket_url(),
            "wss://ha.example:8123/api/websocket"
        );
        assert_eq!(
            endpoint.rest_url("/intent/handle"),
            "https://ha.example:8123/api/intent/handle"
        );
        assert!(endpoint.uses_tls());
        let plain = HaEndpoint::direct("http://10.0.0.5:8123", "secret").unwrap();
        assert_eq!(plain.websocket_url(), "ws://10.0.0.5:8123/api/websocket");
        assert!(!plain.uses_tls());
    }

    #[test]
    fn the_add_on_endpoint_goes_through_the_supervisor_proxy() {
        let endpoint = HaEndpoint::supervisor("supervisor-token").unwrap();
        assert_eq!(endpoint.websocket_url(), "ws://supervisor/core/websocket");
        assert_eq!(
            endpoint.rest_url("/states/select.office_assistant"),
            "http://supervisor/core/api/states/select.office_assistant"
        );
        assert_eq!(
            endpoint.absolute("/api/tts_proxy/abc.flac"),
            Some(MediaUrl(
                "http://supervisor/core/api/tts_proxy/abc.flac".to_owned()
            ))
        );
    }

    #[test]
    fn unusable_urls_and_tokens_are_refused() {
        assert!(matches!(
            HaEndpoint::direct("ftp://ha.local", "t"),
            Err(EndpointError::InvalidUrl(_))
        ));
        assert!(matches!(
            HaEndpoint::direct("ha.local:8123", "t"),
            Err(EndpointError::InvalidUrl(_))
        ));
        assert!(matches!(
            HaEndpoint::direct("http://ha.local:8123/?x=1", "t"),
            Err(EndpointError::InvalidUrl(_))
        ));
        assert_eq!(
            HaEndpoint::direct("http://ha.local", ""),
            Err(EndpointError::EmptyToken)
        );
    }

    #[test]
    fn absolute_media_urls_are_kept_and_other_shapes_are_refused() {
        let endpoint = HaEndpoint::direct("http://ha.local:8123", "t").unwrap();
        assert_eq!(
            endpoint.absolute("https://cdn.example/a.mp3"),
            Some(MediaUrl("https://cdn.example/a.mp3".to_owned()))
        );
        assert_eq!(endpoint.absolute("tts_proxy/a.flac"), None);
    }

    #[test]
    fn debug_output_never_shows_the_token() {
        let endpoint = HaEndpoint::direct("http://ha.local", "very-secret").unwrap();
        assert!(!format!("{endpoint:?}").contains("very-secret"));
    }
}
