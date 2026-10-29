//! The names Home Assistant's Assist matches against, for transcript correction (§4.7):
//! the names and aliases of entities exposed to Assist, and the names and aliases of areas
//! and floors; and the language a pipeline's conversation agent runs in.
//!
//! HA's default conversation agent (`conversation/default_agent.py`) matches an entity by
//! its friendly name and its registry aliases, and only entities exposed to the
//! `conversation` assistant; areas and floors by name and aliases. The commands used are
//! HA's documented WebSocket API:
//!
//! - `homeassistant/expose_entity/list` → `{"exposed_entities": {entity_id: {assistant:
//!   bool}}}`; an entity counts when `conversation` is true.
//! - `get_states` → every state; an exposed entity's name is its `friendly_name`
//!   attribute.
//! - `config/entity_registry/get_entries` `{entity_ids}` → `{entity_id: entry | null}`,
//!   whose `aliases` are the entity's aliases.
//! - `config/area_registry/list` and `config/floor_registry/list` → entries with `name`
//!   and `aliases`.
//! - `assist_pipeline/pipeline/get` `{pipeline_id}` → the pipeline, whose
//!   `conversation_language` (or, when that is `*` or absent, `language`) is the language
//!   its sentences are matched in.

use std::collections::{BTreeMap, BTreeSet};

use echomuse_core::{
    services::{HaError, PipelineId},
    types::WorkScope,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{HaClient, object};

/// HA's events that change what Assist matches by name. A rebuild of the correction
/// tables follows each.
pub const VOCABULARY_EVENTS: [&str; 4] = [
    "entity_registry_updated",
    "area_registry_updated",
    "floor_registry_updated",
    "device_registry_updated",
];

/// What Assist matches by name, deduplicated and sorted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssistVocabulary {
    /// Exposed entities' friendly names and aliases.
    pub names: Vec<String>,
    /// Area names and aliases.
    pub areas: Vec<String>,
    /// Floor names and aliases.
    pub floors: Vec<String>,
}

fn bad(what: &str, err: impl std::fmt::Display) -> HaError {
    HaError::BadResponse(format!("{what}: {err}"))
}

#[derive(Deserialize)]
struct ExposedList {
    exposed_entities: BTreeMap<String, BTreeMap<String, Value>>,
}

/// The entities exposed to Assist (`conversation: true`), sorted.
fn exposed(list: Value) -> Result<Vec<String>, HaError> {
    let list: ExposedList = serde_json::from_value(list).map_err(|e| bad("exposed entities", e))?;
    Ok(list
        .exposed_entities
        .into_iter()
        .filter(|(_, assistants)| assistants.get("conversation") == Some(&Value::Bool(true)))
        .map(|(entity, _)| entity)
        .collect())
}

/// The friendly names of `entities` among `get_states`'s states.
fn friendly_names(states: Value, entities: &BTreeSet<&str>) -> Result<Vec<String>, HaError> {
    #[derive(Deserialize)]
    struct State {
        entity_id: String,
        #[serde(default)]
        attributes: BTreeMap<String, Value>,
    }
    let states: Vec<State> = serde_json::from_value(states).map_err(|e| bad("states", e))?;
    Ok(states
        .into_iter()
        .filter(|s| entities.contains(s.entity_id.as_str()))
        .filter_map(|s| match s.attributes.get("friendly_name") {
            Some(Value::String(name)) => Some(name.clone()),
            _ => None,
        })
        .collect())
}

#[derive(Deserialize)]
struct Named {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
}

/// The aliases in a `config/entity_registry/get_entries` answer (entities HA has no entry
/// for are `null`).
fn entity_aliases(entries: Value) -> Result<Vec<String>, HaError> {
    let entries: BTreeMap<String, Option<Named>> =
        serde_json::from_value(entries).map_err(|e| bad("entity entries", e))?;
    Ok(entries
        .into_values()
        .flatten()
        .flat_map(|entry| entry.aliases)
        .collect())
}

/// The names and aliases of an area or floor registry listing.
fn registry_names(list: Value, what: &str) -> Result<Vec<String>, HaError> {
    let entries: Vec<Named> = serde_json::from_value(list).map_err(|e| bad(what, e))?;
    Ok(entries
        .into_iter()
        .flat_map(|entry| entry.name.into_iter().chain(entry.aliases))
        .collect())
}

/// Trimmed, non-empty, deduplicated and sorted.
fn tidy(names: impl IntoIterator<Item = String>) -> Vec<String> {
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The language a pipeline's sentences are matched in; `None` when HA does not say.
fn pipeline_language(pipeline: Value) -> Result<Option<String>, HaError> {
    #[derive(Deserialize)]
    struct Pipeline {
        #[serde(default)]
        conversation_language: Option<String>,
        #[serde(default)]
        language: Option<String>,
    }
    let pipeline: Pipeline = serde_json::from_value(pipeline).map_err(|e| bad("pipeline", e))?;
    let usable = |l: &Option<String>| l.clone().filter(|l| !l.is_empty() && l != "*");
    Ok(usable(&pipeline.conversation_language).or_else(|| usable(&pipeline.language)))
}

impl HaClient {
    /// What Assist matches by name, read on the current connection.
    pub async fn assist_vocabulary(&self) -> Result<AssistVocabulary, HaError> {
        let timeout = self.options().command_timeout;
        let ask = |kind: &str| object(json!({ "type": kind }));
        let entities = exposed(
            self.command(None, ask("homeassistant/expose_entity/list"), timeout)
                .await?,
        )?;
        let wanted: BTreeSet<&str> = entities.iter().map(String::as_str).collect();
        let mut names = friendly_names(
            self.command(None, ask("get_states"), timeout).await?,
            &wanted,
        )?;
        if !entities.is_empty() {
            let entries = self
                .command(
                    None,
                    object(json!({
                        "type": "config/entity_registry/get_entries",
                        "entity_ids": entities,
                    })),
                    timeout,
                )
                .await?;
            names.extend(entity_aliases(entries)?);
        }
        let areas = registry_names(
            self.command(None, ask("config/area_registry/list"), timeout)
                .await?,
            "area registry",
        )?;
        let floors = registry_names(
            self.command(None, ask("config/floor_registry/list"), timeout)
                .await?,
            "floor registry",
        )?;
        Ok(AssistVocabulary {
            names: tidy(names),
            areas: tidy(areas),
            floors: tidy(floors),
        })
    }

    /// The language `pipeline`'s conversation agent matches sentences in; `None` when HA
    /// does not say.
    pub async fn pipeline_language(
        &self,
        scope: &WorkScope,
        pipeline: &PipelineId,
    ) -> Result<Option<String>, HaError> {
        let pipeline = self
            .command(
                Some(scope),
                object(json!({
                    "type": "assist_pipeline/pipeline/get",
                    "pipeline_id": pipeline.0,
                })),
                self.options().command_timeout,
            )
            .await?;
        pipeline_language(pipeline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_entities_exposed_to_assist_count() {
        let list = json!({"exposed_entities": {
            "light.kitchen": {"conversation": true, "cloud.alexa": false},
            "switch.secret": {"conversation": false},
            "fan.bedroom": {"cloud.google_assistant": true},
        }});
        assert_eq!(exposed(list).unwrap(), vec!["light.kitchen".to_owned()]);
        let states = json!([
            {"entity_id": "light.kitchen", "state": "on",
             "attributes": {"friendly_name": "Kitchen Light"}},
            {"entity_id": "switch.secret", "state": "off",
             "attributes": {"friendly_name": "Secret Switch"}},
        ]);
        let wanted = BTreeSet::from(["light.kitchen"]);
        assert_eq!(
            friendly_names(states, &wanted).unwrap(),
            vec!["Kitchen Light".to_owned()]
        );
    }

    #[test]
    fn aliases_and_registry_names_are_tidied() {
        let entries = json!({
            "light.kitchen": {"entity_id": "light.kitchen", "aliases": ["cooker light", " "]},
            "light.gone": null,
        });
        let areas = json!([
            {"area_id": "kitchen", "name": "Kitchen", "aliases": ["galley"]},
            {"area_id": "hall", "name": "Hall", "aliases": []},
        ]);
        let mut names = entity_aliases(entries).unwrap();
        names.push("Kitchen Light".to_owned());
        assert_eq!(tidy(names), vec!["Kitchen Light", "cooker light"]);
        assert_eq!(
            tidy(registry_names(areas, "areas").unwrap()),
            vec!["Hall", "Kitchen", "galley"]
        );
    }

    #[test]
    fn the_conversation_language_wins_unless_it_is_any() {
        let both = json!({"language": "en", "conversation_language": "en-GB"});
        assert_eq!(pipeline_language(both).unwrap().as_deref(), Some("en-GB"));
        let any = json!({"language": "en", "conversation_language": "*"});
        assert_eq!(pipeline_language(any).unwrap().as_deref(), Some("en"));
        assert_eq!(pipeline_language(json!({})).unwrap(), None);
    }
}
