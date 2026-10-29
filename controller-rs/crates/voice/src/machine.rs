//! The voice machine: one per attached device session.
//!
//! It owns everything live about the session's voice path: the stream registry and the
//! audio subscriptions, the pending wake candidate, the turn, the reply window, the
//! render client and focus leases, the media player, the LED ring, the button policy and
//! the turn view. The device runtime feeds it [`Input`]s in the order they happened and
//! carries out the [`Effect`]s it queues; the machine itself never waits, never reads a
//! clock and never touches a socket, a model or a database, so every rule here can be
//! exercised with synthetic inputs (spec §5.6).
//!
//! The parts of the work live in sibling modules as further `impl Voice` blocks:
//! `candidate` (wake candidates, arbitration), `turn` (endpointing, STT, routing,
//! finishing a turn), `local` (commands handled without Home Assistant), `output`
//! (spoken answers, lines, earcons, announcements, media), `reply` (reply windows),
//! `record` (turn rows).

use std::{collections::HashMap, sync::Arc, time::Duration};

use echomuse_alerts::RingsSnapshot;
use echomuse_core::types::{DeviceId, RuntimeError, SessionId, WorkScope};
use echomuse_esphome::entities::DeviceVolume;
use echomuse_grammar::Grammar;
use echomuse_playback::{
    AckOutcome, FocusEvent, FocusLease, MediaPlayer, PlaybackRef, RenderClient, RenderEvent,
    RenderReport,
};
use echomuse_presentation::{
    button::ButtonPolicy,
    phase::{ActorState, VoiceStateProjection},
    ring::LedProjector,
    turn_view::TurnView,
};
use echomuse_protocol::{
    Capabilities, Capability, DeviceEnvelope, DeviceMessage, MessageType, Sha256, StreamId,
    ema::Packet, streams::StreamRegistry,
};
use echomuse_records::WakeModelName;
use echomuse_settings::DeviceSettings;
use echomuse_speech::BcresnetModelFiles;
use echomuse_turn::{MonoInstant, Sample};
use echomuse_uplink::{
    Delivery, Grant, SessionTerms, SubscriptionId, Uplink, UplinkConfig, wire::Outgoing,
};
use tokio::time::Instant;

use crate::{
    arbiter::WakeArbiter,
    candidate::Candidate,
    io::{Effect, Input, Now, Outbound, SatelliteCommand},
    link::EffectLink,
    output::Playouts,
    reply::ReplyWindow,
    turn::Turn,
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

/// One device session's voice path. See the module documentation.
pub struct Voice {
    pub(crate) device: DeviceId,
    pub(crate) session: SessionId,
    pub(crate) wake: WakeModelInfo,
    pub(crate) grammar: Grammar,
    pub(crate) arbiter: Arc<WakeArbiter>,
    pub(crate) settings: Arc<DeviceSettings>,
    pub(crate) session_scope: WorkScope,
    /// Origin of the turn crate's monotonic clock for this machine.
    origin: Instant,
    pub(crate) link: EffectLink,
    pub(crate) registry: StreamRegistry,
    pub(crate) uplink: Uplink,
    pub(crate) render: RenderClient,
    pub(crate) ring: LedProjector,
    pub(crate) buttons: ButtonPolicy,
    pub(crate) view: TurnView,
    voice_state: VoiceStateProjection,
    pub(crate) media: MediaPlayer,
    pub(crate) muted: bool,
    pub(crate) capture: Option<WorkScope>,
    pub(crate) speech_ready: bool,
    pub(crate) time_zone: Option<echomuse_alerts::schedule::HaTimeZone>,
    pub(crate) rings: Arc<RingsSnapshot>,
    next_call: u64,
    pub(crate) next_turn: u64,
    pub(crate) candidate: Option<Candidate>,
    pub(crate) turn: Option<Turn>,
    pub(crate) window: Option<ReplyWindow>,
    pub(crate) playouts: Playouts,
    /// The last answer this device spoke, for "repeat" (§4.4).
    pub(crate) last_answer: Option<crate::output::LastAnswer>,
    /// Near misses whose recording subscription is open, by subscription.
    pub(crate) near_misses: HashMap<SubscriptionId, crate::candidate::NearMissRecording>,
    /// Requests whose answers have not arrived, by what they are for.
    pub(crate) calls: HashMap<crate::io::CallId, crate::turn::Call>,
    /// The session ended: nothing more happens.
    lost: bool,
    pub(crate) counters: VoiceCounters,
    last_state: Option<ActorState>,
    /// Uplink commands awaiting the device's acknowledgement.
    pub(crate) uplink_requests:
        HashMap<echomuse_protocol::MessageId, (SubscriptionId, echomuse_uplink::Generation)>,
    /// A reply window's question taken over by a new turn, until that turn takes it.
    pub(crate) pending_reply_context: Option<(
        crate::local::Expectation,
        Option<uuid::Uuid>,
        crate::reply::Chain,
    )>,
    pub(crate) ha_device: Option<echomuse_core::services::HaDeviceId>,
    /// The hello's capabilities.
    pub(crate) capabilities: Capabilities,
    pub(crate) kiosk_token: Option<String>,
    /// The `config` last sent, so an unchanged one is not sent again.
    config_sent: Option<crate::config::DeviceConfig>,
    /// The alert sound preview playing, with the request that started it.
    pub(crate) preview: Option<crate::output::Preview>,
    /// The early-answer watch while the controller's own question plays.
    pub(crate) question_watch: Option<crate::early::QuestionWatch>,
    /// Ended turns whose response subscriptions are still open, with their rows.
    pub(crate) lingering: Vec<crate::linger::Lingering>,
    /// A session recording runs: decision evidence is queued ([`crate::evidence`]).
    pub(crate) recording: bool,
    /// The recorder's identity of the last turn policy opened.
    pub(crate) evidence_turns: u64,
    /// The machine time of the input being handled, for evidence queued with effects.
    pub(crate) handled_at: MonoInstant,
    /// The recording modes' subscription.
    pub(crate) diagnostic: crate::diagnostic::Diagnostic,
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
            ring: LedProjector::new(&capabilities, &settings.ring),
            buttons: ButtonPolicy::new(
                capabilities.contains(Capability::ButtonHold),
                settings.advanced.clone(),
            ),
            view: TurnView::new(&capabilities),
            voice_state: VoiceStateProjection::default(),
            media: MediaPlayer::new(&capabilities, volume),
            render: RenderClient::new(&capabilities),
            registry: StreamRegistry::new(terms.afe_metadata),
            uplink: Uplink::new(UplinkConfig::default(), terms, muted),
            link: EffectLink::default(),
            device,
            session,
            wake,
            grammar,
            arbiter,
            settings,
            session_scope,
            origin: now.at,
            muted,
            capture: if muted { None } else { capture },
            speech_ready,
            time_zone,
            rings,
            next_call: 0,
            next_turn: 0,
            candidate: None,
            turn: None,
            window: None,
            playouts: Playouts::default(),
            last_answer: None,
            near_misses: HashMap::new(),
            calls: HashMap::new(),
            ha_device,
            capabilities,
            kiosk_token,
            config_sent: None,
            preview: None,
            question_watch: None,
            lingering: Vec::new(),
            recording: false,
            evidence_turns: 0,
            handled_at: MonoInstant(0),
            diagnostic: crate::diagnostic::Diagnostic::default(),
            counters: VoiceCounters::default(),
            last_state: None,
            uplink_requests: HashMap::new(),
            pending_reply_context: None,
            lost: false,
        };
        voice.push_config();
        if let Some(command) = voice.ring.session_started(now.at) {
            voice.link.send(Outbound::Led(command), 0);
        }
        voice.present(now);
        voice
    }

    /// Handles one input, then everything it set in motion.
    pub fn handle(&mut self, input: Input, now: Now) {
        if self.lost {
            if let Input::Persisted { call, .. } = input {
                self.calls.remove(&call);
            }
            return;
        }
        self.handled_at = self.mono(now.at);
        match input {
            Input::Control(envelope) => self.control(envelope, now),
            Input::Audio(packet) => self.audio(packet, now),
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
                self.capture = scope.filter(|scope| !scope.is_cancelled());
            }
            Input::SessionLost => self.session_lost(now),
            Input::Ack { id, outcome } => self.ack(id, outcome, now),
            Input::Mic { key, result } => self.mic_result(key, result, now),
            Input::Reference { key, result } => self.reference_result(key, result, now),
            Input::Verify { candidate, result } => self.verify_result(candidate, result, now),
            Input::Span { turn, result } => self.span_result(turn, result, now),
            Input::Ha { call, reply } => {
                if self.recording {
                    let body = serde_json::json!({ "reply": format!("{reply:?}") });
                    self.note(now, || crate::evidence::Evidence::HaReply {
                        exchange: call.0,
                        body,
                    });
                }
                self.ha_reply(call, reply, now);
            }
            Input::Decode { call, event } => self.decode_event(call, event, now),
            Input::Alert { call, reply } => self.alert_reply(call, reply, now),
            Input::Timer => self.timers(now),
            Input::KioskToken(token) => {
                self.kiosk_token = token;
                self.push_config();
            }
            Input::Timers(view) => {
                if let Some(command) = self.ring.set_timers(&view, now.at) {
                    self.link.send(Outbound::Led(command), 0);
                }
            }
            Input::Preview { id, sound } => self.preview(id, sound, now),
            Input::StopPreview { id } => self.stop_preview(id, now),
            Input::Diagnostic(armed) => self.set_diagnostic(armed, now),
            Input::Corrected { call, correction } => self.corrected(call, correction, now),
            Input::PauseDecoding { key, through } => self.pause_decoding(&key, through, now),
            Input::ServerTranscript {
                call,
                through,
                result,
            } => self.server_transcript(call, through, result, now),
            Input::Persisted { call, reply } => self.persisted(call, reply, now),
            Input::Rings(rings) => self.rings_changed(rings, now),
            Input::Satellite(request) => self.satellite(request, now),
            Input::RunStarted { call, result } => self.run_started(call, result, now),
            Input::Settings(settings) => self.settings_changed(settings, now),
            Input::Speech(ready) => self.speech_ready = ready,
            Input::TimeZone(zone) => self.time_zone = zone,
            Input::Volume(volume) => self.media.device_volume(volume),
            Input::HaDevice(device) => self.ha_device = device,
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
        let mut due: Vec<Option<Instant>> = vec![
            self.uplink.next_deadline().map(|at| self.instant(at)),
            self.render.next_deadline(),
            self.ring.next_deadline(),
            self.buttons.next_deadline(),
            self.view.next_deadline(),
            self.candidate
                .as_ref()
                .and_then(|c| c.decider.next_deadline())
                .map(|at| self.instant(at)),
        ];
        if let Some(turn) = &self.turn {
            due.push(turn.next_deadline().map(|at| self.instant(at)));
            due.extend(turn.focus_deadlines());
        }
        if let Some(window) = &self.window {
            due.push(window.next_deadline().map(|at| self.instant(at)));
            due.push(self.window_deadline());
        }
        due.extend(self.playouts.focus_deadlines());
        due.push(self.linger_deadline());
        due.into_iter().flatten().min()
    }

    pub fn status(&self) -> VoiceStatus {
        VoiceStatus {
            state: self.actor_state(),
            speech_ready: self.speech_ready,
            muted: self.muted,
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
        self.origin
    }

    // ---- time -----------------------------------------------------------------------

    /// The turn crate's monotonic time for a controller instant.
    pub(crate) fn mono(&self, at: Instant) -> MonoInstant {
        MonoInstant(
            u64::try_from(at.saturating_duration_since(self.origin).as_nanos()).unwrap_or(u64::MAX),
        )
    }

    pub(crate) fn instant(&self, at: MonoInstant) -> Instant {
        self.origin + Duration::from_nanos(at.0)
    }

    pub(crate) fn call_id(&mut self) -> crate::io::CallId {
        self.next_call += 1;
        crate::io::CallId(self.next_call)
    }

    // ---- ingress --------------------------------------------------------------------

    fn control(&mut self, envelope: DeviceEnvelope, now: Now) {
        let generation = envelope.header.generation;
        let message_id = envelope.header.message_id;
        match envelope.message {
            DeviceMessage::StreamOpen(open) => match self.registry.open(&open) {
                Ok(()) => self.uplink.stream_opened(&open),
                Err(error) => self.violation(error),
            },
            DeviceMessage::StreamEnd(end) => match self.registry.end(&end) {
                Ok(()) => self.uplink.stream_ended(&end, self.mono(now.at)),
                Err(error) => self.violation(error),
            },
            DeviceMessage::UplinkEnded(report) => {
                self.uplink.device_ended(&report, self.mono(now.at));
            }
            DeviceMessage::Other { message_type, body } => match message_type {
                MessageType::WakeCandidate => self.wake_candidate(message_id, &body, now),
                MessageType::WakeCandidateEnd => match crate::wire::decode_candidate_end(&body) {
                    Ok(end) => self.candidate_end(end),
                    Err(_) => self.malformed("wake.candidate_end"),
                },
                MessageType::WakeNearMiss => match crate::wire::decode_near_miss(&body) {
                    Ok(near_miss) => self.near_miss(near_miss, now),
                    Err(_) => self.malformed("wake.near_miss"),
                },
                MessageType::RenderProgress | MessageType::RenderFinished => {
                    match RenderReport::parse(message_type, &body) {
                        Some(Ok(report)) => {
                            self.render
                                .on_report(&mut self.link, now.at, generation, report);
                        }
                        _ => self.malformed(message_type.as_str()),
                    }
                }
                MessageType::ButtonAction => self.button_action(&body, now),
                _ => {}
            },
            _ => {}
        }
    }

    fn audio(&mut self, packet: Packet, now: Now) {
        match self.registry.check(&packet) {
            Ok(_) => {
                self.uplink.ingest(&packet, self.mono(now.at));
            }
            Err(error) => self.violation(error),
        }
    }

    fn violation(&mut self, error: echomuse_protocol::ProtocolError) {
        tracing::warn!(device = %self.device, %error, "the device broke the stream rules");
        self.link.effects.push(Effect::Violation(error));
        // The link closes the session; nothing it sends afterwards is trusted.
        self.lost = true;
        self.link.closed = true;
    }

    pub(crate) fn malformed(&mut self, what: &'static str) {
        tracing::warn!(device = %self.device, what, "a device message did not decode");
        *self.counters.malformed.entry(what).or_default() += 1;
    }

    /// Applies grants to the registry before any packet they allow can be checked.
    pub(crate) fn apply_grants(&mut self, grants: &[Grant]) {
        for grant in grants {
            self.registry.grant_backfill(grant.stream, grant.grant);
        }
    }

    // ---- urgent ---------------------------------------------------------------------

    fn privacy(&mut self, muted: bool, now: Now) {
        if muted == self.muted {
            return;
        }
        self.muted = muted;
        if muted {
            // Capture is revoked before anything else: queued audio and jobs are
            // discarded by scope, the subscriptions end, the workers forget the audio.
            if let Some(scope) = self.capture.take() {
                scope.cancel();
            }
            // The turns' charts and recordings keep what was heard before the mute;
            // the response's part is erased with the rest.
            self.chart_before_erasure();
            self.erase_lingering(now);
            self.uplink.mute();
            self.reset_workers();
            self.end_candidate_on_revoke(now);
            self.revoke_turn(echomuse_records::vocab::TerminalReason::Muted, now);
            self.close_window(
                echomuse_records::vocab::ContinuationEnd::Ended(
                    echomuse_records::vocab::TerminalReason::Muted,
                ),
                now,
            );
            self.near_misses.clear();
        } else {
            self.uplink.unmute();
        }
    }

    fn session_lost(&mut self, now: Now) {
        if let Some(scope) = self.capture.take() {
            scope.cancel();
        }
        // The turns' charts and recordings, before the session's audio goes.
        self.chart_before_erasure();
        self.erase_lingering(now);
        self.uplink.session_lost();
        self.end_candidate_on_revoke(now);
        self.revoke_turn(echomuse_records::vocab::TerminalReason::SessionLost, now);
        self.close_window(
            echomuse_records::vocab::ContinuationEnd::Ended(
                echomuse_records::vocab::TerminalReason::SessionLost,
            ),
            now,
        );
        self.playouts.session_lost(&mut self.link, now);
        self.render.session_ended();
        self.media.session_lost();
        let finished = self.media_effects();
        self.apply_media_effects(finished, now);
        self.ring.session_lost();
        self.view.session_lost();
        self.near_misses.clear();
        self.registry.clear();
        // Persistence still runs: the runtime carries out the effects queued so far.
        self.link.closed = true;
        self.lost = true;
    }

    /// Tells every worker to forget what it retained (mute, session end).
    pub(crate) fn reset_workers(&mut self) {
        if self.session_scope.is_cancelled() {
            return;
        }
        self.link.effects.push(Effect::Mic {
            scope: self.session_scope.clone(),
            key: None,
            job: crate::speech::MicJob::Reset,
        });
        self.link.effects.push(Effect::Reference {
            scope: self.session_scope.clone(),
            key: None,
            job: crate::speech::ReferenceJob::Reset,
        });
    }

    // ---- acknowledgements -----------------------------------------------------------

    fn ack(&mut self, id: echomuse_protocol::MessageId, outcome: AckOutcome, now: Now) {
        if self
            .render
            .on_ack(&mut self.link, now.at, &id, outcome.clone())
        {
            return;
        }
        if let Some(turn) = self.turn.as_mut()
            && turn.focus_ack(&id, &outcome)
        {
            return;
        }
        if self.playouts.focus_ack(&id, &outcome) {
            return;
        }
        // Uplink commands: a rejection means the device does not hold the subscription.
        if let Some((subscription, generation)) = self.uplink_requests.remove(&id)
            && let AckOutcome::Rejected { code } = outcome
        {
            self.uplink.command_rejected(
                &subscription,
                generation,
                code.map(|c| c.to_string()),
                self.mono(now.at),
            );
        }
    }

    // ---- after every input ----------------------------------------------------------

    /// Sends what the uplink queued, routes its deliveries and the render client's
    /// events, and brings the presentation up to date, until nothing more changes.
    fn settle(&mut self, now: Now) {
        // An armed recording mode reopens its subscription as soon as capture allows.
        self.reconcile_diagnostic(now);
        for _ in 0..64 {
            let mut changed = false;
            for Outgoing {
                generation,
                command,
            } in self.uplink.take_commands()
            {
                changed = true;
                let subscription = command.subscription_id().clone();
                if let Some(id) = self
                    .link
                    .request(Outbound::Uplink(command), generation.get())
                {
                    self.uplink_requests.insert(id, (subscription, generation));
                }
            }
            let mut deliveries = Vec::new();
            self.uplink.drain(&mut deliveries);
            for delivery in deliveries {
                changed = true;
                self.delivery(delivery, now);
            }
            while let Some(event) = self.render.next_event() {
                changed = true;
                self.render_event(event, now);
            }
            if !changed {
                break;
            }
        }
        self.present(now);
    }

    fn timers(&mut self, now: Now) {
        let mono = self.mono(now.at);
        self.uplink.poll(mono);
        self.render.poll(&mut self.link, now.at);
        if let Some(command) = self.ring.on_timer(now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
        if let Some(output) = self.buttons.on_timer(now.at) {
            self.button_output(output, None, now);
        }
        if let Some(body) = self.view.on_timer(now.at) {
            self.link.send(Outbound::TurnView(body), 0);
        }
        self.candidate_timer(now);
        self.turn_timer(now);
        self.linger_timers(now);
        self.window_timer(now);
        self.playouts.poll_focus(&mut self.link, now.at);
    }

    fn delivery(&mut self, delivery: Delivery, now: Now) {
        let subscription = delivery.subscription().clone();
        if self.is_lingering(&subscription) {
            self.lingering_delivery(delivery, now);
            return;
        }
        if self.is_diagnostic(&subscription) {
            self.diagnostic_delivery(delivery, now);
            return;
        }
        if self.near_misses.contains_key(&subscription) {
            self.near_miss_delivery(delivery, now);
            return;
        }
        if self
            .turn
            .as_ref()
            .and_then(|t| t.response.as_ref())
            .is_some_and(|(response, _)| *response == subscription)
        {
            self.response_delivery(delivery);
            return;
        }
        if self
            .candidate
            .as_ref()
            .is_some_and(|c| c.message.subscription == subscription)
            || self
                .turn
                .as_ref()
                .is_some_and(|t| t.subscription.as_ref() == Some(&subscription))
            || self
                .window
                .as_ref()
                .is_some_and(|w| w.subscription == subscription)
            || self
                .question_watch
                .as_ref()
                .is_some_and(|w| w.subscription == subscription)
        {
            self.analysed_delivery(delivery, now);
        }
        // Anything else (a closed subscription's late audio) is dropped here.
    }

    // ---- presentation ---------------------------------------------------------------

    pub(crate) fn actor_state(&self) -> ActorState {
        if let Some(turn) = &self.turn {
            return turn.actor_state();
        }
        if self.playouts.speaking() {
            return ActorState::Speaking;
        }
        if self.window.is_some() {
            return ActorState::Armed;
        }
        if self.candidate.is_some() {
            return ActorState::Armed;
        }
        ActorState::Idle
    }

    /// Brings the ring and the satellite's voice state up to date with the actor state.
    pub(crate) fn present(&mut self, now: Now) {
        let state = self.actor_state();
        if self.last_state != Some(state) {
            self.last_state = Some(state);
            if let Some(command) = self.ring.set_phase(state.phase(), now.at) {
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
        if let Some(command) = self.ring.set_settings(&settings.ring, now.at) {
            self.link.send(Outbound::Led(command), 0);
        }
        self.buttons.set_settings(settings.advanced.clone());
        self.settings = settings;
        self.push_config();
    }

    /// Sends the device's `config` when it differs from what was last sent (always at
    /// the session's start: the firmware keeps none of it across a restart).
    pub(crate) fn push_config(&mut self) {
        let config = crate::config::DeviceConfig::new(
            &self.capabilities,
            &self.settings,
            self.kiosk_token.as_deref(),
        );
        if self.config_sent.as_ref() == Some(&config) {
            return;
        }
        self.link.send(Outbound::Config(config.clone()), 0);
        self.config_sent = Some(config);
    }

    fn rings_changed(&mut self, rings: Arc<RingsSnapshot>, _now: Now) {
        let ringing = rings
            .devices
            .get(&self.device.0)
            .map(|device| device.active.is_some());
        self.link
            .effects
            .push(Effect::Satellite(SatelliteCommand::AlertRinging(ringing)));
        self.rings = rings;
    }

    /// The capture position most recently heard on `epoch`, if any.
    pub(crate) fn mic_end(&self, epoch: std::num::NonZeroU64) -> Option<Sample> {
        self.uplink.timeline().end(StreamId::Mic, epoch).map(Sample)
    }

    pub(crate) fn focus_event(&mut self, event: FocusEvent, _now: Now) {
        if let FocusEvent::Rejected(code) = event {
            tracing::warn!(device = %self.device, ?code, "the device refused a focus lease");
        }
    }

    pub(crate) fn release_focus(&mut self, lease: Option<FocusLease>, now: Now) {
        if let Some(mut lease) = lease {
            let _ = lease.release(&mut self.link, now.at);
            self.playouts.releasing.push(lease);
        }
    }

    pub(crate) fn render_event(&mut self, event: RenderEvent, now: Now) {
        let playback: &PlaybackRef = match &event {
            RenderEvent::Accepted(p) | RenderEvent::Started(p) => p,
            RenderEvent::Progress { playback, .. }
            | RenderEvent::Cancelled { playback, .. }
            | RenderEvent::LimitExceeded { playback, .. }
            | RenderEvent::Ended { playback, .. } => playback,
        };
        let playback = playback.clone();
        self.playout_event(&playback, event, now);
    }

    pub(crate) fn worker_failed(&mut self, error: &RuntimeError) {
        tracing::warn!(device = %self.device, %error, "a speech job failed");
    }
}
