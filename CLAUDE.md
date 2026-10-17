# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

EchoMuse repurposes Amazon Echo Dot Gen 2 (Fire OS 6 / Android 7.1.2, codename "biscuit") and Echo Show 8 Gen 1 (codename "crown", on Fire OS 6 with the full Android framework or on our LineageOS 18.1 build) as open-source voice assistant satellites. Two components:

- **`device/`** — Go binary that runs directly on the rooted Echo device; one binary for all three images
- **`controller/`** — Python asyncio server that manages devices over the v1 device link (`docs/protocol-v1.md`), runs each device's session actor and the speech worker (VAD, streaming ASR, echo attribution, endpointing), and drives stock Home Assistant (Assist pipeline runs, native satellite timers, one Local Calendar per speaker for alarms). Wake detection is BCResNet on the Dot; the controller's content-addressed wake-model registry (`em_wake_registry`) names the graph each device loads. BCResNet training lives outside this repo (`~/git/bcresnet`).

`docs/post-afe-audio-architecture.md` describes the current architecture (§16 is the most detailed; §18 is the replace/keep inventory), and `docs/protocol-v1.md` the device link. They document the design; they do not freeze it. When a better design is chosen, change the code and update the documents to match.

**Three device images, one firmware binary.** `internal/platform.Detect` picks the platform once at start (`libmixerAPI.so` present → Dot; `ro.product.device=crown` with `ro.lineage.version` set → LineageOS Show; `crown` at `ro.build.version.sdk=25` with `libOpenSLES.so` → Fire OS Show; anything else fails startup), and `cmd/server.go` chooses the audio backend and hardware bindings from it (its package doc holds the full matrix). There are no build tags.

**Echo Dot (`platform: fireos6`).** Fire OS 6 (API 25, `biscuit_puffin`; amonet-biscuit v2 plus R0rt1z2's boot-root.zip) is headless, with no Android framework and only toybox. Audio goes through Amazon's `mixer` daemon via `libmixerAPI` (`device/internal/mixerapi`); Wi-Fi through Amazon's `wifisvc`. The firmware must run with primary gid `aipc` (2901), or the mixer rejects its streams. The wizard's `/system/etc/init/echomuse.rc` provides that. It uses `seclabel u:r:adbd:s0` because init may not enter `su`, and `start_server.sh` then moves itself into `su`, since `wpa_supplicant` may not reply to `adbd`. `docs/fireos6-port.md` holds the evidence and the service denylist.

**Echo Show 8 (`platform: fireos6_crown`).** Fire OS 6574.1 (`NS65741/8146`, API 25) with the full Android framework: audio goes through AudioFlinger to Amazon's HAL and `libasp.so` inside `audioserver`, so the firmware uses OpenSL ES (`internal/opensl`, `slmic.OpenOpenSL`, `slspeaker.NewOpenSL`) with the `VOICE_RECOGNITION` preset, which reaches the ASR pipeline with v3.3 metadata in bit 0; capture periods are stamped from `AFE_TIMESTAMP`. Never stop `audioserver`. Android owns Wi-Fi, Bluetooth, the clock and volume (`STREAM_MUSIC` through pinned `service call audio` codes, `internal/androidvol`); the firmware never touches the network, `tinymix`, LEDs or buttons there, and has no privacy mute yet. Its `.rc` runs as uid 0 / gid `audio` with the same `adbd` → `su` move (audioserver refuses the recorder from `adbd`). Logs and scratch go to `/data/local/tmp/echomuse` (no `/tmp`). Alexa is neutralised by `pm hide` lists plus `appops` microphone denial for `amazon.speech.sim` and Amazon's keyboard, which cannot be hidden (`controller/device_payloads/crown_*.txt`); the user's kiosk (Fully Kiosk) owns the screen. `docs/echo-show-crown.md` holds the evidence, `docs/echo-show-port-plan.md` the plan and status.

**Echo Show 8 on LineageOS (`platform: lineage18_crown`).** Our fork of LineageOS 18.1 for crown (Android 11, API 30, userdebug, SELinux permissive; branches `crown-ns6571-asp` of kdkavanagh/android_device_amazon_crown and android_vendor_amazon_crown) ships Fire OS NS6571's `libasp.so` (SHA-1 `e3816eb3…`), so `VOICE_RECOGNITION` carries v3.3 metadata; upstream builds lack it and the wizard refuses them. Audio goes through AAudio (`internal/aaudio`, dlopen'd; `slmic.OpenAAudio`, `slspeaker.NewAAudio`): capture is stamped from a fit through AAudio's capture timestamps (the metadata is evidence only), render from AAudio's presentation timestamps plus `aaudioRenderLatencyNs`; OpenSL lost both anchors there. Volume is `STREAM_MUSIC` 0–15 behind Android 11's binder codes (`androidvol.Lineage18`: get 16, set 10, max 18), and the light sensor needs `enable_als_polling` switched on (`als.EnablePolling`). Its `.rc` runs in `u:r:su:s0` directly (no domain move), `/system/addon.d/50-echomuse.sh` keeps it across a LineageOS update, and `start_server.sh`'s `lineage` branch does no Alexa neutralisation (there are no Amazon apps). uid 0 is the audio policy's assistant there, so every app capture (the kiosk's microphone included) gets zeros while EchoMuse records. `docs/echo-show-crown.md` §8.4–8.6 holds the evidence, `docs/echo-show-lineage-plan.md` the plan and status.

Fire OS 5 is not supported: firmware from this tree cannot run audio there, the wizard refuses an Android 5.x Dot at connect, and the controller admits only devices that announce `platform: fireos6`, `fireos6_crown` or `lineage18_crown`. A Fire OS 5 Dot moves to Fire OS 6 first (`docs/rooting.md`).

## Building the device binary

The Dot and the Fire OS Show run API 25 and the LineageOS Show API 30; the firmware is still built for API 22, which runs on all three unchanged (AAudio, API 26+, is dlopen'd), and moving the API level is a toolchain change (see the pin below). Standard Go cross-compilation won't work — a custom Docker build environment is required.

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

- **Negotiate by capability, not version.** The device announces what it implements in `session.hello` (`device/internal/proto/proto.go`): the eight protocol capabilities of SPEC §11.1 — `audio_timeline_v1`, `uplink_leases_v1`, `device_wake_v1`, `render_reference_v1`, `render_progress_v1`, `focus_leases_v1`, `alert_cache_v1`, `turn_protocol_v1` — plus the retained hardware ones `leds`, `led_anim`, `buttons`, `button_hold`, and `ambient_light` **only when the sensor is actually readable**, the optional `alert_prefetch` (the device installs the sounds `alert.prefetch` names), the optional `local_wake_chime` (the device plays the wake chime itself at idle candidate open when `config` `wakeSound` is true, and reports it in `wake.candidate.chimed`; the controller pushes `wakeSound` only to such devices), the optional `open_rules_v1` (the device opens candidates on `session.ready` `detector.open_rules`, evaluates `detector.shadow_rules` without acting on them, credits the opening rule in `wake.candidate.rule` and reports `wake.stats.shadow`; the controller sends both lists only to such devices, and an absent `shadow` means no data, never zeros), the optional `afe_metadata_v1` (only while its decoder is validating the native AFE metadata in bit 0 of the ASR capture: on `session.ready` `afe_metadata` the device serves the lease-gated `afe` stream of per-80 ms AFE records, EMA1 kind 5, and reports `wake.stats.afe`; the controller sends `afe_metadata` only to such devices, since older controllers end the session on an unknown EMA1 kind, and keeps the values as evidence that no decision reads — absent means no data, a device without it shows them `unavailable`), the Dot-only optional `wifi_control` (the controller sends `wifi_*` and allows the dashboard's Wi-Fi change/scan only to such devices), `ble_proxy` (`ble_adverts` from any other device are dropped and no HA Bluetooth proxy runs for it) and `privacy_mute` (without it mute is shown unavailable, never off), and the Echo Show-only optional `screen_wake` (the controller pushes `screenWake`/`screenWakeUrl`/`screenWakePassword` only to such devices). The controller admits a v1 device only with all eight (`em_device.REQUIRED_CAPABILITIES`) and reads the rest from `em_device.Device.capabilities` via properties like `led_anim_capable` / `ambient_light_capable`. Never compare version strings — that puts release history in the controller and misjudges dev builds. A UI control whose feature the device lacks is shown **disabled with the reason**, never as a control that silently does nothing. Where the reason depends on the platform ("managed by the device" or "no LED ring on this device" on an Echo Show, "firmware update available" on a Dot whose firmware predates a Dot-only capability), it is the controller's: `em_device.capability_gap`, sent to the dashboard as `capability_gaps`.
- **Only the supported images are admitted.** The controller admits a v1 session only when `session.hello` carries `platform: fireos6` (Echo Dot 2nd gen), `platform: fireos6_crown` (Echo Show 8 1st gen on Fire OS 6) or `platform: lineage18_crown` (the same Echo Show 8 on LineageOS 18.1); one firmware binary runs on all three and picks its backends at runtime. An absent or other value is refused with `session.rejected` reason `protocol`. Pre-v1 firmware has no path at all: the legacy `/control` endpoint is gone.

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
