//! What a correction may not change: negations, numbers, units, on/off words and command
//! verbs, checked on the words before Home Assistant sees a candidate.
//!
//! The word lists are this crate's: English negations, the units Home Assistant's
//! intents and timers take, the words that say which way a device goes, and the verbs
//! household commands start with. A verb is recognised in any inflection ("turned",
//! "setting", "closes").

use std::collections::HashSet;

use crate::text::{is_stopword, number_values};

const NEGATIONS: &[&str] = &[
    "not", "no", "never", "none", "nothing", "nobody", "neither", "nor", "without", "cannot",
    "dont", "doesnt", "isnt", "arent", "wont", "cant", "didnt", "shouldnt",
];

/// Unit words and the unit each names.
const UNITS: &[(&str, &str)] = &[
    ("percent", "percent"),
    ("degree", "degree"),
    ("degrees", "degree"),
    ("celsius", "celsius"),
    ("centigrade", "celsius"),
    ("fahrenheit", "fahrenheit"),
    ("kelvin", "kelvin"),
    ("second", "second"),
    ("seconds", "second"),
    ("sec", "second"),
    ("secs", "second"),
    ("minute", "minute"),
    ("minutes", "minute"),
    ("mins", "minute"),
    ("hour", "hour"),
    ("hours", "hour"),
    ("hr", "hour"),
    ("hrs", "hour"),
    ("day", "day"),
    ("days", "day"),
    ("week", "week"),
    ("weeks", "week"),
    ("month", "month"),
    ("months", "month"),
    ("year", "year"),
    ("years", "year"),
    ("am", "am"),
    ("a.m.", "am"),
    ("pm", "pm"),
    ("p.m.", "pm"),
    ("inch", "inch"),
    ("inches", "inch"),
    ("foot", "foot"),
    ("feet", "foot"),
    ("yard", "yard"),
    ("yards", "yard"),
    ("mile", "mile"),
    ("miles", "mile"),
    ("meter", "meter"),
    ("meters", "meter"),
    ("metre", "meter"),
    ("metres", "meter"),
    ("centimeter", "centimeter"),
    ("centimeters", "centimeter"),
    ("kilometer", "kilometer"),
    ("kilometers", "kilometer"),
    ("gram", "gram"),
    ("grams", "gram"),
    ("kilogram", "kilogram"),
    ("kilograms", "kilogram"),
    ("pound", "pound"),
    ("pounds", "pound"),
    ("ounce", "ounce"),
    ("ounces", "ounce"),
    ("cup", "cup"),
    ("cups", "cup"),
    ("tablespoon", "tablespoon"),
    ("tablespoons", "tablespoon"),
    ("teaspoon", "teaspoon"),
    ("teaspoons", "teaspoon"),
    ("liter", "liter"),
    ("liters", "liter"),
    ("litre", "liter"),
    ("litres", "liter"),
    ("milliliter", "milliliter"),
    ("milliliters", "milliliter"),
    ("gallon", "gallon"),
    ("gallons", "gallon"),
    ("quart", "quart"),
    ("quarts", "quart"),
    ("pint", "pint"),
    ("pints", "pint"),
];

/// Words that say which way something goes.
const POLARITY: &[&str] = &[
    "on",
    "off",
    "up",
    "down",
    "open",
    "opened",
    "close",
    "closed",
    "lock",
    "locked",
    "unlock",
    "unlocked",
    "start",
    "stop",
    "pause",
    "resume",
    "unpause",
    "increase",
    "decrease",
    "raise",
    "lower",
    "louder",
    "quieter",
    "softer",
    "brighter",
    "dimmer",
    "dim",
    "higher",
    "warmer",
    "cooler",
    "colder",
    "hotter",
    "more",
    "less",
    "enable",
    "disable",
    "mute",
    "unmute",
    "arm",
    "armed",
    "disarm",
    "disarmed",
    "activate",
    "deactivate",
    "max",
    "maximum",
    "minimum",
    "full",
    "half",
    "next",
    "previous",
    "back",
    "forward",
];

/// Command verbs, in their base form.
const VERBS: &[&str] = &[
    "turn",
    "set",
    "switch",
    "open",
    "close",
    "lock",
    "unlock",
    "play",
    "pause",
    "stop",
    "start",
    "resume",
    "unpause",
    "cancel",
    "add",
    "remove",
    "delete",
    "show",
    "tell",
    "check",
    "run",
    "activate",
    "deactivate",
    "arm",
    "disarm",
    "mute",
    "unmute",
    "dim",
    "brighten",
    "raise",
    "lower",
    "increase",
    "decrease",
    "skip",
    "snooze",
    "dismiss",
    "find",
    "list",
    "watch",
    "listen",
    "say",
    "repeat",
    "call",
    "send",
    "broadcast",
    "announce",
    "toggle",
    "change",
    "put",
    "make",
    "give",
    "get",
    "enable",
    "disable",
    "complete",
    "buy",
    "read",
    "remind",
    "wake",
    "clear",
    "reset",
    "restart",
    "launch",
    "shut",
    "kill",
    "boost",
    "heat",
    "cool",
    "dock",
    "mow",
    "vacuum",
    "clean",
    "fade",
    "lift",
    "drop",
    "go",
];

fn negation(word: &str) -> bool {
    let bare: String = word.chars().filter(|c| *c != '\'').collect();
    NEGATIONS.contains(&bare.as_str()) || word.ends_with("n't")
}

fn unit(word: &str) -> Option<&'static str> {
    if word.ends_with('%') {
        return Some("percent");
    }
    UNITS.iter().find(|(w, _)| *w == word).map(|(_, u)| *u)
}

fn polarity(word: &str) -> bool {
    POLARITY.contains(&word)
}

/// A command verb's base form, from any inflection.
fn verb(word: &str) -> Option<&'static str> {
    let find = |w: &str| VERBS.iter().find(|v| **v == w).copied();
    if let Some(v) = find(word) {
        return Some(v);
    }
    let undouble = |stem: &str| -> Option<&'static str> {
        find(stem).or_else(|| {
            let bytes = stem.as_bytes();
            let n = bytes.len();
            (n >= 2 && bytes[n - 1] == bytes[n - 2])
                .then(|| find(&stem[..n - 1]))
                .flatten()
        })
    };
    if let Some(stem) = word.strip_suffix("ing") {
        return undouble(stem).or_else(|| find(&format!("{stem}e")));
    }
    if let Some(stem) = word.strip_suffix("ies") {
        return find(&format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix("ied") {
        return find(&format!("{stem}y"));
    }
    if let Some(stem) = word.strip_suffix("ed") {
        return undouble(stem).or_else(|| find(&format!("{stem}e")));
    }
    if let Some(v) = word.strip_suffix("es").and_then(find) {
        return Some(v);
    }
    word.strip_suffix('s').and_then(find)
}

fn is_number(word: &str) -> bool {
    !number_values(std::slice::from_ref(&word.to_owned())).is_empty()
}

/// Whether a correction may never replace or remove `word`: a negation, number, unit,
/// on/off word or command verb.
pub(crate) fn protected(word: &str) -> bool {
    negation(word)
        || is_number(word)
        || unit(word).is_some()
        || polarity(word)
        || verb(word).is_some()
}

/// Whether `words` hold a command verb.
pub(crate) fn has_verb(words: &[String]) -> bool {
    words.iter().any(|w| verb(w).is_some())
}

/// The protected words of some text.
#[derive(Debug, Default, Eq, PartialEq)]
struct Profile {
    negations: usize,
    numbers: Vec<u32>,
    units: Vec<&'static str>,
    polarity: Vec<String>,
    verbs: Vec<&'static str>,
}

fn profile(words: &[String]) -> Profile {
    let mut p = Profile {
        numbers: number_values(words),
        ..Profile::default()
    };
    for word in words {
        if negation(word) {
            p.negations += 1;
        }
        if let Some(u) = unit(word) {
            p.units.push(u);
        }
        if polarity(word) {
            p.polarity.push(word.clone());
        }
        if let Some(v) = verb(word) {
            p.verbs.push(v);
        }
    }
    p.units.sort_unstable();
    p.polarity.sort_unstable();
    p.verbs.sort_unstable();
    p
}

/// Whether every item of `small` is in `big`, counting repeats.
fn contains_all<T: Ord + Clone>(big: &[T], small: &[T]) -> bool {
    let mut big = big.to_vec();
    for item in small {
        match big.iter().position(|b| b == item) {
            Some(i) => {
                big.remove(i);
            }
            None => return false,
        }
    }
    true
}

/// How a candidate changed the heard words.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Edit {
    /// `heard[start..end]` replaced by `inserted` (empty: removed).
    Span {
        start: usize,
        end: usize,
        inserted: Vec<String>,
    },
    /// The whole text replaced.
    Whole,
}

/// Why a candidate does not keep what was said.
pub(crate) type NotPreserved = &'static str;

/// Whether `candidate` keeps the meaning of `heard`.
///
/// - A span edit may not replace or remove a negation, number, unit, on/off word or
///   command verb, nor insert one, except that a `verb_may_be_added` edit may insert a verb
///   into text that has none (a garbled command word). It may not remove a word of a
///   known name when `keep_names` is set.
/// - A whole-text edit keeps the same negations, numbers and units, drops no verb or
///   on/off word, adds no on/off word, adds a verb only when `verb_may_be_added` and the
///   heard text has none, and keeps every word of a known name the heard text has.
pub(crate) fn preserved(
    heard: &[String],
    candidate: &[String],
    edit: &Edit,
    verb_may_be_added: bool,
    name_words: &HashSet<String>,
) -> Result<(), NotPreserved> {
    let heard_profile = profile(heard);
    let no_verb = heard_profile.verbs.is_empty();
    match edit {
        Edit::Span {
            start,
            end,
            inserted,
        } => {
            let removed = &heard[*start..*end];
            for word in removed {
                if negation(word) {
                    return Err("changes a negation");
                }
                if is_number(word) {
                    return Err("changes a number");
                }
                if unit(word).is_some() {
                    return Err("changes a unit");
                }
                if polarity(word) {
                    return Err("changes an on/off word");
                }
                if verb(word).is_some() {
                    return Err("drops an action");
                }
            }
            for word in inserted {
                if negation(word) {
                    return Err("adds a negation");
                }
                if is_number(word) {
                    return Err("adds a number");
                }
                if unit(word).is_some() {
                    return Err("adds a unit");
                }
                if polarity(word) {
                    return Err("adds an on/off word");
                }
                if verb(word).is_some() && !(verb_may_be_added && no_verb) {
                    return Err("adds an action");
                }
            }
            if inserted.is_empty() && removed.iter().any(|w| name_words.contains(w)) {
                return Err("drops a name");
            }
            Ok(())
        }
        Edit::Whole => {
            let c = profile(candidate);
            if c.negations != heard_profile.negations {
                return Err("changes a negation");
            }
            if c.numbers != heard_profile.numbers {
                return Err("changes a number");
            }
            if c.units != heard_profile.units {
                return Err("changes a unit");
            }
            if !contains_all(&c.verbs, &heard_profile.verbs) {
                return Err("drops an action");
            }
            if !contains_all(&c.polarity, &heard_profile.polarity) {
                return Err("drops an on/off word");
            }
            if !contains_all(&heard_profile.polarity, &c.polarity) {
                return Err("adds an on/off word");
            }
            if c.verbs.len() > heard_profile.verbs.len() && !(verb_may_be_added && no_verb) {
                return Err("adds an action");
            }
            let names_kept = heard
                .iter()
                .filter(|w| w.len() >= 3 && !is_stopword(w) && name_words.contains(*w))
                .all(|w| candidate.contains(w));
            if !names_kept {
                return Err("drops a name");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::words;

    fn whole(heard: &str, candidate: &str, verb: bool) -> Result<(), NotPreserved> {
        preserved(
            &words(heard),
            &words(candidate),
            &Edit::Whole,
            verb,
            &HashSet::new(),
        )
    }

    fn span(heard: &str, start: usize, end: usize, inserted: &str) -> Result<(), NotPreserved> {
        let edit = Edit::Span {
            start,
            end,
            inserted: words(inserted),
        };
        preserved(&words(heard), &[], &edit, false, &HashSet::new())
    }

    #[test]
    fn verbs_in_any_inflection() {
        for w in [
            "turned",
            "setting",
            "closes",
            "stopped",
            "plays",
            "watching",
            "cancelled",
        ] {
            assert!(verb(w).is_some(), "{w}");
        }
        assert!(verb("house").is_none());
    }

    #[test]
    fn whole_text_checks() {
        assert_eq!(
            whole("don't turn on the light", "turn on the light", false),
            Err("changes a negation")
        );
        assert_eq!(
            whole("set a 5 minute timer", "set a 6 minute timer", false),
            Err("changes a number")
        );
        assert_eq!(
            whole("set a five minute timer", "set a 5 minute timer", false),
            Ok(())
        );
        assert_eq!(
            whole("set a 5 minute timer", "set a 5 second timer", false),
            Err("changes a unit")
        );
        assert_eq!(
            whole("turn on the fan", "turn off the fan", false),
            Err("drops an on/off word")
        );
        assert_eq!(
            whole("close the blinds", "the blinds", false),
            Err("drops an action")
        );
        assert_eq!(whole("plate cherub rock", "play cherub rock", true), Ok(()));
        assert_eq!(
            whole("plate cherub rock", "play cherub rock", false),
            Err("adds an action")
        );
    }

    #[test]
    fn span_checks() {
        assert_eq!(span("turn on the flex", 3, 4, "Plex"), Ok(()));
        assert_eq!(
            span("turn on the light", 1, 2, "of"),
            Err("changes an on/off word")
        );
        assert_eq!(
            span("set 5 lights", 1, 2, "kitchen"),
            Err("changes a number")
        );
        assert_eq!(
            span("watch flex", 1, 2, "on lamp"),
            Err("adds an on/off word")
        );
    }
}
