//! The span decoding worker: the commit re-decode and the STT copy of a committed span
//! (spec §4.7, §5.1).
//!
//! **Re-decode.** A fresh Kroko decode of the span's evidence copy plus 1.5 s of zeros, for
//! the commit re-check; token times are seconds from the span's first sample.
//!
//! **STT copy.** What Home Assistant's STT receives: the canonical span at a gain, then
//! DTLN noise suppression when the device asks for it, then 0.5 s of digital silence,
//! saturated to int16. The canonical PCM itself is never changed.
//!
//! The gain rule is this rewrite's decision; the spec says only "chosen from the wake word's
//! level, nominally +20 dB". The RMS level of the wake word's canonical support is
//! measured in dBFS, and the gain brings it to −20 dBFS:
//! `gain_db = clamp(−20 − wake_rms_dbfs, 10, 30)`. A wake at the typical raw native-AFE
//! level of about −40 dBFS therefore gets the nominal +20 dB, a quieter speaker more (at
//! most +30 dB, the VAD input's gain), a louder one less (at least +10 dB, so a close
//! talker is not left near the raw level where Home Assistant's STT drops quiet leading
//! words). Turns without a wake word to measure (button presses and replies), and a
//! silent or empty support, get the nominal +20 dB.
//!
//! When DTLN cannot be loaded or fails, the copy is sent without it and says so
//! (`denoised: false`): suppression is an optional enhancement, and the user's words are
//! still in the undenoised audio.

use std::sync::Arc;

use super::models::{Denoise, ModelSet, evidence_copy, fresh_decode};
use super::{SpanJob, SpanOutput, SttCopy, SttGain};

/// The level the gain brings the wake word to.
const TARGET_DBFS: f64 = -20.0;
const MIN_GAIN_DB: f64 = 10.0;
const MAX_GAIN_DB: f64 = 30.0;
const NOMINAL_GAIN_DB: f64 = 20.0;
/// Digital silence after the span: 0.5 s at 16 kHz.
const TRAILING_SILENCE: usize = 8_000;

/// The span decoding worker's state: its models and its denoiser, loaded on first use.
pub struct SpanDecoding {
    models: Arc<dyn ModelSet>,
    denoiser: Option<Box<dyn Denoise>>,
}

/// The STT copy's gain for a turn, in dB (see the module documentation).
pub fn stt_gain_db(gain: &SttGain) -> f64 {
    let SttGain::FromWake(pcm) = gain else {
        return NOMINAL_GAIN_DB;
    };
    if pcm.is_empty() {
        return NOMINAL_GAIN_DB;
    }
    let mean_square = pcm
        .iter()
        .map(|&s| {
            let x = f64::from(s) / 32_768.0;
            x * x
        })
        .sum::<f64>()
        / pcm.len() as f64;
    if mean_square <= 0.0 {
        return NOMINAL_GAIN_DB;
    }
    let dbfs = 10.0 * mean_square.log10();
    (TARGET_DBFS - dbfs).clamp(MIN_GAIN_DB, MAX_GAIN_DB)
}

impl SpanDecoding {
    pub fn new(models: Arc<dyn ModelSet>) -> Self {
        Self {
            models,
            denoiser: None,
        }
    }

    /// Runs one job.
    pub fn run(&mut self, job: SpanJob) -> SpanOutput {
        match job {
            SpanJob::Redecode { turn, pcm } => SpanOutput::Redecode {
                turn,
                result: fresh_decode(&*self.models, &evidence_copy(&pcm))
                    .map_err(|e| e.to_string()),
            },
            SpanJob::SttCopy {
                turn,
                pcm,
                gain,
                denoise,
            } => SpanOutput::SttCopy {
                turn,
                result: Ok(self.stt_copy(turn, &pcm, &gain, denoise)),
            },
        }
    }

    fn stt_copy(
        &mut self,
        turn: uuid::Uuid,
        pcm: &[i16],
        gain: &SttGain,
        denoise: bool,
    ) -> SttCopy {
        let gain_db = stt_gain_db(gain);
        let linear = 10f64.powf(gain_db / 20.0);
        let gained: Vec<f32> = pcm
            .iter()
            .map(|&s| ((f64::from(s) * linear).clamp(-32_768.0, 32_767.0) / 32_768.0) as f32)
            .collect();
        let (audio, denoised) = if denoise {
            match self.denoise(&gained) {
                Ok(clean) => (clean, true),
                Err(error) => {
                    tracing::warn!(%turn, %error, "the STT copy is sent without noise suppression");
                    (gained, false)
                }
            }
        } else {
            (gained, false)
        };
        let out: Vec<i16> = audio
            .iter()
            .map(|&v| (f64::from(v) * 32_768.0).round().clamp(-32_768.0, 32_767.0) as i16)
            .chain(std::iter::repeat_n(0, TRAILING_SILENCE))
            .collect();
        SttCopy {
            pcm: out.into(),
            gain_db,
            denoised,
        }
    }

    fn denoise(&mut self, audio: &[f32]) -> Result<Vec<f32>, String> {
        if self.denoiser.is_none() {
            self.denoiser = Some(self.models.denoiser().map_err(|e| e.to_string())?);
        }
        let denoiser = self
            .denoiser
            .as_mut()
            .ok_or_else(|| "no denoiser".to_owned())?;
        let clean = denoiser.denoise_span(audio).map_err(|e| e.to_string())?;
        if clean.len() != audio.len() {
            return Err(format!(
                "the denoiser returned {} samples for {}",
                clean.len(),
                audio.len()
            ));
        }
        Ok(clean)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::testing::{ToneModels, silence, utterance};

    fn models() -> ToneModels {
        ToneModels::new(&["ophelia", "turn", "on"])
    }

    fn copy(w: &mut SpanDecoding, pcm: &[i16], gain: SttGain, denoise: bool) -> SttCopy {
        let turn = uuid::Uuid::new_v4();
        match w.run(SpanJob::SttCopy {
            turn,
            pcm: pcm.into(),
            gain,
            denoise,
        }) {
            SpanOutput::SttCopy { turn: t, result } => {
                assert_eq!(t, turn);
                result.unwrap()
            }
            SpanOutput::Redecode { .. } => panic!("asked for an STT copy"),
        }
    }

    /// A constant level `dbfs` below full scale.
    fn level(dbfs: f64) -> Arc<[i16]> {
        let v = (32_768.0 * 10f64.powf(dbfs / 20.0)).round() as i16;
        vec![v; 4_000].into()
    }

    #[test]
    fn redecode_times_are_relative_to_the_span() {
        let m = models();
        let mut w = SpanDecoding::new(Arc::new(m.clone()));
        let mut pcm = silence(200);
        pcm.extend(utterance(&m, &["turn", "on"], 240, 160, 1_000));
        let turn = uuid::Uuid::new_v4();
        let SpanOutput::Redecode { turn: t, result } = w.run(SpanJob::Redecode {
            turn,
            pcm: pcm.into(),
        }) else {
            panic!("asked for a re-decode");
        };
        assert_eq!(t, turn);
        let r = result.unwrap();
        assert_eq!(r.text, "turn on");
        // "turn" fills frames 5..=10 (200–440 ms) and is stamped 0.12 s after.
        assert!((r.timestamps[0] - 0.56).abs() < 1e-6);
    }

    #[test]
    fn nominal_gain_is_20_db_with_half_a_second_of_silence() {
        let mut w = SpanDecoding::new(Arc::new(models()));
        let c = copy(&mut w, &[100, -100, 0, 3_000], SttGain::Nominal, false);
        assert_eq!(c.gain_db, 20.0);
        assert!(!c.denoised);
        assert_eq!(c.pcm.len(), 4 + 8_000);
        assert_eq!(&c.pcm[..4], &[1_000, -1_000, 0, 30_000]);
        assert!(c.pcm[4..].iter().all(|&s| s == 0));
    }

    #[test]
    fn the_copy_saturates() {
        let mut w = SpanDecoding::new(Arc::new(models()));
        let c = copy(&mut w, &[4_000, -4_000], SttGain::Nominal, false);
        assert_eq!(&c.pcm[..2], &[32_767, -32_768]);
    }

    #[test]
    fn wake_gain_brings_the_wake_word_to_minus_20_dbfs_within_10_to_30_db() {
        // The levels are rounded to whole int16 steps, hence the tolerance.
        let g = |pcm| stt_gain_db(&SttGain::FromWake(pcm));
        assert!((g(level(-40.0)) - 20.0).abs() < 0.05);
        assert!((g(level(-45.0)) - 25.0).abs() < 0.05);
        assert!((g(level(-34.0)) - 14.0).abs() < 0.05);
        assert_eq!(g(level(-10.0)), 10.0);
        assert_eq!(g(level(-70.0)), 30.0);
        assert_eq!(g(vec![0i16; 100].into()), 20.0);
        assert_eq!(g(Vec::<i16>::new().into()), 20.0);
        assert_eq!(stt_gain_db(&SttGain::Nominal), 20.0);

        let mut w = SpanDecoding::new(Arc::new(models()));
        let c = copy(&mut w, &[100], SttGain::FromWake(level(-45.0)), false);
        assert!((c.gain_db - 25.0).abs() < 0.05);
        assert_eq!(
            f64::from(c.pcm[0]),
            (100.0 * 10f64.powf(c.gain_db / 20.0)).round()
        );
    }

    #[test]
    fn denoising_is_reported_and_its_failure_leaves_the_copy_undenoised() {
        let pcm = [100, -200, 300];
        let mut w = SpanDecoding::new(Arc::new(models()));
        let clean = copy(&mut w, &pcm, SttGain::Nominal, true);
        assert!(clean.denoised);
        assert_eq!(&clean.pcm[..3], &[1_000, -2_000, 3_000]);

        let mut w = SpanDecoding::new(Arc::new(models().failing_denoiser()));
        let plain = copy(&mut w, &pcm, SttGain::Nominal, true);
        assert!(!plain.denoised);
        assert_eq!(plain.pcm, clean.pcm);
    }
}
