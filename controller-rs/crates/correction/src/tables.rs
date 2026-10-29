//! The compiled tables a correction searches, and a speaker's preferences compiled against
//! them.
//!
//! [`Tables`] are built from Home Assistant's names, areas and floors (the grammar crate's
//! [`Vocabulary`]), the fixed words of the intent templates, and known phrases (trigger
//! sentences and earlier requests, when the caller supplies them). They are immutable and
//! identified by a [`TablesId`]: a revision number the builder assigns plus a digest of the
//! inputs. A speaker's [`Preferences`] (preferred terms and phrases, pronunciation guides)
//! are compiled against one `TablesId` and are only used with those tables.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
};

use echomuse_grammar::{Grammar, Vocabulary};
use echomuse_settings::{settings::TranscriptSettings, values::SettingType};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{
    lexicon::{Guides, Lexicon, Pronouncer, SoundSource},
    sounds::{Bigrams, Sounds, Work, similarity},
    text::{is_stopword, letters_key, words},
};

/// Where a candidate's words came from (the record's `source`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An automation's sentence trigger.
    Trigger,
    /// An earlier request Home Assistant answered.
    History,
    /// The speaker's preferred phrases.
    UserPhrase,
    /// The speaker's preferred terms.
    UserTerm,
    /// An exposed entity's name or alias.
    Entity,
    /// An area's name or alias.
    Area,
    /// A floor's name or alias.
    Floor,
    /// The wake phrase (a lead-in removed).
    Wake,
    /// Number words written as digits.
    Numbers,
    /// A fixed word of Home Assistant's intent templates.
    Template,
}

impl Source {
    /// Whether the speaker's own lists supplied it.
    pub fn is_user(self) -> bool {
        matches!(self, Source::UserPhrase | Source::UserTerm)
    }
}

/// The kind of a known phrase.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PhraseSource {
    /// An enabled automation's sentence trigger, expanded to plain sentences.
    Trigger,
    /// The text of an earlier request Home Assistant answered.
    History,
}

/// A whole request the household is known to say.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KnownPhrase {
    /// The phrase as written.
    pub text: String,
    /// Where it is known from.
    pub source: PhraseSource,
}

/// What [`Tables`] are built from.
#[derive(Clone, Debug)]
pub struct TableInputs {
    /// The compiled local grammar: Home Assistant's intent templates. Its fixed words are
    /// the template layer, and with the vocabulary it judges whether a candidate reads as
    /// a whole request.
    pub grammar: Grammar,
    /// Home Assistant's names, areas and floors. An unknown list adds nothing.
    pub vocabulary: Vocabulary,
    /// Known phrases: trigger sentences and earlier requests. Optional; empty adds nothing.
    pub phrases: Vec<KnownPhrase>,
}

impl TableInputs {
    /// Inputs from the compiled grammar, HA's vocabulary and known phrases.
    pub fn new(grammar: &Grammar, vocabulary: Vocabulary, phrases: Vec<KnownPhrase>) -> Self {
        Self {
            grammar: grammar.clone(),
            vocabulary,
            phrases,
        }
    }

    /// A digest of everything the tables are built from, independent of input order.
    fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        let mut add_list = |tag: &str, list: Option<&Vec<String>>| {
            hasher.update(tag.as_bytes());
            match list {
                None => hasher.update([0u8]),
                Some(items) => {
                    let mut sorted: Vec<&String> = items.iter().collect();
                    sorted.sort_unstable();
                    for item in sorted {
                        hasher.update(item.as_bytes());
                        hasher.update([0u8]);
                    }
                    hasher.update([1u8]);
                }
            }
        };
        add_list("names", self.vocabulary.names.as_ref());
        add_list("areas", self.vocabulary.areas.as_ref());
        add_list("floors", self.vocabulary.floors.as_ref());
        let template: Vec<String> = self
            .grammar
            .template_words()
            .into_iter()
            .map(str::to_owned)
            .collect();
        add_list("template", Some(&template));
        let triggers: Vec<String> = self
            .phrases
            .iter()
            .filter(|p| p.source == PhraseSource::Trigger)
            .map(|p| p.text.clone())
            .collect();
        let history: Vec<String> = self
            .phrases
            .iter()
            .filter(|p| p.source == PhraseSource::History)
            .map(|p| p.text.clone())
            .collect();
        add_list("triggers", Some(&triggers));
        add_list("history", Some(&history));
        short_hex(&hasher.finalize())
    }
}

fn short_hex(bytes: &[u8]) -> String {
    bytes.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Which tables: the builder's revision number and a digest of their inputs.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TablesId {
    /// Increases with every build the builder starts.
    pub revision: u64,
    /// The inputs' digest, 12 hex digits.
    pub digest: String,
}

impl fmt::Display for TablesId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.revision, self.digest)
    }
}

/// The most names, areas and floors (with aliases) kept; further ones are left out.
pub const MAX_NAMES: usize = 20_000;
/// The most known phrases kept; further ones are left out.
pub const MAX_PHRASES: usize = 20_000;
/// The most words a name, term or phrase may have to be compared by sound.
pub const MAX_ENTRY_WORDS: usize = 16;

/// One name, word or phrase a candidate can insert.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    /// As written by its source, whitespace collapsed.
    pub text: String,
    /// Canonical words.
    pub words: Vec<String>,
    /// Letters and digits, lowercased.
    pub key: String,
    pub source: Source,
}

#[derive(Clone, Debug)]
struct Row {
    entry: u32,
    sounds: Sounds,
    bigrams: Bigrams,
}

/// The rows of one kind of entry, sorted by length so a search reads only the rows whose
/// length allows the similarity it needs.
#[derive(Clone, Debug, Default)]
pub(crate) struct Layer {
    pub entries: Vec<Entry>,
    rows: Vec<Row>,
}

/// Rows compared in full per query at most; beyond that the rows sharing the most adjacent
/// segment pairs with the query are chosen.
pub const RERANK: usize = 64;

/// One search result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Hit {
    pub entry: u32,
    pub score: f32,
}

/// A turn's allowance of work.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Budget {
    pub work: Work,
    pub limit: Work,
}

impl Budget {
    pub fn new(limit: Work) -> Self {
        Self {
            work: Work::default(),
            limit,
        }
    }

    pub fn exhausted(&self) -> bool {
        self.work.cells >= self.limit.cells || self.work.rows >= self.limit.rows
    }
}

impl Layer {
    fn add(&mut self, text: &str, source: Source, pronouncer: &Pronouncer<'_>) -> Option<u32> {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let words = words(&text);
        if words.is_empty() || words.len() > MAX_ENTRY_WORDS {
            return None;
        }
        let key = letters_key(&text);
        let index = self.entries.len() as u32;
        let per_word: Option<Vec<_>> = words.iter().map(|w| pronouncer.word(w)).collect();
        if let Some(per_word) = per_word {
            for sounds in crate::lexicon::join_variants(&per_word).unwrap_or_default() {
                let bigrams = sounds.bigrams();
                self.rows.push(Row {
                    entry: index,
                    sounds,
                    bigrams,
                });
            }
        }
        self.entries.push(Entry {
            text,
            words,
            key,
            source,
        });
        Some(index)
    }

    fn finish(&mut self) {
        self.rows.sort_by_key(|r| r.sounds.len());
    }

    /// The entries whose pronunciation is at least `min` alike to one of `query`'s, best
    /// first, each once; entries in `skip` are ignored. Stops when the budget runs out.
    pub fn search(
        &self,
        query: &[Sounds],
        min: f32,
        skip: Option<&HashSet<u32>>,
        budget: &mut Budget,
        out: &mut Vec<Hit>,
    ) {
        let mut best: HashMap<u32, f32> = HashMap::new();
        for q in query {
            if budget.exhausted() {
                break;
            }
            let len = q.len() as f32;
            let lo = (len * min).ceil() as usize;
            let hi = (len / min.max(0.01)).floor() as usize;
            let start = self.rows.partition_point(|r| r.sounds.len() < lo);
            let end = self.rows.partition_point(|r| r.sounds.len() <= hi);
            let window = &self.rows[start..end.max(start)];
            // Read no more rows than the budget has left.
            let remaining = budget.limit.rows.saturating_sub(budget.work.rows) as usize;
            let window = &window[..window.len().min(remaining)];
            budget.work.rows += window.len() as u64;
            let mut chosen: Vec<&Row> = window
                .iter()
                .filter(|r| skip.is_none_or(|s| !s.contains(&r.entry)))
                .collect();
            if chosen.len() > RERANK {
                let qb = q.bigrams();
                let score = |r: &Row| {
                    let shared = qb.shared(&r.bigrams) as f32;
                    shared / r.sounds.len().max(q.len()) as f32
                };
                chosen.sort_by(|a, b| score(b).total_cmp(&score(a)).then(a.entry.cmp(&b.entry)));
                chosen.truncate(RERANK);
            }
            for row in chosen {
                if budget.exhausted() {
                    break;
                }
                if let Some(score) = similarity(q, &row.sounds, min, &mut budget.work) {
                    let slot = best.entry(row.entry).or_insert(0.0);
                    *slot = slot.max(score);
                }
            }
        }
        let start = out.len();
        out.extend(best.into_iter().map(|(entry, score)| Hit { entry, score }));
        out[start..].sort_by(|a, b| b.score.total_cmp(&a.score).then(a.entry.cmp(&b.entry)));
    }

    /// Every entry, by index.
    pub fn entry(&self, index: u32) -> &Entry {
        &self.entries[index as usize]
    }
}

/// The three kinds of shared layer.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum LayerKind {
    Names = 0,
    Phrases = 1,
    Template = 2,
}

/// The compiled, immutable correction tables. Build them with [`Tables::build`] off any
/// device's thread (the [`crate::TableBuilder`] does), share them as `Arc<Tables>`.
#[derive(Debug)]
pub struct Tables {
    id: TablesId,
    lexicon: Arc<Lexicon>,
    /// The grammar with this vocabulary.
    pub(crate) grammar: Grammar,
    pub(crate) names: Layer,
    pub(crate) phrases: Layer,
    pub(crate) template: Layer,
    pub(crate) template_words: HashSet<String>,
    /// Letter keys of every name, area and floor.
    pub(crate) name_keys: HashSet<String>,
    /// The words of every name, area and floor that are not stopwords.
    pub(crate) name_words: HashSet<String>,
    /// For each word, the shared entries that contain it.
    word_entries: HashMap<String, Vec<(LayerKind, u32)>>,
    left_out: usize,
}

impl Tables {
    /// Compiles `inputs` under `lexicon` as revision `revision`. Takes tens of milliseconds
    /// for a household's vocabulary and longer with thousands of known phrases.
    pub fn build(revision: u64, inputs: &TableInputs, lexicon: Arc<Lexicon>) -> Self {
        let no_guides = Guides::default();
        let pronouncer = Pronouncer::new(&lexicon, &no_guides);
        let mut names = Layer::default();
        let mut left_out = 0;
        let mut name_keys = HashSet::new();
        let lists = [
            (inputs.vocabulary.names.as_ref(), Source::Entity),
            (inputs.vocabulary.areas.as_ref(), Source::Area),
            (inputs.vocabulary.floors.as_ref(), Source::Floor),
        ];
        let mut seen: HashSet<(String, Source)> = HashSet::new();
        for (list, source) in lists {
            for name in list.into_iter().flatten() {
                let key = letters_key(name);
                if key.is_empty() || !seen.insert((key.clone(), source)) {
                    continue;
                }
                if names.entries.len() >= MAX_NAMES {
                    left_out += 1;
                    continue;
                }
                if names.add(name, source, &pronouncer).is_some() {
                    name_keys.insert(key);
                }
            }
        }
        names.finish();
        let name_words: HashSet<String> = names
            .entries
            .iter()
            .flat_map(|e| e.words.iter())
            .filter(|w| !is_stopword(w))
            .cloned()
            .collect();

        let mut phrases = Layer::default();
        let mut phrase_keys = HashSet::new();
        for phrase in &inputs.phrases {
            let key = letters_key(&phrase.text);
            if key.is_empty() || !phrase_keys.insert(key) {
                continue;
            }
            if phrases.entries.len() >= MAX_PHRASES {
                left_out += 1;
                continue;
            }
            let source = match phrase.source {
                PhraseSource::Trigger => Source::Trigger,
                PhraseSource::History => Source::History,
            };
            // Canonical words: a phrase is sent as its words, not its punctuation.
            let canonical = words(&phrase.text).join(" ");
            phrases.add(&canonical, source, &pronouncer);
        }
        phrases.finish();

        let mut template = Layer::default();
        let mut template_words = HashSet::new();
        for word in inputs.grammar.template_words() {
            let useful = word.len() >= 2
                && word.chars().all(|c| c.is_alphabetic() || c == '\'')
                && !is_stopword(word);
            if useful && template_words.insert(word.to_owned()) {
                template.add(word, Source::Template, &pronouncer);
            }
        }
        template.finish();

        let mut word_entries: HashMap<String, Vec<(LayerKind, u32)>> = HashMap::new();
        for (kind, layer) in [
            (LayerKind::Names, &names),
            (LayerKind::Phrases, &phrases),
            (LayerKind::Template, &template),
        ] {
            for (i, entry) in layer.entries.iter().enumerate() {
                for word in &entry.words {
                    word_entries
                        .entry(word.clone())
                        .or_default()
                        .push((kind, i as u32));
                }
            }
        }

        Self {
            id: TablesId {
                revision,
                digest: inputs.digest(),
            },
            lexicon,
            grammar: inputs.grammar.with_vocabulary(&inputs.vocabulary),
            names,
            phrases,
            template,
            template_words,
            name_keys,
            name_words,
            word_entries,
            left_out,
        }
    }

    /// Which tables these are.
    pub fn id(&self) -> &TablesId {
        &self.id
    }

    /// The dictionary they were built with.
    pub fn lexicon(&self) -> &Arc<Lexicon> {
        &self.lexicon
    }

    /// How many names and phrases were left out by [`MAX_NAMES`] and [`MAX_PHRASES`].
    pub fn left_out(&self) -> usize {
        self.left_out
    }

    pub(crate) fn layer(&self, kind: LayerKind) -> &Layer {
        match kind {
            LayerKind::Names => &self.names,
            LayerKind::Phrases => &self.phrases,
            LayerKind::Template => &self.template,
        }
    }

    /// The words of the names, phrases and template words whose pronunciation was guessed
    /// from their spelling under `preferences`' guides, each with how many entries use it,
    /// most used first: the words a pronunciation guide would help most.
    pub fn guessed_words(&self, preferences: &Preferences) -> Vec<(String, usize)> {
        let pronouncer = Pronouncer::new(&self.lexicon, &preferences.guides);
        let mut counts: Vec<(String, usize)> = self
            .word_entries
            .iter()
            .filter(|(word, _)| {
                pronouncer
                    .word(word)
                    .is_none_or(|w| w.source == SoundSource::Spelling)
            })
            .map(|(word, entries)| (word.clone(), entries.len()))
            .collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }
}

/// A digest of a speaker's transcript settings: a result computed under other settings is
/// stale.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ConfigDigest(String);

impl ConfigDigest {
    /// The digest of the mode, terms, phrases and pronunciations.
    pub fn of(settings: &TranscriptSettings) -> Self {
        let mut hasher = Sha256::new();
        for value in [
            settings.transcript_fix.to_json(),
            settings.transcript_fix_terms.to_json(),
            settings.transcript_fix_phrases.to_json(),
            settings.transcript_fix_pronunciations.to_json(),
        ] {
            hasher.update(value.to_python_text().as_bytes());
            hasher.update([0u8]);
        }
        Self(short_hex(&hasher.finalize()))
    }

    /// The digest, 12 hex digits.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Preference for a candidate that is the user's phrase: up to this much, by its place.
pub const PHRASE_PREFERENCE: f64 = 0.02;
/// Preference for a candidate containing the user's term: up to this much, by its place.
pub const TERM_PREFERENCE: f64 = 0.01;
/// The most preference a candidate gets.
pub const PREFERENCE_CAP: f64 = 0.02;
/// The longest term (in words) looked for inside a candidate.
const MAX_TERM_WORDS: usize = 6;

/// A speaker's transcript preferences, compiled against one [`Tables`]: the preferred terms
/// and phrases as searchable entries, the pronunciation guides, and the shared entries the
/// guides re-pronounce. Small (each list holds at most 200 entries), so compiling takes
/// milliseconds; the builder does it off the device's thread all the same.
#[derive(Debug)]
pub struct Preferences {
    tables: TablesId,
    config: ConfigDigest,
    pub(crate) guides: Guides,
    pub(crate) terms: Layer,
    pub(crate) phrases: Layer,
    /// Shared entries containing a guided word, pronounced again with the guides.
    pub(crate) overrides: [Layer; 3],
    /// For each shared layer, the entries `overrides` replaces.
    pub(crate) superseded: [HashSet<u32>; 3],
    /// Each term's canonical words, highest preference first.
    term_words: Vec<Vec<String>>,
    /// Each phrase's letter key, highest preference first.
    phrase_keys: Vec<String>,
}

impl Preferences {
    /// Compiles `settings`' lists and guides against `tables`.
    pub fn compile(tables: &Tables, settings: &TranscriptSettings) -> Self {
        let guides = Guides::new(&settings.transcript_fix_pronunciations);
        let pronouncer = Pronouncer::new(&tables.lexicon, &guides);
        let mut terms = Layer::default();
        let mut term_words = Vec::new();
        for entry in settings.transcript_fix_terms.0.entries() {
            if terms
                .add(entry.as_str(), Source::UserTerm, &pronouncer)
                .is_some()
            {
                term_words.push(words(entry.as_str()));
            }
        }
        terms.finish();
        let mut phrases = Layer::default();
        let mut phrase_keys = Vec::new();
        for entry in settings.transcript_fix_phrases.0.entries() {
            if phrases
                .add(entry.as_str(), Source::UserPhrase, &pronouncer)
                .is_some()
            {
                phrase_keys.push(letters_key(entry.as_str()));
            }
        }
        phrases.finish();

        let mut overrides: [Layer; 3] = Default::default();
        let mut superseded: [HashSet<u32>; 3] = Default::default();
        for word in guides.words() {
            for &(kind, index) in tables.word_entries.get(word).into_iter().flatten() {
                let k = kind as usize;
                if !superseded[k].insert(index) {
                    continue;
                }
                let entry = tables.layer(kind).entry(index);
                overrides[k].add(&entry.text, entry.source, &pronouncer);
            }
        }
        for layer in &mut overrides {
            layer.finish();
        }
        Self {
            tables: tables.id.clone(),
            config: ConfigDigest::of(settings),
            guides,
            terms,
            phrases,
            overrides,
            superseded,
            term_words,
            phrase_keys,
        }
    }

    /// The tables these were compiled against.
    pub fn tables(&self) -> &TablesId {
        &self.tables
    }

    /// The settings these were compiled from.
    pub fn config(&self) -> &ConfigDigest {
        &self.config
    }

    /// The user's guides.
    pub fn guides(&self) -> &Guides {
        &self.guides
    }

    /// How much the user's lists prefer a candidate with these canonical words.
    pub(crate) fn preference(&self, candidate: &[String]) -> f64 {
        let key = letters_key(&candidate.join(" "));
        let mut total = 0.0;
        let n = self.phrase_keys.len();
        if let Some(i) = self.phrase_keys.iter().position(|k| *k == key) {
            total += PHRASE_PREFERENCE * (n - i) as f64 / n as f64;
        }
        let m = self.term_words.len();
        let contains = |term: &[String]| {
            !term.is_empty()
                && term.len() <= MAX_TERM_WORDS
                && candidate.windows(term.len()).any(|w| w == term)
        };
        if let Some(i) = self.term_words.iter().position(|t| contains(t)) {
            total += TERM_PREFERENCE * (m - i) as f64 / m as f64;
        }
        total.min(PREFERENCE_CAP)
    }

    /// Whether `key` (letters) is one of the user's terms.
    pub(crate) fn is_term_key(&self, key: &str) -> bool {
        self.terms.entries.iter().any(|e| e.key == key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echomuse_settings::values::{FixEntry, FixList, FixTerms, Pronunciation, Pronunciations};

    fn lexicon() -> Arc<Lexicon> {
        Arc::new(Lexicon::parse(
            "kitchen K IH1 CH AH0 N\nlight L AY1 T\nplex P L EH1 K S\nbedroom B EH1 D R UW2 M\n\
             turn T ER1 N\nwinix W IH1 N IH0 K S\n",
        ))
    }

    static GRAMMAR: std::sync::LazyLock<Grammar> =
        std::sync::LazyLock::new(|| Grammar::new().expect("templates compile"));

    fn inputs() -> TableInputs {
        TableInputs {
            grammar: GRAMMAR.clone(),
            vocabulary: Vocabulary {
                names: Some(vec!["Kitchen Light".into(), "Plex".into()]),
                areas: Some(vec!["Bedroom".into()]),
                floors: None,
            },
            phrases: vec![],
        }
    }

    #[test]
    fn digest_ignores_order_and_follows_content() {
        let a = inputs();
        let mut b = inputs();
        b.vocabulary.names = Some(vec!["Plex".into(), "Kitchen Light".into()]);
        assert_eq!(a.digest(), b.digest());
        b.vocabulary.names = Some(vec!["Plex".into()]);
        assert_ne!(a.digest(), b.digest());
    }

    #[test]
    fn builds_layers() {
        let tables = Tables::build(3, &inputs(), lexicon());
        assert_eq!(tables.id().revision, 3);
        assert_eq!(tables.names.entries.len(), 3);
        assert!(tables.template_words.contains("turn"));
        assert!(!tables.template_words.contains("the"));
        assert!(tables.name_words.contains("kitchen"));
        let mut budget = Budget::new(Work {
            cells: 1_000_000,
            rows: 1_000_000,
        });
        let mut hits = Vec::new();
        let flex = Sounds::parse_arpabet("F L EH K S").unwrap();
        tables
            .names
            .search(&[flex], 0.8, None, &mut budget, &mut hits);
        assert_eq!(tables.names.entry(hits[0].entry).text, "Plex");
    }

    #[test]
    fn preferences_rank_and_guides() {
        let tables = Tables::build(1, &inputs(), lexicon());
        let settings = TranscriptSettings {
            transcript_fix_terms: FixTerms(
                FixList::new(vec![
                    FixEntry::new("Plex").unwrap(),
                    FixEntry::new("Winix").unwrap(),
                ])
                .unwrap(),
            ),
            transcript_fix_pronunciations: Pronunciations::new(vec![
                Pronunciation::new("kitchen", "K IH CH IH N").unwrap(),
            ])
            .unwrap(),
            ..TranscriptSettings::default()
        };
        let prefs = Preferences::compile(&tables, &settings);
        assert_eq!(prefs.tables(), tables.id());
        assert_eq!(prefs.preference(&words("watch plex")), TERM_PREFERENCE);
        assert_eq!(prefs.preference(&words("winix on")), TERM_PREFERENCE / 2.0);
        assert_eq!(prefs.preference(&words("nothing")), 0.0);
        // "Kitchen Light" is pronounced again with the guide.
        assert!(prefs.superseded[LayerKind::Names as usize].contains(&0));
        assert_eq!(
            prefs.overrides[LayerKind::Names as usize].entries[0].text,
            "Kitchen Light"
        );
        assert_ne!(
            ConfigDigest::of(&settings),
            ConfigDigest::of(&TranscriptSettings::default())
        );
    }
}
