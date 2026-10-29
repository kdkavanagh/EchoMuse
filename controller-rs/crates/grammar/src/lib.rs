//! Local language handling: whether a transcript is a complete request, and the commands
//! the controller handles without Home Assistant (spec §4.4, §4.5, §4.6).
//!
//! # Pieces
//!
//! - **Normalization** ([`normalize()`]): one spelling of what was said: lowercase, no
//!   punctuation, number words as digits, contractions written out.
//! - **Templates.** Home Assistant's English intent templates (home-assistant-intents
//!   2026.7.30, the release HA 2026.8.1 pins, vendored under `data/`) and the controller's own
//!   command templates, both in hassil's template language, compiled into one word automaton.
//!   The lists HA fills from its registries (`name`, `area`, `floor`) take a [`Vocabulary`];
//!   while it is not known, and for wildcard lists, any words fill them.
//! - **Completeness** ([`Grammar::classify`]): [`Completeness::Complete`],
//!   [`Completeness::Extendable`], [`Completeness::NeedsMore`] or [`Completeness::Unknown`],
//!   judged only for the requests the grammar knows itself (timers, the controller's own
//!   commands, turning a known target on or off). Every other text is `Unknown`, for Home
//!   Assistant's sentence matcher to judge. [`Grammar::reads_whole`] says whether any
//!   template, Home Assistant's included, reads a text whole (transcript correction).
//! - **Context commands** ([`Grammar::context_command`]): `stop`, `cancel`, `snooze` and
//!   `stop the pasta timer` while an alert rings, `stop` while the reply plays, `stop` and
//!   `pause` while media plays. These run before STT.
//! - **Routing after STT** ([`Grammar::route`]): repeat, set or cancel an alarm, alarm
//!   questions, cancelling this speaker's timers, in that order; everything else goes to HA.
//!   Alarm requests map onto the alert service's requests ([`AlarmRequest::set_alarm`],
//!   [`AlarmCancelPlan::request`]); timer cancels onto HA's `HassCancelTimer` and
//!   `HassCancelAllTimers` ([`SpeakerTimers::plan`]).
//! - **Questions** ([`Grammar::meridiem`], [`Grammar::which`]): the answers to "AM or PM?" and
//!   "Which one?".
//! - **Lines** ([`Line`]): what the controller says itself.
//!
//! The wake phrase is not handled here: the turn removes it from the text (architecture
//! §16.6) before any of these functions see it.
//!
//! Every function is pure and deterministic. [`Grammar::new`] compiles the templates once;
//! clones share them.
//!
//! # Attribution
//!
//! The template data is home-assistant-intents (Apache-2.0); the template language and the
//! matcher's semantics follow hassil (Apache-2.0). Both licences and their sources are in
//! `data/`.

#![warn(missing_docs)]

mod alarm;
mod grammar;
mod intents;
mod lines;
mod local;
mod matcher;
pub mod normalize;
mod program;
mod reply;
mod template;
mod timer;
mod vocabulary;

#[cfg(test)]
mod tests;

pub use alarm::{
    AlarmCancel, AlarmCancelPlan, AlarmChoice, AlarmDay, AlarmQuery, AlarmRequest, DayError,
    Meridiem, PendingAlarm, SaidTime,
};
pub use grammar::{
    CommandContext, Completeness, ContextCommand, Grammar, ReplyKind, RingingAlert, Route,
};
pub use intents::INTENTS_VERSION;
pub use lines::{Line, SpokenAlarm, SpokenDay, SpokenTimer, spoken_day, spoken_time};
pub use normalize::{Normalized, normalize};
pub use program::CompileError;
pub use reply::{Choice, TimerPick, WhichReply};
pub use template::{TemplateError, expand as expand_template};
pub use timer::{ChosenTimers, SpeakerTimers, TimerCancel, TimerCancelPlan, TimerCancelRun};
pub use vocabulary::Vocabulary;
