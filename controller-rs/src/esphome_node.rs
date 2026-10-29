//! The ESPHome nodes a device runtime keeps for its device (spec §3.3.1, §3.3.2).
//!
//! A device's voice satellite (and, on a Dot whose proxy is switched on, its Bluetooth
//! proxy) listens only while the device has a session attached. Its mDNS record is
//! published the first time the node starts in this process and stays published after
//! the device disconnects, so Home Assistant keeps the node's address while the device
//! is away. The record cannot be published earlier: the node's model comes from the
//! platform the device names in its hello, and the database does not store it.
//!
//! Everything here is owned by the device runtime's thread. Slow steps (the port lookup
//! in the database, binding, registering the record, waiting for an old node to let go
//! of its port) run as tasks on that thread whose results the runtime checks against
//! the session they were started for.

use std::{
    collections::BTreeMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    num::NonZeroU16,
    sync::Arc,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use echomuse_core::types::{DeviceId, SessionId};

use crate::supervision::RuntimeTasks;
use echomuse_db::{
    DbHandle,
    devices::{EnsureBleProxyPort, EnsureEsphomePort},
};
use echomuse_discovery::{
    DiscoveryHandle, EsphomeAdvertisement, EsphomeNodeKind, project_name, voice_friendly_name,
};
use echomuse_esphome::{
    bluetooth::{
        BleAddressType, BleAdvertisement, BluetoothProxy, BluetoothProxyConfig,
        start_bluetooth_proxy,
    },
    entities::VoiceEntities,
    identity::{bluetooth_proxy_friendly_name, model},
    voice::{RunId, SatelliteRequest, VoiceSatellite, VoiceSatelliteConfig, start_voice_satellite},
};
use echomuse_protocol::Platform;
use echomuse_voice::SatelliteCommand;
use serde_json::{Map, Value};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// Home Assistant requests a satellite may queue for the runtime before the node closes
/// the connection that sent them.
const SATELLITE_REQUEST_CAPACITY: usize = 32;
/// Commands queued from the runtime to a satellite's responder task.
const VOICE_COMMAND_CAPACITY: usize = 32;
/// How long a node that is stopping may hold its port before a replacement binds.
const NODE_STOP_WAIT: Duration = Duration::from_secs(5);

/// What the runtime needs to run its device's ESPHome nodes.
pub struct EsphomeServices {
    /// Port assignment (`devices.esphome_api_port`, `ble_proxy_port`).
    pub db: DbHandle,
    /// The mDNS records.
    pub discovery: DiscoveryHandle,
    /// The address the nodes bind (`SERVER_HOST`).
    pub bind_host: IpAddr,
    /// `ESPHOME_PROJECT_VERSION`.
    pub project_version: String,
}

/// What the attached device asks of its nodes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Wanted {
    /// The device's label, or its id while it has none.
    pub label: String,
    pub platform: Platform,
    pub entities: VoiceEntities,
    /// The device announced `ble_proxy` and its `bleProxyEnabled` setting is on.
    pub bluetooth_proxy: bool,
}

/// The nodes' state, as the device snapshot shows it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EsphomeState {
    /// The voice satellite's port; `None` until it is known.
    pub voice_port: Option<u16>,
    /// Where the satellite listens; `None` while it is not listening.
    pub voice_listening: Option<SocketAddr>,
    /// The satellite's mDNS record was published by this process.
    pub voice_advertised: bool,
    /// The Bluetooth proxy's port; `None` while the device has none.
    pub proxy_port: Option<u16>,
    /// Where the proxy listens; `None` while it is not listening.
    pub proxy_listening: Option<SocketAddr>,
    /// The proxy's mDNS record is published.
    pub proxy_advertised: bool,
    /// The most recent failure to start or advertise a node.
    pub last_error: Option<String>,
    /// Home Assistant requests the runtime cannot carry out yet, by kind. Each was
    /// answered as unavailable where the ESPHome protocol has an answer.
    pub refused: BTreeMap<&'static str, u64>,
    /// Advertisements the proxy did not take because its queue was full.
    pub adverts_dropped: u64,
}

/// One started satellite, served by its responder task.
struct RunningVoice {
    wanted: (String, Platform, VoiceEntities),
    commands: mpsc::Sender<VoiceCommand>,
    requests: mpsc::Receiver<SatelliteRequest>,
    task: JoinHandle<()>,
}

struct RunningProxy {
    wanted: (String, Platform),
    proxy: BluetoothProxy,
}

/// What the runtime asks of a satellite. The responder task owns the satellite, so the
/// runtime never waits on it.
enum VoiceCommand {
    AmbientLight(Option<f32>),
    /// From the voice machine; `started` receives a run's id for `StartRun`.
    Voice {
        command: SatelliteCommand,
        started: Option<oneshot::Sender<Result<RunId, String>>>,
    },
}

/// The outcome of a preparation task, for the session it was started for.
pub(crate) struct Prepared {
    pub session: SessionId,
    voice: Option<Result<StartedVoice, String>>,
    proxy: Option<Result<StartedProxy, String>>,
}

struct StartedVoice {
    wanted: (String, Platform, VoiceEntities),
    satellite: VoiceSatellite,
    requests: mpsc::Receiver<SatelliteRequest>,
    port: u16,
    advertised: Result<(), String>,
}

struct StartedProxy {
    wanted: (String, Platform),
    proxy: BluetoothProxy,
    port: u16,
    advertised: Result<(), String>,
}

/// The device's nodes, owned by its runtime.
pub(crate) struct EsphomeNodes {
    services: Option<Arc<EsphomeServices>>,
    device: DeviceId,
    voice: Option<RunningVoice>,
    proxy: Option<RunningProxy>,
    /// Nodes told to stop whose ports may still be bound.
    stopping: Vec<JoinHandle<()>>,
    /// Spawns the nodes' tasks; a panic in one fails the runtime (§5.5).
    tasks: RuntimeTasks,
}

impl EsphomeNodes {
    pub(crate) fn new(
        device: DeviceId,
        services: Option<Arc<EsphomeServices>>,
        tasks: RuntimeTasks,
    ) -> Self {
        Self {
            services,
            device,
            voice: None,
            proxy: None,
            stopping: Vec::new(),
            tasks,
        }
    }

    /// The satellite's request channel while it runs.
    pub(crate) fn requests(&mut self) -> Option<&mut mpsc::Receiver<SatelliteRequest>> {
        self.voice.as_mut().map(|voice| &mut voice.requests)
    }

    /// A session attached. Nodes already running for the same identity keep running;
    /// anything else is stopped, and what is missing is prepared by the returned task.
    pub(crate) fn on_attach(
        &mut self,
        session: SessionId,
        wanted: Wanted,
        state: &mut EsphomeState,
    ) -> Option<impl Future<Output = Prepared> + Send + use<>> {
        let services = Arc::clone(self.services.as_ref()?);
        let voice_key = (wanted.label.clone(), wanted.platform, wanted.entities);
        if self.voice.as_ref().is_some_and(|v| v.wanted != voice_key) {
            self.stop_voice(state);
        }
        let proxy_key = (wanted.label.clone(), wanted.platform);
        let proxy_wanted = wanted.bluetooth_proxy.then_some(proxy_key);
        if self
            .proxy
            .as_ref()
            .is_some_and(|p| Some(&p.wanted) != proxy_wanted.as_ref())
        {
            self.stop_proxy(state);
        }
        let start_voice = self.voice.is_none().then_some(voice_key);
        let start_proxy = proxy_wanted.filter(|_| self.proxy.is_none());
        let withdraw_proxy = !wanted.bluetooth_proxy && state.proxy_advertised;
        if withdraw_proxy {
            state.proxy_advertised = false;
        }
        if start_voice.is_none() && start_proxy.is_none() && !withdraw_proxy {
            return None;
        }
        let stopping = std::mem::take(&mut self.stopping);
        let device = self.device.clone();
        Some(prepare(
            services,
            device,
            session,
            stopping,
            start_voice,
            start_proxy,
            withdraw_proxy,
        ))
    }

    /// Installs what a preparation task started, if its session is still the attached
    /// one; otherwise the started nodes are stopped again.
    pub(crate) fn install(
        &mut self,
        prepared: Prepared,
        attached: Option<SessionId>,
        state: &mut EsphomeState,
    ) {
        let current = attached == Some(prepared.session);
        if let Some(voice) = prepared.voice {
            match voice {
                Ok(started) => {
                    state.voice_port = Some(started.port);
                    match started.advertised {
                        Ok(()) => state.voice_advertised = true,
                        Err(error) => self.failed(state, error),
                    }
                    if current && self.voice.is_none() {
                        let address = started.satellite.local_addr();
                        tracing::info!(device = %self.device, %address, "voice satellite listening");
                        state.voice_listening = Some(address);
                        let (commands, receiver) = mpsc::channel(VOICE_COMMAND_CAPACITY);
                        let task = self
                            .tasks
                            .spawn("voice satellite", serve_voice(started.satellite, receiver));
                        self.voice = Some(RunningVoice {
                            wanted: started.wanted,
                            commands,
                            requests: started.requests,
                            task,
                        });
                    } else {
                        self.stopping.push(
                            self.tasks
                                .spawn("voice satellite stop", started.satellite.shutdown()),
                        );
                    }
                }
                Err(error) => self.failed(state, error),
            }
        }
        if let Some(proxy) = prepared.proxy {
            match proxy {
                Ok(started) => {
                    state.proxy_port = Some(started.port);
                    match started.advertised {
                        Ok(()) => state.proxy_advertised = true,
                        Err(error) => self.failed(state, error),
                    }
                    if current && self.proxy.is_none() {
                        let address = started.proxy.local_addr();
                        tracing::info!(device = %self.device, %address, "Bluetooth proxy listening");
                        state.proxy_listening = Some(address);
                        self.proxy = Some(RunningProxy {
                            wanted: started.wanted,
                            proxy: started.proxy,
                        });
                    } else {
                        self.stopping.push(
                            self.tasks
                                .spawn("Bluetooth proxy stop", started.proxy.shutdown()),
                        );
                    }
                }
                Err(error) => self.failed(state, error),
            }
        }
    }

    fn failed(&self, state: &mut EsphomeState, error: String) {
        tracing::warn!(device = %self.device, %error, "ESPHome node problem");
        state.last_error = Some(error);
    }

    /// The session detached: the listeners stop, the records stay.
    pub(crate) fn on_detach(&mut self, state: &mut EsphomeState) {
        self.stop_voice(state);
        self.stop_proxy(state);
    }

    /// The satellite's request channel closed on its own: the node stopped.
    pub(crate) fn voice_ended(&mut self, state: &mut EsphomeState) {
        self.failed(state, "the voice satellite stopped unexpectedly".to_owned());
        self.stop_voice(state);
    }

    fn stop_voice(&mut self, state: &mut EsphomeState) {
        if let Some(voice) = self.voice.take() {
            tracing::info!(device = %self.device, "voice satellite stopping");
            // Dropping the command sender ends the responder, which shuts the node down.
            drop(voice.commands);
            drop(voice.requests);
            self.stopping.push(voice.task);
        }
        state.voice_listening = None;
    }

    fn stop_proxy(&mut self, state: &mut EsphomeState) {
        if let Some(proxy) = self.proxy.take() {
            tracing::info!(device = %self.device, "Bluetooth proxy stopping");
            self.stopping.push(
                self.tasks
                    .spawn("Bluetooth proxy stop", proxy.proxy.shutdown()),
            );
        }
        state.proxy_listening = None;
    }

    /// Carries out the voice machine's request of the satellite. `false` when no
    /// satellite runs or its queue is full (its states are level-triggered and the next
    /// change resends them; a run start reports its failure through `started`).
    pub(crate) fn satellite(
        &self,
        command: SatelliteCommand,
        started: Option<oneshot::Sender<Result<RunId, String>>>,
    ) -> bool {
        match &self.voice {
            Some(voice) => match voice
                .commands
                .try_send(VoiceCommand::Voice { command, started })
            {
                Ok(()) => true,
                Err(error) => {
                    if let VoiceCommand::Voice {
                        started: Some(started),
                        ..
                    } = error.into_inner()
                    {
                        let _ = started.send(Err("the satellite's queue is full".to_owned()));
                    }
                    false
                }
            },
            None => {
                if let Some(started) = started {
                    let _ = started.send(Err("no satellite is running".to_owned()));
                }
                false
            }
        }
    }

    /// The latest ambient light reading, for the satellite's sensor.
    pub(crate) fn set_ambient_light(&self, lux: Option<f32>) {
        if let Some(voice) = &self.voice {
            // A full queue only delays a reading the next report replaces.
            let _ = voice.commands.try_send(VoiceCommand::AmbientLight(lux));
        }
    }

    /// Forwards `ble_adverts` to the proxy. `None` when no proxy runs.
    pub(crate) fn forward_adverts(&self, adverts: Vec<BleAdvertisement>) -> Option<bool> {
        let proxy = self.proxy.as_ref()?;
        Some(proxy.proxy.forward(adverts).is_ok())
    }

    /// Stops every node and waits for them, up to `deadline`.
    pub(crate) async fn shutdown(mut self, deadline: Duration) {
        let mut ignored = EsphomeState::default();
        self.on_detach(&mut ignored);
        let stopping = std::mem::take(&mut self.stopping);
        let _ = tokio::time::timeout(deadline, async {
            for task in stopping {
                let _ = task.await;
            }
        })
        .await;
    }
}

async fn serve_voice(satellite: VoiceSatellite, mut commands: mpsc::Receiver<VoiceCommand>) {
    while let Some(command) = commands.recv().await {
        match command {
            VoiceCommand::AmbientLight(lux) => {
                // The entity exists only for devices that announced the sensor.
                let _ = satellite.set_ambient_light(lux);
            }
            VoiceCommand::Voice { command, started } => {
                serve_satellite_command(&satellite, command, started).await;
            }
        }
    }
    satellite.shutdown().await;
}

/// One voice-machine request. Home Assistant may have stopped waiting for an answer or
/// a run already; those outcomes change nothing here and are logged at debug level.
async fn serve_satellite_command(
    satellite: &VoiceSatellite,
    command: SatelliteCommand,
    started: Option<oneshot::Sender<Result<RunId, String>>>,
) {
    match command {
        SatelliteCommand::VoiceState(state) => satellite.set_voice_state(state),
        SatelliteCommand::Media(media) => satellite.set_media(media),
        SatelliteCommand::AlertRinging(ringing) => satellite.set_alert_ringing(ringing),
        SatelliteCommand::ButtonEvent(event) => {
            if let Err(error) = satellite.fire_button_event(event).await {
                tracing::debug!(?error, "a button event reached no one");
            }
        }
        SatelliteCommand::FinishAnnouncement { id, success } => {
            if let Err(error) = satellite.finish_announcement(id, success).await {
                tracing::debug!(?error, "an announcement's answer was not taken");
            }
        }
        SatelliteCommand::StartRun { conversation, .. } => {
            let result = satellite
                .start_run(conversation)
                .await
                .map_err(|error| format!("{error:?}"));
            if let Some(started) = started {
                let _ = started.send(result);
            }
        }
        SatelliteCommand::RunAudio { run, pcm } => {
            let _ = satellite.send_run_audio(run, &pcm).await;
        }
        SatelliteCommand::EndRunAudio(run) => {
            let _ = satellite.end_run_audio(run).await;
        }
        SatelliteCommand::AbortRun(run) => {
            let _ = satellite.abort_run(run).await;
        }
        SatelliteCommand::FinishRunResponse { run, success } => {
            if let Err(error) = satellite.finish_run_response(run, success).await {
                tracing::debug!(?error, "a run's response answer was not taken");
            }
        }
    }
}

async fn prepare(
    services: Arc<EsphomeServices>,
    device: DeviceId,
    session: SessionId,
    stopping: Vec<JoinHandle<()>>,
    voice: Option<(String, Platform, VoiceEntities)>,
    proxy: Option<(String, Platform)>,
    withdraw_proxy: bool,
) -> Prepared {
    // A node that is stopping still holds its port until its task ends.
    let _ = tokio::time::timeout(NODE_STOP_WAIT, async {
        for task in stopping {
            let _ = task.await;
        }
    })
    .await;
    let voice = match voice {
        None => None,
        Some(wanted) => Some(start_voice(&services, &device, wanted).await),
    };
    let proxy = match proxy {
        None => None,
        Some(wanted) => start_proxy(&services, &device, wanted).await,
    };
    if withdraw_proxy
        && let Err(error) = services
            .discovery
            .unregister_esphome(device.clone(), EsphomeNodeKind::BluetoothProxy)
            .await
    {
        tracing::warn!(%device, %error, "the Bluetooth proxy record could not be withdrawn");
    }
    Prepared {
        session,
        voice,
        proxy,
    }
}

fn port(port: u16) -> Result<NonZeroU16, String> {
    NonZeroU16::new(port).ok_or_else(|| "the database holds ESPHome port 0".to_owned())
}

async fn start_voice(
    services: &EsphomeServices,
    device: &DeviceId,
    wanted: (String, Platform, VoiceEntities),
) -> Result<StartedVoice, String> {
    let assigned = services
        .db
        .write(EnsureEsphomePort {
            device: device.clone(),
        })
        .await
        .map_err(|error| format!("no ESPHome port for the voice satellite: {error}"))?;
    let (label, platform, entities) = wanted.clone();
    let (satellite, requests) = start_voice_satellite(VoiceSatelliteConfig {
        device: device.clone(),
        label: label.clone(),
        platform,
        project_version: services.project_version.clone(),
        address: SocketAddr::new(services.bind_host, assigned),
        entities,
        request_capacity: SATELLITE_REQUEST_CAPACITY,
    })
    .map_err(|error| format!("the voice satellite could not start: {error}"))?;
    let advertised = services
        .discovery
        .register_esphome(EsphomeAdvertisement {
            device: device.clone(),
            kind: EsphomeNodeKind::VoiceSatellite,
            friendly_name: voice_friendly_name(&label),
            port: port(assigned)?,
            project_name: project_name(model(platform)),
            project_version: services.project_version.clone(),
        })
        .await
        .map_err(|error| format!("the voice satellite record was not published: {error}"));
    Ok(StartedVoice {
        wanted,
        satellite,
        requests,
        port: assigned,
        advertised,
    })
}

async fn start_proxy(
    services: &EsphomeServices,
    device: &DeviceId,
    wanted: (String, Platform),
) -> Option<Result<StartedProxy, String>> {
    let assigned = match services
        .db
        .write(EnsureBleProxyPort {
            device: device.clone(),
        })
        .await
    {
        // The proxy's port follows the voice port; without one there is no proxy.
        Ok(None) => return None,
        Ok(Some(port)) => port,
        Err(error) => {
            return Some(Err(format!(
                "no ESPHome port for the Bluetooth proxy: {error}"
            )));
        }
    };
    let (label, platform) = wanted.clone();
    let started = start_bluetooth_proxy(BluetoothProxyConfig {
        device: device.clone(),
        label: label.clone(),
        platform,
        project_version: services.project_version.clone(),
        address: SocketAddr::new(services.bind_host, assigned),
    })
    .map_err(|error| format!("the Bluetooth proxy could not start: {error}"));
    let proxy = match started {
        Ok(proxy) => proxy,
        Err(error) => return Some(Err(error)),
    };
    let advertised = match port(assigned) {
        Ok(nonzero) => services
            .discovery
            .register_esphome(EsphomeAdvertisement {
                device: device.clone(),
                kind: EsphomeNodeKind::BluetoothProxy,
                friendly_name: bluetooth_proxy_friendly_name(&label),
                port: nonzero,
                project_name: project_name(model(platform)),
                project_version: services.project_version.clone(),
            })
            .await
            .map_err(|error| format!("the Bluetooth proxy record was not published: {error}")),
        Err(error) => Err(error),
    };
    Some(Ok(StartedProxy {
        wanted,
        proxy,
        port: assigned,
        advertised,
    }))
}

/// Decodes a `ble_adverts` body, `{"adverts":[{"addr","addrType","rssi","data"}]}`
/// (`device/internal/bluetooth/hci.go`), skipping entries that do not decode. Returns
/// the advertisements and how many entries were skipped.
pub(crate) fn decode_adverts(body: &Map<String, Value>) -> (Vec<BleAdvertisement>, usize) {
    let Some(entries) = body.get("adverts").and_then(Value::as_array) else {
        return (Vec::new(), 0);
    };
    let mut adverts = Vec::with_capacity(entries.len());
    let mut skipped = 0;
    for entry in entries {
        match decode_advert(entry) {
            Some(advert) => adverts.push(advert),
            None => skipped += 1,
        }
    }
    (adverts, skipped)
}

fn decode_advert(entry: &Value) -> Option<BleAdvertisement> {
    let address = parse_mac(entry.get("addr")?.as_str()?)?;
    let address_type =
        BleAddressType::from_hci(u8::try_from(entry.get("addrType")?.as_u64()?).ok()?)?;
    let rssi = i32::try_from(entry.get("rssi")?.as_i64()?).ok()?;
    let data = STANDARD.decode(entry.get("data")?.as_str()?).ok()?;
    BleAdvertisement::new(address, address_type, rssi, Bytes::from(data)).ok()
}

fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut octets = [0u8; 6];
    let mut parts = text.split(':');
    for octet in &mut octets {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *octet = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(octets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adverts_decode_as_the_firmware_writes_them() {
        let body = json!({"adverts": [
            {"addr": "AA:BB:CC:DD:EE:FF", "addrType": 1, "rssi": -60, "data": "AgE="},
            {"addr": "not a mac", "addrType": 0, "rssi": -60, "data": ""},
        ]});
        let (adverts, skipped) = decode_adverts(body.as_object().unwrap());
        assert_eq!(adverts.len(), 1);
        assert_eq!(skipped, 1);
        assert_eq!(
            adverts[0],
            BleAdvertisement::new(
                [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
                BleAddressType::Random,
                -60,
                Bytes::from_static(&[2, 1]),
            )
            .unwrap()
        );
    }
}
