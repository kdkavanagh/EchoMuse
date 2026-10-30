# EchoMuse controller (Rust)

The supported EchoMuse controller: one Cargo workspace, the controller binary at the
root and its parts under `crates/`. The Python implementation in `controller/` remains
for historical reference; normal builds, CI and releases use Rust. Current behavior
and verification evidence are in
[`docs/rust-controller-evaluation.md`](../docs/rust-controller-evaluation.md) §9.

The voice crate keeps wake handling, conversation handling and playback in private
components. `Voice` coordinates them synchronously on the existing device executor;
`SessionAudio` provides their shared audio mechanisms. The boundaries and transition
invariants are in [`docs/rust-voice-decomposition.md`](../docs/rust-voice-decomposition.md).

Run every command below from `controller-rs/`. Cargo reads `.cargo/config.toml` only
there, and that file is what points the build at the fetched native libraries.

## What you need

- `rustup`; the toolchain comes from `rust-toolchain.toml` (Rust 1.99.1).
- `curl`, `sha256sum`, `tar`, `bzip2`, `unzip` and `jq` for the fetch scripts.
- Docker with BuildKit, for the image and for `device/compile.sh`.

## 1. Fetch the native libraries

```sh
tools/fetch-native.sh
```

This downloads two archives, checks each against the SHA-256 recorded in
`crates/speech/src/pins.rs` (the script refuses to run if its hashes and that file
disagree), and unpacks:

| Path | What it is |
|---|---|
| `native/sherpa-onnx/lib/libsherpa-onnx-c-api.so`, `libonnxruntime.so` | sherpa-onnx 1.13.8 from the `sherpa-onnx-core` wheel. Kroko ASR runs on these. |
| `native/onnxruntime/libonnxruntime.so.1.20.1` | the official ONNX Runtime 1.20.1 release. BCResNet, Silero and DTLN run on it; `ONNXRUNTIME_LIB` names it at run time. |

The script fetches libraries for the machine it runs on. `--arch arm64` or `--arch amd64`
picks the other architecture, and a second argument puts them in another directory. Run
it again at any time: it keeps verified files and replaces anything else. `native/` is
gitignored.

`.cargo/config.toml` sets `SHERPA_ONNX_LIB_DIR` to `native/sherpa-onnx/lib`, so
`sherpa-onnx-sys` links against these libraries. Without that variable it downloads a
different sherpa-onnx build whose per-token probabilities differ, and the controller
rejects that build at startup. A `SHERPA_ONNX_LIB_DIR` already set in your environment
takes precedence over the file.

Without the fetch, the build stops. `cargo build` of the controller prints
`... native/sherpa-onnx/lib/libsherpa-onnx-c-api.so is missing` followed by the command
to run. A build of `echomuse-speech` alone stops in `sherpa-onnx-sys` with
`SHERPA_ONNX_LIB_DIR does not exist or is not a directory: .../native/sherpa-onnx/lib`.
In both cases, run `tools/fetch-native.sh`.

## 2. Build and test

```sh
cargo build --workspace
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo build --release
```

`cargo test` and `cargo run` find the sherpa-onnx libraries on their own, because
`sherpa-onnx-sys` copies them into the target directory. The release binary needs no
`LD_LIBRARY_PATH`: `build.rs` gives it a run path of `$ORIGIN`, so it loads
`libsherpa-onnx-c-api.so` and `libonnxruntime.so` from its own directory, where
`sherpa-onnx-sys` has already copied them (`target/release/`). To run a copy of the binary
somewhere else, copy those two libraries beside it. You can check what it resolves with:

```sh
ldd target/release/echomuse-controller | grep -E 'sherpa|onnxruntime'
```

### Model tests

The tests in `crates/speech/tests/models.rs` check the real models against their
references. They are `#[ignore]`d, because they need the model files, and they find
them through these variables:

```sh
tools/fetch-speech-assets.sh     # into local/ (see below)
export ECHOMUSE_ORT_LIB=$PWD/native/onnxruntime/libonnxruntime.so.1.20.1
export ECHOMUSE_KROKO_DIR=$PWD/local/speech/kroko
export ECHOMUSE_SILERO_VAD=$PWD/local/speech/silero_vad_v5.onnx
export ECHOMUSE_DTLN_DIR=$PWD/local/models/dtln
cargo test -p echomuse-speech --release --test models -- --ignored
```

A test run with `--ignored` but without one of these variables fails and names the missing
variable.

The voice crate's speech workers have their own model tests, with the same variables:

```sh
cargo test -p echomuse-voice --release --test models -- --ignored
```

They run microphone analysis, wake verification and the STT copy over the Kroko test
recording. Set `ECHOMUSE_WAKE_WAV` to a saved wake clip (a 16 kHz mono WAV, read in place)
to also check that the verifier hears the wake word in it; without it that case says it
was skipped. Never commit such a clip.

## 3. Run locally against a copy of a data directory

Never point a development controller at a live database. Work on a copy.

```sh
tools/fetch-speech-assets.sh     # local/speech, local/models/dtln, local/models/ort_android
tools/build-dashboard.sh         # local/static
../device/compile.sh             # ../device/build/server and version (needs Docker)
cargo build --release
ln -sf ../../bcresnet_audio.onnx ../../bcresnet_audio.json local/
```

`fetch-speech-assets.sh` writes the same layout the image has under `/app`:

| Path | Setting | What it is |
|---|---|---|
| `local/speech/` | `SPEECH_BUNDLE_DIR` | Kroko ASR (`kroko/`, with its `test_wavs/0.wav`), `silero_vad_v5.onnx`, the Kroko attribution and a copy of `controller/speech_bundle.json`, each checked against that manifest |
| `local/models/dtln/` | `NS_MODEL_DIR` | DTLN `model_1.onnx`, `model_2.onnx` |
| `local/models/ort_android/libonnxruntime.so` | `ORT_ANDROID_LIB` | ONNX Runtime 1.19.2 armeabi-v7a, which Dots download from the controller |

Copy the data directory into `local/data`. Take the database with SQLite's backup
command, so the copy is consistent even while the source is in use. The other files
(`tls/`, `oww_models/`, `sounds/`, recordings) can be copied as they are. A Docker
installation's files are usually owned by root, so copy them with `sudo` and take
ownership afterwards:

```sh
SRC=/path/to/echomuse/data
mkdir -p local/data
sudo rsync -a --exclude 'echomuse.db*' "$SRC/" local/data/
sudo sqlite3 "$SRC/echomuse.db" ".backup '$PWD/local/data/echomuse.db'"
sudo chown -R "$USER:" local/data
```

Then run the controller from `local/`. It finds `static/` and the wake model seed relative
to its working directory:

```sh
cd local
DB_PATH=$PWD/data/echomuse.db \
FIRMWARE_DIR=$PWD/../../device/build \
DEVICE_PAYLOADS_DIR=$PWD/../../controller/device_payloads \
SPEECH_BUNDLE_DIR=$PWD/speech \
NS_MODEL_DIR=$PWD/models/dtln \
ORT_ANDROID_LIB=$PWD/models/ort_android/libonnxruntime.so \
ONNXRUNTIME_LIB=$PWD/../native/onnxruntime/libonnxruntime.so.1.20.1 \
HA_URL=http://homeassistant.local:8123 HA_TOKEN=... \
SERVER_IP=192.168.1.20 \
../target/release/echomuse-controller
```

`--help` lists what it does. The other settings (ports, `MDNS_NAME`, `DEVICE_APPROVAL`,
...) are in docs/rust-controller-evaluation.md §3.6. Before its first write the controller
saves `echomuse.db.pre-rust.bak` beside the copied database.

The copy holds the real devices' tokens, and the controller announces itself over mDNS and
acts on the Home Assistant it is given. Only one controller may drive an installation's
devices and Home Assistant at a time, so either stop the production controller first, or
use a test Home Assistant on a network the devices cannot reach.

The installation now runs Rust by default after the supervised real-device checks in
[`docs/rust-controller-evaluation.md` §9.4](../docs/rust-controller-evaluation.md#94-first-supervised-live-trial-2026-10-30).
Keep the current data directory, TLS credentials and device firmware across rebuilds.
The saved Python image is only an emergency recovery option; pending alarm rows may
need manual recovery as recorded there. No normal startup path selects Python.

## 4. Build the image

The build context is the repository root, since the image also builds the device firmware
and takes the dashboard, the device payloads and the wake model seed from outside
`controller-rs/`. `controller-rs/Dockerfile.dockerignore` limits what is sent.

```sh
cd ..    # the repository root
docker build -f controller-rs/Dockerfile -t echomuse-controller-rs .
```

For both published architectures:

```sh
docker buildx build --platform linux/amd64,linux/arm64 -f controller-rs/Dockerfile .
```

The firmware stages always run as `linux/amd64` (the Android NDK is x86-64 only), and the
controller is cross-compiled on the build machine. Only the final stage runs as the target
architecture: it installs ffmpeg and checks that the binary loads sherpa-onnx from beside
itself. On an x86-64 machine, building `linux/arm64` needs QEMU registered with binfmt for
that stage.

Run it with host networking, because devices find the controller over mDNS and each device
gets its own ESPHome port:

```sh
docker run -d --name echomuse --network host \
  -v /path/to/echomuse/data:/app/data \
  -e DB_PATH=/app/data/echomuse.db \
  -e HA_URL=http://homeassistant.local:8123 -e HA_TOKEN=... \
  echomuse-controller-rs
```

The image runs as root, because existing installations keep their data in root-owned
files that must be opened where they are. A new installation can run as the image's
`echomuse` user (`--user echomuse`), with a fresh named volume at `/app/data` or a
directory owned by uid 10001.

What the image holds:

| Path | Setting |
|---|---|
| `/app/bin/echomuse-controller` and its two sherpa-onnx libraries | |
| `/app/models/onnxruntime/libonnxruntime.so.1.20.1` | `ONNXRUNTIME_LIB` |
| `/app/models/dtln/` | `NS_MODEL_DIR` |
| `/app/models/ort_android/libonnxruntime.so` | `ORT_ANDROID_LIB` |
| `/app/speech/` | `SPEECH_BUNDLE_DIR` |
| `/app/firmware/server`, `version` | `FIRMWARE_DIR` |
| `/app/static/` | the dashboard |
| `/app/bcresnet_audio.onnx`, `.json` | the wake model registry seed |
| `/app/device_payloads/` | `start_server.sh` and the Echo Show package lists |

`EM_CONTROLLER_VERSION` is set with `--build-arg EM_CONTROLLER_VERSION=...`; without it
the controller reports the version it was built as.

## 5. Check a copied installation

`tools/data-copy-check.sh` checks that this controller keeps a real installation as it is
(docs/rust-controller-evaluation.md §3.5 and §5.6, "Database and file preservation"). It
never writes to the data directory it is given:

```sh
tools/data-copy-check.sh /path/to/echomuse/data
```

It needs Docker, the native libraries, speech assets and dashboard from §1 and §3, and
`sqlite3`, `jq`, `curl`, `openssl` and `ffmpeg` on this machine. In order, it:

1. builds the release binary, the voice tests and this tree's image (`--skip-build` and
   `--skip-image-build` reuse what is built);
2. copies the data directory into a private scratch directory under `/var/tmp` (the
   database through SQLite's backup API on a read-only mount, so a live installation can
   stay up) and adds one dashboard session for its administrator to the copy;
3. runs the release binary on the copy with the real speech bundle and a simulated Home
   Assistant. The ignored test `data_copy_check` (`tests/data_copy/check.rs`, inside
   `tests/voice.rs`) checks every dashboard read over the stored rows and files (devices,
   turns, near misses, traces, every recording byte for byte, archives, wake models,
   sounds, alerts, settings), drives a simulated speaker with a copied Echo Dot's id and
   stored token through a wake turn on one of the copy's own utterance recordings, and
   then compares every row the controller added or changed with the stored rows of its
   table, column by column (storage type, NULLs, JSON paths, numeric range). Any stored row
   that disappears fails the run. The script also checks the certificate the device TLS
   port serves and that `echomuse.db.pre-rust.bak` holds the copy as it was;
4. restarts the binary and checks that the backup is not rewritten;
5. if `ROLLBACK_IMAGE` explicitly names a previous image, stops Rust and starts that
   image on the same copy as a black box: it must start, make no migration, keep
   `schema_version` 40 and list the new turn. Its errors, tracebacks and migration
   lines are printed verbatim (`--skip-rollback` skips this). No previous image is
   selected by default, and this check does not require a Python runtime;
6. runs this tree's image the same way as step 3 on a second copy;
7. deletes the scratch directory, which holds private data (`--keep` keeps it, and says
   where it is).

The copy holds the real devices' tokens, and a controller announces itself over mDNS and
dials Home Assistant, so every process that touches the copy runs inside one Docker network
namespace made with `--network none`: only a loopback interface and no route. Programs
built on this machine run in it through a chroot into a read-only view of the machine's
root, as the invoking user, with every capability dropped except what chroot needs, and
with `/run` (and the Docker socket in it) hidden. The script prints the namespace's
interfaces, routes and a refused connection before anything touches the copy, and stops if
it finds another interface or a route. `ECHOMUSE_DCC_TURN=<device id>/<turn id>` picks the
stored wake turn that is played; without it the newest suitable one is used.

The wake turn runs in real time and its speech jobs have deadlines, so run the check on a
machine that is not saturated: with a load average near 50 on 16 cores the turn ended
`interrupted` (`queued operation expired`) and the run failed.
