//! What the device runtimes and the registry tell the shared services about devices: a
//! device gained or lost its runtime, a session became ready or ended, an alert message
//! arrived, Home Assistant sent a timer event or pressed the stop button.
//!
//! Every event goes through one bounded channel to the dispatcher on the shared runtime
//! ([`crate::dispatcher`]). A runtime's events for one session reach it in the order
//! the runtime sent them, so an alert service learns of a session before any of its
//! alert messages. A runtime never waits on the channel: when it is full, an event the
//! session needs closes that session instead of being dropped (§5.2).

use echomuse_core::{
    services::TimerEvent,
    types::{DeviceId, SessionId},
};
use echomuse_link::SessionLink;
use echomuse_protocol::{Capabilities, MessageType, Platform, session::HelloAlerts};
use serde_json::{Map, Value};
use tokio::sync::mpsc;

/// How many events may wait for the dispatcher.
pub const EVENT_CAPACITY: usize = 1024;

/// A device link session that has received `session.ready`: the device dialled its
/// audio socket, which it does as soon as it reads it, or sent an alert message.
#[derive(Clone, Debug)]
pub struct ReadySession {
    pub device: DeviceId,
    pub session: SessionId,
    pub link: SessionLink,
    pub platform: Platform,
    pub capabilities: Capabilities,
    /// The hello's alert cache position; `None` when it carried none.
    pub alerts: Option<HelloAlerts>,
    /// The hello's firmware version; `None` when it sent none.
    pub firmware_version: Option<String>,
}

/// Something the shared services should know about a device.
#[derive(Debug)]
pub enum DeviceEvent {
    /// An approved device has a runtime: at startup, or when it was approved.
    Registered(DeviceId),
    /// The device is being deleted.
    Deregistered(DeviceId),
    /// The device's label or settings changed.
    Changed(DeviceId),
    /// A session is ready.
    SessionReady(Box<ReadySession>),
    /// A session that was ready ended.
    SessionEnded {
        device: DeviceId,
        session: SessionId,
    },
    /// `alert.ack`, `alert.local_operation`, `alert.ring_ended` or `alert.state`, in
    /// ingress order.
    Alert {
        device: DeviceId,
        session: SessionId,
        message_type: MessageType,
        body: Map<String, Value>,
    },
    /// Home Assistant changed one of the satellite's timers.
    Timer { device: DeviceId, event: TimerEvent },
    /// Home Assistant pressed the satellite's `stop_alert` button.
    StopAlert { device: DeviceId },
}

/// The sending side of the event channel. Cheap to clone.
#[derive(Clone, Debug)]
pub struct DeviceEvents(mpsc::Sender<DeviceEvent>);

impl DeviceEvents {
    /// The channel, bounded by [`EVENT_CAPACITY`].
    pub fn channel() -> (Self, mpsc::Receiver<DeviceEvent>) {
        let (sender, receiver) = mpsc::channel(EVENT_CAPACITY);
        (Self(sender), receiver)
    }

    /// Queues an event without waiting; `false` when the channel is full or closed.
    pub fn try_send(&self, event: DeviceEvent) -> bool {
        self.0.try_send(event).is_ok()
    }

    /// Queues an event, waiting for room. For callers that are not a device runtime.
    pub async fn send(&self, event: DeviceEvent) {
        // A closed channel means the controller is stopping; nothing is left to tell.
        let _ = self.0.send(event).await;
    }
}

/// Whether a device message belongs to the alert service.
pub fn is_alert_message(message_type: MessageType) -> bool {
    matches!(
        message_type,
        MessageType::AlertAck
            | MessageType::AlertLocalOperation
            | MessageType::AlertRingEnded
            | MessageType::AlertState
    )
}
