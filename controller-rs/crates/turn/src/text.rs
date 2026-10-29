//! Text rules: transcript words, normalization, the wake-word check of the verifier, and
//! wake-phrase removal (post-AFE architecture §8.4 and §16.6 "Wake-phrase removal").
//!
//! No audio is ever trimmed. The wake phrase is removed from text only: from the final STT
//! transcript before it goes to HA, and from the streaming transcript before its words are
//! judged for completeness.

use serde::{Deserialize, Serialize};

use crate::time::Sample;

/// A window of 1–3 words matches the wake phrase when its distance is at most this (§16.6).
pub const PHRASE_MATCH_MAX: f64 = 0.45;
/// The verifier passes a transcript whose best substring is at most this far from the
/// registry's `verify_core` (§16.6 "Wake verification").
pub const VERIFY_PASS_MAX: f64 = 0.40;
/// The longest window, in words, that may hold the wake phrase.
const MAX_WINDOW_WORDS: usize = 3;
/// How far the final transcript's word count after the phrase may differ from the streaming
/// transcript's before the word-count fallback refuses to strip (§16.6 step 3).
const WORD_COUNT_SLACK: usize = 2;

/// One word of a transcript: a maximal run of letters, digits and apostrophes, as a byte range
/// of the original text. Everything else separates words (§16.6 `words(text)`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Word {
    /// Byte offset of the word's first character.
    pub start: usize,
    /// Byte offset just past the word's last character.
    pub end: usize,
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '\''
}

/// The words of `text`, in order. Each keeps its byte range in `text`, so a cut can keep the rest
/// of the original transcript verbatim.
pub fn words(text: &str) -> Vec<Word> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (is_word_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(Word { start: s, end: i });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(Word {
            start: s,
            end: text.len(),
        });
    }
    out
}

/// The normalization of every text the turn judges (the stable prefix, the pause server's
/// words, the questions to HA's sentence matcher and the committed text): the words of `text`
/// ([`words`]: runs of letters, digits and apostrophes), lowercased, without apostrophes at
/// their edges, joined with single spaces. Any other character separates words, inside a
/// word too ("three-minute" → "three minute", "5:30 p.m." → "5 30 p m"), as the deployed
/// controller's stable prefix and its questions to HA did in every field trace; inner
/// apostrophes stay ("don't").
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for w in words(text) {
        let word = text[w.start..w.end].trim_matches('\'');
        if word.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.extend(word.chars().flat_map(char::to_lowercase));
    }
    out
}

/// Whether a re-check of a committed span heard something other than the committed text
/// (§4.6, §4.7). The two are compared word by word, lowercased: the same words agree, and
/// so does one unfinished final word that the re-check completes ("turn off the lig" →
/// "turn off the lights"). Added, dropped or changed words, or a completed word that is
/// not the last, differ.
pub fn recheck_differs(committed: &str, rechecked: &str) -> bool {
    let lower = |text: &str| -> Vec<String> {
        words(text)
            .into_iter()
            .map(|w| text[w.start..w.end].to_lowercase())
            .collect()
    };
    let (before, after) = (lower(committed), lower(rechecked));
    if before == after {
        return false;
    }
    let (Some((last_before, head_before)), Some((last_after, head_after))) =
        (before.split_last(), after.split_last())
    else {
        return true;
    };
    !(head_before == head_after
        && last_after.len() > last_before.len()
        && last_after.starts_with(last_before.as_str()))
}

/// Levenshtein distance between two character sequences, with a reusable row buffer.
fn levenshtein(a: &[char], b: &[char], row: &mut Vec<usize>) -> usize {
    row.clear();
    row.extend(0..=b.len());
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = diagonal + usize::from(ca != cb);
            diagonal = row[j + 1];
            row[j + 1] = substitution.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

fn push_letters(out: &mut Vec<char>, s: &str) {
    for c in s.chars().filter(|c| c.is_alphabetic()) {
        out.extend(c.to_lowercase());
    }
}

/// How far a transcript is from the registry's verification core: the lowest normalized
/// Levenshtein distance between `core` and any substring of the transcript's lowercase letters
/// whose length is within one of the core's (§16.6 "Wake verification"). `None` when the
/// transcript has no substring of those lengths, which includes an empty transcript: that is a
/// failed verification, never a pass.
pub fn verification_distance(transcript: &str, core: &str) -> Option<f64> {
    let mut letters = Vec::new();
    push_letters(&mut letters, transcript);
    let mut core_chars = Vec::new();
    push_letters(&mut core_chars, core);
    if core_chars.is_empty() {
        return None;
    }
    let n = core_chars.len();
    let mut row = Vec::with_capacity(n + 2);
    let mut best: Option<usize> = None;
    for len in n.saturating_sub(1).max(1)..=n + 1 {
        if len > letters.len() {
            break;
        }
        for window in letters.windows(len) {
            let d = levenshtein(window, &core_chars, &mut row);
            best = Some(best.map_or(d, |b| b.min(d)));
        }
    }
    best.map(|d| d as f64 / n as f64)
}

/// Whether the verifier hears the wake word in `transcript`.
pub fn verification_passes(transcript: &str, core: &str) -> bool {
    verification_distance(transcript, core).is_some_and(|d| d <= VERIFY_PASS_MAX)
}

/// A window of consecutive words that minimally matches the wake phrase.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhraseWindow {
    /// Index of the window's first word.
    pub start: usize,
    /// Index just past the window's last word.
    pub end: usize,
    /// The window's distance to the wake phrase.
    pub distance: f64,
}

/// Scores word windows against one wake-phrase spelling.
#[derive(Debug)]
struct PhraseMatcher {
    phrase: Vec<char>,
    letters: Vec<char>,
    row: Vec<usize>,
}

impl PhraseMatcher {
    fn new(phrase: &str) -> Self {
        let mut chars = Vec::new();
        push_letters(&mut chars, phrase);
        Self {
            phrase: chars,
            letters: Vec::new(),
            row: Vec::new(),
        }
    }

    /// The distance of words `[start, end)`: their letters joined, Levenshtein distance to the
    /// phrase, divided by the longer of the two lengths. An empty window is at distance 1.
    fn distance(&mut self, text: &str, ws: &[Word], start: usize, end: usize) -> f64 {
        self.letters.clear();
        for w in &ws[start..end] {
            push_letters(&mut self.letters, &text[w.start..w.end]);
        }
        let longest = self.letters.len().max(self.phrase.len());
        if longest == 0 {
            return 1.0;
        }
        levenshtein(&self.letters, &self.phrase, &mut self.row) as f64 / longest as f64
    }

    fn matches(&mut self, text: &str, ws: &[Word], start: usize, end: usize) -> bool {
        start < end && self.distance(text, ws, start, end) <= PHRASE_MATCH_MAX
    }

    /// Every minimal window: it matches, and neither the window without its first word nor the
    /// window without its last word matches.
    fn minimal_windows(&mut self, text: &str, ws: &[Word]) -> Vec<PhraseWindow> {
        let mut out = Vec::new();
        for start in 0..ws.len() {
            for len in 1..=MAX_WINDOW_WORDS.min(ws.len() - start) {
                let end = start + len;
                let distance = self.distance(text, ws, start, end);
                if distance > PHRASE_MATCH_MAX {
                    continue;
                }
                if self.matches(text, ws, start + 1, end) || self.matches(text, ws, start, end - 1)
                {
                    continue;
                }
                out.push(PhraseWindow {
                    start,
                    end,
                    distance,
                });
            }
        }
        out
    }
}

/// A streaming transcript with the emission time of every word's last token.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimedTranscript {
    text: String,
    words: Vec<Word>,
    last_emitted: Vec<Sample>,
}

impl TimedTranscript {
    /// Builds the transcript from recognizer tokens in order. Tokens are concatenated as the
    /// recognizer spells them (a leading space starts a new word), and each word takes the
    /// emission sample of the token that holds its last character.
    pub fn from_tokens<'a>(tokens: impl IntoIterator<Item = (&'a str, Sample)>) -> Self {
        let mut text = String::new();
        // (byte offset where the token starts, its emission sample)
        let mut starts: Vec<(usize, Sample)> = Vec::new();
        for (t, emitted) in tokens {
            starts.push((text.len(), emitted));
            text.push_str(t);
        }
        let ws = words(&text);
        let last_emitted = ws
            .iter()
            .map(|w| {
                let last_byte = w.end - 1;
                let i = starts.partition_point(|(s, _)| *s <= last_byte);
                starts[i - 1].1
            })
            .collect();
        Self {
            text,
            words: ws,
            last_emitted,
        }
    }

    /// The transcript text as the recognizer spelled it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The transcript's words.
    pub fn words(&self) -> &[Word] {
        &self.words
    }

    /// When each word's last token was emitted.
    pub fn last_emitted(&self) -> &[Sample] {
        &self.last_emitted
    }
}

/// Where step 2 found the wake phrase in the streaming transcript.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamingWake {
    /// The chosen window, if any matched in time.
    pub window: Option<PhraseWindow>,
    /// `s`: the window's first word, or 0 when none matched (the phrase is assumed to open the
    /// utterance, since the recognizer can drop a quiet wake word outright).
    pub position: usize,
    /// `nk`: the number of streaming words after the window, or all of them without one.
    pub words_after: usize,
}

/// Step 2: among minimal windows whose last token was emitted by `lookahead_end` (candidate open
/// + 480 ms), the lowest distance, then the earliest.
pub fn locate_in_streaming(
    transcript: &TimedTranscript,
    lookahead_end: Sample,
    phrase: &str,
) -> StreamingWake {
    let mut matcher = PhraseMatcher::new(phrase);
    let window = matcher
        .minimal_windows(&transcript.text, &transcript.words)
        .into_iter()
        .filter(|w| transcript.last_emitted[w.end - 1] <= lookahead_end)
        .min_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then(a.start.cmp(&b.start))
        });
    StreamingWake {
        window,
        position: window.map_or(0, |w| w.start),
        words_after: window.map_or(transcript.words.len(), |w| transcript.words.len() - w.end),
    }
}

/// How step 3 chose what to remove from the final transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StripRule {
    /// A window starting at most one word after the streaming position.
    Near,
    /// The word-count fallback: the number of words after the window agrees with the streaming
    /// transcript's to within two.
    WordCount,
    /// Nothing chosen; the transcript is sent unchanged.
    Unchanged,
}

/// The final transcript after wake-phrase removal.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stripped<'a> {
    /// What is sent on: the original transcript after the removed part, verbatim.
    pub text: &'a str,
    /// The removed window, in the final transcript's word indices.
    pub window: Option<PhraseWindow>,
    /// How it was chosen.
    pub rule: StripRule,
}

/// The byte offset where the kept text starts: just after word `last`, plus any punctuation and
/// whitespace that follow it.
fn keep_from(text: &str, last: Word) -> usize {
    text[last.end..]
        .char_indices()
        .find(|(_, c)| c.is_alphanumeric())
        .map_or(text.len(), |(i, _)| last.end + i)
}

/// Step 3: removes the wake phrase, and anything said before it, from the final transcript.
pub fn strip_final<'a>(
    final_text: &'a str,
    streaming: &StreamingWake,
    phrase: &str,
) -> Stripped<'a> {
    let ws = words(final_text);
    let mut matcher = PhraseMatcher::new(phrase);
    let windows = matcher.minimal_windows(final_text, &ws);
    let s = streaming.position;
    let near = windows
        .iter()
        .filter(|w| w.start <= s + 1)
        .min_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then(a.start.abs_diff(s).cmp(&b.start.abs_diff(s)))
                .then(a.start.cmp(&b.start))
        })
        .copied();
    let (window, rule) = match near {
        Some(w) => (Some(w), StripRule::Near),
        None => {
            let nk = streaming.words_after;
            let by_count = windows
                .iter()
                .map(|w| (w, (ws.len() - w.end).abs_diff(nk)))
                .min_by(|(a, da), (b, db)| {
                    da.cmp(db)
                        .then(a.start.cmp(&b.start))
                        .then(a.distance.total_cmp(&b.distance))
                })
                .filter(|(_, d)| *d <= WORD_COUNT_SLACK)
                .map(|(w, _)| *w);
            match by_count {
                Some(w) => (Some(w), StripRule::WordCount),
                None => (None, StripRule::Unchanged),
            }
        }
    };
    let text = match window {
        Some(w) => &final_text[keep_from(final_text, ws[w.end - 1])..],
        None => final_text,
    };
    Stripped { text, window, rule }
}

/// Step 4: the streaming transcript cut through the end of step 2's window, then normalized.
/// Without a window nothing is cut.
pub fn cut_streaming(transcript: &TimedTranscript, wake: &StreamingWake) -> String {
    match wake.window {
        Some(w) => {
            let from = keep_from(&transcript.text, transcript.words[w.end - 1]);
            normalize(&transcript.text[from..])
        }
        None => normalize(&transcript.text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHRASE: &str = "ophelia";

    #[test]
    fn a_recheck_may_only_complete_the_final_word() {
        assert!(!recheck_differs("what is the time", "What is the time?"));
        assert!(!recheck_differs("turn off the lig", "turn off the lights"));
        assert!(recheck_differs("turn off the lights", "turn off the lig"));
        assert!(recheck_differs("turn of the lights", "turn off the lights"));
        assert!(recheck_differs(
            "what is the time",
            "what is the time in paris"
        ));
        assert!(recheck_differs("what is the time", "what is time"));
        assert!(recheck_differs("", "stop"));
        assert!(recheck_differs("stop", ""));
    }

    /// A streaming transcript whose words were all emitted at `at`, built from plain text.
    fn streamed(text: &str, at: u64) -> TimedTranscript {
        let tokens: Vec<String> = text.split(' ').map(|w| format!(" {w}")).collect();
        TimedTranscript::from_tokens(tokens.iter().map(|t| (t.as_str(), Sample(at))))
    }

    #[test]
    fn words_keep_byte_ranges_and_apostrophes() {
        let t = "Ophelia, what's  the time?";
        let ws = words(t);
        let texts: Vec<&str> = ws.iter().map(|w| &t[w.start..w.end]).collect();
        assert_eq!(texts, ["Ophelia", "what's", "the", "time"]);
        assert!(words("").is_empty());
        assert!(words(" ,.;").is_empty());
    }

    #[test]
    fn normalize_splits_words_at_punctuation() {
        assert_eq!(normalize("  Stop,  please. "), "stop please");
        assert_eq!(normalize("Don't stop!"), "don't stop");
        assert_eq!(
            normalize("set a timer for 5 minutes."),
            "set a timer for 5 minutes"
        );
        assert_eq!(normalize(" . , "), "");
        // Field traces: Kroko's and the server's punctuation inside a word separates it.
        assert_eq!(
            normalize("Set a three-minute timer."),
            "set a three minute timer"
        );
        assert_eq!(
            normalize("what's the temperature at 5:30 p.m."),
            "what's the temperature at 5 30 p m"
        );
        assert_eq!(normalize("'Cause it's 'quoted'"), "cause it's quoted");
    }

    #[test]
    fn verification_matches_the_spec_renderings() {
        for heard in [
            "Ophili", "Pophili", "Lophili", "Aphili", "Apheli", "Nopheli", "Ophi", "Ophilia",
            "Ophelia,",
        ] {
            assert!(verification_passes(heard, "ophel"), "{heard} should pass");
        }
        for heard in ["Eight", "I need to", "fear", "", "After"] {
            assert!(!verification_passes(heard, "ophel"), "{heard} should fail");
        }
    }

    #[test]
    fn verification_distance_needs_a_substring_of_the_right_length() {
        assert_eq!(verification_distance("", "ophel"), None);
        assert_eq!(verification_distance("oph", "ophel"), None);
        assert_eq!(verification_distance("ophel", "ophel"), Some(0.0));
        // "ophi" is a four-letter substring two edits from "ophel": 2 / 5.
        assert_eq!(verification_distance("ophi", "ophel"), Some(0.4));
    }

    #[test]
    fn final_transcript_strips_the_leading_phrase_and_keeps_later_mentions() {
        let wake = locate_in_streaming(
            &streamed("Ophelia what does Ophelia mean", 0),
            Sample(100),
            PHRASE,
        );
        assert_eq!(wake.position, 0);
        assert_eq!(wake.words_after, 4);
        let s = strip_final("Ophelia, what does Ophelia mean?", &wake, PHRASE);
        assert_eq!(s.text, "what does Ophelia mean?");
        assert_eq!(s.rule, StripRule::Near);
        // STT dropped the leading wake word: the match at index 2 is past s + 1 and has one word
        // after it where the stream had four, so nothing is removed.
        let s = strip_final("what does Ophelia mean", &wake, PHRASE);
        assert_eq!(s.text, "what does Ophelia mean");
        assert_eq!(s.rule, StripRule::Unchanged);
    }

    #[test]
    fn word_count_fallback_removes_a_preamble_the_stream_dropped() {
        // Turn 552: Kroko heard "Ophilia. What time is it?" (s = 0, nk = 4) and HA heard speech
        // before the wake word.
        let wake =
            locate_in_streaming(&streamed("Ophilia. What time is it?", 0), Sample(0), PHRASE);
        assert_eq!((wake.position, wake.words_after), (0, 4));
        let s = strip_final("I gotta go faster. Ophelia What time is it?", &wake, PHRASE);
        assert_eq!(s.text, "What time is it?");
        assert_eq!(s.rule, StripRule::WordCount);
    }

    #[test]
    fn without_a_streaming_window_the_phrase_is_assumed_first() {
        let wake = locate_in_streaming(&streamed("Turn off the lights.", 0), Sample(0), PHRASE);
        assert_eq!(wake.window, None);
        assert_eq!((wake.position, wake.words_after), (0, 4));
        assert_eq!(
            strip_final("Ophelia, what's the weather?", &wake, PHRASE).text,
            "what's the weather?"
        );
        assert_eq!(
            strip_final("Damn it, Ophelia, turn off the lights.", &wake, PHRASE).text,
            "turn off the lights."
        );
        assert_eq!(
            strip_final("play some opera music", &wake, PHRASE).text,
            "play some opera music"
        );
        // A transcript that is only the misheard wake word strips to nothing.
        assert_eq!(strip_final("Sophia.", &wake, PHRASE).text, "");
    }

    #[test]
    fn windows_are_minimal() {
        // "O Phelia" matches as two words, but "Phelia" alone matches too, so the minimal window
        // is that one word.
        let wake = locate_in_streaming(&streamed("O Phelia stop", 0), Sample(0), PHRASE);
        let w = wake.window.expect("matched");
        assert_eq!((w.start, w.end), (1, 2));
        assert_eq!(wake.words_after, 1);
        let wake = locate_in_streaming(&streamed("Oh, feel ya. Stop.", 0), Sample(0), PHRASE);
        assert_eq!(wake.window, None, "spec: not matched");
    }

    #[test]
    fn streaming_window_must_be_emitted_within_the_lookahead() {
        let tokens = [
            (" turn", Sample(1_000)),
            (" off", Sample(2_000)),
            (" O", Sample(3_000)),
            ("phelia", Sample(9_000)),
        ];
        let t = TimedTranscript::from_tokens(tokens);
        assert_eq!(
            t.last_emitted(),
            &[Sample(1_000), Sample(2_000), Sample(9_000)]
        );
        assert_eq!(locate_in_streaming(&t, Sample(8_999), PHRASE).window, None);
        let w = locate_in_streaming(&t, Sample(9_000), PHRASE)
            .window
            .unwrap();
        assert_eq!((w.start, w.end), (2, 3));
    }

    #[test]
    fn streaming_cut_keeps_the_command_normalized() {
        let t = streamed("So, Ophilia. Set a timer for five minutes.", 0);
        let wake = locate_in_streaming(&t, Sample(0), PHRASE);
        assert_eq!(cut_streaming(&t, &wake), "set a timer for five minutes");
        let t = streamed("Set a timer.", 0);
        let wake = locate_in_streaming(&t, Sample(0), PHRASE);
        assert_eq!(cut_streaming(&t, &wake), "set a timer");
        let t = streamed("Ophilia", 0);
        let wake = locate_in_streaming(&t, Sample(0), PHRASE);
        assert_eq!(cut_streaming(&t, &wake), "");
    }

    #[test]
    fn ties_prefer_the_lowest_distance_then_the_earliest() {
        let t = streamed("Ophelia Ophelia stop", 0);
        let wake = locate_in_streaming(&t, Sample(0), PHRASE);
        assert_eq!(wake.window.map(|w| w.start), Some(0));
        let t = streamed("Ofelia Ophelia stop", 0);
        let wake = locate_in_streaming(&t, Sample(0), PHRASE);
        assert_eq!(wake.window.map(|w| w.start), Some(1));
        // Step 3 prefers the window closest to s among equal distances.
        let s = strip_final(
            "Ophelia Ophelia stop",
            &StreamingWake {
                window: None,
                position: 1,
                words_after: 1,
            },
            PHRASE,
        );
        assert_eq!(s.window.map(|w| w.start), Some(1));
        assert_eq!(s.text, "stop");
    }
}
