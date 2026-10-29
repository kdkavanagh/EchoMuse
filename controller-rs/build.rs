//! Build script for the controller binary.
//!
//! It does two things:
//!
//! 1. Gives the executable a run path of `$ORIGIN`, so it loads `libsherpa-onnx-c-api.so`
//!    from its own directory without `LD_LIBRARY_PATH`. `sherpa-onnx-sys` asks for a run
//!    path too, but Cargo applies a dependency's link arguments only to that dependency's
//!    own targets, so the executable would get none. `sherpa-onnx-sys` copies the libraries
//!    into the profile directory beside the executable (`target/release/`), and the image
//!    ships them beside it in `/app/bin/`. `libsherpa-onnx-c-api.so` carries its own
//!    `$ORIGIN` run path, which finds its `libonnxruntime.so` next to it.
//! 2. Stops early with the command to run when the pinned sherpa-onnx libraries have not
//!    been fetched, or were fetched for another architecture. `.cargo/config.toml` points
//!    `SHERPA_ONNX_LIB_DIR` at `native/sherpa-onnx/lib`, which `tools/fetch-native.sh`
//!    fills.

use std::env;
use std::path::PathBuf;
use std::process::exit;

/// The libraries `tools/fetch-native.sh` unpacks and the controller links.
const LIBRARIES: [&str; 2] = ["libsherpa-onnx-c-api.so", "libonnxruntime.so"];

fn fail(problem: &str) -> ! {
    eprintln!(
        "error: {problem}\n\nFetch the pinned sherpa-onnx libraries first, from controller-rs/:\n\n    \
         tools/fetch-native.sh\n\n(.cargo/config.toml then points SHERPA_ONNX_LIB_DIR at \
         native/sherpa-onnx/lib; cargo reads it only when run inside controller-rs/. \
         Cross builds pass --arch and set SHERPA_ONNX_LIB_DIR themselves.)"
    );
    exit(1);
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SHERPA_ONNX_LIB_DIR");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
    }

    let Some(dir) = env::var_os("SHERPA_ONNX_LIB_DIR").map(PathBuf::from) else {
        fail("SHERPA_ONNX_LIB_DIR is not set");
    };
    for library in LIBRARIES {
        let path = dir.join(library);
        println!("cargo:rerun-if-changed={}", path.display());
        if !path.is_file() {
            fail(&format!("{} is missing", path.display()));
        }
    }

    // The fetch script records the architecture next to the libraries; a directory filled
    // some other way has no record and is taken as it is.
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if let Some(record) = dir.parent().map(|parent| parent.join("arch")) {
        println!("cargo:rerun-if-changed={}", record.display());
        if let Ok(fetched) = std::fs::read_to_string(&record)
            && fetched.trim() != target_arch
        {
            fail(&format!(
                "{} holds sherpa-onnx for {}, but this build targets {target_arch}",
                dir.display(),
                fetched.trim()
            ));
        }
    }
}
