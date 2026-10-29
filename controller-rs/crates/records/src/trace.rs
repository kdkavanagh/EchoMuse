//! `turns.decision_trace`: the turn's decision trace as one compact JSON object (post-AFE
//! architecture §11.3), written with the turn's row.
//!
//! The layout is this controller's own: replays and the spec compare outcomes, not trace
//! layout (§5.6, §6.2). Where the existing rows and their readers already name a fact, the
//! same key and shape are used, so the dashboard's trace views and the documented queries
//! keep working:
//!
//! - `turn_id` (the turn's UUID, null for a refused candidate), `trigger`, `terminal_reason`;
//! - for a wake: `candidate_id`, `profile`, `producing_sound`, `wake_threshold` and `hops`
//!   as `{"end_sample", "raw", "smoothed"}`, then the turn crate's whole [`WakeTrace`] as
//!   `wake`;
//! - `verification` in the shape the dashboard's wake-verification view reads
//!   (`deadline_ms`, `input`, `input_ready_ms`, `mic_past_open_ms`, `answered_ms`,
//!   `decided_ms`, `late`, `result` of `pass`/`fail`/`timeout`, `text`, `distance`,
//!   `text_passes`, `error`; `queued_ms` and `decode_ms` are null: the speech worker does
//!   not split its wait from its decode), for a wake that went to the verifier;
//! - the turn crate's [`UtteranceTrace`] as `utterance`: its `start` and `endpoint` events
//!   with `"event":"commit"` (whose `source` says whose words the committed text is), its
//!   `heard`, and its `pauses` in the shape the dashboard's pause view reads (one per pause
//!   decode: `through`, then `kroko` and `server` as `{text, ms, judged, at, error}`,
//!   `server` null when Kroko transcribes alone); [`DecisionTrace::add_details`] adds
//!   `stable_prefix` (the last stable prefix, `""` before any).
//!
//! Kroko's `ms` at a pause is null: the worker does not report how long its decode took.

use echomuse_turn::{
    UtteranceTrace, WakeProfile, WakeTrace,
    trace::TraceKind,
    wake::{VerificationRecord, WakeRule},
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::{
    values::JsonObject,
    vocab::{TerminalReason, TriggerType},
};

/// What the decision trace adds to the turn crate's traces, from what the device runtime
/// saw around them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TraceDetails {
    /// The verifier's timing, for a wake that went to it.
    pub verification: VerificationTiming,
}

/// When a wake verification's input was complete, or how far the mic got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VerificationTiming {
    /// Milliseconds after the candidate arrived when the verifier's input was complete and
    /// sent; `None` when it never was.
    pub input_ready_ms: Option<u64>,
    /// How far past the candidate's open the mic had reached when it was decided, in ms.
    pub mic_past_open_ms: Option<u64>,
}

/// The trigger type of an utterance trace's kind.
pub fn trigger_of(kind: TraceKind) -> TriggerType {
    match kind {
        TraceKind::Wake => TriggerType::Wake,
        TraceKind::Button => TriggerType::Button,
        TraceKind::Reply => TriggerType::Reply,
        TraceKind::HaReply => TriggerType::HaReply,
    }
}

/// What a decision trace is composed from.
#[derive(Clone, Copy, Debug)]
pub struct TraceParts<'a> {
    /// The turn's UUID; None for a refused candidate.
    pub turn_uuid: Option<Uuid>,
    /// What opened the turn.
    pub trigger: TriggerType,
    /// How it ended.
    pub terminal_reason: TerminalReason,
    /// The wake candidate's trace, for a wake.
    pub wake: Option<&'a WakeTrace>,
    /// The utterance's trace, when one ran.
    pub utterance: Option<&'a UtteranceTrace>,
}

/// A composed `turns.decision_trace`.
#[derive(Clone, Debug, PartialEq)]
pub struct DecisionTrace {
    json: JsonObject,
    candidate_id: Option<String>,
}

impl DecisionTrace {
    /// Composes the trace.
    pub fn compose(parts: TraceParts<'_>) -> Self {
        let wake = parts.wake;
        let doc = TraceDoc {
            turn_id: parts.turn_uuid.map(|u| u.to_string()),
            trigger: parts.trigger,
            terminal_reason: parts.terminal_reason,
            candidate_id: wake.map(|w| w.candidate.candidate_id.as_str()),
            profile: wake.map(|w| w.candidate.profile),
            producing_sound: wake.map(|w| w.candidate.producing_sound),
            wake_threshold: wake.map(|w| w.candidate.threshold),
            hops: wake.map(|w| {
                w.candidate
                    .hops
                    .iter()
                    .map(|h| HopDoc {
                        end_sample: h.end.0,
                        raw: h.raw,
                        smoothed: h.smoothed,
                    })
                    .collect()
            }),
            wake,
            verification: wake.and_then(verification_doc),
            utterance: parts.utterance,
        };
        let text = serde_json::to_string(&doc).expect("decision trace serializes");
        Self {
            json: JsonObject::from_stored(text),
            candidate_id: wake.map(|w| w.candidate.candidate_id.clone()),
        }
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        self.json.as_str()
    }

    /// The wake candidate it records, if any.
    pub fn candidate_id(&self) -> Option<&str> {
        self.candidate_id.as_deref()
    }

    /// Adds the AFE evidence summaries under `afe_evidence`, as `turns.afe_evidence`
    /// holds them, replacing any already there.
    pub fn set_afe_evidence(&mut self, evidence: &crate::values::AfeEvidence) {
        let mut doc: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(self.json.as_str()).expect("a decision trace is an object");
        let value: serde_json::Value =
            serde_json::from_str(evidence.as_str()).expect("AFE evidence is an object");
        doc.insert("afe_evidence".to_owned(), value);
        self.json = JsonObject::from_map(&doc);
    }

    /// Adds what the dashboard reads beyond the turn crate's traces (see the module docs):
    /// the verifier's timing, and for `utterance` its stable prefix.
    pub fn add_details(&mut self, utterance: Option<&UtteranceTrace>, details: &TraceDetails) {
        let mut doc: Map<String, Value> =
            serde_json::from_str(self.json.as_str()).expect("a decision trace is an object");
        if let Some(Value::Object(verification)) = doc.get_mut("verification") {
            let timing = details.verification;
            verification.insert("input_ready_ms".to_owned(), json!(timing.input_ready_ms));
            verification.insert(
                "mic_past_open_ms".to_owned(),
                json!(timing.mic_past_open_ms),
            );
            verification.insert("queued_ms".to_owned(), Value::Null);
            verification.insert("decode_ms".to_owned(), Value::Null);
        }
        if let (Some(trace), Some(Value::Object(u))) = (utterance, doc.get_mut("utterance")) {
            let stable = trace.text.last().map_or(String::new(), |c| c.text.clone());
            u.insert("stable_prefix".to_owned(), json!(stable));
        }
        self.json = JsonObject::from_map(&doc);
    }
}

#[derive(Serialize)]
struct HopDoc {
    end_sample: u64,
    raw: Option<f64>,
    smoothed: Option<f64>,
}

#[derive(Serialize)]
struct VerificationDoc<'a> {
    deadline_ms: u64,
    input: Option<[u64; 2]>,
    answered_ms: Option<u64>,
    decided_ms: Option<u64>,
    late: Option<bool>,
    result: Option<&'static str>,
    text: Option<&'a str>,
    distance: Option<f64>,
    text_passes: Option<bool>,
    error: Option<&'static str>,
}

/// The verification view of a candidate whose wake went to the verifier: one heard while the
/// device was producing sound.
fn verification_doc(wake: &WakeTrace) -> Option<VerificationDoc<'_>> {
    let window = wake.verification_window?;
    let record: Option<&VerificationRecord> = wake.verification.as_ref();
    let (result, error) = match wake.rule {
        Some(WakeRule::Verified) => (Some("pass"), None),
        Some(WakeRule::VerifierFailed) => (Some("fail"), None),
        Some(WakeRule::VerifierError) => (None, Some("the decode failed")),
        Some(WakeRule::Deadline) => (Some("timeout"), None),
        _ => (None, None),
    };
    Some(VerificationDoc {
        deadline_ms: wake.deadline_ms,
        input: Some([window.start.0, window.end.0]),
        answered_ms: record.map(|r| r.at_ms),
        decided_ms: wake.decided_ms,
        late: record.map(|r| r.late),
        result,
        text: record.and_then(|r| r.text.as_deref()),
        distance: record.and_then(|r| r.distance),
        text_passes: record.map(|r| r.passes),
        error,
    })
}

#[derive(Serialize)]
struct TraceDoc<'a> {
    turn_id: Option<String>,
    trigger: TriggerType,
    terminal_reason: TerminalReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<WakeProfile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    producing_sound: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wake_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hops: Option<Vec<HopDoc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wake: Option<&'a WakeTrace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification: Option<VerificationDoc<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    utterance: Option<&'a UtteranceTrace>,
}
