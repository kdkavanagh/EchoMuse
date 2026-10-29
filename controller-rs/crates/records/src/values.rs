//! Typed values for the record columns: times in the units each column stores, identities,
//! and the JSON documents the tables hold.

use std::{
    fmt,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use echomuse_protocol::vocab::Sha256;
use serde::{Serialize, Serializer, ser::SerializeSeq};
use serde_json::{Map, Value};

/// Audio as the recordings store it: 16 kHz mono PCM16 samples.
pub type Pcm16 = Arc<[i16]>;

/// The sample rate of every recording and of the sample positions in these records.
pub const SAMPLE_RATE_HZ: u32 = 16_000;

/// Wall time as `turns.ts`, `near_misses.ts` and `wake_shadow_events.ts` store it: seconds
/// since the Unix epoch, fractional (REAL).
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize)]
pub struct WallSeconds(f64);

impl WallSeconds {
    /// Seconds since the epoch; None when not finite or before the epoch.
    pub fn new(seconds: f64) -> Option<Self> {
        (seconds.is_finite() && seconds >= 0.0).then_some(Self(seconds))
    }

    /// The time; None before the epoch.
    pub fn from_system_time(time: SystemTime) -> Option<Self> {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| Self(d.as_secs_f64()))
    }

    /// The stored value.
    pub fn get(self) -> f64 {
        self.0
    }

    /// The start of the UTC hour holding this time, as the hourly rollups key it.
    pub fn hour(self) -> HourStart {
        // Truncation is the intent: the whole seconds of a non-negative finite time.
        let seconds = self.0.floor() as i64;
        HourStart(seconds - seconds.rem_euclid(3_600))
    }

    /// The same time in whole milliseconds, as `device_logs.ts` stores it.
    pub fn millis(self) -> UnixMillis {
        UnixMillis((self.0 * 1_000.0).floor() as i64)
    }
}

/// Wall time as `device_logs.ts` stores it: whole milliseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixMillis(pub i64);

impl UnixMillis {
    /// The time rounded down to a millisecond; None before the epoch or past `i64`.
    pub fn from_system_time(time: SystemTime) -> Option<Self> {
        let ms = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
        i64::try_from(ms).ok().map(Self)
    }
}

/// `hour_ts` of the hourly rollups: the UTC hour's first second since the Unix epoch, a
/// multiple of 3,600.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HourStart(i64);

impl HourStart {
    /// The stored value.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// A duration stored as whole milliseconds (INTEGER), rounded down.
pub(crate) fn whole_ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// A non-empty text identity.
macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            /// Accepts any non-empty text.
            pub fn new(text: impl Into<String>) -> Option<Self> {
                let text = text.into();
                (!text.is_empty()).then_some(Self(text))
            }

            /// The stored text.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }
    };
}

text_id!(
    /// `turns.policy_hash`: the speech policy's identity, for example
    /// `post_afe_10+wyoming:default` (the policy revision, then the pause transcriber).
    PolicyHash
);
text_id!(
    /// `turns.wake_model`: the wake model's name, its registry wake phrase (`ophelia`).
    WakeModelName
);
text_id!(
    /// `near_misses.near_miss_uuid`: the device's `near_miss_id`, kept exactly as sent.
    NearMissUuid
);
text_id!(
    /// A `wake.candidate`'s `candidate_id`, kept exactly as sent.
    CandidateId
);
text_id!(
    /// `turns.response_type`: Home Assistant's `response_type` (`action_done`,
    /// `query_answer`, ...), kept as HA sent it.
    ResponseType
);

/// The compact text of a JSON object, as the JSON columns store it.
#[derive(Clone, Debug, PartialEq)]
pub struct JsonObject(String);

impl JsonObject {
    /// The compact serialization of `object`.
    pub fn from_map(object: &Map<String, Value>) -> Self {
        Self(Value::Object(object.clone()).to_string())
    }

    /// Accepts text that parses as a JSON object, stored compactly.
    pub fn parse(text: &str) -> Option<Self> {
        match serde_json::from_str::<Value>(text).ok()? {
            Value::Object(map) => Some(Self::from_map(&map)),
            _ => None,
        }
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_stored(text: String) -> Self {
        Self(text)
    }
}

/// A JSON object column whose layout its producer owns.
macro_rules! object_column {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq)]
        pub struct $name(pub JsonObject);

        impl $name {
            /// The stored text.
            pub fn as_str(&self) -> &str {
                self.0.as_str()
            }
        }
    };
}

object_column!(
    /// `turns.afe_evidence`: per-stage summaries of the native AFE metadata (afe-metadata.md),
    /// an object of `support_start` and the `pre`, `wake`, `utterance` and `playback_onset`
    /// summaries. SQL NULL means unavailable; inside, a null part means no data.
    AfeEvidence
);
object_column!(
    /// `turns.afe_series` and `near_misses.afe_series`: every 80 ms AFE record behind the
    /// summaries (`start`, `period`, `support`, `utterance`, per-period arrays, `stages`,
    /// `wake_clip`, `audio_clip`, `recording`; afe-metadata.md). Kept for the newest 1,000
    /// rows per device.
    AfeSeries
);
object_column!(
    /// `turns.stt_fix`: the transcript correction record (post-AFE architecture §16.8:
    /// `mode`, `reason`, `original`, `proposed`, `sent`, `applied`, ...). NULL when
    /// correction is off or the turn ended before it ran.
    SttFix
);

/// `turns.wake_windows` and `near_misses.windows`: a saved clip's wake-model windows, scored
/// once in the background. NULL until scored.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipWindows {
    /// The graph that scored the clip.
    pub graph_sha256: Sha256,
    /// The clip's length in samples.
    pub samples: u64,
    /// The window length in samples (22,400: 1.4 s).
    pub window: u64,
    /// Each window's start sample in the clip and its score, stored as `[start, score]`.
    #[serde(serialize_with = "pairs")]
    pub scores: Vec<(u64, f64)>,
}

fn pairs<S: Serializer>(scores: &[(u64, f64)], serializer: S) -> Result<S::Ok, S::Error> {
    let mut seq = serializer.serialize_seq(Some(scores.len()))?;
    for (start, score) in scores {
        seq.serialize_element(&(start, score))?;
    }
    seq.end()
}

impl ClipWindows {
    /// The compact JSON the column stores.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("clip windows serialize")
    }

    /// Reads the stored JSON; None when it is not this shape.
    pub fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let scores = object
            .get("scores")?
            .as_array()?
            .iter()
            .map(|pair| {
                let pair = pair.as_array()?;
                match pair.as_slice() {
                    [start, score] => Some((start.as_u64()?, score.as_f64()?)),
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            graph_sha256: Sha256::parse(object.get("graph_sha256")?.as_str()?)?,
            samples: object.get("samples")?.as_u64()?,
            window: object.get("window")?.as_u64()?,
            scores,
        })
    }
}

/// `turns.reference_coverage`: the share of the utterance's cells whose playback reference
/// was fully covered, 0 to 1.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize)]
pub struct ReferenceCoverage(f64);

impl ReferenceCoverage {
    /// A share in `[0, 1]`.
    pub fn new(share: f64) -> Option<Self> {
        (0.0..=1.0).contains(&share).then_some(Self(share))
    }

    /// The stored value.
    pub fn get(self) -> f64 {
        self.0
    }
}

/// A probability or threshold in `[0, 1]`, as the score columns store them.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize)]
pub struct Score(f64);

impl Score {
    /// A value in `[0, 1]`.
    pub fn new(value: f64) -> Option<Self> {
        (0.0..=1.0).contains(&value).then_some(Self(value))
    }

    /// The stored value.
    pub fn get(self) -> f64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_and_millis() {
        let t = WallSeconds::new(1_793_235_185.803_92).unwrap();
        assert_eq!(t.hour().get(), 1_793_232_000);
        assert_eq!(t.millis(), UnixMillis(1_793_235_185_803));
        assert!(WallSeconds::new(-1.0).is_none());
        assert!(WallSeconds::new(f64::NAN).is_none());
    }

    #[test]
    fn clip_windows_store_pairs_compactly() {
        let w = ClipWindows {
            graph_sha256: Sha256::parse(&"4e".repeat(32)).unwrap(),
            samples: 42_560,
            window: 22_400,
            scores: vec![(0, 0.0141), (2_560, 0.4789)],
        };
        let text = w.to_json();
        assert_eq!(
            text,
            format!(
                r#"{{"graph_sha256":"{}","samples":42560,"window":22400,"scores":[[0,0.0141],[2560,0.4789]]}}"#,
                "4e".repeat(32)
            )
        );
        assert_eq!(ClipWindows::parse(&text), Some(w));
    }

    #[test]
    fn json_object_requires_an_object() {
        assert!(JsonObject::parse("[1]").is_none());
        assert_eq!(
            JsonObject::parse("{ \"a\" : 1 }").unwrap().as_str(),
            r#"{"a":1}"#
        );
    }
}
