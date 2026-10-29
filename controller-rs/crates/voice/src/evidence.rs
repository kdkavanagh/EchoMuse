//! What the session recorder needs for decision replay (Rust controller evaluation §6.2):
//! every input the wake deciders and turn policies took, with the decision each produced,
//! at the point it took effect.
//!
//! While the runtime says a recording runs ([`crate::Voice::set_recording`]), the machine
//! queues each as an [`Effect::Evidence`] beside the call it describes: a candidate's
//! arrival and each answer its decider took (reference score, echo comparison,
//! verification, a deadline that was due), and for each turn the policy it opened with,
//! every evidence push its assembler accepted, the recognizer answers handed to it, the
//! pause server's requests and answers, each `advance` with the grammar's answers and the
//! decisions it made, deadlines and control
//! events (mute, session loss, overruns, worker errors). A push the assembler refused is
//! not noted: the replayed policy is given exactly what the live one accepted. Nothing is
//! queued while no recording runs, so an ordinary session copies nothing.
//!
//! Times are the machine's monotonic clock ([`crate::Voice::clock_origin`] places it);
//! the runtime converts them to the recorder's. The early-answer watch's own policy is
//! not recorded: the reply turn it opens is, with the evidence it was given.

use echomuse_recorder::{GrammarAnswer, TurnCause, TurnControl};
use echomuse_turn::{
    ArrivalConditions, AsrObservation, CandidateEcho, CellIndex, Decision, DeviceInput, EchoCell,
    Epoch, EvidenceAssembler, EvidenceError, EvidencePlan, MonoInstant, PauseServerAnswer,
    RecognizerAnswer, ReferenceScore, Sample, SampleSpan, TurnConfig, TurnSpec, VadInput,
    VerifierResult, WakeCandidate, WakeDecision, WakePolicy,
};

use crate::io::{Effect, Now};
use crate::machine::Voice;

/// One input a decider or policy took, as the recorder records it.
#[derive(Clone, Debug)]
pub enum Evidence {
    /// `WakeDecider::arrive`, and the decision it made at once.
    WakeArrived {
        candidate: WakeCandidate,
        conditions: ArrivalConditions,
        policy: WakePolicy,
        decision: Option<WakeDecision>,
    },
    /// `reference_scored`.
    WakeReference {
        candidate_id: String,
        score: ReferenceScore,
        decision: Option<WakeDecision>,
    },
    /// `echo_compared`.
    WakeEcho {
        candidate_id: String,
        echo: CandidateEcho,
        decision: Option<WakeDecision>,
    },
    /// `verified`.
    WakeVerified {
        candidate_id: String,
        result: VerifierResult,
        decision: Option<WakeDecision>,
    },
    /// `on_timer` with its deadline due.
    WakeTimer {
        candidate_id: String,
        decision: Option<WakeDecision>,
    },
    /// `TurnPolicy::new`.
    TurnOpened {
        turn: u64,
        cause: TurnCause,
        config: TurnConfig,
        spec: TurnSpec,
    },
    /// Accepted loudness cells.
    Cells {
        turn: u64,
        epoch: Epoch,
        first: CellIndex,
        cells: Vec<DeviceInput>,
    },
    /// Accepted VAD.
    Vad {
        turn: u64,
        epoch: Epoch,
        first: CellIndex,
        vad: Vec<VadInput>,
    },
    /// Accepted echo labels.
    Echo {
        turn: u64,
        epoch: Epoch,
        first: CellIndex,
        cells: Vec<EchoCell>,
    },
    /// An accepted gap.
    Gap {
        turn: u64,
        epoch: Epoch,
        span: SampleSpan,
    },
    /// An accepted ASR observation.
    Asr {
        turn: u64,
        epoch: Epoch,
        observation: AsrObservation,
    },
    /// HA's recognizer answer, handed to the policy.
    RecognizerAnswer { turn: u64, answer: RecognizerAnswer },
    /// The pause decode through `pause` went to the pause server.
    PauseServerAsked { turn: u64, pause: Sample },
    /// The pause server's answer, handed to the policy.
    PauseServerAnswer {
        turn: u64,
        answer: PauseServerAnswer,
    },
    /// `advance`: the grammar's answers, each recognizer answer's text and the block it
    /// took effect at so far, and the decisions it appended.
    Advanced {
        turn: u64,
        grammar: Vec<GrammarAnswer>,
        recognizer: Vec<(String, Option<Sample>)>,
        decisions: Vec<Decision>,
    },
    /// `on_timer` with a deadline due, and its decisions.
    Timer { turn: u64, decisions: Vec<Decision> },
    /// A control event routed to the policy, and its decisions.
    Control {
        turn: u64,
        control: TurnControl,
        decisions: Vec<Decision>,
    },
    /// The turn was cancelled.
    Cancelled { turn: u64 },
    /// A request to Home Assistant as it was sent, under the machine's call id.
    HaRequest {
        exchange: u64,
        operation: echomuse_recorder::HaOperation,
        body: serde_json::Value,
    },
    /// Home Assistant's answer to that request, as the machine was given it.
    HaReply {
        exchange: u64,
        body: serde_json::Value,
    },
}
/// The recorder's operation and a summary body for a request to Home Assistant: its text
/// where it has one, and only the length of STT audio (the frames carry the audio).
pub(crate) fn ha_request(
    request: &crate::io::HaRequest,
) -> (echomuse_recorder::HaOperation, serde_json::Value) {
    use crate::io::HaRequest;
    use echomuse_recorder::HaOperation;
    use serde_json::json;
    match request {
        HaRequest::Pipeline { ha_device } => (
            HaOperation::PipelineFor,
            json!({ "ha_device": format!("{ha_device:?}") }),
        ),
        HaRequest::Stt { audio, .. } => (HaOperation::Stt, json!({ "samples": audio.len() })),
        HaRequest::IntentTts {
            text, conversation, ..
        } => (
            HaOperation::IntentTts,
            json!({ "text": text, "conversation": format!("{conversation:?}") }),
        ),
        HaRequest::Tts { text, .. } => (HaOperation::Tts, json!({ "text": text })),
        HaRequest::Recognize { text, .. } => (HaOperation::Recognize, json!({ "text": text })),
        HaRequest::TimerStatus { ha_device } => (
            HaOperation::TimerStatus,
            json!({ "ha_device": format!("{ha_device:?}") }),
        ),
        HaRequest::CallIntent { intent, .. } => (
            HaOperation::CallIntent,
            json!({ "intent": format!("{intent:?}") }),
        ),
    }
}

impl Evidence {
    /// Hands this evidence to a session recorder. `clock` maps the machine's clock to the
    /// recorder's ([`crate::Voice::clock_origin`]); `at` is when it took effect, on the
    /// machine's clock.
    pub fn record(
        self,
        recorder: &mut echomuse_recorder::SessionRecorder,
        at: MonoInstant,
        clock: impl Fn(MonoInstant) -> MonoInstant,
    ) {
        use echomuse_core::types::TurnId;
        let now = clock(at);
        match self {
            Self::WakeArrived {
                candidate,
                conditions,
                policy,
                decision,
            } => recorder.wake_arrived(&candidate, conditions, &policy, decision, now),
            Self::WakeReference {
                candidate_id,
                score,
                decision,
            } => recorder.wake_reference(&candidate_id, score, decision, now),
            Self::WakeEcho {
                candidate_id,
                echo,
                decision,
            } => recorder.wake_echo(&candidate_id, echo, decision, now),
            Self::WakeVerified {
                candidate_id,
                result,
                decision,
            } => recorder.wake_verified(&candidate_id, &result, decision, now),
            Self::WakeTimer {
                candidate_id,
                decision,
            } => recorder.wake_timer(&candidate_id, decision, now),
            Self::TurnOpened {
                turn,
                cause,
                config,
                mut spec,
            } => {
                if let echomuse_turn::TurnKind::Reply(reply) = &mut spec.kind {
                    reply.wall_deadline = clock(reply.wall_deadline);
                }
                recorder.turn_opened(TurnId(turn), cause, &config, &spec, now);
            }
            Self::Cells {
                turn,
                epoch,
                first,
                cells,
            } => recorder.turn_device_cells(TurnId(turn), epoch, first, &cells, now),
            Self::Vad {
                turn,
                epoch,
                first,
                vad,
            } => recorder.turn_vad(TurnId(turn), epoch, first, &vad, now),
            Self::Echo {
                turn,
                epoch,
                first,
                cells,
            } => recorder.turn_echo(TurnId(turn), epoch, first, &cells, now),
            Self::Gap { turn, epoch, span } => recorder.turn_gap(TurnId(turn), epoch, span, now),
            Self::Asr {
                turn,
                epoch,
                observation,
            } => recorder.turn_asr(TurnId(turn), epoch, &observation, now),
            Self::RecognizerAnswer { turn, answer } => {
                recorder.turn_recognizer_answer(TurnId(turn), &answer, now);
            }
            Self::PauseServerAsked { turn, pause } => {
                recorder.turn_pause_server_asked(TurnId(turn), pause, now);
            }
            Self::PauseServerAnswer { turn, answer } => {
                recorder.turn_pause_server_answer(TurnId(turn), &answer, now);
            }
            Self::Advanced {
                turn,
                grammar,
                recognizer,
                decisions,
            } => recorder.turn_advanced_with(TurnId(turn), &recognizer, grammar, &decisions, now),
            Self::Timer { turn, decisions } => recorder.turn_timer(TurnId(turn), &decisions, now),
            Self::Control {
                turn,
                control,
                decisions,
            } => recorder.turn_control(TurnId(turn), control, &decisions, now),
            Self::Cancelled { turn } => {
                recorder.cancelled(echomuse_recorder::CancelTarget::Turn { turn }, now);
            }
            Self::HaRequest {
                exchange,
                operation,
                body,
            } => recorder.ha_request(
                echomuse_recorder::ExchangeId(exchange),
                None,
                operation,
                &body,
                now,
            ),
            Self::HaReply { exchange, body } => {
                recorder.ha_reply(echomuse_recorder::ExchangeId(exchange), Ok(&body), now);
            }
        }
    }
}

/// Where evidence pushes go: a bare assembler, or one whose accepted pushes are noted.
pub(crate) trait Pushes {
    fn plan(&self) -> &EvidencePlan;
    fn push_device_cells(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
    ) -> Result<(), EvidenceError>;
    fn push_vad(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
    ) -> Result<(), EvidenceError>;
    fn push_echo(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        echo: &[EchoCell],
    ) -> Result<(), EvidenceError>;
    fn mark_gap(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EvidenceError>;
    fn push_asr(&mut self, epoch: Epoch, obs: AsrObservation) -> Result<(), EvidenceError>;
}

impl Pushes for EvidenceAssembler {
    fn plan(&self) -> &EvidencePlan {
        EvidenceAssembler::plan(self)
    }
    fn push_device_cells(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
    ) -> Result<(), EvidenceError> {
        EvidenceAssembler::push_device_cells(self, epoch, first, cells)
    }
    fn push_vad(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
    ) -> Result<(), EvidenceError> {
        EvidenceAssembler::push_vad(self, epoch, first, vad)
    }
    fn push_echo(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        echo: &[EchoCell],
    ) -> Result<(), EvidenceError> {
        EvidenceAssembler::push_echo(self, epoch, first, echo)
    }
    fn mark_gap(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EvidenceError> {
        EvidenceAssembler::mark_gap(self, epoch, span)
    }
    fn push_asr(&mut self, epoch: Epoch, obs: AsrObservation) -> Result<(), EvidenceError> {
        EvidenceAssembler::push_asr(self, epoch, obs)
    }
}

/// A turn's assembler whose accepted pushes are queued as evidence.
pub(crate) struct Noting<'a> {
    pub(crate) assembler: &'a mut EvidenceAssembler,
    /// The effects to queue them on, the recorder's turn and the time; `None` while no
    /// recording runs.
    pub(crate) notes: Option<(&'a mut Vec<Effect>, u64, MonoInstant)>,
}

impl Noting<'_> {
    fn note(&mut self, evidence: impl FnOnce(u64) -> Evidence) {
        if let Some((effects, turn, at)) = self.notes.as_mut() {
            effects.push(Effect::Evidence {
                at: *at,
                evidence: Box::new(evidence(*turn)),
            });
        }
    }
}

impl Pushes for Noting<'_> {
    fn plan(&self) -> &EvidencePlan {
        self.assembler.plan()
    }
    fn push_device_cells(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
    ) -> Result<(), EvidenceError> {
        self.assembler.push_device_cells(epoch, first, cells)?;
        self.note(|turn| Evidence::Cells {
            turn,
            epoch,
            first,
            cells: cells.to_vec(),
        });
        Ok(())
    }
    fn push_vad(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
    ) -> Result<(), EvidenceError> {
        self.assembler.push_vad(epoch, first, vad)?;
        self.note(|turn| Evidence::Vad {
            turn,
            epoch,
            first,
            vad: vad.to_vec(),
        });
        Ok(())
    }
    fn push_echo(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        echo: &[EchoCell],
    ) -> Result<(), EvidenceError> {
        self.assembler.push_echo(epoch, first, echo)?;
        self.note(|turn| Evidence::Echo {
            turn,
            epoch,
            first,
            cells: echo.to_vec(),
        });
        Ok(())
    }
    fn mark_gap(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EvidenceError> {
        self.assembler.mark_gap(epoch, span)?;
        self.note(|turn| Evidence::Gap { turn, epoch, span });
        Ok(())
    }
    fn push_asr(&mut self, epoch: Epoch, obs: AsrObservation) -> Result<(), EvidenceError> {
        let noted = self.notes.is_some().then(|| obs.clone());
        self.assembler.push_asr(epoch, obs)?;
        if let Some(observation) = noted {
            self.note(|turn| Evidence::Asr {
                turn,
                epoch,
                observation,
            });
        }
        Ok(())
    }
}

impl Voice {
    /// Queues one piece of evidence while a recording runs.
    pub(crate) fn note(&mut self, now: Now, evidence: impl FnOnce() -> Evidence) {
        if self.recording {
            let at = self.mono(now.at);
            self.link.effects.push(Effect::Evidence {
                at,
                evidence: Box::new(evidence()),
            });
        }
    }

    /// The live turn's assembler, noting accepted pushes while a recording runs.
    pub(crate) fn turn_pushes(&mut self, now: Now) -> Option<Noting<'_>> {
        let at = self.mono(now.at);
        let recording = self.recording;
        let turn = self.turn.as_mut()?;
        let id = turn.evidence_id;
        let assembler = turn.policy.as_mut()?.evidence_mut();
        Some(Noting {
            assembler,
            notes: recording.then_some((&mut self.link.effects, id, at)),
        })
    }

    /// The recorder's identity for the next turn policy.
    pub(crate) fn next_evidence_id(&mut self) -> u64 {
        self.evidence_turns += 1;
        self.evidence_turns
    }

    /// Notes a turn policy that just opened.
    pub(crate) fn note_opened(
        &mut self,
        id: u64,
        cause: TurnCause,
        config: TurnConfig,
        spec: TurnSpec,
        now: Now,
    ) {
        self.note(now, || Evidence::TurnOpened {
            turn: id,
            cause,
            config,
            spec,
        });
    }

    /// Notes a control event the live turn's policy took, with the decisions it made.
    pub(crate) fn note_control(&mut self, control: TurnControl, decisions: &[Decision], now: Now) {
        if let Some(turn) = self.turn.as_ref().filter(|t| t.policy.is_some()) {
            let id = turn.evidence_id;
            let decisions = decisions.to_vec();
            self.note(now, || Evidence::Control {
                turn: id,
                control,
                decisions,
            });
        }
    }

    /// Opens a turn policy under the session's settings, and notes it with `cause`.
    /// Returns the policy and the recorder's identity for it.
    pub(crate) fn open_policy(
        &mut self,
        cause: TurnCause,
        spec: TurnSpec,
        now: Now,
    ) -> Result<(echomuse_turn::TurnPolicy, u64), EvidenceError> {
        let settings = std::sync::Arc::clone(&self.settings);
        let config = self.turn_config(&settings, &spec.kind);
        let policy = echomuse_turn::TurnPolicy::new(config, spec.clone(), self.mono(now.at))?;
        let id = self.next_evidence_id();
        self.note_opened(id, cause, config, spec, now);
        Ok((policy, id))
    }

    /// A fresh policy's assembler, noting accepted pushes under turn `id`.
    pub(crate) fn noting<'a>(
        &'a mut self,
        assembler: &'a mut EvidenceAssembler,
        id: u64,
        now: Now,
    ) -> Noting<'a> {
        let at = self.mono(now.at);
        let notes = self.recording.then_some((&mut self.link.effects, id, at));
        Noting { assembler, notes }
    }
}
