//! The alert message bodies of the device link that the protocol crate does not define
//! (docs/protocol-v1.md §4.7 and §4.10, spec §3.1.5). The firmware is the authority:
//! `device/internal/alerts` (`executor.go`, `delivery.go`) and
//! `device/internal/supervisor/alerts.go` define each field, and the decoders accept
//! exactly what those types marshal.
//!
//! `alert.snapshot` and `alert.delta` come from [`echomuse_protocol::alerts`]. Bodies the
//! controller sends implement [`ControlBody`], so the link encodes them with
//! [`echomuse_protocol::encode_envelope`]; bodies the device sends arrive as
//! `DeviceMessage::Other` and are decoded here with [`AlertInbound::decode`].

use std::{fmt, num::NonZeroU64};

use echomuse_protocol::{
    ControlBody, MessageType, Sha256,
    alerts::{AlertDelta, AlertSnapshotPage, AlertSound},
};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use uuid::Uuid;

/// A JSON object as the link delivers a body.
pub type Body = Map<String, Value>;

/// Why a device alert body was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyError {
    /// The message type.
    pub message_type: MessageType,
    /// What is wrong.
    pub detail: String,
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.message_type, self.detail)
    }
}

impl std::error::Error for BodyError {}

/// Declares a closed string vocabulary with its exact wire spellings.
macro_rules! vocabulary {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $variant:ident => $wire:literal,)+ }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum $name { $($(#[$vmeta])* $variant,)+ }

        impl $name {
            /// The exact wire spelling.
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $wire,)+ }
            }

            /// Parses the exact wire spelling.
            pub fn from_wire(text: &str) -> Option<Self> {
                match text { $($wire => Some($name::$variant),)+ _ => None }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

vocabulary! {
    /// What `alert.act` asks for.
    pub enum ActAction {
        /// Stop the ring; an alarm occurrence is handled.
        Dismiss => "dismiss",
        /// Snooze an alarm (timers cannot be snoozed).
        Snooze => "snooze",
    }
}

vocabulary! {
    /// Who asked for an `alert.act` (the firmware's `actSource`, minus `button`, which only
    /// the device itself uses).
    pub enum ActSource {
        /// A voice command.
        Voice => "voice",
        /// HA's "Stop alert" entity.
        Entity => "entity",
        /// An LLM tool call through the relay.
        Llm => "llm",
        /// The dashboard.
        Dashboard => "dashboard",
    }
}

vocabulary! {
    /// `alert.local_operation.action`.
    pub enum LocalAction {
        /// The occurrence was dismissed.
        Dismiss => "dismiss",
        /// The occurrence was snoozed; `child` names its replacement.
        Snooze => "snooze",
        /// The occurrence expired on the device.
        Expire => "expire",
    }
}

vocabulary! {
    /// `alert.local_operation.reason`, for an expiry.
    pub enum ExpireReason {
        /// It rang for its whole ring limit.
        TimedOut => "timed_out",
        /// It was past its catch-up window when found.
        Missed => "missed",
    }
}

vocabulary! {
    /// `alert.local_operation.source`.
    pub enum LocalSource {
        /// The physical button.
        Button => "button",
        /// A voice command (`alert.act` from the controller).
        Voice => "voice",
        /// HA's "Stop alert" entity.
        Entity => "entity",
        /// An LLM tool call.
        Llm => "llm",
        /// The dashboard.
        Dashboard => "dashboard",
        /// The device's own executor (an expiry).
        Executor => "executor",
    }
}

vocabulary! {
    /// `alert.op_result.state`.
    pub enum OpResultState {
        /// The controller applied the operation.
        Applied => "applied",
        /// The controller rejected it.
        Rejected => "rejected",
    }
}

vocabulary! {
    /// A ring's kind in `alert.state` and `alert.ring_ended`.
    pub enum RingKind {
        /// A calendar alarm (snooze children ring as alarms).
        Alarm => "alarm",
        /// A snooze child, if a firmware names it so.
        Snooze => "snooze",
        /// An HA timer.
        Timer => "timer",
    }
}

vocabulary! {
    /// `alert.ring_ended.reason`.
    pub enum RingEndReason {
        /// Stopped by an `alert.act`.
        Stopped => "stopped",
        /// Stopped by the physical button.
        Button => "button",
        /// Stopped by the "Stop alert" entity.
        Entity => "entity",
        /// It rang for its whole limit.
        Limit => "limit",
        /// Another alert took the foreground.
        Preempted => "preempted",
        /// The device restarted.
        Restart => "restart",
    }
}

vocabulary! {
    /// `alert.state.wakelock`.
    pub enum WakeLockStatus {
        /// The alarm wakelock is held.
        Held => "held",
        /// Nothing is armed, so it is released.
        Released => "released",
        /// The device cannot take it.
        Unavailable => "unavailable",
    }
}

vocabulary! {
    /// `alert.state.store`.
    pub enum StoreStatus {
        /// The device's alert store is healthy.
        Ok => "ok",
        /// It was found damaged and waits for a snapshot.
        Corrupt => "corrupt",
    }
}

// ── Controller → device ──────────────────────────────────────────────────────

/// `alert.op_result`: the controller's verdict on an uploaded local operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AlertOpResult {
    /// The operation.
    pub op_id: Uuid,
    /// Applied or rejected.
    pub state: OpResultState,
    /// Why it was rejected; `null` when applied.
    pub error: Option<String>,
}

impl ControlBody for AlertOpResult {
    const MESSAGE_TYPE: MessageType = MessageType::AlertOpResult;
}

/// `alert.act`: stop or snooze exactly the named occurrence or timer ring. The device
/// acknowledges it: `durable` once an alarm operation is journaled (then it uploads the
/// operation under the same `op_id`), `applied` for a stopped timer, `rejected` with a
/// code otherwise.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AlertAct {
    /// The operation id (UUIDv5 for voice and LLM, UUIDv4 otherwise).
    pub op_id: Uuid,
    /// An occurrence id or an HA timer id.
    pub target_id: String,
    /// What to do.
    pub action: ActAction,
    /// Who asked.
    pub source: ActSource,
}

impl ControlBody for AlertAct {
    const MESSAGE_TYPE: MessageType = MessageType::AlertAct;
}

/// `alert.ring`: ring a finished HA timer. Not persisted on the device.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AlertRing {
    /// HA's timer id.
    pub ring_id: String,
    /// The timer's name; empty for an unnamed timer.
    pub name: String,
    /// The sound.
    pub sound: AlertSound,
    /// The pause between loops in ms.
    pub loop_gap_ms: u64,
    /// The ring limit in ms.
    pub max_ring_ms: NonZeroU64,
}

impl ControlBody for AlertRing {
    const MESSAGE_TYPE: MessageType = MessageType::AlertRing;
}

/// `alert.prefetch`: sounds to install ahead of need (`alert_prefetch` only).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AlertPrefetch {
    /// SHA-256 names of alert sounds.
    pub sounds: Vec<Sha256>,
}

impl ControlBody for AlertPrefetch {
    const MESSAGE_TYPE: MessageType = MessageType::AlertPrefetch;
}

/// One timer of `timer.view`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TimerViewEntry {
    /// HA's timer id.
    pub id: String,
    /// HA's name; `null` for an unnamed timer.
    pub name: Option<String>,
    /// The timer's whole length in ms.
    pub total_ms: u64,
    /// The time left when the message was sent, in ms.
    pub remaining_ms: u64,
    /// False while paused.
    pub active: bool,
}

/// `timer.view`: the speaker's timers, the whole list every time (`timer_view_v1` only).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TimerView {
    /// Active timers soonest first, then paused ones by time left.
    pub timers: Vec<TimerViewEntry>,
}

impl ControlBody for TimerView {
    const MESSAGE_TYPE: MessageType = MessageType::TimerView;
}

/// A body the device does not acknowledge with `command.ack`.
#[derive(Clone, Debug, PartialEq)]
pub enum PostBody {
    /// One snapshot page.
    Snapshot(AlertSnapshotPage),
    /// One delta.
    Delta(AlertDelta),
    /// A verdict on a local operation.
    OpResult(AlertOpResult),
    /// The timer view.
    TimerView(TimerView),
}

/// A body the device acknowledges with `command.ack`.
#[derive(Clone, Debug, PartialEq)]
pub enum CommandBody {
    /// Stop or snooze a ring.
    Act(AlertAct),
    /// Ring a finished timer.
    Ring(AlertRing),
    /// Install sounds ahead of need.
    Prefetch(AlertPrefetch),
}

// ── Device → controller ──────────────────────────────────────────────────────

/// `alert.ack`: the device's durable delivery position after a snapshot or delta.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertAck {
    /// The device's epoch after handling the message; `None` before any snapshot.
    pub delivery_epoch: Option<String>,
    /// The highest delivery sequence it has durably applied.
    pub applied_through: u64,
    /// Whether that position is durable.
    pub durable: bool,
    /// The device wants a full snapshot.
    pub need_snapshot: bool,
}

/// `alert.local_operation.child`: the snooze child the device derived.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChildRef {
    /// The child's schedule.
    pub schedule_id: Uuid,
    /// The child occurrence.
    pub occurrence_id: Uuid,
    /// Its due time in UTC ms.
    pub due_utc_ms: u64,
    /// Its due time, RFC 3339 in the parent's offset.
    pub due_local: String,
}

/// `alert.local_operation`: an operation the device journaled. Re-sent with the same
/// `op_id` at every session start until `alert.op_result` answers it.
#[derive(Clone, Debug, PartialEq)]
pub struct AlertLocalOperation {
    /// The operation id.
    pub op_id: Uuid,
    /// Dismiss, snooze or expire.
    pub action: LocalAction,
    /// The occurrence it handled.
    pub occurrence_id: Uuid,
    /// That occurrence's schedule.
    pub schedule_id: Uuid,
    /// The revision of the occurrence the device held.
    pub revision: u64,
    /// Why it expired; `None` unless `action` is expire.
    pub reason: Option<ExpireReason>,
    /// Who asked.
    pub source: LocalSource,
    /// The snooze child; `Some` exactly for a snooze.
    pub child: Option<ChildRef>,
    /// The body as received, whose canonical JSON is the journal's request.
    pub body: Body,
}

/// `alert.ring_ended`: telemetry only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertRingEnded {
    /// The occurrence or timer id.
    pub id: String,
    /// Alarm or timer.
    pub kind: RingKind,
    /// Why it ended.
    pub reason: RingEndReason,
}

/// `alert.state.active`: what rings now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveRing {
    /// The occurrence id or timer id.
    pub id: String,
    /// Alarm or timer.
    pub kind: RingKind,
    /// The label or timer name.
    pub name: String,
    /// False while a turn holds the foreground.
    pub foreground: bool,
    /// When it started, on the device's monotonic clock.
    pub started_mono_ns: u64,
    /// When it expires, on the device's monotonic clock.
    pub deadline_mono_ns: u64,
}

/// `alert.state`: sent on every change and after `session.ready`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlertState {
    /// The ringing alert; `None` when nothing rings.
    pub active: Option<ActiveRing>,
    /// The ids waiting behind it, in order.
    pub queue: Vec<String>,
    /// Whether the device trusts its UTC clock.
    pub clock_trusted: bool,
    /// The alarm wakelock.
    pub wakelock: WakeLockStatus,
    /// The alert store's health.
    pub store: StoreStatus,
}

/// A decoded device alert message.
#[derive(Clone, Debug, PartialEq)]
pub enum AlertInbound {
    /// `alert.ack`.
    Ack(AlertAck),
    /// `alert.local_operation`.
    LocalOperation(Box<AlertLocalOperation>),
    /// `alert.ring_ended`.
    RingEnded(AlertRingEnded),
    /// `alert.state`.
    State(AlertState),
}

struct Reader<'a> {
    body: &'a Body,
    message_type: MessageType,
}

impl<'a> Reader<'a> {
    fn fail<T>(&self, key: &str, what: &str) -> Result<T, BodyError> {
        Err(BodyError {
            message_type: self.message_type,
            detail: format!("{key} {what}"),
        })
    }

    fn string(&self, key: &str) -> Result<&'a str, BodyError> {
        match self.body.get(key) {
            Some(Value::String(text)) => Ok(text),
            _ => self.fail(key, "must be a string"),
        }
    }

    fn nonempty(&self, key: &str) -> Result<&'a str, BodyError> {
        let text = self.string(key)?;
        if text.is_empty() {
            return self.fail(key, "must not be empty");
        }
        Ok(text)
    }

    fn optional_string(&self, key: &str) -> Result<Option<&'a str>, BodyError> {
        match self.body.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            _ => self.fail(key, "must be a string or null"),
        }
    }

    fn count(&self, key: &str) -> Result<u64, BodyError> {
        match self.body.get(key).and_then(Value::as_u64) {
            Some(n) => Ok(n),
            None => self.fail(key, "must be a non-negative integer"),
        }
    }

    fn count_string(&self, key: &str) -> Result<u64, BodyError> {
        match self.string(key)?.parse::<u64>() {
            Ok(n) => Ok(n),
            Err(_) => self.fail(key, "must be a decimal string"),
        }
    }

    fn boolean(&self, key: &str) -> Result<bool, BodyError> {
        match self.body.get(key) {
            Some(Value::Bool(b)) => Ok(*b),
            _ => self.fail(key, "must be a boolean"),
        }
    }

    /// A UUID in the lowercase hyphenated form the firmware writes.
    fn uuid(&self, key: &str) -> Result<Uuid, BodyError> {
        let text = self.string(key)?;
        match Uuid::parse_str(text) {
            Ok(id) if id.hyphenated().to_string() == text => Ok(id),
            _ => self.fail(key, "must be a lowercase UUID"),
        }
    }

    fn word<T>(&self, key: &str, parse: fn(&str) -> Option<T>) -> Result<T, BodyError> {
        let text = self.string(key)?;
        match parse(text) {
            Some(value) => Ok(value),
            None => self.fail(key, &format!("has unknown value {text:?}")),
        }
    }

    fn nested(&self, key: &str) -> Result<Option<Reader<'a>>, BodyError> {
        match self.body.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Object(body)) => Ok(Some(Reader {
                body,
                message_type: self.message_type,
            })),
            _ => self.fail(key, "must be an object or null"),
        }
    }
}

impl AlertInbound {
    /// Decodes the body of a device alert message; `None` for a type that is not one.
    pub fn decode(message_type: MessageType, body: Body) -> Option<Result<Self, BodyError>> {
        let r = Reader {
            body: &body,
            message_type,
        };
        let decoded = match message_type {
            MessageType::AlertAck => decode_ack(&r).map(AlertInbound::Ack),
            MessageType::AlertLocalOperation => {
                decode_local_operation(&r).map(|op| AlertInbound::LocalOperation(Box::new(op)))
            }
            MessageType::AlertRingEnded => decode_ring_ended(&r).map(AlertInbound::RingEnded),
            MessageType::AlertState => decode_state(&r).map(AlertInbound::State),
            _ => return None,
        };
        Some(decoded.map(|decoded| match decoded {
            AlertInbound::LocalOperation(mut op) => {
                op.body = body.clone();
                AlertInbound::LocalOperation(op)
            }
            other => other,
        }))
    }
}

fn decode_ack(r: &Reader<'_>) -> Result<AlertAck, BodyError> {
    Ok(AlertAck {
        delivery_epoch: r.optional_string("delivery_epoch")?.map(str::to_owned),
        applied_through: r.count("applied_through")?,
        durable: r.boolean("durable")?,
        need_snapshot: r.boolean("need_snapshot")?,
    })
}

fn decode_local_operation(r: &Reader<'_>) -> Result<AlertLocalOperation, BodyError> {
    let action = r.word("action", LocalAction::from_wire)?;
    let reason = r
        .optional_string("reason")?
        .map(|text| ExpireReason::from_wire(text).ok_or(text))
        .transpose()
        .or_else(|text| r.fail("reason", &format!("has unknown value {text:?}")))?;
    let child = match r.nested("child")? {
        None => None,
        Some(c) => Some(ChildRef {
            schedule_id: c.uuid("schedule_id")?,
            occurrence_id: c.uuid("occurrence_id")?,
            due_utc_ms: c.count_string("due_utc_ms")?,
            due_local: c.nonempty("due_local")?.to_owned(),
        }),
    };
    if (action == LocalAction::Snooze) != child.is_some() {
        return r.fail("child", "must be present exactly for a snooze");
    }
    if action != LocalAction::Expire && reason.is_some() {
        return r.fail("reason", "belongs to an expiry only");
    }
    Ok(AlertLocalOperation {
        op_id: r.uuid("op_id")?,
        action,
        occurrence_id: r.uuid("occurrence_id")?,
        schedule_id: r.uuid("schedule_id")?,
        revision: r.count("revision")?,
        reason,
        source: r.word("source", LocalSource::from_wire)?,
        child,
        body: Body::new(),
    })
}

fn decode_ring_ended(r: &Reader<'_>) -> Result<AlertRingEnded, BodyError> {
    Ok(AlertRingEnded {
        id: r.nonempty("id")?.to_owned(),
        kind: r.word("kind", RingKind::from_wire)?,
        reason: r.word("reason", RingEndReason::from_wire)?,
    })
}

fn decode_state(r: &Reader<'_>) -> Result<AlertState, BodyError> {
    let active = match r.nested("active")? {
        None => None,
        Some(a) => Some(ActiveRing {
            id: a.nonempty("id")?.to_owned(),
            kind: a.word("kind", RingKind::from_wire)?,
            name: a.string("name")?.to_owned(),
            foreground: a.boolean("foreground")?,
            started_mono_ns: a.count_string("started_mono_ns")?,
            deadline_mono_ns: a.count_string("deadline_mono_ns")?,
        }),
    };
    let queue = match r.body.get("queue") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>(),
        _ => None,
    };
    let Some(queue) = queue else {
        return r.fail("queue", "must be an array of strings");
    };
    Ok(AlertState {
        active,
        queue,
        clock_trusted: r.boolean("clock_trusted")?,
        wakelock: r.word("wakelock", WakeLockStatus::from_wire)?,
        store: r.word("store", StoreStatus::from_wire)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body(value: Value) -> Body {
        match value {
            Value::Object(map) => map,
            _ => unreachable!(),
        }
    }

    #[test]
    fn controller_bodies_have_the_documented_shapes() {
        let act = AlertAct {
            op_id: Uuid::from_u128(1),
            target_id: "abc".to_owned(),
            action: ActAction::Snooze,
            source: ActSource::Llm,
        };
        assert_eq!(
            serde_json::to_value(&act).unwrap(),
            json!({"op_id": "00000000-0000-0000-0000-000000000001", "target_id": "abc",
                   "action": "snooze", "source": "llm"})
        );
        let ring = AlertRing {
            ring_id: "t1".to_owned(),
            name: "pasta".to_owned(),
            sound: AlertSound::Fallback,
            loop_gap_ms: 2_000,
            max_ring_ms: NonZeroU64::new(900_000).unwrap(),
        };
        assert_eq!(
            serde_json::to_value(&ring).unwrap(),
            json!({"ring_id": "t1", "name": "pasta", "sound": "builtin:fallback",
                   "loop_gap_ms": 2000, "max_ring_ms": 900000})
        );
        let result = AlertOpResult {
            op_id: Uuid::from_u128(2),
            state: OpResultState::Applied,
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            json!({"op_id": "00000000-0000-0000-0000-000000000002", "state": "applied", "error": null})
        );
        let view = TimerView {
            timers: vec![TimerViewEntry {
                id: "t1".to_owned(),
                name: None,
                total_ms: 300_000,
                remaining_ms: 123_456,
                active: true,
            }],
        };
        assert_eq!(
            serde_json::to_value(&view).unwrap(),
            json!({"timers": [{"id": "t1", "name": null, "total_ms": 300000,
                               "remaining_ms": 123456, "active": true}]})
        );
        let sha = Sha256::of(b"x");
        assert_eq!(
            serde_json::to_value(AlertPrefetch {
                sounds: vec![sha.clone()]
            })
            .unwrap(),
            json!({"sounds": [sha.as_str()]})
        );
    }

    #[test]
    fn device_bodies_decode_as_the_firmware_marshals_them() {
        let ack = AlertInbound::decode(
            MessageType::AlertAck,
            body(
                json!({"delivery_epoch": null, "applied_through": 0, "durable": true,
                        "need_snapshot": true}),
            ),
        );
        assert_eq!(
            ack,
            Some(Ok(AlertInbound::Ack(AlertAck {
                delivery_epoch: None,
                applied_through: 0,
                durable: true,
                need_snapshot: true
            })))
        );

        let op = json!({"op_id": "164ed469-897e-5b59-b53b-23431a19381b", "action": "snooze",
            "occurrence_id": "0b8f4e0a-3c4b-5e6d-9f10-112233445566",
            "schedule_id": "9cc967de-5fa3-5373-b4f0-d9841f3f79f7", "revision": 17,
            "reason": null, "source": "button",
            "child": {"schedule_id": "9cc967de-5fa3-5373-b4f0-d9841f3f79f7",
                      "occurrence_id": "164ed469-897e-5b59-b53b-23431a19381b",
                      "due_utc_ms": "1790422741000", "due_local": "2026-09-23T06:39:01-05:00"}});
        let Some(Ok(AlertInbound::LocalOperation(decoded))) =
            AlertInbound::decode(MessageType::AlertLocalOperation, body(op.clone()))
        else {
            panic!("snooze did not decode");
        };
        assert_eq!(decoded.action, LocalAction::Snooze);
        assert_eq!(
            decoded.child.as_ref().unwrap().due_utc_ms,
            1_790_422_741_000
        );
        assert_eq!(Value::Object(decoded.body.clone()), op);

        let mut no_child = op.clone();
        no_child.as_object_mut().unwrap().remove("child");
        assert!(matches!(
            AlertInbound::decode(MessageType::AlertLocalOperation, body(no_child)),
            Some(Err(_))
        ));
        let mut upper = op;
        upper["op_id"] = json!("164ED469-897E-5B59-B53B-23431A19381B");
        assert!(matches!(
            AlertInbound::decode(MessageType::AlertLocalOperation, body(upper)),
            Some(Err(_))
        ));

        let state = AlertInbound::decode(
            MessageType::AlertState,
            body(
                json!({"active": {"id": "t1", "kind": "timer", "name": "pasta", "foreground": true,
                                   "started_mono_ns": "5", "deadline_mono_ns": "900000000005"},
                        "queue": ["a"], "clock_trusted": true, "wakelock": "held", "store": "ok"}),
            ),
        );
        let Some(Ok(AlertInbound::State(state))) = state else {
            panic!("state did not decode");
        };
        assert_eq!(state.active.unwrap().kind, RingKind::Timer);
        assert_eq!(state.queue, ["a"]);

        assert_eq!(
            AlertInbound::decode(
                MessageType::AlertRingEnded,
                body(json!({"id": "t1", "kind": "timer", "reason": "limit"}))
            ),
            Some(Ok(AlertInbound::RingEnded(AlertRingEnded {
                id: "t1".to_owned(),
                kind: RingKind::Timer,
                reason: RingEndReason::Limit
            })))
        );
        assert_eq!(AlertInbound::decode(MessageType::Stats, Body::new()), None);
    }
}
