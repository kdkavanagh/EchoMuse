//! Turn rows (spec §4.11, §3.5.1): what a finished turn records, built from its
//! decision traces and the facts gathered along the way. Unknown values stay NULL.

use std::time::Duration;

use echomuse_core::services::ConversationId;
use echomuse_protocol::StreamId;
use echomuse_records::{
    IntentColumns, MappingError, PolicyHash, ResponseType, TurnAudio, TurnRecord, WallSeconds,
    vocab::{Continuation, PlaybackReason, TerminalReason, TurnOutcome},
};
use echomuse_settings::values::PauseAsr;
use echomuse_turn::{Samples, UtteranceTrace};
use echomuse_uplink::buffer::Span;
use tokio::time::Instant;

use crate::{io::Now, machine::Voice, turn::Turn};

/// The endpoint policy every turn of this controller runs (the turn crate's default),
/// stored as each turn row's `policy_hash`.
pub const POLICY: &str = "post_afe_10";

/// The policy's identity, and for an utterance whose pauses also went to a pause server,
/// that server's model (`+wyoming:<model>`, `default` for the server's own): a decision
/// made on its words depends on it (§4.7).
pub(crate) fn policy_hash(utterance: Option<(&UtteranceTrace, &PauseAsr)>) -> PolicyHash {
    let hash = match utterance {
        Some((trace, server)) if trace.config.pause_server => {
            let model = match server.model() {
                "" => "default",
                model => model,
            };
            format!("{POLICY}+wyoming:{model}")
        }
        _ => POLICY.to_owned(),
    };
    PolicyHash::new(hash).expect("the policy name is not empty")
}

pub(crate) fn wall_seconds(wall_ms: i64) -> WallSeconds {
    WallSeconds::new(wall_ms.max(0) as f64 / 1000.0)
        .or_else(|| WallSeconds::new(0.0))
        .expect("zero is a valid wall time")
}

/// What happened during a turn, as it happened.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    /// How the turn ended, when something decided it before its end.
    pub(crate) terminal: Option<TerminalReason>,
    pub(crate) outcome: Option<TurnOutcome>,
    pub(crate) stt_raw: Option<String>,
    pub(crate) stt_text: Option<String>,
    pub(crate) stt_started: Option<Instant>,
    pub(crate) stt_done: Option<Instant>,
    pub(crate) intent_started: Option<Instant>,
    pub(crate) intent_ended: Option<Instant>,
    /// Whether HA's local agent answered (its `processed_locally`); `None` when HA did not
    /// say, or no intent run answered.
    pub(crate) intent_local: Option<bool>,
    pub(crate) response_type: Option<String>,
    pub(crate) response_text: Option<String>,
    pub(crate) conversation: Option<ConversationId>,
    pub(crate) tts_url_at: Option<Instant>,
    pub(crate) audible_at: Option<Instant>,
    pub(crate) playback_ended: Option<Instant>,
    pub(crate) playback_reason: Option<PlaybackReason>,
    /// An intent run may have reached HA when the turn was revoked.
    pub(crate) unknown_outcome: bool,
    /// The transcript correction's record (`turns.stt_fix`).
    pub(crate) stt_fix: Option<echomuse_records::SttFix>,
    /// A correction check still runs beside the turn; its record goes to the records
    /// service under this call.
    pub(crate) fix_pending: Option<crate::io::CallId>,
    /// The turn's native AFE series (`turns.afe_series`).
    pub(crate) afe_series: Option<echomuse_records::AfeSeries>,
    /// Its evidence summaries (`turns.afe_evidence`); `None` outside an `afe_metadata`
    /// session, or until the turn is charted.
    pub(crate) afe_evidence: Option<echomuse_records::AfeEvidence>,
    /// The turn recording, published by the records service once the row has its id.
    pub(crate) turn_recording: Option<echomuse_records::Pcm16>,
    /// The device time of the commit boundary (the end of the last word), through the mic
    /// stream's clock; `None` without device timing.
    pub(crate) boundary_mono_ns: Option<u64>,
    /// The device time the answer's first frame played: the first `render.progress`
    /// reporting completed frames, at its `mono_ns` less those frames.
    pub(crate) first_frame_mono_ns: Option<u64>,
}

fn between(from: Option<Instant>, to: Option<Instant>) -> Option<Duration> {
    Some(to?.saturating_duration_since(from?))
}

/// The row and saved audio of a finished turn.
pub(crate) fn turn_record(
    voice: &Voice,
    turn: &Turn,
    reason: TerminalReason,
    continuation: Option<Continuation>,
    now: Now,
) -> Result<(TurnRecord, TurnAudio), MappingError> {
    let wake = turn
        .wake
        .as_ref()
        .map(|w| (&w.trace, voice.wake_context(&w.message, w.end_peak)));
    let commit_id = turn.commit.as_ref().map(|(_, id, _)| *id);
    let committed_at = turn.commit.as_ref().map(|(_, _, at)| *at);
    let utterance = turn.utterance.as_ref().map(|u| (u, commit_id));
    let mut record = TurnRecord::turn(
        voice.device.clone(),
        wall_seconds(turn.opened.wall_ms),
        turn.uuid,
        turn.trigger,
        wake,
        utterance,
        policy_hash(
            turn.utterance
                .as_ref()
                .map(|u| (u, &turn.settings.microphones.pause_asr)),
        ),
        reason,
        now.at.saturating_duration_since(turn.opened.at),
    )?;
    let facts = &turn.facts;
    record.outcome = facts.outcome;
    record.stt_raw = facts.stt_raw.clone();
    record.stt_text = facts.stt_text.clone();
    record.stt = between(facts.stt_started, facts.stt_done);
    record.intent =
        between(facts.intent_started, facts.intent_ended).map(|elapsed| IntentColumns {
            elapsed,
            local: facts.intent_local,
        });
    record.response_type = facts.response_type.clone().and_then(ResponseType::new);
    record.response_text = facts.response_text.clone();
    record.conversation_id = facts.conversation.clone();
    record.tts_url = between(committed_at, facts.tts_url_at);
    record.playback = between(facts.audible_at, facts.playback_ended);
    record.playback_reason = facts.playback_reason;
    record.first_audio = between(committed_at, facts.audible_at);
    // Both ends on the device's clock (post-afe-audio-architecture.md §11.3), so transport
    // latency enters neither; NULL when either end lacked device timing.
    record.response_latency = facts
        .boundary_mono_ns
        .zip(facts.first_frame_mono_ns)
        .filter(|(boundary, first)| first >= boundary)
        .map(|(boundary, first)| Duration::from_nanos(first - boundary));
    record.set_trace_details(
        turn.utterance.as_ref(),
        &echomuse_records::TraceDetails {
            verification: turn
                .wake
                .as_ref()
                .map(|w| w.verification)
                .unwrap_or_default(),
        },
    );
    record.reply_to = turn.reply_to;
    record.continuation = continuation;
    record.stt_fix = facts.stt_fix.clone();
    record.afe_series = facts.afe_series.clone();
    if let Some(evidence) = &facts.afe_evidence {
        record.set_afe_evidence(evidence.clone());
    }
    let mut audio = TurnAudio {
        turn_recording: facts.turn_recording.clone(),
        ..TurnAudio::default()
    };
    if turn.settings.microphones.save_utterances
        && let Some(copy) = &turn.stt_copy
    {
        audio.utterance = Some(copy.pcm.clone());
    }
    if turn.settings.wakeword.save_wake_clips
        && let Some(wake) = &turn.wake
    {
        // From 300 ms before the wake word to the end of its support (or, before the
        // device reported that, the verifier's lookahead).
        let start = wake.message.candidate.support_start - Samples(4_800);
        let end = wake.support_end.unwrap_or_else(|| {
            wake.message
                .candidate
                .open()
                .unwrap_or(wake.message.candidate.support_start)
                + Samples::from_ms(480)
        });
        let mut pcm = Vec::new();
        if voice
            .uplink
            .timeline()
            .read_pcm(
                StreamId::Mic,
                wake.message.capture_epoch,
                Span {
                    start: start.0,
                    end: end.0,
                },
                &mut pcm,
            )
            .is_ok()
        {
            audio.wake_clip = Some(pcm.into());
        }
    }
    Ok((record, audio))
}
