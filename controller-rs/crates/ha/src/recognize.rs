//! HA's built-in sentence matcher without execution: `conversation/agent/homeassistant/debug`.
//!
//! HA (`conversation/default_agent.py`, `async_debug_recognize`) answers each sentence, in
//! order, with null when nothing matched, with `{match: true, source: "trigger",
//! sentence_template}` for a sentence trigger (which it reports without firing), or for an
//! intent with `intent.name`, `slots`, `details` (`{name, value, text}` per slot),
//! `targets` (`{entity_id: {matched}}`), `match` (true when no slot was left unmatched),
//! `sentence_template`, `unmatched_slots` (slot to text or number) and, when known,
//! `source` (`builtin` or `custom`).

use std::collections::BTreeMap;

use echomuse_core::services::{HaError, RecognizeRequest, SentenceRecognition, SlotValue};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{HaClient, object};

#[derive(Deserialize)]
struct DebugResult {
    results: Vec<Option<WireRecognition>>,
}

#[derive(Deserialize)]
struct WireRecognition {
    #[serde(rename = "match")]
    matched: bool,
    source: Option<String>,
    sentence_template: String,
    intent: Option<WireIntent>,
    #[serde(default)]
    details: BTreeMap<String, WireSlot>,
    #[serde(default)]
    unmatched_slots: BTreeMap<String, Value>,
    #[serde(default)]
    targets: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct WireIntent {
    name: String,
}

#[derive(Deserialize)]
struct WireSlot {
    value: Value,
    text: String,
}

/// A slot value as text: strings as they are, numbers and other JSON as JSON text. None
/// for a null value, which fills nothing.
fn value_text(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text),
        other => Some(other.to_string()),
    }
}

fn recognition(wire: WireRecognition) -> SentenceRecognition {
    SentenceRecognition {
        matched: wire.matched,
        unfilled_slots: !wire.unmatched_slots.is_empty(),
        source: wire.source,
        template: wire.sentence_template,
        intent: wire.intent.map(|intent| intent.name),
        details: wire
            .details
            .into_iter()
            .filter_map(|(slot, detail)| {
                value_text(detail.value).map(|value| {
                    (
                        slot,
                        SlotValue {
                            value,
                            text: detail.text,
                        },
                    )
                })
            })
            .collect(),
        unmatched: wire
            .unmatched_slots
            .into_iter()
            .filter_map(|(slot, text)| value_text(text).map(|text| (slot, text)))
            .collect(),
        targets: wire.targets.into_keys().collect(),
    }
}

/// Reads HA's answer for `sentences` sentences.
pub(crate) fn parse_recognitions(
    result: Value,
    sentences: usize,
) -> Result<Vec<Option<SentenceRecognition>>, HaError> {
    let parsed: DebugResult = serde_json::from_value(result)
        .map_err(|err| HaError::BadResponse(format!("recognition: {err}")))?;
    if parsed.results.len() != sentences {
        return Err(HaError::BadResponse(format!(
            "recognition: {} results for {sentences} sentences",
            parsed.results.len()
        )));
    }
    Ok(parsed
        .results
        .into_iter()
        .map(|result| result.map(recognition))
        .collect())
}

impl HaClient {
    pub(crate) async fn recognize_sentences(
        &self,
        request: RecognizeRequest,
    ) -> Result<Vec<Option<SentenceRecognition>>, HaError> {
        let count = request.sentences.len();
        let mut body = object(json!({
            "type": "conversation/agent/homeassistant/debug",
            "sentences": request.sentences,
            "device_id": request.ha_device.as_ref().map(|device| device.0.as_str()),
        }));
        if let Some(language) = request.language {
            body.insert("language".to_owned(), Value::String(language));
        }
        let timeout = self.options().command_timeout;
        let result = self.command(Some(&request.scope), body, timeout).await?;
        parse_recognitions(result, count)
    }
}
