//! Deterministic synthetic signals for the tests, all scaled to ±1.

use std::f64::consts::PI;

/// A deterministic generator (splitmix64).
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Roughly Gaussian: the sum of four uniforms, unit variance.
    pub(crate) fn gauss(&mut self) -> f64 {
        let s: f64 = (0..4)
            .map(|_| (self.next() >> 11) as f64 / (1u64 << 53) as f64)
            .sum();
        (s - 2.0) * 3f64.sqrt()
    }
}

fn scale_to(x: &mut [f64], dbfs: f64) {
    let rms = (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt();
    let target = 10f64.powf(dbfs / 20.0);
    if rms > 0.0 {
        for v in x.iter_mut() {
            *v *= target / rms;
        }
    }
}

fn to_f32(x: Vec<f64>) -> Vec<f32> {
    x.into_iter().map(|v| v as f32).collect()
}

/// White noise at `dbfs` RMS.
pub(crate) fn white(n: usize, seed: u64, dbfs: f64) -> Vec<f32> {
    let mut r = Rng::new(seed);
    let mut x: Vec<f64> = (0..n).map(|_| r.gauss()).collect();
    scale_to(&mut x, dbfs);
    to_f32(x)
}

/// Speech-like noise at `dbfs` RMS: white noise through a 100 Hz high-pass and a two-pole
/// 1.5 kHz low-pass (most speech energy sits there), shaped by a 4 Hz syllable envelope that
/// swings between 15% and 100%.
pub(crate) fn speech_like(n: usize, seed: u64, dbfs: f64) -> Vec<f32> {
    let mut r = Rng::new(seed);
    let fs = 16_000.0;
    let hp = (-2.0 * PI * 100.0 / fs).exp();
    let lp = (-2.0 * PI * 1_500.0 / fs).exp();
    let (mut prev_in, mut prev_hp, mut l1, mut l2) = (0.0, 0.0, 0.0, 0.0);
    let phase = (seed % 97) as f64 / 97.0 * 2.0 * PI;
    let mut x: Vec<f64> = (0..n)
        .map(|i| {
            let w = r.gauss();
            let h = hp * (prev_hp + w - prev_in);
            prev_in = w;
            prev_hp = h;
            l1 = (1.0 - lp) * h + lp * l1;
            l2 = (1.0 - lp) * l1 + lp * l2;
            let t = i as f64 / fs;
            let env = 0.15 + 0.85 * (2.0 * PI * 4.0 * t + phase).sin().abs();
            l2 * env
        })
        .collect();
    scale_to(&mut x, dbfs);
    to_f32(x)
}

/// A steady tone at `dbfs` RMS.
pub(crate) fn tone(n: usize, hz: f64, dbfs: f64) -> Vec<f32> {
    let mut x: Vec<f64> = (0..n)
        .map(|i| (2.0 * PI * hz * i as f64 / 16_000.0).sin())
        .collect();
    scale_to(&mut x, dbfs);
    to_f32(x)
}

/// A linear chirp from `f0` to `f1` Hz over `n` samples at `dbfs` RMS.
pub(crate) fn chirp(n: usize, f0: f64, f1: f64, dbfs: f64) -> Vec<f32> {
    let dur = n as f64 / 16_000.0;
    let k = (f1 - f0) / dur;
    let mut x: Vec<f64> = (0..n)
        .map(|i| {
            let t = i as f64 / 16_000.0;
            (2.0 * PI * (f0 * t + 0.5 * k * t * t)).sin()
        })
        .collect();
    scale_to(&mut x, dbfs);
    to_f32(x)
}

/// Causal FIR filtering: `y[n] = Σ taps[j]·x[n − j]`.
pub(crate) fn filter(x: &[f32], taps: &[f32]) -> Vec<f32> {
    (0..x.len())
        .map(|n| {
            taps.iter()
                .enumerate()
                .filter(|(j, _)| *j <= n)
                .map(|(j, h)| h * x[n - j])
                .sum()
        })
        .collect()
}

/// Samples scaled to ±1 as PCM16.
pub(crate) fn pcm(x: &[f32]) -> Vec<i16> {
    x.iter()
        .map(|v| (v * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
        .collect()
}
