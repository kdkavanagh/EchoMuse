//! Transcript normalization: one canonical spelling of what was said, used for the user's
//! words, the template words and the vocabulary alike, so that the same request written
//! differently compares equal.
//!
//! The steps, in order:
//!
//! 1. **Case.** Lowercase everything. Curly apostrophes become straight ones.
//! 2. **Punctuation.** Every character other than a letter, a digit or an apostrophe becomes a
//!    space, with these exceptions: a `.` or `/` between two digits stays (`21.5`, `1/2`); a
//!    `,` between two digits is dropped (`1,000`); a `:` between two digits splits a clock
//!    time into hour and minute (`7:30` → `7 30`); `%` becomes `percent`, `°` becomes
//!    `degrees` and `&` becomes `and`. Hyphens separate words (`ten-second` → `10 second`).
//!    Apostrophes at the edge of a word are dropped.
//! 3. **Clock words.** `7am` and `730pm` split into the number and `am`/`pm`; `a m` and `p m`
//!    after a number (what `a.m.` and `p.m.` become) join into `am` and `pm`; `oclock` and
//!    `o clock` become `o'clock`.
//! 4. **Contractions.** Pronoun and question-word contractions are written out (`what's` →
//!    `what is`, `when's` → `when is`, `i'm` → `i am`), as are the forms speech recognizers
//!    write without the apostrophe (`whats`, `dont`); `n't`, `'re`, `'ve`, `'ll`, `'d` and
//!    `'m` are expanded on any word. A possessive (`today's`) is left alone.
//! 5. **Numbers.** Number words become digits: `ten` → `10`, `twenty five` → `25`,
//!    `one hundred and five` → `105`, `a hundred` → `100`. Two numbers said one after the
//!    other stay apart (`seven thirty` → `7 30`). Ordinal words become digits with their
//!    suffix (`first` → `1st`, `twenty third` → `23rd`), except a bare `second`, which is far
//!    more often the unit of time. `oh` is never a number here; the alarm grammar reads
//!    `seven oh five` itself.
//!
//! Skip words (`please`, `can you`, …) are not part of normalization: they belong to the
//! template data and are removed by the matcher.

use std::fmt;

/// A normalized transcript: its words in order.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct Normalized {
    tokens: Vec<String>,
}

impl Normalized {
    /// The words.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// Whether nothing is left.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    pub(crate) fn into_tokens(self) -> Vec<String> {
        self.tokens
    }
}

impl fmt::Display for Normalized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.tokens.join(" "))
    }
}

/// Normalizes `text` (see the module documentation for the rules).
pub fn normalize(text: &str) -> Normalized {
    let prepared = prepare(text);
    let words: Vec<String> = prepared
        .split_whitespace()
        .map(|w| w.trim_matches('\'').to_owned())
        .filter(|w| !w.is_empty())
        .collect();
    let words = clock_words(words);
    let words = contractions(words);
    let words = numbers(words);
    Normalized {
        tokens: join_meridiem(words),
    }
}

/// Steps 1 and 2: lowercase and punctuation.
fn prepare(text: &str) -> String {
    let chars: Vec<char> = text
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{02bc}' | '`' => '\'',
            other => other,
        })
        .collect();
    let mut out = String::with_capacity(chars.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let between_digits = i > 0
            && chars[i - 1].is_ascii_digit()
            && chars.get(i + 1).is_some_and(char::is_ascii_digit);
        match c {
            c if c.is_alphanumeric() || c == '\'' => out.push(c),
            '.' | '/' if between_digits => out.push(c),
            ',' if between_digits => {}
            '%' => out.push_str(" percent "),
            '°' => out.push_str(" degrees "),
            '&' => out.push_str(" and "),
            _ => out.push(' '),
        }
    }
    out
}

/// Step 3, the parts that work on single words: `7am`, `oclock`.
fn clock_words(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let w = &words[i];
        if let Some((digits, suffix)) = split_meridiem_suffix(w) {
            out.push(digits.to_owned());
            out.push(suffix.to_owned());
        } else if w == "oclock" || w == "o'clock" {
            out.push("o'clock".to_owned());
        } else if w == "o" && words.get(i + 1).is_some_and(|n| n == "clock") {
            out.push("o'clock".to_owned());
            i += 1;
        } else {
            out.push(w.clone());
        }
        i += 1;
    }
    out
}

/// `7am` → `7` `am`, `5500k` → `5500` `k`: digits followed by letters other than an ordinal
/// suffix (`3rd` stays one word).
fn split_meridiem_suffix(word: &str) -> Option<(&str, &str)> {
    let split = word.find(|c: char| !c.is_ascii_digit())?;
    let (digits, suffix) = word.split_at(split);
    let ordinal = matches!(suffix, "st" | "nd" | "rd" | "th");
    (!digits.is_empty() && !ordinal && suffix.chars().all(char::is_alphabetic))
        .then_some((digits, suffix))
}

/// The rest of step 3: `a m` / `p m` after a number or `o'clock`.
fn join_meridiem(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        let after_time = out
            .last()
            .is_some_and(|prev| is_digits(prev) || prev == "o'clock");
        if after_time
            && (words[i] == "a" || words[i] == "p")
            && words.get(i + 1).is_some_and(|n| n == "m")
        {
            out.push(format!("{}m", words[i]));
            i += 2;
        } else {
            out.push(words[i].clone());
            i += 1;
        }
    }
    out
}

pub(crate) fn is_digits(word: &str) -> bool {
    !word.is_empty() && word.bytes().all(|b| b.is_ascii_digit())
}

/// Contractions written out in full, including spellings without the apostrophe.
const CONTRACTIONS: &[(&str, &str)] = &[
    ("what's", "what is"),
    ("whats", "what is"),
    ("that's", "that is"),
    ("thats", "that is"),
    ("it's", "it is"),
    ("where's", "where is"),
    ("wheres", "where is"),
    ("when's", "when is"),
    ("whens", "when is"),
    ("who's", "who is"),
    ("whos", "who is"),
    ("how's", "how is"),
    ("hows", "how is"),
    ("there's", "there is"),
    ("theres", "there is"),
    ("here's", "here is"),
    ("heres", "here is"),
    ("he's", "he is"),
    ("she's", "she is"),
    ("let's", "let us"),
    ("won't", "will not"),
    ("can't", "can not"),
    ("cant", "can not"),
    ("cannot", "can not"),
    ("ain't", "is not"),
    ("shan't", "shall not"),
    ("dont", "do not"),
    ("doesnt", "does not"),
    ("didnt", "did not"),
    ("isnt", "is not"),
    ("arent", "are not"),
    ("wasnt", "was not"),
    ("werent", "were not"),
    ("havent", "have not"),
    ("hasnt", "has not"),
    ("wouldnt", "would not"),
    ("couldnt", "could not"),
    ("shouldnt", "should not"),
];

/// Contraction endings expanded on any word.
const SUFFIXES: &[(&str, &str)] = &[
    ("n't", "not"),
    ("'re", "are"),
    ("'ve", "have"),
    ("'ll", "will"),
    ("'d", "would"),
    ("'m", "am"),
];

/// Step 4.
fn contractions(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len() + 4);
    for word in words {
        if let Some((_, full)) = CONTRACTIONS.iter().find(|(short, _)| *short == word) {
            out.extend(full.split(' ').map(str::to_owned));
            continue;
        }
        match SUFFIXES
            .iter()
            .find_map(|(suffix, full)| word.strip_suffix(suffix).map(|stem| (stem, *full)))
        {
            Some((stem, full)) if !stem.is_empty() => {
                out.push(stem.to_owned());
                out.push(full.to_owned());
            }
            _ => out.push(word),
        }
    }
    out
}

const UNITS: [&str; 10] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];
const TEENS: [&str; 10] = [
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];
const TENS: [&str; 8] = [
    "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
/// Ordinal words 1–19 (`second` is handled apart).
const ORDINAL_SMALL: [(&str, u32); 18] = [
    ("first", 1),
    ("third", 3),
    ("fourth", 4),
    ("fifth", 5),
    ("sixth", 6),
    ("seventh", 7),
    ("eighth", 8),
    ("ninth", 9),
    ("tenth", 10),
    ("eleventh", 11),
    ("twelfth", 12),
    ("thirteenth", 13),
    ("fourteenth", 14),
    ("fifteenth", 15),
    ("sixteenth", 16),
    ("seventeenth", 17),
    ("eighteenth", 18),
    ("nineteenth", 19),
];
const ORDINAL_TENS: [(&str, u32); 8] = [
    ("twentieth", 20),
    ("thirtieth", 30),
    ("fortieth", 40),
    ("fiftieth", 50),
    ("sixtieth", 60),
    ("seventieth", 70),
    ("eightieth", 80),
    ("ninetieth", 90),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NumberWord {
    Unit(u32),
    Teen(u32),
    Tens(u32),
    Hundred,
    Thousand,
    Ordinal(u32),
}

fn number_word(word: &str) -> Option<NumberWord> {
    if let Some(i) = UNITS.iter().position(|w| *w == word) {
        return Some(NumberWord::Unit(i as u32));
    }
    if let Some(i) = TEENS.iter().position(|w| *w == word) {
        return Some(NumberWord::Teen(10 + i as u32));
    }
    if let Some(i) = TENS.iter().position(|w| *w == word) {
        return Some(NumberWord::Tens(20 + 10 * i as u32));
    }
    match word {
        "hundred" => return Some(NumberWord::Hundred),
        "thousand" => return Some(NumberWord::Thousand),
        _ => {}
    }
    ORDINAL_SMALL
        .iter()
        .chain(ORDINAL_TENS.iter())
        .find(|(w, _)| *w == word)
        .map(|(_, n)| NumberWord::Ordinal(*n))
}

/// The state of a number being read: what the last word was.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Last {
    Start,
    Unit,
    Teen,
    Tens,
    Hundred,
    Thousand,
}

/// Reads the longest number starting at `words[start]`; returns its text and the number of
/// words it used.
fn read_number(words: &[String], start: usize) -> Option<(String, usize)> {
    let mut total: u64 = 0;
    let mut current: u64 = 0;
    let mut last = Last::Start;
    let mut used = 0;
    let mut i = start;
    // "a hundred", "a thousand".
    if words[i] == "a"
        && words
            .get(i + 1)
            .is_some_and(|n| n == "hundred" || n == "thousand")
    {
        current = 1;
        last = Last::Unit;
        i += 1;
        used = 1;
    }
    while let Some(word) = words.get(i) {
        let kind = if word == "second" && last == Last::Tens {
            Some(NumberWord::Ordinal(2))
        } else if word == "and"
            && matches!(last, Last::Hundred | Last::Thousand)
            && words
                .get(i + 1)
                .and_then(|n| number_word(n))
                .is_some_and(|n| !matches!(n, NumberWord::Hundred | NumberWord::Thousand))
        {
            i += 1;
            used += 1;
            continue;
        } else {
            number_word(word)
        };
        let Some(kind) = kind else { break };
        let accepted = match kind {
            NumberWord::Unit(0) => {
                if last != Last::Start {
                    break;
                }
                last = Last::Unit;
                true
            }
            NumberWord::Unit(n) => {
                let fits = match last {
                    Last::Start | Last::Hundred | Last::Thousand => true,
                    Last::Tens => current.is_multiple_of(10),
                    Last::Unit | Last::Teen => false,
                };
                if fits {
                    current += u64::from(n);
                    last = Last::Unit;
                }
                fits
            }
            NumberWord::Teen(n) | NumberWord::Tens(n) => {
                let fits = matches!(last, Last::Start | Last::Hundred | Last::Thousand);
                if fits {
                    current += u64::from(n);
                    last = if matches!(kind, NumberWord::Teen(_)) {
                        Last::Teen
                    } else {
                        Last::Tens
                    };
                }
                fits
            }
            NumberWord::Hundred => {
                let fits = matches!(last, Last::Start | Last::Unit | Last::Teen | Last::Tens)
                    && !current.is_multiple_of(100)
                    || last == Last::Start;
                if fits {
                    current = current.max(1) * 100;
                    last = Last::Hundred;
                }
                fits
            }
            NumberWord::Thousand => {
                let fits = last != Last::Thousand;
                if fits {
                    total += current.max(1) * 1000;
                    current = 0;
                    last = Last::Thousand;
                }
                fits
            }
            NumberWord::Ordinal(n) => {
                let fits = match last {
                    Last::Start | Last::Hundred | Last::Thousand => true,
                    Last::Tens => n < 10 && current.is_multiple_of(10),
                    Last::Unit | Last::Teen => false,
                };
                if !fits {
                    break;
                }
                let value = total + current + u64::from(n);
                return Some((format!("{value}{}", ordinal_suffix(value)), used + 1));
            }
        };
        if !accepted {
            break;
        }
        used += 1;
        i += 1;
    }
    (used > 0 && last != Last::Start).then(|| ((total + current).to_string(), used))
}

fn ordinal_suffix(value: u64) -> &'static str {
    match (value % 10, value % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    }
}

/// Step 5.
fn numbers(words: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(words.len());
    let mut i = 0;
    while i < words.len() {
        match read_number(&words, i) {
            Some((text, used)) => {
                out.push(text);
                i += used;
            }
            None => {
                out.push(words[i].clone());
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::normalize;

    fn n(text: &str) -> String {
        normalize(text).to_string()
    }

    #[test]
    fn case_and_punctuation() {
        assert_eq!(n("What time is it?"), "what time is it");
        assert_eq!(n("  Stop.  "), "stop");
        assert_eq!(n("Turn OFF the TV!"), "turn off the tv");
        assert_eq!(
            n("set the living-room lights to 50%"),
            "set the living room lights to 50 percent"
        );
        assert_eq!(n("set it to 21.5°"), "set it to 21.5 degrees");
        assert_eq!(n("rock & roll"), "rock and roll");
        assert_eq!(n("'quoted' words"), "quoted words");
        assert_eq!(n("1,000 things"), "1000 things");
        assert_eq!(
            n("Ophilia, what's the weather?"),
            "ophilia what is the weather"
        );
    }

    #[test]
    fn clock_times() {
        assert_eq!(n("7:30 a.m."), "7 30 am");
        assert_eq!(n("7 p.m."), "7 pm");
        assert_eq!(n("7am"), "7 am");
        assert_eq!(n("1130PM"), "1130 pm");
        assert_eq!(n("seven o'clock"), "7 o'clock");
        assert_eq!(n("seven o clock"), "7 o'clock");
        assert_eq!(n("seven a m"), "7 am");
        assert_eq!(n("a m radio"), "a m radio");
        assert_eq!(n("seven oh five"), "7 oh 5");
        assert_eq!(n("12:05"), "12 05");
    }

    #[test]
    fn contractions_are_written_out() {
        assert_eq!(n("what's the news"), "what is the news");
        assert_eq!(n("whats the news"), "what is the news");
        assert_eq!(n("When’s my alarm"), "when is my alarm");
        assert_eq!(n("I'm here"), "i am here");
        assert_eq!(n("don't stop"), "do not stop");
        assert_eq!(n("dont stop"), "do not stop");
        assert_eq!(n("I'd like music"), "i would like music");
        assert_eq!(n("won't work"), "will not work");
        assert_eq!(n("we're home"), "we are home");
        assert_eq!(n("today's date"), "today's date");
    }

    #[test]
    fn number_words_become_digits() {
        assert_eq!(n("set a ten second timer"), "set a 10 second timer");
        assert_eq!(n("set a ten-second timer"), "set a 10 second timer");
        assert_eq!(n("twenty five minutes"), "25 minutes");
        assert_eq!(n("twenty-five minutes"), "25 minutes");
        assert_eq!(n("seven thirty"), "7 30");
        assert_eq!(n("nine forty five"), "9 45");
        assert_eq!(n("one hundred and five"), "105");
        assert_eq!(n("a hundred"), "100");
        assert_eq!(n("two thousand twenty six"), "2026");
        assert_eq!(n("one two three"), "1 2 3");
        assert_eq!(n("zero"), "0");
        assert_eq!(n("a minute"), "a minute");
        assert_eq!(n("one and two"), "1 and 2");
    }

    #[test]
    fn ordinals() {
        assert_eq!(n("the first"), "the 1st");
        assert_eq!(n("october thirtieth"), "october 30th");
        assert_eq!(n("the twenty third"), "the 23rd");
        assert_eq!(n("the twenty second"), "the 22nd");
        assert_eq!(n("ten seconds"), "10 seconds");
        assert_eq!(n("wait a second"), "wait a second");
        assert_eq!(n("the 3rd"), "the 3rd");
        assert_eq!(n("the eleventh"), "the 11th");
    }

    #[test]
    fn normalizing_twice_changes_nothing() {
        for text in [
            "Set a ten-second timer.",
            "What's the weather in Detroit?",
            "set an alarm for 7:30 a.m. on October thirtieth",
            "Turn off the TV.",
        ] {
            let once = n(text);
            assert_eq!(n(&once), once, "{text}");
        }
    }
}
