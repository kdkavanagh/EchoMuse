# Echo Show 8 (`crown`) port: plan

**Status:** 2026-10-17. Phases 0–5 are implemented in the tree and were measured on G6G16D12143203LD under Fire OS. That unit has since been flashed to LineageOS 18.1 v0.6 to answer Phase 6's questions (findings §8.4–8.5), so the Fire OS phases can no longer be re-run on it without returning to stock. §6 lists the status.
**Evidence:** [echo-show-crown.md](echo-show-crown.md). Section numbers written as "findings §N" refer to it.
**Goal:**
- The Show is an EchoMuse voice satellite: wake word, capture, replies, timers and alarms.
- Android keeps the screen, which runs a third-party kiosk (Fully Kiosk Browser showing an HA dashboard, with its own camera motion wake and screen timeout).
- Alexa is neutralised.

**Not in scope:**
- Driving the display or the kiosk. EchoMuse only optionally pokes the kiosk's local API.
- Wi-Fi and Bluetooth, including BLE. On the Show they are owned by the device's own Android stack (WifiService with `wpa_supplicant`, and `com.android.bluetooth` on `/dev/stpbt`), configured from the device's Settings. EchoMuse's Wi-Fi management and BLE proxy do not run here (decision 7).
- LineageOS, apart from Phase 6.
- Retraining wake models.

## 0. Current state and authorisation (2026-10-17)

**Device state.** LineageOS 18.1 since 2026-10-17: a local lineage-18.1 build with the NS6571 `libasp.so` (findings status line; upstream draft PR amazon-oss/android_device_amazon_crown#2), root adb over USB seeded from TWRP (findings §8.5). Setup wizard on screen, no Wi-Fi, no kiosk. The Fire OS state (Wi-Fi joined from the device, Phase 3 applied, Fully Kiosk and Simple Keyboard) was wiped by the flash.

**Authorised.**
- Writes to `/system`, for example Phase 0 item 5's test `.rc`. Test artefacts are removed after each test.
- Running the firmware and probes by hand over adb.
- Temporary volume changes, restored afterwards.
- Phase 3, Alexa neutralisation, as part of the immediate work (2026-10-17): installing the kiosk (Fully Kiosk, free build) and a replacement keyboard, setting the provisioned flags, and hiding the findings §9 lists with their verification. Everything stays reversible with `pm unhide`.

**Not authorised yet (no full deployment).** No wizard run, no persistent EchoMuse boot service left installed, and no registration or approval on a controller.

**Deferred.** Mute (`amazon-gating`). Decision 9, Phase 0 item 4 and Phase 2's mute items move to Phase 7. Until then the firmware does not integrate the mute button, the Show reports no privacy state, and any mute indication the dashboard shows for it is unavailable with a reason.

**Who does what.**

| Work | Without the user | Needs the user |
|---|---|---|
| Phase 1 code, host tests, firmware run by hand (capture opens, metadata locks, chime plays) | yes | — |
| Phase 1 acceptance (registration, a spoken turn through HA) | — | controller access and device approval; a talker, or an HA speaker near the Show playing recorded wake words (the [playback-capture.md](playback-capture.md) method) |
| Phase 2 code; volume mechanism (Phase 0 item 6); ambient light | yes | — |
| Phase 0 items 1 (playback without a talker), 2 (second recorder, WebView), 3 (screen off), 5 (service domain) | yes | item 1 double talk needs a talker; item 2 against Fully needs Fully installed |
| Phase 3 | yes, with the free Fully build | the Fully PLUS licence (camera motion detection) for that acceptance item |
| Phase 4 code | yes | a real wizard run (WebUSB in the user's browser) |
| Phase 5 code, docs, tests | yes | — |
| Phase 6 (LineageOS) | measurements done (findings §8.4–8.5) | the mute button (done with the user); a decision to build the variant |
| Decisions: §5 open questions, platform name `fireos6_crown` | — | yes |

## 1. Why this is a port

The Show runs Fire OS 6 with the full Android framework (findings §0, §2):
- **Audio:** goes through AudioFlinger to Amazon's HAL and `libasp.so` inside `audioserver`. There is no `mixer` daemon or `libmixerAPI.so`, which are today's only audio backend.
- **Peripherals:** different hardware for mute, ambient light, buttons and lights (findings §3).
- **Framework services:** Wi-Fi, time, volume and Bluetooth belong to Android services.
- **Boot layout:** single slot, ramdisk rootfs, no `/tmp` that can be created (findings §1).

The measured path (findings §4.3–4.4) is the one the Fire OS 5 Dot used: OpenSL with the `VOICE_RECOGNITION` preset for capture, and an OpenSL player for output. It gives Amazon's ASR pipeline, v3.3 metadata in bit 0, and the echo reference by construction.

## 2. Decisions

1. **One firmware binary, with the platform chosen at runtime.**
   - The controller bundles and pushes one `server` to every device, so the Show runs the same binary as the Dot.
   - `internal/platform` detects the image once: `libmixerAPI.so` present means the Dot; `ro.product.device=crown` with `libOpenSLES.so` present means the Show; anything else fails startup with a clear log line.
   - `cmd/server.go` picks the audio backend and hardware bindings from that.
   - There are no build tags.
2. **New `session.hello` platform value `fireos6_crown`.**
   - The controller's `DevicePlatform` gains it, and `em_device` admission accepts both values.
   - The value names the image and product because the audio backend depends on both.
   - LineageOS would be a third value (Phase 6).
3. **Audio backend `internal/opensl`, restored from `a050bf3^`.**
   - Restore `internal/opensl/opensl.go` and `shim.h`, plus the OpenSL halves of `slmic` and `slspeaker`, behind the existing `pkg/mic.Microphone` and `render.Sink` interfaces.
   - Capture: preset `SL_ANDROID_RECORDING_PRESET_VOICE_RECOGNITION`, 16 kHz mono S16, in 1,280-frame (80 ms) OpenSL buffers: ten AFE frames each, so with the metadata phase-locked to the stream start no re-framing is needed and every period ends on a frame.
   - Playback: OpenSL buffer-queue player on `STREAM_MUSIC` at 48 kHz, two 2,048-frame buffers queued.
   - The firmware never opens ALSA PCMs and never writes `tinymix`. `audioserver` owns both PCMs, and stopping it removes the AEC (the Fire OS 5 "`stop media` must not run" rule, now about `audioserver`).
4. **Capture timestamps come from the metadata.**
   - OpenSL has no capture timestamp. Each v3.3 frame's `AFE_TIMESTAMP` is `CLOCK_MONOTONIC` ms (mod 65,536), and the buffer holding a frame completed 7–41 ms (median 18) after it (findings §4.4).
   - The capture adapter anchors period stamps on `AFE_TIMESTAMP`, unwrapped against the read-completion clock, and falls back to read completion while the decoder is unlocked.
   - Render completions are estimated from queue depth as on the Dot (`internal/audio/playclock`), with `timing_quality: "estimated"` as today. The 258 ms handoff-to-mic figure is not the constant: it was a fresh player's first sample, and AudioFlinger's mixer first read it about 168 ms after the handoff (the output leaving standby). Re-read from the probe logs, a sample took about 92 ms from the mixer reading it to its sound, 65–92 ms on `AFE_TIMESTAMP`-anchored stamps, so the pipeline lower bound is 40 ms (`opensl.playPipelineNs`, where the derivation lives). Phase 1 re-measures it.
5. **`afe_metadata_v1` is reused unchanged once validated.**
   - The decoder's rule (sync `0xA5`, version `0x63`, payload 104, checksum) matched 2,725 of 2,726 Show frames.
   - `VOLUME` reads 40 here, the ASP volume index, against 70 on the Dot. It is evidence only, as on the Dot.
6. **Hardware the Show announces:**

   | Capability | Show |
   |---|---|
   | the eight required (`audio_timeline_v1` … `turn_protocol_v1`) | yes |
   | `alert_prefetch`, `local_wake_chime`, `open_rules_v1` | yes |
   | `afe_metadata_v1` | yes (the OpenSL microphone decodes it; locked in the firmware run, findings §10.7) |
   | `ambient_light` | yes when `/sys/bus/platform/drivers/als_ps/lux` is readable (new `bindings/als` reader); optional |
   | `screen_wake` | yes: lights the kiosk screen through Fully's REST API on wake-candidate open when `screenWake` is configured (off by default) |
   | `leds`, `led_anim` | no (no ring) |
   | `buttons`, `button_hold` | no (no action button; volume keys are Android's) |
   | `wifi_control`, `ble_proxy` | no (decision 7) |
   | `privacy_mute` | no until Phase 7: the dashboard shows mute unavailable, "mute not supported on this device yet". The Dot announces it |

7. **Wi-Fi and Bluetooth belong to the Echo device, not EchoMuse.**
   - The firmware never touches `wpa_supplicant`, `wlan0`, DHCP, `/dev/wmtWifi` or `/dev/stpbt` on the Show. It uses whatever network Android has joined.
   - The user joins Wi-Fi and pairs Bluetooth from the device: the OOBE Wi-Fi screen before neutralisation, the device's Settings afterwards.
   - EchoMuse's BLE proxy (`ble_adverts`) is not offered. A Bluetooth-proxy role on this device would be Android's to fill, for example through an app, and is out of scope.
   - Today the controller offers Wi-Fi change/scan and accepts `ble_adverts` from every device. Add optional capabilities `wifi_control` and `ble_proxy`, announced by the Dot platform only. Their absence shows those dashboard controls disabled with the reason "managed by the device" (CLAUDE.md).
   - A Dot still on firmware that predates the capability shows them disabled with "firmware update available" until it updates. The controller already offers that update.
   - Alexa neutralisation (decision 12) keeps Android's Wi-Fi and Bluetooth stack and the Settings pages that drive it. The Amazon helpers layered on top of them are hidden: `com.amazon.wifi.sync`, `whisperjoin`, the smart-home BLE adapters, Sidewalk. Phase 3 verifies that joining Wi-Fi and pairing Bluetooth still work afterwards.
8. **Volume authority is Android's `STREAM_MUSIC` index (0–30)**, so the volume keys, the kiosk and EchoMuse agree. The wire level stays 0–127; the Show maps index = round(level × 30 / 127) and level = round(index × 127 / 30).
   - **Mechanism:** pinned `service call audio` codes (AOSP 7.1's: 3 set, 8 get, 10 max), verified at startup by reading the maximum (30) and a get / set-same / get round trip (findings §10.5, `internal/androidvol`).
   - **Failure:** volume control is disabled for the run; `session.hello` carries `volume.level: null` and the dashboard shows "volume control unavailable: Android volume check failed (see device log)".
   - **Key-driven changes:** the firmware watches the mtime of `settings_system.xml` each second and reads the index only when it changed. `settings get` costs about a second of CPU per call, so it is not polled.
9. **Mute comes from `amazon-gating` (deferred to Phase 7).** The firmware will read `/sys/devices/platform/amazon-gating/state` at boot and on change, and enter privacy (no capture, mute reported) when it is set. The button itself stays Android's.
10. **Process identity: uid 0, egid `audio`.** The test recorded as uid 0 with no privilege drop (findings §4.3). The `aipc` group is a mixer requirement and does not apply.
11. **Boot integration:**
    - One init `.rc` in `/system/etc/init/` (single slot: nothing to mirror).
    - Logs and the server's scratch space go to `/data/local/tmp/echomuse`, not `/tmp`: the rootfs is a read-only ramdisk rebuilt at every boot.
    - The service domain: `seclabel u:r:adbd:s0` in the `.rc`, then `start_server.sh` moves into `u:r:su:s0` as on the Dot. In `adbd`, audioserver refuses the recorder's binder replies (findings §10.4).
12. **Alexa is neutralised by package hiding, not deletion, plus microphone denial for two packages that cannot be hidden.**
    - Use `pm hide` for the findings §9 Tier 1 list (Tier 2 optional), and stop native services per boot. Settings: `device_provisioned=1`, `user_setup_complete=1`.
    - `amazon.speech.sim` and `com.amazon.kindle.otter.oobe` stay visible: KnightSystemUI crash-loops without SIM, and KnightSettings dies without `otter.oobe`. SIM and Amazon's dictation keyboard get `appops set <pkg> RECORD_AUDIO ignore`, which makes AudioFlinger refuse their recorders (findings §9.5).
    - KnightSettings needs its on-device config bundle, which an unregistered device never installs; the wizard installs it with one explicit broadcast (findings §9.5, which also records the factory-reset broadcast that must never be sent).
    - Everything is reversible with `pm unhide` and `appops set <pkg> RECORD_AUDIO default`.
    - The wizard applies it; `start_server.sh` re-asserts the hiding and the microphone denial at every boot, idempotently, because packages with `CHANGE_COMPONENT_ENABLED_STATE` exist until they are themselves hidden.
13. **The kiosk is the user's.**
    - EchoMuse does not install or configure Fully Kiosk, apart from enabling Fully's own HOME component (`.LauncherReplacement`), which ships disabled (findings §9.5). The wizard refuses to hide the Amazon launchers until another HOME activity exists (§4, Phase 3).
    - Fully's own microphone features (acoustic detection) and an in-page Assist mic must stay off: Android 7.1 allows one active capture, and EchoMuse holds it.

## 3. Phases

Each phase ends with an observable result on G6G16D12143203LD.

### Phase 0: Remaining validation (probe only, no firmware)

Rebuild the findings §4.4 probe as `device/tools/slprobe` (C, built by `build_tools.sh`'s toolchain) so these are repeatable:
1. **Double talk and loud playback.** A talker 1–2 m away over music on `STREAM_MUSIC` at several indices. Record `VOICE_RECOGNITION` plus `DTD`, `DNN_VAD_PROB` and `UED`, and look for `MIC_CLIPPED`/`OUTPUT_CLIPPED`.
2. **Concurrency.** Start a second recorder during our capture: Fully's acoustic detection, and a WebView `getUserMedia`. Who wins, and is our stream silenced or broken?
3. **Screen off.** With the screen off (Fully's timeout or `input keyevent 26`), does capture continue, and does the device suspend? Check `dumpsys power` wakelocks.
4. **Mute** (deferred to Phase 7). Press the mute button during capture. Does ASR output go to digital zero, and what do `state` and the metadata's `DEVICE_MUTE` read?
5. **Service domain.** A test `.rc` stanza (`seclabel u:r:adbd:s0`, as on the Dot) that runs the probe at boot. Does capture work from `adbd`, or is the Dot's `echo u:r:su:s0 > /proc/self/attr/current` move needed?
6. **Volume.** Pin and test the `service call audio` codes for get/set `STREAM_MUSIC` on this build (Q1).

**Acceptance:** each question has a measured answer recorded in findings §10. Any answer that invalidates a decision in §2 is raised before Phase 1.

### Phase 1: Audio backend

1. Restore `internal/opensl` and the OpenSL `slmic`/`slspeaker` from `a050bf3^`, adapted to today's `pkg/mic.Microphone` / `render.Sink` and stats shapes.
2. Capture adapter:
   - `VOICE_RECOGNITION`, 1,280-frame periods;
   - `AFE_TIMESTAMP`-anchored period stamps (decision 4);
   - re-open on recorder errors;
   - drop and discontinuity counting.
3. Render adapter:
   - OpenSL player on `STREAM_MUSIC`, 48 kHz;
   - the queue kept shallow, as the Dot's Player keeps the mixer queue at 3 blocks (fireos6-port.md §6), so the wake chime is not heard late;
   - completions estimated from depth plus measured latency.
4. `internal/platform` detection, and backend selection in `cmd/server.go`.
5. Host tests for re-framing and timestamp unwrap only (pure Go).

**Acceptance:**
- With the firmware run by hand under `adb shell` on the stock, still-in-OOBE Show, the device registers with the controller (once Phase 5 admission exists, or against a controller with admission patched locally for the test).
- A spoken wake word produces `wake.candidate`, the turn completes through HA and TTS plays.
- `render.progress`/`render.finished` arrive.
- Wake during TTS and during music on `STREAM_MUSIC` is detected.
- The `[afe]` stats line shows locked v3.3 frames.

### Phase 2: Platform glue

- **Hello:** `platform: fireos6_crown` and the capability set of decision 6.
- **Disabled on the Show:** `wifi.EnsureUp`/`RecoverIfPending`/`Change`/`Scan` and every Wi-Fi message handler, the BLE scanner, `timesync.Run` (Android owns the clock; EchoMuse needs no wall clock, and `clock.request` covers UTC), LEDs, the `buttons` controller, `jack` (unless `h2w` proves useful) and every `tinymix` call.
- **Volume:** controller `volume_set` maps to the `STREAM_MUSIC` index; key changes are reported back (decision 8).
- **Mute:** deferred to Phase 7 (decision 9).
- **Ambient light:** `bindings/als` gains the `als_ps/lux` reader; announced only when readable.
- **Optional:** on wake candidate open, light the screen through Fully's local REST API (`screenOn`), with URL and password from device config. Off by default.

**Acceptance:**
- Volume keys and dashboard volume stay in sync both ways.
- Mute is deferred (Phase 7); the dashboard shows it unavailable for the Show with a reason.
- Ambient lux appears on the dashboard.
- The firmware leaves the network alone: with the Show joined by Android, `wpa_supplicant`, `dhcpcd` and `/dev/stpbt` show no EchoMuse access (`lsof`, logcat), and a Wi-Fi change made in Settings is followed by a clean reconnect to the controller.
- The Dot behaves exactly as before on the same binary (run the Dot's existing acceptance on one Dot).

### Phase 3: Alexa neutralisation

- **Tier lists:** the findings §9.1 and §9.2 lists become data shipped with the controller, like the old Fire OS 5 `debloat_packages.txt`: `controller/device_payloads/crown_hide_packages.txt`, plus the native service stop list inside `start_server.sh`'s crown branch.
- **Order** (findings §9.4):
  1. Refuse unless a non-Amazon HOME activity resolves.
  2. Set the provisioned flags.
  3. Hide Tier 1.
  4. Reboot and verify.
  5. Then, optionally, Tier 2.
- **Repeat the test on changes:** adding a package to a list requires repeating findings §9.4's verification, the rule fireos6-port.md §4.1 set for the Dot.

**Acceptance:**
- After a cold boot, the kiosk is HOME, and no hidden package runs after 10 minutes.
- `dumpsys media.audio_flinger` shows only EchoMuse's input.
- The `VOICE_RECOGNITION` probe still yields pipeline 0 with metadata.
- Fully's motion wake works.
- MemAvailable is recorded before and after each tier.
- Android still owns connectivity: Settings joins a different Wi-Fi network and back, and pairs a Bluetooth device, after each tier and a reboot. The Wi-Fi and Bluetooth settings pages stay reachable from the kiosk (Fully's settings shortcut or the SystemUI shade).

### Phase 4: Boot integration and wizard

- **`start_server.sh` crown branch:**
  - chosen by the platform check;
  - wait for `init.svc.audioserver=running`;
  - re-assert the hide list and stop the native services;
  - run the `server_a`/`server_b` supervisor unchanged, logging to `/data/local/tmp/echomuse/`.
- **Install:** `install_boot_hook` writes `/system/etc/init/echomuse.rc` (remount `/system` rw, then ro) using the domain chosen in Phase 0. There is no mirror step.
- **Wizard (`dashboard.jsx`, `em_api.py`):**
  - `connect_android` accepts `ro.product.name=crown` with Android 7.1.x and a uid 0 adb shell, with its own tested-build pin (`NS65741/8146`).
  - Prerequisite: the Show is already on Wi-Fi, joined by the user through the device (OOBE's Wi-Fi screen or Settings). The wizard does not provision Wi-Fi on this platform.
  - Step list: `connect_android → check_home (kiosk present) → install_boot_hook → install_em → neutralize_alexa → reboot → reconnect → verify_service → confirm_link`. `wifi`, `mirror_boot_hook` and the Dot-only steps are shown as not applicable.
- **Diagnostics probes:** `init.svc.audioserver`, `init.svc.echomuse`, the HOME resolution, the hidden-package count, and `amazon-gating` state.

**Acceptance:**
- A wizard run from the device's current state (rooted, in OOBE, Fully Kiosk installed by hand) ends with the device pending approval.
- A cold boot with no cable comes up talking to the controller with the kiosk on screen.
- Deleting the `.rc` and running `pm unhide` on the list returns the Show to stock.

### Phase 5: Controller, docs, tests

- **Controller:**
  - `DevicePlatform.FIREOS6_CROWN`; admission accepts it.
  - `wifi_control` and `ble_proxy` capability gating (decision 7), with disabled-with-reason controls.
  - LED and button controls are already gated by capability.
  - The controller pushes the same bundled firmware to the Show.
- **Docs:**
  - [rooting.md](rooting.md): a Show section that points at R0rt1z2's crown unlock thread as canon, and says what EchoMuse writes and how to undo it.
  - [protocol-v1.md](protocol-v1.md): the platform value and the two new capabilities.
  - CLAUDE.md: the platform matrix and runtime backend selection.
  - [quickstart.md](quickstart.md).
  - Findings §10 updated with the Phase 0 answers.
- **Tests:**
  - `tests/test_capabilities.py` for the new capabilities and platform admission;
  - the wizard's platform classification test (`pm_verdict.test.mjs`);
  - Go host tests for the timestamp anchoring.

**Acceptance:** CI green (pytest, `go test`, `go vet`, mypy). The dashboard shows the Show with ring controls disabled, and Wi-Fi and BLE proxy controls disabled with "managed by the device".

### Phase 6 (optional): LineageOS 18.1 variant, alongside Fire OS

LineageOS is a third platform beside the Dot and Fire OS crown, not a replacement. The gate questions were measured on v0.6 (findings §8.4–8.5):

| Question | Answer on v0.6 | Consequence |
|---|---|---|
| Does `VOICE_RECOGNITION` reach Amazon's ASR pipeline? | Yes: `PipelineAsr`, 16 kHz mono S16, no converter | the capture path is the same as on Fire OS |
| Does the bit-0 metadata survive? | **No metadata at all** (0 valid frames, no sync byte at any phase); the blobs are older than stock's | no `afe_metadata_v1`, no `AFE_TIMESTAMP` anchoring, no `PLAYBACK_ACTIVE`/`DTD` evidence |
| Echo cancellation and mic level | Mic gain and full-volume speaker level within 0.5 dB of stock; cancellation 5–11 dB weaker (pink 19–24 dB vs 27–29) | more echo leaks into wake and barge-in; the controller's attribution carries more of the load |
| Is a root native recorder allowed? | Yes; uid 0 is the policy's assistant, so it is never silenced, and every app capture gets zeros while it runs. An app that opened the mic first leaves us on the voice-call pipeline | the kiosk can't use the mic while EchoMuse records; `getUserMedia` in the dashboard gets silence |
| Timestamps | AAudio capture and render `getTimestamp` work; the echo sits 88 ms (VR) after predicted playout, stable within 3 ms across opens. OpenSL has none and drifts 50–60 ms between opens | backend: AAudio both directions, `dlopen("libaaudio.so")`, `PERFORMANCE_MODE_NONE`, a per-build latency constant |
| Volume | `STREAM_MUSIC` 0–15; codes 16 get, 10 set (with a package string), 18 max; `settings` is cheap | a second `androidvol` code table and scale |
| Boot hook and updates | `seclabel u:r:su:s0` works directly (no `adbd` → `su` move); an addon.d v2 script keeps the `.rc` across a LineageOS update | `.rc` + `/system/addon.d/NN-echomuse.sh`; a clean flash needs the wizard again |
| Mute | Kernel gates the mic on a long press, latch survives reboot; a tap clears the light but leaves the mic at zeros | `privacy_mute` is possible: read `amazon-gating/state`, and tell the user to hold the button to un-mute |
| Ambient light | Same sysfs `lux`, but stale until `enable_als_polling=1` | the reader enables polling on LineageOS |
| Root adb for the wizard | userdebug, but USB debugging and "Rooted debugging" are off by default; `adb root` is not persistent | the wizard needs the user to enable both on screen once, or the TWRP seeding of findings §8.5 |

**Before any LineageOS device meets the current firmware:** `platform.Detect` classifies LineageOS as Fire OS crown (`ro.product.device=crown` plus OpenSL), and the Fire OS volume check's `service call audio 10` is `setStreamVolume` there. The wizard's NS65741 build pin keeps the firmware off LineageOS today; the variant must detect `ro.lineage.version` first.

Still open on LineageOS: double talk, capture policy with an assistant role holder set, the long-run counter wrap and resampler hazards (findings §8.3), whether a later build brings the metadata back, and a multi-day soak.

The implementation plan for the variant is [echo-show-lineage-plan.md](echo-show-lineage-plan.md).

### Phase 7 (deferred): Mute

- Phase 0 item 4: what the `amazon-gating` button does to capture at the source.
- The `amazon-gating` reader (decision 9): privacy blocks capture and is reported.
- jxlarrea's camera-power latch (findings §8.2) reproduces on LineageOS: after a reboot while muted, camera power stays off once un-muted (findings §8.5). On stock it is Amazon's driver behaviour, untested here.
- On LineageOS a tap clears the light without restoring the mic (findings §8.5); check whether stock's driver does the same.

**Acceptance:** mute in both directions, and across a reboot, never shows a live mic while muted.

## 4. Risks

| Risk | Mitigation |
|---|---|
| An Amazon package re-enables a hidden one, or reinstalls an update (`rdmapplication`, `venezia`, `d3`, `adep`, `alta`, `ambienthome` hold the permissions) | They are all Tier 1 hidden; `start_server.sh` re-asserts the list per boot; OTA stays disabled |
| No HOME activity after hiding the Amazon launchers, leaving `FallbackHome` and a blank screen | The wizard refuses until a non-Amazon HOME resolves; adb root stays reachable for `pm unhide` |
| A second recorder (kiosk, WebView Assist, calls) takes or breaks the single Android 7.1 input | Phase 0 item 2; document the kiosk settings that must stay off; capture re-opens on error |
| Capture stops with the screen off, or the device suspends | Phase 0 item 3. An active AudioFlinger input should hold a wakelock [INFERENCE] |
| A factory reset (`diode`, `settings.windfall` hold `MASTER_CLEAR`) wipes `/data` and EchoMuse | Tier 1 hidden. KnightSettings itself factory-resets on `com.amazon.dcp.sso.action.account.removed` once provisioned: EchoMuse never sends it, and the device has no account to remove (findings §9.5) |
| `amazon-gating` mute does not silence ASR output at the source | Deferred with mute (Phase 7). Until then, mute is not offered for the Show |
| The device is online in OOBE with the stock Amazon stack running, so Amazon services can act before Phase 3 (app updates through `venezia`/`d3`/`rdmapplication`) | OTA stays disabled (findings §1); the device is unregistered, so account-bound services stay idle [INFERENCE]; Phase 3 hides the re-enablers once authorised |
| The frozen WebView 118 eventually falls behind the HA frontend | Out of EchoMuse scope; noted for the user. LineageOS (Phase 6) has an updatable WebView |
| The init domain cannot use binder or AudioFlinger | Phase 0 item 5; the Dot's `su` dyntransition is the known fallback |
| The volume mechanism uses pinned binder transaction codes | One fixed build; verified at startup by a round-trip read; volume control is disabled with a reason if the check fails |
| The output latency differs under load or after the queue change | Re-measured with the firmware's own Player (`sl_latency`, findings §10.7): render stamps 95–172 ms early, never late |
| Hiding Amazon packages breaks the Settings and SystemUI the device needs | Measured: hiding `otter.oobe` killed Settings and hiding SIM crash-looped SystemUI; both moved to Keep, SIM mic-denied instead (findings §9.5). Joining another network and pairing Bluetooth after a reboot still need a person at the screen; adb root and `pm unhide` stay available |
| Bricking | No partition, bootloader or recovery writes. One `.rc` and package-state changes only, all reversible over adb |

## 5. Open questions (resolved 2026-10-17 with the conservative choice; the user may revisit)

1. **Volume mechanism (decision 8):** pinned `service call audio`, measured in Phase 0 item 6.
2. **Ambient light:** announced when readable; it costs one sysfs read and the dashboard already gates it.
3. **Screen wake:** only on request (`screenWake` off by default).
4. **Tier 2:** opt-in from the wizard, default off.

## 6. Implementation status (2026-10-17)

| Phase | In the tree | Measured on G6G16D12143203LD | Still needs the user |
|---|---|---|---|
| 0 | `device/tools/slprobe` (+ `decode.py`, `stimulus.py`) | items 1, 2, 3, 5, 6 (findings §10) | double talk (a talker); Fully acoustic detection and WebView mic (PLUS licence) |
| 1 | `internal/opensl`, `slmic.OpenOpenSL`, `slspeaker.NewOpenSL`, `afe.Period.Anchor`, `internal/audio/playclock`, `device/tools/sl_latency` | metadata locked, every period `AFE_TIMESTAMP`-anchored, render stamps never late (findings §10.7) | registration and a spoken turn through HA, wake during TTS and music |
| 2 | `internal/platform.Detect`, platform matrix in `cmd/server.go`, `internal/androidvol`, `server.NewAndroidHardware`, `bindings/als` sysfs reader, `internal/kiosk` (screen wake) | firmware run by hand: platform, volume check, ALS, capture (findings §10.7) | volume keys ↔ dashboard both ways, lux on the dashboard, a Settings Wi-Fi change followed by a clean reconnect, the Dot's acceptance on the same binary |
| 3 | `crown_hide_packages.txt` (26), `crown_hide_packages_tier2.txt`, `crown_mic_deny_packages.txt` | Tier 1 applied and corrected, 25-minute soak clean (findings §9.5) | Fully motion wake (PLUS), joining another Wi-Fi network and pairing Bluetooth on screen, a power-cycle cold boot, Tier 2 |
| 4 | `start_server.sh` crown branch, `.rc`, wizard steps in `dashboard.jsx`, crown diagnostics probes | the `.rc` stanza and the `su` move (findings §10.4) | a real wizard run, then a cold boot talking to the controller |
| 5 | `DevicePlatform.FIREOS6_CROWN`, capabilities `wifi_control`, `ble_proxy`, `privacy_mute`, `screen_wake`, capability gaps in the dashboard, null volume, docs | — | the dashboard against the live Show |
| 6 | — | all gate questions (findings §8.4–8.5) | decide whether to build the LineageOS variant |
| 7 | — | the LineageOS mute behaviour (findings §8.5) | mute on stock Fire OS |
