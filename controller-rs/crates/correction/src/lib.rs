//! Transcript correction (spec §4.7): proposing a repair of a transcript that Home
//! Assistant's own sentence matcher accepts, without changing what the user meant.
//!
//! # Modes
//!
//! `transcriptFix` is `off`, `shadow` (the default) or `on`:
//!
//! - `off`: nothing is computed and nothing is recorded; the heard text is sent.
//! - `shadow`: the correction is computed beside the turn and recorded with what `on`
//!   would have sent and whether it would have been ready in time; the heard text is sent.
//! - `on`: the turn waits at most [`FINAL_DEADLINE`] (100 ms) after the final transcript;
//!   a correction is sent only if it passed every check within that time.
//!
//! # What the application calls
//!
//! 1. At start: [`TableBuilder::spawn`]. Whenever Home Assistant's names, areas or floors
//!    change, or it reconnects: [`TableBuilder::withdraw`] and then
//!    [`TableBuilder::rebuild`] with [`TableInputs::new`] (the grammar, the vocabulary and,
//!    optionally, known phrases). Tables are published whole on the returned `watch`.
//! 2. For each speaker, whenever the tables or its transcript settings change:
//!    [`TableBuilder::compile_preferences`]. Keep the pair as a [`Snapshot`].
//! 3. Per turn, immediately before the intent run: [`prepare`] with the speaker's
//!    settings, the turn's text, its snapshot and the Home Assistant context. It is
//!    synchronous and bounded ([`WORK_LIMIT`], about 10 ms on one core). A
//!    [`Prepared::Done`] says what to send at once. A [`Prepared::Check`] needs one call to
//!    Home Assistant's non-executing matcher: [`run`] does it within the mode's limit and
//!    decides, re-reading the context so a stale result is never used. When
//!    [`Check::can_change_text`] is false the heard text will be sent whatever the matcher
//!    says, so an `on` turn need not wait; finish the check beside it for the record.
//! 4. Store [`Outcome::record`] in `turns.stt_fix` ([`FixRecord::to_json`]).
//!
//! # How a correction is found
//!
//! Every rule compares sounds. A word's pronunciation comes from the user's guide, the
//! CMU Pronouncing Dictionary, a number read aloud, letter names for a short token without
//! vowels, dictionary words run together, or, failing all of those, letter-to-sound rules.
//! Similarity is a weighted edit distance over speech segments ([`similarity`]). The rules:
//! number words to digits; leftover wake sounds removed from a wake turn's start; a 1–4
//! word span replaced by an HA name, area, floor or preferred term; one word replaced by a
//! fixed word of HA's intent templates; the whole text replaced by a known or preferred
//! phrase; and a cut-off text completed to the phrase it begins or ends. Searches read only
//! the table rows whose length allows the needed similarity, and compare at most
//! [`RERANK`] of them in full per query.
//!
//! # What makes a correction acceptable
//!
//! Before Home Assistant sees it, a candidate must keep the heard text's negations,
//! numbers (by value), units, on/off words and command verbs, and the words of any known
//! name. Then HA's matcher must fully match it, not only through a free-text slot or
//! wildcard (except for whole-text repairs), an inserted name must resolve to what it
//! names, and every slot HA had resolved from a fixed vocabulary for the heard text (name,
//! area, floor, domain, device class, state, numbers) must keep its value, with the same
//! targets. Text HA already understands is never changed. Interpretations closer than
//! [`AMBIGUITY_MARGIN`] are not chosen between; one with an eligible preferred term or
//! phrase outranks those without.
//!
//! # Attribution
//!
//! The pronunciations are the Carnegie Mellon Pronouncing Dictionary, copyright Carnegie
//! Mellon University, used under its BSD-style licence (`data/cmudict/LICENSE`; source and
//! checksums in `data/SOURCE`).

#![warn(missing_docs)]

mod accept;
mod builder;
mod flow;
mod lexicon;
mod meaning;
mod propose;
mod record;
mod sounds;
mod spelling;
mod tables;
mod text;

#[cfg(test)]
mod tests;

pub use accept::AMBIGUITY_MARGIN;
pub use builder::{BuilderPanicked, TableBuilder};
pub use flow::{
    Check, FINAL_DEADLINE, FixContext, HaContext, Outcome, Prepared, SHADOW_LIMIT, Snapshot,
    TurnText, VALIDATE_LIMIT, prepare, run,
};
pub use lexicon::{
    Guides, Lexicon, MAX_TEXT_VARIANTS, MAX_WORD_VARIANTS, Pronouncer, SoundSource, WordSounds,
};
pub use propose::{
    CUTOFF_MIN_COVERAGE, Candidate, LEAD_IN_MIN, MAX_CHARS, MAX_WORDS, MIN_LETTERS, NAME_MIN,
    PHRASE_MIN, Proposal, Rule, Skipped, WORD_MIN, WORK_LIMIT, propose,
};
pub use record::{
    CandidateRecord, ContextRecord, FixRecord, MAX_RECORDED_CANDIDATES, Reason, RecordMode, Status,
    Verdict, Verdicts, WorkRecord,
};
pub use sounds::{Segment, Sounds, Work, similarity};
pub use tables::{
    ConfigDigest, KnownPhrase, MAX_NAMES, MAX_PHRASES, PhraseSource, Preferences, RERANK, Source,
    TableInputs, Tables, TablesId,
};
pub use text::words;
