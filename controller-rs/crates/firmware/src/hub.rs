//! Hellos, as firmware delivery watches for them: after an update or rollback restarts
//! the firmware, the device's next hello says which build came up.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use echomuse_core::types::DeviceId;
use echomuse_protocol::Platform;
use tokio::sync::mpsc;

use crate::types::FirmwareVersion;

/// Hellos buffered for one watcher at most; a watcher only ever needs the first one after
/// it starts waiting.
const WATCH_BUFFER: usize = 8;

/// An admitted `session.hello`, as the integrator reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceHello {
    /// The device.
    pub device: DeviceId,
    /// The image it runs on.
    pub platform: Platform,
    /// The firmware version it reported; None when it sent none or an unreadable one.
    pub firmware_version: Option<FirmwareVersion>,
}

/// Routes hellos to whoever waits for that device. Cheap to clone.
#[derive(Clone, Debug, Default)]
pub(crate) struct HelloHub {
    watchers: Arc<Mutex<HashMap<DeviceId, Vec<mpsc::Sender<DeviceHello>>>>>,
}

impl HelloHub {
    fn lock(&self) -> MutexGuard<'_, HashMap<DeviceId, Vec<mpsc::Sender<DeviceHello>>>> {
        self.watchers.lock().expect("hello hub lock poisoned")
    }

    /// Starts collecting the device's hellos.
    pub(crate) fn watch(&self, device: &DeviceId) -> HelloWatch {
        let (sender, receiver) = mpsc::channel(WATCH_BUFFER);
        self.lock().entry(device.clone()).or_default().push(sender);
        HelloWatch { receiver }
    }

    /// Hands a hello to the device's watchers, forgetting those that stopped.
    pub(crate) fn publish(&self, hello: &DeviceHello) {
        let mut watchers = self.lock();
        let Some(list) = watchers.get_mut(&hello.device) else {
            return;
        };
        list.retain(|sender| match sender.try_send(hello.clone()) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
        if list.is_empty() {
            watchers.remove(&hello.device);
        }
    }
}

/// One watcher's hellos.
#[derive(Debug)]
pub(crate) struct HelloWatch {
    receiver: mpsc::Receiver<DeviceHello>,
}

impl HelloWatch {
    /// Forgets hellos received so far.
    pub(crate) fn discard_received(&mut self) {
        while self.receiver.try_recv().is_ok() {}
    }

    /// The next hello.
    pub(crate) async fn next(&mut self) -> Option<DeviceHello> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(device: &str, version: &str) -> DeviceHello {
        DeviceHello {
            device: DeviceId(device.into()),
            platform: Platform::FireOs6,
            firmware_version: FirmwareVersion::new(version),
        }
    }

    #[tokio::test]
    async fn watchers_get_only_their_device_and_only_while_watching() {
        let hub = HelloHub::default();
        hub.publish(&hello("A", "fw-1"));
        let mut watch = hub.watch(&DeviceId("A".into()));
        hub.publish(&hello("B", "fw-2"));
        hub.publish(&hello("A", "fw-3"));
        assert_eq!(watch.next().await, Some(hello("A", "fw-3")));
        hub.publish(&hello("A", "fw-4"));
        watch.discard_received();
        hub.publish(&hello("A", "fw-5"));
        assert_eq!(watch.next().await, Some(hello("A", "fw-5")));
        drop(watch);
        hub.publish(&hello("A", "fw-6"));
        assert!(hub.lock().is_empty());
    }
}
