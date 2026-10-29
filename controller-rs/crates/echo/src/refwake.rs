//! Reference wake scoring: did the device's own output contain something wake-like? (Post-AFE
//! architecture §5.2 "The reference scorer runs on the controller, on demand", §5.3, §6.1 step
//! 1.)
//!
//! The reference scoring worker runs the deployed BCResNet graph over the uploaded 16 kHz
//! final-mix reference with the same preparation and three-window mean as the device's
//! microphone scorer. Its hop grid is fixed to the reference stream: windows of 22,400 samples
//! end at reference sample indices that are multiples of 2,560, and the smoothing history
//! starts empty at the subscription's first reference sample. A reference candidate opens when
//! the mean of the last three raw scores reaches 0.30, extends while the smoothed score stays at
//! or above half of that, and closes after two scored hops below; a window over missing
//! reference closes it and clears the history, exactly as a gap does on the device.
//!
//! For a microphone candidate, [`ReferenceWakeScorer::judge`] maps every reference candidate
//! into microphone time through the clock estimate plus the acoustic lag, each side widened by
//! one hop (both supports carry ±160 ms of hop uncertainty), and answers with the speech
//! policy's [`ReferenceScore`]: an overlapping candidate, none, or unavailable when the
//! reference that could have overlapped was not all scored.

use std::collections::VecDeque;

use echomuse_speech::{BcresnetScorer, SpeechError, WindowScore};
use echomuse_turn::{Epoch, ReferenceScore, SampleSpan};
use serde::{Deserialize, Serialize};

use crate::clock::Alignment;
use crate::policy::{EchoPolicy, ReferenceWakePolicy, WAKE_HOP_SAMPLES, WAKE_WINDOW_SAMPLES};
use crate::store::{Read, SampleStore};

/// Reference kept for scoring: 10 s, so a whole candidate backfill (2.5 s of reference before
/// the microphone's start plus the microphone's own backfill) can arrive before the worker
/// scores it. Scoring after every delivery keeps far less than this outstanding.
const STORE_CAPACITY: usize = 160_000;
/// Hops kept for judging and the trace: 256 (41 s).
const MAX_HOPS: usize = 256;
/// Candidates kept: 64.
const MAX_CANDIDATES: usize = 64;

/// Scores one 1.4 s window of 16 kHz PCM: the wake graph, or a stand-in in tests.
pub trait WakeWindowScorer {
    /// What a failed inference reports.
    type Error: std::error::Error + 'static;
    /// Prepares and scores `pcm` (22,400 samples).
    fn score_window(&mut self, pcm: &[i16]) -> Result<WindowScore, Self::Error>;
}

impl WakeWindowScorer for BcresnetScorer {
    type Error = SpeechError;
    fn score_window(&mut self, pcm: &[i16]) -> Result<WindowScore, SpeechError> {
        self.score(pcm)
    }
}

/// What one hop of the reference produced.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum HopOutcome {
    /// The graph ran.
    Scored {
        /// The window's wake probability.
        raw: f64,
        /// The mean of the scores in the smoothing history.
        smoothed: f64,
    },
    /// The window's RMS was below the scorer's floor (silence): not scored.
    BelowRmsFloor,
    /// The window spanned missing reference, or inference failed: no value, history cleared.
    Invalid,
}

/// One hop of the reference, for the trace.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceHop {
    /// The window's end, a reference sample index (exclusive).
    pub end: u64,
    /// What it produced.
    pub outcome: HopOutcome,
}

/// A reference candidate, in reference samples.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceCandidate {
    /// The start of the oldest window whose score opened it.
    pub support_start: u64,
    /// The end of the hop at which it opened.
    pub first_crossing_end: u64,
    /// The end of the last window at or above the threshold when it opened.
    pub support_end_at_open: u64,
    /// The end of the hop at which it closed, while known.
    pub closed_at: Option<u64>,
    /// The highest smoothed score while open.
    pub peak: f64,
}

/// Why the reference could not be judged.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceUnavailable {
    /// The scorer holds a different reference epoch than the alignment, or none.
    EpochMismatch,
    /// The reference does not reach back to the first window the candidate needs.
    NotEnoughHistory,
    /// A window the candidate needs spanned missing reference or failed to score.
    InvalidWindow,
    /// The reference ended before the windows the candidate needs.
    Ended,
}

/// The reference scorer's evidence for one microphone candidate.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceJudgement {
    /// The wake decision's input.
    pub score: ReferenceScore,
    /// Why it is unavailable, when it is.
    pub unavailable: Option<ReferenceUnavailable>,
    /// The overlapping reference candidate, when there is one.
    pub candidate: Option<ReferenceCandidate>,
    /// The first and last reference window ends the judgement read.
    pub windows: Option<(u64, u64)>,
}

impl ReferenceJudgement {
    fn unavailable(why: ReferenceUnavailable, windows: Option<(u64, u64)>) -> Self {
        Self {
            score: ReferenceScore::Unavailable,
            unavailable: Some(why),
            candidate: None,
            windows,
        }
    }
}

/// The reference scoring worker's state for one subscription.
pub struct ReferenceWakeScorer<S> {
    scorer: S,
    policy: ReferenceWakePolicy,
    epoch: Option<Epoch>,
    store: SampleStore<i16>,
    /// The end of the first full window after the subscription's first reference sample.
    first_end: Option<u64>,
    /// The end of the next window to score.
    next_end: Option<u64>,
    /// The raw scores in the smoothing mean, oldest first, with their window ends.
    scores: VecDeque<(u64, f64)>,
    unscored: u32,
    below: u32,
    open: bool,
    candidates: VecDeque<ReferenceCandidate>,
    hops: VecDeque<ReferenceHop>,
    window: Vec<i16>,
}

impl<S> std::fmt::Debug for ReferenceWakeScorer<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceWakeScorer")
            .field("epoch", &self.epoch)
            .field("next_end", &self.next_end)
            .field("candidates", &self.candidates)
            .finish_non_exhaustive()
    }
}

/// The smallest multiple of the hop at or above `x`.
fn grid_ceil(x: i64) -> i64 {
    let h = WAKE_HOP_SAMPLES as i64;
    x.div_euclid(h) * h + if x.rem_euclid(h) == 0 { 0 } else { h }
}

/// The largest multiple of the hop at or below `x`.
fn grid_floor(x: i64) -> i64 {
    let h = WAKE_HOP_SAMPLES as i64;
    x.div_euclid(h) * h
}

impl<S: WakeWindowScorer> ReferenceWakeScorer<S> {
    /// A scorer with an empty history.
    pub fn new(scorer: S, policy: ReferenceWakePolicy) -> Self {
        Self {
            scorer,
            policy,
            epoch: None,
            store: SampleStore::new(STORE_CAPACITY),
            first_end: None,
            next_end: None,
            scores: VecDeque::with_capacity(3),
            unscored: 0,
            below: 0,
            open: false,
            candidates: VecDeque::with_capacity(MAX_CANDIDATES),
            hops: VecDeque::with_capacity(MAX_HOPS),
            window: vec![0; WAKE_WINDOW_SAMPLES],
        }
    }

    /// The window scorer.
    pub fn scorer_mut(&mut self) -> &mut S {
        &mut self.scorer
    }

    /// Forgets the reference, the smoothing history, the hops and the candidates: on mute,
    /// session loss, or when the subscription ends.
    pub fn reset(&mut self) {
        self.epoch = None;
        self.store.clear();
        self.first_end = None;
        self.next_end = None;
        self.clear_history();
        self.open = false;
        self.candidates.clear();
        self.hops.clear();
    }

    fn clear_history(&mut self) {
        self.scores.clear();
        self.unscored = 0;
        self.below = 0;
    }

    /// Starts over at a new reference epoch; the same epoch changes nothing.
    fn use_epoch(&mut self, epoch: Epoch) {
        if self.epoch != Some(epoch) {
            self.reset();
            self.epoch = Some(epoch);
        }
    }

    fn begin(&mut self, first: u64) {
        if self.first_end.is_none() {
            let end = grid_ceil((first + WAKE_WINDOW_SAMPLES as u64) as i64) as u64;
            self.first_end = Some(end);
            self.next_end = Some(end);
        }
    }

    /// Adds reference PCM of `epoch` from reference sample `first`.
    pub fn push_pcm(&mut self, epoch: Epoch, first: u64, samples: impl IntoIterator<Item = i16>) {
        self.use_epoch(epoch);
        self.begin(first);
        self.store.append(first, 1, samples);
    }

    /// Adds `frames` samples the device flagged as digital silence.
    pub fn push_silence(&mut self, epoch: Epoch, first: u64, frames: u64) {
        self.use_epoch(epoch);
        self.begin(first);
        self.store.append_fill(first, frames, 1, 0);
    }

    /// Records reference samples `[first, end)` that will never arrive.
    pub fn push_missing(&mut self, epoch: Epoch, first: u64, end: u64) {
        self.use_epoch(epoch);
        self.begin(first);
        self.store.mark_missing(first, end);
    }

    /// Declares that no further reference of the current epoch will arrive.
    pub fn reference_ended(&mut self) {
        self.store.close();
    }

    /// The reference epoch being scored.
    pub fn epoch(&self) -> Option<Epoch> {
        self.epoch
    }

    /// The hops scored, oldest first.
    pub fn hops(&self) -> impl Iterator<Item = &ReferenceHop> {
        self.hops.iter()
    }

    /// The reference candidates, oldest first.
    pub fn candidates(&self) -> impl Iterator<Item = &ReferenceCandidate> {
        self.candidates.iter()
    }

    fn record(&mut self, end: u64, outcome: HopOutcome) {
        if self.hops.len() == MAX_HOPS {
            self.hops.pop_front();
        }
        self.hops.push_back(ReferenceHop { end, outcome });
    }

    /// A window over missing reference or a failed inference: like a gap on the device, it
    /// closes any open candidate and clears the smoothing history.
    fn invalid(&mut self, end: u64) {
        self.record(end, HopOutcome::Invalid);
        self.close(end);
        self.clear_history();
    }

    fn close(&mut self, end: u64) {
        if self.open {
            self.open = false;
            if let Some(c) = self.candidates.back_mut() {
                c.closed_at = Some(end);
            }
        }
    }

    /// Scores every hop whose window has fully arrived, in order. Returns how many hops it
    /// processed. An inference error is returned after its hop is recorded as invalid; calling
    /// again continues with the next hop.
    pub fn advance(&mut self) -> Result<usize, S::Error> {
        let mut done = 0;
        while let Some(end) = self.next_end {
            let start = end - WAKE_WINDOW_SAMPLES as u64;
            match self.store.state(start as i64, end as i64) {
                Read::Pending => break,
                Read::Missing => {
                    if self.store.end() < end && self.store.is_closed() {
                        // Past the end of a closed reference: nothing more will be scored.
                        break;
                    }
                    self.invalid(end);
                }
                Read::Held { .. } => {
                    self.store.copy_to(start as i64, &mut self.window);
                    let score = if self.window.iter().all(|&s| s == 0) {
                        // Digital silence is below the floor; the graph need not run.
                        Ok(WindowScore::BelowRmsFloor)
                    } else {
                        self.scorer.score_window(&self.window)
                    };
                    match score {
                        Ok(WindowScore::BelowRmsFloor) => self.unscored_hop(end),
                        Ok(WindowScore::Scored { probability }) if probability.is_finite() => {
                            self.scored_hop(end, probability)
                        }
                        Ok(WindowScore::Scored { .. }) => self.invalid(end),
                        Err(e) => {
                            self.invalid(end);
                            self.next_end = Some(end + WAKE_HOP_SAMPLES);
                            return Err(e);
                        }
                    }
                }
            }
            self.next_end = Some(end + WAKE_HOP_SAMPLES);
            done += 1;
        }
        Ok(done)
    }

    fn unscored_hop(&mut self, end: u64) {
        self.record(end, HopOutcome::BelowRmsFloor);
        self.unscored += 1;
        if self.unscored >= self.policy.clear_after_unscored {
            self.scores.clear();
            self.unscored = 0;
        }
    }

    fn scored_hop(&mut self, end: u64, raw: f64) {
        self.unscored = 0;
        if self.scores.len() == self.policy.smoothing_windows {
            self.scores.pop_front();
        }
        self.scores.push_back((end, raw));
        let smoothed = self.scores.iter().map(|s| s.1).sum::<f64>() / self.scores.len() as f64;
        self.record(end, HopOutcome::Scored { raw, smoothed });
        let threshold = self.policy.threshold;
        if self.open {
            let c = self
                .candidates
                .back_mut()
                .expect("an open candidate exists");
            c.peak = c.peak.max(smoothed);
            if smoothed >= threshold * self.policy.extend_fraction {
                self.below = 0;
            } else {
                self.below += 1;
                if self.below >= self.policy.close_after_below {
                    self.close(end);
                }
            }
            return;
        }
        // The baseline rule: the mean of the last (up to) three raw scores reaches the
        // threshold.
        if smoothed >= threshold {
            let oldest = self.scores.front().expect("just pushed").0;
            let support_end_at_open = self
                .scores
                .iter()
                .filter(|s| s.1 >= threshold)
                .map(|s| s.0)
                .max()
                .unwrap_or(end);
            if self.candidates.len() == MAX_CANDIDATES {
                self.candidates.pop_front();
            }
            self.candidates.push_back(ReferenceCandidate {
                support_start: oldest - WAKE_WINDOW_SAMPLES as u64,
                first_crossing_end: end,
                support_end_at_open,
                closed_at: None,
                peak: smoothed,
            });
            self.open = true;
            self.below = 0;
        }
    }

    /// The end of the last window at or above the threshold up to `limit`, for a candidate.
    fn support_end(&self, c: &ReferenceCandidate, limit: u64) -> u64 {
        let until = c.closed_at.map_or(limit, |e| e.min(limit));
        self.hops
            .iter()
            .filter(|h| h.end > c.first_crossing_end && h.end <= until)
            .filter(|h| matches!(h.outcome, HopOutcome::Scored { raw, .. } if raw >= self.policy.threshold))
            .map(|h| h.end)
            .fold(c.support_end_at_open, u64::max)
    }

    /// Judges a microphone candidate whose support is `mic_support` (from its support start to
    /// its open) against the reference, through `alignment`. `None` while the reference windows
    /// it needs have not all been scored.
    ///
    /// The windows read are those ending after the support start (mapped to the reference,
    /// minus the longest lag and one hop) and no later than the open (mapped, minus the shortest
    /// lag, plus one hop): the reference the device played up to the moment its microphone
    /// window opened the candidate, with one hop of grid uncertainty. A wake-like sound the
    /// device rendered that could have made its own microphone open the candidate lies in
    /// those windows; waiting for later ones would hold every decision for another 1.4 s.
    /// With a lag established the lag range is the lag ±20 ms (the correlation's own
    /// resolution, [`EchoPolicy::lag_peak_exclusion`]); without one it is the whole 0–500 ms
    /// search.
    pub fn judge(
        &self,
        mic_support: SampleSpan,
        alignment: &Alignment,
        echo: &EchoPolicy,
    ) -> Option<ReferenceJudgement> {
        if self.epoch != Some(alignment.reference_epoch) {
            return Some(ReferenceJudgement::unavailable(
                ReferenceUnavailable::EpochMismatch,
                None,
            ));
        }
        let lag_max = i64::from(echo.lag_max);
        let (lo, hi) = match alignment.lag {
            Some(l) => {
                let l = i64::from(l.0);
                let w = i64::from(echo.lag_peak_exclusion);
                ((l - w).max(0), (l + w).min(lag_max))
            }
            None => (0, lag_max),
        };
        let hop = WAKE_HOP_SAMPLES as i64;
        let r_start = alignment
            .mic_to_reference(mic_support.start.0 as f64)
            .floor() as i64;
        let r_open = alignment.mic_to_reference(mic_support.end.0 as f64).ceil() as i64;
        // Window ends strictly after r_start − hi − hop, and at or before r_open − lo + hop.
        let first = grid_floor(r_start - hi - hop) + hop;
        let last = grid_floor(r_open - lo + hop).max(first);
        let windows = (first.max(0) as u64, last.max(0) as u64);
        let Some(first_end) = self.first_end else {
            return self
                .store
                .is_closed()
                .then_some(ReferenceJudgement::unavailable(
                    ReferenceUnavailable::Ended,
                    Some(windows),
                ));
        };
        let first_end = first_end as i64;
        if first < first_end {
            return Some(ReferenceJudgement::unavailable(
                ReferenceUnavailable::NotEnoughHistory,
                Some(windows),
            ));
        }
        let scored_through = self.next_end? as i64 - hop;
        if scored_through < last {
            if self.store.is_closed() && self.store.end() as i64 <= last {
                return Some(ReferenceJudgement::unavailable(
                    ReferenceUnavailable::Ended,
                    Some(windows),
                ));
            }
            return None;
        }
        let needed = |h: &&ReferenceHop| (first..=last).contains(&(h.end as i64));
        let held = self.hops.iter().filter(needed).count() as i64;
        let invalid = self
            .hops
            .iter()
            .filter(needed)
            .any(|h| h.outcome == HopOutcome::Invalid);
        if invalid || held != (last - first) / hop + 1 {
            return Some(ReferenceJudgement::unavailable(
                ReferenceUnavailable::InvalidWindow,
                Some(windows),
            ));
        }
        let mic = SampleSpan::new(mic_support.start, mic_support.end);
        let overlapping = self
            .candidates
            .iter()
            .filter(|c| c.first_crossing_end as i64 <= last)
            .find(|c| {
                let end = self.support_end(c, last as u64);
                let from = alignment.reference_to_mic(c.support_start as f64) + (lo - hop) as f64;
                let to = alignment.reference_to_mic(end as f64) + (hi + hop) as f64;
                from < mic.end.0 as f64 && (mic.start.0 as f64) < to
            })
            .copied();
        Some(ReferenceJudgement {
            score: if overlapping.is_some() {
                ReferenceScore::Overlap
            } else {
                ReferenceScore::NoOverlap
            },
            unavailable: None,
            candidate: overlapping,
            windows: Some(windows),
        })
    }
}
