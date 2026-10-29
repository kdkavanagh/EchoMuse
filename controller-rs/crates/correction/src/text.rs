//! The canonical form of a transcript every rule reads, and number words.

/// A transcript's words: lowercase, punctuation at word edges removed, hyphens and dashes
/// between words made spaces, curly apostrophes made straight. Punctuation inside a word
/// is kept (`don't`, `p.m.`, `5:30`), and so is a trailing `%`.
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let text = text.replace(['\u{2019}', '\u{2018}'], "'");
    let separator = |c: char| c.is_whitespace() || matches!(c, '-' | '\u{2013}' | '\u{2014}' | '/');
    for raw in text.split(separator) {
        let lower: String = raw.chars().flat_map(char::to_lowercase).collect();
        let trimmed = lower
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '%')
            .trim_start_matches('%');
        // A dotted abbreviation keeps its last dot ("p.m.").
        let tail = lower.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '.');
        let word = if trimmed.contains('.') && tail.ends_with('.') {
            format!("{trimmed}.")
        } else {
            trimmed.to_owned()
        };
        if word.chars().any(char::is_alphanumeric) {
            out.push(word);
        }
    }
    out
}

/// The letters and digits of `text`, lowercased: two spellings with the same key are the
/// same words ("YouTube TV" and "youtube tv").
pub fn letters_key(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// How many letters and digits `text` has.
pub fn letter_count(text: &str) -> usize {
    text.chars().filter(|c| c.is_alphanumeric()).count()
}

/// Words that carry no content on their own: a span to replace may not start or end with
/// one, and two texts sharing only these share nothing.
pub const STOPWORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "in", "at", "for", "my", "your", "our", "is", "it", "its",
    "it's", "and", "or", "me", "i", "please", "with", "from", "by", "this", "that", "be", "are",
    "was", "can", "could", "would", "will", "you", "what", "what's", "how", "do", "does", "some",
    "all", "any", "there", "here", "hey", "um", "uh", "okay", "ok", "so", "just", "now",
];

/// Whether `word` is a stopword.
pub fn is_stopword(word: &str) -> bool {
    STOPWORDS.contains(&word)
}

const UNITS: [&str; 20] = [
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
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
const TENS: [&str; 10] = [
    "", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
const ORDINAL_UNITS: [&str; 20] = [
    "zeroth",
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "sixth",
    "seventh",
    "eighth",
    "ninth",
    "tenth",
    "eleventh",
    "twelfth",
    "thirteenth",
    "fourteenth",
    "fifteenth",
    "sixteenth",
    "seventeenth",
    "eighteenth",
    "nineteenth",
];
const ORDINAL_TENS: [&str; 10] = [
    "",
    "",
    "twentieth",
    "thirtieth",
    "fortieth",
    "fiftieth",
    "sixtieth",
    "seventieth",
    "eightieth",
    "ninetieth",
];

/// The largest number read aloud or parsed from words.
pub const MAX_NUMBER: u32 = 9999;

/// `n` (0 to 9999) in words: 42 → "forty two".
pub fn cardinal_words(n: u32) -> Option<String> {
    if n > MAX_NUMBER {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    let thousands = n / 1000;
    let hundreds = n / 100 % 10;
    let rest = n % 100;
    if thousands > 0 {
        parts.extend([UNITS[thousands as usize], "thousand"]);
    }
    if hundreds > 0 {
        parts.extend([UNITS[hundreds as usize], "hundred"]);
    }
    if rest > 0 || n == 0 {
        if rest < 20 {
            parts.push(UNITS[rest as usize]);
        } else {
            parts.push(TENS[(rest / 10) as usize]);
            if !rest.is_multiple_of(10) {
                parts.push(UNITS[(rest % 10) as usize]);
            }
        }
    }
    Some(parts.join(" "))
}

/// `n` (0 to 9999) as an ordinal in words: 23 → "twenty third".
pub fn ordinal_words(n: u32) -> Option<String> {
    let cardinal = cardinal_words(n)?;
    let rest = n % 100;
    let last = if rest == 0 && n > 0 {
        return Some(format!("{cardinal}th"));
    } else if rest < 20 {
        ORDINAL_UNITS[rest as usize]
    } else if rest.is_multiple_of(10) {
        ORDINAL_TENS[(rest / 10) as usize]
    } else {
        ORDINAL_UNITS[(rest % 10) as usize]
    };
    let mut words: Vec<&str> = cardinal.split(' ').collect();
    words.pop();
    words.push(last);
    Some(words.join(" "))
}

/// The value of a digit token with an ordinal suffix ("3rd").
pub fn ordinal_digits(word: &str) -> Option<u32> {
    let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let suffix = &word[digits.len()..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    matches!(suffix, "st" | "nd" | "rd" | "th")
        .then(|| digits.parse().ok())
        .flatten()
}

fn unit_value(word: &str) -> Option<u32> {
    UNITS.iter().position(|w| *w == word).map(|i| i as u32)
}

fn tens_value(word: &str) -> Option<u32> {
    TENS.iter()
        .position(|w| !w.is_empty() && *w == word)
        .map(|i| i as u32 * 10)
}

/// The value of an ordinal word ("third", "twentieth").
pub fn ordinal_word_value(word: &str) -> Option<u32> {
    ORDINAL_UNITS
        .iter()
        .position(|w| *w == word)
        .map(|i| i as u32)
        .or_else(|| {
            ORDINAL_TENS
                .iter()
                .position(|w| !w.is_empty() && *w == word)
                .map(|i| i as u32 * 10)
        })
}

/// One run of number words: `words[start..end]` say `value`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NumberRun {
    /// The first word.
    pub start: usize,
    /// One past the last word.
    pub end: usize,
    /// The number said.
    pub value: u32,
}

/// The runs of cardinal number words in `words`, longest reading first at each position:
/// "twenty five" is 25, "two hundred and five" is 205, "a hundred" is 100. Ordinal words
/// and digits are not runs.
pub fn number_runs(words: &[String]) -> Vec<NumberRun> {
    let mut runs = Vec::new();
    let mut i = 0;
    while i < words.len() {
        match parse_number(words, i) {
            Some((end, value)) => {
                runs.push(NumberRun {
                    start: i,
                    end,
                    value,
                });
                i = end;
            }
            None => i += 1,
        }
    }
    runs
}

/// Below 100 at `i`: "twenty", "twenty five", "seven".
fn below_hundred(words: &[String], i: usize) -> Option<(usize, u32)> {
    let word = words.get(i)?;
    if let Some(tens) = tens_value(word) {
        if let Some(unit) = words.get(i + 1).and_then(|w| unit_value(w))
            && (1..10).contains(&unit)
        {
            return Some((i + 2, tens + unit));
        }
        return Some((i + 1, tens));
    }
    unit_value(word).map(|v| (i + 1, v))
}

fn parse_number(words: &[String], start: usize) -> Option<(usize, u32)> {
    let is = |i: usize, word: &str| words.get(i).is_some_and(|w| w == word);
    let mut total = 0;
    // "a hundred", "a thousand"
    let (mut end, mut value) =
        if is(start, "a") && (is(start + 1, "hundred") || is(start + 1, "thousand")) {
            (start + 1, 1)
        } else {
            below_hundred(words, start)?
        };
    if is(end, "thousand") && (1..10).contains(&value) {
        total += value * 1000;
        let next = end + 1;
        match below_hundred(words, next) {
            Some((e, v)) => (end, value) = (e, v),
            None => return Some((next, total)),
        }
    }
    if is(end, "hundred") && (1..10).contains(&value) {
        total += value * 100;
        let mut next = end + 1;
        if is(next, "and") && below_hundred(words, next + 1).is_some() {
            next += 1;
        }
        return Some(match below_hundred(words, next) {
            Some((e, v)) => (e, total + v),
            None => (next, total),
        });
    }
    Some((end, total + value))
}

/// Every number `words` say, by value: digits, digit ordinals, number words, ordinal words,
/// and each part of a clock time ("5:30" is 5 and 30). Sorted.
pub fn number_values(words: &[String]) -> Vec<u32> {
    let mut values = Vec::new();
    let runs = number_runs(words);
    let mut in_run = vec![false; words.len()];
    for run in &runs {
        values.push(run.value);
        in_run[run.start..run.end]
            .iter_mut()
            .for_each(|b| *b = true);
    }
    for (i, word) in words.iter().enumerate() {
        if in_run[i] {
            continue;
        }
        let bare = word.trim_end_matches('%');
        // "second" after a number is the unit ("a 5 second timer"), not the ordinal.
        let after_number =
            i > 0 && (in_run[i - 1] || words[i - 1].bytes().all(|b| b.is_ascii_digit()));
        let ordinal = ordinal_digits(bare)
            .or_else(|| ordinal_word_value(bare).filter(|_| !(bare == "second" && after_number)));
        if let Some(v) = ordinal {
            values.push(v);
            continue;
        }
        for part in bare.split([':', '.', ',']) {
            if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
                values.push(part.parse().unwrap_or(u32::MAX));
            }
        }
    }
    values.sort_unstable();
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str) -> Vec<String> {
        words(text)
    }

    #[test]
    fn canonical_words() {
        assert_eq!(w("What's the weather?"), ["what's", "the", "weather"]);
        assert_eq!(
            w("set a 30-minute timer."),
            ["set", "a", "30", "minute", "timer"]
        );
        assert_eq!(w("at 5:30 p.m."), ["at", "5:30", "p.m."]);
        assert_eq!(w("Don’t  stop, 50%!"), ["don't", "stop", "50%"]);
        assert_eq!(w(" -- "), Vec::<String>::new());
    }

    #[test]
    fn numbers_in_words() {
        assert_eq!(cardinal_words(0).unwrap(), "zero");
        assert_eq!(cardinal_words(42).unwrap(), "forty two");
        assert_eq!(
            cardinal_words(1905).unwrap(),
            "one thousand nine hundred five"
        );
        assert_eq!(ordinal_words(23).unwrap(), "twenty third");
        assert_eq!(ordinal_words(30).unwrap(), "thirtieth");
        assert_eq!(ordinal_words(100).unwrap(), "one hundredth");
        assert_eq!(ordinal_digits("3rd"), Some(3));
        assert_eq!(ordinal_digits("rd"), None);
    }

    #[test]
    fn number_word_runs() {
        let runs = number_runs(&w("set a twenty five minute timer for one"));
        assert_eq!(runs.iter().map(|r| r.value).collect::<Vec<_>>(), [25, 1]);
        assert_eq!(number_runs(&w("two hundred and five"))[0].value, 205);
        assert_eq!(number_runs(&w("a hundred"))[0].value, 100);
        assert_eq!(number_runs(&w("three thousand four"))[0].value, 3004);
        assert!(number_runs(&w("a lamp")).is_empty());
    }

    #[test]
    fn values_compare_words_and_digits() {
        assert_eq!(number_values(&w("twenty five")), number_values(&w("25")));
        assert_eq!(number_values(&w("the third one")), [1, 3]);
        assert_eq!(number_values(&w("at 5:30")), [5, 30]);
        assert_eq!(number_values(&w("50%")), [50]);
    }
}
