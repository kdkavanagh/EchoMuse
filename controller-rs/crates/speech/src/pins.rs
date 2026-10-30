//! Hashes of the exact native libraries and model files this crate was qualified with.
//!
//! The speech bundle's own files (Kroko and Silero) are pinned by `controller/speech_bundle.json`
//! and read through [`crate::manifest`]. This module pins what that manifest does not:
//!
//! - the sherpa-onnx native libraries `sherpa-onnx-sys` links against: the `lib/` directory of
//!   the `sherpa-onnx-core` 1.13.8 wheel, the same files the pinned Python package loads, for
//!   `linux/amd64` and `linux/arm64`;
//! - the official ONNX Runtime 1.20.1 release `ort` loads at run time, for both architectures
//!   (the version the BCResNet fixture was generated with);
//! - the two DTLN models at their pinned upstream commit;
//! - the test WAV inside the hash-pinned Kroko archive, and what Kroko transcribes from it.
//!
//! Every digest here was computed from the downloaded file named next to it. The arm64 values
//! are hashes of the arm64 files; nothing in this crate has been run on arm64.
//!
//! sherpa-onnx also publishes shared-library archives on GitHub
//! (`sherpa-onnx-v1.13.8-linux-x64-shared-lib.tar.bz2`, which `sherpa-onnx-sys` downloads by
//! default). That build is not the one pinned: on the Kroko test WAV it emits the same tokens
//! and timestamps as the Python package but different per-token log probabilities, while the
//! core wheel's `libsherpa-onnx-c-api.so` gives byte-identical result JSON at every step.

use crate::digest::Sha256Digest;

/// A CPU architecture the controller image is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arch {
    /// `linux/amd64`.
    X86_64,
    /// `linux/arm64`.
    Aarch64,
}

impl Arch {
    /// The architecture this crate was compiled for, if it is one of the two supported ones.
    pub const fn current() -> Option<Self> {
        if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else {
            None
        }
    }
}

/// The sherpa-onnx release whose C API the ASR wrapper is written against.
pub const SHERPA_ONNX_VERSION: &str = "1.13.8";

/// A downloadable archive and its digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchivePin {
    /// The published file name.
    pub file_name: &'static str,
    /// Where the archive is published.
    pub url: &'static str,
    /// The digest of the archive.
    pub sha256: Sha256Digest,
}

/// The sherpa-onnx native libraries for one architecture: `sherpa_onnx/lib/` inside the
/// `sherpa-onnx-core` wheel. Build with `SHERPA_ONNX_LIB_DIR` set to that directory of a wheel
/// verified against `wheel`, so `sherpa-onnx-sys` links against it instead of downloading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SherpaPins {
    /// The `sherpa-onnx-core` wheel (a zip archive).
    pub wheel: ArchivePin,
    /// `sherpa_onnx/lib/libsherpa-onnx-c-api.so` inside the wheel.
    pub c_api_sha256: Sha256Digest,
    /// `sherpa_onnx/lib/libonnxruntime.so` inside the wheel: sherpa-onnx's own ONNX Runtime.
    pub onnxruntime_sha256: Sha256Digest,
}

/// The ONNX Runtime library `ort` loads for BCResNet, Silero and DTLN on one architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrtPins {
    /// The official release archive.
    pub archive: ArchivePin,
    /// Path of the library inside the archive.
    pub library_member: &'static str,
    /// The digest of that library file.
    pub library_sha256: Sha256Digest,
}

/// The ONNX Runtime version `ort` loads. `ort` is built with its `api-20` feature, so it needs
/// at least 1.20.
pub const ORT_VERSION: &str = "1.20.1";

/// sherpa-onnx pins for `linux/amd64`.
pub const SHERPA_X86_64: SherpaPins = SherpaPins {
    wheel: ArchivePin {
        file_name: "sherpa_onnx_core-1.13.8-py3-none-manylinux2014_x86_64.whl",
        url: "https://files.pythonhosted.org/packages/b6/ca/e27c1fb5c54b181d4a5450f3d5c789d9da02eb42dc9b20cfdab5dda4c864/sherpa_onnx_core-1.13.8-py3-none-manylinux2014_x86_64.whl",
        sha256: Sha256Digest::from_hex_const(
            "4da90acf435373d7b2ba9cc0be806e7e274d28f63f5780880b5dea6721626e36",
        ),
    },
    c_api_sha256: Sha256Digest::from_hex_const(
        "e50ae8bca4db52b7be04f56f8ac1502b940ca124a26fac0d48d8cef7fc9823d4",
    ),
    onnxruntime_sha256: Sha256Digest::from_hex_const(
        "4b3607aebd1784b26b6f9b20e4bd974c7ab8287043e4d095cb7d2cb40b5e566e",
    ),
};

/// sherpa-onnx pins for `linux/arm64`.
pub const SHERPA_AARCH64: SherpaPins = SherpaPins {
    wheel: ArchivePin {
        file_name: "sherpa_onnx_core-1.13.8-py3-none-manylinux2014_aarch64.whl",
        url: "https://files.pythonhosted.org/packages/9c/72/02227b14cd3fb3d504a8814ad144ed321a53331948c893821e02f4abb1d7/sherpa_onnx_core-1.13.8-py3-none-manylinux2014_aarch64.whl",
        sha256: Sha256Digest::from_hex_const(
            "a51a03d55c376c32e15dd63ea852236042ba91a5cd73e253bbf7cf21f6582ba9",
        ),
    },
    c_api_sha256: Sha256Digest::from_hex_const(
        "1be01a1a5c0420e32a81011222e771fc35fa5e8d8cce951281d769d849296cab",
    ),
    onnxruntime_sha256: Sha256Digest::from_hex_const(
        "cfc800d4438f15e8d9017521326776ca7117d8783cb847ccccc2fc61ac4d540f",
    ),
};

/// ONNX Runtime pins for `linux/amd64`.
pub const ORT_X86_64: OrtPins = OrtPins {
    archive: ArchivePin {
        file_name: "onnxruntime-linux-x64-1.20.1.tgz",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.20.1/onnxruntime-linux-x64-1.20.1.tgz",
        sha256: Sha256Digest::from_hex_const(
            "67db4dc1561f1e3fd42e619575c82c601ef89849afc7ea85a003abbac1a1a105",
        ),
    },
    library_member: "onnxruntime-linux-x64-1.20.1/lib/libonnxruntime.so.1.20.1",
    library_sha256: Sha256Digest::from_hex_const(
        "a5faaf78a37590d3fe640f887620e74f6022d34550172b91ad2131bf0ad77d64",
    ),
};

/// ONNX Runtime pins for `linux/arm64`.
pub const ORT_AARCH64: OrtPins = OrtPins {
    archive: ArchivePin {
        file_name: "onnxruntime-linux-aarch64-1.20.1.tgz",
        url: "https://github.com/microsoft/onnxruntime/releases/download/v1.20.1/onnxruntime-linux-aarch64-1.20.1.tgz",
        sha256: Sha256Digest::from_hex_const(
            "ae4fedbdc8c18d688c01306b4b50c63de3445cdf2dbd720e01a2fa3810b8106a",
        ),
    },
    library_member: "onnxruntime-linux-aarch64-1.20.1/lib/libonnxruntime.so.1.20.1",
    library_sha256: Sha256Digest::from_hex_const(
        "f8407a59e7e4c4dddf3b6e01198a24444aa66ac4ad93b4b4284b6e98d1f938b1",
    ),
};

impl SherpaPins {
    /// The pins for an architecture.
    pub const fn for_arch(arch: Arch) -> &'static Self {
        match arch {
            Arch::X86_64 => &SHERPA_X86_64,
            Arch::Aarch64 => &SHERPA_AARCH64,
        }
    }
}

impl OrtPins {
    /// The pins for an architecture.
    pub const fn for_arch(arch: Arch) -> &'static Self {
        match arch {
            Arch::X86_64 => &ORT_X86_64,
            Arch::Aarch64 => &ORT_AARCH64,
        }
    }
}

/// A model file published at a fixed URL, with its digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPin {
    /// Where the file is published.
    pub url: &'static str,
    /// Its size in bytes.
    pub size: u64,
    /// Its digest.
    pub sha256: Sha256Digest,
}

/// DTLN stage one (magnitude mask), at the pinned upstream commit below.
pub const DTLN_MODEL_1: ModelPin = ModelPin {
    url: "https://raw.githubusercontent.com/breizhn/DTLN/1de1f15a8b5b7e1c44905618ff2ef70ca8277fbc/pretrained_model/model_1.onnx",
    size: 1_458_237,
    sha256: Sha256Digest::from_hex_const(
        "22b91cae3855e5a0620e66a917ca6c82c58db0e842c770f58d86751c5e8d4ae3",
    ),
};

/// DTLN stage two (time-domain block), at the same commit.
pub const DTLN_MODEL_2: ModelPin = ModelPin {
    url: "https://raw.githubusercontent.com/breizhn/DTLN/1de1f15a8b5b7e1c44905618ff2ef70ca8277fbc/pretrained_model/model_2.onnx",
    size: 2_510_010,
    sha256: Sha256Digest::from_hex_const(
        "e20c92f9233fccf29cddf86970d0d0161a03aebccc26d6f4d5639c4d5ec2e639",
    ),
};

/// The deployed BCResNet wake graph (`bcresnet_audio.onnx` at the repository root).
pub const DEPLOYED_WAKE_GRAPH_SHA256: Sha256Digest = Sha256Digest::from_hex_const(
    "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f",
);

/// The deployed BCResNet sidecar (`bcresnet_audio.json` at the repository root).
pub const DEPLOYED_WAKE_SIDECAR_SHA256: Sha256Digest = Sha256Digest::from_hex_const(
    "25da0c652c562bf0a45a8f34bb46e38788bfc689e31401f33950831a0f2af51f",
);

/// The Kroko archive's `test_wavs/0.wav` (16-bit mono, 24 kHz, 184,608 bytes). The manifest pins
/// the archive that contains it; this pins the member once extracted.
pub const KROKO_TEST_WAV_SHA256: Sha256Digest = Sha256Digest::from_hex_const(
    "148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6",
);

/// What the pinned Kroko model transcribes from the pinned test WAV followed by 4 s of zeros,
/// fed at 24 kHz in 80 ms blocks (the [`crate::qualify`] protocol). sherpa-onnx's own 1.13.8
/// Python package (the wheel pinned in `speech_bundle.json`) and this crate give the same text.
pub const KROKO_TEST_TRANSCRIPT: &str =
    "Ask not what your country can do for you. Ask what you can do for your country";
