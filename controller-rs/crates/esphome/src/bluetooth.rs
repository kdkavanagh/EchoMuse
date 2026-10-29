//! The Bluetooth proxy node of a Dot (spec §3.3.2): a separate ESPHome
//! device, on the voice port + 1000, that hands Home Assistant the BLE
//! advertisements the device hears while Home Assistant is subscribed.
//!
//! It only scans passively and forwards raw advertisements; it offers no
//! active connections. Its one entity is a diagnostic sensor counting the
//! advertisements it has forwarded since it started.

use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_protocol::Platform;
use thiserror::Error;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};
use tokio_util::sync::CancellationToken;

use crate::api::{
    ApiMessage, BluetoothConnectionsFreeResponse, BluetoothLeRawAdvertisement,
    BluetoothLeRawAdvertisementsResponse, SubscribeBluetoothConnectionsFreeRequest,
    SubscribeBluetoothLeAdvertisementsRequest, UnsubscribeBluetoothLeAdvertisementsRequest,
};
use crate::entities::{self, BLE_ADVERTISEMENTS_KEY};
use crate::frame;
use crate::identity::NodeIdentity;
use crate::server::{ConnId, Core, CoreEvent, FixedReplies, ServerStatus, StartError, bind};

/// The most advertisement data one report carries: 31 bytes of
/// advertisement and 31 of scan response.
pub const MAX_ADVERTISEMENT_DATA: usize = 62;

/// Advertisements per message, ESPHome's batch size.
const ADVERTISEMENT_BATCH: usize = 16;

/// The subscription flag asking for raw advertisements.
const RAW_ADVERTISEMENTS_FLAG: u32 = 1;

/// How often, at most, the advertisement count is published.
const COUNT_PUBLISH_INTERVAL: Duration = Duration::from_secs(10);

/// Batches of advertisements queued from the handle to the node.
const COMMAND_CAPACITY: usize = 64;

/// How the runtime configures a proxy.
#[derive(Clone, Debug)]
pub struct BluetoothProxyConfig {
    /// The device it presents.
    pub device: DeviceId,
    /// The device's label.
    pub label: String,
    /// The image the device runs, which sets the model.
    pub platform: Platform,
    /// `ESPHOME_PROJECT_VERSION`.
    pub project_version: String,
    /// The address to listen on: the voice port + 1000.
    pub address: SocketAddr,
}

/// The kind of address an advertiser uses (the HCI `Address_Type`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BleAddressType {
    /// A public device address.
    Public,
    /// A random device address.
    Random,
    /// A public identity address (resolved from a private one).
    PublicIdentity,
    /// A random static identity address.
    RandomIdentity,
}

impl BleAddressType {
    /// The HCI number, `0` to `3`; `None` for any other.
    pub fn from_hci(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Public),
            1 => Some(Self::Random),
            2 => Some(Self::PublicIdentity),
            3 => Some(Self::RandomIdentity),
            _ => None,
        }
    }

    fn hci(self) -> u32 {
        match self {
            Self::Public => 0,
            Self::Random => 1,
            Self::PublicIdentity => 2,
            Self::RandomIdentity => 3,
        }
    }
}

/// The advertisement data is longer than one report can hold.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("advertisement data of {0} bytes exceeds {MAX_ADVERTISEMENT_DATA}")]
pub struct AdvertisementTooLong(pub usize);

/// One received advertisement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BleAdvertisement {
    address: [u8; 6],
    address_type: BleAddressType,
    rssi: i32,
    data: Bytes,
}

impl BleAdvertisement {
    /// An advertisement from `address`, written most significant octet
    /// first as in `AA:BB:CC:DD:EE:FF`, with `rssi` in dBm and the raw
    /// advertising data.
    pub fn new(
        address: [u8; 6],
        address_type: BleAddressType,
        rssi: i32,
        data: Bytes,
    ) -> Result<Self, AdvertisementTooLong> {
        if data.len() > MAX_ADVERTISEMENT_DATA {
            return Err(AdvertisementTooLong(data.len()));
        }
        Ok(Self {
            address,
            address_type,
            rssi,
            data,
        })
    }

    fn to_message(&self) -> BluetoothLeRawAdvertisement {
        let mut address = [0u8; 8];
        address[2..].copy_from_slice(&self.address);
        BluetoothLeRawAdvertisement {
            address: u64::from_be_bytes(address),
            rssi: self.rssi,
            address_type: self.address_type.hci(),
            data: self.data.clone(),
        }
    }
}

/// Advertisements were not taken: the node's queue was full (they are
/// replaceable, so they are dropped rather than waited for) or it stopped.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("the Bluetooth proxy did not take the advertisements")]
pub struct AdvertisementsDropped;

/// The runtime's handle on a started proxy. Dropping it stops the node
/// (without waiting); [`BluetoothProxy::shutdown`] stops it and waits.
#[derive(Debug)]
pub struct BluetoothProxy {
    adverts: mpsc::Sender<Vec<BleAdvertisement>>,
    status: watch::Receiver<ServerStatus>,
    local_addr: SocketAddr,
    shutdown: CancellationToken,
    task: Option<JoinHandle<()>>,
}

/// Starts a proxy listening on `config.address`. Must be called within a
/// Tokio runtime, on which the node then runs.
pub fn start_bluetooth_proxy(config: BluetoothProxyConfig) -> Result<BluetoothProxy, StartError> {
    let identity = NodeIdentity::bluetooth_proxy(
        &config.device,
        &config.label,
        config.platform,
        &config.project_version,
    );
    let replies = FixedReplies {
        hello: frame::encode(&identity.hello_response())?,
        device_info: frame::encode(&identity.device_info())?,
        entities: entities::bluetooth_entity_frames(&config.label)?,
    };
    let listener = bind(config.address)?;
    let local_addr = listener.local_addr().map_err(|source| StartError::Bind {
        address: config.address,
        source,
    })?;
    let shutdown = CancellationToken::new();
    let (status_tx, status_rx) = watch::channel(ServerStatus::default());
    let (advert_tx, advert_rx) = mpsc::channel(COMMAND_CAPACITY);
    let actor = Actor {
        core: Core::new(listener, replies, shutdown.child_token(), status_tx),
        adverts: advert_rx,
        subscriber: None,
        forwarded: 0,
        published: 0,
        publish_at: None,
    };
    let task = tokio::spawn(actor.run(shutdown.clone()));
    Ok(BluetoothProxy {
        adverts: advert_tx,
        status: status_rx,
        local_addr,
        shutdown,
        task: Some(task),
    })
}

impl BluetoothProxy {
    /// The address the node listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The node's connection status; `subscribed` is whether Home Assistant
    /// takes raw advertisements.
    pub fn status(&self) -> watch::Receiver<ServerStatus> {
        self.status.clone()
    }

    /// Forwards advertisements to the subscribed client. Without one they
    /// go nowhere: the node keeps no backlog.
    pub fn forward(
        &self,
        advertisements: Vec<BleAdvertisement>,
    ) -> Result<(), AdvertisementsDropped> {
        if advertisements.is_empty() {
            return Ok(());
        }
        self.adverts
            .try_send(advertisements)
            .map_err(|_| AdvertisementsDropped)
    }

    /// Stops listening, asks connected clients to disconnect and waits for
    /// the node to finish (a few seconds at most).
    pub async fn shutdown(mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for BluetoothProxy {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

struct Actor {
    core: Core,
    adverts: mpsc::Receiver<Vec<BleAdvertisement>>,
    /// The connection taking raw advertisements.
    subscriber: Option<ConnId>,
    /// Advertisements forwarded since the node started.
    forwarded: u64,
    /// The count clients were last sent.
    published: u64,
    /// When the count is next due out, while it has changed.
    publish_at: Option<Instant>,
}

impl Actor {
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            let publish_at = self.publish_at;
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                adverts = self.adverts.recv() => match adverts {
                    Some(adverts) => self.forward(&adverts),
                    None => break,
                },
                () = async { sleep_until(publish_at.expect("guarded")).await }, if publish_at.is_some() => {
                    self.publish_at = None;
                    self.publish_count();
                }
                event = self.core.next() => self.on_core(event),
            }
        }
        self.core.shutdown().await;
    }

    fn count_frame(&self) -> Bytes {
        // A count stays exact in an f32 up to 2^24; past that the sensor
        // rounds, which a diagnostic tolerates.
        entities::sensor_frame(BLE_ADVERTISEMENTS_KEY, Some(self.forwarded as f32))
    }

    fn publish_count(&mut self) {
        if self.published != self.forwarded {
            self.published = self.forwarded;
            let frame = self.count_frame();
            self.core.send_to_state_subscribers(&frame);
        }
    }

    fn forward(&mut self, adverts: &[BleAdvertisement]) {
        let Some(conn) = self.subscriber else {
            return;
        };
        for batch in adverts.chunks(ADVERTISEMENT_BATCH) {
            let message = BluetoothLeRawAdvertisementsResponse {
                advertisements: batch.iter().map(BleAdvertisement::to_message).collect(),
            };
            let frame = frame::encode(&message).expect("16 advertisements fit a frame");
            if !self.core.send(conn, frame) {
                return;
            }
            self.forwarded += batch.len() as u64;
        }
        if self.publish_at.is_none() {
            self.publish_at = Some(Instant::now() + COUNT_PUBLISH_INTERVAL);
        }
    }

    fn on_core(&mut self, event: CoreEvent) {
        match event {
            CoreEvent::SubscribedStates(conn) => {
                let frame = self.count_frame();
                self.core.send(conn, frame);
            }
            CoreEvent::Message(conn, frame) => match frame.message_type {
                SubscribeBluetoothLeAdvertisementsRequest::ID => {
                    let Ok(request) = frame.decode::<SubscribeBluetoothLeAdvertisementsRequest>()
                    else {
                        self.core.close(conn, crate::server::CloseReason::Malformed);
                        return;
                    };
                    // Parsed advertisements (no raw flag) were removed from
                    // ESPHome in 2025.8 and are not offered here; a client
                    // reading the raw-advertisement feature flag asks for raw.
                    if request.flags & RAW_ADVERTISEMENTS_FLAG != 0 {
                        // The newest subscriber wins, as in ESPHome: an
                        // older one is almost always a dead connection.
                        self.subscriber = Some(conn);
                        self.core.set_subscribed(true);
                    }
                }
                UnsubscribeBluetoothLeAdvertisementsRequest::ID => {
                    if self.subscriber == Some(conn) {
                        self.subscriber = None;
                        self.core.set_subscribed(false);
                    }
                }
                // The proxy offers no active connections, so it has none free.
                SubscribeBluetoothConnectionsFreeRequest::ID => {
                    let reply = BluetoothConnectionsFreeResponse {
                        free: 0,
                        limit: 0,
                        allocated: Vec::new(),
                    };
                    self.core.send(
                        conn,
                        frame::encode(&reply).expect("an empty reply fits a frame"),
                    );
                }
                // Connection, GATT and scanner-mode requests need features
                // the proxy does not advertise.
                _ => {}
            },
            CoreEvent::Closed(conn) => {
                if self.subscriber == Some(conn) {
                    self.subscriber = None;
                    self.core.set_subscribed(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_cross_as_big_endian_integers() {
        let advert = BleAdvertisement::new(
            [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
            BleAddressType::Random,
            -71,
            Bytes::from_static(&[0x02, 0x01, 0x06]),
        )
        .unwrap();
        let message = advert.to_message();
        assert_eq!(message.address, 0xAABB_CCDD_EEFF);
        assert_eq!(message.address_type, 1);
        assert_eq!(message.rssi, -71);
    }

    #[test]
    fn oversized_data_is_refused() {
        assert_eq!(
            BleAdvertisement::new(
                [0; 6],
                BleAddressType::Public,
                -50,
                Bytes::from(vec![0; 63])
            ),
            Err(AdvertisementTooLong(63))
        );
        assert!(
            BleAdvertisement::new(
                [0; 6],
                BleAddressType::Public,
                -50,
                Bytes::from(vec![0; 62])
            )
            .is_ok()
        );
        assert_eq!(BleAddressType::from_hci(4), None);
    }
}
