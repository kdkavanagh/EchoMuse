//! The playback equalizer (spec §4.8): eight bands with an optional speech
//! boost, applied to the audio the controller streams to a device. Alerts and
//! earcons play on the device and are never equalized.
//!
//! The bands, their limits and the filter shapes are the dashboard's
//! (`controller/static/dashboard.jsx`, `EqCurve` and `EqSliders`): the setting
//! `eqBands` holds eight integer gains from −12 to +12 dB for 125, 250, 500,
//! 1k, 2k, 3.5k, 5.5k and 8k Hz. The first band is a low shelf, the last a high
//! shelf (shelf slope 1), and the six between are peaking filters with
//! Q 1.4, all in the RBJ cookbook form the dashboard plots. `eqLoudness`
//! ("Speech boost") adds a +5 dB peak at 2.5 kHz (`docs/configuration.md`);
//! the dashboard does not give its width, so it uses the bands' Q 1.4.
//!
//! The filter runs in `f64` and saturates its output to the PCM16 range, so a
//! boost that would clip clamps instead of wrapping around.

use std::f64::consts::{PI, SQRT_2};

/// The sample rate the controller renders at.
pub const RENDER_SAMPLE_RATE: u32 = 48_000;

/// The band centre frequencies in Hz.
const EQ_BAND_HZ: [f64; 8] = [
    125.0, 250.0, 500.0, 1_000.0, 2_000.0, 3_500.0, 5_500.0, 8_000.0,
];

/// The peaking bands' Q.
const EQ_Q: f64 = 1.4;

/// The largest cut or boost of one band, in dB.
const EQ_GAIN_LIMIT_DB: i8 = 12;

/// The speech boost's centre frequency in Hz.
const SPEECH_BOOST_HZ: f64 = 2_500.0;

/// The speech boost's gain in dB.
const SPEECH_BOOST_DB: f64 = 5.0;

/// A band gain outside −12 to +12 dB.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("eqBands[{band}] = {gain} dB is outside -12 to 12")]
pub struct EqSettingsError {
    /// The band's index.
    pub band: usize,
    /// The refused gain.
    pub gain: i8,
}

/// The `eqBands` and `eqLoudness` settings; the default is flat, without the speech
/// boost.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct EqSettings {
    bands: [i8; 8],
    loudness: bool,
}

impl EqSettings {
    /// Checks every band is within ±12 dB.
    pub fn new(bands: [i8; 8], loudness: bool) -> Result<Self, EqSettingsError> {
        if let Some((band, &gain)) = bands
            .iter()
            .enumerate()
            .find(|(_, gain)| gain.abs() > EQ_GAIN_LIMIT_DB)
        {
            return Err(EqSettingsError { band, gain });
        }
        Ok(Self { bands, loudness })
    }

    /// Whether the settings change nothing.
    pub fn is_flat(&self) -> bool {
        !self.loudness && self.bands.iter().all(|&gain| gain == 0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Shape {
    LowShelf,
    Peak,
    HighShelf,
}

/// One second-order section in transposed direct form II.
#[derive(Clone, Debug, PartialEq)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    /// The RBJ cookbook section the dashboard plots.
    fn new(shape: Shape, centre_hz: f64, gain_db: f64, sample_rate: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w0 = 2.0 * PI * centre_hz / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::Peak => {
                let alpha = sin / (2.0 * EQ_Q);
                (
                    1.0 + alpha * a,
                    -2.0 * cos,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cos,
                    1.0 - alpha / a,
                )
            }
            Shape::LowShelf => {
                let k = 2.0 * a.sqrt() * sin / SQRT_2;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - k),
                    (a + 1.0) + (a - 1.0) * cos + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - k,
                )
            }
            Shape::HighShelf => {
                let k = 2.0 * a.sqrt() * sin / SQRT_2;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - k),
                    (a + 1.0) - (a - 1.0) * cos + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - k,
                )
            }
        };
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn step(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// A stateful equalizer for one playback's audio. Its state carries across
/// calls, so a stream can be processed in chunks of any size.
#[derive(Clone, Debug, PartialEq)]
pub struct Equalizer {
    stages: Vec<Biquad>,
}

impl Equalizer {
    /// The filter chain for `settings` at `sample_rate`. Bands at 0 dB add no
    /// stage, so flat settings pass audio through unchanged.
    pub fn new(settings: &EqSettings, sample_rate: u32) -> Self {
        let rate = f64::from(sample_rate);
        let mut stages: Vec<Biquad> = settings
            .bands
            .iter()
            .enumerate()
            .filter(|(_, gain)| **gain != 0)
            .map(|(band, &gain)| {
                let shape = match band {
                    0 => Shape::LowShelf,
                    7 => Shape::HighShelf,
                    _ => Shape::Peak,
                };
                Biquad::new(shape, EQ_BAND_HZ[band], f64::from(gain), rate)
            })
            .collect();
        if settings.loudness {
            stages.push(Biquad::new(
                Shape::Peak,
                SPEECH_BOOST_HZ,
                SPEECH_BOOST_DB,
                rate,
            ));
        }
        Self { stages }
    }

    /// Filters `pcm` in place, saturating to the PCM16 range.
    pub fn process(&mut self, pcm: &mut [i16]) {
        if self.stages.is_empty() {
            return;
        }
        for sample in pcm {
            let mut x = f64::from(*sample);
            for stage in &mut self.stages {
                x = stage.step(x);
            }
            // Saturate rather than wrap: a boost past full scale clips.
            *sample = x.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// |H(e^jw)| of one section at `hz`.
    fn magnitude(stage: &Biquad, hz: f64) -> f64 {
        let w = 2.0 * PI * hz / f64::from(RENDER_SAMPLE_RATE);
        let (s1, c1) = w.sin_cos();
        let (s2, c2) = (2.0 * w).sin_cos();
        let num_re = stage.b0 + stage.b1 * c1 + stage.b2 * c2;
        let num_im = -(stage.b1 * s1 + stage.b2 * s2);
        let den_re = 1.0 + stage.a1 * c1 + stage.a2 * c2;
        let den_im = -(stage.a1 * s1 + stage.a2 * s2);
        ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).sqrt()
    }

    /// The designed gain of a chain built at [`RENDER_SAMPLE_RATE`], at `hz`, in dB.
    fn response_db(eq: &Equalizer, hz: f64) -> f64 {
        let magnitude: f64 = eq.stages.iter().map(|s| magnitude(s, hz)).product();
        20.0 * magnitude.log10()
    }

    fn single(band: usize, gain: i8) -> Equalizer {
        let mut bands = [0; 8];
        bands[band] = gain;
        Equalizer::new(&EqSettings::new(bands, false).unwrap(), RENDER_SAMPLE_RATE)
    }

    /// The gain a steady sine at `hz` gets through a fresh copy of `eq`, measured from
    /// the processed samples once the filters have settled.
    fn measured_db(eq: &Equalizer, hz: f64) -> f64 {
        let amplitude = 2_000.0;
        let rate = f64::from(RENDER_SAMPLE_RATE);
        let mut pcm: Vec<i16> = (0..2 * RENDER_SAMPLE_RATE as usize)
            .map(|n| (amplitude * (2.0 * PI * hz * n as f64 / rate).sin()).round() as i16)
            .collect();
        let input: Vec<f64> = pcm.iter().map(|&s| f64::from(s)).collect();
        let mut eq = eq.clone();
        // Uneven chunks: state must carry across calls.
        for chunk in pcm.chunks_mut(1_234) {
            eq.process(chunk);
        }
        let tail = RENDER_SAMPLE_RATE as usize;
        let rms = |v: &mut dyn Iterator<Item = f64>| {
            let (sum, n) = v.fold((0.0, 0.0), |(s, n), x| (s + x * x, n + 1.0));
            (sum / n).sqrt()
        };
        let out = rms(&mut pcm[tail..].iter().map(|&s| f64::from(s)));
        let inp = rms(&mut input[tail..].iter().copied());
        20.0 * (out / inp).log10()
    }

    #[test]
    fn settings_follow_the_dashboard_limits() {
        assert!(EqSettings::new([12, -12, 0, 0, 0, 0, 0, 0], true).is_ok());
        assert_eq!(
            EqSettings::new([0, 0, 13, 0, 0, 0, 0, 0], false),
            Err(EqSettingsError { band: 2, gain: 13 })
        );
        assert_eq!(
            EqSettings::new([0, 0, 0, 0, 0, 0, 0, -13], false),
            Err(EqSettingsError { band: 7, gain: -13 })
        );
        assert!(EqSettings::default().is_flat());
        assert!(!EqSettings::new([0; 8], true).unwrap().is_flat());
    }

    #[test]
    fn flat_settings_leave_audio_bit_exact() {
        let mut eq = Equalizer::new(&EqSettings::default(), RENDER_SAMPLE_RATE);
        let original: Vec<i16> = (0..4_800)
            .map(|n| ((n * 37) % 65_536 - 32_768) as i16)
            .collect();
        let mut pcm = original.clone();
        eq.process(&mut pcm);
        assert_eq!(pcm, original);
    }

    #[test]
    fn each_peaking_band_reaches_exactly_its_gain_at_its_centre() {
        for (band, &centre) in EQ_BAND_HZ.iter().enumerate().take(7).skip(1) {
            for gain in [-12, -5, 3, 12] {
                let eq = single(band, gain);
                let at_centre = response_db(&eq, centre);
                assert!(
                    (at_centre - f64::from(gain)).abs() < 1e-9,
                    "band {band} gain {gain}: {at_centre}"
                );
                // A peak is the extreme of its own response.
                for factor in [0.8, 1.25] {
                    assert!(response_db(&eq, centre * factor).abs() < f64::from(gain).abs());
                }
            }
        }
    }

    #[test]
    fn shelves_reach_half_their_gain_at_the_centre_and_all_of_it_beyond() {
        for gain in [-12, 6, 12] {
            let half = f64::from(gain) / 2.0;
            let low = single(0, gain);
            assert!((response_db(&low, 125.0) - half).abs() < 1e-9);
            assert!((response_db(&low, 1.0) - f64::from(gain)).abs() < 0.01);
            assert!(response_db(&low, 8_000.0).abs() < 0.01);
            let high = single(7, gain);
            assert!((response_db(&high, 8_000.0) - half).abs() < 1e-9);
            assert!((response_db(&high, 23_999.0) - f64::from(gain)).abs() < 0.05);
            assert!(response_db(&high, 125.0).abs() < 0.01);
        }
    }

    #[test]
    fn speech_boost_adds_five_db_at_two_and_a_half_kilohertz() {
        let eq = Equalizer::new(&EqSettings::new([0; 8], true).unwrap(), RENDER_SAMPLE_RATE);
        assert!((response_db(&eq, SPEECH_BOOST_HZ) - SPEECH_BOOST_DB).abs() < 1e-9);
    }

    #[test]
    fn processed_audio_matches_the_designed_response_at_every_band_centre() {
        // The dashboard's Clarity preset with a warm low end and the boost.
        let settings = EqSettings::new([4, 3, 2, 0, -2, 7, 4, 2], true).unwrap();
        let eq = Equalizer::new(&settings, RENDER_SAMPLE_RATE);
        for hz in EQ_BAND_HZ.into_iter().chain([SPEECH_BOOST_HZ]) {
            let designed = response_db(&eq, hz);
            let measured = measured_db(&eq, hz);
            assert!(
                (measured - designed).abs() < 0.05,
                "{hz} Hz: measured {measured:.4} dB, designed {designed:.4} dB"
            );
        }
    }

    #[test]
    fn boosted_full_scale_audio_saturates_instead_of_wrapping() {
        let settings = EqSettings::new([12; 8], true).unwrap();
        let mut eq = Equalizer::new(&settings, RENDER_SAMPLE_RATE);
        let mut reference = eq.clone();
        let rate = f64::from(RENDER_SAMPLE_RATE);
        let input: Vec<i16> = (0..RENDER_SAMPLE_RATE as usize)
            .map(|n| (32_767.0 * (2.0 * PI * 1_000.0 * n as f64 / rate).sin()).round() as i16)
            .collect();
        let mut pcm = input.clone();
        eq.process(&mut pcm);

        // The same chain without the final conversion: every output sample
        // is the unclamped value saturated to the PCM16 range.
        let mut clipped = 0;
        for (out, &x) in pcm.iter().zip(&input) {
            let mut y = f64::from(x);
            for stage in &mut reference.stages {
                y = stage.step(y);
            }
            let y = y.round();
            let expected = if y > f64::from(i16::MAX) {
                clipped += 1;
                i16::MAX
            } else if y < f64::from(i16::MIN) {
                clipped += 1;
                i16::MIN
            } else {
                y as i16
            };
            assert_eq!(*out, expected);
        }
        assert!(
            clipped > 1_000,
            "the boost drives the signal past full scale"
        );
        assert!(pcm.contains(&i16::MAX) && pcm.contains(&i16::MIN));
    }
}
