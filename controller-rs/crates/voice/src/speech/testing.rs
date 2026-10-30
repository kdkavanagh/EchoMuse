//! Stand-in models for tests that have no model files.
//!
//! Each word of a [`ToneModels`] vocabulary is "spoken" as a pure tone: word `i` is a sine
//! at [`tone_hz`]`(i)` = 400 + 150·i Hz. The stand-ins recognise words by frequency, never
//! by position, so a fresh decode of any window that holds a word's tone hears the same
//! word, the way the real recognizer hears the same speech wherever a window starts.
//!
//! - **ASR**: the stream cuts its input into 40 ms frames. A frame is voiced when its RMS
//!   is above 0.01 of full scale; its frequency comes from its zero crossings. A burst of
//!   at least three frames of one word's frequency ends after three unvoiced frames, and
//!   the word is emitted then as the token `" word"`, stamped 0.12 s after the burst's
//!   last frame (seconds from the stream's start). Trailing blanks count the 40 ms frames
//!   since the last token, or since the stream started.
//! - **VAD**: 0.95 for a cell whose input RMS is above 0.01 of full scale, else 0.02.
//! - **Denoiser**: returns its input.
//! - **Wake scorer**: below the RMS floor for a near-silent window; otherwise 0.95 when at
//!   least 300 ms of the window is the first word's tone, else 0.05.
//!
//! Each model can be made to fail, to exercise the workers' failure paths.

use std::sync::Arc;

use bytes::Bytes;
use echomuse_echo::WakeWindowScorer;
use echomuse_speech::bcresnet::{RMS_FLOOR as WAKE_RMS_FLOOR, WAKE_WINDOW_SAMPLES as WAKE_WINDOW};
use echomuse_speech::{AsrResult, BcresnetModelFiles, SpeechError, WindowScore};

use super::Pcm16Le;
use super::models::{AsrStream, BoxedWakeScorer, Denoise, ModelSet, Vad};

const SAMPLE_RATE: f64 = 16_000.0;
/// One analysis frame: 40 ms, the real decoder's blank frame.
const FRAME: usize = 640;
const FRAME_SECONDS: f64 = 0.04;
/// RMS (fraction of full scale) above which a frame or cell counts as voiced.
const VOICED_RMS: f64 = 0.01;
/// How far a frame's frequency may be from a word's tone and still be that word.
const TONE_TOLERANCE_HZ: f64 = 40.0;
/// Frames of one word a burst needs, and unvoiced frames that end it.
const BURST_FRAMES: u32 = 3;
const END_FRAMES: u32 = 3;
/// How long after a burst's last frame its word is stamped.
const EMIT_DELAY_SECONDS: f64 = 0.12;
/// Wake tone a window needs to score high: 300 ms.
const WAKE_TONE_SAMPLES: usize = 4_800;

/// The frequency word `index` is spoken at.
pub fn tone_hz(index: usize) -> f64 {
    400.0 + 150.0 * index as f64
}

/// `ms` milliseconds of word `word_index`'s tone at peak `amplitude` (canonical int16).
pub fn tone(word_index: usize, ms: u64, amplitude: i16) -> Vec<i16> {
    let n = samples(ms);
    let w = 2.0 * std::f64::consts::PI * tone_hz(word_index) / SAMPLE_RATE;
    // A phase offset keeps samples off exact zero crossings.
    (0..n)
        .map(|i| (f64::from(amplitude) * (w * i as f64 + 0.3).sin()).round() as i16)
        .collect()
}

/// `ms` milliseconds of digital silence.
pub fn silence(ms: u64) -> Vec<i16> {
    vec![0; samples(ms)]
}

fn samples(ms: u64) -> usize {
    (ms * 16) as usize
}

/// Canonical 16 kHz PCM of `words`: each word's tone for `word_ms`, followed by `gap_ms`
/// of silence.
///
/// # Panics
///
/// When a word is not in `models`' vocabulary: a mistake in the test itself.
pub fn utterance(
    models: &ToneModels,
    words: &[&str],
    word_ms: u64,
    gap_ms: u64,
    amplitude: i16,
) -> Vec<i16> {
    let mut pcm = Vec::new();
    for word in words {
        let index = models
            .index(word)
            .unwrap_or_else(|| panic!("{word:?} is not in the stand-in vocabulary"));
        pcm.extend(tone(index, word_ms, amplitude));
        pcm.extend(silence(gap_ms));
    }
    pcm
}

/// `samples` as the little-endian PCM a job carries.
pub fn pcm16(samples: &[i16]) -> Pcm16Le {
    Pcm16Le::new(Bytes::from(
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>(),
    ))
}

/// Which stand-in models fail, and how.
#[derive(Clone, Copy, Debug, Default)]
struct Failures {
    asr: bool,
    vad: bool,
    denoise: bool,
    wake_load: bool,
}

/// Deterministic stand-in models over a word list (see the module documentation).
#[derive(Clone, Debug)]
pub struct ToneModels {
    words: Arc<[String]>,
    fail: Failures,
}

impl ToneModels {
    /// Stand-ins whose vocabulary is `words`, word `i` at [`tone_hz`]`(i)`.
    pub fn new(words: &[&str]) -> Self {
        Self {
            words: words.iter().map(|w| (*w).to_owned()).collect(),
            fail: Failures::default(),
        }
    }

    /// The vocabulary.
    pub fn words(&self) -> &[String] {
        &self.words
    }

    /// The index of `word`.
    pub fn index(&self, word: &str) -> Option<usize> {
        self.words.iter().position(|w| w == word)
    }

    /// Every ASR stream fails when asked for its result.
    pub fn failing_asr(mut self) -> Self {
        self.fail.asr = true;
        self
    }

    /// Every VAD session fails on its first cell.
    pub fn failing_vad(mut self) -> Self {
        self.fail.vad = true;
        self
    }

    /// Every denoiser fails.
    pub fn failing_denoiser(mut self) -> Self {
        self.fail.denoise = true;
        self
    }

    /// Wake graphs cannot be loaded.
    pub fn failing_wake_graph(mut self) -> Self {
        self.fail.wake_load = true;
        self
    }
}

fn stand_in_failure(what: &str) -> SpeechError {
    SpeechError::InvalidOutput {
        model: "stand-in",
        detail: format!("{what} was made to fail"),
    }
}

impl ModelSet for ToneModels {
    fn asr_stream(&self) -> Result<Box<dyn AsrStream>, SpeechError> {
        Ok(Box::new(ToneStream::new(self.words.len(), self.clone())))
    }

    fn vad(&self) -> Result<Box<dyn Vad>, SpeechError> {
        Ok(Box::new(ToneVad {
            fail: self.fail.vad,
        }))
    }

    fn denoiser(&self) -> Result<Box<dyn Denoise>, SpeechError> {
        Ok(Box::new(ToneDenoiser {
            fail: self.fail.denoise,
        }))
    }

    fn wake_scorer(&self, _files: &BcresnetModelFiles) -> Result<BoxedWakeScorer, SpeechError> {
        if self.fail.wake_load {
            return Err(stand_in_failure("the wake graph"));
        }
        Ok(Box::new(ToneWakeScorer {
            words: self.words.len(),
        }))
    }
}

fn rms(x: &[f32]) -> f64 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64).sqrt()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameClass {
    Silent,
    Word(usize),
    /// Voiced but no word's tone: a frame that straddles a tone's edge.
    Other,
}

/// Classifies one frame against a vocabulary of `words` tones.
fn classify(frame: &[f32], words: usize) -> FrameClass {
    if rms(frame) <= VOICED_RMS {
        return FrameClass::Silent;
    }
    // Sign changes between nonzero samples; zeros (silence inside the frame) are skipped,
    // so the estimate reads only the tone's own span.
    let mut last_sign = 0i8;
    let mut crossings = Vec::new();
    for (i, &v) in frame.iter().enumerate() {
        let sign = if v > 0.0 {
            1
        } else if v < 0.0 {
            -1
        } else {
            continue;
        };
        if last_sign != 0 && sign != last_sign {
            crossings.push(i);
        }
        last_sign = sign;
    }
    let (Some(&first), Some(&last)) = (crossings.first(), crossings.last()) else {
        return FrameClass::Other;
    };
    if crossings.len() < 3 || last == first {
        return FrameClass::Other;
    }
    let hz = (crossings.len() - 1) as f64 * SAMPLE_RATE / (2.0 * (last - first) as f64);
    let index = ((hz - tone_hz(0)) / 150.0).round();
    if index < 0.0 || index as usize >= words {
        return FrameClass::Other;
    }
    let index = index as usize;
    if (hz - tone_hz(index)).abs() <= TONE_TOLERANCE_HZ {
        FrameClass::Word(index)
    } else {
        FrameClass::Other
    }
}

/// The stand-in streaming recognizer.
struct ToneStream {
    words: usize,
    models: ToneModels,
    pending: Vec<f32>,
    /// Frames analysed.
    frames: u64,
    /// The burst under way: its word, its frame count, and the end of its last frame.
    burst: Option<(usize, u32, u64)>,
    unvoiced: u32,
    tokens: Vec<String>,
    timestamps: Vec<f32>,
    last_token_frame: u64,
}

impl ToneStream {
    fn new(words: usize, models: ToneModels) -> Self {
        Self {
            words,
            models,
            pending: Vec::new(),
            frames: 0,
            burst: None,
            unvoiced: 0,
            tokens: Vec::new(),
            timestamps: Vec::new(),
            last_token_frame: 0,
        }
    }

    fn close_burst(&mut self) {
        if let Some((word, count, end_frame)) = self.burst.take()
            && count >= BURST_FRAMES
        {
            self.tokens.push(format!(" {}", self.models.words[word]));
            let seconds = end_frame as f64 * FRAME_SECONDS + EMIT_DELAY_SECONDS;
            self.timestamps.push(seconds as f32);
            self.last_token_frame = self.frames;
        }
    }

    fn frame(&mut self, class: FrameClass) {
        self.frames += 1;
        match class {
            FrameClass::Silent => {
                self.unvoiced += 1;
                if self.unvoiced >= END_FRAMES {
                    self.close_burst();
                }
            }
            FrameClass::Word(word) => {
                self.unvoiced = 0;
                match &mut self.burst {
                    Some((w, count, end)) if *w == word => {
                        *count += 1;
                        *end = self.frames;
                    }
                    _ => {
                        self.close_burst();
                        self.burst = Some((word, 1, self.frames));
                    }
                }
            }
            FrameClass::Other => self.unvoiced = 0,
        }
    }
}

impl AsrStream for ToneStream {
    fn accept(&mut self, samples: &[f32]) -> Result<(), SpeechError> {
        if samples.iter().any(|v| !v.is_finite()) {
            return Err(SpeechError::InvalidInput("samples are not finite".into()));
        }
        self.pending.extend_from_slice(samples);
        Ok(())
    }

    fn decode_ready(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        let (frames, rest) = pending.as_chunks::<FRAME>();
        for frame in frames {
            let class = classify(frame, self.words);
            self.frame(class);
        }
        self.pending = rest.to_vec();
    }

    fn result(&mut self) -> Result<AsrResult, SpeechError> {
        if self.models.fail.asr {
            return Err(stand_in_failure("the ASR stream"));
        }
        Ok(AsrResult {
            text: self
                .tokens
                .iter()
                .map(|t| t.trim())
                .collect::<Vec<_>>()
                .join(" "),
            tokens: self.tokens.clone(),
            timestamps: self.timestamps.clone(),
            num_trailing_blanks: u32::try_from(self.frames - self.last_token_frame)
                .unwrap_or(u32::MAX),
        })
    }
}

struct ToneVad {
    fail: bool,
}

impl Vad for ToneVad {
    fn process_cell(&mut self, cell: &[f32; 512]) -> Result<f32, SpeechError> {
        if self.fail {
            return Err(stand_in_failure("the VAD"));
        }
        Ok(if rms(cell) > VOICED_RMS { 0.95 } else { 0.02 })
    }

    fn reset(&mut self) {}
}

struct ToneDenoiser {
    fail: bool,
}

impl Denoise for ToneDenoiser {
    fn denoise_span(&mut self, input: &[f32]) -> Result<Vec<f32>, SpeechError> {
        if self.fail {
            return Err(stand_in_failure("the denoiser"));
        }
        Ok(input.to_vec())
    }
}

struct ToneWakeScorer {
    words: usize,
}

impl WakeWindowScorer for ToneWakeScorer {
    type Error = SpeechError;

    fn score_window(&mut self, pcm: &[i16]) -> Result<WindowScore, SpeechError> {
        if pcm.len() != WAKE_WINDOW {
            return Err(SpeechError::InvalidInput(format!(
                "a wake window is {WAKE_WINDOW} samples, not {}",
                pcm.len()
            )));
        }
        let x: Vec<f32> = pcm.iter().map(|&s| f32::from(s) / 32_768.0).collect();
        if rms(&x) < WAKE_RMS_FLOOR {
            return Ok(WindowScore::BelowRmsFloor);
        }
        // Peak-normalised like the real scorer, so the score does not depend on level.
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        let x: Vec<f32> = x.iter().map(|v| v * 0.5 / peak).collect();
        let wake_frames = x
            .as_chunks::<FRAME>()
            .0
            .iter()
            .filter(|f| classify(&f[..], self.words) == FrameClass::Word(0))
            .count();
        let probability = if wake_frames * FRAME >= WAKE_TONE_SAMPLES {
            0.95
        } else {
            0.05
        };
        Ok(WindowScore::Scored { probability })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::speech::models::{evidence_copy, fresh_decode, vad_input};

    fn models() -> ToneModels {
        ToneModels::new(&["ophelia", "turn", "on", "the", "kitchen", "light"])
    }

    fn words(r: &AsrResult) -> Vec<&str> {
        r.tokens.iter().map(|t| t.trim()).collect()
    }

    #[test]
    fn an_utterance_decodes_to_its_words_from_any_window() {
        let m = models();
        let mut pcm = silence(300);
        pcm.extend(utterance(
            &m,
            &["ophelia", "turn", "on", "the", "kitchen", "light"],
            240,
            160,
            1000,
        ));
        let whole = fresh_decode(&m, &evidence_copy(&pcm)).unwrap();
        assert_eq!(whole.text, "ophelia turn on the kitchen light");
        for shift in [1usize, 17, 333, 640, 1001] {
            let r = fresh_decode(&m, &evidence_copy(&pcm[shift..])).unwrap();
            assert_eq!(words(&r), words(&whole), "window shifted by {shift}");
            for (a, b) in r.timestamps.iter().zip(&whole.timestamps) {
                let moved = f64::from(*b) - shift as f64 / 16_000.0;
                assert!((f64::from(*a) - moved).abs() <= 0.041, "{a} vs {moved}");
            }
        }
    }

    #[test]
    fn tokens_are_stamped_after_their_burst_and_blanks_count_from_the_last_one() {
        let m = models();
        let mut pcm = utterance(&m, &["turn"], 400, 0, 1000);
        pcm.extend(silence(1000));
        let mut s = m.asr_stream().unwrap();
        s.accept(&evidence_copy(&pcm)).unwrap();
        s.decode_ready();
        let r = s.result().unwrap();
        assert_eq!(r.tokens, vec![" turn".to_owned()]);
        // The tone fills ten whole frames; the word is stamped 0.12 s after the tenth.
        assert!((r.timestamps[0] - 0.52).abs() < 1e-6);
        // 35 frames in all; the token was emitted at the 13th.
        assert_eq!(r.num_trailing_blanks, 35 - 13);
    }

    #[test]
    fn a_short_blip_is_not_a_word() {
        let m = models();
        let mut pcm = tone(1, 80, 1000);
        pcm.extend(silence(500));
        let r = fresh_decode(&m, &evidence_copy(&pcm)).unwrap();
        assert!(r.tokens.is_empty());
        assert_eq!(r.text, "");
    }

    #[test]
    fn vad_flags_tone_cells() {
        let m = models();
        let mut vad = m.vad().unwrap();
        let mut pcm = tone(2, 64, 300);
        pcm.extend(silence(64));
        let input = vad_input(&pcm);
        let probs: Vec<f32> = input
            .as_chunks::<512>()
            .0
            .iter()
            .map(|c| vad.process_cell(c).unwrap())
            .collect();
        assert_eq!(probs, vec![0.95, 0.95, 0.02, 0.02]);
    }

    #[test]
    fn wake_scorer_hears_the_first_word() {
        let m = models();
        let files = BcresnetModelFiles {
            graph: echomuse_speech::PinnedFile::new(
                "g",
                echomuse_speech::Sha256Digest::of_bytes(b""),
            ),
            sidecar: echomuse_speech::PinnedFile::new(
                "s",
                echomuse_speech::Sha256Digest::of_bytes(b""),
            ),
        };
        let mut s = m.wake_scorer(&files).unwrap();
        let window = |pcm: Vec<i16>| {
            let mut w = pcm;
            w.resize(WAKE_WINDOW, 0);
            w
        };
        assert_eq!(
            s.score_window(&silence(1400)).unwrap(),
            WindowScore::BelowRmsFloor
        );
        assert_eq!(
            s.score_window(&window(tone(0, 400, 8000))).unwrap(),
            WindowScore::Scored { probability: 0.95 }
        );
        assert_eq!(
            s.score_window(&window(tone(0, 200, 8000))).unwrap(),
            WindowScore::Scored { probability: 0.05 }
        );
        assert_eq!(
            s.score_window(&window(tone(3, 800, 8000))).unwrap(),
            WindowScore::Scored { probability: 0.05 }
        );
        assert!(s.score_window(&silence(10)).is_err());
        assert!(m.failing_wake_graph().wake_scorer(&files).is_err());
    }
}
