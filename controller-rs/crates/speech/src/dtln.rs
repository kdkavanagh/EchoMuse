//! DTLN streaming noise suppression (Westhausen & Meyer, `breizhn/DTLN`, MIT).
//!
//! This is the upstream real-time algorithm (`real_time_processing_onnx.py` at the pinned
//! commit): a 512-sample block advanced 128 samples at a time.
//!
//! 1. Shift the input buffer left by 128 and append the next 128 samples.
//! 2. Take the real FFT of the buffer; stage one (`model_1.onnx`) maps the magnitude
//!    `[1, 1, 257]` and its LSTM state `[1, 2, 128, 2]` to a mask and a new state.
//! 3. The masked magnitude with the original phase goes back through the inverse FFT; stage two
//!    (`model_2.onnx`) maps that block `[1, 1, 512]` and its own state to an output block.
//! 4. Overlap-add the output block into the output buffer, emit its first 128 samples, shift.
//!
//! The transforms run in `f64`; tensors crossing into ONNX Runtime are `f32`, as upstream. Each
//! emitted sample is the denoised input from [`DTLN_LATENCY_SAMPLES`] earlier.

use std::sync::Arc;

use ort::session::Session;
use ort::value::{TensorElementType, TensorRef};
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use crate::digest::PinnedFile;
use crate::error::SpeechError;
use crate::native::OrtRuntime;
use crate::onnx::{Dim, TensorSpec, check_finite, check_io, extract_f32, load_session};

/// Samples in one analysis block.
pub const DTLN_BLOCK_LEN: usize = 512;
/// Samples the block advances per step.
pub const DTLN_BLOCK_SHIFT: usize = 128;
/// How far output lags input: `block − shift` samples (24 ms at 16 kHz).
pub const DTLN_LATENCY_SAMPLES: usize = DTLN_BLOCK_LEN - DTLN_BLOCK_SHIFT;

const BINS: usize = DTLN_BLOCK_LEN / 2 + 1;
const STATE_LEN: usize = 2 * 128 * 2;
const STATE_DIMS: &[Dim] = &[
    Dim::Exactly(1),
    Dim::Exactly(2),
    Dim::Exactly(128),
    Dim::Exactly(2),
];

/// The two DTLN stages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DtlnModelFiles {
    /// Stage one, the spectral mask.
    pub model_1: PinnedFile,
    /// Stage two, the time-domain block.
    pub model_2: PinnedFile,
}

/// A streaming DTLN denoiser, owned by one worker.
pub struct DtlnDenoiser {
    stage1: Session,
    stage2: Session,
    state1: Vec<f32>,
    state2: Vec<f32>,
    in_buffer: [f32; DTLN_BLOCK_LEN],
    out_buffer: [f32; DTLN_BLOCK_LEN],
    /// Input samples not yet making up a full 128-sample shift.
    pending: Vec<f32>,
    fft: Arc<dyn RealToComplex<f64>>,
    ifft: Arc<dyn ComplexToReal<f64>>,
    time: Vec<f64>,
    spectrum: Vec<Complex<f64>>,
    magnitude: Vec<f32>,
    phase: Vec<f64>,
    block: Vec<f32>,
    fft_scratch: Vec<Complex<f64>>,
}

impl std::fmt::Debug for DtlnDenoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DtlnDenoiser")
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl DtlnDenoiser {
    /// Verifies and loads both stages and checks their inputs and outputs.
    pub fn load(runtime: &OrtRuntime, files: &DtlnModelFiles) -> Result<Self, SpeechError> {
        let stage1 = load_session(runtime, &files.model_1)?;
        check_io(
            "dtln_model_1",
            &stage1,
            &[
                TensorSpec {
                    name: "input_2",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Exactly(1), Dim::Exactly(1), Dim::Exactly(BINS as i64)],
                },
                TensorSpec {
                    name: "input_3",
                    ty: TensorElementType::Float32,
                    dims: STATE_DIMS,
                },
            ],
            &[
                TensorSpec {
                    name: "activation_2",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Exactly(1), Dim::Exactly(1), Dim::Exactly(BINS as i64)],
                },
                TensorSpec {
                    name: "tf_op_layer_stack_2",
                    ty: TensorElementType::Float32,
                    dims: STATE_DIMS,
                },
            ],
        )?;
        let stage2 = load_session(runtime, &files.model_2)?;
        check_io(
            "dtln_model_2",
            &stage2,
            &[
                TensorSpec {
                    name: "input_4",
                    ty: TensorElementType::Float32,
                    dims: &[
                        Dim::Exactly(1),
                        Dim::Exactly(1),
                        Dim::Exactly(DTLN_BLOCK_LEN as i64),
                    ],
                },
                TensorSpec {
                    name: "input_5",
                    ty: TensorElementType::Float32,
                    dims: STATE_DIMS,
                },
            ],
            &[
                TensorSpec {
                    name: "conv1d_3",
                    ty: TensorElementType::Float32,
                    dims: &[
                        Dim::Exactly(1),
                        Dim::Exactly(1),
                        Dim::Exactly(DTLN_BLOCK_LEN as i64),
                    ],
                },
                TensorSpec {
                    name: "tf_op_layer_stack_5",
                    ty: TensorElementType::Float32,
                    dims: STATE_DIMS,
                },
            ],
        )?;
        let mut planner = RealFftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(DTLN_BLOCK_LEN);
        let ifft = planner.plan_fft_inverse(DTLN_BLOCK_LEN);
        let scratch_len = fft.get_scratch_len().max(ifft.get_scratch_len());
        Ok(Self {
            stage1,
            stage2,
            state1: vec![0.0; STATE_LEN],
            state2: vec![0.0; STATE_LEN],
            in_buffer: [0.0; DTLN_BLOCK_LEN],
            out_buffer: [0.0; DTLN_BLOCK_LEN],
            pending: Vec::with_capacity(DTLN_BLOCK_SHIFT),
            time: vec![0.0; DTLN_BLOCK_LEN],
            spectrum: vec![Complex::new(0.0, 0.0); BINS],
            magnitude: vec![0.0; BINS],
            phase: vec![0.0; BINS],
            block: vec![0.0; DTLN_BLOCK_LEN],
            fft_scratch: vec![Complex::new(0.0, 0.0); scratch_len],
            fft,
            ifft,
        })
    }

    /// Returns to the initial state: zero buffers, zero LSTM states, nothing pending.
    pub fn reset(&mut self) {
        self.state1.fill(0.0);
        self.state2.fill(0.0);
        self.in_buffer = [0.0; DTLN_BLOCK_LEN];
        self.out_buffer = [0.0; DTLN_BLOCK_LEN];
        self.pending.clear();
    }

    /// Feeds 16 kHz samples and appends every completed 128-sample output step to `out`. Output
    /// lags input by [`DTLN_LATENCY_SAMPLES`]; samples short of a full step wait for the next
    /// call. On error the denoiser must be [`reset`](Self::reset) before further use.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), SpeechError> {
        if let Some(i) = input.iter().position(|x| !x.is_finite()) {
            return Err(SpeechError::InvalidInput(format!(
                "sample {i} is not finite"
            )));
        }
        let mut rest = input;
        while !rest.is_empty() {
            let take = (DTLN_BLOCK_SHIFT - self.pending.len()).min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == DTLN_BLOCK_SHIFT {
                self.step(out)?;
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Denoises a whole span from a fresh state: resets, processes `input`, pushes zeros through
    /// to drain the latency, and returns exactly `input.len()` samples aligned with the input.
    /// Leaves the denoiser reset.
    pub fn denoise_span(&mut self, input: &[f32]) -> Result<Vec<f32>, SpeechError> {
        self.reset();
        let mut out = Vec::with_capacity(input.len() + DTLN_LATENCY_SAMPLES + DTLN_BLOCK_SHIFT);
        let result = self.process(input, &mut out).and_then(|()| {
            let tail =
                DTLN_LATENCY_SAMPLES + (DTLN_BLOCK_SHIFT - self.pending.len()) % DTLN_BLOCK_SHIFT;
            self.process(&vec![0.0; tail], &mut out)
        });
        self.reset();
        result?;
        out.drain(..DTLN_LATENCY_SAMPLES);
        out.truncate(input.len());
        Ok(out)
    }

    fn step(&mut self, out: &mut Vec<f32>) -> Result<(), SpeechError> {
        self.in_buffer.copy_within(DTLN_BLOCK_SHIFT.., 0);
        self.in_buffer[DTLN_BLOCK_LEN - DTLN_BLOCK_SHIFT..].copy_from_slice(&self.pending);

        for (t, &x) in self.time.iter_mut().zip(&self.in_buffer) {
            *t = f64::from(x);
        }
        self.fft
            .process_with_scratch(&mut self.time, &mut self.spectrum, &mut self.fft_scratch)
            .map_err(|e| SpeechError::InvalidOutput {
                model: "dtln",
                detail: format!("forward FFT: {e}"),
            })?;
        for ((m, p), c) in self
            .magnitude
            .iter_mut()
            .zip(&mut self.phase)
            .zip(&self.spectrum)
        {
            *m = c.norm() as f32;
            *p = c.arg();
        }

        let mag = TensorRef::from_array_view(([1usize, 1, BINS], &self.magnitude[..]))?;
        let st1 = TensorRef::from_array_view(([1usize, 2, 128, 2], &self.state1[..]))?;
        let outputs = self.stage1.run(ort::inputs![mag, st1])?;
        let mask = extract_f32("dtln_model_1", "activation_2", &outputs[0], BINS)?;
        for (((c, &m), &k), &p) in self
            .spectrum
            .iter_mut()
            .zip(&self.magnitude)
            .zip(mask)
            .zip(&self.phase)
        {
            *c = Complex::from_polar(f64::from(m) * f64::from(k), p);
        }
        let next1 = extract_f32(
            "dtln_model_1",
            "tf_op_layer_stack_2",
            &outputs[1],
            STATE_LEN,
        )?;
        self.state1.copy_from_slice(next1);
        drop(outputs);

        // The inverse of a real signal ignores the imaginary parts at DC and Nyquist, as
        // numpy's irfft does.
        self.spectrum[0].im = 0.0;
        self.spectrum[BINS - 1].im = 0.0;
        self.ifft
            .process_with_scratch(&mut self.spectrum, &mut self.time, &mut self.fft_scratch)
            .map_err(|e| SpeechError::InvalidOutput {
                model: "dtln",
                detail: format!("inverse FFT: {e}"),
            })?;
        let scale = 1.0 / DTLN_BLOCK_LEN as f64;
        for (b, &t) in self.block.iter_mut().zip(&self.time) {
            *b = (t * scale) as f32;
        }
        check_finite("dtln", "estimated block", &self.block)?;

        let blk = TensorRef::from_array_view(([1usize, 1, DTLN_BLOCK_LEN], &self.block[..]))?;
        let st2 = TensorRef::from_array_view(([1usize, 2, 128, 2], &self.state2[..]))?;
        let outputs = self.stage2.run(ort::inputs![blk, st2])?;
        let out_block = extract_f32("dtln_model_2", "conv1d_3", &outputs[0], DTLN_BLOCK_LEN)?;
        self.out_buffer.copy_within(DTLN_BLOCK_SHIFT.., 0);
        self.out_buffer[DTLN_BLOCK_LEN - DTLN_BLOCK_SHIFT..].fill(0.0);
        for (o, &b) in self.out_buffer.iter_mut().zip(out_block) {
            *o += b;
        }
        let next2 = extract_f32(
            "dtln_model_2",
            "tf_op_layer_stack_5",
            &outputs[1],
            STATE_LEN,
        )?;
        self.state2.copy_from_slice(next2);
        out.extend_from_slice(&self.out_buffer[..DTLN_BLOCK_SHIFT]);
        Ok(())
    }
}
