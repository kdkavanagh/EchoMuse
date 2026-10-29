//! Decision traces (post-AFE architecture §11.3): typed, serializable records of which evidence
//! and which rule produced each decision, one per turn's utterance and one per wake candidate.
//! They are written with the turn's row by the persistence layer; replays compare externally
//! meaningful outcomes, not this layout (§6.2).

use serde::{Deserialize, Serialize};

use crate::classify::{CellClass, ClassRule};
use crate::completeness::{RecognizerError, TextClass};
use crate::endpoint::{CloseCause, CloseReason, EndBound, Route, TurnConfig};
use crate::pause::{PauseRecord, TextSource};
use crate::time::{Sample, SampleSpan};

/// What kind of turn an utterance belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    /// Opened by an accepted wake word.
    Wake,
    /// Opened by the action button.
    Button,
    /// A reply to the controller's own question or HA's `continue_conversation`.
    Reply,
    /// A reply to an HA-started conversation (route R).
    HaReply,
}

/// A run of consecutive cells with one class and rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// The class.
    pub class: CellClass,
    /// The rule that decided it.
    pub rule: ClassRule,
    /// The first cell's start.
    pub start: Sample,
    /// The last cell's end.
    pub end: Sample,
}

/// How much of each input the utterance's cells had, so a decision can be read against the
/// evidence that was actually available.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceCounts {
    /// Cells classified.
    pub cells: u32,
    /// Gap or muted cells.
    pub gap: u32,
    /// Cells without a VAD result.
    pub vad_unknown: u32,
    /// Cells without the device's loudness.
    pub loudness_unknown: u32,
    /// Cells whose echo label was each value.
    pub echo_no_reference: u32,
    /// See [`EvidenceCounts::echo_no_reference`].
    pub echo_only: u32,
    /// See [`EvidenceCounts::echo_no_reference`].
    pub echo_near_end: u32,
    /// See [`EvidenceCounts::echo_no_reference`].
    pub echo_unknown: u32,
    /// Cells with partial or missing reference coverage.
    pub reference_incomplete: u32,
    /// Cells for which `B` was available.
    pub background_available: u32,
}

/// A change of the stable prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextChange {
    /// The new prefix, wake-cut and normalized.
    pub text: String,
    /// The grammar's class for it.
    pub grammar: Option<TextClass>,
    /// The block it changed at.
    pub at: Sample,
    /// Whose words it was judged on.
    pub source: TextSource,
}

/// One question to HA's sentence matcher and its answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecognizerRecord {
    /// The text asked about.
    pub text: String,
    /// The block at which it was asked.
    pub asked_at: Sample,
    /// The class HA gave, once applied.
    pub class: Option<TextClass>,
    /// The block boundary at which the answer took effect.
    pub applied_at: Option<Sample>,
    /// Round trip on the controller's monotonic clock, in milliseconds.
    pub round_trip_ms: Option<u64>,
    /// Why no class was applied.
    pub error: Option<RecognizerError>,
}

/// The evidence a route read when it set an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteEvidence {
    /// The prefix's class at the decision.
    pub class: Option<TextClass>,
    /// Command speech after the trigger, including cells a guarded reply's run was dated back
    /// over, in milliseconds.
    pub command_ms: u64,
    /// Route A: how many of the last 19 cells voted quiet.
    pub quiet_votes: Option<u32>,
    /// Route A: the decoder's trailing quiet, in milliseconds.
    pub trailing_quiet_ms: Option<u64>,
    /// Whether the trailing quiet came from a finalized pause decode.
    pub finalized: bool,
    /// The pause measured by the route, in milliseconds.
    pub pause_ms: u64,
    /// The pause the route required, in milliseconds.
    pub required_ms: u64,
    /// Route B: the prefix sample it measured from.
    pub prefix_sample: Option<Sample>,
    /// Route B: speech-positive cells after the prefix sample, and how many were background.
    pub speech_positive_after: Option<(u32, u32)>,
}

/// One endpoint event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EndpointEvent {
    /// A route set a provisional end.
    Pending {
        /// The route.
        route: Route,
        /// Where the command ended.
        boundary: Sample,
        /// The frontier at the decision; the lookahead counts from here.
        since: Sample,
        /// What the route read.
        evidence: RouteEvidence,
    },
    /// Further command speech revoked it.
    Revoked {
        /// The revoked boundary.
        boundary: Sample,
        /// The frontier when it was revoked.
        at: Sample,
        /// Command-speech cells after `since`.
        command_cells: u32,
    },
    /// The utterance was committed.
    Commit {
        /// The route.
        route: Route,
        /// Where the command ended.
        boundary: Sample,
        /// The committed audio.
        span: SampleSpan,
        /// Which bound set the span's end.
        end_bound: EndBound,
        /// The emission of the last command word considered, if any.
        last_word: Option<Sample>,
        /// The frontier at the commit.
        at: Sample,
        /// The stable prefix at the commit.
        text: String,
        /// Its class.
        class: Option<TextClass>,
        /// Whose words the text is: the pause server's when its words at any pause were
        /// this text, else Kroko's pause decode's, else the streaming result's.
        source: TextSource,
    },
    /// The utterance closed without a commit.
    Close {
        /// The terminal reason.
        reason: CloseReason,
        /// What caused it.
        cause: CloseCause,
        /// The frontier, when any cell had been classified.
        at: Option<Sample>,
    },
    /// A reply's gap before any command speech: the reply reopens after it.
    Reopen {
        /// Where the new utterance starts.
        start: Sample,
    },
}

/// The record of one utterance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UtteranceTrace {
    /// The policy values it ran with.
    pub config: TurnConfig,
    /// The turn's kind.
    pub kind: TraceKind,
    /// The capture epoch.
    pub epoch: u64,
    /// The utterance start.
    pub start: Sample,
    /// The trigger sample.
    pub trigger: Sample,
    /// A reply's opening guard end.
    pub guard_until: Option<Sample>,
    /// The first command-speech cell starting at or after the trigger.
    pub command_start: Option<Sample>,
    /// Where the run holding the first command speech started, as attribution dated it.
    pub answer_start: Option<Sample>,
    /// The last `F` in force, in dB.
    pub foreground_db: Option<f64>,
    /// Whether `F` was seeded from the wake word.
    pub foreground_seeded: bool,
    /// Command-speech cells after the trigger.
    pub command_cells: u32,
    /// What the cells' evidence held.
    pub evidence: EvidenceCounts,
    /// The class sequence as run-length segments.
    pub segments: Vec<Segment>,
    /// The latest raw streaming text.
    pub streaming_text: String,
    /// The latest judged words as transcribed: the pause server's transcript while it
    /// stands, else Kroko's.
    pub heard: String,
    /// Each change of the stable prefix.
    pub text: Vec<TextChange>,
    /// Blocks whose result was a finalized pause decode.
    pub finalized_at: Vec<Sample>,
    /// Each pause decode, with the pause server's words for it.
    pub pauses: Vec<PauseRecord>,
    /// Questions to HA's sentence matcher.
    pub recognizer: Vec<RecognizerRecord>,
    /// Endpoint events in order.
    pub endpoint: Vec<EndpointEvent>,
    /// The evidence frontier when the utterance ended.
    pub frontier: Option<Sample>,
}

impl UtteranceTrace {
    pub(crate) fn push_cell(
        &mut self,
        class: CellClass,
        rule: ClassRule,
        start: Sample,
        end: Sample,
    ) {
        if let Some(last) = self.segments.last_mut()
            && last.class == class
            && last.rule == rule
            && last.end == start
        {
            last.end = end;
            return;
        }
        self.segments.push(Segment {
            class,
            rule,
            start,
            end,
        });
    }
}
