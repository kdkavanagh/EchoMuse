//! The discovery service: one task that owns the mDNS responder and every
//! record it publishes.
//!
//! Callers hold a [`DiscoveryHandle`] and send it messages; only the service
//! task touches the responder. The controller record is announced at start and
//! again every [`REANNOUNCE_INTERVAL`]. ESPHome records are registered and
//! withdrawn on request. Shutdown withdraws every record, sending goodbye
//! packets, and stops the responder within the caller's deadline.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::time::Duration;

use echomuse_core::types::DeviceId;
use mdns_sd::{IfKind, ServiceDaemon, UnregisterStatus};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};

use crate::esphome::{EsphomeAdvertisement, EsphomeNodeKind};
use crate::records::{ControllerAdvertisement, ServiceRecord};

/// How often the controller record is announced again.
pub const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(120);

/// How long shutdown may take when every handle is dropped without an
/// explicit [`DiscoveryHandle::shutdown`].
pub const IMPLICIT_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(2);

/// How many messages may wait for the service task.
const COMMAND_CAPACITY: usize = 64;

/// A discovery failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DiscoveryError {
    /// The responder refused or could not build the record.
    #[error("mDNS responder error: {0}")]
    Responder(String),
    /// The responder's command queue was full.
    #[error("mDNS responder is busy")]
    Busy,
    /// The service or its responder has stopped.
    #[error("discovery service has stopped")]
    Stopped,
    /// Shutdown did not finish within its deadline.
    #[error("discovery shutdown did not finish within {0:?}")]
    ShutdownTimedOut(Duration),
}

impl From<mdns_sd::Error> for DiscoveryError {
    fn from(error: mdns_sd::Error) -> Self {
        match error {
            mdns_sd::Error::Again => Self::Busy,
            mdns_sd::Error::DaemonShutdown => Self::Stopped,
            other => Self::Responder(other.to_string()),
        }
    }
}

/// What withdrawing a record found.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Withdrawn {
    /// The record was published and is now withdrawn.
    Removed,
    /// No such record was published.
    NotPublished,
}

/// The outcome of withdrawing a record, delivered later by the responder.
pub(crate) type WithdrawFuture =
    Pin<Box<dyn Future<Output = Result<Withdrawn, DiscoveryError>> + Send>>;

/// The responder's confirmation that it has stopped.
pub(crate) type StopFuture = Pin<Box<dyn Future<Output = Result<(), DiscoveryError>> + Send>>;

/// The operations the service needs from an mDNS responder.
pub(crate) trait Responder: Send + 'static {
    /// Answers and announces only on the interface that carries `address`, so records
    /// never go out on networks the advertised address does not belong to.
    fn serve_address(&mut self, address: Ipv4Addr) -> Result<(), DiscoveryError>;
    /// Publishes or republishes `record`, announcing it.
    fn announce(&mut self, record: &ServiceRecord) -> Result<(), DiscoveryError>;
    /// Withdraws the record named `fullname`, sending a goodbye.
    fn withdraw(&mut self, fullname: &str) -> Result<WithdrawFuture, DiscoveryError>;
    /// Stops the responder.
    fn stop(&mut self) -> Result<StopFuture, DiscoveryError>;
}

/// The responder backed by an `mdns-sd` daemon thread.
struct MdnsResponder {
    daemon: ServiceDaemon,
}

impl Responder for MdnsResponder {
    fn serve_address(&mut self, address: Ipv4Addr) -> Result<(), DiscoveryError> {
        self.daemon.disable_interface(IfKind::All)?;
        Ok(self
            .daemon
            .enable_interface(IfKind::Addr(IpAddr::V4(address)))?)
    }

    fn announce(&mut self, record: &ServiceRecord) -> Result<(), DiscoveryError> {
        Ok(self.daemon.register(record.to_service_info()?)?)
    }

    fn withdraw(&mut self, fullname: &str) -> Result<WithdrawFuture, DiscoveryError> {
        let status = self.daemon.unregister(fullname)?;
        Ok(Box::pin(async move {
            match status.recv_async().await {
                Ok(UnregisterStatus::OK) => Ok(Withdrawn::Removed),
                Ok(UnregisterStatus::NotFound) => Ok(Withdrawn::NotPublished),
                Err(_) => Err(DiscoveryError::Stopped),
            }
        }))
    }

    fn stop(&mut self) -> Result<StopFuture, DiscoveryError> {
        let status = self.daemon.shutdown()?;
        Ok(Box::pin(async move {
            // The daemon replies once its thread is about to exit; a closed
            // channel means it has already gone.
            let _ = status.recv_async().await;
            Ok(())
        }))
    }
}

enum Command {
    Register(
        EsphomeAdvertisement,
        oneshot::Sender<Result<(), DiscoveryError>>,
    ),
    Unregister(
        DeviceId,
        EsphomeNodeKind,
        oneshot::Sender<Result<Withdrawn, DiscoveryError>>,
    ),
    UpdateController(
        ControllerAdvertisement,
        oneshot::Sender<Result<(), DiscoveryError>>,
    ),
    Shutdown(Duration, oneshot::Sender<Result<(), DiscoveryError>>),
}

/// The state the service task reports after each re-announcement.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryHealth {
    /// The error from the most recent re-announcement of the controller
    /// record; `None` after a success or before the first one.
    pub last_reannounce_error: Option<DiscoveryError>,
}

/// A cheap, cloneable way to send the discovery service messages.
#[derive(Clone, Debug)]
pub struct DiscoveryHandle {
    commands: mpsc::Sender<Command>,
    health: tokio::sync::watch::Receiver<DiscoveryHealth>,
}

impl DiscoveryHandle {
    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, DiscoveryError>>) -> Command,
    ) -> Result<T, DiscoveryError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(command(reply))
            .await
            .map_err(|_| DiscoveryError::Stopped)?;
        response.await.map_err(|_| DiscoveryError::Stopped)?
    }

    /// Publishes an ESPHome node at the controller's address, replacing any
    /// earlier record for the same device and kind.
    pub async fn register_esphome(
        &self,
        advertisement: EsphomeAdvertisement,
    ) -> Result<(), DiscoveryError> {
        self.request(|reply| Command::Register(advertisement, reply))
            .await
    }

    /// Withdraws a device's ESPHome node.
    pub async fn unregister_esphome(
        &self,
        device: DeviceId,
        kind: EsphomeNodeKind,
    ) -> Result<Withdrawn, DiscoveryError> {
        self.request(|reply| Command::Unregister(device, kind, reply))
            .await
    }

    /// Replaces the controller record, for example when the TLS listener
    /// starts or stops. ESPHome records move to the new address.
    pub async fn update_controller(
        &self,
        controller: ControllerAdvertisement,
    ) -> Result<(), DiscoveryError> {
        self.request(|reply| Command::UpdateController(controller, reply))
            .await
    }

    /// Withdraws every record and stops the responder. The service finishes
    /// within `deadline` whether or not the responder confirms.
    pub async fn shutdown(&self, deadline: Duration) -> Result<(), DiscoveryError> {
        self.request(|reply| Command::Shutdown(deadline, reply))
            .await
    }

    /// The service's latest health report.
    pub fn health(&self) -> tokio::sync::watch::Receiver<DiscoveryHealth> {
        self.health.clone()
    }
}

/// Starts the discovery service on the current Tokio runtime. The mDNS responder
/// serves only the IPv4 interface that carries the controller's address, and
/// announces `controller` before returning.
pub fn start(
    controller: ControllerAdvertisement,
) -> Result<(DiscoveryHandle, JoinHandle<()>), DiscoveryError> {
    let daemon = ServiceDaemon::new()?;
    spawn(MdnsResponder { daemon }, controller, REANNOUNCE_INTERVAL)
}

pub(crate) fn spawn<R: Responder>(
    mut responder: R,
    controller: ControllerAdvertisement,
    reannounce_interval: Duration,
) -> Result<(DiscoveryHandle, JoinHandle<()>), DiscoveryError> {
    responder.serve_address(controller.address)?;
    responder.announce(&controller.record())?;
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    let (health_sender, health) = tokio::sync::watch::channel(DiscoveryHealth::default());
    let service = Service {
        responder,
        controller,
        esphome: HashMap::new(),
        health: health_sender,
    };
    let task = tokio::spawn(service.run(receiver, reannounce_interval));
    Ok((DiscoveryHandle { commands, health }, task))
}

struct Service<R> {
    responder: R,
    controller: ControllerAdvertisement,
    esphome: HashMap<(DeviceId, EsphomeNodeKind), EsphomeAdvertisement>,
    health: tokio::sync::watch::Sender<DiscoveryHealth>,
}

impl<R: Responder> Service<R> {
    async fn run(mut self, mut commands: mpsc::Receiver<Command>, reannounce_interval: Duration) {
        let mut reannounce =
            tokio::time::interval_at(Instant::now() + reannounce_interval, reannounce_interval);
        reannounce.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                command = commands.recv() => match command {
                    Some(Command::Register(advertisement, reply)) => {
                        let _ = reply.send(self.register(advertisement));
                    }
                    Some(Command::Unregister(device, kind, reply)) => {
                        let _ = reply.send(self.unregister(device, kind).await);
                    }
                    Some(Command::UpdateController(controller, reply)) => {
                        let _ = reply.send(self.update_controller(controller).await);
                    }
                    Some(Command::Shutdown(deadline, reply)) => {
                        let _ = reply.send(self.shutdown(deadline).await);
                        return;
                    }
                    None => {
                        let _ = self.shutdown(IMPLICIT_SHUTDOWN_DEADLINE).await;
                        return;
                    }
                },
                _ = reannounce.tick() => {
                    let last_reannounce_error = self.responder.announce(&self.controller.record()).err();
                    self.health.send_replace(DiscoveryHealth { last_reannounce_error });
                }
            }
        }
    }

    fn register(&mut self, advertisement: EsphomeAdvertisement) -> Result<(), DiscoveryError> {
        self.responder
            .announce(&advertisement.record(self.controller.address))?;
        self.esphome.insert(
            (advertisement.device.clone(), advertisement.kind),
            advertisement,
        );
        Ok(())
    }

    async fn unregister(
        &mut self,
        device: DeviceId,
        kind: EsphomeNodeKind,
    ) -> Result<Withdrawn, DiscoveryError> {
        let Some(advertisement) = self.esphome.remove(&(device, kind)) else {
            return Ok(Withdrawn::NotPublished);
        };
        let fullname = advertisement.record(self.controller.address).fullname();
        self.responder.withdraw(&fullname)?.await
    }

    async fn update_controller(
        &mut self,
        controller: ControllerAdvertisement,
    ) -> Result<(), DiscoveryError> {
        let old = self.controller.record();
        let new = controller.record();
        if old.fullname() != new.fullname() {
            self.responder.withdraw(&old.fullname())?.await?;
        }
        let address_changed = self.controller.address != controller.address;
        if address_changed {
            self.responder.serve_address(controller.address)?;
        }
        self.responder.announce(&new)?;
        self.controller = controller;
        if address_changed {
            for advertisement in self.esphome.values() {
                self.responder
                    .announce(&advertisement.record(self.controller.address))?;
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self, deadline: Duration) -> Result<(), DiscoveryError> {
        let address = self.controller.address;
        let mut fullnames: Vec<String> = self
            .esphome
            .drain()
            .map(|(_, advertisement)| advertisement.record(address).fullname())
            .collect();
        fullnames.push(self.controller.record().fullname());
        let responder = &mut self.responder;
        let work = async move {
            // Every withdrawal is attempted even if an earlier one fails; the
            // first failure is reported.
            let mut first_error = None;
            for fullname in fullnames {
                let outcome = match responder.withdraw(&fullname) {
                    Ok(pending) => pending.await.map(drop),
                    Err(error) => Err(error),
                };
                if let Err(error) = outcome {
                    first_error.get_or_insert(error);
                }
            }
            let stopped = match responder.stop() {
                Ok(pending) => pending.await,
                Err(error) => Err(error),
            };
            match (first_error, stopped) {
                (Some(error), _) | (None, Err(error)) => Err(error),
                (None, Ok(())) => Ok(()),
            }
        };
        tokio::time::timeout(deadline, work)
            .await
            .unwrap_or(Err(DiscoveryError::ShutdownTimedOut(deadline)))
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::num::NonZeroU16;
    use std::sync::{Arc, Mutex};

    use echomuse_config::MdnsName;

    use super::*;
    use crate::esphome::project_name;

    /// What the simulated responder was asked to do, in order.
    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Call {
        Announce(ServiceRecord),
        Withdraw(String),
        Stop,
    }

    /// An in-process responder that records calls. Withdrawals of names it has
    /// announced succeed; `hang_on_stop` simulates a responder that never
    /// confirms shutdown.
    #[derive(Clone, Default)]
    struct Simulated {
        calls: Arc<Mutex<Vec<Call>>>,
        served: Arc<Mutex<Vec<Ipv4Addr>>>,
        hang_on_stop: bool,
    }

    impl Simulated {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Responder for Simulated {
        fn serve_address(&mut self, address: Ipv4Addr) -> Result<(), DiscoveryError> {
            self.served.lock().unwrap().push(address);
            Ok(())
        }

        fn announce(&mut self, record: &ServiceRecord) -> Result<(), DiscoveryError> {
            self.calls
                .lock()
                .unwrap()
                .push(Call::Announce(record.clone()));
            Ok(())
        }

        fn withdraw(&mut self, fullname: &str) -> Result<WithdrawFuture, DiscoveryError> {
            let mut calls = self.calls.lock().unwrap();
            let published = calls.iter().any(
                |call| matches!(call, Call::Announce(record) if record.fullname() == fullname),
            );
            calls.push(Call::Withdraw(fullname.to_owned()));
            let outcome = if published {
                Withdrawn::Removed
            } else {
                Withdrawn::NotPublished
            };
            Ok(Box::pin(async move { Ok(outcome) }))
        }

        fn stop(&mut self) -> Result<StopFuture, DiscoveryError> {
            self.calls.lock().unwrap().push(Call::Stop);
            if self.hang_on_stop {
                Ok(Box::pin(std::future::pending()))
            } else {
                Ok(Box::pin(async { Ok(()) }))
            }
        }
    }

    fn controller(tls_port: Option<u16>) -> ControllerAdvertisement {
        ControllerAdvertisement {
            name: MdnsName::new("echomuse").unwrap(),
            address: Ipv4Addr::new(192, 168, 1, 20),
            port: NonZeroU16::new(8767).unwrap(),
            tls_port: tls_port.and_then(NonZeroU16::new),
        }
    }

    fn voice(device: &str, port: u16) -> EsphomeAdvertisement {
        EsphomeAdvertisement {
            device: DeviceId(device.to_owned()),
            kind: EsphomeNodeKind::VoiceSatellite,
            friendly_name: "Kitchen Voice Assistant".to_owned(),
            port: NonZeroU16::new(port).unwrap(),
            project_name: project_name("Echo Dot Gen 2"),
            project_version: "2.0.0".to_owned(),
        }
    }

    fn announced(calls: &[Call]) -> Vec<String> {
        calls
            .iter()
            .filter_map(|call| match call {
                Call::Announce(record) => Some(record.fullname()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn the_controller_record_is_announced_at_start_and_every_interval() {
        let responder = Simulated::default();
        let (handle, task) = spawn(
            responder.clone(),
            controller(Some(8770)),
            REANNOUNCE_INTERVAL,
        )
        .unwrap();
        assert_eq!(
            announced(&responder.calls()),
            ["echomuse._emcontroller._tcp.local."]
        );

        tokio::time::sleep(REANNOUNCE_INTERVAL - Duration::from_secs(1)).await;
        assert_eq!(announced(&responder.calls()).len(), 1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(announced(&responder.calls()).len(), 2);
        tokio::time::sleep(REANNOUNCE_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(announced(&responder.calls()).len(), 3);
        assert_eq!(handle.health().borrow().last_reannounce_error, None);

        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn esphome_records_are_published_at_the_controller_address_and_withdrawn() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        handle
            .register_esphome(voice("G090LF1172340ABC", 16001))
            .await
            .unwrap();
        let Call::Announce(record) = responder.calls().last().unwrap().clone() else {
            panic!("expected an announcement");
        };
        assert_eq!(
            record.fullname(),
            "echomuse-lf1172340abc._esphomelib._tcp.local."
        );
        assert_eq!(record.address, Ipv4Addr::new(192, 168, 1, 20));

        let device = DeviceId("G090LF1172340ABC".to_owned());
        assert_eq!(
            handle
                .unregister_esphome(device.clone(), EsphomeNodeKind::VoiceSatellite)
                .await,
            Ok(Withdrawn::Removed)
        );
        assert_eq!(
            responder.calls().last(),
            Some(&Call::Withdraw(
                "echomuse-lf1172340abc._esphomelib._tcp.local.".to_owned()
            ))
        );
        // Withdrawing again, or a node that was never published, touches nothing.
        let before = responder.calls().len();
        assert_eq!(
            handle
                .unregister_esphome(device, EsphomeNodeKind::VoiceSatellite)
                .await,
            Ok(Withdrawn::NotPublished)
        );
        assert_eq!(responder.calls().len(), before);

        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn turning_tls_on_and_off_republishes_the_controller_record() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        handle
            .update_controller(controller(Some(8770)))
            .await
            .unwrap();
        handle.update_controller(controller(None)).await.unwrap();
        let tls_ports: Vec<Option<String>> = responder
            .calls()
            .iter()
            .filter_map(|call| match call {
                Call::Announce(record) => Some(record.txt_value("tls_port").map(str::to_owned)),
                _ => None,
            })
            .collect();
        assert_eq!(tls_ports, [None, Some("8770".to_owned()), None]);
        // The same instance name is updated in place, never withdrawn.
        assert!(
            !responder
                .calls()
                .iter()
                .any(|call| matches!(call, Call::Withdraw(_)))
        );
        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_address_moves_every_record() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        handle
            .register_esphome(voice("AAAAAAAAAAAA", 16001))
            .await
            .unwrap();
        let mut moved = controller(None);
        moved.address = Ipv4Addr::new(10, 0, 0, 7);
        handle.update_controller(moved).await.unwrap();
        let calls = responder.calls();
        let last_two: Vec<(String, Ipv4Addr)> = calls[calls.len() - 2..]
            .iter()
            .map(|call| match call {
                Call::Announce(record) => (record.fullname(), record.address),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            last_two,
            [
                (
                    "echomuse._emcontroller._tcp.local.".to_owned(),
                    Ipv4Addr::new(10, 0, 0, 7)
                ),
                (
                    "echomuse-aaaaaaaaaaaa._esphomelib._tcp.local.".to_owned(),
                    Ipv4Addr::new(10, 0, 0, 7)
                ),
            ]
        );
        assert_eq!(
            *responder.served.lock().unwrap(),
            [Ipv4Addr::new(192, 168, 1, 20), Ipv4Addr::new(10, 0, 0, 7)]
        );
        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn renaming_the_controller_withdraws_the_old_instance() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        let mut renamed = controller(None);
        renamed.name = MdnsName::new("den").unwrap();
        handle.update_controller(renamed).await.unwrap();
        let calls = responder.calls();
        assert_eq!(
            calls[1],
            Call::Withdraw("echomuse._emcontroller._tcp.local.".to_owned())
        );
        assert!(
            matches!(&calls[2], Call::Announce(record) if record.fullname() == "den._emcontroller._tcp.local.")
        );
        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_withdraws_everything_then_stops_the_responder() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        handle
            .register_esphome(voice("AAAAAAAAAAAA", 16001))
            .await
            .unwrap();
        handle.shutdown(Duration::from_secs(1)).await.unwrap();
        task.await.unwrap();
        let calls = responder.calls();
        assert_eq!(
            calls[calls.len() - 3..],
            [
                Call::Withdraw("echomuse-aaaaaaaaaaaa._esphomelib._tcp.local.".to_owned()),
                Call::Withdraw("echomuse._emcontroller._tcp.local.".to_owned()),
                Call::Stop,
            ]
        );
        assert_eq!(
            handle.register_esphome(voice("BBBBBBBBBBBB", 16002)).await,
            Err(DiscoveryError::Stopped)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_finishes_by_its_deadline_when_the_responder_hangs() {
        let responder = Simulated {
            hang_on_stop: true,
            ..Simulated::default()
        };
        let (handle, task) = spawn(responder, controller(None), REANNOUNCE_INTERVAL).unwrap();
        let deadline = Duration::from_millis(500);
        let started = Instant::now();
        assert_eq!(
            handle.shutdown(deadline).await,
            Err(DiscoveryError::ShutdownTimedOut(deadline))
        );
        assert!(started.elapsed() <= deadline + Duration::from_millis(1));
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_every_handle_shuts_the_service_down() {
        let responder = Simulated::default();
        let (handle, task) =
            spawn(responder.clone(), controller(None), REANNOUNCE_INTERVAL).unwrap();
        drop(handle);
        task.await.unwrap();
        assert_eq!(responder.calls().last(), Some(&Call::Stop));
    }

    /// Publishes the controller record on the real network and browses for it
    /// with a second responder. Multicast may be unavailable where tests run,
    /// so the test is reported as ignored unless asked for with
    /// `cargo test -p echomuse-discovery -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs working IPv4 multicast; run with --ignored"]
    async fn live_loopback_browse_finds_the_controller() {
        let address = echomuse_config::default_route_ipv4().expect("an IPv4 default route");
        let mut advert = controller(Some(8770));
        advert.name = MdnsName::new("echomuse-loopback-test").unwrap();
        advert.address = address;
        let (handle, task) = start(advert).unwrap();

        let browser = ServiceDaemon::new().unwrap();
        let events = browser
            .browse(crate::records::CONTROLLER_SERVICE_TYPE)
            .unwrap();
        let found = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(mdns_sd::ServiceEvent::ServiceResolved(service)) =
                    events.recv_async().await
                    && service.get_fullname() == "echomuse-loopback-test._emcontroller._tcp.local."
                {
                    return service;
                }
            }
        })
        .await
        .expect("the record within 10 s");
        assert_eq!(found.get_port(), 8767);
        assert_eq!(found.get_property_val_str("tls_port"), Some("8770"));
        assert!(found.get_addresses_v4().contains(&address));
        let _ = browser.shutdown();
        handle.shutdown(Duration::from_secs(2)).await.unwrap();
        task.await.unwrap();
    }
}
