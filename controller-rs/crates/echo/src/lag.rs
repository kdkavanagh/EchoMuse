//! Lag estimation: how long after the device played a sound it appears in the microphone
//! stream (post-AFE architecture §16.6 "Reference comparison").
//!
//! The microphone window `x` (mean removed) is correlated against every alignment of the
//! reference `y` from 0 to `lag_max` samples earlier, as a mean-subtracted normalized
//! cross-correlation:
//!
//! ```text
//! ρ(L) = Σ x̃[n]·(y[n − L] − ȳ_L) / (‖x̃‖ · ‖y_L − ȳ_L‖)
//! ```
//!
//! The numerators of every lag come from one FFT cross-correlation; the reference energies
//! come from running sums. A lag is accepted only when the reference under it is non-silent for
//! enough cells and the peak stands clear of every correlation more than 20 ms away from it.
//! Anything else is an explicit unknown with its reason, never a guess.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use crate::policy::{CELL_SAMPLES, EchoPolicy, SAMPLE_RATE};

/// An acoustic lag in samples: microphone sample `m` hears what the device played at reference
/// sample `mic_to_reference(m) − lag`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Lag(pub u32);

impl Lag {
    /// The lag in milliseconds.
    pub fn as_ms(self) -> f64 {
        f64::from(self.0) * 1_000.0 / f64::from(SAMPLE_RATE)
    }
}

/// An accepted lag and how clearly it was found.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LagFound {
    /// The lag.
    pub lag: Lag,
    /// The correlation at the lag.
    pub peak: f64,
    /// The best correlation more than the exclusion zone away from the lag.
    pub runner_up: f64,
    /// Non-silent 32 ms cells of reference under the microphone window at the lag.
    pub active_cells: u32,
}

/// Why no lag was accepted.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum LagUnknown {
    /// Part of the microphone window is missing.
    MicMissing,
    /// Part of the reference the search needs is missing (a gap, a clipped backfill, no
    /// reference stream, or a reference epoch change).
    ReferenceMissing,
    /// The reference is silent everywhere the search looks: the device played nothing.
    ReferenceSilent,
    /// The microphone window is digitally flat: nothing to correlate.
    MicSilent,
    /// The peak's reference has too few non-silent cells.
    TooLittleReference {
        /// How many it had.
        active_cells: u32,
    },
    /// The peak does not stand clear of the rest (periodic sound, or no echo at all).
    NotDistinct {
        /// The best correlation.
        peak: f64,
        /// The best correlation outside the peak's neighbourhood.
        runner_up: f64,
    },
}

/// One lag estimate's result.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum LagOutcome {
    /// A lag was accepted.
    Found(LagFound),
    /// No lag was accepted.
    Unknown(LagUnknown),
}

impl LagOutcome {
    /// The accepted lag, if any.
    pub fn lag(&self) -> Option<Lag> {
        match self {
            Self::Found(f) => Some(f.lag),
            Self::Unknown(_) => None,
        }
    }
}

/// A size `≥ n` whose only prime factors are 2, 3 and 5, so the FFT stays fast.
fn fft_size(n: usize) -> usize {
    let mut size = n.max(2);
    loop {
        let mut m = size;
        for p in [2, 3, 5] {
            while m.is_multiple_of(p) {
                m /= p;
            }
        }
        if m == 1 && size.is_multiple_of(2) {
            return size;
        }
        size += 1;
    }
}

/// The FFT cross-correlator, with its plans and buffers kept between calls: after the first
/// estimate of a given size, an estimate allocates nothing.
pub struct LagEstimator {
    planner: RealFftPlanner<f64>,
    size: usize,
    forward: Option<Arc<dyn RealToComplex<f64>>>,
    inverse: Option<Arc<dyn ComplexToReal<f64>>>,
    a: Vec<f64>,
    b: Vec<f64>,
    spec_a: Vec<Complex<f64>>,
    spec_b: Vec<Complex<f64>>,
    scratch: Vec<Complex<f64>>,
    sum: Vec<f64>,
    sum_sq: Vec<f64>,
    rho: Vec<f64>,
}

impl std::fmt::Debug for LagEstimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LagEstimator")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Default for LagEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl LagEstimator {
    /// An estimator with no plans yet.
    pub fn new() -> Self {
        Self {
            planner: RealFftPlanner::new(),
            size: 0,
            forward: None,
            inverse: None,
            a: Vec::new(),
            b: Vec::new(),
            spec_a: Vec::new(),
            spec_b: Vec::new(),
            scratch: Vec::new(),
            sum: Vec::new(),
            sum_sq: Vec::new(),
            rho: Vec::new(),
        }
    }

    fn plan(&mut self, size: usize) {
        if self.size == size {
            return;
        }
        let forward = self.planner.plan_fft_forward(size);
        let inverse = self.planner.plan_fft_inverse(size);
        let scratch = forward
            .get_scratch_len()
            .max(inverse.get_scratch_len())
            .max(1);
        self.a.resize(size, 0.0);
        self.b.resize(size, 0.0);
        self.spec_a.resize(size / 2 + 1, Complex::default());
        self.spec_b.resize(size / 2 + 1, Complex::default());
        self.scratch.resize(scratch, Complex::default());
        self.forward = Some(forward);
        self.inverse = Some(inverse);
        self.size = size;
    }

    /// Estimates the lag of `mic` (n samples, scaled to ±1) against `reference`, which must hold
    /// `n + lag_max` samples: the reference played from `lag_max` samples before the
    /// microphone window's start (in the clock estimate) to its end. `mic_valid` marks which
    /// microphone samples are real audio; the others take no part in the correlation.
    ///
    /// The caller checks coverage first: both inputs must be fully held.
    pub fn estimate(
        &mut self,
        mic: &[f32],
        mic_valid: Option<&[bool]>,
        reference: &[f32],
        policy: &EchoPolicy,
    ) -> LagOutcome {
        let n = mic.len();
        let lag_max = policy.lag_max as usize;
        assert_eq!(
            reference.len(),
            n + lag_max,
            "reference must cover the search"
        );
        let silence = policy.silence_mean_square();

        // Nothing to find if the device played nothing anywhere in the search.
        let active_anywhere = reference
            .chunks(CELL_SAMPLES)
            .any(|c| mean_square(c) >= silence);
        if !active_anywhere {
            return LagOutcome::Unknown(LagUnknown::ReferenceSilent);
        }

        self.plan(fft_size(n + lag_max));
        let size = self.size;

        // The microphone window with its mean (over valid samples) removed; invalid samples are
        // zero so they add nothing to any lag's correlation.
        let valid = |i: usize| mic_valid.is_none_or(|v| v[i]);
        let (mut total, mut count) = (0.0, 0usize);
        for (i, &s) in mic.iter().enumerate() {
            if valid(i) {
                total += f64::from(s);
                count += 1;
            }
        }
        let mean = if count > 0 { total / count as f64 } else { 0.0 };
        let mut energy = 0.0;
        for (i, slot) in self.a.iter_mut().enumerate() {
            *slot = if i < n && valid(i) {
                let v = f64::from(mic[i]) - mean;
                energy += v * v;
                v
            } else {
                0.0
            };
        }
        if energy <= f64::EPSILON {
            return LagOutcome::Unknown(LagUnknown::MicSilent);
        }
        for (i, slot) in self.b.iter_mut().enumerate() {
            *slot = reference.get(i).map_or(0.0, |&s| f64::from(s));
        }

        // corr[k] = Σ x̃[i]·y[i + k]: no circular wrap, since the transform holds n + lag_max.
        let forward = self.forward.as_ref().expect("planned");
        let inverse = self.inverse.as_ref().expect("planned");
        forward
            .process_with_scratch(&mut self.a, &mut self.spec_a, &mut self.scratch)
            .expect("buffer sizes match the plan");
        forward
            .process_with_scratch(&mut self.b, &mut self.spec_b, &mut self.scratch)
            .expect("buffer sizes match the plan");
        for (a, b) in self.spec_a.iter().zip(self.spec_b.iter_mut()) {
            *b *= a.conj();
        }
        // The DC and Nyquist bins of a real signal's spectrum are real; rounding may leave a
        // trace of imaginary part the inverse transform refuses.
        self.spec_b[0].im = 0.0;
        if let Some(last) = self.spec_b.last_mut() {
            last.im = 0.0;
        }
        inverse
            .process_with_scratch(&mut self.spec_b, &mut self.a, &mut self.scratch)
            .expect("buffer sizes match the plan");

        // Running sums of the reference give each alignment's mean and energy.
        self.sum.clear();
        self.sum_sq.clear();
        self.sum.push(0.0);
        self.sum_sq.push(0.0);
        let (mut s1, mut s2) = (0.0, 0.0);
        for &y in reference {
            let y = f64::from(y);
            s1 += y;
            s2 += y * y;
            self.sum.push(s1);
            self.sum_sq.push(s2);
        }
        let norm = energy.sqrt();
        let scale = 1.0 / size as f64;
        // With gaps in the microphone window the reference's energy is still taken over the
        // whole window, which can only lower a correlation, never raise it.
        self.rho.clear();
        for k in 0..=lag_max {
            let s1 = self.sum[k + n] - self.sum[k];
            let s2 = self.sum_sq[k + n] - self.sum_sq[k];
            let var = (s2 - s1 * s1 / n as f64).max(0.0);
            let r = if var <= f64::EPSILON {
                0.0
            } else {
                self.a[k] * scale / (norm * var.sqrt())
            };
            self.rho.push(r);
        }

        // k = lag_max − L: the largest k is the shortest lag.
        let (k_peak, peak) =
            self.rho
                .iter()
                .copied()
                .enumerate()
                .fold(
                    (0, f64::MIN),
                    |best, (k, r)| if r > best.1 { (k, r) } else { best },
                );
        let exclusion = policy.lag_peak_exclusion as usize;
        let runner_up = self
            .rho
            .iter()
            .enumerate()
            .filter(|&(k, _)| k.abs_diff(k_peak) > exclusion)
            .map(|(_, &r)| r)
            .fold(f64::MIN, f64::max)
            .max(0.0);
        let lag = Lag((lag_max - k_peak) as u32);

        let active_cells = reference[k_peak..k_peak + n]
            .as_chunks::<CELL_SAMPLES>()
            .0
            .iter()
            .filter(|c| mean_square(&c[..]) >= silence)
            .count() as u32;
        if active_cells < policy.lag_min_active_cells {
            return LagOutcome::Unknown(LagUnknown::TooLittleReference { active_cells });
        }
        if peak <= 0.0 || peak - runner_up < policy.lag_peak_margin {
            return LagOutcome::Unknown(LagUnknown::NotDistinct { peak, runner_up });
        }
        LagOutcome::Found(LagFound {
            lag,
            peak,
            runner_up,
            active_cells,
        })
    }
}

/// The mean square of samples scaled to ±1.
pub(crate) fn mean_square(s: &[f32]) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    s.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / s.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth;

    /// Builds a 1 s microphone window that hears `reference_track` through `path` delayed by
    /// `lag` samples, and the matching search reference.
    fn case(lag: usize, path: &[f32], noise_db: f64) -> (Vec<f32>, Vec<f32>) {
        let policy = EchoPolicy::default();
        let n = policy.lag_window as usize;
        let lag_max = policy.lag_max as usize;
        let track = synth::speech_like(n + lag_max + 64, 11, -20.0);
        // The search reference covers track[0 .. n + lag_max]; the mic window starts at track
        // position lag_max, so mic[i] hears track[lag_max + i − lag].
        let echoed = synth::filter(&track, path);
        let noise = synth::white(n, 99, noise_db);
        let mic: Vec<f32> = (0..n)
            .map(|i| echoed[lag_max + i - lag] + noise[i])
            .collect();
        (mic, track[..n + lag_max].to_vec())
    }

    #[test]
    fn fft_sizes_are_smooth() {
        assert_eq!(fft_size(24_000), 24_000);
        assert_eq!(fft_size(24_001), 24_300);
        assert_eq!(fft_size(7), 8);
    }

    /// A pure delay is recovered exactly across the whole 0–500 ms search, including both ends.
    #[test]
    fn pure_delays_are_recovered_exactly() {
        let policy = EchoPolicy::default();
        let mut est = LagEstimator::new();
        for lag in [0, 1, 37, 800, 4_000, 7_999, 8_000] {
            let (mic, reference) = case(lag, &[0.5], -70.0);
            match est.estimate(&mic, None, &reference, &policy) {
                LagOutcome::Found(f) => {
                    assert_eq!(f.lag, Lag(lag as u32));
                    assert!(f.peak > 0.99, "lag {lag}: peak {}", f.peak);
                }
                other => panic!("lag {lag}: {other:?}"),
            }
        }
    }

    /// A loudspeaker-like low-pass path (taps 0.6, 0.3, 0.1: 0.5 samples of group delay) and
    /// noise 30 dB under the echo: the lag is the delay to within ±1 sample.
    #[test]
    fn filtered_delays_are_recovered_within_one_sample() {
        let policy = EchoPolicy::default();
        let mut est = LagEstimator::new();
        for lag in [5, 1_234, 6_400] {
            let (mic, reference) = case(lag, &[0.3, 0.15, 0.05], -56.0);
            let found = est.estimate(&mic, None, &reference, &policy);
            let got = found
                .lag()
                .unwrap_or_else(|| panic!("lag {lag}: {found:?}"));
            assert!(got.0.abs_diff(lag as u32) <= 1, "lag {lag} → {got:?}");
        }
        // A half-sample delay (taps 0.5, 0.5) lands on one of its two neighbours.
        let (mic, reference) = case(3_000, &[0.25, 0.25], -70.0);
        let got = est.estimate(&mic, None, &reference, &policy).lag().unwrap();
        assert!(got == Lag(3_000) || got == Lag(3_001), "{got:?}");
    }

    #[test]
    fn a_silent_reference_has_no_lag() {
        let policy = EchoPolicy::default();
        let n = policy.lag_window as usize;
        let mic = synth::speech_like(n, 3, -30.0);
        let mut est = LagEstimator::new();
        let zeros = vec![0.0; n + policy.lag_max as usize];
        assert_eq!(
            est.estimate(&mic, None, &zeros, &policy),
            LagOutcome::Unknown(LagUnknown::ReferenceSilent)
        );
        // −65 dBFS of hiss is below the −60 dBFS line: still silent.
        let hiss = synth::white(n + policy.lag_max as usize, 5, -65.0);
        assert_eq!(
            est.estimate(&mic, None, &hiss, &policy),
            LagOutcome::Unknown(LagUnknown::ReferenceSilent)
        );
    }

    #[test]
    fn a_short_burst_of_reference_is_too_little() {
        let policy = EchoPolicy::default();
        let (mic, mut reference) = case(2_000, &[0.5], -70.0);
        // Keep only 5 cells (160 ms) of reference under the window at the true lag.
        let keep = 8_000 - 2_000 + 4_096;
        for (i, s) in reference.iter_mut().enumerate() {
            if !(keep..keep + 5 * CELL_SAMPLES).contains(&i) {
                *s = 0.0;
            }
        }
        let mut est = LagEstimator::new();
        assert_eq!(
            est.estimate(&mic, None, &reference, &policy),
            LagOutcome::Unknown(LagUnknown::TooLittleReference { active_cells: 5 })
        );
    }

    #[test]
    fn a_steady_tone_or_unrelated_sound_is_not_distinct() {
        let policy = EchoPolicy::default();
        let n = policy.lag_window as usize;
        let lag_max = policy.lag_max as usize;
        let mut est = LagEstimator::new();
        // A 440 Hz tone correlates equally at every period: no single lag.
        let tone = synth::tone(n + lag_max, 440.0, -20.0);
        let mic: Vec<f32> = tone[lag_max - 1_000..lag_max - 1_000 + n]
            .iter()
            .map(|s| s * 0.5)
            .collect();
        assert!(matches!(
            est.estimate(&mic, None, &tone, &policy),
            LagOutcome::Unknown(LagUnknown::NotDistinct { .. })
        ));
        // A microphone that never heard the reference.
        let other = synth::speech_like(n, 77, -30.0);
        let reference = synth::speech_like(n + lag_max, 78, -20.0);
        assert!(matches!(
            est.estimate(&other, None, &reference, &policy),
            LagOutcome::Unknown(LagUnknown::NotDistinct { .. })
        ));
    }

    #[test]
    fn a_chirp_is_found_and_plans_are_reused() {
        let policy = EchoPolicy::default();
        let n = policy.lag_window as usize;
        let lag_max = policy.lag_max as usize;
        let chirp = synth::chirp(n + lag_max, 200.0, 4_000.0, -20.0);
        let mic: Vec<f32> = chirp[lag_max - 2_500..lag_max - 2_500 + n]
            .iter()
            .map(|s| s * 0.3)
            .collect();
        let mut est = LagEstimator::new();
        for _ in 0..2 {
            assert_eq!(
                est.estimate(&mic, None, &chirp, &policy).lag(),
                Some(Lag(2_500))
            );
        }
        assert_eq!(est.size, fft_size(n + lag_max));
    }
}
