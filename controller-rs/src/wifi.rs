//! Wi-Fi control for devices that announce `wifi_control` (the Echo Dot; spec §4.10,
//! protocol-v1.md §4.8, `device/internal/wifi/wifi.go`).
//!
//! - **Scan.** `wifi_scan {}`; the device answers `wifi_scan_result {error, networks}`
//!   about four seconds later. The dashboard's request waits for it, up to
//!   [`SCAN_TIMEOUT`].
//! - **Change.** `wifi_change {ssid, psk}`, which the device does not acknowledge. It
//!   rewrites its network configuration, which drops the link, and reports
//!   `wifi_result {ok, ssid, error}` once it has joined the new network and reached the
//!   controller, or gone back to the old one. It repeats that report (every 10 s, and
//!   after every `session.ready`) until the controller answers `wifi_commit {}`, which it
//!   does for every report, success or failure: without it the device restores its old
//!   network at its next start. Repeats are idempotent here.
//! - **State.** The device object's `wifi` is `{pending: {ssid} | null, last_result:
//!   {ok, ssid, error} | null}`, published as `device_update` whenever it changes. It is
//!   kept by the controller, not the session, so it survives the reconnect the change
//!   itself causes. A pending change has no deadline: the firmware always reports an
//!   outcome, including after a restart that restored the old network.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use echomuse_api::{
    ApiEvent, BoxFuture, DeviceStatePatch, EventPublisher, ServiceError, WifiControl,
    WifiCredentials, WifiNetwork,
};
use echomuse_core::types::DeviceId;
use echomuse_link::SessionLink;
use echomuse_protocol::{ControlBody, MessageType};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{oneshot, watch};

use crate::app::Directory;

/// How long a scan waits for the device's answer.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// `wifi_scan`.
#[derive(Serialize)]
struct WifiScan {}

impl ControlBody for WifiScan {
    const MESSAGE_TYPE: MessageType = MessageType::WifiScan;
}

/// `wifi_change`.
#[derive(Serialize)]
struct WifiChange<'a> {
    ssid: &'a str,
    psk: &'a str,
}

impl ControlBody for WifiChange<'_> {
    const MESSAGE_TYPE: MessageType = MessageType::WifiChange;
}

/// `wifi_commit`.
#[derive(Serialize)]
struct WifiCommit {}

impl ControlBody for WifiCommit {
    const MESSAGE_TYPE: MessageType = MessageType::WifiCommit;
}

/// A change's outcome as the device reported it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WifiResult {
    pub ok: bool,
    pub ssid: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct WifiState {
    /// The network a change is joining.
    pending: Option<String>,
    last_result: Option<WifiResult>,
}

impl WifiState {
    fn json(&self) -> Value {
        json!({
            "pending": self.pending.as_ref().map(|ssid| json!({ "ssid": ssid })),
            "last_result": self.last_result,
        })
    }
}

type ScanAnswer = Result<Vec<WifiNetwork>, String>;

/// The Wi-Fi state of every device and the scans waiting for answers.
pub struct WifiService {
    directory: watch::Receiver<Directory>,
    publisher: Arc<OnceLock<EventPublisher>>,
    states: Mutex<HashMap<DeviceId, WifiState>>,
    scans: Mutex<HashMap<DeviceId, Vec<oneshot::Sender<ScanAnswer>>>>,
}

impl WifiService {
    /// The service over the runtimes' `directory`.
    pub fn new(
        publisher: Arc<OnceLock<EventPublisher>>,
        directory: watch::Receiver<Directory>,
    ) -> Self {
        Self {
            directory,
            publisher,
            states: Mutex::default(),
            scans: Mutex::default(),
        }
    }

    fn link(&self, device: &DeviceId) -> Result<SessionLink, ServiceError> {
        let handle = self.directory.borrow().get(device).cloned();
        handle
            .ok_or(ServiceError::UnknownDevice)?
            .snapshot()
            .session_link
            .ok_or(ServiceError::NotConnected)
    }

    fn publish(&self, device: &DeviceId, state: &WifiState) {
        if let Some(publisher) = self.publisher.get() {
            publisher.publish(&ApiEvent::DeviceUpdate {
                device_id: device.clone(),
                state: DeviceStatePatch(Map::from_iter([("wifi".to_owned(), state.json())])),
            });
        }
    }

    /// A `wifi_result` or `wifi_scan_result` from `device`'s session `link`, in ingress
    /// order. Called on the device's runtime thread; never waits.
    pub fn device_message(
        &self,
        device: &DeviceId,
        link: &SessionLink,
        message_type: MessageType,
        body: &Map<String, Value>,
    ) {
        match message_type {
            MessageType::WifiResult => self.result(device, link, body),
            MessageType::WifiScanResult => self.scan_result(device, body),
            _ => {}
        }
    }

    fn result(&self, device: &DeviceId, link: &SessionLink, body: &Map<String, Value>) {
        // Every report is answered, so the device stops repeating it and keeps (or has
        // already restored) its configuration.
        if let Err(error) = link.send(&WifiCommit {}, 0) {
            tracing::warn!(%device, %error, "wifi_commit was not sent; the device repeats its report");
        }
        let (Some(ok), Some(ssid)) = (
            body.get("ok").and_then(Value::as_bool),
            body.get("ssid").and_then(Value::as_str),
        ) else {
            tracing::warn!(%device, "a wifi_result did not decode");
            return;
        };
        let result = WifiResult {
            ok,
            ssid: ssid.to_owned(),
            error: body
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        };
        let changed = {
            let mut states = self.states.lock().expect("wifi states lock");
            let state = states.entry(device.clone()).or_default();
            let next = WifiState {
                pending: None,
                last_result: Some(result),
            };
            (*state != next).then(|| {
                *state = next.clone();
                next
            })
        };
        if let Some(state) = changed {
            tracing::info!(%device, ok, ssid, "the device reported its Wi-Fi change");
            self.publish(device, &state);
        }
    }

    fn scan_result(&self, device: &DeviceId, body: &Map<String, Value>) {
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let answer: ScanAnswer = if error.is_empty() {
            Ok(body
                .get("networks")
                .and_then(Value::as_array)
                .map(|networks| {
                    networks
                        .iter()
                        .filter_map(|n| {
                            Some(WifiNetwork {
                                ssid: n.get("ssid")?.as_str()?.to_owned(),
                                signal: i32::try_from(n.get("signal")?.as_i64()?).ok()?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        } else {
            Err(error.to_owned())
        };
        let waiters = self
            .scans
            .lock()
            .expect("wifi scans lock")
            .remove(device)
            .unwrap_or_default();
        for waiter in waiters {
            let _ = waiter.send(answer.clone());
        }
    }

    /// The device object's `wifi`.
    pub fn state_json(&self, device: &DeviceId) -> Value {
        self.states
            .lock()
            .expect("wifi states lock")
            .get(device)
            .cloned()
            .unwrap_or_default()
            .json()
    }

    /// Forgets a deleted device.
    pub fn forget(&self, device: &DeviceId) {
        self.states.lock().expect("wifi states lock").remove(device);
        self.scans.lock().expect("wifi scans lock").remove(device);
    }

    async fn scan_now(&self, device: DeviceId) -> Result<Vec<WifiNetwork>, ServiceError> {
        let link = self.link(&device)?;
        let (reply, answer) = oneshot::channel();
        self.scans
            .lock()
            .expect("wifi scans lock")
            .entry(device.clone())
            .or_default()
            .push(reply);
        link.send(&WifiScan {}, 0)
            .map_err(|_| ServiceError::NotConnected)?;
        match tokio::time::timeout(SCAN_TIMEOUT, answer).await {
            Ok(Ok(Ok(networks))) => Ok(networks),
            Ok(Ok(Err(error))) => Err(ServiceError::Upstream(format!(
                "The device could not scan: {error}"
            ))),
            Ok(Err(_)) => Err(ServiceError::NotConnected),
            Err(_) => Err(ServiceError::Upstream(
                "The device did not answer the scan".to_owned(),
            )),
        }
    }

    fn change_now(
        &self,
        device: DeviceId,
        credentials: WifiCredentials,
    ) -> Result<(), ServiceError> {
        let link = self.link(&device)?;
        let state = {
            let mut states = self.states.lock().expect("wifi states lock");
            let state = states.entry(device.clone()).or_default();
            if let Some(ssid) = &state.pending {
                return Err(ServiceError::Busy(format!(
                    "A change to {ssid} is still in progress"
                )));
            }
            link.send(
                &WifiChange {
                    ssid: credentials.ssid(),
                    psk: credentials.psk(),
                },
                0,
            )
            .map_err(|_| ServiceError::NotConnected)?;
            state.pending = Some(credentials.ssid().to_owned());
            state.clone()
        };
        tracing::info!(%device, ssid = credentials.ssid(), "Wi-Fi change sent");
        self.publish(&device, &state);
        Ok(())
    }
}

/// [`WifiControl`] over the shared service.
pub struct WifiRoutes(pub Arc<WifiService>);

impl WifiControl for WifiRoutes {
    fn scan(&self, device: DeviceId) -> BoxFuture<'_, Result<Vec<WifiNetwork>, ServiceError>> {
        Box::pin(self.0.scan_now(device))
    }

    fn change(
        &self,
        device: DeviceId,
        credentials: WifiCredentials,
    ) -> BoxFuture<'_, Result<(), ServiceError>> {
        let outcome = self.0.change_now(device, credentials);
        Box::pin(async move { outcome })
    }

    fn fields(&self, device: &DeviceId) -> Map<String, Value> {
        Map::from_iter([("wifi".to_owned(), self.0.state_json(device))])
    }
}
