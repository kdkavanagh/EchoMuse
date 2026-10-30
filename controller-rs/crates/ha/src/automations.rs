//! The sentence triggers of Home Assistant's enabled automations, for transcript
//! correction's known phrases (spec §4.7).
//!
//! The shapes follow Home Assistant 2026.8.1:
//!
//! - `GET /api/states` lists every entity; an automation is `automation.<object id>`,
//!   its state `on` while enabled, and its `id` attribute names its stored configuration
//!   (an automation written in YAML without an `id` has none and is left out).
//! - `GET /api/config/automation/config/<id>` (`config/automation.py`) answers with the
//!   stored configuration, or 404. Its triggers are under `triggers`, or `trigger` in an
//!   older configuration; a conversation trigger is `{"trigger": "conversation"}` (or
//!   `"platform"` in the older spelling) with `command` a template or a list of them.
//! - `automation_reloaded` fires whenever the automations change.

use echomuse_core::services::HaError;
use serde_json::Value;

use crate::client::{HaClient, rest_message, rest_read};

/// The event Home Assistant fires when its automations change.
pub const AUTOMATIONS_EVENT: &str = "automation_reloaded";

/// The sentence templates of a stored automation configuration's conversation triggers.
pub fn conversation_commands(config: &Value) -> Vec<String> {
    let triggers = config
        .get("triggers")
        .or_else(|| config.get("trigger"))
        .map(|t| match t {
            Value::Array(items) => items.iter().collect::<Vec<_>>(),
            single => vec![single],
        })
        .unwrap_or_default();
    let mut commands = Vec::new();
    for trigger in triggers {
        let kind = trigger
            .get("trigger")
            .or_else(|| trigger.get("platform"))
            .and_then(Value::as_str);
        if kind != Some("conversation") {
            continue;
        }
        match trigger.get("command") {
            Some(Value::String(one)) => commands.push(one.clone()),
            Some(Value::Array(many)) => {
                commands.extend(many.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            _ => {}
        }
    }
    commands
}

impl HaClient {
    /// The conversation-trigger templates of every enabled automation, as written.
    pub async fn automation_sentences(&self) -> Result<Vec<String>, HaError> {
        let body = rest_read(self.rest_get("/states").await)?;
        let states: Value = serde_json::from_slice(&body)
            .map_err(|error| HaError::BadResponse(format!("states: {error}")))?;
        let ids: Vec<String> = states
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|s| {
                s.get("entity_id")
                    .and_then(Value::as_str)
                    .is_some_and(|e| e.starts_with("automation."))
                    && s.get("state").and_then(Value::as_str) == Some("on")
            })
            .filter_map(|s| s.get("attributes")?.get("id")?.as_str().map(str::to_owned))
            .collect();
        let mut sentences = Vec::new();
        for id in ids {
            let (status, body) = self
                .rest_get(&format!("/config/automation/config/{id}"))
                .await?;
            match status {
                404 => continue,
                200..=299 => {
                    let config: Value = serde_json::from_slice(&body).map_err(|error| {
                        HaError::BadResponse(format!("automation config: {error}"))
                    })?;
                    sentences.extend(conversation_commands(&config));
                }
                _ => {
                    return Err(HaError::Rejected {
                        code: status.to_string(),
                        message: rest_message(&body),
                    });
                }
            }
        }
        Ok(sentences)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::conversation_commands;

    #[test]
    fn both_spellings_of_conversation_triggers_are_read() {
        let new = json!({"triggers": [
            {"trigger": "conversation", "command": ["arm the alarm", "[good] night"]},
            {"trigger": "state", "entity_id": "light.kitchen"},
        ]});
        assert_eq!(
            conversation_commands(&new),
            ["arm the alarm", "[good] night"]
        );
        let old = json!({"trigger": {"platform": "conversation", "command": "movie time"}});
        assert_eq!(conversation_commands(&old), ["movie time"]);
        assert!(conversation_commands(&json!({"alias": "none"})).is_empty());
    }
}
