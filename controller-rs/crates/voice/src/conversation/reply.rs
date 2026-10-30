//! Reply windows (spec §4.5): after an answer that asked something (HA's
//! `continue_conversation`, the controller's own questions, "didn't catch that") the
//! device listens again without a wake word, once the question has finished playing.
//!
//! The window opens a reply subscription with live mic and ten seconds of loudness
//! history; its utterance starts at the first live mic sample (the window's trigger)
//! with the 320 ms guard and closes as no input after 7 s of no command speech. A chain
//! of replies stops after 5 replies or 60 s. A wake or a button press during the window
//! keeps the question. A reply to a conversation Home Assistant started through the
//! satellite (an announcement with `start_conversation`) is answered on the ESPHome
//! path: the committed span's STT copy is streamed into a run HA starts.

use std::time::Duration;

use echomuse_core::types::WorkScope;
use echomuse_esphome::voice::{RunEvent as SatelliteRunEvent, RunId};
use echomuse_playback::Earcon;
use echomuse_records::vocab::{ContinuationEnd, FollowUp, TerminalReason};
use echomuse_turn::{Epoch, ReplyOrigin, Sample, Samples};
use echomuse_uplink::{CloseReason, EndCause, Owner as SubOwner, SubscriptionId};
use tokio::time::Instant;
use uuid::Uuid;

use super::{Conversation, local::Expectation, turn::Call};
use crate::{
    analysis::{Analysis, StreamNews},
    io::{CallId, Effect, Now, SatelliteCommand},
    output::Output,
    session::{ConversationCx, SessionAudio, Shown},
    speech::{StreamKey, SttCopy},
};

/// At most this many replies in one chain (§4.5, tuned).
const CHAIN_REPLIES: u32 = 5;
/// And no reply window this long after the chain's first turn (§4.5, tuned).
const CHAIN_WINDOW: Duration = Duration::from_secs(60);
/// A window whose live mic never arrives gives up after this long.
const MIC_WAIT: Duration = Duration::from_secs(10);
/// The loudness history a reply subscription asks for before the last capture position
/// heard; the device clips the request to what its ring still holds.
const HISTORY: Samples = Samples(160_000);

/// A run of turns answering each other's questions.
#[derive(Clone, Copy, Debug)]
pub(super) struct Chain {
    replies: u32,
    started: Instant,
}

impl Chain {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            replies: 0,
            started: now,
        }
    }

    pub(super) fn allows(&self, now: Instant) -> bool {
        self.replies < CHAIN_REPLIES && now.saturating_duration_since(self.started) < CHAIN_WINDOW
    }

    pub(super) fn next(self) -> Self {
        Self {
            replies: self.replies + 1,
            started: self.started,
        }
    }
}

pub(super) struct ReplyWindow {
    pub(super) subscription: SubscriptionId,
    pub(super) analysis: Analysis,
    /// The reply turn's scope, made when the window opened.
    pub(super) scope: WorkScope,
    /// The turn that asked; `None` for a conversation HA started.
    pub(super) question: Option<Uuid>,
    pub(super) expectation: Expectation,
    pub(super) chain: Chain,
    pub(super) origin: ReplyOrigin,
    pub(super) opened_at: Instant,
    /// Where the window's loudness history starts.
    pub(super) cells_from: Sample,
}

/// The last capture position heard on `epoch`, and where a reply subscription's
/// loudness history starts: [`HISTORY`] before it.
pub(super) fn reply_history(audio: &SessionAudio, epoch: std::num::NonZeroU64) -> (Sample, Sample) {
    let heard = audio.mic_end(epoch).unwrap_or(Sample(0));
    (heard, (heard - HISTORY).round_down(512))
}

impl Conversation {
    /// Opens a reply window for `question`'s answer, which just finished playing; `None`
    /// for a conversation HA started.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_reply_window(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        question: Option<Uuid>,
        expectation: Expectation,
        chain: Chain,
        origin: ReplyOrigin,
        now: Now,
    ) {
        if !chain.allows(now.at) {
            if let Some(question) = question {
                self.resolve_continuation(
                    cx,
                    question,
                    ContinuationEnd::FollowUp(FollowUp::ChainLimit),
                );
            }
            return;
        }
        let (Some(capture), Some(epoch)) = (
            cx.facts.capture.clone(),
            cx.audio.timeline().capture_epoch(),
        ) else {
            if let Some(question) = question {
                self.resolve_continuation(cx, question, ContinuationEnd::FollowUp(FollowUp::NoMic));
            }
            return;
        };
        let (heard, cells_from) = reply_history(cx.audio, epoch);
        let owner = Uuid::new_v4();
        let subscription = match cx.audio.open_reply(
            SubOwner::new(owner.to_string()).expect("a UUID is not empty"),
            cells_from,
            cx.facts.mono(now.at),
        ) {
            Ok(subscription) => subscription,
            Err(error) => {
                tracing::info!(device = %cx.facts.device, %error, "a reply window could not open");
                if let Some(question) = question {
                    self.resolve_continuation(
                        cx,
                        question,
                        ContinuationEnd::FollowUp(FollowUp::NoMic),
                    );
                }
                return;
            }
        };
        let Some(turn_epoch) = Epoch::new(epoch.get()) else {
            return;
        };
        let scope = cx.ids.turn_scope(&capture);
        let key = StreamKey {
            session: cx.facts.session,
            subscription: subscription.clone(),
            epoch: turn_epoch,
        };
        let window = self.window.insert(ReplyWindow {
            subscription,
            analysis: {
                let mut analysis = Analysis::new(key.clone(), epoch);
                analysis.cells_from = Some(cells_from.cell());
                analysis
            },
            scope,
            question,
            expectation,
            chain: chain.next(),
            origin,
            opened_at: now.at,
            cells_from,
        });
        cx.audio.open_analysis(
            cx.link,
            &window.scope,
            key,
            echomuse_echo::EchoPlan {
                epoch: turn_epoch,
                cells_from: cells_from.cell(),
                vad_from: heard.cell(),
            },
            cx.facts.wake.files.clone(),
        );
        if cx.facts.settings.wakeword.wake_sound {
            out.earcon(&mut cx.output(), Earcon::WakeChime);
        }
    }

    pub(super) fn window_news(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        news: StreamNews,
        now: Now,
    ) {
        match news {
            StreamNews::Mic => {
                let start = self.window.as_ref().and_then(|w| w.analysis.mic_start);
                if let Some(start) = start {
                    self.start_reply_turn(cx, out, start, now);
                }
            }
            StreamNews::Ended(ended) => {
                let end = match ended.cause {
                    EndCause::Muted => ContinuationEnd::Ended(TerminalReason::Muted),
                    EndCause::SessionLost => ContinuationEnd::Ended(TerminalReason::SessionLost),
                    _ => ContinuationEnd::FollowUp(FollowUp::NoMic),
                };
                if let Some(window) = self.window.take() {
                    cx.audio
                        .close_analysis(cx.link, window.analysis.key.clone());
                    if let Some(question) = window.question {
                        self.resolve_continuation(cx, question, end);
                    }
                }
            }
            StreamNews::Other => {}
        }
    }

    /// A window whose live mic never came gives up.
    pub(super) fn window_timer(&mut self, cx: &mut ConversationCx<'_>, now: Now) {
        let expired = self
            .window
            .as_ref()
            .is_some_and(|w| now.at.saturating_duration_since(w.opened_at) >= MIC_WAIT);
        if expired {
            self.close_window(cx, ContinuationEnd::FollowUp(FollowUp::NoMic), now);
        }
    }

    pub(super) fn window_deadline(&self) -> Option<Instant> {
        self.window.as_ref().map(|w| w.opened_at + MIC_WAIT)
    }

    pub(super) fn close_window(
        &mut self,
        cx: &mut ConversationCx<'_>,
        end: ContinuationEnd,
        now: Now,
    ) {
        if let Some(window) = self.window.take() {
            Self::close_window_subscription(cx, &window, now);
            if let Some(question) = window.question {
                self.resolve_continuation(cx, question, end);
            }
        }
    }

    pub(super) fn close_window_subscription(
        cx: &mut ConversationCx<'_>,
        window: &ReplyWindow,
        now: Now,
    ) {
        cx.audio.close(
            &window.subscription,
            CloseReason::Closed,
            cx.facts.mono(now.at),
        );
        cx.audio
            .close_analysis(cx.link, window.analysis.key.clone());
    }

    /// An announcement asked for a reply: HA's conversation continues on the satellite.
    pub(super) fn open_ha_conversation(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        now: Now,
    ) {
        if self.turn.is_some() || self.window.is_some() {
            return;
        }
        self.open_reply_window(
            cx,
            out,
            None,
            Expectation::HaConversation,
            Chain::new(now.at),
            ReplyOrigin::HaConversation,
            now,
        );
    }

    /// A reply to an HA-started conversation committed: HA runs STT, intent and TTS on
    /// the STT copy through the satellite.
    pub(super) fn ha_reply_run(&mut self, cx: &mut ConversationCx<'_>, copy: SttCopy, now: Now) {
        let Some(turn) = self.turn.as_mut() else {
            return;
        };
        if turn.ha_run_audio.is_some() {
            return;
        }
        turn.ha_run_audio = Some(copy.pcm);
        turn.answering.run_open = true;
        turn.facts.intent_started = Some(now.at);
        turn.facts.outcome = Some(echomuse_records::vocab::TurnOutcome::Ha);
        let uuid = turn.uuid;
        let call = cx.ids.call();
        self.calls.insert(call, Call::RunStart { turn: uuid });
        cx.link
            .effects
            .push(Effect::Satellite(SatelliteCommand::StartRun {
                call,
                conversation: None,
            }));
    }

    pub(crate) fn run_started(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        call: CallId,
        result: Result<RunId, String>,
        now: Now,
    ) {
        let Some(Call::RunStart { turn }) = self.calls.remove(&call) else {
            cx.counters.stale_results += 1;
            return;
        };
        let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn) else {
            if let Ok(run) = result {
                cx.link
                    .effects
                    .push(Effect::Satellite(SatelliteCommand::AbortRun(run)));
            }
            return;
        };
        match result {
            Ok(run) => {
                current.ha_run = Some(run);
                if let Some(pcm) = current.ha_run_audio.take() {
                    cx.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::RunAudio { run, pcm }));
                    cx.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::EndRunAudio(run)));
                }
            }
            Err(error) => {
                tracing::warn!(device = %cx.facts.device, error, "HA's satellite run did not start");
                current.answering.run_open = false;
                self.fail_turn(cx, out, TerminalReason::HaError, now);
            }
        }
    }

    /// HA's events for the satellite run of a reply to its own conversation.
    pub(crate) fn satellite_run_event(
        &mut self,
        cx: &mut ConversationCx<'_>,
        out: &mut Output,
        run: RunId,
        event: SatelliteRunEvent,
        now: Now,
    ) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.ha_run == Some(run)) else {
            return;
        };
        match event {
            SatelliteRunEvent::Started { tts_url } => turn.ha_stream_url = tts_url,
            SatelliteRunEvent::SttEnded { text } => {
                turn.facts.stt_raw = Some(text.clone());
                turn.facts.stt_text = Some(text.clone());
                turn.facts.stt_done = Some(now.at);
                cx.show(Shown::Thinking(&text), now);
            }
            SatelliteRunEvent::IntentProgress {
                tts_start_streaming: true,
            } => {
                if let Some(url) = turn.ha_stream_url.clone() {
                    self.speak_answer(cx, out, url, now);
                }
            }
            SatelliteRunEvent::IntentEnded {
                conversation_id,
                continue_conversation,
                speech,
            } => {
                turn.facts.intent_ended = Some(now.at);
                turn.facts.response_text = Some(speech.clone());
                turn.facts.conversation = conversation_id
                    .clone()
                    .map(echomuse_core::services::ConversationId);
                if continue_conversation {
                    // The conversation continues on the websocket path (§3.3.1).
                    turn.answering.follow_up = Some(Expectation::Conversation {
                        conversation: conversation_id.map(echomuse_core::services::ConversationId),
                    });
                }
                cx.show(Shown::FinalAnswer(&speech), now);
            }
            SatelliteRunEvent::TtsEnded { url: Some(url) } => {
                turn.facts.tts_url_at = Some(now.at);
                self.speak_answer(cx, out, url, now);
            }
            SatelliteRunEvent::Error { code, message } => {
                tracing::warn!(device = %cx.facts.device, code, message, "HA's satellite run failed");
                turn.facts.terminal = Some(TerminalReason::HaError);
            }
            SatelliteRunEvent::Closed(_) => {
                turn.answering.run_open = false;
                if !turn.answering.playing {
                    self.finish_turn(cx, out, now);
                }
            }
            _ => {}
        }
    }
}
