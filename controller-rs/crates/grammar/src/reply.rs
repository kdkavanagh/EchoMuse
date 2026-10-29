//! Answers to the controller's own questions (§4.5): "AM or PM?" and "Which one?".

use echomuse_core::services::HaTimer;

use crate::{
    alarm::{AlarmChoice, SaidTime},
    normalize::normalize,
    timer::{ChosenTimers, timer_seconds},
};

/// An answer to "Which one?".
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WhichReply {
    /// `the first one`: 1-based, in the order the question listed them.
    Position(usize),
    /// `the last one`.
    Last,
    /// `all of them`, `both`.
    All,
    /// `never mind`, `neither`.
    Decline,
    /// `the pasta one`: a name, as said (normalized words).
    Name(String),
    /// `the ten minute one`.
    Length {
        /// The length in seconds.
        seconds: u32,
    },
    /// `the ten minute pasta timer`.
    LengthAndName {
        /// The length in seconds.
        seconds: u32,
        /// The name, as said.
        name: String,
    },
    /// `the 7 am one`.
    Time(SaidTime),
}

/// What an answer to "Which one?" picked.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Choice<T> {
    /// These.
    Chosen(Vec<T>),
    /// The user declined: say "Okay, I left it."
    Declined,
    /// The answer matches none of the choices.
    NoMatch,
}

/// What an answer to "Which one?" picked among timers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimerPick {
    /// One timer, or all of them.
    Chosen(ChosenTimers),
    /// The user declined: say "Okay, I left it."
    Declined,
    /// The answer picks no timer, or picks several by a length or name they share.
    NoMatch,
}

impl WhichReply {
    fn by_position<T: Clone>(&self, choices: &[T]) -> Option<Choice<T>> {
        match self {
            WhichReply::Position(n) => Some(
                n.checked_sub(1)
                    .and_then(|i| choices.get(i))
                    .map_or(Choice::NoMatch, |c| Choice::Chosen(vec![c.clone()])),
            ),
            WhichReply::Last => Some(
                choices
                    .last()
                    .map_or(Choice::NoMatch, |c| Choice::Chosen(vec![c.clone()])),
            ),
            WhichReply::All => Some(if choices.is_empty() {
                Choice::NoMatch
            } else {
                Choice::Chosen(choices.to_vec())
            }),
            WhichReply::Decline => Some(Choice::Declined),
            _ => None,
        }
    }

    /// The timer the answer picks among `choices`. A length or a name must pick exactly
    /// one: two timers it fits are no answer.
    pub fn choose_timer(&self, choices: &[HaTimer]) -> TimerPick {
        let named = |t: &HaTimer, name: &str| {
            let wanted = normalize(name);
            t.name.as_deref().is_some_and(|n| normalize(n) == wanted)
        };
        let found: Vec<&HaTimer> = match self {
            WhichReply::Decline => return TimerPick::Declined,
            WhichReply::All if !choices.is_empty() => {
                return TimerPick::Chosen(ChosenTimers::All(choices.to_vec()));
            }
            WhichReply::Position(n) => n
                .checked_sub(1)
                .and_then(|i| choices.get(i))
                .into_iter()
                .collect(),
            WhichReply::Last => choices.last().into_iter().collect(),
            WhichReply::Name(name) => choices.iter().filter(|t| named(t, name)).collect(),
            WhichReply::Length { seconds } => choices
                .iter()
                .filter(|t| timer_seconds(t) == *seconds)
                .collect(),
            WhichReply::LengthAndName { seconds, name } => choices
                .iter()
                .filter(|t| timer_seconds(t) == *seconds && named(t, name))
                .collect(),
            WhichReply::All | WhichReply::Time(_) => Vec::new(),
        };
        match found.as_slice() {
            [one] => TimerPick::Chosen(ChosenTimers::One((*one).clone())),
            _ => TimerPick::NoMatch,
        }
    }

    /// The alarms the answer picks among `choices`.
    pub fn choose_alarms(&self, choices: &[AlarmChoice]) -> Choice<AlarmChoice> {
        if let Some(choice) = self.by_position(choices) {
            return choice;
        }
        let found: Vec<AlarmChoice> = match self {
            WhichReply::Name(name) => {
                let wanted = normalize(name);
                choices
                    .iter()
                    .filter(|a| a.label.as_deref().is_some_and(|l| normalize(l) == wanted))
                    .cloned()
                    .collect()
            }
            WhichReply::Time(said) => choices
                .iter()
                .filter(|a| said.matches(a.time()))
                .cloned()
                .collect(),
            _ => Vec::new(),
        };
        if found.is_empty() {
            Choice::NoMatch
        } else {
            Choice::Chosen(found)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timer(id: &str, name: Option<&str>, minutes: u32) -> HaTimer {
        HaTimer {
            id: id.to_owned(),
            name: name.map(str::to_owned),
            ha_device: None,
            start_hours: 0,
            start_minutes: minutes,
            start_seconds: 0,
            is_active: true,
        }
    }

    #[test]
    fn choosing_timers() {
        let timers = [
            timer("a", Some("Pasta"), 10),
            timer("b", None, 3),
            timer("c", None, 10),
        ];
        let id = |p: TimerPick| match p {
            TimerPick::Chosen(ChosenTimers::One(t)) => t.id,
            TimerPick::Chosen(ChosenTimers::All(v)) => {
                v.into_iter().map(|t| t.id).collect::<Vec<_>>().join(",")
            }
            TimerPick::Declined => "declined".to_owned(),
            TimerPick::NoMatch => "none".to_owned(),
        };
        assert_eq!(
            id(WhichReply::Name("pasta".into()).choose_timer(&timers)),
            "a"
        );
        assert_eq!(
            id(WhichReply::Length { seconds: 180 }.choose_timer(&timers)),
            "b"
        );
        // Two ten-minute timers: the length alone picks neither.
        assert_eq!(
            id(WhichReply::Length { seconds: 600 }.choose_timer(&timers)),
            "none"
        );
        assert_eq!(
            id(WhichReply::LengthAndName {
                seconds: 600,
                name: "pasta".into()
            }
            .choose_timer(&timers)),
            "a"
        );
        assert_eq!(id(WhichReply::Position(2).choose_timer(&timers)), "b");
        assert_eq!(id(WhichReply::Position(4).choose_timer(&timers)), "none");
        assert_eq!(id(WhichReply::Last.choose_timer(&timers)), "c");
        assert_eq!(id(WhichReply::All.choose_timer(&timers)), "a,b,c");
        assert_eq!(id(WhichReply::Decline.choose_timer(&timers)), "declined");
        assert_eq!(
            id(WhichReply::Name("tea".into()).choose_timer(&timers)),
            "none"
        );
    }
}
