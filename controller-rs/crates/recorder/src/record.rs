//! The recorded vocabulary: every value a session file holds, as typed serde records.
//!
//! The speech policy's own types are used where they already have a stable serialized form
//! (sample positions, spans, policies, candidates, outcomes). Where they have none, a record
//! type here mirrors them exactly and converts both ways, so a recording reads back into the
//! values the runtime handed the policy.

use echomuse_echo::{EchoPlan, EchoPolicy, OutputCoverage};
use echomuse_protocol::StreamId;
use echomuse_turn::completeness::RecognizerError;
use echomuse_turn::wake::WakeRule;
use echomuse_turn::{
    ArrivalConditions, AsrKind, AsrObservation, CandidateEcho, CellIndex, CloseReason, Commit,
    Decision, DeviceCell, DeviceInput, EchoCell, EchoLabel, EndBound, Epoch, EvidenceError,
    MonoInstant, PauseServerAnswer, RecognizerAnswer, ReferenceCoverage, ReferenceScore,
    ReplyOrigin, ReplyReopen, ReplySpec, Route, Sample, SampleSpan, ServerFailure, TextClass,
    TextSource, Token, TurnConfig, TurnKind, TurnSpec, VadInput, VerifierResult, WakeAnchor,
    WakeCandidate, WakeDecision, WakeOutcome, WakePolicy,
};
use serde::{Deserialize, Serialize};

use crate::grant::{GrantId, StreamScope, UnixMillis};

/// A time on the recording's own clock: nanoseconds of the controller's monotonic clock since
/// the recording started. Replay runs its controlled clock on these values directly.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct RelTime(pub u64);

impl RelTime {
    /// `now` relative to `origin`; an instant before the origin is zero.
    pub fn between(origin: MonoInstant, now: MonoInstant) -> Self {
        Self(now.0.saturating_sub(origin.0))
    }

    /// This time as an instant on the replay clock, whose origin is zero.
    pub const fn instant(self) -> MonoInstant {
        MonoInstant(self.0)
    }
}

/// A recording's identity: a fresh UUIDv4, also its file name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RecordingId(String);

impl RecordingId {
    pub(crate) fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id from text, refusing anything that is not a UUID (so it is always a safe file
    /// name).
    pub fn parse(text: &str) -> Option<Self> {
        uuid::Uuid::parse_str(text)
            .ok()
            .map(|u| Self(u.hyphenated().to_string()))
    }
}

/// The order in which the device runtime observed a message or frame on the device link,
/// counted by the runtime across control and audio ingress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IngressOrdinal(pub u64);

/// One echo analyzer the runtime holds, named by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnalyzerId(pub u64);

/// One HA request and its reply, named by the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExchangeId(pub u64);

/// An uplink stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamName {
    /// Mic audio.
    Mic,
    /// The final-mix reference.
    Reference,
    /// Loudness cells.
    Cells,
    /// AFE records.
    Afe,
}

impl From<StreamId> for StreamName {
    fn from(s: StreamId) -> Self {
        match s {
            StreamId::Mic => Self::Mic,
            StreamId::Reference => Self::Reference,
            StreamId::Cells => Self::Cells,
            StreamId::Afe => Self::Afe,
        }
    }
}

/// Text that the grant either allowed to be kept or withheld.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "transcript", content = "text", rename_all = "snake_case")]
pub enum Transcript {
    /// The text as the runtime had it.
    Kept(String),
    /// The grant did not cover transcripts.
    Withheld,
}

impl Transcript {
    pub(crate) fn gate(text: &str, scope: &StreamScope) -> Self {
        if scope.transcripts {
            Self::Kept(text.to_owned())
        } else {
            Self::Withheld
        }
    }

    /// The text, when kept.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Kept(t) => Some(t),
            Self::Withheld => None,
        }
    }
}

/// The device's privacy state as the runtime holds it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyState {
    /// The device reported its privacy mute on.
    pub muted: bool,
    /// The runtime allows capture at all (for example, the session is admitted and attached).
    pub capture_permitted: bool,
}

impl PrivacyState {
    /// Whether audio may be kept in this state.
    pub const fn audio_allowed(&self) -> bool {
        !self.muted && self.capture_permitted
    }
}

/// A model the speech path ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    /// The device's wake graph, by its registry hash.
    WakeGraph,
    /// The controller's VAD.
    Vad,
    /// The controller's streaming ASR.
    StreamingAsr,
    /// The local completeness grammar.
    CompletenessGrammar,
}

/// A lowercase hexadecimal SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sha256Hex(String);

impl Sha256Hex {
    /// The digest, or `None` unless `text` is 64 hexadecimal digits (stored lowercase).
    pub fn new(text: &str) -> Option<Self> {
        (text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| Self(text.to_ascii_lowercase()))
    }

    /// The digest as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Sha256Hex {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        Self::new(&s).ok_or_else(|| format!("not a SHA-256 digest: {s:?}"))
    }
}

impl From<Sha256Hex> for String {
    fn from(h: Sha256Hex) -> String {
        h.0
    }
}

/// One model's identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRevision {
    /// What it does.
    pub role: ModelRole,
    /// Its content digest.
    pub sha256: Sha256Hex,
}

/// The configuration and model identities a session ran with. The policies themselves are
/// recorded in full with every candidate, turn and analyzer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revisions {
    /// The controller's build identity.
    pub controller: String,
    /// The firmware version the device announced, when it did.
    pub firmware: Option<String>,
    /// The device configuration revision in force, when the runtime has one.
    pub device_config: Option<String>,
    /// The models.
    pub models: Vec<ModelRevision>,
}

/// Who the recording is about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The device id.
    pub device: String,
    /// The device runtime's identity.
    pub runtime: u64,
    /// The transport session's controller identity.
    pub session: u64,
    /// The link `session_id` sent in `session.ready`.
    pub link_session: String,
}

/// The grant as the recording states it, without wall times: when it started and ends,
/// relative to the recording's start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantRecord {
    /// The grant.
    pub id: GrantId,
    /// Who gave it.
    pub administrator: String,
    /// What it allowed.
    pub streams: StreamScope,
    /// How long the grant had run when the recording started, in milliseconds.
    pub elapsed_at_start_ms: u64,
    /// How long it had left, in milliseconds.
    pub remaining_at_start_ms: u64,
}

/// The first record of every file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// The recording.
    pub recording: RecordingId,
    /// The session.
    pub session: SessionRecord,
    /// The grant.
    pub grant: GrantRecord,
    /// The only wall-clock value in the file: the wall time at relative time zero.
    pub anchor: UnixMillis,
    /// The file is deleted this long after the anchor, in milliseconds.
    pub retention_ms: u64,
    /// The identities in force at the start.
    pub revisions: Revisions,
    /// The privacy state at the start.
    pub privacy: PrivacyState,
}

impl Header {
    /// When the file is due for deletion.
    pub fn retain_until(&self) -> UnixMillis {
        UnixMillis(self.anchor.0.saturating_add(self.retention_ms))
    }
}

/// Why a recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The runtime finished it.
    Finished,
    /// The device session ended.
    Disconnected,
    /// The grant's interval ran out.
    Expired,
    /// An administrator withdrew the grant; unwritten entries were discarded.
    Withdrawn,
    /// The runtime dropped its recorder without finishing it.
    Abandoned,
}

/// Counts of everything the file does not hold that it otherwise would.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    /// Entries dropped because the write queue was full.
    pub lost_entries: u64,
    /// Audio frames accepted but discarded unwritten on mute or withdrawal.
    pub discarded_frames: u64,
    /// Other entries discarded unwritten on withdrawal.
    pub discarded_events: u64,
    /// Audio frames not taken at all because the device was muted or capture not permitted.
    pub withheld_frames: u64,
    /// Credential values replaced in HA bodies.
    pub credential_redactions: u64,
}

impl Tally {
    pub(crate) fn add(&mut self, other: &Tally) {
        self.lost_entries += other.lost_entries;
        self.discarded_frames += other.discarded_frames;
        self.discarded_events += other.discarded_events;
        self.withheld_frames += other.withheld_frames;
        self.credential_redactions += other.credential_redactions;
    }
}

/// The last record of a file that ended in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct End {
    /// When it ended.
    pub t: RelTime,
    /// Why.
    pub reason: EndReason,
    /// The totals.
    pub tally: Tally,
    /// The last sequence number the runtime assigned, written or not.
    pub last_seq: Option<u64>,
}

/// A note the writer itself adds where entries are missing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "marker", rename_all = "snake_case")]
pub enum Marker {
    /// The queue was full: this many entries were dropped since the last marker.
    Lost {
        /// How many.
        entries: u64,
    },
    /// Unwritten entries were discarded on mute or withdrawal.
    Discarded {
        /// Audio frames.
        frames: u64,
        /// Other entries.
        events: u64,
    },
}

/// One observed event, in the order the runtime recorded it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The runtime's sequence number; gaps are lost entries.
    pub seq: u64,
    /// When.
    pub t: RelTime,
    /// What.
    pub event: Event,
}

/// Why the session ended, as the runtime saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisconnectReason {
    /// The device closed the link.
    DeviceClosed,
    /// A newer session for the device replaced this one.
    Replaced,
    /// The link failed or timed out.
    LinkLost,
    /// The controller ended it (shutdown, deletion, refusal).
    ControllerClosed,
}

/// What the runtime cancelled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum CancelTarget {
    /// A turn; nothing more is fed to its policy.
    Turn {
        /// The turn.
        turn: u64,
    },
    /// A wake candidate.
    Candidate {
        /// Its id.
        candidate_id: String,
    },
    /// An HA exchange.
    Exchange {
        /// The exchange.
        exchange: ExchangeId,
    },
    /// A playback, by its render generation.
    Playback {
        /// The generation.
        generation: u32,
    },
}

/// Why audio was not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithheldReason {
    /// The device was muted.
    Muted,
    /// Capture was not permitted.
    CaptureNotPermitted,
    /// The frame itself carried the muted flag.
    MutedFrame,
}

/// Every event kind.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// New identities came into force.
    Revisions(Revisions),
    /// `stream.open`.
    StreamOpened {
        /// The link ingress.
        ingress: IngressOrdinal,
        /// The stream.
        stream: StreamName,
        /// Its epoch.
        epoch: Epoch,
    },
    /// `stream.end`.
    StreamEnded {
        /// The link ingress.
        ingress: IngressOrdinal,
        /// The stream.
        stream: StreamName,
        /// Its epoch.
        epoch: Epoch,
    },
    /// The privacy state changed.
    Privacy {
        /// The link ingress of `privacy.changed`, when that caused it.
        ingress: Option<IngressOrdinal>,
        /// The new state.
        state: PrivacyState,
    },
    /// Audio frames were not taken while audio was not allowed; written when it is allowed
    /// again or the recording ends.
    AudioWithheld {
        /// Why.
        reason: WithheldReason,
        /// How many frames.
        frames: u64,
    },
    /// The runtime cancelled something.
    Cancelled(CancelTarget),
    /// The session ended.
    Disconnected {
        /// Why.
        reason: DisconnectReason,
    },
    /// A wake candidate's step.
    Wake(WakeEntry),
    /// A turn's step.
    Turn {
        /// The runtime's turn identity.
        turn: u64,
        /// The step.
        step: TurnStep,
    },
    /// An echo analyzer's step.
    Echo {
        /// The analyzer.
        analyzer: AnalyzerId,
        /// The step.
        step: EchoStep,
    },
    /// An HA exchange's step.
    Ha(HaEntry),
}

impl Event {
    /// The rough size this event takes in the write queue, for its byte bound.
    pub(crate) const QUEUE_COST: usize = 1_024;
}

/// The arrival conditions, as recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionsRecord {
    /// A dashboard diagnostic recording mode was active.
    pub diagnostic_recording: bool,
    /// The speech path was unavailable.
    pub speech_unavailable: bool,
    /// The device was muted.
    pub muted: bool,
    /// Another candidate was pending.
    pub candidate_pending: bool,
}

impl From<ArrivalConditions> for ConditionsRecord {
    fn from(c: ArrivalConditions) -> Self {
        Self {
            diagnostic_recording: c.diagnostic_recording,
            speech_unavailable: c.speech_unavailable,
            muted: c.muted,
            candidate_pending: c.candidate_pending,
        }
    }
}

impl From<ConditionsRecord> for ArrivalConditions {
    fn from(c: ConditionsRecord) -> Self {
        Self {
            diagnostic_recording: c.diagnostic_recording,
            speech_unavailable: c.speech_unavailable,
            muted: c.muted,
            candidate_pending: c.candidate_pending,
        }
    }
}

/// A wake decision, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeDecisionRecord {
    /// The outcome.
    pub outcome: WakeOutcome,
    /// The rule.
    pub rule: WakeRule,
}

impl From<WakeDecision> for WakeDecisionRecord {
    fn from(d: WakeDecision) -> Self {
        Self {
            outcome: d.outcome,
            rule: d.rule,
        }
    }
}

/// The verifier's result, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "text", rename_all = "snake_case")]
pub enum VerifierRecord {
    /// A transcript.
    Text(Transcript),
    /// The decode failed.
    Error,
}

/// One step of one wake candidate, with the decision the decider returned at that step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum WakeEntry {
    /// `WakeDecider::arrive`.
    Arrived {
        /// The candidate.
        candidate: WakeCandidate,
        /// The runtime's state.
        conditions: ConditionsRecord,
        /// The policy.
        policy: WakePolicy,
        /// The decision made at once, if any.
        decision: Option<WakeDecisionRecord>,
    },
    /// `WakeDecider::reference_scored`.
    Reference {
        /// The candidate.
        candidate_id: String,
        /// The score.
        score: ReferenceScore,
        /// The decision it produced, if any.
        decision: Option<WakeDecisionRecord>,
    },
    /// `WakeDecider::echo_compared`.
    Echo {
        /// The candidate.
        candidate_id: String,
        /// The comparison.
        echo: CandidateEcho,
        /// The decision it produced, if any.
        decision: Option<WakeDecisionRecord>,
    },
    /// `WakeDecider::verified`.
    Verified {
        /// The candidate.
        candidate_id: String,
        /// The verifier's result.
        result: VerifierRecord,
        /// The decision it produced, if any.
        decision: Option<WakeDecisionRecord>,
    },
    /// `WakeDecider::on_timer`: the deadline became effective here.
    Timer {
        /// The candidate.
        candidate_id: String,
        /// The decision it produced, if any.
        decision: Option<WakeDecisionRecord>,
    },
}

impl WakeEntry {
    /// The candidate this step belongs to.
    pub fn candidate_id(&self) -> &str {
        match self {
            Self::Arrived { candidate, .. } => &candidate.candidate_id,
            Self::Reference { candidate_id, .. }
            | Self::Echo { candidate_id, .. }
            | Self::Verified { candidate_id, .. }
            | Self::Timer { candidate_id, .. } => candidate_id,
        }
    }

    /// The decision recorded at this step.
    pub fn decision(&self) -> Option<WakeDecisionRecord> {
        match self {
            Self::Arrived { decision, .. }
            | Self::Reference { decision, .. }
            | Self::Echo { decision, .. }
            | Self::Verified { decision, .. }
            | Self::Timer { decision, .. } => *decision,
        }
    }
}

/// What opened a turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cause", rename_all = "snake_case")]
pub enum TurnCause {
    /// An accepted wake candidate.
    Wake {
        /// The candidate.
        candidate_id: String,
    },
    /// The action button.
    Button,
    /// A reply window, after the turn it follows when it reopened one.
    Reply {
        /// The turn whose `ReopenReply` decision opened it, if any.
        reopened_from: Option<u64>,
    },
}

/// A reply window, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplyRecord {
    /// Who asked.
    pub origin: ReplyOrigin,
    /// The opening guard's end.
    pub guard_until: Option<Sample>,
    /// The `no_input` sample.
    pub no_input_at: Sample,
    /// The monotonic bound, on the recording's clock.
    pub wall_deadline: RelTime,
}

/// What opened the turn, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TurnKindRecord {
    /// A wake word.
    Wake {
        /// The candidate's support start.
        support_start: Sample,
        /// The candidate's open.
        candidate_open: Sample,
        /// The registry's wake-phrase spelling (configuration, not a transcript).
        phrase: String,
    },
    /// The button.
    Button,
    /// A reply window.
    Reply(ReplyRecord),
}

/// A turn's spec, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnSpecRecord {
    /// The capture epoch.
    pub epoch: Epoch,
    /// The utterance start.
    pub start: Sample,
    /// The trigger.
    pub trigger: Sample,
    /// The first loudness cell.
    pub cells_from: CellIndex,
    /// The first VAD cell.
    pub vad_from: CellIndex,
    /// What opened it.
    pub kind: TurnKindRecord,
}

impl TurnSpecRecord {
    pub(crate) fn of(spec: &TurnSpec, origin: MonoInstant) -> Self {
        Self {
            epoch: spec.epoch,
            start: spec.start,
            trigger: spec.trigger,
            cells_from: spec.cells_from,
            vad_from: spec.vad_from,
            kind: match &spec.kind {
                TurnKind::Wake(w) => TurnKindRecord::Wake {
                    support_start: w.support_start,
                    candidate_open: w.candidate_open,
                    phrase: w.phrase.clone(),
                },
                TurnKind::Button => TurnKindRecord::Button,
                TurnKind::Reply(r) => TurnKindRecord::Reply(ReplyRecord {
                    origin: r.origin,
                    guard_until: r.guard_until,
                    no_input_at: r.no_input_at,
                    wall_deadline: RelTime::between(origin, r.wall_deadline),
                }),
            },
        }
    }

    /// The spec on the replay clock.
    pub fn to_spec(&self) -> TurnSpec {
        TurnSpec {
            epoch: self.epoch,
            start: self.start,
            trigger: self.trigger,
            cells_from: self.cells_from,
            vad_from: self.vad_from,
            kind: match &self.kind {
                TurnKindRecord::Wake {
                    support_start,
                    candidate_open,
                    phrase,
                } => TurnKind::Wake(WakeAnchor {
                    support_start: *support_start,
                    candidate_open: *candidate_open,
                    phrase: phrase.clone(),
                }),
                TurnKindRecord::Button => TurnKind::Button,
                TurnKindRecord::Reply(r) => TurnKind::Reply(ReplySpec {
                    origin: r.origin,
                    guard_until: r.guard_until,
                    no_input_at: r.no_input_at,
                    wall_deadline: r.wall_deadline.instant(),
                }),
            },
        }
    }
}

/// One device loudness input, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum DeviceInputRecord {
    /// The device's record: loudness, the wire flags (gap 1, muted 2) and the source mask.
    Cell {
        /// Loudness in hundredths of a dB.
        e_centi: i16,
        /// Wire flags.
        flags: u8,
        /// Source mask.
        source_mask: u8,
    },
    /// No record and none coming.
    Unknown,
}

impl From<DeviceInput> for DeviceInputRecord {
    fn from(d: DeviceInput) -> Self {
        match d {
            DeviceInput::Cell(c) => Self::Cell {
                e_centi: c.e_centi(),
                flags: u8::from(c.is_gap()) | (u8::from(c.is_muted()) << 1),
                source_mask: c.source_mask(),
            },
            DeviceInput::Unknown => Self::Unknown,
        }
    }
}

impl DeviceInputRecord {
    /// The input, applying the cell-record rules again.
    pub fn to_input(self) -> Result<DeviceInput, EvidenceError> {
        match self {
            Self::Cell {
                e_centi,
                flags,
                source_mask,
            } => DeviceCell::from_wire(e_centi, flags, source_mask).map(DeviceInput::Cell),
            Self::Unknown => Ok(DeviceInput::Unknown),
        }
    }
}

/// One VAD input: a probability, or `None` for unknown.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VadRecord(pub Option<f32>);

impl From<VadInput> for VadRecord {
    fn from(v: VadInput) -> Self {
        match v {
            VadInput::Probability(p) => Self(Some(p.get())),
            VadInput::Unknown => Self(None),
        }
    }
}

impl VadRecord {
    /// The input, refusing probabilities outside [0, 1].
    pub fn to_input(self) -> Result<VadInput, EvidenceError> {
        match self.0 {
            Some(p) => VadInput::probability(p),
            None => Ok(VadInput::Unknown),
        }
    }
}

/// One echo input, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoCellRecord {
    /// The label.
    pub label: EchoLabel,
    /// The coverage.
    pub coverage: ReferenceCoverage,
}

impl From<EchoCell> for EchoCellRecord {
    fn from(e: EchoCell) -> Self {
        Self {
            label: e.label,
            coverage: e.coverage,
        }
    }
}

impl From<EchoCellRecord> for EchoCell {
    fn from(e: EchoCellRecord) -> Self {
        Self {
            label: e.label,
            coverage: e.coverage,
        }
    }
}

/// One recognizer token, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// The token's spelling.
    pub text: String,
    /// When it was emitted.
    pub emitted: Sample,
}

/// One streaming ASR result, as recorded. The tokens are text, kept only under a transcripts
/// grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsrRecord {
    /// Covered-through sample.
    pub through: Sample,
    /// The tokens, or `None` when transcripts were withheld.
    pub tokens: Option<Vec<TokenRecord>>,
    /// Trailing blank frames.
    pub trailing_blanks: u32,
    /// Which decode.
    pub kind: AsrKind,
}

impl AsrRecord {
    pub(crate) fn of(obs: &AsrObservation, scope: &StreamScope) -> Self {
        Self {
            through: obs.through,
            tokens: scope.transcripts.then(|| {
                obs.tokens
                    .iter()
                    .map(|t| TokenRecord {
                        text: t.text.clone(),
                        emitted: t.emitted,
                    })
                    .collect()
            }),
            trailing_blanks: obs.trailing_blanks,
            kind: obs.kind,
        }
    }

    /// The observation, when its tokens were kept.
    pub fn to_observation(&self) -> Option<AsrObservation> {
        let tokens = self.tokens.as_ref()?;
        Some(AsrObservation {
            through: self.through,
            tokens: tokens
                .iter()
                .map(|t| Token {
                    text: t.text.clone(),
                    emitted: t.emitted,
                })
                .collect(),
            trailing_blanks: self.trailing_blanks,
            kind: self.kind,
        })
    }
}

/// The grammar's answer about one text, as the runtime's [`GrammarTap`](crate::GrammarTap)
/// saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrammarAnswer {
    /// The text judged.
    pub text: String,
    /// The class.
    pub class: TextClass,
}

/// The sentence matcher's answer, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum RecognizerOutcome {
    /// A class.
    Class(TextClass),
    /// No class, and why.
    Error(RecognizerError),
}

impl From<Result<TextClass, RecognizerError>> for RecognizerOutcome {
    fn from(r: Result<TextClass, RecognizerError>) -> Self {
        match r {
            Ok(c) => Self::Class(c),
            Err(e) => Self::Error(e),
        }
    }
}

impl From<RecognizerOutcome> for Result<TextClass, RecognizerError> {
    fn from(r: RecognizerOutcome) -> Self {
        match r {
            RecognizerOutcome::Class(c) => Ok(c),
            RecognizerOutcome::Error(e) => Err(e),
        }
    }
}

/// The pause server's answer, as recorded. Its words are kept only under a transcripts
/// grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "value", rename_all = "snake_case")]
pub enum PauseServerOutcome {
    /// Its transcript.
    Words(Transcript),
    /// No words, and why.
    Failed(ServerFailure),
}

impl PauseServerOutcome {
    pub(crate) fn of(answer: &PauseServerAnswer, scope: &StreamScope) -> Self {
        match &answer.outcome {
            Ok(words) => Self::Words(Transcript::gate(words, scope)),
            Err(failure) => Self::Failed(failure.clone()),
        }
    }

    /// The answer for replay, when its words were kept.
    pub(crate) fn answer(&self, pause: Sample) -> Option<PauseServerAnswer> {
        let outcome = match self {
            Self::Words(words) => Ok(words.text()?.to_owned()),
            Self::Failed(failure) => Err(failure.clone()),
        };
        Some(PauseServerAnswer { pause, outcome })
    }
}

/// A control event routed to a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnControl {
    /// `TurnPolicy::mute`.
    Mute,
    /// `TurnPolicy::session_lost`.
    SessionLost,
    /// `TurnPolicy::audio_overrun`.
    AudioOverrun,
    /// `TurnPolicy::worker_error`.
    WorkerError,
    /// `TurnPolicy::discontinuity`.
    Discontinuity,
}

/// A recognizer answer the policy applied at a block boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedAnswer {
    /// The text it was about.
    pub text: Transcript,
    /// The frontier at which it took effect.
    pub at: Sample,
}

/// One decision, as recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DecisionRecord {
    /// `Decision::CommandStarted`.
    CommandStarted {
        /// The cell's start.
        at: Sample,
    },
    /// `Decision::AskRecognizer`: text sent to HA's sentence matcher.
    AskRecognizer {
        /// The text.
        text: Transcript,
    },
    /// `Decision::EndPending`.
    EndPending {
        /// The route.
        route: Route,
        /// The boundary.
        boundary: Sample,
        /// The frontier.
        since: Sample,
    },
    /// `Decision::EndRevoked`.
    EndRevoked {
        /// The boundary.
        boundary: Sample,
        /// The frontier.
        at: Sample,
    },
    /// `Decision::Commit`.
    Commit {
        /// The route.
        route: Route,
        /// The boundary.
        boundary: Sample,
        /// The committed span.
        span: SampleSpan,
        /// What set its end.
        end_bound: EndBound,
        /// The last command word's emission.
        last_word: Option<Sample>,
        /// The frontier.
        at: Sample,
        /// The stable streaming prefix.
        text: Transcript,
        /// Its class.
        class: Option<TextClass>,
        /// Whether a re-decode check follows.
        recheck: bool,
        /// Whose words the text is; `None` in recordings made before it was kept.
        #[serde(default)]
        source: Option<TextSource>,
    },
    /// `Decision::Close`.
    Close {
        /// Why.
        reason: CloseReason,
        /// The frontier.
        at: Option<Sample>,
    },
    /// `Decision::ReopenReply`.
    ReopenReply {
        /// The new start.
        start: Sample,
        /// Its trigger.
        trigger: Sample,
        /// The kept `no_input` sample.
        no_input_at: Sample,
    },
}

impl DecisionRecord {
    /// The record of `d`, keeping its text only under `scope`.
    pub fn of(d: &Decision, scope: &StreamScope) -> Self {
        match d {
            Decision::CommandStarted { at } => Self::CommandStarted { at: *at },
            Decision::AskRecognizer { text } => Self::AskRecognizer {
                text: Transcript::gate(text, scope),
            },
            Decision::EndPending {
                route,
                boundary,
                since,
            } => Self::EndPending {
                route: *route,
                boundary: *boundary,
                since: *since,
            },
            Decision::EndRevoked { boundary, at } => Self::EndRevoked {
                boundary: *boundary,
                at: *at,
            },
            Decision::Commit(Commit {
                route,
                boundary,
                span,
                end_bound,
                last_word,
                at,
                text,
                class,
                recheck,
                source,
            }) => Self::Commit {
                route: *route,
                boundary: *boundary,
                span: *span,
                end_bound: *end_bound,
                last_word: *last_word,
                at: *at,
                text: Transcript::gate(text, scope),
                class: *class,
                recheck: *recheck,
                source: Some(*source),
            },
            Decision::Close { reason, at } => Self::Close {
                reason: *reason,
                at: *at,
            },
            Decision::ReopenReply(ReplyReopen {
                start,
                trigger,
                no_input_at,
            }) => Self::ReopenReply {
                start: *start,
                trigger: *trigger,
                no_input_at: *no_input_at,
            },
        }
    }
}

/// One step of one turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum TurnStep {
    /// `TurnPolicy::new`.
    Opened {
        /// What opened it.
        cause: TurnCause,
        /// The configuration.
        config: Box<TurnConfig>,
        /// The spec.
        spec: TurnSpecRecord,
    },
    /// `EvidenceAssembler::push_device_cells`; kept only under a cells grant.
    DeviceCells {
        /// The epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// The inputs.
        cells: Vec<DeviceInputRecord>,
    },
    /// `push_device_cells` was called, but the grant did not cover loudness cells.
    DeviceCellsWithheld {
        /// The epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// How many cells.
        count: u64,
    },
    /// `EvidenceAssembler::push_vad`.
    Vad {
        /// The epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// The inputs.
        vad: Vec<VadRecord>,
    },
    /// `EvidenceAssembler::push_echo`.
    Echo {
        /// The epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// The inputs.
        cells: Vec<EchoCellRecord>,
    },
    /// `EvidenceAssembler::mark_gap`.
    Gap {
        /// The epoch.
        epoch: Epoch,
        /// The missing range.
        span: SampleSpan,
    },
    /// `EvidenceAssembler::push_asr`.
    Asr {
        /// The epoch.
        epoch: Epoch,
        /// The result.
        observation: AsrRecord,
    },
    /// `TurnPolicy::recognizer_answer`: an external answer handed to the policy here.
    RecognizerAnswer {
        /// The text it is about.
        text: Transcript,
        /// The answer.
        outcome: RecognizerOutcome,
    },
    /// `TurnPolicy::pause_server_asked`: the pause decode through `pause` went to the
    /// pause server here.
    PauseServerAsked {
        /// The pause decode's audio end.
        pause: Sample,
    },
    /// `TurnPolicy::pause_server_answer`: the pause server's answer handed to the policy
    /// here.
    PauseServerAnswer {
        /// The pause decode's audio end it answers.
        pause: Sample,
        /// Its words, or why there are none.
        outcome: PauseServerOutcome,
    },
    /// `TurnPolicy::advance`, with the grammar's answers it used and what it decided.
    Advanced {
        /// The grammar's answers (empty when transcripts were withheld).
        grammar: Vec<GrammarAnswer>,
        /// Recognizer answers that took effect during this call, and where.
        applied: Vec<AppliedAnswer>,
        /// Its decisions, in order.
        decisions: Vec<DecisionRecord>,
    },
    /// `TurnPolicy::on_timer`: a deadline became effective here.
    Timer {
        /// Its decisions.
        decisions: Vec<DecisionRecord>,
    },
    /// A control event.
    Control {
        /// Which.
        control: TurnControl,
        /// Its decisions.
        decisions: Vec<DecisionRecord>,
    },
}

/// One echo analyzer input or output. PCM is not repeated here: a mic or reference step names
/// the range, and replay reads those samples from the recorded frames.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum EchoStep {
    /// `EchoAnalyzer::new` and `open`.
    Opened {
        /// The policy.
        policy: EchoPolicy,
        /// The plan.
        plan: EchoPlan,
    },
    /// `EchoAnalyzer::reset`.
    Reset,
    /// `push_mic` of capture samples `[first, end)`.
    Mic {
        /// The capture epoch.
        epoch: Epoch,
        /// The first sample.
        first: Sample,
        /// The end, exclusive.
        end: Sample,
        /// The device time of the first sample.
        mono_ns: u64,
    },
    /// `push_mic_missing`.
    MicMissing {
        /// The capture epoch.
        epoch: Epoch,
        /// The missing range.
        span: SampleSpan,
    },
    /// `mark_gap`.
    Gap {
        /// The capture epoch.
        epoch: Epoch,
        /// The range.
        span: SampleSpan,
    },
    /// `push_reference` of reference samples `[first, end)`.
    Reference {
        /// The capture epoch.
        epoch: Epoch,
        /// The reference epoch.
        reference_epoch: Epoch,
        /// The first reference sample.
        first: u64,
        /// The end, exclusive.
        end: u64,
        /// The device time of the first sample.
        mono_ns: u64,
        /// The device's output coverage.
        coverage: OutputCoverage,
    },
    /// `push_reference_silence`.
    ReferenceSilence {
        /// The capture epoch.
        epoch: Epoch,
        /// The reference epoch.
        reference_epoch: Epoch,
        /// The first reference sample.
        first: u64,
        /// How many samples.
        frames: u64,
        /// The device time of the first sample.
        mono_ns: u64,
        /// The device's output coverage.
        coverage: OutputCoverage,
    },
    /// `push_reference_missing`.
    ReferenceMissing {
        /// The capture epoch.
        epoch: Epoch,
        /// The reference epoch.
        reference_epoch: Epoch,
        /// The first missing sample.
        first: u64,
        /// The end, exclusive.
        end: u64,
    },
    /// `reference_ended`.
    ReferenceEnded {
        /// The capture epoch.
        epoch: Epoch,
    },
    /// `push_device_cells`.
    DeviceCells {
        /// The capture epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// The inputs.
        cells: Vec<DeviceInputRecord>,
    },
    /// `push_device_cells` was called, but the grant did not cover loudness cells.
    DeviceCellsWithheld {
        /// The capture epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// How many cells.
        count: u64,
    },
    /// `push_vad`.
    Vad {
        /// The capture epoch.
        epoch: Epoch,
        /// The first cell.
        first: CellIndex,
        /// The inputs.
        vad: Vec<VadRecord>,
    },
    /// `start_labels`.
    LabelsStarted {
        /// The first cell.
        from: CellIndex,
    },
    /// `stop_labels`.
    LabelsStopped,
    /// `poll`, with the labels it produced.
    Polled {
        /// The labels.
        labels: Vec<LabelRecord>,
    },
    /// `compare_candidate`, with its result.
    Compared {
        /// The candidate's support.
        support: SampleSpan,
        /// The candidate it was for, when the runtime asked for one.
        candidate_id: Option<String>,
        /// The comparison's verdict, `None` when no plan was open.
        echo: Option<CandidateEcho>,
    },
}

/// One per-cell echo label, as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRecord {
    /// The cell.
    pub cell: CellIndex,
    /// The label.
    pub echo: EchoCellRecord,
}

/// The HA operations a device runtime performs (the controller's `HomeAssistant` service).
/// There is deliberately no variant for anything else: shell traffic and unrelated HTTP have
/// no way into a recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HaOperation {
    /// Resolving the satellite's pipeline.
    PipelineFor,
    /// Speech to text over a committed span.
    Stt,
    /// An intent run with its spoken answer.
    IntentTts,
    /// Text to speech alone.
    Tts,
    /// HA's sentence matcher.
    Recognize,
    /// Timer status.
    TimerStatus,
    /// A direct intent call.
    CallIntent,
}

/// An HA body, after credential redaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "body", rename_all = "snake_case")]
pub enum HaBody {
    /// The body with every credential replaced.
    Kept {
        /// The JSON.
        json: serde_json::Value,
        /// How many values were replaced.
        redactions: u32,
    },
    /// The grant did not cover transcripts, and HA bodies carry text.
    Withheld,
}

/// HA's failure, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "failure", rename_all = "snake_case")]
pub enum HaFailure {
    /// Never sent.
    NotDispatched,
    /// The connection was lost.
    ConnectionLost,
    /// HA refused, with this code (the message is not kept: it may echo request data).
    Rejected {
        /// HA's error code.
        code: String,
    },
    /// No answer in time.
    Timeout,
    /// An unreadable answer.
    BadResponse,
}

/// One step of one HA exchange.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum HaEntry {
    /// The request as sent.
    Request {
        /// The exchange.
        exchange: ExchangeId,
        /// The turn it serves, if any.
        turn: Option<u64>,
        /// The operation.
        operation: HaOperation,
        /// The body.
        body: HaBody,
    },
    /// HA's reply.
    Reply {
        /// The exchange.
        exchange: ExchangeId,
        /// Its body, or the failure.
        outcome: Result<HaBody, HaFailure>,
    },
}

impl HaEntry {
    /// How many credential values were replaced in this step's body.
    pub fn redactions(&self) -> u32 {
        let body = match self {
            Self::Request { body, .. } => Some(body),
            Self::Reply { outcome, .. } => outcome.as_ref().ok(),
        };
        match body {
            Some(HaBody::Kept { redactions, .. }) => *redactions,
            _ => 0,
        }
    }
}

/// One recorded EMA1 frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameEntry {
    /// The runtime's sequence number.
    pub seq: u64,
    /// When the runtime handed it over.
    pub t: RelTime,
    /// Where it was observed on the link.
    pub ingress: IngressOrdinal,
    /// The frame exactly as received: the 64-byte header and its payload.
    pub frame: bytes::Bytes,
}

/// The verifier's result for replay, when its text was kept.
pub(crate) fn verifier_result(record: &VerifierRecord) -> Option<VerifierResult> {
    match record {
        VerifierRecord::Text(Transcript::Kept(t)) => Some(VerifierResult::Text(t.clone())),
        VerifierRecord::Text(Transcript::Withheld) => None,
        VerifierRecord::Error => Some(VerifierResult::Error),
    }
}

impl VerifierRecord {
    pub(crate) fn of(result: &VerifierResult, scope: &StreamScope) -> Self {
        match result {
            VerifierResult::Text(t) => Self::Text(Transcript::gate(t, scope)),
            VerifierResult::Error => Self::Error,
        }
    }
}

/// A recognizer answer for replay, when its text was kept.
pub(crate) fn recognizer_answer(
    text: &Transcript,
    outcome: RecognizerOutcome,
) -> Option<RecognizerAnswer> {
    text.text().map(|t| RecognizerAnswer {
        text: t.to_owned(),
        outcome: outcome.into(),
    })
}
