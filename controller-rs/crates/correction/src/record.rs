//! The decision record a turn stores in `turns.stt_fix`.
//!
//! The column's JSON shape is an existing contract: the dashboard's Activity view and the
//! turns API read it (`mode`, `reason`, `original`, `proposed`, `sent`, `applied`, the
//! proposal's `rule`, `source`, `raw`, `coverage`, `preference`, `rank`, `status`,
//! `preference_changed`, `verdict`, `candidates`, `context`, `compute_ms`, `latency_ms`,
//! `deadline_met`, `error`). This record keeps every field's meaning. Where it differs:
//!
//! - `cache` is never written: there is no pause-time speculation, so every result is
//!   computed at dispatch.
//! - `context.tables` is the tables' revision as before; `context.tables_digest` adds the
//!   digest of their inputs, which the revision alone does not identify.
//! - `work` is added: the comparison work the proposal did and whether the limit cut it
//!   short, so the bounded cost is visible per turn.
//! - `rule` gains `word` (a template word) and `source` gains `template`; the dashboard
//!   shows unknown words as written.
//! - `skipped` is added when the transcript was too short or too long to propose for.

use std::collections::BTreeMap;

use echomuse_core::services::SentenceRecognition;
use serde::Serialize;

use crate::{
    propose::{Rule, Skipped},
    sounds::Work,
    tables::Source,
};

/// The correction mode a record was made under (the record never exists under `off`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordMode {
    /// Computed beside the turn; the heard text was sent.
    Shadow,
    /// The correction was sent when it passed.
    On,
}

/// Why the turn sent what it sent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// The correction was sent.
    Applied,
    /// Shadow mode: the correction would have been sent.
    WouldApply,
    /// Home Assistant's matcher already understood the heard text.
    OriginalMatched,
    /// Nothing to propose.
    NoCandidate,
    /// No candidate kept what was said and passed the matcher.
    NoValidCandidate,
    /// Two interpretations were too close to choose between.
    Ambiguous,
    /// A follow-up in a conversation: not corrected.
    Contextual,
    /// Tables, preferences or the pipeline's language were not available.
    NotReady,
    /// Not ready within the deadline after the final transcript.
    Timeout,
    /// The matcher failed.
    Error,
    /// Computed under a context that no longer held.
    Stale,
}

/// A candidate's verdict (the record's `status`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Eligible.
    Ok,
    /// Drops or changes what was said; never shown to Home Assistant.
    NotPreserved,
    /// Not checked: past the validation limit.
    NotValidated,
    /// Home Assistant's matcher does not accept it.
    NoMatch,
    /// Matched only by a free-text slot or wildcard.
    CatchAll,
    /// The inserted name did not resolve to its target.
    Unresolved,
    /// It changes a slot or target Home Assistant had resolved for the heard text.
    ChangedResolved,
}

/// Home Assistant's matcher on one text.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Verdict {
    /// Fully matched.
    #[serde(rename = "match")]
    pub matched: bool,
    /// `trigger`, `builtin` or `custom`, as HA sent it.
    pub source: Option<String>,
    /// The template that matched or came closest.
    pub template: String,
    /// For an intent: its name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// For an intent: each filled slot's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slots: Option<BTreeMap<String, String>>,
    /// For an intent: the words each unfilled slot was left with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<BTreeMap<String, String>>,
}

impl Verdict {
    /// The verdict a recognition gives.
    pub fn of(recognition: &SentenceRecognition) -> Self {
        let intent = recognition.intent.clone();
        let is_intent = intent.is_some();
        Self {
            matched: recognition.matched,
            source: recognition.source.clone(),
            template: recognition.template.clone(),
            intent,
            slots: is_intent.then(|| {
                recognition
                    .details
                    .iter()
                    .map(|(k, v)| (k.clone(), v.value.clone()))
                    .collect()
            }),
            unmatched: is_intent.then(|| recognition.unmatched.clone()),
        }
    }
}

/// The matcher on the heard text and on the proposal; `null` where it recognized nothing.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Verdicts {
    /// On the heard text.
    pub original: Option<Verdict>,
    /// On the proposal.
    pub proposed: Option<Verdict>,
}

/// One candidate as recorded.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CandidateRecord {
    /// Its text.
    pub text: String,
    /// The repair.
    pub rule: Rule,
    /// Where its words came from.
    pub source: Source,
    /// Evidence.
    pub raw: f64,
    /// Cut-off coverage.
    pub coverage: Option<f64>,
    /// Preference added.
    pub preference: f64,
    /// Evidence plus preference.
    pub rank: f64,
    /// Its verdict.
    pub status: Status,
    /// Why, when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The template HA matched it with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<String>,
    /// The intent HA matched it as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

/// The context a result was computed in.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextRecord {
    /// Home Assistant connection generation.
    pub connection: u64,
    /// Tables revision.
    pub tables: u64,
    /// Tables input digest.
    pub tables_digest: String,
    /// Settings digest.
    pub config: String,
    /// Pipeline ID.
    pub pipeline: Option<String>,
    /// The language the matcher was asked in.
    pub language: String,
}

/// The work the proposal did.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct WorkRecord {
    /// Edit-distance cells.
    pub cells: u64,
    /// Table rows considered.
    pub rows: u64,
    /// The work limit cut the search short.
    pub truncated: bool,
}

impl WorkRecord {
    pub(crate) fn new(work: Work, truncated: bool) -> Self {
        Self {
            cells: work.cells,
            rows: work.rows,
            truncated,
        }
    }
}

/// A turn's correction record (`turns.stt_fix`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FixRecord {
    /// The mode.
    pub mode: RecordMode,
    /// Why the turn sent what it sent.
    pub reason: Reason,
    /// The transcript as heard (wake phrase removed).
    pub original: String,
    /// The best candidate HA accepted, eligible or not.
    pub proposed: Option<String>,
    /// The text the turn sent.
    pub sent: String,
    /// Whether that was a correction.
    pub applied: bool,
    /// The proposal's repair.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<Rule>,
    /// The proposal's source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// The proposal's evidence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw: Option<f64>,
    /// The proposal's coverage (a cut-off), `null` otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Option<f64>>,
    /// The proposal's preference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preference: Option<f64>,
    /// The proposal's rank.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<f64>,
    /// The proposal's status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// The user's preference chose the spelling sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preference_changed: Option<bool>,
    /// HA's matcher on the heard text and the proposal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdicts>,
    /// Up to [`MAX_RECORDED_CANDIDATES`]: those checked by HA by rank, then those refused or
    /// held before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<CandidateRecord>>,
    /// The context the result was computed in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextRecord>,
    /// The proposal's own time, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_ms: Option<u64>,
    /// Time from the final transcript to the result, in milliseconds.
    pub latency_ms: Option<u64>,
    /// Whether the result was ready within the deadline.
    pub deadline_met: Option<bool>,
    /// What failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The comparison work the proposal did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<WorkRecord>,
    /// Why nothing was proposed, when the text was too short or too long.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Skipped>,
}

/// The most candidates a record lists.
pub const MAX_RECORDED_CANDIDATES: usize = 6;

impl FixRecord {
    /// A record with only the decision: nothing proposed, the heard text sent.
    pub(crate) fn bare(mode: RecordMode, reason: Reason, original: &str) -> Self {
        Self {
            mode,
            reason,
            original: original.to_owned(),
            proposed: None,
            sent: original.to_owned(),
            applied: false,
            rule: None,
            source: None,
            raw: None,
            coverage: None,
            preference: None,
            rank: None,
            status: None,
            preference_changed: None,
            verdict: None,
            candidates: None,
            context: None,
            compute_ms: None,
            latency_ms: None,
            deadline_met: None,
            error: None,
            work: None,
            skipped: None,
        }
    }

    /// The record as the compact JSON object `turns.stt_fix` stores.
    pub fn to_json(&self) -> String {
        // Every field is a string, number, bool, list or string-keyed map, which always
        // serialize (a non-finite number is written as null).
        serde_json::to_string(self).expect("a correction record always serializes")
    }
}
