# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

EchoMuse repurposes Amazon Echo Dot Gen 2 (FireOS 5 / Android 5.1, codename "biscuit") as an open-source voice assistant satellite. Two components:

- **`device/`** — Go binary that runs directly on the rooted Echo Dot
- **`controller/`** — Python asyncio server that manages devices over the v1 device link (`docs/protocol-v1.md`), runs each device's session actor and the speech worker (VAD, streaming ASR, echo attribution, endpointing), and drives stock Home Assistant (Assist pipeline runs, native satellite timers, one Local Calendar per speaker for alarms). Wake detection is BCResNet on the Dot; the controller's content-addressed wake-model registry (`em_wake_registry`) names the graph each device loads. BCResNet training lives outside this repo (`~/git/bcresnet`).

The architecture and its normative contracts are `docs/post-afe-audio-architecture.md` (§16 is the most specific; §18 is the replace/keep inventory). Device and controller must agree with it and with `docs/protocol-v1.md` exactly.

## Building the device binary

The Echo Dot runs FireOS 5 (API 22). Standard Go cross-compilation won't work — a custom Docker build environment is required.

**One-time setup:**

```bash
# GoTinyAlsa is a git submodule at the repo root — the wilbowes/GoTinyAlsa
# fork, NOT upstream Binozo: it carries the GetAudioStream defer-in-loop
# leak fix (v2.9.2). Don't repoint it upstream until that fix is merged there.
#
# The FIRMWARE no longer uses it: audio goes through the HAL over OpenSL ES
# (internal/opensl). It is still needed by device/tools/capture_mics and
# bf_capture, which read the raw 9-channel array directly, so the submodule
# stays and compile.sh still mounts it.
git submodule update --init

# Build the compiler Docker image (from device/)
cd device
docker build -t echomuse-compiler compiler/
```

**The compiler base is pinned by DIGEST, and must stay that way.**
`compiler/Dockerfile` carries the Go toolchain (1.24.0) and NDK
(21.4.7075529) that compile the firmware, so it is the layer sitting
directly on top of FireOS 5 — a 2015 platform that cannot be upgraded. It
was `FROM ghcr.io/binozo/echogo:latest`, a third party's floating tag, and
`release.yml` rebuilds the image **from scratch on every tag push**: every
release was free to pick up a different compiler than the last, with no PR
and no CI signal. The first symptom would be a binary the hardware refuses
to run, which is the one failure here not recoverable from the dashboard.

Moving the pin needs **a real device in the loop**. The host tests and
`go vet` cannot speak to it — they run on amd64 with the host toolchain,
and this image is exercised only by `compile.sh` and `release.yml`, so a
green CI run on a pin change proves nothing about it.

**Compile:**

```bash
cd device
./compile.sh
# Output: build/server
```

`compile.sh` embeds the git version string via `-ldflags "-X .../client.Version=..."`. Dirty trees get a `YYYYMMDD-HHMM-dev` timestamp instead of the tag.

**Run Go tests (host):**

```bash
cd device
go test ./...
```

Tests only cover pure-Go logic — hardware-dependent code is not testable on the host.

**Run controller tests (host):**

```bash
cd controller
python -m pytest tests/        # needs: pytest numpy scipy websockets aiohttp — not the full requirements.txt
```

Controller tests cover pure-logic modules plus the device link, legacy handler and HA client, which need `websockets` and `aiohttp` (CI installs exactly `pytest numpy scipy websockets aiohttp`). Tests that load real models (`test_speech_worker`, parts of `test_wake_registry`/`test_wake_scorer`) skip without `onnxruntime`/`sherpa-onnx`. Keep new tests in that shape: evidence fixtures, not live models or a live HA. Both suites (plus `go vet`) run in CI on every push/PR (`.github/workflows/ci.yml`).

**Device/controller compatibility.** The two halves version independently, so any pairing can occur in the field. Guarded by `tests/test_capabilities.py`:

- **Negotiate by capability, not version.** The device announces what it implements in `session.hello` (`device/internal/proto/proto.go`): the eight protocol capabilities of SPEC §11.1 — `audio_timeline_v1`, `uplink_leases_v1`, `device_wake_v1`, `render_reference_v1`, `render_progress_v1`, `focus_leases_v1`, `alert_cache_v1`, `turn_protocol_v1` — plus the retained hardware ones `leds`, `led_anim`, `buttons`, `button_hold`, and `ambient_light` **only when the sensor is actually readable**. The controller admits a v1 device only with all eight (`em_device.REQUIRED_CAPABILITIES`) and reads the rest from `em_device.Device.capabilities` via properties like `led_anim_capable` / `ambient_light_capable`. Never compare version strings — that puts release history in the controller and misjudges dev builds. A UI control whose feature the device lacks is shown **disabled with the reason**, never as a control that silently does nothing.
- **Old firmware gets only the upgrade.** A pre-v1 device reaches the upgrade-only legacy handler on `/control` (`em_legacy`): it registers, is marked `upgrade_required`, and can be updated and shelled into; nothing it reports changes controller state, and it takes no turns, rings no alerts and plays nothing until upgraded.

### Schema migrations

`em_db.MIGRATIONS` is **append-only** — the stored `schema_version` is an
index into it, so appending to a deployed entry corrupts every database that
already ran it. (Doing exactly that once broke every stats write and
disconnect-looped the fleet.)

A controller applies everything it is missing in one startup, so **a user
several releases behind jumping straight to latest is the normal case**. Each
migration is its own transaction including its Python fixup, and a failure
refuses startup rather than running on a half-migrated schema; re-running
resumes from the last committed version.

## Running the controller

**Bare metal (Python 3.12):**

```bash
cd controller
cp .env.example .env   # fill in SERVER_IP, HA_URL, HA_TOKEN
pip install -r requirements.txt
python tools/fetch_speech_bundle.py ./speech   # hash-pinned Kroko ASR + Silero VAD (+ attribution)
SPEECH_BUNDLE_DIR=./speech python em_controller.py
```

The controller refuses to start without Home Assistant credentials, a verified speech bundle (`SPEECH_BUNDLE_DIR`, default `/app/speech`), or the hash-checked armeabi-v7a `libonnxruntime.so` it serves to Dots as a speech asset (`ORT_ANDROID_LIB`, default `/app/models/ort_android/libonnxruntime.so`; the Dockerfile extracts it from the pinned `onnxruntime-android` 1.19.2 AAR). The deployed wake graph (`bcresnet_audio.onnx` + `.json` at the repo root) seeds the registry on first start.

**Docker:**

```bash
cd controller
docker-compose up --build   # build context is the repo root
```

Dashboard available at `http://<SERVER_IP>:8768`. Devices connect to port 8767 (8770 for TLS): `/device/v1/control`, `/device/v1/audio`, `/device/v1/assets`, `/shell`, and the upgrade-only legacy `/control`.

Key env vars in `.env` (see `.env.example` for the full list):

- `SERVER_IP` — LAN IP advertised via mDNS (devices connect here)
- `HA_URL` / `HA_TOKEN` — Home Assistant base URL and an **administrator** long-lived access token (the add-on uses the Supervisor token via `homeassistant_api: true` instead)
- `DEVICE_APPROVAL` — `strict` or `auto`; only a fallback: the database's `device_approval` system setting (created as `strict`, changed with `PATCH /api/system/config`) takes precedence
