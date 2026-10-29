//! Small audio helpers: reading 16-bit mono WAV files and sherpa-onnx's linear resampler.

use std::path::Path;
use std::ptr::NonNull;

use sherpa_onnx_sys as sys;

use crate::error::SpeechError;
use crate::kroko::KROKO_SAMPLE_RATE;

/// A 16-bit mono WAV file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonoWav {
    /// Samples per second.
    pub sample_rate: u32,
    /// The samples.
    pub samples: Vec<i16>,
}

impl MonoWav {
    /// Reads a 16-bit PCM mono WAV file; any other format is an error.
    pub fn read(path: &Path) -> Result<Self, SpeechError> {
        let wav_err = |detail: String| SpeechError::Wav {
            path: path.to_path_buf(),
            detail,
        };
        let reader = hound::WavReader::open(path).map_err(|e| wav_err(e.to_string()))?;
        let spec = reader.spec();
        if spec.channels != 1
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
        {
            return Err(wav_err(format!(
                "{} channels, {}-bit {:?}; expected mono 16-bit PCM",
                spec.channels, spec.bits_per_sample, spec.sample_format
            )));
        }
        let samples = reader
            .into_samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| wav_err(e.to_string()))?;
        Ok(Self {
            sample_rate: spec.sample_rate,
            samples,
        })
    }

    /// The samples as floats in [-1, 1): `x / 32768`.
    pub fn to_f32(&self) -> Vec<f32> {
        self.samples
            .iter()
            .map(|&s| f32::from(s) / 32768.0)
            .collect()
    }
}

/// sherpa-onnx's streaming windowed-sinc resampler, owned by one caller.
pub struct LinearResampler {
    ptr: NonNull<sys::SherpaOnnxLinearResampler>,
}

// SAFETY: an owned heap object with no thread affinity; every method takes `&mut self`.
unsafe impl Send for LinearResampler {}

impl std::fmt::Debug for LinearResampler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearResampler").finish_non_exhaustive()
    }
}

impl Drop for LinearResampler {
    fn drop(&mut self) {
        // SAFETY: created by SherpaOnnxCreateLinearResampler, destroyed once.
        unsafe { sys::SherpaOnnxDestroyLinearResampler(self.ptr.as_ptr()) };
    }
}

/// Frees a resampler output with the library's own deallocator.
struct NativeResampleOut(*const sys::SherpaOnnxResampleOut);

impl Drop for NativeResampleOut {
    fn drop(&mut self) {
        // SAFETY: returned by SherpaOnnxLinearResamplerResample and released once with its
        // matching free function (C++ `delete[]`/`delete` inside the library).
        unsafe { sys::SherpaOnnxLinearResamplerResampleFree(self.0) };
    }
}

impl LinearResampler {
    /// A resampler from `input_rate` to `output_rate` Hz with a low-pass cutoff and filter
    /// half-width in zero crossings.
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        cutoff_hz: f32,
        num_zeros: u32,
    ) -> Result<Self, SpeechError> {
        let bad = |what: &str| SpeechError::InvalidInput(format!("resampler {what}"));
        let input = i32::try_from(input_rate).map_err(|_| bad("input rate out of range"))?;
        let output = i32::try_from(output_rate).map_err(|_| bad("output rate out of range"))?;
        let zeros = i32::try_from(num_zeros).map_err(|_| bad("zero count out of range"))?;
        if input == 0 || output == 0 {
            return Err(bad("rates must be positive"));
        }
        let nyquist = input_rate.min(output_rate) as f32 / 2.0;
        if !(cutoff_hz > 0.0 && cutoff_hz <= nyquist) {
            return Err(bad("cutoff must be in (0, half the lower rate]"));
        }
        // SAFETY: plain values; returns an owned object or null.
        let ptr = unsafe { sys::SherpaOnnxCreateLinearResampler(input, output, cutoff_hz, zeros) };
        let ptr = NonNull::new(ptr.cast_mut())
            .ok_or(SpeechError::NullHandle("SherpaOnnxCreateLinearResampler"))?;
        Ok(Self { ptr })
    }

    /// A resampler to 16 kHz with the parameters sherpa-onnx's feature extractor uses when it is
    /// given audio at another rate: cutoff `0.99 · 0.5 · min(rates)`, six zero crossings. Feeding
    /// its output gives the same features as handing sherpa-onnx the original audio.
    pub fn to_kroko_rate(input_rate: u32) -> Result<Self, SpeechError> {
        let min_freq = input_rate.min(KROKO_SAMPLE_RATE) as f32;
        let cutoff = (0.99_f64 * 0.5 * f64::from(min_freq)) as f32;
        Self::new(input_rate, KROKO_SAMPLE_RATE, cutoff, 6)
    }

    /// Resamples the next piece of a stream. `flush` ends the stream and emits the filter tail.
    pub fn resample(&mut self, input: &[f32], flush: bool) -> Result<Vec<f32>, SpeechError> {
        let n = i32::try_from(input.len())
            .map_err(|_| SpeechError::InvalidInput("too many samples in one call".into()))?;
        // SAFETY: the resampler is alive and exclusively borrowed; `input` is valid for `n`.
        let out = unsafe {
            sys::SherpaOnnxLinearResamplerResample(
                self.ptr.as_ptr(),
                input.as_ptr(),
                n,
                i32::from(flush),
            )
        };
        if out.is_null() {
            return Err(SpeechError::NullHandle("SherpaOnnxLinearResamplerResample"));
        }
        let owned = NativeResampleOut(out);
        // SAFETY: `owned.0` is a live result object until `owned` drops.
        let (samples, count) = unsafe { ((*owned.0).samples, (*owned.0).n) };
        let count = usize::try_from(count).map_err(|_| SpeechError::InvalidOutput {
            model: "resampler",
            detail: format!("negative sample count {count}"),
        })?;
        if count == 0 {
            return Ok(Vec::new());
        }
        if samples.is_null() {
            return Err(SpeechError::NullHandle("SherpaOnnxResampleOut.samples"));
        }
        // SAFETY: the library allocated `count` floats at `samples`; we copy before freeing.
        Ok(unsafe { std::slice::from_raw_parts(samples, count) }.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bad_resampler_parameters() {
        assert!(LinearResampler::new(0, 16_000, 100.0, 6).is_err());
        assert!(LinearResampler::new(24_000, 16_000, 0.0, 6).is_err());
        assert!(LinearResampler::new(24_000, 16_000, 9_000.0, 6).is_err());
        assert!(LinearResampler::new(u32::MAX, 16_000, 100.0, 6).is_err());
    }

    #[test]
    fn resampling_keeps_duration_and_level() {
        let mut r = LinearResampler::to_kroko_rate(24_000).unwrap();
        let tone: Vec<f32> = (0..24_000)
            .map(|i| 0.5 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 24_000.0).sin())
            .collect();
        let mut out = Vec::new();
        for chunk in tone.chunks(1920) {
            out.extend(r.resample(chunk, false).unwrap());
        }
        out.extend(r.resample(&[], true).unwrap());
        assert!(
            (out.len() as i64 - 16_000).abs() <= 8,
            "got {} samples",
            out.len()
        );
        let mid = &out[4_000..12_000];
        let rms = (mid.iter().map(|x| x * x).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");
    }

    #[test]
    fn wav_reader_rejects_non_wav() {
        let dir = std::env::temp_dir().join(format!("echomuse-speech-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.wav");
        std::fs::write(&path, b"not a wav").unwrap();
        assert!(matches!(MonoWav::read(&path), Err(SpeechError::Wav { .. })));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        w.write_sample(0i16).unwrap();
        w.write_sample(0i16).unwrap();
        w.finalize().unwrap();
        assert!(matches!(MonoWav::read(&path), Err(SpeechError::Wav { .. })));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
