//! Where the controller's speech assets live, with every file pinned.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::digest::PinnedFile;
use crate::dtln::DtlnModelFiles;
use crate::kroko::KrokoModelFiles;
use crate::manifest::SpeechBundleManifest;
use crate::native::NativeLibraries;
use crate::pins::{DTLN_MODEL_1, DTLN_MODEL_2, KROKO_TEST_TRANSCRIPT, KROKO_TEST_WAV_SHA256};
use crate::qualify::{ModelUnderTest, QualificationRequest};

/// The native libraries and the shared speech models, each with the digest it must have. Wake
/// models are per device and come from the wake-model registry as
/// [`crate::BcresnetModelFiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpeechAssets {
    /// sherpa-onnx and ONNX Runtime.
    pub native: NativeLibraries,
    /// Kroko streaming ASR, from the speech bundle.
    pub kroko: KrokoModelFiles,
    /// The Kroko archive's test WAV, used by qualification.
    pub kroko_test_wav: PinnedFile,
    /// Silero VAD v5, from the speech bundle.
    pub vad: PinnedFile,
    /// The DTLN noise-suppression models.
    pub dtln: DtlnModelFiles,
}

impl SpeechAssets {
    /// The assets of a speech bundle directory laid out by its manifest, the extracted Kroko
    /// test WAV at `kroko_test_wav`, and DTLN's `model_1.onnx` and `model_2.onnx` in `dtln_dir`.
    pub fn new(
        native: NativeLibraries,
        manifest: &SpeechBundleManifest,
        bundle_dir: &Path,
        kroko_test_wav: &Path,
        dtln_dir: &Path,
    ) -> Self {
        Self {
            native,
            kroko: manifest.kroko_files(bundle_dir),
            kroko_test_wav: PinnedFile::new(kroko_test_wav, KROKO_TEST_WAV_SHA256),
            vad: manifest.vad_file(bundle_dir),
            dtln: DtlnModelFiles {
                model_1: PinnedFile::new(dtln_dir.join("model_1.onnx"), DTLN_MODEL_1.sha256),
                model_2: PinnedFile::new(dtln_dir.join("model_2.onnx"), DTLN_MODEL_2.sha256),
            },
        }
    }

    /// One qualification request per shared model, for startup qualification.
    pub fn qualification_requests(&self) -> [QualificationRequest; 3] {
        let request = |model| QualificationRequest {
            native: self.native.clone(),
            model,
        };
        [
            request(ModelUnderTest::Kroko {
                files: self.kroko.clone(),
                test_wav: self.kroko_test_wav.clone(),
                expected_transcript: Some(KROKO_TEST_TRANSCRIPT.to_owned()),
            }),
            request(ModelUnderTest::SileroVad {
                model: self.vad.clone(),
            }),
            request(ModelUnderTest::Dtln {
                files: self.dtln.clone(),
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shared_model_is_qualified() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let manifest =
            SpeechBundleManifest::load(&root.join("controller/speech_bundle.json")).unwrap();
        let native = NativeLibraries::pinned("/opt/ort/libonnxruntime.so.1.20.1").unwrap();
        let assets = SpeechAssets::new(
            native,
            &manifest,
            Path::new("/speech"),
            Path::new("/speech/0.wav"),
            Path::new("/dtln"),
        );
        let requests = assets.qualification_requests();
        assert!(requests.iter().all(|r| r.native == assets.native));
        assert!(matches!(
            &requests[0].model,
            ModelUnderTest::Kroko { files, test_wav, .. }
                if *files == assets.kroko && *test_wav == assets.kroko_test_wav
        ));
        assert!(matches!(
            &requests[1].model,
            ModelUnderTest::SileroVad { model } if *model == assets.vad
        ));
        assert!(matches!(
            &requests[2].model,
            ModelUnderTest::Dtln { files } if *files == assets.dtln
        ));
    }
}
