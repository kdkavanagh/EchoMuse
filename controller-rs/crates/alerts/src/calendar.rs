//! Occurrences read from a speaker's Local Calendar (§3.3.4, architecture §10.2, §16.3).
//!
//! Each event is a schedule. Its id is the `echomuse:` line's `id`, or for an event made
//! in HA's UI `uuid5(URL, "<calendar entity>/<uid>")`. An occurrence's key is the
//! instance's `recurrence_id` for a repeating event, otherwise its start in ISO 8601 with
//! offset at second precision, and its id is `uuid5(schedule_id, key)`. HA expands
//! recurrence in its own time zone; that expansion is used as it is.

use echomuse_core::services::HaConnectionGeneration;
use echomuse_ha::{CalendarItem, CalendarTime};
use echomuse_protocol::alerts::{AlertSound, OccurrenceKind, OccurrenceObject};
use uuid::Uuid;

use crate::{
    description::{EchomuseLine, LineOf, ParentRef, RingSettings, defaults, device_label},
    ids,
    schedule::{HaTimeZone, parse_instant},
};

/// Where an event came from.
#[derive(Clone, Debug, PartialEq)]
pub enum EventOrigin {
    /// Written by EchoMuse; the operation that created it.
    EchoMuse {
        /// The creating operation's id.
        op: Uuid,
    },
    /// Made in HA's UI or by an automation.
    Ui,
    /// It has an `echomuse:` line that cannot be read; treated as a UI event and flagged.
    MalformedLine(String),
}

/// One occurrence on a speaker's calendar.
#[derive(Clone, Debug, PartialEq)]
pub struct Occurrence {
    /// `uuid5(schedule_id, key)`.
    pub occurrence_id: Uuid,
    /// The schedule (event) it belongs to.
    pub schedule_id: Uuid,
    /// The occurrence key.
    pub key: String,
    /// Alarm or snooze child.
    pub kind: OccurrenceKind,
    /// The event's title as HA has it.
    pub summary: String,
    /// When it rings, UTC ms.
    pub due_utc_ms: i64,
    /// When it rings, RFC 3339 in HA's time zone.
    pub due_local: String,
    /// How it rings.
    pub ring: RingSettings,
    /// The event's uid.
    pub uid: String,
    /// The instance id of a repeating event.
    pub recurrence_id: Option<String>,
    /// The series' recurrence rule.
    pub rrule: Option<String>,
    /// The occurrence a snooze child replaces.
    pub parent: Option<ParentRef>,
    /// Where the event came from.
    pub origin: EventOrigin,
}

impl Occurrence {
    /// The cache object the device receives, at delivery revision `revision`.
    pub fn object(&self, revision: u64) -> OccurrenceObject {
        OccurrenceObject {
            due_local: self.due_local.clone(),
            due_utc_ms: self.due_utc_ms.max(0) as u64,
            kind: self.kind,
            label: device_label(&self.summary),
            loop_gap_ms: self.ring.loop_gap_ms,
            max_ring_ms: self.ring.max_ring_ms,
            occurrence_id: self.occurrence_id,
            ramp_ms: self.ring.ramp_ms,
            revision,
            schedule_id: self.schedule_id,
            snooze_ms: self.ring.snooze_ms,
            sound: self.ring.sound.clone(),
            volume: self.ring.volume,
        }
    }

    /// The creating operation, for an EchoMuse event.
    pub fn creating_op(&self) -> Option<Uuid> {
        match self.origin {
            EventOrigin::EchoMuse { op } => Some(op),
            _ => None,
        }
    }
}

/// An item of the calendar that is not an alarm occurrence, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Skipped {
    /// An all-day event: it has no time to ring at.
    AllDay(String),
    /// An item without a uid, so it has no stable identity.
    NoUid(String),
    /// A start time that cannot be read.
    UnreadableStart(String),
}

/// The occurrences of one calendar read.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Occurrences {
    /// Occurrences ordered by due time, then id.
    pub items: Vec<Occurrence>,
    /// Items that are not alarm occurrences.
    pub skipped: Vec<Skipped>,
}

/// Reads `items` of `calendar_entity` in `zone`. An event without a valid `echomuse:`
/// line rings with the defaults and `alarm_sound`, the speaker's effective alarm sound.
pub fn occurrences(
    items: &[CalendarItem],
    calendar_entity: &str,
    zone: &HaTimeZone,
    alarm_sound: &AlertSound,
) -> Occurrences {
    let mut read = Occurrences::default();
    for item in items {
        let CalendarTime::DateTime(start) = &item.start else {
            read.skipped.push(Skipped::AllDay(item.summary.clone()));
            continue;
        };
        let Some(uid) = item.uid.clone() else {
            read.skipped.push(Skipped::NoUid(item.summary.clone()));
            continue;
        };
        let Some(instant) = parse_instant(start) else {
            read.skipped.push(Skipped::UnreadableStart(start.clone()));
            continue;
        };
        let due_local = zone.local_text(instant);
        let (schedule_id, kind, ring, parent, origin) =
            match EchomuseLine::parse(item.description.as_deref()) {
                LineOf::Valid(line) => (
                    line.id,
                    line.kind,
                    line.ring,
                    line.parent,
                    EventOrigin::EchoMuse { op: line.op },
                ),
                other => (
                    ids::ui_schedule_id(calendar_entity, &uid),
                    OccurrenceKind::Alarm,
                    RingSettings::defaults(alarm_sound.clone()),
                    None,
                    match other {
                        LineOf::Malformed(why) => EventOrigin::MalformedLine(why),
                        _ => EventOrigin::Ui,
                    },
                ),
            };
        let key = item
            .recurrence_id
            .clone()
            .unwrap_or_else(|| due_local.clone());
        read.items.push(Occurrence {
            occurrence_id: ids::occurrence_id(schedule_id, &key),
            schedule_id,
            key,
            kind,
            summary: if item.summary.is_empty() {
                defaults::LABEL.to_owned()
            } else {
                item.summary.clone()
            },
            due_utc_ms: instant.as_millisecond(),
            due_local,
            ring,
            uid,
            recurrence_id: item.recurrence_id.clone(),
            rrule: item.rrule.clone(),
            parent,
            origin,
        });
    }
    read.items.sort_by_key(|a| (a.due_utc_ms, a.occurrence_id));
    read
}

/// One push of a speaker's calendar subscription, read.
#[derive(Clone, Debug, PartialEq)]
pub struct CalendarView {
    /// The HA connection the push arrived on.
    pub generation: HaConnectionGeneration,
    /// Counts pushes of this speaker in this process; a later push has a higher number.
    pub push: u64,
    /// The window's start, UTC ms.
    pub window_start_ms: i64,
    /// The window's end, UTC ms.
    pub window_end_ms: i64,
    /// The occurrences in the window.
    pub occurrences: Occurrences,
}

impl CalendarView {
    /// The occurrence with id `id`.
    pub fn occurrence(&self, id: Uuid) -> Option<&Occurrence> {
        self.occurrences
            .items
            .iter()
            .find(|o| o.occurrence_id == id)
    }

    /// The occurrence of `schedule_id` with key `key`.
    pub fn find(&self, schedule_id: Uuid, key: &str) -> Option<&Occurrence> {
        self.occurrences
            .items
            .iter()
            .find(|o| o.schedule_id == schedule_id && o.key == key)
    }

    /// The uid of the event of `schedule_id`, when any of its occurrences is in the window.
    pub fn schedule_uid(&self, schedule_id: Uuid) -> Option<&str> {
        self.occurrences
            .items
            .iter()
            .find(|o| o.schedule_id == schedule_id)
            .map(|o| o.uid.as_str())
    }

    /// The occurrence of an event created by operation `op`.
    pub fn created_by(&self, op: Uuid) -> Option<&Occurrence> {
        self.occurrences
            .items
            .iter()
            .find(|o| o.creating_op() == Some(op))
    }
}

#[cfg(test)]
mod tests {
    use echomuse_protocol::Sha256;

    use super::*;
    use crate::description::ParentRef;

    fn chicago() -> HaTimeZone {
        HaTimeZone::named("America/Chicago").unwrap()
    }

    fn timed(
        start: &str,
        uid: &str,
        description: Option<String>,
        rid: Option<&str>,
    ) -> CalendarItem {
        CalendarItem {
            summary: "Wake up".to_owned(),
            start: CalendarTime::DateTime(start.to_owned()),
            end: CalendarTime::DateTime(start.to_owned()),
            description,
            uid: Some(uid.to_owned()),
            recurrence_id: rid.map(str::to_owned),
            rrule: rid.map(|_| "FREQ=DAILY".to_owned()),
        }
    }

    #[test]
    fn identity_follows_the_line_or_the_calendar_and_uid() {
        let sound = AlertSound::Asset(Sha256::of(b"default"));
        let line = EchomuseLine {
            id: Uuid::from_u128(7),
            kind: OccurrenceKind::Snooze,
            op: Uuid::from_u128(8),
            parent: Some(ParentRef {
                id: Uuid::from_u128(9),
                occ: "k".to_owned(),
            }),
            ring: RingSettings::defaults(AlertSound::Fallback),
        };
        let items = [
            timed(
                "2026-09-23T06:39:01.000000-05:00",
                "child",
                Some(line.description(None)),
                None,
            ),
            timed(
                "2026-09-24T06:30:00-05:00",
                "abc",
                None,
                Some("20260924T063000"),
            ),
            CalendarItem {
                start: CalendarTime::Date("2026-09-24".to_owned()),
                ..timed("x", "allday", None, None)
            },
            timed(
                "2026-09-25T06:30:00-05:00",
                "bad",
                Some("echomuse: {}".to_owned()),
                None,
            ),
        ];
        let read = occurrences(&items, "calendar.echomuse_office", &chicago(), &sound);
        assert_eq!(read.skipped, [Skipped::AllDay("Wake up".to_owned())]);
        let [child, ui, bad] = &read.items[..] else {
            panic!("{:?}", read.items);
        };
        // The firmware vector: the child's key is its start at second precision.
        assert_eq!(child.key, "2026-09-23T06:39:01-05:00");
        assert_eq!(
            child.occurrence_id,
            ids::occurrence_id(Uuid::from_u128(7), "2026-09-23T06:39:01-05:00")
        );
        assert_eq!(child.kind, OccurrenceKind::Snooze);
        assert_eq!(child.creating_op(), Some(Uuid::from_u128(8)));

        assert_eq!(
            ui.schedule_id.to_string(),
            "09e72c24-77e1-5473-859a-025b5a1115df"
        );
        assert_eq!(ui.key, "20260924T063000");
        assert_eq!(ui.ring, RingSettings::defaults(sound.clone()));
        assert_eq!(ui.origin, EventOrigin::Ui);
        assert!(matches!(bad.origin, EventOrigin::MalformedLine(_)));
        assert_eq!(bad.ring.sound, sound);

        let object = ui.object(4);
        assert_eq!(object.due_local, "2026-09-24T06:30:00-05:00");
        assert_eq!(object.due_utc_ms, 1_790_249_400_000);
        assert_eq!(object.revision, 4);
    }
}
