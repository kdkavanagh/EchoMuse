FROM python:3.12-slim
# Links the GHCR package page back to the repo (and lets the package inherit
# the repo's public visibility once connected).
LABEL org.opencontainers.image.source="https://github.com/wilbowes/EchoMuse" \
      org.opencontainers.image.description="EchoMuse controller — Echo Dot Gen 2 open-source voice assistant satellite"
WORKDIR /app
# Install curl for vendoring JS dependencies
RUN apt-get update && apt-get install -y --no-install-recommends curl ffmpeg && rm -rf /var/lib/apt/lists/*
# Install Python dependencies
COPY controller/requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
# GPU=1 swaps the CPU onnxruntime from requirements.txt for the CUDA build
# (both provide the `onnxruntime` import, so they cannot coexist). Local
# docker-compose.yml sets it; the published image is CPU-only. The GPU package
# has its own version line (no 1.20.1), so the pins differ deliberately.
ARG GPU=0
RUN if [ "$GPU" = "1" ]; then \
      pip uninstall -y onnxruntime && \
      pip install --no-cache-dir onnxruntime-gpu==1.20.2; \
    fi
# Vendor JS dependencies — served locally, no CDN needed at runtime
RUN mkdir -p /app/static/vendor/fonts && \
    curl -sfL -o /app/static/vendor/react.min.js \
      https://cdnjs.cloudflare.com/ajax/libs/react/18.2.0/umd/react.production.min.js && \
    curl -sfL -o /app/static/vendor/react-dom.min.js \
      https://cdnjs.cloudflare.com/ajax/libs/react-dom/18.2.0/umd/react-dom.production.min.js && \
    curl -sfL -o /app/static/vendor/xterm.js \
      https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js && \
    curl -sfL -o /app/static/vendor/xterm.css \
      https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css && \
    curl -sfL -o /app/static/vendor/addon-fit.js \
      https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js && \
    curl -sfL -A "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
      -o /app/static/vendor/fonts/dm-sans.woff2 \
      "https://fonts.gstatic.com/s/dmsans/v17/rP2Yp2ywxg089UriI5-g4vlH9VoD8Cmcqbu0-K4.woff2" && \
    curl -sfL -A "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
      -o /app/static/vendor/fonts/dm-mono-400.woff2 \
      "https://fonts.gstatic.com/s/dmmono/v16/aFTR7PB1QTsUX8KYvumzEYOtbQ.woff2" && \
    curl -sfL -A "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36" \
      -o /app/static/vendor/fonts/dm-mono-500.woff2 \
      "https://fonts.gstatic.com/s/dmmono/v16/aFTU7PB1QTsUX8KYthqQBA.woff2"
# Controller version — baked in by the controller-release workflow from the
# controller-v* tag; "dev" for local builds (version.py falls back to git
# describe only outside a container).
ARG EM_CONTROLLER_VERSION=dev
ENV EM_CONTROLLER_VERSION=$EM_CONTROLLER_VERSION
# Controller source. The root build context is required for the deployed
# BCResNet registry seed below; Compose and the release workflow name this
# Dockerfile explicitly.
COPY controller/em_*.py ./
COPY controller/version.py .
COPY controller/esphome/ ./esphome/
COPY controller/device_payloads/ ./device_payloads/
COPY controller/echomuse_grammar/ ./echomuse_grammar/
COPY controller/speech_bundle.json .
COPY controller/tools/fetch_speech_bundle.py ./tools/fetch_speech_bundle.py
# Verified deployed graph + sidecar: the registry seed (§5.1, §18.5).
COPY bcresnet_audio.onnx bcresnet_audio.json ./

# DTLN noise-suppression models (em_ns.py) — vendored at build time like the
# JS deps above; pinned to a commit so builds are reproducible. MIT licensed.
RUN mkdir -p /app/models/dtln && \
    curl -sfL -o /app/models/dtln/model_1.onnx \
      https://raw.githubusercontent.com/breizhn/DTLN/1de1f15a8b5b7e1c44905618ff2ef70ca8277fbc/pretrained_model/model_1.onnx && \
    curl -sfL -o /app/models/dtln/model_2.onnx \
      https://raw.githubusercontent.com/breizhn/DTLN/1de1f15a8b5b7e1c44905618ff2ef70ca8277fbc/pretrained_model/model_2.onnx

# Device ONNX Runtime (armeabi-v7a 1.19.2, §16.5): served to devices as a
# hash-named speech asset over /device/v1/assets, never loaded here. Pinned by
# the AAR's SHA-256; em_device_assets re-verifies the extracted library
# (174233cf…) at startup. MIT licensed. Extracted with zipfile (no unzip in
# the slim base).
RUN mkdir -p /app/models/ort_android && \
    curl -sfL -o /tmp/ort.aar \
      https://repo1.maven.org/maven2/com/microsoft/onnxruntime/onnxruntime-android/1.19.2/onnxruntime-android-1.19.2.aar && \
    echo "84737dd1fdb715f0ee695c0a9864f6949b66ecb4c46ca733bc9211bf0c2a7b5f  /tmp/ort.aar" | sha256sum -c - && \
    python -c "import zipfile, shutil; z = zipfile.ZipFile('/tmp/ort.aar'); shutil.copyfileobj(z.open('jni/armeabi-v7a/libonnxruntime.so'), open('/app/models/ort_android/libonnxruntime.so', 'wb'))" && \
    rm /tmp/ort.aar

# Hash-pinned §16.6 controller speech bundle: Kroko streaming ASR (CC-BY-SA),
# Silero v5 (MIT), and the sherpa-onnx wheel (Apache-2.0). The fetcher verifies
# every download and writes Kroko attribution linking
# https://huggingface.co/Banafo/Kroko-ASR into /app/speech.
RUN python ./tools/fetch_speech_bundle.py /app/speech

# Dashboard SPA — placeholder is index.html, full dashboard is dashboard.html
COPY controller/static/ ./static/
# Compile JSX dashboard with esbuild
RUN EARCH="$(dpkg --print-architecture)" && case "$EARCH" in amd64) EPKG=linux-x64 ;; arm64) EPKG=linux-arm64 ;; *) echo "unsupported arch: $EARCH" >&2; exit 1 ;; esac && curl -sfL -o /tmp/esbuild.tar.gz "https://registry.npmjs.org/@esbuild/${EPKG}/-/${EPKG}-0.20.2.tgz" &&     tar -xz -C /tmp -f /tmp/esbuild.tar.gz &&     install -m755 /tmp/package/bin/esbuild /usr/local/bin/esbuild &&     esbuild /app/static/dashboard.jsx       --bundle=false       --jsx=transform       --jsx-factory=React.createElement       --jsx-fragment=React.Fragment       --outfile=/app/static/dashboard.js &&     rm -rf /tmp/esbuild.tar.gz /tmp/package /usr/local/bin/esbuild
# Database persisted via volume mount
# docker run -v ~/EchoMuse/controller/data:/app/data ...
# Set DB_PATH=/app/data/echomuse.db in environment
CMD ["python", "-u", "em_start.py"]
