//! What the alert service needs from the rest of the controller: wall time, the device
//! link and Home Assistant. The integrator implements [`AlertLink`] over the device
//! sessions and [`AlertClock`] over the system clock ([`SystemClock`]); [`AlertHa`] is
//! implemented here for [`HaClient`]. Tests drive the service with simulated ones.
//!
//! The traits use return-position `impl Future`, which costs no allocation per call but
//! is not object safe; the service holds implementations by generic parameter.

use std::{
    fmt,
    future::Future,
    time::{SystemTime, UNIX_EPOCH},
};

use echomuse_core::{
    services::{EffectOutcome, HaError, UnknownCause},
    types::{DeviceId, SessionId},
};
use echomuse_ha::{
    CalendarItem, CalendarPush, CalendarSubscription, ConfigEntry, EventTarget, EventWrite,
    HaClient, HaEvent, HaStatus, Subscription,
};
use echomuse_protocol::{
    EncodeError, EncodedEnvelope, LinkSessionId, encode_envelope, vocab::AckStatus,
};
use serde_json::Value;
use tokio::sync::watch;

use crate::{
    scripts::{REQUEST_EVENT, RESULT_EVENT},
    wire::{CommandBody, PostBody},
};

// ── Wall time ────────────────────────────────────────────────────────────────

/// Wall time, for journal timestamps and alarm scheduling (§5.3). Deadlines and waits
/// run on Tokio's monotonic clock instead, so a change of wall time cannot stretch them.
pub trait AlertClock: Send + Sync + 'static {
    /// The current UTC time in milliseconds since the Unix epoch.
    fn now_utc_ms(&self) -> i64;
}

/// The system's wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl AlertClock for SystemClock {
    fn now_utc_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
            })
    }
}

// ── Device link ──────────────────────────────────────────────────────────────

/// A device's session as the alert service knows it. A body addressed to a session that
/// is no longer the device's current one must not be sent.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionRef {
    /// The device.
    pub device: DeviceId,
    /// The controller's identity of the session the service was told about.
    pub session: SessionId,
}

/// Why a body was not sent. Nothing reached the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinkRefusal {
    /// The session is not the device's current one, or the device is not connected.
    NotCurrent,
    /// The session's egress is full; the link closes the session (§5.2) rather than
    /// dropping a required message.
    Overloaded,
}

impl fmt::Display for LinkRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LinkRefusal::NotCurrent => "the device session is not current",
            LinkRefusal::Overloaded => "the device session's egress is full",
        })
    }
}

/// What became of a body the device acknowledges.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    /// Nothing was sent.
    NotSent(LinkRefusal),
    /// The device's `command.ack`.
    Acked {
        /// Its status.
        status: AckStatus,
        /// Its error code, for a rejection or a partial success.
        error: Option<String>,
    },
    /// It may have reached the device, but no acknowledgement arrived (the session ended
    /// or the link's deadline passed). Never resent.
    Unknown(UnknownCause),
}

/// The device link as the alert service uses it.
pub trait AlertLink: Send + Sync + 'static {
    /// Queues a body the device does not acknowledge (`alert.snapshot`, `alert.delta`,
    /// `alert.op_result`, `timer.view`) on `to`'s session, in call order. Must not wait.
    fn post(&self, to: &SessionRef, body: PostBody) -> Result<(), LinkRefusal>;

    /// Sends a body the device acknowledges (`alert.act`, `alert.ring`, `alert.prefetch`)
    /// on `to`'s session and resolves with the acknowledgement.
    fn command(
        &self,
        to: &SessionRef,
        body: CommandBody,
    ) -> impl Future<Output = CommandOutcome> + Send;
}

impl PostBody {
    /// The control envelope for this body on session `session` of `device`
    /// (generation 0).
    pub fn encode(
        &self,
        session: &LinkSessionId,
        device: &DeviceId,
    ) -> Result<EncodedEnvelope, EncodeError> {
        match self {
            PostBody::Snapshot(body) => encode_envelope(body, Some(session), device, 0),
            PostBody::Delta(body) => encode_envelope(body, Some(session), device, 0),
            PostBody::OpResult(body) => encode_envelope(body, Some(session), device, 0),
            PostBody::TimerView(body) => encode_envelope(body, Some(session), device, 0),
        }
    }
}

impl CommandBody {
    /// The control envelope for this body on session `session` of `device`
    /// (generation 0). Its `message_id` is what the device's `command.ack` echoes.
    pub fn encode(
        &self,
        session: &LinkSessionId,
        device: &DeviceId,
    ) -> Result<EncodedEnvelope, EncodeError> {
        match self {
            CommandBody::Act(body) => encode_envelope(body, Some(session), device, 0),
            CommandBody::Ring(body) => encode_envelope(body, Some(session), device, 0),
            CommandBody::Prefetch(body) => encode_envelope(body, Some(session), device, 0),
        }
    }
}

// ── Home Assistant ───────────────────────────────────────────────────────────

/// The pushes of one calendar subscription.
pub trait CalendarFeed: Send + 'static {
    /// The next push; `None` once the subscription's connection has ended.
    fn next(&mut self) -> impl Future<Output = Option<CalendarPush>> + Send;
}

/// The `echomuse_alert_request` events of one subscription.
pub trait RelayFeed: Send + 'static {
    /// The next event's data; `None` once the subscription's connection has ended.
    fn next(&mut self) -> impl Future<Output = Option<Value>> + Send;
}

/// The Home Assistant operations the alert service uses (§3.3.3, §3.3.4).
pub trait AlertHa: Send + Sync + 'static {
    /// A calendar subscription.
    type Calendar: CalendarFeed;
    /// A relay event subscription.
    type Relay: RelayFeed;

    /// The connection status, updated on every change.
    fn status(&self) -> watch::Receiver<HaStatus>;
    /// HA's time zone (`get_config`).
    fn time_zone(&self) -> impl Future<Output = Result<String, HaError>> + Send;
    /// The Local Calendar config entries.
    fn local_calendars(&self) -> impl Future<Output = Result<Vec<ConfigEntry>, HaError>> + Send;
    /// The calendar entities of a config entry.
    fn calendar_entities(
        &self,
        entry_id: String,
    ) -> impl Future<Output = Result<Vec<String>, HaError>> + Send;
    /// Creates an empty Local Calendar titled `title`.
    fn create_local_calendar(
        &self,
        title: String,
    ) -> impl Future<Output = EffectOutcome<ConfigEntry>> + Send;
    /// Subscribes to `entity` between `start` and `end` (RFC 3339).
    fn subscribe_calendar(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> impl Future<Output = Result<Self::Calendar, HaError>> + Send;
    /// Ends a calendar subscription.
    fn unsubscribe_calendar(&self, feed: Self::Calendar) -> impl Future<Output = ()> + Send;
    /// The items of `entity` between `start` and `end` (RFC 3339), any range.
    fn calendar_events(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> impl Future<Output = Result<Vec<CalendarItem>, HaError>> + Send;
    /// Creates an event.
    fn create_event(
        &self,
        entity: String,
        event: EventWrite,
    ) -> impl Future<Output = EffectOutcome<()>> + Send;
    /// Deletes an event, a series or one instance.
    fn delete_event(
        &self,
        entity: String,
        target: EventTarget,
    ) -> impl Future<Output = EffectOutcome<()>> + Send;
    /// A script's stored configuration; `None` when it does not exist.
    fn script_config(
        &self,
        object_id: String,
    ) -> impl Future<Output = Result<Option<Value>, HaError>> + Send;
    /// Stores a script's configuration.
    fn write_script_config(
        &self,
        object_id: String,
        config: Value,
    ) -> impl Future<Output = EffectOutcome<()>> + Send;
    /// `script.reload`.
    fn reload_scripts(&self) -> impl Future<Output = EffectOutcome<()>> + Send;
    /// Exposes entities to the conversation assistant.
    fn expose_scripts(
        &self,
        entity_ids: Vec<String>,
    ) -> impl Future<Output = EffectOutcome<()>> + Send;
    /// Subscribes to `echomuse_alert_request`.
    fn subscribe_relay(&self) -> impl Future<Output = Result<Self::Relay, HaError>> + Send;
    /// Fires `echomuse_alert_result` with `data`.
    fn fire_result(&self, data: Value) -> impl Future<Output = EffectOutcome<()>> + Send;
}

impl CalendarFeed for CalendarSubscription {
    async fn next(&mut self) -> Option<CalendarPush> {
        CalendarSubscription::next(self).await
    }
}

impl RelayFeed for Subscription {
    async fn next(&mut self) -> Option<Value> {
        self.events.recv().await.map(|event: HaEvent| event.data)
    }
}

impl AlertHa for HaClient {
    type Calendar = CalendarSubscription;
    type Relay = Subscription;

    fn status(&self) -> watch::Receiver<HaStatus> {
        HaClient::status(self)
    }

    async fn time_zone(&self) -> Result<String, HaError> {
        HaClient::time_zone(self).await
    }

    async fn local_calendars(&self) -> Result<Vec<ConfigEntry>, HaError> {
        self.config_entries("local_calendar").await
    }

    async fn calendar_entities(&self, entry_id: String) -> Result<Vec<String>, HaError> {
        self.entities_of_entry(&entry_id, "calendar").await
    }

    async fn create_local_calendar(&self, title: String) -> EffectOutcome<ConfigEntry> {
        HaClient::create_local_calendar(self, &title).await
    }

    async fn subscribe_calendar(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> Result<CalendarSubscription, HaError> {
        self.calendar_subscribe(&entity, &start, &end).await
    }

    async fn unsubscribe_calendar(&self, feed: CalendarSubscription) {
        // A failure leaves the subscription to end with its connection.
        let _ = self.calendar_unsubscribe(feed).await;
    }

    async fn calendar_events(
        &self,
        entity: String,
        start: String,
        end: String,
    ) -> Result<Vec<CalendarItem>, HaError> {
        HaClient::calendar_events(self, &entity, &start, &end).await
    }

    async fn create_event(&self, entity: String, event: EventWrite) -> EffectOutcome<()> {
        self.calendar_create_event(&entity, &event).await
    }

    async fn delete_event(&self, entity: String, target: EventTarget) -> EffectOutcome<()> {
        self.calendar_delete_event(&entity, &target).await
    }

    async fn script_config(&self, object_id: String) -> Result<Option<Value>, HaError> {
        HaClient::script_config(self, &object_id).await
    }

    async fn write_script_config(&self, object_id: String, config: Value) -> EffectOutcome<()> {
        HaClient::write_script_config(self, &object_id, &config).await
    }

    async fn reload_scripts(&self) -> EffectOutcome<()> {
        HaClient::reload_scripts(self).await
    }

    async fn expose_scripts(&self, entity_ids: Vec<String>) -> EffectOutcome<()> {
        self.expose_to_conversation(&entity_ids).await
    }

    async fn subscribe_relay(&self) -> Result<Subscription, HaError> {
        self.subscribe_events(REQUEST_EVENT).await
    }

    async fn fire_result(&self, data: Value) -> EffectOutcome<()> {
        self.fire_event(RESULT_EVENT, data).await
    }
}
