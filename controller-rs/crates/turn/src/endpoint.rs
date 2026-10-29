//! The endpoint reducer: deciding when the user has finished speaking (§4.5, §4.6; post-AFE
//! architecture §8.3 and §16.6 "Endpoint reducer").
//!
//! A [`TurnPolicy`] owns one utterance. The device runtime feeds its evidence into the
//! [`EvidenceAssembler`] it exposes, in whatever order the workers finish, and calls
//! [`TurnPolicy::advance`]. Every cell released in sample order is classified; every block
//! boundary is evaluated against the routes and limits below; every decision those produce is
//! appended to the caller's list, in order, until the utterance ends. Ages and pauses are
//! measured in sample time. The controller's monotonic clock bounds only what sample time
//! cannot: evidence that stops arriving, a reply window nobody answers, and a late answer from
//! HA's sentence matcher.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::asr::{TextTracker, WakeCut};
use crate::classify::{AttributionPolicy, CellClass, CellClassifier, ReplyAttribution};
use crate::completeness::{Completeness, RecognizerAnswer, RecognizerError, TextClass};
use crate::evidence::{
    AsrKind, AsrObservation, CellAudio, CellEvidence, EchoLabel, EvidenceAssembler, EvidenceError,
    EvidencePlan, ReferenceCoverage, Released,
};
use crate::level::{BackgroundLevel, ForegroundLevel};
use crate::pause::{PauseError, PauseRecord, PauseServerAnswer, PauseWords, TextSource};
use crate::time::{CELL_SAMPLES, CellIndex, Epoch, MonoInstant, Sample, SampleSpan, Samples};
use crate::trace::{
    EndpointEvent, EvidenceCounts, RecognizerRecord, RouteEvidence, TextChange, TraceKind,
    UtteranceTrace,
};

/// The endpoint values of policy `post_afe_10` (§4.6, §16.6). Values marked *tuned* in §4 came
/// from field tuning.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EndpointPolicy {
    /// Pause after `complete` text (tuned: 608 ms).
    pub pause_complete: Samples,
    /// Pause after `extendable` text (tuned: 1,216 ms).
    pub pause_extendable: Samples,
    /// Pause after `needs_more` or `unknown` text (tuned: 1,792 ms).
    pub pause_needs_more: Samples,
    /// Lookahead before a pending end commits (tuned: 192 ms).
    pub lookahead: Samples,
    /// Command-speech cells after a pending end's decision that revoke it.
    pub revoke_command_cells: u32,
    /// Command speech route A needs (192 ms).
    pub min_command: Samples,
    /// Route A votes over this many of the latest cells.
    pub vote_cells: u32,
    /// How many of them must be quiet.
    pub vote_quiet_min: u32,
    /// Decoder blank frames route A needs after the last word (10 frames, 400 ms).
    pub trailing_blanks_min: u32,
    /// No command speech this long after the trigger is `no_input` (5 s).
    pub no_input: Samples,
    /// The same for a reply window (7 s).
    pub reply_no_input: Samples,
    /// The longest utterance (tuned: 15 s).
    pub max_utterance: Samples,
    /// The longest utterance with `extendedUtterances` (30 s).
    pub max_extended_utterance: Samples,
    /// The no-progress fallback (3 s).
    pub no_progress: Samples,
    /// Route R's pause for replies to HA-started conversations (1,024 ms).
    pub ha_reply_pause: Samples,
    /// Route B needs at least this percentage of speech-positive cells after the prefix to be
    /// background speech.
    pub background_share_percent: u32,
    /// A reply's committed span starts this far before its answer's run.
    pub reply_span_lead: Samples,
    /// A reply's guard after its trigger (320 ms).
    pub reply_guard: Samples,
    /// The audio kept after the streaming recognizer's emission of the last command word when
    /// that ends the committed audio later than the cell bound.
    pub asr_word_margin: Samples,
    /// Monotonic backstop: an open utterance whose evidence has not advanced for this long is
    /// closed `interrupted`, in milliseconds.
    pub evidence_stall_ms: u64,
    /// Monotonic bound on a reply window without command speech, in milliseconds (10 s).
    pub reply_wall_ms: u64,
    /// HA's sentence matcher must answer within this, in milliseconds (2 s).
    pub recognizer_limit_ms: u64,
}

/// The margin after the last command word's emission, in milliseconds. Measured on 190 saved
/// wake turns whose last command word had no other token within 1 s after it: the word's audio
/// (cells at least 6 dB over the room) ended a median 136 ms *before* Kroko emitted it, and
/// 90% ended no more than 56 ms after. This margin covers those, and keeps the median audio
/// after the word's end near the cell bound's own 192 ms tail.
pub const ASR_WORD_MARGIN_MS: u64 = 64;

impl Default for EndpointPolicy {
    fn default() -> Self {
        Self {
            pause_complete: Samples::from_ms(608),
            pause_extendable: Samples::from_ms(1_216),
            pause_needs_more: Samples::from_ms(1_792),
            lookahead: Samples::from_ms(192),
            revoke_command_cells: 4,
            min_command: Samples::from_ms(192),
            vote_cells: 19,
            vote_quiet_min: 15,
            trailing_blanks_min: 10,
            no_input: Samples::from_ms(5_000),
            reply_no_input: Samples::from_ms(7_000),
            max_utterance: Samples::from_ms(15_000),
            max_extended_utterance: Samples::from_ms(30_000),
            no_progress: Samples::from_ms(3_000),
            ha_reply_pause: Samples::from_ms(1_024),
            background_share_percent: 80,
            reply_span_lead: Samples::from_ms(300),
            reply_guard: Samples::from_ms(320),
            asr_word_margin: Samples::from_ms(ASR_WORD_MARGIN_MS),
            evidence_stall_ms: 3_000,
            reply_wall_ms: 10_000,
            recognizer_limit_ms: 2_000,
        }
    }
}

impl EndpointPolicy {
    fn pause_for(&self, class: TextClass) -> Samples {
        match class {
            TextClass::Complete => self.pause_complete,
            TextClass::Extendable => self.pause_extendable,
            TextClass::NeedsMore | TextClass::Unknown => self.pause_needs_more,
        }
    }
}

/// Everything one utterance's policy is configured with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnConfig {
    /// Endpoint values.
    pub endpoint: EndpointPolicy,
    /// Attribution values.
    pub attribution: AttributionPolicy,
    /// `extendedUtterances`: allow 30 s instead of 15 s.
    pub extended_utterances: bool,
    /// Whether HA's sentence matcher may be asked about text the grammar cannot judge.
    pub recognizer_available: bool,
    /// Whether a pause server (`pauseAsr`) also transcribes each pause decode's audio: each
    /// pause then records its words, and they may stand in for Kroko's (§4.7).
    #[serde(default)]
    pub pause_server: bool,
}

/// Where a wake turn's wake word sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeAnchor {
    /// The candidate's `support_start`.
    pub support_start: Sample,
    /// The candidate's open: the end of its last hop.
    pub candidate_open: Sample,
    /// The registry's wake-phrase spelling (`ophelia`).
    pub phrase: String,
}

/// Who asked the question a reply answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyOrigin {
    /// The controller's own question or HA's `continue_conversation`: routes A and B.
    Expectation,
    /// An HA-started conversation on the ESPHome path: route R replaces A and B.
    HaConversation,
}

/// A reply window's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplySpec {
    /// Who asked.
    pub origin: ReplyOrigin,
    /// The opening guard's end, for a window opened at a drain.
    pub guard_until: Option<Sample>,
    /// Without command speech by this sample the reply closes `no_input`.
    pub no_input_at: Sample,
    /// Without command speech by this instant the reply closes `no_input` regardless of audio.
    pub wall_deadline: MonoInstant,
}

impl ReplySpec {
    /// A window opened at the question's guarded drain (or after the chime that follows it):
    /// a 320 ms opening guard, `no_input` 7 s of audio after the trigger, and 10 s of the
    /// controller's clock from `opened_at`.
    pub fn at_drain(
        origin: ReplyOrigin,
        trigger: Sample,
        opened_at: MonoInstant,
        policy: &EndpointPolicy,
    ) -> Self {
        Self {
            origin,
            guard_until: Some(trigger + policy.reply_guard),
            no_input_at: trigger + policy.reply_no_input,
            wall_deadline: opened_at.after(Duration::from_millis(policy.reply_wall_ms)),
        }
    }

    /// A window opened by an early answer at `onset`: no guard.
    pub fn at_early_answer(
        origin: ReplyOrigin,
        onset: Sample,
        opened_at: MonoInstant,
        policy: &EndpointPolicy,
    ) -> Self {
        Self {
            origin,
            guard_until: None,
            no_input_at: onset + policy.reply_no_input,
            wall_deadline: opened_at.after(Duration::from_millis(policy.reply_wall_ms)),
        }
    }
}

/// What opened the turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnKind {
    /// An accepted wake word.
    Wake(WakeAnchor),
    /// The action button.
    Button,
    /// A reply window.
    Reply(ReplySpec),
}

/// One utterance to judge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnSpec {
    /// The capture epoch.
    pub epoch: Epoch,
    /// The utterance start: where the ASR stream and the committed span begin.
    pub start: Sample,
    /// The trigger sample: the wake's last qualifying hop end, the press, or where the reply
    /// window opened.
    pub trigger: Sample,
    /// The first cell of device loudness to read, for the background estimate.
    pub cells_from: CellIndex,
    /// The subscription's first VAD cell.
    pub vad_from: CellIndex,
    /// What opened the turn.
    pub kind: TurnKind,
}

impl TurnSpec {
    /// The mic stream's start for a wake candidate, `round_down(support_start − 4800, 512)`
    /// (§3.1.6): 300 ms of pre-roll before the wake word's support.
    pub fn candidate_mic_start(support_start: Sample) -> Sample {
        (support_start - Samples(4_800)).round_down(CELL_SAMPLES)
    }

    /// A wake utterance's start, `support_start − 4800`: 300 ms of pre-roll before the wake
    /// word's support, not rounded to a cell. The deployed controller started there (every
    /// stored wake turn's `utterance.start` is exactly that), so its streaming recognizer
    /// and committed span began up to 511 samples after the mic stream's start.
    pub fn wake_start(support_start: Sample) -> Sample {
        support_start - Samples(4_800)
    }

    /// The device loudness history for a mic start: 160,000 samples (10 s) before it.
    pub fn cells_start(mic_start: Sample) -> CellIndex {
        (mic_start - Samples(160_000)).cell()
    }

    /// A reply utterance's start: 780 ms before its trigger (480 ms an answer may overlap the
    /// question's end, plus 300 ms of pre-roll), never before the subscription's first retained
    /// sample.
    pub fn reply_start(trigger: Sample, first_retained: Sample) -> Sample {
        (trigger - Samples::from_ms(780)).max(first_retained)
    }

    fn evidence_plan(&self) -> EvidencePlan {
        let analysis_from = self.start.cell();
        EvidencePlan {
            epoch: self.epoch,
            cells_from: self.cells_from.min(analysis_from),
            vad_from: self.vad_from.min(analysis_from),
            analysis_from,
        }
    }
}

/// The ways an utterance can be committed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Route {
    /// The normal pause.
    A,
    /// A complete command followed by background speech.
    B,
    /// A reply to an HA-started conversation.
    R,
    /// The no-progress fallback with complete text.
    #[serde(rename = "fallback")]
    Fallback,
}

/// Terminal reasons for an utterance that does not commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// No command speech.
    NoInput,
    /// Longer than the utterance limit.
    TooLong,
    /// No stable progress, and no complete text to commit.
    Retry,
    /// Audio went missing or the speech path failed.
    Interrupted,
    /// The audio transport overran.
    AudioOverrun,
    /// The device was muted.
    Muted,
    /// The device session was lost.
    SessionLost,
}

/// What caused a close, for the trace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseCause {
    /// A sample-time limit.
    Limit,
    /// The no-progress fallback without complete text.
    NoProgress,
    /// A gap cell.
    Gap,
    /// A cell the device reported muted.
    MutedCell,
    /// The runtime reported privacy mute.
    Mute,
    /// The runtime reported session loss.
    SessionLost,
    /// The runtime reported a transport overrun.
    Overrun,
    /// The speech worker reported an error.
    WorkerError,
    /// The capture epoch changed or the transport reported a discontinuity.
    Discontinuity,
    /// No evidence advanced for the monotonic stall limit.
    EvidenceStalled,
    /// The reply window's monotonic bound passed without command speech.
    ReplyWallClock,
}

/// A committed utterance.
#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    /// Which route committed it.
    pub route: Route,
    /// Where the command ended.
    pub boundary: Sample,
    /// The immutable audio span to transcribe: the utterance start (a reply: 300 ms before its
    /// answer's run) to the later of the boundary plus the 192 ms tail and the last command
    /// word's emission plus its margin, clipped to analysed audio.
    pub span: SampleSpan,
    /// Which bound set the span's end.
    pub end_bound: EndBound,
    /// The emission of the last command word considered, if any.
    pub last_word: Option<Sample>,
    /// The frontier at which it committed.
    pub at: Sample,
    /// The stable streaming prefix.
    pub text: String,
    /// Its class.
    pub class: Option<TextClass>,
    /// Route B and the fallback re-decode the span and close `retry` if its parse changed.
    pub recheck: bool,
    /// Whose words the text is: the pause server's when its words at any pause were this
    /// text, else Kroko's pause decode's, else the streaming result's. A re-check of a
    /// commit on the server's words asks the same server.
    pub source: TextSource,
}

/// What set the end of the committed audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndBound {
    /// The last command-speech cell plus the 192 ms tail.
    Cell,
    /// The streaming recognizer's last command word plus its margin.
    AsrWord,
}

/// Where committed audio ends, and why.
#[derive(Clone, Copy, Debug)]
struct SpanEnd {
    end: Sample,
    bound: EndBound,
    last_word: Option<Sample>,
}

/// Where a reply reopens after a gap before any command speech.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReplyReopen {
    /// The new utterance's start: the end of the last gap cell.
    pub start: Sample,
    /// Its trigger: the later of that and the original trigger.
    pub trigger: Sample,
    /// The original window's `no_input` end, kept.
    pub no_input_at: Sample,
}

/// One decision for the device runtime.
#[derive(Clone, Debug, PartialEq)]
pub enum Decision {
    /// The first command speech after the trigger.
    CommandStarted {
        /// The cell's start.
        at: Sample,
    },
    /// Ask HA's sentence matcher about this text; answer with [`TurnPolicy::recognizer_answer`].
    AskRecognizer {
        /// The text, to be sent and answered exactly.
        text: String,
    },
    /// A route set a provisional end; it commits after the lookahead unless revoked.
    EndPending {
        /// The route.
        route: Route,
        /// Where the command ended.
        boundary: Sample,
        /// The frontier at the decision.
        since: Sample,
    },
    /// Further command speech revoked the provisional end.
    EndRevoked {
        /// The revoked boundary.
        boundary: Sample,
        /// The frontier when it was revoked.
        at: Sample,
    },
    /// The utterance is committed: transcribe and dispatch it exactly once.
    Commit(Commit),
    /// The utterance ended without a commit; nothing is dispatched.
    Close {
        /// Why.
        reason: CloseReason,
        /// The frontier, when any cell had been classified.
        at: Option<Sample>,
    },
    /// A reply's gap before command speech: open a fresh reply utterance here.
    ReopenReply(ReplyReopen),
}

#[derive(Clone, Copy, Debug)]
struct CellRecord {
    class: CellClass,
    speech_positive: bool,
    end: Sample,
}

/// A provisional end, with the text and class in force when it was decided: the commit
/// carries those, whatever words arrive during the lookahead.
#[derive(Debug)]
struct Pending {
    route: Route,
    boundary: Sample,
    since: Sample,
    judged: Judged,
}

/// The stable prefix and its class at one moment.
#[derive(Debug)]
struct Judged {
    text: String,
    class: Option<TextClass>,
}

#[derive(Debug)]
struct Asked {
    text: String,
    at: MonoInstant,
    record: usize,
}

/// A pause server answer waiting for its block.
#[derive(Debug)]
struct ServerArrival {
    answer: PauseServerAnswer,
    /// The first block it may count from: after every recognizer result pushed before it
    /// arrived, and never before its own pause decode.
    from: Sample,
    at: MonoInstant,
}

/// One utterance's speech policy.
#[derive(Debug)]
pub struct TurnPolicy {
    config: TurnConfig,
    spec: TurnSpec,
    evidence: EvidenceAssembler,
    released: Vec<Released>,
    classifier: CellClassifier,
    background: BackgroundLevel,
    foreground: ForegroundLevel,
    /// Wake-word cells that may seed `F`, collected until the trigger.
    seed: Vec<i16>,
    seed_done: bool,
    text: TextTracker,
    class: Option<TextClass>,
    grammar_class: Option<TextClass>,
    answers: Vec<(String, TextClass)>,
    asked: Vec<Asked>,
    arrived: Vec<(RecognizerAnswer, MonoInstant)>,
    /// When each pause decode's audio went to the pause server.
    server_asked: Vec<(Sample, MonoInstant)>,
    server_arrived: Vec<ServerArrival>,
    /// Texts judged on Kroko's pause decodes, and on the pause server's words.
    kroko_texts: Vec<String>,
    server_texts: Vec<String>,
    cells: Vec<CellRecord>,
    frontier: Option<Sample>,
    command_cells: u32,
    dated_back_cells: u32,
    command_ends: Vec<Sample>,
    command_start: Option<Sample>,
    run_start: Option<Sample>,
    last_speech_positive_end: Option<Sample>,
    pending: Option<Pending>,
    reply_gap_end: Option<Sample>,
    last_progress: MonoInstant,
    finished: bool,
    trace: UtteranceTrace,
}

impl TurnPolicy {
    /// A policy for one utterance, opened at `now` on the controller's monotonic clock.
    pub fn new(
        config: TurnConfig,
        spec: TurnSpec,
        now: MonoInstant,
    ) -> Result<Self, EvidenceError> {
        let evidence = EvidenceAssembler::new(spec.evidence_plan())?;
        let reply = match &spec.kind {
            TurnKind::Reply(r) => Some(ReplyAttribution {
                guard_until: r.guard_until,
            }),
            _ => None,
        };
        let wake = match &spec.kind {
            TurnKind::Wake(w) => Some(WakeCut {
                lookahead_end: w.candidate_open + Samples::from_ms(480),
                phrase: w.phrase.clone(),
            }),
            _ => None,
        };
        let kind = match &spec.kind {
            TurnKind::Wake(_) => TraceKind::Wake,
            TurnKind::Button => TraceKind::Button,
            TurnKind::Reply(r) if r.origin == ReplyOrigin::HaConversation => TraceKind::HaReply,
            TurnKind::Reply(_) => TraceKind::Reply,
        };
        let trace = UtteranceTrace {
            config,
            kind,
            epoch: spec.epoch.get(),
            start: spec.start,
            trigger: spec.trigger,
            guard_until: reply.and_then(|r| r.guard_until),
            command_start: None,
            answer_start: None,
            foreground_db: None,
            foreground_seeded: false,
            command_cells: 0,
            evidence: EvidenceCounts::default(),
            segments: Vec::new(),
            streaming_text: String::new(),
            heard: String::new(),
            text: Vec::new(),
            finalized_at: Vec::new(),
            pauses: Vec::new(),
            recognizer: Vec::new(),
            endpoint: Vec::new(),
            frontier: None,
        };
        Ok(Self {
            classifier: CellClassifier::new(config.attribution, spec.trigger, reply),
            background: BackgroundLevel::new(config.attribution.non_speech_max),
            foreground: ForegroundLevel::new(),
            seed: Vec::new(),
            seed_done: false,
            text: TextTracker::new(spec.trigger, wake),
            class: None,
            grammar_class: None,
            answers: Vec::new(),
            asked: Vec::new(),
            arrived: Vec::new(),
            server_asked: Vec::new(),
            server_arrived: Vec::new(),
            kroko_texts: Vec::new(),
            server_texts: Vec::new(),
            cells: Vec::new(),
            frontier: None,
            command_cells: 0,
            dated_back_cells: 0,
            command_ends: Vec::new(),
            command_start: None,
            run_start: None,
            last_speech_positive_end: None,
            pending: None,
            reply_gap_end: None,
            last_progress: now,
            finished: false,
            released: Vec::new(),
            evidence,
            config,
            spec,
            trace,
        })
    }

    /// The assembler the runtime feeds this utterance's evidence into.
    pub fn evidence_mut(&mut self) -> &mut EvidenceAssembler {
        &mut self.evidence
    }

    /// Whether the utterance has ended (committed, closed or handed to a reopened reply).
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// The decision trace so far.
    pub fn trace(&self) -> &UtteranceTrace {
        &self.trace
    }

    /// The decision trace, consuming the policy.
    pub fn into_trace(self) -> UtteranceTrace {
        self.trace
    }

    /// The next instant at which [`TurnPolicy::on_timer`] has something to check.
    pub fn next_deadline(&self) -> Option<MonoInstant> {
        if self.finished {
            return None;
        }
        let stall = self.last_progress.after(Duration::from_millis(
            self.config.endpoint.evidence_stall_ms,
        ));
        match &self.spec.kind {
            TurnKind::Reply(r) if self.command_start_any().is_none() => {
                Some(stall.min(r.wall_deadline))
            }
            _ => Some(stall),
        }
    }

    /// Records HA's sentence-matcher answer, received at `now`. It takes effect at the next
    /// block boundary the policy evaluates.
    pub fn recognizer_answer(&mut self, answer: RecognizerAnswer, now: MonoInstant) {
        if !self.finished {
            self.arrived.push((answer, now));
        }
    }

    /// The pause decode whose audio ends at `pause` was sent to the pause server at `now`
    /// (§4.7). Its answer's time, or its being dropped, counts from here.
    pub fn pause_server_asked(&mut self, pause: Sample, now: MonoInstant) {
        if !self.finished {
            self.server_asked.push((pause, now));
        }
    }

    /// The pause server's answer, received at `now`. It counts from the first recognizer
    /// result pushed after this call (never before its own pause decode), and only while
    /// that pause decode still stands then; otherwise it is recorded as superseded.
    pub fn pause_server_answer(&mut self, answer: PauseServerAnswer, now: MonoInstant) {
        if self.finished {
            return;
        }
        let from = match self.evidence.asr_through() {
            Some(pushed) if pushed >= answer.pause => Sample(pushed.0 + 1),
            _ => answer.pause,
        };
        self.server_arrived.push(ServerArrival {
            answer,
            from,
            at: now,
        });
    }

    fn server_ms(&self, pause: Sample, now: MonoInstant) -> Option<u64> {
        self.server_asked
            .iter()
            .rev()
            .find(|(p, _)| *p == pause)
            .map(|(_, asked)| now.since(*asked).as_millis() as u64)
    }

    /// Drops every pause server request still out: speech went on, or a later pause sent
    /// another. One never sent stays open.
    fn supersede_server(&mut self, now: MonoInstant) {
        for i in 0..self.trace.pauses.len() {
            let through = self.trace.pauses[i].through;
            let open = self.trace.pauses[i]
                .server
                .as_ref()
                .is_some_and(PauseWords::is_open);
            if !open {
                continue;
            }
            let Some(ms) = self.server_ms(through, now) else {
                continue;
            };
            if let Some(server) = self.trace.pauses[i].server.as_mut() {
                server.ms = Some(ms);
                server.error = Some(PauseError::Superseded);
            }
        }
    }

    /// Records the pause server answers due by the block through `through`, and stands
    /// their words in for Kroko's where their pause decode still stands. Returns whether
    /// any did.
    fn take_server_answers(&mut self, through: Sample) -> bool {
        if self.server_arrived.is_empty() {
            return false;
        }
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.server_arrived)
            .into_iter()
            .partition(|a| a.from <= through);
        self.server_arrived = waiting;
        let mut stood = false;
        for arrival in due {
            let pause = arrival.answer.pause;
            let ms = self.server_ms(pause, arrival.at);
            let Some(server) = self
                .trace
                .pauses
                .iter_mut()
                .rev()
                .find(|p| p.through == pause)
                .and_then(|p| p.server.as_mut())
                .filter(|s| s.is_open())
            else {
                continue;
            };
            server.ms = ms;
            match arrival.answer.outcome {
                Ok(text) => {
                    let words = text.trim();
                    server.text = Some(words.to_owned());
                    if !words.is_empty() && self.text.stand_server(pause, words) {
                        stood = true;
                    }
                }
                Err(failure) => server.error = Some(failure.into()),
            }
        }
        stood
    }

    /// Privacy mute: closes `muted`.
    pub fn mute(&mut self, out: &mut Vec<Decision>) {
        self.close(CloseReason::Muted, CloseCause::Mute, out);
    }

    /// The device session was lost: closes `session_lost`.
    pub fn session_lost(&mut self, out: &mut Vec<Decision>) {
        self.close(CloseReason::SessionLost, CloseCause::SessionLost, out);
    }

    /// The audio transport overran: closes `audio_overrun`.
    pub fn audio_overrun(&mut self, out: &mut Vec<Decision>) {
        self.close(CloseReason::AudioOverrun, CloseCause::Overrun, out);
    }

    /// The speech worker reported an error for this utterance: closes `interrupted`.
    pub fn worker_error(&mut self, out: &mut Vec<Decision>) {
        self.close(CloseReason::Interrupted, CloseCause::WorkerError, out);
    }

    /// The capture epoch changed or the transport reported a discontinuity: closes
    /// `interrupted`. Positions on the new epoch cannot continue this utterance.
    pub fn discontinuity(&mut self, out: &mut Vec<Decision>) {
        self.close(CloseReason::Interrupted, CloseCause::Discontinuity, out);
    }

    /// Checks the monotonic backstops at `now`.
    pub fn on_timer(&mut self, now: MonoInstant, out: &mut Vec<Decision>) {
        if self.finished {
            return;
        }
        if let TurnKind::Reply(r) = &self.spec.kind
            && self.command_start_any().is_none()
            && now >= r.wall_deadline
        {
            self.close(CloseReason::NoInput, CloseCause::ReplyWallClock, out);
            return;
        }
        let stall = Duration::from_millis(self.config.endpoint.evidence_stall_ms);
        if now.since(self.last_progress) >= stall {
            self.close(CloseReason::Interrupted, CloseCause::EvidenceStalled, out);
        }
    }

    /// Releases whatever evidence is ready, classifies it and evaluates every block boundary in
    /// sample order, appending every decision to `out`.
    pub fn advance<C: Completeness + ?Sized>(
        &mut self,
        grammar: &C,
        now: MonoInstant,
        out: &mut Vec<Decision>,
    ) {
        if self.finished {
            return;
        }
        let mut released = std::mem::take(&mut self.released);
        released.clear();
        self.evidence.release(&mut released);
        for item in released.drain(..) {
            if self.finished {
                break;
            }
            match item {
                Released::Cell(cell) => self.on_cell(&cell, out),
                Released::Block(obs) => {
                    self.last_progress = now;
                    self.on_block(obs, grammar, now, out);
                }
            }
        }
        self.released = released;
    }

    /// Whether any command speech after the trigger exists.
    fn command_start_any(&self) -> Option<Sample> {
        (self.command_cells > 0).then_some(self.command_start.unwrap_or(self.spec.trigger))
    }

    fn is_reply(&self) -> bool {
        matches!(self.spec.kind, TurnKind::Reply(_))
    }

    fn close(&mut self, reason: CloseReason, cause: CloseCause, out: &mut Vec<Decision>) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.trace.frontier = self.frontier;
        self.trace.endpoint.push(EndpointEvent::Close {
            reason,
            cause,
            at: self.frontier,
        });
        out.push(Decision::Close {
            reason,
            at: self.frontier,
        });
    }

    fn count_evidence(&mut self, audio: &CellAudio, background: Option<f64>) {
        let e = &mut self.trace.evidence;
        e.cells += 1;
        e.background_available += u32::from(background.is_some());
        match *audio {
            CellAudio::Gap { .. } => e.gap += 1,
            CellAudio::Present {
                energy_centi,
                vad,
                echo,
                ..
            } => {
                e.loudness_unknown += u32::from(energy_centi.is_none());
                e.vad_unknown += u32::from(matches!(vad, crate::evidence::VadInput::Unknown));
                match echo.label {
                    EchoLabel::NoReference => e.echo_no_reference += 1,
                    EchoLabel::EchoOnly => e.echo_only += 1,
                    EchoLabel::NearEndPresent => e.echo_near_end += 1,
                    EchoLabel::Unknown => e.echo_unknown += 1,
                }
                if matches!(
                    echo.coverage,
                    ReferenceCoverage::Partial | ReferenceCoverage::Missing
                ) {
                    e.reference_incomplete += 1;
                }
            }
        }
    }

    fn on_cell(&mut self, cell: &CellEvidence, out: &mut Vec<Decision>) {
        let start = cell.cell.start();
        let end = cell.cell.end();
        let background = self.background.current_db();
        self.background.push(&cell.audio);
        if start < self.spec.start.round_down(CELL_SAMPLES) {
            // Loudness history before the utterance only feeds `B`.
            return;
        }

        // A reply that met a gap before any command speech evaluates nothing until audio comes
        // back; the first cell after the gap reopens it.
        if let Some(gap_end) = self.reply_gap_end {
            match cell.audio {
                CellAudio::Gap { .. } => self.reply_gap_end = Some(end),
                CellAudio::Present { .. } => self.reopen(gap_end, out),
            }
            return;
        }

        // F is seeded from the wake word once the cells reach the trigger.
        let after_trigger = end > self.spec.trigger;
        if after_trigger && !self.seed_done {
            self.seed_done = true;
            if matches!(self.spec.kind, TurnKind::Wake(_)) {
                self.foreground.seed(&self.seed);
                self.trace.foreground_seeded = self.foreground.seeded();
            }
            self.seed = Vec::new();
        }
        let foreground = self.foreground.current_db();
        self.count_evidence(&cell.audio, background);
        let judged = self.classifier.classify(cell, background, foreground);
        self.trace.push_cell(judged.class, judged.rule, start, end);
        self.frontier = Some(end);
        self.trace.frontier = Some(end);

        let speech_positive = cell
            .audio
            .speech_positive(self.config.attribution.speech_positive_min);
        if speech_positive {
            self.last_speech_positive_end = Some(end);
        }
        self.cells.push(CellRecord {
            class: judged.class,
            speech_positive,
            end,
        });
        if let Some(open) = judged.opened {
            self.run_start = Some(open.start);
            if after_trigger {
                self.dated_back_cells += open.dated_back_cells;
            }
        }

        if let CellAudio::Gap { muted } = cell.audio {
            if self.is_reply() && self.command_cells == 0 {
                self.reply_gap_end = Some(end);
                return;
            }
            let (reason, cause) = if muted {
                (CloseReason::Muted, CloseCause::MutedCell)
            } else {
                (CloseReason::Interrupted, CloseCause::Gap)
            };
            self.close(reason, cause, out);
            return;
        }

        if !after_trigger {
            // The wake word's own cells, from support start to the trigger, with VAD ≥ 0.35,
            // may seed F; they are never command speech for no_input or an endpoint.
            if let TurnKind::Wake(w) = &self.spec.kind
                && start >= w.support_start
                && cell
                    .audio
                    .vad()
                    .is_some_and(|p| p >= self.config.attribution.non_speech_max)
                && let CellAudio::Present {
                    energy_centi: Some(centi),
                    ..
                } = cell.audio
            {
                self.seed.push(centi);
            }
            return;
        }

        if judged.class == CellClass::CommandSpeech {
            if let CellAudio::Present {
                energy_centi: Some(e),
                ..
            } = cell.audio
            {
                self.foreground.add(e);
            }
            self.trace.foreground_db = self.foreground.current_db();
            self.command_cells += 1;
            self.trace.command_cells = self.command_cells;
            self.command_ends.push(end);
            if self.command_cells == 1 {
                self.trace.answer_start = self.run_start.or(Some(start));
                out.push(Decision::CommandStarted { at: start });
            }
            if self.command_start.is_none() && start >= self.spec.trigger {
                self.command_start = Some(start);
                self.trace.command_start = Some(start);
            }
        }
    }

    fn reopen(&mut self, gap_end: Sample, out: &mut Vec<Decision>) {
        let TurnKind::Reply(r) = &self.spec.kind else {
            return;
        };
        let reopen = ReplyReopen {
            start: gap_end,
            trigger: gap_end.max(self.spec.trigger),
            no_input_at: r.no_input_at,
        };
        self.finished = true;
        self.trace
            .endpoint
            .push(EndpointEvent::Reopen { start: gap_end });
        out.push(Decision::ReopenReply(reopen));
    }

    fn apply_answers(&mut self, block: Sample) {
        let limit = Duration::from_millis(self.config.endpoint.recognizer_limit_ms);
        for (answer, at) in std::mem::take(&mut self.arrived) {
            let Some(asked) = self.asked.iter().find(|a| a.text == answer.text) else {
                self.trace.recognizer.push(RecognizerRecord {
                    text: answer.text,
                    asked_at: block,
                    class: None,
                    applied_at: None,
                    round_trip_ms: None,
                    error: Some(RecognizerError::NotAsked),
                });
                continue;
            };
            let round_trip = at.since(asked.at);
            let record = &mut self.trace.recognizer[asked.record];
            if record.applied_at.is_some() || record.error.is_some() {
                continue;
            }
            record.round_trip_ms = Some(round_trip.as_millis() as u64);
            if round_trip > limit {
                record.error = Some(RecognizerError::Timeout);
                continue;
            }
            match answer.outcome {
                Ok(class) => {
                    record.class = Some(class);
                    record.applied_at = Some(block);
                    self.answers.push((answer.text, class));
                }
                Err(e) => record.error = Some(e),
            }
        }
        self.refresh_class();
    }

    /// The prefix's class: the grammar's, unless it is `unknown` and HA answered for this text.
    fn refresh_class(&mut self) {
        let prefix = self.text.prefix();
        self.class = match self.grammar_class {
            None => None,
            Some(TextClass::Unknown) => Some(
                self.answers
                    .iter()
                    .find(|(t, _)| t == prefix)
                    .map_or(TextClass::Unknown, |(_, c)| *c),
            ),
            Some(c) => Some(c),
        };
    }

    fn on_block<C: Completeness + ?Sized>(
        &mut self,
        obs: AsrObservation,
        grammar: &C,
        now: MonoInstant,
        out: &mut Vec<Decision>,
    ) {
        let through = obs.through;
        let finalized = obs.kind == AsrKind::Finalized;
        let kroko_words = finalized.then(|| {
            let text: String = obs.tokens.iter().map(|t| t.text.as_str()).collect();
            text.trim().to_owned()
        });
        if finalized {
            self.trace.finalized_at.push(through);
            // A new pause decode drops the previous pause's request.
            self.supersede_server(now);
        }
        self.apply_answers(through);
        if self.text.take(obs) {
            // New speech ended the pause decode: its request no longer describes the audio.
            self.supersede_server(now);
        }
        if let Some(text) = kroko_words {
            self.trace.pauses.push(PauseRecord {
                through,
                kroko: PauseWords {
                    text: Some(text),
                    ..PauseWords::default()
                },
                server: self.config.pause_server.then(PauseWords::default),
            });
        }
        let server_stood = self.take_server_answers(through);
        let changed = self.text.settle(through);
        if finalized
            && !server_stood
            && let Some(pause) = self.trace.pauses.last_mut()
        {
            let prefix = self.text.prefix();
            pause.kroko.judged = Some(prefix.to_owned());
            pause.kroko.at = Some(through);
            if !self.kroko_texts.iter().any(|t| t == prefix) {
                self.kroko_texts.push(prefix.to_owned());
            }
        }
        if server_stood
            && let Some(server) = self
                .trace
                .pauses
                .last_mut()
                .and_then(|p| p.server.as_mut())
                .filter(|s| s.at.is_none())
        {
            let prefix = self.text.prefix();
            server.judged = Some(prefix.to_owned());
            server.at = Some(through);
            if !self.server_texts.iter().any(|t| t == prefix) {
                self.server_texts.push(prefix.to_owned());
            }
        }
        // HA's matcher is asked about a new text the grammar cannot judge once the block has
        // been evaluated: a block that commits asks nothing, as the deployed controller did.
        let mut ask = false;
        if changed {
            let prefix = self.text.prefix().to_owned();
            self.grammar_class = (!prefix.is_empty()).then(|| grammar.classify(&prefix));
            let ha_reply = matches!(
                &self.spec.kind,
                TurnKind::Reply(r) if r.origin == ReplyOrigin::HaConversation
            );
            ask = self.grammar_class == Some(TextClass::Unknown)
                && self.config.recognizer_available
                && !ha_reply
                && !self.asked.iter().any(|a| a.text == prefix);
            self.trace.text.push(TextChange {
                text: prefix,
                grammar: self.grammar_class,
                at: through,
                source: self.text.source(),
            });
            self.refresh_class();
        }
        self.trace.streaming_text.clear();
        self.trace
            .streaming_text
            .push_str(self.text.streaming_text());
        self.trace.heard.clear();
        self.trace.heard.push_str(self.text.heard());
        if self.reply_gap_end.is_none()
            && let Some(frontier) = self.frontier
        {
            self.evaluate(frontier, through, out);
        }
        if ask && !self.finished {
            let prefix = self.text.prefix().to_owned();
            self.asked.push(Asked {
                text: prefix.clone(),
                at: now,
                record: self.trace.recognizer.len(),
            });
            self.trace.recognizer.push(RecognizerRecord {
                text: prefix.clone(),
                asked_at: through,
                class: None,
                applied_at: None,
                round_trip_ms: None,
                error: None,
            });
            out.push(Decision::AskRecognizer { text: prefix });
        }
    }

    fn command_ms(&self) -> Samples {
        Samples(u64::from(self.command_cells + self.dated_back_cells) * CELL_SAMPLES)
    }

    /// Command-speech cells that start at or after `since`.
    fn command_cells_since(&self, since: Sample) -> u32 {
        self.cells
            .iter()
            .rev()
            .take_while(|c| c.end > since)
            .filter(|c| c.class == CellClass::CommandSpeech)
            .count() as u32
    }

    /// The end of the last command-speech cell at or before `at`.
    fn last_command_end_by(&self, at: Sample) -> Option<Sample> {
        let i = self.command_ends.partition_point(|&e| e <= at);
        i.checked_sub(1).map(|i| self.command_ends[i])
    }

    fn evaluate(&mut self, frontier: Sample, through: Sample, out: &mut Vec<Decision>) {
        let p = self.config.endpoint;
        let command = self.command_start_any();

        // Limits.
        let reply = match &self.spec.kind {
            TurnKind::Reply(r) => Some(*r),
            _ => None,
        };
        if command.is_none() {
            let no_input_at = reply.map_or(self.spec.trigger + p.no_input, |r| r.no_input_at);
            if frontier >= no_input_at {
                self.close(CloseReason::NoInput, CloseCause::Limit, out);
                return;
            }
        }
        let max = if self.config.extended_utterances {
            p.max_extended_utterance
        } else {
            p.max_utterance
        };
        let long_from = if reply.is_some() {
            command
        } else {
            Some(self.spec.trigger)
        };
        if long_from.is_some_and(|from| frontier.since(from) >= max) {
            self.close(CloseReason::TooLong, CloseCause::Limit, out);
            return;
        }

        // A pending end: revoked by further command speech, otherwise committed once the
        // lookahead is new audio.
        if let Some(pending) = self.pending.take() {
            let after = self.command_cells_since(pending.since);
            if after >= p.revoke_command_cells {
                self.trace.endpoint.push(EndpointEvent::Revoked {
                    boundary: pending.boundary,
                    at: frontier,
                    command_cells: after,
                });
                out.push(Decision::EndRevoked {
                    boundary: pending.boundary,
                    at: frontier,
                });
            } else {
                // The lookahead counts from the decision, or from the end of the committed audio
                // when the last word carries it past the decision.
                let end = self.span_end(pending.boundary, pending.since);
                if frontier.since(pending.since.max(end.end)) >= p.lookahead {
                    self.commit(
                        pending.route,
                        pending.boundary,
                        end,
                        frontier,
                        pending.judged,
                        out,
                    );
                } else {
                    self.pending = Some(pending);
                }
            }
            return;
        }

        let Some(command_start) = command else {
            return;
        };
        let last_cmd_end = *self.command_ends.last().expect("command speech exists");
        let ha_reply = reply.is_some_and(|r| r.origin == ReplyOrigin::HaConversation);
        let set = if ha_reply {
            self.route_r(frontier, last_cmd_end)
        } else {
            self.route_a(frontier, through, last_cmd_end)
                .or_else(|| self.route_b(frontier))
        };
        if let Some((route, boundary, evidence)) = set {
            self.pending = Some(Pending {
                route,
                boundary,
                since: frontier,
                judged: self.judged(),
            });
            self.trace.endpoint.push(EndpointEvent::Pending {
                route,
                boundary,
                since: frontier,
                evidence,
            });
            out.push(Decision::EndPending {
                route,
                boundary,
                since: frontier,
            });
            return;
        }

        // The no-progress fallback.
        let progress = self.text.progress_sample().max(command_start);
        if frontier.since(progress) >= p.no_progress {
            let boundary = match (self.class, self.text.prefix_sample()) {
                (Some(TextClass::Complete), Some(ps)) => self.last_command_end_by(ps),
                _ => None,
            };
            match boundary {
                Some(b) => {
                    let end = self.span_end(b, frontier);
                    let judged = self.judged();
                    self.commit(Route::Fallback, b, end, frontier, judged, out);
                }
                None => self.close(CloseReason::Retry, CloseCause::NoProgress, out),
            }
        }
    }

    fn route_evidence(&self, pause: Samples, required: Samples) -> RouteEvidence {
        RouteEvidence {
            class: self.class,
            command_ms: self.command_ms().as_ms(),
            quiet_votes: None,
            trailing_quiet_ms: None,
            finalized: false,
            pause_ms: pause.as_ms(),
            required_ms: required.as_ms(),
            prefix_sample: None,
            speech_positive_after: None,
        }
    }

    /// Route A, the normal pause: enough command speech, a stable prefix, a quiet vote, decoder
    /// quiet after the last word, and a pause after the command as long as the text's class
    /// asks for.
    fn route_a(
        &self,
        frontier: Sample,
        through: Sample,
        last_cmd_end: Sample,
    ) -> Option<(Route, Sample, RouteEvidence)> {
        let p = self.config.endpoint;
        let class = self.class?;
        if self.command_ms() < p.min_command || self.text.prefix().is_empty() {
            return None;
        }
        // After needs_more or unknown text, speech well below the command counts as pause, so a
        // free-form request can end under a quieter TV.
        let background_quiet = matches!(class, TextClass::NeedsMore | TextClass::Unknown);
        let quiet = self
            .cells
            .iter()
            .rev()
            .take(p.vote_cells as usize)
            .filter(|c| match c.class {
                CellClass::NonSpeech | CellClass::SelfOutput => true,
                CellClass::BackgroundSpeech => background_quiet,
                _ => false,
            })
            .count() as u32;
        if quiet < p.vote_quiet_min {
            return None;
        }
        let trailing = self.text.trailing_quiet(
            through,
            self.last_speech_positive_end.unwrap_or(self.spec.start),
        );
        let blanks_min =
            Samples(u64::from(p.trailing_blanks_min) * crate::asr::BLANK_FRAME_SAMPLES);
        if trailing.samples() < blanks_min {
            return None;
        }
        let pause = frontier.since(last_cmd_end);
        let required = p.pause_for(class);
        if pause < required {
            return None;
        }
        let mut evidence = self.route_evidence(pause, required);
        evidence.quiet_votes = Some(quiet);
        evidence.trailing_quiet_ms = Some(trailing.samples().as_ms());
        evidence.finalized = self.text.finalized_standing();
        Some((Route::A, last_cmd_end, evidence))
    }

    /// Route B, a complete command under background speech: the prefix is `complete`, has not
    /// changed for 608 ms, and at least 80% of the speech-positive cells since it changed are
    /// background speech; the boundary is the last command speech at or before the prefix
    /// changed. The route needs at least one speech-positive cell since then: with none, "80% of
    /// nothing" would let a soft continuation below the speech-positive bar (command speech held
    /// by hysteresis) be cut off, while route A already ends a quiet pause.
    fn route_b(&self, frontier: Sample) -> Option<(Route, Sample, RouteEvidence)> {
        let p = self.config.endpoint;
        if self.class != Some(TextClass::Complete) {
            return None;
        }
        let ps = self.text.prefix_sample()?;
        let pause = frontier.since(ps);
        if pause < p.pause_complete {
            return None;
        }
        let (positive, background) = self
            .cells
            .iter()
            .rev()
            .take_while(|c| c.end > ps)
            .filter(|c| c.speech_positive)
            .fold((0u32, 0u32), |(n, b), c| {
                (n + 1, b + u32::from(c.class == CellClass::BackgroundSpeech))
            });
        if positive == 0 || background * 100 < positive * p.background_share_percent {
            return None;
        }
        let boundary = self.last_command_end_by(ps)?;
        let mut evidence = self.route_evidence(pause, p.pause_complete);
        evidence.prefix_sample = Some(ps);
        evidence.speech_positive_after = Some((positive, background));
        Some((Route::B, boundary, evidence))
    }

    /// Route R, a reply to an HA-started conversation: any 1,024 ms pause after command speech.
    fn route_r(
        &self,
        frontier: Sample,
        last_cmd_end: Sample,
    ) -> Option<(Route, Sample, RouteEvidence)> {
        let p = self.config.endpoint;
        let pause = frontier.since(last_cmd_end);
        (pause >= p.ha_reply_pause).then(|| {
            (
                Route::R,
                last_cmd_end,
                self.route_evidence(pause, p.ha_reply_pause),
            )
        })
    }

    /// The end of the committed audio for a command that ended at `boundary`, decided with
    /// evidence through `decided`: the later of the cell bound (boundary plus the 192 ms tail)
    /// and the last command word the streaming recognizer emitted by `decided`, plus the word
    /// margin. A soft last word that never counted as command speech is still heard in full;
    /// words emitted after the decision never extend the span.
    fn span_end(&self, boundary: Sample, decided: Sample) -> SpanEnd {
        let p = self.config.endpoint;
        let cell = boundary + p.lookahead;
        let last_word = self.text.last_command_word_by(decided);
        match last_word.map(|w| w + p.asr_word_margin) {
            Some(word) if word > cell => SpanEnd {
                end: word,
                bound: EndBound::AsrWord,
                last_word,
            },
            _ => SpanEnd {
                end: cell,
                bound: EndBound::Cell,
                last_word,
            },
        }
    }

    fn commit(
        &mut self,
        route: Route,
        boundary: Sample,
        end: SpanEnd,
        frontier: Sample,
        judged: Judged,
        out: &mut Vec<Decision>,
    ) {
        let p = self.config.endpoint;
        let start = match &self.spec.kind {
            TurnKind::Reply(_) => self.trace.answer_start.map_or(self.spec.start, |a| {
                (a - p.reply_span_lead).max(self.spec.start)
            }),
            _ => self.spec.start,
        };
        let span = SampleSpan::new(start, end.end.min(frontier));
        let Judged { text, class } = judged;
        // The server's words first: a re-check of them asks the same server.
        let source = if self.server_texts.contains(&text) {
            TextSource::Server
        } else if self.kroko_texts.contains(&text) {
            TextSource::Kroko
        } else {
            TextSource::Streaming
        };
        let commit = Commit {
            route,
            boundary,
            span,
            end_bound: end.bound,
            last_word: end.last_word,
            at: frontier,
            text,
            class,
            recheck: matches!(route, Route::B | Route::Fallback),
            source,
        };
        self.finished = true;
        self.trace.frontier = Some(frontier);
        self.trace.endpoint.push(EndpointEvent::Commit {
            route,
            boundary,
            span,
            end_bound: end.bound,
            last_word: end.last_word,
            at: frontier,
            text: commit.text.clone(),
            class: commit.class,
            source,
        });
        out.push(Decision::Commit(commit));
    }

    /// The stable prefix in force and its class.
    fn judged(&self) -> Judged {
        Judged {
            text: self.text.prefix().to_owned(),
            class: self.class,
        }
    }
}
