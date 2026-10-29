//! The controller's own sentence templates, in hassil's template language: the commands it
//! handles without Home Assistant (§4.4) and the answers to its own questions (§4.5).
//!
//! They are compiled with Home Assistant's templates into one automaton, so completeness is
//! judged the same way for both, and an accepted reading's slots are the parse.

use std::collections::BTreeMap;

use crate::program::{Group, ListDef, Origin, TemplateSet, ValueDef};

/// What a local sentence means.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum LocalIntent {
    /// `stop`, `cancel`, `snooze` or `pause` alone.
    Bare,
    /// `stop the [pasta] timer`, `turn off the alarm`.
    StopAlert,
    /// `snooze the alarm`.
    SnoozeAlert,
    /// `say that again`.
    Repeat,
    /// `set an alarm for 7 am`.
    AlarmSet,
    /// `cancel my 7 am alarm`.
    AlarmCancel,
    /// `cancel all my alarms`.
    AlarmCancelAll,
    /// `what alarms do I have`.
    AlarmList,
    /// `when is my next alarm`.
    AlarmNext,
    /// `cancel the timer`.
    TimerCancelAny,
    /// `cancel all timers`.
    TimerCancelAll,
    /// `cancel the pasta timer`.
    TimerCancelNamed,
    /// `cancel the 10 minute timer`.
    TimerCancelLength,
    /// `pm`, `in the morning`: an answer to "AM or PM?".
    MeridiemReply,
    /// An answer to "Which one?" naming one.
    WhichReply,
    /// `all of them`.
    WhichAll,
    /// `never mind`.
    WhichDecline,
}

/// How an accepted local sentence counts for completeness.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Flag {
    /// Complete as said.
    None,
    /// Complete as said; a context command ("stop") rather than a request.
    Final,
    /// A whole request that also begins other common requests ("what was that song"):
    /// extendable.
    Extendable,
    /// Not complete: the hour lacks AM or PM.
    NeedsMeridiem,
}

const RULES: &[(&str, &str)] = &[
    ("the_my", "(the|my|this|that)"),
    ("an", "(a|an|my|the|another|new)"),
    (
        "set_verb",
        "(set|create|make|add|schedule|set up|put in|put)",
    ),
    (
        "cancel_verb",
        "(cancel|delete|remove|turn off|switch off|disable|clear|get rid of)",
    ),
    (
        "timer_verb",
        "(cancel|stop|turn off|switch off|delete|remove|end|clear|kill)",
    ),
    (
        "time12",
        "({hour12:hour} [o'clock]|{hour12:hour} {minute:minute}|{hour12:hour} oh {minute_digit:minute}|{minute_phrase:offset} {hour12:hour})",
    ),
    (
        "time_ampm",
        "(<time12> {meridiem:meridiem}|{named_time:named}|{hour_late:hour24} [{minute:minute}]|{hour_zero:hour24} {minute:minute}|<time12> {tomorrow:day} {part_of_day:meridiem})",
    ),
    (
        "days",
        "([for] {day_word:day}|<repeat_days>|[for] <one_day>)",
    ),
    (
        "repeat_days",
        "(every {weekday:repeat_day} [[and] {weekday:repeat_day} [[and] {weekday:repeat_day} [[and] {weekday:repeat_day} [[and] {weekday:repeat_day} [[and] {weekday:repeat_day} [[and] {weekday:repeat_day}]]]]]]|[on] [every] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day} [[and] {weekday_plural:repeat_day}]]]]]])",
    ),
    (
        "one_day",
        "([on] [{relative:relative}] {weekday:date_weekday}|[on] [the] {day_ordinal:date_day} [of {month:date_month}]|[on] {month:date_month} [the] ({day_ordinal:date_day}|{day_number:date_day}))",
    ),
    (
        "label",
        "(called|named|labeled|labelled) {alarm_label:label}",
    ),
    (
        "duration",
        "({n_hours:hours} hour[s] [[and] {n_minutes:minutes} minute[s]] [[and] {n_seconds:seconds} second[s]]|{n_minutes:minutes} minute[s] [[and] {n_seconds:seconds} second[s]]|{n_seconds:seconds} second[s]|{fixed_duration:seconds_total})",
    ),
];

/// Value lists: `(list, [(template, out)])`.
const VALUE_LISTS: &[(&str, &[(&str, &str)])] = &[
    (
        "bare_command",
        &[
            ("stop", "stop"),
            ("cancel", "cancel"),
            ("snooze", "snooze"),
            ("pause", "pause"),
        ],
    ),
    ("alert_kind", &[("timer", "timer"), ("alarm", "alarm")]),
    (
        "meridiem",
        &[
            ("(am|a m|in the morning|this morning|in the am)", "am"),
            (
                "(pm|p m|in the afternoon|this afternoon|in the evening|this evening|at night|tonight|in the pm)",
                "pm",
            ),
        ],
    ),
    (
        "part_of_day",
        &[("morning", "am"), ("(afternoon|evening|night)", "pm")],
    ),
    ("tomorrow", &[("tomorrow", "tomorrow")]),
    (
        "named_time",
        &[
            ("(noon|midday|12 noon)", "noon"),
            ("(midnight|12 midnight)", "midnight"),
        ],
    ),
    (
        "minute_phrase",
        &[
            ("half past", "30"),
            ("quarter past", "15"),
            ("quarter (to|till|of|before)", "-15"),
        ],
    ),
    (
        "day_word",
        &[
            ("today", "today"),
            ("tomorrow", "tomorrow"),
            (
                "(every day|everyday|daily|each day|every single day)",
                "daily",
            ),
            (
                "([on] weekdays|every weekday|during the week|monday (through|to|thru|till|until) friday)",
                "weekdays",
            ),
            ("([on] weekends|every weekend|on the weekends)", "weekends"),
        ],
    ),
    (
        "day_adjective",
        &[
            ("daily", "daily"),
            ("weekday", "weekdays"),
            ("weekend", "weekends"),
        ],
    ),
    (
        "weekday",
        &[
            ("monday", "mon"),
            ("tuesday", "tue"),
            ("wednesday", "wed"),
            ("thursday", "thu"),
            ("friday", "fri"),
            ("saturday", "sat"),
            ("sunday", "sun"),
        ],
    ),
    (
        "weekday_plural",
        &[
            ("mondays", "mon"),
            ("tuesdays", "tue"),
            ("wednesdays", "wed"),
            ("thursdays", "thu"),
            ("fridays", "fri"),
            ("saturdays", "sat"),
            ("sundays", "sun"),
        ],
    ),
    (
        "month",
        &[
            ("(january|jan)", "1"),
            ("(february|feb)", "2"),
            ("(march|mar)", "3"),
            ("(april|apr)", "4"),
            ("may", "5"),
            ("(june|jun)", "6"),
            ("(july|jul)", "7"),
            ("(august|aug)", "8"),
            ("(september|sept|sep)", "9"),
            ("(october|oct)", "10"),
            ("(november|nov)", "11"),
            ("(december|dec)", "12"),
        ],
    ),
    (
        "relative",
        &[("(this|this coming|coming)", "this"), ("next", "next")],
    ),
    (
        "fixed_duration",
        &[
            ("(an|a|one) hour", "3600"),
            ("(a|one) minute", "60"),
            ("half an hour", "1800"),
            ("(an|one) hour and a half", "5400"),
            ("a minute and a half", "90"),
        ],
    ),
    (
        "position",
        &[
            ("1st", "1"),
            ("(second|2nd)", "2"),
            ("3rd", "3"),
            ("4th", "4"),
            ("5th", "5"),
            ("last", "last"),
        ],
    ),
];

/// Number lists: `(list, from, to)`.
const RANGES: &[(&str, i64, i64)] = &[
    ("hour12", 1, 12),
    ("minute", 0, 59),
    ("minute_digit", 1, 9),
    ("hour_late", 13, 23),
    ("hour_zero", 0, 0),
    ("day_number", 1, 31),
    ("n_hours", 1, 99),
    ("n_minutes", 1, 999),
    ("n_seconds", 1, 999),
];

const WILDCARDS: &[&str] = &["alarm_label", "ring_name", "timer_name"];

/// Sentences that may use `<time_ampm>`; each is also added with `<time12>` in its place,
/// flagged as missing AM or PM.
const ALARM_SET: &[&str] = &[
    "<set_verb> [<an>] alarm (for|at) <time_ampm> [<days>] [<label>]",
    "<set_verb> [<an>] alarm (called|named) {alarm_label:label} (for|at) <time_ampm> [<days>]",
    "<set_verb> [<an>] alarm [for] <days> (at|for) <time_ampm> [<label>]",
    "<set_verb> [<an>] {day_adjective:day} alarm (for|at) <time_ampm> [<label>]",
    "wake me [up] (at|for) <time_ampm> [<days>]",
    "wake me [up] <days> (at|for) <time_ampm>",
    "alarm (for|at) <time_ampm> [<days>]",
];

/// Sentences whose time already says when in the day.
const ALARM_SET_PART_OF_DAY: &[&str] = &[
    "<set_verb> [<an>] alarm [for] {tomorrow:day} {part_of_day:meridiem} (at|for) <time12> [<label>]",
    "wake me [up] {tomorrow:day} {part_of_day:meridiem} (at|for) <time12>",
];

const ALARM_CANCEL: &[&str] = &[
    "<cancel_verb> [<the_my>] alarm",
    "<cancel_verb> [<the_my>] (<time_ampm>|<time12>) alarm",
    "<cancel_verb> [<the_my>] alarm (for|at|set for) (<time_ampm>|<time12>)",
    "<cancel_verb> [<the_my>] {alarm_label:label} alarm",
    "<cancel_verb> [<the_my>] alarm (called|named) {alarm_label:label}",
];

const ALARM_CANCEL_ALL: &[&str] = &[
    "<cancel_verb> (all [of] [<the_my>] alarms|[<the_my>] alarms|every alarm|both [of] [<the_my>] alarms|all alarms)",
];

const ALARM_LIST: &[&str] = &[
    "(what|which) alarms (do i have|have i got|have i set|are set|are there|are on|are scheduled|are set up) [<days>]",
    "what are my alarms",
    "(do i have|have i got|are there|is there|have i set|did i set) (any|an) alarm[s] [set|on|scheduled] [<days>]",
    "is (my|an|the|any) alarm set",
    "(list|show|tell me) [me] [all] [of] [<the_my>] alarms",
    "[my] alarm[s] status",
    "check [<the_my>] alarm[s]",
];

const ALARM_NEXT: &[&str] = &[
    "when is (my|the) [next] alarm [set for]",
    "what time is (my|the) [next] alarm [set] [for]",
    "what is (my|the) next alarm",
    "what is my alarm set for",
    "(when|what time) (does|will) (my|the) [next] alarm go off",
    "[my] next alarm",
];

const REPEAT: &[&str] = &[
    "[sorry] [will you] (say (that|it) (again|1 more time)|repeat (that|it|yourself|what you [just] said) [again|1 more time]|what did you just say|come again)",
    "[sorry] [will you] (repeat|what did you say|what was that|pardon [me]) (again|1 more time)",
];

/// Repeat requests that also begin other requests ("what was that song", "repeat after
/// me").
const REPEAT_OPEN: &[&str] =
    &["[sorry] [will you] (repeat|what did you say|what was that|pardon [me])"];

const TIMER_CANCEL_ANY: &[&str] = &["<timer_verb> [<the_my>] timer"];
const TIMER_CANCEL_ALL: &[&str] = &[
    "<timer_verb> (all [of] [the|my] timers|both [of] [the|my] timers|every timer|each timer|[the|my] timers|all timers)",
];
const TIMER_CANCEL_NAMED: &[&str] = &[
    "<timer_verb> [the|my] {timer_name:name} timer",
    "<timer_verb> [the|my] timer (called|named|for) {timer_name:name}",
];
const TIMER_CANCEL_LENGTH: &[&str] = &[
    "<timer_verb> [the|my|that] <duration> timer",
    "<timer_verb> [the|my] timer for <duration>",
];

const STOP_ALERT: &[&str] =
    &["(stop|cancel|turn off|dismiss|silence) [<the_my>] [{ring_name:name}] {alert_kind:kind}"];
const SNOOZE_ALERT: &[&str] = &["snooze (<the_my> alarm|it)"];

const MERIDIEM_REPLY: &[&str] = &["{meridiem:meridiem}", "[in the] {part_of_day:meridiem}"];
const WHICH_REPLY: &[&str] = &[
    "[the|my] {timer_name:name} [1|timer|alarm]",
    "[the|my] <duration> [1|timer]",
    "[the|my] <duration> {timer_name:name} (1|timer)",
    "[the] (<time_ampm>|<time12>) [1|alarm]",
    "[the] {position:position} [1|timer|alarm]",
    // A whole cancel answers too ("cancel the 5 minute timer").
    "<timer_verb> [the|my|this|that] {timer_name:name} timer",
    "<timer_verb> [the|my|this|that] <duration> [{timer_name:name}] timer",
    "<timer_verb> [the|my] timer (called|named|for) {timer_name:name}",
    "<timer_verb> [the|my] timer for <duration>",
];
const WHICH_ALL: &[&str] = &[
    "(all|both|every 1|every timer|all of them|both of them|all [the|my] timers|all [the|my] alarms|both timers|both alarms)",
    "<timer_verb> (all|both|all [of] [the|my] timers|both [of] [the|my] timers|every timer|each timer|[the|my] timers|all timers)",
];
const WHICH_DECLINE: &[&str] = &[
    "(never mind|nevermind|neither|none|none of them|neither of them|forget it|no|nothing|leave it|leave them)",
];

/// The controller's template set.
pub(crate) fn templates() -> TemplateSet {
    let rules: BTreeMap<String, String> = RULES
        .iter()
        .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
        .collect();
    let mut lists = BTreeMap::new();
    for (name, values) in VALUE_LISTS {
        lists.insert(
            (*name).to_owned(),
            ListDef::Values(
                values
                    .iter()
                    .map(|(template, out)| ValueDef {
                        template: (*template).to_owned(),
                        out: Some((*out).to_owned()),
                    })
                    .collect(),
            ),
        );
    }
    for (name, from, to) in RANGES {
        lists.insert(
            (*name).to_owned(),
            ListDef::Range {
                from: *from,
                to: *to,
                step: 1,
                halves: false,
            },
        );
    }
    lists.insert(
        "day_ordinal".to_owned(),
        ListDef::Ordinal { from: 1, to: 31 },
    );
    for name in WILDCARDS {
        lists.insert((*name).to_owned(), ListDef::Wildcard);
    }

    let mut sentences = Vec::new();
    let mut add = |templates: &[&str], intent: LocalIntent, flag: Flag, group: Group| {
        for template in templates {
            sentences.push((
                (*template).to_owned(),
                Origin::Local { intent, flag },
                group,
            ));
        }
    };
    add(
        &["{bare_command:command}"],
        LocalIntent::Bare,
        Flag::Final,
        Group::Commands,
    );
    add(
        STOP_ALERT,
        LocalIntent::StopAlert,
        Flag::None,
        Group::Commands,
    );
    add(
        SNOOZE_ALERT,
        LocalIntent::SnoozeAlert,
        Flag::None,
        Group::Commands,
    );
    add(REPEAT, LocalIntent::Repeat, Flag::None, Group::Commands);
    add(
        REPEAT_OPEN,
        LocalIntent::Repeat,
        Flag::Extendable,
        Group::Commands,
    );
    add(
        ALARM_SET,
        LocalIntent::AlarmSet,
        Flag::None,
        Group::Commands,
    );
    let ambiguous: Vec<String> = ALARM_SET
        .iter()
        .map(|t| t.replace("<time_ampm>", "<time12>"))
        .collect();
    let ambiguous: Vec<&str> = ambiguous.iter().map(String::as_str).collect();
    add(
        &ambiguous,
        LocalIntent::AlarmSet,
        Flag::NeedsMeridiem,
        Group::Commands,
    );
    add(
        ALARM_SET_PART_OF_DAY,
        LocalIntent::AlarmSet,
        Flag::None,
        Group::Commands,
    );
    add(
        ALARM_CANCEL,
        LocalIntent::AlarmCancel,
        Flag::None,
        Group::Commands,
    );
    add(
        ALARM_CANCEL_ALL,
        LocalIntent::AlarmCancelAll,
        Flag::None,
        Group::Commands,
    );
    add(
        ALARM_LIST,
        LocalIntent::AlarmList,
        Flag::None,
        Group::Commands,
    );
    add(
        ALARM_NEXT,
        LocalIntent::AlarmNext,
        Flag::None,
        Group::Commands,
    );
    add(
        TIMER_CANCEL_ANY,
        LocalIntent::TimerCancelAny,
        Flag::None,
        Group::Commands,
    );
    add(
        TIMER_CANCEL_ALL,
        LocalIntent::TimerCancelAll,
        Flag::None,
        Group::Commands,
    );
    add(
        TIMER_CANCEL_NAMED,
        LocalIntent::TimerCancelNamed,
        Flag::None,
        Group::Commands,
    );
    add(
        TIMER_CANCEL_LENGTH,
        LocalIntent::TimerCancelLength,
        Flag::None,
        Group::Commands,
    );
    add(
        MERIDIEM_REPLY,
        LocalIntent::MeridiemReply,
        Flag::None,
        Group::Replies,
    );
    add(
        WHICH_REPLY,
        LocalIntent::WhichReply,
        Flag::None,
        Group::Replies,
    );
    add(WHICH_ALL, LocalIntent::WhichAll, Flag::None, Group::Replies);
    add(
        WHICH_DECLINE,
        LocalIntent::WhichDecline,
        Flag::None,
        Group::Replies,
    );

    TemplateSet {
        rules,
        lists,
        sentences,
        skip_words: Vec::new(),
    }
}
