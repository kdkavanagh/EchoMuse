//! The models the speech workers call, behind one interface.
//!
//! A worker never names a concrete model type. It asks its [`ModelSet`] for a fresh ASR
//! stream, a VAD session, a denoiser or a wake scorer, and keeps what it gets for as long
//! as its stream state lives. Production uses [`BundleModels`], the pinned speech bundle;
//! tests use stand-ins (`testing::ToneModels`) so the workers' bookkeeping can be checked
//! without model files. Keeping every model operation behind this one interface is also
//! what would let inference move behind a process boundary later without changing the
//! workers (spec §5.5, "Recovery decision").
//!
//! The level conventions every decode follows live here too, so no worker can apply a
//! different gain by accident: [`evidence_copy`] for Kroko, [`vad_input`] for Silero, and
//! [`fresh_decode`] with its [`FLUSH_SAMPLES`] of trailing zeros.

use echomuse_echo::WakeWindowScorer;
use echomuse_speech::{
    AsrResult, BcresnetModelFiles, BcresnetScorer, DtlnDenoiser, DtlnModelFiles, KrokoRecognizer,
    KrokoStream, OrtRuntime, PinnedFile, SherpaRuntime, SileroVad, SpeechAssets, SpeechError,
};

/// Zeros fed after the audio of every fresh decode: 1.5 s at 16 kHz. Without them the
/// streaming encoder holds back the last words, which only come out once later audio
/// pushes them through.
pub const FLUSH_SAMPLES: usize = 24_000;
static FLUSH: [f32; FLUSH_SAMPLES] = [0.0; FLUSH_SAMPLES];

/// The evidence copy's gain: +20 dB.
const EVIDENCE_GAIN: f64 = 10.0;
/// The VAD input's gain: +30 dB.
const VAD_GAIN: f64 = 31.622_776_601_683_793;

/// One streaming ASR decode, owned by one worker for one utterance or one fresh decode.
pub trait AsrStream: Send {
    /// Adds 16 kHz mono samples in [-1, 1].
    fn accept(&mut self, samples: &[f32]) -> Result<(), SpeechError>;
    /// Runs the decoder over everything accepted that is ready.
    fn decode_ready(&mut self);
    /// The result so far.
    fn result(&mut self) -> Result<AsrResult, SpeechError>;
}

/// A voice activity detector that keeps its state between 32 ms cells.
pub trait Vad: Send {
    /// The speech probability of the next cell (the VAD input, in [-1, 1]).
    fn process_cell(&mut self, cell: &[f32; 512]) -> Result<f32, SpeechError>;
    /// Returns to the state at a stream's first sample.
    fn reset(&mut self);
}

/// Noise suppression for a whole span.
pub trait Denoise: Send {
    /// Returns exactly `input.len()` denoised samples aligned with `input`.
    fn denoise_span(&mut self, input: &[f32]) -> Result<Vec<f32>, SpeechError>;
}

/// A wake graph scoring session as the reference scorer drives it.
pub type BoxedWakeScorer = Box<dyn WakeWindowScorer<Error = SpeechError> + Send>;

/// Where the speech workers get their models. Shared by every worker of every device, so
/// it hands out fresh per-worker sessions and holds only what may be shared.
pub trait ModelSet: Send + Sync + 'static {
    /// A fresh ASR stream: empty features, initial encoder state, empty result.
    fn asr_stream(&self) -> Result<Box<dyn AsrStream>, SpeechError>;
    /// A VAD session in its stream-start state.
    fn vad(&self) -> Result<Box<dyn Vad>, SpeechError>;
    /// A denoiser.
    fn denoiser(&self) -> Result<Box<dyn Denoise>, SpeechError>;
    /// A scoring session on the wake graph `files` names.
    fn wake_scorer(&self, files: &BcresnetModelFiles) -> Result<BoxedWakeScorer, SpeechError>;
}

impl AsrStream for KrokoStream {
    fn accept(&mut self, samples: &[f32]) -> Result<(), SpeechError> {
        KrokoStream::accept(self, samples)
    }

    fn decode_ready(&mut self) {
        KrokoStream::decode_ready(self);
    }

    fn result(&mut self) -> Result<AsrResult, SpeechError> {
        KrokoStream::result(self)
    }
}

impl Vad for SileroVad {
    fn process_cell(&mut self, cell: &[f32; 512]) -> Result<f32, SpeechError> {
        SileroVad::process_cell(self, cell)
    }

    fn reset(&mut self) {
        SileroVad::reset(self);
    }
}

impl Denoise for DtlnDenoiser {
    fn denoise_span(&mut self, input: &[f32]) -> Result<Vec<f32>, SpeechError> {
        DtlnDenoiser::denoise_span(self, input)
    }
}

/// The pinned speech bundle: one Kroko recognizer shared by every stream, and the files
/// each worker loads its own Silero, DTLN and wake graph sessions from.
///
/// The recognizer is shared because it is 70 MB and its C API is safe for concurrent use
/// of distinct streams (the speech crate's `kroko` module documents why); every other
/// model is small and stateful, so each worker gets its own session (spec §5.1).
pub struct BundleModels {
    ort: OrtRuntime,
    kroko: KrokoRecognizer,
    vad: PinnedFile,
    dtln: DtlnModelFiles,
}

impl BundleModels {
    /// Loads the bundle the controller qualified: verifies the native libraries, loads the
    /// recognizer, and loads one Silero and one DTLN session once and drops them, so a file
    /// that changed since qualification fails startup rather than the first turn.
    pub fn load(assets: &SpeechAssets) -> Result<Self, SpeechError> {
        let ort = OrtRuntime::load(&assets.native.onnxruntime)?;
        let sherpa = SherpaRuntime::verify(&assets.native.sherpa)?;
        let kroko = KrokoRecognizer::load(&sherpa, &assets.kroko)?;
        drop(SileroVad::load(&ort, &assets.vad)?);
        drop(DtlnDenoiser::load(&ort, &assets.dtln)?);
        Ok(Self {
            ort,
            kroko,
            vad: assets.vad.clone(),
            dtln: assets.dtln.clone(),
        })
    }
}

impl ModelSet for BundleModels {
    fn asr_stream(&self) -> Result<Box<dyn AsrStream>, SpeechError> {
        Ok(Box::new(self.kroko.create_stream()?))
    }

    fn vad(&self) -> Result<Box<dyn Vad>, SpeechError> {
        Ok(Box::new(SileroVad::load(&self.ort, &self.vad)?))
    }

    fn denoiser(&self) -> Result<Box<dyn Denoise>, SpeechError> {
        Ok(Box::new(DtlnDenoiser::load(&self.ort, &self.dtln)?))
    }

    fn wake_scorer(&self, files: &BcresnetModelFiles) -> Result<BoxedWakeScorer, SpeechError> {
        Ok(Box::new(BcresnetScorer::load(&self.ort, files)?))
    }
}

/// Canonical PCM at a fixed linear gain, saturated to int16 full scale, as floats in
/// [-1, 1].
fn gained(pcm: &[i16], gain: f64) -> Vec<f32> {
    pcm.iter().map(|&s| gained_sample(s, gain)).collect()
}

fn gained_sample(sample: i16, gain: f64) -> f32 {
    let v = (f64::from(sample) * gain).clamp(-32_768.0, 32_767.0);
    (v / 32_768.0) as f32
}

/// Kroko's input: canonical × 10 (+20 dB), saturated to int16.
pub fn evidence_copy(pcm: &[i16]) -> Vec<f32> {
    gained(pcm, EVIDENCE_GAIN)
}

/// One sample of the evidence copy.
pub(crate) fn evidence_sample(sample: i16) -> f32 {
    gained_sample(sample, EVIDENCE_GAIN)
}

/// Silero's input: canonical × 10^1.5 (+30 dB), saturated to int16. Silero v5 is
/// level-sensitive and missed the quietest commands at +20 dB.
pub fn vad_input(pcm: &[i16]) -> Vec<f32> {
    gained(pcm, VAD_GAIN)
}

/// One sample of the VAD input.
pub(crate) fn vad_sample(sample: i16) -> f32 {
    gained_sample(sample, VAD_GAIN)
}

/// A decode of `evidence` (already the evidence copy) on a fresh stream, followed by
/// [`FLUSH_SAMPLES`] of zeros. Token times are seconds from the first sample of
/// `evidence`.
pub fn fresh_decode(models: &dyn ModelSet, evidence: &[f32]) -> Result<AsrResult, SpeechError> {
    let mut stream = models.asr_stream()?;
    stream.accept(evidence)?;
    stream.accept(&FLUSH)?;
    stream.decode_ready();
    stream.result()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_saturate_at_int16_full_scale() {
        assert_eq!(
            evidence_copy(&[100, -100]),
            vec![1000.0 / 32768.0, -1000.0 / 32768.0]
        );
        assert_eq!(evidence_copy(&[4000, -4000]), vec![32767.0 / 32768.0, -1.0]);
        let v = vad_input(&[100]);
        assert!((f64::from(v[0]) - 3_162.277_66 / 32_768.0).abs() < 1e-6);
        assert_eq!(vad_input(&[2000, -2000]), vec![32767.0 / 32768.0, -1.0]);
    }
}
