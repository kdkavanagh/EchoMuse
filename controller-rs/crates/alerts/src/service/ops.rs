//! The JSON the journal stores for each operation (see the `journal` module for the
//! columns): requests, plans and results.

use echomuse_ha::{EventTarget, EventWrite};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    api::{Outcome, Rejection, SetAlarm},
    calendar::Occurrence,
    ids::OpId,
    journal::{OpRow, State},
    relay::CancelSelector,
    wire::AlertLocalOperation,
};

/// Request types, the `type` of every request object.
pub(crate) mod kind {
    pub const SET_ALARM: &str = "set_alarm";
    pub const CANCEL_ALARMS: &str = "cancel_alarms";
    pub const CANCEL_MEMBER: &str = "cancel_member";
    pub const LOCAL_OPERATION: &str = "local_operation";
    pub const MISSED: &str = "missed";
}

pub(crate) fn set_request(request: &SetAlarm) -> Value {
    json!({
        "type": kind::SET_ALARM,
        "endpoint": request.endpoint.0,
        "time": request.when.time.text(),
        "date": request.when.date.map(|d| d.to_string()),
        "days": request.when.repeat.days().names(),
        "label": request.label,
    })
}

pub(crate) fn selector_json(selector: &CancelSelector) -> Value {
    match selector {
        CancelSelector::Schedules(ids) => {
            json!({"schedules": ids.iter().map(Uuid::to_string).collect::<Vec<_>>()})
        }
        CancelSelector::Label(label) => json!({"label": label}),
        CancelSelector::Time(time) => json!({"time": time.text()}),
        CancelSelector::All => json!({"all": true}),
    }
}

pub(crate) fn cancel_request(endpoint: &str, selector: &CancelSelector) -> Value {
    json!({"type": kind::CANCEL_ALARMS, "endpoint": endpoint, "selector": selector_json(selector)})
}

pub(crate) fn cancel_member_request(root: OpId, schedule_id: Uuid) -> Value {
    json!({"type": kind::CANCEL_MEMBER, "root": root.to_string(), "schedule_id": schedule_id.to_string()})
}

pub(crate) fn local_request(endpoint: &str, op: &AlertLocalOperation) -> Value {
    json!({"type": kind::LOCAL_OPERATION, "endpoint": endpoint, "body": Value::Object(op.body.clone())})
}

pub(crate) fn missed_request(endpoint: &str, occurrence: &Occurrence) -> Value {
    json!({
        "type": kind::MISSED,
        "endpoint": endpoint,
        "occurrence_id": occurrence.occurrence_id.to_string(),
        "schedule_id": occurrence.schedule_id.to_string(),
        "key": occurrence.key,
    })
}

pub(crate) fn request_kind(row: &OpRow) -> Option<&str> {
    row.request()?.get("type")?.as_str()
}

pub(crate) fn event_json(event: &EventWrite) -> Value {
    json!({
        "summary": event.summary,
        "dtstart": event.dtstart,
        "dtend": event.dtend,
        "description": event.description,
        "rrule": event.rrule,
    })
}

pub(crate) fn event_from(value: &Value) -> Option<EventWrite> {
    let text = |key: &str| value.get(key)?.as_str().map(str::to_owned);
    Some(EventWrite {
        summary: text("summary")?,
        dtstart: text("dtstart")?,
        dtend: text("dtend")?,
        description: text("description"),
        rrule: text("rrule"),
    })
}

pub(crate) fn target_json(occurrence: &Occurrence) -> Value {
    json!({"uid": occurrence.uid, "recurrence_id": occurrence.recurrence_id})
}

pub(crate) fn target_from(value: &Value) -> Option<EventTarget> {
    Some(EventTarget {
        uid: value.get("uid")?.as_str()?.to_owned(),
        recurrence_id: value
            .get("recurrence_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// The instance an occurrence delete acts on: one instance of a series, or the whole
/// one-shot event.
pub(crate) fn occurrence_target(occurrence: &Occurrence) -> EventTarget {
    EventTarget {
        uid: occurrence.uid.clone(),
        recurrence_id: occurrence.recurrence_id.clone(),
    }
}

pub(crate) fn created_result(occurrence: &Occurrence) -> Value {
    json!({
        "ok": true,
        "schedule_id": occurrence.schedule_id.to_string(),
        "occurrence_id": occurrence.occurrence_id.to_string(),
        "uid": occurrence.uid,
        "due_local": occurrence.due_local,
        "due_utc_ms": occurrence.due_utc_ms.to_string(),
    })
}

pub(crate) fn deleted_result(written: bool) -> Value {
    json!({"ok": true, "written": written})
}

pub(crate) fn rejected_result(code: &str, detail: &str) -> Value {
    json!({"ok": false, "error": code, "detail": detail})
}

/// Stable rejection codes.
pub(crate) mod code {
    pub const HA_REFUSED: &str = "ha_refused";
    pub const NOT_CONFIRMED: &str = "not_confirmed";
    pub const PARENT_GONE: &str = "parent_gone";
    pub const UNKNOWN_OCCURRENCE: &str = "unknown_occurrence";
    pub const INVALID_CHILD: &str = "invalid_child";
}

pub(crate) fn rejection_of(row: &OpRow) -> Rejection {
    let result = row.result.as_ref();
    let text = |key: &str| {
        result
            .and_then(|r| r.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    Rejection {
        code: text("error"),
        detail: text("detail"),
    }
}

/// The outcome a row stands for; `read` turns an applied row's result into `T`.
pub(crate) fn outcome_of<T>(
    row: Option<&OpRow>,
    op_id: OpId,
    read: impl FnOnce(&OpRow) -> T,
) -> Outcome<T> {
    match row {
        Some(row) if row.state == Some(State::Applied) => Outcome::Applied {
            op_id,
            result: read(row),
        },
        Some(row) if row.state == Some(State::Rejected) => Outcome::Rejected {
            op_id,
            reason: rejection_of(row),
        },
        _ => Outcome::Pending { op_id },
    }
}
