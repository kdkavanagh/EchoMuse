//! What the device runtime hands the voice machine ([`Input`]) and what the machine asks
//! the runtime to do ([`Effect`]).
//!
//! The machine never performs I/O, never waits and never reads a clock: every input
//! comes with the time it is handled at ([`Now`]), and every external action is an
//! effect the runtime carries out and answers with another input. Each effect that
//! expects an answer carries the identity its answer must quote (a [`CallId`], a
//! [`MessageId`], a [`StreamKey`], a turn's UUID), so the machine can tell a current
//! answer from a stale one (spec §5.7).

use std::sync::Arc;

use echomuse_alerts::{
    Act, ActOutcome, AlarmEntry, AlarmSet, CancelAlarms, CancelOutcome, Outcome, Refusal,
    RingsSnapshot, ServiceStopped, SetAlarm,
};
use echomuse_core::{
    services::{
        DirectIntent, EffectOutcome, HaDeviceId, HaError, MediaUrl, NotDispatched, PipelineId,
        RunEvent, SatelliteTarget, SentenceRecognition, TimerStatus, Transcript,
    },
    types::{RuntimeError, WorkScope},
};
use echomuse_esphome::{
    entities::{ButtonEvent, MediaState, VoiceState},
    voice::{AnnouncementId, RunId, SatelliteRequest},
};
use echomuse_playback::AckOutcome;
use echomuse_playback::{DecodeEvent, EqSettings, RenderFrame, wire::Command as PlaybackCommand};
use echomuse_presentation::{ring::wire::LedCommand, turn_view::TurnViewBody};
use echomuse_protocol::{
    ControlBody, DeviceEnvelope, MessageId, MessageType, ProtocolError, ema::Packet,
    session::CommandAck,
};
use echomuse_records::{
    NearMissAudio, NearMissRecord, PersistedNearMiss, PersistedTurn, TurnAudio, TurnRecord,
    vocab::ContinuationEnd,
};
use echomuse_settings::DeviceSettings;
use echomuse_uplink::UplinkCommand;
use serde::Serialize;
use tokio::time::Instant;
use uuid::Uuid;

use crate::speech::{
    MicJob, MicOutput, ReferenceJob, ReferenceOutput, SpanJob, SpanOutput, StreamKey, VerifyJob,
    VerifyOutput,
};

/// The time an input is handled at: the controller's monotonic clock for deadlines and
/// wall time (Unix milliseconds) for what is persisted and for alarms. They are never
/// mixed: wall time moving does not move a deadline (§5.3).
#[derive(Clone, Copy, Debug)]
pub struct Now {
    pub at: Instant,
    pub wall_ms: i64,
}

/// Names one asynchronous request the machine made (HA, alerts, persistence, decoding,
/// the satellite). Never reused within a machine.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CallId(pub(crate) u64);

/// Something the device runtime hands the machine. Ingress inputs ([`Input::Control`],
/// [`Input::Audio`]) must arrive in the link's ingress order; urgent ones
/// ([`Input::Privacy`], [`Input::SessionLost`]) as soon as they are known.
#[derive(Debug)]
pub enum Input {
    /// A device control message from the ordered ingress: `stream.open`, `stream.end`,
    /// `uplink.ended`, `wake.candidate`, `wake.candidate_end`, `wake.near_miss`,
    /// `render.progress`, `render.finished`, `button.action`. Other types are ignored.
    Control(DeviceEnvelope),
    /// An audio packet from the ordered ingress, not yet checked against the stream
    /// registry: the machine owns the registry.
    Audio(Packet),
    /// `privacy.changed`, already applied to the capture scope by the link and runtime.
    Privacy {
        muted: bool,
        physical_seq: Option<u64>,
    },
    /// The runtime's current capture scope: a fresh one after an unmute, `None` while
    /// capture is not permitted. Every turn's scope is derived from it.
    Capture(Option<WorkScope>),
    /// The device session ended. The machine revokes everything and does nothing more.
    SessionLost,
    /// The device acknowledged (or did not acknowledge) a command sent with
    /// [`Effect::Request`].
    Ack {
        id: MessageId,
        outcome: AckOutcome,
    },
    Mic {
        key: Option<StreamKey>,
        result: Result<MicOutput, RuntimeError>,
    },
    Reference {
        key: Option<StreamKey>,
        result: Result<ReferenceOutput, RuntimeError>,
    },
    Verify {
        candidate: String,
        result: Result<VerifyOutput, RuntimeError>,
    },
    Span {
        turn: Uuid,
        result: Result<SpanOutput, RuntimeError>,
    },
    /// The answer to an [`Effect::Ha`] call, or one event of an intent run.
    Ha {
        call: CallId,
        reply: HaReply,
    },
    /// The next event of a decode the machine pulled ([`DecodeCommand::Pull`]).
    Decode {
        call: CallId,
        event: DecodeReport,
    },
    Alert {
        call: CallId,
        reply: AlertReply,
    },
    Persisted {
        call: CallId,
        reply: PersistReply,
    },
    /// The alert service's latest view of what rings where.
    Rings(Arc<RingsSnapshot>),
    /// A request Home Assistant sent the device's ESPHome satellite.
    Satellite(SatelliteRequest),
    /// The answer to [`SatelliteCommand::StartRun`].
    RunStarted {
        call: CallId,
        result: Result<RunId, String>,
    },
    /// The device's effective settings changed. A turn already open keeps the snapshot
    /// it opened with.
    Settings(Arc<DeviceSettings>),
    /// Whether the speech models are loaded and usable.
    Speech(bool),
    /// Home Assistant's time zone, for alarms; `None` while it is not known.
    TimeZone(Option<echomuse_alerts::schedule::HaTimeZone>),
    /// This satellite's device in Home Assistant's registry, once looked up.
    HaDevice(Option<echomuse_core::services::HaDeviceId>),
    /// The device reported its volume (`volume_state`); `None`: no volume control.
    Volume(Option<echomuse_esphome::entities::DeviceVolume>),
    /// The machine's [`crate::Voice::next_deadline`] passed.
    Timer,
    /// The Kiosk Satellite token stored for this device changed (`None`: none stored).
    KioskToken(Option<String>),
    /// The speaker's running timers as the alert service shows them (empty: none).
    Timers(echomuse_alerts::wire::TimerView),
    /// Play an alert sound's preview on the device (the dashboard's sound preview).
    /// Answered with [`Effect::PreviewAnswer`] quoting `id`.
    Preview {
        id: PreviewId,
        sound: echomuse_playback::AlertPreviewSound,
    },
    /// Stop the preview playing, if any. Answered with [`Effect::PreviewAnswer`].
    StopPreview {
        id: PreviewId,
    },
    /// Arm (`true`) or disarm the recording modes' `diagnostic` subscription; its audio
    /// comes back as [`Effect::Diagnostic`].
    Diagnostic(bool),
    /// The answer to [`Effect::Correct`]: what to send, then (for a check that cannot
    /// change the text) the record once the check finished.
    Corrected {
        call: CallId,
        correction: Correction,
    },
    /// The microphone worker is starting the pause decode of stream `key`'s utterance
    /// through `through` (sent from inside its job, before the decode), so the pause
    /// server can be asked at the same moment (§4.7).
    PauseDecoding {
        key: crate::speech::StreamKey,
        through: echomuse_turn::Sample,
    },
    /// The answer to [`Effect::ServerTranscribe`]: the server's transcript of the audio
    /// ending at `through`, or why there is none.
    ServerTranscript {
        call: CallId,
        through: echomuse_turn::Sample,
        result: Result<String, echomuse_turn::ServerFailure>,
    },
}

/// Home Assistant's answers, one variant per request kind.
#[derive(Debug)]
pub enum HaReply {
    Pipeline(Result<PipelineId, HaError>),
    Stt(Result<Transcript, HaError>),
    /// The intent run was dispatched (`Ok`) or nothing was sent (`Err`).
    IntentDispatched(Result<(), NotDispatched>),
    /// One event of a dispatched intent run, in HA's order. A run whose event channel
    /// closed without a terminal event is reported as [`RunEvent::Lost`].
    IntentEvent(RunEvent),
    Tts(Result<MediaUrl, HaError>),
    Recognize(Result<Vec<Option<SentenceRecognition>>, HaError>),
    TimerStatus(Result<TimerStatus, HaError>),
    CallIntent(EffectOutcome<()>),
}

/// What one decode pull produced.
#[derive(Debug)]
pub enum DecodeReport {
    Event(DecodeEvent),
    /// The decode ended without a `Finished` event (cancelled, or its task stopped).
    Closed,
}

#[derive(Debug)]
pub enum AlertReply {
    Act(Result<ActOutcome, ServiceStopped>),
    SetAlarm(Result<Outcome<AlarmSet>, ServiceStopped>),
    CancelAlarms(Result<CancelOutcome, ServiceStopped>),
    ListAlarms(Result<Result<Vec<AlarmEntry>, Refusal>, ServiceStopped>),
}

#[derive(Debug)]
pub enum PersistReply {
    Turn(Result<PersistedTurn, String>),
    NearMiss(Result<PersistedNearMiss, String>),
    Continuation(Result<(), String>),
}

/// A controller → device message body. Each is sent with the envelope generation the
/// effect names.
#[derive(Clone, Debug)]
pub enum Outbound {
    Uplink(UplinkCommand),
    Playback(PlaybackCommand),
    Led(LedCommand),
    TurnView(TurnViewBody),
    /// The controller's `command.ack` of a `wake.candidate`.
    CandidateAck(CommandAck),
    VolumeSet(VolumeSet),
    /// The device's `config`.
    Config(crate::config::DeviceConfig),
}

impl Outbound {
    pub fn message_type(&self) -> MessageType {
        match self {
            Self::Uplink(command) => command.message_type(),
            Self::Playback(command) => command.message_type(),
            Self::Led(command) => command.message_type(),
            Self::TurnView(_) => MessageType::TurnView,
            Self::CandidateAck(_) => MessageType::CommandAck,
            Self::VolumeSet(_) => MessageType::VolumeSet,
            Self::Config(_) => MessageType::Config,
        }
    }
}

/// `volume_set {level}` (device/internal/supervisor/session.go: `volumeState`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct VolumeSet {
    pub level: u8,
}

impl ControlBody for VolumeSet {
    const MESSAGE_TYPE: MessageType = MessageType::VolumeSet;
}

/// A Home Assistant request. Every one carries the scope of the turn (or session) it is
/// for; the client refuses a cancelled scope without sending anything.
#[derive(Clone, Debug)]
pub enum HaRequest {
    /// The pipeline the satellite's select chooses (HA's preferred one without a device).
    Pipeline {
        ha_device: Option<HaDeviceId>,
    },
    Stt {
        target: SatelliteTarget,
        audio: Arc<[i16]>,
    },
    /// Never resent: an intent run's outcome after dispatch may be unknown (§3.3.3).
    IntentTts {
        target: SatelliteTarget,
        text: String,
        conversation: Option<echomuse_core::services::ConversationId>,
    },
    Tts {
        target: SatelliteTarget,
        text: String,
    },
    /// HA's sentence matcher on a stable prefix: asked about the sentences
    /// [`recognizer_sentences`] gives for `text`, answered in that order.
    Recognize {
        text: String,
        ha_device: Option<HaDeviceId>,
    },
    TimerStatus {
        ha_device: Option<HaDeviceId>,
    },
    /// Never resent: a state-changing request (§3.3.3).
    CallIntent {
        intent: DirectIntent,
        ha_device: Option<HaDeviceId>,
    },
}

/// The word added to a stable prefix to ask HA's matcher whether a wildcard could take
/// more words. Nobody says it, and it is no entity, area or list value; it stays fixed so
/// that HA's recognition cache, keyed by text, answers a request said before.
pub const RECOGNIZER_PROBE: &str = "zqxj";

/// What HA's matcher is asked about a stable prefix (§4.6): the text, then the text and
/// [`RECOGNIZER_PROBE`].
pub fn recognizer_sentences(text: &str) -> Vec<String> {
    vec![text.to_owned(), format!("{text} {RECOGNIZER_PROBE}")]
}

/// HA's answers to [`recognizer_sentences`] as a completeness class (§4.6). A full match
/// is complete, unless the text with the probe word matches too: a wildcard (`play
/// {query}`, `what's the weather {when}`) can take more words, so it is extendable. No
/// match, but the closest intent left slots unfilled: it needs more. Nothing recognized:
/// unknown. A missing probe answer counts as no match.
pub(crate) fn recognized_class(
    answers: &[Option<SentenceRecognition>],
) -> echomuse_turn::TextClass {
    use echomuse_turn::TextClass;
    let matched = |i: usize| {
        answers
            .get(i)
            .and_then(Option::as_ref)
            .is_some_and(|r| r.matched)
    };
    match answers.first().and_then(Option::as_ref) {
        Some(text) if text.matched && matched(1) => TextClass::Extendable,
        Some(text) if text.matched => TextClass::Complete,
        Some(text) if text.unfilled_slots => TextClass::NeedsMore,
        _ => TextClass::Unknown,
    }
}

#[cfg(test)]
mod recognizer_tests {
    use echomuse_turn::TextClass;

    use super::*;

    fn recognition(matched: bool, unfilled_slots: bool) -> Option<SentenceRecognition> {
        Some(SentenceRecognition {
            matched,
            unfilled_slots,
            source: Some("builtin".to_owned()),
            template: String::new(),
            intent: None,
            details: Default::default(),
            unmatched: Default::default(),
            targets: Vec::new(),
        })
    }

    #[test]
    fn the_probe_word_tells_a_wildcard_from_a_whole_request() {
        assert_eq!(
            recognizer_sentences("what time is it"),
            ["what time is it", "what time is it zqxj"]
        );
        // "what time is it zqxj" leaves a slot unfilled: nothing takes more words.
        let time = [recognition(true, false), recognition(false, true)];
        assert_eq!(recognized_class(&time), TextClass::Complete);
        // "play zqxj" plays something called zqxj.
        let play = [recognition(true, false), recognition(true, false)];
        assert_eq!(recognized_class(&play), TextClass::Extendable);
        let timer = [recognition(false, true), recognition(false, true)];
        assert_eq!(recognized_class(&timer), TextClass::NeedsMore);
        assert_eq!(recognized_class(&[None, None]), TextClass::Unknown);
        // Without the probe's answer the text's match counts as whole.
        assert_eq!(
            recognized_class(&[recognition(true, false)]),
            TextClass::Complete
        );
    }
}

/// A decode the runtime runs with the playback crate's ffmpeg decoder. The machine pulls
/// one event at a time so a reply's audio is read only as fast as the device plays it.
#[derive(Clone, Debug)]
pub enum DecodeCommand {
    /// `url` as Home Assistant gave it (absolute, or relative to its base URL).
    Start {
        call: CallId,
        url: String,
        skip_frames: u64,
        equalizer: Option<EqSettings>,
    },
    Pull(CallId),
    Cancel(CallId),
}

#[derive(Clone, Debug)]
pub enum AlertRequest {
    Act(Act),
    SetAlarm(SetAlarm),
    CancelAlarms(CancelAlarms),
    ListAlarms,
}

/// A record for the records service. The runtime hands each to the service as the effect
/// is carried out, so it is written whatever becomes of the session afterwards.
#[derive(Clone, Debug)]
pub enum PersistRequest {
    /// A turn's row with its recordings (the turn recording included, published once the
    /// row has its id). With `fix_follows`, the row waits for the transcript-correction
    /// record of that correction call, which the runtime routes to the records service.
    Turn {
        record: Box<TurnRecord>,
        audio: TurnAudio,
        fix_follows: Option<CallId>,
    },
    NearMiss {
        record: Box<NearMissRecord>,
        audio: NearMissAudio,
    },
    Continuation {
        turn_uuid: Uuid,
        end: ContinuationEnd,
    },
}

/// What the machine asks of the device's ESPHome voice satellite.
#[derive(Clone, Debug)]
pub enum SatelliteCommand {
    VoiceState(Option<VoiceState>),
    Media(Option<MediaState>),
    AlertRinging(Option<bool>),
    ButtonEvent(ButtonEvent),
    FinishAnnouncement {
        id: AnnouncementId,
        success: bool,
    },
    /// Starts an HA-side run (a reply to an HA-started conversation).
    StartRun {
        call: CallId,
        conversation: Option<String>,
    },
    RunAudio {
        run: RunId,
        pcm: Arc<[i16]>,
    },
    EndRunAudio(RunId),
    AbortRun(RunId),
    FinishRunResponse {
        run: RunId,
        success: bool,
    },
}

/// Something the machine asks the device runtime to do.
#[derive(Debug)]
pub enum Effect {
    /// Send a message the device does not acknowledge.
    Send {
        body: Outbound,
        generation: u32,
    },
    /// Send a message the device acknowledges; answer with [`Input::Ack`] quoting `id`.
    Request {
        id: MessageId,
        body: Outbound,
        generation: u32,
    },
    /// Queue a render frame, in order, waiting for room; frames whose playback token is
    /// cancelled are dropped unwritten.
    RenderFrame(RenderFrame),
    /// The device broke the stream rules: report and close the session.
    Violation(ProtocolError),
    /// Run a job on a speech worker under `scope`; answer with [`Input::Mic`] etc.
    Mic {
        scope: WorkScope,
        key: Option<StreamKey>,
        job: MicJob,
    },
    Reference {
        scope: WorkScope,
        key: Option<StreamKey>,
        job: ReferenceJob,
    },
    Verify {
        scope: WorkScope,
        job: VerifyJob,
    },
    Span {
        scope: WorkScope,
        turn: Uuid,
        job: SpanJob,
    },
    Ha {
        call: CallId,
        scope: WorkScope,
        request: HaRequest,
    },
    Decode(DecodeCommand),
    Alert {
        call: CallId,
        request: AlertRequest,
    },
    Persist {
        call: CallId,
        request: PersistRequest,
    },
    Satellite(SatelliteCommand),
    /// The answer to [`Input::Preview`] or [`Input::StopPreview`].
    PreviewAnswer {
        id: PreviewId,
        outcome: PreviewOutcome,
    },
    /// Audio, or the end, of the recording modes' subscription.
    Diagnostic(crate::diagnostic::DiagnosticAudio),
    /// Correct a turn's transcript (§4.7); answer with [`Input::Corrected`] quoting `call`.
    Correct {
        call: CallId,
        request: CorrectionRequest,
    },
    /// Decision evidence for the session recorder, queued only while
    /// [`crate::Voice::set_recording`] says one runs; `at` is on the machine's clock
    /// ([`crate::Voice::clock_origin`]).
    Evidence {
        at: echomuse_turn::MonoInstant,
        evidence: Box<crate::evidence::Evidence>,
    },
    /// Transcribe audio on the pause server (`pauseAsr`), within the request's limit;
    /// answer with [`Input::ServerTranscript`] quoting `call` and `through`, the sample the
    /// audio ends at.
    ServerTranscribe {
        call: CallId,
        through: echomuse_turn::Sample,
        request: crate::pause_server::ServerRequest,
    },
}

/// A turn's final transcript, for transcript correction (§4.7).
#[derive(Clone, Debug)]
pub struct CorrectionRequest {
    /// The turn's scope: a turn that ended withdraws its interest in the check.
    pub scope: WorkScope,
    /// The transcript as heard, wake phrase removed.
    pub heard: String,
    /// On a wake turn, the wake phrase (its leftovers at the start may be removed).
    pub wake_phrase: Option<String>,
    /// A follow-up in a conversation: never corrected.
    pub contextual: bool,
    /// The settings the turn opened with.
    pub settings: echomuse_settings::settings::TranscriptSettings,
    /// The satellite and pipeline the turn runs in.
    pub target: SatelliteTarget,
}

/// What transcript correction decided: the text to send. `record` is the decision for
/// `turns.stt_fix` (`None` when correction is off); when `record_follows`, the text
/// cannot change and the check runs on beside the turn: its record goes straight to the
/// records service with the turn's row ([`PersistRequest::Turn`]'s `fix_follows`).
#[derive(Clone, Debug, PartialEq)]
pub struct Correction {
    pub send: String,
    pub applied: bool,
    pub record: Option<echomuse_records::SttFix>,
    pub record_follows: bool,
}

/// Names one preview request; chosen by whoever asks, quoted in its answer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PreviewId(pub u64);

/// What became of a preview request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewOutcome {
    /// The device accepted the `render.start` and plays the sound.
    Started,
    /// A stop request: whether a preview was playing (and was cancelled).
    Stopped { was_playing: bool },
    /// The device has no local playback (`render_progress`).
    Unsupported,
    /// The device refused the `render.start`, with its code when it gave one.
    Rejected(Option<String>),
    /// No acknowledgement came back (it timed out, or the session ended meanwhile): the
    /// sound may or may not be playing.
    Unknown,
    /// The session ended before the request could be sent.
    SessionEnded,
}
