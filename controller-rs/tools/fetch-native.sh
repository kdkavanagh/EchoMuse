#!/usr/bin/env bash
# Fetches the pinned native libraries the controller builds and runs against, for one
# architecture, and unpacks them under DEST (default: controller-rs/native):
#
#   DEST/sherpa-onnx/lib/libsherpa-onnx-c-api.so   from the sherpa-onnx-core 1.13.8 wheel
#   DEST/sherpa-onnx/lib/libonnxruntime.so         (sherpa-onnx's own ONNX Runtime)
#   DEST/sherpa-onnx/arch                          the Rust target architecture they are for
#   DEST/onnxruntime/libonnxruntime.so.1.20.1      the official ONNX Runtime 1.20.1 release
#
# .cargo/config.toml points SHERPA_ONNX_LIB_DIR at DEST/sherpa-onnx/lib, so after this
# plain `cargo build` and `cargo test` link the pinned libraries. The 1.20.1 library is
# what ONNXRUNTIME_LIB names at run time (the image's /app/models/onnxruntime/).
#
# Only the two libraries pins.rs pins are taken from the wheel: everything in the lib
# directory is verified. Downloads are kept in DEST/.downloads and reused when their
# digest still matches.
#
# Usage: tools/fetch-native.sh [--arch amd64|arm64] [DEST]
#   --arch  defaults to this machine's architecture (x86_64/aarch64 are accepted too)

# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

arch=$(uname -m)
dest="$RS_DIR/native"
while [ $# -gt 0 ]; do
    case $1 in
    --arch)
        [ $# -ge 2 ] || die "--arch needs a value"
        arch=$2
        shift 2
        ;;
    -h | --help)
        sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
        exit 0
        ;;
    -*) die "unknown option $1" ;;
    *)
        dest=$1
        shift
        ;;
    esac
done

# Hashes from crates/speech/src/pins.rs (SHERPA_* and ORT_*), checked against it below.
case $arch in
amd64 | x86_64)
    rust_arch=x86_64
    wheel_url=https://files.pythonhosted.org/packages/b6/ca/e27c1fb5c54b181d4a5450f3d5c789d9da02eb42dc9b20cfdab5dda4c864/sherpa_onnx_core-1.13.8-py3-none-manylinux2014_x86_64.whl
    wheel_sha=4da90acf435373d7b2ba9cc0be806e7e274d28f63f5780880b5dea6721626e36
    c_api_sha=e50ae8bca4db52b7be04f56f8ac1502b940ca124a26fac0d48d8cef7fc9823d4
    sherpa_ort_sha=4b3607aebd1784b26b6f9b20e4bd974c7ab8287043e4d095cb7d2cb40b5e566e
    ort_url=https://github.com/microsoft/onnxruntime/releases/download/v1.20.1/onnxruntime-linux-x64-1.20.1.tgz
    ort_sha=67db4dc1561f1e3fd42e619575c82c601ef89849afc7ea85a003abbac1a1a105
    ort_member=onnxruntime-linux-x64-1.20.1/lib/libonnxruntime.so.1.20.1
    ort_lib_sha=a5faaf78a37590d3fe640f887620e74f6022d34550172b91ad2131bf0ad77d64
    ;;
arm64 | aarch64)
    rust_arch=aarch64
    wheel_url=https://files.pythonhosted.org/packages/9c/72/02227b14cd3fb3d504a8814ad144ed321a53331948c893821e02f4abb1d7/sherpa_onnx_core-1.13.8-py3-none-manylinux2014_aarch64.whl
    wheel_sha=a51a03d55c376c32e15dd63ea852236042ba91a5cd73e253bbf7cf21f6582ba9
    c_api_sha=1be01a1a5c0420e32a81011222e771fc35fa5e8d8cce951281d769d849296cab
    sherpa_ort_sha=cfc800d4438f15e8d9017521326776ca7117d8783cb847ccccc2fc61ac4d540f
    ort_url=https://github.com/microsoft/onnxruntime/releases/download/v1.20.1/onnxruntime-linux-aarch64-1.20.1.tgz
    ort_sha=ae4fedbdc8c18d688c01306b4b50c63de3445cdf2dbd720e01a2fa3810b8106a
    ort_member=onnxruntime-linux-aarch64-1.20.1/lib/libonnxruntime.so.1.20.1
    ort_lib_sha=f8407a59e7e4c4dddf3b6e01198a24444aa66ac4ad93b4b4284b6e98d1f938b1
    ;;
*) die "no pinned native libraries for architecture '$arch' (amd64 or arm64)" ;;
esac

need curl unzip tar sha256sum
pinned_in_rust "$wheel_sha" "the sherpa-onnx-core wheel"
pinned_in_rust "$c_api_sha" "libsherpa-onnx-c-api.so"
pinned_in_rust "$sherpa_ort_sha" "sherpa-onnx's libonnxruntime.so"
pinned_in_rust "$ort_sha" "the ONNX Runtime 1.20.1 archive"
pinned_in_rust "$ort_lib_sha" "libonnxruntime.so.1.20.1"

mkdir -p "$dest"
dest=$(cd "$dest" && pwd)
downloads="$dest/.downloads"
wheel="$downloads/$(basename "$wheel_url")"
ort_archive="$downloads/$(basename "$ort_url")"
fetch "$wheel_url" "$wheel_sha" "$wheel"
fetch "$ort_url" "$ort_sha" "$ort_archive"

# extract_checked ARCHIVE_KIND ARCHIVE MEMBER SHA256 DEST: one member, verified before
# it takes DEST's name.
extract_checked() {
    local kind=$1 archive=$2 member=$3 want=$4 out=$5
    if file_ok "$out" "$want"; then
        return 0
    fi
    mkdir -p "$(dirname "$out")"
    case $kind in
    zip) unzip -p "$archive" "$member" >"$out.part" ;;
    tgz) tar -xzOf "$archive" "$member" >"$out.part" ;;
    esac
    local got
    got=$(sha256_of "$out.part")
    if [ "$got" != "$want" ]; then
        rm -f "$out.part"
        die "$member in $(basename "$archive") has SHA-256 $got, expected $want"
    fi
    chmod 0755 "$out.part"
    mv "$out.part" "$out"
}

sherpa_lib="$dest/sherpa-onnx/lib"
# A directory left by another architecture must not keep libraries this run did not check.
if [ -f "$dest/sherpa-onnx/arch" ] && [ "$(cat "$dest/sherpa-onnx/arch")" != "$rust_arch" ]; then
    rm -rf "$dest/sherpa-onnx" "$dest/onnxruntime"
fi
extract_checked zip "$wheel" sherpa_onnx/lib/libsherpa-onnx-c-api.so "$c_api_sha" \
    "$sherpa_lib/libsherpa-onnx-c-api.so"
extract_checked zip "$wheel" sherpa_onnx/lib/libonnxruntime.so "$sherpa_ort_sha" \
    "$sherpa_lib/libonnxruntime.so"
extract_checked tgz "$ort_archive" "$ort_member" "$ort_lib_sha" \
    "$dest/onnxruntime/libonnxruntime.so.1.20.1"
printf '%s\n' "$rust_arch" >"$dest/sherpa-onnx/arch"

extra=$(find "$sherpa_lib" -mindepth 1 ! -name libsherpa-onnx-c-api.so ! -name libonnxruntime.so)
[ -z "$extra" ] || die "unexpected files in $sherpa_lib (remove them): $extra"

echo "sherpa-onnx 1.13.8 ($rust_arch): $sherpa_lib"
echo "ONNX Runtime 1.20.1 ($rust_arch): $dest/onnxruntime/libonnxruntime.so.1.20.1"
