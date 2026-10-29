//! Silero VAD v5 with explicit recurrent state.
//!
//! The pinned `silero_vad_v5.onnx` takes `input` float32 `[1, 576]` (the previous cell's last
//! 64 samples followed by the current 512-sample cell, zeros at stream start), `state` float32
//! `[2, 1, 128]` and `sr` int64 `[]` = 16,000, and returns `output` `[1, 1]` (the speech
//! probability) and `stateN` `[2, 1, 128]` (post-AFE architecture §16.6, "VAD API").
//!
//! The carried state is a plain value, [`SileroState`], so a worker can save it (for example a
//! reply window's voice snapshot), restore it later, or reset it at a subscription's first
//! sample, an epoch change, a discontinuity or mute.

use ort::session::Session;
use ort::value::{TensorElementType, TensorRef};

use crate::digest::PinnedFile;
use crate::error::SpeechError;
use crate::native::OrtRuntime;
use crate::onnx::{Dim, TensorSpec, check_finite, check_io, extract_f32, load_session};

/// Samples in one VAD cell: 32 ms at 16 kHz.
pub const SILERO_CELL_SAMPLES: usize = 512;
/// Samples of the previous cell carried in front of each cell.
pub const SILERO_CONTEXT_SAMPLES: usize = 64;
/// Floats in the recurrent state, `2 × 1 × 128`.
pub const SILERO_STATE_LEN: usize = 256;
/// The sample rate the model is run at.
pub const SILERO_SAMPLE_RATE: i64 = 16_000;

const MODEL: &str = "silero_vad_v5";
const INPUT_LEN: usize = SILERO_CONTEXT_SAMPLES + SILERO_CELL_SAMPLES;

/// Everything Silero carries from one cell to the next.
#[derive(Clone, Copy, PartialEq)]
pub struct SileroState {
    recurrent: [f32; SILERO_STATE_LEN],
    context: [f32; SILERO_CONTEXT_SAMPLES],
}

impl SileroState {
    /// The state at the start of a stream: all zeros.
    pub const INITIAL: Self = Self {
        recurrent: [0.0; SILERO_STATE_LEN],
        context: [0.0; SILERO_CONTEXT_SAMPLES],
    };

    /// The recurrent state, `[2, 1, 128]` in row-major order.
    pub fn recurrent(&self) -> &[f32; SILERO_STATE_LEN] {
        &self.recurrent
    }

    /// The last 64 samples of the previous cell.
    pub fn context(&self) -> &[f32; SILERO_CONTEXT_SAMPLES] {
        &self.context
    }

    /// Rebuilds a state from its parts, for example one persisted with a turn. Non-finite
    /// values are rejected.
    pub fn from_parts(
        recurrent: [f32; SILERO_STATE_LEN],
        context: [f32; SILERO_CONTEXT_SAMPLES],
    ) -> Result<Self, SpeechError> {
        if recurrent.iter().chain(&context).any(|v| !v.is_finite()) {
            return Err(SpeechError::InvalidInput(
                "Silero state is not finite".into(),
            ));
        }
        Ok(Self { recurrent, context })
    }
}

impl Default for SileroState {
    fn default() -> Self {
        Self::INITIAL
    }
}

impl std::fmt::Debug for SileroState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let energy: f32 = self.recurrent.iter().map(|v| v * v).sum();
        f.debug_struct("SileroState")
            .field("recurrent_energy", &energy)
            .finish_non_exhaustive()
    }
}

/// One Silero session and its state, owned by one worker.
pub struct SileroVad {
    session: Session,
    state: SileroState,
    input: Vec<f32>,
}

impl std::fmt::Debug for SileroVad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SileroVad")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl SileroVad {
    /// Verifies and loads the model and checks its inputs and outputs.
    pub fn load(runtime: &OrtRuntime, model: &PinnedFile) -> Result<Self, SpeechError> {
        let session = load_session(runtime, model)?;
        check_io(
            MODEL,
            &session,
            &[
                TensorSpec {
                    name: "input",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Any, Dim::Any],
                },
                TensorSpec {
                    name: "state",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Exactly(2), Dim::Any, Dim::Exactly(128)],
                },
                TensorSpec {
                    name: "sr",
                    ty: TensorElementType::Int64,
                    dims: &[],
                },
            ],
            &[
                TensorSpec {
                    name: "output",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Any, Dim::Exactly(1)],
                },
                TensorSpec {
                    name: "stateN",
                    ty: TensorElementType::Float32,
                    dims: &[Dim::Any, Dim::Any, Dim::Any],
                },
            ],
        )?;
        Ok(Self {
            session,
            state: SileroState::INITIAL,
            input: vec![0.0; INPUT_LEN],
        })
    }

    /// Runs one 512-sample cell of the VAD input (floats in [-1, 1]) and returns its speech
    /// probability, advancing the state. On error the state is unchanged.
    pub fn process_cell(&mut self, cell: &[f32; SILERO_CELL_SAMPLES]) -> Result<f32, SpeechError> {
        check_finite(MODEL, "cell", cell)
            .map_err(|_| SpeechError::InvalidInput("VAD cell is not finite".into()))?;
        self.input[..SILERO_CONTEXT_SAMPLES].copy_from_slice(&self.state.context);
        self.input[SILERO_CONTEXT_SAMPLES..].copy_from_slice(cell);
        let sr = [SILERO_SAMPLE_RATE];
        let input = TensorRef::from_array_view(([1usize, INPUT_LEN], &self.input[..]))?;
        let state = TensorRef::from_array_view(([2usize, 1, 128], &self.state.recurrent[..]))?;
        let sr = TensorRef::from_array_view(([0usize; 0], &sr[..]))?;
        let outputs = self.session.run(ort::inputs![input, state, sr])?;
        let probability = extract_f32(MODEL, "output", &outputs[0], 1)?[0];
        if !(0.0..=1.0).contains(&probability) {
            return Err(SpeechError::InvalidOutput {
                model: MODEL,
                detail: format!("probability {probability} outside [0, 1]"),
            });
        }
        let next = extract_f32(MODEL, "stateN", &outputs[1], SILERO_STATE_LEN)?;
        self.state.recurrent.copy_from_slice(next);
        self.state
            .context
            .copy_from_slice(&cell[SILERO_CELL_SAMPLES - SILERO_CONTEXT_SAMPLES..]);
        Ok(probability)
    }

    /// A copy of the current state.
    pub fn state(&self) -> SileroState {
        self.state
    }

    /// Replaces the current state, for example with a saved voice snapshot.
    pub fn restore(&mut self, state: &SileroState) {
        self.state = *state;
    }

    /// Returns to the stream-start state.
    pub fn reset(&mut self) {
        self.state = SileroState::INITIAL;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_parts_round_trip_and_reject_non_finite() {
        let mut r = [0.0f32; SILERO_STATE_LEN];
        r[3] = 0.25;
        let c = [0.5f32; SILERO_CONTEXT_SAMPLES];
        let s = SileroState::from_parts(r, c).unwrap();
        assert_eq!(s.recurrent()[3], 0.25);
        assert_eq!(s.context()[0], 0.5);
        assert_ne!(s, SileroState::INITIAL);
        r[0] = f32::NAN;
        assert!(SileroState::from_parts(r, c).is_err());
        assert_eq!(SileroState::default(), SileroState::INITIAL);
    }
}
