# check=skip=FromPlatformFlagConstDisallowed
#
# The Rust controller's image. The build context is the repository root:
#
#   docker build -f controller-rs/Dockerfile -t echomuse-controller-rs .
#
# Published for linux/amd64 and linux/arm64. Everything the image downloads is checked by
# SHA-256 against a pin in the repository. The controller is compiled on the build
# machine for the target architecture (natively, or cross-compiled for the other one);
# only the final stage runs as the target architecture.
#
# Image layout (WORKDIR /app); the environment defaults below match the config crate's:
#
#   /app/bin/echomuse-controller   the controller, run path $ORIGIN
#   /app/bin/libsherpa-onnx-c-api.so, libonnxruntime.so
#                                  sherpa-onnx 1.13.8 from the pinned sherpa-onnx-core wheel
#   /app/models/onnxruntime/libonnxruntime.so.1.20.1   ONNXRUNTIME_LIB
#   /app/models/dtln/              NS_MODEL_DIR
#   /app/models/ort_android/libonnxruntime.so          ORT_ANDROID_LIB, served to Dots
#   /app/speech/                   SPEECH_BUNDLE_DIR
#   /app/firmware/                 FIRMWARE_DIR: server and version
#   /app/static/                   the compiled dashboard
#   /app/bcresnet_audio.onnx, bcresnet_audio.json   the wake model registry seed
#   /app/device_payloads/          start_server.sh and the Echo Show package lists
#   /app/data/                     where DB_PATH usually points, on a volume

# ── Device firmware ──────────────────────────────────────────────────────────
#
# The compiler, companion and firmware stages are controller/Dockerfile's, unchanged, so
# both images built from one tree carry the same firmware bytes' version (the fw- hash of
# the sources). Change them there first and copy them here. The full rationale for each
# pin is in controller/Dockerfile; in short:
#
# The compiler base is pinned by DIGEST, and must stay that way: it carries the Go
# toolchain (1.24.0) and the NDK (21.4.7075529) that compile the firmware for Fire OS 6,
# a platform that cannot be upgraded. Moving the pin needs a real device in the loop.
# linux/amd64 whatever the target platform: the NDK ships only linux-x86_64 binaries, so a
# multi-platform build compiles the firmware once and every image carries that same ARMv7
# binary.
FROM --platform=linux/amd64 ghcr.io/binozo/echogo@sha256:c9456551c639290b1cc3e1df891177beadb5d0d7621a5c2ec81079a276019147 AS compiler

ENV API=22
ENV CC=/opt/android/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi22-clang
ENV CXX=/opt/android/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi22-clang++
ENV AS=/opt/android/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/bin/armv7a-linux-androideabi22-clang
ENV NDK_LIBS=/opt/android/ndk/21.4.7075529/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/arm-linux-androideabi/22
ENV CGO_LDFLAGS="-Wl,--hash-style=both"

# Recompile tinyalsa for API 22 (the firmware's target; runs on Fire OS 6's API 25)
WORKDIR /tinyalsa/src
RUN make clean && \
    make CC=$CC LD=$LD AR=$AR AS=$AS libtinyalsa.a libtinyalsa.so && \
    cp /tinyalsa/src/libtinyalsa.so $NDK_LIBS/libtinyalsa.so

WORKDIR /sdk

# The companion app for the Echo Show on LineageOS (docs/echo-show-companion-plan.md),
# embedded in the firmware. The JDK image is pinned by digest and the two SDK zips by
# SHA-256; no Gradle or AndroidX.
FROM --platform=linux/amd64 eclipse-temurin:17-jdk@sha256:fe947d993c32dd9cf651a4353a6ed723c2370b3a9d034ea4cab4775404ea531a AS companion
WORKDIR /android
RUN set -e; \
    curl -sfL -o build-tools.zip https://dl.google.com/android/repository/build-tools_r30.0.3-linux.zip; \
    echo "24593500aa95d2f99fb4f10658aae7e65cb519be6cd33fa164f15f27f3c4a2d6  build-tools.zip" | sha256sum -c -; \
    curl -sfL -o platform.zip https://dl.google.com/android/repository/platform-30_r03.zip; \
    echo "f3f5b75744dbf6ee6ed3e8174a71e513bfee502d0bc3463ea97e517bff68d84e  platform.zip" | sha256sum -c -; \
    jar xf build-tools.zip android-11/aapt2 android-11/zipalign android-11/lib64/libc++.so \
      android-11/lib/d8.jar android-11/lib/apksigner.jar; \
    mv android-11 build-tools; \
    chmod +x build-tools/aapt2 build-tools/zipalign; \
    jar xf platform.zip android-11/android.jar; \
    mv android-11/android.jar .; \
    rmdir android-11; \
    rm build-tools.zip platform.zip

# The app's version is `cmp-` plus a hash of every file under device/companion; its pure
# Java tests run first and fail the image; the signing key is generated per build.
WORKDIR /build
COPY device/companion/ ./companion/
RUN set -e; \
    VERSION="cmp-$(find companion -type f -print0 \
      | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-12)"; \
    BT=/android/build-tools; \
    SDK=/android/android.jar; \
    javac --release 8 -encoding UTF-8 -d test-classes \
      $(find companion/core companion/ksplugin/core companion/test -name '*.java'); \
    java -cp test-classes org.echomuse.companion.LinkTest; \
    java -cp test-classes org.echomuse.ksplugin.TurnViewTest; \
    java -cp test-classes org.echomuse.ksplugin.TimerViewTest; \
    $BT/aapt2 compile --dir companion/res -o res.zip; \
    $BT/aapt2 link -o unsigned.apk -I $SDK --manifest companion/AndroidManifest.xml \
      --java gen --version-name "$VERSION" --version-code 1 res.zip; \
    javac --release 8 -encoding UTF-8 -classpath $SDK -d classes \
      $(find companion/core companion/app gen -name '*.java'); \
    java -cp $BT/lib/d8.jar com.android.tools.r8.D8 --release --min-api 30 --lib $SDK \
      --output . $(find classes -name '*.class'); \
    jar -u -M -f unsigned.apk classes.dex; \
    $BT/zipalign -p 4 unsigned.apk aligned.apk; \
    keytool -genkeypair -keystore signing.p12 -storetype PKCS12 -storepass echomuse \
      -alias companion -keyalg RSA -keysize 2048 -validity 36500 -dname "CN=EchoMuse companion"; \
    mkdir -p /out; \
    java -jar $BT/lib/apksigner.jar sign --ks signing.p12 --ks-pass pass:echomuse \
      --ks-key-alias companion --v1-signing-enabled false --v2-signing-enabled true \
      --v3-signing-enabled true --v4-signing-enabled false --out /out/companion.apk aligned.apk; \
    rm signing.p12; \
    java -jar $BT/lib/apksigner.jar verify --min-sdk-version 30 /out/companion.apk; \
    printf '%s\n' "$VERSION" > /out/companion_version

# The Kiosk Satellite plugin (docs/echo-show-companion-plan.md §11.4), with reproducible
# bytes so an unchanged plugin is never reinstalled.
RUN set -e; \
    VERSION="1.0.0-$(sed 's/^cmp-//' /out/companion_version)"; \
    SDK=/android/android.jar; \
    DATE=1980-01-01T00:00:02Z; \
    mkdir -p ks/gen/org/echomuse/ksplugin ks/zip; \
    printf 'package org.echomuse.ksplugin;\n\n/** Generated by the image build. */\nfinal class BuildInfo {\n    static final String VERSION = "%s";\n\n    private BuildInfo() {}\n}\n' \
      "$VERSION" > ks/gen/org/echomuse/ksplugin/BuildInfo.java; \
    javac --release 8 -encoding UTF-8 -classpath $SDK -d ks/sdk \
      $(find companion/kssdk -name '*.java'); \
    javac --release 8 -encoding UTF-8 -classpath $SDK:ks/sdk -sourcepath companion/core -d ks/classes \
      $(find companion/ksplugin ks/gen -name '*.java' | LC_ALL=C sort); \
    java -cp /android/build-tools/lib/d8.jar com.android.tools.r8.D8 --release --min-api 30 --lib $SDK \
      --classpath ks/sdk --output ks $(find ks/classes -name '*.class' | LC_ALL=C sort); \
    (cd ks && jar --date=$DATE -c -M -f zip/plugin.jar classes.dex); \
    sed "s/\"1\.0\.0-dev\"/\"$VERSION\"/" companion/ksplugin/kiosk-satellite-plugin.json \
      > ks/zip/kiosk-satellite-plugin.json; \
    grep -q "\"version\": \"$VERSION\"" ks/zip/kiosk-satellite-plugin.json; \
    cp companion/ksplugin/LICENSE ks/zip/; \
    (cd ks/zip && jar --date=$DATE -c -M -f /out/ksplugin.zip kiosk-satellite-plugin.json plugin.jar LICENSE); \
    SIZE=$(stat -c %s /out/ksplugin.zip); \
    if [ "$SIZE" -gt 4194304 ]; then echo "ksplugin.zip is $SIZE bytes, over Kiosk Satellite's 4 MB" >&2; exit 1; fi; \
    echo "ksplugin.zip: plugin $VERSION, $SIZE bytes, sha256 $(sha256sum /out/ksplugin.zip | cut -c1-64)"

# The firmware this controller installs. Its version is `fw-` plus a hash of its own
# sources (Go tests are kept out of the context), so it moves exactly when the firmware
# does; the built app, its version and the plugin ZIP are not part of the hash.
FROM compiler AS firmware
COPY device/go.mod device/go.sum ./
RUN go mod download
COPY device/cmd/ ./cmd/
COPY device/internal/ ./internal/
COPY device/pkg/ ./pkg/
COPY device/companion/ ./companion/
COPY --from=companion /out/companion.apk ./internal/companion/app.apk
COPY --from=companion /out/companion_version ./internal/companion/app.version
COPY --from=companion /out/ksplugin.zip ./internal/companion/ksplugin.zip
RUN set -e; \
    VERSION="fw-$(find go.mod go.sum cmd internal pkg companion -type f \
      ! -path 'internal/companion/app.*' ! -path 'internal/companion/ksplugin.zip' -print0 \
      | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -c1-12)"; \
    mkdir -p /out; \
    CGO_CFLAGS="-Wno-deprecated-declarations -Wno-null-dereference" go build \
      -tags server \
      -ldflags "-X github.com/wilbowes/EchoMuse/internal/client.Version=${VERSION} \
                -X github.com/wilbowes/EchoMuse/internal/client.BuildUnix=$(date +%s)" \
      -o /out/server ./cmd/; \
    printf '%s\n' "$VERSION" > /out/version

# ── Speech assets ────────────────────────────────────────────────────────────
#
# Kroko and Silero by controller/speech_bundle.json, DTLN by crates/speech/src/pins.rs,
# and the ONNX Runtime library served to Dots (armeabi-v7a 1.19.2). None of it depends on
# the target architecture, so this runs once on the build machine.
FROM --platform=$BUILDPLATFORM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS speech
RUN apt-get update && \
    apt-get install -y --no-install-recommends bzip2 ca-certificates curl jq unzip && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY controller/speech_bundle.json ./controller/speech_bundle.json
COPY controller-rs/crates/speech/src/pins.rs ./controller-rs/crates/speech/src/pins.rs
COPY controller-rs/tools/ ./controller-rs/tools/
RUN controller-rs/tools/fetch-speech-assets.sh /out && rm -rf /out/.downloads

# ── Dashboard ────────────────────────────────────────────────────────────────
#
# controller/static with its vendored libraries and fonts, dashboard.jsx compiled by
# esbuild 0.20.2, every download pinned by SHA-256.
FROM --platform=$BUILDPLATFORM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3 AS dashboard
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl && \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY controller/static/ ./controller/static/
COPY controller-rs/crates/speech/src/pins.rs ./controller-rs/crates/speech/src/pins.rs
COPY controller-rs/tools/ ./controller-rs/tools/
RUN controller-rs/tools/build-dashboard.sh /out/static

# ── Controller ───────────────────────────────────────────────────────────────
#
# Rust 1.99.1, the toolchain controller-rs/rust-toolchain.toml names (move both together),
# pinned by the digest of its multi-architecture index. It runs on the build machine and
# cross-compiles when the target architecture differs, which is far faster than building
# under emulation. The sherpa-onnx libraries it links are the pinned wheel's for the
# target architecture (tools/fetch-native.sh), and the binary gets a run path of $ORIGIN
# from build.rs, so it finds them beside it in /app/bin.
FROM --platform=$BUILDPLATFORM rust:1.99.1-slim-bookworm@sha256:24707e70a4f1e23d9a7b25202ec43126d28a52294b96aad5c670df1fd1f1d400 AS controller
ARG TARGETARCH
ARG BUILDARCH
RUN set -e; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl unzip; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      case "$TARGETARCH" in \
        arm64) apt-get install -y --no-install-recommends \
                 gcc-aarch64-linux-gnu libc6-dev-arm64-cross libstdc++6-arm64-cross ;; \
        amd64) apt-get install -y --no-install-recommends \
                 gcc-x86-64-linux-gnu libc6-dev-amd64-cross libstdc++6-amd64-cross ;; \
        *) echo "no controller build for $TARGETARCH" >&2; exit 1 ;; \
      esac; \
    fi; \
    rm -rf /var/lib/apt/lists/*; \
    case "$TARGETARCH" in \
      amd64) echo x86_64-unknown-linux-gnu ;; \
      arm64) echo aarch64-unknown-linux-gnu ;; \
      *) echo "no controller build for $TARGETARCH" >&2; exit 1 ;; \
    esac > /rust-target; \
    rustup target add "$(cat /rust-target)"

WORKDIR /src/controller-rs
COPY controller-rs/tools/ ./tools/
COPY controller-rs/crates/speech/src/pins.rs ./crates/speech/src/pins.rs
RUN tools/fetch-native.sh --arch "$TARGETARCH" /native && rm -rf /native/.downloads
ENV SHERPA_ONNX_LIB_DIR=/native/sherpa-onnx/lib

COPY controller-rs/Cargo.toml controller-rs/Cargo.lock controller-rs/build.rs ./
COPY controller-rs/.cargo/ ./.cargo/
COPY controller-rs/src/ ./src/
COPY controller-rs/crates/ ./crates/
# One target cache for every architecture: with --target, each architecture's output has
# its own directory, and `locked` keeps two platform builds from sharing it at once.
RUN --mount=type=cache,id=echomuse-rs-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=echomuse-rs-target,target=/src/controller-rs/target,sharing=locked \
    set -e; \
    TARGET="$(cat /rust-target)"; \
    if [ "$TARGETARCH" != "$BUILDARCH" ]; then \
      case "$TARGETARCH" in \
        arm64) export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
                      CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
                      AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar ;; \
        amd64) export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
                      CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc \
                      AR_x86_64_unknown_linux_gnu=x86_64-linux-gnu-ar ;; \
      esac; \
    fi; \
    cargo build --release --locked --target "$TARGET"; \
    mkdir -p /out/bin /out/models/onnxruntime; \
    cp "target/$TARGET/release/echomuse-controller" /out/bin/; \
    cp /native/sherpa-onnx/lib/libsherpa-onnx-c-api.so /native/sherpa-onnx/lib/libonnxruntime.so /out/bin/; \
    cp /native/onnxruntime/libonnxruntime.so.1.20.1 /out/models/onnxruntime/

# ── Image ────────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
LABEL org.opencontainers.image.source="https://github.com/wilbowes/EchoMuse" \
      org.opencontainers.image.description="EchoMuse controller (Rust) — open-source voice assistant satellites on Amazon Echo devices"
# ffmpeg decodes media for playback; ca-certificates are the roots for HTTPS to Home
# Assistant; libstdc++ is sherpa-onnx's.
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates ffmpeg libstdc++6 && \
    rm -rf /var/lib/apt/lists/*

# The image runs as root by default because existing installations' data (the Python
# controller's volume, the add-on's /data) is owned by root and must be opened in place.
# A new installation can run as `echomuse` (`--user echomuse`); a new named volume at
# /app/data takes that user's ownership from the image.
RUN useradd --system --user-group --uid 10001 --home-dir /app --no-create-home \
      --shell /usr/sbin/nologin echomuse && \
    mkdir -p /app/data && chown echomuse:echomuse /app/data
WORKDIR /app

COPY --from=speech /out/ /app/
COPY --from=dashboard /out/static/ /app/static/
COPY controller/device_payloads/ /app/device_payloads/
# The deployed wake graph and sidecar: the registry seed.
COPY bcresnet_audio.onnx bcresnet_audio.json /app/
COPY --from=firmware /out/ /app/firmware/
COPY --from=controller /out/ /app/

# The binary must load sherpa-onnx from beside itself, with no LD_LIBRARY_PATH.
RUN ldd /app/bin/echomuse-controller | grep -q 'libsherpa-onnx-c-api.so => /app/bin/' && \
    /app/bin/echomuse-controller --help >/dev/null

ENV FIRMWARE_DIR=/app/firmware \
    SPEECH_BUNDLE_DIR=/app/speech \
    ORT_ANDROID_LIB=/app/models/ort_android/libonnxruntime.so \
    ONNXRUNTIME_LIB=/app/models/onnxruntime/libonnxruntime.so.1.20.1 \
    NS_MODEL_DIR=/app/models/dtln
# The release version, set by the release build; empty means the binary's own version.
ARG EM_CONTROLLER_VERSION=
ENV EM_CONTROLLER_VERSION=$EM_CONTROLLER_VERSION
ENTRYPOINT ["/app/bin/echomuse-controller"]
