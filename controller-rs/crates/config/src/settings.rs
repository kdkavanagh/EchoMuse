//! The controller's settings, read from an explicit environment map.
//!
//! The library never reads the process environment itself. The binary collects
//! the variables (after [`crate::addon`] has merged the add-on options into
//! them) and hands them to [`ControllerConfig::from_env`].
//!
//! Every variable follows one rule for absence: a variable that is missing, or
//! whose value is empty after trimming surrounding whitespace, is unset and
//! takes its default. A variable that is set but cannot be parsed is an error
//! naming that variable; it never silently falls back to the default.

use std::collections::BTreeMap;
use std::fmt;
use std::net::Ipv4Addr;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};

use thiserror::Error;

/// The environment the configuration is built from: variable name to value.
pub type Environment = BTreeMap<String, String>;

/// Every environment variable the controller reads (spec §3.6).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EnvVar {
    /// `SERVER_HOST`: the address every listener binds to.
    ServerHost,
    /// `SERVER_PORT`: the plain WebSocket device listener.
    ServerPort,
    /// `SERVER_TLS_PORT`: the TLS device listener; `0` disables it.
    ServerTlsPort,
    /// `API_PORT`: the dashboard and HTTP API.
    ApiPort,
    /// `SERVER_IP`: the IPv4 address advertised over mDNS.
    ServerIp,
    /// `MDNS_NAME`: the mDNS instance and host label.
    MdnsName,
    /// `REQUIRE_DEVICE_TLS`: `1` refuses plain or tokenless device sockets.
    RequireDeviceTls,
    /// `HA_URL`: Home Assistant's base URL.
    HaUrl,
    /// `HA_TOKEN`: an administrator's long-lived access token.
    HaToken,
    /// `SUPERVISOR_TOKEN`: the token the Supervisor injects into the add-on.
    SupervisorToken,
    /// `DB_PATH`: the SQLite database.
    DbPath,
    /// `TLS_DIR`: the device-link CA and server certificate.
    TlsDir,
    /// `FIRMWARE_DIR`: the bundled device firmware (`server` and `version`).
    FirmwareDir,
    /// `SPEECH_BUNDLE_DIR`: the verified speech model bundle.
    SpeechBundleDir,
    /// `ORT_ANDROID_LIB`: the Android ONNX Runtime library served to Dots.
    OrtAndroidLib,
    /// `ONNXRUNTIME_LIB`: the controller's own ONNX Runtime library, which runs the wake,
    /// voice-activity and noise models on this host.
    OnnxRuntimeLib,
    /// `NS_MODEL_DIR`: the DTLN noise suppression models.
    NsModelDir,
    /// `DEVICE_PAYLOADS_DIR`: `start_server.sh` and the Echo Show package lists that
    /// firmware delivery and the setup wizard install on devices.
    DevicePayloadsDir,
    /// `ESPHOME_PROJECT_VERSION`: the project version each ESPHome satellite reports.
    EsphomeProjectVersion,
    /// `ECHOMUSE_HOME_ASSISTANT_INGRESS`: `true` when running behind add-on ingress.
    HomeAssistantIngress,
    /// `DEVICE_APPROVAL`: the approval policy used when the database has none.
    DeviceApproval,
    /// `EM_CONTROLLER_VERSION`: the controller version, set by the published image.
    ControllerVersion,
}

impl EnvVar {
    /// Every variable, in the order of spec §3.6.
    pub const ALL: [Self; 22] = [
        Self::ServerHost,
        Self::ServerPort,
        Self::ServerTlsPort,
        Self::ApiPort,
        Self::ServerIp,
        Self::MdnsName,
        Self::RequireDeviceTls,
        Self::HaUrl,
        Self::HaToken,
        Self::SupervisorToken,
        Self::DbPath,
        Self::TlsDir,
        Self::FirmwareDir,
        Self::SpeechBundleDir,
        Self::OrtAndroidLib,
        Self::OnnxRuntimeLib,
        Self::NsModelDir,
        Self::DevicePayloadsDir,
        Self::EsphomeProjectVersion,
        Self::HomeAssistantIngress,
        Self::DeviceApproval,
        Self::ControllerVersion,
    ];

    /// The variable's name in the environment.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ServerHost => "SERVER_HOST",
            Self::ServerPort => "SERVER_PORT",
            Self::ServerTlsPort => "SERVER_TLS_PORT",
            Self::ApiPort => "API_PORT",
            Self::ServerIp => "SERVER_IP",
            Self::MdnsName => "MDNS_NAME",
            Self::RequireDeviceTls => "REQUIRE_DEVICE_TLS",
            Self::HaUrl => "HA_URL",
            Self::HaToken => "HA_TOKEN",
            Self::SupervisorToken => "SUPERVISOR_TOKEN",
            Self::DbPath => "DB_PATH",
            Self::TlsDir => "TLS_DIR",
            Self::FirmwareDir => "FIRMWARE_DIR",
            Self::SpeechBundleDir => "SPEECH_BUNDLE_DIR",
            Self::OrtAndroidLib => "ORT_ANDROID_LIB",
            Self::OnnxRuntimeLib => "ONNXRUNTIME_LIB",
            Self::NsModelDir => "NS_MODEL_DIR",
            Self::DevicePayloadsDir => "DEVICE_PAYLOADS_DIR",
            Self::EsphomeProjectVersion => "ESPHOME_PROJECT_VERSION",
            Self::HomeAssistantIngress => "ECHOMUSE_HOME_ASSISTANT_INGRESS",
            Self::DeviceApproval => "DEVICE_APPROVAL",
            Self::ControllerVersion => "EM_CONTROLLER_VERSION",
        }
    }

    /// The value of this variable in `env`, trimmed; `None` when it is
    /// missing or empty.
    pub fn read(self, env: &Environment) -> Option<&str> {
        env.get(self.name())
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
    }
}

impl fmt::Display for EnvVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Default plain device listener port.
pub const DEFAULT_SERVER_PORT: u16 = 8767;
/// Default TLS device listener port.
pub const DEFAULT_SERVER_TLS_PORT: u16 = 8770;
/// Default dashboard and API port.
pub const DEFAULT_API_PORT: u16 = 8768;
/// Default bind address for every listener.
pub const DEFAULT_SERVER_HOST: &str = "0.0.0.0";
/// Default mDNS instance and host label.
pub const DEFAULT_MDNS_NAME: &str = "echomuse";
/// Default database path, relative to the working directory.
pub const DEFAULT_DB_PATH: &str = "echomuse.db";
/// Default bundled firmware directory inside the image.
pub const DEFAULT_FIRMWARE_DIR: &str = "/app/firmware";
/// Default speech bundle directory inside the image.
pub const DEFAULT_SPEECH_BUNDLE_DIR: &str = "/app/speech";
/// Default Android ONNX Runtime library inside the image.
pub const DEFAULT_ORT_ANDROID_LIB: &str = "/app/models/ort_android/libonnxruntime.so";
/// Default host ONNX Runtime library inside the image (the pinned 1.20.1 build).
pub const DEFAULT_ONNXRUNTIME_LIB: &str = "/app/models/onnxruntime/libonnxruntime.so.1.20.1";
/// Default DTLN model directory inside the image.
pub const DEFAULT_NS_MODEL_DIR: &str = "/app/models/dtln";
/// Default device payload directory inside the image.
pub const DEFAULT_DEVICE_PAYLOADS_DIR: &str = "/app/device_payloads";
/// The controller version when neither the environment nor the build names one.
pub const UNKNOWN_CONTROLLER_VERSION: &str = "dev";
/// The name of the TLS directory beside the database when `TLS_DIR` is unset.
pub const TLS_DIR_NAME: &str = "tls";

/// Home Assistant's WebSocket endpoint inside an add-on.
pub const SUPERVISOR_WEBSOCKET_URL: &str = "ws://supervisor/core/websocket";
/// Home Assistant's base URL inside an add-on; REST paths go under `/api`.
pub const SUPERVISOR_BASE_URL: &str = "http://supervisor/core";

/// A validated mDNS label: 1 to 63 ASCII letters, digits or hyphens, not
/// starting or ending with a hyphen. It names both the service instance and the
/// host (`{name}.local.`), so it must be a valid host label.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MdnsName(String);

/// Why a string is not an [`MdnsName`].
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum MdnsNameError {
    /// The name is longer than one DNS label (63 bytes).
    #[error("longer than 63 characters")]
    TooLong,
    /// The name holds something other than ASCII letters, digits and hyphens.
    #[error("only ASCII letters, digits and hyphens are allowed")]
    InvalidCharacter,
    /// The name starts or ends with a hyphen.
    #[error("must not start or end with a hyphen")]
    EdgeHyphen,
    /// The name is empty.
    #[error("empty")]
    Empty,
}

impl MdnsName {
    /// Validates `name` as an mDNS label.
    pub fn new(name: &str) -> Result<Self, MdnsNameError> {
        if name.is_empty() {
            return Err(MdnsNameError::Empty);
        }
        if name.len() > 63 {
            return Err(MdnsNameError::TooLong);
        }
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(MdnsNameError::InvalidCharacter);
        }
        if name.starts_with('-') || name.ends_with('-') {
            return Err(MdnsNameError::EdgeHyphen);
        }
        Ok(Self(name.to_owned()))
    }

    /// The label as given.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MdnsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Home Assistant's base URL from `HA_URL`: `http` or `https`, with no
/// trailing slash. REST requests go under `{base}/api`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HaBaseUrl {
    secure: bool,
    /// Everything after `://`, without a trailing slash.
    authority_and_path: String,
}

impl HaBaseUrl {
    /// Parses an `http://` or `https://` URL; the scheme is matched without
    /// regard to case and trailing slashes are removed.
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        let secure = if scheme.eq_ignore_ascii_case("https") {
            true
        } else if scheme.eq_ignore_ascii_case("http") {
            false
        } else {
            return None;
        };
        let rest = rest.trim_end_matches('/');
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if host.is_empty() || rest.contains(char::is_whitespace) {
            return None;
        }
        Some(Self {
            secure,
            authority_and_path: rest.to_owned(),
        })
    }

    /// The base URL, for example `http://homeassistant.local:8123`.
    pub fn as_http(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}", self.authority_and_path)
    }

    /// The WebSocket API URL: the base with `ws`/`wss` and `/api/websocket`.
    pub fn websocket_url(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}/api/websocket", self.authority_and_path)
    }
}

/// How the controller reaches Home Assistant.
#[derive(Clone, Eq, PartialEq)]
pub enum HomeAssistantAccess {
    /// Docker or bare metal: `HA_URL` with an administrator's `HA_TOKEN`.
    Direct {
        /// Home Assistant's base URL.
        base_url: HaBaseUrl,
        /// The long-lived access token.
        token: String,
    },
    /// The add-on: the Supervisor's proxy with `SUPERVISOR_TOKEN`.
    Supervisor {
        /// The token the Supervisor injected.
        token: String,
    },
}

impl HomeAssistantAccess {
    /// The WebSocket API URL.
    pub fn websocket_url(&self) -> String {
        match self {
            Self::Direct { base_url, .. } => base_url.websocket_url(),
            Self::Supervisor { .. } => SUPERVISOR_WEBSOCKET_URL.to_owned(),
        }
    }

    /// The base URL REST paths are appended to (`{base}/api/...`).
    pub fn base_url(&self) -> String {
        match self {
            Self::Direct { base_url, .. } => base_url.as_http(),
            Self::Supervisor { .. } => SUPERVISOR_BASE_URL.to_owned(),
        }
    }

    /// The bearer token for both APIs.
    pub fn token(&self) -> &str {
        match self {
            Self::Direct { token, .. } | Self::Supervisor { token } => token,
        }
    }

    /// The user-facing base URL configured with `HA_URL`; `None` in add-on
    /// mode, where the dashboard shares Home Assistant's origin.
    pub fn configured_url(&self) -> Option<String> {
        match self {
            Self::Direct { base_url, .. } => Some(base_url.as_http()),
            Self::Supervisor { .. } => None,
        }
    }
}

impl fmt::Debug for HomeAssistantAccess {
    // Tokens never reach logs through Debug.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct { base_url, .. } => f
                .debug_struct("Direct")
                .field("base_url", &base_url.as_http())
                .field("token", &"<redacted>")
                .finish(),
            Self::Supervisor { .. } => f
                .debug_struct("Supervisor")
                .field("token", &"<redacted>")
                .finish(),
        }
    }
}

/// The policy for new devices when the database holds no `device_approval`
/// setting.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DeviceApproval {
    /// An administrator approves each new device.
    #[default]
    Strict,
    /// New devices are approved automatically.
    Auto,
}

impl DeviceApproval {
    /// The wire and environment spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Auto => "auto",
        }
    }
}

/// Listener and advertisement settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkConfig {
    /// The address every listener binds to, passed to the bind call as given.
    pub server_host: String,
    /// The plain WebSocket device listener.
    pub server_port: NonZeroU16,
    /// The TLS device listener; `None` when `SERVER_TLS_PORT` is `0`.
    pub server_tls_port: Option<NonZeroU16>,
    /// The dashboard and HTTP API.
    pub api_port: NonZeroU16,
    /// The configured advertisement address; `None` means detect it from the
    /// default route (see [`crate::network`]).
    pub server_ip: Option<Ipv4Addr>,
    /// The mDNS instance and host label.
    pub mdns_name: MdnsName,
    /// Refuse device sockets that are not TLS with a matching token.
    pub require_device_tls: bool,
}

/// Filesystem locations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathsConfig {
    /// The SQLite database.
    pub db_path: PathBuf,
    /// The device-link TLS files; defaults to `tls` beside the database.
    pub tls_dir: PathBuf,
    /// The bundled firmware directory.
    pub firmware_dir: PathBuf,
    /// The speech bundle directory.
    pub speech_bundle_dir: PathBuf,
    /// The Android ONNX Runtime library served to Dots.
    pub ort_android_lib: PathBuf,
    /// The controller's own ONNX Runtime library.
    pub onnxruntime_lib: PathBuf,
    /// The DTLN noise suppression models.
    pub ns_model_dir: PathBuf,
    /// The files firmware delivery installs beside the firmware (`start_server.sh`
    /// and the Echo Show package lists).
    pub device_payloads_dir: PathBuf,
}

/// The whole controller configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerConfig {
    /// Listener and advertisement settings.
    pub network: NetworkConfig,
    /// How to reach Home Assistant.
    pub home_assistant: HomeAssistantAccess,
    /// Filesystem locations.
    pub paths: PathsConfig,
    /// The project version each emulated ESPHome satellite reports; defaults
    /// to the controller version.
    pub esphome_project_version: String,
    /// Serve only requests that arrive through Home Assistant's add-on ingress.
    pub ingress_only: bool,
    /// The approval policy used only when the database holds none.
    pub device_approval_fallback: DeviceApproval,
    /// The controller's own version.
    pub controller_version: String,
}

/// What is wrong with one variable.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ConfigProblem {
    /// The value is not a decimal port number.
    #[error("{value:?} is not a port number")]
    NotAPort {
        /// The value as given.
        value: String,
    },
    /// The port is `0` where a listener is required.
    #[error("port 0 is not allowed here")]
    ZeroPort,
    /// Two listeners would bind the same port.
    #[error("port {port} is also used by {other}")]
    PortConflict {
        /// The shared port.
        port: u16,
        /// The other variable using it.
        other: EnvVar,
    },
    /// The value is not an IPv4 address.
    #[error("{value:?} is not an IPv4 address")]
    NotIpv4 {
        /// The value as given.
        value: String,
    },
    /// The value is not a valid mDNS label.
    #[error("{value:?} is not a valid mDNS name: {reason}")]
    InvalidMdnsName {
        /// The value as given.
        value: String,
        /// Why it was refused.
        reason: MdnsNameError,
    },
    /// The value is not one of the accepted spellings.
    #[error("{value:?} is not one of {accepted:?}")]
    NotOneOf {
        /// The value as given.
        value: String,
        /// The accepted values.
        accepted: &'static [&'static str],
    },
    /// The value is not an `http://` or `https://` URL.
    #[error("{value:?} is not an http:// or https:// URL")]
    NotHttpUrl {
        /// The value as given.
        value: String,
    },
    /// The variable is required by another one that is set.
    #[error("must be set together with {with}")]
    MissingCompanion {
        /// The variable that was set alone.
        with: EnvVar,
    },
    /// Neither direct credentials nor a Supervisor token is present.
    #[error("Home Assistant is not configured: set HA_URL and HA_TOKEN, or run as the add-on")]
    HomeAssistantNotConfigured,
}

/// A problem with one named variable.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{variable}: {problem}")]
pub struct ConfigError {
    /// The variable at fault.
    pub variable: EnvVar,
    /// What is wrong with it.
    pub problem: ConfigProblem,
}

/// Every problem found in one pass over the environment; never empty.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

/// Collects problems so one start reports all of them.
struct Problems(Vec<ConfigError>);

impl Problems {
    fn push(&mut self, variable: EnvVar, problem: ConfigProblem) {
        self.0.push(ConfigError { variable, problem });
    }

    /// Records the error and returns `None`, or passes the value on.
    fn take<T>(&mut self, variable: EnvVar, result: Result<T, ConfigProblem>) -> Option<T> {
        result.map_err(|problem| self.push(variable, problem)).ok()
    }
}

fn parse_port(value: &str) -> Result<u16, ConfigProblem> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ConfigProblem::NotAPort {
            value: value.to_owned(),
        });
    }
    value.parse::<u16>().map_err(|_| ConfigProblem::NotAPort {
        value: value.to_owned(),
    })
}

fn listener_port(
    env: &Environment,
    var: EnvVar,
    default: u16,
) -> Result<NonZeroU16, ConfigProblem> {
    let port = var.read(env).map_or(Ok(default), parse_port)?;
    NonZeroU16::new(port).ok_or(ConfigProblem::ZeroPort)
}

fn flag(
    env: &Environment,
    var: EnvVar,
    on: &'static str,
    off: &'static str,
    accepted: &'static [&'static str],
) -> Result<bool, ConfigProblem> {
    match var.read(env) {
        None => Ok(false),
        Some(value) if value == on => Ok(true),
        Some(value) if value == off => Ok(false),
        Some(value) => Err(ConfigProblem::NotOneOf {
            value: value.to_owned(),
            accepted,
        }),
    }
}

fn path_or(env: &Environment, var: EnvVar, default: &str) -> PathBuf {
    PathBuf::from(var.read(env).unwrap_or(default))
}

/// `tls` beside the database; a bare file name puts it in the working
/// directory.
pub fn default_tls_dir(db_path: &Path) -> PathBuf {
    match db_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(TLS_DIR_NAME),
        _ => PathBuf::from(".").join(TLS_DIR_NAME),
    }
}

fn home_assistant(env: &Environment) -> Result<HomeAssistantAccess, ConfigError> {
    let url = EnvVar::HaUrl.read(env);
    let token = EnvVar::HaToken.read(env);
    let error = |variable, problem| ConfigError { variable, problem };
    match (url, token) {
        (Some(url), Some(token)) => {
            let base_url = HaBaseUrl::parse(url).ok_or_else(|| {
                error(
                    EnvVar::HaUrl,
                    ConfigProblem::NotHttpUrl {
                        value: url.to_owned(),
                    },
                )
            })?;
            Ok(HomeAssistantAccess::Direct {
                base_url,
                token: token.to_owned(),
            })
        }
        (Some(_), None) => Err(error(
            EnvVar::HaToken,
            ConfigProblem::MissingCompanion {
                with: EnvVar::HaUrl,
            },
        )),
        (None, Some(_)) => Err(error(
            EnvVar::HaUrl,
            ConfigProblem::MissingCompanion {
                with: EnvVar::HaToken,
            },
        )),
        (None, None) => match EnvVar::SupervisorToken.read(env) {
            Some(token) => Ok(HomeAssistantAccess::Supervisor {
                token: token.to_owned(),
            }),
            None => Err(error(
                EnvVar::HaUrl,
                ConfigProblem::HomeAssistantNotConfigured,
            )),
        },
    }
}

impl ControllerConfig {
    /// Builds the configuration from `env`.
    ///
    /// `build_version` is the version the binary was built as, used when
    /// `EM_CONTROLLER_VERSION` is unset; with neither, the version is `dev`.
    /// All problems are reported together.
    pub fn from_env(env: &Environment, build_version: Option<&str>) -> Result<Self, ConfigErrors> {
        let mut problems = Problems(Vec::new());

        let server_port = problems.take(
            EnvVar::ServerPort,
            listener_port(env, EnvVar::ServerPort, DEFAULT_SERVER_PORT),
        );
        let api_port = problems.take(
            EnvVar::ApiPort,
            listener_port(env, EnvVar::ApiPort, DEFAULT_API_PORT),
        );
        let server_tls_port = problems.take(
            EnvVar::ServerTlsPort,
            EnvVar::ServerTlsPort
                .read(env)
                .map_or(Ok(DEFAULT_SERVER_TLS_PORT), parse_port)
                .map(NonZeroU16::new),
        );
        let mut listeners: Vec<(EnvVar, u16)> = Vec::new();
        for (var, port) in [
            (EnvVar::ServerPort, server_port),
            (EnvVar::ServerTlsPort, server_tls_port.flatten()),
            (EnvVar::ApiPort, api_port),
        ] {
            let Some(port) = port else { continue };
            let port = port.get();
            if let Some(&(other, _)) = listeners.iter().find(|(_, used)| *used == port) {
                problems.push(var, ConfigProblem::PortConflict { port, other });
            }
            listeners.push((var, port));
        }

        let server_ip = problems.take(
            EnvVar::ServerIp,
            EnvVar::ServerIp
                .read(env)
                .map(|value| {
                    value
                        .parse::<Ipv4Addr>()
                        .map_err(|_| ConfigProblem::NotIpv4 {
                            value: value.to_owned(),
                        })
                })
                .transpose(),
        );
        let mdns_value = EnvVar::MdnsName.read(env).unwrap_or(DEFAULT_MDNS_NAME);
        let mdns_name = problems.take(
            EnvVar::MdnsName,
            MdnsName::new(mdns_value).map_err(|reason| ConfigProblem::InvalidMdnsName {
                value: mdns_value.to_owned(),
                reason,
            }),
        );
        let require_device_tls = problems.take(
            EnvVar::RequireDeviceTls,
            flag(env, EnvVar::RequireDeviceTls, "1", "0", &["0", "1"]),
        );
        let ingress_only = problems.take(
            EnvVar::HomeAssistantIngress,
            flag(
                env,
                EnvVar::HomeAssistantIngress,
                "true",
                "false",
                &["true", "false"],
            ),
        );
        let device_approval = problems.take(
            EnvVar::DeviceApproval,
            match EnvVar::DeviceApproval.read(env) {
                None | Some("strict") => Ok(DeviceApproval::Strict),
                Some("auto") => Ok(DeviceApproval::Auto),
                Some(value) => Err(ConfigProblem::NotOneOf {
                    value: value.to_owned(),
                    accepted: &["strict", "auto"],
                }),
            },
        );
        let home_assistant = home_assistant(env)
            .map_err(|error| problems.0.push(error))
            .ok();

        let db_path = path_or(env, EnvVar::DbPath, DEFAULT_DB_PATH);
        let tls_dir = EnvVar::TlsDir
            .read(env)
            .map_or_else(|| default_tls_dir(&db_path), PathBuf::from);
        let paths = PathsConfig {
            tls_dir,
            firmware_dir: path_or(env, EnvVar::FirmwareDir, DEFAULT_FIRMWARE_DIR),
            speech_bundle_dir: path_or(env, EnvVar::SpeechBundleDir, DEFAULT_SPEECH_BUNDLE_DIR),
            ort_android_lib: path_or(env, EnvVar::OrtAndroidLib, DEFAULT_ORT_ANDROID_LIB),
            onnxruntime_lib: path_or(env, EnvVar::OnnxRuntimeLib, DEFAULT_ONNXRUNTIME_LIB),
            ns_model_dir: path_or(env, EnvVar::NsModelDir, DEFAULT_NS_MODEL_DIR),
            device_payloads_dir: path_or(
                env,
                EnvVar::DevicePayloadsDir,
                DEFAULT_DEVICE_PAYLOADS_DIR,
            ),
            db_path,
        };

        let controller_version = EnvVar::ControllerVersion
            .read(env)
            .or(build_version.map(str::trim).filter(|v| !v.is_empty()))
            .unwrap_or(UNKNOWN_CONTROLLER_VERSION)
            .to_owned();
        let esphome_project_version = EnvVar::EsphomeProjectVersion
            .read(env)
            .map_or_else(|| controller_version.clone(), str::to_owned);

        match (
            server_port,
            server_tls_port,
            api_port,
            server_ip,
            mdns_name,
            require_device_tls,
            ingress_only,
            device_approval,
            home_assistant,
        ) {
            (
                Some(server_port),
                Some(server_tls_port),
                Some(api_port),
                Some(server_ip),
                Some(mdns_name),
                Some(require_device_tls),
                Some(ingress_only),
                Some(device_approval_fallback),
                Some(home_assistant),
            ) if problems.0.is_empty() => Ok(Self {
                network: NetworkConfig {
                    server_host: EnvVar::ServerHost
                        .read(env)
                        .unwrap_or(DEFAULT_SERVER_HOST)
                        .to_owned(),
                    server_port,
                    server_tls_port,
                    api_port,
                    server_ip,
                    mdns_name,
                    require_device_tls,
                },
                home_assistant,
                paths,
                esphome_project_version,
                ingress_only,
                device_approval_fallback,
                controller_version,
            }),
            _ => Err(ConfigErrors(problems.0)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Environment {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn with_ha(pairs: &[(&str, &str)]) -> Environment {
        let mut map = env(pairs);
        map.entry("HA_URL".into())
            .or_insert_with(|| "http://homeassistant.local:8123".into());
        map.entry("HA_TOKEN".into())
            .or_insert_with(|| "secret".into());
        map
    }

    fn problems(env: &Environment) -> Vec<ConfigError> {
        ControllerConfig::from_env(env, None).unwrap_err().0
    }

    #[test]
    fn defaults_match_the_documented_values() {
        let config = ControllerConfig::from_env(&with_ha(&[]), None).unwrap();
        let network = &config.network;
        assert_eq!(network.server_host, "0.0.0.0");
        assert_eq!(network.server_port.get(), 8767);
        assert_eq!(network.server_tls_port.map(NonZeroU16::get), Some(8770));
        assert_eq!(network.api_port.get(), 8768);
        assert_eq!(network.server_ip, None);
        assert_eq!(network.mdns_name.as_str(), "echomuse");
        assert!(!network.require_device_tls);
        assert_eq!(config.paths.db_path, PathBuf::from("echomuse.db"));
        assert_eq!(config.paths.tls_dir, PathBuf::from("./tls"));
        assert_eq!(config.paths.firmware_dir, PathBuf::from("/app/firmware"));
        assert_eq!(config.paths.speech_bundle_dir, PathBuf::from("/app/speech"));
        assert_eq!(
            config.paths.ort_android_lib,
            PathBuf::from("/app/models/ort_android/libonnxruntime.so")
        );
        assert_eq!(
            config.paths.onnxruntime_lib,
            PathBuf::from("/app/models/onnxruntime/libonnxruntime.so.1.20.1")
        );
        assert_eq!(config.paths.ns_model_dir, PathBuf::from("/app/models/dtln"));
        assert_eq!(
            config.paths.device_payloads_dir,
            PathBuf::from("/app/device_payloads")
        );
        assert_eq!(config.controller_version, "dev");
        assert_eq!(config.esphome_project_version, "dev");
        assert!(!config.ingress_only);
        assert_eq!(config.device_approval_fallback, DeviceApproval::Strict);
    }

    #[test]
    fn every_variable_is_read() {
        let config = ControllerConfig::from_env(
            &with_ha(&[
                ("SERVER_HOST", "192.168.1.2"),
                ("SERVER_PORT", "9000"),
                ("SERVER_TLS_PORT", "9001"),
                ("API_PORT", "9002"),
                ("SERVER_IP", " 10.0.0.5 "),
                ("MDNS_NAME", "kitchen-hub"),
                ("REQUIRE_DEVICE_TLS", "1"),
                ("DB_PATH", "/data/echomuse.db"),
                ("FIRMWARE_DIR", "../device/build"),
                ("SPEECH_BUNDLE_DIR", "./speech"),
                ("ORT_ANDROID_LIB", "/opt/ort.so"),
                ("ONNXRUNTIME_LIB", "/opt/host-ort.so"),
                ("NS_MODEL_DIR", "/opt/dtln"),
                ("DEVICE_PAYLOADS_DIR", "/opt/payloads"),
                ("ESPHOME_PROJECT_VERSION", "2.0.0"),
                ("ECHOMUSE_HOME_ASSISTANT_INGRESS", "true"),
                ("DEVICE_APPROVAL", "auto"),
                ("EM_CONTROLLER_VERSION", "v2.19.0"),
            ]),
            Some("v0.0.1"),
        )
        .unwrap();
        assert_eq!(config.network.server_host, "192.168.1.2");
        assert_eq!(config.network.server_port.get(), 9000);
        assert_eq!(
            config.network.server_tls_port.map(NonZeroU16::get),
            Some(9001)
        );
        assert_eq!(config.network.api_port.get(), 9002);
        assert_eq!(config.network.server_ip, Some(Ipv4Addr::new(10, 0, 0, 5)));
        assert_eq!(config.network.mdns_name.as_str(), "kitchen-hub");
        assert!(config.network.require_device_tls);
        assert_eq!(config.paths.tls_dir, PathBuf::from("/data/tls"));
        assert_eq!(config.paths.firmware_dir, PathBuf::from("../device/build"));
        assert_eq!(config.paths.speech_bundle_dir, PathBuf::from("./speech"));
        assert_eq!(config.paths.ort_android_lib, PathBuf::from("/opt/ort.so"));
        assert_eq!(
            config.paths.onnxruntime_lib,
            PathBuf::from("/opt/host-ort.so")
        );
        assert_eq!(config.paths.ns_model_dir, PathBuf::from("/opt/dtln"));
        assert_eq!(
            config.paths.device_payloads_dir,
            PathBuf::from("/opt/payloads")
        );
        assert_eq!(config.esphome_project_version, "2.0.0");
        assert!(config.ingress_only);
        assert_eq!(config.device_approval_fallback, DeviceApproval::Auto);
        assert_eq!(config.controller_version, "v2.19.0");
    }

    #[test]
    fn tls_dir_follows_the_database_unless_set() {
        let beside =
            ControllerConfig::from_env(&with_ha(&[("DB_PATH", "data/em.db")]), None).unwrap();
        assert_eq!(beside.paths.tls_dir, PathBuf::from("data/tls"));
        let explicit = ControllerConfig::from_env(
            &with_ha(&[("DB_PATH", "data/em.db"), ("TLS_DIR", "/srv/tls")]),
            None,
        )
        .unwrap();
        assert_eq!(explicit.paths.tls_dir, PathBuf::from("/srv/tls"));
        let empty = ControllerConfig::from_env(
            &with_ha(&[("DB_PATH", "/data/em.db"), ("TLS_DIR", "  ")]),
            None,
        )
        .unwrap();
        assert_eq!(empty.paths.tls_dir, PathBuf::from("/data/tls"));
    }

    #[test]
    fn version_falls_back_from_environment_to_build_to_dev() {
        let built = ControllerConfig::from_env(&with_ha(&[]), Some("v2.20.0-3-gabc")).unwrap();
        assert_eq!(built.controller_version, "v2.20.0-3-gabc");
        assert_eq!(built.esphome_project_version, "v2.20.0-3-gabc");
        let blank_build = ControllerConfig::from_env(&with_ha(&[]), Some(" ")).unwrap();
        assert_eq!(blank_build.controller_version, "dev");
    }

    #[test]
    fn tls_port_zero_disables_the_tls_listener() {
        let config =
            ControllerConfig::from_env(&with_ha(&[("SERVER_TLS_PORT", "0")]), None).unwrap();
        assert_eq!(config.network.server_tls_port, None);
    }

    #[test]
    fn bad_values_name_their_variable_and_are_all_reported() {
        let found = problems(&with_ha(&[
            ("SERVER_PORT", "eighty"),
            ("API_PORT", "0"),
            ("SERVER_TLS_PORT", "70000"),
            ("SERVER_IP", "fe80::1"),
            ("MDNS_NAME", "echo.muse"),
            ("REQUIRE_DEVICE_TLS", "yes"),
            ("ECHOMUSE_HOME_ASSISTANT_INGRESS", "1"),
            ("DEVICE_APPROVAL", "manual"),
        ]));
        let variables: Vec<EnvVar> = found.iter().map(|e| e.variable).collect();
        assert_eq!(
            variables,
            [
                EnvVar::ServerPort,
                EnvVar::ApiPort,
                EnvVar::ServerTlsPort,
                EnvVar::ServerIp,
                EnvVar::MdnsName,
                EnvVar::RequireDeviceTls,
                EnvVar::HomeAssistantIngress,
                EnvVar::DeviceApproval,
            ]
        );
        assert_eq!(found[1].problem, ConfigProblem::ZeroPort);
        assert!(matches!(
            found[4].problem,
            ConfigProblem::InvalidMdnsName {
                reason: MdnsNameError::InvalidCharacter,
                ..
            }
        ));
    }

    #[test]
    fn port_boundaries() {
        for (value, ok) in [
            ("1", true),
            ("65535", true),
            ("65536", false),
            ("+80", false),
            ("-1", false),
        ] {
            let result = ControllerConfig::from_env(&with_ha(&[("SERVER_PORT", value)]), None);
            assert_eq!(result.is_ok(), ok, "SERVER_PORT={value}");
        }
    }

    #[test]
    fn listeners_may_not_share_a_port() {
        let found = problems(&with_ha(&[("SERVER_PORT", "8768")]));
        assert_eq!(
            found,
            [ConfigError {
                variable: EnvVar::ApiPort,
                problem: ConfigProblem::PortConflict {
                    port: 8768,
                    other: EnvVar::ServerPort,
                },
            }]
        );
    }

    #[test]
    fn mdns_name_limits() {
        assert!(MdnsName::new(&"a".repeat(63)).is_ok());
        assert_eq!(MdnsName::new(&"a".repeat(64)), Err(MdnsNameError::TooLong));
        assert_eq!(MdnsName::new("-echo"), Err(MdnsNameError::EdgeHyphen));
        assert_eq!(MdnsName::new(""), Err(MdnsNameError::Empty));
    }

    #[test]
    fn direct_credentials_take_precedence_over_the_supervisor() {
        let config = ControllerConfig::from_env(
            &env(&[
                ("HA_URL", "https://ha.example:8123/"),
                ("HA_TOKEN", " abc "),
                ("SUPERVISOR_TOKEN", "sup"),
            ]),
            None,
        )
        .unwrap();
        let ha = &config.home_assistant;
        assert_eq!(ha.websocket_url(), "wss://ha.example:8123/api/websocket");
        assert_eq!(ha.base_url(), "https://ha.example:8123");
        assert_eq!(ha.token(), "abc");
        assert_eq!(
            ha.configured_url().as_deref(),
            Some("https://ha.example:8123")
        );
    }

    #[test]
    fn supervisor_token_is_used_without_direct_credentials() {
        let config =
            ControllerConfig::from_env(&env(&[("SUPERVISOR_TOKEN", "sup")]), None).unwrap();
        let ha = &config.home_assistant;
        assert_eq!(ha.websocket_url(), "ws://supervisor/core/websocket");
        assert_eq!(ha.base_url(), "http://supervisor/core");
        assert_eq!(ha.token(), "sup");
        assert_eq!(ha.configured_url(), None);
        assert!(!format!("{ha:?}").contains("sup\""));
    }

    #[test]
    fn home_assistant_misconfiguration_is_named() {
        assert_eq!(
            problems(&env(&[]))[0],
            ConfigError {
                variable: EnvVar::HaUrl,
                problem: ConfigProblem::HomeAssistantNotConfigured,
            }
        );
        assert_eq!(
            problems(&env(&[
                ("HA_URL", "http://ha:8123"),
                ("SUPERVISOR_TOKEN", "s")
            ]))[0],
            ConfigError {
                variable: EnvVar::HaToken,
                problem: ConfigProblem::MissingCompanion {
                    with: EnvVar::HaUrl
                },
            }
        );
        assert_eq!(
            problems(&env(&[
                ("HA_URL", "homeassistant.local:8123"),
                ("HA_TOKEN", "t")
            ]))[0]
                .variable,
            EnvVar::HaUrl
        );
        assert_eq!(
            problems(&env(&[("HA_URL", "http:///api"), ("HA_TOKEN", "t")]))[0].variable,
            EnvVar::HaUrl
        );
    }

    #[test]
    fn plain_http_maps_to_plain_websocket() {
        let url = HaBaseUrl::parse("HTTP://10.0.0.2:8123//").unwrap();
        assert_eq!(url.as_http(), "http://10.0.0.2:8123");
        assert_eq!(url.websocket_url(), "ws://10.0.0.2:8123/api/websocket");
    }

    #[test]
    fn every_variable_has_a_distinct_name() {
        let mut names: Vec<&str> = EnvVar::ALL.iter().map(|v| v.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), EnvVar::ALL.len());
    }
}
