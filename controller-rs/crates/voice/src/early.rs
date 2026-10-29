//! The early answer (spec §4.5; post-AFE architecture §16.6 "Early answer"): while the
//! controller's own question plays ("AM or PM?", "Which one?", "Sorry, I didn't catch
//! that."), a reply subscription listens, and an answer that starts before the question
//! has finished cuts the question off and becomes the reply turn at once.
//!
//! The watch opens when the question becomes audible. Its subscription carries live mic,
//! the reference and ten seconds of loudness history, like a reply window's. The
//! microphone worker runs VAD, echo labels and a streaming decode over it from the first
//! live mic sample, and the evidence assembler releases the cells in sample order to the
//! turn crate's [`EarlyAnswerWatch`]: 15 consecutive cells (480 ms) of clear near-end
//! speech (VAD ≥ 0.85, at least 12 dB above the room, and an echo label saying it is not
//! the device's own sound). Unknown evidence never qualifies, so the question's own words
//! and a missing reference keep the question playing.
//!
//! When the watch fires, the asking turn ends (its row records the continuation as
//! pending, and its answer is cancelled), and the reply turn opens on the watch's
//! subscription with no opening guard: its utterance starts 300 ms before the answer's
//! onset, its trigger is the onset, and it gets a fresh decode and echo labels from its
//! start, with the loudness and VAD already heard. When the question plays to its end
//! instead, the watch closes and the reply window opens as usual.
//!
//! Only the controller's own questions are watched. A question Home Assistant asks
//! (`continue_conversation`) is HA's answer; its reply window opens at the drain.

use echomuse_core::types::{TurnId, WorkScope};
use echomuse_turn::{
    EarlyAnswerPolicy, EarlyAnswerWatch, Epoch, EvidenceAssembler, EvidenceError, EvidencePlan,
    Released, Sample, Samples,
};
use echomuse_uplink::{CloseReason, Owner as SubOwner, SubscriptionId};
use uuid::Uuid;

use crate::{
    analysis::{Analysis, Owner, StreamNews},
    io::Now,
    local::Expectation,
    machine::Voice,
    reply::ReplyWindow,
    speech::{MicJob, ReferenceJob, StreamKey},
};

/// The pre-roll before an early answer's onset that its utterance keeps (post-AFE §16.6).
const PRE_ROLL: Samples = Samples(4_800);

/// The watch over one question.
pub(crate) struct QuestionWatch {
    /// The asking turn.
    pub(crate) question: Uuid,
    pub(crate) subscription: SubscriptionId,
    pub(crate) analysis: Analysis,
    /// The reply turn's scope, should the answer come early.
    pub(crate) scope: WorkScope,
    /// Where the subscription's loudness history starts.
    pub(crate) cells_from: Sample,
    /// Releases the cells in order once the first live mic sample is known.
    pub(crate) assembler: Option<EvidenceAssembler>,
    /// The first cell the watch judges (where its live mic starts).
    pub(crate) start: Option<Sample>,
    pub(crate) watch: EarlyAnswerWatch,
    /// The evidence could not be put in order: the watch stops judging.
    pub(crate) broken: bool,
}

/// Whether the controller asked this itself (and so watches for an early answer).
pub(crate) fn own_question(expectation: &Expectation) -> bool {
    match expectation {
        Expectation::Retry
        | Expectation::Meridiem { .. }
        | Expectation::CancelMeridiem { .. }
        | Expectation::WhichTimer { .. }
        | Expectation::WhichAlarm { .. } => true,
        Expectation::Conversation { .. } | Expectation::HaConversation => false,
    }
}

impl Voice {
    /// The current turn's answer became audible: when it asks the controller's own
    /// question, watch for an answer that starts before it ends.
    pub(crate) fn question_audible(&mut self, question: Uuid, now: Now) {
        if self.question_watch.is_some() {
            return;
        }
        let Some(turn) = self.turn.as_ref().filter(|t| t.uuid == question) else {
            return;
        };
        let watched = !turn.restricted
            && turn.answering.follow_up.as_ref().is_some_and(own_question)
            && turn.chain.allows(now.at);
        if !watched {
            return;
        }
        let (Some(capture), Some(epoch)) =
            (self.capture.clone(), self.uplink.timeline().capture_epoch())
        else {
            return;
        };
        let Some(turn_epoch) = Epoch::new(epoch.get()) else {
            return;
        };
        // Ten seconds of loudness before the question, for the room's background level;
        // the device clips the request to what its ring still holds.
        let heard = self.mic_end(epoch).unwrap_or(Sample(0));
        let cells_from = (heard - Samples(160_000)).round_down(512);
        let owner = SubOwner::new(format!("early-{question}")).expect("not empty");
        let opened = match self.uplink.open_reply(owner, cells_from, self.mono(now.at)) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::info!(device = %self.device, %error, "the early-answer watch could not open");
                return;
            }
        };
        self.apply_grants(&opened.grants);
        self.next_turn += 1;
        let scope = capture.for_turn(TurnId(self.next_turn));
        let key = StreamKey {
            session: self.session,
            subscription: opened.subscription_id.clone(),
            epoch: turn_epoch,
        };
        let mut analysis = Analysis::new(key.clone(), epoch);
        analysis.cells_from = Some(cells_from.cell());
        self.question_watch = Some(QuestionWatch {
            question,
            subscription: opened.subscription_id,
            analysis,
            scope,
            cells_from,
            assembler: None,
            start: None,
            watch: EarlyAnswerWatch::new(EarlyAnswerPolicy::default()),
            broken: false,
        });
        self.mic_job(
            Owner::Watch,
            MicJob::Open {
                key: key.clone(),
                plan: echomuse_echo::EchoPlan {
                    epoch: turn_epoch,
                    cells_from: cells_from.cell(),
                    vad_from: heard.cell(),
                },
            },
        );
        self.reference_job(
            Owner::Watch,
            ReferenceJob::Open {
                key,
                graph: self.wake.files.clone(),
            },
        );
    }

    /// Ends the watch without an early answer: the question played out, or its turn ended.
    pub(crate) fn close_question_watch(&mut self, now: Now) {
        if let Some(watch) = self.question_watch.take() {
            let _ = self
                .uplink
                .close(&watch.subscription, CloseReason::Closed, self.mono(now.at));
            self.close_analysis(watch.analysis.key);
        }
    }

    pub(crate) fn watch_news(&mut self, news: StreamNews, now: Now) {
        match news {
            StreamNews::Ended(_) => {
                // The device ended the subscription (mute, a new capture epoch): the
                // question plays on unwatched.
                if let Some(watch) = self.question_watch.take() {
                    self.close_analysis(watch.analysis.key);
                }
            }
            StreamNews::Mic => {
                self.start_watching();
                self.watch_evidence(now);
            }
            StreamNews::Other => self.watch_evidence(now),
        }
    }

    /// Once the first live mic sample is known: the judged cells start at the first
    /// whole cell from there, with echo labels and a decode from that cell.
    fn start_watching(&mut self) {
        let Some(watch) = self.question_watch.as_mut() else {
            return;
        };
        if watch.assembler.is_some() || watch.broken {
            return;
        }
        let Some(mic_start) = watch.analysis.mic_start else {
            return;
        };
        let start = Sample(mic_start.0.div_ceil(512) * 512);
        let epoch = watch.analysis.epoch();
        let plan = EvidencePlan {
            epoch,
            cells_from: watch.cells_from.cell().min(start.cell()),
            vad_from: start.cell(),
            analysis_from: start.cell(),
        };
        let Ok(mut assembler) = EvidenceAssembler::new(plan) else {
            watch.broken = true;
            return;
        };
        if let Some(pending) = watch.analysis.pending.take()
            && pending.replay(epoch, &mut assembler).is_err()
        {
            watch.broken = true;
        }
        watch.assembler = Some(assembler);
        watch.start = Some(start);
        let key = watch.analysis.key.clone();
        self.mic_job(Owner::Watch, MicJob::StartUtterance { key, from: start });
    }

    /// Hands evidence to the watch's assembler, if it has one.
    pub(crate) fn watch_assembler(
        &mut self,
        push: impl FnOnce(&mut EvidenceAssembler) -> Result<(), EvidenceError>,
    ) {
        if let Some(watch) = self.question_watch.as_mut()
            && let Some(assembler) = watch.assembler.as_mut()
            && let Err(error) = push(assembler)
        {
            tracing::debug!(device = %self.device, ?error, "early-answer evidence refused");
            watch.broken = true;
        }
    }

    /// Judges the cells released so far.
    pub(crate) fn watch_evidence(&mut self, now: Now) {
        let Some(watch) = self.question_watch.as_mut() else {
            return;
        };
        if watch.broken {
            // Evidence out of order can prove nothing: the question plays on.
            self.close_question_watch(now);
            return;
        }
        let Some(assembler) = watch.assembler.as_mut() else {
            return;
        };
        let mut released = Vec::new();
        assembler.release(&mut released);
        let onset = released.iter().find_map(|item| match item {
            Released::Cell(cell) => watch.watch.observe(cell),
            Released::Block(_) => None,
        });
        if let Some(onset) = onset {
            self.early_answer(onset, now);
        }
    }

    pub(crate) fn watch_failed(&mut self, now: Now) {
        self.close_question_watch(now);
    }

    /// The answer began at `onset` while the question still played: the question is cut
    /// off and the reply turn opens on the watch's subscription.
    fn early_answer(&mut self, onset: Sample, now: Now) {
        let Some(watch) = self.question_watch.take() else {
            return;
        };
        let asking = self
            .turn
            .as_mut()
            .filter(|t| t.uuid == watch.question && t.answering.follow_up.is_some());
        let Some(asking) = asking else {
            let _ = self
                .uplink
                .close(&watch.subscription, CloseReason::Closed, self.mono(now.at));
            self.close_analysis(watch.analysis.key);
            return;
        };
        tracing::info!(device = %self.device, turn = %asking.uuid, onset = onset.0, "an early answer cut the question off");
        asking.answering.early_answer = true;
        asking.facts.playback_reason = Some(echomuse_records::vocab::PlaybackReason::Cancelled);
        let expectation = asking.answering.follow_up.clone().expect("checked above");
        let (question, chain, origin) = (asking.uuid, asking.chain, asking.origin);
        // The asking turn keeps its own ending ("didn't catch that" stays an empty
        // transcript); its row records the continuation as pending.
        self.finish_turn(now);
        let earliest = watch.start.unwrap_or(onset);
        let start = (onset - PRE_ROLL).round_down(512).max(earliest);
        let reply = echomuse_turn::ReplySpec::at_early_answer(
            origin,
            onset,
            self.mono(now.at),
            &echomuse_turn::EndpointPolicy::default(),
        );
        let window = ReplyWindow {
            subscription: watch.subscription,
            analysis: watch.analysis,
            scope: watch.scope,
            question: Some(question),
            expectation,
            chain: chain.next(),
            origin,
            opened_at: now.at,
            cells_from: watch.cells_from,
        };
        self.begin_reply_turn(window, start, onset, reply, now);
    }
}
