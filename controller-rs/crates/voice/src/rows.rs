//! What every persisted row of this controller shares (spec §4.11, §3.5.1): the policy
//! identity it records and its wall time.

use echomuse_records::{PolicyHash, WallSeconds};
use echomuse_settings::values::PauseAsr;
use echomuse_turn::UtteranceTrace;

/// The endpoint policy every turn of this controller runs (the turn crate's default),
/// stored as each turn row's `policy_hash`.
const POLICY: &str = "post_afe_10";

/// The policy's identity, and for an utterance whose pauses also went to a pause server,
/// that server's model (`+wyoming:<model>`, `default` for the server's own): a decision
/// made on its words depends on it (§4.7).
pub(crate) fn policy_hash(utterance: Option<(&UtteranceTrace, &PauseAsr)>) -> PolicyHash {
    let hash = match utterance {
        Some((trace, server)) if trace.config.pause_server => {
            let model = match server.model() {
                "" => "default",
                model => model,
            };
            format!("{POLICY}+wyoming:{model}")
        }
        _ => POLICY.to_owned(),
    };
    PolicyHash::new(hash).expect("the policy name is not empty")
}

pub(crate) fn wall_seconds(wall_ms: i64) -> WallSeconds {
    WallSeconds::new(wall_ms.max(0) as f64 / 1000.0)
        .or_else(|| WallSeconds::new(0.0))
        .expect("zero is a valid wall time")
}
