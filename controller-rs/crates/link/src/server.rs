//! The device listeners (spec §3.1.1): plain and TLS, each serving the control,
//! audio and asset sockets and the shell dial-back. Requests are handled by hyper so a
//! socket that may not join is refused with an HTTP status before any upgrade.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_db::DbHandle;
use echomuse_db::devices::GetDeviceToken;
use echomuse_protocol::LinkSessionId;
use http::header::{
    CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, SEC_WEBSOCKET_VERSION, UPGRADE,
};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time::{Instant, timeout};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::admission::{Admission, AssetSource, DynAdmission, DynAssets};
use crate::assets::serve_assets;
use crate::auth::{HEADER_SESSION, PresentedToken, Transport, authorize};
use crate::config::{LinkConfig, LinkConfigError, MAX_FRAME_BYTES};
use crate::control::Ws;
use crate::handshake::serve_control;
use crate::registry::Registry;
use crate::session::SessionCore;
use crate::shell::{ShellBroker, ShellMode};

/// The device socket paths.
pub const PATH_CONTROL: &str = "/device/v1/control";
/// The audio socket path.
pub const PATH_AUDIO: &str = "/device/v1/audio";
/// The asset socket path.
pub const PATH_ASSETS: &str = "/device/v1/assets";
/// The shell dial-back path prefix; the device id follows.
pub const PATH_SHELL_PREFIX: &str = "/shell/";

/// What every task of the server shares.
pub(crate) struct Shared {
    pub(crate) config: LinkConfig,
    pub(crate) db: DbHandle,
    pub(crate) admission: Box<dyn DynAdmission>,
    pub(crate) assets: Arc<dyn DynAssets>,
    pub(crate) registry: Registry,
    pub(crate) shells: ShellBroker,
    pub(crate) tracker: TaskTracker,
    pub(crate) shutdown: CancellationToken,
    pub(crate) mono_epoch: Instant,
    pub(crate) server_boot_id: Uuid,
}

/// Why the server did not start.
#[derive(Debug)]
pub enum LinkError {
    /// The configuration is unusable.
    Config(LinkConfigError),
    /// A listener could not bind.
    Bind {
        /// Which listener.
        transport: Transport,
        /// Why.
        source: io::Error,
    },
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Bind { transport, source } => {
                write!(
                    f,
                    "could not bind the {transport} device listener: {source}"
                )
            }
        }
    }
}

impl std::error::Error for LinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Bind { source, .. } => Some(source),
        }
    }
}

/// The running device listeners. Dropping it stops accepting and ends every session;
/// [`LinkServer::shutdown`] also waits for their tasks.
pub struct LinkServer {
    shared: Arc<Shared>,
    plain_addr: SocketAddr,
    tls_addr: Option<SocketAddr>,
}

impl std::fmt::Debug for LinkServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkServer")
            .field("plain_addr", &self.plain_addr)
            .field("tls_addr", &self.tls_addr)
            .finish_non_exhaustive()
    }
}

impl LinkServer {
    /// Binds both listeners and starts accepting, routing shell dial-backs into
    /// `shells`. Must run inside a Tokio runtime.
    pub async fn bind<A: Admission, S: AssetSource>(
        config: LinkConfig,
        db: DbHandle,
        admission: A,
        assets: S,
        shells: ShellBroker,
    ) -> Result<Self, LinkError> {
        config.validate().map_err(LinkError::Config)?;
        let plain = TcpListener::bind((config.host.as_str(), config.plain_port))
            .await
            .map_err(|source| LinkError::Bind {
                transport: Transport::Plain,
                source,
            })?;
        let tls = match &config.tls {
            None => None,
            Some(tls) => {
                let listener = TcpListener::bind((config.host.as_str(), tls.port))
                    .await
                    .map_err(|source| LinkError::Bind {
                        transport: Transport::Tls,
                        source,
                    })?;
                Some((listener, TlsAcceptor::from(Arc::clone(&tls.server_config))))
            }
        };
        let plain_addr = plain.local_addr().map_err(|source| LinkError::Bind {
            transport: Transport::Plain,
            source,
        })?;
        let tls_addr = match &tls {
            None => None,
            Some((listener, _)) => {
                Some(listener.local_addr().map_err(|source| LinkError::Bind {
                    transport: Transport::Tls,
                    source,
                })?)
            }
        };
        let shared = Arc::new(Shared::new(config, db, admission, assets, shells));
        shared
            .tracker
            .spawn(accept_plain(Arc::clone(&shared), plain));
        if let Some((listener, acceptor)) = tls {
            shared
                .tracker
                .spawn(accept_tls(Arc::clone(&shared), listener, acceptor));
        }
        tracing::info!(%plain_addr, ?tls_addr, "device listeners started");
        Ok(Self {
            shared,
            plain_addr,
            tls_addr,
        })
    }

    /// The plain listener's address.
    pub fn plain_addr(&self) -> SocketAddr {
        self.plain_addr
    }

    /// The TLS listener's address; `None` when TLS is off.
    pub fn tls_addr(&self) -> Option<SocketAddr> {
        self.tls_addr
    }

    /// The `server_boot_id` every `session.ready` of this server carries.
    pub fn server_boot_id(&self) -> Uuid {
        self.shared.server_boot_id
    }

    /// Stops accepting, ends every session with
    /// [`CloseReason::Shutdown`](crate::CloseReason::Shutdown), closes every shell, and
    /// waits for every task to finish.
    pub async fn shutdown(self) {
        self.shared.stop();
        self.shared.tracker.wait().await;
    }

    /// Serves one in-process connection as if a listener had accepted it.
    #[cfg(test)]
    pub(crate) fn serve_io<IO>(&self, io: IO, transport: Transport, peer: SocketAddr)
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let shared = Arc::clone(&self.shared);
        self.shared
            .tracker
            .spawn(serve_connection(shared, io, transport, peer));
    }
}

impl Drop for LinkServer {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

impl Shared {
    fn new<A: Admission, S: AssetSource>(
        config: LinkConfig,
        db: DbHandle,
        admission: A,
        assets: S,
        shells: ShellBroker,
    ) -> Self {
        Self {
            config,
            db,
            admission: Box::new(admission),
            assets: Arc::new(assets),
            registry: Registry::default(),
            shells,
            tracker: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            mono_epoch: Instant::now(),
            server_boot_id: Uuid::new_v4(),
        }
    }

    fn stop(&self) {
        self.shutdown.cancel();
        self.shells.close_all();
        self.tracker.close();
    }
}

/// How long to wait after a failed accept, which usually means the process is out of
/// file descriptors, before trying again.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

async fn accept_plain(shared: Arc<Shared>, listener: TcpListener) {
    loop {
        let accepted = tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let task = serve_connection(Arc::clone(&shared), stream, Transport::Plain, peer);
                shared.tracker.spawn(task);
            }
            Err(error) => {
                tracing::warn!(%error, "plain device listener accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

async fn accept_tls(shared: Arc<Shared>, listener: TcpListener, acceptor: TlsAcceptor) {
    loop {
        let accepted = tokio::select! {
            _ = shared.shutdown.cancelled() => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let acceptor = acceptor.clone();
                let task_shared = Arc::clone(&shared);
                shared.tracker.spawn(async move {
                    let handshake = timeout(
                        task_shared.config.timings.tls_handshake,
                        acceptor.accept(stream),
                    );
                    let stream = tokio::select! {
                        _ = task_shared.shutdown.cancelled() => return,
                        stream = handshake => stream,
                    };
                    match stream {
                        Ok(Ok(stream)) => {
                            serve_connection(task_shared, stream, Transport::Tls, peer).await;
                        }
                        Ok(Err(error)) => {
                            tracing::info!(%peer, %error, "TLS handshake failed");
                        }
                        Err(_) => tracing::info!(%peer, "TLS handshake timed out"),
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "TLS device listener accept failed");
                tokio::time::sleep(ACCEPT_BACKOFF).await;
            }
        }
    }
}

/// Serves HTTP/1.1 on one connection until it upgrades or closes.
async fn serve_connection<IO>(shared: Arc<Shared>, io: IO, transport: Transport, peer: SocketAddr)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service_shared = Arc::clone(&shared);
    let service = service_fn(move |request| {
        let shared = Arc::clone(&service_shared);
        async move { Ok::<_, Infallible>(route(shared, transport, peer, request).await) }
    });
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(shared.config.timings.hello);
    let connection = builder
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades();
    tokio::select! {
        _ = shared.shutdown.cancelled() => {}
        served = connection => {
            if let Err(error) = served {
                tracing::debug!(%peer, %error, "device HTTP connection ended with an error");
            }
        }
    }
}

/// Which socket a request asks for.
enum Endpoint {
    Control,
    Audio,
    Assets,
    Shell { device: DeviceId, mode: ShellMode },
}

impl Endpoint {
    fn of(request: &Request<Incoming>) -> Option<Self> {
        let path = request.uri().path();
        Some(match path {
            PATH_CONTROL => Self::Control,
            PATH_AUDIO => Self::Audio,
            PATH_ASSETS => Self::Assets,
            _ => {
                let device = path.strip_prefix(PATH_SHELL_PREFIX)?;
                if device.is_empty() || device.contains('/') {
                    return None;
                }
                let pty = request
                    .uri()
                    .query()
                    .is_some_and(|query| query.split('&').any(|pair| pair == "pty=1"));
                Self::Shell {
                    device: DeviceId(device.to_owned()),
                    mode: if pty { ShellMode::Pty } else { ShellMode::Pipe },
                }
            }
        })
    }
}

type HttpResponse = Response<Empty<Bytes>>;

fn status(code: StatusCode) -> HttpResponse {
    let mut response = Response::new(Empty::new());
    *response.status_mut() = code;
    response
}

/// The request's `Sec-WebSocket-Key`, when it is a WebSocket version 13 upgrade.
fn websocket_key(request: &Request<Incoming>) -> Option<HeaderValue> {
    let headers = request.headers();
    let has_token = |name, token: &str| {
        headers.get_all(name).iter().any(|value| {
            value.to_str().is_ok_and(|text| {
                text.split(',')
                    .any(|part| part.trim().eq_ignore_ascii_case(token))
            })
        })
    };
    let upgrade = request.method() == Method::GET
        && has_token(CONNECTION, "upgrade")
        && has_token(UPGRADE, "websocket")
        && headers
            .get(SEC_WEBSOCKET_VERSION)
            .is_some_and(|version| version == "13");
    if !upgrade {
        return None;
    }
    headers.get(SEC_WEBSOCKET_KEY).cloned()
}

async fn route(
    shared: Arc<Shared>,
    transport: Transport,
    peer: SocketAddr,
    mut request: Request<Incoming>,
) -> HttpResponse {
    let Some(endpoint) = Endpoint::of(&request) else {
        return status(StatusCode::NOT_FOUND);
    };
    let Some(key) = websocket_key(&request) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let token = PresentedToken::from_headers(request.headers());
    match endpoint {
        Endpoint::Control => {
            let task_shared = Arc::clone(&shared);
            upgrade(&shared, &mut request, &key, move |ws| {
                serve_control(task_shared, ws, transport, peer, token)
            })
        }
        Endpoint::Audio => {
            let core = match joinable_session(&shared, &request, transport, token.as_ref()).await {
                Ok(core) => core,
                Err(code) => return status(code),
            };
            let Some(deliver) = shared.registry.take_audio(&core) else {
                tracing::info!(device = %core.device, "refusing a second audio socket");
                return status(StatusCode::FORBIDDEN);
            };
            upgrade(&shared, &mut request, &key, move |ws| async move {
                // A session that ended meanwhile drops the socket.
                let _ = deliver.send(ws);
            })
        }
        Endpoint::Assets => {
            let core = match joinable_session(&shared, &request, transport, token.as_ref()).await {
                Ok(core) => core,
                Err(code) => return status(code),
            };
            let source = Arc::clone(&shared.assets);
            let write = shared.config.timings.write;
            let stop = core.stop.clone();
            upgrade(&shared, &mut request, &key, move |ws| {
                serve_assets(source, ws, stop, write)
            })
        }
        Endpoint::Shell { device, mode } => {
            if let Err(code) = check_device_token(&shared, &device, transport, token.as_ref()).await
            {
                return status(code);
            }
            if !shared.shells.is_expected(&device) {
                tracing::info!(%device, "refusing a shell nobody asked for");
                return status(StatusCode::FORBIDDEN);
            }
            let shells = shared.shells.clone();
            upgrade(&shared, &mut request, &key, move |ws| async move {
                shells.deliver(&device, ws, mode);
            })
        }
    }
}

/// Checks that an audio or asset upgrade may join the session its `X-EM-Session`
/// names: a live session, on the same kind of listener as its control socket, with
/// a token the device's rules accept.
async fn joinable_session(
    shared: &Shared,
    request: &Request<Incoming>,
    transport: Transport,
    token: Option<&PresentedToken>,
) -> Result<Arc<SessionCore>, StatusCode> {
    let session = request
        .headers()
        .get(HEADER_SESSION)
        .and_then(|value| value.to_str().ok())
        .map(|text| LinkSessionId::from(text.to_owned()));
    let Some(core) = session.and_then(|session| shared.registry.lookup(&session)) else {
        tracing::info!(path = %request.uri().path(), "refusing a socket that names no live session");
        return Err(StatusCode::FORBIDDEN);
    };
    if core.transport != transport {
        tracing::info!(device = %core.device, %transport, "refusing a socket on the wrong kind of listener");
        return Err(StatusCode::FORBIDDEN);
    }
    check_device_token(shared, &core.device, transport, token).await?;
    // The session may have ended while the token was read.
    if !shared.registry.is_live(&core) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(core)
}

async fn check_device_token(
    shared: &Shared,
    device: &DeviceId,
    transport: Transport,
    token: Option<&PresentedToken>,
) -> Result<(), StatusCode> {
    let query = GetDeviceToken {
        device: device.clone(),
    };
    let stored = match shared.db.read(query).await {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(%device, %error, "could not read a device token");
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
    };
    authorize(
        token,
        stored.as_ref(),
        transport,
        shared.config.require_device_tls,
    )
    .map_err(|refusal| {
        tracing::info!(%device, %transport, %refusal, "refusing a device socket");
        StatusCode::FORBIDDEN
    })
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
}

/// Answers 101 and runs `serve` on the upgraded socket. Compression is never
/// negotiated.
fn upgrade<F, Fut>(
    shared: &Shared,
    request: &mut Request<Incoming>,
    key: &HeaderValue,
    serve: F,
) -> HttpResponse
where
    F: FnOnce(Ws) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let Ok(accept) = HeaderValue::from_str(&derive_accept_key(key.as_bytes())) else {
        return status(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let on_upgrade = hyper::upgrade::on(request);
    shared.tracker.spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => {
                let ws = WebSocketStream::from_raw_socket(
                    TokioIo::new(upgraded),
                    Role::Server,
                    Some(websocket_config()),
                )
                .await;
                serve(ws).await;
            }
            Err(error) => tracing::debug!(%error, "device socket upgrade failed"),
        }
    });
    let mut response = status(StatusCode::SWITCHING_PROTOCOLS);
    let headers = response.headers_mut();
    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
    headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert(SEC_WEBSOCKET_ACCEPT, accept);
    response
}
