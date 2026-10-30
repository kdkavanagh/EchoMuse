//! The conversation (spec §4.4–§4.7, §3.3.3): the turn, its policy and the requests
//! made for it, the reply window, the early-answer watch, the turns whose response
//! subscriptions linger, and what "repeat" would say again.
//!
//! [`Conversation`] owns all of that. Its children divide the work: `turn` (starting a
//! turn, endpointing, STT, routing, the answer, ending it), `local` (commands handled
//! without Home Assistant), `reply` (reply windows and HA-started conversations), `early`
//! (the early answer), `linger` (rows waiting for their response subscription),
//! `record` (turn rows and charts) and `pause_server` (pause transcription).
//!
//! It plays nothing itself: it asks [`Output`] to play, interrupt or end an answer by the
//! turn's identity, and hears back through [`Notices`], which it checks against the turn
//! they name. A wake reaches it as an owned [`WakeTransfer`] the machine admitted.

mod early;
mod linger;
mod local;
pub(crate) mod pause_server;
mod record;
mod reply;
mod turn;

use std::collections::HashMap;

use echomuse_presentation::{button::TurnActivity, phase::ActorState};
use echomuse_protocol::MessageId;
use echomuse_records::vocab::{
    ContinuationEnd, FollowUp, PlaybackReason, TerminalReason, TriggerType,
};
use echomuse_turn::{Decision, Epoch, EvidenceError, Sample, TurnPolicy, TurnSpec};
use echomuse_uplink::{Delivery, SubscriptionId};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    analysis::{Analysis, Fresh, StreamNews, WorkerEvidence, push_worker_evidence},
    evidence::{Evidence, Noting},
    io::{CallId, Now},
    output::{AnswerEnded, Notices, Output},
    session::{ConversationCx, ConversationFacts},
    speech::{MicOutput, StreamKey},
    wire::CandidateEnd,
};

use self::{
    early::QuestionWatch,
    linger::Lingering,
    local::Expectation,
    reply::{Chain, ReplyWindow},
    turn::{Call, Stage, Turn},
};

/// The last answer this device spoke, for "repeat" (§4.4). Kept in memory only: its
/// audio, when it ended, and the question it asked, which a repeat asks again.
#[derive(Clone, Debug)]
struct LastAnswer {
    url: String,
    ended_at: Instant,
    follow_up: Option<Expectation>,
}

/// A reply window's question taken over by a new turn (a wake or a button press during
/// the window, §4.5): moved into that turn once.
struct ReplyContext {
    expectation: Expectation,
    question: Option<Uuid>,
    chain: Chain,
}

/// Which of the conversation's analysed streams.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stream {
    Turn,
    Window,
    /// The early-answer watch while the controller's own question plays.
    Watch,
}

/// The conversation. See the module documentation.
#[derive(Default)]
pub(crate) struct Conversation {
    turn: Option<Turn>,
    window: Option<ReplyWindow>,
    watch: Option<QuestionWatch>,
    /// Ended turns whose response subscriptions are still open, with their rows.
    lingering: Vec<Lingering>,
    /// Requests whose answers have not arrived, by what they are for.
    calls: HashMap<CallId, Call>,
    last_answer: Option<LastAnswer>,
}

impl Conversation {
    // ---- summaries ------------------------------------------------------------------

    /// The open turn's state; `None` without one.
    pub(crate) fn turn_state(&self) -> Option<ActorState> {
        self.turn.as_ref().map(Turn::actor_state)
    }

    pub(crate) fn window_open(&self) -> bool {
        self.window.is_some()
    }

    /// What the button acts on.
    pub(crate) fn activity(&self) -> TurnActivity {
        if let Some(turn) = &self.turn {
            let reply = matches!(turn.trigger, TriggerType::Reply | TriggerType::HaReply);
            return if reply && !turn.heard {
                TurnActivity::ReplyWindow
            } else {
                TurnActivity::Turn
            };
        }
        if self.window.is_some() {
            return TurnActivity::ReplyWindow;
        }
        TurnActivity::Idle
    }

    /// When the conversation next has something to do on time.
    pub(crate) fn next_deadline(&self, facts: &ConversationFacts<'_>) -> Option<Instant> {
        let turn = self.turn.as_ref().and_then(|turn| {
            let policy = turn.next_deadline().map(|at| facts.instant(at));
            [policy, turn.focus_deadline()].into_iter().flatten().min()
        });
        [turn, self.window_deadline(), self.linger_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    // ---- streams --------------------------------------------------------------------

    fn stream_of(&self, subscription: &SubscriptionId) -> Option<Stream> {
        if self
            .turn
            .as_ref()
            .is_some_and(|t| t.subscription.as_ref() == Some(subscription))
        {
            return Some(Stream::Turn);
        }
        if self
            .window
            .as_ref()
            .is_some_and(|w| &w.subscription == subscription)
        {
            return Some(Stream::Window);
        }
        if self
            .watch
            .as_ref()
            .is_some_and(|w| &w.subscription == subscription)
        {
            return Some(Stream::Watch);
        }
        None
    }

    fn stream_by_key(&self, key: &StreamKey) -> Option<Stream> {
        if self
            .turn
            .as_ref()
            .and_then(|t| t.analysis.as_ref())
            .is_some_and(|a| &a.key == key)
        {
            return Some(Stream::Turn);
        }
        if self.window.as_ref().is_some_and(|w| w.analysis.key == *key) {
            return Some(Stream::Window);
        }
        if self.watch.as_ref().is_some_and(|w| w.analysis.key == *key) {
            return Some(Stream::Watch);
        }
        None
    }

    /// Whether a worker result for `key` is the conversation's: routed by the stream's
    /// current owner, so a result queued before a candidate became a turn reaches it.
    pub(crate) fn owns_key(&self, key: &StreamKey) -> bool {
        self.stream_by_key(key).is_some()
    }

    fn analysis_mut(&mut self, stream: Stream) -> Option<&mut Analysis> {
        match stream {
            Stream::Turn => self.turn.as_mut().and_then(|t| t.analysis.as_mut()),
            Stream::Window => self.window.as_mut().map(|w| &mut w.analysis),
            Stream::Watch => self.watch.as_mut().map(|w| &mut w.analysis),
        }
    }

    /// Routes one chunk (or the end) of an analysed subscription; anything else (a
    /// closed subscription's late audio) is dropped.
    pub(crate) fn analysed_delivery(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        delivery: Delivery,
        now: Now,
    ) {
        let Some(stream) = self.stream_of(delivery.subscription()) else {
            return;
        };
        let news = match delivery {
            Delivery::Ended(ended) => StreamNews::Ended(ended),
            Delivery::Chunk(chunk) => {
                let (analysis, scope) = match stream {
                    Stream::Turn => {
                        let Some(turn) = self.turn.as_mut() else {
                            return;
                        };
                        let Some(analysis) = turn.analysis.as_mut() else {
                            return;
                        };
                        (analysis, &turn.scope)
                    }
                    Stream::Window => {
                        let Some(window) = self.window.as_mut() else {
                            return;
                        };
                        (&mut window.analysis, &window.scope)
                    }
                    Stream::Watch => {
                        let Some(watch) = self.watch.as_mut() else {
                            return;
                        };
                        (&mut watch.analysis, &watch.scope)
                    }
                };
                let epoch = analysis.epoch();
                let Some((news, fresh)) = analysis.chunk(cx.link, scope, chunk) else {
                    return;
                };
                self.fresh(cx, stream, epoch, fresh, now);
                news
            }
        };
        match stream {
            Stream::Turn => self.turn_news(cx, out, news, now),
            Stream::Window => self.window_news(cx, out, news, now),
            Stream::Watch => self.watch_news(cx, out, news, now),
        }
    }

    /// Loudness cells or a gap for the policy that holds the stream.
    fn fresh(
        &mut self,
        cx: &mut ConversationCx<'_>,
        stream: Stream,
        epoch: Epoch,
        fresh: Fresh,
        now: Now,
    ) {
        if fresh.is_nothing() {
            return;
        }
        match stream {
            Stream::Turn => {
                let result = self
                    .turn
                    .as_mut()
                    .and_then(|turn| turn.pushes(cx, now))
                    .map(|mut pushes| fresh.push(epoch, &mut pushes));
                if let Some(Err(error)) = result {
                    self.evidence_failed(cx, error);
                }
            }
            Stream::Watch => self.watch_assembler(cx, |assembler| fresh.push(epoch, assembler)),
            Stream::Window => {}
        }
    }

    /// The microphone worker's output for one of the conversation's streams.
    pub(crate) fn mic_output(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        key: &StreamKey,
        output: MicOutput,
        now: Now,
    ) {
        let Some(stream) = self.stream_by_key(key) else {
            return;
        };
        let failed = output.failure.is_some();
        let fresh = self
            .analysis_mut(stream)
            .and_then(|analysis| analysis.worker_evidence(output.vad, output.echo, output.asr));
        if let Some(evidence) = fresh {
            self.worker_evidence(cx, stream, key.epoch, evidence, now);
        }
        if failed {
            self.stream_failed(cx, out, stream, now);
            return;
        }
        match stream {
            Stream::Turn => self.turn_evidence(cx, out, now),
            Stream::Watch => self.watch_evidence(cx, out, now),
            Stream::Window => {}
        }
    }

    fn worker_evidence(
        &mut self,
        cx: &mut ConversationCx<'_>,
        stream: Stream,
        epoch: Epoch,
        evidence: WorkerEvidence,
        now: Now,
    ) {
        match stream {
            Stream::Turn => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.note_tokens(&evidence.asr);
                }
                // The pause server is asked when the worker starts a pause decode; should
                // that notice have been lost, the pause decode's own result asks.
                for observation in &evidence.asr {
                    if observation.kind == echomuse_turn::AsrKind::Finalized {
                        self.ask_pause_server(cx, observation.through, now);
                    }
                }
                let result = self
                    .turn
                    .as_mut()
                    .and_then(|turn| turn.pushes(cx, now))
                    .map(|mut pushes| push_worker_evidence(&mut pushes, epoch, evidence));
                if let Some(Err(error)) = result {
                    self.evidence_failed(cx, error);
                }
            }
            Stream::Watch => {
                self.watch_assembler(cx, |assembler| {
                    push_worker_evidence(assembler, epoch, evidence)
                });
            }
            Stream::Window => {}
        }
    }

    /// The microphone worker failed on stream `key`.
    pub(crate) fn mic_failed(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        key: &StreamKey,
        now: Now,
    ) {
        if let Some(stream) = self.stream_by_key(key) {
            self.stream_failed(cx, out, stream, now);
        }
    }

    /// A worker could not analyse the stream: its evidence is unknown from here.
    fn stream_failed(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        stream: Stream,
        now: Now,
    ) {
        match stream {
            Stream::Turn => self.turn_worker_error(cx, out, now),
            Stream::Window => {
                self.close_window(cx, ContinuationEnd::Ended(TerminalReason::Interrupted), now)
            }
            Stream::Watch => self.close_question_watch(cx, now),
        }
    }

    /// The assembler refused evidence (it conflicts or is out of order): the turn's
    /// evidence can no longer be trusted, so it closes as a worker error.
    fn evidence_failed(&mut self, cx: &ConversationCx<'_>, error: EvidenceError) {
        tracing::warn!(device = %cx.facts.device, ?error, "turn evidence refused");
        if let Some(turn) = self.turn.as_mut() {
            turn.evidence_broken = true;
        }
    }

    // ---- the session recorder's view of the turn policy -----------------------------

    /// Notes a control event the live turn's policy took, with the decisions it made.
    fn note_control(
        &self,
        cx: &mut ConversationCx<'_>,
        control: echomuse_recorder::TurnControl,
        decisions: &[Decision],
        now: Now,
    ) {
        if let Some(turn) = self.turn.as_ref().filter(|t| t.policy.is_some()) {
            let id = turn.evidence_id;
            let decisions = decisions.to_vec();
            cx.note(now, || Evidence::Control {
                turn: id,
                control,
                decisions,
            });
        }
    }

    /// Opens a turn policy under the session's settings, and notes it with `cause`.
    /// Returns the policy and the recorder's identity for it.
    fn open_policy(
        cx: &mut ConversationCx<'_>,
        cause: echomuse_recorder::TurnCause,
        spec: TurnSpec,
        now: Now,
    ) -> Result<(TurnPolicy, u64), EvidenceError> {
        let config = turn::turn_config(cx.facts.settings, &spec.kind);
        let policy = TurnPolicy::new(config, spec.clone(), cx.facts.mono(now.at))?;
        let id = cx.ids.policy();
        cx.note(now, || Evidence::TurnOpened {
            turn: id,
            cause,
            config,
            spec,
        });
        Ok((policy, id))
    }

    // ---- output -------------------------------------------------------------------

    /// What a render event meant, checked against the turn it names.
    pub(crate) fn output_notices(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        notices: Notices,
        now: Now,
    ) {
        if let Some((uuid, first_frame)) = notices.first_frame
            && let Some(current) = self.turn.as_mut().filter(|t| t.uuid == uuid)
            && current.facts.first_frame_mono_ns.is_none()
        {
            current.facts.first_frame_mono_ns = first_frame;
        }
        if let Some(audible) = notices.audible
            && let Some(current) = self.turn.as_mut().filter(|t| t.uuid == audible.turn)
        {
            // The answer's turn speaks from now, and a question it asks is watched for an
            // early answer.
            current.stage = Stage::Speaking;
            current.facts.audible_at.get_or_insert(now.at);
            if !audible.cancelled {
                self.question_audible(cx, audible.turn, now);
            }
        }
        if let Some(ended) = notices.ended {
            self.answer_ended(cx, out, ended, now);
        }
        if notices.ha_conversation {
            self.open_ha_conversation(cx, out, now);
        }
    }

    /// An answer ended. Only the current turn's: a question an early answer cut off,
    /// or a superseded turn's answer, changes nothing.
    fn answer_ended(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        ended: AnswerEnded,
        now: Now,
    ) {
        let AnswerEnded {
            turn,
            reason,
            completed_url,
        } = ended;
        let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn) else {
            return;
        };
        current.facts.playback_ended = Some(now.at);
        if reason == PlaybackReason::Drained
            && let Some(url) = completed_url
        {
            self.last_answer = Some(LastAnswer {
                url,
                ended_at: now.at,
                follow_up: current.answering.follow_up.clone(),
            });
        }
        if reason != PlaybackReason::Drained && reason != PlaybackReason::Cancelled {
            current.facts.terminal.get_or_insert(match reason {
                PlaybackReason::ResponseTimeout => TerminalReason::ResponseTimeout,
                _ => TerminalReason::Interrupted,
            });
        }
        self.answer_done(cx, out, Some(reason), now);
    }

    /// A focus lease acknowledgement for the turn's input focus.
    pub(crate) fn focus_ack(
        &mut self,
        id: &MessageId,
        outcome: &echomuse_playback::AckOutcome,
    ) -> bool {
        self.turn
            .as_mut()
            .is_some_and(|turn| turn.focus_ack(id, outcome))
    }

    // ---- what the machine hands on --------------------------------------------------

    /// `wake.candidate_end` for a candidate that already became the turn.
    pub(crate) fn candidate_end(&mut self, end: &CandidateEnd) {
        if let Some(wake) = self
            .turn
            .as_mut()
            .and_then(|t| t.wake.as_mut())
            .filter(|w| w.message.candidate.candidate_id == end.candidate_id)
        {
            wake.support_end = Some(end.support_end);
            wake.end_peak = end.peak_smoothed;
        }
    }

    pub(crate) fn timers(&mut self, cx: &mut ConversationCx<'_>, out: &mut Output, now: Now) {
        self.turn_timer(cx, out, now);
        self.linger_timers(cx, now);
        self.window_timer(cx, now);
    }

    /// The button cancelled the turn, or the reply window.
    pub(crate) fn button_cancel(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        now: Now,
    ) {
        if self.turn.is_some() {
            self.end_turn(cx, out, TerminalReason::Superseded, now);
        } else {
            self.close_window(
                cx,
                ContinuationEnd::FollowUp(FollowUp::PromptCancelled),
                now,
            );
        }
    }

    /// Mute or session loss, after capture was revoked and the subscriptions ended: the
    /// turn and the reply window end.
    pub(crate) fn revoke(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        reason: TerminalReason,
        now: Now,
    ) {
        self.revoke_turn(cx, out, reason, now);
        self.close_window(cx, ContinuationEnd::Ended(reason), now);
    }

    /// Whether a delivery is the open turn's response subscription's.
    pub(crate) fn owns_response(&self, subscription: &SubscriptionId) -> bool {
        self.turn
            .as_ref()
            .and_then(|t| t.response.as_ref())
            .is_some_and(|(response, _)| response == subscription)
    }

    /// The microphone worker is starting a pause decode of stream `key`'s utterance.
    pub(crate) fn pause_decoding(
        &mut self,
        cx: &mut ConversationCx<'_>,
        key: &StreamKey,
        through: Sample,
        now: Now,
    ) {
        let ours = self
            .turn
            .as_ref()
            .and_then(|t| t.analysis.as_ref())
            .is_some_and(|a| a.key == *key);
        if ours {
            self.ask_pause_server(cx, through, now);
        }
    }
}

impl Turn {
    /// The turn's assembler, noting accepted pushes while a recording runs.
    fn pushes<'b>(&'b mut self, cx: &'b mut ConversationCx<'_>, now: Now) -> Option<Noting<'b>> {
        let id = self.evidence_id;
        let assembler = self.policy.as_mut()?.evidence_mut();
        Some(cx.noting(assembler, id, now))
    }
}
