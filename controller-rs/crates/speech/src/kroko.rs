//! Kroko streaming ASR through the sherpa-onnx 1.13.8 C API.
//!
//! [`KrokoRecognizer`] owns the native recognizer (the 70 MB encoder plus decoder and joiner);
//! [`KrokoStream`] is one decoding stream. Streams are created from a recognizer and keep it
//! alive: the native recognizer is destroyed only after its last stream.
//!
//! # Sharing one recognizer between threads
//!
//! The recognizer is `Send + Sync`, so one loaded model can serve every speech worker, each
//! with its own streams. That rests on the pinned 1.13.8 source (`sherpa-onnx/c-api/c-api.cc`
//! and `sherpa-onnx/csrc/`), read for the configuration this wrapper builds (transducer,
//! `greedy_search`, CPU provider, no hotwords, no inverse text normalization, no homophone
//! rules):
//!
//! - Every C entry point taking a recognizer forwards to a `const` method of
//!   `OnlineRecognizerTransducerImpl` (`CreateStream`, `IsReady`, `DecodeStreams`, `GetResult`,
//!   `Reset`). Those methods mutate only the `OnlineStream` passed in (its features, encoder
//!   states, decoder result and frame counters), never the recognizer.
//! - `DecodeStreams` reaches the model through `OnlineZipformer2TransducerModel::RunEncoder`,
//!   `RunDecoder` and `RunJoiner`, which build inputs in locals and call `Ort::Session::Run`
//!   on sessions created once in the constructor; the remaining members (names, dims, the
//!   default CPU allocator) are read-only after construction. ONNX Runtime documents
//!   `Session::Run` as safe to call concurrently on one session.
//! - `OnlineTransducerGreedySearchDecoder::Decode` reads its members (`model_`, `unk_id_`,
//!   `blank_penalty_`, `temperature_scale_`) and writes only the per-call result vector.
//! - `GetResult` reads `itn_list_` and `hr_`, which are empty here and never written after
//!   construction.
//!
//! So distinct streams may be decoded concurrently on one recognizer. One stream must never be
//! used from two threads at once: its methods take `&mut self`, and [`KrokoStream`] is `Send`
//! but not `Sync`. The acceptance test `kroko_independent_streams_across_threads_match_sequential`
//! checks the result: streams decoded on several threads at once give byte-identical JSON to the
//! same audio decoded alone.
//!
//! # Memory ownership
//!
//! `SherpaOnnxGetOnlineStreamResultAsJson` returns a string allocated with C++ `new[]` inside
//! the library. It is copied into a Rust `String` and released with
//! `SherpaOnnxDestroyOnlineStreamResultJson` (which calls `delete[]`), never with Rust's
//! allocator or `free`. The recognizer and streams are released with their `Destroy` functions.
//! Version strings are static and not released.
//!
//! # What the wrapper cannot catch
//!
//! sherpa-onnx loads a model it is given without validating it; a wrong file can crash the
//! process (§7.1 of the Rust controller evaluation). Load only files that passed
//! [`crate::qualify`] in a child process. A C++ exception escaping the C API aborts the
//! process.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::NonNull;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sherpa_onnx_sys as sys;

use crate::digest::PinnedFile;
use crate::error::SpeechError;
use crate::native::SherpaRuntime;

/// The sample rate streams accept.
pub const KROKO_SAMPLE_RATE: u32 = 16_000;
/// Fbank feature bins the Kroko model was trained with.
const FEATURE_DIM: i32 = 80;

/// The four Kroko model files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KrokoModelFiles {
    /// The streaming zipformer2 encoder.
    pub encoder: PinnedFile,
    /// The prediction network.
    pub decoder: PinnedFile,
    /// The joiner.
    pub joiner: PinnedFile,
    /// The token table.
    pub tokens: PinnedFile,
}

/// The native recognizer. Destroyed when the last [`KrokoRecognizer`] clone and the last
/// [`KrokoStream`] made from it are dropped.
struct RecognizerHandle {
    ptr: NonNull<sys::OnlineRecognizer>,
}

// SAFETY: the handle is an owned heap object of the C API with no thread affinity; it can be
// destroyed from any thread.
unsafe impl Send for RecognizerHandle {}
// SAFETY: concurrent calls through `&RecognizerHandle` only ever pass distinct streams, and the
// pinned sherpa-onnx source shows those calls do not mutate the recognizer (see module docs).
unsafe impl Sync for RecognizerHandle {}

impl Drop for RecognizerHandle {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from SherpaOnnxCreateOnlineRecognizer and is destroyed once, after
        // every stream (each holds an `Arc` to this handle).
        unsafe { sys::SherpaOnnxDestroyOnlineRecognizer(self.ptr.as_ptr()) };
    }
}

/// A loaded Kroko model. Clones share one native recognizer; see the module docs for why
/// sharing across threads is sound.
#[derive(Clone)]
pub struct KrokoRecognizer {
    handle: Arc<RecognizerHandle>,
}

impl std::fmt::Debug for KrokoRecognizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrokoRecognizer").finish_non_exhaustive()
    }
}

fn c_path(path: &Path) -> Result<CString, SpeechError> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        SpeechError::InvalidInput(format!("path {} contains a NUL byte", path.display()))
    })
}

impl KrokoRecognizer {
    /// Verifies the four files against their pins and creates the recognizer: transducer,
    /// greedy search, 16 kHz, 80 features, one ONNX Runtime thread, CPU, endpointing off.
    ///
    /// `_runtime` proves the linked sherpa-onnx libraries are the pinned build.
    pub fn load(_runtime: &SherpaRuntime, files: &KrokoModelFiles) -> Result<Self, SpeechError> {
        for f in [&files.encoder, &files.decoder, &files.joiner, &files.tokens] {
            f.verify()?;
        }
        let encoder = c_path(&files.encoder.path)?;
        let decoder = c_path(&files.decoder.path)?;
        let joiner = c_path(&files.joiner.path)?;
        let tokens = c_path(&files.tokens.path)?;
        let provider = c"cpu";
        let method = c"greedy_search";
        // SAFETY: the config is plain data the C API documents as "zero-initialize, then fill
        // in"; zero pointers and zero numbers select its defaults.
        let mut config: sys::OnlineRecognizerConfig = unsafe { std::mem::zeroed() };
        config.feat_config.sample_rate = KROKO_SAMPLE_RATE as i32;
        config.feat_config.feature_dim = FEATURE_DIM;
        config.model_config.transducer.encoder = encoder.as_ptr();
        config.model_config.transducer.decoder = decoder.as_ptr();
        config.model_config.transducer.joiner = joiner.as_ptr();
        config.model_config.tokens = tokens.as_ptr();
        config.model_config.num_threads = 1;
        config.model_config.provider = provider.as_ptr();
        config.decoding_method = method.as_ptr();
        config.enable_endpoint = 0;
        // SAFETY: every pointer in `config` refers to a CString alive until this call returns;
        // the C API copies them into its own config.
        let ptr = unsafe { sys::SherpaOnnxCreateOnlineRecognizer(&config) };
        let ptr = NonNull::new(ptr.cast_mut())
            .ok_or(SpeechError::NullHandle("SherpaOnnxCreateOnlineRecognizer"))?;
        Ok(Self {
            handle: Arc::new(RecognizerHandle { ptr }),
        })
    }

    /// A fresh stream: empty features, initial encoder state, empty result.
    pub fn create_stream(&self) -> Result<KrokoStream, SpeechError> {
        // SAFETY: the recognizer is alive (we hold its Arc); creating a stream only reads it.
        let ptr = unsafe { sys::SherpaOnnxCreateOnlineStream(self.handle.ptr.as_ptr()) };
        let ptr = NonNull::new(ptr.cast_mut())
            .ok_or(SpeechError::NullHandle("SherpaOnnxCreateOnlineStream"))?;
        Ok(KrokoStream {
            ptr,
            recognizer: Arc::clone(&self.handle),
        })
    }
}

/// One decoding stream, owned by one worker. All methods take `&mut self`; the type is `Send`
/// (it may move to a worker thread) but not `Sync`.
pub struct KrokoStream {
    ptr: NonNull<sys::OnlineStream>,
    /// Keeps the native recognizer alive until after this stream is destroyed.
    recognizer: Arc<RecognizerHandle>,
}

impl std::fmt::Debug for KrokoStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KrokoStream").finish_non_exhaustive()
    }
}

// SAFETY: a stream is an owned heap object with no thread affinity. Moving it between threads
// is fine; it is not `Sync`, and `&mut self` on every method keeps it to one thread at a time.
unsafe impl Send for KrokoStream {}

impl Drop for KrokoStream {
    fn drop(&mut self) {
        // SAFETY: `ptr` came from SherpaOnnxCreateOnlineStream and is destroyed once, before the
        // `recognizer` field (dropped after this body) can release the recognizer.
        unsafe { sys::SherpaOnnxDestroyOnlineStream(self.ptr.as_ptr()) };
    }
}

/// Frees a result string with the library's own deallocator when dropped.
struct NativeJson(*const std::ffi::c_char);

impl Drop for NativeJson {
    fn drop(&mut self) {
        // SAFETY: the pointer came from SherpaOnnxGetOnlineStreamResultAsJson (C++ `new[]`) and
        // is released exactly once with its matching `delete[]`.
        unsafe { sys::SherpaOnnxDestroyOnlineStreamResultJson(self.0) };
    }
}

impl KrokoStream {
    /// Appends 16 kHz mono samples in [-1, 1]. Non-finite samples are rejected before they reach
    /// the feature extractor.
    pub fn accept(&mut self, samples: &[f32]) -> Result<(), SpeechError> {
        if samples.is_empty() {
            return Ok(());
        }
        let n = check_samples(samples)?;
        // SAFETY: the stream is alive and exclusively borrowed; `samples` is valid for `n`
        // floats and the C API copies them.
        unsafe {
            sys::SherpaOnnxOnlineStreamAcceptWaveform(
                self.ptr.as_ptr(),
                KROKO_SAMPLE_RATE as i32,
                samples.as_ptr(),
                n,
            )
        };
        Ok(())
    }

    /// Runs the encoder while a full chunk of features is ready; returns how many chunks ran.
    pub fn decode_ready(&mut self) -> usize {
        let mut steps = 0;
        // SAFETY: recognizer and stream are alive; this stream is exclusively borrowed, and the
        // recognizer tolerates concurrent calls on other streams (module docs).
        while unsafe {
            sys::SherpaOnnxIsOnlineStreamReady(self.recognizer.ptr.as_ptr(), self.ptr.as_ptr())
        } != 0
        {
            // SAFETY: as above.
            unsafe {
                sys::SherpaOnnxDecodeOnlineStream(self.recognizer.ptr.as_ptr(), self.ptr.as_ptr())
            };
            steps += 1;
        }
        steps
    }

    /// The current result exactly as the library serialises it.
    pub fn result_json(&mut self) -> Result<String, SpeechError> {
        // SAFETY: recognizer and stream are alive and non-null (this function does not check).
        let ptr = unsafe {
            sys::SherpaOnnxGetOnlineStreamResultAsJson(
                self.recognizer.ptr.as_ptr(),
                self.ptr.as_ptr(),
            )
        };
        if ptr.is_null() {
            return Err(SpeechError::NullHandle(
                "SherpaOnnxGetOnlineStreamResultAsJson",
            ));
        }
        let owned = NativeJson(ptr);
        // SAFETY: the library returns a NUL-terminated copy that `owned` frees after we copy it.
        let text = unsafe { std::ffi::CStr::from_ptr(owned.0) };
        text.to_str()
            .map(str::to_owned)
            .map_err(|_| SpeechError::MalformedResult("result is not UTF-8".into()))
    }

    /// The current result, parsed and checked.
    pub fn result(&mut self) -> Result<AsrResult, SpeechError> {
        AsrResult::parse(&self.result_json()?)
    }

    /// The C API's reset: clears the decoded result (keeping the last context tokens as the
    /// decoder's history) and the frame counters, but keeps the encoder state and already
    /// computed features. A new utterance should use a fresh stream from
    /// [`KrokoRecognizer::create_stream`] instead.
    pub fn reset(&mut self) {
        // SAFETY: recognizer and stream are alive; the stream is exclusively borrowed.
        unsafe {
            sys::SherpaOnnxOnlineStreamReset(self.recognizer.ptr.as_ptr(), self.ptr.as_ptr())
        };
    }

    /// Marks the end of input, so the feature extractor emits its last partial frames.
    pub fn input_finished(&mut self) {
        // SAFETY: the stream is alive and exclusively borrowed.
        unsafe { sys::SherpaOnnxOnlineStreamInputFinished(self.ptr.as_ptr()) };
    }
}

/// The sample count as the C API's `int32_t`, after checking every sample is finite.
fn check_samples(samples: &[f32]) -> Result<i32, SpeechError> {
    let n = i32::try_from(samples.len()).map_err(|_| {
        SpeechError::InvalidInput(format!("{} samples in one call is too many", samples.len()))
    })?;
    if let Some(i) = samples.iter().position(|s| !s.is_finite()) {
        return Err(SpeechError::InvalidInput(format!(
            "sample {i} is not finite"
        )));
    }
    Ok(n)
}

/// A streaming result: what has been recognised so far and how long the decoder has emitted
/// only blanks.
#[derive(Debug, Clone, PartialEq)]
pub struct AsrResult {
    /// The text so far.
    pub text: String,
    /// The emitted tokens, in order.
    pub tokens: Vec<String>,
    /// When each token was emitted, in seconds from the start of the stream (emission times,
    /// not word alignments). Same length as `tokens`.
    pub timestamps: Vec<f32>,
    /// Decoder frames (40 ms each) since the last emitted token.
    pub num_trailing_blanks: u32,
}

#[derive(Deserialize)]
struct RawResult {
    text: String,
    tokens: Vec<String>,
    timestamps: Vec<f32>,
    num_trailing_blanks: i64,
}

impl AsrResult {
    /// Parses the library's result JSON. Missing fields, a token/timestamp count mismatch,
    /// non-finite or negative times, and a negative blank count are errors.
    pub fn parse(json: &str) -> Result<Self, SpeechError> {
        let raw: RawResult =
            serde_json::from_str(json).map_err(|e| SpeechError::MalformedResult(e.to_string()))?;
        if raw.tokens.len() != raw.timestamps.len() {
            return Err(SpeechError::MalformedResult(format!(
                "{} tokens but {} timestamps",
                raw.tokens.len(),
                raw.timestamps.len()
            )));
        }
        if let Some(t) = raw.timestamps.iter().find(|t| !t.is_finite() || **t < 0.0) {
            return Err(SpeechError::MalformedResult(format!("token timestamp {t}")));
        }
        let num_trailing_blanks = u32::try_from(raw.num_trailing_blanks).map_err(|_| {
            SpeechError::MalformedResult(format!(
                "num_trailing_blanks {} out of range",
                raw.num_trailing_blanks
            ))
        })?;
        Ok(Self {
            text: raw.text,
            tokens: raw.tokens,
            timestamps: raw.timestamps,
            num_trailing_blanks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"text": " Ask not", "tokens": [" Ask", " not"], "timestamps": [0.32, 0.44], "ys_probs": [-0.77, -0.16], "lm_probs": [], "context_scores": [], "segment": 0, "words": [], "num_trailing_blanks": 32, "start_time": 0.00, "is_final": false, "is_eof": false}"#;

    #[test]
    fn parses_the_library_json() {
        let r = AsrResult::parse(SAMPLE).unwrap();
        assert_eq!(r.text, " Ask not");
        assert_eq!(r.tokens, vec![" Ask", " not"]);
        assert_eq!(r.timestamps, vec![0.32, 0.44]);
        assert_eq!(r.num_trailing_blanks, 32);
    }

    #[test]
    fn parses_an_empty_result() {
        let r = AsrResult::parse(
            r#"{"text": "", "tokens": [], "timestamps": [], "num_trailing_blanks": 0}"#,
        )
        .unwrap();
        assert!(r.text.is_empty() && r.tokens.is_empty());
        assert_eq!(r.num_trailing_blanks, 0);
    }

    #[test]
    fn rejects_malformed_results() {
        for bad in [
            "",
            "{",
            r#"{"text": "a", "tokens": ["a"], "timestamps": []}"#,
            r#"{"text": "a", "tokens": ["a"], "timestamps": [], "num_trailing_blanks": 1}"#,
            r#"{"text": "a", "tokens": ["a"], "timestamps": [-1.0], "num_trailing_blanks": 1}"#,
            r#"{"text": "", "tokens": [], "timestamps": [], "num_trailing_blanks": -3}"#,
            r#"{"text": 3, "tokens": [], "timestamps": [], "num_trailing_blanks": 0}"#,
            r#"{"text": "", "tokens": [], "timestamps": [], "num_trailing_blanks": 1.5}"#,
        ] {
            assert!(
                matches!(AsrResult::parse(bad), Err(SpeechError::MalformedResult(_))),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_non_finite_samples() {
        assert_eq!(check_samples(&[0.0, 0.5, -1.0]).unwrap(), 3);
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            match check_samples(&[0.0, bad]) {
                Err(SpeechError::InvalidInput(msg)) => assert!(msg.contains("sample 1")),
                other => panic!("accepted {bad}: {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_paths_with_nul() {
        assert!(matches!(
            c_path(Path::new("a\0b")),
            Err(SpeechError::InvalidInput(_))
        ));
    }

    #[test]
    fn stream_is_send_and_recognizer_is_shareable() {
        fn send<T: Send>() {}
        fn send_sync<T: Send + Sync>() {}
        send::<KrokoStream>();
        send_sync::<KrokoRecognizer>();
    }
}
