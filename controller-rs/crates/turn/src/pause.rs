//! Pause transcription by a Wyoming server (Rust controller evaluation §4.7, `pauseAsr`):
//! the values the policy takes and records for it.
//!
//! At each pause the speech worker re-decodes the utterance with Kroko, and the runtime can
//! send the same audio to a speech-to-text server. The policy is told when that request
//! went out ([`crate::TurnPolicy::pause_server_asked`]) and what came back
//! ([`crate::TurnPolicy::pause_server_answer`]). An answer counts from the first recognizer
//! result pushed after it, and only while that pause's decode still stands; its words then
//! stand in for Kroko's as the judged text. Each pause's words from both transcribers are
//! recorded as a [`PauseRecord`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::time::Sample;

/// Whose words a judged text is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextSource {
    /// Kroko's live streaming result.
    Streaming,
    /// Kroko's fresh decode at a pause.
    Kroko,
    /// The pause server's transcript of that pause.
    Server,
}

/// Why the pause server gave no words.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerFailure {
    /// No answer within the limit.
    Timeout,
    /// The server could not be reached, broke the protocol or reported an error: what.
    Failed(String),
}

/// The pause server's answer for the pause decode whose audio ended at `pause`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PauseServerAnswer {
    /// The end of the audio it transcribed: the pause decode's `through`.
    pub pause: Sample,
    /// Its transcript as given (wake word included), or why there is none.
    pub outcome: Result<String, ServerFailure>,
}

/// Why a transcriber's words at a pause are missing. Stored as a bare string:
/// `superseded`, `timeout`, or the failure's own text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PauseError {
    /// Speech went on (or a later pause sent another request) before it answered.
    Superseded,
    /// No answer within the limit.
    Timeout,
    /// What went wrong.
    Failed(String),
}

impl PauseError {
    /// The stored text.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Superseded => "superseded",
            Self::Timeout => "timeout",
            Self::Failed(what) => what,
        }
    }
}

impl From<ServerFailure> for PauseError {
    fn from(failure: ServerFailure) -> Self {
        match failure {
            ServerFailure::Timeout => Self::Timeout,
            ServerFailure::Failed(what) => Self::Failed(what),
        }
    }
}

impl Serialize for PauseError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PauseError {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Ok(match text.as_str() {
            "superseded" => Self::Superseded,
            "timeout" => Self::Timeout,
            _ => Self::Failed(text),
        })
    }
}

/// One transcriber's words at one pause.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseWords {
    /// Its transcript of the utterance through the pause, wake word included, trimmed;
    /// `None` while it gave none.
    pub text: Option<String>,
    /// From the request to the answer, or to its being dropped, in milliseconds; `None`
    /// when not measured.
    pub ms: Option<u64>,
    /// The text judged on those words (wake phrase removed, normalized); `None` when they
    /// never were.
    pub judged: Option<String>,
    /// The block from which they were judged.
    pub at: Option<Sample>,
    /// Why there are no words.
    pub error: Option<PauseError>,
}

impl PauseWords {
    /// Whether nothing has come back for it yet.
    pub fn is_open(&self) -> bool {
        self.text.is_none() && self.error.is_none()
    }
}

/// One pause: Kroko's fresh decode of the utterance through `through`, and the pause
/// server's transcript of the same audio.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseRecord {
    /// The end of the audio both transcribed.
    pub through: Sample,
    /// Kroko's words.
    pub kroko: PauseWords,
    /// The server's words; `None` when Kroko transcribes alone.
    pub server: Option<PauseWords>,
}
