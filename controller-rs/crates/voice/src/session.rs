//! The session mechanisms the machine's components share (rust-voice-decomposition.md
//! §3, §4), and what each component borrows of them for one call.
//!
//! [`SessionAudio`] owns the stream registry and the uplink with its command
//! acknowledgements; it holds no candidate or turn state and no policy. Opening or
//! converting a subscription installs its backfill grants in the registry before any
//! later packet can be checked against them. It also keeps the focus leases released and
//! awaiting their acknowledgement ([`ReleasedFocus`]), which belong to the session rather
//! than to the operation that let them go. [`Ids`] allocates the identities every component
//! draws from one session-wide sequence.
//!
//! Each component borrows only what it uses: [`OutputCx`] (the effect sink, call ids,
//! released focus, the stale-result count and the device's playback settings),
//! [`WakeCx`] and [`ConversationCx`] (their own view of the session's facts, the audio
//! mechanisms, the effect sink, identities and counters). None of them receives the
//! machine or another component. The ring and the turn view stay the machine's
//! ([`Presenter`]): a component names what to show as a typed [`Shown`] value, which the
//! presenter applies at once, so its effect keeps its place among the others.

use std::{collections::HashMap, num::NonZeroU64, sync::Arc, time::Duration};

use echomuse_alerts::RingsSnapshot;
use echomuse_core::{
    services::HaDeviceId,
    types::{DeviceId, SessionId, TurnId, WorkScope},
};
use echomuse_grammar::Grammar;
use echomuse_playback::{AckOutcome, FocusLease};
use echomuse_presentation::{
    ring::{Cue, LedProjector},
    turn_view::{TurnView, ViewEnd},
};
use echomuse_protocol::{
    MessageId, ProtocolError, StreamId,
    ema::Packet,
    streams::{StreamEnd, StreamOpen, StreamRegistry, UplinkEnded},
};
use echomuse_settings::DeviceSettings;
use echomuse_turn::{EvidenceAssembler, MonoInstant, Sample};
use echomuse_uplink::{
    CandidateOpen, CloseReason, CommandError, Delivery, Generation, Grant, NearMissOpen, OpenError,
    Opened, Owner, SessionTerms, SubscriptionId, Uplink, UplinkConfig, timeline::Timeline,
    wire::Outgoing,
};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    arbiter::WakeArbiter,
    evidence::{Evidence, Noting},
    io::{CallId, Effect, Now, Outbound},
    link::EffectLink,
    machine::{VoiceCounters, WakeModelInfo},
    speech::{MicJob, ReferenceJob, StreamKey},
};

/// The machine's monotonic clock: the turn crate's time counts from its origin.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clock {
    origin: Instant,
}

impl Clock {
    pub(crate) fn new(origin: Instant) -> Self {
        Self { origin }
    }

    pub(crate) fn origin(self) -> Instant {
        self.origin
    }

    /// The turn crate's monotonic time for a controller instant.
    pub(crate) fn mono(self, at: Instant) -> MonoInstant {
        MonoInstant(
            u64::try_from(at.saturating_duration_since(self.origin).as_nanos()).unwrap_or(u64::MAX),
        )
    }

    pub(crate) fn instant(self, at: MonoInstant) -> Instant {
        self.origin + Duration::from_nanos(at.0)
    }
}

/// The identities allocated across the session: call ids, turn scopes and the session
/// recorder's policy identities, each from one session-wide sequence.
#[derive(Debug, Default)]
pub(crate) struct Ids {
    calls: u64,
    turns: u64,
    policies: u64,
}

impl Ids {
    pub(crate) fn call(&mut self) -> CallId {
        self.calls += 1;
        CallId(self.calls)
    }

    /// A new turn's scope under `base` (capture, or a candidate's capture scope).
    pub(crate) fn turn_scope(&mut self, base: &WorkScope) -> WorkScope {
        self.turns += 1;
        base.for_turn(TurnId(self.turns))
    }

    /// The recorder's identity for the next turn policy.
    pub(crate) fn policy(&mut self) -> u64 {
        self.policies += 1;
        self.policies
    }
}

/// Focus leases released and awaiting the device's acknowledgement.
#[derive(Debug, Default)]
pub(crate) struct ReleasedFocus {
    leases: Vec<FocusLease>,
}

impl ReleasedFocus {
    /// Releases a lease; its acknowledgement is waited for here.
    pub(crate) fn release(&mut self, link: &mut EffectLink, lease: Option<FocusLease>, now: Now) {
        if let Some(mut lease) = lease {
            let _ = lease.release(link, now.at);
            self.leases.push(lease);
        }
    }

    pub(crate) fn poll(&mut self, link: &mut EffectLink, now: Instant) {
        for lease in &mut self.leases {
            let _ = lease.poll(link, now);
        }
        self.leases.retain(FocusLease::is_held);
    }

    /// A `command.ack` for one of the released leases.
    pub(crate) fn ack(&mut self, id: &MessageId, outcome: &AckOutcome) -> bool {
        self.leases
            .iter_mut()
            .any(|lease| lease.on_ack(id, outcome.clone()).is_ok())
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.leases
            .iter()
            .filter_map(FocusLease::next_deadline)
            .min()
    }

    /// The session ended: nothing more will be acknowledged.
    pub(crate) fn clear(&mut self) {
        self.leases.clear();
    }
}

/// What a component shows on the ring or the turn view.
pub(crate) enum Shown<'t> {
    Cue(Cue),
    /// A turn started listening.
    Listening(Uuid),
    /// The transcript being worked on.
    Thinking(&'t str),
    /// The answer's text so far, streamed.
    AnswerSoFar(&'t str),
    /// The answer's final text.
    FinalAnswer(&'t str),
    /// The turn ended.
    Done(ViewEnd),
}

/// The ring and the turn view: the machine's presentation.
pub(crate) struct Presenter {
    pub(crate) ring: LedProjector,
    pub(crate) view: TurnView,
}

impl Presenter {
    /// Applies what a component shows and sends what changed.
    fn show(&mut self, link: &mut EffectLink, shown: Shown<'_>, now: Now) {
        let body = match shown {
            Shown::Cue(cue) => {
                if let Some(command) = self.ring.cue(cue, now.at) {
                    link.send(Outbound::Led(command), 0);
                }
                return;
            }
            Shown::Listening(turn) => self.view.listening(turn),
            Shown::Thinking(text) => self.view.thinking(text),
            Shown::AnswerSoFar(text) => self.view.answer_so_far(text, now.at),
            Shown::FinalAnswer(text) => self.view.final_answer(text),
            Shown::Done(end) => self.view.done(end),
        };
        if let Some(body) = body {
            link.send(Outbound::TurnView(body), 0);
        }
    }
}

/// The session's audio mechanisms. See the module documentation.
pub(crate) struct SessionAudio {
    registry: StreamRegistry,
    uplink: Uplink,
    /// Uplink commands awaiting the device's acknowledgement.
    commands: HashMap<MessageId, (SubscriptionId, Generation)>,
    /// The session's own scope: closing an analysis and resetting the workers use it,
    /// since a stream's own scope may already be cancelled.
    session_scope: WorkScope,
    /// Released focus leases awaiting their acknowledgement.
    released: ReleasedFocus,
}

impl SessionAudio {
    pub(crate) fn new(terms: SessionTerms, muted: bool, session_scope: WorkScope) -> Self {
        Self {
            registry: StreamRegistry::new(terms.afe_metadata),
            uplink: Uplink::new(UplinkConfig::default(), terms, muted),
            commands: HashMap::new(),
            session_scope,
            released: ReleasedFocus::default(),
        }
    }

    // ---- ingress --------------------------------------------------------------------

    pub(crate) fn stream_open(&mut self, open: &StreamOpen) -> Result<(), ProtocolError> {
        self.registry.open(open)?;
        self.uplink.stream_opened(open);
        Ok(())
    }

    pub(crate) fn stream_end(
        &mut self,
        end: &StreamEnd,
        now: MonoInstant,
    ) -> Result<(), ProtocolError> {
        self.registry.end(end)?;
        self.uplink.stream_ended(end, now);
        Ok(())
    }

    pub(crate) fn device_ended(&mut self, report: &UplinkEnded, now: MonoInstant) {
        self.uplink.device_ended(report, now);
    }

    /// Checks a packet against the registry and, if it passes, hands it to the uplink.
    pub(crate) fn packet(
        &mut self,
        packet: &Packet,
        now: MonoInstant,
    ) -> Result<(), ProtocolError> {
        self.registry.check(packet)?;
        self.uplink.ingest(packet, now);
        Ok(())
    }

    // ---- reads ----------------------------------------------------------------------

    pub(crate) fn terms(&self) -> SessionTerms {
        self.uplink.terms()
    }

    pub(crate) fn timeline(&self) -> &Timeline {
        self.uplink.timeline()
    }

    pub(crate) fn diagnostic_live(&self) -> bool {
        self.uplink.diagnostic_live()
    }

    /// The capture position most recently heard on `epoch`, if any.
    pub(crate) fn mic_end(&self, epoch: NonZeroU64) -> Option<Sample> {
        self.uplink.timeline().end(StreamId::Mic, epoch).map(Sample)
    }

    /// When [`Self::poll`] next has something to do.
    pub(crate) fn next_deadline(&self) -> Option<MonoInstant> {
        self.uplink.next_deadline()
    }

    // ---- subscriptions --------------------------------------------------------------

    /// Applies grants to the registry before any packet they allow can be checked.
    fn install(&mut self, grants: &[Grant]) {
        for grant in grants {
            self.registry.grant_backfill(grant.stream, grant.grant);
        }
    }

    fn opened(&mut self, opened: Result<Opened, OpenError>) -> Result<SubscriptionId, OpenError> {
        let opened = opened?;
        self.install(&opened.grants);
        Ok(opened.subscription_id)
    }

    pub(crate) fn accept_candidate(
        &mut self,
        candidate: CandidateOpen,
        now: MonoInstant,
    ) -> Result<(), OpenError> {
        let grants = self.uplink.accept_candidate(candidate, now)?;
        self.install(&grants);
        Ok(())
    }

    /// Converts an accepted candidate's subscription into its turn's, in place.
    pub(crate) fn convert_to_turn(
        &mut self,
        id: &SubscriptionId,
        turn: Owner,
        now: MonoInstant,
    ) -> Result<(), CommandError> {
        self.uplink.convert_to_turn(id, turn, now)
    }

    pub(crate) fn open_button_turn(
        &mut self,
        turn: Owner,
        press: Sample,
        now: MonoInstant,
    ) -> Result<SubscriptionId, OpenError> {
        let opened = self.uplink.open_button_turn(turn, press, now);
        self.opened(opened)
    }

    pub(crate) fn open_reply(
        &mut self,
        owner: Owner,
        cells_from: Sample,
        now: MonoInstant,
    ) -> Result<SubscriptionId, OpenError> {
        let opened = self.uplink.open_reply(owner, cells_from, now);
        self.opened(opened)
    }

    pub(crate) fn open_response(
        &mut self,
        turn: Owner,
        afe_from: Sample,
        mic_from: Option<Sample>,
        now: MonoInstant,
    ) -> Result<SubscriptionId, OpenError> {
        let opened = self.uplink.open_response(turn, afe_from, mic_from, now);
        self.opened(opened)
    }

    pub(crate) fn open_near_miss(
        &mut self,
        near_miss: NearMissOpen,
        now: MonoInstant,
    ) -> Result<SubscriptionId, OpenError> {
        let opened = self.uplink.open_near_miss(near_miss, now);
        self.opened(opened)
    }

    pub(crate) fn open_diagnostic(
        &mut self,
        owner: Owner,
        now: MonoInstant,
    ) -> Result<SubscriptionId, OpenError> {
        let opened = self.uplink.open_diagnostic(owner, now);
        self.opened(opened)
    }

    /// Closes a subscription; one already gone is not an error worth reporting.
    pub(crate) fn close(&mut self, id: &SubscriptionId, reason: CloseReason, now: MonoInstant) {
        let _ = self.uplink.close(id, reason, now);
    }

    /// Sends the uplink's queued commands, remembering each for its acknowledgement.
    /// Returns whether any went.
    pub(crate) fn send_commands(&mut self, link: &mut EffectLink) -> bool {
        let mut sent = false;
        for Outgoing {
            generation,
            command,
        } in self.uplink.take_commands()
        {
            sent = true;
            let subscription = command.subscription_id().clone();
            if let Some(id) = link.request(Outbound::Uplink(command), generation.get()) {
                self.commands.insert(id, (subscription, generation));
            }
        }
        sent
    }

    /// Moves every queued delivery to `out`, in the order released.
    pub(crate) fn drain(&mut self, out: &mut Vec<Delivery>) {
        self.uplink.drain(out);
    }

    pub(crate) fn poll(&mut self, now: MonoInstant) {
        self.uplink.poll(now);
    }

    pub(crate) fn mute(&mut self) {
        self.uplink.mute();
    }

    pub(crate) fn unmute(&mut self) {
        self.uplink.unmute();
    }

    /// The session ended: the uplink's queued audio and commands go.
    pub(crate) fn session_lost(&mut self) {
        self.uplink.session_lost();
    }

    /// Forgets every stream epoch and released lease: the session is over.
    pub(crate) fn clear(&mut self) {
        self.released.clear();
        self.registry.clear();
    }

    // ---- released focus -------------------------------------------------------------

    /// The released-focus bookkeeping alone, for the output's own releases.
    pub(crate) fn released_focus(&mut self) -> &mut ReleasedFocus {
        &mut self.released
    }

    /// Releases a focus lease; its acknowledgement is waited for here.
    pub(crate) fn release(&mut self, link: &mut EffectLink, lease: Option<FocusLease>, now: Now) {
        self.released.release(link, lease, now);
    }

    pub(crate) fn poll_released(&mut self, link: &mut EffectLink, now: Instant) {
        self.released.poll(link, now);
    }

    pub(crate) fn released_deadline(&self) -> Option<Instant> {
        self.released.next_deadline()
    }

    /// A released lease's or an uplink command's acknowledgement: a rejected uplink
    /// command means the device does not hold the subscription.
    pub(crate) fn ack(&mut self, id: &MessageId, outcome: AckOutcome, now: MonoInstant) {
        if self.released.ack(id, &outcome) {
            return;
        }
        if let Some((subscription, generation)) = self.commands.remove(id)
            && let AckOutcome::Rejected { code } = outcome
        {
            self.uplink.command_rejected(
                &subscription,
                generation,
                code.map(|c| c.to_string()),
                now,
            );
        }
    }

    // ---- analysed subscriptions -----------------------------------------------------

    /// Opens a stream on both workers under `scope`: the mic analysis under `plan`, and
    /// reference scoring with the device's wake graph.
    pub(crate) fn open_analysis(
        &self,
        link: &mut EffectLink,
        scope: &WorkScope,
        key: StreamKey,
        plan: echomuse_echo::EchoPlan,
        graph: Option<echomuse_speech::BcresnetModelFiles>,
    ) {
        crate::analysis::mic_job(
            link,
            scope,
            MicJob::Open {
                key: key.clone(),
                plan,
            },
        );
        crate::analysis::reference_job(link, scope, ReferenceJob::Open { key, graph });
    }

    /// Closes a stream on both workers. Uses the session scope: the stream's own may
    /// already be cancelled, and the workers must still forget its audio.
    pub(crate) fn close_analysis(&self, link: &mut EffectLink, key: StreamKey) {
        if self.session_scope.is_cancelled() {
            return;
        }
        link.effects.push(Effect::Mic {
            scope: self.session_scope.clone(),
            key: Some(key.clone()),
            job: MicJob::Close { key: key.clone() },
        });
        link.effects.push(Effect::Reference {
            scope: self.session_scope.clone(),
            key: Some(key.clone()),
            job: ReferenceJob::Close { key },
        });
    }

    /// Tells every worker to forget what it retained (mute, session end).
    pub(crate) fn reset_workers(&self, link: &mut EffectLink) {
        if self.session_scope.is_cancelled() {
            return;
        }
        link.effects.push(Effect::Mic {
            scope: self.session_scope.clone(),
            key: None,
            job: MicJob::Reset,
        });
        link.effects.push(Effect::Reference {
            scope: self.session_scope.clone(),
            key: None,
            job: ReferenceJob::Reset,
        });
    }
}

// ---- what each component borrows ----------------------------------------------------

/// What the device's output borrows for one call.
pub(crate) struct OutputCx<'a> {
    pub(crate) link: &'a mut EffectLink,
    pub(crate) ids: &'a mut Ids,
    pub(crate) released: &'a mut ReleasedFocus,
    /// Results for work that already ended.
    pub(crate) stale: &'a mut u64,
    pub(crate) device: &'a DeviceId,
    /// For the content equalizer.
    pub(crate) settings: &'a DeviceSettings,
}

/// What the wake handling reads of the session.
#[derive(Clone, Copy)]
pub(crate) struct WakeFacts<'a> {
    pub(crate) device: &'a DeviceId,
    pub(crate) session: SessionId,
    pub(crate) wake: &'a WakeModelInfo,
    pub(crate) settings: &'a DeviceSettings,
    pub(crate) muted: bool,
    /// The capture scope; `None` while capture is revoked.
    pub(crate) capture: &'a Option<WorkScope>,
    pub(crate) speech_ready: bool,
    pub(crate) clock: Clock,
}

impl WakeFacts<'_> {
    pub(crate) fn mono(&self, at: Instant) -> MonoInstant {
        self.clock.mono(at)
    }
}

/// What the wake handling borrows for one call.
pub(crate) struct WakeCx<'a> {
    pub(crate) facts: WakeFacts<'a>,
    pub(crate) link: &'a mut EffectLink,
    pub(crate) audio: &'a mut SessionAudio,
    pub(crate) ids: &'a mut Ids,
    pub(crate) counters: &'a mut VoiceCounters,
    /// A session recording runs: decision evidence is queued.
    pub(crate) recording: bool,
    presenter: &'a mut Presenter,
}

impl<'a> WakeCx<'a> {
    pub(crate) fn new(
        facts: WakeFacts<'a>,
        link: &'a mut EffectLink,
        audio: &'a mut SessionAudio,
        ids: &'a mut Ids,
        counters: &'a mut VoiceCounters,
        recording: bool,
        presenter: &'a mut Presenter,
    ) -> Self {
        Self {
            facts,
            link,
            audio,
            ids,
            counters,
            recording,
            presenter,
        }
    }

    /// Queues one piece of evidence while a recording runs.
    pub(crate) fn note(&mut self, now: Now, evidence: impl FnOnce() -> Evidence) {
        note(self.link, self.recording, self.facts.clock, now, evidence);
    }

    pub(crate) fn show(&mut self, shown: Shown<'_>, now: Now) {
        self.presenter.show(self.link, shown, now);
    }

    pub(crate) fn malformed(&mut self, what: &'static str) {
        tracing::warn!(device = %self.facts.device, what, "a device message did not decode");
        *self.counters.malformed.entry(what).or_default() += 1;
    }

    pub(crate) fn worker_failed(&self, error: &echomuse_core::types::RuntimeError) {
        tracing::warn!(device = %self.facts.device, %error, "a speech job failed");
    }
}

/// What the conversation reads of the session.
#[derive(Clone, Copy)]
pub(crate) struct ConversationFacts<'a> {
    pub(crate) device: &'a DeviceId,
    pub(crate) session: SessionId,
    pub(crate) wake: &'a WakeModelInfo,
    pub(crate) grammar: &'a Grammar,
    pub(crate) arbiter: &'a WakeArbiter,
    /// The current settings; a turn keeps the snapshot it opened with.
    pub(crate) settings: &'a Arc<DeviceSettings>,
    /// The capture scope; `None` while capture is revoked.
    pub(crate) capture: &'a Option<WorkScope>,
    pub(crate) rings: &'a RingsSnapshot,
    pub(crate) time_zone: &'a Option<echomuse_alerts::schedule::HaTimeZone>,
    pub(crate) ha_device: &'a Option<HaDeviceId>,
    pub(crate) clock: Clock,
}

impl ConversationFacts<'_> {
    pub(crate) fn mono(&self, at: Instant) -> MonoInstant {
        self.clock.mono(at)
    }

    pub(crate) fn instant(&self, at: MonoInstant) -> Instant {
        self.clock.instant(at)
    }
}

/// What the conversation borrows for one call.
pub(crate) struct ConversationCx<'a> {
    pub(crate) facts: ConversationFacts<'a>,
    pub(crate) link: &'a mut EffectLink,
    pub(crate) audio: &'a mut SessionAudio,
    pub(crate) ids: &'a mut Ids,
    pub(crate) counters: &'a mut VoiceCounters,
    /// A session recording runs: decision evidence is queued.
    pub(crate) recording: bool,
    presenter: &'a mut Presenter,
}

impl<'a> ConversationCx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        facts: ConversationFacts<'a>,
        link: &'a mut EffectLink,
        audio: &'a mut SessionAudio,
        ids: &'a mut Ids,
        counters: &'a mut VoiceCounters,
        recording: bool,
        presenter: &'a mut Presenter,
    ) -> Self {
        Self {
            facts,
            link,
            audio,
            ids,
            counters,
            recording,
            presenter,
        }
    }

    /// What the output borrows when the conversation asks it to play or stop.
    pub(crate) fn output(&mut self) -> OutputCx<'_> {
        OutputCx {
            link: &mut *self.link,
            ids: &mut *self.ids,
            released: self.audio.released_focus(),
            stale: &mut self.counters.stale_results,
            device: self.facts.device,
            settings: self.facts.settings,
        }
    }

    /// Queues one piece of evidence while a recording runs.
    pub(crate) fn note(&mut self, now: Now, evidence: impl FnOnce() -> Evidence) {
        note(self.link, self.recording, self.facts.clock, now, evidence);
    }

    /// An assembler whose accepted pushes are noted under turn `id` while a recording
    /// runs.
    pub(crate) fn noting<'b>(
        &'b mut self,
        assembler: &'b mut EvidenceAssembler,
        id: u64,
        now: Now,
    ) -> Noting<'b> {
        let at = self.facts.mono(now.at);
        let notes = self.recording.then_some((&mut self.link.effects, id, at));
        Noting { assembler, notes }
    }

    pub(crate) fn show(&mut self, shown: Shown<'_>, now: Now) {
        self.presenter.show(self.link, shown, now);
    }

    pub(crate) fn worker_failed(&self, error: &echomuse_core::types::RuntimeError) {
        tracing::warn!(device = %self.facts.device, %error, "a speech job failed");
    }
}

fn note(
    link: &mut EffectLink,
    recording: bool,
    clock: Clock,
    now: Now,
    evidence: impl FnOnce() -> Evidence,
) {
    if recording {
        link.effects.push(Effect::Evidence {
            at: clock.mono(now.at),
            evidence: Box::new(evidence()),
        });
    }
}
