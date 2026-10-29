//! The running controller: every shared service, the device runtimes and the listeners,
//! started in order and stopped in the reverse order (spec §5.1, §5.7 step 6).
//!
//! Startup:
//! 1. open the database (refusing a database it cannot use) and start its service;
//! 2. load or create the device-link TLS credentials when the TLS listener is on;
//! 3. issue the dashboard setup token while no user exists;
//! 4. hash the runtime asset (`ORT_ANDROID_LIB`) once, off the shared runtime;
//! 5. load the wake-model registry and the device settings;
//! 6. start the Home Assistant session;
//! 7. advertise the controller over mDNS;
//! 8. start a runtime for every approved device;
//! 9. serve the dashboard API on `API_PORT`;
//! 10. listen for devices on `SERVER_PORT` (and `SERVER_TLS_PORT`).
//!
//! Shutdown stops admission, revokes every device's live work and stops the runtimes
//! (with their ESPHome nodes), then the listeners, discovery (sending goodbyes), the
//! API, the Home Assistant session and last the database service. Each step has its
//! own deadline, and a step that misses it is reported rather than waited for.

use std::{
    fmt,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use echomuse_alerts::{
    AlertConfig, AlertHandle, AlertService, ServiceFailure, SystemClock as AlertsClock,
};
use echomuse_api::{
    ApiConfig, ApiDeps, ApiServer, BootstrapToken, FeatureStatus, HaStatus as ApiHaStatus, Ingress,
    SystemWallClock,
};
use echomuse_config::{
    ControllerConfig, DeviceLinkTls, HomeAssistantAccess, TlsError,
    settings::UNKNOWN_CONTROLLER_VERSION, tls::ServerCertStatus,
};
use echomuse_db::{
    DatabaseService, DbHandle, OpenError, OpenedDatabase, ServiceConfig, Startup,
    devices::ApprovalMode,
};
use echomuse_discovery::{ControllerAdvertisement, DiscoveryHandle};
use echomuse_firmware::{
    BundledFirmware, DevicePayloads, FirmwareInfo, FirmwareService, FirmwareServiceConfig,
    FirmwareTimings,
};
use echomuse_ha::{HaClient, HaEndpoint, HaOptions, HaStatus, SessionEnd};
use echomuse_link::SystemClock as LinkSystemClock;
use echomuse_link::{LinkConfig, LinkServer};
use echomuse_protocol::Sha256;
use echomuse_settings::{
    registry::{SeedModel, WakeRegistry},
    service::{SettingsHandle, SettingsService},
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use echomuse_grammar::Grammar;
use echomuse_playback::Decoder;
use echomuse_records::{RecordingsLayout, RecordsService, RetentionLimits};
use echomuse_voice::speech::models::ModelSet;

use crate::{
    speech_models::{self, SpeechPaths},
    voice_session::VoiceServices,
};

/// Where the speech models come from.
#[derive(Clone)]
pub enum SpeechSource {
    /// The pinned bundle under `SPEECH_BUNDLE_DIR`, qualified in a child process.
    Bundle,
    /// Models given by the caller (tests use stand-ins).
    Provided(Arc<dyn ModelSet>),
}

impl fmt::Debug for SpeechSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Bundle => "Bundle",
            Self::Provided(_) => "Provided",
        })
    }
}

use crate::{
    admission::{ControllerAdmission, ControllerAssets, RuntimeAsset},
    alert_link::DeviceAlertLink,
    app::{ApprovedDevices, Controller, Registry},
    device::{DeviceRuntimeConfig, RuntimeServices},
    device_events::{DeviceEvent, DeviceEvents},
    dispatcher::{Dispatcher, publish_alert_notices},
    esphome_node::EsphomeServices,
    firmware_control::{ControllerFirmware, FirmwareAccess, FirmwareDashboard},
    runtimes::ControllerRuntimes,
    supervision::{Supervision, SupervisionOptions},
    wake_qualify::WakeModelQualifier,
};

/// How long each shutdown step may take before it is reported as stuck.
const STEP_DEADLINE: Duration = Duration::from_secs(5);
/// How long the database may take to finish queued writes.
const DATABASE_DEADLINE: Duration = Duration::from_secs(10);

/// What the controller starts from besides its configuration.
#[derive(Clone, Debug)]
pub struct ControlPlaneOptions {
    pub config: ControllerConfig,
    /// The compiled dashboard.
    pub static_dir: PathBuf,
    /// The deployed wake graph and its sidecar, which seed an empty registry.
    pub wake_seed_graph: PathBuf,
    pub wake_seed_sidecar: PathBuf,
    /// The binary that runs the qualification children (`qualify-model` and
    /// `qualify-wake-probes`): this executable in production.
    pub qualifier_program: PathBuf,
    /// Device runtime queue bounds and deadlines.
    pub runtime: DeviceRuntimeConfig,
    /// How long each firmware delivery step may take.
    pub firmware_timings: FirmwareTimings,
    /// Where the speech models come from.
    pub speech: SpeechSource,
    /// The ffmpeg that decodes what the devices play.
    pub ffmpeg: PathBuf,
    /// What a failure does: the fatal hook, the native-call watchdog's deadlines and the
    /// runtime restart backoff (§5.5).
    pub supervision: SupervisionOptions,
}

impl ControlPlaneOptions {
    /// The image's layout: the dashboard in `static` and the deployed wake model
    /// beside it, relative to the working directory (`/app` in the image), with this
    /// executable as the qualification child.
    pub fn new(config: ControllerConfig) -> Self {
        Self {
            config,
            static_dir: PathBuf::from("static"),
            wake_seed_graph: PathBuf::from("bcresnet_audio.onnx"),
            wake_seed_sidecar: PathBuf::from("bcresnet_audio.json"),
            qualifier_program: std::env::current_exe()
                .unwrap_or_else(|_| PathBuf::from("/proc/self/exe")),
            runtime: DeviceRuntimeConfig::default(),
            firmware_timings: FirmwareTimings::default(),
            speech: SpeechSource::Bundle,
            ffmpeg: PathBuf::from("ffmpeg"),
            supervision: SupervisionOptions::default(),
        }
    }
}

/// Why the controller did not start. Everything already started was stopped again.
#[derive(Debug)]
pub enum StartupError {
    Database(OpenError),
    DatabaseService(echomuse_db::StartError),
    Tls(TlsError),
    RuntimeAsset {
        path: PathBuf,
        source: std::io::Error,
    },
    WakeRegistry(String),
    Settings(String),
    HomeAssistant(String),
    AdvertisedAddress(String),
    Discovery(String),
    /// `SERVER_HOST` is not an IP address, which the ESPHome nodes need to bind.
    BindHost(String),
    DeviceList(String),
    RuntimeLimits(String),
    DeviceRuntime {
        device: String,
        error: String,
    },
    Api {
        address: String,
        source: std::io::Error,
    },
    SetupToken(String),
    Link(String),
    /// `FIRMWARE_DIR` does not hold firmware this controller can install.
    Firmware(String),
    /// The recordings beside the database could not be reconciled.
    Records(String),
    Grammar(String),
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(f, "the database cannot be used: {error}"),
            Self::DatabaseService(error) => {
                write!(f, "the database service did not start: {error}")
            }
            Self::Tls(error) => write!(f, "the device TLS credentials cannot be used: {error}"),
            Self::RuntimeAsset { path, source } => write!(
                f,
                "ORT_ANDROID_LIB {} cannot be read: {source}",
                path.display()
            ),
            Self::WakeRegistry(error) => {
                write!(f, "the wake-model registry cannot be used: {error}")
            }
            Self::Settings(error) => write!(f, "the device settings cannot be loaded: {error}"),
            Self::HomeAssistant(error) => {
                write!(f, "the Home Assistant session did not start: {error}")
            }
            Self::AdvertisedAddress(error) => write!(f, "{error}"),
            Self::Discovery(error) => write!(f, "mDNS advertising did not start: {error}"),
            Self::BindHost(host) => write!(
                f,
                "SERVER_HOST {host:?} is not an IP address; the ESPHome nodes cannot bind it"
            ),
            Self::DeviceList(error) => write!(f, "the approved devices cannot be read: {error}"),
            Self::RuntimeLimits(error) => {
                write!(f, "the device runtime limits are unusable: {error}")
            }
            Self::DeviceRuntime { device, error } => {
                write!(f, "the runtime of device {device} did not start: {error}")
            }
            Self::Api { address, source } => {
                write!(f, "the dashboard API cannot listen on {address}: {source}")
            }
            Self::SetupToken(error) => write!(f, "the setup token could not be issued: {error}"),
            Self::Link(error) => write!(f, "the device listeners did not start: {error}"),
            Self::Firmware(error) => write!(f, "the bundled firmware cannot be used: {error}"),
            Self::Records(error) => write!(f, "the voice records cannot be used: {error}"),
            Self::Grammar(error) => write!(f, "the command grammar did not compile: {error}"),
        }
    }
}

impl std::error::Error for StartupError {}

/// Shutdown steps that did not finish cleanly.
#[derive(Debug, Default)]
pub struct ShutdownReport {
    pub problems: Vec<String>,
}

impl ShutdownReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// What the controller found and did while starting, for its log.
#[derive(Clone, Debug)]
pub struct StartupNotes {
    pub database: Startup,
    pub tls: Option<TlsNotes>,
    pub runtime_asset: Sha256,
    pub advertised: ControllerAdvertisement,
    pub approved_devices: usize,
    /// Why wake-model uploads are refused; `None` when they are qualified.
    pub wake_uploads_refused: Option<String>,
    /// The bundled firmware every update installs.
    pub firmware: FirmwareInfo,
}

#[derive(Clone, Debug)]
pub struct TlsNotes {
    pub created_ca: bool,
    pub server_cert: ServerCertStatus,
}

/// The running controller.
pub struct ControlPlane {
    db_service: DatabaseService,
    db: DbHandle,
    setup_token: Option<String>,
    settings_service: SettingsService,
    settings: SettingsHandle,
    ha: HaClient,
    ha_task: JoinHandle<SessionEnd>,
    discovery: DiscoveryHandle,
    discovery_task: JoinHandle<()>,
    registry: Registry,
    registry_task: JoinHandle<()>,
    alerts: AlertHandle,
    alerts_task: JoinHandle<Result<(), ServiceFailure>>,
    dispatcher_task: JoinHandle<()>,
    api_address: SocketAddr,
    api_stop: CancellationToken,
    api_task: JoinHandle<std::io::Result<()>>,
    link: LinkServer,
    admission_closing: CancellationToken,
    notes: StartupNotes,
    records: RecordsService,
}

/// What has started so far, so a failed start can stop it again.
#[derive(Default)]
struct Partial {
    db_service: Option<DatabaseService>,
    settings: Option<SettingsService>,
    ha: Option<(HaClient, JoinHandle<SessionEnd>)>,
    discovery: Option<(DiscoveryHandle, JoinHandle<()>)>,
    controller: Option<Controller>,
    registry: Option<(Registry, JoinHandle<()>)>,
    api: Option<(CancellationToken, JoinHandle<std::io::Result<()>>)>,
    alerts: Option<(AlertHandle, JoinHandle<Result<(), ServiceFailure>>)>,
    dispatcher: Option<JoinHandle<()>>,
}

impl Partial {
    async fn stop(self) {
        if let Some(controller) = self.controller
            && let Err(error) = controller.shutdown().await
        {
            tracing::error!(%error, "device runtimes did not stop cleanly");
        }
        if let Some((registry, task)) = self.registry {
            let _ = tokio::time::timeout(STEP_DEADLINE, registry.shutdown()).await;
            let _ = tokio::time::timeout(STEP_DEADLINE, task).await;
        }
        if let Some((alerts, task)) = self.alerts {
            alerts.shutdown();
            let _ = tokio::time::timeout(STEP_DEADLINE, task).await;
        }
        if let Some(task) = self.dispatcher {
            task.abort();
        }
        if let Some((stop, task)) = self.api {
            stop.cancel();
            let _ = tokio::time::timeout(STEP_DEADLINE, task).await;
        }
        if let Some((discovery, task)) = self.discovery {
            let _ = discovery.shutdown(STEP_DEADLINE).await;
            let _ = tokio::time::timeout(STEP_DEADLINE, task).await;
        }
        if let Some((ha, task)) = self.ha {
            ha.stop();
            let _ = tokio::time::timeout(STEP_DEADLINE, task).await;
        }
        if let Some(settings) = self.settings {
            let _ = tokio::time::timeout(STEP_DEADLINE, settings.shutdown()).await;
        }
        if let Some(service) = self.db_service {
            let _ = service.shutdown(DATABASE_DEADLINE).await;
        }
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| std::panic::resume_unwind(error.into_panic()))
}

impl ControlPlane {
    /// Starts everything, or stops what already started and reports why it could not.
    pub async fn start(options: ControlPlaneOptions) -> Result<Self, StartupError> {
        let mut partial = Partial::default();
        match Self::start_inner(options, &mut partial).await {
            Ok(plane) => Ok(plane),
            Err(error) => {
                partial.stop().await;
                Err(error)
            }
        }
    }

    async fn start_inner(
        options: ControlPlaneOptions,
        partial: &mut Partial,
    ) -> Result<Self, StartupError> {
        let config = options.config;
        // First, so every service task started below is observed (§5.5).
        let supervision = Supervision::start(options.supervision)
            .map_err(|error| StartupError::RuntimeLimits(error.to_string()))?;
        let tasks = supervision.tasks.clone();
        let watchdog = supervision.watchdog.clone();
        let db_path = config.paths.db_path.clone();
        let data_dir = data_dir(&db_path);

        // A fresh install records the deployment's DEVICE_APPROVAL choice; an existing
        // database keeps the setting it already holds.
        let initial_approval = match config.device_approval_fallback {
            echomuse_config::DeviceApproval::Strict => echomuse_db::devices::ApprovalMode::Strict,
            echomuse_config::DeviceApproval::Auto => echomuse_db::devices::ApprovalMode::Auto,
        };
        let opened =
            blocking(move || OpenedDatabase::open_with_initial_approval(db_path, initial_approval))
                .await
                .map_err(StartupError::Database)?;
        let database_startup = opened.startup().clone();
        let db_service = DatabaseService::start(opened, ServiceConfig::default())
            .map_err(StartupError::DatabaseService)?;
        let db = db_service.handle();
        partial.db_service = Some(db_service);

        let tls = match config.network.server_tls_port {
            None => None,
            Some(_) => {
                let dir = config.paths.tls_dir.clone();
                Some(
                    blocking(move || DeviceLinkTls::load_or_create(&dir))
                        .await
                        .map_err(StartupError::Tls)?,
                )
            }
        };

        let setup_token = BootstrapToken::issue_if_needed(&db)
            .await
            .map_err(|error| StartupError::SetupToken(format!("{error:?}")))?;
        let setup_text = setup_token.as_ref().map(|t| t.expose().to_owned());

        let runtime_path = config.paths.ort_android_lib.clone();
        let runtime_asset = {
            let path = runtime_path.clone();
            blocking(move || std::fs::read(&path).map(|bytes| Sha256::of(&bytes)))
                .await
                .map_err(|source| StartupError::RuntimeAsset {
                    path: runtime_path.clone(),
                    source,
                })?
        };

        let firmware_dir = config.paths.firmware_dir.clone();
        let bundled = blocking(move || BundledFirmware::load(firmware_dir))
            .await
            .map_err(|error| {
                StartupError::Firmware(format!(
                    "FIRMWARE_DIR {}: {error}",
                    config.paths.firmware_dir.display()
                ))
            })?;
        let firmware_info = bundled.info().clone();

        let models_dir = data_dir.join("oww_models");
        let seed = SeedModel::deployed(options.wake_seed_graph, options.wake_seed_sidecar);
        let registry = blocking(move || WakeRegistry::open_or_seed(models_dir, &seed))
            .await
            .map_err(|error| StartupError::WakeRegistry(error.to_string()))?;
        let settings_service = SettingsService::start(db.clone(), registry)
            .await
            .map_err(|error| StartupError::Settings(error.to_string()))?;
        let settings = settings_service.handle();
        partial.settings = Some(settings_service);

        let endpoint = match &config.home_assistant {
            HomeAssistantAccess::Direct { base_url, token } => {
                HaEndpoint::direct(&base_url.as_http(), token)
            }
            HomeAssistantAccess::Supervisor { token } => HaEndpoint::supervisor(token),
        }
        .map_err(|error| StartupError::HomeAssistant(error.to_string()))?;
        let (ha, ha_task) = HaClient::start(endpoint, HaOptions::default())
            .map_err(|error| StartupError::HomeAssistant(error.to_string()))?;
        let ha_task = tasks.observe("Home Assistant session", ha_task);
        partial.ha = Some((ha.clone(), ha_task));

        let network = config.network.clone();
        let address = blocking(move || network.advertised_address())
            .await
            .map_err(|error| StartupError::AdvertisedAddress(error.to_string()))?;
        if address.source == echomuse_config::AddressSource::DefaultRoute {
            tracing::warn!(
                ip = %address.ip,
                "SERVER_IP is not set; advertising the default route's address"
            );
        }
        let advertised = ControllerAdvertisement {
            name: config.network.mdns_name.clone(),
            address: address.ip,
            port: config.network.server_port,
            tls_port: tls.as_ref().and(config.network.server_tls_port),
        };
        let (discovery, discovery_task) = echomuse_discovery::start(advertised.clone())
            .map_err(|error| StartupError::Discovery(error.to_string()))?;
        let discovery_task = tasks.observe("mDNS discovery", discovery_task);
        partial.discovery = Some((discovery.clone(), discovery_task));

        let bind_host: IpAddr = config
            .network
            .server_host
            .parse()
            .map_err(|_| StartupError::BindHost(config.network.server_host.clone()))?;
        let esphome = Arc::new(EsphomeServices {
            db: db.clone(),
            discovery: discovery.clone(),
            bind_host,
            project_version: config.esphome_project_version.clone(),
        });
        // The voice records, and the device services that hear each runtime's session
        // traffic: device logs, Wi-Fi, the recording modes and the session recorder.
        let records = RecordsService::new(
            db.clone(),
            RecordingsLayout::beside_database(&config.paths.db_path),
            RetentionLimits::default(),
        );
        let services_publisher: Arc<OnceLock<echomuse_api::EventPublisher>> =
            Arc::new(OnceLock::new());
        let wifi = Arc::new(crate::wifi::WifiService::new(services_publisher.clone()));
        let recording = crate::recording_modes::RecordingHub::load(
            db.clone(),
            records.layout(),
            services_publisher.clone(),
            tasks.clone(),
        )
        .await
        .map_err(StartupError::Records)?;
        let recorder = {
            let directory = data_dir.join("session_recordings");
            let version = config.controller_version.clone();
            let tasks = tasks.clone();
            blocking(move || crate::recorder_link::RecorderHub::open(directory, version, &tasks))
                .await
                .map_err(|error| StartupError::Records(format!("session recordings: {error}")))?
        };
        let logs = crate::device_logs::LogSink::start(records.clone(), &tasks);
        let capture = crate::capture::CaptureHub::new(
            logs.clone(),
            services_publisher.clone(),
            tasks.clone(),
        )
        .map_err(|error| StartupError::Records(format!("capture: {error}")))?;
        let device_taps = crate::taps::DeviceTaps {
            logs,
            wifi: wifi.clone(),
            recording: recording.clone(),
            recorder: recorder.clone(),
            capture: capture.clone(),
        };
        let (events, events_rx) = DeviceEvents::channel();
        let mut controller = Controller::with_services(
            options.runtime,
            RuntimeServices {
                esphome: Some(esphome),
                events: Some(events.clone()),
                voice: None,
                supervision: Some(supervision),
                taps: Some(device_taps.clone()),
            },
        )
        .map_err(|error| StartupError::RuntimeLimits(error.to_string()))?;
        let approved = db
            .read(ApprovedDevices)
            .await
            .map_err(|error| StartupError::DeviceList(error.to_string()));
        let approved = match approved {
            Ok(approved) => approved,
            Err(error) => {
                partial.controller = Some(controller);
                return Err(error);
            }
        };
        let directory = controller.directory();
        wifi.set_directory(directory.clone());
        recording.set_directory(directory.clone());
        capture.set_directory(directory.clone());

        // Alerts: every approved device is an endpoint; the dispatcher registers them.
        let (alerts, alerts_task) = AlertService::start(
            AlertConfig::default(),
            db.clone(),
            Arc::new(ha.clone()),
            Arc::new(DeviceAlertLink::new(directory.clone())),
            Arc::new(AlertsClock),
        );
        // The service ends with an error when one of its own tasks panicked; that stops
        // the process at once, not at the next shutdown.
        let alerts_task = tasks.observe_fallible("alert service", alerts_task);
        partial.alerts = Some((alerts.clone(), alerts_task));

        // The voice path: records, speech models, the grammar and the decoder. Models
        // load (and qualify) in the background; until they are ready every device's
        // session refuses wakes and says why (§4.1).
        match records.start_up().await {
            Ok(report) => tracing::info!(?report, "records reconciled"),
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::Records(error.to_string()));
            }
        }
        let speech = match options.speech {
            SpeechSource::Bundle => speech_models::start_loading(
                SpeechPaths::from_config(&config.paths),
                options.qualifier_program.clone(),
                &tasks,
            ),
            SpeechSource::Provided(models) => speech_models::provided(models),
        };
        let grammar = match blocking(Grammar::new).await {
            Ok(grammar) => grammar,
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::Grammar(error.to_string()));
            }
        };
        let ha_base = match &config.home_assistant {
            HomeAssistantAccess::Direct { base_url, .. } => base_url.as_http(),
            HomeAssistantAccess::Supervisor { .. } => "http://supervisor/core".to_owned(),
        };
        let ha_base = match reqwest::Url::parse(&ha_base) {
            Ok(url) => url,
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::HomeAssistant(format!("{ha_base}: {error}")));
            }
        };
        let decoder = match media_client() {
            Ok(client) => Decoder::new(
                tokio::runtime::Handle::current(),
                client,
                options.ffmpeg.clone(),
            ),
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::HomeAssistant(error));
            }
        };
        // Transcript correction's tables, built off every device thread (§4.7, §5.4).
        let correction = match crate::correction::CorrectionTables::start(
            ha.clone(),
            grammar.clone(),
            db.clone(),
            records.subscribe_turns(),
            &controller.tasks(),
        ) {
            Ok(correction) => correction,
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::Grammar(format!(
                    "the correction table builder did not start: {error}"
                )));
            }
        };
        // The dashboard's correction tools and clip-window scoring (§4.7, §4.11).
        let clip_scorer = match crate::clip_windows::ClipScorer::start(
            db.clone(),
            records.clone(),
            settings.clone(),
            speech.clone(),
            watchdog,
            &tasks,
        ) {
            Ok(scorer) => scorer,
            Err(error) => {
                partial.controller = Some(controller);
                return Err(StartupError::Records(format!(
                    "the clip scoring thread did not start: {error}"
                )));
            }
        };
        let diagnostics = echomuse_api::Diagnostics {
            text_fix: Arc::new(crate::text_fix::DashboardTextFix {
                ha: ha.clone(),
                grammar: grammar.clone(),
                settings: settings.clone(),
                correction: correction.clone(),
            }),
            clip_scoring: Arc::new(clip_scorer),
            runtime_asset: Some(runtime_asset.clone()),
        };
        controller.set_voice_services(Arc::new(VoiceServices {
            ha: ha.clone(),
            ha_base,
            alerts: alerts.clone(),
            records: records.clone(),
            settings: settings.clone(),
            grammar,
            arbiter: controller.arbiter(),
            speech: speech.clone(),
            decoder,
            correction,
        }));
        for device in &approved {
            if let Err(error) = controller.add_device(device.clone()).await {
                partial.controller = Some(controller);
                return Err(StartupError::DeviceRuntime {
                    device: device.0.clone(),
                    error: error.to_string(),
                });
            }
        }
        for device in &approved {
            events.send(DeviceEvent::Registered(device.clone())).await;
        }

        // Firmware delivery.
        let (access, shells_slot) = FirmwareAccess::new(directory.clone());
        let (firmware_events, firmware_publisher, update_states) = FirmwareDashboard::new();
        let service_directory = directory.clone();
        let service_updates = update_states.clone();
        let sound_service = {
            let sounds_dir = echomuse_presentation::sounds::SoundsDir::beside_database(&data_dir);
            let ffmpeg = options.ffmpeg.clone();
            blocking(move || {
                echomuse_presentation::sounds::service::SoundService::start(sounds_dir, ffmpeg)
            })
            .await
            .map_err(|error| StartupError::Records(format!("the sound catalog: {error}")))?
        };
        let kiosk_http = media_client().map_err(StartupError::HomeAssistant)?;
        let controller_version = Some(config.controller_version.clone())
            .filter(|version| version != UNKNOWN_CONTROLLER_VERSION);
        let firmware = FirmwareService::new(
            access,
            db.clone(),
            firmware_events,
            FirmwareServiceConfig {
                bundled,
                payloads: DevicePayloads::new(config.paths.device_payloads_dir.clone()),
                link_ca_pem: tls.as_ref().map(|tls| tls.ca_pem().to_owned()),
                controller_version: controller_version.clone(),
                timings: options.firmware_timings,
                clock: Arc::new(LinkSystemClock),
            },
        );

        let (registry, registry_task, publisher_slot) = Registry::start(controller, db.clone());
        partial.registry = Some((registry.clone(), registry_task));
        partial.dispatcher = Some(
            tasks.spawn(
                "device event dispatcher",
                Dispatcher {
                    alerts: alerts.clone(),
                    firmware: firmware.clone(),
                    db: db.clone(),
                    settings: settings.clone(),
                    sounds: sound_service.handle(),
                    directory: directory.clone(),
                    tasks: tasks.clone(),
                }
                .run(events_rx),
            ),
        );

        let qualifier = WakeModelQualifier::prepare(
            config.paths.onnxruntime_lib.clone(),
            options.qualifier_program,
            data_dir.join("oww_models"),
        )
        .await;
        let wake_uploads_refused = qualifier.unavailable().map(str::to_owned);
        let (ha_status, ha_status_rx) = watch::channel(ApiHaStatus::default());
        drop(tasks.spawn(
            "Home Assistant status",
            publish_ha_status(ha.status(), ha_status, calendar_url(&config.home_assistant)),
        ));
        let mut api_config = ApiConfig::new(
            options.static_dir,
            Ingress::from_setting(config.ingress_only),
        );
        api_config.device_approval = match config.device_approval_fallback {
            echomuse_config::DeviceApproval::Strict => ApprovalMode::Strict,
            echomuse_config::DeviceApproval::Auto => ApprovalMode::Auto,
        };
        api_config.controller_version = controller_version;
        let api = ApiServer::new(
            api_config,
            ApiDeps {
                db: db.clone(),
                bootstrap: setup_token,
                settings: settings.clone(),
                runtimes: ControllerRuntimes::new(
                    registry.clone(),
                    db.clone(),
                    discovery.clone(),
                    data_dir.clone(),
                    events,
                    device_taps,
                ),
                qualifier,
                firmware: ControllerFirmware::new(
                    firmware,
                    directory,
                    db.clone(),
                    update_states,
                    tasks.clone(),
                ),
                records: records.clone(),
                services: echomuse_api::DeviceServices {
                    wifi: Arc::new(crate::wifi::WifiRoutes(wifi)),
                    recording: Arc::new(crate::recording_modes::RecordingRoutes(recording)),
                    recorder: Arc::new(crate::recorder_link::RecorderRoutes(recorder)),
                    shell: Arc::new(crate::shell_bridge::ShellBridge {
                        directory: service_directory.clone(),
                        shells: shells_slot.clone(),
                        firmware: service_updates,
                        tasks: tasks.clone(),
                    }),
                    alerts: Arc::new(crate::alerts_dashboard::DashboardAlerts {
                        alerts: alerts.clone(),
                        db: db.clone(),
                    }),
                    sounds: Arc::new(crate::sounds::DashboardSounds {
                        sounds: sound_service.handle(),
                        service: Arc::new(sound_service),
                        settings: settings.clone(),
                        directory: service_directory.clone(),
                    }),
                    kiosk: Arc::new(crate::kiosk::KioskSatellite {
                        db: db.clone(),
                        settings: settings.clone(),
                        directory: service_directory,
                        http: kiosk_http,
                    }),
                },
                capture,
                diagnostics,
                ha_status: ha_status_rx,
                clock: SystemWallClock,
            },
        );
        let publisher = api.publisher();
        let _ = firmware_publisher.set(publisher.clone());
        let _ = services_publisher.set(publisher.clone());
        drop(tasks.spawn(
            "alert notices",
            publish_alert_notices(alerts, publisher.clone()),
        ));
        drop(tasks.spawn(
            "turn notices",
            crate::dashboard::publish_turns_stored(records.subscribe_turns(), publisher.clone()),
        ));
        // The registry serves once it can publish device changes.
        let _ = publisher_slot.send(publisher);

        let api_bind = (
            config.network.server_host.as_str(),
            config.network.api_port.get(),
        );
        let listener = TcpListener::bind(api_bind)
            .await
            .map_err(|source| StartupError::Api {
                address: format!("{}:{}", api_bind.0, api_bind.1),
                source,
            })?;
        let api_address = listener.local_addr().map_err(|source| StartupError::Api {
            address: format!("{}:{}", api_bind.0, api_bind.1),
            source,
        })?;
        let api_stop = CancellationToken::new();
        let stopped = api_stop.clone();
        let api_task = tasks.spawn_fallible(
            "dashboard API",
            api.serve(listener, async move { stopped.cancelled().await }),
        );
        partial.api = Some((api_stop.clone(), api_task));

        let admission_closing = CancellationToken::new();
        let link_config = LinkConfig::from_controller(&config, tls.as_ref())
            .map_err(|error| StartupError::Link(error.to_string()))?;
        let link = LinkServer::bind(
            link_config,
            db.clone(),
            ControllerAdmission::new(
                registry.clone(),
                settings.clone(),
                runtime_asset.clone(),
                admission_closing.clone(),
                speech,
            ),
            ControllerAssets::new(
                RuntimeAsset {
                    path: runtime_path,
                    sha256: runtime_asset.clone(),
                },
                settings.clone(),
                data_dir.join("sounds").join("assets"),
            ),
        )
        .await
        .map_err(|error| StartupError::Link(error.to_string()))?;

        // The firmware service reaches device shells through the link server's broker.
        let _ = shells_slot.set(link.shells());

        let (ha, ha_task) = partial.ha.take().expect("started above");
        let (discovery, discovery_task) = partial.discovery.take().expect("started above");
        let (registry, registry_task) = partial.registry.take().expect("started above");
        let (api_stop, api_task) = partial.api.take().expect("started above");
        let (alerts, alerts_task) = partial.alerts.take().expect("started above");
        let dispatcher_task = partial.dispatcher.take().expect("started above");
        let db_service = partial.db_service.take().expect("started above");
        let settings_service = partial.settings.take().expect("started above");
        Ok(Self {
            db_service,
            db,
            setup_token: setup_text,
            settings_service,
            settings,
            ha,
            ha_task,
            discovery,
            discovery_task,
            registry,
            registry_task,
            alerts,
            alerts_task,
            dispatcher_task,
            api_address,
            api_stop,
            api_task,
            link,
            admission_closing,
            records,
            notes: StartupNotes {
                database: database_startup,
                tls: tls.as_ref().map(|tls| TlsNotes {
                    created_ca: tls.created_ca(),
                    server_cert: tls.server_cert().clone(),
                }),
                runtime_asset,
                advertised,
                approved_devices: approved.len(),
                wake_uploads_refused,
                firmware: firmware_info,
            },
        })
    }

    /// The setup token to print while no user exists.
    pub fn setup_token(&self) -> Option<&str> {
        self.setup_token.as_deref()
    }

    pub fn notes(&self) -> &StartupNotes {
        &self.notes
    }

    pub fn api_address(&self) -> SocketAddr {
        self.api_address
    }

    pub fn device_address(&self) -> SocketAddr {
        self.link.plain_addr()
    }

    pub fn device_tls_address(&self) -> Option<SocketAddr> {
        self.link.tls_addr()
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn home_assistant(&self) -> watch::Receiver<HaStatus> {
        self.ha.status()
    }

    /// Stops everything in order, each step under its own deadline.
    pub async fn shutdown(self) -> ShutdownReport {
        let mut report = ShutdownReport::default();
        let mut problem = |step: &str, detail: String| {
            tracing::error!(step, %detail, "shutdown step did not finish cleanly");
            report.problems.push(format!("{step}: {detail}"));
        };

        // Stop admission, then revoke every device's live work and stop the runtimes,
        // which closes their link sessions and stops their ESPHome nodes.
        self.admission_closing.cancel();
        let runtime_deadline = STEP_DEADLINE * 6;
        match tokio::time::timeout(runtime_deadline, self.registry.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => problem("device runtimes", error.to_string()),
            Err(_) => problem("device runtimes", "deadline passed".to_owned()),
        }
        if tokio::time::timeout(STEP_DEADLINE, self.registry_task)
            .await
            .is_err()
        {
            problem("device registry", "deadline passed".to_owned());
        }

        if tokio::time::timeout(STEP_DEADLINE, self.link.shutdown())
            .await
            .is_err()
        {
            problem("device listeners", "deadline passed".to_owned());
        }

        // The alert service, then the dispatcher that fed it.
        self.alerts.shutdown();
        match tokio::time::timeout(STEP_DEADLINE, self.alerts_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(failure))) => problem("alerts", failure.to_string()),
            Ok(Err(error)) => problem("alerts", error.to_string()),
            Err(_) => problem("alerts", "deadline passed".to_owned()),
        }
        self.dispatcher_task.abort();

        if let Err(error) = self.discovery.shutdown(STEP_DEADLINE).await {
            problem("discovery", error.to_string());
        }
        if tokio::time::timeout(STEP_DEADLINE, self.discovery_task)
            .await
            .is_err()
        {
            problem("discovery", "the service task did not end".to_owned());
        }

        self.api_stop.cancel();
        match tokio::time::timeout(STEP_DEADLINE, self.api_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => problem("dashboard API", error.to_string()),
            Ok(Err(error)) => problem("dashboard API", error.to_string()),
            Err(_) => problem("dashboard API", "deadline passed".to_owned()),
        }

        self.ha.stop();
        match tokio::time::timeout(STEP_DEADLINE, self.ha_task).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => problem("Home Assistant", error.to_string()),
            Err(_) => problem("Home Assistant", "deadline passed".to_owned()),
        }

        match tokio::time::timeout(STEP_DEADLINE, self.settings_service.shutdown()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => problem("settings", error.to_string()),
            Err(_) => problem("settings", "deadline passed".to_owned()),
        }
        self.records.shutdown().await;
        drop(self.settings);
        drop(self.db);
        if let Err(error) = self.db_service.shutdown(DATABASE_DEADLINE).await {
            problem("database", error.to_string());
        }
        report
    }
}

/// The directory the database lives in, where the files beside it are kept.
/// The HTTP client the media decoder fetches with, trusting the system's roots (HA may
/// serve TTS over HTTPS), with the crypto provider named explicitly.
fn media_client() -> Result<reqwest::Client, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| format!("the media client's TLS: {error}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .build()
        .map_err(|error| format!("the media client: {error}"))
}

fn data_dir(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
        _ => PathBuf::from("."),
    }
}

/// Home Assistant's calendar editor, which the alert panel links to: the `calendar`
/// panel under `HA_URL`. Unknown in the add-on, where the dashboard is reached through
/// Home Assistant's own origin and no base URL is configured.
fn calendar_url(access: &HomeAssistantAccess) -> Option<String> {
    access
        .configured_url()
        .map(|base| format!("{}/calendar", base.trim_end_matches('/')))
}

/// Publishes the Home Assistant session's state as the API's status, and logs each
/// change. The provisioning steps of §3.3.3 (voice, calendar, scripts, vocabulary,
/// timers, recognizer) are not probed by this controller yet, so the only feature
/// reported is the `connection` itself.
async fn publish_ha_status(
    mut status: watch::Receiver<HaStatus>,
    api: watch::Sender<ApiHaStatus>,
    calendar_url: Option<String>,
) {
    loop {
        let current = status.borrow_and_update().clone();
        match (&current.generation, &current.last_fault) {
            (Some(_), _) => tracing::info!(
                version = current.ha_version.as_deref().unwrap_or("unknown"),
                "connected to Home Assistant"
            ),
            (None, Some(fault)) => tracing::warn!(%fault, "Home Assistant is not connected"),
            (None, None) => {}
        }
        let connection = FeatureStatus {
            ok: current.generation.is_some(),
            detail: match (&current.generation, &current.last_fault) {
                (Some(_), _) => None,
                (None, Some(fault)) => Some(fault.to_string()),
                (None, None) => Some("not connected yet".to_owned()),
            },
        };
        api.send_replace(ApiHaStatus {
            features: [("connection".to_owned(), connection)]
                .into_iter()
                .collect(),
            calendar_url: calendar_url.clone(),
        });
        if status.changed().await.is_err() {
            return;
        }
    }
}
