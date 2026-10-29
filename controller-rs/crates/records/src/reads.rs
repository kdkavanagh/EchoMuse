//! What the dashboard reads back: the turn and near-miss lists, the wake clips an archive
//! holds, the response latencies and the shadow-rule statistics, each one read on the
//! database's read pool.
//!
//! Text columns come back as stored, older spellings included, and JSON columns as their
//! stored text: the API decides how each reaches the dashboard. A list never reads the large
//! per-row documents (`decision_trace`, `afe_series`); [`GetTurn`](crate::GetTurn) and
//! [`GetNearMiss`](crate::GetNearMiss) read one row whole.

use echomuse_core::types::DeviceId;
use echomuse_db::ReadQuery;
use rusqlite::{Connection, Row, params};

use crate::{
    error::RecordError,
    near_misses::NearMissRowId,
    rollups::LEAD_BUCKETS,
    turns::TurnRowId,
    vocab::{NearMissVerdict, TerminalReason},
};

/// Whose rows a list reads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ListScope {
    /// Every device's.
    Fleet,
    /// One device's.
    Device(DeviceId),
}

impl ListScope {
    fn device(&self) -> Option<&str> {
        match self {
            Self::Fleet => None,
            Self::Device(device) => Some(device.0.as_str()),
        }
    }
}

/// One `turns` row as a list shows it: every column the dashboard reads, raw. Millisecond
/// columns keep the negative sentinels rows from before the post-AFE cutover hold.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct TurnListing {
    pub id: TurnRowId,
    pub device_id: String,
    pub ts: f64,
    pub trigger_type: Option<String>,
    pub wake_model: Option<String>,
    pub wake_score: Option<f64>,
    pub wake_threshold: Option<f64>,
    pub wake_model_sha256: Option<String>,
    pub wake_attribution: Option<String>,
    pub outcome: Option<String>,
    pub asr_text: Option<String>,
    pub reference_coverage: Option<f64>,
    pub commit_route: Option<String>,
    pub commit_id: Option<String>,
    pub endpoint_class: Option<String>,
    pub endpoint_ms: Option<i64>,
    pub audio_ms: Option<i64>,
    pub stt_raw: Option<String>,
    pub stt_text: Option<String>,
    pub stt_ms: Option<i64>,
    pub stt_fix: Option<String>,
    pub intent_ms: Option<i64>,
    pub intent_local: Option<bool>,
    pub response_type: Option<String>,
    pub response_text: Option<String>,
    pub conversation_id: Option<String>,
    pub tts_url_ms: Option<i64>,
    pub playback_ms: Option<i64>,
    pub playback_reason: Option<String>,
    pub first_audio_ms: Option<i64>,
    pub response_latency_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub policy_hash: Option<String>,
    pub terminal_reason: Option<String>,
    pub turn_uuid: Option<String>,
    pub reply_to: Option<String>,
    pub continuation: Option<String>,
    pub afe_evidence: Option<String>,
    pub wake_windows: Option<String>,
    pub audio_file: Option<String>,
    pub wake_file: Option<String>,
}

const TURN_COLUMNS: &str = "id, device_id, ts, trigger_type, wake_model, wake_score, \
     wake_threshold, wake_model_sha256, wake_attribution, outcome, asr_text, reference_coverage, \
     commit_route, commit_id, endpoint_class, endpoint_ms, audio_ms, stt_raw, stt_text, stt_ms, \
     stt_fix, intent_ms, intent_local, response_type, response_text, conversation_id, \
     tts_url_ms, playback_ms, playback_reason, first_audio_ms, response_latency_ms, total_ms, \
     policy_hash, terminal_reason, turn_uuid, reply_to, continuation, afe_evidence, \
     wake_windows, audio_file, wake_file";

fn turn_listing(row: &Row<'_>) -> rusqlite::Result<TurnListing> {
    Ok(TurnListing {
        id: TurnRowId(row.get("id")?),
        device_id: row.get("device_id")?,
        ts: row.get("ts")?,
        trigger_type: row.get("trigger_type")?,
        wake_model: row.get("wake_model")?,
        wake_score: row.get("wake_score")?,
        wake_threshold: row.get("wake_threshold")?,
        wake_model_sha256: row.get("wake_model_sha256")?,
        wake_attribution: row.get("wake_attribution")?,
        outcome: row.get("outcome")?,
        asr_text: row.get("asr_text")?,
        reference_coverage: row.get("reference_coverage")?,
        commit_route: row.get("commit_route")?,
        commit_id: row.get("commit_id")?,
        endpoint_class: row.get("endpoint_class")?,
        endpoint_ms: row.get("endpoint_ms")?,
        audio_ms: row.get("audio_ms")?,
        stt_raw: row.get("stt_raw")?,
        stt_text: row.get("stt_text")?,
        stt_ms: row.get("stt_ms")?,
        stt_fix: row.get("stt_fix")?,
        intent_ms: row.get("intent_ms")?,
        intent_local: row.get::<_, Option<i64>>("intent_local")?.map(|v| v != 0),
        response_type: row.get("response_type")?,
        response_text: row.get("response_text")?,
        conversation_id: row.get("conversation_id")?,
        tts_url_ms: row.get("tts_url_ms")?,
        playback_ms: row.get("playback_ms")?,
        playback_reason: row.get("playback_reason")?,
        first_audio_ms: row.get("first_audio_ms")?,
        response_latency_ms: row.get("response_latency_ms")?,
        total_ms: row.get("total_ms")?,
        policy_hash: row.get("policy_hash")?,
        terminal_reason: row.get("terminal_reason")?,
        turn_uuid: row.get("turn_uuid")?,
        reply_to: row.get("reply_to")?,
        continuation: row.get("continuation")?,
        afe_evidence: row.get("afe_evidence")?,
        wake_windows: row.get("wake_windows")?,
        audio_file: row.get("audio_file")?,
        wake_file: row.get("wake_file")?,
    })
}

/// The newest `limit` rows of a table in `scope` from `since` on, oldest first. Each scope
/// has its own statement, so SQLite can use the table's `(device_id, ts)` or `(ts)` index.
fn newest<T>(
    conn: &Connection,
    table: &str,
    columns: &str,
    scope: &ListScope,
    since: Option<f64>,
    limit: u32,
    map: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let device = if scope.device().is_some() {
        "device_id = ?1"
    } else {
        "?1 IS NULL"
    };
    let mut statement = conn.prepare_cached(&format!(
        "SELECT {columns} FROM {table} WHERE {device} AND (?2 IS NULL OR ts >= ?2) \
         ORDER BY ts DESC, id DESC LIMIT ?3"
    ))?;
    let mut rows = statement
        .query_map(params![scope.device(), since, limit], map)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.reverse();
    Ok(rows)
}

/// The newest `limit` turns (and refused wake candidates) of `scope` whose `ts` is at least
/// `since`, oldest first, as the dashboard lists them.
#[derive(Clone, Debug)]
pub struct ListTurns {
    /// Whose turns.
    pub scope: ListScope,
    /// The earliest `ts`, in wall seconds; `None` for no bound.
    pub since: Option<f64>,
    /// The most rows.
    pub limit: u32,
}

impl ReadQuery for ListTurns {
    type Output = Vec<TurnListing>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<TurnListing>, RecordError> {
        Ok(newest(
            conn,
            "turns",
            TURN_COLUMNS,
            &self.scope,
            self.since,
            self.limit,
            turn_listing,
        )?)
    }
}

/// One `near_misses` row as a list shows it: every column but `afe_series`, raw.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct NearMissListing {
    pub id: NearMissRowId,
    pub device_id: String,
    pub ts: f64,
    pub near_miss_uuid: String,
    pub profile: String,
    pub floor: f64,
    pub peak: f64,
    pub wake_threshold: Option<f64>,
    pub wake_model_sha256: Option<String>,
    pub duration_ms: i64,
    pub end_reason: String,
    pub recording: String,
    pub audio_ms: Option<i64>,
    pub verdict: Option<String>,
    pub windows: Option<String>,
}

const NEAR_MISS_COLUMNS: &str = "id, device_id, ts, near_miss_uuid, profile, floor, peak, \
     wake_threshold, wake_model_sha256, duration_ms, end_reason, recording, audio_ms, verdict, \
     windows";

fn near_miss_listing(row: &Row<'_>) -> rusqlite::Result<NearMissListing> {
    Ok(NearMissListing {
        id: NearMissRowId(row.get("id")?),
        device_id: row.get("device_id")?,
        ts: row.get("ts")?,
        near_miss_uuid: row.get("near_miss_uuid")?,
        profile: row.get("profile")?,
        floor: row.get("floor")?,
        peak: row.get("peak")?,
        wake_threshold: row.get("wake_threshold")?,
        wake_model_sha256: row.get("wake_model_sha256")?,
        duration_ms: row.get("duration_ms")?,
        end_reason: row.get("end_reason")?,
        recording: row.get("recording")?,
        audio_ms: row.get("audio_ms")?,
        verdict: row.get("verdict")?,
        windows: row.get("windows")?,
    })
}

/// The newest `limit` near misses of `scope` whose `ts` is at least `since`, oldest first.
#[derive(Clone, Debug)]
pub struct ListNearMisses {
    /// Whose near misses.
    pub scope: ListScope,
    /// The earliest `ts`, in wall seconds; `None` for no bound.
    pub since: Option<f64>,
    /// The most rows.
    pub limit: u32,
}

impl ReadQuery for ListNearMisses {
    type Output = Vec<NearMissListing>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<NearMissListing>, RecordError> {
        Ok(newest(
            conn,
            "near_misses",
            NEAR_MISS_COLUMNS,
            &self.scope,
            self.since,
            self.limit,
            near_miss_listing,
        )?)
    }
}

/// A turn whose row says its wake clip was saved.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeClipRow {
    /// The turn; the clip is `wakes/<device>/<id>.wav`.
    pub id: TurnRowId,
    /// The turn's `ts`, in wall seconds.
    pub ts: f64,
}

/// Every turn of a device whose `wake_file` is set, oldest first. Retention may have removed
/// the file since; the reader checks.
#[derive(Clone, Debug)]
pub struct WakeClipRows {
    /// The device.
    pub device: DeviceId,
}

impl ReadQuery for WakeClipRows {
    type Output = Vec<WakeClipRow>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<WakeClipRow>, RecordError> {
        let mut statement = conn.prepare_cached(
            "SELECT id, ts FROM turns WHERE device_id = ?1 AND wake_file IS NOT NULL ORDER BY id",
        )?;
        let rows = statement.query_map([&self.device.0], |row| {
            Ok(WakeClipRow {
                id: TurnRowId(row.get(0)?),
                ts: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// One measured response latency.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResponseLatency {
    /// The turn's `ts`, in wall seconds.
    pub ts: f64,
    /// `response_latency_ms`.
    pub ms: i64,
}

/// A device's turns from `since` on that measured a response latency, oldest first. A
/// negative value is not a measurement and is left out.
#[derive(Clone, Debug)]
pub struct ResponseLatencies {
    /// The device.
    pub device: DeviceId,
    /// The earliest `ts`, in wall seconds.
    pub since: f64,
}

impl ReadQuery for ResponseLatencies {
    type Output = Vec<ResponseLatency>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<ResponseLatency>, RecordError> {
        let mut statement = conn.prepare_cached(
            "SELECT ts, response_latency_ms FROM turns WHERE device_id = ?1 AND ts >= ?2 \
             AND response_latency_ms IS NOT NULL AND response_latency_ms >= 0 ORDER BY ts, id",
        )?;
        let rows = statement.query_map(params![self.device.0, self.since], |row| {
            Ok(ResponseLatency {
                ts: row.get(0)?,
                ms: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// One shadow rule's `wake_shadow` counters summed over the hours read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShadowTotals {
    /// `rule_key`.
    pub rule_key: String,
    /// The hours that had a row.
    pub hours_reported: u64,
    /// `hops`: hops scored in the rule's profile.
    pub hops: u64,
    /// `opens`.
    pub opens: u64,
    /// `matched`.
    pub matched: u64,
    /// `lead_hist`, bucket by bucket: lead (live open − shadow open, in hops) −3..+3 at
    /// index lead + 3, clamped at both ends.
    pub lead_hist: [u64; LEAD_BUCKETS],
    /// `unmatched`.
    pub unmatched: u64,
    /// `retried`.
    pub retried: u64,
    /// `live_only`.
    pub live_only: u64,
    /// `events_dropped`.
    pub events_dropped: u64,
}

fn count(row: &Row<'_>, column: &str) -> rusqlite::Result<u64> {
    Ok(u64::try_from(row.get::<_, i64>(column)?).unwrap_or(0))
}

/// A device's `wake_shadow` rows from hour `since_hour` on, summed by rule, ordered by key.
#[derive(Clone, Debug)]
pub struct ShadowTotalsSince {
    /// The device.
    pub device: DeviceId,
    /// The first `hour_ts` read.
    pub since_hour: i64,
}

impl ReadQuery for ShadowTotalsSince {
    type Output = Vec<ShadowTotals>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<ShadowTotals>, RecordError> {
        let mut statement = conn.prepare_cached(
            "SELECT hour_ts, rule_key, hops, opens, matched, lead_hist, unmatched, retried, \
             live_only, events_dropped FROM wake_shadow WHERE device_id = ?1 AND hour_ts >= ?2 \
             ORDER BY rule_key, hour_ts",
        )?;
        let mut rows = statement.query(params![self.device.0, self.since_hour])?;
        let mut totals: Vec<ShadowTotals> = Vec::new();
        while let Some(row) = rows.next()? {
            let hour: i64 = row.get("hour_ts")?;
            let key: String = row.get("rule_key")?;
            let text: String = row.get("lead_hist")?;
            let lead: Vec<u64> = serde_json::from_str(&text)
                .ok()
                .filter(|v: &Vec<u64>| v.len() == LEAD_BUCKETS)
                .ok_or(RecordError::BadStoredValue {
                    table: "wake_shadow",
                    column: "lead_hist",
                    id: hour,
                })?;
            if totals.last().is_none_or(|t| t.rule_key != key) {
                totals.push(ShadowTotals {
                    rule_key: key,
                    hours_reported: 0,
                    hops: 0,
                    opens: 0,
                    matched: 0,
                    lead_hist: [0; LEAD_BUCKETS],
                    unmatched: 0,
                    retried: 0,
                    live_only: 0,
                    events_dropped: 0,
                });
            }
            let total = totals.last_mut().expect("pushed above");
            total.hours_reported += 1;
            total.hops = total.hops.saturating_add(count(row, "hops")?);
            total.opens = total.opens.saturating_add(count(row, "opens")?);
            total.matched = total.matched.saturating_add(count(row, "matched")?);
            for (sum, add) in total.lead_hist.iter_mut().zip(lead) {
                *sum = sum.saturating_add(add);
            }
            total.unmatched = total.unmatched.saturating_add(count(row, "unmatched")?);
            total.retried = total.retried.saturating_add(count(row, "retried")?);
            total.live_only = total.live_only.saturating_add(count(row, "live_only")?);
            total.events_dropped = total
                .events_dropped
                .saturating_add(count(row, "events_dropped")?);
        }
        Ok(totals)
    }
}

/// One `wake_shadow_events` row.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredShadowEvent {
    /// The row id.
    pub id: i64,
    /// `ts`, wall seconds.
    pub ts: f64,
    /// `rule_key`.
    pub rule_key: String,
    /// `kind`, as stored.
    pub kind: String,
    /// `peak_raw`.
    pub peak_raw: f64,
    /// `raws`, the stored JSON array text.
    pub raws: String,
}

/// A device's newest `limit` shadow events from `since` on, newest first.
#[derive(Clone, Debug)]
pub struct RecentShadowEvents {
    /// The device.
    pub device: DeviceId,
    /// The earliest `ts`, in wall seconds.
    pub since: f64,
    /// The most rows.
    pub limit: u32,
}

impl ReadQuery for RecentShadowEvents {
    type Output = Vec<StoredShadowEvent>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<StoredShadowEvent>, RecordError> {
        let mut statement = conn.prepare_cached(
            "SELECT id, ts, rule_key, kind, peak_raw, raws FROM wake_shadow_events \
             WHERE device_id = ?1 AND ts >= ?2 ORDER BY ts DESC, id DESC LIMIT ?3",
        )?;
        let rows = statement.query_map(params![self.device.0, self.since, self.limit], |row| {
            Ok(StoredShadowEvent {
                id: row.get(0)?,
                ts: row.get(1)?,
                rule_key: row.get(2)?,
                kind: row.get(3)?,
                peak_raw: row.get(4)?,
                raws: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// One clip an archive across devices holds: the row it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct ArchiveClip {
    /// The row's device.
    pub device_id: String,
    /// The row id that names the file.
    pub id: i64,
    /// The row's `ts`, in wall seconds.
    pub ts: f64,
}

fn archive_clip(row: &Row<'_>) -> rusqlite::Result<ArchiveClip> {
    Ok(ArchiveClip {
        device_id: row.get(0)?,
        id: row.get(1)?,
        ts: row.get(2)?,
    })
}

/// The terminal reasons of a refused wake candidate, as the dashboard counts them
/// (dashboard.jsx, `REFUSED_WAKE`): its wake was never confirmed.
pub const REFUSED_WAKE: [TerminalReason; 5] = [
    TerminalReason::SelfOutput,
    TerminalReason::EchoOnly,
    TerminalReason::UnverifiedWake,
    TerminalReason::VerifierTimeout,
    TerminalReason::ArbitrationLost,
];

/// The turns whose wake clip was saved, whose wake was confirmed (not refused) and whose
/// `wake_score` lies in `[min, max]`, of one device or every device, in time order.
#[derive(Clone, Debug)]
pub struct ConfirmedWakeClips {
    /// One device's, or every device's.
    pub scope: ListScope,
    /// The lowest wake score.
    pub min: f64,
    /// The highest wake score.
    pub max: f64,
}

impl ReadQuery for ConfirmedWakeClips {
    type Output = Vec<ArchiveClip>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<ArchiveClip>, RecordError> {
        let refused: Vec<String> = REFUSED_WAKE
            .iter()
            .map(|r| format!("'{}'", r.as_str()))
            .collect();
        let mut statement = conn.prepare_cached(&format!(
            "SELECT device_id, id, ts FROM turns WHERE (?1 IS NULL OR device_id = ?1) \
             AND wake_file IS NOT NULL AND wake_score BETWEEN ?2 AND ?3 \
             AND (terminal_reason IS NULL OR terminal_reason NOT IN ({})) ORDER BY ts, id",
            refused.join(", ")
        ))?;
        let rows = statement.query_map(
            params![self.scope.device(), self.min, self.max],
            archive_clip,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// The near misses whose recording was saved, whose `peak` lies in `[min, max]` and, when
/// `verdict` is given, that are marked with it, of one device or every device, in time
/// order.
#[derive(Clone, Debug)]
pub struct SavedNearMissRecordings {
    /// One device's, or every device's.
    pub scope: ListScope,
    /// The lowest peak.
    pub min: f64,
    /// The highest peak.
    pub max: f64,
    /// Only the near misses marked with this.
    pub verdict: Option<NearMissVerdict>,
}

impl ReadQuery for SavedNearMissRecordings {
    type Output = Vec<ArchiveClip>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<ArchiveClip>, RecordError> {
        let mut statement = conn.prepare_cached(
            "SELECT device_id, id, ts FROM near_misses WHERE (?1 IS NULL OR device_id = ?1) \
             AND recording = 'saved' AND peak BETWEEN ?2 AND ?3 \
             AND (?4 IS NULL OR verdict = ?4) ORDER BY ts, id",
        )?;
        let rows = statement.query_map(
            params![
                self.scope.device(),
                self.min,
                self.max,
                self.verdict.map(NearMissVerdict::as_str)
            ],
            archive_clip,
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// The requests Home Assistant answered (`outcome` `ha`, `completed`, an `action_done` or
/// `query_answer` response), newest first and each text once, as they were sent: a
/// correction's text where one was applied (`stt_fix.applied`, `stt_fix.sent`), the
/// transcript otherwise. A shadow proposal is never one of them. Every device's, for
/// transcript correction's known phrases (spec §4.7).
#[derive(Clone, Copy, Debug)]
pub struct AnsweredRequests {
    /// The most texts returned.
    pub limit: u32,
}

impl ReadQuery for AnsweredRequests {
    type Output = Vec<String>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Vec<String>, RecordError> {
        let mut statement = conn.prepare(
            "SELECT stt_text, stt_fix FROM turns \
             WHERE outcome = 'ha' AND terminal_reason = 'completed' \
             AND response_type IN ('action_done', 'query_answer') AND stt_text IS NOT NULL \
             ORDER BY id DESC",
        )?;
        let rows = statement.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        let mut texts: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            let (heard, fix) = row?;
            let applied = fix
                .and_then(|fix| serde_json::from_str::<serde_json::Value>(&fix).ok())
                .filter(|fix| fix["applied"] == true)
                .and_then(|fix| fix["sent"].as_str().map(str::to_owned));
            let text = applied.unwrap_or(heard);
            let text = text.trim().to_owned();
            if !text.is_empty() && seen.insert(text.to_lowercase()) {
                texts.push(text);
                if texts.len() >= self.limit as usize {
                    break;
                }
            }
        }
        Ok(texts)
    }
}
