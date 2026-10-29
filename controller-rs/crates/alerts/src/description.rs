//! The `echomuse:` line that ends the description of every calendar event EchoMuse writes
//! (§3.3.4, architecture §16.3): `echomuse: ` followed by canonical JSON with exactly the
//! keys `id`, `kind`, `loop_gap_ms`, `max_ring_ms`, `op`, `parent`, `ramp_ms`,
//! `snooze_ms`, `sound`, `v` (always 1) and `volume`.
//!
//! An event without the line was made in HA's UI or by an automation. A line that is
//! present but malformed is treated the same way and flagged; it is never guessed at.

use std::{fmt, num::NonZeroU64};

use echomuse_protocol::{
    Sha256,
    alerts::{AlertSound, AlertVolume, MAX_LABEL_CHARS, OccurrenceKind},
    canonical::canonical_json,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The line's prefix.
pub const LINE_PREFIX: &str = "echomuse: ";

/// The settings of an event without a valid line, and of a new alarm unless the caller
/// says otherwise (architecture §16.3).
pub mod defaults {
    /// Snooze length.
    pub const SNOOZE_MS: u64 = 540_000;
    /// Ring limit.
    pub const MAX_RING_MS: u64 = 600_000;
    /// Pause between sound loops.
    pub const LOOP_GAP_MS: u64 = 2_000;
    /// Volume ramp.
    pub const RAMP_MS: u64 = 20_000;
    /// The label of an alarm without one.
    pub const LABEL: &str = "Alarm";
}

/// The occurrence a snooze child replaces.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParentRef {
    /// The parent's schedule.
    pub id: Uuid,
    /// The parent occurrence's key.
    pub occ: String,
}

/// How an occurrence rings.
#[derive(Clone, Debug, PartialEq)]
pub struct RingSettings {
    /// The pause between sound loops in ms.
    pub loop_gap_ms: u64,
    /// The ring limit in ms.
    pub max_ring_ms: NonZeroU64,
    /// The volume ramp in ms.
    pub ramp_ms: u64,
    /// The snooze length in ms.
    pub snooze_ms: u64,
    /// The sound.
    pub sound: AlertSound,
    /// The alarm's own gain; `None` rings at the media volume.
    pub volume: Option<AlertVolume>,
}

impl RingSettings {
    /// The defaults, ringing `sound`.
    pub fn defaults(sound: AlertSound) -> Self {
        Self {
            loop_gap_ms: defaults::LOOP_GAP_MS,
            max_ring_ms: NonZeroU64::new(defaults::MAX_RING_MS).expect("nonzero"),
            ramp_ms: defaults::RAMP_MS,
            snooze_ms: defaults::SNOOZE_MS,
            sound,
            volume: None,
        }
    }
}

/// The content of an `echomuse:` line.
#[derive(Clone, Debug, PartialEq)]
pub struct EchomuseLine {
    /// The schedule id.
    pub id: Uuid,
    /// Alarm or snooze child.
    pub kind: OccurrenceKind,
    /// The operation that created the event.
    pub op: Uuid,
    /// The occurrence a snooze child replaces; `None` for an alarm.
    pub parent: Option<ParentRef>,
    /// How it rings.
    pub ring: RingSettings,
}

/// What an event's description says about it.
#[derive(Clone, Debug, PartialEq)]
pub enum LineOf {
    /// No `echomuse:` line: an event made in HA's UI or by an automation.
    Absent,
    /// A valid line.
    Valid(EchomuseLine),
    /// A line that is present but malformed; the text says why.
    Malformed(String),
}

impl EchomuseLine {
    /// The line's JSON object.
    fn to_value(&self) -> Value {
        let sound = match &self.ring.sound {
            AlertSound::Fallback => Value::from("builtin:fallback"),
            AlertSound::Asset(sha) => Value::from(sha.as_str()),
        };
        json!({
            "id": self.id.to_string(),
            "kind": self.kind.as_str(),
            "loop_gap_ms": self.ring.loop_gap_ms,
            "max_ring_ms": self.ring.max_ring_ms.get(),
            "op": self.op.to_string(),
            "parent": self.parent.as_ref().map(|p| json!({"id": p.id.to_string(), "occ": p.occ})),
            "ramp_ms": self.ring.ramp_ms,
            "snooze_ms": self.ring.snooze_ms,
            "sound": sound,
            "v": 1,
            "volume": self.ring.volume.map(AlertVolume::get),
        })
    }

    /// The whole line: the prefix and the canonical JSON.
    pub fn to_line(&self) -> String {
        let text = canonical_json(&self.to_value())
            .expect("the line holds only finite numbers, strings and objects");
        format!("{LINE_PREFIX}{text}")
    }

    /// A description: the optional human text, then the line last.
    pub fn description(&self, human: Option<&str>) -> String {
        match human.map(str::trim_end).filter(|text| !text.is_empty()) {
            Some(text) => format!("{text}\n{}", self.to_line()),
            None => self.to_line(),
        }
    }

    /// Reads the line that ends `description`.
    pub fn parse(description: Option<&str>) -> LineOf {
        let Some(last) = description.and_then(|d| d.lines().rev().find(|l| !l.trim().is_empty()))
        else {
            return LineOf::Absent;
        };
        let Some(json_text) = last.trim().strip_prefix("echomuse:") else {
            return LineOf::Absent;
        };
        match serde_json::from_str::<Value>(json_text.trim()) {
            Ok(Value::Object(object)) => match read(&object) {
                Ok(line) => LineOf::Valid(line),
                Err(why) => LineOf::Malformed(why),
            },
            Ok(_) => LineOf::Malformed("not a JSON object".to_owned()),
            Err(err) => LineOf::Malformed(format!("not JSON: {err}")),
        }
    }
}

impl fmt::Display for EchomuseLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_line())
    }
}

const KEYS: [&str; 11] = [
    "id",
    "kind",
    "loop_gap_ms",
    "max_ring_ms",
    "op",
    "parent",
    "ramp_ms",
    "snooze_ms",
    "sound",
    "v",
    "volume",
];

fn read(object: &Map<String, Value>) -> Result<EchomuseLine, String> {
    if object.len() != KEYS.len() || KEYS.iter().any(|key| !object.contains_key(*key)) {
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        return Err(format!("keys {keys:?} are not exactly {KEYS:?}"));
    }
    if object["v"].as_u64() != Some(1) {
        return Err("v is not 1".to_owned());
    }
    let uuid = |key: &str| -> Result<Uuid, String> {
        object[key]
            .as_str()
            .and_then(|text| Uuid::parse_str(text).ok())
            .ok_or_else(|| format!("{key} is not a UUID"))
    };
    let count = |key: &str| -> Result<u64, String> {
        object[key]
            .as_u64()
            .ok_or_else(|| format!("{key} is not a non-negative integer"))
    };
    let kind = match object["kind"].as_str() {
        Some("alarm") => OccurrenceKind::Alarm,
        Some("snooze") => OccurrenceKind::Snooze,
        _ => return Err("kind is not alarm or snooze".to_owned()),
    };
    let parent = match &object["parent"] {
        Value::Null => None,
        Value::Object(p) => {
            let id = p
                .get("id")
                .and_then(Value::as_str)
                .and_then(|text| Uuid::parse_str(text).ok())
                .ok_or("parent.id is not a UUID")?;
            let occ = p
                .get("occ")
                .and_then(Value::as_str)
                .filter(|occ| !occ.is_empty())
                .ok_or("parent.occ is not a key")?;
            Some(ParentRef {
                id,
                occ: occ.to_owned(),
            })
        }
        _ => return Err("parent is not an object or null".to_owned()),
    };
    if (kind == OccurrenceKind::Snooze) != parent.is_some() {
        return Err("a snooze child, and only a snooze child, names its parent".to_owned());
    }
    let sound = match object["sound"].as_str() {
        Some("builtin:fallback") => AlertSound::Fallback,
        Some(text) => AlertSound::Asset(Sha256::parse(text).ok_or("sound is not a SHA-256")?),
        None => return Err("sound is not a string".to_owned()),
    };
    let volume = match &object["volume"] {
        Value::Null => None,
        value => Some(
            value
                .as_f64()
                .and_then(AlertVolume::new)
                .ok_or("volume is not in (0, 1]")?,
        ),
    };
    let max_ring_ms = NonZeroU64::new(count("max_ring_ms")?).ok_or("max_ring_ms is zero")?;
    Ok(EchomuseLine {
        id: uuid("id")?,
        kind,
        op: uuid("op")?,
        parent,
        ring: RingSettings {
            loop_gap_ms: count("loop_gap_ms")?,
            max_ring_ms,
            ramp_ms: count("ramp_ms")?,
            snooze_ms: count("snooze_ms")?,
            sound,
            volume,
        },
    })
}

/// A label the device accepts: at most [`MAX_LABEL_CHARS`] characters, cut at that many
/// when longer (an event title from HA's UI may be).
pub fn device_label(summary: &str) -> String {
    summary.chars().take(MAX_LABEL_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line() -> EchomuseLine {
        EchomuseLine {
            id: Uuid::from_u128(0x10),
            kind: OccurrenceKind::Snooze,
            op: Uuid::from_u128(0x20),
            parent: Some(ParentRef {
                id: Uuid::from_u128(0x30),
                occ: "20260923T063000".to_owned(),
            }),
            ring: RingSettings {
                volume: Some(AlertVolume::new(0.5).unwrap()),
                ..RingSettings::defaults(AlertSound::Asset(Sha256::of(b"bell")))
            },
        }
    }

    #[test]
    fn the_line_is_canonical_json_with_exactly_the_listed_keys() {
        let text = line().to_line();
        let sha = Sha256::of(b"bell");
        assert_eq!(
            text,
            format!(
                "echomuse: {{\"id\":\"00000000-0000-0000-0000-000000000010\",\"kind\":\"snooze\",\
                 \"loop_gap_ms\":2000,\"max_ring_ms\":600000,\"op\":\"00000000-0000-0000-0000-000000000020\",\
                 \"parent\":{{\"id\":\"00000000-0000-0000-0000-000000000030\",\"occ\":\"20260923T063000\"}},\
                 \"ramp_ms\":20000,\"snooze_ms\":540000,\"sound\":\"{sha}\",\"v\":1,\"volume\":0.5}}"
            )
        );
    }

    #[test]
    fn a_description_round_trips_with_human_text_before_the_line() {
        let original = line();
        let description = original.description(Some("Bins go out\n"));
        assert!(description.starts_with("Bins go out\necho"));
        assert_eq!(
            EchomuseLine::parse(Some(&description)),
            LineOf::Valid(original.clone())
        );
        let alarm = EchomuseLine {
            kind: OccurrenceKind::Alarm,
            parent: None,
            ring: RingSettings::defaults(AlertSound::Fallback),
            ..original
        };
        assert_eq!(
            EchomuseLine::parse(Some(&alarm.description(None))),
            LineOf::Valid(alarm)
        );
    }

    #[test]
    fn events_without_a_valid_line_are_told_apart() {
        assert_eq!(EchomuseLine::parse(None), LineOf::Absent);
        assert_eq!(EchomuseLine::parse(Some("Dentist")), LineOf::Absent);
        let mut extra = line().to_value();
        extra["colour"] = json!("red");
        let text = format!("echomuse: {extra}");
        assert!(matches!(
            EchomuseLine::parse(Some(&text)),
            LineOf::Malformed(_)
        ));
        let mut wrong_version = line().to_value();
        wrong_version["v"] = json!(2);
        assert!(matches!(
            EchomuseLine::parse(Some(&format!("echomuse: {wrong_version}"))),
            LineOf::Malformed(_)
        ));
        assert!(matches!(
            EchomuseLine::parse(Some("echomuse: {not json")),
            LineOf::Malformed(_)
        ));
        let mut orphan = line().to_value();
        orphan["parent"] = Value::Null;
        assert!(matches!(
            EchomuseLine::parse(Some(&format!("echomuse: {orphan}"))),
            LineOf::Malformed(_)
        ));
    }

    #[test]
    fn long_labels_are_cut_for_the_device() {
        assert_eq!(device_label(&"é".repeat(130)).chars().count(), 120);
        assert_eq!(device_label("Wake up"), "Wake up");
    }
}
