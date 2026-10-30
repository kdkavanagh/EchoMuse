//! The dashboard HTTP API: sign-in, sessions, roles, add-on ingress, the dashboard's
//! pages, the live event stream, and the device, configuration and wake-model routes,
//! serving the existing dashboard unchanged (§3.4).
//!
//! The integrator, on the shared Tokio runtime:
//!
//! 1. asks [`BootstrapToken::issue_if_needed`] for a setup token and, when it gets one,
//!    prints [`BootstrapToken::expose`] to the log for the person installing it;
//! 2. builds an [`ApiServer`] from an [`ApiConfig`] (with `DEVICE_APPROVAL` and
//!    `EM_CONTROLLER_VERSION`) and [`ApiDeps`]: the database and settings handles, that
//!    token, its [`DeviceRuntimes`], a [`ModelQualifier`], the [`HaStatus`] channel and
//!    a [`WallClock`];
//! 3. keeps [`ApiServer::publisher`] to publish [`ApiEvent`]s, shaping each runtime's
//!    `device_update` with [`LiveDevice::state_patch_since`];
//! 4. binds `API_PORT` and runs [`ApiServer::serve`] until shutdown.
//!
//! Routes served now:
//!
//! | Route | Who |
//! |---|---|
//! | `POST /api/setup` | anyone holding the setup token, while no user exists |
//! | `GET /api/system/setup-state` | anyone |
//! | `POST /api/auth/login` | anyone |
//! | `POST /api/auth/logout`, `GET /api/auth/me`, `POST /api/auth/change-password` | signed in |
//! | `WS /api/events` | signed in |
//! | `GET /api/devices`, `/api/devices/pending`, `/api/devices/{id}`, `…/config`, `…/logs` | signed in |
//! | `PATCH`/`DELETE /api/devices/{id}`, `POST …/approve`, `POST …/config` | admin |
//! | `GET /api/global/config`, `/api/system/status`, `/api/ha/status`, `/api/wake_models` | signed in |
//! | `POST /api/global/config`, `GET`/`PATCH /api/system/config` | admin |
//! | `POST /api/wake_models/upload`, `DELETE /api/wake_models/{graph_sha256}` | admin |
//! | `GET /api/turns`, `/api/near_misses`, `/api/clip_windows`, `/api/wakeclips.zip`, `/api/near_misses.zip`, `/api/devices/{id}/turns`, `…/near_misses`, `…/turns/{turn}/{trace,afe,wake,audio,recording}`, `…/near_misses/{nm}/{afe,recording}`, `…/wake_shadow`, `…/response_latency`, `…/wakeclips.zip` | signed in |
//! | `PATCH /api/devices/{id}/near_misses/{nm}` | admin |
//! | Wi-Fi, recording modes and their files, the session recorder, the shell, alarms, sounds, Kiosk Satellite login | see [`device_services`] |
//! | Capture mode, its windows and recordings | see [`capture`] |
//! | `GET /api/devices/{id}/activity`, `…/speech_assets`, `/api/releases/controller`, `POST /api/text_fix/trial`, `GET /api/text_fix/unknown_words` | signed in |
//! | `GET /api/support/bundle`, `/api/speech/wyoming` | admin |
//! | `GET /`, `GET /dashboard`, `GET /static/…` | anyone |
//!
//! See [`devices`], [`system`], [`wake_models`], [`activity`], [`recordings`],
//! [`device_services`] and [`capture`] for each route's shapes, and for the rest the
//! private modules `device_activity`, `speech`, `text_fix`, `support` and `releases`.
//! `crates/api/ROUTES.md` lists every request the shipped dashboard makes against these
//! routes.
//!
//! Every failure answers with the body `{error, code}` ([`ApiError`]); unknown paths
//! answer 404 and known paths with another method 405 in that shape.

pub mod activity;
mod auth;
mod body;
pub mod capture;
mod clock;
mod device_activity;
pub mod device_services;
pub mod devices;
pub mod diagnostics;
mod error;
mod events;
pub mod firmware;
mod gaps;
mod ingress;
mod pages;
mod params;
pub mod recordings;
mod releases;
mod runtime;
mod services;
mod speech;
mod state;
mod support;
pub mod system;
mod text_fix;
pub mod wake_models;
mod zip_writer;

use std::{future::Future, io, net::SocketAddr, path::PathBuf};

use axum::{Router, http::StatusCode, middleware};
use echomuse_db::{DbHandle, devices::ApprovalMode};
use echomuse_records::RecordsService;
use echomuse_settings::service::SettingsHandle;
use tokio::{
    net::TcpListener,
    sync::{Mutex, watch},
};
use tokio_util::sync::CancellationToken;

pub use auth::{
    AdminUser, AuthenticatedUser, Role, UserId,
    password::{BcryptCost, CONCURRENT_HASHES, MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS},
    store::{DEFAULT_SESSION_DAYS, MAX_SESSION_DAYS},
    throttle::ThrottleConfig,
    token::BootstrapToken,
};
pub use capture::{
    CaptureArm, CaptureControl, CaptureError, CaptureRecording, CaptureState, CaptureTag,
    CaptureTransport, CaptureWebhook, IdleSeconds, RecordingRequest, TransportKind, WindowFailure,
    WindowFailureReason, WindowId, WindowMs, WindowView,
};
pub use clock::{SystemWallClock, WallClock};
pub use devices::DeviceLabel;
pub use diagnostics::{
    ClipKind, ClipRef, ClipScoring, Diagnostics, LocalRoute, SoundedWord, TextFixError,
    TextFixService, TextFixTrial, TextFixTrialRequest, UnknownWord,
};
pub use error::{ApiError, ErrorCode};
pub use events::{
    AlertNotice, ApiEvent, DeviceStatePatch, DeviceView, EventPublisher, EventsConfig,
    FeatureStatus, TimerDisplay,
};
pub use firmware::{
    DeployReport, DeploySkip, DeploySkipReason, FirmwareControl, FirmwareRefusal, UpdateStatus,
};
pub use gaps::{GapReason, capability_gaps};
pub use ingress::{INGRESS_PATH_HEADER, Ingress, SUPERVISOR_INGRESS_PROXY};
pub use runtime::{
    DeviceRuntimes, LiveDevice, LiveFailure, LiveHealth, LiveSession, LiveVolume, RuntimeError,
    SettingsApplied,
};
pub use services::{
    AlarmCancel, AlarmReply, AlarmRequest, AlertControl, AmbientListing, AmbientLive, ArchiveFile,
    BoxFuture, ClipEntry, ClipName, ClipPart, CollectLive, DeviceServices, KioskLogin,
    RecorderGrant, RecorderScope, RecordingMode, RecordingModes, SamplesListing, ServiceError,
    SessionRecorders, ShellAccess, ShellChannels, ShellFrame, SoundControl, WifiControl,
    WifiCredentials, WifiNetwork,
};
pub use system::HaStatus;
pub use wake_models::{ModelCandidate, ModelQualifier, ModelRefused};

use crate::{
    auth::{password::Passwords, throttle::Throttle},
    events::EventHub,
    state::{ApiState, Inner},
};

/// How the API behaves.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// The compiled dashboard: `index.html`, `dashboard.html` and the assets they load.
    pub static_dir: PathBuf,
    /// Whether only Home Assistant's ingress proxy is served.
    pub ingress: Ingress,
    /// The work factor of new password hashes.
    pub bcrypt_cost: BcryptCost,
    /// Limits on failed sign-in and setup attempts.
    pub login_throttle: ThrottleConfig,
    /// Limits of the live event stream.
    pub events: EventsConfig,
    /// The deployment's `DEVICE_APPROVAL`: the approval policy reported while the
    /// database holds none.
    pub device_approval: ApprovalMode,
    /// The running controller's version (`EM_CONTROLLER_VERSION`); `None` when unknown.
    pub controller_version: Option<String>,
}

impl ApiConfig {
    /// The production settings for this static directory and ingress mode, with
    /// `DEVICE_APPROVAL` at its documented default (`strict`) and no known version.
    pub fn new(static_dir: impl Into<PathBuf>, ingress: Ingress) -> Self {
        Self {
            static_dir: static_dir.into(),
            ingress,
            bcrypt_cost: BcryptCost::DEFAULT,
            login_throttle: ThrottleConfig::default(),
            events: EventsConfig::default(),
            device_approval: ApprovalMode::Strict,
            controller_version: None,
        }
    }
}

/// The services the API uses.
pub struct ApiDeps {
    /// The database service.
    pub db: DbHandle,
    /// The setup token printed at startup, or `None` when a user already exists.
    pub bootstrap: Option<BootstrapToken>,
    /// The settings service: device and fleet configuration, and the wake-model
    /// registry.
    pub settings: SettingsHandle,
    /// The device runtimes: live device state and device commands.
    pub runtimes: Box<dyn DeviceRuntimes>,
    /// Runs uploaded wake models before they are accepted.
    pub qualifier: Box<dyn ModelQualifier>,
    /// Firmware delivery and the setup wizard's files.
    pub firmware: Box<dyn FirmwareControl>,
    /// The voice records: the turn and near-miss rows the activity routes read, the
    /// recordings beside the database, and near-miss verdicts.
    pub records: RecordsService,
    /// Wi-Fi, recording modes, the session recorder, the shell, alarms, sounds and
    /// Kiosk Satellite login.
    pub services: DeviceServices,
    /// Capture windows (playback-capture.md).
    pub capture: std::sync::Arc<dyn CaptureControl>,
    /// Transcript correction's tools, clip-window scoring and the runtime asset's hash.
    pub diagnostics: Diagnostics,
    /// Home Assistant's provisioning status. Every new value is also sent to the open
    /// dashboards as `ha_status`.
    pub ha_status: watch::Receiver<HaStatus>,
    /// Wall time for session timestamps.
    pub clock: Box<dyn WallClock>,
}

/// The HTTP API, ready to serve.
pub struct ApiServer {
    router: Router,
    publisher: EventPublisher,
    ha_status: watch::Receiver<HaStatus>,
    shutdown: CancellationToken,
}

impl ApiServer {
    /// Build the routes over these services.
    pub fn new(config: ApiConfig, deps: ApiDeps) -> Self {
        let shutdown = CancellationToken::new();
        let events = EventHub::new(config.events, shutdown.clone());
        let publisher = events.publisher();
        let ha_status = deps.ha_status.clone();
        let state = ApiState::new(Inner {
            db: deps.db,
            clock: deps.clock,
            passwords: Passwords::new(config.bcrypt_cost),
            throttle: Throttle::new(config.login_throttle),
            bootstrap: Mutex::new(deps.bootstrap),
            ingress: config.ingress,
            static_dir: config.static_dir.clone().into_boxed_path(),
            events,
            settings: deps.settings,
            runtimes: deps.runtimes,
            qualifier: deps.qualifier,
            firmware: deps.firmware,
            records: deps.records,
            services: deps.services,
            capture: deps.capture,
            diagnostics: deps.diagnostics,
            ha_status: deps.ha_status,
            approval_fallback: config.device_approval,
            controller_version: config.controller_version,
        });
        let router = Router::new()
            .merge(auth::routes::routes())
            .merge(events::routes())
            .merge(devices::routes())
            .merge(system::routes())
            .merge(wake_models::routes())
            .merge(firmware::routes())
            .merge(activity::routes())
            .merge(recordings::routes())
            .merge(device_services::routes())
            .merge(capture::routes())
            .merge(device_activity::routes())
            .merge(speech::routes())
            .merge(text_fix::routes())
            .merge(support::routes())
            .merge(releases::routes())
            .merge(pages::routes(&config.static_dir))
            .fallback(|| async { ApiError::not_found() })
            .method_not_allowed_fallback(|| async {
                ApiError::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    ErrorCode::MethodNotAllowed,
                    "This path does not take that method",
                )
            })
            .layer(middleware::from_fn_with_state(
                state.clone(),
                ingress::admit,
            ))
            .with_state(state);
        Self {
            router,
            publisher,
            ha_status,
            shutdown,
        }
    }

    /// Publishes to the open `/api/events` sockets. Keep it as long as needed; it works
    /// before, during and after serving.
    pub fn publisher(&self) -> EventPublisher {
        self.publisher.clone()
    }

    /// Serve on `listener` until `shutdown` completes. Then the events sockets close,
    /// new connections are refused, and requests in progress finish before this returns.
    /// While serving, each new Home Assistant status is published as `ha_status`.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> io::Result<()> {
        let sockets = self.shutdown.clone();
        // Dropping this future also closes the sockets and stops the forwarding.
        let _closes_sockets = self.shutdown.clone().drop_guard();
        tokio::spawn(forward_ha_status(
            self.ha_status,
            self.publisher,
            self.shutdown,
        ));
        let service = self
            .router
            .into_make_service_with_connect_info::<SocketAddr>();
        axum::serve(listener, service)
            .with_graceful_shutdown(async move {
                shutdown.await;
                sockets.cancel();
            })
            .await
    }
}

/// Publish every Home Assistant status after the current one, until shutdown or until
/// the integrator drops the sender.
async fn forward_ha_status(
    mut status: watch::Receiver<HaStatus>,
    publisher: EventPublisher,
    shutdown: CancellationToken,
) {
    status.mark_unchanged();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            changed = status.changed() => {
                if changed.is_err() {
                    return;
                }
                let features = status.borrow_and_update().features.clone();
                publisher.publish(&ApiEvent::HaStatus { features });
            }
        }
    }
}
