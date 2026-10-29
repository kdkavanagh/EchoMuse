//! What the devices routes need from the device runtimes, which the integrator owns
//! (§5.4: the API never touches runtime state; it reads published snapshots and sends
//! named commands).
//!
//! The integrator implements [`DeviceRuntimes`] over its registry: [`live`] answers from
//! each runtime's latest published snapshot without waiting, and the commands complete
//! when the runtime has done the work. [`LiveDevice`] is also how the integrator shapes
//! the live part of a device for `device_update` events
//! ([`LiveDevice::state_patch_since`]), so the device object has one source.
//!
//! [`live`]: DeviceRuntimes::live

use std::{collections::BTreeMap, fmt, future::Future, net::IpAddr, pin::Pin, sync::Arc};

use echomuse_core::types::DeviceId;
use echomuse_protocol::{Capabilities, Capability, Platform};
use serde_json::{Map, Value, json};

use crate::{
    devices::DeviceLabel,
    events::DeviceStatePatch,
    gaps::{GapReason, capability_gaps},
};

/// What a device's runtime last published: the part of the device object that only
/// the runtime knows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveDevice {
    /// The ESPHome voice port the runtime serves; `None` when it serves none, and the
    /// stored port is shown.
    pub esphome_port: Option<u16>,
    /// The attached device link session; `None` while no session is attached.
    pub session: Option<LiveSession>,
    /// The runtime's health; `None` for a device without a runtime (pending).
    pub health: Option<LiveHealth>,
}

/// A device runtime's health (spec §5.5): whether it can serve the device now, and its
/// error window. Shown on the device object as `health`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveHealth {
    /// Why the device is unavailable; `None` while it is available.
    pub unavailable: Option<String>,
    /// Its runtime failed and is being rebuilt; its sessions are refused until then.
    pub restarting: bool,
    /// Failures since it last ran a stable minute without one.
    pub recent_failures: u32,
    /// The latest failure, if there was one.
    pub last_failure: Option<LiveFailure>,
}

/// One failure of a device runtime.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveFailure {
    pub reason: String,
    /// When it happened, in Unix seconds.
    pub at: i64,
}

impl LiveHealth {
    /// The `health` object: `available`, `reason` (null while available), `restarting`,
    /// `recent_failures` and `last_failure` (`{reason, at}` or null).
    pub fn to_json(&self) -> Value {
        json!({
            "available": self.unavailable.is_none(),
            "reason": self.unavailable,
            "restarting": self.restarting,
            "recent_failures": self.recent_failures,
            "last_failure": self.last_failure.as_ref().map(|failure| json!({
                "reason": failure.reason,
                "at": failure.at,
            })),
        })
    }
}

/// The attached session, as the runtime knows it.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveSession {
    /// The control socket's peer address.
    pub ip: IpAddr,
    /// The image the firmware runs on.
    pub platform: Platform,
    /// What the hello announced.
    pub capabilities: Capabilities,
    /// Whether the link is TLS.
    pub tls: bool,
    /// The hello's firmware version; `None` when it sent none.
    pub firmware_version: Option<String>,
    /// The hello's OS version; `None` when it sent none.
    pub os_version: Option<String>,
    /// The privacy mute as last reported; `None` when not reported.
    pub muted: Option<bool>,
    /// The volume as last reported; `None` before the device reports it.
    pub volume: Option<LiveVolume>,
    /// The latest `stats` body, as reported; `None` before the first.
    pub stats: Option<Arc<Map<String, Value>>>,
    /// The SHA-256 of every asset the hello listed as installed; `None` when it sent no
    /// list.
    pub installed_assets: Option<Vec<echomuse_protocol::Sha256>>,
}

/// A device's volume as it reports it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LiveVolume {
    /// The level, 0–127.
    Level(u8),
    /// The device has no volume control this run.
    Unavailable,
}

/// The highest [`LiveVolume::Level`]; the dashboard reads the volume as a fraction of it.
pub const MAX_VOLUME_LEVEL: u8 = 127;

impl LiveDevice {
    /// The device object's fields this decides, as the dashboard reads them: the
    /// runtime's `health`, whether it is connected, and while it is, the session's
    /// platform, capabilities and their gaps, link, volume, mute, stats, address and
    /// versions. A value nobody reported is `null`.
    pub fn fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        if let Some(port) = self.esphome_port {
            fields.insert("esphome_port".into(), json!(port));
        }
        fields.insert(
            "health".into(),
            self.health
                .as_ref()
                .map_or(Value::Null, LiveHealth::to_json),
        );
        let Some(session) = &self.session else {
            for key in [
                "muted",
                "platform",
                "capabilities",
                "capability_gaps",
                "linkTls",
                "volume",
                "volume_gap",
                "stats",
            ] {
                fields.insert(key.into(), Value::Null);
            }
            fields.insert("connected".into(), json!(false));
            return fields;
        };
        let mut names: Vec<&str> = session
            .capabilities
            .known()
            .map(Capability::as_str)
            .collect();
        names.extend(session.capabilities.unknown());
        let gaps: BTreeMap<Capability, GapReason> =
            capability_gaps(session.platform, &session.capabilities);
        let (volume, volume_gap) = match session.volume {
            Some(LiveVolume::Level(level)) => (
                json!(f64::from(level.min(MAX_VOLUME_LEVEL)) / f64::from(MAX_VOLUME_LEVEL)),
                Value::Null,
            ),
            Some(LiveVolume::Unavailable) => (Value::Null, json!(GapReason::NoVolumeControl)),
            None => (Value::Null, Value::Null),
        };
        fields.extend([
            ("connected".into(), json!(true)),
            ("muted".into(), json!(session.muted)),
            ("platform".into(), json!(session.platform)),
            ("capabilities".into(), json!(names)),
            ("capability_gaps".into(), json!(gaps)),
            ("linkTls".into(), json!(session.tls)),
            ("volume".into(), volume),
            ("volume_gap".into(), volume_gap),
            (
                "stats".into(),
                session
                    .stats
                    .as_ref()
                    .map_or(Value::Null, |stats| Value::Object(Map::clone(stats))),
            ),
            ("ip".into(), json!(session.ip.to_string())),
            ("firmware_ver".into(), json!(session.firmware_version)),
            ("os_version".into(), json!(session.os_version)),
        ]);
        fields
    }

    /// The fields that differ from `previous`'s, for a `device_update` event. Empty
    /// when nothing the dashboard shows changed.
    pub fn state_patch_since(&self, previous: &LiveDevice) -> DeviceStatePatch {
        let before = previous.fields();
        DeviceStatePatch(
            self.fields()
                .into_iter()
                .filter(|(key, value)| before.get(key) != Some(value))
                .collect(),
        )
    }

    /// Whether a session is attached.
    pub fn connected(&self) -> bool {
        self.session.is_some()
    }
}

/// Devices whose stored settings just changed. The settings service has already
/// stored and published them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SettingsApplied {
    /// Every device whose effective settings changed, sorted by id.
    pub changed: Vec<DeviceId>,
    /// Of those, the ones whose change takes effect only through a new
    /// `session.ready`: their sessions are renegotiated (§3.1.4).
    pub new_session: Vec<DeviceId>,
}

/// Why a runtime command did not complete.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeError {
    /// The device is not registered.
    UnknownDevice,
    /// The runtimes are shutting down and take no more commands.
    Unavailable,
    /// The command failed; the text goes to the log, not to the client.
    Failed(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDevice => f.write_str("the device is not registered"),
            Self::Unavailable => f.write_str("the device runtimes are shutting down"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// The device runtimes, as the devices routes use them. The integrator implements it
/// over its registry.
pub trait DeviceRuntimes: Send + Sync + 'static {
    /// The device's runtime state as last published; `None` when it has no runtime (a
    /// pending device). Answers without waiting.
    fn live(&self, device: &DeviceId) -> Option<LiveDevice>;

    /// Approve a registered device under `label`, store it, and start its runtime
    /// (`Registry::approve`). Approving an approved device stores the new label.
    fn approve(
        &self,
        device: DeviceId,
        label: DeviceLabel,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Delete a device (§5.7.6): revoke its admission and routing, stop its runtime and
    /// listeners, then delete its row and every record and file kept for it. Completes
    /// when it is gone.
    fn delete(&self, device: DeviceId) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Settings changed: send each changed, connected device its new configuration,
    /// and renegotiate the session of each in [`SettingsApplied::new_session`]. Returns
    /// the devices the change was sent to.
    fn settings_applied(
        &self,
        change: SettingsApplied,
    ) -> impl Future<Output = Vec<DeviceId>> + Send;

    /// A device's label was stored: services that name the device (its Home Assistant
    /// calendar, its ESPHome nodes) learn the new one.
    fn relabeled(&self, device: DeviceId, label: String) -> impl Future<Output = ()> + Send;
}

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`DeviceRuntimes`] behind a pointer.
pub(crate) trait Runtimes: Send + Sync + 'static {
    fn live(&self, device: &DeviceId) -> Option<LiveDevice>;
    fn approve(
        &self,
        device: DeviceId,
        label: DeviceLabel,
    ) -> BoxFuture<'_, Result<(), RuntimeError>>;
    fn delete(&self, device: DeviceId) -> BoxFuture<'_, Result<(), RuntimeError>>;
    fn settings_applied(&self, change: SettingsApplied) -> BoxFuture<'_, Vec<DeviceId>>;
    fn relabeled(&self, device: DeviceId, label: String) -> BoxFuture<'_, ()>;
}

impl<R: DeviceRuntimes> Runtimes for R {
    fn live(&self, device: &DeviceId) -> Option<LiveDevice> {
        DeviceRuntimes::live(self, device)
    }

    fn approve(
        &self,
        device: DeviceId,
        label: DeviceLabel,
    ) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(DeviceRuntimes::approve(self, device, label))
    }

    fn delete(&self, device: DeviceId) -> BoxFuture<'_, Result<(), RuntimeError>> {
        Box::pin(DeviceRuntimes::delete(self, device))
    }

    fn settings_applied(&self, change: SettingsApplied) -> BoxFuture<'_, Vec<DeviceId>> {
        Box::pin(DeviceRuntimes::settings_applied(self, change))
    }

    fn relabeled(&self, device: DeviceId, label: String) -> BoxFuture<'_, ()> {
        Box::pin(DeviceRuntimes::relabeled(self, device, label))
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn session(platform: Platform, names: &[&str]) -> LiveSession {
        LiveSession {
            ip: IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            platform,
            capabilities: Capabilities::from_names(names.iter().copied()),
            tls: true,
            firmware_version: Some("fw-synthetic".to_owned()),
            os_version: None,
            muted: Some(false),
            volume: Some(LiveVolume::Level(127)),
            stats: None,
            installed_assets: None,
        }
    }

    #[test]
    fn a_disconnected_device_shows_nothing_live() {
        let fields = LiveDevice::default().fields();
        assert_eq!(fields["connected"], json!(false));
        for key in [
            "muted",
            "platform",
            "capabilities",
            "capability_gaps",
            "volume",
        ] {
            assert_eq!(fields[key], Value::Null, "{key}");
        }
        // The stored address and versions stay as they are.
        assert!(!fields.contains_key("ip"));
        assert!(!fields.contains_key("firmware_ver"));
    }

    #[test]
    fn a_patch_carries_only_what_changed() {
        let before = LiveDevice {
            esphome_port: Some(16001),
            session: Some(session(Platform::FireOs6, &["leds"])),
            health: Some(LiveHealth::default()),
        };
        let mut after = before.clone();
        let live = after.session.as_mut().unwrap();
        live.muted = Some(true);
        live.volume = Some(LiveVolume::Unavailable);
        assert_eq!(
            Value::Object(after.state_patch_since(&before).0),
            json!({"muted": true, "volume": null, "volume_gap": "the device reports no volume control"})
        );
        assert!(before.state_patch_since(&before).0.is_empty());
        let gone = LiveDevice {
            esphome_port: Some(16001),
            session: None,
            health: Some(LiveHealth::default()),
        };
        let patch = gone.state_patch_since(&before).0;
        assert_eq!(patch["connected"], json!(false));
        assert_eq!(patch["platform"], Value::Null);
        assert!(!patch.contains_key("health"));
    }

    #[test]
    fn health_shows_why_a_device_is_unavailable_and_its_error_window() {
        assert_eq!(LiveDevice::default().fields()["health"], Value::Null);
        let healthy = LiveDevice {
            health: Some(LiveHealth::default()),
            ..LiveDevice::default()
        };
        let failed = LiveDevice {
            health: Some(LiveHealth {
                unavailable: Some("the device runtime failed: worker thread panicked".to_owned()),
                restarting: true,
                recent_failures: 2,
                last_failure: Some(LiveFailure {
                    reason: "the device runtime failed: worker thread panicked".to_owned(),
                    at: 1_700_000_000,
                }),
            }),
            ..LiveDevice::default()
        };
        assert_eq!(
            Value::Object(failed.state_patch_since(&healthy).0),
            json!({"health": {
                "available": false,
                "reason": "the device runtime failed: worker thread panicked",
                "restarting": true,
                "recent_failures": 2,
                "last_failure": {
                    "reason": "the device runtime failed: worker thread panicked",
                    "at": 1_700_000_000,
                },
            }})
        );
        assert_eq!(
            healthy.fields()["health"],
            json!({"available": true, "reason": null, "restarting": false,
                   "recent_failures": 0, "last_failure": null})
        );
    }
}
