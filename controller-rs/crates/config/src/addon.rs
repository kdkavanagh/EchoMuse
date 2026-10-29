//! Home Assistant add-on options.
//!
//! The Supervisor writes the add-on's options to `/data/options.json`. Each
//! known option maps to one environment variable (spec §3.6); a variable that
//! is already set wins over its option. Options the controller does not know
//! are reported back and never reach the environment under any name.

use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

use crate::settings::{EnvVar, Environment};

/// Where the Supervisor writes the add-on's options.
pub const OPTIONS_PATH: &str = "/data/options.json";

/// One option from the add-on manifest (`controller/config.yaml`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddonOption {
    /// `server_host` → `SERVER_HOST`.
    ServerHost,
    /// `server_ip` → `SERVER_IP`.
    ServerIp,
    /// `mdns_name` → `MDNS_NAME`.
    MdnsName,
    /// `esphome_project_version` → `ESPHOME_PROJECT_VERSION`.
    EsphomeProjectVersion,
    /// `require_device_tls` → `REQUIRE_DEVICE_TLS`.
    RequireDeviceTls,
    /// `device_approval` → `DEVICE_APPROVAL`.
    DeviceApproval,
}

impl AddonOption {
    /// Every option the manifest declares.
    pub const ALL: [Self; 6] = [
        Self::ServerHost,
        Self::ServerIp,
        Self::MdnsName,
        Self::EsphomeProjectVersion,
        Self::RequireDeviceTls,
        Self::DeviceApproval,
    ];

    /// The option's key in `options.json`.
    pub const fn key(self) -> &'static str {
        match self {
            Self::ServerHost => "server_host",
            Self::ServerIp => "server_ip",
            Self::MdnsName => "mdns_name",
            Self::EsphomeProjectVersion => "esphome_project_version",
            Self::RequireDeviceTls => "require_device_tls",
            Self::DeviceApproval => "device_approval",
        }
    }

    /// The environment variable the option sets.
    pub const fn variable(self) -> EnvVar {
        match self {
            Self::ServerHost => EnvVar::ServerHost,
            Self::ServerIp => EnvVar::ServerIp,
            Self::MdnsName => EnvVar::MdnsName,
            Self::EsphomeProjectVersion => EnvVar::EsphomeProjectVersion,
            Self::RequireDeviceTls => EnvVar::RequireDeviceTls,
            Self::DeviceApproval => EnvVar::DeviceApproval,
        }
    }

    /// The option with this key, if the manifest declares one.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|option| option.key() == key)
    }
}

/// What merging the options did.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AddonMerge {
    /// Options written into the environment.
    pub applied: Vec<AddonOption>,
    /// Options left out because their variable was already set.
    pub overridden_by_environment: Vec<AddonOption>,
    /// Keys the manifest does not declare; not applied.
    pub unknown_keys: Vec<String>,
}

/// Why the options could not be merged.
#[derive(Debug, Error)]
pub enum AddonOptionsError {
    /// The options file exists but could not be read.
    #[error("cannot read {path}: {source}")]
    Read {
        /// The options file.
        path: PathBuf,
        /// The read failure.
        source: io::Error,
    },
    /// The file is not a JSON object.
    #[error("add-on options are not a JSON object: {0}")]
    NotAnObject(String),
    /// An option has a value no environment variable can carry.
    #[error("add-on option {key} has an unsupported value {value}")]
    UnsupportedValue {
        /// The option's key.
        key: &'static str,
        /// The value, as JSON.
        value: String,
    },
}

/// The environment text for one option value; `None` for an empty string,
/// which leaves the option unset.
fn env_text(option: AddonOption, value: &Value) -> Result<Option<String>, AddonOptionsError> {
    match value {
        Value::String(text) if text.is_empty() => Ok(None),
        Value::String(text) => Ok(Some(text.clone())),
        // Booleans take the spelling of the flag variables they set.
        Value::Bool(true) => Ok(Some("1".to_owned())),
        Value::Bool(false) => Ok(Some("0".to_owned())),
        Value::Number(number) => Ok(Some(number.to_string())),
        Value::Null | Value::Array(_) | Value::Object(_) => {
            Err(AddonOptionsError::UnsupportedValue {
                key: option.key(),
                value: value.to_string(),
            })
        }
    }
}

/// Merges the options in `json` into `env`.
///
/// A variable counts as already set when it holds a non-empty value, the same
/// rule [`EnvVar::read`] uses. Nothing is written when any option is invalid.
pub fn merge_options(env: &mut Environment, json: &str) -> Result<AddonMerge, AddonOptionsError> {
    let parsed: Value = serde_json::from_str(json)
        .map_err(|error| AddonOptionsError::NotAnObject(error.to_string()))?;
    let Value::Object(options) = parsed else {
        return Err(AddonOptionsError::NotAnObject(parsed.to_string()));
    };

    let mut merge = AddonMerge::default();
    let mut writes = Vec::new();
    for (key, value) in &options {
        let Some(option) = AddonOption::from_key(key) else {
            merge.unknown_keys.push(key.clone());
            continue;
        };
        let Some(text) = env_text(option, value)? else {
            continue;
        };
        if option.variable().read(env).is_some() {
            merge.overridden_by_environment.push(option);
        } else {
            writes.push((option, text));
        }
    }
    for (option, text) in writes {
        env.insert(option.variable().name().to_owned(), text);
        merge.applied.push(option);
    }
    Ok(merge)
}

/// Merges the options file at `path` into `env` when it exists. Outside the
/// add-on there is no file and the environment is returned unchanged with
/// `None`.
pub fn merge_options_file(
    env: &mut Environment,
    path: &Path,
) -> Result<Option<AddonMerge>, AddonOptionsError> {
    if !path.is_file() {
        return Ok(None);
    }
    let json = std::fs::read_to_string(path).map_err(|source| AddonOptionsError::Read {
        path: path.to_owned(),
        source,
    })?;
    merge_options(env, &json).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults block of `controller/config.yaml`, as the Supervisor
    /// writes it.
    const MANIFEST_DEFAULTS: &str = r#"{
        "server_host": "0.0.0.0",
        "server_ip": "",
        "mdns_name": "echomuse",
        "esphome_project_version": "2.0.0",
        "require_device_tls": false,
        "device_approval": "strict"
    }"#;

    #[test]
    fn manifest_defaults_map_to_their_variables() {
        let mut env = Environment::new();
        let merge = merge_options(&mut env, MANIFEST_DEFAULTS).unwrap();
        let expected: Environment = [
            ("SERVER_HOST", "0.0.0.0"),
            ("MDNS_NAME", "echomuse"),
            ("ESPHOME_PROJECT_VERSION", "2.0.0"),
            ("REQUIRE_DEVICE_TLS", "0"),
            ("DEVICE_APPROVAL", "strict"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        assert_eq!(env, expected);
        assert_eq!(merge.applied.len(), 5);
        assert!(merge.unknown_keys.is_empty());
    }

    #[test]
    fn a_set_variable_wins_and_an_empty_one_does_not() {
        let mut env: Environment = [
            ("SERVER_IP".to_owned(), "10.0.0.9".to_owned()),
            ("MDNS_NAME".to_owned(), "  ".to_owned()),
        ]
        .into_iter()
        .collect();
        let merge = merge_options(
            &mut env,
            r#"{"server_ip": "10.0.0.1", "mdns_name": "den", "require_device_tls": true}"#,
        )
        .unwrap();
        assert_eq!(env["SERVER_IP"], "10.0.0.9");
        assert_eq!(env["MDNS_NAME"], "den");
        assert_eq!(env["REQUIRE_DEVICE_TLS"], "1");
        assert_eq!(merge.overridden_by_environment, [AddonOption::ServerIp]);
    }

    #[test]
    fn unknown_options_are_reported_and_not_applied() {
        let mut env = Environment::new();
        let merge = merge_options(&mut env, r#"{"ha_token": "x", "DB_PATH": "/tmp/x"}"#).unwrap();
        assert!(env.is_empty());
        assert_eq!(merge.unknown_keys, ["DB_PATH", "ha_token"]);
    }

    #[test]
    fn invalid_options_change_nothing() {
        let mut env = Environment::new();
        let error =
            merge_options(&mut env, r#"{"server_host": "::", "server_ip": null}"#).unwrap_err();
        assert!(matches!(
            error,
            AddonOptionsError::UnsupportedValue {
                key: "server_ip",
                ..
            }
        ));
        assert!(env.is_empty());
        assert!(matches!(
            merge_options(&mut env, "[1]"),
            Err(AddonOptionsError::NotAnObject(_))
        ));
        assert!(matches!(
            merge_options(&mut env, "{"),
            Err(AddonOptionsError::NotAnObject(_))
        ));
    }

    #[test]
    fn a_missing_file_leaves_the_environment_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = Environment::new();
        assert!(
            merge_options_file(&mut env, &dir.path().join("options.json"))
                .unwrap()
                .is_none()
        );
        assert!(env.is_empty());

        let path = dir.path().join("options.json");
        std::fs::write(&path, r#"{"device_approval": "auto"}"#).unwrap();
        let merge = merge_options_file(&mut env, &path).unwrap().unwrap();
        assert_eq!(merge.applied, [AddonOption::DeviceApproval]);
        assert_eq!(env["DEVICE_APPROVAL"], "auto");
    }

    #[test]
    fn merged_options_produce_a_valid_configuration() {
        let mut env: Environment = [("SUPERVISOR_TOKEN".to_owned(), "t".to_owned())]
            .into_iter()
            .collect();
        merge_options(&mut env, MANIFEST_DEFAULTS).unwrap();
        let config = crate::ControllerConfig::from_env(&env, None).unwrap();
        assert!(!config.network.require_device_tls);
        assert_eq!(config.esphome_project_version, "2.0.0");
    }
}
