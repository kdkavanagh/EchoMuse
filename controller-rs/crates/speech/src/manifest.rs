//! The speech-bundle manifest, `controller/speech_bundle.json`.
//!
//! The manifest is an external contract: it names every file of the controller's speech bundle
//! (the Kroko streaming ASR model and Silero VAD v5) with its size and SHA-256, relative to the
//! bundle directory. This module reads it and turns it into [`PinnedFile`]s.

use std::path::Path;

use serde::Deserialize;

use crate::digest::{PinnedFile, Sha256Digest};
use crate::error::SpeechError;
use crate::kroko::KrokoModelFiles;
use crate::pins::SHERPA_ONNX_VERSION;

/// One file the manifest pins.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestFile {
    /// Path relative to the bundle directory (or, for downloads, the file name).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Digest of the bytes.
    pub sha256: Sha256Digest,
    /// Where it is downloaded from, when it is downloaded directly.
    #[serde(default)]
    pub url: Option<String>,
}

/// The runtime section: the sherpa-onnx Python wheel the bundle was built against.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestRuntime {
    /// Package name, `sherpa-onnx`.
    pub package: String,
    /// Package version; must equal the C API version this crate links.
    pub version: String,
    /// The pinned wheel.
    pub wheel: ManifestFile,
}

/// The four Kroko model files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestAsrFiles {
    /// The streaming encoder.
    pub encoder: ManifestFile,
    /// The prediction network.
    pub decoder: ManifestFile,
    /// The joiner.
    pub joiner: ManifestFile,
    /// The token table.
    pub tokens: ManifestFile,
}

/// The ASR section.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestAsr {
    /// Model name.
    pub name: String,
    /// The release archive the files come from.
    pub archive: ManifestFile,
    /// The extracted files.
    pub files: ManifestAsrFiles,
    /// Path of the test WAV inside the archive.
    pub test_wav_member: String,
}

/// The attribution section (Kroko is CC-BY-SA).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestAttribution {
    /// Attribution file name in the bundle.
    pub path: String,
    /// Licence name.
    pub license: String,
    /// Link to the model's licence page.
    pub url: String,
}

/// `speech_bundle.json`, version 1.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SpeechBundleManifest {
    /// Manifest format version; only 1 exists.
    pub version: u32,
    /// The sherpa-onnx runtime the bundle was built for.
    pub runtime: ManifestRuntime,
    /// Streaming ASR.
    pub asr: ManifestAsr,
    /// Silero VAD v5.
    pub vad: ManifestFile,
    /// Licence attribution.
    pub attribution: ManifestAttribution,
}

impl SpeechBundleManifest {
    /// Parses and checks a manifest: format version 1, and a runtime version equal to the
    /// sherpa-onnx C API this crate links.
    pub fn parse(json: &str) -> Result<Self, SpeechError> {
        let manifest: Self =
            serde_json::from_str(json).map_err(|e| SpeechError::Manifest(e.to_string()))?;
        if manifest.version != 1 {
            return Err(SpeechError::Manifest(format!(
                "manifest version {}, only 1 is understood",
                manifest.version
            )));
        }
        if manifest.runtime.package != "sherpa-onnx"
            || manifest.runtime.version != SHERPA_ONNX_VERSION
        {
            return Err(SpeechError::Manifest(format!(
                "bundle runtime is {} {}, this build links sherpa-onnx {SHERPA_ONNX_VERSION}",
                manifest.runtime.package, manifest.runtime.version
            )));
        }
        Ok(manifest)
    }

    /// Reads and parses the manifest file at `path`.
    pub fn load(path: &Path) -> Result<Self, SpeechError> {
        let json = std::fs::read_to_string(path).map_err(|source| SpeechError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&json)
    }

    /// The Kroko files inside `bundle_dir`, pinned by the manifest.
    pub fn kroko_files(&self, bundle_dir: &Path) -> KrokoModelFiles {
        let pin = |f: &ManifestFile| PinnedFile::new(bundle_dir.join(&f.path), f.sha256);
        KrokoModelFiles {
            encoder: pin(&self.asr.files.encoder),
            decoder: pin(&self.asr.files.decoder),
            joiner: pin(&self.asr.files.joiner),
            tokens: pin(&self.asr.files.tokens),
        }
    }

    /// The Silero VAD file inside `bundle_dir`, pinned by the manifest.
    pub fn vad_file(&self, bundle_dir: &Path) -> PinnedFile {
        PinnedFile::new(bundle_dir.join(&self.vad.path), self.vad.sha256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_manifest() -> String {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../controller/speech_bundle.json");
        std::fs::read_to_string(path)
            .expect("controller/speech_bundle.json is part of the repository")
    }

    #[test]
    fn parses_the_repository_manifest() {
        let m = SpeechBundleManifest::parse(&repo_manifest()).unwrap();
        let bundle = Path::new("/b");
        let files = m.kroko_files(bundle);
        for (pinned, listed) in [
            (&files.encoder, &m.asr.files.encoder),
            (&files.decoder, &m.asr.files.decoder),
            (&files.joiner, &m.asr.files.joiner),
            (&files.tokens, &m.asr.files.tokens),
        ] {
            assert_eq!(pinned.path, bundle.join(&listed.path));
            assert_eq!(pinned.sha256, listed.sha256);
        }
        let vad = m.vad_file(bundle);
        assert_eq!(vad.path, bundle.join(&m.vad.path));
        assert_eq!(vad.sha256, m.vad.sha256);
    }

    #[test]
    fn rejects_other_versions_and_runtimes() {
        let good = repo_manifest();
        let v2 = good.replacen("\"version\": 1", "\"version\": 2", 1);
        assert!(matches!(
            SpeechBundleManifest::parse(&v2),
            Err(SpeechError::Manifest(_))
        ));
        let other_runtime = good.replace("\"1.13.8\"", "\"1.12.0\"");
        assert!(matches!(
            SpeechBundleManifest::parse(&other_runtime),
            Err(SpeechError::Manifest(_))
        ));
        let bad_digest = good.replace(
            "6b99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396",
            "not-a-digest",
        );
        assert!(matches!(
            SpeechBundleManifest::parse(&bad_digest),
            Err(SpeechError::Manifest(_))
        ));
        assert!(SpeechBundleManifest::parse("{").is_err());
    }
}
