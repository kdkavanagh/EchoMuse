#!/bin/bash
set -e

REPO_ROOT=$(git rev-parse --show-toplevel)
BUILD_DIR="$(pwd)/build"

build_tool() {
    local name=$1
    local tool_dir="$(pwd)/tools/$name"

    echo "Building $name..."
    docker run --rm \
        --entrypoint bash \
        -e CGO_LDFLAGS="-Wl,--hash-style=both" \
        -v "$tool_dir":/sdk \
        -v "$REPO_ROOT/GoTinyAlsa":/GoTinyAlsa \
        echomuse-compiler \
        -c "cd /sdk && go build -tags server -o $name ."

    mkdir -p "$BUILD_DIR"
    mv "$tool_dir/$name" "$BUILD_DIR/$name"
    echo "Output: $BUILD_DIR/$name"
}

# afe_probe imports internal/opensl, and Go's internal-package rule only
# permits that from inside github.com/wilbowes/EchoMuse, so it builds with the
# whole device module mounted. It needs -tags server: internal/opensl is
# itself //go:build server (it dlopens OpenSL ES, which only exists on the
# device), so building without the tag fails with
# "build constraints exclude all Go files", the same error cmd/server hits.
build_module_tool_server() {
    local name=$1

    echo "Building $name..."
    mkdir -p "$BUILD_DIR"
    docker run --rm \
        --entrypoint bash \
        -e CGO_LDFLAGS="-Wl,--hash-style=both" \
        -v "$(pwd)":/sdk \
        -v "$REPO_ROOT/GoTinyAlsa":/GoTinyAlsa \
        echomuse-compiler \
        -c "cd /sdk && go build -tags server -o build/$name ./tools/$name"

    echo "Output: $BUILD_DIR/$name"
}

build_tool capture_mics
build_tool bf_capture
build_module_tool_server afe_probe

echo ""
echo "Deploy:"
echo "  adb shell su -c 'stop echomuse'"
echo "  adb push $BUILD_DIR/capture_mics /sdcard/capture_mics"
echo "  adb push $BUILD_DIR/bf_capture /sdcard/bf_capture"
echo "  adb shell \"su -c 'cp /sdcard/capture_mics /data/local/bin/capture_mics && chmod 755 /data/local/bin/capture_mics'\""
echo "  adb shell \"su -c 'cp /sdcard/bf_capture /data/local/bin/bf_capture && chmod 755 /data/local/bin/bf_capture'\""
echo ""
echo "Run:"
echo "  adb shell su -c 'bf_capture --angle 330 --seconds 5'"
echo "  adb pull /tmp/ ."
echo ""
echo "Without adb, push over the controller shell plane instead:"
echo "  python controller/tools/push_file.py <local> <remote>   (resumable)"
echo ""
echo "afe_probe is the docs/native-afe-migration.md phase-0 spike — records"
echo "through OpenSL ES at MIC/VOICE_RECOGNITION/VOICE_COMMUNICATION, plays a"
echo "known tone for an ERLE estimate, writes WAVs, and prints a go/no-go line:"
echo "  adb push $BUILD_DIR/afe_probe /sdcard/afe_probe"
echo "  adb shell \"su -c 'cp /sdcard/afe_probe /data/local/tmp/afe_probe && chmod 755 /data/local/tmp/afe_probe'\""
echo "  adb shell \"su -c '/data/local/tmp/afe_probe -seconds 8 -out /sdcard'\""
echo "  adb pull /sdcard/afe_probe_mic.wav /sdcard/afe_probe_voice_recognition.wav /sdcard/afe_probe_voice_communication.wav ."
