//! Turn rows (spec §4.11, §3.5.1): what a finished turn records, built from its
//! decision traces and the facts gathered along the way, and its native AFE chart and
//! turn recording (afe-metadata.md §2.1). Unknown values stay NULL.

use std::time::Duration;

use echomuse_core::services::ConversationId;
use echomuse_protocol::StreamId;
use echomuse_records::{
    IntentColumns, MappingError, ResponseType, TurnAudio, TurnRecord,
    vocab::{Continuation, PlaybackReason, TerminalReason, TurnOutcome},
};
use echomuse_turn::Samples;
use echomuse_uplink::buffer::Span;
use tokio::time::Instant;

use super::turn::Turn;
use crate::{
    afe::{LEAD, PERIOD, Spans, evidence, series},
    io::Now,
    rows::{policy_hash, wall_seconds},
    session::{ConversationFacts, SessionAudio},
    wake::wake_context,
};

/// What happened during a turn, as it happened.
#[derive(Clone, Debug, Default)]
pub(super) struct Facts {
    /// How the turn ended, when something decided it before its end.
    pub(super) terminal: Option<TerminalReason>,
    pub(super) outcome: Option<TurnOutcome>,
    pub(super) stt_raw: Option<String>,
    pub(super) stt_text: Option<String>,
    pub(super) stt_started: Option<Instant>,
    pub(super) stt_done: Option<Instant>,
    pub(super) intent_started: Option<Instant>,
    pub(super) intent_ended: Option<Instant>,
    /// Whether HA's local agent answered (its `processed_locally`); `None` when HA did not
    /// say, or no intent run answered.
    pub(super) intent_local: Option<bool>,
    pub(super) response_type: Option<String>,
    pub(super) response_text: Option<String>,
    pub(super) conversation: Option<ConversationId>,
    pub(super) tts_url_at: Option<Instant>,
    pub(super) audible_at: Option<Instant>,
    pub(super) playback_ended: Option<Instant>,
    pub(super) playback_reason: Option<PlaybackReason>,
    /// An intent run may have reached HA when the turn was revoked.
    pub(super) unknown_outcome: bool,
    /// The transcript correction's record (`turns.stt_fix`).
    pub(super) stt_fix: Option<echomuse_records::SttFix>,
    /// A correction check still runs beside the turn; its record goes to the records
    /// service under this call.
    pub(super) fix_pending: Option<crate::io::CallId>,
    /// The turn's native AFE series (`turns.afe_series`).
    pub(super) afe_series: Option<echomuse_records::AfeSeries>,
    /// Its evidence summaries (`turns.afe_evidence`); `None` outside an `afe_metadata`
    /// session, or until the turn is charted.
    pub(super) afe_evidence: Option<echomuse_records::AfeEvidence>,
    /// The turn recording, published by the records service once the row has its id.
    pub(super) turn_recording: Option<echomuse_records::Pcm16>,
    /// The device time of the commit boundary (the end of the last word), through the mic
    /// stream's clock; `None` without device timing.
    pub(super) boundary_mono_ns: Option<u64>,
    /// The device time the answer's first frame played: the first `render.progress`
    /// reporting completed frames, at its `mono_ns` less those frames.
    pub(super) first_frame_mono_ns: Option<u64>,
}

fn between(from: Option<Instant>, to: Option<Instant>) -> Option<Duration> {
    Some(to?.saturating_duration_since(from?))
}

/// The row and saved audio of a finished turn.
pub(super) fn turn_record(
    session: &ConversationFacts<'_>,
    recorded: &SessionAudio,
    turn: &Turn,
    reason: TerminalReason,
    continuation: Option<Continuation>,
    now: Now,
) -> Result<(TurnRecord, TurnAudio), MappingError> {
    let wake = turn
        .wake
        .as_ref()
        .map(|w| (&w.trace, wake_context(session.wake, &w.message, w.end_peak)));
    let commit_id = turn.commit.as_ref().map(|(_, id, _)| *id);
    let committed_at = turn.commit.as_ref().map(|(_, _, at)| *at);
    let utterance = turn.utterance.as_ref().map(|u| (u, commit_id));
    let mut record = TurnRecord::turn(
        session.device.clone(),
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
        if recorded
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

/// Builds an ending turn's AFE evidence and series and, while the speaker saves
/// utterances, its turn recording (the canonical mic from the chart's start through what
/// was heard), into its facts. With `response_erased` (a mute or the session's end), the
/// recording stops where the committed subscription's mic ended. Nothing outside an
/// `afe_metadata` session.
pub(super) fn chart_turn(audio: &SessionAudio, turn: &mut Turn, response_erased: bool) {
    if !audio.terms().afe_metadata {
        return;
    }
    let mut records = std::mem::take(&mut turn.afe);
    if let Some(analysis) = &turn.analysis {
        for (at, record) in &analysis.afe {
            records.entry(*at).or_insert(*record);
        }
    }
    let support = turn.wake.as_ref().map(|wake| {
        let start = wake.message.candidate.support_start;
        let end = wake.support_end.unwrap_or_else(|| {
            wake.message.candidate.open().unwrap_or(start) + Samples::from_ms(480)
        });
        (start.0, end.0)
    });
    let epoch = turn.epoch();
    let heard_end = epoch.and_then(|epoch| audio.mic_end(epoch)).map(|s| s.0);
    let utterance = match (&turn.commit, &turn.utterance) {
        (Some((commit, _, _)), _) => Some((commit.span.start.0, commit.span.end.0)),
        (None, Some(trace)) => heard_end.map(|end| (trace.start.0, end.max(trace.start.0))),
        (None, None) => None,
    };
    turn.facts.afe_evidence = Some(evidence(&records, support, utterance));
    let Some(epoch) = epoch else {
        return;
    };
    let start = support
        .map(|(s, _)| s.saturating_sub(LEAD))
        .or(utterance.map(|(s, _)| s))
        .or(records.keys().next().copied());
    let Some(start) = start.map(|s| s / PERIOD * PERIOD) else {
        return;
    };
    let wake_clip = support
        .filter(|_| turn.settings.wakeword.save_wake_clips)
        .map(|(s, e)| (s.saturating_sub(4_800), e));
    let audio_clip =
        utterance.filter(|_| turn.settings.microphones.save_utterances && turn.stt_copy.is_some());
    // The canonical mic from the chart's start, or from the turn's first mic sample when
    // its subscription began later (a candidate's mic starts 300 ms before the support,
    // its chart a second before).
    let mut recording = None;
    let mic_from = turn
        .analysis
        .as_ref()
        .and_then(|a| a.mic_start)
        .map_or(start, |s| s.0.max(start));
    // A mute or the session's end erases the response's part: the recording ends where
    // the committed subscription's mic did.
    let committed_end = turn
        .analysis
        .as_ref()
        .and_then(|a| a.mic_end)
        .map(|s| s.0)
        .filter(|_| response_erased && turn.commit.is_some());
    let recording_end = match (heard_end, committed_end) {
        (Some(heard), Some(committed)) => Some(heard.min(committed)),
        (heard, _) => heard,
    };
    if turn.settings.microphones.save_utterances
        && let Some(end) = recording_end.filter(|end| *end > mic_from)
    {
        let mut pcm = Vec::new();
        if audio
            .timeline()
            .read_pcm(
                StreamId::Mic,
                epoch,
                Span {
                    start: mic_from,
                    end,
                },
                &mut pcm,
            )
            .is_ok()
        {
            turn.facts.turn_recording = Some(pcm.into());
            recording = Some((mic_from, end));
        }
    }
    let spans = Spans {
        support,
        utterance,
        wake_clip,
        audio_clip,
        recording,
        stages: stages(turn),
    };
    let silero = turn.analysis.as_ref().and_then(|a| a.log.vad_track());
    let end = recording.map(|(_, e)| e).or(heard_end).unwrap_or(start);
    turn.facts.afe_series = series(start, end, &records, &spans, silero.as_ref());
    if turn.facts.afe_series.is_none() {
        // No record arrived: no chart, and so no turn recording to place on one.
        turn.facts.turn_recording = None;
    }
}

/// A committed turn's processing stages on the capture timeline (see the `afe` module).
fn stages(turn: &Turn) -> Vec<(&'static str, (u64, u64))> {
    let Some((commit, _, committed_at)) = &turn.commit else {
        return Vec::new();
    };
    let (frontier, committed_at) = (commit.at.0, *committed_at);
    let sample = |at: Option<Instant>| {
        at.map(|at| {
            let after = at.saturating_duration_since(committed_at);
            frontier + u64::try_from(after.as_micros() * 16 / 1_000).unwrap_or(u64::MAX)
        })
    };
    let facts = &turn.facts;
    let candidates = [
        ("endpoint", Some(commit.boundary.0), Some(frontier)),
        ("stt", sample(facts.stt_started), sample(facts.stt_done)),
        (
            "intent",
            sample(facts.intent_started),
            sample(facts.intent_ended),
        ),
        ("tts", sample(facts.tts_url_at), sample(facts.audible_at)),
        (
            "reply",
            sample(facts.audible_at),
            sample(facts.playback_ended),
        ),
    ];
    candidates
        .into_iter()
        .filter_map(|(name, start, end)| match (start, end) {
            (Some(start), Some(end)) if end >= start => Some((name, (start, end))),
            _ => None,
        })
        .collect()
}
