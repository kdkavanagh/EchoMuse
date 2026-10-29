//! Edits from the dashboard: a fleet edit is a set of setting values; a device edit is
//! the sections it overrides plus values. Unknown and removed keys and invalid values
//! are refused with an error naming the key (§4.10).

use std::collections::BTreeMap;
use std::fmt;

use echomuse_core::types::DeviceId;
use echomuse_protocol::Sha256;

use crate::json::Json;
use crate::settings::{REMOVED_KEYS, Section, SectionSet, SettingKey, SettingValue};
use crate::values::{ValueError, WakeRule};

/// The key a device edit carries its overridden sections under, beside the settings.
pub const SECTIONS_FIELD: &str = "config_sections";

/// Validated values to store, at most one per key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsPatch {
    values: BTreeMap<SettingKey, SettingValue>,
}

impl SettingsPatch {
    /// An empty patch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one value, replacing an earlier one for the same key.
    pub fn set(&mut self, value: SettingValue) -> &mut Self {
        self.values.insert(value.key(), value);
        self
    }

    /// The value for `key`, when the patch sets it.
    pub fn get(&self, key: SettingKey) -> Option<&SettingValue> {
        self.values.get(&key)
    }

    /// The values, in [`SettingKey`] order.
    pub fn values(&self) -> impl Iterator<Item = &SettingValue> + Clone {
        self.values.values()
    }

    /// Whether it sets nothing.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Parse a JSON object of setting keys, as the dashboard sends the fleet config.
    pub fn from_json_text(text: &str) -> Result<Self, EditError> {
        Self::from_json(&Json::parse(text).map_err(|e| EditError::NotAnObject(e.to_string()))?)
    }

    /// The same from a `serde_json` value.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, EditError> {
        Self::from_json(&Json::from_value(value))
    }

    fn from_json(value: &Json) -> Result<Self, EditError> {
        let Json::Object(object) = value else {
            return Err(EditError::NotAnObject("not a JSON object".to_owned()));
        };
        let mut patch = Self::new();
        for (key, value) in object.iter() {
            patch.set(parse_entry(key, value)?);
        }
        Ok(patch)
    }
}

fn parse_entry(key: &str, value: &Json) -> Result<SettingValue, EditError> {
    let setting = match SettingKey::from_wire(key) {
        Some(setting) => setting,
        None if REMOVED_KEYS.contains(&key) => return Err(EditError::RemovedKey(key.to_owned())),
        None => return Err(EditError::UnknownKey(key.to_owned())),
    };
    SettingValue::parse(setting, value).map_err(|error| EditError::InvalidValue {
        key: setting,
        error,
    })
}

/// A device edit: the sections it overrides from now on, and values.
///
/// A value whose section stays on the fleet is not stored and is reported back in
/// [`EditOutcome::not_stored`]: the dashboard sends the whole effective config with
/// every save. Sections newly overridden start from the fleet's current values unless
/// the edit sets them. `startupVolume` is device state and always stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceEdit {
    /// The overridden sections after the edit; `None` keeps them.
    pub sections: Option<SectionSet>,
    /// The values.
    pub values: SettingsPatch,
}

impl DeviceEdit {
    /// Parse the dashboard's device config body: setting keys plus `config_sections`,
    /// a list of section ids.
    pub fn from_json_text(text: &str) -> Result<Self, EditError> {
        Self::from_json(&Json::parse(text).map_err(|e| EditError::NotAnObject(e.to_string()))?)
    }

    /// The same from a `serde_json` value.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, EditError> {
        Self::from_json(&Json::from_value(value))
    }

    fn from_json(value: &Json) -> Result<Self, EditError> {
        let Json::Object(object) = value else {
            return Err(EditError::NotAnObject("not a JSON object".to_owned()));
        };
        let mut edit = Self::default();
        for (key, value) in object.iter() {
            if key == SECTIONS_FIELD {
                edit.sections = Some(parse_sections(value)?);
            } else {
                edit.values.set(parse_entry(key, value)?);
            }
        }
        Ok(edit)
    }
}

fn parse_sections(value: &Json) -> Result<SectionSet, EditError> {
    let Json::Array(items) = value else {
        return Err(EditError::InvalidSections(
            "must be a list of section ids".to_owned(),
        ));
    };
    items
        .iter()
        .map(|item| match item {
            Json::String(id) => {
                Section::from_wire(id).ok_or_else(|| EditError::UnknownSection(id.clone()))
            }
            other => Err(EditError::InvalidSections(format!(
                "{} is not a section id",
                other.to_python_text()
            ))),
        })
        .collect()
}

/// Why an edit was refused. Nothing was written.
#[derive(Debug)]
pub enum EditError {
    /// The body is not a JSON object.
    NotAnObject(String),
    /// A key that is not a setting.
    UnknownKey(String),
    /// A setting the configuration removed.
    RemovedKey(String),
    /// A value the setting does not take.
    InvalidValue {
        /// The setting.
        key: SettingKey,
        /// What is wrong.
        error: ValueError,
    },
    /// A `config_sections` entry that is not a section id.
    UnknownSection(String),
    /// `config_sections` is not a list of ids.
    InvalidSections(String),
    /// The edit selects a wake model the registry does not have, or one whose files are
    /// unusable.
    UnknownWakeModel(Sha256),
    /// An extra open rule repeats one of the selected model's two baseline rules.
    OpenRuleIsBaseline {
        /// The rule.
        rule: WakeRule,
    },
    /// The device has no row.
    UnknownDevice(DeviceId),
    /// The stored document cannot be read, so the edit is not written over it.
    StoredUnreadable,
    /// The database did not store the edit.
    Database(String),
    /// The settings service has stopped.
    Closed,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnObject(reason) => write!(f, "the config must be a JSON object: {reason}"),
            Self::UnknownKey(key) => write!(f, "{key}: not a setting"),
            Self::RemovedKey(key) => write!(f, "{key}: this setting was removed"),
            Self::InvalidValue { key, error } => write!(f, "{key}: {error}"),
            Self::UnknownSection(id) => write!(f, "{SECTIONS_FIELD}: unknown section {id:?}"),
            Self::InvalidSections(reason) => write!(f, "{SECTIONS_FIELD}: {reason}"),
            Self::UnknownWakeModel(sha) => {
                write!(f, "wakeModel: {sha} is not a usable registry model")
            }
            Self::OpenRuleIsBaseline { rule } => write!(
                f,
                "wakeOpenRules: {} {}-window {} >= {:.2} is one of the model's own rules",
                rule.profile,
                rule.windows.get(),
                rule.combine,
                rule.threshold.get()
            ),
            Self::UnknownDevice(device) => write!(f, "no device {device}"),
            Self::StoredUnreadable => {
                f.write_str("the stored config is unreadable; it is left as it is")
            }
            Self::Database(reason) => write!(f, "the config was not stored: {reason}"),
            Self::Closed => f.write_str("the settings service has stopped"),
        }
    }
}

impl std::error::Error for EditError {}

/// What an applied edit changed. Its snapshot is the one published after the commit.
#[derive(Clone, Debug)]
pub struct EditOutcome {
    /// The snapshot published with the edit.
    pub snapshot: std::sync::Arc<crate::service::SettingsSnapshot>,
    /// Devices whose effective settings changed, sorted by id.
    pub changed_devices: Vec<DeviceId>,
    /// Of those, the ones whose wake graph, rules or near-miss floor changed: they take
    /// effect only through a new `session.ready` (spec §3.1.4).
    pub new_session_needed: Vec<DeviceId>,
    /// Values the edit carried that were not stored because their section follows the
    /// fleet on this device.
    pub not_stored: Vec<SettingKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_removed_and_invalid_keys_are_refused_by_name() {
        assert!(matches!(
            SettingsPatch::from_json_text(r#"{"wakeSound": true, "colour": 1}"#),
            Err(EditError::UnknownKey(k)) if k == "colour"
        ));
        assert!(matches!(
            SettingsPatch::from_json_text(r#"{"replyOnset": 3}"#),
            Err(EditError::RemovedKey(k)) if k == "replyOnset"
        ));
        let err = SettingsPatch::from_json_text(r#"{"duckDb": -41}"#).unwrap_err();
        assert!(matches!(
            err,
            EditError::InvalidValue {
                key: SettingKey::DuckDb,
                ..
            }
        ));
        assert_eq!(err.to_string(), "duckDb: -41 is outside -40 to 0");
        assert!(matches!(
            DeviceEdit::from_json_text(r#"{"config_sections": ["ring", "lights"]}"#),
            Err(EditError::UnknownSection(s)) if s == "lights"
        ));
        let edit = DeviceEdit::from_json_text(
            r#"{"config_sections": ["timers"], "timerRingSeconds": 600}"#,
        )
        .unwrap();
        assert_eq!(edit.sections, Some(SectionSet::from([Section::Timers])));
        assert!(edit.values.get(SettingKey::TimerRingSeconds).is_some());
    }
}
