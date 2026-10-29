//! The parts of `session.ready` that come from settings (spec §3.1.4): the speech assets
//! and the detector policy, built with the protocol crate's own validating constructors.
//!
//! - `assets`: the runtime library's SHA-256, and the graph and sidecar of the device's
//!   selected registry model.
//! - `detector.thresholds`: the model's idle and playback thresholds, and the device's
//!   effective `nearMissThreshold` as `near_miss`.
//! - `detector.provisional_duck.duck_db`: the device's effective `duckDb`.
//! - `detector.open_rules`: only when `wakeOpenRules` has rules: the model's two baseline
//!   rules (the three-window mean at the idle and at the playback threshold), then those
//!   rules. Left off, the device derives exactly the two baseline rules itself.
//! - `detector.shadow_rules`: only when `wakeShadowRules` has rules.
//!
//! [`SessionReady::new`](echomuse_protocol::session::SessionReady::new) then strips the
//! rule lists for a device without `open_rules_v1` and `afe_metadata` for one without
//! `afe_metadata_v1`, so nothing here depends on capabilities.

use std::fmt;

use echomuse_protocol::Sha256;
use echomuse_protocol::session::{
    DetectorPolicy, OpenRule, PolicyError, ReadyGrant, RuleCombine, RuleProfile, SpeechAssets,
    Thresholds,
};

use crate::registry::WakeModel;
use crate::settings::DeviceSettings;
use crate::values::WakeRule;

/// Why no `session.ready` can be composed for a device.
#[derive(Clone, Debug, PartialEq)]
pub enum ComposeError {
    /// The selected wake model is not in the registry.
    ModelNotInRegistry(Sha256),
    /// The selected model's files are missing or do not hash to their names, so the
    /// device could not fetch them.
    ModelFilesUnusable(Sha256),
    /// The model passed is not the one the settings select.
    WrongModel {
        /// The settings' selection.
        selected: Sha256,
        /// The model passed.
        given: Sha256,
    },
    /// The device would refuse the policy.
    Policy(PolicyError),
}

impl fmt::Display for ComposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelNotInRegistry(sha) => write!(f, "wake model {sha} is not in the registry"),
            Self::ModelFilesUnusable(sha) => {
                write!(f, "wake model {sha} has missing or corrupt files")
            }
            Self::WrongModel { selected, given } => {
                write!(f, "settings select wake model {selected}, not {given}")
            }
            Self::Policy(e) => write!(f, "detector policy: {e}"),
        }
    }
}

impl std::error::Error for ComposeError {}

impl From<PolicyError> for ComposeError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}

/// `session.ready`'s `assets` and `detector`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadyParts {
    /// `assets`.
    pub assets: SpeechAssets,
    /// `detector`.
    pub detector: DetectorPolicy,
}

impl ReadyParts {
    /// The full grant, with the session's capture permission and whether to ask for the
    /// `afe` stream.
    pub fn into_grant(self, capture_permitted: bool, afe_metadata: bool) -> ReadyGrant {
        ReadyGrant {
            capture_permitted,
            assets: self.assets,
            detector: self.detector,
            afe_metadata,
        }
    }
}

fn open_rule(rule: &WakeRule) -> Result<OpenRule, PolicyError> {
    OpenRule::new(
        rule.profile,
        rule.windows.get(),
        rule.combine,
        rule.threshold.get(),
    )
}

/// Compose `assets` and `detector` from a device's effective settings, the registry
/// model they select and the runtime library's SHA-256.
pub fn compose_ready(
    settings: &DeviceSettings,
    model: &WakeModel,
    runtime_sha256: Sha256,
) -> Result<ReadyParts, ComposeError> {
    let selected = &settings.wakeword.wake_model.0;
    if model.graph_sha256() != selected {
        return Err(ComposeError::WrongModel {
            selected: selected.clone(),
            given: model.graph_sha256().clone(),
        });
    }
    if !model.is_usable() {
        return Err(ComposeError::ModelFilesUnusable(selected.clone()));
    }
    let model_thresholds = model.thresholds();
    let thresholds = Thresholds::new(
        model_thresholds.idle(),
        model_thresholds.playback(),
        settings.wakeword.near_miss_threshold.get(),
    )?;
    let extra = settings.wakeword.wake_open_rules.rules();
    let open_rules = if extra.is_empty() {
        None
    } else {
        let baseline = [
            OpenRule::new(
                RuleProfile::Idle,
                3,
                RuleCombine::Mean,
                model_thresholds.idle(),
            )?,
            OpenRule::new(
                RuleProfile::Playback,
                3,
                RuleCombine::Mean,
                model_thresholds.playback(),
            )?,
        ];
        let mut rules = baseline.to_vec();
        for rule in extra {
            rules.push(open_rule(rule)?);
        }
        Some(rules)
    };
    let shadow = settings.wakeword.wake_shadow_rules.rules();
    let shadow_rules = if shadow.is_empty() {
        None
    } else {
        Some(
            shadow
                .iter()
                .map(open_rule)
                .collect::<Result<Vec<_>, _>>()?,
        )
    };
    let detector = DetectorPolicy::new(
        thresholds,
        settings.playback.duck_db.get(),
        open_rules,
        shadow_rules,
    )?;
    Ok(ReadyParts {
        assets: SpeechAssets {
            runtime_sha256,
            graph_sha256: model.graph_sha256().clone(),
            sidecar_sha256: model.sidecar_sha256().clone(),
        },
        detector,
    })
}
