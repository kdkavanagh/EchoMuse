//! A turn's native AFE series (`turns.afe_series`, afe-metadata.md §2.1): every 80 ms
//! capture period's AFE record from the chart's start through the turn's end, in the
//! shape the dashboard's chart reads (`AfeSeriesJson`).
//!
//! The records arrive on the turn's own subscriptions in an `afe_metadata` session: the
//! candidate (or button, or reply) subscription from its start, and after a commit the
//! response subscription opened before the committed one closes, which carries them on
//! through the spoken answer (and the mic, while the speaker saves utterances, for the
//! turn recording). Values are in the chart's units: DNN VAD as raw × 0.25, double talk
//! as raw ÷ 31, RMS in dB (raw − 256, null when the AFE computed none), ERLE raw. A period
//! with no record is null in every per-period array; a record without frames has null
//! values but its frame, loss and flag entries. Flags list the periods they were set in.
//!
//! The spans place the turn's parts on the same capture timeline, each `[start, end)`:
//! `support` (the wake word), `utterance`, and the saved audio — `wake_clip`,
//! `audio_clip` (the STT copy) and `recording` (the turn recording, `null` when none was
//! kept). Silero VAD is the controller's own speech probability for each 32 ms cell of
//! the turn's subscription (`silero_start`, `silero`; `null` and `[]` when none ran).
//!
//! `stages` places a committed turn's processing on the same timeline (`{}` without a
//! commit): `endpoint` from where the command ended to the frontier the commit was made
//! at, then `stt`, `intent`, `tts` (from the answer's URL to its first audible sample)
//! and `reply` (the answer playing), each mapped from the controller's clock onto capture
//! samples from the commit (its frontier at its instant, 16 samples a millisecond). The
//! dashboard draws them as bars; a stage whose start or end is unknown is left out.
//!
//! The evidence summaries (`turns.afe_evidence`, also in the decision trace) summarise
//! the records over three spans anchored on the wake word's `support_start`: `pre` (the
//! second before it), `wake` (the support window) and `utterance`, each `null` when no
//! record falls in it; `playback_onset` is the capture sample of the last playback rise
//! at or before `support_start`. The column stays NULL outside an `afe_metadata` session.

use std::collections::BTreeMap;

use echomuse_protocol::ema::{AfeFlags, AfeRecord};
use echomuse_records::{AfeEvidence, AfeSeries, JsonObject};
use serde_json::{Map, Value, json};

/// Capture samples per AFE period.
pub(crate) const PERIOD: u64 = 1_280;
/// How far before the wake word's support the chart starts.
pub(crate) const LEAD: u64 = 16_000;
/// AFE frames per period, each 128 samples.
const PERIOD_FRAMES: u64 = 10;

/// A turn's parts on the capture timeline.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Spans {
    pub(crate) support: Option<(u64, u64)>,
    pub(crate) utterance: Option<(u64, u64)>,
    pub(crate) wake_clip: Option<(u64, u64)>,
    pub(crate) audio_clip: Option<(u64, u64)>,
    pub(crate) recording: Option<(u64, u64)>,
    /// The processing stages, by the dashboard's names, in order.
    pub(crate) stages: Vec<(&'static str, (u64, u64))>,
}

/// The controller's Silero VAD over the turn: the first cell's start sample and each
/// cell's probability (`None` where none was computed).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Silero {
    pub(crate) start: u64,
    pub(crate) cells: Vec<Option<f32>>,
}

const FLAGS: [(AfeFlags, &str); 6] = [
    (AfeFlags::GAP, "gap"),
    (AfeFlags::SYNC, "sync"),
    (AfeFlags::OUTPUT_CLIPPED, "output_clipped"),
    (AfeFlags::MIC_CLIPPED, "mic_clipped"),
    (AfeFlags::AEC_DIVERGED, "aec_diverged"),
    (AfeFlags::DEVICE_MUTE, "device_mute"),
];

fn span(span: Option<(u64, u64)>) -> Value {
    span.map_or(Value::Null, |(a, b)| json!([a, b]))
}

/// The series of `records` (by period start) from `start` (rounded down to a period)
/// through `end`; `None` when no record falls in it.
pub(crate) fn series(
    start: u64,
    end: u64,
    records: &BTreeMap<u64, AfeRecord>,
    spans: &Spans,
    silero: Option<&Silero>,
) -> Option<AfeSeries> {
    let start = start / PERIOD * PERIOD;
    let end = end.max(
        records
            .range(start..)
            .next_back()
            .map_or(start, |(at, _)| at + PERIOD),
    );
    // No record in the chart's span: no chart.
    records.range(start..end).next()?;
    let periods: Vec<Option<&AfeRecord>> = (start..end)
        .step_by(PERIOD as usize)
        .map(|at| records.get(&at))
        .collect();
    let column = |f: &dyn Fn(&AfeRecord) -> Value, valued_only: bool| -> Value {
        Value::Array(
            periods
                .iter()
                .map(|r| match r {
                    Some(r) if !valued_only || r.frames > 0 => f(r),
                    _ => Value::Null,
                })
                .collect(),
        )
    };
    let rms = |raw: u8| {
        if raw == 0 {
            Value::Null
        } else {
            json!(i32::from(raw) - 256)
        }
    };
    let mut flags = Map::new();
    for (flag, name) in FLAGS {
        let hit: Vec<usize> = periods
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_some_and(|r| r.flags.contains(flag)))
            .map(|(i, _)| i)
            .collect();
        if !hit.is_empty() {
            flags.insert(name.to_owned(), json!(hit));
        }
    }
    let mut object = json!({
        "start": start,
        "period": PERIOD,
        "frames": column(&|r| json!(r.frames), false),
        "lost": column(&|r| json!(r.lost), false),
        "playback": column(&|r| json!(r.playback), true),
        "volume": column(&|r| json!(r.volume), true),
        "vad_max": column(&|r| json!(f64::from(r.vad_max) * 0.25), true),
        "vad_frames": column(&|r| json!(r.vad_frames), true),
        "dtd_max": column(&|r| json!(f64::from(r.dtd_max) / 31.0), true),
        "dtd_frames": column(&|r| json!(r.dtd_frames), true),
        "erle_max": column(&|r| if r.erle_frames > 0 { json!(r.erle_max) } else { Value::Null }, true),
        "erle_mean": column(&|r| if r.erle_frames > 0 { json!(r.erle_mean) } else { Value::Null }, true),
        "erle_frames": column(&|r| json!(r.erle_frames), true),
        "rms_max_db": column(&|r| rms(r.rms_max), true),
        "rms_mean_db": column(&|r| rms(r.rms_mean), true),
        "flags": flags,
        "support": span(spans.support),
        "utterance": span(spans.utterance),
        "wake_clip": span(spans.wake_clip),
        "audio_clip": span(spans.audio_clip),
        "recording": span(spans.recording),
    });
    if let Value::Object(map) = &mut object {
        let stages: Map<String, Value> = spans
            .stages
            .iter()
            .map(|(name, s)| ((*name).to_owned(), json!([s.0, s.1])))
            .collect();
        map.insert("stages".to_owned(), Value::Object(stages));
        map.insert(
            "silero_start".to_owned(),
            silero.map_or(Value::Null, |silero| json!(silero.start)),
        );
        map.insert(
            "silero".to_owned(),
            Value::Array(silero.map_or_else(Vec::new, |silero| {
                silero
                    .cells
                    .iter()
                    .map(|p| {
                        p.map_or(Value::Null, |p| {
                            json!((f64::from(p) * 1000.0).round() / 1000.0)
                        })
                    })
                    .collect()
            })),
        );
    }
    match object {
        Value::Object(map) => Some(AfeSeries(JsonObject::from_map(&map))),
        _ => None,
    }
}

fn round(value: f64, places: i32) -> f64 {
    let scale = 10f64.powi(places);
    (value * scale).round() / scale
}

/// One span's summary of the records whose periods overlap `[start, end)`; `null` when
/// no record falls in it.
fn part(records: &BTreeMap<u64, AfeRecord>, start: u64, end: u64) -> Value {
    if end <= start {
        return Value::Null;
    }
    let first = start / PERIOD * PERIOD;
    let last = (end - 1) / PERIOD * PERIOD;
    let received: Vec<&AfeRecord> = records.range(first..=last).map(|(_, r)| r).collect();
    if received.is_empty() {
        return Value::Null;
    }
    let periods_expected = (last - first) / PERIOD + 1;
    let with_frames: Vec<&AfeRecord> = received.iter().copied().filter(|r| r.frames > 0).collect();
    let sum =
        |f: &dyn Fn(&AfeRecord) -> u64, of: &[&AfeRecord]| -> u64 { of.iter().map(|r| f(r)).sum() };
    let flagged = |flag: AfeFlags| received.iter().filter(|r| r.flags.contains(flag)).count();
    let erle_frames = sum(&|r| u64::from(r.erle_frames), &with_frames);
    let erle_mean = (erle_frames > 0).then(|| {
        let weighted = sum(
            &|r| u64::from(r.erle_mean) * u64::from(r.erle_frames),
            &with_frames,
        );
        round(weighted as f64 / erle_frames as f64, 2)
    });
    let rms: Vec<&AfeRecord> = with_frames
        .iter()
        .copied()
        .filter(|r| r.rms_mean != 0)
        .collect();
    let rms_frames = sum(&|r| u64::from(r.frames), &rms);
    let rms_mean_db = (rms_frames > 0).then(|| {
        let weighted: f64 = rms
            .iter()
            .map(|r| (f64::from(r.rms_mean) - 256.0) * f64::from(r.frames))
            .sum();
        round(weighted / rms_frames as f64, 2)
    });
    let rms_max_db = with_frames
        .iter()
        .filter(|r| r.rms_max != 0)
        .map(|r| i32::from(r.rms_max) - 256)
        .max();
    let max = |f: &dyn Fn(&AfeRecord) -> u8| with_frames.iter().map(|r| f(r)).max();
    json!({
        "start": start,
        "end": end,
        "periods_expected": periods_expected,
        "periods_received": received.len(),
        "periods_with_frames": with_frames.len(),
        "frames": sum(&|r| u64::from(r.frames), &received),
        "frames_expected": periods_expected * PERIOD_FRAMES,
        "playback_frames": sum(&|r| u64::from(r.playback), &with_frames),
        "erle_max": max(&|r| r.erle_max),
        "erle_mean": erle_mean,
        "erle_frames": erle_frames,
        "dtd_max": max(&|r| r.dtd_max).map(|m| round(f64::from(m) / 31.0, 3)),
        "dtd_frames": sum(&|r| u64::from(r.dtd_frames), &with_frames),
        "rms_max_db": rms_max_db,
        "rms_mean_db": rms_mean_db,
        "vad_max": max(&|r| r.vad_max).map(|m| f64::from(m) * 0.25),
        "vad_frames": sum(&|r| u64::from(r.vad_frames), &with_frames),
        "volume": with_frames.last().map(|r| r.volume),
        "output_clipped": flagged(AfeFlags::OUTPUT_CLIPPED),
        "mic_clipped": flagged(AfeFlags::MIC_CLIPPED),
        "aec_diverged": flagged(AfeFlags::AEC_DIVERGED),
        "device_mute": flagged(AfeFlags::DEVICE_MUTE),
        "gaps": flagged(AfeFlags::GAP),
        "syncs": flagged(AfeFlags::SYNC),
        "lost_frames": sum(&|r| u64::from(r.lost), &received),
    })
}

/// The capture sample of the last playback rise at or before `support_start`: a period
/// with playback frames after a received period without any, placed at the period's
/// start plus the frames before the playback began (128 samples each). A run whose
/// start was not received has no known rise.
fn playback_onset(records: &BTreeMap<u64, AfeRecord>, support_start: u64) -> Option<u64> {
    let mut onset = None;
    let mut previous: Option<(u64, &AfeRecord)> = None;
    for (at, record) in records.range(..=support_start) {
        if record.playback > 0
            && let Some((before, quiet)) = previous
            && before + PERIOD == *at
            && quiet.playback == 0
        {
            let frames_before = PERIOD_FRAMES.saturating_sub(u64::from(record.playback));
            let rise = at + frames_before * (PERIOD / PERIOD_FRAMES);
            if rise <= support_start {
                onset = Some(rise);
            }
        }
        previous = Some((*at, record));
    }
    onset
}

/// The evidence summaries (`turns.afe_evidence`) of a turn or refused candidate whose
/// wake word, if any, was supported over `support` and whose utterance, if any, is
/// `utterance`.
pub(crate) fn evidence(
    records: &BTreeMap<u64, AfeRecord>,
    support: Option<(u64, u64)>,
    utterance: Option<(u64, u64)>,
) -> AfeEvidence {
    let support_start = support.map(|(start, _)| start);
    let object = json!({
        "support_start": support_start,
        "pre": support_start.map_or(Value::Null, |s| part(records, s.saturating_sub(LEAD), s)),
        "wake": support.map_or(Value::Null, |(s, e)| part(records, s, e)),
        "playback_onset": support_start.and_then(|s| playback_onset(records, s)),
        "utterance": utterance.map_or(Value::Null, |(s, e)| part(records, s, e)),
    });
    match object {
        Value::Object(map) => AfeEvidence(JsonObject::from_map(&map)),
        _ => unreachable!("an object literal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(frames: u8) -> AfeRecord {
        AfeRecord {
            frames,
            vad_max: 3,
            vad_frames: frames,
            dtd_max: 31,
            rms_max: 200,
            rms_mean: 0,
            volume: 64,
            flags: AfeFlags::SYNC,
            ..AfeRecord::default()
        }
    }

    #[test]
    fn periods_without_records_are_null_and_values_take_the_chart_units() {
        let records = BTreeMap::from([(1_280, record(10)), (3_840, record(0))]);
        let spans = Spans {
            support: Some((1_500, 2_000)),
            recording: Some((0, 5_120)),
            ..Spans::default()
        };
        let silero = Silero {
            start: 1_536,
            cells: vec![Some(0.9), None],
        };
        let series = series(100, 5_120, &records, &spans, Some(&silero)).unwrap();
        let value: Value = serde_json::from_str(series.as_str()).unwrap();
        assert_eq!(value["start"], 0);
        assert_eq!(value["period"], 1_280);
        assert_eq!(value["frames"], json!([null, 10, null, 0]));
        assert_eq!(value["vad_max"], json!([null, 0.75, null, null]));
        assert_eq!(value["dtd_max"], json!([null, 1.0, null, null]));
        assert_eq!(value["rms_max_db"], json!([null, -56, null, null]));
        assert_eq!(value["rms_mean_db"], json!([null, null, null, null]));
        assert_eq!(value["flags"], json!({"sync": [1, 3]}));
        assert_eq!(value["support"], json!([1_500, 2_000]));
        assert_eq!(value["utterance"], Value::Null);
        assert_eq!(value["recording"], json!([0, 5_120]));
        assert_eq!(value["silero"], json!([0.9, null]));
    }

    #[test]
    fn no_record_no_series() {
        assert!(series(0, 12_800, &BTreeMap::new(), &Spans::default(), None).is_none());
    }

    #[test]
    fn stages_and_an_absent_silero_take_the_stored_shape() {
        let records = BTreeMap::from([(0, record(10))]);
        let spans = Spans {
            stages: vec![("endpoint", (100, 900)), ("stt", (950, 1_000))],
            ..Spans::default()
        };
        let value: Value =
            serde_json::from_str(series(0, 1_280, &records, &spans, None).unwrap().as_str())
                .unwrap();
        assert_eq!(
            value["stages"],
            json!({"endpoint": [100, 900], "stt": [950, 1_000]})
        );
        assert_eq!(value["silero_start"], Value::Null);
        assert_eq!(value["silero"], json!([]));
    }

    /// A period as the stored evidence summarises it (real rows, afe-metadata.md §2.1):
    /// ERLE's mean weighted by its frames, RMS in dB, DTD and VAD in their units.
    fn loud(playback: u8, erle_mean: u8, erle_frames: u8) -> AfeRecord {
        AfeRecord {
            frames: 10,
            playback,
            erle_max: erle_mean + 2,
            erle_mean,
            erle_frames,
            dtd_max: 4,
            dtd_frames: 2,
            rms_max: 209,
            rms_mean: 180,
            vad_max: 3,
            vad_frames: 5,
            volume: 70,
            ..AfeRecord::default()
        }
    }

    #[test]
    fn evidence_summarises_each_span_and_null_means_no_data() {
        // Periods 0–4; the wake word is supported over [3_000, 5_000).
        let mut records = BTreeMap::new();
        records.insert(
            0,
            AfeRecord {
                frames: 10,
                volume: 70,
                ..AfeRecord::default()
            },
        );
        records.insert(1_280, loud(4, 10, 2));
        records.insert(2_560, loud(10, 4, 6));
        records.insert(
            3_840,
            AfeRecord {
                frames: 9,
                lost: 1,
                flags: AfeFlags::GAP,
                volume: 71,
                ..AfeRecord::default()
            },
        );
        let value: Value =
            serde_json::from_str(evidence(&records, Some((3_000, 5_000)), None).0.as_str())
                .unwrap();
        assert_eq!(value["support_start"], 3_000);
        assert_eq!(value["utterance"], Value::Null);
        let pre = &value["pre"];
        assert_eq!(pre["periods_expected"], 3);
        assert_eq!(pre["periods_received"], 3);
        assert_eq!(pre["frames"], 30);
        assert_eq!(pre["frames_expected"], 30);
        assert_eq!(pre["playback_frames"], 14);
        assert_eq!(pre["erle_frames"], 8);
        // (10·2 + 4·6) / 8
        assert_eq!(pre["erle_mean"], 5.5);
        assert_eq!(pre["erle_max"], 12);
        assert_eq!(pre["dtd_max"], 0.129);
        assert_eq!(pre["rms_max_db"], -47);
        assert_eq!(pre["rms_mean_db"], -76.0);
        assert_eq!(pre["vad_max"], 0.75);
        let wake = &value["wake"];
        assert_eq!(wake["periods_expected"], 2);
        assert_eq!(wake["periods_received"], 2);
        assert_eq!(wake["gaps"], 1);
        assert_eq!(wake["lost_frames"], 1);
        // The support starts inside period 2: its records count.
        assert_eq!(wake["erle_mean"], 4.0);
        assert_eq!(wake["playback_frames"], 10);
        assert_eq!(wake["volume"], 71);
        // The playback rose in period 1, four frames before its end.
        assert_eq!(value["playback_onset"], 1_280 + 6 * 128);
        let empty: Value = serde_json::from_str(
            evidence(&BTreeMap::new(), Some((3_000, 5_000)), None)
                .0
                .as_str(),
        )
        .unwrap();
        assert_eq!(empty["pre"], Value::Null);
        assert_eq!(empty["playback_onset"], Value::Null);
    }
}
