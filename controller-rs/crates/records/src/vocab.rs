//! The closed text vocabularies the record tables store, spelled exactly as the existing rows
//! spell them (spec §3.5.1 rule 5).
//!
//! Each spelling comes from the real rows of a deployed database at `schema_version` 40, the
//! dashboard that reads them (`controller/static/dashboard.jsx`, whose constants name each
//! column's values), the firmware's wire vocabularies (`device/internal`) and the design docs
//! (post-AFE architecture §7 for terminal reasons). Older rows hold values from before the
//! post-AFE cutover (`ok`, `wakeword(0.102)`, ...); reading keeps those as [`Stored::Other`]
//! rather than failing.

use std::fmt;

use echomuse_turn::{CloseReason, Route, TextClass, WakeProfile, WakeRefusal};

/// Declares a fieldless enum whose variants map one to one onto stored spellings, with
/// `as_str`, `parse`, `ALL` and `Display`.
macro_rules! stored_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        $vis enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant,)+];

            /// The exact stored spelling.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $text, )+
                }
            }

            /// Parses the exact stored spelling; anything else is `None`.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $( $text => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

/// A stored text value as read back: one this controller writes, or another spelling an older
/// controller wrote, kept as it is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Stored<T> {
    /// A value of the current vocabulary.
    Known(T),
    /// Any other stored text.
    Other(String),
}

impl<T> Stored<T> {
    /// Reads `text` with `parse`, keeping an unknown spelling.
    pub fn read(text: String, parse: impl FnOnce(&str) -> Option<T>) -> Self {
        match parse(&text) {
            Some(value) => Self::Known(value),
            None => Self::Other(text),
        }
    }

    /// The known value, if it is one.
    pub fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}

stored_enum! {
    /// `turns.trigger_type`: what opened the turn.
    pub enum TriggerType {
        /// An accepted wake word, or a refused wake candidate.
        Wake => "wake",
        /// The action button.
        Button => "button",
        /// A reply to the controller's own question or HA's `continue_conversation`.
        Reply => "reply",
        /// A reply to a conversation Home Assistant started.
        HaReply => "ha_reply",
    }
}

stored_enum! {
    /// `turns.terminal_reason`: how the turn ended. Also a resolved `turns.continuation`
    /// when the reply expectation ended this way.
    pub enum TerminalReason {
        /// The turn ran to its end.
        Completed => "completed",
        /// A newer wake or button press ended it.
        Superseded => "superseded",
        /// No reply in the reply window (a continuation only).
        ReplyTimeout => "reply_timeout",
        /// Audio went missing or the speech path failed.
        Interrupted => "interrupted",
        /// No command speech.
        NoInput => "no_input",
        /// The device was muted.
        Muted => "muted",
        /// The device session was lost.
        SessionLost => "session_lost",
        /// Another device won the wake.
        ArbitrationLost => "arbitration_lost",
        /// No wake verification verdict by the deadline.
        VerifierTimeout => "verifier_timeout",
        /// The verifier did not hear the wake word.
        UnverifiedWake => "unverified_wake",
        /// The device itself said something wake-like.
        SelfOutput => "self_output",
        /// The mic held only the device's own output.
        EchoOnly => "echo_only",
        /// HA's STT returned no text.
        EmptyTranscript => "empty_transcript",
        /// No stable progress and no complete text to commit.
        Retry => "retry",
        /// Longer than the utterance limit.
        TooLong => "too_long",
        /// The audio transport overran.
        AudioOverrun => "audio_overrun",
        /// The speech path was unavailable.
        SpeechUnavailable => "speech_unavailable",
        /// HA's intent stage passed its deadline.
        HaTimeout => "ha_timeout",
        /// The response did not start or progress in time.
        ResponseTimeout => "response_timeout",
        /// HA's STT failed after its one retry.
        SttFailed => "stt_failed",
        /// The pipeline reported an `error` event.
        HaError => "ha_error",
        /// HA may or may not have applied the request.
        OutcomeUnknown => "outcome_unknown",
        /// An alert took the foreground.
        AlertPreempted => "alert_preempted",
        /// A wake candidate that broke the message rules. Not in any existing row.
        Malformed => "malformed",
        /// A wake candidate refused while a diagnostic recording was active. Not in any
        /// existing row.
        DiagnosticRecording => "diagnostic_recording",
        /// A wake candidate refused while another was pending. Not in any existing row.
        CandidatePending => "candidate_pending",
    }
}

impl From<CloseReason> for TerminalReason {
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::NoInput => Self::NoInput,
            CloseReason::TooLong => Self::TooLong,
            CloseReason::Retry => Self::Retry,
            CloseReason::Interrupted => Self::Interrupted,
            CloseReason::AudioOverrun => Self::AudioOverrun,
            CloseReason::Muted => Self::Muted,
            CloseReason::SessionLost => Self::SessionLost,
        }
    }
}

stored_enum! {
    /// Why a wake candidate did not become a turn: the turn crate's refusals plus losing
    /// arbitration. Stored as both `turns.wake_attribution` and `turns.terminal_reason` of the
    /// candidate's row, as existing refused rows are.
    pub enum CandidateRefusal {
        /// See [`WakeRefusal::Malformed`].
        Malformed => "malformed",
        /// See [`WakeRefusal::DiagnosticRecording`].
        DiagnosticRecording => "diagnostic_recording",
        /// See [`WakeRefusal::SpeechUnavailable`].
        SpeechUnavailable => "speech_unavailable",
        /// See [`WakeRefusal::Muted`].
        Muted => "muted",
        /// See [`WakeRefusal::CandidatePending`].
        CandidatePending => "candidate_pending",
        /// See [`WakeRefusal::SelfOutput`].
        SelfOutput => "self_output",
        /// See [`WakeRefusal::EchoOnly`].
        EchoOnly => "echo_only",
        /// See [`WakeRefusal::UnverifiedWake`].
        UnverifiedWake => "unverified_wake",
        /// See [`WakeRefusal::VerifierTimeout`].
        VerifierTimeout => "verifier_timeout",
        /// The candidate was accepted here but another device won the arbitration.
        ArbitrationLost => "arbitration_lost",
    }
}

impl CandidateRefusal {
    /// The terminal reason with the same spelling.
    pub const fn terminal_reason(self) -> TerminalReason {
        match self {
            Self::Malformed => TerminalReason::Malformed,
            Self::DiagnosticRecording => TerminalReason::DiagnosticRecording,
            Self::SpeechUnavailable => TerminalReason::SpeechUnavailable,
            Self::Muted => TerminalReason::Muted,
            Self::CandidatePending => TerminalReason::CandidatePending,
            Self::SelfOutput => TerminalReason::SelfOutput,
            Self::EchoOnly => TerminalReason::EchoOnly,
            Self::UnverifiedWake => TerminalReason::UnverifiedWake,
            Self::VerifierTimeout => TerminalReason::VerifierTimeout,
            Self::ArbitrationLost => TerminalReason::ArbitrationLost,
        }
    }
}

impl From<WakeRefusal> for CandidateRefusal {
    fn from(refusal: WakeRefusal) -> Self {
        match refusal {
            WakeRefusal::Malformed => Self::Malformed,
            WakeRefusal::DiagnosticRecording => Self::DiagnosticRecording,
            WakeRefusal::SpeechUnavailable => Self::SpeechUnavailable,
            WakeRefusal::Muted => Self::Muted,
            WakeRefusal::CandidatePending => Self::CandidatePending,
            WakeRefusal::SelfOutput => Self::SelfOutput,
            WakeRefusal::EchoOnly => Self::EchoOnly,
            WakeRefusal::UnverifiedWake => Self::UnverifiedWake,
            WakeRefusal::VerifierTimeout => Self::VerifierTimeout,
        }
    }
}

/// `turns.wake_attribution`: how a wake was decided. NULL for button and reply turns.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum WakeAttribution {
    /// The device was quiet: accepted on its score (`idle`).
    Idle,
    /// The device was producing sound and the verifier heard the wake word (`verified`).
    Verified,
    /// Refused, stored as the refusal's own spelling.
    Refused(CandidateRefusal),
}

impl WakeAttribution {
    /// The exact stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Verified => "verified",
            Self::Refused(refusal) => refusal.as_str(),
        }
    }

    /// Parses the exact stored spelling; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "idle" => Some(Self::Idle),
            "verified" => Some(Self::Verified),
            other => CandidateRefusal::parse(other).map(Self::Refused),
        }
    }
}

impl fmt::Display for WakeAttribution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

stored_enum! {
    /// `turns.outcome`: who handled the committed request. NULL when nothing was handled.
    pub enum TurnOutcome {
        /// Home Assistant's pipeline.
        Ha => "ha",
        /// A command handled on the controller.
        LocalCommand => "local_command",
        /// An alarm was set or changed.
        Alarm => "alarm",
        /// An alarm question was answered.
        AlarmQuery => "alarm_query",
        /// A timer request.
        Timer => "timer",
        /// The controller asked a clarifying question.
        Clarification => "clarification",
        /// The last answer was repeated.
        Repeat => "repeat",
    }
}

stored_enum! {
    /// `turns.playback_reason`: how the spoken answer ended.
    pub enum PlaybackReason {
        /// Played to the end.
        Drained => "drained",
        /// Cut off.
        Cancelled => "cancelled",
        /// The render failed.
        Failed => "failed",
        /// The device ran out of audio.
        Underrun => "underrun",
        /// A response limit passed.
        ResponseTimeout => "response_timeout",
        /// A newer generation fenced it.
        Fenced => "fenced",
    }
}

stored_enum! {
    /// `turns.commit_route`: the endpoint route that committed the utterance.
    pub enum CommitRoute {
        /// The normal pause.
        A => "A",
        /// A complete command followed by background speech.
        B => "B",
        /// A reply to an HA-started conversation.
        R => "R",
        /// The no-progress fallback with complete text.
        Fallback => "fallback",
    }
}

impl From<Route> for CommitRoute {
    fn from(route: Route) -> Self {
        match route {
            Route::A => Self::A,
            Route::B => Self::B,
            Route::R => Self::R,
            Route::Fallback => Self::Fallback,
        }
    }
}

stored_enum! {
    /// `turns.endpoint_class`: the committed text's completeness class.
    pub enum EndpointClass {
        /// A whole request.
        Complete => "complete",
        /// A whole request that commonly goes on.
        Extendable => "extendable",
        /// Clearly unfinished.
        NeedsMore => "needs_more",
        /// The grammar could not judge it.
        Unknown => "unknown",
    }
}

impl From<TextClass> for EndpointClass {
    fn from(class: TextClass) -> Self {
        match class {
            TextClass::Complete => Self::Complete,
            TextClass::Extendable => Self::Extendable,
            TextClass::NeedsMore => Self::NeedsMore,
            TextClass::Unknown => Self::Unknown,
        }
    }
}

stored_enum! {
    /// What became of a question a turn's answer asked, when the reply expectation did not
    /// end with a terminal reason.
    pub enum FollowUp {
        /// Answered without the wake word.
        Answered => "answered",
        /// A wake word or button press took over, with the question as context.
        Wake => "wake",
        /// The question was cut off, so no reply window opened.
        PromptCancelled => "prompt_cancelled",
        /// The question never finished playing.
        PromptFailed => "prompt_failed",
        /// No reply window: the microphone stream was unavailable.
        NoMic => "no_mic",
        /// No reply window: the reply chain limit was reached.
        ChainLimit => "chain_limit",
    }
}

/// The one final value of `turns.continuation`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContinuationEnd {
    /// A follow-up outcome.
    FollowUp(FollowUp),
    /// The reply expectation ended with this terminal reason.
    Ended(TerminalReason),
}

impl ContinuationEnd {
    /// The exact stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FollowUp(f) => f.as_str(),
            Self::Ended(r) => r.as_str(),
        }
    }

    /// Parses a stored final value; `pending` and anything unknown are `None`.
    pub fn parse(text: &str) -> Option<Self> {
        FollowUp::parse(text)
            .map(Self::FollowUp)
            .or_else(|| TerminalReason::parse(text).map(Self::Ended))
    }
}

/// `turns.continuation`: NULL when the answer asked nothing, `pending` while the reply
/// expectation is open, then exactly one final value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Continuation {
    /// `pending`: the reply window is still open.
    Pending,
    /// The final value.
    Resolved(ContinuationEnd),
}

impl Continuation {
    /// The stored spelling of the pending state.
    pub const PENDING: &'static str = "pending";

    /// The exact stored spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => Self::PENDING,
            Self::Resolved(end) => end.as_str(),
        }
    }

    /// Parses the exact stored spelling; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        if text == Self::PENDING {
            Some(Self::Pending)
        } else {
            ContinuationEnd::parse(text).map(Self::Resolved)
        }
    }
}

/// The wake profile spelling of `near_misses.profile` (the wire's `idle`/`playback`).
pub const fn profile_str(profile: WakeProfile) -> &'static str {
    match profile {
        WakeProfile::Idle => "idle",
        WakeProfile::Playback => "playback",
    }
}

/// Parses `idle` or `playback`.
pub fn parse_profile(text: &str) -> Option<WakeProfile> {
    match text {
        "idle" => Some(WakeProfile::Idle),
        "playback" => Some(WakeProfile::Playback),
        _ => None,
    }
}

stored_enum! {
    /// `near_misses.end_reason`: why the episode closed (`wake.near_miss` `reason`).
    pub enum NearMissEnd {
        /// The score fell back under the floor.
        Below => "below",
        /// A gap in the scored audio.
        Gap => "gap",
        /// The detector was reset.
        Reset => "reset",
        /// The device was muted.
        Mute => "mute",
        /// The capture overran.
        Overrun => "overrun",
    }
}

stored_enum! {
    /// `near_misses.recording`: whether the episode's audio was kept, and why not. Whether a
    /// saved file has since been removed by retention is not stored: readers check the file.
    pub enum NearMissRecording {
        /// The WAV exists (or existed until retention removed it).
        Saved => "saved",
        /// Save wake clips was off.
        Off => "off",
        /// Capture was not permitted (privacy mute, a recording mode).
        NotPermitted => "not_permitted",
        /// The recording failed, or was never published (see the crate docs).
        Failed => "failed",
    }
}

stored_enum! {
    /// `near_misses.verdict`: what someone decided after listening. NULL while unmarked; a
    /// marked near miss and its recording are exempt from retention.
    pub enum NearMissVerdict {
        /// The wake word was said; the device should have woken.
        MissedWake => "missed_wake",
        /// It was not the wake word.
        CorrectlyIgnored => "correctly_ignored",
    }
}

stored_enum! {
    /// `device_logs.level`.
    pub enum LogLevel {
        /// Informational. The only level the firmware's `log` message sends.
        Info => "info",
        /// A warning.
        Warn => "warn",
        /// An error.
        Error => "error",
    }
}

stored_enum! {
    /// `device_logs.source`: who wrote the line.
    pub enum LogSource {
        /// A `log` message from the device.
        Device => "device",
        /// The controller, about the device.
        Controller => "controller",
    }
}

stored_enum! {
    /// `wake_shadow_events.kind`: how a shadow episode no live candidate overlapped resolved.
    pub enum ShadowEventKind {
        /// No live candidate opened in time: a would-be false wake.
        Unmatched => "unmatched",
        /// A live candidate opened in time: likely a missed wake the user repeated.
        Retried => "retried",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip() {
        for v in TerminalReason::ALL {
            assert_eq!(TerminalReason::parse(v.as_str()), Some(*v));
        }
        for v in CandidateRefusal::ALL {
            let a = WakeAttribution::Refused(*v);
            assert_eq!(WakeAttribution::parse(a.as_str()), Some(a));
            assert_eq!(v.terminal_reason().as_str(), v.as_str());
        }
        for v in FollowUp::ALL {
            let c = Continuation::Resolved(ContinuationEnd::FollowUp(*v));
            assert_eq!(Continuation::parse(c.as_str()), Some(c));
        }
        assert_eq!(Continuation::parse("pending"), Some(Continuation::Pending));
        assert_eq!(
            Continuation::parse("reply_timeout"),
            Some(Continuation::Resolved(ContinuationEnd::Ended(
                TerminalReason::ReplyTimeout
            )))
        );
        assert_eq!(TriggerType::parse("wakeword(0.102)"), None);
    }

    #[test]
    fn stored_keeps_unknown_text() {
        let s = Stored::read("ok".to_owned(), TurnOutcome::parse);
        assert_eq!(s, Stored::Other("ok".to_owned()));
        assert_eq!(
            Stored::read("ha".to_owned(), TurnOutcome::parse).known(),
            Some(&TurnOutcome::Ha)
        );
    }
}
