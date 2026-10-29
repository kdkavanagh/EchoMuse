//! Calendars, Local Calendar provisioning and HA's time zone, for the alert service
//! (§3.3.3, §3.3.4).
//!
//! The shapes follow Home Assistant 2026.8.1:
//!
//! - `calendar/__init__.py`: `calendar/event/create`, `calendar/event/delete` and
//!   `calendar/event/subscribe` over the WebSocket. Event times are `dtstart`/`dtend`.
//!   A subscription is confirmed with a `result` and then pushes
//!   `{"events": [CalendarEvent.as_dict()]}` (or `{"events": null}` when the calendar could
//!   not be read), first at once and then after every change, debounced by a second.
//!   `CalendarEvent.as_dict` writes `start`/`end` with `isoformat()` (a date for an all-day
//!   event), `summary`, `description`, `location`, `uid`, `recurrence_id`, `rrule` and
//!   `all_day`, leaving out absent values.
//! - The REST `GET /api/calendars/<entity>?start=&end=` answers with
//!   `_api_event_dict_factory` items, whose times are `{"dateTime": …}` or `{"date": …}`.
//! - `local_calendar/config_flow.py`: the user step takes `calendar_name` and `import`
//!   (`create_empty`) and creates an entry titled with the name.
//! - `config_entries/get` lists entries of a domain; `config/entity_registry/list` names
//!   each entity's `config_entry_id`; `get_config` carries `time_zone`.

use std::{collections::VecDeque, sync::Weak};

use echomuse_core::services::{EffectOutcome, HaConnectionGeneration, HaError, NotDispatched};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::{
    client::{HaClient, RestFailure, object},
    connection::{Link, Route, RouteGuard, RunItem},
};

/// A calendar event to create, in the stock `calendar/event/create` shape.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventWrite {
    /// The event's title.
    pub summary: String,
    /// The start, RFC 3339 with offset.
    pub dtstart: String,
    /// The end, RFC 3339 with offset.
    pub dtend: String,
    /// The description, absent when `None`.
    pub description: Option<String>,
    /// The recurrence rule, absent for a one-shot event.
    pub rrule: Option<String>,
}

impl EventWrite {
    fn to_json(&self) -> Value {
        let mut event = Map::new();
        event.insert("summary".to_owned(), Value::from(self.summary.clone()));
        event.insert("dtstart".to_owned(), Value::from(self.dtstart.clone()));
        event.insert("dtend".to_owned(), Value::from(self.dtend.clone()));
        if let Some(description) = &self.description {
            event.insert("description".to_owned(), Value::from(description.clone()));
        }
        if let Some(rrule) = &self.rrule {
            event.insert("rrule".to_owned(), Value::from(rrule.clone()));
        }
        Value::Object(event)
    }
}

/// The event, or the one instance of a repeating event, a delete acts on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventTarget {
    /// The event's uid.
    pub uid: String,
    /// One instance of a repeating event; `None` acts on the whole event or series.
    pub recurrence_id: Option<String>,
}

/// When a calendar item starts or ends.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalendarTime {
    /// A date and time, ISO 8601 with offset as HA wrote it.
    DateTime(String),
    /// A whole day (`YYYY-MM-DD`), for an all-day event.
    Date(String),
}

/// One event, or one instance of a repeating event, as HA lists it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CalendarItem {
    /// The title.
    pub summary: String,
    /// The start.
    pub start: CalendarTime,
    /// The end.
    pub end: CalendarTime,
    /// The description; `None` when HA sent none.
    pub description: Option<String>,
    /// The event's uid; `None` when the calendar gave none.
    pub uid: Option<String>,
    /// The instance id of a repeating event; `None` for a one-shot event.
    pub recurrence_id: Option<String>,
    /// The series' recurrence rule; `None` for a one-shot event.
    pub rrule: Option<String>,
}

/// One push of a calendar subscription.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CalendarPush {
    /// The full list of items in the subscribed window.
    Events(Vec<CalendarItem>),
    /// HA could not read the calendar this time (`"events": null`).
    Unavailable,
    /// A push this controller cannot read; the text says why.
    Unreadable(String),
}

/// A Home Assistant config entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigEntry {
    /// The entry id.
    pub entry_id: String,
    /// The entry's title.
    pub title: String,
}

/// An open `calendar/event/subscribe`.
///
/// HA forgets subscriptions when the connection drops, so the pushes end with their
/// connection; subscribe again on a new [`HaConnectionGeneration`].
pub struct CalendarSubscription {
    /// The connection the subscription lives on.
    pub generation: HaConnectionGeneration,
    items: mpsc::UnboundedReceiver<RunItem>,
    early: VecDeque<Value>,
    id: u64,
    link: Weak<Link>,
}

impl std::fmt::Debug for CalendarSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalendarSubscription")
            .field("generation", &self.generation)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CalendarSubscription {
    /// The next push; `None` once the connection has ended.
    pub async fn next(&mut self) -> Option<CalendarPush> {
        if let Some(event) = self.early.pop_front() {
            return Some(parse_push(event));
        }
        loop {
            match self.items.recv().await? {
                RunItem::Event(event) => return Some(parse_push(event)),
                RunItem::Lost | RunItem::Refused(_) => return None,
                RunItem::Started => {}
            }
        }
    }
}

#[derive(Deserialize)]
struct WireItem {
    #[serde(default)]
    summary: String,
    start: Value,
    end: Value,
    description: Option<String>,
    uid: Option<String>,
    recurrence_id: Option<String>,
    rrule: Option<String>,
    #[serde(default)]
    all_day: bool,
}

/// A subscription item's time: an `isoformat()` string, a date when `all_day`.
fn push_time(value: &Value, all_day: bool) -> Option<CalendarTime> {
    let text = value.as_str()?.to_owned();
    Some(if all_day {
        CalendarTime::Date(text)
    } else {
        CalendarTime::DateTime(text)
    })
}

/// A REST item's time: `{"dateTime": …}` or `{"date": …}`.
fn api_time(value: &Value) -> Option<CalendarTime> {
    if let Some(text) = value.get("dateTime").and_then(Value::as_str) {
        return Some(CalendarTime::DateTime(text.to_owned()));
    }
    value
        .get("date")
        .and_then(Value::as_str)
        .map(|text| CalendarTime::Date(text.to_owned()))
}

fn item(
    wire: WireItem,
    time: impl Fn(&Value, bool) -> Option<CalendarTime>,
) -> Option<CalendarItem> {
    Some(CalendarItem {
        start: time(&wire.start, wire.all_day)?,
        end: time(&wire.end, wire.all_day)?,
        summary: wire.summary,
        description: wire.description,
        uid: wire.uid,
        recurrence_id: wire.recurrence_id,
        rrule: wire.rrule,
    })
}

fn parse_push(event: Value) -> CalendarPush {
    let Some(events) = event.get("events") else {
        return CalendarPush::Unreadable("push without events".to_owned());
    };
    if events.is_null() {
        return CalendarPush::Unavailable;
    }
    let wire: Vec<WireItem> = match serde_json::from_value(events.clone()) {
        Ok(wire) => wire,
        Err(err) => return CalendarPush::Unreadable(format!("calendar push: {err}")),
    };
    let mut items = Vec::with_capacity(wire.len());
    for one in wire {
        match item(one, push_time) {
            Some(item) => items.push(item),
            None => return CalendarPush::Unreadable("calendar item without times".to_owned()),
        }
    }
    CalendarPush::Events(items)
}

fn parse_api_items(body: &[u8]) -> Result<Vec<CalendarItem>, HaError> {
    let wire: Vec<WireItem> = serde_json::from_slice(body)
        .map_err(|err| HaError::BadResponse(format!("calendar events: {err}")))?;
    wire.into_iter()
        .map(|one| {
            item(one, |value, _| api_time(value))
                .ok_or_else(|| HaError::BadResponse("calendar item without times".to_owned()))
        })
        .collect()
}

/// The outcome of a state-changing WebSocket command.
pub(crate) fn effect(result: Result<Value, HaError>) -> EffectOutcome<Value> {
    use echomuse_core::services::UnknownCause;
    match result {
        Ok(value) => EffectOutcome::Applied(value),
        Err(HaError::NotDispatched(reason)) => EffectOutcome::NotDispatched(reason),
        Err(HaError::Rejected { code, message }) => EffectOutcome::Rejected { code, message },
        Err(HaError::Timeout) => EffectOutcome::Unknown(UnknownCause::Timeout),
        Err(HaError::ConnectionLost) => EffectOutcome::Unknown(UnknownCause::ConnectionLost),
        Err(HaError::BadResponse(_)) => EffectOutcome::Unknown(UnknownCause::Inconclusive),
    }
}

/// The outcome of a state-changing REST call: a 4xx is a refusal made before anything
/// changed, any other failure after sending is unknown.
pub(crate) fn rest_effect(sent: Result<(u16, Vec<u8>), RestFailure>) -> EffectOutcome<Vec<u8>> {
    use echomuse_core::services::UnknownCause;
    match sent {
        Err(RestFailure::NotSent) => EffectOutcome::NotDispatched(NotDispatched::Unavailable),
        Err(RestFailure::TimedOut) => EffectOutcome::Unknown(UnknownCause::Timeout),
        Err(RestFailure::Lost) => EffectOutcome::Unknown(UnknownCause::ConnectionLost),
        Ok((status, body)) if (200..300).contains(&status) => EffectOutcome::Applied(body),
        Ok((status, body)) if (400..500).contains(&status) => EffectOutcome::Rejected {
            code: status.to_string(),
            message: rest_message(&body),
        },
        Ok(_) => EffectOutcome::Unknown(UnknownCause::Inconclusive),
    }
}

/// HA's JSON `message`, or the body as text.
pub(crate) fn rest_message(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

/// A REST read: 2xx gives the body, anything else is an error.
fn rest_read(sent: Result<(u16, Vec<u8>), RestFailure>) -> Result<Vec<u8>, HaError> {
    let (status, body) = sent?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(HaError::Rejected {
            code: status.to_string(),
            message: rest_message(&body),
        })
    }
}

fn query_value(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

#[derive(Deserialize)]
struct WireEntry {
    entry_id: String,
    #[serde(default)]
    title: String,
}

#[derive(Deserialize)]
struct WireRegistryEntry {
    entity_id: String,
    config_entry_id: Option<String>,
}

#[derive(Deserialize)]
struct WireFlow {
    #[serde(rename = "type")]
    kind: String,
    flow_id: Option<String>,
    reason: Option<String>,
    result: Option<WireEntry>,
}

impl HaClient {
    /// Creates an event (`calendar/event/create`). Never resent once it may have been sent.
    pub async fn calendar_create_event(
        &self,
        entity_id: &str,
        event: &EventWrite,
    ) -> EffectOutcome<()> {
        let body = object(json!({
            "type": "calendar/event/create",
            "entity_id": entity_id,
            "event": event.to_json(),
        }));
        match effect(
            self.command(None, body, self.options().command_timeout)
                .await,
        ) {
            EffectOutcome::Applied(_) => EffectOutcome::Applied(()),
            other => other.map_applied(),
        }
    }

    /// Deletes an event, a series, or one instance of a series (`calendar/event/delete`).
    pub async fn calendar_delete_event(
        &self,
        entity_id: &str,
        target: &EventTarget,
    ) -> EffectOutcome<()> {
        let mut body = object(json!({
            "type": "calendar/event/delete",
            "entity_id": entity_id,
            "uid": target.uid,
        }));
        if let Some(recurrence_id) = &target.recurrence_id {
            body.insert(
                "recurrence_id".to_owned(),
                Value::from(recurrence_id.clone()),
            );
        }
        match effect(
            self.command(None, body, self.options().command_timeout)
                .await,
        ) {
            EffectOutcome::Applied(_) => EffectOutcome::Applied(()),
            other => other.map_applied(),
        }
    }

    /// Subscribes to the events of `entity_id` between `start` and `end` (RFC 3339).
    pub async fn calendar_subscribe(
        &self,
        entity_id: &str,
        start: &str,
        end: &str,
    ) -> Result<CalendarSubscription, HaError> {
        let link = self
            .current_link()
            .ok_or(HaError::NotDispatched(NotDispatched::Unavailable))?;
        let (sender, mut items) = mpsc::unbounded_channel();
        let body = object(json!({
            "type": "calendar/event/subscribe",
            "entity_id": entity_id,
            "start": start,
            "end": end,
        }));
        let id = link
            .dispatch(None, body, Route::Run(sender))
            .await
            .map_err(HaError::NotDispatched)?;
        let route = RouteGuard::new(&link, id);
        let mut early = VecDeque::new();
        let confirmed = tokio::time::timeout(self.options().command_timeout, async {
            loop {
                match items.recv().await {
                    Some(RunItem::Started) => return Ok(()),
                    Some(RunItem::Event(event)) => early.push_back(event),
                    Some(RunItem::Refused(refusal)) => return Err(HaError::from(refusal)),
                    Some(RunItem::Lost) | None => return Err(HaError::ConnectionLost),
                }
            }
        })
        .await;
        match confirmed {
            Err(_) => Err(HaError::Timeout),
            Ok(Err(error)) => Err(error),
            Ok(Ok(())) => {
                route.keep();
                Ok(CalendarSubscription {
                    generation: link.generation,
                    items,
                    early,
                    id,
                    link: std::sync::Arc::downgrade(&link),
                })
            }
        }
    }

    /// Ends a calendar subscription. Once its connection is gone HA has already
    /// forgotten it, and this only drops the local end.
    pub async fn calendar_unsubscribe(
        &self,
        subscription: CalendarSubscription,
    ) -> Result<(), HaError> {
        let Some(link) = subscription.link.upgrade() else {
            return Ok(());
        };
        link.remove(subscription.id);
        if link.is_lost() {
            return Ok(());
        }
        let body = object(json!({"type": "unsubscribe_events", "subscription": subscription.id}));
        match self
            .command(None, body, self.options().command_timeout)
            .await
        {
            Err(HaError::ConnectionLost | HaError::NotDispatched(NotDispatched::Unavailable)) => {
                Ok(())
            }
            other => other.map(drop),
        }
    }

    /// The items of `entity_id` between `start` and `end` (RFC 3339) through REST, which
    /// reads any range (the backlog before the subscription window).
    pub async fn calendar_events(
        &self,
        entity_id: &str,
        start: &str,
        end: &str,
    ) -> Result<Vec<CalendarItem>, HaError> {
        let path = format!(
            "/calendars/{entity_id}?start={}&end={}",
            query_value(start),
            query_value(end)
        );
        parse_api_items(&rest_read(self.rest_get(&path).await)?)
    }

    /// HA's configured time zone, an IANA name (`get_config`).
    pub async fn time_zone(&self) -> Result<String, HaError> {
        let body = object(json!({"type": "get_config"}));
        let config = self
            .command(None, body, self.options().command_timeout)
            .await?;
        config
            .get("time_zone")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HaError::BadResponse("get_config without time_zone".to_owned()))
    }

    /// The config entries of `domain` (`config_entries/get`).
    pub async fn config_entries(&self, domain: &str) -> Result<Vec<ConfigEntry>, HaError> {
        let body = object(json!({"type": "config_entries/get", "domain": domain}));
        let result = self
            .command(None, body, self.options().command_timeout)
            .await?;
        let entries: Vec<WireEntry> = serde_json::from_value(result)
            .map_err(|err| HaError::BadResponse(format!("config entries: {err}")))?;
        Ok(entries
            .into_iter()
            .map(|entry| ConfigEntry {
                entry_id: entry.entry_id,
                title: entry.title,
            })
            .collect())
    }

    /// The entity ids of `domain` that belong to config entry `entry_id`
    /// (`config/entity_registry/list`).
    pub async fn entities_of_entry(
        &self,
        entry_id: &str,
        domain: &str,
    ) -> Result<Vec<String>, HaError> {
        let body = object(json!({"type": "config/entity_registry/list"}));
        let result = self
            .command(None, body, self.options().command_timeout)
            .await?;
        let entries: Vec<WireRegistryEntry> = serde_json::from_value(result)
            .map_err(|err| HaError::BadResponse(format!("entity registry: {err}")))?;
        let prefix = format!("{domain}.");
        Ok(entries
            .into_iter()
            .filter(|entry| {
                entry.config_entry_id.as_deref() == Some(entry_id)
                    && entry.entity_id.starts_with(&prefix)
            })
            .map(|entry| entry.entity_id)
            .collect())
    }

    /// Creates an empty Local Calendar named `name` through the stock config flow. A
    /// flow HA aborts (a calendar with that storage key exists) is a refusal.
    pub async fn create_local_calendar(&self, name: &str) -> EffectOutcome<ConfigEntry> {
        let started = rest_effect(
            self.rest_post(
                "/config/config_entries/flow",
                &json!({"handler": "local_calendar", "show_advanced_options": false}),
            )
            .await,
        );
        let flow = match read_flow(started) {
            Ok(flow) => flow,
            Err(outcome) => return outcome,
        };
        let Some(flow_id) = flow.flow_id.filter(|_| flow.kind == "form") else {
            return refused_flow(&flow.kind, flow.reason);
        };
        let finished = rest_effect(
            self.rest_post(
                &format!("/config/config_entries/flow/{flow_id}"),
                &json!({"calendar_name": name, "import": "create_empty"}),
            )
            .await,
        );
        let flow = match read_flow(finished) {
            Ok(flow) => flow,
            Err(outcome) => return outcome,
        };
        match (flow.kind.as_str(), flow.result) {
            ("create_entry", Some(entry)) => EffectOutcome::Applied(ConfigEntry {
                entry_id: entry.entry_id,
                title: entry.title,
            }),
            ("create_entry", None) => {
                EffectOutcome::Unknown(echomuse_core::services::UnknownCause::Inconclusive)
            }
            (kind, _) => refused_flow(kind, flow.reason),
        }
    }
}

fn read_flow(outcome: EffectOutcome<Vec<u8>>) -> Result<WireFlow, EffectOutcome<ConfigEntry>> {
    match outcome {
        EffectOutcome::Applied(body) => serde_json::from_slice(&body).map_err(|_| {
            EffectOutcome::Unknown(echomuse_core::services::UnknownCause::Inconclusive)
        }),
        other => Err(other.map_applied()),
    }
}

fn refused_flow(kind: &str, reason: Option<String>) -> EffectOutcome<ConfigEntry> {
    EffectOutcome::Rejected {
        code: kind.to_owned(),
        message: reason.unwrap_or_default(),
    }
}

/// Re-types an outcome that did not apply.
pub(crate) trait NotApplied<T> {
    /// The same outcome for another result type; must not be [`EffectOutcome::Applied`].
    fn map_applied<U>(self) -> EffectOutcome<U>;
}

impl<T> NotApplied<T> for EffectOutcome<T> {
    fn map_applied<U>(self) -> EffectOutcome<U> {
        match self {
            EffectOutcome::NotDispatched(reason) => EffectOutcome::NotDispatched(reason),
            EffectOutcome::Rejected { code, message } => EffectOutcome::Rejected { code, message },
            EffectOutcome::Unknown(cause) => EffectOutcome::Unknown(cause),
            EffectOutcome::Applied(_) => {
                unreachable!("an applied outcome is handled by the caller")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_reads_timed_and_all_day_items_and_a_failed_fetch() {
        let push = parse_push(json!({"events": [
            {"start": "2026-09-23T06:30:00-05:00", "end": "2026-09-23T06:31:00-05:00",
             "summary": "Wake up", "description": "echomuse: {}", "uid": "u1",
             "recurrence_id": "20260923T063000", "rrule": "FREQ=DAILY", "all_day": false},
            {"start": "2026-09-24", "end": "2026-09-25", "summary": "Holiday", "all_day": true}
        ]}));
        let CalendarPush::Events(items) = push else {
            panic!("{push:?}");
        };
        assert_eq!(
            items[0].start,
            CalendarTime::DateTime("2026-09-23T06:30:00-05:00".to_owned())
        );
        assert_eq!(items[0].recurrence_id.as_deref(), Some("20260923T063000"));
        assert_eq!(items[1].start, CalendarTime::Date("2026-09-24".to_owned()));
        assert_eq!(items[1].uid, None);
        assert_eq!(
            parse_push(json!({"events": null})),
            CalendarPush::Unavailable
        );
        assert!(matches!(parse_push(json!({})), CalendarPush::Unreadable(_)));
    }

    #[test]
    fn rest_items_use_the_api_time_objects() {
        let items = parse_api_items(
            br#"[{"start": {"dateTime": "2026-09-01T07:00:00-05:00"},
                  "end": {"dateTime": "2026-09-01T07:01:00-05:00"},
                  "summary": "Alarm", "description": null, "location": null,
                  "uid": "u2", "recurrence_id": null, "rrule": null}]"#,
        )
        .unwrap();
        assert_eq!(
            items[0].start,
            CalendarTime::DateTime("2026-09-01T07:00:00-05:00".to_owned())
        );
        assert_eq!(items[0].rrule, None);
    }

    #[test]
    fn an_event_write_leaves_out_absent_fields() {
        let event = EventWrite {
            summary: "Alarm".to_owned(),
            dtstart: "2026-09-23T06:30:00-05:00".to_owned(),
            dtend: "2026-09-23T06:31:00-05:00".to_owned(),
            description: None,
            rrule: None,
        };
        assert_eq!(
            event.to_json(),
            json!({"summary": "Alarm", "dtstart": "2026-09-23T06:30:00-05:00",
                   "dtend": "2026-09-23T06:31:00-05:00"})
        );
    }
}
