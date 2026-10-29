//! Intents run directly through HA's REST endpoint `POST /api/intent/handle`.
//!
//! HA (`intent/__init__.py`, `IntentHandleView`) takes `{name, data, device_id}`, where each
//! data entry becomes a slot value, and answers with its intent response
//! (`helpers/intent.py`, `IntentResponse.as_dict`). An intent that fails to match or to
//! run is still HTTP 200, with `response_type: "error"`, `data.code` and the reason as
//! plain speech. A malformed request is a 4xx before any handler runs. Any other failure
//! inside a handler is an HTTP 500, after which HA cannot say what the handler did.

use echomuse_core::{
    services::{
        DirectIntent, DirectIntentRequest, EffectOutcome, HaDeviceId, HaError, HaTimer,
        NotDispatched, TimerSelector, TimerStatus, UnknownCause,
    },
    types::WorkScope,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    client::{HaClient, RestFailure},
    connection::Refusal,
};

/// HA's `IntentResponseType`, as sent: `action_done`, `partial_action_done`,
/// `query_answer` or `error`.
const ERROR_RESPONSE: &str = "error";

#[derive(Deserialize)]
struct IntentResponse {
    response_type: String,
    speech: Option<Speech>,
    data: Option<ResponseData>,
    speech_slots: Option<Value>,
}

#[derive(Deserialize)]
struct Speech {
    plain: Option<PlainSpeech>,
}

#[derive(Deserialize)]
struct PlainSpeech {
    speech: String,
}

#[derive(Deserialize)]
struct ResponseData {
    code: Option<String>,
}

/// `HassTimerStatus` speech slots (`intent/timers.py`). HA reports an absent name or
/// device as an empty string and a start unit the timer was not given as 0.
#[derive(Deserialize)]
struct TimerSlots {
    timers: Vec<WireTimer>,
}

#[derive(Deserialize)]
struct WireTimer {
    id: String,
    name: String,
    device_id: String,
    start_hours: u32,
    start_minutes: u32,
    start_seconds: u32,
    is_active: bool,
}

/// What an answered intent request says.
enum IntentAnswer {
    /// HA ran the intent.
    Done(IntentResponse),
    /// HA refused it before or while matching; nothing was applied.
    Refused(Refusal),
    /// HA answered without saying whether it was applied.
    Inconclusive(String),
}

fn non_empty(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

fn answer(status: u16, body: &[u8]) -> IntentAnswer {
    match status {
        200 => match serde_json::from_slice::<IntentResponse>(body) {
            Err(err) => IntentAnswer::Inconclusive(format!("unreadable intent response: {err}")),
            Ok(response) if response.response_type == ERROR_RESPONSE => {
                match response.data.and_then(|data| data.code) {
                    Some(code) => IntentAnswer::Refused(Refusal {
                        code,
                        message: response
                            .speech
                            .and_then(|speech| speech.plain)
                            .map(|plain| plain.speech)
                            .unwrap_or_default(),
                    }),
                    None => IntentAnswer::Inconclusive("error response without a code".to_owned()),
                }
            }
            Ok(response) => IntentAnswer::Done(response),
        },
        400..=499 => IntentAnswer::Refused(Refusal {
            code: format!("http_{status}"),
            message: message_of(body),
        }),
        _ => IntentAnswer::Inconclusive(format!("HTTP {status}: {}", message_of(body))),
    }
}

/// HA's JSON error `message`, or the body as text.
fn message_of(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Message {
        message: String,
    }
    serde_json::from_slice::<Message>(body)
        .map(|parsed| parsed.message)
        .unwrap_or_else(|_| String::from_utf8_lossy(body).into_owned())
}

fn request_body(name: &str, slots: Map<String, Value>, device: Option<&HaDeviceId>) -> Value {
    json!({
        "name": name,
        "data": slots,
        "device_id": device.map(|device| device.0.as_str()),
    })
}

fn cancel_slots(selector: &TimerSelector) -> Map<String, Value> {
    let mut slots = Map::new();
    if let Some(name) = &selector.name {
        slots.insert("name".to_owned(), Value::String(name.clone()));
    }
    for (slot, value) in [
        ("start_hours", selector.start_hours),
        ("start_minutes", selector.start_minutes),
        ("start_seconds", selector.start_seconds),
    ] {
        if let Some(value) = value {
            slots.insert(slot.to_owned(), Value::from(value));
        }
    }
    slots
}

fn timers_of(response: IntentResponse) -> Result<TimerStatus, HaError> {
    let slots = response
        .speech_slots
        .ok_or_else(|| HaError::BadResponse("HassTimerStatus without speech slots".to_owned()))?;
    let slots: TimerSlots = serde_json::from_value(slots)
        .map_err(|err| HaError::BadResponse(format!("HassTimerStatus timers: {err}")))?;
    let timers = slots
        .timers
        .into_iter()
        .map(|timer| HaTimer {
            id: timer.id,
            name: non_empty(timer.name),
            ha_device: non_empty(timer.device_id).map(HaDeviceId),
            start_hours: timer.start_hours,
            start_minutes: timer.start_minutes,
            start_seconds: timer.start_seconds,
            is_active: timer.is_active,
        })
        .collect();
    Ok(TimerStatus {
        timers,
        response_type: response.response_type,
    })
}

impl HaClient {
    pub(crate) async fn timers(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> Result<TimerStatus, HaError> {
        if scope.is_cancelled() {
            return Err(HaError::NotDispatched(NotDispatched::Cancelled));
        }
        let body = request_body("HassTimerStatus", Map::new(), ha_device.as_ref());
        let (status, reply) = self.rest_post("/intent/handle", &body).await?;
        match answer(status, &reply) {
            IntentAnswer::Done(response) => timers_of(response),
            IntentAnswer::Refused(refusal) => Err(refusal.into()),
            IntentAnswer::Inconclusive(detail) => Err(HaError::BadResponse(detail)),
        }
    }

    pub(crate) async fn run_direct_intent(
        &self,
        request: DirectIntentRequest,
    ) -> EffectOutcome<()> {
        let (name, slots) = match &request.intent {
            DirectIntent::CancelTimer(selector) => ("HassCancelTimer", cancel_slots(selector)),
            DirectIntent::CancelAllTimers => ("HassCancelAllTimers", Map::new()),
        };
        if request.scope.is_cancelled() {
            return EffectOutcome::NotDispatched(NotDispatched::Cancelled);
        }
        let body = request_body(name, slots, request.ha_device.as_ref());
        let sent = tokio::select! {
            biased;
            sent = self.rest_post("/intent/handle", &body) => sent,
            () = request.scope.cancelled() => {
                return EffectOutcome::Unknown(UnknownCause::Withdrawn);
            }
        };
        match sent {
            Err(RestFailure::NotSent) => EffectOutcome::NotDispatched(NotDispatched::Unavailable),
            Err(RestFailure::TimedOut) => EffectOutcome::Unknown(UnknownCause::Timeout),
            Err(RestFailure::Lost) => EffectOutcome::Unknown(UnknownCause::ConnectionLost),
            Ok((status, reply)) => match answer(status, &reply) {
                IntentAnswer::Done(_) => EffectOutcome::Applied(()),
                IntentAnswer::Refused(refusal) => EffectOutcome::Rejected {
                    code: refusal.code,
                    message: refusal.message,
                },
                IntentAnswer::Inconclusive(_) => EffectOutcome::Unknown(UnknownCause::Inconclusive),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_units_and_name_a_selector_gives_become_slots() {
        let slots = cancel_slots(&TimerSelector {
            name: Some("pasta".to_owned()),
            start_hours: None,
            start_minutes: Some(5),
            start_seconds: None,
        });
        assert_eq!(
            Value::Object(slots),
            json!({"name": "pasta", "start_minutes": 5})
        );
        assert!(cancel_slots(&TimerSelector::default()).is_empty());
    }

    #[test]
    fn answers_are_classified_by_status_and_response_type() {
        let done = br#"{"speech":{},"card":{},"language":"en","response_type":"action_done","data":{"success":[],"failed":[]}}"#;
        assert!(matches!(answer(200, done), IntentAnswer::Done(_)));

        let failed = br#"{"speech":{"plain":{"speech":"Timer not found","extra_data":null}},"card":{},"language":"en","response_type":"error","data":{"code":"failed_to_handle"}}"#;
        match answer(200, failed) {
            IntentAnswer::Refused(refusal) => {
                assert_eq!(refusal.code, "failed_to_handle");
                assert_eq!(refusal.message, "Timer not found");
            }
            _ => panic!("an error response is a refusal"),
        }

        match answer(400, br#"{"message":"Message format incorrect"}"#) {
            IntentAnswer::Refused(refusal) => {
                assert_eq!(refusal.code, "http_400");
                assert_eq!(refusal.message, "Message format incorrect");
            }
            _ => panic!("a 4xx is a refusal"),
        }
        assert!(matches!(
            answer(500, b"500 Internal Server Error"),
            IntentAnswer::Inconclusive(_)
        ));
        assert!(matches!(answer(502, b""), IntentAnswer::Inconclusive(_)));
        assert!(matches!(
            answer(200, b"<html>"),
            IntentAnswer::Inconclusive(_)
        ));
    }
}
