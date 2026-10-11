# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

EchoMuse repurposes Amazon Echo Dot Gen 2 (Fire OS 6 / Android 7.1.2, codename "biscuit") as an open-source voice assistant satellite. Two components:

- **`device/`** — Go binary that runs directly on the rooted Echo Dot
- **`controller/`** — Python asyncio server that manages devices over the v1 device link (`docs/protocol-v1.md`), runs each device's session actor and the speech worker (VAD, streaming ASR, echo attribution, endpointing), and drives stock Home Assistant (Assist pipeline runs, native satellite timers, one Local Calendar per speaker for alarms). Wake detection is BCResNet on the Dot; the controller's content-addressed wake-model registry (`em_wake_registry`) names the graph each device loads. BCResNet training lives outside this repo (`~/git/bcresnet`).

`docs/post-afe-audio-architecture.md` describes the current architecture (§16 is the most detailed; §18 is the replace/keep inventory), and `docs/protocol-v1.md` the device link. They document the design; they do not freeze it. When a better design is chosen, change the code and update the documents to match.

**One device image: Fire OS 6.**

Fire OS 6 (API 25, `biscuit_puffin`; amonet-biscuit v2 plus R0rt1z2's boot-root.zip) is headless, with no Android framework and only toybox. Audio goes through Amazon's `mixer` daemon via `libmixerAPI` (`device/internal/mixerapi`); Wi-Fi through Amazon's `wifisvc`. The firmware must run with primary gid `aipc` (2901), or the mixer rejects its streams. The wizard's `/system/etc/init/echomuse.rc` provides that. It uses `seclabel u:r:adbd:s0` because init may not enter `su`, and `start_server.sh` then moves itself into `su`, since `wpa_supplicant` may not reply to `adbd`. `docs/fireos6-port.md` holds the evidence and the service denylist.

Fire OS 5 is not supported: firmware from this tree cannot run audio there, the wizard refuses an Android 5.x Dot at connect, and the controller admits only devices that announce `platform: fireos6`. A Fire OS 5 Dot moves to Fire OS 6 first (`docs/rooting.md`).

## Building the device binary

The Echo Dot runs Fire OS 6 (API 25); the firmware is still built for API 22, which runs there unchanged, and moving the API level is a toolchain change (see the pin below). Standard Go cross-compilation won't work — a custom Docker build environment is required.

**One-time setup:**

```bash
# GoTinyAlsa is a git submodule at the repo root — the wilbowes/GoTinyAlsa
# fork, NOT upstream Binozo: it carries the GetAudioStream defer-in-loop
# leak fix (v2.9.2). Don't repoint it upstream until that fix is merged there.
#
# The FIRMWARE does not use it: audio goes through Amazon's mixer daemon via
# libmixerAPI (internal/mixerapi). It is still needed by device/tools/capture_mics
# and bf_capture, which read the raw 9-channel array directly, so the submodule
# stays and build_tools.sh still mounts it.
git submodule update --init
```

**The firmware is built inside the controller image, and the controller
installs nothing else.** `controller/Dockerfile` has a `compiler` stage (the
toolchain), a `firmware` stage that compiles `device/` into `/out/server`
plus `/out/version`, and the final image copies both to `/app/firmware/`
(`em_firmware`, `FIRMWARE_DIR`). The wizard and every update push exactly
those bytes; there are no GitHub firmware releases and no uploads. The
version is `fw-` plus a hash of the firmware sources (tests are kept out of
the context by `controller/Dockerfile.dockerignore`), so it moves only when
the firmware does, and a controller-only change offers the Dots no update.

**The compiler base is pinned by DIGEST, and must stay that way.**
The `compiler` stage carries the Go toolchain (1.24.0) and NDK
(21.4.7075529) that compile the firmware, so it is the layer sitting
directly on top of the Dot's Fire OS 6 — a vendor image that cannot be upgraded. It
was `FROM ghcr.io/binozo/echogo:latest`, a third party's floating tag, and
the release workflow rebuilt it **from scratch on every tag push**: every
release was free to pick up a different compiler than the last, with no PR
and no CI signal. The first symptom would be a binary the hardware refuses
to run, which is the one failure here not recoverable from the dashboard.
The stage is `--platform=linux/amd64` (the NDK is x86-64 only), so building
the image needs an x86-64 builder or emulation.

Moving the pin needs **a real device in the loop**. The host tests and
`go vet` cannot speak to it — they run on amd64 with the host toolchain,
and this image is exercised only by the image build, `compile.sh` and
`build_tools.sh`, so a green CI run on a pin change proves nothing about it.

**Compile without the controller** (bare metal, or a quick ARM compile check):

```bash
device/compile.sh
# Output: device/build/server and device/build/version (the image's firmware stage)
```

`device/tools/build_tools.sh` (from `device/`) builds the capture/probe tools
in the same toolchain, tagged `echomuse-compiler` from `--target compiler`.

**Run Go tests (host):**

```bash
cd device
go test ./...
```

Tests only cover pure-Go logic — hardware-dependent code is not testable on the host.

**Run controller tests (host):**

```bash
cd controller
python -m pytest tests/        # needs: pytest numpy scipy websockets aiohttp bcrypt zeroconf protobuf — not the full requirements.txt
```

Controller tests cover pure-logic modules plus the device link, HA client, auth and BT proxy, which need `websockets`, `aiohttp`, `bcrypt`, `zeroconf` and `protobuf` (CI installs exactly `pytest numpy scipy websockets aiohttp bcrypt zeroconf protobuf`). Tests that load real models (`test_speech_worker`, parts of `test_wake_registry`/`test_wake_scorer`) skip without `onnxruntime`/`sherpa-onnx`. Keep new tests in that shape: evidence fixtures, not live models or a live HA. The suites (plus `go vet` and mypy) run in CI on every push/PR (`.github/workflows/ci.yml`).

**Type check the controller (host):**

```bash
cd controller
python -m mypy      # settings and file set in mypy.ini; needs mypy + the typed deps CI installs
```

The controller is held at zero mypy errors under `mypy.ini` (untyped defs disallowed, no bare generics, strict equality). Closed string vocabularies are `enum.StrEnum` (identical on the wire, since values serialize and compare as the raw string); structured data is a dataclass, or a `TypedDict` where a JSON dict passes through. `Any` stays at the genuinely dynamic boundary and is parsed into a typed object there.

**Device/controller compatibility.** The two halves version independently, so any pairing can occur in the field. Guarded by `tests/test_capabilities.py`:

- **Negotiate by capability, not version.** The device announces what it implements in `session.hello` (`device/internal/proto/proto.go`): the eight protocol capabilities of SPEC §11.1 — `audio_timeline_v1`, `uplink_leases_v1`, `device_wake_v1`, `render_reference_v1`, `render_progress_v1`, `focus_leases_v1`, `alert_cache_v1`, `turn_protocol_v1` — plus the retained hardware ones `leds`, `led_anim`, `buttons`, `button_hold`, and `ambient_light` **only when the sensor is actually readable**, the optional `alert_prefetch` (the device installs the sounds `alert.prefetch` names), the optional `local_wake_chime` (the device plays the wake chime itself at idle candidate open when `config` `wakeSound` is true, and reports it in `wake.candidate.chimed`; the controller pushes `wakeSound` only to such devices), the optional `open_rules_v1` (the device opens candidates on `session.ready` `detector.open_rules`, evaluates `detector.shadow_rules` without acting on them, credits the opening rule in `wake.candidate.rule` and reports `wake.stats.shadow`; the controller sends both lists only to such devices, and an absent `shadow` means no data, never zeros), and the optional `afe_metadata_v1` (only while its decoder is validating the native AFE metadata in bit 0 of `micAsr`: on `session.ready` `afe_metadata` the device serves the lease-gated `afe` stream of per-80 ms AFE records, EMA1 kind 5, and reports `wake.stats.afe`; the controller sends `afe_metadata` only to such devices, since older controllers end the session on an unknown EMA1 kind, and keeps the values as evidence that no decision reads — absent means no data, a device without it shows them `unavailable`). The controller admits a v1 device only with all eight (`em_device.REQUIRED_CAPABILITIES`) and reads the rest from `em_device.Device.capabilities` via properties like `led_anim_capable` / `ambient_light_capable`. Never compare version strings — that puts release history in the controller and misjudges dev builds. A UI control whose feature the device lacks is shown **disabled with the reason**, never as a control that silently does nothing.
- **Only Fire OS 6 is admitted.** The controller admits a v1 session only when `session.hello` carries `platform: fireos6`; an absent or other value is refused with `session.rejected` reason `protocol`. Pre-v1 firmware has no path at all: the legacy `/control` endpoint is gone.

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
../device/compile.sh                            # the firmware it installs → ../device/build/
SPEECH_BUNDLE_DIR=./speech FIRMWARE_DIR=../device/build python em_controller.py
```

The controller refuses to start without Home Assistant credentials, a verified speech bundle (`SPEECH_BUNDLE_DIR`, default `/app/speech`), the hash-checked armeabi-v7a `libonnxruntime.so` it serves to Dots as a speech asset (`ORT_ANDROID_LIB`, default `/app/models/ort_android/libonnxruntime.so`; the Dockerfile extracts it from the pinned `onnxruntime-android` 1.19.2 AAR), or the device firmware it installs (`FIRMWARE_DIR`, default `/app/firmware`, holding `server` and `version`). The deployed wake graph (`bcresnet_audio.onnx` + `.json` at the repo root) seeds the registry on first start.

**Docker:**

```bash
cd controller
docker-compose up --build   # build context is the repo root
```

Dashboard available at `http://<SERVER_IP>:8768`. Devices connect to port 8767 (8770 for TLS): `/device/v1/control`, `/device/v1/audio`, `/device/v1/assets` and `/shell`.

Key env vars in `.env` (see `.env.example` for the full list):

- `SERVER_IP` — LAN IP advertised via mDNS (devices connect here)
- `HA_URL` / `HA_TOKEN` — Home Assistant base URL and an **administrator** long-lived access token (the add-on uses the Supervisor token via `homeassistant_api: true` instead)
- `DEVICE_APPROVAL` — `strict` or `auto`; only a fallback: the database's `device_approval` system setting (created as `strict`, changed with `PATCH /api/system/config`) takes precedence
