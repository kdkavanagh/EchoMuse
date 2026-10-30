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
    types::WorkScope,
};
use echomuse_grammar::{CommandContext, Line, ReplyKind, RingingAlert};
use echomuse_playback::{AckOutcome, Earcon, FocusKind, FocusLease, FocusOwner};
use echomuse_presentation::{phase::ActorState, ring::Cue, turn_view::ViewEnd};
use echomuse_protocol::{MessageId, StreamId};
use echomuse_records::vocab::{
    Continuation, ContinuationEnd, FollowUp, PlaybackReason, TerminalReason, TriggerType,
};
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

use super::{Conversation, ReplyContext, local::Expectation, record::Facts, reply::Chain};
use crate::{
    analysis::{Analysis, StreamNews, mic_job},
    arbiter::WakeGrant,
    io::{CallId, Effect, HaReply, HaRequest, Now, PersistRequest},
    output::Output,
    session::{ConversationCx, ConversationFacts, Shown},
    speech::{MicJob, SpanJob, SpanOutput, StreamKey, SttCopy, SttGain},
    wake::{AcceptedWake, WakeTransfer},
};

/// What an outstanding request is for.
#[derive(Clone, Debug)]
pub(super) enum Call {
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

impl Call {
    fn turn(&self) -> Uuid {
        match self {
            Self::Pipeline { turn }
            | Self::Stt { turn }
            | Self::Intent { turn }
            | Self::Tts { turn }
            | Self::Recognize { turn, .. }
            | Self::TimerStatus { turn }
            | Self::CancelTimers { turn }
            | Self::Alert { turn }
            | Self::RunStart { turn }
            | Self::Correct { turn }
            | Self::PauseServer { turn, .. }
            | Self::SpanServer { turn } => *turn,
        }
    }
}

/// Where the turn is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage {
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
pub(super) struct Answering {
    /// An HA intent run (or an HA-side ESPHome run) was dispatched and has not ended.
    pub(super) run_open: bool,
    /// The answer's audio is playing (or about to).
    pub(super) playing: bool,
    /// The answer's text so far, streamed.
    pub(super) streamed: String,
    /// A reply window follows the answer.
    pub(super) follow_up: Option<Expectation>,
    /// The answer cut the question off: its reply turn opened at once, without a window.
    pub(super) early_answer: bool,
}

pub(super) struct Turn {
    pub(super) uuid: Uuid,
    pub(super) scope: WorkScope,
    pub(super) trigger: TriggerType,
    /// The turn's audio subscription, until it commits or closes.
    pub(super) subscription: Option<SubscriptionId>,
    pub(super) analysis: Option<Analysis>,
    pub(super) wake: Option<AcceptedWake>,
    pub(super) grant: Option<WakeGrant>,
    /// A media-context loser of arbitration: only its media commands act (§4.2).
    pub(super) restricted: bool,
    /// The settings snapshot the turn opened with (§5.4).
    pub(super) settings: Arc<DeviceSettings>,
    pub(super) policy: Option<TurnPolicy>,
    pub(super) utterance: Option<UtteranceTrace>,
    /// The latest streaming recognizer tokens, for removing the wake phrase.
    pub(super) tokens: Vec<Token>,
    pub(super) stage: Stage,
    /// What was going on when the turn started, for commands handled before STT.
    pub(super) context: CommandContext,
    /// The ring the context names: (device, ring id).
    pub(super) ring: Option<(echomuse_core::types::DeviceId, String)>,
    /// The question this turn answers, if it is a reply (or took over a reply window).
    pub(super) expectation: Option<Expectation>,
    /// The question's turn.
    pub(super) reply_to: Option<Uuid>,
    pub(super) chain: Chain,
    pub(super) origin: ReplyOrigin,
    pub(super) input_focus: Option<FocusLease>,
    pub(super) opened: Now,
    pub(super) commit: Option<(Commit, Uuid, Instant)>,
    pub(super) span_pcm: Option<Arc<[i16]>>,
    pub(super) pipeline: Option<Result<PipelineId, HaError>>,
    pub(super) stt_copy: Option<SttCopy>,
    pub(super) facts: Facts,
    pub(super) answering: Answering,
    pub(super) evidence_broken: bool,
    /// Command speech was heard (a reply window that heard none is not a turn row).
    pub(super) heard: bool,
    /// The reply spec the turn's utterance runs under (kept for a reopen).
    pub(super) reply_spec: Option<ReplySpec>,
    /// An alert operation the turn waits on.
    pub(super) alert_wait: Option<super::local::AlertWait>,
    /// The timer request waiting on HA's timer list.
    pub(super) timer_request: Option<super::local::TimerRequest>,
    /// Timer cancels being sent to HA one at a time.
    pub(super) timers_cancelling: Option<echomuse_grammar::TimerCancelRun>,
    /// The satellite run of a reply to an HA-started conversation.
    pub(super) ha_run: Option<echomuse_esphome::voice::RunId>,
    pub(super) ha_run_audio: Option<Arc<[i16]>>,
    pub(super) ha_stream_url: Option<String>,
    /// The turn's native AFE records once its committed subscription closed (the
    /// committed subscription's, then the response subscription's), by period start.
    pub(super) afe: std::collections::BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
    /// The response subscription a commit opened in an `afe_metadata` session, and when
    /// it closes at the latest (30 s after the commit).
    pub(super) response: Option<(SubscriptionId, Instant)>,
    /// The session recorder's identity of the turn's current policy.
    pub(super) evidence_id: u64,
    /// The latest pause decode whose audio went to the pause server.
    pub(super) pause_asked: Option<Sample>,
}

/// What every new turn starts with; the rest of a [`Turn`] starts empty.
struct Opening {
    uuid: Uuid,
    scope: WorkScope,
    trigger: TriggerType,
    subscription: SubscriptionId,
    analysis: Analysis,
    settings: Arc<DeviceSettings>,
    policy: TurnPolicy,
    evidence_id: u64,
    context: CommandContext,
    ring: Option<(echomuse_core::types::DeviceId, String)>,
    input_focus: Option<FocusLease>,
    opened: Now,
    reply: Option<ReplyContext>,
}

impl Turn {
    fn open(opening: Opening, now: Now) -> Self {
        let Opening {
            uuid,
            scope,
            trigger,
            subscription,
            analysis,
            settings,
            policy,
            evidence_id,
            context,
            ring,
            input_focus,
            opened,
            reply,
        } = opening;
        let (expectation, reply_to, chain) = match reply {
            Some(reply) => (Some(reply.expectation), reply.question, reply.chain),
            None => (None, None, Chain::new(now.at)),
        };
        Self {
            uuid,
            scope,
            trigger,
            subscription: Some(subscription),
            analysis: Some(analysis),
            wake: None,
            grant: None,
            restricted: false,
            settings,
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
            opened,
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
        }
    }

    pub(super) fn actor_state(&self) -> ActorState {
        match self.stage {
            Stage::Listening { end_pending: false } => ActorState::Listening,
            Stage::Listening { end_pending: true } => ActorState::EndPending,
            Stage::Committed => ActorState::Committed,
            Stage::Thinking => ActorState::Thinking,
            Stage::Speaking => ActorState::Speaking,
        }
    }

    pub(super) fn next_deadline(&self) -> Option<MonoInstant> {
        self.policy.as_ref().and_then(TurnPolicy::next_deadline)
    }

    /// When the input focus lease or the response subscription next needs attention.
    pub(super) fn focus_deadline(&self) -> Option<Instant> {
        let focus = self
            .input_focus
            .as_ref()
            .and_then(FocusLease::next_deadline);
        let response = self.response.as_ref().map(|(_, until)| *until);
        [focus, response].into_iter().flatten().min()
    }

    pub(super) fn focus_ack(&mut self, id: &MessageId, outcome: &AckOutcome) -> bool {
        match self.input_focus.as_mut() {
            Some(lease) => lease.on_ack(id, outcome.clone()).is_ok(),
            None => false,
        }
    }

    pub(super) fn note_tokens(&mut self, observations: &[echomuse_turn::AsrObservation]) {
        if let Some(last) = observations.last() {
            self.tokens = last.tokens.clone();
        }
    }

    pub(super) fn epoch(&self) -> Option<std::num::NonZeroU64> {
        self.analysis.as_ref().map(|a| a.capture_epoch)
    }
}

/// The grammar as the turn crate asks it, answering a question's own replies first.
struct TurnGrammar<'a> {
    grammar: &'a echomuse_grammar::Grammar,
    reply: Option<ReplyKind>,
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

pub(super) fn turn_config(settings: &DeviceSettings, kind: &TurnKind) -> TurnConfig {
    // A reply to an HA-started conversation reads no completeness: nothing to transcribe
    // at its pauses (§4.7).
    let ha_reply = matches!(kind, TurnKind::Reply(r) if r.origin == ReplyOrigin::HaConversation);
    TurnConfig {
        extended_utterances: settings.microphones.extended_utterances,
        recognizer_available: true,
        pause_server: !ha_reply
            && settings.microphones.pause_asr.engine()
                == echomuse_settings::values::PauseAsrEngine::Wyoming,
        ..TurnConfig::default()
    }
}

/// What is going on now, for commands handled before STT (§4.4).
fn command_context(
    facts: &ConversationFacts<'_>,
    out: &Output,
) -> (
    CommandContext,
    Option<(echomuse_core::types::DeviceId, String)>,
) {
    let own = facts
        .rings
        .devices
        .get(&facts.device.0)
        .and_then(|d| d.active.as_ref());
    let (alert, ring) = match own {
        Some(active) => (
            Some(RingingAlert {
                kind: active.kind,
                name: active.name.clone(),
            }),
            Some((facts.device.clone(), active.id.clone())),
        ),
        None => {
            // "Stop" near a device while exactly one other device rings stops that ring
            // (§4.2).
            let elsewhere = facts.rings.ringing_elsewhere(facts.device);
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
    let summary = out.summary();
    (
        CommandContext {
            alert,
            reply_playing: summary.answer_playing,
            media_playing: summary.media_playing,
        },
        ring,
    )
}

impl Conversation {
    pub(super) fn is_current(&self, uuid: &Uuid) -> bool {
        self.turn.as_ref().map(|t| t.uuid) == Some(*uuid)
    }

    /// Ends whatever turn is open because a new one starts. A reply window's question
    /// is returned for the new turn to take (§4.5).
    fn supersede(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        now: Now,
    ) -> Option<ReplyContext> {
        let mut kept = None;
        if let Some(window) = self.window.take() {
            Self::close_window_subscription(cx, &window, now);
            if let Some(question) = window.question {
                self.resolve_continuation(cx, question, ContinuationEnd::FollowUp(FollowUp::Wake));
            }
            kept = Some(ReplyContext {
                expectation: window.expectation,
                question: window.question,
                chain: window.chain,
            });
        }
        if self.turn.is_some() {
            self.end_turn(cx, out, TerminalReason::Superseded, now);
        }
        // The device's own answer stops when a new turn begins.
        out.interrupt_answers(&mut cx.output());
        kept
    }

    /// Abandons an admitted wake that cannot become a turn after all: its subscription
    /// and analysis close and its grant goes back; no turn is installed.
    fn abandon_wake(
        cx: &mut ConversationCx<'_>,
        wake: &AcceptedWake,
        analysis: Analysis,
        grant: Option<WakeGrant>,
        now: Now,
    ) {
        cx.audio.close(
            &wake.message.subscription,
            CloseReason::Closed,
            cx.facts.mono(now.at),
        );
        cx.audio.close_analysis(cx.link, analysis.key);
        if let Some(grant) = grant {
            let _ = cx.facts.arbiter.release(grant);
        }
    }

    /// An accepted, admitted wake becomes a turn: its candidate subscription is
    /// converted in place, and its analysis, evidence and scope move into the turn.
    pub(crate) fn start_wake_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        transfer: WakeTransfer,
        grant: Option<WakeGrant>,
        restricted: bool,
        now: Now,
    ) {
        let (context, ring) = command_context(&cx.facts, out);
        let reply = self.supersede(cx, out, now);
        let WakeTransfer {
            wake,
            mut analysis,
            scope,
            arrived,
        } = transfer;
        let turn_scope = cx.ids.turn_scope(&scope);
        let uuid = Uuid::new_v4();
        let owner = SubOwner::new(uuid.to_string()).expect("a UUID is not empty");
        if let Err(error) =
            cx.audio
                .convert_to_turn(&wake.message.subscription, owner, cx.facts.mono(now.at))
        {
            tracing::info!(device = %cx.facts.device, %error, "an accepted candidate's subscription could not become the turn's");
            Self::abandon_wake(cx, &wake, analysis, grant, now);
            return;
        }
        let candidate = &wake.message.candidate;
        let (Some(open), Some(trigger)) = (candidate.open(), candidate.trigger()) else {
            Self::abandon_wake(cx, &wake, analysis, grant, now);
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
                phrase: cx.facts.wake.phrase.clone(),
            }),
        };
        let settings = Arc::clone(cx.facts.settings);
        let cause = echomuse_recorder::TurnCause::Wake {
            candidate_id: candidate.candidate_id.clone(),
        };
        let (mut policy, evidence_id) = match Self::open_policy(cx, cause, spec, now) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(device = %cx.facts.device, ?error, "a wake turn's plan was refused");
                Self::abandon_wake(cx, &wake, analysis, grant, now);
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
                    &mut cx.noting(policy.evidence_mut(), evidence_id, now),
                )
                .is_err()
            {
                evidence_broken = true;
            }
        }
        let key = analysis.key.clone();
        let chimed = wake.message.chimed;
        let input_focus = FocusLease::acquire(
            cx.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            2,
        )
        .ok();
        let mut turn = Turn::open(
            Opening {
                uuid,
                scope: turn_scope,
                trigger: TriggerType::Wake,
                subscription: wake.message.subscription.clone(),
                analysis,
                settings: Arc::clone(&settings),
                policy,
                evidence_id,
                context,
                ring,
                input_focus,
                opened: arrived,
                reply,
            },
            now,
        );
        turn.wake = Some(wake);
        turn.grant = grant;
        turn.restricted = restricted;
        turn.tokens = tokens;
        turn.evidence_broken = evidence_broken;
        let turn = self.turn.insert(turn);
        mic_job(
            cx.link,
            &turn.scope,
            MicJob::StartUtterance { key, from: start },
        );
        out.dialog_started();
        if !restricted {
            cx.show(Shown::Listening(uuid), now);
            if settings.wakeword.wake_sound && !chimed {
                out.earcon(&mut cx.output(), Earcon::WakeChime);
            }
        }
        self.turn_evidence(cx, out, now);
    }

    /// A button press starts a turn from 300 ms before it (§4.3).
    pub(crate) fn start_button_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        press: echomuse_presentation::button::ButtonTurn,
        now: Now,
    ) {
        let Some(capture) = cx.facts.capture.clone() else {
            cx.show(Shown::Cue(Cue::Error), now);
            return;
        };
        let current = cx.audio.timeline().capture_epoch();
        if current.map(|e| e.get()) != Some(press.epoch) {
            // The press's capture epoch has been replaced: no audio to listen to.
            cx.show(Shown::Cue(Cue::Error), now);
            return;
        }
        let (context, ring) = command_context(&cx.facts, out);
        let reply = self.supersede(cx, out, now);
        let scope = cx.ids.turn_scope(&capture);
        let uuid = Uuid::new_v4();
        let owner = SubOwner::new(uuid.to_string()).expect("a UUID is not empty");
        let press_sample = Sample(press.press_sample);
        let subscription = match cx.audio.open_button_turn(
            owner,
            press_sample,
            cx.facts.mono(now.at),
        ) {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::info!(device = %cx.facts.device, %error, "a button turn's subscription was refused");
                cx.show(Shown::Cue(Cue::Error), now);
                return;
            }
        };
        let epoch_nz = current.expect("checked above");
        let Some(epoch) = echomuse_turn::Epoch::new(epoch_nz.get()) else {
            return;
        };
        let key = StreamKey {
            session: cx.facts.session,
            subscription: subscription.clone(),
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
        let settings = Arc::clone(cx.facts.settings);
        let Ok((policy, evidence_id)) =
            Self::open_policy(cx, echomuse_recorder::TurnCause::Button, spec, now)
        else {
            // Nothing will listen on the subscription.
            cx.audio
                .close(&subscription, CloseReason::Closed, cx.facts.mono(now.at));
            return;
        };
        let mut analysis = Analysis::new(key.clone(), epoch_nz);
        analysis.cells_from = Some(TurnSpec::cells_start(mic_start));
        analysis.pending = None;
        let input_focus = FocusLease::acquire(
            cx.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            1,
        )
        .ok();
        let reply = reply.filter(|_| press.keeps_reply);
        let turn = self.turn.insert(Turn::open(
            Opening {
                uuid,
                scope,
                trigger: TriggerType::Button,
                subscription,
                analysis,
                settings: Arc::clone(&settings),
                policy,
                evidence_id,
                context,
                ring,
                input_focus,
                opened: now,
                reply,
            },
            now,
        ));
        cx.audio.open_analysis(
            cx.link,
            &turn.scope,
            key.clone(),
            echomuse_echo::EchoPlan {
                epoch,
                cells_from: TurnSpec::cells_start(mic_start),
                vad_from: mic_start.cell(),
            },
            cx.facts.wake.files.clone(),
        );
        mic_job(
            cx.link,
            &turn.scope,
            MicJob::StartUtterance {
                key,
                from: mic_start,
            },
        );
        out.dialog_started();
        cx.show(Shown::Listening(uuid), now);
        if settings.wakeword.wake_sound {
            out.earcon(&mut cx.output(), Earcon::WakeChime);
        }
    }

    /// Starts the reply turn of an open window at its first mic sample (§4.5).
    pub(super) fn start_reply_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        trigger: Sample,
        now: Now,
    ) {
        let Some(window) = self.window.take() else {
            return;
        };
        let start = Sample(trigger.0.div_ceil(512) * 512);
        let reply = ReplySpec::at_drain(
            window.origin,
            start,
            cx.facts.mono(window.opened_at),
            &echomuse_turn::EndpointPolicy::default(),
        );
        self.begin_reply_turn(cx, out, window, start, start, reply, now);
    }

    /// Opens a reply turn on `window`'s subscription: its utterance from `start`, its
    /// trigger at `trigger`, under `reply`'s guard and no-input rules.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn begin_reply_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        mut window: super::reply::ReplyWindow,
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
        let settings = Arc::clone(cx.facts.settings);
        let cause = echomuse_recorder::TurnCause::Reply {
            reopened_from: None,
        };
        let (mut policy, evidence_id) = match Self::open_policy(cx, cause, spec, now) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::warn!(device = %cx.facts.device, ?error, "a reply turn's plan was refused");
                Self::close_window_subscription(cx, &window, now);
                if let Some(question) = window.question {
                    self.resolve_continuation(
                        cx,
                        question,
                        ContinuationEnd::FollowUp(FollowUp::NoMic),
                    );
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
                        &mut cx.noting(policy.evidence_mut(), evidence_id, now),
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
                        &mut cx.noting(policy.evidence_mut(), evidence_id, now),
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
            cx.link,
            now.at,
            FocusKind::DialogInput,
            FocusOwner::new(uuid.to_string()),
            1,
        )
        .ok();
        let (context, ring) = command_context(&cx.facts, out);
        let mut turn = Turn::open(
            Opening {
                uuid,
                scope: window.scope,
                trigger: if origin == ReplyOrigin::HaConversation {
                    TriggerType::HaReply
                } else {
                    TriggerType::Reply
                },
                subscription: window.subscription,
                analysis: window.analysis,
                settings,
                policy,
                evidence_id,
                context,
                ring,
                input_focus,
                opened: now,
                reply: Some(ReplyContext {
                    expectation: window.expectation,
                    question: window.question,
                    chain: window.chain,
                }),
            },
            now,
        );
        turn.origin = origin;
        turn.tokens = tokens;
        turn.evidence_broken = evidence_broken;
        turn.reply_spec = Some(reply);
        let turn = self.turn.insert(turn);
        mic_job(
            cx.link,
            &turn.scope,
            MicJob::StartUtterance { key, from: start },
        );
        out.dialog_started();
        self.turn_evidence(cx, out, now);
    }

    pub(super) fn turn_news(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        news: StreamNews,
        now: Now,
    ) {
        match news {
            StreamNews::Ended(ended) => {
                let Some(turn) = self.turn.as_mut() else {
                    return;
                };
                turn.subscription = None;
                let Some(policy) = turn.policy.as_mut() else {
                    return;
                };
                let mut made = Vec::new();
                let control = match ended.cause {
                    EndCause::Muted => {
                        policy.mute(&mut made);
                        echomuse_recorder::TurnControl::Mute
                    }
                    EndCause::SessionLost => {
                        policy.session_lost(&mut made);
                        echomuse_recorder::TurnControl::SessionLost
                    }
                    EndCause::Overflow => {
                        policy.audio_overrun(&mut made);
                        echomuse_recorder::TurnControl::AudioOverrun
                    }
                    EndCause::Device(report)
                        if report.reason
                            == echomuse_protocol::streams::UplinkEndedReason::Overrun =>
                    {
                        policy.audio_overrun(&mut made);
                        echomuse_recorder::TurnControl::AudioOverrun
                    }
                    _ => {
                        policy.discontinuity(&mut made);
                        echomuse_recorder::TurnControl::Discontinuity
                    }
                };
                self.note_control(cx, control, &made, now);
                self.decisions(cx, out, made, now);
            }
            StreamNews::Mic | StreamNews::Other => self.turn_evidence(cx, out, now),
        }
    }

    /// Advances the turn policy over what has arrived and acts on its decisions.
    pub(super) fn turn_evidence(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        now: Now,
    ) {
        let facts = cx.facts;
        let mono = facts.mono(now.at);
        let recording = cx.recording;
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        let reply_kind = turn.expectation.as_ref().and_then(Expectation::reply_kind);
        let broken = turn.evidence_broken;
        let id = turn.evidence_id;
        let Some(policy) = turn.policy.as_mut() else {
            return;
        };
        let mut made = Vec::new();
        if broken {
            policy.worker_error(&mut made);
            self.note_control(cx, echomuse_recorder::TurnControl::WorkerError, &made, now);
        } else {
            let grammar = TurnGrammar {
                grammar: facts.grammar,
                reply: reply_kind,
            };
            if recording {
                let tap = echomuse_recorder::GrammarTap::new(&grammar);
                policy.advance(&tap, mono, &mut made);
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
                    decisions: made.clone(),
                };
                cx.note(now, || evidence);
            } else {
                policy.advance(&grammar, mono, &mut made);
            }
        }
        self.decisions(cx, out, made, now);
    }

    pub(super) fn turn_worker_error(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        now: Now,
    ) {
        let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) else {
            return;
        };
        let mut made = Vec::new();
        policy.worker_error(&mut made);
        self.note_control(cx, echomuse_recorder::TurnControl::WorkerError, &made, now);
        self.decisions(cx, out, made, now);
    }

    pub(super) fn turn_timer(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        let mono = cx.facts.mono(now.at);
        if self
            .turn
            .as_ref()
            .and_then(|t| t.response.as_ref())
            .is_some_and(|(_, until)| *until <= now.at)
        {
            self.close_response(cx, now);
        }
        let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) else {
            return;
        };
        if policy.next_deadline().is_some_and(|due| due <= mono) {
            let mut made = Vec::new();
            policy.on_timer(mono, &mut made);
            if let Some(id) = self.turn.as_ref().map(|t| t.evidence_id) {
                let decisions = made.clone();
                cx.note(now, || crate::evidence::Evidence::Timer {
                    turn: id,
                    decisions,
                });
            }
            self.decisions(cx, out, made, now);
        }
        if let Some(turn) = self.turn.as_mut()
            && let Some(lease) = turn.input_focus.as_mut()
            && let Some(echomuse_playback::FocusEvent::Rejected(code)) = lease.poll(cx.link, now.at)
        {
            tracing::warn!(device = %cx.facts.device, ?code, "the device refused a focus lease");
        }
    }

    fn decisions(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        decisions: Vec<Decision>,
        now: Now,
    ) {
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
                                cx,
                                question,
                                ContinuationEnd::FollowUp(FollowUp::Answered),
                            );
                        }
                    }
                }
                Decision::AskRecognizer { text } => self.ask_recognizer(cx, text),
                Decision::EndPending { .. } => self.set_listening(true),
                Decision::EndRevoked { .. } => self.set_listening(false),
                Decision::Commit(commit) => self.commit(cx, out, commit, now),
                Decision::Close { reason, .. } => {
                    self.take_utterance();
                    self.end_turn(cx, out, TerminalReason::from(reason), now);
                }
                Decision::ReopenReply(reopen) => self.reopen_reply(cx, out, reopen, now),
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
    fn reopen_reply(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reopen: echomuse_turn::ReplyReopen,
        now: Now,
    ) {
        let mono = cx.facts.mono(now.at);
        let Some(reply_kind) = self
            .turn
            .as_ref()
            .and_then(|t| t.reply_spec)
            .map(TurnKind::Reply)
        else {
            return;
        };
        let config = turn_config(cx.facts.settings, &reply_kind);
        let recording = cx.recording;
        let id = cx.ids.policy();
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
                self.end_turn(cx, out, TerminalReason::Interrupted, now);
                return;
            }
        };
        if recording {
            cx.link.effects.push(Effect::Evidence {
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
            notes: recording.then_some((&mut cx.link.effects, id, mono)),
        };
        if analysis
            .log
            .replay_history(analysis.epoch(), &mut pushes)
            .is_err()
        {
            turn.evidence_broken = true;
        }
        let key = analysis.key.clone();
        turn.policy = Some(policy);
        turn.reply_spec = Some(reply);
        mic_job(
            cx.link,
            &turn.scope,
            MicJob::StartUtterance { key, from: start },
        );
    }

    fn ask_recognizer(&mut self, cx: &mut ConversationCx<'_>, text: String) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let scope = turn.scope.clone();
        let uuid = turn.uuid;
        let call = cx.ids.call();
        self.calls.insert(
            call,
            Call::Recognize {
                turn: uuid,
                text: text.clone(),
            },
        );
        cx.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::Recognize {
                text,
                ha_device: cx.facts.ha_device.clone(),
            },
        });
    }

    // ---- commit ---------------------------------------------------------------------

    fn commit(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, commit: Commit, now: Now) {
        self.take_utterance();
        let mono = cx.facts.mono(now.at);
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
            cx.audio
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
        self.open_response(cx, now);
        if let Some(subscription) = subscription {
            cx.audio.close(&subscription, CloseReason::Committed, mono);
        }
        if let Some(key) = analysis_key {
            cx.audio.close_analysis(cx.link, key);
        }
        if let Some(lease) = self.turn.as_mut().and_then(|t| t.input_focus.take()) {
            cx.audio.release(cx.link, Some(lease), now);
        }
        let Some(pcm) = pcm else {
            tracing::warn!(device = %cx.facts.device, "the committed span is not held");
            self.end_turn(cx, out, TerminalReason::Interrupted, now);
            return;
        };
        if let Some(turn) = self.turn.as_mut() {
            turn.span_pcm = Some(Arc::clone(&pcm));
        }
        // Commands handled before STT, from the streaming text (§4.4).
        let command_text = commit.text.clone();
        if let Some(command) = cx.facts.grammar.context_command(&command_text, &context) {
            self.context_command(cx, out, command, command_text, now);
            return;
        }
        if restricted {
            // Another device answers this wake; only this device's media commands act.
            self.end_turn(cx, out, TerminalReason::ArbitrationLost, now);
            return;
        }
        let (uuid, scope, recheck) = {
            let turn = self.turn.as_ref().expect("present");
            (turn.uuid, turn.scope.clone(), commit.recheck)
        };
        if !recheck {
            self.transcribe(cx);
        } else if commit.source == echomuse_turn::TextSource::Server
            && self.recheck_on_server(cx, &commit, &pcm)
        {
            // The committed text is the pause server's words: the same server re-checks it.
        } else {
            cx.link.effects.push(Effect::Span {
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
    fn open_response(&mut self, cx: &mut ConversationCx<'_>, now: Now) {
        if !cx.audio.terms().afe_metadata {
            return;
        }
        let mono = cx.facts.mono(now.at);
        let Some(turn) = self.turn.as_mut() else {
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
        match cx
            .audio
            .open_response(owner, Sample(afe_from), mic_from, mono)
        {
            Ok(subscription) => {
                turn.response = Some((subscription, now.at + std::time::Duration::from_secs(30)));
            }
            Err(error) => {
                tracing::info!(device = %cx.facts.device, %error, "the response subscription could not open");
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

    /// Closes the turn's response subscription (30 s passed since its commit).
    fn close_response(&mut self, cx: &mut ConversationCx<'_>, now: Now) {
        let Some((subscription, _)) = self.turn.as_mut().and_then(|t| t.response.take()) else {
            return;
        };
        cx.audio
            .close(&subscription, CloseReason::Closed, cx.facts.mono(now.at));
    }

    /// Starts the STT copy and resolves the pipeline, in parallel.
    pub(super) fn transcribe(&mut self, cx: &mut ConversationCx<'_>) {
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
                match cx.audio.timeline().read_pcm(
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
        cx.link.effects.push(Effect::Span {
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
            let call = cx.ids.call();
            self.calls.insert(call, Call::Pipeline { turn: uuid });
            cx.link.effects.push(Effect::Ha {
                call,
                scope,
                request: HaRequest::Pipeline {
                    ha_device: cx.facts.ha_device.clone(),
                },
            });
        }
    }

    pub(crate) fn span_result(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        uuid: Uuid,
        result: Result<SpanOutput, echomuse_core::types::RuntimeError>,
        now: Now,
    ) {
        if !self.is_current(&uuid) {
            cx.counters.stale_results += 1;
            return;
        }
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                cx.worker_failed(&error);
                self.end_turn(cx, out, TerminalReason::Interrupted, now);
                return;
            }
        };
        match output {
            SpanOutput::Redecode { result, .. } => match result {
                Ok(redecoded) => {
                    if self.redecode_changed(&cx.facts, &redecoded) {
                        tracing::info!(device = %cx.facts.device, "the committed span re-decoded differently");
                        self.end_turn(cx, out, TerminalReason::Retry, now);
                    } else {
                        self.transcribe(cx);
                    }
                }
                Err(error) => {
                    tracing::warn!(device = %cx.facts.device, error, "the commit re-decode failed");
                    self.end_turn(cx, out, TerminalReason::Interrupted, now);
                }
            },
            SpanOutput::SttCopy { result, .. } => match result {
                Ok(copy) => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.stt_copy = Some(copy);
                    }
                    self.maybe_stt(cx, out, now);
                }
                Err(error) => {
                    tracing::warn!(device = %cx.facts.device, error, "the STT copy failed");
                    self.end_turn(cx, out, TerminalReason::Interrupted, now);
                }
            },
        }
    }

    /// Whether the re-decoded span's command differs from what the commit heard.
    fn redecode_changed(
        &self,
        facts: &ConversationFacts<'_>,
        redecoded: &echomuse_speech::AsrResult,
    ) -> bool {
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
                let located = locate_in_streaming(&transcript, lookahead, &facts.wake.phrase);
                echomuse_turn::text::cut_streaming(&transcript, &located)
            }
            None => transcript.text().to_owned(),
        };
        self.command_differs(&text)
    }

    /// Whether a re-check's command text differs from what the commit heard, other than by
    /// completing the final word (§4.6).
    pub(super) fn command_differs(&self, text: &str) -> bool {
        let Some((commit, _, _)) = self.turn.as_ref().and_then(|t| t.commit.as_ref()) else {
            return false;
        };
        echomuse_turn::text::recheck_differs(&commit.text, text)
    }

    /// The command in a transcript of the whole utterance that has no word timings (HA's,
    /// or the pause server's): for a wake turn, the wake phrase and anything said before it
    /// are removed near where the streaming words placed it (§4.1).
    pub(super) fn final_command(&self, facts: &ConversationFacts<'_>, raw: &str) -> String {
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
                let located = locate_in_streaming(&transcript, lookahead, &facts.wake.phrase);
                strip_final(raw, &located, &facts.wake.phrase)
                    .text
                    .trim()
                    .to_owned()
            }
            None => raw.trim().to_owned(),
        }
    }

    /// Sends the STT copy once both it and the pipeline are known.
    fn maybe_stt(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let Some(copy) = turn.stt_copy.clone() else {
            return;
        };
        if turn.origin == ReplyOrigin::HaConversation {
            self.ha_reply_run(cx, copy, now);
            return;
        }
        let pipeline = match &turn.pipeline {
            None => return,
            Some(Ok(pipeline)) => pipeline.clone(),
            Some(Err(error)) => {
                tracing::warn!(device = %cx.facts.device, %error, "no pipeline for STT");
                self.fail_turn(cx, out, TerminalReason::SttFailed, now);
                return;
            }
        };
        let uuid = turn.uuid;
        let scope = turn.scope.clone();
        let call = cx.ids.call();
        self.calls.insert(call, Call::Stt { turn: uuid });
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.stt_started = Some(now.at);
        }
        cx.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::Stt {
                target: SatelliteTarget {
                    pipeline,
                    ha_device: cx.facts.ha_device.clone(),
                },
                audio: copy.pcm,
            },
        });
    }

    pub(super) fn target(&self, facts: &ConversationFacts<'_>) -> Option<SatelliteTarget> {
        let turn = self.turn.as_ref()?;
        match &turn.pipeline {
            Some(Ok(pipeline)) => Some(SatelliteTarget {
                pipeline: pipeline.clone(),
                ha_device: facts.ha_device.clone(),
            }),
            _ => None,
        }
    }

    /// Home Assistant answered.
    pub(crate) fn ha_reply(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        call: CallId,
        reply: HaReply,
        now: Now,
    ) {
        let purpose = match &reply {
            // An intent run's events keep its call registered until its terminal event.
            HaReply::IntentEvent(event) if !event.is_terminal() => self.calls.get(&call).cloned(),
            HaReply::IntentDispatched(Ok(())) => self.calls.get(&call).cloned(),
            _ => self.calls.remove(&call),
        };
        let Some(purpose) = purpose else {
            cx.counters.stale_results += 1;
            return;
        };
        match (purpose, reply) {
            (Call::Pipeline { turn }, HaReply::Pipeline(result)) if self.is_current(&turn) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.pipeline = Some(result);
                }
                self.maybe_stt(cx, out, now);
            }
            (Call::Stt { turn }, HaReply::Stt(result)) if self.is_current(&turn) => {
                self.transcript(cx, out, result, now);
            }
            (Call::Recognize { turn, text }, HaReply::Recognize(result))
                if self.is_current(&turn) =>
            {
                let outcome = match result {
                    Ok(answers) => Ok(crate::io::recognized_class(&answers)),
                    Err(HaError::NotDispatched(_)) => Err(RecognizerError::Unavailable),
                    Err(HaError::Timeout) => Err(RecognizerError::Timeout),
                    Err(_) => Err(RecognizerError::Failed),
                };
                let mono = cx.facts.mono(now.at);
                let answer = RecognizerAnswer { text, outcome };
                if let Some(id) = self
                    .turn
                    .as_ref()
                    .filter(|t| t.policy.is_some())
                    .map(|t| t.evidence_id)
                {
                    let noted = answer.clone();
                    cx.note(now, || crate::evidence::Evidence::RecognizerAnswer {
                        turn: id,
                        answer: noted,
                    });
                }
                if let Some(policy) = self.turn.as_mut().and_then(|t| t.policy.as_mut()) {
                    policy.recognizer_answer(answer, mono);
                }
                // Takes effect at the next block boundary the policy evaluates.
            }
            (Call::Intent { turn }, HaReply::IntentDispatched(result))
                if self.is_current(&turn) =>
            {
                if result.is_err() {
                    // Nothing was sent: HA is not reachable, or the turn was cancelled.
                    self.calls.remove(&call);
                    self.intent_failed(cx, out, now);
                }
            }
            (Call::Intent { turn }, HaReply::IntentEvent(event)) if self.is_current(&turn) => {
                self.intent_event(cx, out, event, now);
            }
            (Call::Tts { turn }, HaReply::Tts(result)) if self.is_current(&turn) => match result {
                Ok(url) => {
                    if let Some(turn) = self.turn.as_mut() {
                        turn.facts.tts_url_at = Some(now.at);
                    }
                    self.speak_answer(cx, out, url.0, now);
                }
                Err(error) => {
                    tracing::warn!(device = %cx.facts.device, %error, "a spoken line could not be synthesized");
                    cx.show(Shown::Cue(Cue::Error), now);
                    self.answer_done(cx, out, None, now);
                }
            },
            (Call::TimerStatus { turn }, HaReply::TimerStatus(result))
                if self.is_current(&turn) =>
            {
                self.timer_status(cx, out, result, now);
            }
            (Call::CancelTimers { turn }, HaReply::CallIntent(outcome))
                if self.is_current(&turn) =>
            {
                self.timer_cancelled(cx, out, outcome, now);
            }
            (Call::Intent { .. }, HaReply::IntentEvent(event)) if event.is_terminal() => {
                // A superseded turn's run ended; its outcome was already recorded.
            }
            _ => cx.counters.stale_results += 1,
        }
    }

    fn transcript(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
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
                tracing::warn!(device = %cx.facts.device, %error, "HA's STT failed");
                self.fail_turn(cx, out, TerminalReason::SttFailed, now);
                return;
            }
        };
        turn.facts.stt_raw = Some(raw.clone());
        // The wake phrase is removed from the text sent on; the audio keeps it (§4.1).
        let text = self.final_command(&cx.facts, &raw);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.facts.stt_text = Some(text.clone());
        if echomuse_grammar::normalize(&text).is_empty() {
            // Nothing usable: say so and listen again without the wake word (§4.5).
            turn.facts.terminal = Some(TerminalReason::EmptyTranscript);
            turn.answering.follow_up = Some(Expectation::Retry);
            self.say(cx, out, Line::DidntCatch, now);
            return;
        }
        turn.stage = Stage::Thinking;
        cx.show(Shown::Thinking(&text), now);
        self.correct(cx, out, text, now);
    }

    /// Corrects the transcript before routing it (§4.7): off, it is routed at once;
    /// otherwise the runtime decides what to send, within 100 ms in `on` mode.
    fn correct(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, text: String, now: Now) {
        let target = self.target(&cx.facts);
        let Some(turn) = self.turn.as_ref() else {
            return;
        };
        let settings = turn.settings.transcript.clone();
        let off = settings.transcript_fix == echomuse_settings::values::TranscriptFixMode::Off;
        let Some(target) = target.filter(|_| !off) else {
            self.route(cx, out, text, now);
            return;
        };
        let request = crate::io::CorrectionRequest {
            scope: turn.scope.clone(),
            heard: text,
            wake_phrase: turn.wake.as_ref().map(|_| cx.facts.wake.phrase.clone()),
            contextual: turn.expectation.is_some()
                || matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply),
            settings,
            target,
        };
        let uuid = turn.uuid;
        let call = cx.ids.call();
        self.calls.insert(call, Call::Correct { turn: uuid });
        cx.link.effects.push(Effect::Correct { call, request });
    }

    /// Transcript correction answered.
    pub(crate) fn corrected(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        call: CallId,
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
            Some(Call::Correct { turn }) if self.is_current(&turn) => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.stt_fix = record;
                    // The record of a check running on beside the turn goes to the
                    // records service with the row, under this call.
                    turn.facts.fix_pending = record_follows.then_some(call);
                }
                if applied {
                    tracing::info!(device = %cx.facts.device, "transcript corrected");
                }
                self.route(cx, out, send, now);
            }
            _ => cx.counters.stale_results += 1,
        }
    }

    /// Sends the request to Home Assistant's own intent handling.
    pub(super) fn dispatch_to_ha(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        text: String,
        now: Now,
    ) {
        let Some(target) = self.target(&cx.facts) else {
            self.fail_turn(cx, out, TerminalReason::HaError, now);
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
        let call = cx.ids.call();
        self.calls.insert(call, Call::Intent { turn: uuid });
        cx.link.effects.push(Effect::Ha {
            call,
            scope,
            request: HaRequest::IntentTts {
                target,
                text,
                conversation,
            },
        });
    }

    fn intent_event(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        event: RunEvent,
        now: Now,
    ) {
        match event {
            RunEvent::AnswerDelta(delta) => {
                let Some(turn) = self.turn.as_mut() else {
                    return;
                };
                turn.answering.streamed.push_str(&delta);
                let text = turn.answering.streamed.clone();
                cx.show(Shown::AnswerSoFar(&text), now);
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
                cx.show(Shown::FinalAnswer(&end.speech), now);
            }
            RunEvent::TtsReady { url, .. } => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.tts_url_at = Some(now.at);
                }
                self.speak_answer(cx, out, url.0, now);
            }
            RunEvent::Ended => self.run_ended(cx, out, now),
            RunEvent::Rejected { code, message } | RunEvent::Failed { code, message } => {
                tracing::warn!(device = %cx.facts.device, code, message, "HA's intent run failed");
                self.intent_failed(cx, out, now);
            }
            RunEvent::Lost => {
                // The request may or may not have been carried out: never resent.
                tracing::warn!(device = %cx.facts.device, "HA's intent run was lost: outcome unknown");
                if let Some(turn) = self.turn.as_mut() {
                    turn.facts.terminal = Some(TerminalReason::OutcomeUnknown);
                    turn.answering.run_open = false;
                    if turn.answering.playing {
                        return;
                    }
                }
                self.say(cx, out, Line::NotSureItWorked, now);
            }
        }
    }

    fn intent_failed(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.answering.run_open = false;
        turn.facts.terminal = Some(TerminalReason::HaError);
        if turn.answering.playing {
            return;
        }
        self.say(cx, out, Line::SomethingWentWrong, now);
    }

    fn run_ended(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        turn.answering.run_open = false;
        if !turn.answering.playing {
            // HA answered with no audio.
            self.finish_turn(cx, out, now);
        }
    }

    // ---- the answer -----------------------------------------------------------------

    /// Plays the current turn's answer from `url` (HA's TTS, or a line's).
    pub(super) fn speak_answer(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        url: String,
        now: Now,
    ) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.answering.playing {
            // HA gives one TTS URL per run; a second is ignored.
            return;
        }
        turn.answering.playing = true;
        out.play_answer(&mut cx.output(), turn.uuid, url, now);
    }

    /// Says one of the controller's own lines for the current turn (HA synthesises it).
    pub(super) fn say(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        line: Line,
        now: Now,
    ) {
        let text = super::local::line_text(&cx.facts, &line, now);
        let target = self.target(&cx.facts);
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.stage != Stage::Speaking {
            turn.stage = Stage::Thinking;
        }
        turn.facts.response_text = Some(text.clone());
        turn.facts.intent_ended.get_or_insert(now.at);
        cx.show(Shown::FinalAnswer(&text), now);
        let (uuid, scope) = (turn.uuid, turn.scope.clone());
        match target {
            Some(target) => {
                let call = cx.ids.call();
                self.calls.insert(call, Call::Tts { turn: uuid });
                cx.link.effects.push(Effect::Ha {
                    call,
                    scope,
                    request: HaRequest::Tts { target, text },
                });
            }
            None => {
                // No pipeline to speak with (HA unreachable): the error cue answers.
                cx.show(Shown::Cue(Cue::Error), now);
                self.answer_done(cx, out, None, now);
            }
        }
    }

    /// Asks a question: the line, then a reply window expecting `expectation`.
    pub(super) fn ask(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        line: Line,
        expectation: Expectation,
        now: Now,
    ) {
        if let Some(turn) = self.turn.as_mut() {
            turn.answering.follow_up = Some(expectation);
        }
        self.say(cx, out, line, now);
    }

    /// A turn that failed after commit: an error cue and, when HA can still speak, a
    /// line (it cannot when HA itself failed: the error cue is the answer).
    pub(super) fn fail_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reason: TerminalReason,
        now: Now,
    ) {
        if let Some(turn) = self.turn.as_mut() {
            turn.facts.terminal = Some(reason);
        }
        self.end_turn(cx, out, reason, now);
    }

    /// The answer finished playing (or there was none).
    pub(super) fn answer_done(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reason: Option<PlaybackReason>,
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
            self.finish_turn(cx, out, now);
        }
    }

    /// A turn ran to its end: completed unless something recorded otherwise.
    pub(super) fn finish_turn(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        let reason = self
            .turn
            .as_ref()
            .and_then(|t| t.facts.terminal)
            .unwrap_or(TerminalReason::Completed);
        self.end_turn(cx, out, reason, now);
    }

    /// Ends the turn: releases what it holds, persists it, and opens its reply window.
    pub(super) fn end_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reason: TerminalReason,
        now: Now,
    ) {
        let Some(mut turn) = self.turn.take() else {
            return;
        };
        if let Some(policy) = turn.policy.take() {
            turn.utterance = Some(policy.into_trace());
        }
        // A committed turn's response subscription stays open a second after the turn
        // ends, and the turn's chart, recording and row wait for it (`linger`).
        // Otherwise the chart and the turn recording are made now, from what the turn's
        // subscriptions brought, before their audio goes (unless the session's loss or a
        // mute already made them).
        let lingers = turn.response.is_some();
        if !lingers && turn.facts.afe_evidence.is_none() {
            super::record::chart_turn(cx.audio, &mut turn, false);
        }
        // Cancelling the scope withdraws interest in every request still out for it.
        turn.scope.cancel();
        self.calls.retain(|_, call| call.turn() != turn.uuid);
        let mono = cx.facts.mono(now.at);
        if let Some(subscription) = turn.subscription.take() {
            let reason = if reason == TerminalReason::ArbitrationLost {
                CloseReason::ArbitrationLost
            } else {
                CloseReason::Closed
            };
            cx.audio.close(&subscription, reason, mono);
        }
        if lingers {
            // The workers forget the stream; its AFE records and Silero cells stay with
            // the turn for the chart.
            if let Some(key) = turn.analysis.as_ref().map(|a| a.key.clone()) {
                cx.audio.close_analysis(cx.link, key);
            }
        } else if let Some(analysis) = turn.analysis.take() {
            cx.audio.close_analysis(cx.link, analysis.key);
        }
        // The question's early-answer watch ends with its turn (an early answer took it
        // before ending the turn).
        if self.watch.as_ref().is_some_and(|w| w.question == turn.uuid) {
            self.close_question_watch(cx, now);
        }
        if let Some(grant) = turn.grant.take() {
            let _ = cx.facts.arbiter.release(grant);
        }
        let lease = turn.input_focus.take();
        cx.audio.release(cx.link, lease, now);
        out.end_answers_of(&mut cx.output(), turn.uuid);
        out.dialog_ended(&mut cx.output(), now);
        cx.counters.turns_finished += 1;
        let completed = reason == TerminalReason::Completed;
        let follow_up = turn.answering.follow_up.clone();
        let early = turn.answering.early_answer;
        // The window opens only once the question has finished playing (§4.5); an early
        // answer's reply turn opens at once instead.
        let opens_window = !early
            && follow_up.is_some()
            && (completed || reason == TerminalReason::EmptyTranscript)
            && turn.facts.playback_reason == Some(PlaybackReason::Drained);
        if !turn.restricted {
            let end = if completed {
                ViewEnd::Completed {
                    follow_up: opens_window || early,
                }
            } else {
                ViewEnd::Abandoned
            };
            cx.show(Shown::Done(end), now);
            if let Some(cue) = Cue::for_turn_end(reason, turn.trigger) {
                cx.show(Shown::Cue(cue), now);
            }
            if reason == TerminalReason::NoInput
                && matches!(turn.trigger, TriggerType::Wake | TriggerType::Button)
                && turn.settings.wakeword.wake_sound
            {
                out.earcon(&mut cx.output(), Earcon::NoInputChime);
            }
        }
        // A reply window that heard nothing is not a turn: it resolves its question.
        let reply = matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply);
        if reply && !turn.heard {
            if let Some((response, _)) = turn.response.take() {
                cx.audio.close(&response, CloseReason::Closed, mono);
            }
            if let Some(question) = turn.reply_to {
                let end = if reason == TerminalReason::NoInput {
                    ContinuationEnd::Ended(TerminalReason::ReplyTimeout)
                } else {
                    ContinuationEnd::Ended(reason)
                };
                self.resolve_continuation(cx, question, end);
            }
            return;
        }
        // An answer that asked something either opens its window, or records why not.
        let continuation = match (&follow_up, opens_window, turn.facts.playback_reason) {
            (None, _, _) => None,
            (Some(_), _, _) if early => Some(Continuation::Pending),
            (Some(_), true, _) => Some(Continuation::Pending),
            (Some(_), false, Some(PlaybackReason::Cancelled)) => Some(Continuation::Resolved(
                ContinuationEnd::FollowUp(FollowUp::PromptCancelled),
            )),
            (Some(_), false, _) => Some(Continuation::Resolved(ContinuationEnd::FollowUp(
                FollowUp::PromptFailed,
            ))),
        };
        if let Some(run) = turn.ha_run {
            cx.link.effects.push(Effect::Satellite(
                crate::io::SatelliteCommand::FinishRunResponse {
                    run,
                    success: completed,
                },
            ));
        }
        let (uuid, chain, origin) = (turn.uuid, turn.chain, turn.origin);
        self.persist_turn(cx, turn, reason, continuation, now);
        if let Some(expectation) = follow_up.filter(|_| opens_window) {
            self.open_reply_window(cx, out, Some(uuid), expectation, chain, origin, now);
        }
    }

    /// Mute or session loss: the turn ends at once, whatever stage it is in.
    pub(super) fn revoke_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reason: TerminalReason,
        now: Now,
    ) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if let Some(policy) = turn.policy.as_mut() {
            let mut made = Vec::new();
            let control = if reason == TerminalReason::Muted {
                policy.mute(&mut made);
                echomuse_recorder::TurnControl::Mute
            } else {
                policy.session_lost(&mut made);
                echomuse_recorder::TurnControl::SessionLost
            };
            self.note_control(cx, control, &made, now);
        }
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.answering.run_open {
            // The run may have reached HA: its outcome is unknown, and it is never
            // resent (§3.3.3).
            tracing::info!(device = %cx.facts.device, turn = %turn.uuid, "a revoked turn's intent run has an unknown outcome");
            turn.facts.unknown_outcome = true;
        }
        turn.subscription = None;
        self.end_turn(cx, out, reason, now);
    }

    /// Builds the ended turn's row and hands it to the records service, or, while its
    /// response subscription lingers, keeps it with the turn until that closes.
    fn persist_turn(
        &mut self,
        cx: &mut ConversationCx<'_>,
        turn: Turn,
        reason: TerminalReason,
        continuation: Option<Continuation>,
        now: Now,
    ) {
        match super::record::turn_record(&cx.facts, cx.audio, &turn, reason, continuation, now) {
            Ok((record, audio)) => {
                if let Some((subscription, cap)) = turn.response.clone() {
                    self.linger(turn, subscription, cap, record, audio, now);
                    return;
                }
                let fix_follows = turn.facts.fix_pending;
                write_turn(cx, record, audio, fix_follows);
            }
            Err(error) => {
                tracing::warn!(device = %cx.facts.device, ?error, "a turn row could not be built");
                if let Some((subscription, _)) = turn.response {
                    cx.audio
                        .close(&subscription, CloseReason::Closed, cx.facts.mono(now.at));
                }
            }
        }
    }

    pub(super) fn resolve_continuation(
        &mut self,
        cx: &mut ConversationCx<'_>,
        question: Uuid,
        end: ContinuationEnd,
    ) {
        // A question whose row lingers with its response subscription takes the final
        // value into that row: no row exists yet for an update to find.
        if self.resolve_lingering(question, end) {
            return;
        }
        // The records service puts it into the question's row even when that row is
        // still waiting for its correction record.
        let call = cx.ids.call();
        cx.link.effects.push(Effect::Persist {
            call,
            request: PersistRequest::Continuation {
                turn_uuid: question,
                end,
            },
        });
    }
}

/// Hands a turn's row to the records service.
pub(super) fn write_turn(
    cx: &mut ConversationCx<'_>,
    record: echomuse_records::TurnRecord,
    audio: echomuse_records::TurnAudio,
    fix_follows: Option<CallId>,
) {
    let call = cx.ids.call();
    // A correction check still running beside the turn sends its record to the records
    // service, which holds the row for it (at most the check's limit): the row is written
    // even if this session ends first.
    cx.link.effects.push(Effect::Persist {
        call,
        request: PersistRequest::Turn {
            record: Box::new(record),
            audio,
            fix_follows,
        },
    });
}
