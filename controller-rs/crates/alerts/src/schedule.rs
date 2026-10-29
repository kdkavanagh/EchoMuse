//! Alarm times in Home Assistant's time zone (§3.3.4, §4.9): when a new alarm first rings,
//! its recurrence rule, and the text forms of instants the calendar and the device use.
//!
//! Alarms are wall-clock schedules. A local time that does not exist (the hour skipped
//! when clocks go forward) resolves to the same distance after the gap, and a local time
//! that happens twice (when clocks go back) resolves to the first, which is how HA's
//! calendar library expands recurrence (architecture §10.4: a repeating 02:30 alarm on the
//! spring-forward day rings at 03:30, and a repeating 01:30 alarm rings once, at the
//! first 01:30). Expanding a stored series is HA's job; this module only places the first
//! instance of a new alarm.

use std::fmt;

use jiff::{
    Timestamp, Zoned,
    civil::{Date, DateTime, Weekday},
    tz::TimeZone,
};

/// Home Assistant's configured time zone.
#[derive(Clone, Debug)]
pub struct HaTimeZone {
    name: String,
    zone: TimeZone,
}

impl PartialEq for HaTimeZone {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

/// A time zone name the time zone database does not know.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownTimeZone(pub String);

impl fmt::Display for UnknownTimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown time zone {:?}", self.0)
    }
}

impl std::error::Error for UnknownTimeZone {}

impl HaTimeZone {
    /// The zone with IANA name `name`, as `get_config` reports it.
    pub fn named(name: &str) -> Result<Self, UnknownTimeZone> {
        TimeZone::get(name)
            .map(|zone| Self {
                name: name.to_owned(),
                zone,
            })
            .map_err(|_| UnknownTimeZone(name.to_owned()))
    }

    /// The IANA name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The instant `utc_ms` in this zone.
    pub fn at_ms(&self, utc_ms: i64) -> Option<Zoned> {
        Timestamp::from_millisecond(utc_ms)
            .ok()
            .map(|ts| ts.to_zoned(self.zone.clone()))
    }

    /// RFC 3339 at second precision with this zone's offset at that instant, the form of
    /// a one-shot occurrence key and of `due_local`.
    pub fn local_text(&self, instant: Timestamp) -> String {
        local_text(&instant.to_zoned(self.zone.clone()))
    }

    fn resolve(&self, local: DateTime) -> Option<Zoned> {
        local.to_zoned(self.zone.clone()).ok()
    }
}

/// RFC 3339 at second precision with the instant's own offset (`2026-09-23T06:30:00-05:00`).
pub fn local_text(zoned: &Zoned) -> String {
    zoned.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// Reads an RFC 3339 / ISO 8601 instant with an offset, as HA writes calendar times.
pub fn parse_instant(text: &str) -> Option<Timestamp> {
    text.parse::<Timestamp>().ok()
}

/// `text` (RFC 3339 with a numeric offset) moved by `ms` milliseconds and written in the
/// same offset: a snooze child's end keeps the offset of the start the device derived,
/// as HA's event schema requires start and end in one time zone.
pub fn shift_same_offset(text: &str, ms: i64) -> Option<String> {
    let instant = parse_instant(text)?;
    let tail = text.get(text.len().checked_sub(6)?..)?;
    let sign = match tail.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hours: i32 = tail.get(1..3)?.parse().ok()?;
    let minutes: i32 = tail.get(4..6)?.parse().ok()?;
    let offset = jiff::tz::Offset::from_seconds(sign * (hours * 3600 + minutes * 60)).ok()?;
    let shifted = instant
        .checked_add(jiff::SignedDuration::from_millis(ms))
        .ok()?;
    Some(local_text(&shifted.to_zoned(TimeZone::fixed(offset))))
}

/// A time of day.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TimeOfDay {
    hour: i8,
    minute: i8,
    second: i8,
}

impl TimeOfDay {
    /// `hour:minute:second` on a 24-hour clock; `None` outside it.
    pub fn new(hour: u8, minute: u8, second: u8) -> Option<Self> {
        (hour < 24 && minute < 60 && second < 60).then_some(Self {
            hour: hour as i8,
            minute: minute as i8,
            second: second as i8,
        })
    }

    /// Parses `HH:MM` or `HH:MM:SS` (the relay's rule; one-digit hours are accepted).
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split(':');
        let mut field = |max_len: usize| -> Option<Option<u8>> {
            match parts.next() {
                None => Some(None),
                Some(p)
                    if !p.is_empty()
                        && p.len() <= max_len
                        && p.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    Some(Some(p.parse().ok()?))
                }
                Some(_) => None,
            }
        };
        let hour = field(2)??;
        let minute = field(2)??;
        let second = field(2)?.unwrap_or(0);
        if parts.next().is_some() {
            return None;
        }
        Self::new(hour, minute, second)
    }

    /// `HH:MM:SS`.
    pub fn text(self) -> String {
        format!("{:02}:{:02}:{:02}", self.hour, self.minute, self.second)
    }

    /// The hour.
    pub fn hour(self) -> u8 {
        self.hour as u8
    }

    /// The minute.
    pub fn minute(self) -> u8 {
        self.minute as u8
    }

    /// The second.
    pub fn second(self) -> u8 {
        self.second as u8
    }

    fn on(self, date: Date) -> DateTime {
        date.at(self.hour, self.minute, self.second, 0)
    }
}

/// A set of weekdays.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Weekdays(u8);

/// The weekdays in order, Monday first, with their `BYDAY` and relay spellings.
const DAYS: [(Weekday, &str, &str); 7] = [
    (Weekday::Monday, "MO", "mon"),
    (Weekday::Tuesday, "TU", "tue"),
    (Weekday::Wednesday, "WE", "wed"),
    (Weekday::Thursday, "TH", "thu"),
    (Weekday::Friday, "FR", "fri"),
    (Weekday::Saturday, "SA", "sat"),
    (Weekday::Sunday, "SU", "sun"),
];

impl Weekdays {
    /// No day.
    pub const NONE: Weekdays = Weekdays(0);
    /// Every day.
    pub const ALL: Weekdays = Weekdays(0x7f);

    fn bit(day: Weekday) -> u8 {
        1 << day.to_monday_zero_offset()
    }

    /// The set with `day` added.
    pub fn with(self, day: Weekday) -> Self {
        Self(self.0 | Self::bit(day))
    }

    /// Whether `day` is in the set.
    pub fn contains(self, day: Weekday) -> bool {
        self.0 & Self::bit(day) != 0
    }

    /// Whether the set is empty.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Reads the relay's day names (`mon` … `sun`), case-insensitively.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut set = Self::NONE;
        for name in names {
            let (day, _, _) = DAYS
                .iter()
                .find(|(_, _, short)| short.eq_ignore_ascii_case(name.trim()))?;
            set = set.with(*day);
        }
        Some(set)
    }

    /// The relay's day names, Monday first.
    pub fn names(self) -> Vec<&'static str> {
        DAYS.iter()
            .filter(|(day, _, _)| self.contains(*day))
            .map(|(_, _, short)| *short)
            .collect()
    }

    fn byday(self) -> String {
        DAYS.iter()
            .filter(|(day, _, _)| self.contains(*day))
            .map(|(_, code, _)| *code)
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// How an alarm repeats.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Repeat {
    /// Once.
    Once,
    /// Every day (`FREQ=DAILY`).
    Daily,
    /// On some weekdays (`FREQ=WEEKLY;BYDAY=…`); never empty and never all seven.
    Weekly(Weekdays),
}

impl Repeat {
    /// The repeat for a set of days: none is once, all seven is daily.
    pub fn on(days: Weekdays) -> Self {
        if days.is_empty() {
            Repeat::Once
        } else if days == Weekdays::ALL {
            Repeat::Daily
        } else {
            Repeat::Weekly(days)
        }
    }

    /// The days it rings on, all seven for a daily alarm and none for a one-shot.
    pub fn days(self) -> Weekdays {
        match self {
            Repeat::Once => Weekdays::NONE,
            Repeat::Daily => Weekdays::ALL,
            Repeat::Weekly(days) => days,
        }
    }

    /// The calendar's recurrence rule; `None` for a one-shot alarm.
    pub fn rrule(self) -> Option<String> {
        match self {
            Repeat::Once => None,
            Repeat::Daily => Some("FREQ=DAILY".to_owned()),
            Repeat::Weekly(days) => Some(format!("FREQ=WEEKLY;BYDAY={}", days.byday())),
        }
    }
}

/// When a new alarm rings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AlarmWhen {
    /// The time of day.
    pub time: TimeOfDay,
    /// The first day it may ring; `None` means the next time the time comes round.
    pub date: Option<Date>,
    /// How it repeats.
    pub repeat: Repeat,
}

/// Why an alarm has no first instance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScheduleError {
    /// A one-shot alarm on a date and time already past.
    InThePast,
    /// The date is outside the calendar's range.
    OutOfRange,
}

impl fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ScheduleError::InThePast => "that time has already passed",
            ScheduleError::OutOfRange => "that date is out of range",
        })
    }
}

impl std::error::Error for ScheduleError {}

impl AlarmWhen {
    /// The first instant after `now` at which the alarm rings, in `zone`: the first day
    /// from `date` (or today) that the repeat allows whose local time is still ahead.
    pub fn first_due(&self, now: Timestamp, zone: &HaTimeZone) -> Result<Zoned, ScheduleError> {
        let today = now.to_zoned(zone.zone.clone()).date();
        let start = self.date.unwrap_or(today);
        if self.repeat == Repeat::Once && self.date.is_some() {
            let due = zone
                .resolve(self.time.on(start))
                .ok_or(ScheduleError::OutOfRange)?;
            return if due.timestamp() > now {
                Ok(due)
            } else {
                Err(ScheduleError::InThePast)
            };
        }
        let days = match self.repeat {
            Repeat::Once | Repeat::Daily => Weekdays::ALL,
            Repeat::Weekly(days) => days,
        };
        let mut date = start;
        // Eight days reach the same weekday a week later, which a time already past
        // today needs.
        for _ in 0..8 {
            if days.contains(date.weekday()) {
                let due = zone
                    .resolve(self.time.on(date))
                    .ok_or(ScheduleError::OutOfRange)?;
                if due.timestamp() > now {
                    return Ok(due);
                }
            }
            date = date.tomorrow().map_err(|_| ScheduleError::OutOfRange)?;
        }
        Err(ScheduleError::OutOfRange)
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    fn chicago() -> HaTimeZone {
        HaTimeZone::named("America/Chicago").unwrap()
    }

    fn at(text: &str) -> Timestamp {
        parse_instant(text).unwrap()
    }

    fn weekly(days: &[&str]) -> Repeat {
        Repeat::on(Weekdays::from_names(days.iter().copied()).unwrap())
    }

    #[test]
    fn a_weekly_alarm_keeps_its_wall_time_across_the_autumn_change() {
        // Friday 2026-10-30 in Chicago (CDT, -05:00); clocks go back on Sunday 1 November.
        let when = AlarmWhen {
            time: TimeOfDay::new(7, 0, 0).unwrap(),
            date: None,
            repeat: weekly(&["mon"]),
        };
        let due = when
            .first_due(at("2026-10-30T15:00:00-05:00"), &chicago())
            .unwrap();
        assert_eq!(local_text(&due), "2026-11-02T07:00:00-06:00");
        assert_eq!(due.timestamp(), at("2026-11-02T13:00:00Z"));
    }

    #[test]
    fn a_time_skipped_in_spring_rings_after_the_gap_and_a_repeated_one_rings_first() {
        let zone = chicago();
        // Sunday 2027-03-14: 02:00 CST jumps to 03:00 CDT.
        let spring = AlarmWhen {
            time: TimeOfDay::new(2, 30, 0).unwrap(),
            date: None,
            repeat: weekly(&["sun"]),
        };
        let due = spring
            .first_due(at("2027-03-13T12:00:00-06:00"), &zone)
            .unwrap();
        assert_eq!(local_text(&due), "2027-03-14T03:30:00-05:00");
        assert_eq!(due.timestamp(), at("2027-03-14T08:30:00Z"));

        // Sunday 2026-11-01: 01:30 happens twice; the first is CDT.
        let autumn = AlarmWhen {
            time: TimeOfDay::new(1, 30, 0).unwrap(),
            date: None,
            repeat: Repeat::Daily,
        };
        let due = autumn
            .first_due(at("2026-10-31T23:00:00-05:00"), &zone)
            .unwrap();
        assert_eq!(local_text(&due), "2026-11-01T01:30:00-05:00");
    }

    #[test]
    fn the_first_instance_is_the_next_allowed_day_still_ahead() {
        let zone = chicago();
        let now = at("2026-10-28T08:00:00-05:00"); // a Wednesday
        let seven = TimeOfDay::new(7, 0, 0).unwrap();
        let once = AlarmWhen {
            time: seven,
            date: None,
            repeat: Repeat::Once,
        };
        assert_eq!(
            local_text(&once.first_due(now, &zone).unwrap()),
            "2026-10-29T07:00:00-05:00"
        );
        let wednesdays = AlarmWhen {
            time: seven,
            date: None,
            repeat: weekly(&["wed"]),
        };
        assert_eq!(
            local_text(&wednesdays.first_due(now, &zone).unwrap()),
            "2026-11-04T07:00:00-06:00",
            "today's 07:00 has passed, so a week later"
        );
        let dated = AlarmWhen {
            time: seven,
            date: Some(date(2026, 10, 28)),
            repeat: Repeat::Once,
        };
        assert_eq!(dated.first_due(now, &zone), Err(ScheduleError::InThePast));
    }

    #[test]
    fn rules_and_times_have_the_calendar_spellings() {
        assert_eq!(Repeat::on(Weekdays::NONE).rrule(), None);
        assert_eq!(Repeat::on(Weekdays::ALL).rrule().unwrap(), "FREQ=DAILY");
        assert_eq!(
            weekly(&["fri", "mon", "Wed"]).rrule().unwrap(),
            "FREQ=WEEKLY;BYDAY=MO,WE,FR"
        );
        assert_eq!(Weekdays::from_names(["mon", "funday"]), None);
        assert_eq!(TimeOfDay::parse("7:05").unwrap().text(), "07:05:00");
        assert_eq!(TimeOfDay::parse("23:59:59").unwrap().second(), 59);
        for bad in ["24:00", "7", "07:60", "07:00:00:00", "seven", "07:0a", ""] {
            assert_eq!(TimeOfDay::parse(bad), None, "{bad}");
        }
        assert_eq!(
            chicago().local_text(at("2026-09-23T11:30:00.250Z")),
            "2026-09-23T06:30:00-05:00"
        );
        let utc = HaTimeZone::named("UTC").unwrap();
        assert_eq!(
            utc.local_text(at("2026-09-23T11:30:00Z")),
            "2026-09-23T11:30:00+00:00"
        );
        assert!(HaTimeZone::named("Mars/Olympus").is_err());
    }
}
