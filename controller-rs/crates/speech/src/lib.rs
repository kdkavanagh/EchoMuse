//! Speech models run by the speech workers: streaming ASR, wake scoring, voice activity
//! detection and noise suppression, behind one Rust interface.
//!
//! | Model | Engine | Type | Owner |
//! |---|---|---|---|
//! | Kroko streaming ASR | sherpa-onnx 1.13.8 C API (`sherpa-onnx-sys`, shared build) | [`KrokoRecognizer`] shared, [`KrokoStream`] per worker | microphone analysis, wake verification, span decoding |
//! | BCResNet wake graph | `ort` on ONNX Runtime 1.20.1 | [`BcresnetScorer`] | reference scoring |
//! | Silero VAD v5 | `ort` | [`SileroVad`] with [`SileroState`] | microphone analysis |
//! | DTLN | `ort` | [`DtlnDenoiser`] | span decoding (STT copy) |
//!
//! Every stateful call takes `&mut self`, so a model session or stream belongs to one worker.
//! Only the Kroko recognizer is shared, on the evidence documented in [`kroko`].
//!
//! Loading starts from verified native libraries: [`SherpaRuntime::verify`] checks the linked
//! sherpa-onnx libraries, [`OrtRuntime::load`] loads the pinned ONNX Runtime for `ort`. Every
//! model file is a [`PinnedFile`] checked against its SHA-256 at load. Models should be
//! [qualified](qualify) in a child process before the main process loads them.
//!
//! # Native libraries at build and run time
//!
//! Build with `SHERPA_ONNX_LIB_DIR` pointing at `sherpa_onnx/lib/` of the `sherpa-onnx-core`
//! 1.13.8 wheel for the target architecture, verified against [`pins::SherpaPins`]. Without it,
//! `sherpa-onnx-sys` downloads an unverified GitHub archive whose build gives different
//! numbers (see [`pins`]), and [`SherpaRuntime::verify`] rejects it at run time.
//!
//! The binary then needs `libsherpa-onnx-c-api.so` and its `libonnxruntime.so` at run time.
//! `sherpa-onnx-sys` asks for a run path, but Cargo applies a dependency's linker arguments
//! only to that dependency, so the final executable has none: the application must either
//! link with `-Wl,-rpath,$ORIGIN` (its own build script) and ship the two libraries beside the
//! executable, or set `LD_LIBRARY_PATH`. `cargo test` finds them because `sherpa-onnx-sys`
//! copies them into the target directory. `ort` needs the official ONNX Runtime 1.20.1 library
//! at the path given to [`OrtRuntime::load`].

pub mod assets;
pub mod audio;
pub mod bcresnet;
pub mod digest;
pub mod dtln;
mod error;
pub mod kroko;
pub mod manifest;
pub mod native;
mod onnx;
pub mod pins;
pub mod qualify;
pub mod silero;

pub use assets::SpeechAssets;
pub use audio::{LinearResampler, MonoWav};
pub use bcresnet::{BcresnetModelFiles, BcresnetScorer, WakeSidecar, WindowScore};
pub use digest::{PinnedFile, Sha256Digest};
pub use dtln::{DtlnDenoiser, DtlnModelFiles};
pub use error::SpeechError;
pub use kroko::{AsrResult, KrokoModelFiles, KrokoRecognizer, KrokoStream};
pub use manifest::SpeechBundleManifest;
pub use native::{NativeLibraries, OrtRuntime, SherpaLibraries, SherpaRuntime};
pub use silero::{SileroState, SileroVad};
