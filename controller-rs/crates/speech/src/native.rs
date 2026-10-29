//! The two native runtimes: sherpa-onnx (linked) and ONNX Runtime for `ort` (loaded by path).
//!
//! **sherpa-onnx** is linked through `sherpa-onnx-sys`'s shared build, so the dynamic loader maps
//! `libsherpa-onnx-c-api.so` and its `libonnxruntime.so` before `main` runs, from the build's
//! run path (the extracted archive in the target directory, then the executable's directory) or
//! `LD_LIBRARY_PATH`. Hashing a path before use would not prove which file was mapped, so
//! [`SherpaRuntime::verify`] asks the loader which files it actually mapped (`dladdr` on a C API
//! symbol, and `dl_iterate_phdr` for its ONNX Runtime) and hashes those.
//!
//! **ONNX Runtime for `ort`** is opened by path with `ort`'s `load-dynamic` feature.
//! [`OrtRuntime::load`] hashes the file, loads exactly that path once per process and creates the
//! process's single `ort` environment. Every `ort` session in this crate is built from an
//! [`OrtRuntime`], so no session can be created before the pinned library is loaded: without it,
//! `ort` would `dlopen("libonnxruntime.so")` by name and could pick up sherpa-onnx's copy.
//!
//! The two ONNX Runtime copies are separate files with different sonames (`libonnxruntime.so`
//! and `libonnxruntime.so.1`), so both are mapped side by side. They export only the `Ort*` C
//! entry points, and `ort` resolves its entry point from its own library handle.

use std::ffi::{CStr, OsStr, c_char, c_int, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::digest::{PinnedFile, Sha256Digest};
use crate::error::SpeechError;
use crate::pins::{Arch, OrtPins, SHERPA_ONNX_VERSION, SherpaPins};

/// Where the native libraries are and what they must hash to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeLibraries {
    /// The ONNX Runtime library `ort` loads (BCResNet, Silero, DTLN).
    pub onnxruntime: PinnedFile,
    /// The sherpa-onnx libraries the binary is linked against (Kroko).
    pub sherpa: SherpaLibraries,
}

/// What the linked sherpa-onnx libraries must be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SherpaLibraries {
    /// The version `SherpaOnnxGetVersionStr` must report.
    pub version: String,
    /// Digest of the mapped `libsherpa-onnx-c-api.so`.
    pub c_api_sha256: Sha256Digest,
    /// Digest of the mapped `libonnxruntime.so` beside it.
    pub onnxruntime_sha256: Sha256Digest,
    /// When set, the mapped libraries must be the files in this directory. When unset, they
    /// may be anywhere the loader found them, as long as their digests match.
    pub directory: Option<PathBuf>,
}

impl NativeLibraries {
    /// The pinned libraries for the architecture this crate was compiled for: the official
    /// ONNX Runtime 1.20.1 library at `onnxruntime_path`, and the sherpa-onnx 1.13.8 shared build.
    pub fn pinned(
        onnxruntime_path: impl Into<PathBuf>,
        sherpa_directory: Option<PathBuf>,
    ) -> Result<Self, SpeechError> {
        let arch = Arch::current().ok_or_else(|| {
            SpeechError::NativeLibrary("no pinned native libraries for this architecture".into())
        })?;
        let ort = OrtPins::for_arch(arch);
        let sherpa = SherpaPins::for_arch(arch);
        Ok(Self {
            onnxruntime: PinnedFile::new(onnxruntime_path, ort.library_sha256),
            sherpa: SherpaLibraries {
                version: SHERPA_ONNX_VERSION.to_owned(),
                c_api_sha256: sherpa.c_api_sha256,
                onnxruntime_sha256: sherpa.onnxruntime_sha256,
                directory: sherpa_directory,
            },
        })
    }
}

/// A native library as the loader mapped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedLibrary {
    /// Canonical path of the mapped file.
    pub path: PathBuf,
    /// Its digest.
    pub sha256: Sha256Digest,
    /// The version string it reports.
    pub version: String,
}

/// Proof that `ort`'s pinned ONNX Runtime is loaded, holding the process's `ort` environment.
/// Cheap to clone; every clone refers to the same environment.
#[derive(Clone)]
pub struct OrtRuntime {
    env: ort::environment::Environment,
    library: LoadedLibrary,
}

impl std::fmt::Debug for OrtRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrtRuntime")
            .field("library", &self.library)
            .finish_non_exhaustive()
    }
}

/// The one ONNX Runtime `ort` may load in this process. A process-wide value, not device state:
/// ONNX Runtime can be loaded only once per process.
static ORT_RUNTIME: Mutex<Option<OrtRuntime>> = Mutex::new(None);

impl OrtRuntime {
    /// Verifies and loads the pinned ONNX Runtime library, or returns the already-loaded one if
    /// it is the same file. Loading a different library after one is loaded is an error.
    pub fn load(library: &PinnedFile) -> Result<Self, SpeechError> {
        let canonical = library
            .path
            .canonicalize()
            .map_err(|source| SpeechError::Io {
                path: library.path.clone(),
                source,
            })?;
        let mut slot = ORT_RUNTIME.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(loaded) = slot.as_ref() {
            if loaded.library.path == canonical && loaded.library.sha256 == library.sha256 {
                return Ok(loaded.clone());
            }
            return Err(SpeechError::NativeLibrary(format!(
                "ONNX Runtime {} is already loaded; cannot also load {}",
                loaded.library.path.display(),
                canonical.display()
            )));
        }
        PinnedFile::new(&canonical, library.sha256).verify()?;
        let builder = ort::init_from(&canonical)
            .map_err(|e| SpeechError::NativeLibrary(format!("loading ONNX Runtime: {e}")))?;
        let env = builder.with_name("echomuse-speech").build()?;
        let version = ort_library_version(&canonical)?;
        let runtime = Self {
            env,
            library: LoadedLibrary {
                path: canonical,
                sha256: library.sha256,
                version,
            },
        };
        *slot = Some(runtime.clone());
        Ok(runtime)
    }

    /// The loaded library.
    pub fn library(&self) -> &LoadedLibrary {
        &self.library
    }

    /// The process's `ort` environment, for building sessions of other models on the same
    /// verified library.
    pub fn environment(&self) -> &ort::environment::Environment {
        &self.env
    }
}

/// Asks the already-mapped ONNX Runtime at `path` for its version string.
fn ort_library_version(path: &Path) -> Result<String, SpeechError> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| SpeechError::NativeLibrary("library path contains a NUL byte".into()))?;
    // SAFETY: `c_path` is a valid NUL-terminated string. RTLD_NOLOAD only looks up an object
    // that is already mapped (ort mapped it above) and adds a reference we release below.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD) };
    if handle.is_null() {
        return Err(SpeechError::NativeLibrary(format!(
            "{} is not mapped after loading",
            path.display()
        )));
    }
    // SAFETY: `handle` is a live handle and the symbol name is NUL-terminated.
    let sym = unsafe { libc::dlsym(handle, c"OrtGetApiBase".as_ptr()) };
    let result = if sym.is_null() {
        Err(SpeechError::NativeLibrary(format!(
            "{} does not export OrtGetApiBase",
            path.display()
        )))
    } else {
        // SAFETY: `OrtGetApiBase` has this signature in every ONNX Runtime release; the returned
        // struct and its version string are static data of the library, which stays mapped.
        unsafe {
            let get: unsafe extern "C" fn() -> *const ort::sys::OrtApiBase =
                std::mem::transmute(sym);
            let base = get();
            if base.is_null() {
                Err(SpeechError::NullHandle("OrtGetApiBase"))
            } else {
                c_string(((*base).GetVersionString)(), "OrtApiBase.GetVersionString")
            }
        }
    };
    // SAFETY: releases the reference RTLD_NOLOAD added; ort keeps its own.
    unsafe { libc::dlclose(handle) };
    result
}

/// Proof that the linked sherpa-onnx libraries are the pinned ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SherpaRuntime {
    c_api: LoadedLibrary,
    onnxruntime: LoadedLibrary,
}

/// The last verified sherpa-onnx configuration, so repeated loads do not rehash 30 MB.
static SHERPA_RUNTIME: Mutex<Option<(SherpaLibraries, SherpaRuntime)>> = Mutex::new(None);

impl SherpaRuntime {
    /// Finds the sherpa-onnx C API library and its ONNX Runtime as mapped into this process and
    /// checks their version and digests.
    pub fn verify(config: &SherpaLibraries) -> Result<Self, SpeechError> {
        let mut slot = SHERPA_RUNTIME
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((verified_config, runtime)) = slot.as_ref()
            && verified_config == config
        {
            return Ok(runtime.clone());
        }
        // SAFETY: returns a pointer to a static string inside the library, or null.
        let version = c_string(
            unsafe { sherpa_onnx_sys::SherpaOnnxGetVersionStr() },
            "SherpaOnnxGetVersionStr",
        )?;
        if version != config.version {
            return Err(SpeechError::NativeLibrary(format!(
                "linked sherpa-onnx reports version {version}, expected {}",
                config.version
            )));
        }
        let c_api_path = mapped_path_of(sherpa_onnx_sys::SherpaOnnxGetVersionStr as *const c_void)?;
        let c_api_dir = c_api_path
            .parent()
            .ok_or_else(|| {
                SpeechError::NativeLibrary("sherpa-onnx library has no directory".into())
            })?
            .to_path_buf();
        if let Some(dir) = &config.directory {
            let want = dir.canonicalize().map_err(|source| SpeechError::Io {
                path: dir.clone(),
                source,
            })?;
            if want != c_api_dir {
                return Err(SpeechError::NativeLibrary(format!(
                    "sherpa-onnx was loaded from {}, expected {}",
                    c_api_dir.display(),
                    want.display()
                )));
            }
        }
        PinnedFile::new(&c_api_path, config.c_api_sha256).verify()?;

        // sherpa-onnx's ONNX Runtime is the `libonnxruntime.so` beside the C API library (its
        // run path is `$ORIGIN`); it must be the one the loader actually mapped.
        let expected = c_api_dir.join("libonnxruntime.so");
        let ort_path = expected.canonicalize().map_err(|source| SpeechError::Io {
            path: expected.clone(),
            source,
        })?;
        let loaded = mapped_objects();
        let ort_mapped = loaded
            .iter()
            .any(|p| p.canonicalize().is_ok_and(|p| p == ort_path));
        if !ort_mapped {
            let onnx: Vec<String> = loaded
                .iter()
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("libonnxruntime"))
                })
                .map(|p| p.display().to_string())
                .collect();
            return Err(SpeechError::NativeLibrary(format!(
                "sherpa-onnx's ONNX Runtime {} is not mapped; mapped ONNX Runtime files: {onnx:?}",
                ort_path.display()
            )));
        }
        PinnedFile::new(&ort_path, config.onnxruntime_sha256).verify()?;
        // SAFETY: returns a pointer to a static string inside ONNX Runtime, or null.
        let ort_version = c_string(
            unsafe { sherpa_onnx_sys::SherpaOnnxGetOnnxruntimeVersionStr() },
            "SherpaOnnxGetOnnxruntimeVersionStr",
        )?;
        let runtime = Self {
            c_api: LoadedLibrary {
                path: c_api_path,
                sha256: config.c_api_sha256,
                version,
            },
            onnxruntime: LoadedLibrary {
                path: ort_path,
                sha256: config.onnxruntime_sha256,
                version: ort_version,
            },
        };
        *slot = Some((config.clone(), runtime.clone()));
        Ok(runtime)
    }

    /// The mapped `libsherpa-onnx-c-api.so`.
    pub fn c_api(&self) -> &LoadedLibrary {
        &self.c_api
    }

    /// The mapped sherpa-onnx ONNX Runtime.
    pub fn onnxruntime(&self) -> &LoadedLibrary {
        &self.onnxruntime
    }
}

/// Copies a NUL-terminated string returned by a native function. The pointer is not freed: the
/// callers pass only functions that return static strings.
pub(crate) fn c_string(ptr: *const c_char, what: &'static str) -> Result<String, SpeechError> {
    if ptr.is_null() {
        return Err(SpeechError::NullHandle(what));
    }
    // SAFETY: non-null, and every caller passes a NUL-terminated string that outlives this call.
    let s = unsafe { CStr::from_ptr(ptr) };
    s.to_str()
        .map(str::to_owned)
        .map_err(|_| SpeechError::NativeLibrary(format!("{what} returned text that is not UTF-8")))
}

/// The canonical path of the mapped object containing `addr`.
fn mapped_path_of(addr: *const c_void) -> Result<PathBuf, SpeechError> {
    // SAFETY: `Dl_info` is plain data; dladdr fills it or returns 0.
    let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
    // SAFETY: `addr` is the address of a function in a mapped library; `info` is writable.
    let found = unsafe { libc::dladdr(addr, &mut info) };
    if found == 0 || info.dli_fname.is_null() {
        return Err(SpeechError::NativeLibrary(
            "the loader cannot name the library holding the sherpa-onnx C API".into(),
        ));
    }
    // SAFETY: dli_fname points at the loader's NUL-terminated name of a mapped object, which
    // stays valid while that object is mapped (it is never unmapped).
    let name = unsafe { CStr::from_ptr(info.dli_fname) };
    let path = PathBuf::from(OsStr::from_bytes(name.to_bytes()));
    path.canonicalize()
        .map_err(|source| SpeechError::Io { path, source })
}

/// Paths of every object the loader has mapped into the process.
fn mapped_objects() -> Vec<PathBuf> {
    unsafe extern "C" fn collect(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut c_void,
    ) -> c_int {
        // SAFETY: `data` is the `&mut Vec<PathBuf>` passed below, alive for the whole iteration;
        // `info` is valid for the duration of this callback.
        unsafe {
            let out = &mut *(data as *mut Vec<PathBuf>);
            let name = (*info).dlpi_name;
            if !name.is_null() {
                let bytes = CStr::from_ptr(name).to_bytes();
                if !bytes.is_empty() {
                    out.push(PathBuf::from(OsStr::from_bytes(bytes)));
                }
            }
        }
        0
    }
    let mut out: Vec<PathBuf> = Vec::new();
    // SAFETY: the callback only reads loader-owned data and appends to `out`.
    unsafe { libc::dl_iterate_phdr(Some(collect), &mut out as *mut Vec<PathBuf> as *mut c_void) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_libraries_follow_the_architecture() {
        let libs = NativeLibraries::pinned("/x/libonnxruntime.so.1.20.1", None).unwrap();
        let arch = Arch::current().unwrap();
        assert_eq!(
            libs.onnxruntime.sha256,
            OrtPins::for_arch(arch).library_sha256
        );
        assert_eq!(
            libs.sherpa.c_api_sha256,
            SherpaPins::for_arch(arch).c_api_sha256
        );
        assert_eq!(libs.sherpa.version, "1.13.8");
        let json = serde_json::to_string(&libs).unwrap();
        assert_eq!(
            serde_json::from_str::<NativeLibraries>(&json).unwrap(),
            libs
        );
    }

    #[test]
    fn linked_sherpa_is_the_pinned_build() {
        let arch = Arch::current().unwrap();
        let pins = SherpaPins::for_arch(arch);
        let config = SherpaLibraries {
            version: SHERPA_ONNX_VERSION.into(),
            c_api_sha256: pins.c_api_sha256,
            onnxruntime_sha256: pins.onnxruntime_sha256,
            directory: None,
        };
        let rt = SherpaRuntime::verify(&config).unwrap();
        assert!(rt.c_api().path.ends_with("libsherpa-onnx-c-api.so"));
        assert_eq!(rt.c_api().version, "1.13.8");
        assert!(!rt.onnxruntime().version.is_empty());

        let wrong_version = SherpaLibraries {
            version: "0.0.0".into(),
            ..config.clone()
        };
        assert!(matches!(
            SherpaRuntime::verify(&wrong_version),
            Err(SpeechError::NativeLibrary(_))
        ));
        let wrong_hash = SherpaLibraries {
            c_api_sha256: Sha256Digest::of_bytes(b"not the library"),
            ..config.clone()
        };
        assert!(matches!(
            SherpaRuntime::verify(&wrong_hash),
            Err(SpeechError::HashMismatch { .. })
        ));
        let wrong_dir = SherpaLibraries {
            directory: Some(std::env::temp_dir()),
            ..config
        };
        assert!(matches!(
            SherpaRuntime::verify(&wrong_dir),
            Err(SpeechError::NativeLibrary(_))
        ));
    }

    #[test]
    fn c_string_rejects_null() {
        assert!(matches!(
            c_string(std::ptr::null(), "f"),
            Err(SpeechError::NullHandle("f"))
        ));
    }
}
