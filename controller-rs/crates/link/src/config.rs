//! What the link listens on, how much it buffers and how long it waits.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use echomuse_config::{ControllerConfig, DeviceApproval, DeviceLinkTls};
use echomuse_core::types::QueueLimits;
use echomuse_db::devices::ApprovalMode;
use tokio_rustls::rustls::ServerConfig;

/// The largest WebSocket frame or message any device socket accepts (spec §3.1.1).
pub const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

/// The largest binary chunk the asset socket sends (spec §3.1.9).
pub const MAX_ASSET_CHUNK_BYTES: usize = 64 * 1024;

/// The controller's wall clock. Wall time supplies the `utc_ms` of `session.ready` and
/// `clock.reply` and the time a connection attempt is recorded; it never drives a
/// deadline, which run on Tokio's monotonic clock (spec §5.3).
pub trait WallClock: Send + Sync + 'static {
    /// The current wall time.
    fn now(&self) -> SystemTime;
}

/// The operating system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl WallClock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Milliseconds since the Unix epoch, or `None` when the clock reads before 1970.
pub(crate) fn utc_ms(clock: &dyn WallClock) -> Option<u64> {
    let since = clock.now().duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(since.as_millis()).ok()
}

/// The TLS device listener.
#[derive(Clone, Debug)]
pub struct TlsListenerConfig {
    /// The port; 0 asks the operating system for a free one.
    pub port: u16,
    /// The rustls configuration presenting the server certificate devices verify
    /// against their pinned CA.
    pub server_config: Arc<ServerConfig>,
}

/// One bounded egress queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EgressLimits {
    /// Frames queued at most.
    pub messages: usize,
    /// Bytes queued at most.
    pub bytes: u32,
}

/// The bounds of every queue the link owns (spec §5.2).
#[derive(Clone, Copy, Debug)]
pub struct LinkLimits {
    /// The ordered ingress channel of one session: control messages and audio packets
    /// together. `max_age` is the longest an item may wait in it before the session is
    /// closed as overloaded.
    pub ingress: QueueLimits,
    /// Urgent events (privacy changes) queued at most per session.
    pub urgent_events: usize,
    /// `log` and `ble_adverts` messages queued at most per session. When this queue is
    /// full, the oldest unread ones are not lost silently: the next delivered item counts
    /// the ones dropped before it.
    pub telemetry_events: usize,
    /// The control socket's writer queue.
    pub control_egress: EgressLimits,
    /// The audio socket's writer queue for render frames.
    pub render_egress: EgressLimits,
}

impl Default for LinkLimits {
    fn default() -> Self {
        Self {
            // A wake candidate's backfill arrives as one burst of up to about a hundred
            // packets across mic, cells, AFE and reference (spec §3.1.6), on top of live
            // audio and control. Two seconds of residence leaves room for a busy runtime
            // without letting audio go stale.
            ingress: QueueLimits {
                messages: 1024,
                bytes: 4 * 1024 * 1024,
                max_age: Duration::from_secs(2),
            },
            urgent_events: 16,
            telemetry_events: 64,
            control_egress: EgressLimits {
                messages: 256,
                bytes: 4 * 1024 * 1024,
            },
            // Progress-based flow control keeps at most 258,304 frames ahead of the
            // device (spec §3.1.7): 68 render packets of 3,840 frames, 7,744 bytes each.
            render_egress: EgressLimits {
                messages: 128,
                bytes: 1024 * 1024,
            },
        }
    }
}

impl LinkLimits {
    fn validate(&self) -> Result<(), LinkConfigError> {
        let max = tokio::sync::Semaphore::MAX_PERMITS;
        let egress_ok = |e: EgressLimits| {
            e.messages > 0 && e.messages <= max && e.bytes > 0 && e.bytes as usize <= max
        };
        let ok = self.ingress.validate().is_ok()
            && self.urgent_events > 0
            && self.urgent_events <= max
            && self.telemetry_events > 0
            && self.telemetry_events <= max
            && egress_ok(self.control_egress)
            && egress_ok(self.render_egress)
            && self.control_egress.bytes as usize >= echomuse_protocol::MAX_CONTROL_BYTES;
        if ok {
            Ok(())
        } else {
            Err(LinkConfigError::InvalidLimits)
        }
    }
}

/// The link's deadlines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkTimings {
    /// From the control upgrade to the hello (spec §3.1.3: 10 s). Also bounds reading
    /// an HTTP upgrade request.
    pub hello: Duration,
    /// From the hello to `session.ready` being written: database work and the
    /// integrator's admission. The device gives up 10 s after sending its hello.
    pub ready: Duration,
    /// From `session.ready` to the audio socket joining. The firmware dials audio as
    /// soon as it reads `session.ready`, and losing audio ends a session, so a session
    /// whose audio never arrives ends too.
    pub audio_join: Duration,
    /// The controller's heartbeat period (spec §3.1.3: 1 s).
    pub heartbeat: Duration,
    /// Control silence after which the link reads as degraded (2 s).
    pub degraded: Duration,
    /// Control silence after which the session is lost (3 s).
    pub lost: Duration,
    /// The longest one frame may take to write; a socket that stalls longer is closed.
    /// The firmware uses the same 3 s.
    pub write: Duration,
    /// The longest a TLS handshake may take.
    pub tls_handshake: Duration,
}

impl Default for LinkTimings {
    fn default() -> Self {
        Self {
            hello: Duration::from_secs(10),
            ready: Duration::from_secs(10),
            audio_join: Duration::from_secs(10),
            heartbeat: Duration::from_secs(1),
            degraded: Duration::from_secs(2),
            lost: Duration::from_secs(3),
            write: Duration::from_secs(3),
            tls_handshake: Duration::from_secs(10),
        }
    }
}

impl LinkTimings {
    fn validate(&self) -> Result<(), LinkConfigError> {
        let all = [
            self.hello,
            self.ready,
            self.audio_join,
            self.heartbeat,
            self.degraded,
            self.lost,
            self.write,
            self.tls_handshake,
        ];
        if all.iter().any(Duration::is_zero) || self.degraded > self.lost {
            return Err(LinkConfigError::InvalidTimings);
        }
        Ok(())
    }
}

/// Everything the link server needs besides the database and the integrator's traits.
#[derive(Clone)]
pub struct LinkConfig {
    /// The address both listeners bind to, passed to the bind call as given.
    pub host: String,
    /// The plain WebSocket listener's port; 0 asks the operating system for a free one.
    pub plain_port: u16,
    /// The TLS listener, `None` when `SERVER_TLS_PORT` is 0.
    pub tls: Option<TlsListenerConfig>,
    /// `REQUIRE_DEVICE_TLS`: refuse sockets that are not TLS with a matching token.
    pub require_device_tls: bool,
    /// `DEVICE_APPROVAL`, used only when the database holds no `device_approval`.
    pub approval_fallback: ApprovalMode,
    /// Queue bounds.
    pub limits: LinkLimits,
    /// Deadlines.
    pub timings: LinkTimings,
    /// The wall clock.
    pub wall_clock: Arc<dyn WallClock>,
}

impl fmt::Debug for LinkConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinkConfig")
            .field("host", &self.host)
            .field("plain_port", &self.plain_port)
            .field("tls_port", &self.tls.as_ref().map(|t| t.port))
            .field("require_device_tls", &self.require_device_tls)
            .field("approval_fallback", &self.approval_fallback)
            .field("limits", &self.limits)
            .field("timings", &self.timings)
            .finish_non_exhaustive()
    }
}

/// Why a [`LinkConfig`] cannot be used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkConfigError {
    /// `SERVER_TLS_PORT` is set but no TLS credentials were loaded.
    MissingTlsCredentials,
    /// A queue bound is zero or too large, or the control queue cannot hold one
    /// maximum-size control frame.
    InvalidLimits,
    /// A deadline is zero, or degradation comes after loss.
    InvalidTimings,
}

impl fmt::Display for LinkConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTlsCredentials => {
                f.write_str("SERVER_TLS_PORT is set but no TLS credentials were loaded")
            }
            Self::InvalidLimits => f.write_str("a link queue bound is zero or too large"),
            Self::InvalidTimings => {
                f.write_str("a link deadline is zero or degradation comes after loss")
            }
        }
    }
}

impl std::error::Error for LinkConfigError {}

impl LinkConfig {
    /// The listeners and policies `controller` configures, with default limits and
    /// timings and the system clock. `tls` must be given when `SERVER_TLS_PORT` is not
    /// 0; it is ignored otherwise.
    pub fn from_controller(
        controller: &ControllerConfig,
        tls: Option<&DeviceLinkTls>,
    ) -> Result<Self, LinkConfigError> {
        let network = &controller.network;
        let tls = match network.server_tls_port {
            None => None,
            Some(port) => {
                let credentials = tls.ok_or(LinkConfigError::MissingTlsCredentials)?;
                Some(TlsListenerConfig {
                    port: port.get(),
                    server_config: credentials.server_config(),
                })
            }
        };
        Ok(Self {
            host: network.server_host.clone(),
            plain_port: network.server_port.get(),
            tls,
            require_device_tls: network.require_device_tls,
            approval_fallback: match controller.device_approval_fallback {
                DeviceApproval::Strict => ApprovalMode::Strict,
                DeviceApproval::Auto => ApprovalMode::Auto,
            },
            limits: LinkLimits::default(),
            timings: LinkTimings::default(),
            wall_clock: Arc::new(SystemClock),
        })
    }

    pub(crate) fn validate(&self) -> Result<(), LinkConfigError> {
        self.limits.validate()?;
        self.timings.validate()
    }
}
