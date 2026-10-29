//! Pronunciations: the CMU Pronouncing Dictionary, the user's guides, and the fallbacks for
//! words neither knows.
//!
//! The dictionary is the Carnegie Mellon Pronouncing Dictionary (CMUdict), copyright
//! Carnegie Mellon University, vendored under `data/cmudict` with its BSD-style licence.

use std::collections::HashMap;

use echomuse_settings::values::Pronunciations;
use serde::Serialize;

use crate::{
    sounds::{Segment, Sounds},
    spelling,
    text::{cardinal_words, ordinal_digits, ordinal_words},
};

const CMUDICT: &str = include_str!("../data/cmudict/cmudict.dict");

/// The most alternatives kept for one word.
pub const MAX_WORD_VARIANTS: usize = 2;
/// The most pronunciations of a whole text compared.
pub const MAX_TEXT_VARIANTS: usize = 4;
/// The longest word split into dictionary words.
const MAX_COMPOUND_LETTERS: usize = 30;
/// The most dictionary words one word is split into.
const MAX_COMPOUND_PIECES: usize = 3;

/// A pronouncing dictionary: each word's pronunciations, the most common first.
#[derive(Debug, Default)]
pub struct Lexicon {
    words: HashMap<Box<str>, Vec<Sounds>>,
}

impl Lexicon {
    /// The vendored CMU Pronouncing Dictionary. Parsing it takes a noticeable fraction of a
    /// second: do it once, off any device's thread (the table builder does), and share it.
    pub fn cmu() -> Self {
        Self::parse(CMUDICT)
    }

    /// Reads CMUdict's format: `word PHONEMES`, alternatives as `word(2) PHONEMES`, a `#`
    /// comment after the phonemes. Lines that do not parse are skipped.
    pub fn parse(text: &str) -> Self {
        let mut words: HashMap<Box<str>, Vec<Sounds>> = HashMap::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() || line.starts_with(";;;") {
                continue;
            }
            let Some((head, phonemes)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let word = match head.find('(') {
                Some(i) if head.ends_with(')') => &head[..i],
                _ => head,
            };
            let Some(sounds) = Sounds::parse_arpabet(phonemes) else {
                continue;
            };
            let entry = words
                .entry(word.to_lowercase().into_boxed_str())
                .or_default();
            if !entry.contains(&sounds) {
                entry.push(sounds);
            }
        }
        Self { words }
    }

    /// A word's pronunciations, the most common first.
    pub fn get(&self, word: &str) -> Option<&[Sounds]> {
        self.words.get(word).map(Vec::as_slice)
    }

    /// How many words it knows.
    pub fn len(&self) -> usize {
        self.words.len()
    }

    /// Whether it knows no word.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// The user's pronunciation guides (`transcriptFixPronunciations`): each replaces the
/// dictionary's pronunciation of its word.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Guides {
    words: HashMap<String, Sounds>,
}

impl Guides {
    /// The guides of a validated setting.
    pub fn new(pronunciations: &Pronunciations) -> Self {
        let words = pronunciations
            .entries()
            .iter()
            .filter_map(|p| Some((p.word().to_owned(), Sounds::parse_arpabet(p.phonemes())?)))
            .collect();
        Self { words }
    }

    /// The guide for a word.
    pub fn get(&self, word: &str) -> Option<&Sounds> {
        self.words.get(word)
    }

    /// The guided words.
    pub fn words(&self) -> impl Iterator<Item = &str> {
        self.words.keys().map(String::as_str)
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }
}

/// Where a word's pronunciation came from, most trusted first.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SoundSource {
    /// The user's guide.
    Guide,
    /// The CMU Pronouncing Dictionary.
    Dictionary,
    /// A number, ordinal or percent sign read as words.
    Number,
    /// A short token without vowels read letter by letter ("rmv").
    Letters,
    /// Dictionary words run together ("youtubetv": "youtube" + "tv").
    Compound,
    /// Guessed from the spelling.
    Spelling,
}

/// A word's pronunciations and where they came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WordSounds {
    /// The least trusted source any part came from.
    pub source: SoundSource,
    /// At most [`MAX_WORD_VARIANTS`], the most common first.
    pub variants: Vec<Sounds>,
}

const LETTER_NAMES: [&str; 26] = [
    "EY",
    "B IY",
    "S IY",
    "D IY",
    "IY",
    "EH F",
    "JH IY",
    "EY CH",
    "AY",
    "JH EY",
    "K EY",
    "EH L",
    "EH M",
    "EH N",
    "OW",
    "P IY",
    "K Y UW",
    "AA R",
    "EH S",
    "T IY",
    "Y UW",
    "V IY",
    "D AH B AH L Y UW",
    "EH K S",
    "W AY",
    "Z IY",
];

fn letter_name(letter: u8) -> Option<Sounds> {
    let i = letter.checked_sub(b'a')? as usize;
    Sounds::parse_arpabet(LETTER_NAMES.get(i)?)
}

/// Pronounces words: the user's guides first, then the dictionary, then numbers, letter
/// names, compounds of dictionary words, and finally a guess from the spelling.
#[derive(Clone, Copy, Debug)]
pub struct Pronouncer<'a> {
    lexicon: &'a Lexicon,
    guides: &'a Guides,
}

impl<'a> Pronouncer<'a> {
    /// A pronouncer reading `guides` before `lexicon`.
    pub fn new(lexicon: &'a Lexicon, guides: &'a Guides) -> Self {
        Self { lexicon, guides }
    }

    fn known(&self, word: &str) -> Option<(SoundSource, Vec<Sounds>)> {
        if let Some(guide) = self.guides.get(word) {
            return Some((SoundSource::Guide, vec![guide.clone()]));
        }
        let entries = self.lexicon.get(word)?;
        Some((
            SoundSource::Dictionary,
            entries.iter().take(MAX_WORD_VARIANTS).cloned().collect(),
        ))
    }

    /// One canonical word's pronunciations; `None` only for a word with no letters or digits
    /// or one too long to compare.
    pub fn word(&self, word: &str) -> Option<WordSounds> {
        if let Some((source, variants)) = self.known(word) {
            return Some(WordSounds { source, variants });
        }
        // A word with an apostrophe the dictionary lacks ("courtney's") reads without it.
        let bare: String = word.chars().filter(|c| *c != '\'').collect();
        if bare != word
            && let Some((source, variants)) = self.known(&bare)
        {
            return Some(WordSounds { source, variants });
        }
        let mut source = SoundSource::Dictionary;
        let mut segments: Vec<Segment> = Vec::new();
        for piece in pieces(word) {
            let (piece_source, sounds) = match piece {
                Piece::Letters(letters) => self.letters(letters)?,
                Piece::Digits(digits, ordinal) => {
                    (SoundSource::Number, self.digits(digits, ordinal)?)
                }
                Piece::Percent => (SoundSource::Number, self.phrase("percent")?),
            };
            source = source.max(piece_source);
            segments.extend_from_slice(sounds.segments());
        }
        Some(WordSounds {
            source,
            variants: vec![Sounds::new(segments)?],
        })
    }

    /// The first pronunciation of each of `text`'s words, joined.
    fn phrase(&self, text: &str) -> Option<Sounds> {
        let mut segments = Vec::new();
        for word in text.split(' ') {
            segments.extend_from_slice(self.known(word)?.1.first()?.segments());
        }
        Sounds::new(segments)
    }

    fn digits(&self, digits: &str, ordinal: bool) -> Option<Sounds> {
        match digits.parse::<u32>() {
            Ok(n)
                if n <= crate::text::MAX_NUMBER
                    && !(digits.len() > 1 && digits.starts_with('0')) =>
            {
                let words = if ordinal {
                    ordinal_words(n)?
                } else {
                    cardinal_words(n)?
                };
                self.phrase(&words)
            }
            // A long number, or one with leading zeros, is read digit by digit.
            _ => {
                let mut segments = Vec::new();
                for d in digits.bytes() {
                    let words = cardinal_words(u32::from(d - b'0'))?;
                    segments.extend_from_slice(self.phrase(&words)?.segments());
                }
                Sounds::new(segments)
            }
        }
    }

    fn letters(&self, letters: &str) -> Option<(SoundSource, Sounds)> {
        if let Some((source, variants)) = self.known(letters) {
            return Some((source, variants.into_iter().next()?));
        }
        let bytes = letters.as_bytes();
        let vowelless = !bytes.iter().any(|b| b"aeiouy".contains(b));
        if vowelless && bytes.len() <= 4 {
            let mut segments = Vec::new();
            for &b in bytes {
                segments.extend_from_slice(letter_name(b)?.segments());
            }
            return Some((SoundSource::Letters, Sounds::new(segments)?));
        }
        if let Some(sounds) = self.compound(letters) {
            return Some((SoundSource::Compound, sounds));
        }
        Some((SoundSource::Spelling, spelling::guess(letters)?))
    }

    /// `letters` as two or three dictionary words of at least two letters run together:
    /// the fewest pieces, then the longest shortest piece.
    fn compound(&self, letters: &str) -> Option<Sounds> {
        let n = letters.len();
        if !(4..=MAX_COMPOUND_LETTERS).contains(&n) || !letters.is_ascii() {
            return None;
        }
        // best[i]: the best split of letters[..i] as (pieces, shortest piece, cuts).
        let mut best: Vec<Option<(usize, usize, Vec<usize>)>> = vec![None; n + 1];
        best[0] = Some((0, usize::MAX, Vec::new()));
        for end in 2..=n {
            for start in 0..=end - 2 {
                let Some((pieces, shortest, cuts)) = best[start].clone() else {
                    continue;
                };
                if pieces >= MAX_COMPOUND_PIECES || self.known(&letters[start..end]).is_none() {
                    continue;
                }
                let candidate = (pieces + 1, shortest.min(end - start), {
                    let mut c = cuts;
                    c.push(end);
                    c
                });
                let better = match &best[end] {
                    None => true,
                    Some((p, s, _)) => candidate.0 < *p || (candidate.0 == *p && candidate.1 > *s),
                };
                if better {
                    best[end] = Some(candidate);
                }
            }
        }
        let (pieces, _, cuts) = best[n].clone()?;
        if pieces < 2 {
            return None;
        }
        let mut segments = Vec::new();
        let mut start = 0;
        for end in cuts {
            segments.extend_from_slice(self.known(&letters[start..end])?.1.first()?.segments());
            start = end;
        }
        Sounds::new(segments)
    }

    /// A text's pronunciations: its words' joined. The first is every word's most common
    /// one; each further one changes a single word to its alternative, up to
    /// [`MAX_TEXT_VARIANTS`]. `None` when a word has none or the text is too long.
    pub fn text<S: AsRef<str>>(&self, words: &[S]) -> Option<Vec<Sounds>> {
        let per_word: Vec<WordSounds> = words
            .iter()
            .map(|w| self.word(w.as_ref()))
            .collect::<Option<_>>()?;
        join_variants(&per_word)
    }
}

/// The text pronunciations of already pronounced words, as [`Pronouncer::text`].
pub(crate) fn join_variants(per_word: &[WordSounds]) -> Option<Vec<Sounds>> {
    let first: Vec<Segment> = per_word
        .iter()
        .flat_map(|w| w.variants[0].segments().iter().copied())
        .collect();
    let mut out = vec![Sounds::new(first)?];
    'words: for (i, word) in per_word.iter().enumerate() {
        for alternative in word.variants.iter().skip(1) {
            if out.len() >= MAX_TEXT_VARIANTS {
                break 'words;
            }
            let segments: Vec<Segment> = per_word
                .iter()
                .enumerate()
                .flat_map(|(j, w)| {
                    let s = if i == j { alternative } else { &w.variants[0] };
                    s.segments().iter().copied()
                })
                .collect();
            if let Some(sounds) = Sounds::new(segments)
                && !out.contains(&sounds)
            {
                out.push(sounds);
            }
        }
    }
    Some(out)
}

enum Piece<'a> {
    Letters(&'a str),
    Digits(&'a str, bool),
    Percent,
}

/// A word's runs of letters, of digits (with an ordinal suffix) and percent signs; other
/// characters separate runs.
fn pieces(word: &str) -> Vec<Piece<'_>> {
    let bytes = word.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let digits = &word[start..i];
            let mut end = i;
            while end < bytes.len() && bytes[end].is_ascii_lowercase() {
                end += 1;
            }
            if ordinal_digits(&word[start..end]).is_some() {
                out.push(Piece::Digits(digits, true));
                i = end;
            } else {
                out.push(Piece::Digits(digits, false));
            }
        } else if b.is_ascii_lowercase() || b == b'\'' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_lowercase() || bytes[i] == b'\'') {
                i += 1;
            }
            let letters = word[start..i].trim_matches('\'');
            if !letters.is_empty() {
                out.push(Piece::Letters(letters));
            }
        } else if b == b'%' {
            out.push(Piece::Percent);
            i += 1;
        } else {
            // Not ASCII or punctuation: skip the whole character.
            i += word[i..].chars().next().map_or(1, char::len_utf8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lexicon() -> Lexicon {
        Lexicon::parse(
            "the DH AH0\nthe(2) DH IY0\nyoutube Y UW1 T Y UW2 B\ntv T IY1 V IY1\nfifty F IH1 F T IY0\n\
             percent P ER0 S EH1 N T\nthird TH ER1 D\nfive F AY1 V\nthirty TH ER1 D IY0\n\
             one W AH1 N\ntwo T UW1\nthree TH R IY1\naalborg AO1 L B AO0 R G # place, danish\n",
        )
    }

    fn say(p: &Pronouncer<'_>, word: &str) -> (SoundSource, String) {
        let w = p.word(word).unwrap();
        (w.source, w.variants[0].to_string())
    }

    #[test]
    fn reads_the_dictionary_format() {
        let lexicon = lexicon();
        assert_eq!(lexicon.get("the").unwrap().len(), 2);
        assert_eq!(
            lexicon.get("aalborg").unwrap()[0].to_string(),
            "AO L B AO R G"
        );
    }

    #[test]
    fn the_vendored_dictionary_loads() {
        let lexicon = Lexicon::cmu();
        assert!(lexicon.len() > 120_000);
        assert!(lexicon.get("plex").is_some());
    }

    #[test]
    fn sources_in_order() {
        let lexicon = lexicon();
        let guides = Guides::new(
            &Pronunciations::new(vec![
                echomuse_settings::values::Pronunciation::new("tv", "T IY").unwrap(),
            ])
            .unwrap(),
        );
        let plain = Guides::default();
        let p = Pronouncer::new(&lexicon, &plain);
        let g = Pronouncer::new(&lexicon, &guides);
        assert_eq!(say(&g, "tv"), (SoundSource::Guide, "T IY".into()));
        assert_eq!(say(&p, "tv"), (SoundSource::Dictionary, "T IY V IY".into()));
        assert_eq!(
            say(&p, "50%"),
            (SoundSource::Number, "F IH F T IY P ER S EH N T".into())
        );
        assert_eq!(say(&p, "3rd").0, SoundSource::Number);
        assert_eq!(say(&p, "rmv").0, SoundSource::Letters);
        assert_eq!(
            say(&p, "youtubetv"),
            (SoundSource::Compound, "Y UW T Y UW B T IY V IY".into())
        );
        assert_eq!(say(&p, "winix").0, SoundSource::Spelling);
        assert_eq!(say(&p, "5:30").1, "F AA IH V TH ER D IY");
    }

    #[test]
    fn text_variants_change_one_word_at_a_time() {
        let lexicon = lexicon();
        let guides = Guides::default();
        let p = Pronouncer::new(&lexicon, &guides);
        let variants = p.text(&["the", "tv"]).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1].to_string(), "DH IY T IY V IY");
    }
}
