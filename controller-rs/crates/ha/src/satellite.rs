//! Which pipeline a satellite's pipeline select chooses, by HA's own rule.
//!
//! HA (`assist_pipeline/select.py`, `get_chosen_pipeline`) finds the satellite's first
//! pipeline select by its unique id `<prefix>-pipeline`, reads its state, and treats a
//! missing select, a missing state, the option `preferred`, or a name no pipeline has as
//! "the preferred pipeline"; otherwise it is the pipeline with that name. HA's run command
//! does not consult the select itself, so the controller resolves it and names the pipeline
//! in each run.

use echomuse_core::{
    services::{HaDeviceId, HaError, NotDispatched, PipelineId},
    types::WorkScope,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{HaClient, object};

/// The select option that stands for HA's preferred pipeline (`OPTION_PREFERRED`).
const PREFERRED_OPTION: &str = "preferred";
/// The unique id suffix of a satellite's first pipeline select. Its further selects end
/// in `-pipeline_2` and so on.
const PIPELINE_SELECT_SUFFIX: &str = "-pipeline";

#[derive(Deserialize)]
struct RegistryEntry {
    entity_id: String,
    device_id: Option<String>,
    /// Integrations may register non-string unique ids.
    unique_id: Value,
}

#[derive(Deserialize)]
struct PipelineList {
    pipelines: Vec<PipelineSummary>,
    preferred_pipeline: Option<String>,
}

#[derive(Deserialize)]
struct PipelineSummary {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct EntityState {
    state: String,
}

fn bad(detail: String) -> HaError {
    HaError::BadResponse(detail)
}

/// The first pipeline select of `device` in an `config/entity_registry/list` result.
fn pipeline_select(registry: Value, device: &HaDeviceId) -> Result<Option<String>, HaError> {
    let entries: Vec<RegistryEntry> =
        serde_json::from_value(registry).map_err(|err| bad(format!("entity registry: {err}")))?;
    Ok(entries
        .into_iter()
        .find(|entry| {
            entry.entity_id.starts_with("select.")
                && entry.device_id.as_deref() == Some(device.0.as_str())
                && entry
                    .unique_id
                    .as_str()
                    .is_some_and(|id| id.ends_with(PIPELINE_SELECT_SUFFIX))
        })
        .map(|entry| entry.entity_id))
}

/// HA's rule for the pipeline a select option chooses.
fn chosen_pipeline(option: Option<&str>, list: PipelineList) -> Result<PipelineId, HaError> {
    let named = option
        .filter(|option| *option != PREFERRED_OPTION)
        .and_then(|option| {
            list.pipelines
                .iter()
                .find(|pipeline| pipeline.name == option)
        })
        .map(|pipeline| pipeline.id.clone());
    named
        .or(list.preferred_pipeline)
        .map(PipelineId)
        .ok_or_else(|| bad("HA named no preferred pipeline".to_owned()))
}

impl HaClient {
    pub(crate) async fn satellite_pipeline(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> Result<PipelineId, HaError> {
        let timeout = self.options().command_timeout;
        let option = match &ha_device {
            None => None,
            Some(device) => {
                let registry = self
                    .command(
                        Some(&scope),
                        object(json!({"type": "config/entity_registry/list"})),
                        timeout,
                    )
                    .await?;
                match pipeline_select(registry, device)? {
                    None => None,
                    Some(entity) => self.entity_state(&scope, &entity).await?,
                }
            }
        };
        let list = self
            .command(
                Some(&scope),
                object(json!({"type": "assist_pipeline/pipeline/list"})),
                timeout,
            )
            .await?;
        let list: PipelineList =
            serde_json::from_value(list).map_err(|err| bad(format!("pipeline list: {err}")))?;
        chosen_pipeline(option.as_deref(), list)
    }

    /// The entity's state through `GET /api/states/<entity_id>`; None when HA has none.
    async fn entity_state(
        &self,
        scope: &WorkScope,
        entity: &str,
    ) -> Result<Option<String>, HaError> {
        if scope.is_cancelled() {
            return Err(HaError::NotDispatched(NotDispatched::Cancelled));
        }
        let (status, body) = self.rest_get(&format!("/states/{entity}")).await?;
        match status {
            200 => serde_json::from_slice::<EntityState>(&body)
                .map(|state| Some(state.state))
                .map_err(|err| bad(format!("state of {entity}: {err}"))),
            404 => Ok(None),
            _ => Err(HaError::Rejected {
                code: format!("http_{status}"),
                message: String::from_utf8_lossy(&body).into_owned(),
            }),
        }
    }
}

/// The device of a satellite in HA's device registry: the one whose connections name
/// `mac` as its network MAC (ESPHome registers its nodes that way, lowercase with
/// colons). `None` when HA has no such device yet.
fn device_with_mac(registry: Value, mac: &str) -> Result<Option<HaDeviceId>, HaError> {
    #[derive(Deserialize)]
    struct DeviceEntry {
        id: String,
        #[serde(default)]
        connections: Vec<(String, String)>,
    }
    let entries: Vec<DeviceEntry> =
        serde_json::from_value(registry).map_err(|err| bad(format!("device registry: {err}")))?;
    let wanted = mac.to_ascii_lowercase();
    Ok(entries
        .into_iter()
        .find(|entry| {
            entry
                .connections
                .iter()
                .any(|(kind, value)| kind == "mac" && value.to_ascii_lowercase() == wanted)
        })
        .map(|entry| HaDeviceId(entry.id)))
}

impl HaClient {
    /// The satellite's device in HA's device registry, found by its MAC
    /// (`config/device_registry/list`). Each run and timer query names it (§3.3.3).
    pub async fn device_with_mac(&self, mac: &str) -> Result<Option<HaDeviceId>, HaError> {
        let registry = self
            .command(
                None,
                object(json!({"type": "config/device_registry/list"})),
                self.options().command_timeout,
            )
            .await?;
        device_with_mac(registry, mac)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_satellite_is_the_device_whose_connections_name_its_mac() {
        let registry = json!([
            {"id": "other", "connections": [["mac", "aa:bb:cc:dd:ee:00"]]},
            {"id": "kitchen", "connections": [["bluetooth", "00:11"], ["mac", "aa:bb:cc:dd:ee:ff"]]},
            {"id": "no-connections"},
        ]);
        let found = device_with_mac(registry.clone(), "AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(found, Some(HaDeviceId("kitchen".to_owned())));
        assert_eq!(
            device_with_mac(registry, "00:00:00:00:00:01").unwrap(),
            None
        );
    }

    use super::*;

    fn list() -> PipelineList {
        PipelineList {
            pipelines: vec![
                PipelineSummary {
                    id: "01home".to_owned(),
                    name: "Home Assistant".to_owned(),
                },
                PipelineSummary {
                    id: "01cloud".to_owned(),
                    name: "Cloud".to_owned(),
                },
            ],
            preferred_pipeline: Some("01home".to_owned()),
        }
    }

    #[test]
    fn a_named_option_chooses_that_pipeline_and_anything_else_the_preferred_one() {
        let id = |option| chosen_pipeline(option, list()).unwrap().0;
        assert_eq!(id(Some("Cloud")), "01cloud");
        assert_eq!(id(Some("preferred")), "01home");
        assert_eq!(id(Some("unavailable")), "01home");
        assert_eq!(id(None), "01home");
    }

    #[test]
    fn only_the_devices_first_pipeline_select_is_used() {
        let registry = json!([
            {"entity_id": "select.office_assistant_2", "device_id": "dev1",
             "unique_id": "AA:BB-pipeline_2", "platform": "esphome"},
            {"entity_id": "select.kitchen_assistant", "device_id": "dev2",
             "unique_id": "CC:DD-pipeline", "platform": "esphome"},
            {"entity_id": "sensor.office_numeric", "device_id": "dev1",
             "unique_id": 42, "platform": "demo"},
            {"entity_id": "select.office_assistant", "device_id": "dev1",
             "unique_id": "AA:BB-pipeline", "platform": "esphome"}
        ]);
        let device = HaDeviceId("dev1".to_owned());
        assert_eq!(
            pipeline_select(registry, &device).unwrap().as_deref(),
            Some("select.office_assistant")
        );
        let none = json!([{"entity_id": "select.x", "device_id": null, "unique_id": "Z-pipeline"}]);
        assert_eq!(pipeline_select(none, &device).unwrap(), None);
    }
}
