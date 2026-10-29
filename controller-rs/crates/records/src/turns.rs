//! `turns`: one row per voice turn and per refused wake candidate (§4.11 "Decision traces
//! are kept for every turn and every refused wake").
//!
//! # Column encodings
//!
//! Read from the rows of a deployed database at schema 40 and from the dashboard that reads
//! them. "Turn crate" names where [`TurnRecord::turn`] and [`TurnRecord::candidate`] take a
//! value from its traces; everything else the device runtime supplies.
//!
//! | Column | Type | Encoding | NULL means |
//! |---|---|---|---|
//! | `id` | INTEGER | row id, never reused (AUTOINCREMENT); recordings are named by it | — |
//! | `device_id` | TEXT | the device serial | never NULL |
//! | `ts` | REAL | wall seconds since the epoch when the turn (or candidate) opened | never NULL |
//! | `trigger_type` | TEXT | [`TriggerType`] (turn crate: the utterance's kind; `wake` for a candidate) | never written NULL |
//! | `wake_model` | TEXT | the registry wake phrase (`ophelia`) | not a wake, or the graph is not in the registry |
//! | `wake_score` | REAL | the candidate's peak 3-window smoothed score, 0–1 (turn crate: the hops, raised by `wake.candidate_end`'s `peak_smoothed`) | not a wake, or no scored hop |
//! | `wake_threshold` | REAL | the threshold of the rule that opened the candidate (turn crate) | not a wake, or a malformed threshold |
//! | `wake_model_sha256` | TEXT | the scoring graph's SHA-256, 64 lowercase hex | not a wake |
//! | `wake_attribution` | TEXT | [`WakeAttribution`] (turn crate: the wake rule; `arbitration_lost` from the arbiter) | not a wake |
//! | `outcome` | TEXT | [`TurnOutcome`] | nothing was handled |
//! | `asr_text` | TEXT | the streaming recognizer's latest raw text, wake word included (turn crate) | no utterance, or nothing heard |
//! | `reference_coverage` | REAL | share of the utterance's cells with full playback-reference coverage, 0–1 (turn crate) | no utterance, or no cell classified |
//! | `commit_route` | TEXT | [`CommitRoute`] (turn crate) | not committed |
//! | `commit_id` | TEXT | the commit's UUID, hyphenated lowercase | not committed |
//! | `endpoint_class` | TEXT | [`EndpointClass`] of the committed text (turn crate) | not committed, or the text was never classed |
//! | `endpoint_ms` | INTEGER | sample-time ms from the end of the command to the commit decision (turn crate) | not committed |
//! | `audio_ms` | INTEGER | ms of committed audio sent to STT (turn crate: the commit span) | not committed |
//! | `stt_raw` | TEXT | HA's STT transcript as returned | STT did not run or failed |
//! | `stt_text` | TEXT | the transcript with the wake phrase removed (for a local command, the text it acted on) | no transcript |
//! | `stt_ms` | INTEGER | wall ms of HA's STT run | STT did not run |
//! | `stt_fix` | TEXT | [`SttFix`] JSON | correction off, or the turn ended first |
//! | `intent_ms` | INTEGER | wall ms from sending the intent request (an intent run, or the timer list a timer cancel reads) to its answer | no intent request |
//! | `intent_local` | INTEGER | 1 when HA's local agent answered, 0 when the conversation agent did | not reported (the controller's own commands) |
//! | `response_type` | TEXT | HA's `response_type` as sent | no intent answer |
//! | `response_text` | TEXT | the spoken answer's text | none |
//! | `conversation_id` | TEXT | HA's conversation id | none |
//! | `tts_url_ms` | INTEGER | wall ms from sending the run to the answer's audio URL | no audio URL |
//! | `playback_ms` | INTEGER | wall ms the answer played | nothing played |
//! | `playback_reason` | TEXT | [`PlaybackReason`] | nothing played |
//! | `first_audio_ms` | INTEGER | controller-monotonic ms from handling the commit to the answer becoming audible | nothing became audible |
//! | `response_latency_ms` | INTEGER | device-clock ms from the commit boundary to the answer's first played frame | nothing played, or either end lacked device timing |
//! | `total_ms` | INTEGER | wall ms from the turn opening to its end | never written NULL |
//! | `policy_hash` | TEXT | [`PolicyHash`] | never written NULL |
//! | `terminal_reason` | TEXT | [`TerminalReason`] | never written NULL |
//! | `turn_uuid` | TEXT | the turn's UUID, hyphenated lowercase; the turn's operation key | a refused candidate (its key is the trace's `candidate_id`) |
//! | `reply_to` | TEXT | `turn_uuid` of the turn whose question this answers | not a reply |
//! | `continuation` | TEXT | [`Continuation`]: `pending`, then one final value | the answer asked nothing |
//! | `decision_trace` | TEXT | compact JSON ([`DecisionTrace`]) | older than the newest 1,000 rows of the device |
//! | `afe_evidence` | TEXT | [`AfeEvidence`] JSON | no AFE metadata |
//! | `afe_series` | TEXT | [`AfeSeries`] JSON | no AFE metadata, or older than the newest 1,000 rows |
//! | `wake_windows` | TEXT | [`ClipWindows`] JSON of the wake clip | no wake clip, or not scored yet |
//! | `audio_file` | TEXT | `<device>_<id>.wav` in `recordings/`, set once the file exists | not saved; kept after retention removes the file |
//! | `wake_file` | TEXT | `<id>.wav` in `wakes/<device>/`, set once the file exists | not saved; kept after retention removes the file |
//!
//! The turn recording (`turn_recordings/<device>_<id>.wav`) has no column: it is saved
//! exactly when its file exists.
//!
//! Columns from before the post-AFE cutover are never written, so new rows hold NULL there
//! and old rows keep their history: `noise_floor`, `vad_end_ms`, `tts_fetch_ms`, `tts_bytes`,
//! `underruns`, `playback_periods`, `min_depth`, `prime_wait_ms`, `recv_span_ms`,
//! `max_gap_ms`, `bytes_recv`, `send_ms`, `delivery_ms`, `eq_ms`, `dev_wake_score`,
//! `dev_wake_delta_ms`, `dev_shadow`, `dev_threshold`, `ctrl_wake_score`,
//! `ctrl_wake_delta_ms`.

use std::time::Duration;

use echomuse_core::{services::ConversationId, types::DeviceId};
use echomuse_db::{ReadQuery, WriteCommand};
use echomuse_protocol::vocab::Sha256;
use echomuse_turn::{UtteranceTrace, WakeOutcome, WakeTrace, trace::EndpointEvent, wake::WakeRule};
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};
use uuid::Uuid;

use crate::{
    error::{MappingError, RecordError},
    trace::{DecisionTrace, TraceParts, trigger_of},
    values::{
        AfeEvidence, AfeSeries, CandidateId, ClipWindows, PolicyHash, ReferenceCoverage,
        ResponseType, Score, SttFix, WakeModelName, WallSeconds, whole_ms,
    },
    vocab::{
        CandidateRefusal, CommitRoute, Continuation, ContinuationEnd, EndpointClass,
        PlaybackReason, Stored, TerminalReason, TriggerType, TurnOutcome, WakeAttribution,
    },
};

/// How many of a device's newest turns keep `decision_trace` and `afe_series`
/// (afe-metadata.md: "for the newest 1,000 rows per device, as the decision trace is").
pub const TRACE_RETENTION: u32 = 1_000;

/// A `turns` row id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TurnRowId(pub i64);

/// A row's operation key: what makes a repeated completion find the row it already wrote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnKey {
    /// A turn, by its `turn_uuid`.
    Turn(Uuid),
    /// A refused wake candidate, by the `candidate_id` its decision trace records; its
    /// `turn_uuid` stays NULL, as existing refused rows have it.
    Candidate(CandidateId),
}

/// The wake columns, all present for a wake turn or candidate.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeColumns {
    /// `wake_model`.
    pub model: Option<WakeModelName>,
    /// `wake_model_sha256`.
    pub graph_sha256: Sha256,
    /// `wake_score`.
    pub score: Option<Score>,
    /// `wake_threshold`.
    pub threshold: Option<Score>,
    /// `wake_attribution`.
    pub attribution: WakeAttribution,
}

/// The commit columns, all present exactly when the utterance committed.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitColumns {
    /// `commit_route`.
    pub route: CommitRoute,
    /// `commit_id`.
    pub id: Uuid,
    /// `endpoint_class`.
    pub class: Option<EndpointClass>,
    /// `endpoint_ms`, in sample time.
    pub endpoint: Duration,
    /// `audio_ms`.
    pub audio: Duration,
}

/// `intent_ms` and `intent_local`, present together when an intent request answered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntentColumns {
    /// `intent_ms`.
    pub elapsed: Duration,
    /// `intent_local`: whether HA's local agent answered; `None` when not reported.
    pub local: Option<bool>,
}

/// One `turns` row as the controller writes it. See the module docs for each column.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnRecord {
    /// `device_id`.
    pub device: DeviceId,
    /// `ts`.
    pub ts: WallSeconds,
    /// The operation key; `turn_uuid` for a turn.
    pub key: TurnKey,
    /// `trigger_type`.
    pub trigger: TriggerType,
    /// The wake columns.
    pub wake: Option<WakeColumns>,
    /// `policy_hash`.
    pub policy_hash: PolicyHash,
    /// `terminal_reason`.
    pub terminal_reason: TerminalReason,
    /// `outcome`.
    pub outcome: Option<TurnOutcome>,
    /// `total_ms`.
    pub total: Duration,
    /// `asr_text`.
    pub asr_text: Option<String>,
    /// `reference_coverage`.
    pub reference_coverage: Option<ReferenceCoverage>,
    /// The commit columns.
    pub commit: Option<CommitColumns>,
    /// `stt_raw`.
    pub stt_raw: Option<String>,
    /// `stt_text`.
    pub stt_text: Option<String>,
    /// `stt_ms`.
    pub stt: Option<Duration>,
    /// `stt_fix`.
    pub stt_fix: Option<SttFix>,
    /// `intent_ms` and `intent_local`.
    pub intent: Option<IntentColumns>,
    /// `response_type`.
    pub response_type: Option<ResponseType>,
    /// `response_text`.
    pub response_text: Option<String>,
    /// `conversation_id`.
    pub conversation_id: Option<ConversationId>,
    /// `tts_url_ms`.
    pub tts_url: Option<Duration>,
    /// `playback_ms`.
    pub playback: Option<Duration>,
    /// `playback_reason`.
    pub playback_reason: Option<PlaybackReason>,
    /// `first_audio_ms`.
    pub first_audio: Option<Duration>,
    /// `response_latency_ms`.
    pub response_latency: Option<Duration>,
    /// `reply_to`.
    pub reply_to: Option<Uuid>,
    /// `continuation`: None, [`Continuation::Pending`] when the answer asked a question, or
    /// a final value already known when the row is written.
    pub continuation: Option<Continuation>,
    /// `decision_trace`. A refused candidate's row must carry it: its key lives there.
    pub decision_trace: Option<DecisionTrace>,
    /// `afe_evidence`.
    pub afe_evidence: Option<AfeEvidence>,
    /// `afe_series`.
    pub afe_series: Option<AfeSeries>,
    /// `wake_windows`; normally scored after the wake clip is saved.
    pub wake_windows: Option<ClipWindows>,
}

/// What the device runtime knows about a wake beyond the candidate's trace.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeContext {
    /// The registry wake phrase of the graph, if the graph is in the registry.
    pub model: Option<WakeModelName>,
    /// The graph that scored the candidate.
    pub graph_sha256: Sha256,
    /// `wake.candidate_end`'s `peak_smoothed`, once it arrived.
    pub candidate_end_peak: Option<f64>,
}

/// The columns an utterance trace determines.
#[derive(Clone, Debug, PartialEq)]
pub struct UtteranceColumns {
    /// `trigger_type`.
    pub trigger: TriggerType,
    /// `asr_text`.
    pub asr_text: Option<String>,
    /// `reference_coverage`.
    pub reference_coverage: Option<ReferenceCoverage>,
    /// The commit columns.
    pub commit: Option<CommitColumns>,
    /// The terminal reason when the utterance closed without a commit.
    pub close: Option<TerminalReason>,
}

impl UtteranceColumns {
    /// Maps an utterance trace. `commit_id` is the id the runtime gave its commit: required
    /// exactly when the trace committed.
    pub fn from_trace(
        trace: &UtteranceTrace,
        commit_id: Option<Uuid>,
    ) -> Result<Self, MappingError> {
        let mut commit = None;
        let mut close = None;
        for event in &trace.endpoint {
            match event {
                EndpointEvent::Commit {
                    route,
                    boundary,
                    span,
                    at,
                    class,
                    ..
                } => {
                    let id = commit_id.ok_or(MappingError::CommitId)?;
                    commit = Some(CommitColumns {
                        route: (*route).into(),
                        id,
                        class: class.map(EndpointClass::from),
                        endpoint: Duration::from_millis(at.since(*boundary).as_ms()),
                        audio: Duration::from_millis(span.len().as_ms()),
                    });
                }
                EndpointEvent::Close { reason, .. } => close = Some((*reason).into()),
                _ => {}
            }
        }
        if commit.is_none() && commit_id.is_some() {
            return Err(MappingError::CommitId);
        }
        let counts = &trace.evidence;
        let reference_coverage = (counts.cells > 0).then(|| {
            let covered = counts.cells.saturating_sub(counts.reference_incomplete);
            ReferenceCoverage::new(f64::from(covered) / f64::from(counts.cells))
                .expect("a share of cells is in [0, 1]")
        });
        Ok(Self {
            trigger: trigger_of(trace.kind),
            asr_text: (!trace.streaming_text.is_empty()).then(|| trace.streaming_text.clone()),
            reference_coverage,
            commit,
            close,
        })
    }
}

impl WakeColumns {
    /// Maps a decided wake candidate. `refusal` overrides an acceptance for a candidate that
    /// lost arbitration; a refused trace maps to its own refusal.
    pub fn from_trace(
        trace: &WakeTrace,
        context: WakeContext,
        lost_arbitration: bool,
    ) -> Result<Self, MappingError> {
        let attribution = match (trace.outcome, trace.rule) {
            (None, _) | (_, None) => return Err(MappingError::Undecided),
            (Some(WakeOutcome::Refused(r)), _) if !lost_arbitration => {
                WakeAttribution::Refused(r.into())
            }
            (Some(WakeOutcome::Refused(_)), _) => return Err(MappingError::OutcomeMismatch),
            (Some(WakeOutcome::Accepted), _) if lost_arbitration => {
                WakeAttribution::Refused(CandidateRefusal::ArbitrationLost)
            }
            (Some(WakeOutcome::Accepted), Some(WakeRule::Verified)) => WakeAttribution::Verified,
            (Some(WakeOutcome::Accepted), Some(_)) => WakeAttribution::Idle,
        };
        let peak = trace
            .candidate
            .hops
            .iter()
            .filter_map(|h| h.smoothed)
            .chain(context.candidate_end_peak)
            .filter(|v| v.is_finite())
            .reduce(f64::max);
        Ok(Self {
            model: context.model,
            graph_sha256: context.graph_sha256,
            score: peak.and_then(Score::new),
            threshold: Score::new(trace.candidate.threshold),
            attribution,
        })
    }
}

impl TurnRecord {
    /// Sets `afe_evidence`, and the same summaries in the decision trace.
    pub fn set_afe_evidence(&mut self, evidence: AfeEvidence) {
        if let Some(trace) = self.decision_trace.as_mut() {
            trace.set_afe_evidence(&evidence);
        }
        self.afe_evidence = Some(evidence);
    }

    /// Adds the verifier's timing, the stable prefix, the pause decodes and the commit's
    /// source to the decision trace ([`crate::DecisionTrace::add_details`]).
    pub fn set_trace_details(
        &mut self,
        utterance: Option<&echomuse_turn::UtteranceTrace>,
        details: &crate::trace::TraceDetails,
    ) {
        if let Some(trace) = self.decision_trace.as_mut() {
            trace.add_details(utterance, details);
        }
    }

    /// A row with only the columns every row has; the rest are None until set.
    pub fn bare(
        device: DeviceId,
        ts: WallSeconds,
        key: TurnKey,
        trigger: TriggerType,
        policy_hash: PolicyHash,
        terminal_reason: TerminalReason,
        total: Duration,
    ) -> Self {
        Self {
            device,
            ts,
            key,
            trigger,
            wake: None,
            policy_hash,
            terminal_reason,
            outcome: None,
            total,
            asr_text: None,
            reference_coverage: None,
            commit: None,
            stt_raw: None,
            stt_text: None,
            stt: None,
            stt_fix: None,
            intent: None,
            response_type: None,
            response_text: None,
            conversation_id: None,
            tts_url: None,
            playback: None,
            playback_reason: None,
            first_audio: None,
            response_latency: None,
            reply_to: None,
            continuation: None,
            decision_trace: None,
            afe_evidence: None,
            afe_series: None,
            wake_windows: None,
        }
    }

    /// The row of a wake candidate that did not become a turn: refused by the wake decision,
    /// or accepted and then lost arbitration (`lost_arbitration`). Its wake columns,
    /// attribution, terminal reason and decision trace come from the trace.
    pub fn candidate(
        device: DeviceId,
        ts: WallSeconds,
        trace: &WakeTrace,
        context: WakeContext,
        lost_arbitration: bool,
        policy_hash: PolicyHash,
        total: Duration,
    ) -> Result<Self, MappingError> {
        let wake = WakeColumns::from_trace(trace, context, lost_arbitration)?;
        let WakeAttribution::Refused(refusal) = wake.attribution else {
            return Err(MappingError::OutcomeMismatch);
        };
        let key = CandidateId::new(trace.candidate.candidate_id.clone())
            .map(TurnKey::Candidate)
            .ok_or(MappingError::OutcomeMismatch)?;
        let terminal_reason = refusal.terminal_reason();
        let mut record = Self::bare(
            device,
            ts,
            key,
            TriggerType::Wake,
            policy_hash,
            terminal_reason,
            total,
        );
        record.wake = Some(wake);
        record.decision_trace = Some(DecisionTrace::compose(TraceParts {
            turn_uuid: None,
            trigger: TriggerType::Wake,
            terminal_reason,
            wake: Some(trace),
            utterance: None,
        }));
        Ok(record)
    }

    /// The row of a turn: an accepted wake (with its candidate's trace), a button press or a
    /// reply, with its utterance's trace when one ran. The trigger, wake, utterance and
    /// decision-trace columns come from the traces (`trigger` is used only when no utterance
    /// ran; otherwise the utterance's kind is the trigger); the runtime sets the rest
    /// afterwards.
    #[allow(clippy::too_many_arguments)]
    pub fn turn(
        device: DeviceId,
        ts: WallSeconds,
        turn_uuid: Uuid,
        trigger: TriggerType,
        wake: Option<(&WakeTrace, WakeContext)>,
        utterance: Option<(&UtteranceTrace, Option<Uuid>)>,
        policy_hash: PolicyHash,
        terminal_reason: TerminalReason,
        total: Duration,
    ) -> Result<Self, MappingError> {
        let wake_columns = match &wake {
            Some((trace, context)) => {
                let columns = WakeColumns::from_trace(trace, context.clone(), false)?;
                if matches!(columns.attribution, WakeAttribution::Refused(_)) {
                    return Err(MappingError::OutcomeMismatch);
                }
                Some(columns)
            }
            None => None,
        };
        let utterance_columns = utterance
            .map(|(trace, commit_id)| UtteranceColumns::from_trace(trace, commit_id))
            .transpose()?;
        let trigger = utterance_columns.as_ref().map_or(trigger, |u| u.trigger);
        let mut record = Self::bare(
            device,
            ts,
            TurnKey::Turn(turn_uuid),
            trigger,
            policy_hash,
            terminal_reason,
            total,
        );
        record.wake = wake_columns;
        if let Some(u) = utterance_columns {
            record.asr_text = u.asr_text;
            record.reference_coverage = u.reference_coverage;
            record.commit = u.commit;
        }
        record.decision_trace = Some(DecisionTrace::compose(TraceParts {
            turn_uuid: Some(turn_uuid),
            trigger,
            terminal_reason,
            wake: wake.as_ref().map(|(t, _)| *t),
            utterance: utterance.map(|(t, _)| t),
        }));
        Ok(record)
    }
}

/// The result of [`InsertTurn`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TurnInsert {
    /// The row: the new one, or the one an earlier completion wrote.
    pub id: TurnRowId,
    /// False when the key already had a row, which is left as it was.
    pub inserted: bool,
}

/// Inserts a turn row once per operation key. A repeat of the same key on the same device
/// returns the existing row unchanged; the key on another device is a
/// [`RecordError::KeyConflict`]. Then clears `decision_trace` and `afe_series` of the
/// device's rows past the newest `trace_retention`.
#[derive(Clone, Debug)]
pub struct InsertTurn {
    /// The row.
    pub record: TurnRecord,
    /// See [`TRACE_RETENTION`].
    pub trace_retention: u32,
}

impl InsertTurn {
    /// Inserts with the standard trace retention.
    pub fn new(record: TurnRecord) -> Self {
        Self {
            record,
            trace_retention: TRACE_RETENTION,
        }
    }
}

/// How far either side of a candidate row's `ts` a repeat of it is looked for. A repeated
/// completion carries the same record, so its `ts` is the same; the window only bounds the
/// rows whose trace is read.
const CANDIDATE_REPEAT_WINDOW_S: f64 = 3_600.0;

fn find_turn(tx: &Connection, record: &TurnRecord) -> rusqlite::Result<Option<(i64, String)>> {
    match &record.key {
        TurnKey::Turn(uuid) => tx
            .query_row(
                "SELECT id, device_id FROM turns WHERE turn_uuid = ?1 ORDER BY id LIMIT 1",
                [uuid.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional(),
        // A candidate id is the device's own, so it is looked for among that device's rows,
        // through the (device_id, ts) index.
        TurnKey::Candidate(candidate) => tx
            .query_row(
                "SELECT id, device_id FROM turns WHERE device_id = ?1 AND ts >= ?2 \
                 AND ts <= ?3 AND turn_uuid IS NULL AND decision_trace IS NOT NULL \
                 AND json_extract(decision_trace, '$.candidate_id') = ?4 ORDER BY id LIMIT 1",
                params![
                    record.device.0,
                    record.ts.get() - CANDIDATE_REPEAT_WINDOW_S,
                    record.ts.get() + CANDIDATE_REPEAT_WINDOW_S,
                    candidate.as_str()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional(),
    }
}

fn ms(d: Option<Duration>) -> Option<i64> {
    d.map(whole_ms)
}

impl WriteCommand for InsertTurn {
    type Output = TurnInsert;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<TurnInsert, RecordError> {
        let r = &self.record;
        if let TurnKey::Candidate(candidate) = &r.key {
            let traced = r
                .decision_trace
                .as_ref()
                .and_then(DecisionTrace::candidate_id);
            if traced != Some(candidate.as_str()) {
                return Err(RecordError::Invalid(
                    "a candidate row's decision trace records its candidate_id",
                ));
            }
        }
        if let Some((id, device)) = find_turn(tx, r)? {
            if device != r.device.0 {
                return Err(RecordError::KeyConflict {
                    table: "turns",
                    key: match &r.key {
                        TurnKey::Turn(u) => u.to_string(),
                        TurnKey::Candidate(c) => c.to_string(),
                    },
                });
            }
            return Ok(TurnInsert {
                id: TurnRowId(id),
                inserted: false,
            });
        }
        let wake = r.wake.as_ref();
        let commit = r.commit.as_ref();
        tx.execute(
            "INSERT INTO turns (device_id, ts, trigger_type, wake_model, wake_score, \
             wake_threshold, outcome, stt_text, total_ms, stt_ms, tts_url_ms, playback_ms, \
             audio_ms, wake_model_sha256, policy_hash, wake_attribution, reference_coverage, \
             commit_route, terminal_reason, commit_id, asr_text, endpoint_class, endpoint_ms, \
             stt_raw, intent_ms, intent_local, response_type, response_text, turn_uuid, \
             conversation_id, reply_to, continuation, playback_reason, first_audio_ms, \
             decision_trace, afe_evidence, afe_series, response_latency_ms, stt_fix, \
             wake_windows) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, \
             ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40)",
            params![
                r.device.0,
                r.ts.get(),
                r.trigger.as_str(),
                wake.and_then(|w| w.model.as_ref().map(WakeModelName::as_str)),
                wake.and_then(|w| w.score.map(Score::get)),
                wake.and_then(|w| w.threshold.map(Score::get)),
                r.outcome.map(TurnOutcome::as_str),
                r.stt_text,
                whole_ms(r.total),
                ms(r.stt),
                ms(r.tts_url),
                ms(r.playback),
                commit.map(|c| whole_ms(c.audio)),
                wake.map(|w| w.graph_sha256.as_str()),
                r.policy_hash.as_str(),
                wake.map(|w| w.attribution.as_str()),
                r.reference_coverage.map(ReferenceCoverage::get),
                commit.map(|c| c.route.as_str()),
                r.terminal_reason.as_str(),
                commit.map(|c| c.id.to_string()),
                r.asr_text,
                commit.and_then(|c| c.class.map(EndpointClass::as_str)),
                commit.map(|c| whole_ms(c.endpoint)),
                r.stt_raw,
                r.intent.map(|i| whole_ms(i.elapsed)),
                r.intent.and_then(|i| i.local).map(i64::from),
                r.response_type.as_ref().map(ResponseType::as_str),
                r.response_text,
                match &r.key {
                    TurnKey::Turn(u) => Some(u.to_string()),
                    TurnKey::Candidate(_) => None,
                },
                r.conversation_id.as_ref().map(|c| c.0.as_str()),
                r.reply_to.map(|u| u.to_string()),
                r.continuation.map(Continuation::as_str),
                r.playback_reason.map(PlaybackReason::as_str),
                ms(r.first_audio),
                r.decision_trace.as_ref().map(DecisionTrace::as_str),
                r.afe_evidence.as_ref().map(AfeEvidence::as_str),
                r.afe_series.as_ref().map(AfeSeries::as_str),
                ms(r.response_latency),
                r.stt_fix.as_ref().map(SttFix::as_str),
                r.wake_windows.as_ref().map(ClipWindows::to_json),
            ],
        )?;
        let id = tx.last_insert_rowid();
        prune_turn_traces(tx, &r.device, self.trace_retention)?;
        Ok(TurnInsert {
            id: TurnRowId(id),
            inserted: true,
        })
    }
}

/// Clears `decision_trace` and `afe_series` of a device's turns past its newest `keep`.
pub(crate) fn prune_turn_traces(
    tx: &Connection,
    device: &DeviceId,
    keep: u32,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE turns SET decision_trace = NULL, afe_series = NULL \
         WHERE device_id = ?1 AND (decision_trace IS NOT NULL OR afe_series IS NOT NULL) \
         AND id < (SELECT id FROM turns WHERE device_id = ?1 ORDER BY id DESC \
                   LIMIT 1 OFFSET ?2)",
        params![device.0, i64::from(keep.max(1)) - 1],
    )
}

/// The result of [`RecordContinuation`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContinuationUpdate {
    /// `pending` became the final value.
    Resolved,
    /// The row already held a final value, which stays.
    AlreadyFinal(ContinuationEnd),
    /// The row's answer asked nothing (NULL); nothing changed.
    NotAsked,
    /// The row holds an unknown spelling; nothing changed.
    Unrecognized,
    /// No turn of this device has the UUID.
    NoSuchTurn,
}

/// Moves a turn's `continuation` from `pending` to its one final value. A second call never
/// changes the first final value.
#[derive(Clone, Debug)]
pub struct RecordContinuation {
    /// The asking turn's device.
    pub device: DeviceId,
    /// The asking turn.
    pub turn_uuid: Uuid,
    /// The final value.
    pub end: ContinuationEnd,
}

impl WriteCommand for RecordContinuation {
    type Output = ContinuationUpdate;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<ContinuationUpdate, RecordError> {
        let changed = tx.execute(
            "UPDATE turns SET continuation = ?3 \
             WHERE device_id = ?1 AND turn_uuid = ?2 AND continuation = 'pending'",
            params![self.device.0, self.turn_uuid.to_string(), self.end.as_str()],
        )?;
        if changed > 0 {
            return Ok(ContinuationUpdate::Resolved);
        }
        let current: Option<Option<String>> = tx
            .query_row(
                "SELECT continuation FROM turns WHERE device_id = ?1 AND turn_uuid = ?2 \
                 ORDER BY id LIMIT 1",
                params![self.device.0, self.turn_uuid.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(match current {
            None => ContinuationUpdate::NoSuchTurn,
            Some(None) => ContinuationUpdate::NotAsked,
            Some(Some(text)) => match ContinuationEnd::parse(&text) {
                Some(end) => ContinuationUpdate::AlreadyFinal(end),
                None => ContinuationUpdate::Unrecognized,
            },
        })
    }
}

/// Resolves every `pending` continuation to `session_lost`, at startup: a reply expectation
/// is not persisted (§5.7 step 5), so one left pending by a stopped controller can never be
/// answered, and the restart ended every device session it belonged to.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResolveStaleContinuations;

impl WriteCommand for ResolveStaleContinuations {
    type Output = usize;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<usize, RecordError> {
        Ok(tx.execute(
            "UPDATE turns SET continuation = ?1 WHERE continuation = 'pending'",
            [TerminalReason::SessionLost.as_str()],
        )?)
    }
}

/// Stores a scored wake clip's windows in `wake_windows`. Returns false when the row is gone.
#[derive(Clone, Debug)]
pub struct SetTurnWakeWindows {
    /// The turn.
    pub id: TurnRowId,
    /// The windows.
    pub windows: ClipWindows,
}

impl WriteCommand for SetTurnWakeWindows {
    type Output = bool;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, RecordError> {
        Ok(tx.execute(
            "UPDATE turns SET wake_windows = ?2 WHERE id = ?1",
            params![self.id.0, self.windows.to_json()],
        )? > 0)
    }
}

/// One `turns` row as stored, every column. Text vocabularies keep older spellings as
/// [`Stored::Other`]; millisecond columns are raw, since rows from before the cutover hold
/// negative sentinels for unmeasured stages.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct StoredTurn {
    pub id: TurnRowId,
    pub device_id: String,
    pub ts: f64,
    pub trigger_type: Option<Stored<TriggerType>>,
    pub wake_model: Option<String>,
    pub wake_score: Option<f64>,
    pub wake_threshold: Option<f64>,
    pub noise_floor: Option<f64>,
    pub outcome: Option<Stored<TurnOutcome>>,
    pub stt_text: Option<String>,
    pub total_ms: Option<i64>,
    pub vad_end_ms: Option<i64>,
    pub stt_ms: Option<i64>,
    pub tts_url_ms: Option<i64>,
    pub tts_fetch_ms: Option<i64>,
    pub playback_ms: Option<i64>,
    pub audio_ms: Option<i64>,
    pub tts_bytes: Option<i64>,
    pub underruns: Option<i64>,
    pub playback_periods: Option<i64>,
    pub min_depth: Option<i64>,
    pub prime_wait_ms: Option<i64>,
    pub recv_span_ms: Option<i64>,
    pub max_gap_ms: Option<i64>,
    pub bytes_recv: Option<i64>,
    pub send_ms: Option<i64>,
    pub delivery_ms: Option<i64>,
    pub eq_ms: Option<i64>,
    pub audio_file: Option<String>,
    pub dev_wake_score: Option<f64>,
    pub dev_wake_delta_ms: Option<i64>,
    pub dev_shadow: Option<i64>,
    pub dev_threshold: Option<f64>,
    pub ctrl_wake_score: Option<f64>,
    pub ctrl_wake_delta_ms: Option<i64>,
    pub wake_file: Option<String>,
    pub wake_model_sha256: Option<String>,
    pub policy_hash: Option<String>,
    pub wake_attribution: Option<Stored<WakeAttribution>>,
    pub reference_coverage: Option<f64>,
    pub commit_route: Option<Stored<CommitRoute>>,
    pub terminal_reason: Option<Stored<TerminalReason>>,
    pub commit_id: Option<String>,
    pub asr_text: Option<String>,
    pub endpoint_class: Option<Stored<EndpointClass>>,
    pub endpoint_ms: Option<i64>,
    pub stt_raw: Option<String>,
    pub intent_ms: Option<i64>,
    pub intent_local: Option<bool>,
    pub response_type: Option<String>,
    pub response_text: Option<String>,
    pub turn_uuid: Option<String>,
    pub conversation_id: Option<String>,
    pub reply_to: Option<String>,
    pub continuation: Option<Stored<Continuation>>,
    pub playback_reason: Option<Stored<PlaybackReason>>,
    pub first_audio_ms: Option<i64>,
    pub decision_trace: Option<String>,
    pub afe_evidence: Option<String>,
    pub afe_series: Option<String>,
    pub response_latency_ms: Option<i64>,
    pub stt_fix: Option<String>,
    pub wake_windows: Option<String>,
}

fn vocab<T>(
    row: &Row<'_>,
    column: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> rusqlite::Result<Option<Stored<T>>> {
    Ok(row
        .get::<_, Option<String>>(column)?
        .map(|text| Stored::read(text, parse)))
}

fn stored_turn(row: &Row<'_>) -> rusqlite::Result<StoredTurn> {
    Ok(StoredTurn {
        id: TurnRowId(row.get("id")?),
        device_id: row.get("device_id")?,
        ts: row.get("ts")?,
        trigger_type: vocab(row, "trigger_type", TriggerType::parse)?,
        wake_model: row.get("wake_model")?,
        wake_score: row.get("wake_score")?,
        wake_threshold: row.get("wake_threshold")?,
        noise_floor: row.get("noise_floor")?,
        outcome: vocab(row, "outcome", TurnOutcome::parse)?,
        stt_text: row.get("stt_text")?,
        total_ms: row.get("total_ms")?,
        vad_end_ms: row.get("vad_end_ms")?,
        stt_ms: row.get("stt_ms")?,
        tts_url_ms: row.get("tts_url_ms")?,
        tts_fetch_ms: row.get("tts_fetch_ms")?,
        playback_ms: row.get("playback_ms")?,
        audio_ms: row.get("audio_ms")?,
        tts_bytes: row.get("tts_bytes")?,
        underruns: row.get("underruns")?,
        playback_periods: row.get("playback_periods")?,
        min_depth: row.get("min_depth")?,
        prime_wait_ms: row.get("prime_wait_ms")?,
        recv_span_ms: row.get("recv_span_ms")?,
        max_gap_ms: row.get("max_gap_ms")?,
        bytes_recv: row.get("bytes_recv")?,
        send_ms: row.get("send_ms")?,
        delivery_ms: row.get("delivery_ms")?,
        eq_ms: row.get("eq_ms")?,
        audio_file: row.get("audio_file")?,
        dev_wake_score: row.get("dev_wake_score")?,
        dev_wake_delta_ms: row.get("dev_wake_delta_ms")?,
        dev_shadow: row.get("dev_shadow")?,
        dev_threshold: row.get("dev_threshold")?,
        ctrl_wake_score: row.get("ctrl_wake_score")?,
        ctrl_wake_delta_ms: row.get("ctrl_wake_delta_ms")?,
        wake_file: row.get("wake_file")?,
        wake_model_sha256: row.get("wake_model_sha256")?,
        policy_hash: row.get("policy_hash")?,
        wake_attribution: vocab(row, "wake_attribution", WakeAttribution::parse)?,
        reference_coverage: row.get("reference_coverage")?,
        commit_route: vocab(row, "commit_route", CommitRoute::parse)?,
        terminal_reason: vocab(row, "terminal_reason", TerminalReason::parse)?,
        commit_id: row.get("commit_id")?,
        asr_text: row.get("asr_text")?,
        endpoint_class: vocab(row, "endpoint_class", EndpointClass::parse)?,
        endpoint_ms: row.get("endpoint_ms")?,
        stt_raw: row.get("stt_raw")?,
        intent_ms: row.get("intent_ms")?,
        intent_local: row.get::<_, Option<i64>>("intent_local")?.map(|v| v != 0),
        response_type: row.get("response_type")?,
        response_text: row.get("response_text")?,
        turn_uuid: row.get("turn_uuid")?,
        conversation_id: row.get("conversation_id")?,
        reply_to: row.get("reply_to")?,
        continuation: vocab(row, "continuation", Continuation::parse)?,
        playback_reason: vocab(row, "playback_reason", PlaybackReason::parse)?,
        first_audio_ms: row.get("first_audio_ms")?,
        decision_trace: row.get("decision_trace")?,
        afe_evidence: row.get("afe_evidence")?,
        afe_series: row.get("afe_series")?,
        response_latency_ms: row.get("response_latency_ms")?,
        stt_fix: row.get("stt_fix")?,
        wake_windows: row.get("wake_windows")?,
    })
}

/// Reads one turn row by id.
#[derive(Clone, Copy, Debug)]
pub struct GetTurn(pub TurnRowId);

impl ReadQuery for GetTurn {
    type Output = Option<StoredTurn>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Option<StoredTurn>, RecordError> {
        Ok(conn
            .query_row("SELECT * FROM turns WHERE id = ?1", [self.0.0], stored_turn)
            .optional()?)
    }
}
