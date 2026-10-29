//! The wake messages' bodies (docs/protocol-v1.md §4.4), decoded into the turn crate's
//! types. 64-bit values travel as decimal strings; a body that does not carry what a
//! decision needs is refused here with the field it lacked, and a body that is complete
//! but breaks the candidate rules (hop order, thresholds) is left to the wake decider,
//! which refuses it as `malformed` with its own trace.

use std::num::NonZeroU64;

use echomuse_protocol::Sha256;
use echomuse_turn::{Sample, WakeCandidate, WakeHop, WakeProfile};
use echomuse_uplink::SubscriptionId;
use serde_json::{Map, Value};

/// What the body is missing or has wrong.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BodyFault {
    pub field: &'static str,
    pub rule: &'static str,
}

impl std::fmt::Display for BodyFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.field, self.rule)
    }
}

fn fault(field: &'static str, rule: &'static str) -> BodyFault {
    BodyFault { field, rule }
}

fn text<'a>(body: &'a Map<String, Value>, field: &'static str) -> Result<&'a str, BodyFault> {
    body.get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| fault(field, "must be a non-empty string"))
}

/// A uint64 decimal string, as the firmware writes every 64-bit value.
fn decimal(value: Option<&Value>, field: &'static str) -> Result<u64, BodyFault> {
    let text = value
        .and_then(Value::as_str)
        .ok_or_else(|| fault(field, "must be a decimal string"))?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(fault(field, "must be a decimal string"));
    }
    text.parse()
        .map_err(|_| fault(field, "must fit an unsigned 64-bit number"))
}

fn optional_decimal(
    body: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<u64>, BodyFault> {
    match body.get(field) {
        None | Some(Value::Null) => Ok(None),
        value => decimal(value, field).map(Some),
    }
}

fn number(body: &Map<String, Value>, field: &'static str) -> Result<f64, BodyFault> {
    body.get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| fault(field, "must be a finite number"))
}

fn profile(value: Option<&Value>, field: &'static str) -> Result<WakeProfile, BodyFault> {
    match value.and_then(Value::as_str) {
        Some("idle") => Ok(WakeProfile::Idle),
        Some("playback") => Ok(WakeProfile::Playback),
        _ => Err(fault(field, "must be idle or playback")),
    }
}

/// An alert ringing or backgrounded on the device when the candidate opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateAlert {
    pub id: String,
    /// `alarm` or `timer` as sent.
    pub kind: String,
    pub name: String,
    pub foreground: bool,
}

/// A decoded `wake.candidate`.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateMessage {
    pub candidate: WakeCandidate,
    /// The candidate subscription the device opened (`lease_id`).
    pub subscription: SubscriptionId,
    pub capture_epoch: NonZeroU64,
    /// The wake graph the device scored with; `None` when it did not say.
    pub graph_sha256: Option<Sha256>,
    /// The device played the wake chime itself (`local_wake_chime`); absent means false.
    pub chimed: bool,
    pub active_alert: Option<CandidateAlert>,
}

/// The candidate's id and subscription, when a body is too broken to decode fully: the
/// refusal can still name them.
#[derive(Clone, Debug, PartialEq)]
pub struct BrokenCandidate {
    pub candidate_id: Option<String>,
    pub subscription: Option<SubscriptionId>,
    pub fault: BodyFault,
}

pub fn decode_candidate(body: &Map<String, Value>) -> Result<CandidateMessage, BrokenCandidate> {
    let candidate_id = text(body, "candidate_id").ok().map(str::to_owned);
    let subscription = text(body, "lease_id")
        .ok()
        .and_then(SubscriptionId::from_device);
    let broken = |fault| BrokenCandidate {
        candidate_id: candidate_id.clone(),
        subscription: subscription.clone(),
        fault,
    };
    decode_candidate_fields(body).map_err(broken)
}

fn decode_candidate_fields(body: &Map<String, Value>) -> Result<CandidateMessage, BodyFault> {
    let candidate_id = text(body, "candidate_id")?.to_owned();
    let subscription = SubscriptionId::from_device(text(body, "lease_id")?)
        .ok_or_else(|| fault("lease_id", "must be a non-empty string"))?;
    let capture_epoch = NonZeroU64::new(decimal(body.get("capture_epoch"), "capture_epoch")?)
        .ok_or_else(|| fault("capture_epoch", "must not be zero"))?;
    let graph_sha256 = match body.get("graph_sha256") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(Sha256::parse)
                .ok_or_else(|| fault("graph_sha256", "must be a SHA-256"))?,
        ),
    };
    let hops = body
        .get("hops")
        .and_then(Value::as_array)
        .ok_or_else(|| fault("hops", "must be an array"))?
        .iter()
        .map(|hop| {
            let hop = hop
                .as_object()
                .ok_or_else(|| fault("hops", "must hold objects"))?;
            let score = |field: &'static str| -> Result<Option<f64>, BodyFault> {
                match hop.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(value) => value
                        .as_f64()
                        .filter(|v| v.is_finite())
                        .map(Some)
                        .ok_or_else(|| fault("hops", "scores must be numbers or null")),
                }
            };
            Ok(WakeHop {
                end: Sample(decimal(hop.get("end_sample"), "hops.end_sample")?),
                raw: score("raw")?,
                smoothed: score("smoothed")?,
            })
        })
        .collect::<Result<Vec<_>, BodyFault>>()?;
    let active_alert = match body.get("active_alert") {
        None | Some(Value::Null) => None,
        Some(Value::Object(alert)) => Some(CandidateAlert {
            id: text(alert, "id")?.to_owned(),
            kind: text(alert, "kind")?.to_owned(),
            name: alert
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            foreground: alert
                .get("foreground")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some(_) => return Err(fault("active_alert", "must be an object or null")),
    };
    let candidate = WakeCandidate {
        candidate_id,
        profile: profile(body.get("profile"), "profile")?,
        threshold: number(body, "threshold")?,
        producing_sound: body
            .get("producing_sound")
            .and_then(Value::as_bool)
            .ok_or_else(|| fault("producing_sound", "must be a boolean"))?,
        first_crossing_end: Sample(decimal(
            body.get("first_crossing_end"),
            "first_crossing_end",
        )?),
        support_start: Sample(decimal(body.get("support_start"), "support_start")?),
        hops,
    };
    Ok(CandidateMessage {
        candidate,
        subscription,
        capture_epoch,
        graph_sha256,
        chimed: body.get("chimed").and_then(Value::as_bool).unwrap_or(false),
        active_alert,
    })
}

/// A decoded `wake.candidate_end`.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateEnd {
    pub candidate_id: String,
    pub support_end: Sample,
    pub peak_smoothed: Option<f64>,
}

pub fn decode_candidate_end(body: &Map<String, Value>) -> Result<CandidateEnd, BodyFault> {
    Ok(CandidateEnd {
        candidate_id: text(body, "candidate_id")?.to_owned(),
        support_end: Sample(decimal(body.get("support_end"), "support_end")?),
        peak_smoothed: body
            .get("peak_smoothed")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite()),
    })
}

/// A decoded `wake.near_miss` (`near_miss_v1`).
#[derive(Clone, Debug, PartialEq)]
pub struct NearMiss {
    pub id: String,
    pub capture_epoch: NonZeroU64,
    pub graph_sha256: Option<Sha256>,
    pub profile: WakeProfile,
    pub floor: f64,
    pub peak: f64,
    pub support_start: Sample,
    pub support_end: Sample,
    /// `below`, `gap`, `reset`, `mute` or `overrun`, as sent.
    pub reason: String,
}

pub fn decode_near_miss(body: &Map<String, Value>) -> Result<NearMiss, BodyFault> {
    Ok(NearMiss {
        id: text(body, "near_miss_id")?.to_owned(),
        capture_epoch: NonZeroU64::new(decimal(body.get("capture_epoch"), "capture_epoch")?)
            .ok_or_else(|| fault("capture_epoch", "must not be zero"))?,
        graph_sha256: body
            .get("graph_sha256")
            .and_then(Value::as_str)
            .and_then(Sha256::parse),
        profile: profile(body.get("profile"), "profile")?,
        floor: number(body, "floor")?,
        peak: number(body, "peak")?,
        support_start: Sample(decimal(body.get("support_start"), "support_start")?),
        support_end: Sample(decimal(body.get("support_end"), "support_end")?),
        reason: text(body, "reason")?.to_owned(),
    })
}

/// `wake.candidate`'s optional `mono_ns`, for diagnostics.
pub fn candidate_mono_ns(body: &Map<String, Value>) -> Option<u64> {
    optional_decimal(body, "mono_ns").ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_candidate_decodes_as_the_firmware_sends_it() {
        let message = decode_candidate(&body(json!({
            "candidate_id": "c1", "lease_id": "l1", "capture_epoch": "7",
            "graph_sha256": "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f",
            "scorer_revision": 3, "profile": "idle", "threshold": 0.9,
            "producing_sound": false, "chimed": true, "first_crossing_end": "48000",
            "support_start": "25600", "mono_ns": "123",
            "active_alert": null,
            "hops": [{"end_sample": "48000", "raw": 0.93, "smoothed": null, "profile": "idle"}],
        })))
        .unwrap();
        assert_eq!(message.candidate.candidate_id, "c1");
        assert_eq!(message.subscription.as_str(), "l1");
        assert_eq!(message.capture_epoch.get(), 7);
        assert!(message.chimed);
        assert_eq!(message.candidate.hops[0].end, Sample(48_000));
        assert_eq!(message.candidate.hops[0].smoothed, None);
    }

    #[test]
    fn a_broken_candidate_still_names_itself() {
        let broken = decode_candidate(&body(json!({
            "candidate_id": "c1", "lease_id": "l1", "capture_epoch": 7,
        })))
        .unwrap_err();
        assert_eq!(broken.candidate_id.as_deref(), Some("c1"));
        assert_eq!(broken.fault.field, "capture_epoch");
    }

    #[test]
    fn a_near_miss_decodes() {
        let near = decode_near_miss(&body(json!({
            "near_miss_id": "n1", "capture_epoch": "7", "graph_sha256": null,
            "profile": "playback", "floor": 0.17, "peak": 0.31,
            "support_start": "1000", "support_end": "9000", "mono_ns": "5", "reason": "below",
        })))
        .unwrap();
        assert_eq!(near.profile, WakeProfile::Playback);
        assert_eq!(near.support_end, Sample(9_000));
        assert_eq!(near.graph_sha256, None);
    }
}
