//! The LLM relay (§3.3.4, architecture §16.7): `echomuse_alert_request` events from the
//! five scripts, read and checked here before the service acts on them.
//!
//! Each event's data is `{request_id, action, args}`. HA renders the scripts' templates
//! to native types, so `args` arrives as an object (a string holding a JSON object is
//! read too). A missing required field, a `cancel_alarm` without `name`, `time` or
//! `all: true`, or a `time` that is not `HH:MM[:SS]` is answered with
//! `{"ok": false, "error": "<reason>"}` without a write.

use serde_json::{Map, Value, json};

use crate::{
    api::{ActOutcome, AlarmEntry, AlarmSet, CancelOutcome, Outcome},
    ids::OpId,
    schedule::{TimeOfDay, Weekdays},
    wire::RingKind,
};

/// How a cancellation picks its alarms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelSelector {
    /// These schedules.
    Schedules(Vec<uuid::Uuid>),
    /// Alarms whose label matches, ignoring case.
    Label(String),
    /// Alarms ringing at this time of day.
    Time(TimeOfDay),
    /// Every alarm of the speaker.
    All,
}

/// A checked relay request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayCall {
    /// `set_alarm`.
    SetAlarm {
        /// The time of day.
        time: TimeOfDay,
        /// The weekdays; none means once.
        days: Weekdays,
        /// The label.
        name: Option<String>,
        /// The speaker named, if any.
        speaker: Option<String>,
    },
    /// `list_alarms`.
    ListAlarms {
        /// The speaker named, if any.
        speaker: Option<String>,
    },
    /// `cancel_alarm`.
    CancelAlarm {
        /// Which alarms.
        selector: CancelSelector,
        /// The speaker named, if any.
        speaker: Option<String>,
    },
    /// `dismiss_alert`.
    DismissAlert {
        /// The speaker named, if any.
        speaker: Option<String>,
    },
    /// `snooze_alarm`.
    SnoozeAlarm {
        /// The speaker named, if any.
        speaker: Option<String>,
    },
}

impl RelayCall {
    /// The speaker the call names, if any.
    pub fn speaker(&self) -> Option<&str> {
        match self {
            RelayCall::SetAlarm { speaker, .. }
            | RelayCall::ListAlarms { speaker }
            | RelayCall::CancelAlarm { speaker, .. }
            | RelayCall::DismissAlert { speaker }
            | RelayCall::SnoozeAlarm { speaker } => speaker.as_deref(),
        }
    }
}

/// One relay request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayRequest {
    /// The script's request id: `"<context id>|<action>|<args as JSON>"`.
    pub request_id: String,
    /// The checked call, or why it was refused.
    pub call: Result<RelayCall, String>,
}

impl RelayRequest {
    /// The operation id of any write the request makes: `uuid5(URL, request_id)`.
    pub fn op_id(&self) -> OpId {
        OpId::relay(&self.request_id)
    }
}

fn text(args: &Map<String, Value>, key: &str) -> Option<String> {
    match args.get(key) {
        Some(Value::String(text)) => Some(text.trim().to_owned()).filter(|t| !t.is_empty()),
        _ => None,
    }
}

fn flag(args: &Map<String, Value>, key: &str) -> Result<bool, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(t)) if t.eq_ignore_ascii_case("true") => Ok(true),
        Some(Value::String(t)) if t.eq_ignore_ascii_case("false") || t.is_empty() => Ok(false),
        Some(_) => Err(format!("{key} must be true or false")),
    }
}

fn time(args: &Map<String, Value>) -> Result<Option<TimeOfDay>, String> {
    match text(args, "time") {
        None => Ok(None),
        Some(t) => TimeOfDay::parse(&t)
            .map(Some)
            .ok_or_else(|| format!("time {t:?} is not HH:MM[:SS]")),
    }
}

fn days(args: &Map<String, Value>) -> Result<Weekdays, String> {
    let names: Vec<String> = match args.get("days") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or("days must be weekday names")?,
        Some(Value::String(t)) if t.trim().is_empty() => Vec::new(),
        Some(Value::String(t)) => match serde_json::from_str::<Vec<String>>(t) {
            Ok(names) => names,
            Err(_) => vec![t.clone()],
        },
        Some(_) => return Err("days must be weekday names".to_owned()),
    };
    Weekdays::from_names(names.iter().map(String::as_str))
        .ok_or_else(|| "days must be mon, tue, wed, thu, fri, sat or sun".to_owned())
}

/// Reads one `echomuse_alert_request` event's data. `None` when it has no request id,
/// since an answer could not reach anyone.
pub fn parse_request(data: &Value) -> Option<RelayRequest> {
    let request_id = data.get("request_id")?.as_str()?.to_owned();
    if request_id.is_empty() {
        return None;
    }
    let call = parse_call(data);
    Some(RelayRequest { request_id, call })
}

fn parse_call(data: &Value) -> Result<RelayCall, String> {
    let action = data
        .get("action")
        .and_then(Value::as_str)
        .ok_or("missing action")?;
    let parsed;
    let args = match data.get("args") {
        Some(Value::Object(args)) => args,
        Some(Value::String(t)) => {
            parsed = serde_json::from_str::<Map<String, Value>>(t)
                .map_err(|_| "args must be an object".to_owned())?;
            &parsed
        }
        None | Some(Value::Null) => {
            parsed = Map::new();
            &parsed
        }
        Some(_) => return Err("args must be an object".to_owned()),
    };
    let speaker = text(args, "speaker");
    match action {
        "set_alarm" => Ok(RelayCall::SetAlarm {
            time: time(args)?.ok_or("missing time")?,
            days: days(args)?,
            name: text(args, "name"),
            speaker,
        }),
        "list_alarms" => Ok(RelayCall::ListAlarms { speaker }),
        "cancel_alarm" => {
            let selector = if flag(args, "all")? {
                CancelSelector::All
            } else if let Some(name) = text(args, "name") {
                CancelSelector::Label(name)
            } else if let Some(time) = time(args)? {
                CancelSelector::Time(time)
            } else {
                return Err("cancel_alarm needs name, time or all".to_owned());
            };
            Ok(RelayCall::CancelAlarm { selector, speaker })
        }
        "dismiss_alert" => Ok(RelayCall::DismissAlert { speaker }),
        "snooze_alarm" => Ok(RelayCall::SnoozeAlarm { speaker }),
        other => Err(format!("unknown action {other:?}")),
    }
}

/// `{"ok": false, "error": reason}`.
pub fn error_result(reason: &str) -> Value {
    json!({"ok": false, "error": reason})
}

/// The answer for an operation journaled but not yet confirmed.
pub fn pending_result(op_id: OpId) -> Value {
    json!({"ok": true, "stored_in_ha": false, "pending": true, "op_id": op_id.to_string()})
}

/// The `echomuse_alert_result` event data.
pub fn result_event(request_id: &str, result: &Value) -> Value {
    json!({"request_id": request_id, "result": result})
}

/// The answer for a request still unanswered when the relay's deadline passes.
pub(crate) fn unconfirmed_result(op_id: OpId) -> Value {
    json!({
        "ok": false,
        "pending": true,
        "op_id": op_id.to_string(),
        "error": "the speaker has not confirmed yet",
    })
}

/// How a relay answer is worded.
#[derive(Clone, Debug)]
pub(crate) enum RelayShape {
    Set {
        speaker: String,
    },
    Cancel,
    Stop {
        kind: RingKind,
        name: String,
        snooze: bool,
    },
}

/// The answer to `list_alarms`.
pub(crate) fn list_result(speaker: &str, entries: &[AlarmEntry]) -> Value {
    json!({
        "ok": true,
        "speaker": speaker,
        "alarms": entries.iter().map(|e| json!({
            "label": e.label,
            "due": e.due_local,
            "repeats": e.rrule,
            "kind": e.kind.as_str(),
        })).collect::<Vec<_>>(),
    })
}

/// The answer to `set_alarm`.
pub(crate) fn set_result(outcome: &Outcome<AlarmSet>, shape: &RelayShape) -> Value {
    let speaker = match shape {
        RelayShape::Set { speaker } => speaker.clone(),
        _ => String::new(),
    };
    match outcome {
        Outcome::Applied { op_id, result } => json!({
            "ok": true,
            "stored_in_ha": true,
            "armed_on_endpoint": result.armed_on_endpoint,
            "delivery_pending": !result.armed_on_endpoint,
            "speaker": speaker,
            "first_due": result.first_due_local,
            "op_id": op_id.to_string(),
        }),
        Outcome::Pending { op_id } => pending_result(*op_id),
        Outcome::Rejected { op_id, reason } => json!({
            "ok": false, "error": reason.code, "detail": reason.detail, "op_id": op_id.to_string(),
        }),
        Outcome::Refused(refusal) => error_result(&refusal.to_string()),
    }
}

/// The answer to `cancel_alarm`.
pub(crate) fn cancel_result(outcome: &CancelOutcome) -> Value {
    match outcome {
        CancelOutcome::Refused(refusal) => error_result(&refusal.to_string()),
        CancelOutcome::Journaled { root, targets } => {
            let state = |o: &Outcome<()>| match o {
                Outcome::Applied { .. } => "cancelled",
                Outcome::Pending { .. } => "pending",
                Outcome::Rejected { .. } => "failed",
                Outcome::Refused(_) => "refused",
            };
            json!({
                "ok": targets.iter().all(|t| !matches!(t.outcome, Outcome::Rejected { .. } | Outcome::Refused(_))),
                "stored_in_ha": targets.iter().all(|t| matches!(t.outcome, Outcome::Applied { .. })),
                "cancelled": targets.iter().map(|t| json!({"label": t.label, "state": state(&t.outcome)})).collect::<Vec<_>>(),
                "op_id": root.to_string(),
            })
        }
    }
}

/// The answer to `dismiss_alert` and `snooze_alarm`; `snooze_due` is when an applied
/// snooze rings again, as its stored result says.
pub(crate) fn stop_result(
    outcome: &ActOutcome,
    shape: &RelayShape,
    snooze_due: Option<&Value>,
) -> Value {
    let (kind, name, snooze) = match shape {
        RelayShape::Stop { kind, name, snooze } => (*kind, name.clone(), *snooze),
        _ => (RingKind::Alarm, String::new(), false),
    };
    let what = json!({"kind": kind.as_str(), "name": name});
    match outcome {
        ActOutcome::Applied { op_id, .. } => {
            let mut result = json!({"ok": true, "stored_in_ha": kind != RingKind::Timer, "op_id": op_id.to_string()});
            if snooze {
                result["snoozed"] = what;
                if let Some(due) = snooze_due {
                    result["due"] = due.clone();
                }
            } else {
                result["stopped"] = what;
            }
            result
        }
        ActOutcome::Pending { op_id } => {
            let mut result = pending_result(*op_id);
            result[if snooze { "snoozed" } else { "stopped" }] = what;
            result
        }
        ActOutcome::Rejected { op_id, reason } => json!({
            "ok": false, "error": reason.code, "detail": reason.detail, "op_id": op_id.to_string(),
        }),
        ActOutcome::Unknown { op_id, .. } => json!({
            "ok": false, "error": "the speaker did not confirm", "op_id": op_id.to_string(),
        }),
        ActOutcome::Refused(refusal) => error_result(&refusal.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(action: &str, args: Value) -> Result<RelayCall, String> {
        parse_request(&json!({"request_id": "ctx|a|{}", "action": action, "args": args}))
            .unwrap()
            .call
    }

    #[test]
    fn requests_are_read_as_the_scripts_render_them() {
        assert_eq!(
            call(
                "set_alarm",
                json!({"time": "07:30:00", "days": ["mon", "fri"], "name": "", "speaker": "Kitchen"})
            ),
            Ok(RelayCall::SetAlarm {
                time: TimeOfDay::new(7, 30, 0).unwrap(),
                days: Weekdays::from_names(["mon", "fri"]).unwrap(),
                name: None,
                speaker: Some("Kitchen".to_owned()),
            })
        );
        assert_eq!(
            call(
                "cancel_alarm",
                json!({"name": "", "time": "", "all": true, "speaker": ""})
            ),
            Ok(RelayCall::CancelAlarm {
                selector: CancelSelector::All,
                speaker: None
            })
        );
        assert_eq!(
            call(
                "cancel_alarm",
                json!("{\"name\": \"Wake up\", \"time\": \"\", \"all\": false, \"speaker\": \"\"}")
            ),
            Ok(RelayCall::CancelAlarm {
                selector: CancelSelector::Label("Wake up".to_owned()),
                speaker: None
            })
        );
    }

    #[test]
    fn invalid_requests_say_why_without_a_write() {
        assert_eq!(
            call("set_alarm", json!({"time": ""})),
            Err("missing time".to_owned())
        );
        assert!(
            call("set_alarm", json!({"time": "7.30"}))
                .unwrap_err()
                .contains("HH:MM")
        );
        assert!(call("set_alarm", json!({"time": "07:30", "days": ["someday"]})).is_err());
        assert_eq!(
            call(
                "cancel_alarm",
                json!({"name": "", "time": "", "all": false})
            ),
            Err("cancel_alarm needs name, time or all".to_owned())
        );
        assert!(call("launch", json!({})).is_err());
        assert_eq!(parse_request(&json!({"action": "list_alarms"})), None);
        let request = parse_request(&json!({"request_id": "r1", "action": "list_alarms"})).unwrap();
        assert_eq!(request.op_id(), OpId::relay("r1"));
    }
}
