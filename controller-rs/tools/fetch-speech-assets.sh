#!/usr/bin/env bash
# Fetches the controller's speech models and the runtime it serves to Dots, verifies every
# file, and lays them out under ROOT the way the image does under /app
# (default ROOT: controller-rs/local):
#
#   ROOT/speech/                  SPEECH_BUNDLE_DIR: the bundle controller/speech_bundle.json
#                                 describes, laid out by its paths
#     kroko/encoder.onnx, decoder.onnx, joiner.onnx, tokens.txt
#     kroko/test_wavs/0.wav       the archive's test WAV, used by model qualification
#     silero_vad_v5.onnx
#     KROKO-ATTRIBUTION.txt       the Kroko model's CC-BY-SA attribution
#     speech_bundle.json          a copy of the manifest the files were checked against
#   ROOT/models/dtln/             NS_MODEL_DIR: DTLN model_1.onnx and model_2.onnx
#   ROOT/models/ort_android/libonnxruntime.so
#                                 ORT_ANDROID_LIB: ONNX Runtime 1.19.2 armeabi-v7a, served
#                                 to Dots as a speech asset, never loaded here
#
# Hashes come from controller/speech_bundle.json (Kroko, Silero), crates/speech/src/pins.rs
# (DTLN, the test WAV) and the archive and library hashes below (Android ONNX Runtime).
# The Android library's hash is also recorded in docs/post-afe-audio-architecture.md.
# The manifest's Python wheel is not fetched: the Rust controller links sherpa-onnx through
# tools/fetch-native.sh instead. Downloads are kept in ROOT/.downloads.
#
# Usage: tools/fetch-speech-assets.sh [ROOT]

# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

root="$RS_DIR/local"
case ${1:-} in
-h | --help)
    sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
    exit 0
    ;;
-*) die "unknown option $1" ;;
"") ;;
*) root=$1 ;;
esac
[ $# -le 1 ] || die "takes at most one argument, ROOT"

need curl jq tar bzip2 unzip sha256sum

manifest="$REPO_DIR/controller/speech_bundle.json"
[ -f "$manifest" ] || die "$manifest is missing"
[ "$(jq -r .version "$manifest")" = 1 ] || die "$manifest is not version 1"

# DTLN and the test WAV: crates/speech/src/pins.rs (DTLN_MODEL_1/2, KROKO_TEST_WAV_SHA256).
dtln_commit=1de1f15a8b5b7e1c44905618ff2ef70ca8277fbc
dtln1_sha=22b91cae3855e5a0620e66a917ca6c82c58db0e842c770f58d86751c5e8d4ae3
dtln2_sha=e20c92f9233fccf29cddf86970d0d0161a03aebccc26d6f4d5639c4d5ec2e639
test_wav_sha=148b936b43ce7c546a866e64da059f0458aee2d65e617f16e9d94f06e8d99ed6
pinned_in_rust "$dtln1_sha" "DTLN model_1.onnx"
pinned_in_rust "$dtln2_sha" "DTLN model_2.onnx"
pinned_in_rust "$test_wav_sha" "the Kroko test WAV"

# ONNX Runtime 1.19.2 for Android: pin the AAR and the armeabi-v7a library inside it.
# Devices verify the library's hash when they install this speech asset.
aar_url=https://repo1.maven.org/maven2/com/microsoft/onnxruntime/onnxruntime-android/1.19.2/onnxruntime-android-1.19.2.aar
aar_sha=84737dd1fdb715f0ee695c0a9864f6949b66ecb4c46ca733bc9211bf0c2a7b5f
android_lib_sha=174233cf1a3f841f1eac82a4328f4f23f8819d5c62969c09e994a6b1abf498c1

mkdir -p "$root"
root=$(cd "$root" && pwd)
downloads="$root/.downloads"
speech="$root/speech"

m() { jq -er "$1" "$manifest"; }

# checked_in_bundle PATH SIZE SHA256: true when the bundle file has that size and digest.
checked_in_bundle() {
    [ -f "$speech/$1" ] && [ "$(stat -c %s "$speech/$1")" = "$2" ] &&
        [ "$(sha256_of "$speech/$1")" = "$3" ]
}

# Kroko: the archive, then each manifest file from the archive's top directory.
asr_name=$(m .asr.name)
kroko_keys="encoder decoder joiner tokens"
kroko_ok=true
for key in $kroko_keys; do
    checked_in_bundle "$(m ".asr.files.$key.path")" "$(m ".asr.files.$key.size")" \
        "$(m ".asr.files.$key.sha256")" || kroko_ok=false
done
test_wav_member=$(m .asr.test_wav_member)
test_wav="kroko/${test_wav_member#"$asr_name"/}"
file_ok "$speech/$test_wav" "$test_wav_sha" || kroko_ok=false
if [ "$kroko_ok" = false ]; then
    archive="$downloads/$(m .asr.archive.path)"
    fetch "$(m .asr.archive.url)" "$(m .asr.archive.sha256)" "$archive"
    staging="$downloads/kroko.staging"
    rm -rf "$staging"
    mkdir -p "$staging"
    tar -xjf "$archive" -C "$staging"
    for key in $kroko_keys; do
        path=$(m ".asr.files.$key.path")
        install_file "$staging/$asr_name/$(basename "$path")" "$speech/$path"
        checked_in_bundle "$path" "$(m ".asr.files.$key.size")" "$(m ".asr.files.$key.sha256")" ||
            die "$path from $(m .asr.archive.path) does not match $manifest"
    done
    install_file "$staging/$test_wav_member" "$speech/$test_wav"
    check_file "$speech/$test_wav" "$test_wav_sha"
    rm -rf "$staging"
fi

# Silero VAD v5.
vad_path=$(m .vad.path)
fetch "$(m .vad.url)" "$(m .vad.sha256)" "$speech/$vad_path"
checked_in_bundle "$vad_path" "$(m .vad.size)" "$(m .vad.sha256)" ||
    die "$vad_path does not match the size in $manifest"

# Attribution, which the Kroko model's licence requires to travel with it.
attribution="$speech/$(m .attribution.path)"
cat >"$attribution.part" <<EOF
Kroko streaming ASR model: $asr_name
Licence: $(m .attribution.license)
Source: $(m .attribution.url)
Archive: $(m .asr.archive.url) (SHA-256 $(m .asr.archive.sha256))
EOF
mv "$attribution.part" "$attribution"
install_file "$manifest" "$speech/speech_bundle.json"

# DTLN.
dtln_base="https://raw.githubusercontent.com/breizhn/DTLN/$dtln_commit/pretrained_model"
fetch "$dtln_base/model_1.onnx" "$dtln1_sha" "$root/models/dtln/model_1.onnx"
fetch "$dtln_base/model_2.onnx" "$dtln2_sha" "$root/models/dtln/model_2.onnx"

# ONNX Runtime for Android.
android_lib="$root/models/ort_android/libonnxruntime.so"
if ! file_ok "$android_lib" "$android_lib_sha"; then
    aar="$downloads/onnxruntime-android-1.19.2.aar"
    fetch "$aar_url" "$aar_sha" "$aar"
    mkdir -p "$(dirname "$android_lib")"
    unzip -p "$aar" jni/armeabi-v7a/libonnxruntime.so >"$android_lib.part"
    if [ "$(sha256_of "$android_lib.part")" != "$android_lib_sha" ]; then
        rm -f "$android_lib.part"
        die "jni/armeabi-v7a/libonnxruntime.so in the AAR does not have SHA-256 $android_lib_sha"
    fi
    mv "$android_lib.part" "$android_lib"
fi

echo "speech bundle: $speech"
echo "DTLN models: $root/models/dtln"
echo "Android ONNX Runtime: $android_lib"
