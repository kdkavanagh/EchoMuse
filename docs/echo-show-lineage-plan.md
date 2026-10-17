# Echo Show 8 (`crown`) on LineageOS 18.1: plan

**Status:** Phases L1–L5 implemented 2026-10-17; L6 partly done (§7). The measurements it rests on are done.
**Evidence:** [echo-show-crown.md](echo-show-crown.md) §8.4 (audio) and §8.5 (platform), written "findings §N" below. The Fire OS port it extends is [echo-show-port-plan.md](echo-show-port-plan.md), written "port plan".
**Goal:** EchoMuse runs on an Echo Show 8 (2019) under LineageOS 18.1 as a third platform beside the Dot and Fire OS crown, from the same firmware binary and the same controller.

**Not in scope:**
- Replacing Fire OS crown support. Both Show images stay supported.
- The kiosk, Wi-Fi and Bluetooth. As on Fire OS crown, they belong to Android and the user (port plan decision 7).
- Mute. The hardware button works as-is; EchoMuse does not read it and does not announce `privacy_mute`.
- Double talk, and the long-run hazards of findings §8.3, beyond the soak in Phase L6.

## 1. Starting point

| Area | LineageOS 18.1 on crown (measured) | Fire OS crown, for comparison |
|---|---|---|
| ASR capture | `VOICE_RECOGNITION` from uid 0 reaches Amazon's ASR pipeline, 16 kHz mono S16, no converter | same |
| Capture policy | uid 0 is the policy's assistant: never silenced; every app capture gets zeros while it runs | a second recorder gets nothing |
| AFE metadata (bit 0) | v3.3 on our fork (NS6571 `libasp.so`): 37,467/37,470 frames valid in 5 min. Upstream v0.6 has none | v3.3 |
| Timestamps | AAudio: capture and render `getTimestamp` work; echo 88 ms after predicted playout, stable within 3 ms. OpenSL: FAST capture, no timestamps, lag moves 50–60 ms between opens | OpenSL plus `AFE_TIMESTAMP` anchoring |
| Echo cancellation | 5–11 dB weaker than stock (v0.6; the NS6571 `libasp` showed no visible change in an A/B) | stock |
| Volume | `STREAM_MUSIC` 0–15; `service call audio` 16 get, 10 set (with a package string), 18 max | 0–30; 8 get, 3 set, 10 max |
| Boot hook | `.rc` with `seclabel u:r:su:s0` records at boot; an addon.d v2 script keeps it across a LineageOS update | `.rc` in `adbd`, then a move to `su` |
| Light sensor | same sysfs `lux`, stale until `enable_als_polling=1` | live |
| Root adb | userdebug; USB debugging and "Rooted debugging" are off until enabled on screen, and `adb root` does not persist | `boot-root.img`, root from boot |
| Alexa | not installed | neutralised by package hiding and appops |

**ROM.** This plan assumes our fork of LineageOS 18.1 for crown, which ships the NS6571 `libasp.so` with `libtensorflowlite_c.so` and `libedgeflow_core_neon.so`: branches `crown-ns6571-asp` of kdkavanagh/android_device_amazon_crown and kdkavanagh/android_vendor_amazon_crown, offered upstream as draft PRs amazon-oss/android_device_amazon_crown#2 and amazon-oss/android_vendor_amazon_crown#1. A build of it is on the test unit. Upstream v0.6, without the metadata encoder, is not a target.

## 2. Decisions

1. **Platform value `lineage18_crown`.** Go `proto.PlatformLineage18Crown`, `platform.KindCrownLineage`; Python `DevicePlatform.LINEAGE18_CROWN`. The name carries the image and the Android generation, because the audio backend and the volume codes depend on both.
2. **Detection order** (`internal/platform.detect`):
   1. `libmixerAPI.so` present → Dot.
   2. `ro.product.device=crown` and `ro.lineage.version` non-empty → LineageOS crown.
   3. `ro.product.device=crown`, `ro.build.version.sdk=25` and `libOpenSLES.so` present → Fire OS crown.
   4. Anything else fails startup.

   Rule 3 gains the SDK check now, independently of this plan: today a LineageOS Show passes the Fire OS rule, and the Fire OS volume check's `service call audio 10` is `setStreamVolume` there (findings §8.5).
3. **Audio backend: AAudio in both directions**, loaded with `dlopen("libaaudio.so")` because the firmware targets API 22. Capture: `VOICE_RECOGNITION`, 16 kHz, mono, I16, `PERFORMANCE_MODE_NONE` (the FAST path drops timestamps). Render: `USAGE_MEDIA`, 48 kHz. OpenSL is not used on LineageOS: it lost both anchors (findings §8.4).
4. **Capture stamps from AAudio's timestamps; the metadata is evidence**, as on the Dot. AAudio's capture position comes from the HAL and stamps sample time, while `AFE_TIMESTAMP` marks AFE processing time and arrives in bursts within ±8 ms of its grid (`internal/audio/afe`). Fit a line through `getTimestamp` pairs: single stamps jitter 15–16 ms peak to peak and sit about 5 ms late. Capture and render then share one clock source.
5. **Render stamps** are AAudio's predicted playout plus a named per-platform latency constant. It must stay at or below the measured 88 ms so that the mic never appears to lead its reference (`playclock.go`); Phase L2 re-measures it with the firmware's own backend.
6. **`afe_metadata_v1` is announced.** `supervisor.afeCapable` announces it once the decoder has accepted a frame, which our fork's `libasp` provides from the first capture period.
7. **Volume:** `internal/androidvol` gets a code table per platform. LineageOS: get 16, set 10 with `s16 com.android.shell`, max 18, `MaxIndex` 15; wire level ↔ index by `round(level × 15 / 127)` and `round(index × 127 / 15)`. The startup check, the `settings_system.xml` mtime watch and the null-volume fallback are unchanged (`settings_system.xml` is still plain XML on Android 11).
8. **Light sensor:** on LineageOS the reader writes `1` to `/sys/bus/platform/drivers/als_ps/enable_als_polling` before its first read. Without it the value is a stale cache.
9. **Boot hook:** `/system/etc/init/echomuse.rc` with `user root`, `group root audio inet`, `seclabel u:r:su:s0`, and no move between domains. `/system/addon.d/50-echomuse.sh` (v2 format) backs up and restores the `.rc`. Binary, scripts and state stay on `/data`, which updates do not touch. A clean flash loses the `.rc`; re-running the wizard puts it back.
10. **No Alexa neutralisation.** There are no Amazon apps. `start_server.sh` gets a LineageOS branch that skips the hide lists, the appops denial, the service stop list and the domain move.
11. **Root adb for the wizard:** the user enables USB debugging and "Rooted debugging" once in Developer options. The wizard then issues adb's `root:` service and reconnects, which the WebUSB client does not do today. The TWRP seeding of findings §8.5 stays documented for headless installs.
12. **The kiosk loses the microphone** while EchoMuse records (uid 0 is the assistant). That includes the HA dashboard's Assist button and Fully's acoustic detection. This is accepted and documented: EchoMuse is the Show's voice front end.

## 3. Capability set

| Capability | `lineage18_crown` |
|---|---|
| The eight required (SPEC §11.1) | yes; `alert_cache_v1` on the same wakelock condition as today |
| `alert_prefetch`, `local_wake_chime`, `open_rules_v1` | yes |
| `afe_metadata_v1` | yes (decision 6) |
| `ambient_light` | when the sensor reads (decision 8) |
| `screen_wake` | yes (Fully's REST API, as on Fire OS crown; with the companion app, the app lights the screen instead) |
| `leds`, `led_anim`, `buttons`, `button_hold` | no; firmware with the companion app announces them: an on-screen light bar and touch zones ([echo-show-companion-plan.md](echo-show-companion-plan.md) decision 8) |
| `screen_overlay` | with the companion app only; older firmware reads "firmware update available" |
| `wifi_control`, `ble_proxy` | no; dashboard reason "managed by the device" |
| `privacy_mute` | no; reason "mute not supported on this device yet" |

The ring, the action button and screen wake on this image are the companion app's, specified in [echo-show-companion-plan.md](echo-show-companion-plan.md).

## 4. Phases

### Phase L1: Detection and platform plumbing

- `platform.detect` per decision 2, including the SDK check on the Fire OS rule, with table tests for every rule and for a LineageOS image that must not reach the Fire OS path.
- `proto.PlatformLineage18Crown`, hello's platform, and the capability set of §3 in `supervisor` (platform injected through `Config` in tests).
- `cmd/server.go`: the LineageOS row of the platform matrix (same as Fire OS crown except for the audio backend, the volume table and ALS polling). Scratch and logs in `/data/local/tmp/echomuse`.

**Acceptance:** `go test ./...` and `go vet` green; a LineageOS probe fixture selects `KindCrownLineage`; a Fire OS fixture still selects `KindCrown`.

### Phase L2: AAudio backend

- `internal/aaudio`: a `dlopen` shim for the AAudio calls used by `device/tools/aaprobe/aaprobe.c` (builder, read/write, `getTimestamp`, state, xruns), with Go wrappers.
- `slmic.OpenAAudio()` implementing `pkg/mic.Microphone`: 1,280-frame reads, the AFE decoder on every period, stamps from a line fitted through `getTimestamp` pairs (decision 4), reopen on error or stall with drop counting as in `slmic/opensl.go`.
- `slspeaker.NewAAudio()` implementing `render.Sink`, stamped by decision 5.
- The fitting and stamping logic in pure Go with host tests: jitter, the 5 ms bias, wrap of frame counts, reopen, monotonic and never-late clamps.
- `device/tools/sl_latency` takes the backend as a flag and reads the volume through `androidvol`'s table instead of Fire OS's code 8.

**Acceptance:** host tests green; on the device, `sl_latency` over AAudio shows render stamps never late across five opens and a lag spread under 10 ms; the AFE decoder locks on every open.

### Phase L3: Platform glue

- `androidvol` code tables (decision 7), with parser tests for Android 11's replies.
- ALS polling (decision 8).
- `cmd/stats.go`: no Fire OS–only sources on this path.

**Acceptance:** on the device, a dashboard volume change moves `STREAM_MUSIC`, a key press moves the dashboard, and `lux` changes when the sensor is covered.

### Phase L4: Install, boot and update

- `start_server.sh` LineageOS branch (decision 10), chosen by `ro.lineage.version`.
- The `.rc` and the addon.d script (decision 9) as payloads, shipped like the existing ones.
- Wizard (`controller/static/dashboard.jsx`, `controller/em_api.py`):
  - `classifyDevice` accepts crown with `ro.lineage.version`, Android 11, and `/system/lib/libasp.so` at the NS6571 SHA-1 (`e3816eb3`). It refuses upstream builds, naming our ROM, and refuses without root, naming the Developer options to enable.
  - The WebUSB adb client issues `root:` and reconnects when the shell is not uid 0 (decision 11).
  - Step list: `connect_android → install_boot_hook (.rc + addon.d) → install_em → reboot → reconnect → verify_service → confirm_link`. `check_home`, `neutralize_alexa`, `wifi` and `mirror_boot_hook` are not applicable.
  - Diagnostics: `init.svc.echomuse`, `ro.lineage.version`, the `libasp.so` SHA-1, and whether the addon.d script is present.
- Docs: a LineageOS section in `docs/rooting.md` (TWRP, flashing our ROM, Developer options, undo) and in `docs/quickstart.md`.

**Acceptance:** wizard tests for the new classification and step list; a real wizard run on the Show ends with the service running and the device linked.

### Phase L5: Controller

- `DevicePlatform.LINEAGE18_CROWN`, admitted alongside `fireos6` and `fireos6_crown`.
- Capability gaps as for `fireos6_crown`; the ESPHome model name "Echo Show 8 Gen 1 (crown, LineageOS)".
- `tests/test_capabilities.py`: admission of the three platforms and refusal of others; the gaps.
- `docs/protocol-v1.md`: the platform value and the Show's volume scale per image. CLAUDE.md: the third platform.

**Acceptance:** pytest and mypy green.

### Phase L6: Device acceptance

- A cold boot, registration and approval, and a spoken turn through HA.
- Wake while TTS and music play.
- A dirty flash of the same LineageOS zip: the service comes back without the wizard.
- A 24-hour soak under capture: no audio-service restarts, no frame-counter gaps, no stamp jumps. Findings §8.3's 37 h and 74.6 h hazards need a longer run, recorded as open if not done.

## 5. Risks

| Risk | Mitigation |
|---|---|
| Our fork falls behind upstream LineageOS releases | The change is three files and their list entries; rebase both branches on each upstream release and re-run Phase L6 |
| A rebase changes the audio stack | The wizard checks the `libasp` SHA-1, and the dashboard shows the AFE decoder's health |
| The current firmware reaches a LineageOS Show | Decision 2's SDK check lands first, in Phase L1 |
| The kiosk's microphone features stop working | Documented (decision 12) |
| adbd listens on TCP 5555 once adb is enabled | Key authorisation stays on; documented in the rooting section |
| SELinux permissive, unofficial ROM | The user accepted this (2026-10-17); stated in the docs |
| The API 22 firmware binary has not run on Android 11 | Phase L1's first device run; probes built for API 30 and the init test service ran there |

## 6. Open questions

1. Platform name: `lineage18_crown` or a name without the version.
2. Whether the wizard should also offer the TWRP route (flash plus adb seeding) for users who do not want to use Developer options.
3. Whether the latency constant should be measured per build by a loopback at install time, or fixed per platform.
4. How users get the ROM: a release zip on the fork, or build instructions only.

## 7. Status (2026-10-17)

| Phase | State | Evidence |
|---|---|---|
| L1 Detection | done | `platform.detect` per decision 2 with table tests (a LineageOS image never reaches the Fire OS rule); `proto.PlatformLineage18Crown`; capability set of §3 (`supervisor_test` "lineage show"); the matrix row in `cmd/server.go`. On the unit: `Platform: lineage18_crown (crown_lineage, "LineageOS 18.1-20261017-UNOFFICIAL-crown")` |
| L2 AAudio | done | `internal/aaudio` (dlopen shim, blocking read/write), `slmic.OpenAAudio` (`fitClock`), `slspeaker.NewAAudio` (`aaClock`), host tests for both clocks; `sl_latency -backend`. Five opens: no late stamp, per-open medians within 1.7 ms, AFE decoder locked on every open ([findings §8.6](echo-show-crown.md#86-the-firmwares-aaudio-backend-on-our-rom-measured-2026-10-17)). `aaudioRenderLatencyNs` = 60 ms |
| L3 Glue | done, partly verified | `androidvol.Lineage18` (16/10/18, max 15) with Android 11 reply fixtures; `als.EnablePolling`; `stats.go` needed no change (`wlan0` counters and `/proc/net/wireless` read the same, no `wpa_cli`). On the unit: the volume check passed, keys moved the index while the firmware ran, `lux` read the room. Dashboard volume and lux through a controller session not exercised |
| L4 Install | done, wizard not run over WebUSB | `start_server.sh` `lineage` branch; `.rc` and `50-echomuse.sh` as wizard payloads; wizard classification, `root:` + reconnect, step list, diagnostics; docs. The wizard's install_boot_hook and install_em commands, run by hand over adb with the wizard's payloads, brought the service up at boot in `u:r:su:s0`; adb's `root:` answered "restarting adbd as root" and the reconnect was uid 0. A browser WebUSB run is still to do |
| L5 Controller | done | pytest, mypy and the wizard JS tests green on tracked files |
| L6 Acceptance | partly | Dirty flash of the same zip: `.rc` and addon.d script restored, service running at the next boot with no wizard (it also wipes the TWRP adb seeding, as `docs/rooting.md` says). Open: registration, approval and a spoken turn (the LAN controller predates `lineage18_crown` and refuses it with `protocol`); wake over TTS and music; the 24-hour soak |
