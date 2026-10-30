//! Typed boundaries between a device runtime and the shared services it calls (§5.4).
//!
//! This module holds the Home Assistant contract and the outcome vocabulary shared by
//! services. Alert and turn-persistence contracts live in the crates that own them.
//!
//! Home Assistant methods return `Send` futures and take owned requests, so a device
//! runtime can await them on its own executor. The implementation decides where its
//! work runs; the shared connection does not move every caller's future to that runtime.
//! Return-position `impl Future` avoids boxing these calls. API ports that genuinely
//! need trait objects expose boxed futures directly rather than duplicate their traits.
//!
//! # Freshness, dispatch and outcomes
//!
//! * Requests that act for a turn carry its [`WorkScope`]. An implementation checks the
//!   scope immediately before dispatch and refuses a cancelled one without sending
//!   anything. Results arriving after cancellation are discarded by the caller's identity
//!   check as well; cancelling withdraws interest and never undoes an effect.
//! * Read-only Home Assistant operations (STT, TTS, recognition, status reads) return
//!   [`HaError`] on failure. Only STT may be retried, once, after a transport failure,
//!   because it has no side effects (§3.3.3).
//! * Operations that change state in Home Assistant return [`EffectOutcome`], which keeps
//!   "never sent", "refused", "done" and "unknown" apart. Once a request may have been
//!   sent it is never resent: an unanswered request is [`EffectOutcome::Unknown`].
//! * Database writes succeed only when their transaction has committed ([`PersistError`]).

use std::{collections::BTreeMap, fmt, future::Future, sync::Arc, time::Duration};

use tokio::sync::mpsc;

use crate::types::WorkScope;

// ── Shared vocabulary ────────────────────────────────────────────────────────

/// The why of an operation that was refused before anything was sent or journaled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotDispatched {
    /// The requesting scope was cancelled before dispatch.
    Cancelled,
    /// The service has no connection to send on; nothing was sent.
    Unavailable,
}

impl fmt::Display for NotDispatched {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("request scope was cancelled before dispatch"),
            Self::Unavailable => f.write_str("service unavailable; nothing was sent"),
        }
    }
}

impl std::error::Error for NotDispatched {}

/// Why the outcome of a possibly dispatched state change is not known.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnknownCause {
    /// The connection dropped after the request may have been sent.
    ConnectionLost,
    /// No reply arrived before the deadline.
    Timeout,
    /// The requesting scope was cancelled after dispatch, so nobody waits for the reply.
    Withdrawn,
    /// The receiver answered, but its answer does not say whether the request was applied
    /// (an unreadable reply, or a server error after the handler may have run).
    Inconclusive,
}

/// The result of an operation that can change state outside the controller (§3.3.3).
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub enum EffectOutcome<T> {
    /// Nothing was sent.
    NotDispatched(NotDispatched),
    /// The receiver answered that it refused the request; nothing was applied.
    Rejected { code: String, message: String },
    /// The receiver confirmed it applied the request.
    Applied(T),
    /// The request may have been applied. Report it as unknown; never resend it.
    Unknown(UnknownCause),
}

// ── Home Assistant ───────────────────────────────────────────────────────────

/// Sample rate of the audio an STT run receives: PCM16 mono, the only input format HA's
/// `assist_pipeline/run` accepts without resampling (§4.7).
pub const STT_SAMPLE_RATE_HZ: u32 = 16_000;
/// The run timeout HA is given for an STT-only run. The spec states no figure for STT; this
/// uses its response limit for HA's intent stage (§4.8), which also covers transcribing the
/// longest extended utterance (§4.6).
pub const STT_RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// The run timeout HA is given for an intent-to-TTS run: the response limit for HA's
/// intent (§4.8).
pub const INTENT_RUN_TIMEOUT: Duration = Duration::from_secs(30);
/// The run timeout HA is given for a TTS-only run. The audio cannot start before its URL
/// arrives, so this is the limit for the answer's audio to start (§4.8).
pub const TTS_RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// Reply deadline of ordinary commands and REST calls. The spec states none; this is a
/// design choice, equal to the spec's other request/reply wait (the alert relay, §3.3.4).
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
/// Capacity of an intent run's event channel. A conversation agent that streams its answer
/// sends one `intent-progress` per piece of text, so the bound leaves room for a long
/// answer arriving faster than it is read.
pub const RUN_EVENT_CAPACITY: usize = 256;

/// A Home Assistant device registry id (the satellite's HA device), not a
/// [`DeviceId`](crate::types::DeviceId).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct HaDeviceId(pub String);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PipelineId(pub String);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ConversationId(pub String);

/// An absolute media URL HA returned (a TTS proxy URL). Fetch it promptly: these expire.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MediaUrl(pub String);

/// Changes each time the HA connection is re-established. A run belongs to the
/// generation it was dispatched on; events of an older generation are never delivered.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HaConnectionGeneration(pub u64);

/// Which satellite and pipeline a run is for (§3.3.3 "Device and pipeline").
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SatelliteTarget {
    pub pipeline: PipelineId,
    /// None when the satellite is not (yet) found in HA's device registry.
    pub ha_device: Option<HaDeviceId>,
}

/// Failure of an operation without side effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HaError {
    NotDispatched(NotDispatched),
    /// The transport failed or the connection was lost while waiting.
    ConnectionLost,
    /// HA answered with an error.
    Rejected {
        code: String,
        message: String,
    },
    /// HA did not answer before the deadline.
    Timeout,
    /// HA answered with something this controller cannot read.
    BadResponse(String),
}

impl fmt::Display for HaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDispatched(reason) => reason.fmt(f),
            Self::ConnectionLost => f.write_str("Home Assistant connection lost"),
            Self::Rejected { code, message } => {
                write!(f, "Home Assistant refused: {code}: {message}")
            }
            Self::Timeout => f.write_str("Home Assistant did not answer in time"),
            Self::BadResponse(detail) => write!(f, "unreadable Home Assistant answer: {detail}"),
        }
    }
}

impl std::error::Error for HaError {}

/// Transcribe exactly `audio` with HA's segmenter off.
#[derive(Clone, Debug)]
pub struct SttRequest {
    pub scope: WorkScope,
    pub target: SatelliteTarget,
    /// The committed span at [`STT_SAMPLE_RATE_HZ`], gain already applied (§4.7).
    pub audio: Arc<[i16]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Transcript {
    /// HA's `stt_output.text`, trimmed. May be empty.
    pub text: String,
}

/// Run the intent stage on `text`, then TTS.
#[derive(Clone, Debug)]
pub struct IntentRequest {
    pub scope: WorkScope,
    pub target: SatelliteTarget,
    pub text: String,
    /// Continue this HA conversation; None starts a new one.
    pub conversation: Option<ConversationId>,
}

/// A dispatched intent-to-TTS run. It is never resubmitted.
///
/// Events arrive in HA's order on a bounded channel and end with exactly one terminal
/// event ([`RunEvent::is_terminal`]). If the channel closes without a terminal event (the
/// implementation fenced the run because its scope was cancelled, its connection
/// generation ended, or the receiver fell [`RUN_EVENT_CAPACITY`] events behind), the
/// outcome is unknown, exactly as for [`RunEvent::Lost`]. Dropping the receiver abandons
/// the run: later events are discarded. Neither undoes what HA may already have done.
#[derive(Debug)]
pub struct IntentRun {
    pub generation: HaConnectionGeneration,
    pub events: mpsc::Receiver<RunEvent>,
}

/// `intent-end`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentEnd {
    /// The plain speech, else the SSML speech, else empty.
    pub speech: String,
    pub conversation: Option<ConversationId>,
    pub continue_conversation: bool,
    /// HA's `response_type` as sent (for example `action_done` or `error`).
    pub response_type: Option<String>,
    /// True when HA's built-in agent answered, false when the pipeline's conversation
    /// agent did; None when HA did not say.
    pub processed_locally: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunEvent {
    /// The next piece of answer text while the conversation agent streams it.
    AnswerDelta(String),
    IntentEnded(IntentEnd),
    /// The response audio. `streamed`: the run-start URL, playable while the agent is
    /// still generating; otherwise the `tts-end` URL. Sent at most once per run.
    TtsReady {
        url: MediaUrl,
        streamed: bool,
    },
    /// HA refused to start the run; nothing executed. Terminal.
    Rejected {
        code: String,
        message: String,
    },
    /// The pipeline's `error` event. Terminal.
    Failed {
        code: String,
        message: String,
    },
    /// `run-end`. Terminal.
    Ended,
    /// The connection dropped before the run ended; its outcome is unknown. Terminal.
    Lost,
}

impl RunEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Rejected { .. } | Self::Failed { .. } | Self::Ended | Self::Lost
        )
    }
}

/// Synthesize one of the controller's own lines with the pipeline's engine and voice.
#[derive(Clone, Debug)]
pub struct TtsRequest {
    pub scope: WorkScope,
    pub target: SatelliteTarget,
    pub text: String,
}

/// Run HA's built-in sentence matcher on each sentence without executing anything
/// (`conversation/agent/homeassistant/debug`). A matching sentence trigger is reported,
/// never fired.
#[derive(Clone, Debug)]
pub struct RecognizeRequest {
    pub scope: WorkScope,
    pub ha_device: Option<HaDeviceId>,
    pub sentences: Vec<String>,
    /// None: HA's configured language.
    pub language: Option<String>,
}

/// One filled slot: HA's resolved value and the words of the sentence it matched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotValue {
    /// A string value as HA sent it; a number or other value as its JSON text.
    pub value: String,
    pub text: String,
}

/// HA's recognition of one sentence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SentenceRecognition {
    /// A sentence trigger, or an intent with every slot filled.
    pub matched: bool,
    /// The closest intent left slots unmatched.
    pub unfilled_slots: bool,
    /// HA's `source` as sent: `trigger`, `builtin` or `custom`.
    pub source: Option<String>,
    /// The sentence template that matched, or came closest.
    pub template: String,
    /// For an intent, never a sentence trigger: its name.
    pub intent: Option<String>,
    pub details: BTreeMap<String, SlotValue>,
    /// The words each unfilled slot was left with.
    pub unmatched: BTreeMap<String, String>,
    /// The entities HA would target (the keys of its `targets`), sorted. Empty unless every
    /// slot was filled.
    pub targets: Vec<String>,
}

/// Which HA timer a `HassCancelTimer` picks: the start units the timer was given (HA keeps
/// only those) and its name. Choosing slots that pick exactly one timer is the caller's job.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimerSelector {
    pub name: Option<String>,
    pub start_hours: Option<u32>,
    pub start_minutes: Option<u32>,
    pub start_seconds: Option<u32>,
}

/// A state-changing intent run directly through `POST /api/intent/handle`, not through a
/// conversation agent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectIntent {
    /// `HassCancelTimer`.
    CancelTimer(TimerSelector),
    /// `HassCancelAllTimers`: every timer in HA, not only this speaker's.
    CancelAllTimers,
}

#[derive(Clone, Debug)]
pub struct DirectIntentRequest {
    pub scope: WorkScope,
    pub ha_device: Option<HaDeviceId>,
    pub intent: DirectIntent,
}

/// One timer of `HassTimerStatus` `speech_slots.timers`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HaTimer {
    pub id: String,
    /// None when HA reports no name (it sends an empty string).
    pub name: Option<String>,
    /// The HA device that started it; None when HA reports none (an empty string).
    pub ha_device: Option<HaDeviceId>,
    /// The start units the timer was given; HA reports a unit it was not given as 0.
    pub start_hours: u32,
    pub start_minutes: u32,
    pub start_seconds: u32,
    pub is_active: bool,
}

/// HA's answer to `HassTimerStatus`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerStatus {
    /// Every timer in HA, whichever device started it.
    pub timers: Vec<HaTimer>,
    /// HA's `response_type` as sent (`action_done`).
    pub response_type: String,
}

/// What happened to one of a satellite's Home Assistant timers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TimerEventKind {
    /// The timer was set.
    Started,
    /// It was changed or paused.
    Updated,
    /// It was cancelled.
    Cancelled,
    /// It ran out.
    Finished,
}

/// A timer event from Home Assistant, which owns the satellite's timers. Home Assistant
/// sends these to the satellite over the ESPHome API.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimerEvent {
    /// What happened.
    pub kind: TimerEventKind,
    /// Home Assistant's timer ID.
    pub timer_id: String,
    /// The timer's name, if it has one.
    pub name: Option<String>,
    /// The timer's full length in seconds.
    pub total_seconds: u32,
    /// Seconds left.
    pub seconds_left: u32,
    /// Whether it is counting down (not paused).
    pub is_active: bool,
}

/// The Home Assistant session as a device runtime uses it. One implementation multiplexes
/// every device's requests over one connection.
pub trait HomeAssistant: Send + Sync + 'static {
    /// The pipeline the satellite's pipeline select chooses, by HA's own rule. None, for a
    /// satellite HA's device registry does not know, gives HA's preferred pipeline.
    fn pipeline_for(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> impl Future<Output = Result<PipelineId, HaError>> + Send;

    /// STT-only run. One retry after a transport failure is allowed and is the
    /// implementation's, not the caller's.
    fn run_stt(
        &self,
        request: SttRequest,
    ) -> impl Future<Output = Result<Transcript, HaError>> + Send;

    /// Dispatch an intent-to-TTS run. Fails only when nothing was sent; once dispatched,
    /// every outcome, loss included, arrives on [`IntentRun::events`].
    fn run_intent_tts(
        &self,
        request: IntentRequest,
    ) -> impl Future<Output = Result<IntentRun, NotDispatched>> + Send;

    /// TTS-only run; resolves to the absolute audio URL.
    fn run_tts(
        &self,
        request: TtsRequest,
    ) -> impl Future<Output = Result<MediaUrl, HaError>> + Send;

    /// One result per sentence, in order; None where nothing was recognized.
    fn recognize(
        &self,
        request: RecognizeRequest,
    ) -> impl Future<Output = Result<Vec<Option<SentenceRecognition>>, HaError>> + Send;

    /// HA's running timers (`HassTimerStatus`, which changes nothing).
    fn timer_status(
        &self,
        scope: WorkScope,
        ha_device: Option<HaDeviceId>,
    ) -> impl Future<Output = Result<TimerStatus, HaError>> + Send;

    /// Run a state-changing intent directly. Never resent once it may have been sent.
    fn call_intent(
        &self,
        request: DirectIntentRequest,
    ) -> impl Future<Output = EffectOutcome<()>> + Send;
}

// ── Database outcomes ────────────────────────────────────────────────────────

/// Why a database write did not report a commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersistError {
    /// The database service is not accepting work; nothing was written.
    Closed,
    /// The transaction failed and rolled back; nothing was written.
    Failed(String),
    /// The command was accepted but the writer stopped before reporting; it may or may
    /// not have committed.
    Unknown,
}

impl fmt::Display for PersistError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => f.write_str("database service is closed"),
            Self::Failed(detail) => write!(f, "transaction rolled back: {detail}"),
            Self::Unknown => f.write_str("database writer stopped before reporting"),
        }
    }
}

impl std::error::Error for PersistError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_run_events_are_exactly_the_four_ends() {
        let terminal = [
            RunEvent::Rejected {
                code: "c".into(),
                message: "m".into(),
            },
            RunEvent::Failed {
                code: "c".into(),
                message: "m".into(),
            },
            RunEvent::Ended,
            RunEvent::Lost,
        ];
        assert!(terminal.iter().all(RunEvent::is_terminal));
        let ongoing = [
            RunEvent::AnswerDelta("It is".into()),
            RunEvent::TtsReady {
                url: MediaUrl("http://ha/tts.mp3".into()),
                streamed: true,
            },
            RunEvent::IntentEnded(IntentEnd {
                speech: "It is sunny".into(),
                conversation: None,
                continue_conversation: false,
                response_type: Some("query_answer".into()),
                processed_locally: Some(true),
            }),
        ];
        assert!(!ongoing.iter().any(RunEvent::is_terminal));
    }
}
