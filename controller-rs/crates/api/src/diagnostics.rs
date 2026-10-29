//! What the diagnostic and speech routes need from the integrator: transcript
//! correction's trial and its word list, and the scoring of saved clips' wake windows
//! (spec §4.7, §4.11).
//!
//! The routes check the caller's role and the request's shape themselves; the services
//! answer [`ServiceError`] (or [`TextFixError::NotReady`]) for everything else. No method
//! blocks the caller's runtime: model calls run on the integrator's own threads.

use std::{fmt, sync::Arc};

use echomuse_core::types::DeviceId;
use echomuse_protocol::Sha256;
use serde::Serialize;
use serde_json::Value;

use crate::services::{BoxFuture, ServiceError};

// ─── Transcript correction ──────────────────────────────────────────────────────────

/// `POST /api/text_fix/trial`: a transcript to try as a standalone command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextFixTrialRequest {
    /// What speech to text heard, without the wake phrase.
    pub text: String,
    /// Whether it follows the wake word (leftover wake sounds at its start may go).
    pub wake: bool,
    /// The speaker whose settings apply; `None` for the fleet's.
    pub device: Option<DeviceId>,
}

/// How one word of a trial is pronounced, as the dashboard's `Sounded` lists it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SoundedWord {
    /// The word, as correction spells it.
    pub word: String,
    /// Its pronunciations in ARPAbet, the most common first; empty when it has none.
    pub phonemes: Vec<String>,
    /// Where they came from (`guide`, `dictionary`, `number`, `letters`, `compound`,
    /// `spelling`); `None` when the word has no pronunciation.
    pub source: Option<&'static str>,
}

/// A command the controller handles itself, which correction never sees.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalRoute {
    /// "Say that again".
    Repeat,
    /// Setting or cancelling an alarm.
    Alarm,
    /// A question about alarms.
    AlarmQuery,
    /// Cancelling a timer on this speaker.
    TimerCancel,
}

/// What a trial found, in the shape the dashboard's `TextFixTester` reads.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TextFixTrial {
    /// The text tried.
    pub text: String,
    /// The speaker's (or the fleet's) configured mode: `off`, `shadow` or `on`.
    pub mode: &'static str,
    /// The local command the text is, if any.
    pub route: Option<LocalRoute>,
    /// The correction record as `on` would make it (the `turns.stt_fix` shape); `None`
    /// when no record was made.
    pub record: Option<Value>,
    /// How each heard word is pronounced.
    pub heard_words: Vec<SoundedWord>,
    /// How each word of the proposal is pronounced; `None` without a proposal.
    pub proposed_words: Option<Vec<SoundedWord>>,
    /// Whether Home Assistant's device registry holds the speaker's satellite; `None` in
    /// the fleet view, or when HA could not be asked.
    pub satellite: Option<bool>,
    /// The language HA's matcher was asked in; `None` when not known.
    pub language: Option<String>,
    /// The wake phrase whose leftovers may be removed; `None` when not a wake turn.
    pub wake_phrase: Option<String>,
}

/// One word the tables could only guess a pronunciation for.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UnknownWord {
    /// The word.
    pub word: String,
    /// How many names, phrases and template words use it.
    pub count: usize,
}

/// Why a correction request was not answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextFixError {
    /// The tables are being rebuilt or Home Assistant is reconnecting; ask again soon
    /// (503, code `not_ready`).
    NotReady(String),
    /// Anything else.
    Service(ServiceError),
}

impl fmt::Display for TextFixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotReady(why) => f.write_str(why),
            Self::Service(error) => error.fmt(f),
        }
    }
}

impl From<ServiceError> for TextFixError {
    fn from(error: ServiceError) -> Self {
        Self::Service(error)
    }
}

/// Transcript correction, for the dashboard's settings panel.
pub trait TextFixService: Send + Sync + 'static {
    /// What correction would send for this text, as `on` would, under the saved
    /// settings. Only Home Assistant's non-executing sentence matcher is asked.
    fn trial(
        &self,
        request: TextFixTrialRequest,
    ) -> BoxFuture<'_, Result<TextFixTrial, TextFixError>>;

    /// The words of HA's names and the template words whose pronunciation is only
    /// guessed from their spelling under the speaker's (or the fleet's) guides, most
    /// used first.
    fn unknown_words(
        &self,
        device: Option<DeviceId>,
    ) -> BoxFuture<'_, Result<Vec<UnknownWord>, TextFixError>>;
}

// ─── Clip windows ───────────────────────────────────────────────────────────────────

/// Which saved clip.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ClipKind {
    /// A turn's wake clip (`turns.wake_windows`).
    WakeClip,
    /// A near miss's recording (`near_misses.windows`).
    NearMiss,
}

/// One saved clip, by its row.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ClipRef {
    /// What kind of clip.
    pub kind: ClipKind,
    /// Its device.
    pub device: DeviceId,
    /// Its row id.
    pub id: i64,
}

/// The wake model's windows over each saved clip, scored once in the background.
pub trait ClipScoring: Send + Sync + 'static {
    /// The saved clips still to score; `None` while that is not known (the scorer has
    /// not counted yet, or cannot run).
    fn pending(&self) -> Option<u64>;

    /// Scores, now, each of `clips` that has no windows yet, and stores them. It
    /// finishes once every clip is scored or cannot be; a clip that cannot be (its file
    /// is gone, no wake model can score it) is left without windows. Fails only when
    /// scoring cannot run at all, with the reason.
    fn score_now(&self, clips: Vec<ClipRef>) -> BoxFuture<'_, Result<(), ServiceError>>;
}

// ─── Together ───────────────────────────────────────────────────────────────────────

/// The diagnostic and speech services the routes call.
#[derive(Clone)]
pub struct Diagnostics {
    /// Transcript correction.
    pub text_fix: Arc<dyn TextFixService>,
    /// Clip-window scoring.
    pub clip_scoring: Arc<dyn ClipScoring>,
    /// The runtime asset every `session.ready` names (`ORT_ANDROID_LIB`'s SHA-256);
    /// `None` when not known.
    pub runtime_asset: Option<Sha256>,
}

impl fmt::Debug for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Diagnostics")
            .field("runtime_asset", &self.runtime_asset)
            .finish_non_exhaustive()
    }
}
