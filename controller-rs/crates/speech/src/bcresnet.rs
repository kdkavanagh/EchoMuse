//! The BCResNet wake model: its sidecar, window preparation and scoring.
//!
//! This is the deployed artifact contract of the post-AFE architecture, §5.1, as the Go
//! firmware implements it (`device/internal/wakeword/bcresnet`): the graph takes `audio`,
//! float32 `[batch, 22400]` (1.4 s at 16 kHz), and returns `logits`, float32
//! `[batch, labels]`, with no softmax. Each int16 window is prepared as
//!
//! ```text
//! x = pcm / 32768
//! if sqrt(mean(x²)) < 1e-4: not scored
//! p = max|x|; if p > 1e-4: x = x · (normPeak / p)
//! prob = softmax(logits − max(logits))[wakeIndex]
//! ```
//!
//! The scorer holds no streaming state: hops and smoothing belong to its caller.

use ort::session::Session;
use ort::value::{TensorElementType, TensorRef};
use serde::{Deserialize, Serialize};

use crate::digest::PinnedFile;
use crate::error::SpeechError;
use crate::native::OrtRuntime;
use crate::onnx::{Dim, TensorSpec, check_io, extract_f32, load_session};

/// The only sample rate the graph accepts.
pub const WAKE_SAMPLE_RATE: u32 = 16_000;
/// Samples in one window: 1.4 s at 16 kHz.
pub const WAKE_WINDOW_SAMPLES: usize = 22_400;
/// A window whose RMS (of `pcm / 32768`) is below this is not scored.
pub const RMS_FLOOR: f64 = 1e-4;
/// A window whose peak is at or below this is not rescaled.
pub const PEAK_GUARD: f32 = 1e-4;

const MODEL: &str = "bcresnet";

/// The graph file and its sidecar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BcresnetModelFiles {
    /// `bcresnet_audio.onnx` or an uploaded graph.
    pub graph: PinnedFile,
    /// Its sidecar JSON.
    pub sidecar: PinnedFile,
}

/// The sidecar shipped beside the graph. Field names are the exporter's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WakeSidecar {
    /// Always `bcresnet`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Must be 16,000.
    pub sample_rate: u32,
    /// Must be 22,400.
    pub window: usize,
    /// Class names, one per logit.
    pub labels: Vec<String>,
    /// Index of the wake class in `labels`.
    pub wake_index: usize,
    /// Mel bins of the in-graph front end (informational).
    pub n_mels: u32,
    /// Window length in seconds (informational).
    pub clip_seconds: f64,
    /// The peak each window is normalised to, in (0, 1].
    pub norm_peak: f64,
}

impl WakeSidecar {
    /// Decodes and validates sidecar JSON.
    pub fn parse(json: &[u8]) -> Result<Self, SpeechError> {
        let s: Self = serde_json::from_slice(json)
            .map_err(|e| SpeechError::Sidecar(format!("not valid sidecar JSON: {e}")))?;
        s.validate()?;
        Ok(s)
    }

    /// The load rules the sidecar alone decides: type `bcresnet`, 16 kHz, a 22,400-sample
    /// window, a wake index inside the labels and a normalisation peak in (0, 1].
    pub fn validate(&self) -> Result<(), SpeechError> {
        let fail = |m: String| Err(SpeechError::Sidecar(m));
        if self.kind != "bcresnet" {
            return fail(format!("type {:?}, want \"bcresnet\"", self.kind));
        }
        if self.sample_rate != WAKE_SAMPLE_RATE {
            return fail(format!(
                "sampleRate {}, want {WAKE_SAMPLE_RATE}",
                self.sample_rate
            ));
        }
        if self.window != WAKE_WINDOW_SAMPLES {
            return fail(format!(
                "window {}, want {WAKE_WINDOW_SAMPLES}",
                self.window
            ));
        }
        if self.labels.is_empty() {
            return fail("no labels".into());
        }
        if self.wake_index >= self.labels.len() {
            return fail(format!(
                "wakeIndex {} outside {} labels",
                self.wake_index,
                self.labels.len()
            ));
        }
        if !(self.norm_peak > 0.0 && self.norm_peak <= 1.0) {
            return fail(format!("normPeak {} outside (0, 1]", self.norm_peak));
        }
        Ok(())
    }
}

/// Writes the model input for one int16 window into `dst`. Returns `false`, leaving `dst`
/// unusable as model input, when the window is below the RMS floor.
pub fn prepare_window(dst: &mut [f32], pcm: &[i16], norm_peak: f64) -> Result<bool, SpeechError> {
    if pcm.len() != WAKE_WINDOW_SAMPLES || dst.len() != WAKE_WINDOW_SAMPLES {
        return Err(SpeechError::InvalidInput(format!(
            "window of {} samples into {}, want {WAKE_WINDOW_SAMPLES}",
            pcm.len(),
            dst.len()
        )));
    }
    let mut sum_sq = 0.0f64;
    let mut peak = 0.0f32;
    for (d, &v) in dst.iter_mut().zip(pcm) {
        let x = f32::from(v) / 32768.0;
        *d = x;
        sum_sq += f64::from(x) * f64::from(x);
        peak = peak.max(x.abs());
    }
    if (sum_sq / pcm.len() as f64).sqrt() < RMS_FLOOR {
        return Ok(false);
    }
    if peak > PEAK_GUARD {
        // The gain is computed in f64 and applied in f32, as the device does.
        let g = (norm_peak / f64::from(peak)) as f32;
        for x in dst.iter_mut() {
            *x *= g;
        }
    }
    Ok(true)
}

/// `softmax(logits − max(logits))[wake_index]`, or `None` when a logit is not finite or the
/// index is out of range.
pub fn wake_probability(logits: &[f32], wake_index: usize) -> Option<f64> {
    if wake_index >= logits.len() || logits.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let m = logits
        .iter()
        .fold(f64::NEG_INFINITY, |m, &v| m.max(f64::from(v)));
    let mut sum = 0.0;
    let mut wake = 0.0;
    for (i, &v) in logits.iter().enumerate() {
        let e = (f64::from(v) - m).exp();
        sum += e;
        if i == wake_index {
            wake = e;
        }
    }
    Some(wake / sum)
}

/// The score of one window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowScore {
    /// The window's RMS is below the floor; the graph did not run.
    BelowRmsFloor,
    /// The graph ran.
    Scored {
        /// The wake probability.
        probability: f64,
    },
}

/// One wake graph with its sidecar, owned by one worker.
pub struct BcresnetScorer {
    session: Session,
    sidecar: WakeSidecar,
    prepared: Vec<f32>,
    logits: Vec<f32>,
}

impl std::fmt::Debug for BcresnetScorer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BcresnetScorer")
            .field("sidecar", &self.sidecar)
            .finish_non_exhaustive()
    }
}

impl BcresnetScorer {
    /// Verifies both files, parses the sidecar, loads the graph and checks it has input
    /// `audio` `[batch, 22400]` and output `logits` `[batch, labels]`.
    pub fn load(runtime: &OrtRuntime, files: &BcresnetModelFiles) -> Result<Self, SpeechError> {
        let sidecar = WakeSidecar::parse(&files.sidecar.read_verified()?)?;
        let session = load_session(runtime, &files.graph)?;
        check_io(
            MODEL,
            &session,
            &[TensorSpec {
                name: "audio",
                ty: TensorElementType::Float32,
                dims: &[Dim::Batch, Dim::Exactly(WAKE_WINDOW_SAMPLES as i64)],
            }],
            &[TensorSpec {
                name: "logits",
                ty: TensorElementType::Float32,
                // The label count is checked against the sidecar just below.
                dims: &[Dim::Batch, Dim::Any],
            }],
        )?;
        let declared = session.outputs()[0]
            .dtype()
            .tensor_shape()
            .map(|s| s[1])
            .unwrap_or(-1);
        if declared != sidecar.labels.len() as i64 {
            return Err(SpeechError::ModelContract {
                model: MODEL,
                detail: format!(
                    "graph emits {declared} logits, sidecar names {} labels",
                    sidecar.labels.len()
                ),
            });
        }
        Ok(Self {
            session,
            prepared: vec![0.0; WAKE_WINDOW_SAMPLES],
            logits: Vec::with_capacity(sidecar.labels.len()),
            sidecar,
        })
    }

    /// The validated sidecar.
    pub fn sidecar(&self) -> &WakeSidecar {
        &self.sidecar
    }

    /// Prepares one 22,400-sample int16 window and scores it.
    pub fn score(&mut self, pcm: &[i16]) -> Result<WindowScore, SpeechError> {
        let mut prepared = std::mem::take(&mut self.prepared);
        let result = match prepare_window(&mut prepared, pcm, self.sidecar.norm_peak) {
            Ok(false) => Ok(WindowScore::BelowRmsFloor),
            Ok(true) => self.run_into_logits(&prepared).and_then(|()| {
                wake_probability(&self.logits, self.sidecar.wake_index)
                    .map(|probability| WindowScore::Scored { probability })
                    .ok_or_else(|| SpeechError::InvalidOutput {
                        model: MODEL,
                        detail: "non-finite wake probability".into(),
                    })
            }),
            Err(e) => Err(e),
        };
        self.prepared = prepared;
        result
    }

    /// Runs the graph on an already-prepared window and returns its logits.
    pub fn logits(&mut self, prepared: &[f32]) -> Result<&[f32], SpeechError> {
        if prepared.len() != WAKE_WINDOW_SAMPLES {
            return Err(SpeechError::InvalidInput(format!(
                "prepared window of {} samples, want {WAKE_WINDOW_SAMPLES}",
                prepared.len()
            )));
        }
        if let Some(i) = prepared.iter().position(|x| !x.is_finite()) {
            return Err(SpeechError::InvalidInput(format!(
                "sample {i} is not finite"
            )));
        }
        self.run_into_logits(prepared)?;
        Ok(&self.logits)
    }

    fn run_into_logits(&mut self, prepared: &[f32]) -> Result<(), SpeechError> {
        let input = TensorRef::from_array_view(([1usize, WAKE_WINDOW_SAMPLES], prepared))?;
        let outputs = self.session.run(ort::inputs![input])?;
        let n = self.sidecar.labels.len();
        let data = extract_f32(MODEL, "logits", &outputs[0], n)?;
        self.logits.clear();
        self.logits.extend_from_slice(data);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn repo_sidecar() -> Vec<u8> {
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../bcresnet_audio.json"))
            .unwrap()
    }

    #[test]
    fn parses_the_deployed_sidecar() {
        let s = WakeSidecar::parse(&repo_sidecar()).unwrap();
        assert_eq!(s.labels, vec!["noise", "ohphelia", "unknown"]);
        assert_eq!(s.wake_index, 1);
        assert_eq!(s.norm_peak, 0.8);
    }

    #[test]
    fn sidecar_load_rules() {
        let good: serde_json::Value = serde_json::from_slice(&repo_sidecar()).unwrap();
        let with = |key: &str, v: serde_json::Value| {
            let mut j = good.clone();
            j[key] = v;
            WakeSidecar::parse(serde_json::to_string(&j).unwrap().as_bytes())
        };
        for (key, v) in [
            ("type", serde_json::json!("oww")),
            ("sampleRate", serde_json::json!(8000)),
            ("window", serde_json::json!(16000)),
            ("labels", serde_json::json!([])),
            ("wakeIndex", serde_json::json!(3)),
            ("normPeak", serde_json::json!(0.0)),
            ("normPeak", serde_json::json!(1.5)),
            ("wakeIndex", serde_json::json!(-1)),
        ] {
            assert!(
                matches!(with(key, v.clone()), Err(SpeechError::Sidecar(_))),
                "accepted {key} = {v}"
            );
        }
        assert!(matches!(
            WakeSidecar::parse(b"{"),
            Err(SpeechError::Sidecar(_))
        ));
    }

    #[test]
    fn preparation_follows_the_contract() {
        let mut dst = vec![0.0; WAKE_WINDOW_SAMPLES];
        // Silence is below the floor.
        assert!(!prepare_window(&mut dst, &vec![0i16; WAKE_WINDOW_SAMPLES], 0.8).unwrap());
        // A loud window is scaled to the peak.
        let mut pcm = vec![0i16; WAKE_WINDOW_SAMPLES];
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = if i % 2 == 0 { 1000 } else { -2000 };
        }
        assert!(prepare_window(&mut dst, &pcm, 0.8).unwrap());
        let peak = dst.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.8).abs() < 1e-6, "peak {peak}");
        assert!((dst[0] / dst[1] + 0.5).abs() < 1e-6);
        // Wrong lengths are errors.
        assert!(prepare_window(&mut dst, &pcm[..100], 0.8).is_err());
        assert!(prepare_window(&mut dst[..100], &pcm, 0.8).is_err());
    }

    #[test]
    fn window_just_above_the_floor_is_not_rescaled_below_the_guard() {
        // Every sample ±3 → x = 9.2e-5 per sample: RMS below 1e-4, so not scored.
        let mut dst = vec![0.0; WAKE_WINDOW_SAMPLES];
        assert!(!prepare_window(&mut dst, &vec![3i16; WAKE_WINDOW_SAMPLES], 0.8).unwrap());
        // ±4 → 1.22e-4: above the RMS floor and above the peak guard, so rescaled.
        assert!(prepare_window(&mut dst, &vec![4i16; WAKE_WINDOW_SAMPLES], 0.8).unwrap());
        assert!((dst[0] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn probability_is_a_stable_softmax() {
        let p = wake_probability(&[0.0, 0.0, 0.0], 1).unwrap();
        assert!((p - 1.0 / 3.0).abs() < 1e-12);
        let p = wake_probability(&[1000.0, 1001.0, 999.0], 1).unwrap();
        assert!(p > 0.66 && p < 0.67);
        assert_eq!(wake_probability(&[0.0, f32::NAN], 0), None);
        assert_eq!(wake_probability(&[0.0, 1.0], 2), None);
    }
}
