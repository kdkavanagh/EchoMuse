//! `near_misses`: one row per `wake.near_miss` (protocol-v1 §4.4), always written; its
//! recording only while the speaker's `saveWakeClips` is on.
//!
//! # Column encodings
//!
//! | Column | Type | Encoding | NULL means |
//! |---|---|---|---|
//! | `id` | INTEGER | row id, never reused; the recording is `nearmiss_recordings/<device>_<id>.wav` | — |
//! | `device_id` | TEXT | the device serial | never NULL |
//! | `ts` | REAL | wall seconds since the epoch of the episode | never NULL |
//! | `near_miss_uuid` | TEXT | the wire `near_miss_id`; the row's operation key | never NULL |
//! | `profile` | TEXT | the wire `profile`: `idle` or `playback` | never NULL |
//! | `floor` | REAL | the wire `floor`: the near-miss threshold latched at the episode's open | never NULL |
//! | `peak` | REAL | the wire `peak`: highest 3-window smoothed score of the episode | never NULL |
//! | `wake_threshold` | REAL | the registry wake threshold of that profile, the one it missed | the graph is not in the registry |
//! | `wake_model_sha256` | TEXT | the wire `graph_sha256` | not reported |
//! | `duration_ms` | INTEGER | `(support_end − support_start) / 16`: the scored stretch | never NULL |
//! | `end_reason` | TEXT | [`NearMissEnd`] (the wire `reason`) | never NULL |
//! | `recording` | TEXT | [`NearMissRecording`] | never NULL |
//! | `audio_ms` | INTEGER | the saved WAV's length in ms, set with `recording = 'saved'` | not saved |
//! | `afe_series` | TEXT | [`AfeSeries`] JSON | no AFE metadata, no recording, or older than the newest 1,000 rows |
//! | `verdict` | TEXT | [`NearMissVerdict`] | nobody has marked it |
//! | `windows` | TEXT | [`ClipWindows`] JSON of the recording | not scored yet |
//!
//! The stored vocabulary has no "being saved" value. A near miss whose audio is being
//! published is inserted as `failed` with `audio_ms` NULL and becomes `saved` only once its
//! WAV exists ([`MarkNearMissSaved`]); `failed` is exactly what remains if the controller
//! stops before that.

use std::time::Duration;

use echomuse_core::types::DeviceId;
use echomuse_db::{ReadQuery, WriteCommand};
use echomuse_protocol::vocab::Sha256;
use echomuse_turn::WakeProfile;
use rusqlite::{Connection, OptionalExtension, Row, Transaction, params};

use crate::{
    error::RecordError,
    values::{AfeSeries, ClipWindows, NearMissUuid, Score, WallSeconds, whole_ms},
    vocab::{NearMissEnd, NearMissRecording, NearMissVerdict, Stored, parse_profile, profile_str},
};

/// How many unmarked near misses each device keeps (configuration.md, "Near-miss floor").
/// Marked ones are all kept.
pub const NEAR_MISS_ROW_RETENTION: u32 = 2_000;
/// How many of a device's newest near misses keep `afe_series` (afe-metadata.md).
pub const NEAR_MISS_SERIES_RETENTION: u32 = 1_000;

/// A `near_misses` row id.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NearMissRowId(pub i64);

/// One `near_misses` row as the controller writes it. `recording`, `audio_ms`, `verdict` and
/// `windows` are set by their own commands.
#[derive(Clone, Debug, PartialEq)]
pub struct NearMissRecord {
    /// `device_id`.
    pub device: DeviceId,
    /// `ts`.
    pub ts: WallSeconds,
    /// `near_miss_uuid`.
    pub uuid: NearMissUuid,
    /// `profile`.
    pub profile: WakeProfile,
    /// `floor`.
    pub floor: Score,
    /// `peak`.
    pub peak: Score,
    /// `wake_threshold`.
    pub wake_threshold: Option<Score>,
    /// `wake_model_sha256`.
    pub wake_model_sha256: Option<Sha256>,
    /// `duration_ms`.
    pub duration: Duration,
    /// `end_reason`.
    pub end_reason: NearMissEnd,
    /// `afe_series`.
    pub afe_series: Option<AfeSeries>,
}

/// `duration_ms` of an episode from the wire's `support_start` and `support_end` samples.
pub fn support_duration(support_start: u64, support_end: u64) -> Duration {
    Duration::from_millis(support_end.saturating_sub(support_start) / 16)
}

/// The result of [`InsertNearMiss`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NearMissInsert {
    /// The row: the new one, or the one an earlier completion wrote.
    pub id: NearMissRowId,
    /// False when the key already had a row, which is left as it was.
    pub inserted: bool,
    /// Unmarked rows of the device deleted to keep `row_retention`. Their recordings are
    /// removed by the next file retention pass.
    pub pruned: Vec<NearMissRowId>,
}

/// Inserts a near miss once per `near_miss_uuid`, then keeps the device's newest
/// `row_retention` unmarked rows (and every marked one) and clears `afe_series` past the
/// newest `series_retention`.
#[derive(Clone, Debug)]
pub struct InsertNearMiss {
    /// The row.
    pub record: NearMissRecord,
    /// Its initial `recording`: `off`, `not_permitted` or `failed`. `saved` is set only
    /// by [`MarkNearMissSaved`] once the WAV exists.
    pub recording: NearMissRecording,
    /// See [`NEAR_MISS_ROW_RETENTION`].
    pub row_retention: u32,
    /// See [`NEAR_MISS_SERIES_RETENTION`].
    pub series_retention: u32,
}

impl InsertNearMiss {
    /// Inserts with the standard retention.
    pub fn new(record: NearMissRecord, recording: NearMissRecording) -> Self {
        Self {
            record,
            recording,
            row_retention: NEAR_MISS_ROW_RETENTION,
            series_retention: NEAR_MISS_SERIES_RETENTION,
        }
    }
}

impl WriteCommand for InsertNearMiss {
    type Output = NearMissInsert;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<NearMissInsert, RecordError> {
        if self.recording == NearMissRecording::Saved {
            return Err(RecordError::Invalid(
                "a near miss becomes saved only once its recording exists",
            ));
        }
        let r = &self.record;
        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, device_id FROM near_misses WHERE near_miss_uuid = ?1 \
                 ORDER BY id LIMIT 1",
                [r.uuid.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((id, device)) = existing {
            if device != r.device.0 {
                return Err(RecordError::KeyConflict {
                    table: "near_misses",
                    key: r.uuid.to_string(),
                });
            }
            return Ok(NearMissInsert {
                id: NearMissRowId(id),
                inserted: false,
                pruned: Vec::new(),
            });
        }
        tx.execute(
            "INSERT INTO near_misses (device_id, ts, near_miss_uuid, profile, floor, peak, \
             wake_threshold, wake_model_sha256, duration_ms, end_reason, recording, audio_ms, \
             afe_series, verdict, windows) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, ?12, NULL, NULL)",
            params![
                r.device.0,
                r.ts.get(),
                r.uuid.as_str(),
                profile_str(r.profile),
                r.floor.get(),
                r.peak.get(),
                r.wake_threshold.map(Score::get),
                r.wake_model_sha256.as_ref().map(Sha256::as_str),
                whole_ms(r.duration),
                r.end_reason.as_str(),
                self.recording.as_str(),
                r.afe_series.as_ref().map(AfeSeries::as_str),
            ],
        )?;
        let id = NearMissRowId(tx.last_insert_rowid());
        let pruned = prune_near_misses(tx, &r.device, self.row_retention)?;
        prune_near_miss_series(tx, &r.device, self.series_retention)?;
        Ok(NearMissInsert {
            id,
            inserted: true,
            pruned,
        })
    }
}

/// Deletes a device's unmarked near misses past its newest `keep` unmarked ones.
pub(crate) fn prune_near_misses(
    tx: &Connection,
    device: &DeviceId,
    keep: u32,
) -> rusqlite::Result<Vec<NearMissRowId>> {
    let mut statement = tx.prepare(
        "DELETE FROM near_misses WHERE device_id = ?1 AND verdict IS NULL AND id IN \
         (SELECT id FROM near_misses WHERE device_id = ?1 AND verdict IS NULL \
          ORDER BY id DESC LIMIT -1 OFFSET ?2) RETURNING id",
    )?;
    let ids = statement
        .query_map(params![device.0, i64::from(keep)], |r| {
            r.get(0).map(NearMissRowId)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids)
}

/// Clears `afe_series` of a device's near misses past its newest `keep`.
pub(crate) fn prune_near_miss_series(
    tx: &Connection,
    device: &DeviceId,
    keep: u32,
) -> rusqlite::Result<usize> {
    tx.execute(
        "UPDATE near_misses SET afe_series = NULL \
         WHERE device_id = ?1 AND afe_series IS NOT NULL AND id < \
         (SELECT id FROM near_misses WHERE device_id = ?1 ORDER BY id DESC \
          LIMIT 1 OFFSET ?2)",
        params![device.0, i64::from(keep.max(1)) - 1],
    )
}

/// Applies the near-miss row and series retention to one device, as [`InsertNearMiss`] does
/// after each insert; startup runs it so a changed limit takes effect.
#[derive(Clone, Debug)]
pub struct PruneNearMisses {
    /// The device.
    pub device: DeviceId,
    /// See [`NEAR_MISS_ROW_RETENTION`].
    pub row_retention: u32,
    /// See [`NEAR_MISS_SERIES_RETENTION`].
    pub series_retention: u32,
}

impl WriteCommand for PruneNearMisses {
    type Output = Vec<NearMissRowId>;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<Vec<NearMissRowId>, RecordError> {
        let pruned = prune_near_misses(tx, &self.device, self.row_retention)?;
        prune_near_miss_series(tx, &self.device, self.series_retention)?;
        Ok(pruned)
    }
}

/// Marks a near miss's recording saved once its WAV exists. False when the row is gone or
/// was not waiting for its recording (`failed`).
#[derive(Clone, Debug)]
pub struct MarkNearMissSaved {
    /// The row.
    pub id: NearMissRowId,
    /// The WAV's length, stored as `audio_ms`.
    pub audio: Duration,
}

impl WriteCommand for MarkNearMissSaved {
    type Output = bool;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, RecordError> {
        Ok(tx.execute(
            "UPDATE near_misses SET recording = 'saved', audio_ms = ?2 \
             WHERE id = ?1 AND recording = 'failed'",
            params![self.id.0, whole_ms(self.audio)],
        )? > 0)
    }
}

/// Stores a scored recording's windows. False when the row is gone.
#[derive(Clone, Debug)]
pub struct SetNearMissWindows {
    /// The row.
    pub id: NearMissRowId,
    /// The windows.
    pub windows: ClipWindows,
}

impl WriteCommand for SetNearMissWindows {
    type Output = bool;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<bool, RecordError> {
        Ok(tx.execute(
            "UPDATE near_misses SET windows = ?2 WHERE id = ?1",
            params![self.id.0, self.windows.to_json()],
        )? > 0)
    }
}

/// The result of [`SetNearMissVerdict`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerdictUpdate {
    /// The verdict was stored (or cleared).
    Stored,
    /// No near miss of this device has the id.
    NoSuchNearMiss,
    /// Only a near miss with a saved recording can be marked (configuration.md, "Marking
    /// near misses"); clearing is always allowed.
    NotRecorded,
}

/// Marks a near miss, or clears its mark with None.
#[derive(Clone, Debug)]
pub struct SetNearMissVerdict {
    /// The near miss's device.
    pub device: DeviceId,
    /// The row.
    pub id: NearMissRowId,
    /// The verdict.
    pub verdict: Option<NearMissVerdict>,
}

impl WriteCommand for SetNearMissVerdict {
    type Output = VerdictUpdate;
    type Error = RecordError;

    fn apply(self, tx: &Transaction<'_>) -> Result<VerdictUpdate, RecordError> {
        let recording: Option<String> = tx
            .query_row(
                "SELECT recording FROM near_misses WHERE id = ?1 AND device_id = ?2",
                params![self.id.0, self.device.0],
                |r| r.get(0),
            )
            .optional()?;
        let Some(recording) = recording else {
            return Ok(VerdictUpdate::NoSuchNearMiss);
        };
        if self.verdict.is_some() && recording != NearMissRecording::Saved.as_str() {
            return Ok(VerdictUpdate::NotRecorded);
        }
        tx.execute(
            "UPDATE near_misses SET verdict = ?2 WHERE id = ?1",
            params![self.id.0, self.verdict.map(NearMissVerdict::as_str)],
        )?;
        Ok(VerdictUpdate::Stored)
    }
}

/// One `near_misses` row as stored, every column.
#[derive(Clone, Debug, PartialEq)]
#[allow(missing_docs)]
pub struct StoredNearMiss {
    pub id: NearMissRowId,
    pub device_id: String,
    pub ts: f64,
    pub near_miss_uuid: String,
    pub profile: Stored<WakeProfile>,
    pub floor: f64,
    pub peak: f64,
    pub wake_threshold: Option<f64>,
    pub wake_model_sha256: Option<String>,
    pub duration_ms: i64,
    pub end_reason: Stored<NearMissEnd>,
    pub recording: Stored<NearMissRecording>,
    pub audio_ms: Option<i64>,
    pub afe_series: Option<String>,
    pub verdict: Option<Stored<NearMissVerdict>>,
    pub windows: Option<String>,
}

fn stored_near_miss(row: &Row<'_>) -> rusqlite::Result<StoredNearMiss> {
    Ok(StoredNearMiss {
        id: NearMissRowId(row.get("id")?),
        device_id: row.get("device_id")?,
        ts: row.get("ts")?,
        near_miss_uuid: row.get("near_miss_uuid")?,
        profile: Stored::read(row.get("profile")?, parse_profile),
        floor: row.get("floor")?,
        peak: row.get("peak")?,
        wake_threshold: row.get("wake_threshold")?,
        wake_model_sha256: row.get("wake_model_sha256")?,
        duration_ms: row.get("duration_ms")?,
        end_reason: Stored::read(row.get("end_reason")?, NearMissEnd::parse),
        recording: Stored::read(row.get("recording")?, NearMissRecording::parse),
        audio_ms: row.get("audio_ms")?,
        afe_series: row.get("afe_series")?,
        verdict: row
            .get::<_, Option<String>>("verdict")?
            .map(|v| Stored::read(v, NearMissVerdict::parse)),
        windows: row.get("windows")?,
    })
}

/// Reads one near miss by id.
#[derive(Clone, Copy, Debug)]
pub struct GetNearMiss(pub NearMissRowId);

impl ReadQuery for GetNearMiss {
    type Output = Option<StoredNearMiss>;
    type Error = RecordError;

    fn read(self, conn: &Connection) -> Result<Option<StoredNearMiss>, RecordError> {
        Ok(conn
            .query_row(
                "SELECT * FROM near_misses WHERE id = ?1",
                [self.0.0],
                stored_near_miss,
            )
            .optional()?)
    }
}
