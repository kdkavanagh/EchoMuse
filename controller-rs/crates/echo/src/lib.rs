//! Echo attribution: whether microphone audio is only the device's own output, and whether that
//! output itself contained the wake word (Rust controller evaluation §3.1.6, §4.1, §4.6, §5.1,
//! §6.1; post-AFE architecture §5.2, §5.3, §6.1 and §16.6).
//!
//! While a device makes sound, its microphone hears that sound back, after the native AFE has
//! cancelled what it can. This crate gives the speech policy (`echomuse-turn`) the evidence it
//! needs to tell that echo from a person:
//!
//! - [`EchoAnalyzer`], held by the **microphone analysis worker**, places the uploaded
//!   final-mix reference on the microphone timeline through the device's clock stamps
//!   ([`clock`]), measures the remaining acoustic lag by FFT cross-correlation over a 0–500 ms
//!   search ([`lag`]), and labels every 32 ms cell ([`CellLabel`] → the speech policy's
//!   `EchoCell`) and every wake candidate's support ([`CandidateComparison`] → `CandidateEcho`)
//!   with the three-way rule of [`compare`]: `echo_only`, `near_end_present` (double talk), or
//!   unknown, plus `no_reference` while the device plays nothing.
//! - [`ReferenceWakeScorer`], held by the **reference scoring worker**, runs the wake model over
//!   the reference on its own hop grid and judges a microphone candidate
//!   ([`ReferenceJudgement`] → the speech policy's `ReferenceScore`).
//!
//! Both are plain owned state: no threads, no I/O, no clock reads. Each result depends only on
//! the inputs, never on the order they arrived in, and missing reference evidence is reported
//! as unknown or unavailable, never as silence.
//!
//! # From the workers to the wake decision and the endpoint
//!
//! | Produced | By | The speech policy's input |
//! |---|---|---|
//! | [`CellLabel::echo`] | [`EchoAnalyzer::poll`] | `EvidenceAssembler::push_echo` |
//! | [`CandidateComparison::echo`] | [`EchoAnalyzer::compare_candidate`] | `WakeDecider::echo_compared` |
//! | [`ReferenceJudgement::score`] | [`ReferenceWakeScorer::judge`] with [`EchoAnalyzer::alignment`] | `WakeDecider::reference_scored` |
//!
//! The thresholds, and where each comes from, are in [`policy`].

pub mod analyzer;
pub mod clock;
pub mod compare;
pub mod lag;
pub mod policy;
pub mod refwake;
mod store;

#[cfg(test)]
mod synth;
#[cfg(test)]
mod tests;

pub use analyzer::{
    CandidateComparison, CellLabel, EchoAnalyzer, EchoError, EchoPlan, LagEstimate, OutputCoverage,
};
pub use clock::{Alignment, ClockFit};
pub use compare::{CellMeasure, Undecided, Verdict};
pub use lag::{Lag, LagEstimator, LagFound, LagOutcome, LagUnknown};
pub use policy::{EchoPolicy, ReferenceWakePolicy};
pub use refwake::{
    HopOutcome, ReferenceCandidate, ReferenceHop, ReferenceJudgement, ReferenceUnavailable,
    ReferenceWakeScorer, WakeWindowScorer,
};
