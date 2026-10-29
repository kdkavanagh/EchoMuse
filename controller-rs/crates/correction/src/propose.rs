//! The per-turn proposer: candidate repairs of a transcript, found by sound against the
//! compiled tables, with a fixed upper bound on the work one transcript can cost.

use std::collections::{HashMap, HashSet};

use serde::Serialize;

use crate::{
    lexicon::Pronouncer,
    meaning::{Edit, has_verb, preserved, protected},
    sounds::{Sounds, Work, similarity},
    tables::{Budget, Entry, Hit, Layer, LayerKind, Preferences, Source, Tables},
    text::{is_stopword, letter_count, letters_key, number_runs, words},
};

/// The repair a candidate makes (the record's `rule`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// Number words written as digits.
    Numbers,
    /// Leftover wake-phrase sounds removed from the start of a wake turn.
    LeadIn,
    /// A span of one to four words replaced by a name, area, floor or preferred term.
    Name,
    /// One word replaced by a fixed word of Home Assistant's intent templates.
    Word,
    /// The whole text replaced by a known or preferred phrase that sounds like it.
    Phrase,
    /// A cut-off text completed to the known or preferred phrase it begins or ends.
    Cutoff,
}

/// Similarity a name or term needs to replace a span.
pub const NAME_MIN: f32 = 0.80;
/// Similarity a template word needs to replace a word.
pub const WORD_MIN: f32 = 0.78;
/// Similarity a phrase needs to replace the whole text (over the words that differ).
pub const PHRASE_MIN: f32 = 0.70;
/// Similarity leading words need to the wake phrase's end to be removed.
pub const LEAD_IN_MIN: f32 = 0.75;
/// The least share of a phrase's letters a cut-off text must have.
pub const CUTOFF_MIN_COVERAGE: f64 = 0.5;
/// The fewest segments of the wake phrase's end a lead-in is compared with.
const MIN_TAIL_SEGMENTS: usize = 3;
/// The most leading words a lead-in removes.
const MAX_LEAD_IN_WORDS: usize = 3;
/// The most words a replaced span has.
pub const MAX_SPAN_WORDS: usize = 4;
/// Hits kept per searched span, word or text.
const HITS_PER_QUERY: usize = 3;
/// Cut-off phrases kept.
const MAX_CUTOFFS: usize = 4;
/// The most times one proposal asks the local grammar whether a text is a whole request.
pub const MAX_GRAMMAR_CHECKS: usize = 8;
/// Candidates generated at most before merging.
pub const MAX_CANDIDATES: usize = 48;

/// The least letters and digits a transcript needs for a proposal.
pub const MIN_LETTERS: usize = 4;
/// The most words a transcript may have for a proposal; a longer one is not a command.
pub const MAX_WORDS: usize = 24;
/// The most characters a transcript may have for a proposal.
pub const MAX_CHARS: usize = 300;
/// The most work one transcript's proposal may do: edit-distance cells and table rows.
/// About 10 ms on one core in an optimized build.
pub const WORK_LIMIT: Work = Work {
    cells: 2_000_000,
    rows: 400_000,
};

/// Why a transcript got no proposal at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Skipped {
    /// Fewer than [`MIN_LETTERS`] letters and digits.
    TooShort,
    /// More than [`MAX_WORDS`] words or [`MAX_CHARS`] characters.
    TooLong,
}

/// One proposed repair.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// The text it would send: canonical words, an inserted name in its own spelling.
    pub text: String,
    /// Its canonical words.
    pub words: Vec<String>,
    /// The repair.
    pub rule: Rule,
    /// Where the inserted words came from.
    pub source: Source,
    /// The evidence for it, 0 to 1: how alike the words it changes sound (for a cut-off,
    /// the whole texts; for numbers, 1).
    pub raw: f64,
    /// For a cut-off: the share of the phrase's letters that was heard.
    pub coverage: Option<f64>,
    /// What the user's lists add to its rank.
    pub preference: f64,
    /// Evidence plus preference: the order candidates are checked in.
    pub rank: f64,
    /// Why it does not keep what was said; `None` when it does.
    pub not_preserved: Option<&'static str>,
    pub(crate) edit: Edit,
    /// A name, area, floor or term it inserts, as written.
    pub(crate) inserted: Option<String>,
}

impl Candidate {
    /// Whether it decides by evidence the matcher's free text cannot fake: a whole phrase
    /// or a lead-in is judged by the whole text, so a free-text match does not refuse it.
    pub(crate) fn whole_text(&self) -> bool {
        matches!(self.rule, Rule::LeadIn | Rule::Phrase | Rule::Cutoff)
    }
}

/// The proposer's result for one transcript.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    /// The transcript's canonical words.
    pub heard: Vec<String>,
    /// Candidates that keep what was said, merged by text, best rank first.
    pub candidates: Vec<Candidate>,
    /// Candidates refused before Home Assistant because they change what was said.
    pub rejected: Vec<Candidate>,
    /// The work done.
    pub work: Work,
    /// The work limit cut the search short.
    pub truncated: bool,
    /// Why nothing was searched, if so.
    pub skipped: Option<Skipped>,
}

/// Proposes repairs of `heard` from `tables` and the speaker's `preferences`. A wake turn
/// passes its `wake_phrase`, so leftover wake sounds at the start can be removed.
///
/// Pure and bounded: at most [`WORK_LIMIT`] of comparison work, whatever the input.
/// `preferences` must have been compiled against `tables`; the caller checks
/// ([`crate::prepare`] does).
pub fn propose(
    tables: &Tables,
    preferences: &Preferences,
    heard: &str,
    wake_phrase: Option<&str>,
) -> Proposal {
    let heard_words = words(heard);
    let mut proposal = Proposal {
        heard: heard_words.clone(),
        candidates: Vec::new(),
        rejected: Vec::new(),
        work: Work::default(),
        truncated: false,
        skipped: None,
    };
    if letter_count(heard) < MIN_LETTERS || heard_words.is_empty() {
        proposal.skipped = Some(Skipped::TooShort);
        return proposal;
    }
    if heard_words.len() > MAX_WORDS || heard.chars().count() > MAX_CHARS {
        proposal.skipped = Some(Skipped::TooLong);
        return proposal;
    }
    let mut p = Proposer {
        tables,
        prefs: preferences,
        pron: Pronouncer::new(tables.lexicon(), &preferences.guides),
        heard: &heard_words,
        heard_key: letters_key(&heard_words.join(" ")),
        grammar_checks: 0,
        heard_whole: None,
        budget: Budget::new(WORK_LIMIT),
        found: Vec::new(),
    };
    p.numbers();
    if let Some(wake) = wake_phrase {
        p.lead_in(wake);
    }
    p.cutoff();
    p.phrase();
    p.names();
    p.template_words();
    let truncated = p.budget.exhausted();
    let work = p.budget.work;
    let found = std::mem::take(&mut p.found);
    let (kept, rejected): (Vec<_>, Vec<_>) =
        found.into_iter().partition(|c| c.not_preserved.is_none());
    proposal.candidates = merge(kept);
    proposal.rejected = merge(rejected);
    proposal.work = work;
    proposal.truncated = truncated;
    proposal
}

/// One candidate per text (by letters), the better ranked kept; best rank first.
fn merge(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut by_key: HashMap<String, Candidate> = HashMap::new();
    for c in candidates {
        let key = letters_key(&c.text);
        match by_key.get(&key) {
            Some(existing) if order(existing, &c).is_le() => {}
            _ => {
                by_key.insert(key, c);
            }
        }
    }
    let mut out: Vec<Candidate> = by_key.into_values().collect();
    out.sort_by(order);
    out
}

/// Best first: rank, then evidence, then text.
pub(crate) fn order(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.rank
        .total_cmp(&a.rank)
        .then(b.raw.total_cmp(&a.raw))
        .then_with(|| a.text.cmp(&b.text))
}

struct Proposer<'a> {
    tables: &'a Tables,
    prefs: &'a Preferences,
    pron: Pronouncer<'a>,
    heard: &'a [String],
    heard_key: String,
    /// Grammar classifications made so far.
    grammar_checks: usize,
    /// The grammar's reading of the heard text, once asked.
    heard_whole: Option<bool>,
    budget: Budget,
    found: Vec<Candidate>,
}

impl<'a> Proposer<'a> {
    fn full(&self) -> bool {
        self.found.len() >= MAX_CANDIDATES || self.budget.exhausted()
    }

    /// Records a candidate with its preservation verdict, unless it changes nothing.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        text: String,
        rule: Rule,
        source: Source,
        raw: f64,
        coverage: Option<f64>,
        edit: Edit,
        inserted: Option<String>,
    ) {
        if self.found.len() >= MAX_CANDIDATES {
            return;
        }
        let candidate_words = words(&text);
        if candidate_words.is_empty() || letters_key(&text) == self.heard_key {
            return;
        }
        let verb_may_be_added = matches!(rule, Rule::Phrase | Rule::Word);
        let not_preserved = preserved(
            self.heard,
            &candidate_words,
            &edit,
            verb_may_be_added,
            &self.tables.name_words,
        )
        .err();
        let preference = self.prefs.preference(&candidate_words);
        self.found.push(Candidate {
            text,
            words: candidate_words,
            rule,
            source,
            raw,
            coverage,
            preference,
            rank: raw + preference,
            not_preserved,
            edit,
            inserted,
        });
    }

    /// The heard words with `heard[start..end]` replaced by `insert`.
    fn replaced(&self, start: usize, end: usize, insert: &str) -> String {
        let mut parts: Vec<&str> = self.heard[..start].iter().map(String::as_str).collect();
        parts.extend(insert.split_whitespace());
        parts.extend(self.heard[end..].iter().map(String::as_str));
        parts.join(" ")
    }

    /// Search one shared layer, its re-pronounced entries included.
    fn search_shared(
        &mut self,
        kind: LayerKind,
        query: &[Sounds],
        min: f32,
    ) -> Vec<(&'a Entry, f32)> {
        let k = kind as usize;
        let base = self.tables.layer(kind);
        let mut hits: Vec<Hit> = Vec::new();
        base.search(
            query,
            min,
            Some(&self.prefs.superseded[k]),
            &mut self.budget,
            &mut hits,
        );
        let mut out: Vec<(&'a Entry, f32)> = hits
            .iter()
            .map(|h| (base.entry(h.entry), h.score))
            .collect();
        let overrides: &'a Layer = &self.prefs.overrides[k];
        let mut extra = Vec::new();
        overrides.search(query, min, None, &mut self.budget, &mut extra);
        out.extend(extra.iter().map(|h| (overrides.entry(h.entry), h.score)));
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.text.cmp(&b.0.text)));
        out
    }

    fn search_user(
        &mut self,
        layer: &'a Layer,
        query: &[Sounds],
        min: f32,
    ) -> Vec<(&'a Entry, f32)> {
        let mut hits = Vec::new();
        layer.search(query, min, None, &mut self.budget, &mut hits);
        hits.iter()
            .map(|h| (layer.entry(h.entry), h.score))
            .collect()
    }

    /// Whether two words are the same: spelled alike or sharing a pronunciation.
    fn same_word(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        match (self.pron.word(a), self.pron.word(b)) {
            (Some(x), Some(y)) => x.variants.iter().any(|v| y.variants.contains(v)),
            _ => false,
        }
    }

    /// How alike the words that differ between `from` and `to` sound: shared words are left
    /// out wherever they stand. Both sides empty is 1; one side empty is 0.
    fn changed_similarity(&mut self, from: &[String], to: &[String], min: f32) -> Option<f32> {
        let mut rest_to: Vec<&String> = to.iter().collect();
        let mut rest_from: Vec<&String> = Vec::new();
        for word in from {
            match rest_to.iter().position(|t| self.same_word(word, t)) {
                Some(i) => {
                    rest_to.remove(i);
                }
                None => rest_from.push(word),
            }
        }
        match (rest_from.is_empty(), rest_to.is_empty()) {
            (true, true) => return Some(1.0),
            (true, false) | (false, true) => return None,
            _ => {}
        }
        let a = self.pron.text(&rest_from)?;
        let b = self.pron.text(&rest_to)?;
        let mut best: Option<f32> = None;
        for x in &a {
            for y in &b {
                if self.budget.exhausted() {
                    return best;
                }
                if let Some(s) = similarity(x, y, min, &mut self.budget.work) {
                    best = Some(best.map_or(s, |b: f32| b.max(s)));
                }
            }
        }
        best
    }

    fn numbers(&mut self) {
        let runs = number_runs(self.heard);
        if runs.is_empty() {
            return;
        }
        let mut parts: Vec<String> = Vec::new();
        let mut i = 0;
        for run in &runs {
            parts.extend(self.heard[i..run.start].iter().cloned());
            parts.push(run.value.to_string());
            i = run.end;
        }
        parts.extend(self.heard[i..].iter().cloned());
        self.push(
            parts.join(" "),
            Rule::Numbers,
            Source::Numbers,
            1.0,
            None,
            Edit::Whole,
            None,
        );
    }

    fn lead_in(&mut self, wake_phrase: &str) {
        let wake_words = words(wake_phrase);
        let Some(wake) = self.pron.text(&wake_words) else {
            return;
        };
        // The wake phrase's ends: the leftovers a recognizer writes are its tail.
        let mut tails: Vec<Sounds> = Vec::new();
        for w in &wake {
            for start in 0..=w.len().saturating_sub(MIN_TAIL_SEGMENTS) {
                if let Some(tail) = Sounds::new(w.segments()[start..].to_vec())
                    && !tails.contains(&tail)
                {
                    tails.push(tail);
                }
            }
        }
        for k in 1..=MAX_LEAD_IN_WORDS.min(self.heard.len().saturating_sub(1)) {
            if self.full() {
                return;
            }
            let rest = &self.heard[k..];
            if letter_count(&rest.join("")) < MIN_LETTERS {
                break;
            }
            let Some(lead) = self.pron.text(&self.heard[..k]) else {
                continue;
            };
            let mut best: Option<f32> = None;
            for l in &lead {
                for t in &tails {
                    if self.budget.exhausted() {
                        break;
                    }
                    if let Some(s) = similarity(l, t, LEAD_IN_MIN, &mut self.budget.work) {
                        best = Some(best.map_or(s, |b: f32| b.max(s)));
                    }
                }
            }
            if let Some(score) = best {
                let edit = Edit::Span {
                    start: 0,
                    end: k,
                    inserted: Vec::new(),
                };
                self.push(
                    rest.join(" "),
                    Rule::LeadIn,
                    Source::Wake,
                    f64::from(score),
                    None,
                    edit,
                    None,
                );
            }
        }
    }

    fn content_words(words: &[String]) -> Vec<&String> {
        words.iter().filter(|w| !is_stopword(w)).collect()
    }

    /// Whether `entry` shares a content word with the heard text.
    fn shares_content(&self, entry: &Entry) -> bool {
        let heard = Self::content_words(self.heard);
        let theirs = Self::content_words(&entry.words);
        heard
            .iter()
            .any(|h| theirs.iter().any(|t| self.same_word(h, t)))
    }

    fn phrase(&mut self) {
        let Some(query) = self.pron.text(self.heard) else {
            return;
        };
        let mut hits = self.search_shared(LayerKind::Phrases, &query, PHRASE_MIN);
        hits.truncate(HITS_PER_QUERY * 2);
        let mut user = self.search_user(&self.prefs.phrases, &query, PHRASE_MIN);
        user.truncate(HITS_PER_QUERY);
        hits.extend(user);
        for (entry, _) in hits {
            if self.full() {
                return;
            }
            if entry.key == self.heard_key || !self.shares_content(entry) {
                continue;
            }
            if let Some(score) = self.changed_similarity(self.heard, &entry.words, PHRASE_MIN) {
                self.push(
                    entry.text.clone(),
                    Rule::Phrase,
                    entry.source,
                    f64::from(score),
                    None,
                    Edit::Whole,
                    None,
                );
            }
        }
    }

    /// A cut-off text completed to a known phrase whose letters begin or end with it. Not
    /// tried when the local grammar already reads the heard text as a whole request.
    fn cutoff(&mut self) {
        if self.heard_reads_whole() {
            return;
        }
        let heard_len = self.heard_key.chars().count();
        if heard_len < MIN_LETTERS {
            return;
        }
        let layers: [&'a Layer; 3] = [
            self.tables.layer(LayerKind::Phrases),
            &self.prefs.overrides[LayerKind::Phrases as usize],
            &self.prefs.phrases,
        ];
        let mut found: Vec<(&'a Entry, f64)> = Vec::new();
        for layer in layers {
            let remaining = self.budget.limit.rows.saturating_sub(self.budget.work.rows) as usize;
            let entries = &layer.entries[..layer.entries.len().min(remaining)];
            self.budget.work.rows += entries.len() as u64;
            for entry in entries {
                let len = entry.key.chars().count();
                if len <= heard_len
                    || !(entry.key.starts_with(&self.heard_key)
                        || entry.key.ends_with(&self.heard_key))
                {
                    continue;
                }
                let coverage = heard_len as f64 / len as f64;
                if coverage >= CUTOFF_MIN_COVERAGE {
                    found.push((entry, coverage));
                }
            }
        }
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.text.cmp(&b.0.text)));
        found.dedup_by(|a, b| a.0.key == b.0.key);
        found.truncate(MAX_CUTOFFS);
        let Some(heard) = self.pron.text(self.heard) else {
            return;
        };
        for (entry, coverage) in found {
            if self.full() {
                return;
            }
            let mut raw: f32 = 0.0;
            if let Some(theirs) = self.pron.text(&entry.words) {
                for x in &heard {
                    for y in &theirs {
                        if self.budget.exhausted() {
                            break;
                        }
                        if let Some(s) = similarity(x, y, 0.0, &mut self.budget.work) {
                            raw = raw.max(s);
                        }
                    }
                }
            }
            self.push(
                entry.text.clone(),
                Rule::Cutoff,
                entry.source,
                f64::from(raw),
                Some(coverage),
                Edit::Whole,
                None,
            );
        }
    }

    fn names(&mut self) {
        let n = self.heard.len();
        let known_word = |w: &String| self.tables.template_words.contains(w);
        for start in 0..n {
            for end in (start + 1)..=(start + MAX_SPAN_WORDS).min(n) {
                if self.full() {
                    return;
                }
                let span = &self.heard[start..end];
                if end - start == n
                    || is_stopword(&span[0])
                    || is_stopword(&span[span.len() - 1])
                    || span.iter().any(|w| protected(w))
                    || span.iter().all(known_word)
                    || letter_count(&span.join("")) < MIN_LETTERS
                {
                    continue;
                }
                let key = letters_key(&span.join(" "));
                if self.tables.name_keys.contains(&key) || self.prefs.is_term_key(&key) {
                    continue;
                }
                let Some(query) = self.pron.text(span) else {
                    continue;
                };
                let mut hits = self.search_shared(LayerKind::Names, &query, NAME_MIN);
                hits.extend(self.search_user(&self.prefs.terms, &query, NAME_MIN));
                hits.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.text.cmp(&b.0.text)));
                hits.truncate(HITS_PER_QUERY);
                for (entry, _) in hits {
                    if entry.key == key {
                        continue;
                    }
                    if let Some(score) = self.changed_similarity(span, &entry.words, NAME_MIN) {
                        let edit = Edit::Span {
                            start,
                            end,
                            inserted: entry.words.clone(),
                        };
                        let text = self.replaced(start, end, &entry.text);
                        self.push(
                            text,
                            Rule::Name,
                            entry.source,
                            f64::from(score),
                            None,
                            edit,
                            Some(entry.text.clone()),
                        );
                    }
                }
            }
        }
    }

    /// Whether the local grammar reads the heard text as a whole request (asked once).
    fn heard_reads_whole(&mut self) -> bool {
        if let Some(whole) = self.heard_whole {
            return whole;
        }
        let whole = self.reads_whole(&self.heard.join(" "));
        self.heard_whole = Some(whole);
        whole
    }

    /// Whether the local grammar reads `text` as a whole request: a template matched it
    /// completely ([`echomuse_grammar::Grammar::reads_whole`]). Counts against
    /// [`MAX_GRAMMAR_CHECKS`]; `false` once they are used up.
    fn reads_whole(&mut self, text: &str) -> bool {
        if self.grammar_checks >= MAX_GRAMMAR_CHECKS {
            return false;
        }
        self.grammar_checks += 1;
        self.tables.grammar.reads_whole(text)
    }

    /// One word replaced by a template word, kept only when the local grammar reads the
    /// result as a whole request and did not read the heard text as one. Ordinary words
    /// sound like template words all the time ("good" and "get"); the grammar is what says
    /// the replacement made a command.
    fn template_words(&mut self) {
        if self.heard_reads_whole() {
            return;
        }
        let no_verb = !has_verb(self.heard);
        let heard = self.heard;
        let mut seen: HashSet<&str> = HashSet::new();
        for (i, word) in heard.iter().enumerate() {
            if self.full() {
                return;
            }
            if word.chars().count() < 3
                || is_stopword(word)
                || protected(word)
                || self.tables.template_words.contains(word)
                || self.tables.name_words.contains(word)
                || !seen.insert(word.as_str())
            {
                continue;
            }
            let Some(sounds) = self.pron.word(word) else {
                continue;
            };
            let mut hits = self.search_shared(LayerKind::Template, &sounds.variants, WORD_MIN);
            hits.truncate(HITS_PER_QUERY);
            for (entry, score) in hits {
                // A template verb only stands in for a garbled command word.
                if crate::meaning::has_verb(&entry.words) && !no_verb {
                    continue;
                }
                let edit = Edit::Span {
                    start: i,
                    end: i + 1,
                    inserted: entry.words.clone(),
                };
                let text = self.replaced(i, i + 1, &entry.text);
                if !self.reads_whole(&text) {
                    continue;
                }
                self.push(
                    text,
                    Rule::Word,
                    Source::Template,
                    f64::from(score),
                    None,
                    edit,
                    None,
                );
            }
        }
    }
}
