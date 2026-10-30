//! A turn's and a near miss's recordings, and archives of the saved clips (§3.4, §3.5.2).
//!
//! | Route | Who | Answer |
//! |---|---|---|
//! | `GET /api/devices/{id}/turns/{turn}/wake` | signed in | the wake clip, `audio/wav` |
//! | `GET /api/devices/{id}/turns/{turn}/audio` | signed in | the utterance STT received |
//! | `GET /api/devices/{id}/turns/{turn}/recording` | signed in | the turn recording |
//! | `GET /api/devices/{id}/near_misses/{nm}/recording` | signed in | the near miss's recording |
//! | `GET /api/devices/{id}/wakeclips.zip` | signed in | every wake clip the device still holds |
//! | `GET /api/wakeclips.zip?min=&max=&device=` | signed in | the wake clip of every confirmed wake scored in the range |
//! | `GET /api/near_misses.zip?min=&max=&device=&verdict=` | signed in | the recording of every near miss whose peak is in the range |
//!
//! A recording its row never saved, or one retention has removed since, answers 404, which
//! the dashboard takes as "no longer kept" (dashboard.jsx, `TurnObservability`'s
//! `markGone`). Files are streamed from disk, never read whole into memory.
//!
//! An archive names each clip as the clip's own download does: the device's slug, then the
//! wake's UTC time and `-wake` (`kitchen-2026-10-26232503-wake.wav`; dashboard.jsx,
//! `downloadAudio`) or `-nearmiss` and the row id (`kitchen-nearmiss446.wav`,
//! `downloadNear`), a repeated name numbered `(1)` as a browser would (configuration.md
//! "Activity across every device"). Clips are in time order, each dated by its row. An
//! archive with nothing to hold answers 404, which the dashboard reports as "no saved clip
//! matches".
//!
//! The archives across devices take `min` and `max` (0 to 1, both included; a wake by its
//! `wake_score`, a near miss by its `peak`), optionally `device`, and for near misses
//! `verdict`. A confirmed wake is a wake turn the dashboard does not count as refused
//! (`REFUSED_WAKE`).
//!
//! **Crops** (dashboard.jsx, `FleetActivity`'s `cut`). With `crop` (seconds, 0.1 to 10) an
//! archive holds a fixed length of each clip instead of the whole clip, cut around the
//! wake model's windows the controller scored over the clip ([`ClipScoring`]); a clip it
//! has not reached yet is scored first, so a cropped archive may take a moment. Without
//! `window_min` each clip gives one crop, centred on the middle of its highest-scoring
//! window (the first of equals), named as the whole clip is. With `window_min` (0 to 1)
//! each window that scored at least that gives one crop, named after the clip with the
//! window's start in milliseconds added (`kitchen-2026-10-26232503-wake-480ms.wav`), so a
//! training split can keep a clip's crops together. A crop that would run past either end
//! of its clip is moved inside it; a clip shorter than the crop is taken whole. A clip
//! that cannot be scored (its wake model is gone) is left out. With nothing to hold, the
//! 404 says whether no clip could be scored or no window reached `window_min`.
//!
//! [`ClipScoring`]: crate::diagnostics::ClipScoring

use std::collections::{HashMap, HashSet};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use echomuse_core::types::DeviceId;
use echomuse_db::ReadQuery;
use echomuse_records::{
    ArchiveClip, ClipWindows, ConfirmedWakeClips, ListScope, RecordingKind, RecordingsLayout,
    SavedNearMissRecordings, WakeClipRows,
    vocab::{NearMissRecording, NearMissVerdict},
};
use rusqlite::Connection;
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use crate::{
    activity::{RowPath, known_device, near_miss_of_device, recording_path, turn_of_device},
    auth::AuthenticatedUser,
    device_services::service_error,
    diagnostics::{ClipKind, ClipRef},
    error::{ApiError, ErrorCode},
    params::{DevicePath, QueryParams},
    state::ApiState,
    zip_writer::{DosTime, SampleSpan, ZipEntry, ZipPart, ZipPlan, unique_name, zip_download},
};

pub(crate) fn routes() -> Router<ApiState> {
    Router::new()
        .route("/api/devices/{id}/turns/{row}/wake", get(wake_clip))
        .route("/api/devices/{id}/turns/{row}/audio", get(utterance))
        .route(
            "/api/devices/{id}/turns/{row}/recording",
            get(turn_recording),
        )
        .route(
            "/api/devices/{id}/near_misses/{row}/recording",
            get(near_miss_recording),
        )
        .route("/api/devices/{id}/wakeclips.zip", get(wake_clips_zip))
        .route(
            "/api/devices/{id}/wakeclips",
            axum::routing::delete(delete_wake_clips),
        )
        .route("/api/wakeclips.zip", get(confirmed_wakes_zip))
        .route("/api/near_misses.zip", get(near_misses_zip))
}

fn not_kept() -> ApiError {
    ApiError::no_such("This recording is not kept")
}

/// A device's name in file names, by the dashboard's rule (dashboard.jsx, `fileSlug`): the
/// label, or the id without one, with every run of other characters than ASCII letters and
/// digits made one `-`, trimmed of `-` and lowercased; `device` when nothing is left.
pub fn file_slug(label: Option<&str>, device: &DeviceId) -> String {
    let source = label.filter(|l| !l.is_empty()).unwrap_or(&device.0);
    let mut slug = String::with_capacity(source.len());
    let mut gap = false;
    for c in source.chars() {
        if c.is_ascii_alphanumeric() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c.to_ascii_lowercase());
        } else {
            gap = true;
        }
    }
    if slug.is_empty() {
        "device".to_owned()
    } else {
        slug
    }
}

/// A wake's time as the clip's download names it: `YYYY-MM-DDHHMMSS`, UTC, whole seconds
/// (dashboard.jsx, `downloadAudio`: the ISO time without `T` and `:`).
pub fn download_time(ts: f64) -> String {
    let seconds = ts.floor() as i64;
    let (year, month, day) = crate::zip_writer::civil_from_days(seconds.div_euclid(86_400));
    let of_day = seconds.rem_euclid(86_400);
    format!(
        "{year:04}-{month:02}-{day:02}{:02}{:02}{:02}",
        of_day / 3_600,
        of_day / 60 % 60,
        of_day % 60
    )
}

/// Streams a WAV file, or 404 when it is not there.
async fn serve_wav(path: Option<std::path::PathBuf>) -> Result<Response, ApiError> {
    let path = path.ok_or_else(not_kept)?;
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_kept()),
        Err(e) => {
            tracing::error!(error = %e, "recording {} could not be opened", path.display());
            return Err(ApiError::internal("The recording could not be read"));
        }
    };
    let length = file
        .metadata()
        .await
        .map_err(|_| ApiError::internal("The recording could not be read"))?
        .len();
    let mut response = Body::from_stream(ReaderStream::new(file)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/wav"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    Ok(response)
}

/// Which of a turn's recordings.
#[derive(Clone, Copy)]
enum TurnClip {
    Wake,
    Utterance,
    Recording,
}

async fn turn_clip(state: &ApiState, path: RowPath, clip: TurnClip) -> Result<Response, ApiError> {
    let turn = turn_of_device(state, &path).await?;
    // The utterance and the wake clip are saved exactly when their column is set; the turn
    // recording has no column and is saved exactly when its file exists.
    let (kind, saved) = match clip {
        TurnClip::Wake => (RecordingKind::WakeClip, turn.wake_file.is_some()),
        TurnClip::Utterance => (RecordingKind::Utterance, turn.audio_file.is_some()),
        TurnClip::Recording => (RecordingKind::TurnRecording, true),
    };
    if !saved {
        return Err(not_kept());
    }
    let layout = state.records().layout();
    serve_wav(recording_path(layout, kind, &path.device, path.row)).await
}

async fn wake_clip(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Response, ApiError> {
    turn_clip(&state, path, TurnClip::Wake).await
}

async fn utterance(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Response, ApiError> {
    turn_clip(&state, path, TurnClip::Utterance).await
}

async fn turn_recording(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Response, ApiError> {
    turn_clip(&state, path, TurnClip::Recording).await
}

async fn near_miss_recording(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    path: RowPath,
) -> Result<Response, ApiError> {
    let near_miss = near_miss_of_device(&state, &path).await?;
    if near_miss.recording.known() != Some(&NearMissRecording::Saved) {
        return Err(not_kept());
    }
    let layout = state.records().layout();
    serve_wav(recording_path(
        layout,
        RecordingKind::NearMiss,
        &path.device,
        path.row,
    ))
    .await
}

/// What kind of clip an archive holds.
#[derive(Clone, Copy)]
enum ArchiveKind {
    WakeClips,
    NearMisses,
}

/// One clip to archive: its row, and its device's slug.
struct ArchiveRow {
    device: DeviceId,
    slug: String,
    clip: ArchiveClip,
}

/// The crops of each clip, by row id; a clip without an entry is left out.
type Crops = HashMap<i64, Vec<Crop>>;

/// Streams the clips of `rows` still on disk as `filename`, whole, or as `crops` when
/// given; 404 with `none` when nothing is left.
async fn archive(
    state: &ApiState,
    kind: ArchiveKind,
    rows: Vec<ArchiveRow>,
    crops: Option<Crops>,
    filename: String,
    none: &'static str,
) -> Result<Response, ApiError> {
    let layout: RecordingsLayout = state.records().layout().clone();
    // Which clips are still on disk, and how large: a file check per clip, off the runtime.
    let (parts, plan) = tokio::task::spawn_blocking(move || {
        let mut taken = HashSet::new();
        let mut plan = ZipPlan::default();
        let mut parts = Vec::new();
        for row in rows {
            let (recording, stem) = match kind {
                ArchiveKind::WakeClips => (
                    RecordingKind::WakeClip,
                    format!("{}-{}-wake", row.slug, download_time(row.clip.ts)),
                ),
                ArchiveKind::NearMisses => (
                    RecordingKind::NearMiss,
                    format!("{}-nearmiss{}", row.slug, row.clip.id),
                ),
            };
            let Some(path) = recording_path(&layout, recording, &row.device, row.clip.id) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let time = DosTime::from_unix(row.clip.ts.floor() as i64);
            let pieces: Vec<(String, Option<SampleSpan>, u64)> = match &crops {
                None => vec![(format!("{stem}.wav"), None, metadata.len())],
                Some(crops) => crops
                    .get(&row.clip.id)
                    .into_iter()
                    .flatten()
                    .map(|crop| {
                        let name = match crop.at_ms {
                            None => format!("{stem}.wav"),
                            Some(ms) => format!("{stem}-{ms}ms.wav"),
                        };
                        (name, Some(crop.span), crop.span.wav_size())
                    })
                    .collect(),
            };
            for (name, span, size) in pieces {
                let name = unique_name(&mut taken, &name);
                plan.add(&name, size);
                parts.push(ZipPart {
                    entry: ZipEntry {
                        name,
                        path: path.clone(),
                        time,
                    },
                    span,
                });
            }
        }
        (parts, plan)
    })
    .await
    .map_err(|_| ApiError::internal("Listing the clips failed"))?;
    if parts.is_empty() {
        return Err(ApiError::no_such(none));
    }
    if !plan.fits() {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::TooLarge,
            "These clips are more than one ZIP archive can hold (4 GiB); narrow the range",
        ));
    }
    Ok(zip_download(parts, &filename))
}

// ─── Crops ──────────────────────────────────────────────────────────────────────

/// Capture samples per second.
const RATE: u64 = 16_000;
/// The shortest crop, seconds.
pub const CROP_MIN_SECONDS: f64 = 0.1;
/// The longest crop, seconds.
pub const CROP_MAX_SECONDS: f64 = 10.0;

/// How an archive cuts its clips.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Cut {
    /// Each clip whole.
    Whole,
    /// `samples` around each clip's best window.
    Best { samples: u64 },
    /// `samples` around every window that scored at least `min`.
    Every { samples: u64, min: f64 },
}

/// One crop of a clip.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Crop {
    span: SampleSpan,
    /// The window's start in milliseconds, which names the crop; `None` for the one crop
    /// of a clip, named as the clip.
    at_ms: Option<u64>,
}

fn parse_cut(query: &ArchiveQuery) -> Result<Cut, ApiError> {
    let Some(crop) = query.crop.as_deref() else {
        return match query.window_min {
            None => Ok(Cut::Whole),
            Some(_) => Err(ApiError::bad_request(
                "window_min cuts crops, so it needs crop as well",
            )),
        };
    };
    let seconds: f64 = crop
        .trim()
        .parse()
        .ok()
        .filter(|s: &f64| (CROP_MIN_SECONDS..=CROP_MAX_SECONDS).contains(s))
        .ok_or_else(|| {
            ApiError::bad_request(format!(
                "crop must be {CROP_MIN_SECONDS} to {CROP_MAX_SECONDS} seconds"
            ))
        })?;
    let samples = (seconds * RATE as f64).round() as u64;
    match query.window_min.as_deref() {
        None => Ok(Cut::Best { samples }),
        Some(text) => {
            let min = text
                .trim()
                .parse()
                .ok()
                .filter(|m: &f64| (0.0..=1.0).contains(m))
                .ok_or_else(|| ApiError::bad_request("window_min must be a score from 0 to 1"))?;
            Ok(Cut::Every { samples, min })
        }
    }
}

/// `samples` centred on the middle of the window starting at `start`, moved inside the
/// clip; the whole clip when it is shorter.
fn span_around(windows: &ClipWindows, start: u64, samples: u64) -> SampleSpan {
    if samples >= windows.samples {
        return SampleSpan {
            start: 0,
            len: windows.samples,
        };
    }
    let centre = start + windows.window.min(windows.samples) / 2;
    let start = centre
        .saturating_sub(samples / 2)
        .min(windows.samples - samples);
    SampleSpan {
        start,
        len: samples,
    }
}

/// A clip's crops under `cut`.
fn crops_of(windows: &ClipWindows, cut: Cut) -> Vec<Crop> {
    match cut {
        Cut::Whole => Vec::new(),
        Cut::Best { samples } => {
            let mut best: Option<(u64, f64)> = None;
            for &(start, score) in &windows.scores {
                if best.is_none_or(|(_, top)| score > top) {
                    best = Some((start, score));
                }
            }
            best.map(|(start, _)| Crop {
                span: span_around(windows, start, samples),
                at_ms: None,
            })
            .into_iter()
            .collect()
        }
        Cut::Every { samples, min } => windows
            .scores
            .iter()
            .filter(|(_, score)| *score >= min)
            .map(|&(start, _)| Crop {
                span: span_around(windows, start, samples),
                at_ms: Some(start * 1_000 / RATE),
            })
            .collect(),
    }
}

/// The stored windows of each clip that has them.
struct StoredWindows {
    kind: ClipKind,
    ids: Vec<i64>,
}

impl ReadQuery for StoredWindows {
    type Output = HashMap<i64, ClipWindows>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<HashMap<i64, ClipWindows>> {
        let sql = match self.kind {
            ClipKind::WakeClip => "SELECT wake_windows FROM turns WHERE id = ?1",
            ClipKind::NearMiss => "SELECT windows FROM near_misses WHERE id = ?1",
        };
        let mut statement = conn.prepare_cached(sql)?;
        let mut out = HashMap::new();
        for id in self.ids {
            let text: Option<Option<String>> = statement
                .query_row([id], |row| row.get(0))
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    error => Err(error),
                })?;
            if let Some(windows) = text.flatten().as_deref().and_then(ClipWindows::parse) {
                out.insert(id, windows);
            }
        }
        Ok(out)
    }
}

/// Scores any of `rows` not scored yet, then cuts each under `cut`. 404 when no crop is
/// left, saying why.
async fn crops_for(
    state: &ApiState,
    kind: ArchiveKind,
    rows: &[ArchiveRow],
    cut: Cut,
) -> Result<Crops, ApiError> {
    let kind = match kind {
        ArchiveKind::WakeClips => ClipKind::WakeClip,
        ArchiveKind::NearMisses => ClipKind::NearMiss,
    };
    let refs: Vec<ClipRef> = rows
        .iter()
        .map(|row| ClipRef {
            kind,
            device: row.device.clone(),
            id: row.clip.id,
        })
        .collect();
    let ids = refs.iter().map(|r| r.id).collect();
    state
        .diagnostics()
        .clip_scoring
        .score_now(refs)
        .await
        .map_err(service_error)?;
    let windows = state.db().read(StoredWindows { kind, ids }).await?;
    let crops: Crops = windows
        .iter()
        .map(|(id, windows)| (*id, crops_of(windows, cut)))
        .filter(|(_, crops)| !crops.is_empty())
        .collect();
    if crops.is_empty() {
        return Err(match (windows.is_empty(), cut) {
            (false, Cut::Every { min, .. }) => ApiError::no_such(format!(
                "No window of the matching clips scored {min:.2} or more; lower the window score"
            )),
            _ => ApiError::no_such(
                "None of the matching clips could be scored, so none can be cropped",
            ),
        });
    }
    Ok(crops)
}

/// `DELETE …/wakeclips`: refused. The records service has no command that deletes a
/// device's wake clips while keeping their turn rows consistent with publication and
/// retention (§3.5.2), and the shipped dashboard never calls it; retention keeps each
/// device's newest 1,000, and deleting the device removes them all. 501, so the caller
/// is told rather than given a 404.
async fn delete_wake_clips(
    State(state): State<ApiState>,
    crate::auth::AdminUser(_): crate::auth::AdminUser,
    DevicePath(device): DevicePath,
) -> Result<Response, ApiError> {
    known_device(&state, &device).await?;
    Err(ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        ErrorCode::NotApplicable,
        "Deleting a device's wake clips is not built in this controller; retention keeps the \
         newest 1,000, and deleting the device removes them",
    ))
}

/// `GET …/wakeclips.zip`: every wake clip of the device still on disk, refused candidates'
/// included, streamed as a ZIP (dashboard.jsx, `doWakeZip`: "every wake clip the device
/// still holds"). 404 when none is kept.
async fn wake_clips_zip(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    DevicePath(device): DevicePath,
) -> Result<Response, ApiError> {
    let row = known_device(&state, &device).await?;
    let slug = file_slug(row.label.as_deref(), &device);
    let clips = state
        .db()
        .read(WakeClipRows {
            device: device.clone(),
        })
        .await?;
    let rows = clips
        .into_iter()
        .map(|clip| ArchiveRow {
            device: device.clone(),
            slug: slug.clone(),
            clip: ArchiveClip {
                device_id: device.0.clone(),
                id: clip.id.0,
                ts: clip.ts,
            },
        })
        .collect();
    archive(
        &state,
        ArchiveKind::WakeClips,
        rows,
        None,
        format!("{slug}-wakeclips.zip"),
        "No wake clip is kept for this device",
    )
    .await
}

/// The query of an archive across devices (dashboard.jsx, `FleetActivity`'s `download`).
#[derive(Deserialize)]
struct ArchiveQuery {
    min: Option<f64>,
    max: Option<f64>,
    device: Option<String>,
    verdict: Option<String>,
    crop: Option<String>,
    window_min: Option<String>,
}

/// Every device's label, by id.
struct DeviceLabels;

impl ReadQuery for DeviceLabels {
    type Output = HashMap<String, Option<String>>;
    type Error = rusqlite::Error;

    fn read(self, conn: &Connection) -> rusqlite::Result<HashMap<String, Option<String>>> {
        conn.prepare_cached("SELECT device_id, label FROM devices")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }
}

/// The checked range, scope and cut of an archive query.
struct ArchiveFilter {
    min: f64,
    max: f64,
    scope: ListScope,
    cut: Cut,
}

async fn archive_filter(state: &ApiState, query: &ArchiveQuery) -> Result<ArchiveFilter, ApiError> {
    let cut = parse_cut(query)?;
    let (min, max) = (query.min.unwrap_or(0.0), query.max.unwrap_or(1.0));
    if !(0.0..=1.0).contains(&min) || !(0.0..=1.0).contains(&max) || min > max {
        return Err(ApiError::bad_request(
            "min and max must be scores from 0 to 1, min no more than max",
        ));
    }
    let scope = match &query.device {
        None => ListScope::Fleet,
        Some(id) => {
            let device = DeviceId(id.clone());
            known_device(state, &device).await?;
            ListScope::Device(device)
        }
    };
    Ok(ArchiveFilter {
        min,
        max,
        scope,
        cut,
    })
}

async fn labelled(state: &ApiState, clips: Vec<ArchiveClip>) -> Result<Vec<ArchiveRow>, ApiError> {
    let labels = state.db().read(DeviceLabels).await?;
    Ok(clips
        .into_iter()
        .map(|clip| {
            let device = DeviceId(clip.device_id.clone());
            let label = labels.get(&clip.device_id).and_then(Option::as_deref);
            ArchiveRow {
                slug: file_slug(label, &device),
                device,
                clip,
            }
        })
        .collect())
}

/// `GET /api/wakeclips.zip`: the wake clip of every confirmed wake whose wake score is in
/// the range, of one device or every device.
async fn confirmed_wakes_zip(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    QueryParams(query): QueryParams<ArchiveQuery>,
) -> Result<Response, ApiError> {
    if query.verdict.is_some() {
        return Err(ApiError::bad_request("Wake clips have no verdict"));
    }
    let filter = archive_filter(&state, &query).await?;
    let clips = state
        .db()
        .read(ConfirmedWakeClips {
            scope: filter.scope,
            min: filter.min,
            max: filter.max,
        })
        .await?;
    let rows = labelled(&state, clips).await?;
    let crops = cropped(&state, ArchiveKind::WakeClips, &rows, filter.cut).await?;
    archive(
        &state,
        ArchiveKind::WakeClips,
        rows,
        crops,
        "echomuse-wakes.zip".to_owned(),
        "No saved wake clip matches",
    )
    .await
}

/// `GET /api/near_misses.zip`: the recording of every near miss whose peak is in the range,
/// marked with `verdict` when one is given, of one device or every device.
async fn near_misses_zip(
    State(state): State<ApiState>,
    _user: AuthenticatedUser,
    QueryParams(query): QueryParams<ArchiveQuery>,
) -> Result<Response, ApiError> {
    let verdict = match query.verdict.as_deref() {
        None => None,
        Some(word) => Some(NearMissVerdict::parse(word).ok_or_else(|| {
            ApiError::bad_request("verdict must be missed_wake or correctly_ignored")
        })?),
    };
    let filter = archive_filter(&state, &query).await?;
    let clips = state
        .db()
        .read(SavedNearMissRecordings {
            scope: filter.scope,
            min: filter.min,
            max: filter.max,
            verdict,
        })
        .await?;
    let rows = labelled(&state, clips).await?;
    let crops = cropped(&state, ArchiveKind::NearMisses, &rows, filter.cut).await?;
    archive(
        &state,
        ArchiveKind::NearMisses,
        rows,
        crops,
        "echomuse-nearmisses.zip".to_owned(),
        "No saved near-miss recording matches",
    )
    .await
}

/// The crops of `rows` under `cut`; `None` for whole clips, or when nothing matched (the
/// archive then says so).
async fn cropped(
    state: &ApiState,
    kind: ArchiveKind,
    rows: &[ArchiveRow],
    cut: Cut,
) -> Result<Option<Crops>, ApiError> {
    if cut == Cut::Whole || rows.is_empty() {
        return Ok(None);
    }
    crops_for(state, kind, rows, cut).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_the_dashboard() {
        let id = DeviceId("G0SYNTHETIC00001".to_owned());
        assert_eq!(file_slug(Some("Kitchen Echo!"), &id), "kitchen-echo");
        assert_eq!(file_slug(Some("  --Living  room-- "), &id), "living-room");
        assert_eq!(file_slug(None, &id), "g0synthetic00001");
        assert_eq!(file_slug(Some(""), &id), "g0synthetic00001");
        assert_eq!(file_slug(Some("ÉÉ"), &id), "device");
    }

    #[test]
    fn download_times_match_the_documented_name() {
        // configuration.md: kitchen-2026-10-26232503-wake.wav.
        assert_eq!(download_time(1_793_057_103.75), "2026-10-26232503");
    }

    fn windows(samples: u64, scores: &[(u64, f64)]) -> ClipWindows {
        ClipWindows {
            graph_sha256: echomuse_protocol::Sha256::parse(&"4e".repeat(32)).unwrap(),
            samples,
            window: 22_400,
            scores: scores.to_vec(),
        }
    }

    #[test]
    fn the_best_crop_is_centred_on_the_best_window_and_kept_inside_the_clip() {
        // A real clip's layout: 32,000 samples, windows every 2,560, the last at the end.
        let w = windows(
            32_000,
            &[
                (0, 0.01),
                (2_560, 0.02),
                (5_120, 0.35),
                (7_680, 0.41),
                (9_600, 0.26),
            ],
        );
        let crops = crops_of(&w, Cut::Best { samples: 22_400 });
        assert_eq!(
            crops,
            [Crop {
                span: SampleSpan {
                    start: 7_680,
                    len: 22_400
                },
                at_ms: None,
            }]
        );
        // A crop longer than the window is moved inside the clip at the end...
        let crops = crops_of(&w, Cut::Best { samples: 30_000 });
        assert_eq!(
            crops[0].span,
            SampleSpan {
                start: 2_000,
                len: 30_000
            }
        );
        // ...and at the start, and a crop longer than the clip takes it whole.
        let early = windows(32_000, &[(0, 0.9), (9_600, 0.1)]);
        let crops = crops_of(&early, Cut::Best { samples: 30_000 });
        assert_eq!(
            crops[0].span,
            SampleSpan {
                start: 0,
                len: 30_000
            }
        );
        let crops = crops_of(&early, Cut::Best { samples: 40_000 });
        assert_eq!(
            crops[0].span,
            SampleSpan {
                start: 0,
                len: 32_000
            }
        );
        // No windows, no crop.
        assert!(crops_of(&windows(32_000, &[]), Cut::Best { samples: 100 }).is_empty());
    }

    #[test]
    fn every_window_at_or_above_the_score_gives_a_named_crop() {
        let w = windows(32_000, &[(0, 0.1), (2_560, 0.17), (5_120, 0.5)]);
        let crops = crops_of(
            &w,
            Cut::Every {
                samples: 16_000,
                min: 0.17,
            },
        );
        let named: Vec<(u64, Option<u64>)> =
            crops.iter().map(|c| (c.span.start, c.at_ms)).collect();
        assert_eq!(named, [(5_760, Some(160)), (8_320, Some(320))]);
    }

    #[test]
    fn cut_queries_are_checked() {
        let query = |crop: Option<&str>, min: Option<&str>| ArchiveQuery {
            min: None,
            max: None,
            device: None,
            verdict: None,
            crop: crop.map(str::to_owned),
            window_min: min.map(str::to_owned),
        };
        assert_eq!(parse_cut(&query(None, None)).unwrap(), Cut::Whole);
        assert_eq!(
            parse_cut(&query(Some("1.40"), None)).unwrap(),
            Cut::Best { samples: 22_400 }
        );
        assert_eq!(
            parse_cut(&query(Some("0.10"), Some("0.17"))).unwrap(),
            Cut::Every {
                samples: 1_600,
                min: 0.17
            }
        );
        for (crop, min) in [
            (None, Some("0.5")),
            (Some("0.05"), None),
            (Some("10.5"), None),
            (Some("x"), None),
            (Some("1.4"), Some("1.5")),
        ] {
            assert!(parse_cut(&query(crop, min)).is_err(), "{crop:?} {min:?}");
        }
    }
}
