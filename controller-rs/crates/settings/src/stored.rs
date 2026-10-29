//! The settings documents as the database stores them, read tolerantly and written back
//! in the same shape.
//!
//! - `system_config` `global_device_config`: the fleet document, a JSON object of
//!   setting keys. A fresh installation has no row; then every fleet value is the
//!   default.
//! - `devices.config`: the device's own document, the same shape. Real rows carry a full
//!   copy of every key even while every section follows the fleet; only the keys of
//!   overridden sections, and `startupVolume`, take effect.
//! - `devices.config_sections`: a JSON list of the overridden section ids, `[]` when the
//!   device follows the fleet entirely.
//! - `devices.use_global_config`: 1 exactly when `config_sections` is empty, which is
//!   what it meant before per-section scoping; the list is the authority.
//!
//! Decision: a stored key this controller does not recognise, or one the configuration
//! removed, is kept and preserved, not dropped and not refused. The document is kept as
//! parsed, in its order; an edit replaces only the values it changes, so every other
//! key, recognised or not, is written back exactly as it was, and an unchanged document
//! is never rewritten. Such keys and invalid stored values are reported as
//! [`StoredIssue`]s. Refusing would stop the controller over a key that changes nothing;
//! dropping would destroy data the previous controller may still read after a rollback
//! (§3.5.1 rules 4, 5 and 9).

use std::collections::BTreeMap;
use std::fmt;

use crate::json::{Json, JsonObject};
use crate::settings::{
    DeviceSettings, REMOVED_KEYS, Scope, Section, SectionSet, SettingKey, SettingValue,
};
use crate::values::ValueError;

/// Something in a stored document that is not taken as a setting.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredIssue {
    /// The text is not JSON, or not a JSON object; no value of it is used, and no edit
    /// is written over it.
    Unreadable {
        /// Why.
        reason: String,
    },
    /// A key this controller does not know; kept as stored.
    Unrecognised {
        /// The key.
        key: String,
    },
    /// A key the configuration removed; kept as stored.
    Removed {
        /// The key.
        key: String,
    },
    /// A known key whose stored value is invalid; the value from the next layer down
    /// (the fleet's, or the default) is used instead, and the stored one is kept.
    Invalid {
        /// The key.
        key: SettingKey,
        /// What is wrong with the value.
        error: ValueError,
    },
    /// A `config_sections` entry that is not a section id; kept as stored.
    UnknownSection {
        /// The entry.
        section: String,
    },
    /// `use_global_config` disagrees with `config_sections`; the list is used.
    UseGlobalMismatch {
        /// The stored flag.
        use_global_config: bool,
    },
}

impl fmt::Display for StoredIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { reason } => write!(f, "unreadable document: {reason}"),
            Self::Unrecognised { key } => write!(f, "{key}: not a setting this controller knows"),
            Self::Removed { key } => write!(f, "{key}: a removed setting"),
            Self::Invalid { key, error } => write!(f, "{key}: {error}"),
            Self::UnknownSection { section } => {
                write!(f, "config_sections: unknown section {section:?}")
            }
            Self::UseGlobalMismatch { use_global_config } => write!(
                f,
                "use_global_config is {} but config_sections says otherwise",
                u8::from(*use_global_config)
            ),
        }
    }
}

/// A stored settings document: its exact text, the parsed object in its order, its
/// valid values by key, and what was not taken.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredConfig {
    text: Option<String>,
    document: Option<JsonObject>,
    values: BTreeMap<SettingKey, SettingValue>,
    issues: Vec<StoredIssue>,
}

impl StoredConfig {
    /// A document from its stored text; `None` when there is none (no row, or NULL).
    pub fn parse(text: Option<&str>) -> Self {
        let Some(text) = text else {
            return Self {
                text: None,
                document: Some(JsonObject::new()),
                values: BTreeMap::new(),
                issues: Vec::new(),
            };
        };
        let mut config = Self {
            text: Some(text.to_owned()),
            document: None,
            values: BTreeMap::new(),
            issues: Vec::new(),
        };
        match Json::parse(text) {
            Ok(Json::Object(object)) => {
                for (key, value) in object.iter() {
                    match SettingKey::from_wire(key) {
                        Some(setting) => match SettingValue::parse(setting, value) {
                            Ok(v) => {
                                config.values.insert(setting, v);
                            }
                            Err(error) => config.issues.push(StoredIssue::Invalid {
                                key: setting,
                                error,
                            }),
                        },
                        None if REMOVED_KEYS.contains(&key) => {
                            config.issues.push(StoredIssue::Removed {
                                key: key.to_owned(),
                            })
                        }
                        None => config.issues.push(StoredIssue::Unrecognised {
                            key: key.to_owned(),
                        }),
                    }
                }
                config.document = Some(object);
            }
            Ok(_) => config.issues.push(StoredIssue::Unreadable {
                reason: "not a JSON object".to_owned(),
            }),
            Err(e) => config.issues.push(StoredIssue::Unreadable {
                reason: e.to_string(),
            }),
        }
        config
    }

    /// The stored text; `None` when nothing is stored.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// Whether the document could be read.
    pub fn is_readable(&self) -> bool {
        self.document.is_some()
    }

    /// The valid stored value of `key`.
    pub fn value(&self, key: SettingKey) -> Option<&SettingValue> {
        self.values.get(&key)
    }

    /// Everything not taken as a setting.
    pub fn issues(&self) -> &[StoredIssue] {
        &self.issues
    }

    /// The document written back in its stored shape: Python's `json.dumps` text, keys
    /// in their order. Unchanged from [`text`](Self::text) for a document read from the
    /// previous controller's writes.
    pub fn render(&self) -> Option<String> {
        self.document
            .as_ref()
            .map(|d| Json::Object(d.clone()).to_python_text())
    }

    /// The text after setting `changes`, or `None` when no stored value would change. A
    /// key already stored is replaced in place; a new key goes at the end. A change equal
    /// to the stored valid value leaves that key's text untouched.
    pub fn with_changes<'a>(
        &self,
        changes: impl IntoIterator<Item = &'a SettingValue>,
    ) -> Result<Option<String>, Unwritable> {
        let Some(document) = &self.document else {
            return Err(Unwritable);
        };
        let mut document = document.clone();
        let mut changed = false;
        for value in changes {
            let key = value.key();
            if self.values.get(&key) == Some(value) {
                continue;
            }
            document.insert(key.as_str(), value.to_json());
            changed = true;
        }
        Ok(changed.then(|| Json::Object(document).to_python_text()))
    }
}

/// The stored document is unreadable, so no edit is written over it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unwritable;

/// `devices.config_sections` with `use_global_config`.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredSections {
    known: SectionSet,
    unknown: Vec<String>,
    issues: Vec<StoredIssue>,
}

impl StoredSections {
    /// The overridden sections from the stored list and flag. An unreadable list is
    /// reported and taken as empty.
    pub fn parse(text: &str, use_global_config: bool) -> Self {
        let mut sections = Self {
            known: SectionSet::new(),
            unknown: Vec::new(),
            issues: Vec::new(),
        };
        match Json::parse(text) {
            Ok(Json::Array(items)) => {
                for item in items {
                    match item {
                        Json::String(id) => match Section::from_wire(&id) {
                            Some(section) => {
                                sections.known.insert(section);
                            }
                            None => {
                                sections.issues.push(StoredIssue::UnknownSection {
                                    section: id.clone(),
                                });
                                sections.unknown.push(id);
                            }
                        },
                        other => sections.issues.push(StoredIssue::UnknownSection {
                            section: other.to_python_text(),
                        }),
                    }
                }
            }
            Ok(_) => sections.issues.push(StoredIssue::Unreadable {
                reason: "config_sections is not a JSON list".to_owned(),
            }),
            Err(e) => sections.issues.push(StoredIssue::Unreadable {
                reason: format!("config_sections: {e}"),
            }),
        }
        if use_global_config != sections.known.is_empty() {
            sections
                .issues
                .push(StoredIssue::UseGlobalMismatch { use_global_config });
        }
        sections
    }

    /// The overridden sections.
    pub fn overridden(&self) -> &SectionSet {
        &self.known
    }

    /// What was not taken.
    pub fn issues(&self) -> &[StoredIssue] {
        &self.issues
    }

    /// The stored list for `sections`, in the dashboard's section order, followed by any
    /// stored entries this controller does not know, kept as they were.
    pub fn render(&self, sections: &SectionSet) -> String {
        let items = Section::ALL
            .iter()
            .filter(|s| sections.contains(s))
            .map(|s| Json::String(s.as_str().to_owned()))
            .chain(self.unknown.iter().map(|u| Json::String(u.clone())))
            .collect();
        Json::Array(items).to_python_text()
    }
}

/// The fleet's settings: the defaults with the fleet document's valid values over them.
pub fn fleet_settings(fleet: &StoredConfig) -> DeviceSettings {
    let mut settings = DeviceSettings::default();
    for value in fleet.values.values() {
        settings.set(value.clone());
    }
    settings
}

/// A device's effective settings: the fleet's, with the device's valid values for the
/// sections it overrides and for its state keys layered over them.
pub fn effective_settings(
    fleet: &DeviceSettings,
    device: &StoredConfig,
    overridden: &SectionSet,
) -> DeviceSettings {
    let mut settings = fleet.clone();
    for (key, value) in &device.values {
        let applies = match key.scope() {
            Scope::Section(section) => overridden.contains(&section),
            Scope::State => true,
        };
        if applies {
            settings.set(value.clone());
        }
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::{DuckDb, NearMissFloor, StartupVolume};

    #[test]
    fn issues_are_reported_and_values_kept() {
        let text =
            r#"{"duckDb": -35, "replyOnset": 3, "futureKey": [1], "nearMissThreshold": 0.9}"#;
        let config = StoredConfig::parse(Some(text));
        assert_eq!(
            config.value(SettingKey::DuckDb),
            Some(&SettingValue::DuckDb(DuckDb::new(-35.0).unwrap()))
        );
        assert_eq!(config.value(SettingKey::NearMissThreshold), None);
        assert_eq!(config.issues().len(), 3);
        assert!(matches!(&config.issues()[0], StoredIssue::Removed { key } if key == "replyOnset"));
        assert!(
            matches!(&config.issues()[1], StoredIssue::Unrecognised { key } if key == "futureKey")
        );
        assert!(matches!(
            &config.issues()[2],
            StoredIssue::Invalid {
                key: SettingKey::NearMissThreshold,
                ..
            }
        ));
        assert_eq!(config.render().as_deref(), Some(text));
        // An edit keeps every other key, recognised or not, as stored.
        let floor = SettingValue::NearMissThreshold(NearMissFloor::new(0.2).unwrap());
        assert_eq!(
            config.with_changes([&floor]).unwrap().as_deref(),
            Some(r#"{"duckDb": -35, "replyOnset": 3, "futureKey": [1], "nearMissThreshold": 0.2}"#)
        );
        // Setting a stored value to itself changes nothing.
        let same = SettingValue::DuckDb(DuckDb::new(-35.0).unwrap());
        assert_eq!(config.with_changes([&same]).unwrap(), None);
    }

    #[test]
    fn unreadable_documents_are_never_written_over() {
        let config = StoredConfig::parse(Some("{not json"));
        assert!(!config.is_readable());
        let volume = SettingValue::StartupVolume(StartupVolume::new(10).unwrap());
        assert_eq!(config.with_changes([&volume]), Err(Unwritable));
        assert_eq!(config.text(), Some("{not json"));
    }

    #[test]
    fn sections_parse_and_render() {
        let sections = StoredSections::parse(r#"["timers", "ring", "lighting"]"#, false);
        assert_eq!(sections.overridden().len(), 2);
        assert_eq!(sections.issues().len(), 1);
        let mut set = sections.overridden().clone();
        set.insert(Section::Playback);
        assert_eq!(
            sections.render(&set),
            r#"["playback", "ring", "timers", "lighting"]"#
        );
        let mismatch = StoredSections::parse("[]", false);
        assert_eq!(
            mismatch.issues(),
            [StoredIssue::UseGlobalMismatch {
                use_global_config: false
            }]
        );
    }
}
