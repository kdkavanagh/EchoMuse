//! The voice machine: one per attached device session.
//!
//! [`Voice`] is the session's coordinator. It keeps the session's facts and permission
//! (privacy, the capture scope, settings, capabilities, Home Assistant metadata), the
//! identities allocated across the session, the presentation (LED ring, button policy,
//! turn view, satellite voice state), the recording modes' subscription and the effect
//! sink. Three components own the rest, and only the coordinator calls them:
//!
//! - [`Wake`] — the pending wake candidate, its decider, analysis and verification, and
//!   the near misses being recorded;
//! - [`Conversation`] — the turn and its policy and requests, the reply window, the
//!   early-answer watch, lingering turn rows, and what "repeat" says again;
//! - [`Output`] — the render client, the media player, playbacks, previews, earcons and
//!   announcements.
//!
//! [`SessionAudio`] holds the stream registry and the uplink they share. For each call a
//! component borrows only what it uses ([`WakeCx`], [`ConversationCx`], [`OutputCx`]):
//! its own view of the session's facts, the mechanisms and the effect sink. An accepted
//! candidate leaves the wake handling as an owned transfer that the coordinator admits
//! through the wake arbiter and the conversation turns into a turn; output tells the
//! conversation what playback meant through notices the coordinator hands on.
//!
//! The device runtime feeds the machine [`Input`]s in the order they happened and carries
//! out the [`Effect`]s it queues; the machine itself never waits, never reads a clock and
//! never touches a socket, a model or a database, so every rule here can be exercised
//! with synthetic inputs (spec §5.6).

mod button;
pub(crate) mod diagnostic;

use std::{collections::HashMap, sync::Arc, time::Duration};

use echomuse_alerts::RingsSnapshot;
use echomuse_core::types::{DeviceId, RuntimeError, SessionId, WorkScope};
use echomuse_esphome::{entities::DeviceVolume, voice::SatelliteRequest};
use echomuse_grammar::Grammar;
use echomuse_playback::{AckOutcome, RenderReport};
use echomuse_presentation::{
    button::ButtonPolicy,
    phase::{ActorState, VoiceStateProjection},
    ring::{Cue, LedProjector},
    turn_view::TurnView,
};
use echomuse_protocol::{
    Capabilities, Capability, DeviceEnvelope, DeviceMessage, MessageType, Sha256, ema::Packet,
};
use echomuse_records::{WakeModelName, vocab::TerminalReason};
use echomuse_settings::DeviceSettings;
use echomuse_speech::BcresnetModelFiles;
use echomuse_turn::MonoInstant;
use echomuse_uplink::{Delivery, SessionTerms};
use tokio::time::Instant;

use crate::{
    arbiter::{ClaimDecision, WakeArbiter},
    conversation::Conversation,
    io::{Effect, Input, Now, Outbound, SatelliteCommand},
    link::EffectLink,
    output::Output,
    session::{
        Clock, ConversationCx, ConversationFacts, Ids, OutputCx, Presenter, SessionAudio, WakeCx,
        WakeFacts,
    },
    speech::{MicOutput, ReferenceOutput, StreamKey},
    wake::{Wake, WakeTransfer},
};

/// The device's selected wake model, as the registry describes it.
#[derive(Clone, Debug)]
pub struct WakeModelInfo {
    pub graph_sha256: Sha256,
    /// The registry's name for it (its wake phrase); `None` when the graph is not in the
    /// registry.
    pub name: Option<WakeModelName>,
    /// The spoken wake phrase, e.g. `ophelia`: removed from the text sent to HA.
    pub phrase: String,
    /// The verifier's core, e.g. `ophel`.
    pub verify_core: String,
    /// The graph and sidecar for reference scoring; `None` when they cannot be used.
    pub files: Option<BcresnetModelFiles>,
    /// The registry's idle and playback thresholds; `None` when the graph is not in the
    /// registry.
    pub thresholds: Option<(f64, f64)>,
}

/// Everything a machine starts from.
pub struct VoiceConfig {
    pub device: DeviceId,
    pub session: SessionId,
    pub capabilities: Capabilities,
    /// `session.ready` asked for the `afe` stream.
    pub afe_metadata: bool,
    /// The hello's privacy state; an unknown state counts as muted.
    pub muted: bool,
    pub volume: Option<DeviceVolume>,
    pub wake: WakeModelInfo,
    pub grammar: Grammar,
    pub arbiter: Arc<WakeArbiter>,
    pub settings: Arc<DeviceSettings>,
    /// The session's own scope: not tied to capture, so housekeeping jobs (resetting a
    /// worker after a mute) can still run while capture is revoked.
    pub session_scope: WorkScope,
    /// The capture scope at attach; `None` while muted.
    pub capture: Option<WorkScope>,
    /// The speech models are loaded.
    pub speech_ready: bool,
    pub rings: Arc<RingsSnapshot>,
    pub time_zone: Option<echomuse_alerts::schedule::HaTimeZone>,
    /// This satellite's device in Home Assistant's registry; `None` while unknown.
    pub ha_device: Option<echomuse_core::services::HaDeviceId>,
    /// The Kiosk Satellite token stored for the device; `None` while none is stored.
    pub kiosk_token: Option<String>,
}

/// Counters of what the machine refused or ignored, for the device's status.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VoiceCounters {
    /// Device messages whose body did not decode, by type.
    pub malformed: HashMap<&'static str, u64>,
    /// Results that arrived for work that had already ended (§5.7).
    pub stale_results: u64,
    /// Wake candidates refused, accepted and lost.
    pub candidates_refused: u64,
    pub candidates_accepted: u64,
    pub turns_finished: u64,
}

/// The machine's view for the device's snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceStatus {
    pub state: ActorState,
    pub speech_ready: bool,
    pub muted: bool,
    pub counters: VoiceCounters,
}

/// The session's facts and permission, which only the coordinator changes; each
/// component reads its own view of them.
struct Facts {
    device: DeviceId,
    session: SessionId,
    wake: WakeModelInfo,
    grammar: Grammar,
    arbiter: Arc<WakeArbiter>,
    settings: Arc<DeviceSettings>,
    /// The privacy state.
    muted: bool,
    /// The capture scope; `None` while capture is revoked.
    capture: Option<WorkScope>,
    speech_ready: bool,
    time_zone: Option<echomuse_alerts::schedule::HaTimeZone>,
    rings: Arc<RingsSnapshot>,
    ha_device: Option<echomuse_core::services::HaDeviceId>,
    clock: Clock,
}

impl Facts {
    fn mono(&self, at: Instant) -> MonoInstant {
        self.clock.mono(at)
    }

    fn wake_view(&self) -> WakeFacts<'_> {
        WakeFacts {
            device: &self.device,
            session: self.session,
            wake: &self.wake,
            settings: &self.settings,
            muted: self.muted,
            capture: &self.capture,
            speech_ready: self.speech_ready,
            clock: self.clock,
        }
    }

    fn conversation_view(&self) -> ConversationFacts<'_> {
        ConversationFacts {
            device: &self.device,
            session: self.session,
            wake: &self.wake,
            grammar: &self.grammar,
            arbiter: &self.arbiter,
            settings: &self.settings,
            capture: &self.capture,
            rings: &self.rings,
            time_zone: &self.time_zone,
            ha_device: &self.ha_device,
            clock: self.clock,
        }
    }
}

/// One device session's voice path. See the module documentation.
pub struct Voice {
    facts: Facts,
    link: EffectLink,
    audio: SessionAudio,
    ids: Ids,
    counters: VoiceCounters,
    presenter: Presenter,
    buttons: ButtonPolicy,
    voice_state: VoiceStateProjection,
    last_state: Option<ActorState>,
    wake: Wake,
    conversation: Conversation,
    output: Output,
    /// The recording modes' subscription.
    diagnostic: diagnostic::Diagnostic,
    /// The hello's capabilities.
    capabilities: Capabilities,
    kiosk_token: Option<String>,
    /// The `config` last sent, so an unchanged one is not sent again.
    config_sent: Option<crate::config::DeviceConfig>,
    /// A session recording runs: decision evidence is queued ([`crate::evidence`]).
    recording: bool,
    /// The machine time of the input being handled, for evidence queued with effects.
    handled_at: MonoInstant,
    /// The session ended: nothing more happens.
    lost: bool,
}

impl Voice {
    pub fn new(config: VoiceConfig, now: Now) -> Self {
        let VoiceConfig {
            device,
            session,
            capabilities,
            afe_metadata,
            muted,
            volume,
            wake,
            grammar,
            arbiter,
            settings,
            session_scope,
            capture,
            speech_ready,
            rings,
            time_zone,
            ha_device,
            kiosk_token,
        } = config;
        let terms = SessionTerms::new(&capabilities, afe_metadata);
        let mut voice = Self {
            presenter: Presenter {
                ring: LedProjector::new(&capabilities, &settings.ring),
                view: TurnView::new(&capabilities),
            },
            buttons: ButtonPolicy::new(
                capabilities.contains(Capability::ButtonHold),
                settings.advanced.clone(),
            ),
            voice_state: VoiceStateProjection::default(),
            output: Output::new(&capabilities, volume),
            audio: SessionAudio::new(terms, muted, session_scope),
            link: EffectLink::default(),
            facts: Facts {
                device,
                session,
                wake,
                grammar,
                arbiter,
                settings,
                muted,
                capture: if muted { None } else { capture },
                speech_ready,
                time_zone,
                rings,
                ha_device,
                clock: Clock::new(now.at),
            },
            ids: Ids::default(),
            counters: VoiceCounters::default(),
            last_state: None,
            wake: Wake::default(),
            conversation: Conversation::default(),
            diagnostic: diagnostic::Diagnostic::default(),
            capabilities,
            kiosk_token,
            config_sent: None,
            recording: false,
            handled_at: MonoInstant(0),
            lost: false,
        };
        voice.push_config();
        if let Some(command) = voice.presenter.ring.session_started(now.at) {
            voice.link.send(Outbound::Led(command), 0);
        }
        voice.present(now);
        voice
    }

    /// Handles one input, then everything it set in motion.
    pub fn handle(&mut self, input: Input, now: Now) {
        if self.lost {
            return;
        }
        self.handled_at = self.facts.mono(now.at);
        match input {
            Input::Control(envelope) => self.control(envelope, now),
            Input::Audio(packet) => self.packet(packet, now),
            Input::Privacy {
                muted,
                physical_seq,
            } => {
                if let Some(seq) = physical_seq {
                    self.buttons.accept_physical_seq(seq);
                }
                self.privacy(muted, now);
            }
            Input::Capture(scope) => {
                self.facts.capture = scope.filter(|scope| !scope.is_cancelled());
            }
            Input::SessionLost => self.session_lost(now),
            Input::Ack { id, outcome } => self.ack(id, outcome, now),
            Input::Mic { key, result } => self.mic_result(key, result, now),
            Input::Reference { key, result } => self.reference_result(key, result, now),
            Input::Verify { candidate, result } => {
                self.with_wake(now, |wake, cx| {
                    wake.verify_result(cx, &candidate, result, now)
                });
            }
            Input::Span { turn, result } => self.with_conversation(|conversation, cx, out| {
                conversation.span_result(cx, out, turn, result, now);
            }),
            Input::Ha { call, reply } => self.with_conversation(|conversation, cx, out| {
                if cx.recording {
                    let body = serde_json::json!({ "reply": format!("{reply:?}") });
                    cx.note(now, || crate::evidence::Evidence::HaReply {
                        exchange: call.0,
                        body,
                    });
                }
                conversation.ha_reply(cx, out, call, reply, now);
            }),
            Input::Decode { call, event } => self.with_output(|output, cx| {
                output.decode_event(cx, call, event, now);
            }),
            Input::Alert { call, reply } => self.with_conversation(|conversation, cx, out| {
                conversation.alert_reply(cx, out, call, reply, now);
            }),
            Input::Timer => self.timers(now),
            Input::KioskToken(token) => {
                self.kiosk_token = token;
                self.push_config();
            }
            Input::Timers(view) => {
                if let Some(command) = self.presenter.ring.set_timers(&view, now.at) {
                    self.link.send(Outbound::Led(command), 0);
                }
            }
            Input::Preview { id, sound } => {
                self.with_output(|output, cx| output.preview(cx, id, sound));
            }
            Input::StopPreview { id } => self.with_output(|output, cx| output.stop_preview(cx, id)),
            Input::Diagnostic(armed) => self.set_diagnostic(armed, now),
            Input::Corrected { call, correction } => {
                self.with_conversation(|conversation, cx, out| {
                    conversation.corrected(cx, out, call, correction, now);
                });
            }
            Input::PauseDecoding { key, through } => {
                self.with_conversation(|conversation, cx, _| {
                    conversation.pause_decoding(cx, &key, through, now);
                });
            }
            Input::ServerTranscript {
                call,
                through,
                result,
            } => self.with_conversation(|conversation, cx, out| {
                conversation.server_transcript(cx, out, call, through, result, now);
            }),
            Input::Persisted { reply, .. } => self.persisted(reply),
            Input::Rings(rings) => self.rings_changed(rings),
            Input::Satellite(request) => self.satellite(request, now),
            Input::RunStarted { call, result } => {
                self.with_conversation(|conversation, cx, out| {
                    conversation.run_started(cx, out, call, result, now);
                });
            }
            Input::Settings(settings) => self.settings_changed(settings, now),
            Input::Speech(ready) => self.facts.speech_ready = ready,
            Input::TimeZone(zone) => self.facts.time_zone = zone,
            Input::Volume(volume) => self.output.device_volume(volume),
            Input::HaDevice(device) => self.facts.ha_device = device,
        }
        self.settle(now);
    }

    /// Takes the effects queued so far, in order.
    pub fn drain_effects(&mut self) -> Vec<Effect> {
        let effects = std::mem::take(&mut self.link.effects);
        if !self.recording {
            return effects;
        }
        // While a recording runs, each request to Home Assistant is noted just before it
        // goes, at the time of the input that asked for it.
        let mut noted = Vec::with_capacity(effects.len());
        for effect in effects {
            if let Effect::Ha { call, request, .. } = &effect {
                let (operation, body) = crate::evidence::ha_request(request);
                noted.push(Effect::Evidence {
                    at: self.handled_at,
                    evidence: Box::new(crate::evidence::Evidence::HaRequest {
                        exchange: call.0,
                        operation,
                        body,
                    }),
                });
            }
            noted.push(effect);
        }
        noted
    }

    /// When [`Input::Timer`] is next due; `None` while nothing waits on time.
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.lost {
            return None;
        }
        let clock = self.facts.clock;
        [
            self.audio.next_deadline().map(|at| clock.instant(at)),
            self.output.next_deadline(),
            self.audio.released_deadline(),
            self.presenter.ring.next_deadline(),
            self.buttons.next_deadline(),
            self.presenter.view.next_deadline(),
            self.wake.next_deadline().map(|at| clock.instant(at)),
            self.conversation
                .next_deadline(&self.facts.conversation_view()),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn status(&self) -> VoiceStatus {
        VoiceStatus {
            state: self.actor_state(),
            speech_ready: self.facts.speech_ready,
            muted: self.facts.muted,
            counters: self.counters.clone(),
        }
    }

    /// The session ended.
    pub fn is_lost(&self) -> bool {
        self.lost
    }

    /// Says whether a session recording runs: while it does, decision evidence is
    /// queued as [`Effect::Evidence`] (see the `evidence` module).
    pub fn set_recording(&mut self, recording: bool) {
        self.recording = recording;
    }

    /// Where the machine's monotonic clock starts: [`Effect::Evidence`] times count
    /// from it.
    pub fn clock_origin(&self) -> Instant {
        self.facts.clock.origin()
    }

    // ---- the components -------------------------------------------------------------

    /// The wake handling and what it borrows.
    fn wake_parts(&mut self) -> (WakeCx<'_>, &mut Wake) {
        (
            WakeCx::new(
                self.facts.wake_view(),
                &mut self.link,
                &mut self.audio,
                &mut self.ids,
                &mut self.counters,
                self.recording,
                &mut self.presenter,
            ),
            &mut self.wake,
        )
    }

    /// The conversation, what it borrows, and the output it asks to play.
    fn conversation_parts(&mut self) -> (ConversationCx<'_>, &mut Conversation, &mut Output) {
        (
            ConversationCx::new(
                self.facts.conversation_view(),
                &mut self.link,
                &mut self.audio,
                &mut self.ids,
                &mut self.counters,
                self.recording,
                &mut self.presenter,
            ),
            &mut self.conversation,
            &mut self.output,
        )
    }

    /// The output and what it borrows.
    fn output_parts(&mut self) -> (OutputCx<'_>, &mut Output) {
        (
            OutputCx {
                link: &mut self.link,
                ids: &mut self.ids,
                released: self.audio.released_focus(),
                stale: &mut self.counters.stale_results,
                device: &self.facts.device,
                settings: &self.facts.settings,
            },
            &mut self.output,
        )
    }

    fn with_conversation(
        &mut self,
        act: impl FnOnce(&mut Conversation, &mut ConversationCx<'_>, &mut Output),
    ) {
        let (mut cx, conversation, output) = self.conversation_parts();
        act(conversation, &mut cx, output);
    }

    fn with_output(&mut self, act: impl FnOnce(&mut Output, &mut OutputCx<'_>)) {
        let (mut cx, output) = self.output_parts();
        act(output, &mut cx);
    }

    /// Runs one step of the wake handling and admits the candidate it accepted, if any,
    /// before anything else happens.
    fn with_wake(
        &mut self,
        now: Now,
        step: impl FnOnce(&mut Wake, &mut WakeCx<'_>) -> Option<WakeTransfer>,
    ) {
        let (mut cx, wake) = self.wake_parts();
        if let Some(transfer) = step(wake, &mut cx) {
            self.admit(transfer, now);
        }
    }

    /// Claims the room for an accepted wake (§4.2) and starts its turn, or stands down.
    fn admit(&mut self, transfer: WakeTransfer, now: Now) {
        let summary = self.output.summary();
        // A wake that interrupts this device's own audible reply skips arbitration: the
        // room already answered here.
        let (grant, restricted) = if summary.answer_playing {
            (None, false)
        } else {
            let window = Duration::from_millis(u64::from(
                self.facts.settings.wakeword.wake_arbitration_ms.get(),
            ));
            match self
                .facts
                .arbiter
                .claim(&transfer.scope, window, now.at.into_std())
            {
                Ok(ClaimDecision::Won(grant)) => (Some(grant), false),
                Ok(ClaimDecision::Lost { winner }) => {
                    tracing::info!(device = %self.facts.device, %winner, "another device took the wake");
                    // A media-context loser may still pause or stop its own media
                    // (§4.2): the turn runs, but only those commands act.
                    if !summary.media_playing {
                        let (mut cx, wake) = self.wake_parts();
                        wake.outclaimed(&mut cx, transfer, now);
                        return;
                    }
                    (None, true)
                }
                Err(error) => {
                    // The capture scope was cancelled in between: capture is revoked.
                    tracing::info!(device = %self.facts.device, %error, "an accepted wake could not claim");
                    let (mut cx, wake) = self.wake_parts();
                    wake.withdraw(&mut cx, transfer, now);
                    return;
                }
            }
        };
        let (mut cx, conversation, output) = self.conversation_parts();
        conversation.start_wake_turn(&mut cx, output, transfer, grant, restricted, now);
    }

    // ---- ingress --------------------------------------------------------------------

    fn control(&mut self, envelope: DeviceEnvelope, now: Now) {
        let generation = envelope.header.generation;
        let message_id = envelope.header.message_id;
        let mono = self.facts.mono(now.at);
        match envelope.message {
            DeviceMessage::StreamOpen(open) => {
                if let Err(error) = self.audio.stream_open(&open) {
                    self.violation(error);
                }
            }
            DeviceMessage::StreamEnd(end) => {
                if let Err(error) = self.audio.stream_end(&end, mono) {
                    self.violation(error);
                }
            }
            DeviceMessage::UplinkEnded(report) => self.audio.device_ended(&report, mono),
            DeviceMessage::Other { message_type, body } => match message_type {
                MessageType::WakeCandidate => {
                    self.with_wake(now, |wake, cx| wake.arrive(cx, message_id, &body, now));
                }
                MessageType::WakeCandidateEnd => match crate::wire::decode_candidate_end(&body) {
                    Ok(end) => {
                        if !self.wake.candidate_end(&end) {
                            self.conversation.candidate_end(&end);
                        }
                    }
                    Err(_) => self.malformed("wake.candidate_end"),
                },
                MessageType::WakeNearMiss => match crate::wire::decode_near_miss(&body) {
                    Ok(near_miss) => {
                        let (mut cx, wake) = self.wake_parts();
                        wake.near_miss(&mut cx, near_miss, now);
                    }
                    Err(_) => self.malformed("wake.near_miss"),
                },
                MessageType::RenderProgress | MessageType::RenderFinished => {
                    match RenderReport::parse(message_type, &body) {
                        Some(Ok(report)) => self.with_output(|output, cx| {
                            output.render_report(cx, generation, report, now);
                        }),
                        _ => self.malformed(message_type.as_str()),
                    }
                }
                MessageType::ButtonAction => self.button_action(&body, now),
                _ => {}
            },
            _ => {}
        }
    }

    fn packet(&mut self, packet: Packet, now: Now) {
        if let Err(error) = self.audio.packet(&packet, self.facts.mono(now.at)) {
            self.violation(error);
        }
    }

    fn violation(&mut self, error: echomuse_protocol::ProtocolError) {
        tracing::warn!(device = %self.facts.device, %error, "the device broke the stream rules");
        self.link.effects.push(Effect::Violation(error));
        // The link closes the session; nothing it sends afterwards is trusted.
        self.lost = true;
        self.link.closed = true;
    }

    fn malformed(&mut self, what: &'static str) {
        tracing::warn!(device = %self.facts.device, what, "a device message did not decode");
        *self.counters.malformed.entry(what).or_default() += 1;
    }

    fn cue(&mut self, cue: Cue, now: Now) {
        if let Some(command) = self.presenter.ring.cue(cue, now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
    }

    // ---- urgent ---------------------------------------------------------------------

    fn privacy(&mut self, muted: bool, now: Now) {
        if muted == self.facts.muted {
            return;
        }
        self.facts.muted = muted;
        if !muted {
            self.audio.unmute();
            return;
        }
        // Capture is revoked before anything else: queued audio and jobs are discarded by
        // scope, the subscriptions end, the workers forget the audio. Unmuting brings a
        // fresh scope; this one never comes back.
        if let Some(scope) = self.facts.capture.take() {
            scope.cancel();
        }
        // The turns' charts and recordings keep what was heard before the mute; the
        // response's part is erased with the rest.
        self.with_conversation(|conversation, cx, _| {
            conversation.chart_before_erasure(cx);
            conversation.erase_lingering(cx, now);
        });
        self.audio.mute();
        self.audio.reset_workers(&mut self.link);
        let (mut cx, wake) = self.wake_parts();
        wake.revoke(&mut cx);
        // A mute revokes input only: media and announcements play on.
        self.with_conversation(|conversation, cx, out| {
            conversation.revoke(cx, out, TerminalReason::Muted, now);
        });
        self.wake.clear_near_misses();
    }

    fn session_lost(&mut self, now: Now) {
        if let Some(scope) = self.facts.capture.take() {
            scope.cancel();
        }
        // The turns' charts and recordings, before the session's audio goes.
        self.with_conversation(|conversation, cx, _| {
            conversation.chart_before_erasure(cx);
            conversation.erase_lingering(cx, now);
        });
        self.audio.session_lost();
        let (mut cx, wake) = self.wake_parts();
        wake.revoke(&mut cx);
        self.with_conversation(|conversation, cx, out| {
            conversation.revoke(cx, out, TerminalReason::SessionLost, now);
        });
        self.with_output(|output, cx| output.session_lost(cx, now));
        self.presenter.ring.session_lost();
        self.presenter.view.session_lost();
        self.wake.clear_near_misses();
        self.audio.clear();
        // Persistence still runs: the runtime carries out the effects queued so far.
        self.link.closed = true;
        self.lost = true;
    }

    // ---- acknowledgements -----------------------------------------------------------

    fn ack(&mut self, id: echomuse_protocol::MessageId, outcome: AckOutcome, now: Now) {
        let rendered = {
            let (mut cx, output) = self.output_parts();
            output.render_ack(&mut cx, &id, outcome.clone(), now)
        };
        if rendered
            || self.conversation.focus_ack(&id, &outcome)
            || self.output.focus_ack(&id, &outcome)
        {
            return;
        }
        self.audio.ack(&id, outcome, self.facts.mono(now.at));
    }

    // ---- worker results -------------------------------------------------------------

    /// The microphone worker's answer, routed by the stream's current owner: a result
    /// queued for a candidate's stream reaches the turn it became.
    fn mic_result(
        &mut self,
        key: Option<StreamKey>,
        result: Result<MicOutput, RuntimeError>,
        now: Now,
    ) {
        let Some(key) = key else {
            return;
        };
        let wake_owns = self.wake.owns_key(&key);
        if !wake_owns && !self.conversation.owns_key(&key) {
            self.counters.stale_results += 1;
            return;
        }
        let mut output = match result {
            Ok(output) => output,
            Err(error) => {
                self.worker_failed(&error);
                if wake_owns {
                    self.with_wake(now, |wake, cx| wake.evidence_failed(cx, now));
                } else {
                    self.with_conversation(|conversation, cx, out| {
                        conversation.mic_failed(cx, out, &key, now);
                    });
                }
                return;
            }
        };
        if let Some(failure) = &output.failure {
            tracing::warn!(device = %self.facts.device, failure, "microphone analysis failed");
        }
        if wake_owns && let Some(alignment) = output.alignment.take() {
            let (mut cx, wake) = self.wake_parts();
            wake.alignment(&mut cx, alignment);
        }
        for (candidate, comparison) in std::mem::take(&mut output.comparisons) {
            self.with_wake(now, |wake, cx| {
                wake.comparison(cx, &candidate, comparison, now)
            });
        }
        // The evidence goes to whoever owns the stream now: a comparison above may have
        // passed the candidate's stream to its turn.
        if self.wake.owns_key(&key) {
            let failed = output.failure.is_some();
            self.wake.worker_evidence(output);
            if failed {
                self.with_wake(now, |wake, cx| wake.evidence_failed(cx, now));
            }
        } else {
            self.with_conversation(|conversation, cx, out| {
                conversation.mic_output(cx, out, &key, output, now);
            });
        }
    }

    fn reference_result(
        &mut self,
        key: Option<StreamKey>,
        result: Result<ReferenceOutput, RuntimeError>,
        now: Now,
    ) {
        let Some(key) = key else {
            return;
        };
        if !self.wake.owns_key(&key) && !self.conversation.owns_key(&key) {
            self.counters.stale_results += 1;
            return;
        }
        match result {
            Ok(output) => self.with_wake(now, |wake, cx| wake.reference_output(cx, output, now)),
            Err(error) => {
                self.worker_failed(&error);
                self.with_wake(now, |wake, cx| wake.reference_failed(cx, now));
            }
        }
    }

    fn worker_failed(&self, error: &RuntimeError) {
        tracing::warn!(device = %self.facts.device, %error, "a speech job failed");
    }

    fn persisted(&mut self, reply: crate::io::PersistReply) {
        if let crate::io::PersistReply::Turn(Err(error))
        | crate::io::PersistReply::NearMiss(Err(error))
        | crate::io::PersistReply::Continuation(Err(error)) = &reply
        {
            tracing::warn!(device = %self.facts.device, error, "a record was not written");
        }
    }

    /// A request Home Assistant sent the satellite.
    fn satellite(&mut self, request: SatelliteRequest, now: Now) {
        match request {
            SatelliteRequest::Media(request) => self.output.satellite_media(request),
            SatelliteRequest::Announce(announcement) => self.output.announce(announcement),
            SatelliteRequest::Run { run, event } => {
                self.with_conversation(|conversation, cx, out| {
                    conversation.satellite_run_event(cx, out, run, event, now);
                });
            }
            // Timers and the stop button belong to the alert service; the runtime
            // forwards them there.
            SatelliteRequest::Timer(_) | SatelliteRequest::StopAlert => {}
        }
        self.with_output(|output, cx| output.apply_media_effects(cx, now));
    }

    // ---- after every input ----------------------------------------------------------

    /// Sends what the uplink queued, routes its deliveries and the render client's
    /// events (handing what playback meant to the conversation at once), and brings the
    /// presentation up to date, until nothing more changes.
    fn settle(&mut self, now: Now) {
        // An armed recording mode reopens its subscription as soon as capture allows.
        self.reconcile_diagnostic(now);
        for _ in 0..64 {
            let mut changed = self.audio.send_commands(&mut self.link);
            let mut deliveries = Vec::new();
            self.audio.drain(&mut deliveries);
            for delivery in deliveries {
                changed = true;
                self.delivery(delivery, now);
            }
            while let Some(event) = self.output.next_render_event() {
                changed = true;
                let (notices, refill) = {
                    let (mut cx, output) = self.output_parts();
                    output.render_event(&mut cx, event, now)
                };
                self.with_conversation(|conversation, cx, out| {
                    conversation.output_notices(cx, out, notices, now);
                });
                self.with_output(|output, cx| output.refill(cx, refill));
            }
            if !changed {
                break;
            }
        }
        self.present(now);
    }

    fn timers(&mut self, now: Now) {
        self.audio.poll(self.facts.mono(now.at));
        self.with_output(|output, cx| output.poll_render(cx, now));
        if let Some(command) = self.presenter.ring.on_timer(now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
        if let Some(output) = self.buttons.on_timer(now.at) {
            self.button_output(output, now);
        }
        if let Some(body) = self.presenter.view.on_timer(now.at) {
            self.link.send(Outbound::TurnView(body), 0);
        }
        self.with_wake(now, |wake, cx| wake.timer(cx, now));
        self.with_conversation(|conversation, cx, out| conversation.timers(cx, out, now));
        self.with_output(|output, cx| output.poll_focus(cx, now));
        self.audio.poll_released(&mut self.link, now.at);
    }

    fn delivery(&mut self, delivery: Delivery, now: Now) {
        let subscription = delivery.subscription().clone();
        if self.conversation.is_lingering(&subscription) {
            self.with_conversation(|conversation, cx, _| {
                conversation.lingering_delivery(cx, delivery, now);
            });
            return;
        }
        if self.is_diagnostic(&subscription) {
            self.diagnostic_delivery(delivery, now);
            return;
        }
        if self.wake.owns_near_miss(&subscription) {
            let (mut cx, wake) = self.wake_parts();
            wake.near_miss_delivery(&mut cx, delivery);
            return;
        }
        if self.conversation.owns_response(&subscription) {
            self.conversation.response_delivery(delivery);
            return;
        }
        if self.wake.owns_subscription(&subscription) {
            self.with_wake(now, |wake, cx| wake.delivery(cx, delivery, now));
            return;
        }
        // An analysed subscription's audio; anything else (a closed subscription's late
        // audio) is dropped there.
        self.with_conversation(|conversation, cx, out| {
            conversation.analysed_delivery(cx, out, delivery, now);
        });
    }

    // ---- presentation ---------------------------------------------------------------

    /// The displayed state: a projection of what the components are doing at once.
    fn actor_state(&self) -> ActorState {
        if let Some(state) = self.conversation.turn_state() {
            return state;
        }
        if self.output.speaking() {
            return ActorState::Speaking;
        }
        if self.conversation.window_open() || self.wake.pending() {
            return ActorState::Armed;
        }
        ActorState::Idle
    }

    /// Brings the ring and the satellite's voice state up to date with the actor state.
    fn present(&mut self, now: Now) {
        let state = self.actor_state();
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            if let Some(command) = self.presenter.ring.set_phase(state.phase(), now.at) {
                self.link.send(Outbound::Led(command), 0);
            }
        }
        if let Some(change) = self.voice_state.update(Some(state)) {
            self.link
                .effects
                .push(Effect::Satellite(SatelliteCommand::VoiceState(change)));
        }
    }

    fn settings_changed(&mut self, settings: Arc<DeviceSettings>, now: Now) {
        if let Some(command) = self.presenter.ring.set_settings(&settings.ring, now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
        self.buttons.set_settings(settings.advanced.clone());
        self.facts.settings = settings;
        self.push_config();
    }

    /// Sends the device's `config` when it differs from what was last sent (always at
    /// the session's start: the firmware keeps none of it across a restart).
    fn push_config(&mut self) {
        let config = crate::config::DeviceConfig::new(
            &self.capabilities,
            &self.facts.settings,
            self.kiosk_token.as_deref(),
        );
        if self.config_sent.as_ref() == Some(&config) {
            return;
        }
        self.link.send(Outbound::Config(config.clone()), 0);
        self.config_sent = Some(config);
    }

    fn rings_changed(&mut self, rings: Arc<RingsSnapshot>) {
        let ringing = rings
            .devices
            .get(&self.facts.device.0)
            .map(|device| device.active.is_some());
        self.link
            .effects
            .push(Effect::Satellite(SatelliteCommand::AlertRinging(ringing)));
        self.facts.rings = rings;
    }
}
