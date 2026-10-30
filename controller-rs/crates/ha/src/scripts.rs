//! Script configuration, entity exposure and custom events, for the alert service's LLM
//! tools (§3.3.4).
//!
//! The shapes follow Home Assistant 2026.8.1:
//!
//! - `config/script.py` and `config/view.py`: `GET /api/config/script/config/<id>` answers
//!   with the stored configuration or 404, and `POST` stores the posted object as it is
//!   (after validating it; a 400 names what is malformed) and then reloads scripts.
//! - `homeassistant/expose_entity` takes `assistants`, `entity_ids` and `should_expose`.
//! - `call_service` and `fire_event` are the stock WebSocket commands.

use echomuse_core::services::{EffectOutcome, HaError};
use serde_json::{Value, json};

use crate::client::{HaClient, object, rest_effect, rest_message};

impl HaClient {
    /// The stored configuration of script `object_id`; `None` when HA has none.
    pub async fn script_config(&self, object_id: &str) -> Result<Option<Value>, HaError> {
        let (status, body) = self
            .rest_get(&format!("/config/script/config/{object_id}"))
            .await?;
        match status {
            404 => Ok(None),
            200..=299 => serde_json::from_slice(&body)
                .map(Some)
                .map_err(|err| HaError::BadResponse(format!("script config: {err}"))),
            _ => Err(HaError::Rejected {
                code: status.to_string(),
                message: rest_message(&body),
            }),
        }
    }

    /// Stores the configuration of script `object_id`. Never resent once it may have been
    /// sent.
    pub async fn write_script_config(&self, object_id: &str, config: &Value) -> EffectOutcome<()> {
        let sent = self
            .rest_post(&format!("/config/script/config/{object_id}"), config)
            .await;
        rest_effect(sent, |_| Some(()))
    }

    /// Reloads HA's scripts (`script.reload`).
    pub async fn reload_scripts(&self) -> EffectOutcome<()> {
        self.apply(object(
            json!({"type": "call_service", "domain": "script", "service": "reload"}),
        ))
        .await
    }

    /// Exposes `entity_ids` to the conversation assistant
    /// (`homeassistant/expose_entity`).
    pub async fn expose_to_conversation(&self, entity_ids: &[String]) -> EffectOutcome<()> {
        self.apply(object(json!({
            "type": "homeassistant/expose_entity",
            "assistants": ["conversation"],
            "entity_ids": entity_ids,
            "should_expose": true,
        })))
        .await
    }

    /// Fires `event_type` on HA's event bus with `data` (`fire_event`).
    pub async fn fire_event(&self, event_type: &str, data: Value) -> EffectOutcome<()> {
        self.apply(object(json!({
            "type": "fire_event",
            "event_type": event_type,
            "event_data": data,
        })))
        .await
    }
}
