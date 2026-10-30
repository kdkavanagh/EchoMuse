//! The device runtime's side of a recording: [`SessionRecorder`], and the writer thread that
//! puts its entries on disk.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::SystemTime;

use bytes::Bytes;
use echomuse_core::services::HaError;
use echomuse_core::types::{DeviceId, RuntimeId, SessionId, TurnId};
use echomuse_echo::{CandidateComparison, CellLabel};
use echomuse_protocol::ema::{Flags, Packet};
use echomuse_protocol::{LinkSessionId, StreamId};
use echomuse_turn::{
    ArrivalConditions, AsrObservation, CandidateEcho, CellIndex, Completeness, Decision,
    DeviceInput, EchoCell, Epoch, MonoInstant, PauseServerAnswer, RecognizerAnswer, ReferenceScore,
    Sample, SampleSpan, TextClass, TurnConfig, TurnPolicy, TurnSpec, VadInput, VerifierResult,
    WakeCandidate, WakeDecision, WakePolicy,
};

use crate::format::{RecordType, write_frame, write_json, write_preamble};
use crate::grant::{GrantPass, GrantShared, StreamScope, UnixMillis};
use crate::queue::{Body, Item, Purge, Pushed, Queue};
use crate::record::{
    AnalyzerId, AppliedAnswer, AsrRecord, CancelTarget, DecisionRecord, DisconnectReason,
    EchoCellRecord, EchoStep, End, EndReason, Entry, Event, ExchangeId, GrammarAnswer, GrantRecord,
    HaBody, HaEntry, HaFailure, HaOperation, Header, IngressOrdinal, LabelRecord, Marker,
    PauseServerOutcome, PrivacyState, RecordingId, RelTime, Revisions, SessionRecord, Tally,
    Transcript, TurnCause, TurnControl, TurnSpecRecord, TurnStep, VadRecord, VerifierRecord,
    WakeEntry, WithheldReason,
};
use crate::redact::{redact, redact_text};
use crate::store::{RecordingStore, private_dir};

/// The session a recording is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionIdentity {
    /// The device.
    pub device: DeviceId,
    /// The device runtime.
    pub runtime: RuntimeId,
    /// The transport session.
    pub session: SessionId,
    /// The link `session_id`.
    pub link_session: LinkSessionId,
}

/// Why a recording could not start.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The pass is for another device.
    #[error("the grant is for another device")]
    WrongDevice,
    /// The grant ended before the recording could start.
    #[error("the grant has ended ({0:?})")]
    GrantEnded(EndReason),
    /// The writer thread could not be started.
    #[error("could not start the writer thread: {0}")]
    Spawn(#[source] io::Error),
}

/// A recording's state, for the runtime and the dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecorderState {
    /// Taking entries. `incomplete` once anything has been lost to a full queue.
    Recording {
        /// Whether entries have been lost.
        incomplete: bool,
    },
    /// Ended.
    Ended {
        /// Why.
        reason: EndReason,
        /// Whether the writer has finished writing it.
        written: bool,
    },
    /// The store deleted it while it was being written.
    Deleted,
    /// Writing failed; the file is left as it was, cut short.
    Failed(String),
}

/// What happened to a frame handed to [`SessionRecorder::frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameTaken {
    /// Queued for writing.
    Queued,
    /// Dropped: the queue was full. The recording is now incomplete.
    Lost,
    /// Not kept: the grant does not cover this stream (or it is a render frame).
    NotInScope,
    /// Not kept: the device was muted, capture was not permitted, or the frame was flagged
    /// muted. Counted and recorded as withheld audio.
    Withheld,
    /// Not kept: the bytes are not a valid EMA1 frame.
    Invalid,
    /// The recording has ended.
    Ended,
}

/// Wraps the runtime's grammar while a turn advances, so the recorder learns exactly which
/// texts the policy judged and how. Replay answers the same texts the same way.
///
/// ```ignore
/// let tap = GrammarTap::new(&grammar);
/// policy.advance(&tap, now, &mut out);
/// recorder.turn_advanced(turn, &policy, tap.take(), &out, now);
/// ```
#[derive(Debug)]
pub struct GrammarTap<'a, C: ?Sized> {
    inner: &'a C,
    answers: RefCell<Vec<GrammarAnswer>>,
}

impl<'a, C: Completeness + ?Sized> GrammarTap<'a, C> {
    /// Wraps `inner`.
    pub fn new(inner: &'a C) -> Self {
        Self {
            inner,
            answers: RefCell::new(Vec::new()),
        }
    }

    /// The answers given since the last call, each text once.
    pub fn take(&self) -> Vec<GrammarAnswer> {
        std::mem::take(&mut *self.answers.borrow_mut())
    }
}

impl<C: Completeness + ?Sized> Completeness for GrammarTap<'_, C> {
    fn classify(&self, text: &str) -> TextClass {
        let class = self.inner.classify(text);
        let mut answers = self.answers.borrow_mut();
        if !answers.iter().any(|a| a.text == text) {
            answers.push(GrammarAnswer {
                text: text.to_owned(),
                class,
            });
        }
        class
    }
}

/// One device session's recording, owned by its device runtime.
///
/// Every method takes the controller's monotonic `now` and returns at once: entries are
/// converted here and queued for the writer thread, and nothing waits for the disk. Each call
/// first checks the grant; once it has expired or been withdrawn, the call ends the recording
/// instead and nothing more is kept.
#[derive(Debug)]
pub struct SessionRecorder {
    id: RecordingId,
    queue: Arc<Queue>,
    grant: Arc<GrantShared>,
    streams: StreamScope,
    origin: MonoInstant,
    last_now: MonoInstant,
    next_seq: u64,
    privacy: PrivacyState,
    withheld: Option<(WithheldReason, u64)>,
    /// Per turn, which of its recognizer records have been reported applied.
    applied: HashMap<u64, Vec<bool>>,
    writer: Option<JoinHandle<()>>,
}

/// A recording that has been told to end. The writer finishes on its own; [`Finishing::wait`]
/// is for tests and shutdown only, never for the device runtime.
#[derive(Debug)]
pub struct Finishing {
    queue: Arc<Queue>,
    writer: Option<JoinHandle<()>>,
}

impl Finishing {
    /// Waits for the writer thread to finish and returns the final state.
    pub fn wait(mut self) -> RecorderState {
        if let Some(h) = self.writer.take() {
            let _ = h.join();
        }
        self.queue.status()
    }
}

impl SessionRecorder {
    /// Starts recording a session under a live grant. `privacy` is the state the hello
    /// installed; `wall` is `now` on the wall clock, the recording's one wall-clock anchor.
    pub fn start(
        pass: GrantPass,
        store: &RecordingStore,
        session: SessionIdentity,
        revisions: Revisions,
        privacy: PrivacyState,
        now: MonoInstant,
        wall: SystemTime,
    ) -> Result<Self, StartError> {
        let grant = pass.shared;
        if grant.device != session.device {
            return Err(StartError::WrongDevice);
        }
        grant.live_at(now).map_err(StartError::GrantEnded)?;
        let id = RecordingId::generate();
        let queue = Arc::new(Queue::new(store.config().queue, now));
        grant.attach(&queue).map_err(StartError::GrantEnded)?;
        let header = Header {
            recording: id.clone(),
            session: SessionRecord {
                device: session.device.0.clone(),
                runtime: session.runtime.0,
                session: session.session.0,
                link_session: session.link_session.as_str().to_owned(),
            },
            grant: GrantRecord {
                id: grant.id.clone(),
                administrator: grant.administrator.as_str().to_owned(),
                streams: grant.streams,
                elapsed_at_start_ms: millis(now.since(grant.starts)),
                remaining_at_start_ms: millis(grant.ends.since(now)),
            },
            anchor: UnixMillis::of(wall),
            retention_ms: millis(store.config().retention),
            revisions,
            privacy,
        };
        let (part, done) = store.paths(&session.device, &id);
        store.register(&id, &session.device.0, &queue);
        let writer = {
            let queue = Arc::clone(&queue);
            let store = store.clone();
            let id = id.clone();
            thread::Builder::new()
                .name("session-recorder".to_owned())
                .spawn(move || {
                    if let Err(e) = write_recording(&queue, &header, &part, &done) {
                        queue.fail(e.to_string());
                    }
                    queue.finished();
                    store.unregister(&id);
                })
        };
        let writer = match writer {
            Ok(h) => h,
            Err(e) => {
                store.unregister(&id);
                queue.close(EndReason::Abandoned, Purge::Everything, now);
                return Err(StartError::Spawn(e));
            }
        };
        Ok(Self {
            id,
            queue,
            streams: grant.streams,
            grant,
            origin: now,
            last_now: now,
            next_seq: 0,
            privacy,
            withheld: None,
            applied: HashMap::new(),
            writer: Some(writer),
        })
    }

    /// The recording's id.
    pub fn id(&self) -> &RecordingId {
        &self.id
    }

    /// The recording's state.
    pub fn state(&self) -> RecorderState {
        self.queue.status()
    }

    /// Ends the recording if the grant has ended by `now`.
    pub fn poll(&mut self, now: MonoInstant) {
        self.admit(now);
    }

    fn admit(&mut self, now: MonoInstant) -> bool {
        self.last_now = self.last_now.max(now);
        if !self.queue.is_open() {
            return false;
        }
        if let Err(reason) = self.grant.live_at(now) {
            self.end(reason, now);
            return false;
        }
        true
    }

    fn end(&mut self, reason: EndReason, now: MonoInstant) {
        self.flush_withheld(now);
        let purge = if reason == EndReason::Withdrawn {
            Purge::Everything
        } else {
            Purge::Nothing
        };
        self.queue.close(reason, purge, now);
    }

    fn rel(&self, now: MonoInstant) -> RelTime {
        RelTime::between(self.origin, now)
    }

    fn take_seq(&mut self) -> u64 {
        let s = self.next_seq;
        self.next_seq += 1;
        s
    }

    fn push(&mut self, event: Event, now: MonoInstant, guaranteed: bool) -> Pushed {
        let seq = self.take_seq();
        let item = Item::new(
            seq,
            self.rel(now),
            Body::Event(Box::new(event)),
            self.queue.privacy_epoch(),
        );
        self.queue.push(item, guaranteed)
    }

    fn record(&mut self, event: Event, now: MonoInstant) {
        if self.admit(now) {
            self.push(event, now, false);
        }
    }

    fn withhold(&mut self, reason: WithheldReason, now: MonoInstant) {
        match &mut self.withheld {
            Some((r, n)) if *r == reason => *n += 1,
            _ => {
                self.flush_withheld(now);
                self.withheld = Some((reason, 1));
            }
        }
    }

    fn flush_withheld(&mut self, now: MonoInstant) {
        if let Some((reason, frames)) = self.withheld.take() {
            self.queue.count(Tally {
                withheld_frames: frames,
                ..Tally::default()
            });
            self.push(Event::AudioWithheld { reason, frames }, now, true);
        }
    }

    /// An EMA1 uplink frame exactly as received, with its ingress ordinal. It is kept only
    /// when the grant covers its stream and audio is allowed.
    pub fn frame(
        &mut self,
        ingress: IngressOrdinal,
        frame: &Bytes,
        now: MonoInstant,
    ) -> FrameTaken {
        if !self.admit(now) {
            return FrameTaken::Ended;
        }
        let Ok(packet) = Packet::parse(frame.clone()) else {
            return FrameTaken::Invalid;
        };
        let header = packet.header();
        if !self.streams.allows(header.kind) {
            return FrameTaken::NotInScope;
        }
        if self.privacy.muted {
            self.withhold(WithheldReason::Muted, now);
            return FrameTaken::Withheld;
        }
        if !self.privacy.capture_permitted {
            self.withhold(WithheldReason::CaptureNotPermitted, now);
            return FrameTaken::Withheld;
        }
        if header.flags.contains(Flags::MUTED) {
            self.withhold(WithheldReason::MutedFrame, now);
            return FrameTaken::Withheld;
        }
        self.flush_withheld(now);
        let seq = self.take_seq();
        let item = Item::new(
            seq,
            self.rel(now),
            Body::Frame {
                ingress,
                frame: frame.clone(),
            },
            self.queue.privacy_epoch(),
        );
        match self.queue.push(item, false) {
            Pushed::Queued => FrameTaken::Queued,
            Pushed::Lost => FrameTaken::Lost,
            Pushed::Closed => FrameTaken::Ended,
        }
    }

    /// `stream.open`.
    pub fn stream_opened(
        &mut self,
        ingress: IngressOrdinal,
        stream: StreamId,
        epoch: Epoch,
        now: MonoInstant,
    ) {
        self.record(
            Event::StreamOpened {
                ingress,
                stream: stream.into(),
                epoch,
            },
            now,
        );
    }

    /// `stream.end`.
    pub fn stream_ended(
        &mut self,
        ingress: IngressOrdinal,
        stream: StreamId,
        epoch: Epoch,
        now: MonoInstant,
    ) {
        self.record(
            Event::StreamEnded {
                ingress,
                stream: stream.into(),
                epoch,
            },
            now,
        );
    }

    /// The privacy state changed. Call it before handing on any audio queued behind the
    /// change. On mute (or when capture stops being permitted) every frame not yet written is
    /// discarded, and no audio is kept until it is allowed again.
    pub fn privacy(
        &mut self,
        ingress: Option<IngressOrdinal>,
        state: PrivacyState,
        now: MonoInstant,
    ) {
        if !self.admit(now) {
            return;
        }
        let was = self.privacy.audio_allowed();
        self.privacy = state;
        if was && !state.audio_allowed() {
            self.queue.mute();
        }
        self.flush_withheld(now);
        self.push(Event::Privacy { ingress, state }, now, true);
    }

    /// New configuration or model identities came into force.
    pub fn revisions(&mut self, revisions: Revisions, now: MonoInstant) {
        self.record(Event::Revisions(revisions), now);
    }

    /// The runtime cancelled something.
    pub fn cancelled(&mut self, target: CancelTarget, now: MonoInstant) {
        if self.admit(now) {
            self.push(Event::Cancelled(target), now, true);
        }
    }

    fn wake(&mut self, entry: WakeEntry, now: MonoInstant) {
        self.record(Event::Wake(entry), now);
    }

    /// `WakeDecider::arrive`, and the decision it made at once.
    pub fn wake_arrived(
        &mut self,
        candidate: &WakeCandidate,
        conditions: ArrivalConditions,
        policy: &WakePolicy,
        decision: Option<WakeDecision>,
        now: MonoInstant,
    ) {
        self.wake(
            WakeEntry::Arrived {
                candidate: candidate.clone(),
                conditions: conditions.into(),
                policy: policy.clone(),
                decision: decision.map(Into::into),
            },
            now,
        );
    }

    /// `WakeDecider::reference_scored`, and its decision.
    pub fn wake_reference(
        &mut self,
        candidate_id: &str,
        score: ReferenceScore,
        decision: Option<WakeDecision>,
        now: MonoInstant,
    ) {
        self.wake(
            WakeEntry::Reference {
                candidate_id: candidate_id.to_owned(),
                score,
                decision: decision.map(Into::into),
            },
            now,
        );
    }

    /// `WakeDecider::echo_compared`, and its decision.
    pub fn wake_echo(
        &mut self,
        candidate_id: &str,
        echo: CandidateEcho,
        decision: Option<WakeDecision>,
        now: MonoInstant,
    ) {
        self.wake(
            WakeEntry::Echo {
                candidate_id: candidate_id.to_owned(),
                echo,
                decision: decision.map(Into::into),
            },
            now,
        );
    }

    /// `WakeDecider::verified`, and its decision. The transcript is kept only under a
    /// transcripts grant.
    pub fn wake_verified(
        &mut self,
        candidate_id: &str,
        result: &VerifierResult,
        decision: Option<WakeDecision>,
        now: MonoInstant,
    ) {
        let result = VerifierRecord::of(result, &self.streams);
        self.wake(
            WakeEntry::Verified {
                candidate_id: candidate_id.to_owned(),
                result,
                decision: decision.map(Into::into),
            },
            now,
        );
    }

    /// `WakeDecider::on_timer`, and its decision.
    pub fn wake_timer(
        &mut self,
        candidate_id: &str,
        decision: Option<WakeDecision>,
        now: MonoInstant,
    ) {
        self.wake(
            WakeEntry::Timer {
                candidate_id: candidate_id.to_owned(),
                decision: decision.map(Into::into),
            },
            now,
        );
    }

    fn turn(&mut self, turn: TurnId, step: TurnStep, now: MonoInstant) {
        self.record(Event::Turn { turn: turn.0, step }, now);
    }

    /// `TurnPolicy::new`.
    pub fn turn_opened(
        &mut self,
        turn: TurnId,
        cause: TurnCause,
        config: &TurnConfig,
        spec: &TurnSpec,
        now: MonoInstant,
    ) {
        let spec = TurnSpecRecord::of(spec, self.origin);
        self.turn(
            turn,
            TurnStep::Opened {
                cause,
                config: Box::new(*config),
                spec,
            },
            now,
        );
    }

    /// `push_device_cells`. Kept only under a cells grant; otherwise the recording notes that
    /// this evidence was withheld.
    pub fn turn_device_cells(
        &mut self,
        turn: TurnId,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
        now: MonoInstant,
    ) {
        let step = if self.streams.cells {
            TurnStep::DeviceCells {
                epoch,
                first,
                cells: cells.iter().map(|&c| c.into()).collect(),
            }
        } else {
            TurnStep::DeviceCellsWithheld {
                epoch,
                first,
                count: cells.len() as u64,
            }
        };
        self.turn(turn, step, now);
    }

    /// `push_vad`.
    pub fn turn_vad(
        &mut self,
        turn: TurnId,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
        now: MonoInstant,
    ) {
        let vad = vad.iter().map(|&v| VadRecord::from(v)).collect();
        self.turn(turn, TurnStep::Vad { epoch, first, vad }, now);
    }

    /// `push_echo`.
    pub fn turn_echo(
        &mut self,
        turn: TurnId,
        epoch: Epoch,
        first: CellIndex,
        cells: &[EchoCell],
        now: MonoInstant,
    ) {
        let cells = cells.iter().map(|&c| EchoCellRecord::from(c)).collect();
        self.turn(
            turn,
            TurnStep::Echo {
                epoch,
                first,
                cells,
            },
            now,
        );
    }

    /// `mark_gap`.
    pub fn turn_gap(&mut self, turn: TurnId, epoch: Epoch, span: SampleSpan, now: MonoInstant) {
        self.turn(turn, TurnStep::Gap { epoch, span }, now);
    }

    /// `push_asr`. The tokens are kept only under a transcripts grant.
    pub fn turn_asr(
        &mut self,
        turn: TurnId,
        epoch: Epoch,
        observation: &AsrObservation,
        now: MonoInstant,
    ) {
        let observation = AsrRecord::of(observation, &self.streams);
        self.turn(turn, TurnStep::Asr { epoch, observation }, now);
    }

    /// `TurnPolicy::recognizer_answer`: HA's answer handed to the policy at this point.
    pub fn turn_recognizer_answer(
        &mut self,
        turn: TurnId,
        answer: &RecognizerAnswer,
        now: MonoInstant,
    ) {
        let step = TurnStep::RecognizerAnswer {
            text: Transcript::gate(&answer.text, &self.streams),
            outcome: answer.outcome.into(),
        };
        self.turn(turn, step, now);
    }

    /// `TurnPolicy::pause_server_asked`: the pause decode through `pause` went to the pause
    /// server at this point.
    pub fn turn_pause_server_asked(&mut self, turn: TurnId, pause: Sample, now: MonoInstant) {
        self.turn(turn, TurnStep::PauseServerAsked { pause }, now);
    }

    /// `TurnPolicy::pause_server_answer`: the pause server's answer handed to the policy at
    /// this point. Its words are kept only under a transcripts grant.
    pub fn turn_pause_server_answer(
        &mut self,
        turn: TurnId,
        answer: &PauseServerAnswer,
        now: MonoInstant,
    ) {
        let step = TurnStep::PauseServerAnswer {
            pause: answer.pause,
            outcome: PauseServerOutcome::of(answer, &self.streams),
        };
        self.turn(turn, step, now);
    }

    /// `TurnPolicy::advance`: the grammar answers from the [`GrammarTap`] it ran with, and the
    /// decisions it appended. `policy` is read for the recognizer answers that took effect.
    pub fn turn_advanced(
        &mut self,
        turn: TurnId,
        policy: &TurnPolicy,
        grammar: Vec<GrammarAnswer>,
        decisions: &[Decision],
        now: MonoInstant,
    ) {
        let recognizer: Vec<(String, Option<Sample>)> = policy
            .trace()
            .recognizer
            .iter()
            .map(|r| (r.text.clone(), r.applied_at))
            .collect();
        self.turn_advanced_with(turn, &recognizer, grammar, decisions, now);
    }

    /// [`SessionRecorder::turn_advanced`] for a runtime that no longer holds the policy:
    /// `recognizer` is each recognizer answer's text and the block it took effect at so
    /// far, in the order the policy's trace lists them.
    pub fn turn_advanced_with(
        &mut self,
        turn: TurnId,
        recognizer: &[(String, Option<Sample>)],
        grammar: Vec<GrammarAnswer>,
        decisions: &[Decision],
        now: MonoInstant,
    ) {
        let scope = self.streams;
        let seen = self.applied.entry(turn.0).or_default();
        let mut applied = Vec::new();
        for (i, (text, applied_at)) in recognizer.iter().enumerate() {
            if seen.len() <= i {
                seen.resize(i + 1, false);
            }
            if let Some(at) = applied_at
                && !seen[i]
            {
                seen[i] = true;
                applied.push(AppliedAnswer {
                    text: Transcript::gate(text, &scope),
                    at: *at,
                });
            }
        }
        let step = TurnStep::Advanced {
            grammar: if scope.transcripts {
                grammar
            } else {
                Vec::new()
            },
            applied,
            decisions: decisions
                .iter()
                .map(|d| DecisionRecord::of(d, &scope))
                .collect(),
        };
        self.turn(turn, step, now);
    }

    /// `TurnPolicy::on_timer`, and the decisions it appended.
    pub fn turn_timer(&mut self, turn: TurnId, decisions: &[Decision], now: MonoInstant) {
        let decisions = decisions
            .iter()
            .map(|d| DecisionRecord::of(d, &self.streams))
            .collect();
        self.turn(turn, TurnStep::Timer { decisions }, now);
    }

    /// A control event routed to the turn, and the decisions it appended.
    pub fn turn_control(
        &mut self,
        turn: TurnId,
        control: TurnControl,
        decisions: &[Decision],
        now: MonoInstant,
    ) {
        let decisions = decisions
            .iter()
            .map(|d| DecisionRecord::of(d, &self.streams))
            .collect();
        self.turn(turn, TurnStep::Control { control, decisions }, now);
    }

    /// An input the runtime gave an echo analyzer (any [`EchoStep`] but `Polled` and
    /// `Compared`, which have their own methods). Loudness cells are kept only under a cells
    /// grant.
    pub fn echo_input(&mut self, analyzer: AnalyzerId, step: EchoStep, now: MonoInstant) {
        let step = match step {
            EchoStep::DeviceCells {
                epoch,
                first,
                cells,
            } if !self.streams.cells => EchoStep::DeviceCellsWithheld {
                epoch,
                first,
                count: cells.len() as u64,
            },
            other => other,
        };
        self.record(Event::Echo { analyzer, step }, now);
    }

    /// `EchoAnalyzer::poll`, with the labels it produced.
    pub fn echo_polled(&mut self, analyzer: AnalyzerId, labels: &[CellLabel], now: MonoInstant) {
        let labels = labels
            .iter()
            .map(|l| LabelRecord {
                cell: l.cell,
                echo: l.echo.into(),
            })
            .collect();
        self.record(
            Event::Echo {
                analyzer,
                step: EchoStep::Polled { labels },
            },
            now,
        );
    }

    /// `EchoAnalyzer::compare_candidate`, with its result.
    pub fn echo_compared(
        &mut self,
        analyzer: AnalyzerId,
        support: SampleSpan,
        candidate_id: Option<&str>,
        result: Option<&CandidateComparison>,
        now: MonoInstant,
    ) {
        self.record(
            Event::Echo {
                analyzer,
                step: EchoStep::Compared {
                    support,
                    candidate_id: candidate_id.map(str::to_owned),
                    echo: result.map(|c| c.echo),
                },
            },
            now,
        );
    }

    fn body(&mut self, json: &serde_json::Value) -> HaBody {
        if !self.streams.transcripts {
            return HaBody::Withheld;
        }
        let (json, redactions) = redact(json);
        if redactions > 0 {
            self.queue.count(Tally {
                credential_redactions: u64::from(redactions),
                ..Tally::default()
            });
        }
        HaBody::Kept { json, redactions }
    }

    /// An HA request as sent. Credentials are removed here, before anything is queued; under a
    /// grant without transcripts the body is withheld entirely.
    pub fn ha_request(
        &mut self,
        exchange: ExchangeId,
        turn: Option<TurnId>,
        operation: HaOperation,
        body: &serde_json::Value,
        now: MonoInstant,
    ) {
        if !self.admit(now) {
            return;
        }
        let body = self.body(body);
        self.push(
            Event::Ha(HaEntry::Request {
                exchange,
                turn: turn.map(|t| t.0),
                operation,
                body,
            }),
            now,
            false,
        );
    }

    /// HA's reply, or the failure, for an exchange.
    pub fn ha_reply(
        &mut self,
        exchange: ExchangeId,
        reply: Result<&serde_json::Value, &HaError>,
        now: MonoInstant,
    ) {
        if !self.admit(now) {
            return;
        }
        let outcome = match reply {
            Ok(json) => Ok(self.body(json)),
            Err(e) => Err(match e {
                HaError::NotDispatched(_) => HaFailure::NotDispatched,
                HaError::ConnectionLost => HaFailure::ConnectionLost,
                HaError::Rejected { code, .. } => HaFailure::Rejected {
                    code: redact_text(code),
                },
                HaError::Timeout => HaFailure::Timeout,
                HaError::BadResponse(_) => HaFailure::BadResponse,
            }),
        };
        self.push(Event::Ha(HaEntry::Reply { exchange, outcome }), now, false);
    }

    /// The session ended: records why and ends the recording.
    pub fn disconnected(mut self, reason: DisconnectReason, now: MonoInstant) -> Finishing {
        if self.admit(now) {
            self.push(Event::Disconnected { reason }, now, true);
            self.end(EndReason::Disconnected, now);
        }
        self.finishing()
    }

    /// Ends the recording.
    pub fn finish(mut self, now: MonoInstant) -> Finishing {
        if self.admit(now) {
            self.end(EndReason::Finished, now);
        }
        self.finishing()
    }

    fn finishing(&mut self) -> Finishing {
        Finishing {
            queue: Arc::clone(&self.queue),
            writer: self.writer.take(),
        }
    }

    #[cfg(test)]
    pub(crate) fn queue(&self) -> &Arc<Queue> {
        &self.queue
    }
}

impl Drop for SessionRecorder {
    /// A recorder dropped without [`SessionRecorder::finish`] still ends its file in order,
    /// marked `abandoned`. The writer thread is left to finish on its own.
    fn drop(&mut self) {
        if self.writer.is_some() && self.queue.is_open() {
            self.flush_withheld(self.last_now);
            self.queue
                .close(EndReason::Abandoned, Purge::Nothing, self.last_now);
        }
    }
}

fn millis(d: std::time::Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The writer thread's work: create the file, write the header, then every batch, then the
/// end record, and publish the file under its final name.
fn write_recording(queue: &Queue, header: &Header, part: &Path, done: &Path) -> io::Result<()> {
    if queue.is_aborted() {
        return Ok(());
    }
    if let Some(dir) = part.parent() {
        private_dir(dir)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(part)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut out = BufWriter::new(file);
    write_preamble(&mut out)?;
    write_json(&mut out, RecordType::Header, header)?;
    out.flush()?;
    loop {
        let batch = queue.take();
        if batch.aborted {
            drop(out);
            remove_quietly(part);
            return Ok(());
        }
        if batch.lost > 0 {
            write_json(
                &mut out,
                RecordType::Marker,
                &Marker::Lost {
                    entries: batch.lost,
                },
            )?;
        }
        if batch.discarded_frames + batch.discarded_events > 0 {
            write_json(
                &mut out,
                RecordType::Marker,
                &Marker::Discarded {
                    frames: batch.discarded_frames,
                    events: batch.discarded_events,
                },
            )?;
        }
        for item in batch.items {
            match item.body {
                Body::Frame { ingress, frame } => {
                    // Taken before a mute but not yet written: discarded, not saved.
                    if item.privacy_epoch != queue.privacy_epoch() {
                        queue.discarded_by_writer();
                        continue;
                    }
                    write_frame(&mut out, item.seq, item.t.0, ingress.0, &frame)?;
                }
                Body::Event(event) => {
                    let entry = Entry {
                        seq: item.seq,
                        t: item.t,
                        event: *event,
                    };
                    write_json(&mut out, RecordType::Event, &entry)?;
                }
            }
        }
        out.flush()?;
        if let Some(closing) = batch.closing {
            let (tally, last_seq) = queue.totals();
            write_json(
                &mut out,
                RecordType::End,
                &End {
                    t: closing.t,
                    reason: closing.reason,
                    tally,
                    last_seq,
                },
            )?;
            out.flush()?;
            let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
            file.sync_all()?;
            drop(file);
            if queue.is_aborted() {
                remove_quietly(part);
                return Ok(());
            }
            fs::rename(part, done)?;
            if let Some(dir) = done.parent()
                && let Ok(d) = fs::File::open(dir)
            {
                let _ = d.sync_all();
            }
            return Ok(());
        }
    }
}

fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path);
}
