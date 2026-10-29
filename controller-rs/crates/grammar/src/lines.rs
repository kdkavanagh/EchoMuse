//! What the controller says itself (§4.4): its fixed lines, its questions, and its spoken
//! confirmations for alarms and timers.
//!
//! The wording is the deployed controller's, word for word: times as `7 AM` and `6:30 PM`
//! (`12 PM` at noon, `12 AM` at midnight), days relative to today when near (`today`,
//! `tomorrow`, `on Friday`) and as a date otherwise (`on November 12`), repeats as `every
//! day`, `on weekdays`, `on weekends` or `on Mondays and Wednesdays`, counts as digits. A
//! timer is said by its start length and its name: `10 minute pasta timer`. Stopping or
//! snoozing a ringing alert says nothing: the sound stopping is the answer.

use echomuse_alerts::schedule::{Repeat, TimeOfDay, Weekdays};
use echomuse_core::services::HaTimer;
use jiff::civil::{Date, DateTime, Weekday};

use crate::alarm::{AlarmChoice, AlarmRequest, local_datetime};

/// Timers or alarms read out one by one; the rest are counted.
const MAX_SPOKEN: usize = 5;

/// When an alarm rings, as said back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpokenDay {
    /// Once, on this date.
    On(Date),
    /// Repeating.
    Repeats(Repeat),
}

/// An alarm as the controller says it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpokenAlarm {
    /// Its time.
    pub time: TimeOfDay,
    /// Its day or repeat.
    pub day: SpokenDay,
    /// Its label, if it has one.
    pub label: Option<String>,
    /// A snoozed alarm's next ring.
    pub snoozed: bool,
}

impl SpokenAlarm {
    /// One of the speaker's alarms.
    pub fn from_choice(choice: &AlarmChoice) -> Self {
        Self {
            time: choice.time(),
            day: match choice.repeat {
                Repeat::Once => SpokenDay::On(choice.due.date()),
                repeat => SpokenDay::Repeats(repeat),
            },
            label: choice.label.clone(),
            snoozed: choice.snoozed,
        }
    }

    /// A new alarm, with the first time it rings as the alert service reported it
    /// (`AlarmSet::first_due_local`). When that text cannot be read, the alarm is said by
    /// its time and repeat alone.
    pub fn from_request(request: &AlarmRequest, first_due_local: &str) -> Self {
        let due: Option<DateTime> = local_datetime(first_due_local);
        Self {
            time: request.time,
            day: match (request.day, due) {
                (crate::alarm::AlarmDay::Repeat(repeat), _) if repeat != Repeat::Once => {
                    SpokenDay::Repeats(repeat)
                }
                (_, Some(due)) => SpokenDay::On(due.date()),
                (_, None) => SpokenDay::Repeats(Repeat::Once),
            },
            label: request.label.clone(),
            snoozed: false,
        }
    }

    /// `7 AM tomorrow`.
    fn when(&self, today: Date) -> String {
        let day = spoken_day(self.day, today);
        let time = spoken_time(self.time);
        if day.is_empty() {
            time
        } else {
            format!("{time} {day}")
        }
    }

    /// `work at 7 AM tomorrow`, `a snoozed alarm at 7:09 AM today`, or the time and day
    /// alone.
    fn item(&self, today: Date) -> String {
        if self.snoozed {
            return format!("a snoozed alarm at {}", self.when(today));
        }
        match &self.label {
            Some(label) => format!("{label} at {}", self.when(today)),
            None => self.when(today),
        }
    }
}

/// A timer as the controller says it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpokenTimer {
    /// Its name, if it has one.
    pub name: Option<String>,
    /// Its start units.
    pub hours: u32,
    /// Minutes.
    pub minutes: u32,
    /// Seconds.
    pub seconds: u32,
}

impl SpokenTimer {
    /// One of HA's timers.
    pub fn from_timer(timer: &HaTimer) -> Self {
        Self {
            name: timer.name.clone(),
            hours: timer.start_hours,
            minutes: timer.start_minutes,
            seconds: timer.start_seconds,
        }
    }

    /// Its length and name as an adjective: `5 minute`, `1 hour 30 minute pasta`.
    fn length(&self) -> String {
        let mut parts: Vec<String> = [
            (self.hours, "hour"),
            (self.minutes, "minute"),
            (self.seconds, "second"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n} {unit}"))
        .collect();
        if let Some(name) = self.name.as_deref().map(str::trim) {
            parts.push(name.to_owned());
        }
        parts.join(" ")
    }

    /// `5 minute timer`, `10 minute pasta timer`.
    fn label(&self) -> String {
        format!("{} timer", self.length()).trim().to_owned()
    }
}

/// Something the controller says.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Line {
    /// An empty or unreliable transcript.
    DidntCatch,
    /// The utterance ran past its limit.
    TooLong,
    /// A failure with no better line.
    SomethingWentWrong,
    /// The outcome of an action is unknown.
    NotSureItWorked,
    /// The alarm's hour lacked AM or PM.
    AmOrPm,
    /// The answer to "AM or PM?" was neither.
    PleaseSayAmOrPm,
    /// A question was given up after a second unclear answer, or declined.
    LeftIt,
    /// A repeat request with nothing said in the last two minutes.
    NothingToRepeat,
    /// The alarm is stored.
    AlarmSet(SpokenAlarm),
    /// The alarm is journaled and not yet confirmed; the service keeps applying it.
    AlarmSaving(SpokenAlarm),
    /// The date does not exist.
    NoSuchDate,
    /// The speaker's alarms, soonest first.
    Alarms(Vec<SpokenAlarm>),
    /// The next alarm, or none.
    NextAlarm(Option<SpokenAlarm>),
    /// The speaker's alarms could not be read.
    AlarmsUnavailable,
    /// This many alarms were cancelled.
    AlarmsCancelled(usize),
    /// No alarm matched the cancel.
    AlarmNotFound,
    /// Several alarms match a cancel that named no time.
    WhichAlarm,
    /// A timer cancel finished: the timers cancelled, of `total` asked for.
    TimersCancelled {
        /// Those HA confirmed cancelled.
        timers: Vec<SpokenTimer>,
        /// How many the cancel was for.
        total: usize,
    },
    /// The speaker has no timers.
    NoTimers,
    /// A timer could not be cancelled.
    TimerNotCancelled,
    /// Several timers run.
    WhichTimer(Vec<SpokenTimer>),
    /// The answer to "Which one?" picked no timer.
    WhichTimerAgain,
    /// The timer chosen is gone.
    TimerFinished(SpokenTimer),
    /// Several of the speaker's timers share this one's length and name.
    TimersIndistinct {
        /// The timer.
        timer: SpokenTimer,
        /// How many share them.
        count: usize,
    },
}

impl Line {
    /// The words, with days said relative to `today` (local date in HA's time zone).
    pub fn text(&self, today: Date) -> String {
        match self {
            Line::DidntCatch => "Sorry, I didn't catch that.".to_owned(),
            Line::TooLong => "That was too long. Try a shorter request.".to_owned(),
            Line::SomethingWentWrong => "Something went wrong.".to_owned(),
            Line::NotSureItWorked => "I'm not sure that worked.".to_owned(),
            Line::AmOrPm => "AM or PM?".to_owned(),
            Line::PleaseSayAmOrPm => "Please say AM or PM.".to_owned(),
            Line::LeftIt => "Okay, I left it.".to_owned(),
            Line::NothingToRepeat => "I haven't said anything in the last two minutes.".to_owned(),
            Line::AlarmSet(alarm) => match &alarm.label {
                Some(label) => {
                    format!("{} alarm set for {}.", capitalize(label), alarm.when(today))
                }
                None => format!("Alarm set for {}.", alarm.when(today)),
            },
            Line::AlarmSaving(alarm) => format!(
                "I'm still saving your alarm for {}. It isn't confirmed yet.",
                alarm.when(today)
            ),
            Line::NoSuchDate => "That date doesn't exist.".to_owned(),
            Line::Alarms(alarms) => {
                if alarms.is_empty() {
                    return "You don't have any alarms.".to_owned();
                }
                let mut items: Vec<String> = alarms
                    .iter()
                    .take(MAX_SPOKEN)
                    .map(|a| a.item(today))
                    .collect();
                if alarms.len() > MAX_SPOKEN {
                    items.push(format!("{} more", alarms.len() - MAX_SPOKEN));
                }
                format!(
                    "You have {} alarm{}: {}.",
                    alarms.len(),
                    if alarms.len() == 1 { "" } else { "s" },
                    list(items, "and")
                )
            }
            Line::NextAlarm(None) => "You don't have any alarms.".to_owned(),
            Line::NextAlarm(Some(alarm)) => format!("Your next alarm is {}.", alarm.item(today)),
            Line::AlarmsUnavailable => "I couldn't check your alarms.".to_owned(),
            Line::AlarmsCancelled(count) => match count {
                0 | 1 => "Alarm cancelled.".to_owned(),
                n => format!("Cancelled {n} alarms."),
            },
            Line::AlarmNotFound => "I couldn't find that alarm.".to_owned(),
            Line::WhichAlarm => "Which alarm? Say its time.".to_owned(),
            Line::TimersCancelled { timers, total } => {
                if *total == 0 {
                    return "You don't have any timers.".to_owned();
                }
                let line = match timers.as_slice() {
                    [] => return "I couldn't cancel that timer.".to_owned(),
                    [one] => format!("{} cancelled.", capitalize(&one.label())),
                    many if many.len() > MAX_SPOKEN => format!("{} timers cancelled.", many.len()),
                    many => format!(
                        "{} timers cancelled.",
                        capitalize(&list(many.iter().map(SpokenTimer::length).collect(), "and"))
                    ),
                };
                match total.saturating_sub(timers.len()) {
                    0 => line,
                    left => format!("{line} {left} could not be cancelled."),
                }
            }
            Line::NoTimers => "You don't have any timers.".to_owned(),
            Line::TimerNotCancelled => "I couldn't cancel that timer.".to_owned(),
            Line::WhichTimer(timers) if timers.len() > MAX_SPOKEN => format!(
                "You have {} timers. Which one? Say its length, like the 5 minute timer.",
                timers.len()
            ),
            Line::WhichTimer(timers) => format!(
                "Which one? {}?",
                capitalize(&list(
                    timers
                        .iter()
                        .map(|t| format!("your {}", t.label()))
                        .collect(),
                    "or"
                ))
            ),
            Line::WhichTimerAgain => {
                "Which timer? Say its length, like the 5 minute timer, or say all of them."
                    .to_owned()
            }
            Line::TimerFinished(timer) => format!("Your {} already finished.", timer.label()),
            Line::TimersIndistinct { timer, count } => format!(
                "You have {count} {}s, and I can't tell them apart. Say cancel all timers to \
                 cancel them.",
                timer.label()
            ),
        }
    }
}

/// `7 AM`, `7:05 PM`, `12 PM` (noon), `12 AM` (midnight).
pub fn spoken_time(time: TimeOfDay) -> String {
    let (hour, minute) = (time.hour(), time.minute());
    let suffix = if hour < 12 { "AM" } else { "PM" };
    let h12 = match hour % 12 {
        0 => 12,
        h => h,
    };
    if minute == 0 {
        format!("{h12} {suffix}")
    } else {
        format!("{h12}:{minute:02} {suffix}")
    }
}

const DAY_NAMES: [(Weekday, &str); 7] = [
    (Weekday::Monday, "Monday"),
    (Weekday::Tuesday, "Tuesday"),
    (Weekday::Wednesday, "Wednesday"),
    (Weekday::Thursday, "Thursday"),
    (Weekday::Friday, "Friday"),
    (Weekday::Saturday, "Saturday"),
    (Weekday::Sunday, "Sunday"),
];

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn day_name(day: Weekday) -> &'static str {
    DAY_NAMES
        .iter()
        .find(|(d, _)| *d == day)
        .map_or("", |(_, name)| name)
}

/// `today`, `tomorrow`, `on Friday` within the week, `on November 12` beyond it; `every
/// day`, `on weekdays`, `on Mondays and Wednesdays`; nothing for a one-off without a date.
pub fn spoken_day(day: SpokenDay, today: Date) -> String {
    match day {
        SpokenDay::On(date) => match (date - today).get_days() {
            0 => "today".to_owned(),
            1 => "tomorrow".to_owned(),
            2..=6 => format!("on {}", day_name(date.weekday())),
            _ => format!("on {} {}", MONTHS[(date.month() - 1) as usize], date.day()),
        },
        SpokenDay::Repeats(Repeat::Once) => String::new(),
        SpokenDay::Repeats(Repeat::Daily) => "every day".to_owned(),
        SpokenDay::Repeats(Repeat::Weekly(days)) => {
            let weekdays = DAY_NAMES[..5]
                .iter()
                .fold(Weekdays::NONE, |set, (d, _)| set.with(*d));
            let weekend = Weekdays::NONE.with(Weekday::Saturday).with(Weekday::Sunday);
            let every = weekdays.with(Weekday::Saturday).with(Weekday::Sunday);
            if days == every {
                "every day".to_owned()
            } else if days == weekdays {
                "on weekdays".to_owned()
            } else if days == weekend {
                "on weekends".to_owned()
            } else {
                let names = DAY_NAMES
                    .iter()
                    .filter(|(d, _)| days.contains(*d))
                    .map(|(_, name)| format!("{name}s"))
                    .collect();
                format!("on {}", list(names, "and"))
            }
        }
    }
}

/// `a`, `a and b`, `a, b, and c`.
fn list(items: Vec<String>, conjunction: &str) -> String {
    match items.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} {conjunction} {b}"),
        [init @ .., last] => format!("{}, {conjunction} {last}", init.join(", ")),
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    fn t(h: u8, m: u8) -> TimeOfDay {
        TimeOfDay::new(h, m, 0).unwrap()
    }

    fn today() -> Date {
        date(2026, 10, 28)
    }

    fn alarm(h: u8, m: u8, day: SpokenDay, label: Option<&str>) -> SpokenAlarm {
        SpokenAlarm {
            time: t(h, m),
            day,
            label: label.map(str::to_owned),
            snoozed: false,
        }
    }

    fn weekdays() -> Repeat {
        Repeat::on(
            [
                Weekday::Monday,
                Weekday::Tuesday,
                Weekday::Wednesday,
                Weekday::Thursday,
                Weekday::Friday,
            ]
            .into_iter()
            .fold(Weekdays::NONE, Weekdays::with),
        )
    }

    fn timer(name: Option<&str>, hours: u32, minutes: u32, seconds: u32) -> SpokenTimer {
        SpokenTimer {
            name: name.map(str::to_owned),
            hours,
            minutes,
            seconds,
        }
    }

    #[test]
    fn times_and_days_are_spoken_as_the_deployed_controller_says_them() {
        assert_eq!(spoken_time(t(7, 0)), "7 AM");
        assert_eq!(spoken_time(t(19, 5)), "7:05 PM");
        assert_eq!(spoken_time(t(0, 0)), "12 AM");
        assert_eq!(spoken_time(t(0, 30)), "12:30 AM");
        assert_eq!(spoken_time(t(12, 0)), "12 PM");
        assert_eq!(spoken_time(t(12, 15)), "12:15 PM");
        let on = |y, m, d| spoken_day(SpokenDay::On(date(y, m, d)), today());
        assert_eq!(on(2026, 10, 28), "today");
        assert_eq!(on(2026, 10, 29), "tomorrow");
        assert_eq!(on(2026, 10, 30), "on Friday");
        assert_eq!(on(2026, 11, 12), "on November 12");
        assert_eq!(on(2027, 1, 1), "on January 1");
        let repeats = |r| spoken_day(SpokenDay::Repeats(r), today());
        assert_eq!(repeats(Repeat::Daily), "every day");
        assert_eq!(repeats(weekdays()), "on weekdays");
        assert_eq!(
            repeats(Repeat::on(
                Weekdays::NONE.with(Weekday::Saturday).with(Weekday::Sunday)
            )),
            "on weekends"
        );
        assert_eq!(
            repeats(Repeat::on(
                Weekdays::NONE
                    .with(Weekday::Monday)
                    .with(Weekday::Wednesday)
                    .with(Weekday::Friday)
            )),
            "on Mondays, Wednesdays, and Fridays"
        );
    }

    #[test]
    fn fixed_lines() {
        let today = today();
        assert_eq!(Line::DidntCatch.text(today), "Sorry, I didn't catch that.");
        assert_eq!(
            Line::TooLong.text(today),
            "That was too long. Try a shorter request."
        );
        assert_eq!(
            Line::SomethingWentWrong.text(today),
            "Something went wrong."
        );
        assert_eq!(
            Line::NotSureItWorked.text(today),
            "I'm not sure that worked."
        );
        assert_eq!(Line::AmOrPm.text(today), "AM or PM?");
        assert_eq!(Line::PleaseSayAmOrPm.text(today), "Please say AM or PM.");
        assert_eq!(Line::LeftIt.text(today), "Okay, I left it.");
        assert_eq!(
            Line::NothingToRepeat.text(today),
            "I haven't said anything in the last two minutes."
        );
    }

    #[test]
    fn alarm_lines_singular_plural_and_empty() {
        let today = today();
        let tomorrow = SpokenDay::On(date(2026, 10, 29));
        let a = alarm(7, 0, tomorrow, None);
        let b = alarm(6, 30, SpokenDay::Repeats(weekdays()), Some("work"));
        let c = alarm(9, 0, SpokenDay::On(date(2026, 10, 31)), None);
        let snoozed = SpokenAlarm {
            snoozed: true,
            ..alarm(7, 9, SpokenDay::On(today), None)
        };
        assert_eq!(
            Line::AlarmSet(a.clone()).text(today),
            "Alarm set for 7 AM tomorrow."
        );
        assert_eq!(
            Line::AlarmSet(alarm(7, 0, SpokenDay::Repeats(Repeat::Once), None)).text(today),
            "Alarm set for 7 AM."
        );
        assert_eq!(
            Line::AlarmSet(b.clone()).text(today),
            "Work alarm set for 6:30 AM on weekdays."
        );
        assert_eq!(
            Line::AlarmSaving(a.clone()).text(today),
            "I'm still saving your alarm for 7 AM tomorrow. It isn't confirmed yet."
        );
        assert_eq!(
            Line::Alarms(vec![]).text(today),
            "You don't have any alarms."
        );
        assert_eq!(
            Line::Alarms(vec![a.clone()]).text(today),
            "You have 1 alarm: 7 AM tomorrow."
        );
        assert_eq!(
            Line::Alarms(vec![a.clone(), b.clone(), c.clone(), snoozed.clone()]).text(today),
            "You have 4 alarms: 7 AM tomorrow, work at 6:30 AM on weekdays, 9 AM on Saturday, \
             and a snoozed alarm at 7:09 AM today."
        );
        let seven = vec![a.clone(); 7];
        assert_eq!(
            Line::Alarms(seven).text(today),
            "You have 7 alarms: 7 AM tomorrow, 7 AM tomorrow, 7 AM tomorrow, 7 AM tomorrow, \
             7 AM tomorrow, and 2 more."
        );
        assert_eq!(
            Line::NextAlarm(None).text(today),
            "You don't have any alarms."
        );
        assert_eq!(
            Line::NextAlarm(Some(a)).text(today),
            "Your next alarm is 7 AM tomorrow."
        );
        assert_eq!(
            Line::NextAlarm(Some(b)).text(today),
            "Your next alarm is work at 6:30 AM on weekdays."
        );
        assert_eq!(
            Line::AlarmsUnavailable.text(today),
            "I couldn't check your alarms."
        );
        assert_eq!(Line::AlarmsCancelled(1).text(today), "Alarm cancelled.");
        assert_eq!(Line::AlarmsCancelled(3).text(today), "Cancelled 3 alarms.");
        assert_eq!(
            Line::AlarmNotFound.text(today),
            "I couldn't find that alarm."
        );
        assert_eq!(Line::WhichAlarm.text(today), "Which alarm? Say its time.");
    }

    #[test]
    fn timer_lines_singular_plural_partial_and_empty() {
        let today = today();
        let pasta = timer(Some("pasta"), 0, 10, 0);
        let plain = timer(None, 1, 30, 0);
        let five = timer(None, 0, 5, 0);
        let cancelled = |timers: Vec<SpokenTimer>, total| Line::TimersCancelled { timers, total };
        assert_eq!(
            cancelled(vec![pasta.clone()], 1).text(today),
            "10 minute pasta timer cancelled."
        );
        assert_eq!(
            cancelled(vec![timer(None, 0, 2, 0)], 1).text(today),
            "2 minute timer cancelled."
        );
        assert_eq!(
            cancelled(vec![five.clone(), pasta.clone()], 2).text(today),
            "5 minute and 10 minute pasta timers cancelled."
        );
        assert_eq!(
            cancelled(vec![five.clone(), pasta.clone(), plain.clone()], 3).text(today),
            "5 minute, 10 minute pasta, and 1 hour 30 minute timers cancelled."
        );
        assert_eq!(
            cancelled(vec![five.clone(); 6], 6).text(today),
            "6 timers cancelled."
        );
        assert_eq!(
            cancelled(vec![five.clone()], 3).text(today),
            "5 minute timer cancelled. 2 could not be cancelled."
        );
        assert_eq!(
            cancelled(vec![], 2).text(today),
            "I couldn't cancel that timer."
        );
        assert_eq!(
            cancelled(vec![], 0).text(today),
            "You don't have any timers."
        );
        assert_eq!(Line::NoTimers.text(today), "You don't have any timers.");
        assert_eq!(
            Line::TimerNotCancelled.text(today),
            "I couldn't cancel that timer."
        );
        assert_eq!(
            Line::WhichTimer(vec![five.clone(), pasta.clone()]).text(today),
            "Which one? Your 5 minute timer or your 10 minute pasta timer?"
        );
        assert_eq!(
            Line::WhichTimer(vec![five.clone(), pasta.clone(), plain.clone()]).text(today),
            "Which one? Your 5 minute timer, your 10 minute pasta timer, or your 1 hour 30 \
             minute timer?"
        );
        assert_eq!(
            Line::WhichTimer(vec![five.clone(); 6]).text(today),
            "You have 6 timers. Which one? Say its length, like the 5 minute timer."
        );
        assert_eq!(
            Line::WhichTimerAgain.text(today),
            "Which timer? Say its length, like the 5 minute timer, or say all of them."
        );
        assert_eq!(
            Line::TimerFinished(plain).text(today),
            "Your 1 hour 30 minute timer already finished."
        );
        assert_eq!(
            Line::TimersIndistinct {
                timer: five,
                count: 2
            }
            .text(today),
            "You have 2 5 minute timers, and I can't tell them apart. Say cancel all timers \
             to cancel them."
        );
    }
}
