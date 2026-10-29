//! Loading the speech bundle at startup (spec §5.5, §5.7).
//!
//! Speech readiness is separate from the controller's: the controller serves the
//! dashboard, alerts and firmware while the bundle loads, and keeps running if it cannot be
//! loaded. Only wake and button turns refuse while speech is not ready.
//!
//! Loading runs off the async runtime. Each of the bundle's three shared models (Kroko, Silero
//! VAD, DTLN) is first qualified in a short-lived child process of this same binary
//! (`qualify-model`, 120 s each, as long as Kroko's 70 MB encoder may take to load and
//! decode), so a model that crashes or hangs its native runtime takes down only the child.
//! The main process then loads [`BundleModels`] from the same pinned files and publishes
//! them. Any failure is published as [`SpeechStatus::Unavailable`] with its reason and
//! logged.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use echomuse_config::PathsConfig;
use echomuse_speech::qualify::{QUALIFY_MODEL_SUBCOMMAND, QualifierCommand, qualify_in_child};
use echomuse_speech::{NativeLibraries, SpeechAssets, SpeechBundleManifest};
use echomuse_voice::speech::models::{BundleModels, ModelSet};
use tokio::sync::watch;

use crate::supervision::TaskSupervisor;

/// How long each qualification child may take.
const QUALIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// Whether the speech models are usable.
#[derive(Clone)]
pub enum SpeechStatus {
    /// Qualification and loading are under way.
    Loading,
    /// The models every device's speech workers use.
    Ready(Arc<dyn ModelSet>),
    /// The bundle cannot be used, and why.
    Unavailable(String),
}

impl std::fmt::Debug for SpeechStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loading => f.write_str("Loading"),
            Self::Ready(_) => f.write_str("Ready"),
            Self::Unavailable(reason) => f.debug_tuple("Unavailable").field(reason).finish(),
        }
    }
}

/// Where the speech bundle's files are.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpeechPaths {
    /// The speech bundle directory, with its `speech_bundle.json`.
    pub bundle_dir: PathBuf,
    /// The controller's own ONNX Runtime 1.20.1 library.
    pub onnxruntime_lib: PathBuf,
    /// The directory holding DTLN's `model_1.onnx` and `model_2.onnx`.
    pub ns_model_dir: PathBuf,
}

impl SpeechPaths {
    pub fn from_config(paths: &PathsConfig) -> Self {
        Self {
            bundle_dir: paths.speech_bundle_dir.clone(),
            onnxruntime_lib: paths.onnxruntime_lib.clone(),
            ns_model_dir: paths.ns_model_dir.clone(),
        }
    }
}

/// Starts qualifying and loading the bundle on a blocking thread; the receiver shows
/// [`SpeechStatus::Loading`] until it settles. `qualifier_program` is the binary that routes
/// the `qualify-model` subcommand (this executable in production). The task that publishes
/// the outcome is spawned through `tasks`, so a panic in it stops the process (§5.5); a
/// panic while loading is reported as unavailable speech. Must be called inside a Tokio
/// runtime.
pub fn start_loading(
    paths: SpeechPaths,
    qualifier_program: PathBuf,
    tasks: &TaskSupervisor,
) -> watch::Receiver<SpeechStatus> {
    let (tx, rx) = watch::channel(SpeechStatus::Loading);
    let task = tokio::task::spawn_blocking(move || load(&paths, &qualifier_program));
    drop(tasks.spawn("speech model loading", async move {
        let status = match task.await {
            Ok(Ok(models)) => {
                tracing::info!("speech models are ready");
                SpeechStatus::Ready(models)
            }
            Ok(Err(reason)) => {
                tracing::error!(%reason, "speech is unavailable; wake and button turns are refused");
                SpeechStatus::Unavailable(reason)
            }
            Err(error) => {
                let reason = format!("speech loading did not finish: {error}");
                tracing::error!(%reason, "speech is unavailable; wake and button turns are refused");
                SpeechStatus::Unavailable(reason)
            }
        };
        // Nobody listening is not an error: the controller may be shutting down.
        let _ = tx.send(status);
    }));
    rx
}

/// A receiver that already holds `models`, for tests and tools that load them otherwise.
pub fn provided(models: Arc<dyn ModelSet>) -> watch::Receiver<SpeechStatus> {
    watch::channel(SpeechStatus::Ready(models)).1
}

/// The Kroko test WAV inside the bundle: the archive member `test_wav_member` names, under the
/// directory the bundle keeps the Kroko files in (`kroko/test_wavs/0.wav`).
fn kroko_test_wav(manifest: &SpeechBundleManifest, bundle_dir: &Path) -> Result<PathBuf, String> {
    let member = Path::new(&manifest.asr.test_wav_member);
    let relative = member.strip_prefix(&manifest.asr.name).map_err(|_| {
        format!(
            "the bundle's test WAV member {} is not inside the archive {}",
            member.display(),
            manifest.asr.name
        )
    })?;
    let kroko_dir = Path::new(&manifest.asr.files.encoder.path)
        .parent()
        .unwrap_or_else(|| Path::new(""));
    Ok(bundle_dir.join(kroko_dir).join(relative))
}

fn load(paths: &SpeechPaths, program: &Path) -> Result<Arc<dyn ModelSet>, String> {
    let manifest_path = paths.bundle_dir.join("speech_bundle.json");
    let manifest = SpeechBundleManifest::load(&manifest_path)
        .map_err(|e| format!("speech bundle manifest {}: {e}", manifest_path.display()))?;
    let native = NativeLibraries::pinned(paths.onnxruntime_lib.clone(), None)
        .map_err(|e| format!("ONNX Runtime {}: {e}", paths.onnxruntime_lib.display()))?;
    let test_wav = kroko_test_wav(&manifest, &paths.bundle_dir)?;
    let assets = SpeechAssets::new(
        native,
        &manifest,
        &paths.bundle_dir,
        &test_wav,
        &paths.ns_model_dir,
    );
    let command = QualifierCommand::new(program, [QUALIFY_MODEL_SUBCOMMAND.into()]);
    for request in assets.qualification_requests() {
        let kind = model_name(&request.model);
        qualify_in_child(&command, &request, QUALIFY_TIMEOUT)
            .map_err(|rejection| format!("{kind} failed qualification: {rejection}"))?;
        tracing::info!(model = kind, "speech model qualified");
    }
    let models =
        BundleModels::load(&assets).map_err(|e| format!("the speech models did not load: {e}"))?;
    Ok(Arc::new(models))
}

fn model_name(model: &echomuse_speech::qualify::ModelUnderTest) -> &'static str {
    use echomuse_speech::qualify::ModelUnderTest;
    match model {
        ModelUnderTest::Kroko { .. } => "Kroko ASR",
        ModelUnderTest::SileroVad { .. } => "Silero VAD",
        ModelUnderTest::Dtln { .. } => "DTLN",
        ModelUnderTest::Bcresnet { .. } => "the wake graph",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest laid out like the pinned bundle's (the digests are placeholders).
    fn manifest() -> SpeechBundleManifest {
        let file = |path: &str| {
            format!(
                r#"{{"path": "{path}", "size": 1, "sha256": "{}"}}"#,
                "0".repeat(64)
            )
        };
        let json = format!(
            r#"{{
                "version": 1,
                "runtime": {{"package": "sherpa-onnx", "version": "1.13.8", "wheel": {wheel}}},
                "asr": {{
                    "name": "sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06",
                    "archive": {archive},
                    "files": {{"encoder": {enc}, "decoder": {dec}, "joiner": {join}, "tokens": {tok}}},
                    "test_wav_member": "sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06/test_wavs/0.wav"
                }},
                "vad": {vad},
                "attribution": {{"path": "KROKO-ATTRIBUTION.txt", "license": "CC-BY-SA", "url": "u"}}
            }}"#,
            wheel = file("sherpa.whl"),
            archive = file("kroko.tar.bz2"),
            enc = file("kroko/encoder.onnx"),
            dec = file("kroko/decoder.onnx"),
            join = file("kroko/joiner.onnx"),
            tok = file("kroko/tokens.txt"),
            vad = file("silero_vad_v5.onnx"),
        );
        SpeechBundleManifest::parse(&json).unwrap()
    }

    #[test]
    fn the_test_wav_is_found_beside_the_kroko_files() {
        assert_eq!(
            kroko_test_wav(&manifest(), Path::new("/app/speech")).unwrap(),
            PathBuf::from("/app/speech/kroko/test_wavs/0.wav")
        );
    }

    #[tokio::test]
    async fn a_missing_bundle_is_unavailable_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut rx = start_loading(
            SpeechPaths {
                bundle_dir: dir.path().join("speech"),
                onnxruntime_lib: dir.path().join("libonnxruntime.so.1.20.1"),
                ns_model_dir: dir.path().join("dtln"),
            },
            PathBuf::from("/bin/false"),
            &TaskSupervisor::new(crate::supervision::FatalHook::new(|reason| {
                panic!("the loading task failed: {reason}")
            })),
        );
        let status = rx
            .wait_for(|s| !matches!(s, SpeechStatus::Loading))
            .await
            .unwrap()
            .clone();
        let SpeechStatus::Unavailable(reason) = status else {
            panic!("expected unavailable, got {status:?}");
        };
        assert!(reason.contains("speech_bundle.json"), "{reason}");
    }
}
