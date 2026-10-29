//! Completeness: whether the words heard so far look like a whole request (§4.6).
//!
//! The local grammar judges text synchronously through the [`Completeness`] trait; this crate
//! does not implement the grammar. Text the grammar leaves [`TextClass::Unknown`] may be sent to
//! HA's sentence matcher, whose answer comes back asynchronously as a [`RecognizerAnswer`] and
//! takes effect at the next block boundary (post-AFE §16.6 "HA's sentence matcher").

use serde::{Deserialize, Serialize};

/// How complete a text looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextClass {
    /// A whole request: the shortest pause ends it.
    Complete,
    /// A whole request that commonly goes on ("turn on the kitchen" beside "kitchen lights").
    Extendable,
    /// Clearly unfinished ("set a timer for").
    NeedsMore,
    /// The grammar cannot judge it.
    Unknown,
}

/// The local grammar, as the reducer sees it.
pub trait Completeness {
    /// Judges a normalized, wake-cut streaming text. Must be pure and deterministic.
    fn classify(&self, text: &str) -> TextClass;
}

/// A grammar that judges nothing: every text is [`TextClass::Unknown`].
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGrammar;

impl Completeness for NoGrammar {
    fn classify(&self, _text: &str) -> TextClass {
        TextClass::Unknown
    }
}

/// Why the sentence matcher gave no class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecognizerError {
    /// HA is disconnected or the feature is unavailable.
    Unavailable,
    /// The request failed.
    Failed,
    /// The answer came more than 2 s after the question.
    Timeout,
    /// No question about this text was outstanding.
    NotAsked,
}

/// HA's sentence matcher's answer about one text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecognizerAnswer {
    /// The text that was asked about, exactly as the reducer asked.
    pub text: String,
    /// The class, or why there is none.
    pub outcome: Result<TextClass, RecognizerError>,
}
