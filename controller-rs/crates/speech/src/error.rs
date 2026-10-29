//! The one error type every model operation returns.

use std::path::PathBuf;

use crate::digest::Sha256Digest;

/// What went wrong loading or running a speech model. Native null returns, malformed native
/// output and invalid caller input are all values of this type, never panics.
#[derive(Debug, thiserror::Error)]
pub enum SpeechError {
    /// A file could not be read.
    #[error("reading {}: {source}", path.display())]
    Io {
        /// The file.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// A file's bytes are not the pinned ones.
    #[error("{} has SHA-256 {actual}, expected {expected}", path.display())]
    HashMismatch {
        /// The file.
        path: PathBuf,
        /// The pinned digest.
        expected: Sha256Digest,
        /// The digest of the bytes found.
        actual: Sha256Digest,
    },
    /// A native library is missing, is not the pinned build, or was loaded from somewhere else.
    #[error("native library: {0}")]
    NativeLibrary(String),
    /// ONNX Runtime failed to load a model or run it.
    #[error("ONNX Runtime: {0}")]
    Onnx(#[from] ort::Error),
    /// A native constructor returned a null handle.
    #[error("{0} returned a null handle")]
    NullHandle(&'static str),
    /// A model's inputs or outputs are not the ones this crate was written for.
    #[error("model {model}: {detail}")]
    ModelContract {
        /// Which model.
        model: &'static str,
        /// What does not match.
        detail: String,
    },
    /// The wake model's sidecar JSON is missing a field or breaks a load rule.
    #[error("wake sidecar: {0}")]
    Sidecar(String),
    /// The caller passed audio or arguments the model cannot take.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// A model produced NaN or infinity, or a value outside its documented range.
    #[error("model {model} produced an invalid output: {detail}")]
    InvalidOutput {
        /// Which model.
        model: &'static str,
        /// What was wrong.
        detail: String,
    },
    /// The recognizer's result JSON could not be parsed or is inconsistent.
    #[error("recognizer result JSON: {0}")]
    MalformedResult(String),
    /// A WAV file could not be decoded or has the wrong format.
    #[error("WAV {}: {detail}", path.display())]
    Wav {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        detail: String,
    },
    /// A manifest or request is not in the expected shape.
    #[error("manifest: {0}")]
    Manifest(String),
}
