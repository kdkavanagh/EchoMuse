//! Decision replay (§6.2 "Two forms of replay", the first form).
//!
//! The recorded evidence and external answers are fed to fresh `WakeDecider`s and
//! `TurnPolicy`s in exactly the recorded order, under a controlled clock that reads the
//! recording's relative times. Each policy's own deadlines are fired at their instants, in
//! order with the recorded entries, so a changed budget or backstop takes effect where it
//! would have. Recognizer answers are injected at their recorded points, and only when the
//! replayed policy asked the same text; a question the recording never asked is reported, never
//! answered.
//!
//! With [`EchoSource::Recompute`], the echo analyzers are driven too: their inputs are replayed
//! from the recorded steps, with PCM read from the recorded frames, and their labels and
//! candidate comparisons replace the recorded ones as the policies' evidence.
//!
//! The outcomes compared are the externally meaningful ones: whether each wake was accepted or
//! refused and why, and for each turn the committed span and text, the text sent to HA's
//! sentence matcher, and how it ended. Internal trace layout is not compared.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use echomuse_echo::{EchoAnalyzer, EchoPolicy};
use echomuse_protocol::ema::{Content, Kind, Packet};
use echomuse_turn::{
    CandidateEcho, CellIndex, CloseReason, Completeness, Decision, DeviceInput, EchoCell, Epoch,
    EvidenceError, MonoInstant, Route, SampleSpan, TextClass, TurnConfig, TurnPolicy, VadInput,
    WakeDecider, WakeOutcome, WakePolicy,
};
use serde::{Deserialize, Serialize};

use crate::grant::StreamScope;
use crate::reader::{Limitation, RecordedItem, Recording};
use crate::record::{
    AnalyzerId, AppliedAnswer, CancelTarget, DecisionRecord, EchoStep, Event, RecordingId, RelTime,
    Transcript, TurnCause, TurnControl, TurnStep, WakeEntry, recognizer_answer, verifier_result,
};

/// Where the echo evidence comes from.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum EchoSource {
    /// The labels and comparisons as recorded.
    #[default]
    Recorded,
    /// Recomputed by echo analyzers from the recorded frames, with this policy (the recorded
    /// one when `None`).
    Recompute {
        /// A policy to use instead of the recorded one.
        policy: Option<EchoPolicy>,
    },
}

/// Where the grammar's answers come from.
#[derive(Clone, Copy, Default)]
pub enum GrammarSource<'a> {
    /// The answers the recording holds; a text it never judged is `Unknown` and reported.
    #[default]
    Recorded,
    /// This grammar.
    Given(&'a dyn Completeness),
}

impl std::fmt::Debug for GrammarSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Recorded => f.write_str("Recorded"),
            Self::Given(_) => f.write_str("Given(..)"),
        }
    }
}

/// What to change for a replay. The default replays the recording exactly as recorded.
#[derive(Clone, Debug, Default)]
pub struct ReplayOptions<'a> {
    /// A turn configuration for every turn instead of the recorded ones.
    pub turn_config: Option<TurnConfig>,
    /// A wake policy for every candidate instead of the recorded ones.
    pub wake_policy: Option<WakePolicy>,
    /// The echo evidence.
    pub echo: EchoSource,
    /// The grammar.
    pub grammar: GrammarSource<'a>,
}

/// Why one candidate or turn could not be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotComparable {
    /// Its decision read text the grant withheld.
    TranscriptWithheld,
    /// Its decision read loudness cells the grant withheld.
    CellsWithheld,
}

/// One candidate's outcome, recorded and replayed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeComparison {
    /// The candidate.
    pub candidate_id: String,
    /// The recorded outcome, `None` if the recording holds no decision.
    pub recorded: Option<WakeOutcome>,
    /// The replayed outcome, `None` if the replay reached none.
    pub replayed: Option<WakeOutcome>,
    /// Why the two cannot be compared, when they cannot.
    pub not_comparable: Option<NotComparable>,
}

impl WakeComparison {
    /// Whether the outcomes agree.
    pub fn matches(&self) -> bool {
        self.recorded == self.replayed
    }
}

/// The audio a turn committed, and its text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedSpan {
    /// The route.
    pub route: Route,
    /// The committed audio.
    pub span: SampleSpan,
    /// The stable streaming text at the commit.
    pub text: Transcript,
}

/// How a turn ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "terminal", content = "reason", rename_all = "snake_case")]
pub enum Terminal {
    /// It committed.
    Committed,
    /// It closed without a commit.
    Closed(CloseReason),
    /// It handed over to a reopened reply window.
    Reopened,
    /// The runtime cancelled it before it ended.
    Cancelled,
    /// It had not ended when the recording did.
    Open,
}

/// A turn's externally meaningful outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnOutcome {
    /// The committed audio and text, if it committed.
    pub committed: Option<CommittedSpan>,
    /// The text sent to HA's sentence matcher, in order.
    pub asked_ha: Vec<Transcript>,
    /// How it ended.
    pub terminal: Terminal,
}

impl TurnOutcome {
    fn of(decisions: &[DecisionRecord], cancelled: bool) -> Self {
        let mut committed = None;
        let mut asked_ha = Vec::new();
        let mut terminal = None;
        for d in decisions {
            match d {
                DecisionRecord::AskRecognizer { text } => asked_ha.push(text.clone()),
                DecisionRecord::Commit {
                    route, span, text, ..
                } if terminal.is_none() => {
                    committed = Some(CommittedSpan {
                        route: *route,
                        span: *span,
                        text: text.clone(),
                    });
                    terminal = Some(Terminal::Committed);
                }
                DecisionRecord::Close { reason, .. } if terminal.is_none() => {
                    terminal = Some(Terminal::Closed(*reason));
                }
                DecisionRecord::ReopenReply { .. } if terminal.is_none() => {
                    terminal = Some(Terminal::Reopened);
                }
                _ => {}
            }
        }
        Self {
            committed,
            asked_ha,
            terminal: terminal.unwrap_or(if cancelled {
                Terminal::Cancelled
            } else {
                Terminal::Open
            }),
        }
    }
}

/// One turn's outcome, recorded and replayed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnComparison {
    /// The runtime's turn identity.
    pub turn: u64,
    /// What opened it.
    pub cause: TurnCause,
    /// The recorded outcome.
    pub recorded: TurnOutcome,
    /// The replayed outcome.
    pub replayed: TurnOutcome,
    /// Whether the replayed decisions would have opened this turn too (its wake accepted, or
    /// its preceding turn reopening a reply). Its inputs are replayed either way.
    pub opened_in_replay: bool,
    /// Where recorded recognizer answers took effect in the recording.
    pub recorded_applied: Vec<AppliedAnswer>,
    /// ...and in the replay.
    pub replayed_applied: Vec<AppliedAnswer>,
    /// Why the outcomes cannot be compared, when they cannot.
    pub not_comparable: Option<NotComparable>,
}

impl TurnComparison {
    /// Whether the outcomes agree.
    pub fn matches(&self) -> bool {
        self.recorded == self.replayed
    }
}

/// Why an echo analyzer could not be recomputed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum EchoUnavailable {
    /// Mic samples it was given are not all in the recording.
    MicMissing {
        /// The capture epoch.
        epoch: u64,
        /// The first sample asked for.
        first: u64,
        /// The end asked for.
        end: u64,
    },
    /// Reference samples it was given are not all in the recording.
    ReferenceMissing {
        /// The reference epoch.
        epoch: u64,
        /// The first sample asked for.
        first: u64,
        /// The end asked for.
        end: u64,
    },
    /// It was given loudness cells the grant withheld.
    CellsWithheld,
    /// Its steps begin before it was opened in the recording.
    NotOpened,
    /// It refused a recorded input.
    InputRefused,
}

/// One analyzer's recomputation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoComparison {
    /// The analyzer.
    pub analyzer: AnalyzerId,
    /// Labels recomputed and compared with the recorded ones.
    pub labels_compared: u64,
    /// ...that differed.
    pub labels_differing: u64,
    /// Recorded labels the recomputation did not produce.
    pub labels_missing: u64,
    /// Candidate comparisons recomputed.
    pub candidates_compared: u64,
    /// ...that differed.
    pub candidates_differing: u64,
    /// Why it stopped being recomputed, if it did.
    pub unavailable: Option<EchoUnavailable>,
}

/// Something the replay met that the recorded run did not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "divergence", rename_all = "snake_case")]
pub enum Divergence {
    /// The replayed policy asked HA's sentence matcher about text the recording never asked;
    /// no answer was invented for it.
    UnexpectedRequest {
        /// The turn.
        turn: u64,
        /// The text.
        text: String,
    },
    /// A recorded answer was not injected, because the replay never asked about its text.
    UnmatchedReply {
        /// The turn.
        turn: u64,
        /// The text.
        text: String,
    },
    /// The replayed policy judged text the recording holds no grammar answer for; it was
    /// treated as `unknown`.
    UnrecordedGrammarText {
        /// The turn.
        turn: u64,
        /// The text.
        text: String,
    },
    /// The replayed policy refused recorded evidence.
    EvidenceRefused {
        /// The turn.
        turn: u64,
        /// The entry.
        seq: u64,
        /// Why.
        error: EvidenceError,
    },
    /// The replayed policy could not be opened with the recorded spec.
    TurnRefused {
        /// The turn.
        turn: u64,
        /// Why.
        error: EvidenceError,
    },
    /// A step names a turn or candidate the recording never opened.
    Orphan {
        /// The entry.
        seq: u64,
    },
}

/// An externally meaningful difference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mismatch", rename_all = "snake_case")]
pub enum Mismatch {
    /// A wake decided differently.
    Wake {
        /// The candidate.
        candidate_id: String,
        /// Recorded.
        recorded: Option<WakeOutcome>,
        /// Replayed.
        replayed: Option<WakeOutcome>,
    },
    /// A turn ended differently.
    Turn {
        /// The turn.
        turn: u64,
        /// Recorded.
        recorded: Box<TurnOutcome>,
        /// Replayed.
        replayed: Box<TurnOutcome>,
    },
}

/// What a replay shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "equivalence", rename_all = "snake_case")]
pub enum Equivalence {
    /// Every outcome matched, and the recording is complete for decision replay.
    Proven,
    /// Every comparable outcome matched, but the recording is incomplete or redacted where
    /// decision replay reads it, or some outcomes could not be compared. This is not proof.
    Unproven {
        /// What prevents proof.
        limitations: Vec<Limitation>,
        /// Candidates and turns that could not be compared.
        not_comparable: u64,
    },
    /// Some outcome differed.
    Differs,
}

/// The result of a decision replay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplayReport {
    /// The recording.
    pub recording: RecordingId,
    /// Everything the recording and the replay lack.
    pub limitations: Vec<Limitation>,
    /// Every candidate, in arrival order.
    pub wakes: Vec<WakeComparison>,
    /// Every turn, in opening order.
    pub turns: Vec<TurnComparison>,
    /// Every recomputed echo analyzer.
    pub echo: Vec<EchoComparison>,
    /// What the replay met that the recorded run did not.
    pub divergences: Vec<Divergence>,
}

impl ReplayReport {
    /// The externally meaningful differences among comparable outcomes.
    pub fn mismatches(&self) -> Vec<Mismatch> {
        let wakes = self
            .wakes
            .iter()
            .filter(|w| w.not_comparable.is_none() && !w.matches())
            .map(|w| Mismatch::Wake {
                candidate_id: w.candidate_id.clone(),
                recorded: w.recorded,
                replayed: w.replayed,
            });
        let turns = self
            .turns
            .iter()
            .filter(|t| t.not_comparable.is_none() && !t.matches())
            .map(|t| Mismatch::Turn {
                turn: t.turn,
                recorded: Box::new(t.recorded.clone()),
                replayed: Box::new(t.replayed.clone()),
            });
        wakes.chain(turns).collect()
    }

    /// Whether the replay reproduced the recording, and whether that is proof.
    pub fn equivalence(&self) -> Equivalence {
        if !self.mismatches().is_empty() {
            return Equivalence::Differs;
        }
        let limitations: Vec<Limitation> = self
            .limitations
            .iter()
            .filter(|l| l.affects_decision_replay())
            .cloned()
            .collect();
        let not_comparable = self
            .wakes
            .iter()
            .filter(|w| w.not_comparable.is_some())
            .count()
            + self
                .turns
                .iter()
                .filter(|t| t.not_comparable.is_some())
                .count();
        if limitations.is_empty() && not_comparable == 0 {
            Equivalence::Proven
        } else {
            Equivalence::Unproven {
                limitations,
                not_comparable: not_comparable as u64,
            }
        }
    }
}

/// The recorded grammar answers, for replay.
struct RecordedGrammar {
    table: HashMap<String, TextClass>,
    unrecorded: RefCell<Vec<String>>,
}

impl Completeness for RecordedGrammar {
    fn classify(&self, text: &str) -> TextClass {
        match self.table.get(text) {
            Some(c) => *c,
            None => {
                self.unrecorded.borrow_mut().push(text.to_owned());
                TextClass::Unknown
            }
        }
    }
}

/// The recorded PCM of each stream epoch, for the echo analyzers.
#[derive(Default)]
struct FrameStore {
    streams: HashMap<(Kind, u64), Vec<(u64, Segment)>>,
}

enum Segment {
    Pcm(Bytes),
    Silence(u64),
}

impl Segment {
    fn len(&self) -> u64 {
        match self {
            Self::Pcm(b) => b.len() as u64 / 2,
            Self::Silence(n) => *n,
        }
    }

    fn sample(&self, i: u64) -> i16 {
        match self {
            Self::Pcm(b) => {
                let i = i as usize * 2;
                i16::from_le_bytes([b[i], b[i + 1]])
            }
            Self::Silence(_) => 0,
        }
    }
}

impl FrameStore {
    fn build(recording: &Recording) -> Self {
        let mut store = Self::default();
        for f in recording.frames() {
            let Ok(packet) = Packet::parse(f.frame.clone()) else {
                continue;
            };
            let h = packet.header();
            let seg = match (h.kind, packet.content()) {
                (Kind::Mic | Kind::Reference, Content::DigitalSilence { frames }) => {
                    Segment::Silence(u64::from(frames))
                }
                (Kind::Mic | Kind::Reference, Content::Pcm(_)) => {
                    Segment::Pcm(packet.payload().clone())
                }
                _ => continue,
            };
            store
                .streams
                .entry((h.kind, h.epoch))
                .or_default()
                .push((h.first_sample, seg));
        }
        store
    }

    /// Samples `[first, end)` of a stream epoch, or `None` unless every one was recorded. Where
    /// frames overlap, the first recorded wins.
    fn pcm(&self, kind: Kind, epoch: u64, first: u64, end: u64) -> Option<Vec<i16>> {
        let len = usize::try_from(end.checked_sub(first)?).ok()?;
        let mut out: Vec<Option<i16>> = vec![None; len];
        for (start, seg) in self.streams.get(&(kind, epoch)).into_iter().flatten() {
            let seg_end = start + seg.len();
            let from = first.max(*start);
            let to = end.min(seg_end);
            for pos in from..to {
                let slot = &mut out[(pos - first) as usize];
                if slot.is_none() {
                    *slot = Some(seg.sample(pos - start));
                }
            }
        }
        out.into_iter().collect()
    }
}

struct WakeRun {
    candidate_id: String,
    decider: Option<WakeDecider>,
    recorded: Option<WakeOutcome>,
    not_comparable: Option<NotComparable>,
}

struct TurnRun {
    turn: u64,
    cause: TurnCause,
    policy: Option<TurnPolicy>,
    recorded: Vec<DecisionRecord>,
    replayed: Vec<DecisionRecord>,
    recorded_asks: Vec<Transcript>,
    replay_asked: Vec<String>,
    recorded_applied: Vec<AppliedAnswer>,
    cancelled: bool,
    opened_in_replay: bool,
    not_comparable: Option<NotComparable>,
}

impl TurnRun {
    fn absorb(&mut self, out: &mut Vec<Decision>) {
        for d in out.drain(..) {
            if let Decision::AskRecognizer { text } = &d {
                self.replay_asked.push(text.clone());
            }
            self.replayed
                .push(DecisionRecord::of(&d, &StreamScope::ALL));
        }
    }
}

struct EchoRun {
    analyzer: Option<EchoAnalyzer>,
    report: EchoComparison,
}

struct Replay<'r, 'o> {
    recording: &'r Recording,
    options: &'o ReplayOptions<'o>,
    recorded_grammar: RecordedGrammar,
    frames: Option<FrameStore>,
    wakes: Vec<WakeRun>,
    wake_index: HashMap<String, usize>,
    turns: BTreeMap<u64, TurnRun>,
    turn_order: Vec<u64>,
    echo: BTreeMap<AnalyzerId, EchoRun>,
    labels: HashMap<(u64, u64), EchoCell>,
    candidate_echo: HashMap<String, CandidateEcho>,
    echo_fallbacks: u64,
    divergences: Vec<Divergence>,
}

/// Replays a recording's decisions.
pub fn replay(recording: &Recording, options: &ReplayOptions<'_>) -> ReplayReport {
    let mut table = HashMap::new();
    for e in recording.events() {
        if let Event::Turn {
            step: TurnStep::Advanced { grammar, .. },
            ..
        } = &e.event
        {
            for a in grammar {
                table.insert(a.text.clone(), a.class);
            }
        }
    }
    let recompute = matches!(options.echo, EchoSource::Recompute { .. });
    let mut r = Replay {
        recording,
        options,
        recorded_grammar: RecordedGrammar {
            table,
            unrecorded: RefCell::new(Vec::new()),
        },
        frames: recompute.then(|| FrameStore::build(recording)),
        wakes: Vec::new(),
        wake_index: HashMap::new(),
        turns: BTreeMap::new(),
        turn_order: Vec::new(),
        echo: BTreeMap::new(),
        labels: HashMap::new(),
        candidate_echo: HashMap::new(),
        echo_fallbacks: 0,
        divergences: Vec::new(),
    };
    r.run();
    r.report()
}

impl Replay<'_, '_> {
    fn run(&mut self) {
        let mut last = RelTime(0);
        for item in &self.recording.items {
            let RecordedItem::Event(entry) = item else {
                continue;
            };
            self.fire_due(entry.t.instant());
            last = last.max(entry.t);
            self.apply(entry.seq, entry.t.instant(), &entry.event);
        }
        let end = self.recording.end.as_ref().map_or(last, |e| e.t.max(last));
        self.fire_due(end.instant());
    }

    /// Fires every replayed deadline at or before `now`, at its own instant.
    fn fire_due(&mut self, now: MonoInstant) {
        for w in &mut self.wakes {
            if let Some(d) = w.decider.as_mut()
                && let Some(deadline) = d.next_deadline()
                && deadline <= now
            {
                d.on_timer(deadline);
            }
        }
        for t in self.turns.values_mut() {
            let mut out = Vec::new();
            while let Some(p) = t.policy.as_mut() {
                let Some(deadline) = p.next_deadline().filter(|d| *d <= now) else {
                    break;
                };
                p.on_timer(deadline, &mut out);
                let again = p.next_deadline();
                t.absorb(&mut out);
                if again == Some(deadline) {
                    break;
                }
            }
        }
    }

    fn apply(&mut self, seq: u64, now: MonoInstant, event: &Event) {
        match event {
            Event::Wake(w) => self.wake(seq, now, w),
            Event::Turn { turn, step } => self.turn(seq, now, *turn, step),
            Event::Echo { analyzer, step } => self.echo_step(*analyzer, step),
            Event::Cancelled(CancelTarget::Turn { turn }) => {
                if let Some(t) = self.turns.get_mut(turn) {
                    t.cancelled = true;
                }
            }
            Event::Revisions(_)
            | Event::StreamOpened { .. }
            | Event::StreamEnded { .. }
            | Event::Privacy { .. }
            | Event::AudioWithheld { .. }
            | Event::Cancelled(_)
            | Event::Disconnected { .. }
            | Event::Ha(_) => {}
        }
    }

    fn wake(&mut self, seq: u64, now: MonoInstant, entry: &WakeEntry) {
        let id = entry.candidate_id();
        if let WakeEntry::Arrived {
            candidate,
            conditions,
            policy,
            decision,
        } = entry
        {
            let policy = self
                .options
                .wake_policy
                .clone()
                .unwrap_or_else(|| policy.clone());
            let decider = WakeDecider::arrive(candidate.clone(), (*conditions).into(), policy, now);
            self.wake_index.insert(id.to_owned(), self.wakes.len());
            self.wakes.push(WakeRun {
                candidate_id: id.to_owned(),
                decider: Some(decider),
                recorded: decision.map(|d| d.outcome),
                not_comparable: None,
            });
            return;
        }
        let Some(&i) = self.wake_index.get(id) else {
            self.divergences.push(Divergence::Orphan { seq });
            return;
        };
        let substitute = match entry {
            WakeEntry::Echo { candidate_id, .. } if self.frames.is_some() => {
                let found = self.candidate_echo.get(candidate_id).copied();
                if found.is_none() {
                    self.echo_fallbacks += 1;
                }
                found
            }
            _ => None,
        };
        let run = &mut self.wakes[i];
        if run.recorded.is_none() {
            run.recorded = entry.decision().map(|d| d.outcome);
        }
        let Some(d) = run.decider.as_mut() else {
            return;
        };
        match entry {
            WakeEntry::Arrived { .. } => {}
            WakeEntry::Reference { score, .. } => {
                d.reference_scored(*score, now);
            }
            WakeEntry::Echo { echo, .. } => {
                d.echo_compared(substitute.unwrap_or(*echo), now);
            }
            WakeEntry::Verified { result, .. } => match verifier_result(result) {
                Some(v) => {
                    d.verified(v, now);
                }
                None => run.not_comparable = Some(NotComparable::TranscriptWithheld),
            },
            WakeEntry::Timer { .. } => {
                d.on_timer(now);
            }
        }
    }

    fn turn_opened_in_replay(&self, cause: &TurnCause) -> bool {
        match cause {
            TurnCause::Wake { candidate_id } => self
                .wake_index
                .get(candidate_id)
                .and_then(|&i| self.wakes[i].decider.as_ref())
                .and_then(WakeDecider::decision)
                .is_some_and(|d| d.outcome == WakeOutcome::Accepted),
            TurnCause::Button
            | TurnCause::Reply {
                reopened_from: None,
            } => true,
            TurnCause::Reply {
                reopened_from: Some(prev),
            } => self.turns.get(prev).is_some_and(|t| {
                t.replayed
                    .iter()
                    .any(|d| matches!(d, DecisionRecord::ReopenReply { .. }))
            }),
        }
    }

    fn turn(&mut self, seq: u64, now: MonoInstant, turn: u64, step: &TurnStep) {
        if let TurnStep::Opened {
            cause,
            config,
            spec,
        } = step
        {
            let opened_in_replay = self.turn_opened_in_replay(cause);
            let config = self.options.turn_config.unwrap_or(**config);
            let policy = match TurnPolicy::new(config, spec.to_spec(), now) {
                Ok(p) => Some(p),
                Err(error) => {
                    self.divergences
                        .push(Divergence::TurnRefused { turn, error });
                    None
                }
            };
            self.turn_order.push(turn);
            self.turns.insert(
                turn,
                TurnRun {
                    turn,
                    cause: cause.clone(),
                    policy,
                    recorded: Vec::new(),
                    replayed: Vec::new(),
                    recorded_asks: Vec::new(),
                    replay_asked: Vec::new(),
                    recorded_applied: Vec::new(),
                    cancelled: false,
                    opened_in_replay,
                    not_comparable: None,
                },
            );
            return;
        }
        let substitute = match step {
            TurnStep::Echo {
                epoch,
                first,
                cells,
            } if self.frames.is_some() => Some(self.substitute_labels(*epoch, *first, cells)),
            _ => None,
        };
        let grammar: &dyn Completeness = match self.options.grammar {
            GrammarSource::Recorded => &self.recorded_grammar,
            GrammarSource::Given(g) => g,
        };
        let Some(run) = self.turns.get_mut(&turn) else {
            self.divergences.push(Divergence::Orphan { seq });
            return;
        };
        let mut refused = None;
        let mut out = Vec::new();
        match step {
            TurnStep::Opened { .. } => {}
            TurnStep::DeviceCells {
                epoch,
                first,
                cells,
            } => {
                let inputs: Result<Vec<DeviceInput>, _> =
                    cells.iter().map(|c| c.to_input()).collect();
                if let Some(p) = run.policy.as_mut() {
                    refused = inputs
                        .and_then(|i| p.evidence_mut().push_device_cells(*epoch, *first, &i))
                        .err();
                }
            }
            TurnStep::DeviceCellsWithheld { .. } => {
                run.not_comparable = Some(NotComparable::CellsWithheld);
            }
            TurnStep::Vad { epoch, first, vad } => {
                let inputs: Result<Vec<VadInput>, _> = vad.iter().map(|v| v.to_input()).collect();
                if let Some(p) = run.policy.as_mut() {
                    refused = inputs
                        .and_then(|i| p.evidence_mut().push_vad(*epoch, *first, &i))
                        .err();
                }
            }
            TurnStep::Echo {
                epoch,
                first,
                cells,
            } => {
                let cells: Vec<EchoCell> =
                    substitute.unwrap_or_else(|| cells.iter().map(|&c| c.into()).collect());
                if let Some(p) = run.policy.as_mut() {
                    refused = p.evidence_mut().push_echo(*epoch, *first, &cells).err();
                }
            }
            TurnStep::Gap { epoch, span } => {
                if let Some(p) = run.policy.as_mut() {
                    refused = p.evidence_mut().mark_gap(*epoch, *span).err();
                }
            }
            TurnStep::Asr { epoch, observation } => match observation.to_observation() {
                Some(obs) => {
                    if let Some(p) = run.policy.as_mut() {
                        refused = p.evidence_mut().push_asr(*epoch, obs).err();
                    }
                }
                None => run.not_comparable = Some(NotComparable::TranscriptWithheld),
            },
            TurnStep::RecognizerAnswer { text, outcome } => {
                match recognizer_answer(text, *outcome) {
                    Some(answer) if run.replay_asked.contains(&answer.text) => {
                        if let Some(p) = run.policy.as_mut() {
                            p.recognizer_answer(answer, now);
                        }
                    }
                    Some(answer) => self.divergences.push(Divergence::UnmatchedReply {
                        turn,
                        text: answer.text,
                    }),
                    None => run.not_comparable = Some(NotComparable::TranscriptWithheld),
                }
            }
            TurnStep::PauseServerAsked { pause } => {
                if let Some(p) = run.policy.as_mut() {
                    p.pause_server_asked(*pause, now);
                }
            }
            TurnStep::PauseServerAnswer { pause, outcome } => match outcome.answer(*pause) {
                Some(answer) => {
                    if let Some(p) = run.policy.as_mut() {
                        p.pause_server_answer(answer, now);
                    }
                }
                None => run.not_comparable = Some(NotComparable::TranscriptWithheld),
            },
            TurnStep::Advanced {
                applied, decisions, ..
            } => {
                run.recorded_applied.extend(applied.iter().cloned());
                record_decisions(run, decisions);
                if let Some(p) = run.policy.as_mut() {
                    p.advance(grammar, now, &mut out);
                }
            }
            TurnStep::Timer { decisions } => {
                record_decisions(run, decisions);
                if let Some(p) = run.policy.as_mut() {
                    p.on_timer(now, &mut out);
                }
            }
            TurnStep::Control { control, decisions } => {
                record_decisions(run, decisions);
                if let Some(p) = run.policy.as_mut() {
                    match control {
                        TurnControl::Mute => p.mute(&mut out),
                        TurnControl::SessionLost => p.session_lost(&mut out),
                        TurnControl::AudioOverrun => p.audio_overrun(&mut out),
                        TurnControl::WorkerError => p.worker_error(&mut out),
                        TurnControl::Discontinuity => p.discontinuity(&mut out),
                    }
                }
            }
        }
        run.absorb(&mut out);
        if let Some(error) = refused {
            self.divergences
                .push(Divergence::EvidenceRefused { turn, seq, error });
        }
        for text in self.recorded_grammar.unrecorded.borrow_mut().drain(..) {
            self.divergences
                .push(Divergence::UnrecordedGrammarText { turn, text });
        }
    }

    fn substitute_labels(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[crate::record::EchoCellRecord],
    ) -> Vec<EchoCell> {
        cells
            .iter()
            .enumerate()
            .map(
                |(i, &recorded)| match self.labels.get(&(epoch.get(), first.0 + i as u64)) {
                    Some(c) => *c,
                    None => {
                        self.echo_fallbacks += 1;
                        recorded.into()
                    }
                },
            )
            .collect()
    }

    fn echo_step(&mut self, id: AnalyzerId, step: &EchoStep) {
        let Some(frames) = self.frames.as_ref() else {
            return;
        };
        let EchoSource::Recompute {
            policy: override_policy,
        } = &self.options.echo
        else {
            return;
        };
        if let EchoStep::Opened { policy, plan } = step {
            let mut analyzer =
                EchoAnalyzer::new(override_policy.clone().unwrap_or_else(|| policy.clone()));
            let run = self.echo.entry(id).or_insert_with(|| EchoRun {
                analyzer: None,
                report: EchoComparison {
                    analyzer: id,
                    labels_compared: 0,
                    labels_differing: 0,
                    labels_missing: 0,
                    candidates_compared: 0,
                    candidates_differing: 0,
                    unavailable: None,
                },
            });
            if run.report.unavailable.is_none() {
                if analyzer.open(*plan).is_ok() {
                    run.analyzer = Some(analyzer);
                } else {
                    run.report.unavailable = Some(EchoUnavailable::InputRefused);
                }
            }
            return;
        }
        let run = self.echo.entry(id).or_insert_with(|| EchoRun {
            analyzer: None,
            report: EchoComparison {
                analyzer: id,
                labels_compared: 0,
                labels_differing: 0,
                labels_missing: 0,
                candidates_compared: 0,
                candidates_differing: 0,
                unavailable: Some(EchoUnavailable::NotOpened),
            },
        });
        let Some(a) = run.analyzer.as_mut() else {
            if let EchoStep::Polled { labels } = step {
                run.report.labels_missing += labels.len() as u64;
            }
            return;
        };
        let result: Result<(), EchoUnavailable> = match step {
            EchoStep::Opened { .. } => Ok(()),
            EchoStep::Reset => {
                a.reset();
                Ok(())
            }
            EchoStep::Mic {
                epoch,
                first,
                end,
                mono_ns,
            } => match frames.pcm(Kind::Mic, epoch.get(), first.0, end.0) {
                Some(pcm) => a
                    .push_mic(*epoch, *first, *mono_ns, pcm)
                    .map_err(|_| EchoUnavailable::InputRefused),
                None => Err(EchoUnavailable::MicMissing {
                    epoch: epoch.get(),
                    first: first.0,
                    end: end.0,
                }),
            },
            EchoStep::MicMissing { epoch, span } => a
                .push_mic_missing(*epoch, *span)
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::Gap { epoch, span } => a
                .mark_gap(*epoch, *span)
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::Reference {
                epoch,
                reference_epoch,
                first,
                end,
                mono_ns,
                coverage,
            } => match frames.pcm(Kind::Reference, reference_epoch.get(), *first, *end) {
                Some(pcm) => a
                    .push_reference(*epoch, *reference_epoch, *first, *mono_ns, *coverage, pcm)
                    .map_err(|_| EchoUnavailable::InputRefused),
                None => Err(EchoUnavailable::ReferenceMissing {
                    epoch: reference_epoch.get(),
                    first: *first,
                    end: *end,
                }),
            },
            EchoStep::ReferenceSilence {
                epoch,
                reference_epoch,
                first,
                frames,
                mono_ns,
                coverage,
            } => a
                .push_reference_silence(
                    *epoch,
                    *reference_epoch,
                    *first,
                    *frames,
                    *mono_ns,
                    *coverage,
                )
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::ReferenceMissing {
                epoch,
                reference_epoch,
                first,
                end,
            } => a
                .push_reference_missing(*epoch, *reference_epoch, *first, *end)
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::ReferenceEnded { epoch } => a
                .reference_ended(*epoch)
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::DeviceCells {
                epoch,
                first,
                cells,
            } => cells
                .iter()
                .map(|c| c.to_input())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| EchoUnavailable::InputRefused)
                .and_then(|i| {
                    a.push_device_cells(*epoch, *first, &i)
                        .map_err(|_| EchoUnavailable::InputRefused)
                }),
            EchoStep::DeviceCellsWithheld { .. } => Err(EchoUnavailable::CellsWithheld),
            EchoStep::Vad { epoch, first, vad } => vad
                .iter()
                .map(|v| v.to_input())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| EchoUnavailable::InputRefused)
                .and_then(|i| {
                    a.push_vad(*epoch, *first, &i)
                        .map_err(|_| EchoUnavailable::InputRefused)
                }),
            EchoStep::LabelsStarted { from } => a
                .start_labels(*from)
                .map_err(|_| EchoUnavailable::InputRefused),
            EchoStep::LabelsStopped => {
                a.stop_labels();
                Ok(())
            }
            EchoStep::Polled { labels } => {
                let epoch = a.plan().map(|p| p.epoch.get());
                let mut fresh = Vec::new();
                a.poll(&mut fresh);
                let fresh: HashMap<u64, EchoCell> =
                    fresh.into_iter().map(|l| (l.cell.0, l.echo)).collect();
                for l in labels {
                    match fresh.get(&l.cell.0) {
                        Some(c) => {
                            run.report.labels_compared += 1;
                            if crate::record::EchoCellRecord::from(*c) != l.echo {
                                run.report.labels_differing += 1;
                            }
                        }
                        None => run.report.labels_missing += 1,
                    }
                }
                if let Some(epoch) = epoch {
                    for (cell, c) in fresh {
                        self.labels.insert((epoch, cell), c);
                    }
                }
                Ok(())
            }
            EchoStep::Compared {
                support,
                candidate_id,
                echo,
            } => {
                let fresh = a.compare_candidate(*support).map(|c| c.echo);
                run.report.candidates_compared += 1;
                if fresh != *echo {
                    run.report.candidates_differing += 1;
                }
                if let (Some(id), Some(e)) = (candidate_id, fresh) {
                    self.candidate_echo.insert(id.clone(), e);
                }
                Ok(())
            }
        };
        if let Err(why) = result {
            run.report.unavailable = Some(why);
            run.analyzer = None;
        }
    }

    fn report(mut self) -> ReplayReport {
        let mut limitations = self.recording.integrity.limitations();
        if self.echo_fallbacks > 0 {
            limitations.push(Limitation::EchoNotRecomputed {
                results: self.echo_fallbacks,
            });
        }
        let wakes = self
            .wakes
            .iter()
            .map(|w| WakeComparison {
                candidate_id: w.candidate_id.clone(),
                recorded: w.recorded,
                replayed: w
                    .decider
                    .as_ref()
                    .and_then(WakeDecider::decision)
                    .map(|d| d.outcome),
                not_comparable: w.not_comparable,
            })
            .collect();
        let mut turns = Vec::new();
        for id in &self.turn_order {
            let Some(t) = self.turns.get(id) else {
                continue;
            };
            for text in &t.replay_asked {
                let asked = Transcript::Kept(text.clone());
                if !t.recorded_asks.contains(&asked) {
                    self.divergences.push(Divergence::UnexpectedRequest {
                        turn: t.turn,
                        text: text.clone(),
                    });
                }
            }
            let replayed_applied = t
                .policy
                .as_ref()
                .map(|p| {
                    p.trace()
                        .recognizer
                        .iter()
                        .filter_map(|r| {
                            r.applied_at.map(|at| AppliedAnswer {
                                text: Transcript::Kept(r.text.clone()),
                                at,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            turns.push(TurnComparison {
                turn: t.turn,
                cause: t.cause.clone(),
                recorded: TurnOutcome::of(&t.recorded, t.cancelled),
                replayed: TurnOutcome::of(&t.replayed, t.cancelled),
                opened_in_replay: t.opened_in_replay,
                recorded_applied: t.recorded_applied.clone(),
                replayed_applied,
                not_comparable: t.not_comparable,
            });
        }
        ReplayReport {
            recording: self.recording.header.recording.clone(),
            limitations,
            wakes,
            turns,
            echo: self.echo.into_values().map(|e| e.report).collect(),
            divergences: self.divergences,
        }
    }
}

fn record_decisions(run: &mut TurnRun, decisions: &[DecisionRecord]) {
    for d in decisions {
        if let DecisionRecord::AskRecognizer { text } = d {
            run.recorded_asks.push(text.clone());
        }
    }
    run.recorded.extend(decisions.iter().cloned());
}
