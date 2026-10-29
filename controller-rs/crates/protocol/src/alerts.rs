//! Alert cache objects on the wire (docs/protocol-v1.md §4.7, spec §3.1.8):
//! occurrence and tombstone objects, `alert.snapshot` pages with their
//! digest, and `alert.delta`.
//!
//! The device re-encodes every object it receives in canonical form and
//! checks the snapshot digest over the whole ordered array, so the digest
//! here is computed from exactly the values that are sent. An object the
//! firmware would refuse (`device/internal/alerts/objects.go`) makes it ask
//! for another snapshot, so the types below rule out what they can and
//! [`snapshot_pages`] checks the rest.

use std::fmt;
use std::num::NonZeroU64;

use serde::{Serialize, Serializer};
use uuid::Uuid;

use crate::canonical::{CanonicalError, canonical_json};
use crate::envelope::ControlBody;
use crate::json::u64_string;
use crate::vocab::{MessageType, Sha256};

/// The most objects one `alert.snapshot` page carries.
pub const SNAPSHOT_PAGE_OBJECTS: usize = 128;

/// The longest label the device accepts, in Unicode scalar values.
pub const MAX_LABEL_CHARS: usize = 120;

wire_enum! {
    /// An occurrence's `kind`.
    pub enum OccurrenceKind {
        /// A scheduled alarm.
        Alarm => "alarm",
        /// The child of a snoozed alarm.
        Snooze => "snooze",
    }
}

wire_enum! {
    /// A tombstone's reason.
    pub enum TombstoneReason {
        /// The occurrence was dismissed.
        Dismissed => "dismissed",
        /// It was snoozed; a child replaces it.
        Snoozed => "snoozed",
        /// It rang out unanswered.
        Expired => "expired",
        /// It was deleted.
        Deleted => "deleted",
    }
}

/// The sound an occurrence rings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AlertSound {
    /// The firmware's built-in sound, `builtin:fallback`.
    Fallback,
    /// An installed alert sound, named by its SHA-256.
    Asset(Sha256),
}

impl Serialize for AlertSound {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            AlertSound::Fallback => serializer.serialize_str("builtin:fallback"),
            AlertSound::Asset(sha) => sha.serialize(serializer),
        }
    }
}

/// An alert's own gain, in (0, 1]: the device refuses silence.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct AlertVolume(f64);

impl AlertVolume {
    /// A gain in (0, 1]; anything else is `None`.
    pub fn new(gain: f64) -> Option<Self> {
        (gain > 0.0 && gain <= 1.0).then_some(Self(gain))
    }

    /// The gain.
    pub fn get(self) -> f64 {
        self.0
    }
}

/// One occurrence object.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct OccurrenceObject {
    /// The local due time, RFC 3339 with offset.
    pub due_local: String,
    /// The due time in UTC ms (a decimal string on the wire).
    #[serde(serialize_with = "u64_string")]
    pub due_utc_ms: u64,
    /// Alarm or snooze.
    pub kind: OccurrenceKind,
    /// The label, at most [`MAX_LABEL_CHARS`].
    pub label: String,
    /// The pause between sound loops in ms.
    pub loop_gap_ms: u64,
    /// How long it rings before it expires, in ms.
    pub max_ring_ms: NonZeroU64,
    /// The occurrence's id.
    pub occurrence_id: Uuid,
    /// The volume ramp in ms.
    pub ramp_ms: u64,
    /// The delivery sequence of its last change.
    pub revision: u64,
    /// The schedule it belongs to.
    pub schedule_id: Uuid,
    /// The snooze length in ms.
    pub snooze_ms: u64,
    /// The sound.
    pub sound: AlertSound,
    /// The alert's own gain; `None` (JSON null) rings at the media volume.
    pub volume: Option<AlertVolume>,
}

/// One tombstone object.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TombstoneObject {
    /// The occurrence it removes.
    pub occurrence_id: Uuid,
    /// The delivery sequence of the removal.
    pub revision: u64,
    /// Why.
    pub tombstone: TombstoneReason,
}

/// An object of `alert.delta`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
pub enum CacheObject {
    /// A new or changed occurrence.
    Occurrence(OccurrenceObject),
    /// A removed occurrence.
    Tombstone(TombstoneObject),
}

/// One `alert.snapshot` page.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AlertSnapshotPage {
    /// The delivery epoch the snapshot starts.
    pub delivery_epoch: String,
    /// The delivery sequence the snapshot brings the device to.
    pub high_water_mark: u64,
    /// This page's position, from 0.
    pub page_index: usize,
    /// The number of pages.
    pub page_count: usize,
    /// The digest of the canonical JSON array of every page's objects.
    pub sha256: Sha256,
    /// At most [`SNAPSHOT_PAGE_OBJECTS`] live occurrences.
    pub objects: Vec<OccurrenceObject>,
}

impl ControlBody for AlertSnapshotPage {
    const MESSAGE_TYPE: MessageType = MessageType::AlertSnapshot;
}

/// `alert.delta`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct AlertDelta {
    /// The delivery epoch.
    pub delivery_epoch: String,
    /// The delta's sequence: the device's acknowledged sequence plus one.
    pub sequence: u64,
    /// The changed objects.
    pub objects: Vec<CacheObject>,
}

impl ControlBody for AlertDelta {
    const MESSAGE_TYPE: MessageType = MessageType::AlertDelta;
}

/// Why a snapshot cannot be sent.
#[derive(Clone, Debug, PartialEq)]
pub enum SnapshotError {
    /// A label longer than [`MAX_LABEL_CHARS`].
    LabelTooLong {
        /// The occurrence.
        occurrence_id: Uuid,
        /// Its label's length.
        chars: usize,
    },
    /// An object without a canonical form.
    Canonical(CanonicalError),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::LabelTooLong {
                occurrence_id,
                chars,
            } => {
                write!(
                    f,
                    "occurrence {occurrence_id} label of {chars} characters exceeds {MAX_LABEL_CHARS}"
                )
            }
            SnapshotError::Canonical(err) => write!(f, "snapshot has no canonical form: {err}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// The digest of an ordered object array: the SHA-256 of its canonical JSON.
pub fn snapshot_digest(ordered: &[OccurrenceObject]) -> Result<Sha256, SnapshotError> {
    let value = serde_json::to_value(ordered).expect("occurrence objects serialize");
    let text = canonical_json(&value).map_err(SnapshotError::Canonical)?;
    Ok(Sha256::of(text.as_bytes()))
}

/// The `alert.snapshot` pages for the live occurrences `objects`: ordered by
/// `(due_utc_ms, occurrence_id)`, at most [`SNAPSHOT_PAGE_OBJECTS`] per page,
/// every page carrying the digest of the whole ordered array. An empty cache
/// is one empty page.
pub fn snapshot_pages(
    delivery_epoch: &str,
    high_water_mark: u64,
    mut objects: Vec<OccurrenceObject>,
) -> Result<Vec<AlertSnapshotPage>, SnapshotError> {
    for object in &objects {
        let chars = object.label.chars().count();
        if chars > MAX_LABEL_CHARS {
            return Err(SnapshotError::LabelTooLong {
                occurrence_id: object.occurrence_id,
                chars,
            });
        }
    }
    // Lowercase hyphenated UUID text sorts in the same order as the UUID's
    // bytes, so comparing the ids orders them as their wire text.
    objects.sort_by_key(|o| (o.due_utc_ms, o.occurrence_id));
    let sha256 = snapshot_digest(&objects)?;
    let page_count = objects.len().div_ceil(SNAPSHOT_PAGE_OBJECTS).max(1);
    let mut pages = Vec::with_capacity(page_count);
    let mut rest = objects.into_iter();
    for page_index in 0..page_count {
        pages.push(AlertSnapshotPage {
            delivery_epoch: delivery_epoch.to_owned(),
            high_water_mark,
            page_index,
            page_count,
            sha256: sha256.clone(),
            objects: rest.by_ref().take(SNAPSHOT_PAGE_OBJECTS).collect(),
        });
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::canonical::canonical_json_text;

    fn occurrence(due_utc_ms: u64, id: u128, volume: Option<f64>) -> OccurrenceObject {
        OccurrenceObject {
            due_local: "2026-09-23T06:39:01-05:00".into(),
            due_utc_ms,
            kind: OccurrenceKind::Alarm,
            label: "Wake up".into(),
            loop_gap_ms: 2_000,
            max_ring_ms: NonZeroU64::new(600_000).unwrap(),
            occurrence_id: Uuid::from_u128(id),
            ramp_ms: 20_000,
            revision: 17,
            schedule_id: Uuid::parse_str("9cc967de-5fa3-5373-b4f0-d9841f3f79f7").unwrap(),
            snooze_ms: 540_000,
            sound: AlertSound::Fallback,
            volume: volume.map(|v| AlertVolume::new(v).unwrap()),
        }
    }

    #[test]
    fn an_occurrence_serializes_with_the_documented_field_types() {
        let value = serde_json::to_value(occurrence(1_790_000_000_000, 1, Some(0.5))).unwrap();
        assert_eq!(
            value,
            json!({"due_local": "2026-09-23T06:39:01-05:00", "due_utc_ms": "1790000000000", "kind": "alarm",
                   "label": "Wake up", "loop_gap_ms": 2000, "max_ring_ms": 600000,
                   "occurrence_id": "00000000-0000-0000-0000-000000000001", "ramp_ms": 20000, "revision": 17,
                   "schedule_id": "9cc967de-5fa3-5373-b4f0-d9841f3f79f7", "snooze_ms": 540000,
                   "sound": "builtin:fallback", "volume": 0.5})
        );
        let silent = serde_json::to_value(occurrence(1, 1, None)).unwrap();
        assert_eq!(
            silent["volume"],
            Value::Null,
            "null volume means the media volume, never zero"
        );
        let tomb = TombstoneObject {
            occurrence_id: Uuid::from_u128(2),
            revision: 9,
            tombstone: TombstoneReason::Snoozed,
        };
        assert_eq!(
            serde_json::to_value(CacheObject::Tombstone(tomb)).unwrap(),
            json!({"occurrence_id": "00000000-0000-0000-0000-000000000002", "revision": 9, "tombstone": "snoozed"})
        );
        assert_eq!(AlertVolume::new(0.0), None);
        assert_eq!(AlertVolume::new(1.5), None);
        assert!(AlertVolume::new(f64::NAN).is_none());
    }

    #[test]
    fn the_digest_is_over_the_text_the_device_re_encodes() {
        let objects = vec![occurrence(5, 1, Some(0.25)), occurrence(6, 2, None)];
        let wire = serde_json::to_string(&objects).unwrap();
        let firmware_view = canonical_json_text(&wire).unwrap();
        assert_eq!(
            snapshot_digest(&objects).unwrap(),
            Sha256::of(firmware_view.as_bytes())
        );
        assert!(firmware_view.starts_with(
            "[{\"due_local\":\"2026-09-23T06:39:01-05:00\",\"due_utc_ms\":\"5\",\"kind\":\"alarm\""
        ));
    }

    #[test]
    fn an_empty_cache_is_one_empty_page_hashing_the_empty_array() {
        let pages = snapshot_pages("e1", 0, Vec::new()).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!((pages[0].page_index, pages[0].page_count), (0, 1));
        assert!(pages[0].objects.is_empty());
        assert_eq!(pages[0].sha256, Sha256::of(b"[]"));
    }

    #[test]
    fn pages_hold_128_objects_in_due_then_id_order_under_one_digest() {
        let mut objects: Vec<_> = (0..300u128)
            .map(|i| occurrence(1_000 + (i as u64 % 7), 1_000 - i, None))
            .collect();
        objects.reverse();
        let pages = snapshot_pages("epoch-3", 42, objects).unwrap();
        assert_eq!(
            pages.iter().map(|p| p.objects.len()).collect::<Vec<_>>(),
            [128, 128, 44]
        );
        assert!(pages.iter().all(|p| p.page_count == 3
            && p.high_water_mark == 42
            && p.delivery_epoch == "epoch-3"));
        assert!(pages.iter().all(|p| p.sha256 == pages[0].sha256));
        assert_eq!(
            pages.iter().map(|p| p.page_index).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        let all: Vec<OccurrenceObject> = pages.iter().flat_map(|p| p.objects.clone()).collect();
        assert!(
            all.windows(2)
                .all(|w| (w[0].due_utc_ms, w[0].occurrence_id.to_string())
                    <= (w[1].due_utc_ms, w[1].occurrence_id.to_string()))
        );
        assert_eq!(snapshot_digest(&all).unwrap(), pages[0].sha256);
    }

    #[test]
    fn a_label_the_device_would_refuse_stops_the_snapshot() {
        let mut long = occurrence(1, 1, None);
        long.label = "é".repeat(MAX_LABEL_CHARS + 1);
        assert!(matches!(
            snapshot_pages("e", 1, vec![long.clone()]),
            Err(SnapshotError::LabelTooLong { chars: 121, .. })
        ));
        long.label = "é".repeat(MAX_LABEL_CHARS);
        assert!(snapshot_pages("e", 1, vec![long]).is_ok());
    }

    #[test]
    fn snapshot_and_delta_bodies_have_the_documented_shape() {
        let pages = snapshot_pages("e", 3, vec![occurrence(1, 1, None)]).unwrap();
        let page = serde_json::to_value(&pages[0]).unwrap();
        let keys: Vec<&str> = page
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 6);
        for key in [
            "delivery_epoch",
            "high_water_mark",
            "page_index",
            "page_count",
            "sha256",
            "objects",
        ] {
            assert!(keys.contains(&key), "{key}");
        }
        let delta = AlertDelta {
            delivery_epoch: "e".into(),
            sequence: 4,
            objects: vec![CacheObject::Tombstone(TombstoneObject {
                occurrence_id: Uuid::from_u128(1),
                revision: 4,
                tombstone: TombstoneReason::Deleted,
            })],
        };
        assert_eq!(
            serde_json::to_value(&delta).unwrap(),
            json!({"delivery_epoch": "e", "sequence": 4,
                   "objects": [{"occurrence_id": "00000000-0000-0000-0000-000000000001", "revision": 4, "tombstone": "deleted"}]})
        );
    }
}
