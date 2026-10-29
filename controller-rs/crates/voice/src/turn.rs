//! A turn: from its start (an accepted wake, a button press, a reply window) through
//! endpointing, the STT copy and Home Assistant's transcript, routing, the answer, and
//! its end (spec §4.6, §4.7, §3.3.3).
//!
//! The turn's scope is derived from the capture scope, so a mute, the session's end or
//! a newer turn cancels it, and every request made for it carries that scope: a request
//! not yet sent is not sent, an answer that arrives later is dropped by identity, and an
//! intent run that may have been dispatched is recorded as an unknown outcome rather
//! than repeated.

use std::sync::Arc;

use echomuse_core::{
    services::{HaError, PipelineId, RunEvent, SatelliteTarget},
    types::{TurnId, WorkScope},
};
use echomuse_grammar::{CommandContext, ReplyKind, RingingAlert};
use echomuse_playback::{AckOutcome, FocusKind, FocusLease, FocusOwner};
use echomuse_presentation::{phase::ActorState, ring::Cue, turn_view::ViewEnd};
use echomuse_protocol::{MessageId, StreamId};
use echomuse_records::vocab::{ContinuationEnd, FollowUp, TerminalReason, TriggerType};
use echomuse_settings::DeviceSettings;
use echomuse_turn::{
    Commit, Completeness, Decision, MonoInstant, RecognizerAnswer, RecognizerError, ReplyOrigin,
    ReplySpec, Sample, Samples, TextClass, Token, TurnConfig, TurnKind, TurnPolicy, TurnSpec,
    UtteranceTrace, WakeAnchor,
    text::{TimedTranscript, locate_in_streaming, strip_final},
};
use echomuse_uplink::{CloseReason, EndCause, Owner as SubOwner, SubscriptionId, buffer::Span};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    analysis::{Analysis, StreamNews},
    arbiter::WakeGrant,
    candidate::{AcceptedWake, Candidate},
    io::{Effect, HaReply, HaRequest, Now, Outbound, PersistRequest},
    local::Expectation,
    machine::Voice,
    record::Facts,
    reply::Chain,
    speech::{MicJob, SpanJob, SpanOutput, SttCopy, SttGain},
};

/// What an outstanding request is for.
#[derive(Clone, Debug)]
pub(crate) enum Call {
    Pipeline {
        turn: Uuid,
    },
    Stt {
        turn: Uuid,
    },
    Intent {
        turn: Uuid,
    },
    Tts {
        turn: Uuid,
    },
    Recognize {
        turn: Uuid,
        text: String,
    },
    TimerStatus {
        turn: Uuid,
    },
    CancelTimers {
        turn: Uuid,
    },
    Alert {
        turn: Uuid,
    },
    Persist,
    PersistTurn,
    RunStart {
        turn: Uuid,
    },
    /// Transcript correction's decision for a turn.
    Correct {
        turn: Uuid,
    },
    /// The pause server's transcript of the turn's utterance through `pause`.
    PauseServer {
        turn: Uuid,
        pause: Sample,
    },
    /// The pause server's transcript of the turn's committed span, for its re-check.
    SpanServer {
        turn: Uuid,
    },
}

/// Where the turn is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stage {
    /// Endpointing; `end_pending` while a provisional end waits for its lookahead.
    Listening { end_pending: bool },
    /// Committed: the re-decode, the STT copy and the transcript.
    Committed,
    /// Routing and waiting for the answer.
    Thinking,
    /// The answer plays.
    Speaking,
}

/// How a committed request is being answered.
#[derive(Clone, Debug, Default)]
pub(crate) struct Answering {
    /// An HA intent run (or an HA-side ESPHome run) was dispatched and has not ended.
    pub(crate) run_open: bool,
    /// The answer's audio is playing (or about to).
    pub(crate) playing: bool,
    /// The answer's text so far, streamed.
    pub(crate) streamed: String,
    /// A reply window follows the answer.
    pub(crate) follow_up: Option<Expectation>,
    /// The answer cut the question off: its reply turn opened at once, without a window.
    pub(crate) early_answer: bool,
}

pub(crate) struct Turn {
    pub(crate) uuid: Uuid,
    pub(crate) scope: WorkScope,
    pub(crate) trigger: TriggerType,
    /// The turn's audio subscription, until it commits or closes.
    pub(crate) subscription: Option<SubscriptionId>,
    pub(crate) analysis: Option<Analysis>,
    pub(crate) wake: Option<AcceptedWake>,
    pub(crate) grant: Option<WakeGrant>,
    /// A media-context loser of arbitration: only its media commands act (§4.2).
    pub(crate) restricted: bool,
    /// The settings snapshot the turn opened with (§5.4).
    pub(crate) settings: Arc<DeviceSettings>,
    pub(crate) policy: Option<TurnPolicy>,
    pub(crate) utterance: Option<UtteranceTrace>,
    /// The latest streaming recognizer tokens, for removing the wake phrase.
    pub(crate) tokens: Vec<Token>,
    pub(crate) stage: Stage,
    /// What was going on when the turn started, for commands handled before STT.
    pub(crate) context: CommandContext,
    /// The ring the context names: (device, ring id).
    pub(crate) ring: Option<(echomuse_core::types::DeviceId, String)>,
    /// The question this turn answers, if it is a reply (or took over a reply window).
    pub(crate) expectation: Option<Expectation>,
    /// The question's turn.
    pub(crate) reply_to: Option<Uuid>,
    pub(crate) chain: Chain,
    pub(crate) origin: ReplyOrigin,
    pub(crate) input_focus: Option<FocusLease>,
    pub(crate) opened: Now,
    pub(crate) commit: Option<(Commit, Uuid, Instant)>,
    pub(crate) span_pcm: Option<Arc<[i16]>>,
    pub(crate) pipeline: Option<Result<PipelineId, HaError>>,
    pub(crate) stt_copy: Option<SttCopy>,
    pub(crate) facts: Facts,
    pub(crate) answering: Answering,
    pub(crate) evidence_broken: bool,
    /// Command speech was heard (a reply window that heard none is not a turn row).
    pub(crate) heard: bool,
    /// The reply spec the turn's utterance runs under (kept for a reopen).
    pub(crate) reply_spec: Option<ReplySpec>,
    /// An alert operation the turn waits on.
    pub(crate) alert_wait: Option<crate::local::AlertWait>,
    /// The timer request waiting on HA's timer list.
    pub(crate) timer_request: Option<crate::local::TimerRequest>,
    /// Timer cancels being sent to HA one at a time.
    pub(crate) timers_cancelling: Option<echomuse_grammar::TimerCancelRun>,
    /// The satellite run of a reply to an HA-started conversation.
    pub(crate) ha_run: Option<echomuse_esphome::voice::RunId>,
    pub(crate) ha_run_audio: Option<Arc<[i16]>>,
    pub(crate) ha_stream_url: Option<String>,
    /// The turn's native AFE records once its committed subscription closed (the
    /// committed subscription's, then the response subscription's), by period start.
    pub(crate) afe: std::collections::BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
    /// The response subscription a commit opened in an `afe_metadata` session, and when
    /// it closes at the latest (30 s after the commit).
    pub(crate) response: Option<(SubscriptionId, Instant)>,
    /// The session recorder's identity of the turn's current policy.
    pub(crate) evidence_id: u64,
    /// The latest pause decode whose audio went to the pause server.
    pub(crate) pause_asked: Option<Sample>,
}

impl Turn {
    pub(crate) fn actor_state(&self) -> ActorState {
        match self.stage {
            Stage::Listening { end_pending: false } => ActorState::Listening,
            Stage::Listening { end_pending: true } => ActorState::EndPending,
            Stage::Committed => ActorState::Committed,
            Stage::Thinking => ActorState::Thinking,
            Stage::Speaking => ActorState::Speaking,
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<MonoInstant> {
        self.policy.as_ref().and_then(TurnPolicy::next_deadline)
    }

    pub(crate) fn focus_deadlines(&self) -> Vec<Option<Instant>> {
        vec![
            self.input_focus
                .as_ref()
                .and_then(FocusLease::next_deadline),
            self.response.as_ref().map(|(_, until)| *until),
        ]
    }

    pub(crate) fn focus_ack(&mut self, id: &MessageId, outcome: &AckOutcome) -> bool {
        match self.input_focus.as_mut() {
            Some(lease) => lease.on_ack(id, outcome.clone()).is_ok(),
            None => false,
        }
    }

    pub(crate) fn note_tokens(&mut self, observations: &[echomuse_turn::AsrObservation]) {
        if let Some(last) = observations.last() {
            self.tokens = last.tokens.clone();
        }
    }

    pub(crate) fn epoch(&self) -> Option<std::num::NonZeroU64> {
        self.analysis.as_ref().map(|a| a.capture_epoch)
    }
}

/// The grammar as the turn crate asks it, answering a question's own replies first.
pub(crate) struct TurnGrammar<'a> {
    pub(crate) grammar: &'a echomuse_grammar::Grammar,
    pub(crate) reply: Option<ReplyKind>,
}

impl Completeness for TurnGrammar<'_> {
    fn classify(&self, text: &str) -> TextClass {
        use echomuse_grammar::Completeness as G;
        let class = match self.reply {
            Some(kind) => self.grammar.classify_reply(text, kind),
            None => self.grammar.classify(text),
        };
        match class {
            G::Complete => TextClass::Complete,
            G::Extendable => TextClass::Extendable,
            G::NeedsMore => TextClass::NeedsMore,
            G::Unknown => TextClass::Unknown,
        }
    }
}

impl Voice {
    pub(crate) fn turn_config(&self, settings: &DeviceSettings, kind: &TurnKind) -> TurnConfig {
        // A reply to an HA-started conversation reads no completeness: nothing to transcribe
        // at its pauses (§4.7).
        let ha_reply =
            matches!(kind, TurnKind::Reply(r) if r.origin == ReplyOrigin::HaConversation);
        TurnConfig {
            extended_utterances: settings.microphones.extended_utterances,
            recognizer_available: true,
            pause_server: !ha_reply
                && settings.microphones.pause_asr.engine()
                    == echomuse_settings::values::PauseAsrEngine::Wyoming,
            ..TurnConfig::default()
        }
    }

    fn new_turn_scope(&mut self, base: &WorkScope) -> (TurnId, WorkScope) {
        self.next_turn += 1;
        let id = TurnId(self.next_turn);
        (id, base.for_turn(id))
    }

    /// What is going on now, for commands handled before STT (§4.4).
    fn command_context(
        &self,
    ) -> (
        CommandContext,
        Option<(echomuse_core::types::DeviceId, String)>,
    ) {
        let own = self
            .rings
            .devices
            .get(&self.device.0)
            .and_then(|d| d.active.as_ref());
        let (alert, ring) = match own {
            Some(active) => (
                Some(RingingAlert {
                    kind: active.kind,
                    name: active.name.clone(),
                }),
                Some((self.device.clone(), active.id.clone())),
            ),
            None => {
                // "Stop" near a device while exactly one other device rings stops
                // that ring (§4.2).
                let elsewhere = self.rings.ringing_elsewhere(&self.device);
                match elsewhere.as_slice() {
                    [(device, active)] => (
                        Some(RingingAlert {
                            kind: active.kind,
                            name: active.name.clone(),
                        }),
                        Some((device.clone(), active.id.clone())),
                    ),
                    _ => (None, None),
                }
            }
        };
        (
            CommandContext {
                alert,
                reply_playing: self.playouts.answer_playing(),
                media_playing: self.media_playing(),
            },
            ring,
        )
    }

    /// Ends whatever turn is open because a new one starts.
    fn supersede(&mut self, now: Now) -> Option<Expectation> {
        // A wake during a reply window keeps the question (§4.5).
        let mut kept = None;
        if let Some(window) = self.window.take() {
            self.close_window_subscription(&window, now);
            if let Some(question) = window.question {
                self.resolve_continuation(question, ContinuationEnd::FollowUp(FollowUp::Wake));
            }
            kept = Some((window.expectation, window.question, window.chain));
            self.pending_reply_context = kept.clone();
        }
        if self.turn.is_some() {
            self.end_turn(TerminalReason::Superseded, now);
        }
        // The device's own answer stops when a new turn begins.
        self.playouts
            .interrupt_answers(&mut self.render, &mut self.link, now);
        kept.map(|k| k.0)
    }

    pub(crate) fn start_wake_turn(
        &mut self,
        candidate: Candidate,
        grant: Option<WakeGrant>,
        restricted: bool,
        now: Now,
    ) {
        let (context, ring) = self.command_context();
        self.supersede(now);
        let reply_context = self.pending_reply_context.take();
        let verification = candidate.verification_timing();
        let Candidate {
            message,
            decider,
            mut analysis,
            scope,
            arrived,
            support_end,
            end_peak,
            ..
        } = candidate;
        let (_, turn_scope) = self.new_turn_scope(&scope);
        let uuid = Uuid::new_v4();
        let owner = SubOwner::new(uuid.to_string()).expect("a UUID is not empty");
        if let Err(error) =
            self.uplink
                .convert_to_turn(&message.subscription, owner, self.mono(now.at))
        {
            tracing::info!(device = %self.device, %error, "an accepted candidate's subscription is gone");
            self.close_analysis(analysis.key.clone());
            if let Some(grant) = grant {
                let _ = self.arbiter.release(grant);
            }
            return;
        }
        let candidate = &message.candidate;
        let (Some(open), Some(trigger)) = (candidate.open(), candidate.trigger()) else {
            return;
        };
        let mic_start = TurnSpec::candidate_mic_start(candidate.support_start);
        let start = TurnSpec::wake_start(candidate.support_start);
        analysis.cells_from = Some(TurnSpec::cells_start(mic_start));
        let spec = TurnSpec {
            epoch: analysis.epoch(),
            start,
            trigger,
            cells_from: TurnSpec::cells_start(mic_start),
            vad_from: mic_start.cell(),
            kind: TurnKind::Wake(WakeAnchor {
                support_start: candidate.support_start,
                candidate_open: open,
                phrase: self.wake.phrase.clone(),
            }),
        };
        let settings = Arc::clone(&self.settings);
        let cause = echomuse_recorder::TurnCause::Wake {
            candidate_id: message.candidate.candidate_id.clone(),
        };
        let (mut policy, evidence_id) = match self.open_policy(cause, spec, now) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(device = %self.device, ?error, "a wake turn's plan was refused");
                return;
            }
        };
        let mut evidence_broken = false;
        let mut tokens = Vec::new();
        if let Some(pending) = analysis.pending.take() {
            tokens = pending.last_tokens();
            let epoch = analysis.epoch();
            if pending
                .replay(
                    epoch,
                    &mut self.noting(policy.evidence_mut(), evidence_id, now),
                )
                .is_err()
            {
                evidence_broken = true;
            }
        }
        let key = analysis.key.clone();
        let chimed = message.chimed;
        let generation = 2;
        let input_focus = FocusLease::acquire(
            &mut self.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            generation,
        )
        .ok();
        let (expectation, reply_to, chain) = match reply_context {
            Some((expectation, question, chain)) => (Some(expectation), question, chain),
            None => (None, None, Chain::new(now.at)),
        };
        let wake = AcceptedWake {
            message,
            trace: decider.trace().clone(),
            support_end,
            end_peak,
            verification,
        };
        self.turn = Some(Turn {
            uuid,
            scope: turn_scope,
            trigger: TriggerType::Wake,
            subscription: Some(wake.message.subscription.clone()),
            analysis: Some(analysis),
            wake: Some(wake),
            grant,
            restricted,
            settings: Arc::clone(&settings),
            policy: Some(policy),
            utterance: None,
            tokens,
            stage: Stage::Listening { end_pending: false },
            context,
            ring,
            expectation,
            reply_to,
            chain,
            origin: ReplyOrigin::Expectation,
            input_focus,
            opened: arrived,
            commit: None,
            span_pcm: None,
            pipeline: None,
            stt_copy: None,
            facts: Facts::default(),
            answering: Answering::default(),
            evidence_broken,
            heard: false,
            reply_spec: None,
            alert_wait: None,
            timer_request: None,
            timers_cancelling: None,
            ha_run: None,
            ha_run_audio: None,
            ha_stream_url: None,
            afe: Default::default(),
            response: None,
            evidence_id,
            pause_asked: None,
        });
        self.mic_job(
            crate::analysis::Owner::Turn,
            MicJob::StartUtterance { key, from: start },
        );
        self.media.dialog_started();
        if !restricted {
            if let Some(body) = self.view.listening(uuid) {
                self.link.send(Outbound::TurnView(body), 0);
            }
            if settings.wakeword.wake_sound && !chimed {
                self.earcon(echomuse_playback::Earcon::WakeChime, now);
            }
        }
        self.turn_evidence(now);
    }

    /// A button press starts a turn from 300 ms before it (§4.3).
    pub(crate) fn start_button_turn(
        &mut self,
        press: echomuse_presentation::button::ButtonTurn,
        now: Now,
    ) {
        let Some(capture) = self.capture.clone() else {
            self.cue(Cue::Error, now);
            return;
        };
        let current = self.uplink.timeline().capture_epoch();
        if current.map(|e| e.get()) != Some(press.epoch) {
            // The press's capture epoch has been replaced: no audio to listen to.
            self.cue(Cue::Error, now);
            return;
        }
        let (context, ring) = self.command_context();
        let kept = self.supersede(now);
        let reply_context = self.pending_reply_context.take();
        let (_, scope) = self.new_turn_scope(&capture);
        let uuid = Uuid::new_v4();
        let owner = SubOwner::new(uuid.to_string()).expect("a UUID is not empty");
        let press_sample = Sample(press.press_sample);
        let opened = match self
            .uplink
            .open_button_turn(owner, press_sample, self.mono(now.at))
        {
            Ok(opened) => opened,
            Err(error) => {
                tracing::info!(device = %self.device, %error, "a button turn's subscription was refused");
                self.cue(Cue::Error, now);
                return;
            }
        };
        self.apply_grants(&opened.grants);
        let epoch_nz = current.expect("checked above");
        let Some(epoch) = echomuse_turn::Epoch::new(epoch_nz.get()) else {
            return;
        };
        let key = crate::speech::StreamKey {
            session: self.session,
            subscription: opened.subscription_id.clone(),
            epoch,
        };
        let mic_start = TurnSpec::candidate_mic_start(press_sample);
        let spec = TurnSpec {
            epoch,
            start: mic_start,
            trigger: press_sample,
            cells_from: TurnSpec::cells_start(mic_start),
            vad_from: mic_start.cell(),
            kind: TurnKind::Button,
        };
        let settings = Arc::clone(&self.settings);
        let Ok((policy, evidence_id)) =
            self.open_policy(echomuse_recorder::TurnCause::Button, spec, now)
        else {
            return;
        };
        let mut analysis = Analysis::new(key.clone(), epoch_nz);
        analysis.cells_from = Some(TurnSpec::cells_start(mic_start));
        analysis.pending = None;
        let input_focus = FocusLease::acquire(
            &mut self.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            1,
        )
        .ok();
        let keeps = press.keeps_reply && kept.is_some();
        let (expectation, reply_to, chain) = match reply_context.filter(|_| keeps) {
            Some((expectation, question, chain)) => (Some(expectation), question, chain),
            None => (None, None, Chain::new(now.at)),
        };
        self.turn = Some(Turn {
            uuid,
            scope,
            trigger: TriggerType::Button,
            subscription: Some(opened.subscription_id),
            analysis: Some(analysis),
            wake: None,
            grant: None,
            restricted: false,
            settings: Arc::clone(&settings),
            policy: Some(policy),
            utterance: None,
            tokens: Vec::new(),
            stage: Stage::Listening { end_pending: false },
            context,
            ring,
            expectation,
            reply_to,
            chain,
            origin: ReplyOrigin::Expectation,
            input_focus,
            opened: now,
            commit: None,
            span_pcm: None,
            pipeline: None,
            stt_copy: None,
            facts: Facts::default(),
            answering: Answering::default(),
            evidence_broken: false,
            heard: false,
            reply_spec: None,
            alert_wait: None,
            timer_request: None,
            timers_cancelling: None,
            ha_run: None,
            ha_run_audio: None,
            ha_stream_url: None,
            afe: Default::default(),
            response: None,
            evidence_id,
            pause_asked: None,
        });
        self.mic_job(
            crate::analysis::Owner::Turn,
            MicJob::Open {
                key: key.clone(),
                plan: echomuse_echo::EchoPlan {
                    epoch,
                    cells_from: TurnSpec::cells_start(mic_start),
                    vad_from: mic_start.cell(),
                },
            },
        );
        self.reference_job(
            crate::analysis::Owner::Turn,
            crate::speech::ReferenceJob::Open {
                key: key.clone(),
                graph: self.wake.files.clone(),
            },
        );
        self.mic_job(
            crate::analysis::Owner::Turn,
            MicJob::StartUtterance {
                key,
                from: mic_start,
            },
        );
        self.media.dialog_started();
        if let Some(body) = self.view.listening(uuid) {
            self.link.send(Outbound::TurnView(body), 0);
        }
        if settings.wakeword.wake_sound {
            self.earcon(echomuse_playback::Earcon::WakeChime, now);
        }
    }

    /// Starts the reply turn of an open window at its first mic sample (§4.5).
    pub(crate) fn start_reply_turn(&mut self, trigger: Sample, now: Now) {
        let Some(window) = self.window.take() else {
            return;
        };
        let start = Sample(trigger.0.div_ceil(512) * 512);
        let reply = ReplySpec::at_drain(
            window.origin,
            start,
            self.mono(window.opened_at),
            &echomuse_turn::EndpointPolicy::default(),
        );
        self.begin_reply_turn(window, start, start, reply, now);
    }

    /// Opens a reply turn on `window`'s subscription: its utterance from `start`, its
    /// trigger at `trigger`, under `reply`'s guard and no-input rules.
    pub(crate) fn begin_reply_turn(
        &mut self,
        mut window: crate::reply::ReplyWindow,
        start: Sample,
        trigger: Sample,
        reply: ReplySpec,
        now: Now,
    ) {
        let origin = window.origin;
        let spec = TurnSpec {
            epoch: window.analysis.epoch(),
            start,
            trigger,
            cells_from: window.cells_from.cell().min(start.cell()),
            vad_from: start.cell(),
            kind: TurnKind::Reply(reply),
        };
        let settings = Arc::clone(&self.settings);
        let cause = echomuse_recorder::TurnCause::Reply {
            reopened_from: None,
        };
        let (mut policy, evidence_id) = match self.open_policy(cause, spec, now) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(device = %self.device, ?error, "a reply turn's plan was refused");
                self.close_window_subscription(&window, now);
                if let Some(question) = window.question {
                    self.resolve_continuation(question, ContinuationEnd::FollowUp(FollowUp::NoMic));
                }
                return;
            }
        };
        let mut evidence_broken = false;
        let mut tokens = Vec::new();
        let epoch = window.analysis.epoch();
        match window.analysis.pending.take() {
            Some(pending) => {
                tokens = pending.last_tokens();
                if pending
                    .replay(
                        epoch,
                        &mut self.noting(policy.evidence_mut(), evidence_id, now),
                    )
                    .is_err()
                {
                    evidence_broken = true;
                }
            }
            // An early answer's stream was already judged: its loudness, VAD and gaps
            // go to the reply's utterance, whose echo labels and decode start afresh.
            None => {
                if window
                    .analysis
                    .log
                    .replay_history(
                        epoch,
                        &mut self.noting(policy.evidence_mut(), evidence_id, now),
                    )
                    .is_err()
                {
                    evidence_broken = true;
                }
            }
        }
        let key = window.analysis.key.clone();
        let uuid = Uuid::new_v4();
        let input_focus = FocusLease::acquire(
            &mut self.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            1,
        )
        .ok();
        let (context, ring) = self.command_context();
        self.turn = Some(Turn {
            uuid,
            scope: window.scope.clone(),
            trigger: if origin == ReplyOrigin::HaConversation {
                TriggerType::HaReply
            } else {
                TriggerType::Reply
            },
            subscription: Some(window.subscription.clone()),
            analysis: Some(window.analysis),
            wake: None,
            grant: None,
            restricted: false,
            settings,
            policy: Some(policy),
            utterance: None,
            tokens,
            stage: Stage::Listening { end_pending: false },
            context,
            ring,
            expectation: Some(window.expectation),
            reply_to: window.question,
            chain: window.chain,
            origin,
            input_focus,
            opened: now,
            commit: None,
            span_pcm: None,
            pipeline: None,
            stt_copy: None,
            facts: Facts::default(),
            answering: Answering::default(),
            evidence_broken,
            heard: false,
            reply_spec: Some(reply),
            alert_wait: None,
            timer_request: None,
            timers_cancelling: None,
            ha_run: None,
            ha_run_audio: None,
            ha_stream_url: None,
            afe: Default::default(),
            response: None,
            evidence_id,
            pause_asked: None,
        });
        self.mic_job(
            crate::analysis::Owner::Turn,
            MicJob::StartUtterance { key, from: start },
        );
        self.media.dialog_started();
        self.turn_evidence(now);
    }

    pub(crate) fn turn_news(&mut self, news: StreamNews, now: Now) {
        match news {
            StreamNews::Ended(ended) => {
                let Some(turn) = self.turn.as_mut() else {
                    return;
                };
                turn.subscription = None;
                let Some(policy) = turn.policy.as_mut() else {
                    return;
                };
                let mut out = Vec::new();
                let control = match ended.cause {
                    EndCause::Muted => {
                        policy.mute(&mut out);
                        echomuse_recorder::TurnControl::Mute
                    }
                    EndCause::SessionLost => {
                        policy.session_lost(&mut out);
                        echomuse_recorder::TurnControl::SessionLost
                    }
                    EndCause::Overflow => {
                        policy.audio_overrun(&mut out);
                        echomuse_recorder::TurnControl::AudioOverrun
                    }
                    EndCause::Device(report)
                        if report.reason
                            == echomuse_protocol::streams::UplinkEndedReason::Overrun =>
                    {
                        policy.audio_overrun(&mut out);
                        echomuse_recorder::TurnControl::AudioOverrun
                    }
                    _ => {
                        policy.discontinuity(&mut out);
                        echomuse_recorder::TurnControl::Discontinuity
                    }
                };
                self.note_control(control, &out, now);
                self.decisions(out, now);
            }
            StreamNews::Mic | StreamNews::Other => self.turn_evidence(now),
        }
    }

    /// Advances the turn policy over what has arrived and acts on its decisions.
    pub(crate) fn turn_evidence(&mut self, now: Now) {
        let mono = self.mono(now.at);
        let recording = self.recording;
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let reply_kind = turn.expectation.as_ref().and_then(Expectation::reply_kind);
        let broken = turn.evidence_broken;
        let id = turn.evidence_id;
        let Some(policy) = turn.policy.as_mut() else {
            return;
        };
        let mut out = Vec::new();
        if broken {
            policy.worker_error(&mut out);
            self.note_control(echomuse_recorder::TurnControl::WorkerError, &out, now);
        } else {
            let grammar = TurnGrammar {
                grammar: &self.grammar,
                reply: reply_kind,
            };
            if recording {
                let tap = echomuse_recorder::GrammarTap::new(&grammar);
                policy.advance(&tap, mono, &mut out);
                let recognizer = policy
                    .trace()
                    .recognizer
                    .iter()
                    .map(|r| (r.text.clone(), r.applied_at))
                    .collect();
                let evidence = crate::evidence::Evidence::Advanced {
                    turn: id,
                    grammar: tap.take(),
                    recognizer,
                    decisions: out.clone(),
                };
                self.note(now, || evidence);
            } else {
                policy.advance(&grammar, mono, &mut out);
            }
        }
        self.decisions(out, now);
    }

    pub(crate) fn turn_worker_error(&mut self, now: Now) {
        let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) else {
            return;
        };
        let mut out = Vec::new();
        policy.worker_error(&mut out);
        self.note_control(echomuse_recorder::TurnControl::WorkerError, &out, now);
        self.decisions(out, now);
    }

    pub(crate) fn turn_timer(&mut self, now: Now) {
        let mono = self.mono(now.at);
        if self
            .turn
            .as_ref()
            .and_then(|t| t.response.as_ref())
            .is_some_and(|(_, until)| *until <= now.at)
        {
            self.close_response(now);
        }
        let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) else {
            return;
        };
        if policy.next_deadline().is_some_and(|due| due <= mono) {
            let mut out = Vec::new();
            policy.on_timer(mono, &mut out);
            if let Some(id) = self.turn.as_ref().map(|t| t.evidence_id) {
                let decisions = out.clone();
                self.note(now, || crate::evidence::Evidence::Timer {
                    turn: id,
                    decisions,
                });
            }
            self.decisions(out, now);
        }
        if let Some(turn) = self.turn.as_mut()
            && let Some(lease) = turn.input_focus.as_mut()
            && let Some(event) = lease.poll(&mut self.link, now.at)
        {
            self.focus_event(event, now);
        }
    }

    fn decisions(&mut self, decisions: Vec<Decision>, now: Now) {
        for decision in decisions {
            match decision {
                Decision::CommandStarted { .. } => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.heard = true;
                        if let Some(question) = turn.reply_to
                            && turn.trigger != TriggerType::Wake
                            && turn.trigger != TriggerType::Button
                        {
                            self.resolve_continuation(
                                question,
                                ContinuationEnd::FollowUp(FollowUp::Answered),
                            );
                        }
                    }
                }
                Decision::AskRecognizer { text } => self.ask_recognizer(text, now),
                Decision::EndPending { .. } => self.set_listening(true),
                Decision::EndRevoked { .. } => self.set_listening(false),
                Decision::Commit(commit) => self.commit(commit, now),
                Decision::Close { reason, .. } => {
                    self.take_utterance();
                    self.end_turn(TerminalReason::from(reason), now);
                }
                Decision::ReopenReply(reopen) => self.reopen_reply(reopen, now),
            }
        }
    }

    fn set_listening(&mut self, end_pending: bool) {
        if let Some(turn) = self.turn.as_mut()
            && matches!(turn.stage, Stage::Listening { .. })
        {
            turn.stage = Stage::Listening { end_pending };
        }
    }

    fn take_utterance(&mut self) {
        if let Some(turn) = self.turn.as_mut()
            && let Some(policy) = turn.policy.take()
        {
            turn.utterance = Some(policy.into_trace());
        }
    }

    /// A gap before any command speech in a reply: a fresh utterance (and ASR stream)
    /// starts where the gap ended, keeping the window's guard and its no-input point.
    fn reopen_reply(&mut self, reopen: echomuse_turn::ReplyReopen, now: Now) {
        let mono = self.mono(now.at);
        let Some(reply_kind) = self
            .turn
            .as_ref()
            .and_then(|t| t.reply_spec)
            .map(TurnKind::Reply)
        else {
            return;
        };
        let config = self.turn_config(&self.settings.clone(), &reply_kind);
        let recording = self.recording;
        let id = self.next_evidence_id();
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let (Some(policy), Some(analysis), Some(previous_spec)) =
            (turn.policy.take(), turn.analysis.as_ref(), turn.reply_spec)
        else {
            return;
        };
        let previous = policy.into_trace();
        let reply = ReplySpec {
            no_input_at: reopen.no_input_at,
            ..previous_spec
        };
        let start = reopen.start;
        let spec = TurnSpec {
            epoch: analysis.epoch(),
            start,
            trigger: reopen.trigger,
            cells_from: analysis
                .cells_from
                .unwrap_or(start.cell())
                .min(start.cell()),
            vad_from: start.cell(),
            kind: TurnKind::Reply(reply),
        };
        let mut policy = match TurnPolicy::new(config, spec.clone(), mono) {
            Ok(policy) => policy,
            Err(_) => {
                turn.utterance = Some(previous);
                self.end_turn(TerminalReason::Interrupted, now);
                return;
            }
        };
        if recording {
            self.link.effects.push(Effect::Evidence {
                at: mono,
                evidence: Box::new(crate::evidence::Evidence::TurnOpened {
                    turn: id,
                    cause: echomuse_recorder::TurnCause::Reply {
                        reopened_from: Some(turn.evidence_id),
                    },
                    config,
                    spec,
                }),
            });
        }
        turn.evidence_id = id;
        // Loudness, VAD and gaps already heard go to the new utterance; its echo labels
        // and ASR restart with the worker's fresh utterance.
        let mut pushes = crate::evidence::Noting {
            assembler: policy.evidence_mut(),
            notes: recording.then_some((&mut self.link.effects, id, mono)),
        };
        if analysis
            .log
            .replay_history(analysis.epoch(), &mut pushes)
            .is_err()
        {
            turn.evidence_broken = true;
        }
        turn.policy = Some(policy);
        turn.reply_spec = Some(reply);
        let key = analysis.key.clone();
        self.mic_job(
            crate::analysis::Owner::Turn,
            MicJob::StartUtterance { key, from: start },
        );
    }

    fn ask_recognizer(&mut self, text: String, _now: Now) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let scope = turn.scope.clone();
        let uuid = turn.uuid;
        let call = self.call_id();
        self.calls.insert(
            call,
            Call::Recognize {
                turn: uuid,
                text: text.clone(),
            },
        );
        self.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::Recognize {
                text,
                ha_device: self.ha_device.clone(),
            },
        });
    }

    // ---- commit ---------------------------------------------------------------------

    fn commit(&mut self, commit: Commit, now: Now) {
        self.take_utterance();
        let mono = self.mono(now.at);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.stage = Stage::Committed;
        turn.heard = true;
        let commit_id = Uuid::new_v4();
        turn.commit = Some((commit.clone(), commit_id, now.at));
        turn.facts.boundary_mono_ns = turn
            .analysis
            .as_ref()
            .and_then(|a| a.mono_at(commit.boundary.0));
        let epoch = turn.epoch();
        let subscription = turn.subscription.take();
        let analysis_key = turn.analysis.as_ref().map(|a| a.key.clone());
        let restricted = turn.restricted;
        let context = turn.context.clone();
        // The committed span is immutable: read it before the subscription's audio goes.
        let pcm = epoch.and_then(|epoch| {
            let mut pcm = Vec::new();
            self.uplink
                .timeline()
                .read_pcm(
                    StreamId::Mic,
                    epoch,
                    Span {
                        start: commit.span.start.0,
                        end: commit.span.end.0,
                    },
                    &mut pcm,
                )
                .ok()
                .map(|()| Arc::<[i16]>::from(pcm))
        });
        self.open_response(now);
        if let Some(subscription) = subscription {
            let _ = self
                .uplink
                .close(&subscription, CloseReason::Committed, mono);
        }
        if let Some(key) = analysis_key {
            self.close_analysis(key);
        }
        if let Some(lease) = self.turn.as_mut().and_then(|t| t.input_focus.take()) {
            self.release_focus(Some(lease), now);
        }
        let Some(pcm) = pcm else {
            tracing::warn!(device = %self.device, "the committed span is not held");
            self.end_turn(TerminalReason::Interrupted, now);
            return;
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.span_pcm = Some(Arc::clone(&pcm));
        }
        // Commands handled before STT, from the streaming text (§4.4).
        let command_text = commit.text.clone();
        if let Some(command) = self.grammar.context_command(&command_text, &context) {
            self.context_command(command, command_text, now);
            return;
        }
        if restricted {
            // Another device answers this wake; only this device's media commands act.
            self.end_turn(TerminalReason::ArbitrationLost, now);
            return;
        }
        let (uuid, scope, recheck) = {
            let turn = self.turn.as_ref().expect("present");
            (turn.uuid, turn.scope.clone(), commit.recheck)
        };
        if !recheck {
            self.transcribe(now);
        } else if commit.source == echomuse_turn::TextSource::Server
            && self.recheck_on_server(&commit, &pcm)
        {
            // The committed text is the pause server's words: the same server re-checks it.
        } else {
            self.link.effects.push(Effect::Span {
                scope,
                turn: uuid,
                job: SpanJob::Redecode { turn: uuid, pcm },
            });
        }
    }

    /// In an `afe_metadata` session a commit opens the response subscription before the
    /// committed one closes: AFE records from where the committed subscription's reached,
    /// and, while the speaker saves utterances, the mic from its last sample, for the
    /// turn recording (afe-metadata.md §2.1).
    fn open_response(&mut self, now: Now) {
        if !self.uplink.terms().afe_metadata {
            return;
        }
        let mono = self.mono(now.at);
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let Some(analysis) = turn.analysis.as_ref() else {
            return;
        };
        let mic_end = analysis.mic_end;
        let afe_from = match analysis.afe.keys().next_back() {
            Some(last) => last + crate::afe::PERIOD,
            None => match mic_end {
                Some(end) => end.0 / crate::afe::PERIOD * crate::afe::PERIOD,
                None => return,
            },
        };
        let mic_from = mic_end.filter(|_| turn.settings.microphones.save_utterances);
        let owner = SubOwner::new(format!("response-{}", turn.uuid)).expect("not empty");
        match self
            .uplink
            .open_response(owner, Sample(afe_from), mic_from, mono)
        {
            Ok(opened) => {
                self.apply_grants(&opened.grants);
                if let Some(turn) = self.turn.as_mut() {
                    turn.response = Some((
                        opened.subscription_id,
                        now.at + std::time::Duration::from_secs(30),
                    ));
                }
            }
            Err(error) => {
                tracing::info!(device = %self.device, %error, "the response subscription could not open");
            }
        }
    }

    /// Audio of the turn's response subscription: its AFE records join the turn's;
    /// its mic stays in the timeline for the turn recording.
    pub(crate) fn response_delivery(&mut self, delivery: echomuse_uplink::Delivery) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        match delivery {
            echomuse_uplink::Delivery::Chunk(chunk) => {
                if let echomuse_uplink::ChunkBody::Data {
                    data: echomuse_uplink::ChunkData::Afe(records),
                    ..
                } = chunk.body
                {
                    crate::analysis::store_afe(&mut turn.afe, chunk.span.start, records);
                }
            }
            echomuse_uplink::Delivery::Ended(_) => turn.response = None,
        }
    }

    /// Closes the turn's response subscription (the turn ended, or 30 s passed since
    /// its commit).
    pub(crate) fn close_response(&mut self, now: Now) {
        let Some((subscription, _)) = self.turn.as_mut().and_then(|t| t.response.take()) else {
            return;
        };
        let _ = self
            .uplink
            .close(&subscription, CloseReason::Closed, self.mono(now.at));
    }
    /// Starts the STT copy and resolves the pipeline, in parallel.
    pub(crate) fn transcribe(&mut self, now: Now) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let (Some(pcm), uuid, scope) = (turn.span_pcm.clone(), turn.uuid, turn.scope.clone())
        else {
            return;
        };
        let gain = match &turn.wake {
            Some(wake) => {
                let support = wake.message.candidate.support_start;
                let end = wake
                    .message
                    .candidate
                    .open()
                    .unwrap_or(support + Samples::from_ms(500));
                let epoch = wake.message.capture_epoch;
                let mut support_pcm = Vec::new();
                match self.uplink.timeline().read_pcm(
                    StreamId::Mic,
                    epoch,
                    Span {
                        start: support.0,
                        end: end.0,
                    },
                    &mut support_pcm,
                ) {
                    Ok(()) => SttGain::FromWake(support_pcm.into()),
                    Err(_) => SttGain::Nominal,
                }
            }
            None => SttGain::Nominal,
        };
        let denoise = turn.settings.microphones.ns_asr;
        let ha_reply = turn.origin == ReplyOrigin::HaConversation;
        self.link.effects.push(Effect::Span {
            scope: scope.clone(),
            turn: uuid,
            job: SpanJob::SttCopy {
                turn: uuid,
                pcm,
                gain,
                denoise,
            },
        });
        if !ha_reply {
            let call = self.call_id();
            self.calls.insert(call, Call::Pipeline { turn: uuid });
            self.link.effects.push(Effect::Ha {
                call,
                scope,
                request: HaRequest::Pipeline {
                    ha_device: self.ha_device.clone(),
                },
            });
        }
        let _ = now;
    }

    pub(crate) fn span_result(
        &mut self,
        uuid: Uuid,
        result: Result<SpanOutput, echomuse_core::types::RuntimeError>,
        now: Now,
    ) {
        if self.turn.as_ref().map(|t| t.uuid) != Some(uuid) {
            self.counters.stale_results += 1;
            return;
        }
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                self.worker_failed(&error);
                self.end_turn(TerminalReason::Interrupted, now);
                return;
            }
        };
        match output {
            SpanOutput::Redecode { result, .. } => match result {
                Ok(redecoded) => {
                    if self.redecode_changed(&redecoded) {
                        tracing::info!(device = %self.device, "the committed span re-decoded differently");
                        self.end_turn(TerminalReason::Retry, now);
                    } else {
                        self.transcribe(now);
                    }
                }
                Err(error) => {
                    tracing::warn!(device = %self.device, error, "the commit re-decode failed");
                    self.end_turn(TerminalReason::Interrupted, now);
                }
            },
            SpanOutput::SttCopy { result, .. } => match result {
                Ok(copy) => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.stt_copy = Some(copy);
                    }
                    self.maybe_stt(now);
                }
                Err(error) => {
                    tracing::warn!(device = %self.device, error, "the STT copy failed");
                    self.end_turn(TerminalReason::Interrupted, now);
                }
            },
        }
    }

    /// Whether the re-decoded span's command differs from what the commit heard.
    fn redecode_changed(&self, redecoded: &echomuse_speech::AsrResult) -> bool {
        let Some(turn) = self.turn.as_ref() else {
            return false;
        };
        let Some((commit, _, _)) = &turn.commit else {
            return false;
        };
        let start = commit.span.start;
        let tokens: Vec<(String, Sample)> = redecoded
            .tokens
            .iter()
            .zip(&redecoded.timestamps)
            .map(|(token, seconds)| {
                (
                    token.clone(),
                    start + Samples((f64::from(*seconds) * 16_000.0).round() as u64),
                )
            })
            .collect();
        let transcript = TimedTranscript::from_tokens(tokens.iter().map(|(t, s)| (t.as_str(), *s)));
        let text = match &turn.wake {
            Some(wake) => {
                let lookahead =
                    wake.message.candidate.open().unwrap_or(start) + Samples::from_ms(480);
                let located = locate_in_streaming(&transcript, lookahead, &self.wake.phrase);
                echomuse_turn::text::cut_streaming(&transcript, &located)
            }
            None => transcript.text().to_owned(),
        };
        self.command_differs(&text)
    }

    /// Whether a re-check's command text differs from what the commit heard, other than by
    /// completing the final word (§4.6).
    pub(crate) fn command_differs(&self, text: &str) -> bool {
        let Some((commit, _, _)) = self.turn.as_ref().and_then(|t| t.commit.as_ref()) else {
            return false;
        };
        echomuse_turn::text::recheck_differs(&commit.text, text)
    }

    /// The command in a transcript of the whole utterance that has no word timings (HA's,
    /// or the pause server's): for a wake turn, the wake phrase and anything said before it
    /// are removed near where the streaming words placed it (§4.1).
    pub(crate) fn final_command(&self, raw: &str) -> String {
        match self
            .turn
            .as_ref()
            .and_then(|t| t.wake.as_ref().map(|w| (t, w)))
        {
            Some((turn, wake)) => {
                let transcript = TimedTranscript::from_tokens(
                    turn.tokens.iter().map(|t| (t.text.as_str(), t.emitted)),
                );
                let lookahead = wake
                    .message
                    .candidate
                    .open()
                    .unwrap_or(wake.message.candidate.support_start)
                    + Samples::from_ms(480);
                let located = locate_in_streaming(&transcript, lookahead, &self.wake.phrase);
                strip_final(raw, &located, &self.wake.phrase)
                    .text
                    .trim()
                    .to_owned()
            }
            None => raw.trim().to_owned(),
        }
    }

    /// Sends the STT copy once both it and the pipeline are known.
    fn maybe_stt(&mut self, now: Now) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let Some(copy) = turn.stt_copy.clone() else {
            return;
        };
        if turn.origin == ReplyOrigin::HaConversation {
            self.ha_reply_run(copy, now);
            return;
        }
        let pipeline = match &turn.pipeline {
            None => return,
            Some(Ok(pipeline)) => pipeline.clone(),
            Some(Err(error)) => {
                tracing::warn!(device = %self.device, %error, "no pipeline for STT");
                self.fail_turn(TerminalReason::SttFailed, now);
                return;
            }
        };
        let uuid = turn.uuid;
        let scope = turn.scope.clone();
        let call = self.call_id();
        self.calls.insert(call, Call::Stt { turn: uuid });
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.stt_started = Some(now.at);
        }
        self.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::Stt {
                target: SatelliteTarget {
                    pipeline,
                    ha_device: self.ha_device.clone(),
                },
                audio: copy.pcm,
            },
        });
    }

    pub(crate) fn target(&self) -> Option<SatelliteTarget> {
        let turn = self.turn.as_ref()?;
        match &turn.pipeline {
            Some(Ok(pipeline)) => Some(SatelliteTarget {
                pipeline: pipeline.clone(),
                ha_device: self.ha_device.clone(),
            }),
            _ => None,
        }
    }

    /// Home Assistant answered.
    pub(crate) fn ha_reply(&mut self, call: crate::io::CallId, reply: HaReply, now: Now) {
        let purpose = match &reply {
            // An intent run's events keep its call registered until its terminal event.
            HaReply::IntentEvent(event) if !event.is_terminal() => self.calls.get(&call).cloned(),
            HaReply::IntentDispatched(Ok(())) => self.calls.get(&call).cloned(),
            _ => self.calls.remove(&call),
        };
        let Some(purpose) = purpose else {
            self.counters.stale_results += 1;
            return;
        };
        let current =
            |turn: &Uuid, voice: &Voice| voice.turn.as_ref().map(|t| t.uuid) == Some(*turn);
        match (purpose, reply) {
            (Call::Pipeline { turn }, HaReply::Pipeline(result)) if current(&turn, self) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.pipeline = Some(result);
                }
                self.maybe_stt(now);
            }
            (Call::Stt { turn }, HaReply::Stt(result)) if current(&turn, self) => {
                self.transcript(result, now);
            }
            (Call::Recognize { turn, text }, HaReply::Recognize(result))
                if current(&turn, self) =>
            {
                let outcome = match result {
                    Ok(answers) => Ok(crate::io::recognized_class(&answers)),
                    Err(HaError::NotDispatched(_)) => Err(RecognizerError::Unavailable),
                    Err(HaError::Timeout) => Err(RecognizerError::Timeout),
                    Err(_) => Err(RecognizerError::Failed),
                };
                let mono = self.mono(now.at);
                let answer = RecognizerAnswer { text, outcome };
                if let Some(id) = self
                    .turn
                    .as_ref()
                    .filter(|t| t.policy.is_some())
                    .map(|t| t.evidence_id)
                {
                    let noted = answer.clone();
                    self.note(now, || crate::evidence::Evidence::RecognizerAnswer {
                        turn: id,
                        answer: noted,
                    });
                }
                if let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) {
                    policy.recognizer_answer(answer, mono);
                }
                // Takes effect at the next block boundary the policy evaluates.
            }
            (Call::Intent { turn }, HaReply::IntentDispatched(result)) if current(&turn, self) => {
                if result.is_err() {
                    // Nothing was sent: HA is not reachable, or the turn was cancelled.
                    self.calls.remove(&call);
                    self.intent_failed(now, false);
                }
            }
            (Call::Intent { turn }, HaReply::IntentEvent(event)) if current(&turn, self) => {
                self.intent_event(event, now);
            }
            (Call::Tts { turn }, HaReply::Tts(result)) if current(&turn, self) => match result {
                Ok(url) => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.facts.tts_url_at = Some(now.at);
                    }
                    self.speak_answer(url.0, now);
                }
                Err(error) => {
                    tracing::warn!(device = %self.device, %error, "a spoken line could not be synthesized");
                    self.cue(Cue::Error, now);
                    self.answer_done(None, now);
                }
            },
            (Call::TimerStatus { turn }, HaReply::TimerStatus(result)) if current(&turn, self) => {
                self.timer_status(result, now);
            }
            (Call::CancelTimers { turn }, HaReply::CallIntent(outcome)) if current(&turn, self) => {
                self.timer_cancelled(outcome, now);
            }
            (Call::Intent { .. }, HaReply::IntentEvent(event)) if event.is_terminal() => {
                // A superseded turn's run ended; its outcome was already recorded.
            }
            _ => self.counters.stale_results += 1,
        }
    }

    fn transcript(
        &mut self,
        result: Result<echomuse_core::services::Transcript, HaError>,
        now: Now,
    ) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.facts.stt_done = Some(now.at);
        let raw = match result {
            Ok(transcript) => transcript.text,
            Err(error) => {
                tracing::warn!(device = %self.device, %error, "HA's STT failed");
                self.fail_turn(TerminalReason::SttFailed, now);
                return;
            }
        };
        turn.facts.stt_raw = Some(raw.clone());
        // The wake phrase is removed from the text sent on; the audio keeps it (§4.1).
        let text = self.final_command(&raw);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.facts.stt_text = Some(text.clone());
        if echomuse_grammar::normalize(&text).is_empty() {
            // Nothing usable: say so and listen again without the wake word (§4.5).
            turn.facts.terminal = Some(TerminalReason::EmptyTranscript);
            turn.answering.follow_up = Some(Expectation::Retry);
            self.say(echomuse_grammar::Line::DidntCatch, now);
            return;
        }
        turn.stage = Stage::Thinking;
        if let Some(body) = self.view.thinking(&text) {
            self.link.send(Outbound::TurnView(body), 0);
        }
        self.correct(text, now);
    }

    /// Corrects the transcript before routing it (§4.7): off, it is routed at once;
    /// otherwise the runtime decides what to send, within 100 ms in `on` mode.
    fn correct(&mut self, text: String, now: Now) {
        let target = self.target();
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let settings = turn.settings.transcript.clone();
        let off = settings.transcript_fix == echomuse_settings::values::TranscriptFixMode::Off;
        let Some(target) = target.filter(|_| !off) else {
            self.route(text, now);
            return;
        };
        let request = crate::io::CorrectionRequest {
            scope: turn.scope.clone(),
            heard: text,
            wake_phrase: turn.wake.as_ref().map(|_| self.wake.phrase.clone()),
            contextual: turn.expectation.is_some()
                || matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply),
            settings,
            target,
        };
        let uuid = turn.uuid;
        let call = self.call_id();
        self.calls.insert(call, Call::Correct { turn: uuid });
        self.link.effects.push(Effect::Correct { call, request });
    }

    /// Transcript correction answered.
    pub(crate) fn corrected(
        &mut self,
        call: crate::io::CallId,
        correction: crate::io::Correction,
        now: Now,
    ) {
        let crate::io::Correction {
            send,
            applied,
            record,
            record_follows,
        } = correction;
        match self.calls.remove(&call) {
            Some(Call::Correct { turn }) if self.turn.as_ref().map(|t| t.uuid) == Some(turn) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.stt_fix = record;
                    // The record of a check running on beside the turn goes to the
                    // records service with the row, under this call.
                    turn.facts.fix_pending = record_follows.then_some(call);
                }
                if applied {
                    tracing::info!(device = %self.device, "transcript corrected");
                }
                self.route(send, now);
            }
            _ => self.counters.stale_results += 1,
        }
    }

    /// Sends the request to Home Assistant's own intent handling.
    pub(crate) fn dispatch_to_ha(&mut self, text: String, now: Now) {
        let Some(target) = self.target() else {
            self.fail_turn(TerminalReason::HaError, now);
            return;
        };
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.facts.outcome = Some(echomuse_records::vocab::TurnOutcome::Ha);
        turn.facts.intent_started = Some(now.at);
        turn.facts.intent_ended = None;
        turn.facts.intent_local = None;
        turn.facts.response_type = None;
        turn.answering.run_open = true;
        let conversation = turn
            .expectation
            .as_ref()
            .and_then(Expectation::conversation);
        let uuid = turn.uuid;
        let scope = turn.scope.clone();
        let call = self.call_id();
        self.calls.insert(call, Call::Intent { turn: uuid });
        self.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::IntentTts {
                target,
                text,
                conversation,
            },
        });
    }

    fn intent_event(&mut self, event: RunEvent, now: Now) {
        match event {
            RunEvent::AnswerDelta(delta) => {
                let Some(turn) = self.turn.as_mut() else {
                    return;
                };
                turn.answering.streamed.push_str(&delta);
                let text = turn.answering.streamed.clone();
                if let Some(body) = self.view.answer_so_far(&text, now.at) {
                    self.link.send(Outbound::TurnView(body), 0);
                }
            }
            RunEvent::IntentEnded(end) => {
                let Some(turn) = self.turn.as_mut() else {
                    return;
                };
                turn.facts.intent_ended = Some(now.at);
                turn.facts.response_text = Some(end.speech.clone());
                turn.facts.response_type = end.response_type.clone();
                turn.facts.conversation = end.conversation.clone();
                turn.facts.intent_local = end.processed_locally;
                if end.continue_conversation {
                    turn.answering.follow_up = Some(Expectation::Conversation {
                        conversation: end.conversation.clone(),
                    });
                }
                if let Some(body) = self.view.final_answer(&end.speech) {
                    self.link.send(Outbound::TurnView(body), 0);
                }
            }
            RunEvent::TtsReady { url, .. } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.tts_url_at = Some(now.at);
                }
                self.speak_answer(url.0, now);
            }
            RunEvent::Ended => self.run_ended(now),
            RunEvent::Rejected { code, message } | RunEvent::Failed { code, message } => {
                tracing::warn!(device = %self.device, code, message, "HA's intent run failed");
                self.intent_failed(now, true);
            }
            RunEvent::Lost => {
                // The request may or may not have been carried out: never resent.
                tracing::warn!(device = %self.device, "HA's intent run was lost: outcome unknown");
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.terminal = Some(TerminalReason::OutcomeUnknown);
                    turn.answering.run_open = false;
                    if turn.answering.playing {
                        return;
                    }
                }
                self.say(echomuse_grammar::Line::NotSureItWorked, now);
            }
        }
    }

    fn intent_failed(&mut self, now: Now, dispatched: bool) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.answering.run_open = false;
        turn.facts.terminal = Some(TerminalReason::HaError);
        let _ = dispatched;
        if turn.answering.playing {
            return;
        }
        self.say(echomuse_grammar::Line::SomethingWentWrong, now);
    }

    fn run_ended(&mut self, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.answering.run_open = false;
        if !turn.answering.playing {
            // HA answered with no audio.
            self.finish_turn(now);
        }
    }

    /// A turn that failed after commit: an error cue and, when HA can still speak, a
    /// line (it cannot when HA itself failed: the error cue is the answer).
    pub(crate) fn fail_turn(&mut self, reason: TerminalReason, now: Now) {
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.terminal = Some(reason);
        }
        self.end_turn(reason, now);
    }

    /// The answer finished playing (or there was none).
    pub(crate) fn answer_done(
        &mut self,
        reason: Option<echomuse_records::vocab::PlaybackReason>,
        now: Now,
    ) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.answering.playing = false;
        if let Some(reason) = reason {
            turn.facts.playback_reason = Some(reason);
        }
        if !turn.answering.run_open {
            self.finish_turn(now);
        }
    }

    /// A turn ran to its end: completed unless something recorded otherwise.
    pub(crate) fn finish_turn(&mut self, now: Now) {
        let reason = self
            .turn
            .as_ref()
            .and_then(|t| t.facts.terminal)
            .unwrap_or(TerminalReason::Completed);
        self.end_turn(reason, now);
    }

    /// Ends the turn: releases what it holds, persists it, and opens its reply window.
    pub(crate) fn end_turn(&mut self, reason: TerminalReason, now: Now) {
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        if let Some(policy) = turn.policy.take() {
            turn.utterance = Some(policy.into_trace());
        }
        // A committed turn's response subscription stays open a second after the turn
        // ends, and the turn's chart, recording and row wait for it (crate::linger).
        // Otherwise the chart and the turn recording are made now, from what the turn's
        // subscriptions brought, before their audio goes (unless the session's loss or a
        // mute already made them).
        let lingers = turn.response.is_some();
        if !lingers && turn.facts.afe_evidence.is_none() {
            self.chart_turn(&mut turn, false);
        }
        // Cancelling the scope withdraws interest in every request still out for it.
        turn.scope.cancel();
        self.calls.retain(|_, call| !call_for(call, turn.uuid));
        if let Some(subscription) = turn.subscription.take() {
            let reason = if reason == TerminalReason::ArbitrationLost {
                CloseReason::ArbitrationLost
            } else {
                CloseReason::Closed
            };
            let _ = self.uplink.close(&subscription, reason, self.mono(now.at));
        }
        if lingers {
            // The workers forget the stream; its AFE records and Silero cells stay with
            // the turn for the chart.
            if let Some(key) = turn.analysis.as_ref().map(|a| a.key.clone()) {
                self.close_analysis(key);
            }
        } else if let Some(analysis) = turn.analysis.take() {
            self.close_analysis(analysis.key);
        }
        // The question's early-answer watch ends with its turn (an early answer took it
        // before ending the turn).
        if self
            .question_watch
            .as_ref()
            .is_some_and(|w| w.question == turn.uuid)
        {
            self.close_question_watch(now);
        }
        if let Some(grant) = turn.grant.take() {
            let _ = self.arbiter.release(grant);
        }
        let lease = turn.input_focus.take();
        self.release_focus(lease, now);
        self.playouts
            .end_answers_of(turn.uuid, &mut self.render, &mut self.link, now);
        self.media.dialog_ended();
        let effects = self.media_effects();
        self.apply_media_effects(effects, now);
        self.counters.turns_finished += 1;
        let completed = reason == TerminalReason::Completed;
        let follow_up = turn.answering.follow_up.clone();
        let early = turn.answering.early_answer;
        // The window opens only once the question has finished playing (§4.5); an early
        // answer's reply turn opens at once instead.
        let opens_window = !early
            && follow_up.is_some()
            && (completed || reason == TerminalReason::EmptyTranscript)
            && turn.facts.playback_reason == Some(echomuse_records::vocab::PlaybackReason::Drained);
        if !turn.restricted {
            if let Some(body) = self.view.done(if completed {
                ViewEnd::Completed {
                    follow_up: opens_window || early,
                }
            } else {
                ViewEnd::Abandoned
            }) {
                self.link.send(Outbound::TurnView(body), 0);
            }
            if let Some(cue) = Cue::for_turn_end(reason, turn.trigger) {
                self.cue(cue, now);
            }
            if reason == TerminalReason::NoInput
                && matches!(turn.trigger, TriggerType::Wake | TriggerType::Button)
                && turn.settings.wakeword.wake_sound
            {
                self.earcon(echomuse_playback::Earcon::NoInputChime, now);
            }
        }
        // A reply window that heard nothing is not a turn: it resolves its question.
        let reply = matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply);
        if reply && !turn.heard {
            if let Some((response, _)) = turn.response.take() {
                let _ = self
                    .uplink
                    .close(&response, CloseReason::Closed, self.mono(now.at));
            }
            if let Some(question) = turn.reply_to {
                let end = if reason == TerminalReason::NoInput {
                    ContinuationEnd::Ended(TerminalReason::ReplyTimeout)
                } else {
                    ContinuationEnd::Ended(reason)
                };
                self.resolve_continuation(question, end);
            }
            return;
        }
        // An answer that asked something either opens its window, or records why not.
        let continuation = match (&follow_up, opens_window, turn.facts.playback_reason) {
            (None, _, _) => None,
            (Some(_), _, _) if early => Some(echomuse_records::vocab::Continuation::Pending),
            (Some(_), true, _) => Some(echomuse_records::vocab::Continuation::Pending),
            (Some(_), false, Some(echomuse_records::vocab::PlaybackReason::Cancelled)) => {
                Some(echomuse_records::vocab::Continuation::Resolved(
                    ContinuationEnd::FollowUp(FollowUp::PromptCancelled),
                ))
            }
            (Some(_), false, _) => Some(echomuse_records::vocab::Continuation::Resolved(
                ContinuationEnd::FollowUp(FollowUp::PromptFailed),
            )),
        };
        if let Some(run) = turn.ha_run {
            self.link.effects.push(Effect::Satellite(
                crate::io::SatelliteCommand::FinishRunResponse {
                    run,
                    success: completed,
                },
            ));
        }
        let (uuid, chain, origin) = (turn.uuid, turn.chain, turn.origin);
        self.persist_turn(turn, reason, continuation, now);
        if let Some(expectation) = follow_up.filter(|_| opens_window) {
            self.open_window(uuid, expectation, chain, origin, now);
        }
    }

    /// Mute or session loss: the turn ends at once, whatever stage it is in.
    pub(crate) fn revoke_turn(&mut self, reason: TerminalReason, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if let Some(policy) = turn.policy.as_mut() {
            let mut out = Vec::new();
            let control = if reason == TerminalReason::Muted {
                policy.mute(&mut out);
                echomuse_recorder::TurnControl::Mute
            } else {
                policy.session_lost(&mut out);
                echomuse_recorder::TurnControl::SessionLost
            };
            self.note_control(control, &out, now);
        }
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.answering.run_open {
            // The run may have reached HA: its outcome is unknown, and it is never
            // resent (§3.3.3).
            tracing::info!(device = %self.device, turn = %turn.uuid, "a revoked turn's intent run has an unknown outcome");
            turn.facts.unknown_outcome = true;
        }
        turn.subscription = None;
        self.end_turn(reason, now);
    }

    /// Builds the ended turn's row and hands it to the records service, or, while its
    /// response subscription lingers, keeps it with the turn until that closes.
    pub(crate) fn persist_turn(
        &mut self,
        turn: Turn,
        reason: TerminalReason,
        continuation: Option<echomuse_records::vocab::Continuation>,
        now: Now,
    ) {
        match crate::record::turn_record(self, &turn, reason, continuation, now) {
            Ok((record, audio)) => {
                if let Some((subscription, cap)) = turn.response.clone() {
                    self.linger(turn, subscription, cap, record, audio, now);
                    return;
                }
                let fix_follows = turn.facts.fix_pending;
                self.write_turn(record, audio, fix_follows);
            }
            Err(error) => {
                tracing::warn!(device = %self.device, ?error, "a turn row could not be built");
                if let Some((subscription, _)) = turn.response {
                    let _ =
                        self.uplink
                            .close(&subscription, CloseReason::Closed, self.mono(now.at));
                }
            }
        }
    }

    /// Hands a turn's row to the records service.
    pub(crate) fn write_turn(
        &mut self,
        record: echomuse_records::TurnRecord,
        audio: echomuse_records::TurnAudio,
        fix_follows: Option<crate::io::CallId>,
    ) {
        let call = self.call_id();
        self.calls.insert(call, Call::PersistTurn);
        // A correction check still running beside the turn sends its record to the
        // records service, which holds the row for it (at most the check's limit): the
        // row is written even if this session ends first.
        self.link.effects.push(Effect::Persist {
            call,
            request: PersistRequest::Turn {
                record: Box::new(record),
                audio,
                fix_follows,
            },
        });
    }

    pub(crate) fn resolve_continuation(&mut self, question: Uuid, end: ContinuationEnd) {
        // A question whose row lingers with its response subscription takes the final
        // value into that row: no row exists yet for an update to find.
        if self.resolve_lingering(question, end) {
            return;
        }
        // The records service puts it into the question's row even when that row is
        // still waiting for its correction record.
        let call = self.call_id();
        self.calls.insert(call, Call::Persist);
        self.link.effects.push(Effect::Persist {
            call,
            request: PersistRequest::Continuation {
                turn_uuid: question,
                end,
            },
        });
    }

    pub(crate) fn persisted(
        &mut self,
        call: crate::io::CallId,
        reply: crate::io::PersistReply,
        _now: Now,
    ) {
        self.calls.remove(&call);
        if let crate::io::PersistReply::Turn(Err(error))
        | crate::io::PersistReply::NearMiss(Err(error))
        | crate::io::PersistReply::Continuation(Err(error)) = &reply
        {
            tracing::warn!(device = %self.device, error, "a record was not written");
        }
    }
}

fn call_for(call: &Call, uuid: Uuid) -> bool {
    match call {
        Call::Pipeline { turn }
        | Call::Stt { turn }
        | Call::Intent { turn }
        | Call::Tts { turn }
        | Call::Recognize { turn, .. }
        | Call::TimerStatus { turn }
        | Call::CancelTimers { turn }
        | Call::Alert { turn }
        | Call::RunStart { turn }
        | Call::Correct { turn }
        | Call::PauseServer { turn, .. }
        | Call::SpanServer { turn } => *turn == uuid,
        Call::Persist | Call::PersistTurn => false,
    }
}
