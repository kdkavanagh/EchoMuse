//! The client handle: starting the session, commands, subscriptions and REST calls.

use std::{
    fmt,
    sync::{Arc, Weak},
    time::Duration,
};

use echomuse_core::{
    services::{
        COMMAND_TIMEOUT, EffectOutcome, HaConnectionGeneration, HaError, NotDispatched,
        UnknownCause,
    },
    types::WorkScope,
};
use rustls::{ClientConfig, RootCertStore};
use serde_json::{Map, Value, json};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{
    connection::{
        EventRoute, HaEvent, HaStatus, Link, Reply, Route, RouteGuard, SessionEnd, run_session,
    },
    endpoint::HaEndpoint,
};

/// Timing and capacity settings of the session. [`Default`] gives the production values.
#[derive(Clone, Copy, Debug)]
pub struct HaOptions {
    /// Reply deadline of ordinary commands and REST calls, and of each step of connecting.
    pub command_timeout: Duration,
    /// Wait before the first reconnect attempt; it doubles per failed attempt.
    pub reconnect_initial: Duration,
    /// The longest wait between reconnect attempts.
    pub reconnect_max: Duration,
    /// How often the connection is pinged to detect a socket that silently died.
    pub heartbeat: Duration,
    /// How long the single STT retry waits for a new connection.
    pub stt_reconnect_wait: Duration,
    /// Capacity of each event subscription's channel.
    pub subscription_capacity: usize,
}

impl Default for HaOptions {
    fn default() -> Self {
        Self {
            command_timeout: COMMAND_TIMEOUT,
            // The spec's restart backoff (§5.7).
            reconnect_initial: Duration::from_secs(1),
            reconnect_max: Duration::from_secs(30),
            heartbeat: Duration::from_secs(15),
            // Covers the first reconnect attempt after the default 1 s wait, with time for
            // connecting and authenticating on a local network.
            stt_reconnect_wait: Duration::from_secs(3),
            subscription_capacity: 256,
        }
    }
}

/// Why the session could not start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StartError {
    /// The endpoint uses TLS and the system trust store gave no usable certificate.
    NoTrustRoots,
    /// The TLS configuration could not be built.
    Tls(String),
    /// The HTTP client could not be built.
    Http(String),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoTrustRoots => {
                f.write_str("no trusted certificates found for TLS to Home Assistant")
            }
            Self::Tls(detail) => write!(f, "TLS setup failed: {detail}"),
            Self::Http(detail) => write!(f, "HTTP client setup failed: {detail}"),
        }
    }
}

impl std::error::Error for StartError {}

/// State shared by the handle and the session task.
pub(crate) struct Shared {
    pub endpoint: Arc<HaEndpoint>,
    pub options: HaOptions,
    pub tls: Arc<ClientConfig>,
    pub http: reqwest::Client,
    pub status: watch::Sender<HaStatus>,
    pub stop: CancellationToken,
}

/// The Home Assistant session: one WebSocket connection shared by every device, plus REST.
/// Cheap to clone; every clone uses the same connection.
#[derive(Clone)]
pub struct HaClient {
    pub(crate) shared: Arc<Shared>,
    links: watch::Receiver<Option<Arc<Link>>>,
}

/// An open `subscribe_events` subscription.
///
/// HA forgets subscriptions when the connection drops, so the channel ends with its
/// connection; subscribe again when [`HaStatus::generation`] changes. The channel also ends
/// when the subscriber falls a full channel behind, rather than silently losing events.
#[derive(Debug)]
pub struct Subscription {
    /// The connection the subscription lives on.
    pub generation: HaConnectionGeneration,
    /// The events in HA's order; ends with the connection or on falling behind.
    pub events: mpsc::Receiver<HaEvent>,
    id: u64,
    link: Weak<Link>,
}

/// A REST call that did not produce a response.
pub(crate) enum RestFailure {
    /// The connection could not be opened, so nothing was sent.
    NotSent,
    TimedOut,
    /// The connection failed after the request may have been sent.
    Lost,
}

impl From<RestFailure> for HaError {
    fn from(failure: RestFailure) -> Self {
        match failure {
            RestFailure::NotSent => HaError::NotDispatched(NotDispatched::Unavailable),
            RestFailure::TimedOut => HaError::Timeout,
            RestFailure::Lost => HaError::ConnectionLost,
        }
    }
}

impl HaClient {
    /// Starts the session task on the current Tokio runtime and returns the handle with
    /// the task's join handle, which the owner supervises. The task reconnects until
    /// [`HaClient::stop`] or until HA refuses the token.
    pub fn start(
        endpoint: HaEndpoint,
        options: HaOptions,
    ) -> Result<(Self, JoinHandle<SessionEnd>), StartError> {
        // reqwest needs a TLS configuration even for plain HTTP, and this one names its
        // crypto provider explicitly.
        let tls = Arc::new(tls_config(endpoint.uses_tls())?);
        let http = reqwest::Client::builder()
            .no_proxy()
            .tls_backend_preconfigured(ClientConfig::clone(&tls))
            .build()
            .map_err(|err| StartError::Http(err.to_string()))?;
        let (status, _) = watch::channel(HaStatus::default());
        let shared = Arc::new(Shared {
            endpoint: Arc::new(endpoint),
            options,
            tls,
            http,
            status,
            stop: CancellationToken::new(),
        });
        let (link_sender, links) = watch::channel(None);
        let task = tokio::spawn(run_session(shared.clone(), link_sender));
        Ok((Self { shared, links }, task))
    }

    /// The connection status, updated on every change.
    pub fn status(&self) -> watch::Receiver<HaStatus> {
        self.shared.status.subscribe()
    }

    /// Ends the session: the connection closes and every open request fails.
    pub fn stop(&self) {
        self.shared.stop.cancel();
    }

    pub(crate) fn endpoint(&self) -> &Arc<HaEndpoint> {
        &self.shared.endpoint
    }

    pub(crate) fn options(&self) -> &HaOptions {
        &self.shared.options
    }

    pub(crate) fn current_link(&self) -> Option<Arc<Link>> {
        self.links
            .borrow()
            .as_ref()
            .filter(|link| !link.is_lost())
            .cloned()
    }

    /// Waits up to `wait` for a live connection other than `not`.
    pub(crate) async fn wait_for_link(
        &self,
        not: Option<HaConnectionGeneration>,
        wait: Duration,
    ) -> Option<Arc<Link>> {
        let mut links = self.links.clone();
        let found = tokio::time::timeout(
            wait,
            links.wait_for(|link| {
                link.as_ref()
                    .is_some_and(|link| !link.is_lost() && Some(link.generation) != not)
            }),
        )
        .await;
        match found {
            Ok(Ok(link)) => link.clone(),
            _ => None,
        }
    }

    /// Sends one command and returns HA's `result`, the dynamic JSON each caller parses.
    pub(crate) async fn command(
        &self,
        scope: Option<&WorkScope>,
        body: Map<String, Value>,
        timeout: Duration,
    ) -> Result<Value, HaError> {
        let link = self
            .current_link()
            .ok_or(HaError::NotDispatched(NotDispatched::Unavailable))?;
        let (sender, reply) = oneshot::channel();
        let id = link
            .dispatch(scope, body, Route::Reply(Some(sender)))
            .await
            .map_err(HaError::NotDispatched)?;
        let _route = RouteGuard::new(&link, id);
        match tokio::time::timeout(timeout, reply).await {
            Err(_) => Err(HaError::Timeout),
            Ok(Err(_) | Ok(Reply::Lost)) => Err(HaError::ConnectionLost),
            Ok(Ok(Reply::Refused(refusal))) => Err(refusal.into()),
            Ok(Ok(Reply::Done(result))) => Ok(result),
        }
    }

    /// Sends a state-changing command whose result carries nothing the caller reads.
    /// Never resent once it may have been sent.
    pub(crate) async fn apply(&self, body: Map<String, Value>) -> EffectOutcome<()> {
        match self
            .command(None, body, self.shared.options.command_timeout)
            .await
        {
            Ok(_) => EffectOutcome::Applied(()),
            Err(HaError::NotDispatched(reason)) => EffectOutcome::NotDispatched(reason),
            Err(HaError::Rejected { code, message }) => EffectOutcome::Rejected { code, message },
            Err(HaError::Timeout) => EffectOutcome::Unknown(UnknownCause::Timeout),
            Err(HaError::ConnectionLost) => EffectOutcome::Unknown(UnknownCause::ConnectionLost),
            Err(HaError::BadResponse(_)) => EffectOutcome::Unknown(UnknownCause::Inconclusive),
        }
    }

    /// Subscribes to HA's events of `event_type` on the current connection.
    pub async fn subscribe_events(&self, event_type: &str) -> Result<Subscription, HaError> {
        let link = self
            .current_link()
            .ok_or(HaError::NotDispatched(NotDispatched::Unavailable))?;
        let (confirm, confirmed) = oneshot::channel();
        let (events, receiver) = mpsc::channel(self.shared.options.subscription_capacity);
        let body = object(json!({"type": "subscribe_events", "event_type": event_type}));
        let route = Route::Events(EventRoute {
            confirm: Some(confirm),
            events,
        });
        let id = link
            .dispatch(None, body, route)
            .await
            .map_err(HaError::NotDispatched)?;
        let route = RouteGuard::new(&link, id);
        let outcome = tokio::time::timeout(self.shared.options.command_timeout, confirmed).await;
        match outcome {
            Err(_) => Err(HaError::Timeout),
            Ok(Err(_) | Ok(Reply::Lost)) => Err(HaError::ConnectionLost),
            Ok(Ok(Reply::Refused(refusal))) => Err(refusal.into()),
            Ok(Ok(Reply::Done(_))) => {
                // The subscription outlives this call; keep its route.
                route.keep();
                Ok(Subscription {
                    generation: link.generation,
                    events: receiver,
                    id,
                    link: Arc::downgrade(&link),
                })
            }
        }
    }

    /// Ends `subscription`. Once its connection is gone HA has already forgotten it, and
    /// this only drops the local end.
    pub async fn unsubscribe(&self, subscription: Subscription) -> Result<(), HaError> {
        self.end_subscription(subscription.link, subscription.id)
            .await
    }

    /// Ends the subscription `id` on `link`, an `unsubscribe_events` for both kinds of
    /// subscription. A connection that is already gone needs nothing sent.
    pub(crate) async fn end_subscription(&self, link: Weak<Link>, id: u64) -> Result<(), HaError> {
        let Some(link) = link.upgrade() else {
            return Ok(());
        };
        link.remove(id);
        if link.is_lost() {
            return Ok(());
        }
        let body = object(json!({"type": "unsubscribe_events", "subscription": id}));
        match self
            .command(None, body, self.shared.options.command_timeout)
            .await
        {
            Err(HaError::ConnectionLost | HaError::NotDispatched(NotDispatched::Unavailable)) => {
                Ok(())
            }
            other => other.map(drop),
        }
    }

    /// `GET <rest base>/api<path>` with the bearer token.
    pub(crate) async fn rest_get(&self, path: &str) -> Result<(u16, Vec<u8>), RestFailure> {
        let request = self.shared.http.get(self.shared.endpoint.rest_url(path));
        self.rest_send(request).await
    }

    /// `POST <rest base>/api<path>` with a JSON body and the bearer token.
    pub(crate) async fn rest_post(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<(u16, Vec<u8>), RestFailure> {
        let request = self
            .shared
            .http
            .post(self.shared.endpoint.rest_url(path))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        self.rest_send(request).await
    }

    async fn rest_send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(u16, Vec<u8>), RestFailure> {
        let classify = |err: reqwest::Error| {
            if err.is_connect() {
                RestFailure::NotSent
            } else if err.is_timeout() {
                RestFailure::TimedOut
            } else {
                RestFailure::Lost
            }
        };
        let response = request
            .bearer_auth(self.shared.endpoint.token())
            .timeout(self.shared.options.command_timeout)
            .send()
            .await
            .map_err(classify)?;
        let status = response.status().as_u16();
        let body = response.bytes().await.map_err(classify)?;
        Ok((status, body.to_vec()))
    }
}

/// The JSON object of a `json!` literal.
pub(crate) fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => unreachable!("request bodies are object literals"),
    }
}

/// The outcome of a state-changing REST call: a 4xx is a refusal made before anything
/// changed; any other failure after sending is unknown, as is a 2xx answer `read` cannot
/// use.
pub(crate) fn rest_effect<T>(
    sent: Result<(u16, Vec<u8>), RestFailure>,
    read: impl FnOnce(&[u8]) -> Option<T>,
) -> EffectOutcome<T> {
    match sent {
        Err(RestFailure::NotSent) => EffectOutcome::NotDispatched(NotDispatched::Unavailable),
        Err(RestFailure::TimedOut) => EffectOutcome::Unknown(UnknownCause::Timeout),
        Err(RestFailure::Lost) => EffectOutcome::Unknown(UnknownCause::ConnectionLost),
        Ok((status, body)) if (200..300).contains(&status) => read(&body).map_or(
            EffectOutcome::Unknown(UnknownCause::Inconclusive),
            EffectOutcome::Applied,
        ),
        Ok((status, body)) if (400..500).contains(&status) => EffectOutcome::Rejected {
            code: status.to_string(),
            message: rest_message(&body),
        },
        Ok(_) => EffectOutcome::Unknown(UnknownCause::Inconclusive),
    }
}

/// A REST read: 2xx gives the body, anything else is a refusal.
pub(crate) fn rest_read(sent: Result<(u16, Vec<u8>), RestFailure>) -> Result<Vec<u8>, HaError> {
    let (status, body) = sent?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(HaError::Rejected {
            code: status.to_string(),
            message: rest_message(&body),
        })
    }
}

/// HA's JSON `message`, or the body as text.
pub(crate) fn rest_message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// Trust roots from the operating system's store, with the `ring` provider chosen
/// explicitly so the result does not depend on which providers other crates enable.
/// `require_roots`: the endpoint uses TLS, so an empty trust store is an error.
fn tls_config(require_roots: bool) -> Result<ClientConfig, StartError> {
    let mut roots = RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    roots.add_parsable_certificates(loaded.certs);
    if require_roots && roots.is_empty() {
        return Err(StartError::NoTrustRoots);
    }
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|err| StartError::Tls(err.to_string()))
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}
