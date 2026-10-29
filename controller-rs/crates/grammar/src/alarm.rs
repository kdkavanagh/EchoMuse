//! Alarm commands parsed from speech, and their mapping onto the alert service's requests
//! ([`echomuse_alerts`]).
//!
//! Times: a 12-hour time needs AM or PM (`7 am`, `7:30 in the evening`, `seven tonight`);
//! without it the request is a [`PendingAlarm`] and the controller asks "AM or PM?". Hours 13
//! to 23 and hour 0 are read on a 24-hour clock (`17:30`), and `noon` and `midnight` are
//! exact. `half past seven`, `quarter past seven` and `quarter to eight` are understood.
//!
//! Days: `today`, `tomorrow`, a weekday (`on Friday`, the next Friday, today included while the
//! time is still ahead), a date (`on October 30th`, `on the 30th`), or a repeat (`every day`,
//! `daily`, `weekdays`, `weekends`, `every Monday and Wednesday`, `on Saturdays`). With no day
//! the alarm rings the next time the time comes round.

use echomuse_alerts::{
    AlarmEntry, OpId, OpSource, SetAlarm,
    relay::CancelSelector,
    schedule::{AlarmWhen, Repeat, TimeOfDay, Weekdays},
};
use echomuse_core::types::DeviceId;
use echomuse_protocol::alerts::OccurrenceKind;
use jiff::civil::{Date, DateTime, Weekday};
use uuid::Uuid;

use crate::matcher::{Capture, integer};

/// AM or PM.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Meridiem {
    /// Before noon.
    Am,
    /// From noon.
    Pm,
}

/// A time of day as said: exact, or a 12-hour time without AM or PM.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SaidTime {
    /// An exact time.
    Exact(TimeOfDay),
    /// `hour` (1–12) and `minute`, AM or PM not said.
    Ambiguous {
        /// 1 to 12.
        hour: u8,
        /// 0 to 59.
        minute: u8,
    },
}

impl SaidTime {
    /// The time with `meridiem` applied; an exact time is unchanged.
    pub fn with_meridiem(self, meridiem: Meridiem) -> TimeOfDay {
        match self {
            SaidTime::Exact(time) => time,
            SaidTime::Ambiguous { hour, minute } => twelve_hour(hour, minute, meridiem),
        }
    }

    /// Whether `time` is what was said: equal, or for an ambiguous time, either reading.
    pub fn matches(self, time: TimeOfDay) -> bool {
        match self {
            SaidTime::Exact(exact) => {
                exact.hour() == time.hour() && exact.minute() == time.minute()
            }
            SaidTime::Ambiguous { hour, minute } => {
                time.minute() == minute && time.hour() % 12 == hour % 12
            }
        }
    }
}

fn twelve_hour(hour: u8, minute: u8, meridiem: Meridiem) -> TimeOfDay {
    let base = hour % 12;
    let hour = match meridiem {
        Meridiem::Am => base,
        Meridiem::Pm => base + 12,
    };
    TimeOfDay::new(hour, minute, 0).expect("hour and minute were range-checked")
}

/// Which day an alarm request names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlarmDay {
    /// No day: the next time the time comes round.
    Next,
    /// Today.
    Today,
    /// Tomorrow.
    Tomorrow,
    /// The next such weekday, today included while the time is still ahead.
    Weekday(Weekday),
    /// A date: `month` (1–12) and `day`; without a month, this month or the next that has
    /// that day.
    Date {
        /// 1 to 12; `None` when only the day was said.
        month: Option<u8>,
        /// 1 to 31.
        day: u8,
    },
    /// A repeating alarm.
    Repeat(Repeat),
}

/// A complete request to set an alarm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmRequest {
    /// When in the day.
    pub time: TimeOfDay,
    /// Which day, or how it repeats.
    pub day: AlarmDay,
    /// Its label, as said (normalized words); `None` for a plain alarm.
    pub label: Option<String>,
}

/// A request to set an alarm whose hour lacks AM or PM: the controller asks "AM or PM?".
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingAlarm {
    /// 1 to 12.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// Which day, or how it repeats.
    pub day: AlarmDay,
    /// Its label.
    pub label: Option<String>,
}

impl PendingAlarm {
    /// The request completed by the answer.
    pub fn with_meridiem(&self, meridiem: Meridiem) -> AlarmRequest {
        AlarmRequest {
            time: twelve_hour(self.hour, self.minute, meridiem),
            day: self.day,
            label: self.label.clone(),
        }
    }
}

/// Why a request names no day the calendar has.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DayError {
    /// The date does not exist (`February 30th`).
    NoSuchDate,
}

impl std::fmt::Display for DayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("that date does not exist")
    }
}

impl std::error::Error for DayError {}

impl AlarmRequest {
    /// When it rings, given the local date and time now in Home Assistant's time zone.
    pub fn when(&self, now: DateTime) -> Result<AlarmWhen, DayError> {
        let today = now.date();
        let ahead_today = self.time_on(today) > now;
        let once = |date: Date| AlarmWhen {
            time: self.time,
            date: Some(date),
            repeat: Repeat::Once,
        };
        Ok(match self.day {
            AlarmDay::Next => AlarmWhen {
                time: self.time,
                date: None,
                repeat: Repeat::Once,
            },
            AlarmDay::Today => once(today),
            AlarmDay::Tomorrow => once(today.tomorrow().map_err(|_| DayError::NoSuchDate)?),
            AlarmDay::Weekday(day) => {
                let mut date = today;
                if date.weekday() == day && !ahead_today {
                    date = date.tomorrow().map_err(|_| DayError::NoSuchDate)?;
                }
                while date.weekday() != day {
                    date = date.tomorrow().map_err(|_| DayError::NoSuchDate)?;
                }
                once(date)
            }
            AlarmDay::Date {
                month: Some(month),
                day,
            } => {
                let this_year = Date::new(today.year(), month as i8, day as i8)
                    .map_err(|_| DayError::NoSuchDate)?;
                if this_year > today || (this_year == today && ahead_today) {
                    once(this_year)
                } else {
                    // February 29th waits for the next leap year.
                    (1..=8)
                        .find_map(|years| {
                            Date::new(today.year() + years, month as i8, day as i8).ok()
                        })
                        .map(once)
                        .ok_or(DayError::NoSuchDate)?
                }
            }
            AlarmDay::Date { month: None, day } => {
                let mut first = today.first_of_month();
                let mut found = None;
                for _ in 0..13 {
                    if let Ok(date) = Date::new(first.year(), first.month(), day as i8)
                        && (date > today || (date == today && ahead_today))
                    {
                        found = Some(date);
                        break;
                    }
                    first = first
                        .last_of_month()
                        .tomorrow()
                        .map_err(|_| DayError::NoSuchDate)?;
                }
                once(found.ok_or(DayError::NoSuchDate)?)
            }
            AlarmDay::Repeat(repeat) => AlarmWhen {
                time: self.time,
                date: None,
                repeat,
            },
        })
    }

    fn time_on(&self, date: Date) -> DateTime {
        date.at(
            self.time.hour() as i8,
            self.time.minute() as i8,
            self.time.second() as i8,
            0,
        )
    }

    /// The alert service's request for this alarm on `endpoint`, under the voice operation
    /// id `op_id` ([`OpId::voice_alarm`] of the turn that asked).
    pub fn set_alarm(
        &self,
        op_id: OpId,
        endpoint: DeviceId,
        now: DateTime,
    ) -> Result<SetAlarm, DayError> {
        Ok(SetAlarm {
            op_id,
            endpoint,
            when: self.when(now)?,
            label: self.label.clone(),
            source: OpSource::Voice,
        })
    }
}

/// Which alarms a cancel names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AlarmCancel {
    /// `cancel my alarm`: the only one, or the controller asks which.
    Only,
    /// `cancel all my alarms`.
    All,
    /// `cancel my 7 am alarm`, or `cancel my 7 alarm` without AM or PM.
    At(SaidTime),
    /// `cancel my work alarm`.
    Labelled(String),
}

/// One alarm the speaker has, as a cancel or a question about alarms sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlarmChoice {
    /// Its schedule.
    pub schedule_id: Uuid,
    /// When it next rings, local date and time.
    pub due: DateTime,
    /// How it repeats.
    pub repeat: Repeat,
    /// Its label; "Alarm" counts as none.
    pub label: Option<String>,
    /// A snoozed alarm's next ring (the snooze child), not a schedule of its own.
    pub snoozed: bool,
}

impl AlarmChoice {
    /// Reads the alert service's listing: one choice per schedule, at its soonest
    /// occurrence, soonest first. Entries whose time cannot be read are left out.
    pub fn from_entries(entries: &[AlarmEntry]) -> Vec<AlarmChoice> {
        let mut out: Vec<AlarmChoice> = Vec::new();
        let mut sorted: Vec<&AlarmEntry> = entries.iter().collect();
        sorted.sort_by_key(|e| e.due_utc_ms);
        for entry in sorted {
            if out.iter().any(|c| c.schedule_id == entry.schedule_id) {
                continue;
            }
            let Some(due) = local_datetime(&entry.due_local) else {
                continue;
            };
            let label = (!entry.label.trim().is_empty()
                && !entry.label.eq_ignore_ascii_case("alarm"))
            .then(|| entry.label.trim().to_owned());
            out.push(AlarmChoice {
                schedule_id: entry.schedule_id,
                due,
                repeat: entry.rrule.as_deref().map_or(Repeat::Once, parse_rrule),
                label,
                snoozed: entry.kind == OccurrenceKind::Snooze,
            });
        }
        out
    }

    /// Its time of day.
    pub fn time(&self) -> TimeOfDay {
        TimeOfDay::new(
            self.due.hour() as u8,
            self.due.minute() as u8,
            self.due.second() as u8,
        )
        .expect("a civil time is a valid time of day")
    }
}

/// The local date and time of an RFC 3339 text with an offset, as written in it.
pub(crate) fn local_datetime(text: &str) -> Option<DateTime> {
    text.get(..19)?.parse().ok()
}

/// Reads the recurrence rules the alert service writes (`FREQ=DAILY`,
/// `FREQ=WEEKLY;BYDAY=MO,WE`); anything else counts as daily when it says `DAILY`, and as
/// once otherwise.
pub(crate) fn parse_rrule(rule: &str) -> Repeat {
    let mut freq = None;
    let mut days = Weekdays::NONE;
    for part in rule.split(';') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => freq = Some(value.trim().to_ascii_uppercase()),
            "BYDAY" => {
                for code in value.split(',') {
                    let day = match code.trim().to_ascii_uppercase().as_str() {
                        "MO" => Weekday::Monday,
                        "TU" => Weekday::Tuesday,
                        "WE" => Weekday::Wednesday,
                        "TH" => Weekday::Thursday,
                        "FR" => Weekday::Friday,
                        "SA" => Weekday::Saturday,
                        "SU" => Weekday::Sunday,
                        _ => continue,
                    };
                    days = days.with(day);
                }
            }
            _ => {}
        }
    }
    match freq.as_deref() {
        Some("DAILY") => Repeat::Daily,
        Some("WEEKLY") if !days.is_empty() => Repeat::on(days),
        _ => Repeat::Once,
    }
}

/// What to do about a cancel, given the speaker's alarms.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AlarmCancelPlan {
    /// Cancel these; send [`AlarmCancelPlan::request`].
    Cancel(Vec<AlarmChoice>),
    /// Cancel every alarm of the speaker.
    CancelAll(usize),
    /// The speaker has no alarms.
    NoAlarms,
    /// No alarm matches.
    NotFound,
    /// Several match: ask which.
    WhichOne(Vec<AlarmChoice>),
    /// Alarms match at both AM and PM: ask "AM or PM?", then plan [`AlarmCancel::At`] with
    /// the answer.
    AskMeridiem,
}

impl AlarmCancel {
    /// Decides against the speaker's alarms ([`AlarmChoice::from_entries`]).
    pub fn plan(&self, alarms: &[AlarmChoice]) -> AlarmCancelPlan {
        if alarms.is_empty() {
            return AlarmCancelPlan::NoAlarms;
        }
        match self {
            AlarmCancel::All => AlarmCancelPlan::CancelAll(alarms.len()),
            AlarmCancel::Only => {
                if alarms.len() == 1 {
                    AlarmCancelPlan::Cancel(alarms.to_vec())
                } else {
                    AlarmCancelPlan::WhichOne(alarms.to_vec())
                }
            }
            AlarmCancel::At(said) => {
                let found: Vec<AlarmChoice> = alarms
                    .iter()
                    .filter(|a| said.matches(a.time()))
                    .cloned()
                    .collect();
                if found.is_empty() {
                    return AlarmCancelPlan::NotFound;
                }
                let both_halves = matches!(said, SaidTime::Ambiguous { .. })
                    && found.iter().any(|a| a.time().hour() < 12)
                    && found.iter().any(|a| a.time().hour() >= 12);
                if both_halves {
                    AlarmCancelPlan::AskMeridiem
                } else {
                    AlarmCancelPlan::Cancel(found)
                }
            }
            AlarmCancel::Labelled(label) => {
                let wanted = crate::normalize::normalize(label);
                let found: Vec<AlarmChoice> = alarms
                    .iter()
                    .filter(|a| {
                        a.label
                            .as_deref()
                            .is_some_and(|l| crate::normalize::normalize(l) == wanted)
                    })
                    .cloned()
                    .collect();
                if found.is_empty() {
                    AlarmCancelPlan::NotFound
                } else {
                    AlarmCancelPlan::Cancel(found)
                }
            }
        }
    }
}

impl AlarmCancelPlan {
    /// The alert service's cancellation for a plan that cancels, under the voice operation id
    /// `op_id`; `None` for a plan that cancels nothing.
    pub fn request(
        &self,
        op_id: OpId,
        endpoint: DeviceId,
    ) -> Option<echomuse_alerts::CancelAlarms> {
        let selector = match self {
            AlarmCancelPlan::Cancel(alarms) => {
                CancelSelector::Schedules(alarms.iter().map(|a| a.schedule_id).collect())
            }
            AlarmCancelPlan::CancelAll(_) => CancelSelector::All,
            _ => return None,
        };
        Some(echomuse_alerts::CancelAlarms {
            op_id,
            endpoint,
            selector,
            source: OpSource::Voice,
        })
    }
}

/// A question about the speaker's alarms.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AlarmQuery {
    /// `what alarms do I have`.
    List,
    /// `when is my next alarm`.
    Next,
}

fn find<'a>(caps: &'a [Capture], slot: &str) -> Option<&'a Capture> {
    caps.iter().find(|c| &*c.slot == slot)
}

fn number(caps: &[Capture], slot: &str) -> Option<i64> {
    find(caps, slot).and_then(|c| integer(c.value_or_text()))
}

/// The time the captures of an accepted alarm sentence say.
pub(crate) fn said_time(caps: &[Capture]) -> Option<SaidTime> {
    if let Some(named) = find(caps, "named") {
        let hour = if named.value_or_text() == "noon" {
            12
        } else {
            0
        };
        return TimeOfDay::new(hour, 0, 0).map(SaidTime::Exact);
    }
    if let Some(hour) = number(caps, "hour24") {
        let minute = number(caps, "minute").unwrap_or(0);
        return TimeOfDay::new(u8::try_from(hour).ok()?, u8::try_from(minute).ok()?, 0)
            .map(SaidTime::Exact);
    }
    let hour = u8::try_from(number(caps, "hour")?).ok()?;
    let offset = find(caps, "offset").and_then(|c| c.value_or_text().parse::<i64>().ok());
    let (hour, minute) = match offset {
        Some(-15) => (if hour == 1 { 12 } else { hour - 1 }, 45),
        Some(offset) => (hour, u8::try_from(offset).ok()?),
        None => (
            hour,
            u8::try_from(number(caps, "minute").unwrap_or(0)).ok()?,
        ),
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    Some(match find(caps, "meridiem").map(Capture::value_or_text) {
        Some("am") => SaidTime::Exact(twelve_hour(hour, minute, Meridiem::Am)),
        Some("pm") => SaidTime::Exact(twelve_hour(hour, minute, Meridiem::Pm)),
        _ => SaidTime::Ambiguous { hour, minute },
    })
}

/// The day the captures say.
pub(crate) fn said_day(caps: &[Capture]) -> Option<AlarmDay> {
    let repeat_days: Vec<&str> = caps
        .iter()
        .filter(|c| &*c.slot == "repeat_day")
        .map(Capture::value_or_text)
        .collect();
    if !repeat_days.is_empty() {
        let days = Weekdays::from_names(repeat_days)?;
        return Some(AlarmDay::Repeat(Repeat::on(days)));
    }
    if let Some(day) = find(caps, "day") {
        let weekdays = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
        ];
        return Some(match day.value_or_text() {
            "today" => AlarmDay::Today,
            "tomorrow" => AlarmDay::Tomorrow,
            "daily" => AlarmDay::Repeat(Repeat::Daily),
            "weekdays" => AlarmDay::Repeat(Repeat::on(
                weekdays.into_iter().fold(Weekdays::NONE, Weekdays::with),
            )),
            "weekends" => AlarmDay::Repeat(Repeat::on(
                Weekdays::NONE.with(Weekday::Saturday).with(Weekday::Sunday),
            )),
            _ => return None,
        });
    }
    if let Some(weekday) = find(caps, "date_weekday") {
        let set = Weekdays::from_names([weekday.value_or_text()])?;
        let day = [
            Weekday::Monday,
            Weekday::Tuesday,
            Weekday::Wednesday,
            Weekday::Thursday,
            Weekday::Friday,
            Weekday::Saturday,
            Weekday::Sunday,
        ]
        .into_iter()
        .find(|d| set.contains(*d))?;
        return Some(AlarmDay::Weekday(day));
    }
    if let Some(day) = find(caps, "date_day") {
        let text = day.value_or_text();
        let day = integer(text).or_else(|| crate::matcher::ordinal_value(text))?;
        let day = u8::try_from(day).ok().filter(|d| (1..=31).contains(d))?;
        let month = match number(caps, "date_month") {
            Some(m) => Some(u8::try_from(m).ok().filter(|m| (1..=12).contains(m))?),
            None => None,
        };
        return Some(AlarmDay::Date { month, day });
    }
    Some(AlarmDay::Next)
}

/// The label the captures say.
pub(crate) fn said_label(caps: &[Capture]) -> Option<String> {
    find(caps, "label").map(|c| c.text.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn t(h: u8, m: u8) -> TimeOfDay {
        TimeOfDay::new(h, m, 0).unwrap()
    }

    fn request(time: TimeOfDay, day: AlarmDay) -> AlarmRequest {
        AlarmRequest {
            time,
            day,
            label: None,
        }
    }

    // Wednesday 2026-10-28, 20:00.
    fn now() -> DateTime {
        date(2026, 10, 28).at(20, 0, 0, 0)
    }

    #[test]
    fn twelve_hour_times() {
        assert_eq!(twelve_hour(12, 0, Meridiem::Am), t(0, 0));
        assert_eq!(twelve_hour(12, 30, Meridiem::Pm), t(12, 30));
        assert_eq!(twelve_hour(7, 5, Meridiem::Pm), t(19, 5));
        let pending = PendingAlarm {
            hour: 7,
            minute: 0,
            day: AlarmDay::Tomorrow,
            label: None,
        };
        assert_eq!(pending.with_meridiem(Meridiem::Am).time, t(7, 0));
        assert!(SaidTime::Ambiguous { hour: 7, minute: 0 }.matches(t(19, 0)));
        assert!(!SaidTime::Exact(t(7, 0)).matches(t(19, 0)));
    }

    #[test]
    fn days_resolve_against_now() {
        let when = |time, day| request(time, day).when(now()).unwrap();
        assert_eq!(when(t(7, 0), AlarmDay::Next).date, None);
        assert_eq!(
            when(t(7, 0), AlarmDay::Tomorrow).date,
            Some(date(2026, 10, 29))
        );
        // Wednesday at 21:00 is still ahead; at 07:00 it is next week's Wednesday.
        assert_eq!(
            when(t(21, 0), AlarmDay::Weekday(Weekday::Wednesday)).date,
            Some(date(2026, 10, 28))
        );
        assert_eq!(
            when(t(7, 0), AlarmDay::Weekday(Weekday::Wednesday)).date,
            Some(date(2026, 11, 4))
        );
        assert_eq!(
            when(t(7, 0), AlarmDay::Weekday(Weekday::Friday)).date,
            Some(date(2026, 10, 30))
        );
        assert_eq!(
            when(
                t(7, 0),
                AlarmDay::Date {
                    month: Some(10),
                    day: 30
                }
            )
            .date,
            Some(date(2026, 10, 30))
        );
        assert_eq!(
            when(
                t(7, 0),
                AlarmDay::Date {
                    month: Some(1),
                    day: 2
                }
            )
            .date,
            Some(date(2027, 1, 2))
        );
        assert_eq!(
            when(
                t(7, 0),
                AlarmDay::Date {
                    month: None,
                    day: 3
                }
            )
            .date,
            Some(date(2026, 11, 3))
        );
        assert_eq!(
            when(
                t(7, 0),
                AlarmDay::Date {
                    month: None,
                    day: 31
                }
            )
            .date,
            Some(date(2026, 10, 31))
        );
        assert_eq!(
            request(
                t(7, 0),
                AlarmDay::Date {
                    month: Some(2),
                    day: 30
                }
            )
            .when(now()),
            Err(DayError::NoSuchDate)
        );
        let daily = when(t(7, 0), AlarmDay::Repeat(Repeat::Daily));
        assert_eq!((daily.date, daily.repeat), (None, Repeat::Daily));
    }

    #[test]
    fn rrules_read_back() {
        assert_eq!(parse_rrule("FREQ=DAILY"), Repeat::Daily);
        assert_eq!(
            parse_rrule("FREQ=WEEKLY;BYDAY=MO,WE"),
            Repeat::Weekly(
                Weekdays::NONE
                    .with(Weekday::Monday)
                    .with(Weekday::Wednesday)
            )
        );
        assert_eq!(parse_rrule("FREQ=MONTHLY"), Repeat::Once);
    }
}
