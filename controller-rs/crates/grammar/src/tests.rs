//! Behaviour tests: synthetic sentences written for stated cases, and sentences generated
//! from the vendored templates.

use std::sync::LazyLock;

use echomuse_alerts::{
    OpId, OpSource,
    schedule::{Repeat, TimeOfDay, Weekdays},
    wire::RingKind,
};
use echomuse_core::types::DeviceId;
use jiff::civil::{Weekday, date};

use crate::{
    matcher,
    program::{Group, ListDef, Origin, TemplateSet},
    template::{self, Expr},
    *,
};

static GRAMMAR: LazyLock<Grammar> =
    LazyLock::new(|| Grammar::new().expect("the vendored templates compile"));
static WITH_VOCABULARY: LazyLock<Grammar> =
    LazyLock::new(|| GRAMMAR.with_vocabulary(&vocabulary()));

fn grammar() -> &'static Grammar {
    &GRAMMAR
}

fn vocabulary() -> Vocabulary {
    Vocabulary {
        names: Some(vec![
            "kitchen light".to_owned(),
            "kitchen".to_owned(),
            "kitchen lights".to_owned(),
            "tv".to_owned(),
            "front door".to_owned(),
        ]),
        areas: Some(vec!["kitchen".to_owned(), "living room".to_owned()]),
        floors: Some(vec!["upstairs".to_owned()]),
    }
}

fn with_vocabulary() -> &'static Grammar {
    &WITH_VOCABULARY
}

fn t(h: u8, m: u8) -> TimeOfDay {
    TimeOfDay::new(h, m, 0).unwrap()
}

use Completeness::{Complete, Extendable, NeedsMore, Unknown};

#[test]
fn completeness_of_stated_cases() {
    let g = grammar();
    let cases: &[(&str, Completeness)] = &[
        // Bare local commands commit fast.
        ("stop", Complete),
        ("Stop.", Complete),
        ("please stop", Complete),
        ("cancel", Complete),
        ("snooze", Complete),
        ("pause", Complete),
        // Starts of requests that wait for a value.
        ("set a timer for", NeedsMore),
        ("timer for five", NeedsMore),
        ("set a 10 minute timer named", NeedsMore),
        ("cancel the", NeedsMore),
        ("stop the", NeedsMore),
        ("set an alarm for", NeedsMore),
        ("set an alarm", NeedsMore),
        ("wake me up at", NeedsMore),
        // Starts that wait for a word, not a value, are left to Home Assistant.
        ("set a timer", Unknown),
        ("how much time", Unknown),
        ("can you", Unknown),
        // An alarm hour without AM or PM needs more.
        ("set an alarm for seven", NeedsMore),
        ("set an alarm for 7 tomorrow", NeedsMore),
        // Whole requests, whatever optional words or longer templates could follow.
        ("set a timer for five minutes", Complete),
        ("set a ten second timer", Complete),
        ("set a pasta timer for 10 minutes", Complete),
        ("how much time is left", Complete),
        ("cancel the timer", Complete),
        ("set an alarm for 7 am", Complete),
        ("cancel my 7 am alarm", Complete),
        ("what alarms do I have", Complete),
        ("say that again", Complete),
        ("what did you just say", Complete),
        ("stop the timer", Complete),
        ("turn off the alarm", Complete),
        // A whole request that ends in a word such as "on" still waits.
        ("what alarms are on", NeedsMore),
        // A timer's name neither starts nor ends with such a word.
        ("set a timer for 5 minutes for", Unknown),
        // Any words can be a timer's command: "<command> in <minutes> minutes".
        ("what is the weather in", NeedsMore),
        // Repeat forms that also begin other requests.
        ("repeat", Extendable),
        ("what was that", Extendable),
        ("what was that again", Complete),
        // Everything else is Home Assistant's matcher's to judge.
        ("what time is it", Unknown),
        ("what is the time", Unknown),
        ("never mind", Unknown),
        ("play cherub rock", Unknown),
        ("turn on the", Unknown),
        ("why is the sky blue", Unknown),
        ("who wrote moby dick", Unknown),
        ("", Unknown),
    ];
    let mut wrong = Vec::new();
    for (text, expected) in cases {
        let got = g.classify(text);
        if got != *expected {
            wrong.push(format!("{text:?}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn known_targets_turn_on_and_off() {
    let open = grammar();
    let closed = with_vocabulary();
    // Without a vocabulary no target is known.
    assert_eq!(open.classify("turn off kitchen"), Unknown);
    // "kitchen" is a name and the start of "kitchen lights".
    assert_eq!(closed.classify("turn off kitchen"), Extendable);
    assert_eq!(closed.classify("kitchen on"), Extendable);
    // Whole targets nothing longer starts with.
    assert_eq!(closed.classify("turn on the kitchen light"), Complete);
    assert_eq!(closed.classify("switch off the kitchen lights"), Complete);
    assert_eq!(closed.classify("tv on"), Complete);
    assert_eq!(closed.classify("turn off the living room"), Complete);
    // The rest of a target, or its on or off, is still to come.
    assert_eq!(closed.classify("turn on the"), NeedsMore);
    assert_eq!(closed.classify("switch on"), NeedsMore);
    assert_eq!(closed.classify("turn on the front"), NeedsMore);
    assert_eq!(closed.classify("front door"), NeedsMore);
    // An optional ending is Home Assistant's to judge.
    assert_eq!(
        closed.classify("turn on the kitchen light in the living room"),
        Unknown
    );
    assert_eq!(closed.classify("turn on the bathroom fan"), Unknown);
}

#[test]
fn route_order_and_forms() {
    let g = grammar();
    let route = |text: &str| g.route(text);
    // 1. Repeat.
    for text in [
        "say that again",
        "could you repeat that",
        "what did you just say",
        "repeat",
        "what was that",
        "pardon me",
        "sorry, say it one more time",
    ] {
        assert_eq!(route(text), Route::Repeat, "{text}");
    }
    assert_eq!(route("what was that song"), Route::SendToHa);
    assert_eq!(route("repeat after me"), Route::SendToHa);
    // 2. Alarms.
    let set = |text: &str| match route(text) {
        Route::SetAlarm(request) => request,
        other => panic!("{text}: {other:?}"),
    };
    assert_eq!(set("set an alarm for 7 am").time, t(7, 0));
    assert_eq!(set("set an alarm for 7 am").day, AlarmDay::Next);
    assert_eq!(set("Set an alarm for 7:30 p.m.").time, t(19, 30));
    assert_eq!(
        set("set an alarm for seven thirty in the evening").time,
        t(19, 30)
    );
    assert_eq!(set("set an alarm for seven oh five am").time, t(7, 5));
    assert_eq!(set("set an alarm for half past six am").time, t(6, 30));
    assert_eq!(set("set an alarm for quarter to seven am").time, t(6, 45));
    assert_eq!(set("set an alarm for 12 am").time, t(0, 0));
    assert_eq!(set("set an alarm for noon").time, t(12, 0));
    assert_eq!(set("set an alarm for midnight").time, t(0, 0));
    assert_eq!(set("set an alarm for 17:30").time, t(17, 30));
    assert_eq!(set("set an alarm for seventeen thirty").time, t(17, 30));
    assert_eq!(set("set an alarm for 10 tonight").time, t(22, 0));
    assert_eq!(
        set("set an alarm for 7 o'clock in the morning").time,
        t(7, 0)
    );
    assert_eq!(set("wake me up at 6 am tomorrow").day, AlarmDay::Tomorrow);
    assert_eq!(
        set("set an alarm for 7 tomorrow morning").day,
        AlarmDay::Tomorrow
    );
    assert_eq!(set("set an alarm for tomorrow morning at 7").time, t(7, 0));
    assert_eq!(set("set an alarm for today at 5 pm").day, AlarmDay::Today);
    let weekdays = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
    ]
    .into_iter()
    .fold(Weekdays::NONE, Weekdays::with);
    let weekend = Weekdays::NONE.with(Weekday::Saturday).with(Weekday::Sunday);
    assert_eq!(
        set("set an alarm for 6 30 am on weekdays").day,
        AlarmDay::Repeat(Repeat::on(weekdays))
    );
    assert_eq!(
        set("set a weekday alarm for 6 am").day,
        AlarmDay::Repeat(Repeat::on(weekdays))
    );
    assert_eq!(
        set("set an alarm for 6 am every weekday").day,
        AlarmDay::Repeat(Repeat::on(weekdays))
    );
    assert_eq!(
        set("set an alarm for 9 am on weekends").day,
        AlarmDay::Repeat(Repeat::on(weekend))
    );
    assert_eq!(
        set("set an alarm for 7 am every day").day,
        AlarmDay::Repeat(Repeat::Daily)
    );
    assert_eq!(
        set("set a daily alarm for 7 am").day,
        AlarmDay::Repeat(Repeat::Daily)
    );
    assert_eq!(
        set("set an alarm for 7 am every monday and wednesday").day,
        AlarmDay::Repeat(Repeat::on(
            Weekdays::NONE
                .with(Weekday::Monday)
                .with(Weekday::Wednesday)
        ))
    );
    assert_eq!(
        set("set an alarm every monday, wednesday and friday at 7 am").day,
        AlarmDay::Repeat(Repeat::on(
            Weekdays::NONE
                .with(Weekday::Monday)
                .with(Weekday::Wednesday)
                .with(Weekday::Friday)
        ))
    );
    assert_eq!(
        set("set an alarm for 8 am on saturdays").day,
        AlarmDay::Repeat(Repeat::on(Weekdays::NONE.with(Weekday::Saturday)))
    );
    assert_eq!(
        set("set an alarm for 7 am on friday").day,
        AlarmDay::Weekday(Weekday::Friday)
    );
    assert_eq!(
        set("set an alarm for 7 am on October thirtieth").day,
        AlarmDay::Date {
            month: Some(10),
            day: 30
        }
    );
    assert_eq!(
        set("set an alarm for 7 am on december 3").day,
        AlarmDay::Date {
            month: Some(12),
            day: 3
        }
    );
    assert_eq!(
        set("set an alarm for 7 am on the 3rd").day,
        AlarmDay::Date {
            month: None,
            day: 3
        }
    );
    let labelled = set("set an alarm for 7 am called work");
    assert_eq!(labelled.label.as_deref(), Some("work"));
    assert_eq!(
        set("set an alarm called gym for 6 am").label.as_deref(),
        Some("gym")
    );
    assert_eq!(set("alarm for 6 am").time, t(6, 0));
    assert_eq!(set("please set an alarm for 6 am").time, t(6, 0));

    // AM or PM missing.
    match route("set an alarm for seven") {
        Route::AskMeridiem(pending) => {
            assert_eq!(
                (pending.hour, pending.minute, pending.day),
                (7, 0, AlarmDay::Next)
            );
            assert_eq!(pending.with_meridiem(Meridiem::Pm).time, t(19, 0));
        }
        other => panic!("{other:?}"),
    }
    match route("wake me up at 6 30 tomorrow") {
        Route::AskMeridiem(pending) => {
            assert_eq!(
                (pending.hour, pending.minute, pending.day),
                (6, 30, AlarmDay::Tomorrow)
            );
        }
        other => panic!("{other:?}"),
    }

    // Cancels.
    assert_eq!(
        route("cancel all my alarms"),
        Route::CancelAlarms(AlarmCancel::All)
    );
    assert_eq!(
        route("delete my alarms"),
        Route::CancelAlarms(AlarmCancel::All)
    );
    assert_eq!(
        route("cancel my alarm"),
        Route::CancelAlarms(AlarmCancel::Only)
    );
    assert_eq!(
        route("turn off my alarm"),
        Route::CancelAlarms(AlarmCancel::Only)
    );
    assert_eq!(
        route("cancel my 7 am alarm"),
        Route::CancelAlarms(AlarmCancel::At(SaidTime::Exact(t(7, 0))))
    );
    assert_eq!(
        route("cancel the alarm for 6:30 pm"),
        Route::CancelAlarms(AlarmCancel::At(SaidTime::Exact(t(18, 30))))
    );
    assert_eq!(
        route("cancel my seven o'clock alarm"),
        Route::CancelAlarms(AlarmCancel::At(SaidTime::Ambiguous { hour: 7, minute: 0 }))
    );
    assert_eq!(
        route("cancel my work alarm"),
        Route::CancelAlarms(AlarmCancel::Labelled("work".to_owned()))
    );

    // 3. Alarm questions.
    for text in [
        "what alarms do I have",
        "which alarms are set",
        "do I have any alarms",
        "list my alarms",
        "are there any alarms set",
    ] {
        assert_eq!(route(text), Route::AlarmQuery(AlarmQuery::List), "{text}");
    }
    for text in [
        "when is my next alarm",
        "when's my alarm",
        "what time is my alarm set for",
        "what time does my alarm go off",
    ] {
        assert_eq!(route(text), Route::AlarmQuery(AlarmQuery::Next), "{text}");
    }

    // 4. Timers on this speaker.
    assert_eq!(
        route("cancel the timer"),
        Route::CancelTimers(TimerCancel::Any)
    );
    assert_eq!(
        route("stop the timer"),
        Route::CancelTimers(TimerCancel::Any)
    );
    assert_eq!(
        route("cancel all timers"),
        Route::CancelTimers(TimerCancel::All)
    );
    assert_eq!(
        route("cancel both timers"),
        Route::CancelTimers(TimerCancel::All)
    );
    assert_eq!(
        route("turn off my timers"),
        Route::CancelTimers(TimerCancel::All)
    );
    for text in [
        "cancel every timer",
        "cancel each timer",
        "cancel all of my timers",
        "can you cancel all timers please",
    ] {
        assert_eq!(route(text), Route::CancelTimers(TimerCancel::All), "{text}");
    }
    for text in [
        "please cancel the timer",
        "cancel my timer please",
        "end the timer",
    ] {
        assert_eq!(route(text), Route::CancelTimers(TimerCancel::Any), "{text}");
    }
    // A named or timed cancel is HA's, and so is every other timer request: HA's own
    // timer intents (with the custom sentences in its configuration) answer them.
    for text in [
        "cancel the pasta timer",
        "cancel the 10 minute timer",
        "cancel the timer for 10 minutes",
        "set a ten second timer",
        "set a timer for 5 minutes",
        "how much time is left",
        "how much time is left on the timer",
        "what timers do I have",
        "pause the timer",
        "resume my timer",
        "add 5 minutes to the timer",
        "remove 2 minutes from the timer",
        // Not a cancel verb: the field sent "canceled the timer" to HA too (turn 925).
        "canceled the timer",
        // Verbs the deployed controller left to HA.
        "kill the timer",
        "switch off all timers",
    ] {
        assert_eq!(route(text), Route::SendToHa, "{text}");
    }

    // Everything else.
    for text in ["what's the weather", "turn off the lights", "stop", ""] {
        assert_eq!(route(text), Route::SendToHa, "{text}");
    }
}

#[test]
fn alarm_requests_map_onto_the_alert_service() {
    let g = grammar();
    let now = date(2026, 10, 28).at(20, 0, 0, 0);
    let Route::SetAlarm(request) = g.route("set an alarm for 6 30 am on weekdays") else {
        panic!()
    };
    let op = OpId::voice_alarm("turn-1");
    let set = request
        .set_alarm(op, DeviceId("kitchen".to_owned()), now)
        .unwrap();
    assert_eq!(set.op_id, op);
    assert_eq!(set.source, OpSource::Voice);
    assert_eq!(set.when.time, t(6, 30));
    assert_eq!(set.when.date, None);
    assert_eq!(
        set.when.repeat.rrule().as_deref(),
        Some("FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR")
    );
    let Route::SetAlarm(request) = g.route("set an alarm for 7 am on friday") else {
        panic!()
    };
    let when = request.when(now).unwrap();
    assert_eq!(
        (when.date, when.repeat),
        (Some(date(2026, 10, 30)), Repeat::Once)
    );
}

fn choice(id: u128, h: u8, m: u8, label: Option<&str>) -> AlarmChoice {
    AlarmChoice {
        schedule_id: uuid::Uuid::from_u128(id),
        due: date(2026, 10, 29).at(h as i8, m as i8, 0, 0),
        repeat: Repeat::Once,
        label: label.map(str::to_owned),
        snoozed: false,
    }
}

#[test]
fn alarm_cancel_plans() {
    let morning = choice(1, 7, 0, None);
    let evening = choice(2, 19, 0, Some("Work"));
    let both = [morning.clone(), evening.clone()];
    assert_eq!(AlarmCancel::Only.plan(&[]), AlarmCancelPlan::NoAlarms);
    assert_eq!(
        AlarmCancel::Only.plan(std::slice::from_ref(&morning)),
        AlarmCancelPlan::Cancel(vec![morning.clone()])
    );
    assert_eq!(
        AlarmCancel::Only.plan(&both),
        AlarmCancelPlan::WhichOne(both.to_vec())
    );
    assert_eq!(AlarmCancel::All.plan(&both), AlarmCancelPlan::CancelAll(2));
    assert_eq!(
        AlarmCancel::At(SaidTime::Exact(t(19, 0))).plan(&both),
        AlarmCancelPlan::Cancel(vec![evening.clone()])
    );
    assert_eq!(
        AlarmCancel::At(SaidTime::Ambiguous { hour: 7, minute: 0 }).plan(&both),
        AlarmCancelPlan::AskMeridiem
    );
    assert_eq!(
        AlarmCancel::At(SaidTime::Ambiguous { hour: 7, minute: 0 })
            .plan(std::slice::from_ref(&morning)),
        AlarmCancelPlan::Cancel(vec![morning.clone()])
    );
    assert_eq!(
        AlarmCancel::At(SaidTime::Exact(t(8, 0))).plan(&both),
        AlarmCancelPlan::NotFound
    );
    assert_eq!(
        AlarmCancel::Labelled("work".to_owned()).plan(&both),
        AlarmCancelPlan::Cancel(vec![evening.clone()])
    );
    let request = AlarmCancelPlan::Cancel(vec![evening])
        .request(OpId::voice_alarm("turn-2"), DeviceId("kitchen".to_owned()))
        .unwrap();
    assert_eq!(
        request.selector,
        echomuse_alerts::relay::CancelSelector::Schedules(vec![uuid::Uuid::from_u128(2)])
    );
    assert!(
        AlarmCancelPlan::NotFound
            .request(OpId::voice_alarm("turn-3"), DeviceId("kitchen".to_owned()))
            .is_none()
    );
}

fn alert(kind: RingKind, name: &str) -> CommandContext {
    CommandContext {
        alert: Some(RingingAlert {
            kind,
            name: name.to_owned(),
        }),
        ..CommandContext::default()
    }
}

#[test]
fn context_commands() {
    let g = grammar();
    let timer = alert(RingKind::Timer, "Pasta");
    let alarm = alert(RingKind::Alarm, "");
    let cmd = |text: &str, ctx: &CommandContext| g.context_command(text, ctx);
    use ContextCommand::*;
    assert_eq!(cmd("stop", &timer), Some(DismissAlert));
    assert_eq!(cmd("Stop.", &alarm), Some(DismissAlert));
    assert_eq!(cmd("cancel", &alarm), Some(DismissAlert));
    assert_eq!(cmd("stop please", &alarm), Some(DismissAlert));
    assert_eq!(cmd("snooze", &alarm), Some(SnoozeAlert));
    assert_eq!(cmd("snooze the alarm", &alarm), Some(SnoozeAlert));
    assert_eq!(
        cmd("snooze", &alert(RingKind::Snooze, "")),
        Some(SnoozeAlert)
    );
    // Timers cannot be snoozed.
    assert_eq!(cmd("snooze", &timer), None);
    // Named stops must name what rings.
    assert_eq!(cmd("stop the pasta timer", &timer), Some(DismissAlert));
    assert_eq!(cmd("stop the timer", &timer), Some(DismissAlert));
    assert_eq!(cmd("turn off the timer", &timer), Some(DismissAlert));
    assert_eq!(cmd("stop the tea timer", &timer), None);
    assert_eq!(cmd("stop the alarm", &timer), None);
    assert_eq!(cmd("dismiss the alarm", &alarm), Some(DismissAlert));
    assert_eq!(cmd("stop the pasta alarm", &alarm), None);
    // The reply and media.
    let reply = CommandContext {
        reply_playing: true,
        ..CommandContext::default()
    };
    assert_eq!(cmd("stop", &reply), Some(StopReply));
    assert_eq!(cmd("pause", &reply), None);
    let media = CommandContext {
        media_playing: true,
        ..CommandContext::default()
    };
    assert_eq!(cmd("stop", &media), Some(StopMedia));
    assert_eq!(cmd("pause", &media), Some(PauseMedia));
    assert_eq!(cmd("cancel", &media), None);
    // An alert comes before media.
    let both = CommandContext {
        media_playing: true,
        reply_playing: true,
        ..alert(RingKind::Alarm, "")
    };
    assert_eq!(cmd("stop", &both), Some(DismissAlert));
    assert_eq!(cmd("pause", &both), Some(PauseMedia));
    // No context, or no command.
    assert_eq!(cmd("stop", &CommandContext::default()), None);
    assert_eq!(cmd("what time is it", &both), None);
    assert_eq!(cmd("don't stop", &both), None);
}

#[test]
fn answers_to_questions() {
    let g = grammar();
    for (text, expected) in [
        ("pm", Some(Meridiem::Pm)),
        ("P.M.", Some(Meridiem::Pm)),
        ("a m", Some(Meridiem::Am)),
        ("in the morning", Some(Meridiem::Am)),
        ("morning", Some(Meridiem::Am)),
        ("evening", Some(Meridiem::Pm)),
        ("at night", Some(Meridiem::Pm)),
        ("tuesday", None),
    ] {
        assert_eq!(g.meridiem(text), expected, "{text}");
    }
    for (text, expected) in [
        ("the pasta one", WhichReply::Name("pasta".to_owned())),
        ("pasta", WhichReply::Name("pasta".to_owned())),
        ("the ten minute one", WhichReply::Length { seconds: 600 }),
        ("the one hour timer", WhichReply::Length { seconds: 3600 }),
        ("the first one", WhichReply::Position(1)),
        ("the second one", WhichReply::Position(2)),
        ("the last one", WhichReply::Last),
        ("both of them", WhichReply::All),
        ("never mind", WhichReply::Decline),
        ("leave them", WhichReply::Decline),
        ("neither of them", WhichReply::Decline),
        ("the 7 am one", WhichReply::Time(SaidTime::Exact(t(7, 0)))),
        ("my 10 minute timer", WhichReply::Length { seconds: 600 }),
        ("10 minutes", WhichReply::Length { seconds: 600 }),
        (
            "the 10 minute pasta timer",
            WhichReply::LengthAndName {
                seconds: 600,
                name: "pasta".to_owned(),
            },
        ),
        // A whole cancel answers too.
        (
            "cancel the 5 minute timer",
            WhichReply::Length { seconds: 300 },
        ),
        (
            "cancel the pasta timer",
            WhichReply::Name("pasta".to_owned()),
        ),
        (
            "cancel the timer called pasta",
            WhichReply::Name("pasta".to_owned()),
        ),
        ("cancel both timers", WhichReply::All),
        ("cancel all", WhichReply::All),
        ("every timer", WhichReply::All),
    ] {
        assert_eq!(g.which(text), Some(expected), "{text}");
    }
    // A bare cancel names no timer: at most its words are read as a name.
    assert!(
        matches!(
            g.which("cancel the timer"),
            None | Some(WhichReply::Name(_))
        ),
        "{:?}",
        g.which("cancel the timer")
    );
    assert_eq!(g.classify_reply("pm", ReplyKind::Meridiem), Complete);
    assert_eq!(
        g.classify_reply("the pasta one", ReplyKind::Which),
        Complete
    );
    assert_eq!(
        g.classify_reply("set a timer for", ReplyKind::Which),
        NeedsMore
    );
}

// ---- Sentences generated from the vendored templates ----

/// Words the sampler puts into wildcard slots: they explain nothing literally.
const OPEN_WORDS: [&str; 2] = ["zebra", "yak"];

struct Sampler<'a> {
    set: &'a TemplateSet,
    seed: u64,
    mode: Mode,
}

#[derive(Clone, Copy)]
enum Mode {
    First,
    Last,
    Random,
}

impl Sampler<'_> {
    fn pick(&mut self, n: usize) -> usize {
        match self.mode {
            Mode::First => 0,
            Mode::Last => n - 1,
            Mode::Random => {
                self.seed = self
                    .seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((self.seed >> 33) as usize) % n
            }
        }
    }

    fn sample(&mut self, expr: &Expr, out: &mut String) {
        match expr {
            Expr::Text(text) => out.push_str(text),
            Expr::Seq(items) => {
                for item in items {
                    self.sample(item, out);
                }
            }
            Expr::Alt(items) => {
                let i = self.pick(items.len());
                self.sample(&items[i], out);
            }
            Expr::Perm(items) => {
                for item in items {
                    self.sample(item, out);
                }
            }
            Expr::Rule(name) => {
                let rule = template::parse(&self.set.rules[name]).unwrap();
                self.sample(&rule, out);
            }
            Expr::List { list, .. } => match self.set.lists.get(list) {
                Some(ListDef::Values(values)) => {
                    let i = self.pick(values.len());
                    let value = template::parse(&values[i].template).unwrap();
                    self.sample(&value, out);
                }
                Some(ListDef::Range { from, to, step, .. }) => {
                    let n = match self.pick(3) {
                        0 => *from,
                        1 => *to,
                        _ => from + ((to - from) / step / 2) * step,
                    };
                    out.push_str(&n.to_string());
                }
                Some(ListDef::Ordinal { .. }) => out.push_str("3rd"),
                Some(ListDef::Wildcard) => out.push_str(&OPEN_WORDS.join(" ")),
                None => out.push_str(match list.as_str() {
                    "name" => "front door",
                    "area" => "living room",
                    _ => "upstairs",
                }),
            },
        }
    }
}

/// Generated sentences of a set: every sentence three ways (first alternatives, last
/// alternatives, a seeded random choice).
fn generated(set: &TemplateSet, groups: &[Group]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (i, (text, origin, group)) in set.sentences.iter().enumerate() {
        if !groups.contains(group) {
            continue;
        }
        if matches!(
            origin,
            Origin::Local {
                flag: crate::local::Flag::NeedsMeridiem | crate::local::Flag::Final,
                ..
            }
        ) {
            continue;
        }
        let expr = template::parse(text).unwrap();
        for mode in [Mode::First, Mode::Last, Mode::Random] {
            let mut sampler = Sampler {
                set,
                seed: i as u64 + 1,
                mode,
            };
            let mut sentence = String::new();
            sampler.sample(&expr, &mut sentence);
            out.push((text.clone(), sentence));
        }
    }
    out
}

/// Timer and command templates: every generated sentence is a whole request (complete,
/// or extendable for an open repeat request), and a prefix is never called whole unless it
/// is itself a whole template, never when it ends in a word such as `the` or `for`.
fn check_generated(set: &TemplateSet, groups: &[Group]) {
    let g = with_vocabulary();
    let mut failures = Vec::new();
    let mut checked = 0;
    let whole_template = |text: &str| {
        let words = matcher::remove_skip_words(&g.program, normalize(text).tokens());
        !matcher::run(
            &g.program,
            &g.vocab,
            words,
            &[Group::HaTimers, Group::Commands],
        )
        .accepts
        .is_empty()
    };
    let open_word = |w: &String| {
        [
            "a", "an", "the", "for", "to", "in", "on", "at", "and", "or", "with",
        ]
        .contains(&w.as_str())
    };
    for (template, sentence) in generated(set, groups) {
        let words = normalize(&sentence).into_tokens();
        let class = g.classify(&sentence);
        checked += 1;
        // "when is my alarm set for" is whole, but ends in a word that never ends one.
        let whole = matches!(class, Complete | Extendable)
            || (class == NeedsMore && words.last().is_some_and(open_word));
        if !whole {
            failures.push(format!("{sentence:?} ({template}): {class:?}"));
        }
        for len in 1..words.len() {
            let prefix = &words[..len];
            let text = prefix.join(" ");
            let class = g.classify(&text);
            checked += 1;
            let ends_open = prefix.last().is_some_and(open_word);
            let allowed = match class {
                NeedsMore | Unknown => true,
                Complete | Extendable => !ends_open && whole_template(&text),
            };
            if !allowed {
                failures.push(format!(
                    "prefix {text:?} of {sentence:?} ({template}): {class:?}"
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {checked} judgements wrong:\n{}",
        failures.len(),
        failures
            .iter()
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn generated_home_assistant_timer_sentences_are_whole_and_their_prefixes_are_not() {
    check_generated(
        &crate::intents::home_assistant().unwrap(),
        &[Group::HaTimers],
    );
}

#[test]
fn generated_local_sentences_are_whole_and_their_prefixes_are_not() {
    check_generated(&crate::local::templates(), &[Group::Commands]);
}

/// Home Assistant's other sentences are its matcher's to judge: without a vocabulary
/// (so no target can be turned on or off) the grammar leaves them unknown, unless the
/// words are also one of the timer or command templates or the start of one.
#[test]
fn generated_other_home_assistant_sentences_are_left_to_its_matcher() {
    let g = grammar();
    let mut failures = Vec::new();
    let set = crate::intents::home_assistant().unwrap();
    for (template, sentence) in generated(&set, &[Group::Ha]) {
        let class = g.classify(&sentence);
        let words = matcher::remove_skip_words(&g.program, normalize(&sentence).tokens());
        let ours = !matcher::run(
            &g.program,
            &g.vocab,
            words,
            &[Group::HaTimers, Group::Commands],
        )
        .threads
        .is_empty();
        if class != Unknown && !ours {
            failures.push(format!("{sentence:?} ({template}): {class:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn generated_local_alarm_sentences_route_locally() {
    let set = crate::local::templates();
    let g = grammar();
    for (template, sentence) in generated(&set, &[Group::Commands]) {
        let route = g.route(&sentence);
        // Timer cancels said with "kill" or "switch off" are HA's, as in the deployed
        // controller; the templates take them for completeness only.
        let left_to_ha = sentence.starts_with("kill") || sentence.starts_with("switch off");
        let expected_local = (template.contains("alarm") && !template.starts_with("snooze"))
            || template.contains("repeat")
            || template.contains("pardon")
            || ((template.contains("<timer_verb> [<the_my>] timer")
                || template.contains("timers"))
                && !left_to_ha);
        if expected_local {
            assert_ne!(route, Route::SendToHa, "{sentence:?} ({template})");
        }
    }
}

#[test]
#[ignore = "diagnostic: prints the readings of EXPLAIN texts"]
fn explain() {
    let texts = std::env::var("EXPLAIN").unwrap_or_default();
    let files = std::env::var("EXPLAIN_VOCAB").ok().map(|dir| {
        let read = |f: &str| {
            Some(
                std::fs::read_to_string(format!("{dir}/{f}"))
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect(),
            )
        };
        grammar().with_vocabulary(&Vocabulary {
            names: read("names.txt"),
            areas: read("areas.txt"),
            floors: read("floors.txt"),
        })
    });
    let vocab = files.as_ref().unwrap_or(with_vocabulary());
    for (g, label) in [(grammar(), "open"), (vocab, "vocab")] {
        for text in texts.split('|') {
            let words = matcher::remove_skip_words(&g.program, normalize(text).tokens());
            let run = matcher::run(
                &g.program,
                &g.vocab,
                words,
                &[Group::Ha, Group::HaTimers, Group::Commands],
            );
            println!("== [{label}] {text:?} -> {:?}", g.classify(text));
            for a in &run.accepts {
                println!(
                    "  accept {} {}",
                    a.anchors, g.program.sentences[a.sentence as usize].template
                );
            }
            for t in run
                .threads
                .iter()
                .filter(|t| run.waits_for_value(t))
                .take(5)
            {
                let sentence = &g.program.sentences[run.sentence(t) as usize];
                println!(
                    "  waits for a value: {:?} {}",
                    sentence.group, sentence.template
                );
            }
        }
    }
}
