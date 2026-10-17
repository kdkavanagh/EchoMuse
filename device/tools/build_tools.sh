#!/bin/bash
set -e

REPO_ROOT=$(git rev-parse --show-toplevel)
BUILD_DIR="$(pwd)/build"

# The tools build with the firmware's own toolchain: the `compiler` stage of
# controller/Dockerfile, tagged echomuse-compiler. Cached after the first run.
docker build -f "$REPO_ROOT/controller/Dockerfile" --target compiler \
    -t echomuse-compiler "$REPO_ROOT"

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

# C tools build with the same image's NDK clang. $CC is that NDK's
# armv7a-linux-androideabi$API-clang (API 22, the firmware's target); a tool
# for another API level swaps the suffix for that level's clang wrapper from
# the same NDK, which links that level's platform libraries. slprobe stays at
# the firmware's API and links libOpenSLES.so; aaprobe needs API 30 (it only
# runs on Android 11) and links libaaudio.so.
build_c_tool() {
    local name=$1 api=$2
    shift 2
    local libs="$*"
    local tool_dir="$(pwd)/tools/$name"

    echo "Building $name (API $api)..."
    docker run --rm \
        --entrypoint bash \
        -v "$tool_dir":/sdk \
        echomuse-compiler \
        -c "cd /sdk && \${CC%\$API-clang}$api-clang -O2 -Wall -Wextra -o $name $name.c $libs -lm -Wl,--hash-style=both"

    mkdir -p "$BUILD_DIR"
    mv "$tool_dir/$name" "$BUILD_DIR/$name"
    echo "Output: $BUILD_DIR/$name"
}

# Go tools inside the firmware's own module (sl_latency) import its internal
# packages, so they build from device/ with the firmware stage's cgo flags.
build_fw_tool() {
    local name=$1
    local tool_dir="$(pwd)/tools/$name"

    echo "Building $name..."
    docker run --rm \
        --entrypoint bash \
        -e CGO_CFLAGS="-Wno-deprecated-declarations -Wno-null-dereference" \
        -v "$(pwd)":/sdk \
        echomuse-compiler \
        -c "cd /sdk && go build -tags server -o tools/$name/$name ./tools/$name"

    mkdir -p "$BUILD_DIR"
    mv "$tool_dir/$name" "$BUILD_DIR/$name"
    echo "Output: $BUILD_DIR/$name"
}

# The C probes first: they need no GoTinyAlsa submodule, the Go tools do.
build_c_tool slprobe 22 -lOpenSLES
build_c_tool aaprobe 30 -laaudio
build_fw_tool sl_latency
build_tool capture_mics
build_tool bf_capture

echo ""
echo "Deploy (the Fire OS 6 adb shell is already root):"
echo "  adb shell stop echomuse"
echo "  adb push $BUILD_DIR/capture_mics /data/local/bin/capture_mics"
echo "  adb push $BUILD_DIR/bf_capture /data/local/bin/bf_capture"
echo "  adb shell chmod 755 /data/local/bin/capture_mics /data/local/bin/bf_capture"
echo ""
echo "Run:"
echo "  adb shell /data/local/bin/bf_capture --angle 330 --seconds 5"
echo "  adb pull /tmp/ ."
echo "  adb shell start mixer && adb shell start echomuse"
echo ""
echo "Without adb, push over the controller shell plane instead:"
echo "  python controller/tools/push_file.py <local> <remote>   (resumable)"
echo ""
echo "slprobe (Echo Show, crown; see tools/README.md):"
echo "  adb push $BUILD_DIR/slprobe /data/local/tmp/slprobe"
echo "  adb shell /data/local/tmp/slprobe -p vr -d 10 -o /data/local/tmp/cap"
echo ""
echo "aaprobe (LineageOS 18.1 / Android 11, crown; see tools/README.md):"
echo "  adb push $BUILD_DIR/aaprobe /data/local/tmp/aaprobe"
echo "  adb shell /data/local/tmp/aaprobe -p vr -m -d 10 -o /data/local/tmp/aa"
echo ""
echo "sl_latency (Echo Show, crown; see tools/README.md):"
echo "  adb shell stop echomuse"
echo "  adb push $BUILD_DIR/sl_latency /data/local/tmp/sl_latency"
echo "  adb shell /data/local/tmp/sl_latency -n 10 -o /data/local/tmp/echomuse/lat"
