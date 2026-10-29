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

use echomuse_core::types::{TurnId, WorkScope};
use echomuse_esphome::voice::{RunEvent as SatelliteRunEvent, RunId};
use echomuse_playback::Earcon;
use echomuse_records::vocab::{ContinuationEnd, FollowUp, TerminalReason};
use echomuse_turn::{Epoch, MonoInstant, ReplyOrigin, Sample, Samples};
use echomuse_uplink::{CloseReason, EndCause, Owner as SubOwner, SubscriptionId};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    analysis::{Analysis, Owner, StreamNews},
    io::{CallId, Effect, Now, SatelliteCommand},
    local::Expectation,
    machine::Voice,
    speech::{MicJob, ReferenceJob, StreamKey, SttCopy},
    turn::Call,
};

/// At most this many replies in one chain (§4.5, tuned).
const CHAIN_REPLIES: u32 = 5;
/// And no reply window this long after the chain's first turn (§4.5, tuned).
const CHAIN_WINDOW: Duration = Duration::from_secs(60);
/// A window whose live mic never arrives gives up after this long.
const MIC_WAIT: Duration = Duration::from_secs(10);

/// A run of turns answering each other's questions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chain {
    pub(crate) replies: u32,
    pub(crate) started: Instant,
}

impl Chain {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            replies: 0,
            started: now,
        }
    }

    pub(crate) fn allows(&self, now: Instant) -> bool {
        self.replies < CHAIN_REPLIES && now.saturating_duration_since(self.started) < CHAIN_WINDOW
    }

    pub(crate) fn next(self) -> Self {
        Self {
            replies: self.replies + 1,
            started: self.started,
        }
    }
}

pub(crate) struct ReplyWindow {
    pub(crate) subscription: SubscriptionId,
    pub(crate) analysis: Analysis,
    /// The reply turn's scope, made when the window opened.
    pub(crate) scope: WorkScope,
    /// The turn that asked; `None` for a conversation HA started.
    pub(crate) question: Option<Uuid>,
    pub(crate) expectation: Expectation,
    pub(crate) chain: Chain,
    pub(crate) origin: ReplyOrigin,
    pub(crate) opened_at: Instant,
    /// Where the window's loudness history starts.
    pub(crate) cells_from: Sample,
}

impl ReplyWindow {
    pub(crate) fn next_deadline(&self) -> Option<MonoInstant> {
        None
    }
}

impl Voice {
    /// Opens the reply window for `question`'s answer, which just finished playing.
    pub(crate) fn open_window(
        &mut self,
        question: Uuid,
        expectation: Expectation,
        chain: crate::reply::Chain,
        origin: ReplyOrigin,
        now: Now,
    ) {
        self.open_reply_window(Some(question), expectation, chain, origin, now);
    }

    fn open_reply_window(
        &mut self,
        question: Option<Uuid>,
        expectation: Expectation,
        chain: Chain,
        origin: ReplyOrigin,
        now: Now,
    ) {
        let resolve = |voice: &mut Voice, end| {
            if let Some(question) = question {
                voice.resolve_continuation(question, end);
            }
        };
        if !chain.allows(now.at) {
            resolve(self, ContinuationEnd::FollowUp(FollowUp::ChainLimit));
            return;
        }
        let (Some(capture), Some(epoch)) =
            (self.capture.clone(), self.uplink.timeline().capture_epoch())
        else {
            resolve(self, ContinuationEnd::FollowUp(FollowUp::NoMic));
            return;
        };
        // Ten seconds of loudness before the window, from the last capture position
        // heard; the device clips the request to what its ring still holds.
        let heard = self.mic_end(epoch).unwrap_or(Sample(0));
        let cells_from = (heard - Samples(160_000)).round_down(512);
        let owner = Uuid::new_v4();
        let opened = match self.uplink.open_reply(
            SubOwner::new(owner.to_string()).expect("a UUID is not empty"),
            cells_from,
            self.mono(now.at),
        ) {
            Ok(opened) => opened,
            Err(error) => {
                tracing::info!(device = %self.device, %error, "a reply window could not open");
                resolve(self, ContinuationEnd::FollowUp(FollowUp::NoMic));
                return;
            }
        };
        self.apply_grants(&opened.grants);
        let Some(turn_epoch) = Epoch::new(epoch.get()) else {
            return;
        };
        self.next_turn += 1;
        let scope = capture.for_turn(TurnId(self.next_turn));
        let key = StreamKey {
            session: self.session,
            subscription: opened.subscription_id.clone(),
            epoch: turn_epoch,
        };
        self.window = Some(ReplyWindow {
            subscription: opened.subscription_id,
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
        self.mic_job(
            Owner::Window,
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
            Owner::Window,
            ReferenceJob::Open {
                key,
                graph: self.wake.files.clone(),
            },
        );
        if self.settings.wakeword.wake_sound {
            self.earcon(Earcon::WakeChime, now);
        }
    }

    pub(crate) fn window_news(&mut self, news: StreamNews, now: Now) {
        match news {
            StreamNews::Mic => {
                let start = self.window.as_ref().and_then(|w| w.analysis.mic_start);
                if let Some(start) = start {
                    self.start_reply_turn(start, now);
                }
            }
            StreamNews::Ended(ended) => {
                let end = match ended.cause {
                    EndCause::Muted => ContinuationEnd::Ended(TerminalReason::Muted),
                    EndCause::SessionLost => ContinuationEnd::Ended(TerminalReason::SessionLost),
                    _ => ContinuationEnd::FollowUp(FollowUp::NoMic),
                };
                if let Some(window) = self.window.take() {
                    self.close_analysis(window.analysis.key.clone());
                    if let Some(question) = window.question {
                        self.resolve_continuation(question, end);
                    }
                }
            }
            StreamNews::Other => {}
        }
    }

    pub(crate) fn window_evidence(&mut self, _now: Now) {}

    pub(crate) fn window_failed(&mut self, now: Now) {
        self.close_window(ContinuationEnd::Ended(TerminalReason::Interrupted), now);
    }

    /// A window whose live mic never came gives up.
    pub(crate) fn window_timer(&mut self, now: Now) {
        let expired = self
            .window
            .as_ref()
            .is_some_and(|w| now.at.saturating_duration_since(w.opened_at) >= MIC_WAIT);
        if expired {
            self.close_window(ContinuationEnd::FollowUp(FollowUp::NoMic), now);
        }
    }

    pub(crate) fn window_deadline(&self) -> Option<Instant> {
        self.window.as_ref().map(|w| w.opened_at + MIC_WAIT)
    }

    pub(crate) fn close_window(&mut self, end: ContinuationEnd, now: Now) {
        if let Some(window) = self.window.take() {
            self.close_window_subscription(&window, now);
            if let Some(question) = window.question {
                self.resolve_continuation(question, end);
            }
        }
    }

    pub(crate) fn close_window_subscription(&mut self, window: &ReplyWindow, now: Now) {
        let _ = self
            .uplink
            .close(&window.subscription, CloseReason::Closed, self.mono(now.at));
        self.close_analysis(window.analysis.key.clone());
    }

    /// An announcement asked for a reply: HA's conversation continues on the satellite.
    pub(crate) fn open_ha_conversation(&mut self, now: Now) {
        if self.turn.is_some() || self.window.is_some() {
            return;
        }
        self.open_reply_window(
            None,
            Expectation::HaConversation,
            Chain::new(now.at),
            ReplyOrigin::HaConversation,
            now,
        );
    }

    /// A reply to an HA-started conversation committed: HA runs STT, intent and TTS on
    /// the STT copy through the satellite.
    pub(crate) fn ha_reply_run(&mut self, copy: SttCopy, now: Now) {
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
        let call = self.call_id();
        self.calls.insert(call, Call::RunStart { turn: uuid });
        self.link
            .effects
            .push(Effect::Satellite(SatelliteCommand::StartRun {
                call,
                conversation: None,
            }));
    }

    pub(crate) fn run_started(&mut self, call: CallId, result: Result<RunId, String>, now: Now) {
        let Some(Call::RunStart { turn }) = self.calls.remove(&call) else {
            self.counters.stale_results += 1;
            return;
        };
        let Some(current) = self.turn.as_mut().filter(|t| t.uuid == turn) else {
            if let Ok(run) = result {
                self.link
                    .effects
                    .push(Effect::Satellite(SatelliteCommand::AbortRun(run)));
            }
            return;
        };
        match result {
            Ok(run) => {
                current.ha_run = Some(run);
                if let Some(pcm) = current.ha_run_audio.take() {
                    self.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::RunAudio { run, pcm }));
                    self.link
                        .effects
                        .push(Effect::Satellite(SatelliteCommand::EndRunAudio(run)));
                }
            }
            Err(error) => {
                tracing::warn!(device = %self.device, error, "HA's satellite run did not start");
                current.answering.run_open = false;
                self.fail_turn(TerminalReason::HaError, now);
            }
        }
    }

    /// HA's events for the satellite run of a reply to its own conversation.
    pub(crate) fn satellite_run_event(&mut self, run: RunId, event: SatelliteRunEvent, now: Now) {
        let Some(turn) = self.turn.as_mut().filter(|t| t.ha_run == Some(run)) else {
            return;
        };
        match event {
            SatelliteRunEvent::Started { tts_url } => turn.ha_stream_url = tts_url,
            SatelliteRunEvent::SttEnded { text } => {
                turn.facts.stt_raw = Some(text.clone());
                turn.facts.stt_text = Some(text.clone());
                turn.facts.stt_done = Some(now.at);
                if let Some(body) = self.view.thinking(&text) {
                    self.link.send(crate::io::Outbound::TurnView(body), 0);
                }
            }
            SatelliteRunEvent::IntentProgress {
                tts_start_streaming: true,
            } => {
                if let Some(url) = turn.ha_stream_url.clone() {
                    self.speak_answer(url, now);
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
                if let Some(body) = self.view.final_answer(&speech) {
                    self.link.send(crate::io::Outbound::TurnView(body), 0);
                }
            }
            SatelliteRunEvent::TtsEnded { url: Some(url) } => {
                turn.facts.tts_url_at = Some(now.at);
                self.speak_answer(url, now);
            }
            SatelliteRunEvent::Error { code, message } => {
                tracing::warn!(device = %self.device, code, message, "HA's satellite run failed");
                turn.facts.terminal = Some(TerminalReason::HaError);
            }
            SatelliteRunEvent::Closed(_) => {
                turn.answering.run_open = false;
                if !turn.answering.playing {
                    self.finish_turn(now);
                }
            }
            _ => {}
        }
    }
}
