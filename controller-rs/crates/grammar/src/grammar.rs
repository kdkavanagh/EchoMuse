//! The local grammar: completeness, context commands, routing after STT, and the answers to
//! the controller's questions.

use std::sync::Arc;

use echomuse_alerts::wire::RingKind;

use crate::{
    alarm::{AlarmCancel, AlarmQuery, AlarmRequest, Meridiem, PendingAlarm, SaidTime},
    alarm::{said_day, said_label, said_time},
    intents,
    local::{self, Flag, LocalIntent},
    matcher::{self, Accepted, Capture, Loc, RunResult, Thread, integer},
    normalize::normalize,
    program::{CompileError, Group, Origin, Program, SentenceId},
    reply::WhichReply,
    timer::TimerCancel,
    vocabulary::{VocabIndex, Vocabulary},
};

/// How finished a transcript looks (§4.6): it sets how long a pause ends the turn.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Completeness {
    /// A whole request.
    Complete,
    /// A whole request that may still go on: part of a longer known name (`turn off
    /// kitchen` beside `kitchen lights`), or a repeat request that also begins others
    /// (`what was that` → `what was that song`).
    Extendable,
    /// The start of a request (`set a timer for`).
    NeedsMore,
    /// The grammar cannot judge it; Home Assistant's sentence matcher may.
    Unknown,
}

/// Which question a reply answers.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReplyKind {
    /// "AM or PM?"
    Meridiem,
    /// "Which one?"
    Which,
}

/// What rings on the speaker (or on the one other speaker that rings, §4.2) when a wake
/// opens.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RingingAlert {
    /// Alarm, snooze or timer.
    pub kind: RingKind,
    /// The alarm's label or the timer's name; empty when it has none.
    pub name: String,
}

/// The context a wake opened in (§4.4).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandContext {
    /// An alert rings.
    pub alert: Option<RingingAlert>,
    /// The device's own reply is playing.
    pub reply_playing: bool,
    /// Media is playing.
    pub media_playing: bool,
}

/// A command handled right after a wake, before STT (§4.4).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContextCommand {
    /// Stop the ringing alert (`alert.act` dismiss).
    DismissAlert,
    /// Snooze the ringing alarm (`alert.act` snooze).
    SnoozeAlert,
    /// Stop the device's reply.
    StopReply,
    /// Stop the media.
    StopMedia,
    /// Pause the media.
    PauseMedia,
}

/// Where a final transcript goes (§4.4, tried in this order).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Route {
    /// Replay the last answer, if it ended within 120 s (the caller checks the time).
    Repeat,
    /// Set an alarm.
    SetAlarm(AlarmRequest),
    /// Set an alarm once the user says AM or PM: ask "AM or PM?".
    AskMeridiem(PendingAlarm),
    /// Cancel alarms ([`AlarmCancel::plan`]).
    CancelAlarms(AlarmCancel),
    /// Answer a question about alarms.
    AlarmQuery(AlarmQuery),
    /// Cancel timers on this speaker ([`crate::SpeakerTimers::plan`]).
    CancelTimers(TimerCancel),
    /// Everything else: Home Assistant's intent handling.
    SendToHa,
}

/// The compiled local grammar. Cloning shares the compiled templates.
#[derive(Clone, Debug)]
pub struct Grammar {
    pub(crate) program: Arc<Program>,
    pub(crate) vocab: Arc<VocabIndex>,
}

/// Words that never end a request (`set a timer for`, `turn on the`).
const TERMINAL_WORDS: &[&str] = &[
    "a", "an", "the", "for", "to", "in", "on", "at", "and", "or", "with",
];

impl Completeness {
    /// Complete is the strongest, then Extendable, NeedsMore and Unknown.
    fn strength(self) -> u8 {
        match self {
            Self::Complete => 3,
            Self::Extendable => 2,
            Self::NeedsMore => 1,
            Self::Unknown => 0,
        }
    }
}

impl Grammar {
    /// Compiles Home Assistant's vendored English templates and the controller's own. Takes
    /// a fraction of a second; do it once and clone.
    pub fn new() -> Result<Self, CompileError> {
        let ha = intents::home_assistant()?;
        let program = Program::compile(&[ha, local::templates()])?;
        Ok(Self {
            program: Arc::new(program),
            vocab: Arc::new(VocabIndex::default()),
        })
    }

    /// The same grammar with Home Assistant's current names, areas and floors.
    pub fn with_vocabulary(&self, vocabulary: &Vocabulary) -> Self {
        Self {
            program: self.program.clone(),
            vocab: Arc::new(VocabIndex::new(vocabulary)),
        }
    }

    /// Every word the compiled templates spell literally (template text and closed list
    /// values), normalized and sorted.
    pub fn template_words(&self) -> Vec<&str> {
        let mut words: Vec<&str> = self.program.words.keys().map(String::as_str).collect();
        words.sort_unstable();
        words
    }

    /// Normalized words with skip words removed; `None` when nothing is left.
    fn words(&self, text: &str) -> Option<Vec<String>> {
        let tokens = normalize(text).into_tokens();
        let words = matcher::remove_skip_words(&self.program, &tokens);
        (!words.is_empty()).then_some(words)
    }

    fn run(&self, words: Vec<String>, groups: &[Group]) -> RunResult<'_> {
        matcher::run(&self.program, &self.vocab, words, groups)
    }

    fn origin(&self, accepted: &Accepted) -> &Origin {
        &self.program.sentences[accepted.sentence as usize].origin
    }

    fn local(&self, accepted: &Accepted) -> Option<(LocalIntent, Flag)> {
        match self.origin(accepted) {
            Origin::Local { intent, flag } => Some((*intent, *flag)),
            Origin::Ha { .. } => None,
        }
    }

    /// How finished `text` looks (§4.6). The local grammar judges only the requests it
    /// knows itself, and leaves every other text to Home Assistant's sentence matcher as
    /// [`Completeness::Unknown`]. The text is normalized ([`crate::normalize()`]); the
    /// strongest class any of these requests gives wins:
    ///
    /// - **Timers** (Home Assistant's own timer templates) and **the controller's own
    ///   commands** (a bare stop, cancel, snooze or pause; stopping a ringing alert;
    ///   alarms; questions about alarms; cancelling timers; repeat), with skip words
    ///   removed. A whole template → [`Completeness::Complete`], whatever optional words
    ///   or longer templates could follow; an alarm hour without AM or PM →
    ///   [`Completeness::NeedsMore`]; a repeat request that also begins other requests
    ///   (`what was that`) → [`Completeness::Extendable`]. The start of a template →
    ///   [`Completeness::NeedsMore`] when it waits for a value (a number, a name, any
    ///   words, or the unit after a number) or is the start of an alarm request that
    ///   already said `alarm` or `wake me`; any other start → [`Completeness::Unknown`].
    /// - **Turning a known target on or off** (`turn off the kitchen lights`, `kitchen
    ///   lights off`; targets are Home Assistant's names, areas and floors, none when no
    ///   vocabulary is known) → [`Completeness::Complete`], or
    ///   [`Completeness::Extendable`] when a longer target starts with it. `turn on [the]`,
    ///   a target without on or off, and part of a target → [`Completeness::NeedsMore`].
    /// - A whole request whose last word is one such as `the`, `for` or `on` (other than
    ///   the on or off of a target) → [`Completeness::NeedsMore`].
    pub fn classify(&self, text: &str) -> Completeness {
        let tokens = normalize(text).into_tokens();
        if tokens.is_empty() {
            return Completeness::Unknown;
        }
        let commands = self.commands_class(&tokens);
        let home = self.home_class(&tokens);
        if home.strength() > commands.strength() {
            home
        } else {
            commands
        }
    }

    /// Timers and the controller's own commands.
    fn commands_class(&self, tokens: &[String]) -> Completeness {
        let words = matcher::remove_skip_words(&self.program, tokens);
        let Some(last) = words.last() else {
            return Completeness::Unknown;
        };
        let ends_open = TERMINAL_WORDS.contains(&last.as_str());
        let alarm_said = words.iter().any(|w| w == "alarm" || w == "alarms")
            || words.starts_with(&["wake".to_owned(), "me".to_owned()])
            || words.get(1).is_some_and(|w| w == "all");
        let run = self.run(words, &[Group::HaTimers, Group::Commands]);
        let whole = run
            .accepts
            .iter()
            .filter(|a| {
                !a.is_guesswork()
                    && (!self.is_ha_timer(a.sentence)
                        || run.accepted_wildcards_bounded(a, TERMINAL_WORDS))
            })
            .map(|a| match self.local(a) {
                Some((_, Flag::NeedsMeridiem)) => Completeness::NeedsMore,
                Some((_, Flag::Extendable)) => Completeness::Extendable,
                _ => Completeness::Complete,
            })
            .max_by_key(|c| c.strength());
        if let Some(class) = whole {
            return if ends_open {
                Completeness::NeedsMore
            } else {
                class
            };
        }
        let waits = run.threads.iter().any(|t| {
            let sentence = run.sentence(t);
            !t.is_guesswork()
                && (!self.is_ha_timer(sentence) || run.wildcards_bounded_so_far(t, TERMINAL_WORDS))
                && (run.waits_for_value(t) || (alarm_said && self.is_alarm(sentence)))
        });
        if waits {
            Completeness::NeedsMore
        } else {
            Completeness::Unknown
        }
    }

    /// Whether a sentence is one of Home Assistant's timer templates, whose names (a
    /// wildcard) neither start nor end with a number or a word such as `the` or `for`.
    fn is_ha_timer(&self, sentence: SentenceId) -> bool {
        self.program.sentences[sentence as usize].group == Group::HaTimers
    }

    /// Whether a sentence sets or cancels alarms.
    fn is_alarm(&self, sentence: SentenceId) -> bool {
        matches!(
            self.program.sentences[sentence as usize].origin,
            Origin::Local {
                intent: LocalIntent::AlarmSet
                    | LocalIntent::AlarmCancel
                    | LocalIntent::AlarmCancelAll,
                ..
            }
        )
    }

    /// Turning a known target on or off: `(turn|switch) (on|off) [the] <target>` or
    /// `<target> (on|off)`, on the words as said (skip words kept).
    fn home_class(&self, words: &[String]) -> Completeness {
        if !self.vocab.has_targets() {
            return Completeness::Unknown;
        }
        let one_of =
            |w: Option<&String>, set: &[&str]| w.is_some_and(|w| set.contains(&w.as_str()));
        let n = words.len();
        let mut targets: Vec<(&[String], bool)> = Vec::new();
        if one_of(words.first(), &["turn", "switch"]) && one_of(words.get(1), &["on", "off"]) {
            let from = if one_of(words.get(2), &["the"]) { 3 } else { 2 };
            if from >= n {
                return Completeness::NeedsMore;
            }
            targets.push((&words[from..], false));
        }
        if n >= 2 && one_of(words.last(), &["on", "off"]) {
            targets.push((&words[..n - 1], true));
        }
        for (target, action_last) in targets {
            match self.vocab.target_prefix(target) {
                Some(prefix) if prefix.whole => {
                    let ends_open = !action_last && one_of(words.last(), TERMINAL_WORDS);
                    return if ends_open {
                        Completeness::NeedsMore
                    } else if prefix.longer {
                        Completeness::Extendable
                    } else {
                        Completeness::Complete
                    };
                }
                Some(_) => return Completeness::NeedsMore,
                None => {}
            }
        }
        if self.vocab.target_prefix(words).is_some() {
            // A bare target still needs its on or off; part of one needs the rest.
            Completeness::NeedsMore
        } else {
            Completeness::Unknown
        }
    }

    /// Whether some template, Home Assistant's or the controller's, reads `text` as a whole
    /// request: a reading of a whole template explains at least as many words literally
    /// as any other reading, or the best reading stops part way through a known name.
    /// Transcript correction asks this; it is not completeness.
    pub fn reads_whole(&self, text: &str) -> bool {
        let Some(words) = self.words(text) else {
            return false;
        };
        let run = self.run(words, &[Group::Ha, Group::HaTimers, Group::Commands]);
        // Readings that guess more words into unknown names than they explain are no
        // evidence ("why is the sky blue" as "<area> the <name> blue").
        let accepts: Vec<&Accepted> = run.accepts.iter().filter(|a| !a.is_guesswork()).collect();
        if accepts
            .iter()
            .any(|a| matches!(self.local(a), Some((_, Flag::Final))))
        {
            return true;
        }
        let threads = run.threads.iter().filter(|t| !t.is_guesswork());
        let in_name = |t: &&Thread| matches!(t.loc, Loc::Phrase { .. });
        // A reading whose last word went into a wildcard is no evidence that more is
        // needed: any words fit "cancel the <name> timer" up to "timer".
        let best = threads
            .clone()
            .filter(|t| !in_name(t) && !t.tail_open)
            .map(|t| t.anchors)
            .chain(accepts.iter().map(|a| a.anchors))
            .max()
            .unwrap_or(0);
        if threads.filter(in_name).any(|t| t.anchors > best) {
            return true;
        }
        let complete = accepts
            .iter()
            .filter(|a| !matches!(self.local(a), Some((_, Flag::NeedsMeridiem))))
            .map(|a| a.anchors)
            .max();
        best > 0 && complete.is_some_and(|a| a > 0 && a >= best)
    }

    /// How finished a reply to the controller's question looks: an answer the question
    /// takes is [`Completeness::Complete`]; anything else is judged by
    /// [`Grammar::classify`].
    pub fn classify_reply(&self, text: &str, kind: ReplyKind) -> Completeness {
        let answered = match kind {
            ReplyKind::Meridiem => self.meridiem(text).is_some(),
            ReplyKind::Which => self.which_reading(text).is_some_and(|(reply, anchors)| {
                anchors > 0 || !matches!(&reply, WhichReply::Name(n) if n.split(' ').count() > 2)
            }),
        };
        if answered {
            Completeness::Complete
        } else {
            self.classify(text)
        }
    }

    /// The command `text` (the streaming text after the wake phrase) gives in `context`, if
    /// any. An alert takes `stop`, `cancel`, `snooze` (alarms only) and
    /// `(stop|cancel|turn off|dismiss|silence) [the] [<name>] (timer|alarm)`, where the noun
    /// must be what rings and the name, if said, its name; the device's reply takes `stop`;
    /// media takes `stop` and `pause`. The alert comes first, then the reply, then media.
    pub fn context_command(&self, text: &str, context: &CommandContext) -> Option<ContextCommand> {
        let words = self.words(text)?;
        let run = self.run(words, &[Group::Commands]);
        let mut bare: Option<String> = None;
        let mut stop_alert: Vec<Vec<Capture>> = Vec::new();
        let mut snooze_alert = false;
        for accepted in &run.accepts {
            match self.local(accepted) {
                Some((LocalIntent::Bare, _)) => {
                    bare = run
                        .captures(accepted)
                        .iter()
                        .find(|c| &*c.slot == "command")
                        .map(|c| c.value_or_text().to_owned());
                }
                Some((LocalIntent::StopAlert, _)) => stop_alert.push(run.captures(accepted)),
                Some((LocalIntent::SnoozeAlert, _)) => snooze_alert = true,
                _ => {}
            }
        }
        if let Some(alert) = &context.alert {
            let is_alarm = matches!(alert.kind, RingKind::Alarm | RingKind::Snooze);
            match bare.as_deref() {
                Some("stop" | "cancel") => return Some(ContextCommand::DismissAlert),
                Some("snooze") if is_alarm => return Some(ContextCommand::SnoozeAlert),
                _ => {}
            }
            if snooze_alert && is_alarm {
                return Some(ContextCommand::SnoozeAlert);
            }
            let ring_name = normalize(&alert.name);
            for caps in &stop_alert {
                let kind = caps
                    .iter()
                    .find(|c| &*c.slot == "kind")
                    .map(Capture::value_or_text);
                let kind_matches = match kind {
                    Some("alarm") => is_alarm,
                    Some("timer") => alert.kind == RingKind::Timer,
                    _ => false,
                };
                let name_matches = caps
                    .iter()
                    .find(|c| &*c.slot == "name")
                    .is_none_or(|c| !ring_name.is_empty() && normalize(&c.text) == ring_name);
                if kind_matches && name_matches {
                    return Some(ContextCommand::DismissAlert);
                }
            }
        }
        if context.reply_playing && bare.as_deref() == Some("stop") {
            return Some(ContextCommand::StopReply);
        }
        if context.media_playing {
            match bare.as_deref() {
                Some("stop") => return Some(ContextCommand::StopMedia),
                Some("pause") => return Some(ContextCommand::PauseMedia),
                _ => {}
            }
        }
        None
    }

    /// Where a final transcript (after wake-phrase removal) goes: a repeat request, an alarm
    /// to set or cancel, a question about alarms, a cancel of this speaker's timers that
    /// names no timer, tried in that order; everything else to Home Assistant. A cancel that
    /// names a timer by name or length goes to Home Assistant, whose timer intents match
    /// it.
    pub fn route(&self, text: &str) -> Route {
        let Some(words) = self.words(text) else {
            return Route::SendToHa;
        };
        // The timer cancels the deployed controller took itself begin with cancel, stop,
        // turn off, delete, remove, end or clear; "kill" and "switch off" (which the
        // timer templates take for completeness) leave the request to HA, as it did.
        let routed_timer_verb = !(words[0] == "kill"
            || (words[0] == "switch" && words.get(1).is_some_and(|w| w == "off")));
        let run = self.run(words, &[Group::Commands]);
        let mut by_intent: Vec<(LocalIntent, Flag, &Accepted)> = run
            .accepts
            .iter()
            .filter_map(|a| self.local(a).map(|(intent, flag)| (intent, flag, a)))
            .collect();
        // Most literal reading first within an intent.
        by_intent.sort_by_key(|x| std::cmp::Reverse(x.2.anchors));
        let of = |intent: LocalIntent, flag: Option<Flag>| {
            by_intent
                .iter()
                .filter(move |(i, f, _)| *i == intent && flag.is_none_or(|flag| *f == flag))
                .map(|(_, _, a)| run.captures(a))
        };

        if of(LocalIntent::Repeat, None).next().is_some() {
            return Route::Repeat;
        }
        for caps in of(LocalIntent::AlarmSet, Some(Flag::None)) {
            if let (Some(SaidTime::Exact(time)), Some(day)) = (said_time(&caps), said_day(&caps)) {
                return Route::SetAlarm(AlarmRequest {
                    time,
                    day,
                    label: said_label(&caps),
                });
            }
        }
        for caps in of(LocalIntent::AlarmSet, Some(Flag::NeedsMeridiem)) {
            if let (Some(SaidTime::Ambiguous { hour, minute }), Some(day)) =
                (said_time(&caps), said_day(&caps))
            {
                return Route::AskMeridiem(PendingAlarm {
                    hour,
                    minute,
                    day,
                    label: said_label(&caps),
                });
            }
        }
        if of(LocalIntent::AlarmCancelAll, None).next().is_some() {
            return Route::CancelAlarms(AlarmCancel::All);
        }
        if let Some(caps) = of(LocalIntent::AlarmCancel, None).next() {
            let cancel = if let Some(time) = said_time(&caps) {
                AlarmCancel::At(time)
            } else if let Some(label) = said_label(&caps) {
                AlarmCancel::Labelled(label)
            } else {
                AlarmCancel::Only
            };
            return Route::CancelAlarms(cancel);
        }
        if of(LocalIntent::AlarmNext, None).next().is_some() {
            return Route::AlarmQuery(AlarmQuery::Next);
        }
        if of(LocalIntent::AlarmList, None).next().is_some() {
            return Route::AlarmQuery(AlarmQuery::List);
        }
        if routed_timer_verb && of(LocalIntent::TimerCancelAll, None).next().is_some() {
            return Route::CancelTimers(TimerCancel::All);
        }
        if routed_timer_verb && of(LocalIntent::TimerCancelAny, None).next().is_some() {
            return Route::CancelTimers(TimerCancel::Any);
        }
        Route::SendToHa
    }

    /// The answer to "AM or PM?", if `text` is one (`pm`, `p m`, `in the morning`,
    /// `evening`, `at night`).
    pub fn meridiem(&self, text: &str) -> Option<Meridiem> {
        let words = self.words(text)?;
        let run = self.run(words, &[Group::Replies]);
        run.accepts
            .iter()
            .filter(|a| matches!(self.local(a), Some((LocalIntent::MeridiemReply, _))))
            .max_by_key(|a| a.anchors)
            .and_then(|a| {
                match run
                    .captures(a)
                    .iter()
                    .find(|c| &*c.slot == "meridiem")
                    .map(Capture::value_or_text)
                {
                    Some("am") => Some(Meridiem::Am),
                    Some("pm") => Some(Meridiem::Pm),
                    _ => None,
                }
            })
    }

    /// The answer to "Which one?", if `text` is one: a position (`the first one`, `the last
    /// one`), `all of them`, `never mind`, a length (`the ten minute one`), a time (`the 7 am
    /// one`) or a name (`the pasta one`, any other words).
    pub fn which(&self, text: &str) -> Option<WhichReply> {
        self.which_reading(text).map(|(reply, _)| reply)
    }

    fn which_reading(&self, text: &str) -> Option<(WhichReply, u16)> {
        let words = self.words(text)?;
        let run = self.run(words, &[Group::Replies]);
        let mut readings: Vec<&Accepted> = run
            .accepts
            .iter()
            .filter(|a| {
                matches!(
                    self.local(a),
                    Some((
                        LocalIntent::WhichReply | LocalIntent::WhichAll | LocalIntent::WhichDecline,
                        _
                    ))
                )
            })
            .collect();
        readings.sort_by_key(|x| std::cmp::Reverse(x.anchors));
        for accepted in readings {
            let caps = run.captures(accepted);
            let find = |slot: &str| caps.iter().find(|c| &*c.slot == slot);
            let reply = match self.local(accepted).map(|(i, _)| i) {
                Some(LocalIntent::WhichAll) => WhichReply::All,
                Some(LocalIntent::WhichDecline) => WhichReply::Decline,
                _ => {
                    if let Some(position) = find("position") {
                        match position.value_or_text() {
                            "last" => WhichReply::Last,
                            n => WhichReply::Position(integer(n)? as usize),
                        }
                    } else if let Some(seconds) = duration_seconds(&caps) {
                        match find("name") {
                            Some(name) => WhichReply::LengthAndName {
                                seconds,
                                name: name.text.clone(),
                            },
                            None => WhichReply::Length { seconds },
                        }
                    } else if let Some(time) = said_time(&caps) {
                        WhichReply::Time(time)
                    } else if let Some(name) = find("name") {
                        WhichReply::Name(name.text.clone())
                    } else {
                        continue;
                    }
                }
            };
            return Some((reply, accepted.anchors));
        }
        None
    }
}

/// The length a duration capture says, in seconds.
fn duration_seconds(caps: &[Capture]) -> Option<u32> {
    let get = |slot: &str| {
        caps.iter()
            .find(|c| &*c.slot == slot)
            .and_then(|c| integer(c.value_or_text()))
    };
    if let Some(total) = get("seconds_total") {
        return u32::try_from(total).ok();
    }
    let (h, m, s) = (get("hours"), get("minutes"), get("seconds"));
    if h.is_none() && m.is_none() && s.is_none() {
        return None;
    }
    u32::try_from(h.unwrap_or(0) * 3600 + m.unwrap_or(0) * 60 + s.unwrap_or(0)).ok()
}
