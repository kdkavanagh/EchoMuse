# Fire OS 6 port: plan

**Status:** implemented 2026-10-06. Phases 1–3 are verified on device G090LF0965260F1J; Phase 4 has been checked against the device by hand (see §6). The phase text below is the plan as written. §6 records where the hardware disagreed with it.
**Date:** 2026-10-05 (plan), 2026-10-06 (implementation).
**Scope:** run the existing firmware and provisioning flow, the "FireOS with root" path, on an Echo Dot Gen 2 that was unlocked with amonet-biscuit **v2.0.0** and therefore runs **Fire OS 6**. The native AFE stays in the path. emOS is out of scope.
**Not in scope:** measuring capture or playback quality again (ERLE, wake rates, endpoint latency), retraining wake models, and porting `tools/afe_probe` (it stays Fire OS 5 only).

**Fire OS 5 support removed (2026-10-10).** Every Dot now runs Fire OS 6, so this plan's decisions about keeping both platforms are history. What that removed:
- decision 1's runtime backend choice: the OpenSL ES backend (`internal/opensl`, the OpenSL halves of `slmic`/`slspeaker`) and `EM_AUDIO_BACKEND` are gone, and libmixerAPI (`internal/mixerapi`) is the only audio backend;
- the platform switch (`platform.FireOS6()`) and `start_server.sh`'s Fire OS 5 branch;
- decision 6's two values: `session.hello` `platform` is always `fireos6`, and the controller refuses a hello without it;
- Phase 4's Fire OS 5 side: its wizard step list, the `rootShell` helper, the Fire OS 5 build pin, the Magisk debloat script and its `pm hide` list;
- `tools/afe_probe`, which ran only on Fire OS 5.

The evidence and measurements below, including the comparisons with Fire OS 5, are unchanged.

## 1. Why this is a port, not a version bump

amonet v2.0.0 installs Fire OS 6 bootloaders, and Fire OS 5 no longer boots on top of them. R0rt1z2's thread says: *"Starting with version 2.0 of amonet, YOU CAN ONLY FLASH FIRE OS 6 FIRMWARE."* Going back to v1.1.0 is undocumented and risks a hard brick. Fire OS 6 is also more than Android 7. It is Amazon's headless "Puffin" image, with no Java framework at all.

## 2. Evidence (device G090LF0965260F1J, 2026-10-05)

| Fact | Observation |
|---|---|
| Build | Both slots: `Fire OS 6.5.6.9 (NS6569/6009)`, `ro.build.version.incremental=0011980470660`, identical fingerprint, `ro.build.version.sdk=25`, product `biscuit_puffin`. (The first wizard connect read `Fire OS 6574.1 (NS65741/8146)`, the build amonet v2.0.0's update file installs; the Dot was on NS6569 for all testing.) |
| Kernel / ABI | `3.18.19 armv7l`, 32-bit kernel; `abilist=armeabi-v7a,armeabi` |
| Layout | System-as-root: `/dev/root` (`dm-0`) mounted read-only, `androidboot.veritymode=disabled`, A/B with active slot `_a` |
| Root | boot-root.zip runs `adbd` as uid 0 in `u:r:su:s0`. That domain is **permissive**, which is boot-root's `sepolicy.rules`. There is no `su` binary and no Magisk. SELinux is otherwise **Enforcing**. |
| Framework | No zygote, no `audioserver`/`mediaserver`, no `/system/lib/libOpenSLES.so`. `pm`/`am`/`settings`/`svc` exist as files but have no framework behind them. |
| Firmware today | `server` built by the current `compile.sh` (API 22, NDK r21e) **starts on API 25** and fails at exactly one point: `capture: slmic: opensl: open libOpenSLES.so: dlopen failed: library "libOpenSLES.so" not found` |
| Audio owner | Amazon's `mixer` daemon (init service `mixer`, uid `audio` 1005, `u:r:mixer:s0`). It loads `audio.primary.mt8163_headless.so`, holds `pcmC0D23p`/`pcmC0D24c`, and runs ASP/LASP |
| Audio client API | `/system/lib/libmixerAPI.so`, a plain C ABI: `MixerOpenRec[Ch]`, `MixerGetBufRec[Timed]`, `MixerReleaseBufRec`, `MixerOpenPlay[Adv]`, `MixerGetBufPlay[Timed]`, `MixerReleaseBufPlay`, `MixerFlush`, `MixerDrain`, `MixerPause`, `MixerResume`, `MixerClose`, `MixerGetUnderflowMs`, `MixerSetupAsyncCallbacks`, … PuffinApp (Alexa) uses it |
| Hardware parity | tinymix controls 0–160 have the same index and name as `device/tools/tinymix_controls_output.txt` (241 controls in total vs 239). Inputs: `event1=mtk-kpd`, `event2=keys`. The LED (`is31fl3236` at `11007000.i2c/i2c-0/0-003f`), `amz_privacy`, `tsl2540` (`0-0039`), `h2w`, `/sys/power/wake_lock`, `/dev/stpbt` and `/dev/ptmx` are all present at the paths the firmware already uses |
| Differences | gpio444 is claimed by `amz_privacy` (`amz_priv_trig`), so the firmware's export fails. There is no `ip` binary (`ifconfig` and `ndc` exist). Wi-Fi is driven by Amazon's `wifisvc`, which runs `wpa_supplicant` (init service `p2p_supplicant`, same conf path and the same `/data/misc/wifi/sockets`) and spawns `dhcpcd` itself. The wall clock read 2010 before Wi-Fi was up (`sntpd` is Amazon's) |

### 2.1 The native AFE is reachable, and the AEC reference is kept by construction

The evidence comes from small C test programs. Each was built in the firmware's compiler image (NDK 21.4.7075529, `armv7a-linux-androideabi22-clang -O2 … -ldl`), pushed to `/data/local/tmp`, run from the root adb shell and removed afterwards. Each `dlopen`s `libmixerAPI.so`, resolves the calls below with `dlsym` and logs every return value, status, size and timestamp, while `logcat` shows the mixer's and the HAL's side. The prototypes were inferred from the NDK's `llvm-objdump` disassembly of the device's `/system/lib/libmixerAPI.so` (exports such as `MixerOpenPlay` at 0xc804 and `MixerOpenRec` at 0xd770) `[INFERENCE, confirmed by behaviour]`:

```c
void *MixerOpenRec(const char *type);            /* = MixerOpenRecCh(type, 1) */
void *MixerGetBufRecTimed(void *h, int *status, unsigned *size, uint64_t *ts);
int   MixerReleaseBufRec(void *h);
void *MixerOpenPlay(unsigned rate, unsigned ch, unsigned bits, const char *type); /* NULL → "Music"; mode is PLAYBACK_MODE_MUSIC (§6) */
void *MixerGetBufPlay(void *h, int *status, unsigned *size);
int   MixerReleaseBufPlay(void *h, unsigned bytes);
int   MixerClose(void *h);
```

- **Capture.** A program called `MixerOpenRec("micAsr")`, printed `MixerGetRate`/`MixerGetNumCh`/`MixerGetSampleSizeBits`/`MixerGetBufSize`/`MixerGetStreamName`, then made 20–40 reads with `MixerGetBufRecTimed`, logging status, size, `ts` and the mean absolute sample, and called `MixerReleaseBufRec` after each. The open produced `Add Record … t micAsr r 16000 c 1`. The HAL then opened `input_source=6`, which is `AUDIO_SOURCE_VOICE_RECOGNITION`, the same ASP selection the Fire OS 5 OpenSL path makes (`opensl.PresetVoiceRecognition`). Reads came back as 512-byte blocks (16 ms, 16 kHz mono S16) with a 64-bit timestamp. Bit 0 of every sample carries the AFE's per-frame metadata ([alexa-afe.md](alexa-afe.md), "AFE metadata in bit 0"); what EchoMuse does with it is in [afe-metadata.md](afe-metadata.md). Other record types exist: `micRaw`, `micMultiChAsr` (`mic_channels`/`ASR_out`/`ref_out`), `micHfp`, `micPstn`.
- **Credentials.** Run as uid 0 / gid 0, the program was rejected: every read returned status 110 (`ETIMEDOUT`) with no data, and logcat showed `Mixer_Utils:AddStream:Failed to open /data/mixer_streams//mstream…`. The library writes a mode-0660 descriptor that the mixer (uid 1005, member of group `aipc` 2901) cannot read. The same program was accepted after `setgroups` plus `setresgid(2901, 2901, 2901)` before the first library call (that run also dropped to uid `audio`). Every later program kept uid 0 with egid `aipc`, as the firmware does (§3, decision 3).
- **Contention.** The mixer serves one ASR mode at a time. Opening `micAsr` made it set `multi_ch_asr_disable=1`, which tore down PuffinApp's `micMultiChAsr` (PuffinApp logged `recordStreamFailed … status=112` and re-opened). PuffinApp then took the stream back, and our reads failed with status 112 (`EHOSTDOWN`). With `stop puffin`, capture ran cleanly.
- **Playback.** A program called `MixerOpenPlay(48000, 1, 16, 0)`, wrote 0.5 s of an 880 Hz tone through `MixerGetBufPlay`/`MixerReleaseBufPlay`, then called `MixerDrain` and `MixerClose`. The stream was added as `PLAYBACK_MODE_MUSIC` (`Input:Const:r 48000 n 1 Music`, primed with 4,800 bytes = 50 ms). The mixer routed it through `AlgoLoopback` (48→16 kHz resample) into ASP (`updateAudioConfigToAFE`). In other words, anything played through the mixer becomes the AFE's far-end reference without us doing anything extra. That is the Fire OS 6 equivalent of §4.1's "render through the native output path". The HAL reports `playback_first_sample_timestamps`.

So the Fire OS 6 path is: **firmware ⇄ libmixerAPI ⇄ `mixer` ⇄ HAL (ASP)**. This replaces **firmware ⇄ OpenSL ⇄ AudioFlinger ⇄ HAL (ASP)**. Raw ALSA stays off-limits, as §4.1 requires.

## 3. Decisions

1. **One firmware binary, with the backend chosen at runtime.** Use OpenSL when `libOpenSLES.so` resolves, otherwise libmixerAPI. There are no build tags and no second binary: the controller installs the one firmware bundled in its image on both platforms. The compiler pin does not move: the API 22 build was observed running on API 25.
2. **New package `internal/mixerapi`**, shaped like `internal/opensl`: dlopen'd, with a blocking `Read`/`Write` API, implementing the existing seams `pkg/mic.Microphone` and `render.Sink`. `cmd/server.go` picks the backend. The `slmic`/`slspeaker` call sites otherwise stay unchanged. The tinymix amp control (control 5) is shared.
3. **The process runs as uid 0 with primary gid `aipc`.** The init service declares `user root` and `group aipc audio system inet wifi net_admin net_raw bluetooth net_bt_stack wakelock input`, so the first group becomes the egid and the mixer can read our stream descriptors. No thread-level uid juggling. Verified: such a service runs as `uid=0 gid=2901(aipc)`.
4. **Boot integration is one init `.rc` plus an empty `/tmp`, in both system slots, written from Android.** On the active slot the root adb shell remounts `/` with toybox `mount -o rw,remount /` (the argument order matters: `-o remount,rw /` fails with `'/dev/root'->'/': No such file or directory`), writes `/system/etc/init/echomuse.rc` and `/tmp` (see §6), and remounts read-only. The inactive slot's `system` partition is mounted under `/data/local/tmp` for the same two writes, so a bootloader fallback still runs EchoMuse rather than stock Alexa. That write is safe: verity is off for both slots, since `androidboot.veritymode=disabled` comes from the shared amonet bootloader and both boot images carry the identical cmdline. The wizard logs both slots' fingerprints and warns when they differ; on this Dot they are identical, down to `libmixerAPI.so` and `mixer`. There is no TWRP step, no boot image or cmdline write, no Magisk and no sepolicy change. The service runs with `seclabel u:r:adbd:s0`: init may enter `adbd` (it starts adbd itself), and boot-root made that domain permissive. `seclabel u:r:su:s0` was tried and init refuses it (`avc: denied { transition } … scontext=u:r:init:s0 tcontext=u:r:su:s0`).

   The two slots are written at different times. `install_boot_hook` writes only the running slot. `mirror_boot_hook` writes the other one only after a boot from the first has reached the controller (`confirm_link`, Phase 4). Until then a bad install leaves the other slot stock, and the bootloader's fallback lands on a known-good Alexa image rather than a second copy of the fault.
5. **Amazon services are stopped, not deleted.** `start_server.sh` stops a denylist before it launches the server. Uninstalling means removing the `.rc` and `/tmp` from both slots (`docs/rooting.md`).
6. **One protocol addition.** `session.hello` carries `platform` (`fireos5` | `fireos6`; absent means `fireos5`), so the controller can run each platform's debloat and disable controls a platform lacks. Capabilities are unchanged; anything the hardware lacks is reported the same way it is today, for example `ambient_light` only when readable.

## 4. Phases

Each phase ends with an observable result on this Dot. Unit tests are added only where the logic is pure Go (block re-framing, timestamp mapping).

### Phase 1: Audio backend (`internal/mixerapi`)

1. Pin the prototypes in §2.1 against the disassembly for every symbol we call, including `MixerGetBufPlayTimed`, `MixerFlush`, `MixerDrain`, `MixerGetUnderflowMs` and `MixerSetupAsyncCallbacks`. Write them down in the package doc, labelled as inferred.
2. **Capture adapter.** Open `micAsr` and re-frame the 16 ms blocks into the 1,280-frame (80 ms) periods §4.1 requires. Each period is stamped `CLOCK_MONOTONIC`. Determine the clock domain of the mixer's `ts`: compare it with `clock_gettime(CLOCK_MONOTONIC/BOOTTIME)` at read time. If it is monotonic, backdate from it. If not, stamp at read completion, as OpenSL does today. Count drops on status `ETIMEDOUT`/`EHOSTDOWN`. Re-open on `EHOSTDOWN`, which means the mixer restarted or a stream was torn down.
3. **Render adapter.** Open a 48 kHz mono `MUSIC` stream. `Write` must block at playback rate. Use `MixerGetBufPlayTimed` (or `GetUnderflowMs`) for backpressure, because `GetBufPlay` did not block in the §2.1 playback test. Produce per-buffer completion `monoNs` for `render.progress` from the timed API, keeping §4.3's "estimated" quality. Map `Clear()` to `MixerFlush` and `Restart()` to close+open. *(Superseded: see §6. That test wrote only 0.5 s, less than the 800 ms a fresh stream accepts without blocking.)*
4. **Backend selection** in `cmd/server.go`. `EM_AUDIO_BACKEND=opensl|mixer` overrides it, for diagnosis.
5. **Gain staging (functional only).** The mixer applies its own stream gain (`persist.mixer.init.main.volume=70`, `AlgoRampGain`). Pick one volume authority: keep the firmware's DAC control 61, set the mixer's stream gain to unity once, and confirm that the mixer does not rewrite control 61. No loudness measurement.

**Acceptance:** with `puffin` stopped and `server` run by hand under `adb shell` (gid `aipc`), the Dot registers with the controller. A spoken wake word produces `wake.candidate`, the turn completes through HA, and TTS plays. `render.progress` and `render.finished` arrive. Wake during music playback is still detected, which shows that the reference path is live.

### Phase 2: Platform glue in the firmware

- **Mute.** Port upstream `7e80c4a` ("Mute: keep Amazon's privacy driver in step on FireOS 6 kernels"): find `amz_privacy` by glob, enter privacy through `privacy_trigger`, and reconcile at boot and after every press. Check what `micAsr` delivers while the driver is in privacy, since the mixer has `handleMuteStateChanged`, so the mute LED never shows over a live mic.
- **Wi-Fi.** On Fire OS 6, stop `wifisvc`. `internal/wifi` swaps `svc wifi disable/enable` for `wpa_cli -p /data/misc/wifi/sockets reconfigure` and runs DHCP itself, which is what `wifisvc` did. The conf format and ownership (`wifi:wifi 0660`) are unchanged. *(The supplicant does not survive at boot; see §6.)*
- **Bluetooth.** Stop `btmanagerd`, `BTSinkPlayer` and `blemesh_service` to free `/dev/stpbt`. Skip the `pm disable`/`settings put` calls when there is no framework.
- **Buttons, LEDs.** Paths are unchanged. Stop `acebuttond`/`aceinputmanager` in place of `acebutton`, and keep stopping `ledcontroller`.
- **ALS.** The Fire OS 6 kernel binds `tsl258x` to the `tsl2584tsv` at `0-0029` and exposes it over IIO (`/sys/bus/iio/devices/iio:device0/illuminance0_input`, read 21 lux). The `tsl2540` at `0-0039` has no driver. `bindings/als` matches only `tsl2540`/`als_lux`, so it needs an IIO reader keyed on the `name` attribute. The reading stays available with `ace_sensorsd` stopped.
- **Clock.** Confirm that `sntpd` sets the wall clock once Wi-Fi is up, before TLS verification and alarm scheduling. If it does not, the plan needs a time source, which is a decision to raise, not to improvise. *(It cannot; see §6.)*
- **`start_server.sh`** gets a Fire OS 6 branch, chosen by `[ -d /data/mixer_streams ]`:
  - Wait for `init.svc.mixer=running` instead of the `echoaudio` process.
  - `ifconfig p2p0 down` instead of `ip link`.
  - Stop the denylist in §4.1. The tinymix block is unchanged, because the indices match. The A/B `server_a`/`server_b` supervisor is unchanged.

#### 4.1 Service denylist (measured 2026-10-05)

Fire OS 6 has no APKs (`/system/app` holds only `Stk`, `/system/priv-app` only `CarrierConfig`, and there is no framework). The Fire OS 5 `pm hide` list therefore has no counterpart here. Everything is a native init service, so the denylist is a list of init service names. It is re-applied by `stop` on every boot from `start_server.sh`. Like the Fire OS 5 service list, it runs after boot settles. It replaces both `debloat_packages.txt` and `echomuse-debloat.sh` on Fire OS 6.

Measured on this Dot with every service below stopped:
- `MemAvailable` went from 316,600 to 420,852 kB, about +102 MB.
- `micAsr` delivered 80 of 80 reads and playback was accepted.
- `wpa_supplicant` survived `stop wifisvc`, and the IIO ambient light sensor stayed readable.

| Stop | Service names | Why |
|---|---|---|
| Alexa | `puffin` (38 MB), `puffinmrmd` (21 MB), `ahe`, `shs`, `dacd` | `puffin` takes the mixer's single ASR mode back (§2.1); the rest are its cloud and skill helpers |
| Cloud / smart home | `smarthomed` (26 MB), `commsd`, `uxeventd` (15 MB), `amakit_server`, `tokend`, `credmgrsvc`, `trackerd`, `UdssCampSvc`, `ace_dioded` | No consumer once Alexa is gone |
| Setup / OTA | `oobed_on_boot` (and the `dnsmasq` it spawns outside init: kill it), `provisionerd`, `otad`, `ace_otad`, `factory-reset` | Setup mode fights our Wi-Fi. OTA is already neutered by boot-root. `factory-reset` would wipe `/data`, and with it the firmware |
| Telemetry | `perfrecoveryd`, `ace_metricd`, `logmgr`, `acedropboxd`, `aceusagestatd`, `ace_coex_metric`, `dha_service` | Counterpart of Fire OS 5's `vitals_service`/`perfrecoveryd` stops |
| Hardware we own | `ledcontroller`, `acebuttond`, `aceinputmanager`, `ace_sensorsd`, `btmanagerd`, `BTSinkPlayer`, `blemesh_service`, `wifisvc`, `sntpd`, `avahi-daemon` | The firmware drives the ring, buttons, ALS, `/dev/stpbt`, `wpa_supplicant`+`dhcpcd`, NTP (`internal/timesync`; `sntpd` cannot work without `wifisvc`, see §6), and its own mDNS |

**Keep. These are required:**
- `mixer`.
- `perfmonitord`. The mixer makes a synchronous AIPC call to it on every stream open. With it stopped, each `MixerOpenRec` stalled for 20 s (`ACE-AIPC … pollConnect … epoll_wait timeout with timeout_ms: 20000`) before any audio arrived.
- `minerva_service` (AIPC endpoint 33). The mixer connects to it for metrics. No stall was seen without it, but it is kept because the mixer is its client.
- `shmd`, `servicemanager`, `dbus`, `ace_messaging`, `ace_eventmgr`. These are the shm, binder and lipc transport the mixer API runs on.
- `acepowerd`, `pwrsvcd`. The mixer's `power-resource-wrapper` acquires through them.
- `acethermald`, `netmgrd` (the `dnsproxyd` socket), `time_update` (restores `persist.sys.saved_time` at boot), `p2p_supplicant`, `wmt_launcher` (Wi-Fi/BT firmware), `logd`, `vold`, `keystore`, `kisd`, `rpmb_svc`, `securetime`, `debuggerd`, `ueventd`, `adbd`, `console`, `store_seed`.

Adding a service to the denylist requires repeating the test: stop it, then confirm that a `micAsr` open delivers audio promptly and that playback is accepted.

**Acceptance:** every hardware feature the dashboard exposes works on the Fire OS 6 Dot. That means the ring, mute in both directions including across a reboot, volume buttons, the action button, headphone detection, ALS, Wi-Fi join and change with rollback, and the BLE proxy. Anything that cannot work is shown disabled with a reason, per CLAUDE.md.

### Phase 3: Boot integration (verified 2026-10-05)

From the root adb shell: `mount -o rw,remount /`, write `/system/etc/init/echomuse.rc` (mode 0644; it is labelled `system_file` automatically), then `mount -o ro,remount /`:

```
service echomuse /system/bin/sh /data/local/bin/start_server.sh
    class late_start
    user root
    group aipc audio system inet wifi net_admin net_raw bluetooth net_bt_stack wakelock input
    seclabel u:r:adbd:s0
    disabled

on property:sys.boot_completed=1
    start echomuse
```

A reboot test with the same stanza ran the script as `uid=0 gid=2901(aipc) groups=2901(aipc),1005(audio) context=u:r:adbd:s0`, 11.7 s after boot. The same service with `seclabel u:r:su:s0` exited 127, with `avc: denied { transition } … tcontext=u:r:su:s0`.

**Acceptance:** a cold boot, with no cable and no adb, comes up talking to the controller. Deleting the `.rc` (remount, `rm`, remount) returns the Dot to stock behaviour.

### Phase 4: Provisioning wizard, Fire OS 6 flow (`dashboard.jsx`, `em_api.py`)

- **`connect_android`.** Accept `ro.build.version.release` 7.1.x **only** when `ro.product.name` is `biscuit_puffin` **and** the adb shell is already uid 0. Without uid 0, refuse and point the user at boot-root.zip. The 5.x path is unchanged. Keep the Fire OS 5 tested-build pin and add a separate Fire OS 6 pin (`NS6569/6009`, the build tested), each warning only on mismatch.
- Add a **`rootShell` helper**: `su -c` on Fire OS 5, a bare command on Fire OS 6. Every `su -c` call site goes through it.
- **Step list for Fire OS 6:** `connect_android → install_boot_hook (Phase 3, running slot only) → install_em (unchanged payload paths) → wifi (writes the conf only; the firmware brings Wi-Fi up) → reboot (snapshots the device's controller row first) → reconnect → verify_service (init.svc.echomuse and init.svc.mixer running) → confirm_link (the device's row was refreshed after the reboot, or it is connected) → mirror_boot_hook (the other slot, only now)`. The other slot is written last so that a bad install leaves a stock, known-good fallback slot rather than two broken ones. These steps do not apply and are shown as such: `connect_twrp`, `patch_boot`, `install_magisk`, `preseed_db`, `verify_root`, `disable_alexa`, `debloat`. Their jobs are covered by boot-root, by the `.rc` file and by `start_server.sh`. The Fire OS 5 init append that neutralises a `mixer` service must never reach Fire OS 6, where Amazon's `mixer` is the audio path.
- **Diagnostics probes** gain `getprop init.svc.mixer`, `getprop init.svc.echomuse`, `ls -l /data/mixer_streams`, the contents of `echomuse.rc`, and the active slot.

**Acceptance:** a full wizard run on this Dot, starting from the state it is in now (Fire OS 6 plus boot-root), ends with the device pending approval in the dashboard. A run on a Fire OS 5 Dot is unchanged.

### Phase 5: Docs and tests

- **`docs/rooting.md`**: add a Fire OS 6 section covering amonet v2.0.0, R0rt1z2's Fire OS 6 flash and boot-root.zip, plus what EchoMuse writes (one `.rc` on the active slot) and how to undo it. Correct the "FireOS 6 (Android 7.2)" line. Mirror the change in `docs/quickstart.md`.
- **`docs/native-afe-migration.md` / `post-afe-audio-architecture.md` §4.1**: name the mixer path as the Fire OS 6 realisation of the same rule.
- **`CLAUDE.md` / `device/CLAUDE.md`**: the platform matrix, the backend selection and the gid `aipc` requirement.
- **Tests**: `pm_verdict.test.mjs` for the Fire OS 6 platform classification and step list; `test_deploy.py` for the Fire OS 6 build pin in docs; Go unit tests for mixer block re-framing and timestamp mapping.

## 5. Risks

| Risk | Mitigation |
|---|---|
| An Amazon daemon restarts and takes ASR mode back (as PuffinApp did) | `puffin` is `disabled` in its `.rc`, so find and stop whatever starts it. The capture adapter re-opens on `EHOSTDOWN` and logs who opened (`Mixer_Connect` in logcat) |
| Inferred prototypes are wrong for a call we have not exercised | Phase 1 step 1. Each symbol is exercised on the device by a test program (§2.1) before it goes into the binding |
| `seclabel u:r:su:s0` refused by init | Resolved: `seclabel u:r:adbd:s0` (Phase 3) |
| Mixer gain or AVL normalisation (`LASP`, `AVL content type`) changes loudness relative to Fire OS 5 | Resolved (§6): at the mixer's default main volume (70) the output at the codec matches Fire OS 5's, so control 61 stays the single volume authority |
| Wall clock not set before TLS | Resolved (§6): TLS never verifies earlier than the build time, alarms use controller UTC, and `internal/timesync` sets the clock from NTP and writes the RTC |
| Bricking | No boot, LK or preloader writes in this flow. TWRP stays reachable (boot-root forces adb, and Vol-Up/`boot-recovery.sh` enter recovery) |

## 6. Where the hardware disagreed with the plan (2026-10-06)

Every item below was found or confirmed on G090LF0965260F1J. The code carries the evidence at the site. The test programs are those of §2.1: egid `aipc`, `puffin` stopped, and a 48 kHz mono `MUSIC` play stream (`MixerOpenPlay(48000, 1, 16, 0)`) whose chunks are 4,800 bytes (50 ms). A chunk's *handoff* is `CLOCK_MONOTONIC` taken after its `MixerReleaseBufPlay` returns.

- **Render timing (`mixerapi/mixerapi.go`, `mixerapi/playclock.go`).** `GetBufPlay` does block. A fresh `Music` stream accepts exactly 16 chunks (800 ms) instantly. After that, every round-trip waits about one chunk, paced by the DAC. A writer that lets `GetBufPlay` pace it therefore keeps 800 ms queued, and everything it plays, the wake chime included, is heard about 0.96 s after it is written. The first release did exactly that, and its local wake chime landed on the user's command (Kitchen turns 500–508, 2026-10-07). The Player now keeps the queue short and stamps completions from its depth:
  - It hands a chunk over only while fewer than 3 blocks wait in the mixer's queue, and `Write` holds back while a whole chunk waits in the Player. The depth is `mixer::DataTrans::GetNumBlkReady`, called on the stream handle and polled every 10 ms. The handle is the library's `DataTrans*`: `MixerGetBufSize`, `MixerGetUnderflowMs` and `MixerGetNumBytes` forward it unchanged to `DataTrans` members through the PLT.
  - A chunk starts no earlier than handoff + (blocks ahead − 1) × 50 ms + 20 ms. The head block may already be playing. A chunk also starts no earlier than the previous chunk's end. Reading the depth at every handoff tracks the DAC, so the estimate cannot drift.
  - The stream type that `MixerOpenPlay`'s fourth argument names (NULL = `Music`) sizes the client queue in `MixerOpenPlayAdv`: `Voip` 4 × 50 ms, `LineIn` 8 × 8 ms, `WHA` 64 × 16 ms, anything else 16 × 50 ms. It is also the mixer's volume class, so the Player keeps `Music`.
  - `MixerGetUnderflowMs` reads `DataTrans::GetResetUnderflowMs`, which resets the count on every read.

  `MixerGetBufPlayTimed`'s timestamp argument read back 0 on every call (3 s of a 440 Hz tone; buffers, sizes and pacing matched plain `GetBufPlay`), so it is not used. The first three measurements below established the first release's model: a blocked handoff plus 780 ms, and a fresh one plus 20 ms. Two runs each unless stated:
  - *Pacing.* 200 chunks of zeros (10 s), timing each `MixerGetBufPlay` + `MixerReleaseBufPlay` round-trip. Chunks 0–15 took ≤0.03 ms each. Chunk 16 was the first over 15 ms (31.5 ms). Of the 200, 16 took under 1 ms, 9 took 30–45 ms, 153 took 45–55 ms and 22 took 55–80 ms; the longest took 68.9 ms. In steady state the round-trips alternated between about 48 and 64 ms, averaging one chunk (40 chunks per 2,000 ms). One run.
  - *Handoff to sound.* A second thread recorded `micAsr` (`MixerGetBufRecTimed`, 16 ms blocks). Six bursts were played, each on a new play stream, with 0.8 s between streams. A burst is a chunk whose first 960 samples are a 1 kHz sine at amplitude 20,000 (20 ms). Bursts 0–2 followed 4 s of silence, so their handoff blocked. Bursts 3–5 were the stream's first chunk. Each burst was followed by 1.5 s of silence, `MixerDrain` and `MixerClose`.
    - The burst is found in the first capture block after the handoff whose peak |x| reaches max(4 × the largest peak in the first 40 blocks, 3,000). If no block reaches it, the loudest block within 2.5 s is used.
    - Its time is that block's `ts`, taken as the end of the block, minus the samples after the peak.
    - Result: 782–877 ms after a blocked handoff, and 89–130 ms after the first handoff of a stream opened right after another closed (6 bursts each).
  - *End to end*, the stamps the firmware actually sends. The probe used the firmware's own `internal/mixerapi` Recorder (`micAsr`, 1,280-frame periods) and Player (2,048-frame writes, completions from `playclock.go`). Sequence:
    1. Wait 1.5 s after capture opens.
    2. "Fresh": a burst write as the stream's first write, then 93 silent writes (~4 s).
    3. "Steady", four times: a burst write, then 34 silent writes (~1.5 s).
    4. A 2.5 s writer stall, so the mixer queue drains.
    5. "After a writer stall": a burst write, then 34 silent writes.

    The reference onset is the burst write's completion stamp minus one write (42.7 ms). The mic onset is the start of the 20 ms (320-sample) window with the largest sum of |x|, among capture samples stamped from 300 ms before to 1.5 s after it. Lag = mic − reference: +10.8 and +8.0 ms fresh, +54.5 to +89.2 ms steady (8 bursts), and +88.2 and +68.0 ms after the stall. Every lag was positive (the mic never appears to lead its own reference) and inside the 0–500 ms search.
  - *Queue depth (2026-10-07).* The same end-to-end probe, run through each platform's `slspeaker` sink and `slmic` capture: one silent 3 s lead-in, then eight to ten bursts 1.5 s apart, a 2.5 s stall and one more burst. "Write to mic" is the mic onset minus the time the burst's `Write` was called.
    - Fire OS 6, full 16-block queue: 957–986 ms, with the first release's stamps.
    - Fire OS 6, 3-block limit: 321–385 ms.
    - Fire OS 6, 2-block limit: 271–331 ms.
    - Fire OS 5 OpenSL sink, Office Dot: 356–371 ms. Its capture is stamped at read completion, so this figure is if anything late.

    With the depth model, lag was +59 to +99 ms behind 1 or 2 queued blocks and +63 to +77 ms after the stall. Counting the head block as well gave −0.3 to +48 ms behind 2. The 3-block limit stayed: it matches Fire OS 5 and leaves a late pump more margin. A 30 s soak at the 2-block limit underran only during the deliberate stall.
- **Gain matches Fire OS 5 at the mixer's default main volume.** The mixer scales `Music` by main volume. That is `persist.mixer.init.main.volume` (70), changed with `audio_manager_set_prop MainVolume`. Main volume is also the AFE's speaker volume index, which selects the output EQ and limiter.
  - *Test.* A −12 dBFS tone sweep (125 Hz–4 kHz, 2 s per tone) went through each platform's sink. It was read back at the codec input from the DL1 write-back (`tinycap -D 0 -d 9`). The I2S0 write-back, device 16, read all zeros on Fire OS 6.
  - *Main volume 70.* The Kitchen output was within 0.04 dB of the Office Dot's from 250 Hz to 4 kHz, and 1 dB lower at 125 Hz.
  - *Main volume 100.* The output was 13–18 dB louder from 500 Hz up.
  - *Codec.* Every tinymix control matched, apart from control 61 (127 vs 124, each Dot's own volume).

  The firmware leaves main volume alone, and control 61 stays the volume authority.
- **`MixerFlush` wedges a play stream.** In the pacing run, `MixerFlush` after the 200 chunks returned in 15.9 ms. The same stream then took 16 chunks instantly, and every later `GetBufPlay` waited 3.0 s. The Player closes instead and never calls `MixerFlush` or `MixerDrain`. Three rounds of open, 30 chunks of zeros (blocking from chunk 16) and `MixerClose` showed the close takes 1.3–1.5 ms with 800 ms queued. `render.Sink` has no `Clear`, so nothing needs a flush.
- **`MixerGetRate` races the open.** The capture programs printed rate, channels and bits right after open and again before close. Right after `MixerOpenRec("micAsr")` they can read `0xFFFFFFFF`; by close they read 16000/1/16. The same race appeared in 1 of 14 `micMultiChAsr` opens (`docs/alexa-afe.md`). The 16 kHz rate is fixed in code rather than read back. `micRaw` returns `ts=0`.
- **Capture `ts` is `CLOCK_MONOTONIC`, as planned (Phase 1 step 2).** With `puffin` stopped, a program opened `micAsr` and took `CLOCK_MONOTONIC` and `CLOCK_BOOTTIME` right after each of 15 reads.
  - `ts` is nanoseconds in the same range as both clocks. On the first read `ts` was 445,328,803,977, and the monotonic clock read 72.4 ms later.
  - Read completion − `ts` stayed between 72 and 78 ms.
  - The two clocks differed by under 1 µs, because the Dot had not suspended since boot, so this test cannot tell them apart.
  - The firmware treats `ts` as `CLOCK_MONOTONIC` at the end of its block (`mixerapi/reframe.go`). `alexa-afe.md` ("Stream format") has the same figures over longer captures.
- **Wi-Fi radio and DHCP.** `start_server.sh` stops `wifisvc` before it has powered the radio. At boot there is no `wlan0` and no supplicant (`wlan.driver.status=unloaded`). The firmware does what `wifisvc` would have done:
  - It writes `1` to `/dev/wmtWifi`, as MediaTek's `libhardware_legacy` `wifi_load_driver` does; `wlan0` appears within 1 s.
  - It runs `ctl.start wpa_supplicant`, the `wlan0`-only service. The control socket answers in about 3 s.
  - It runs `reconnect`, then `ctl.start dhcpcd-wlan0`. That is the service `wifisvc`'s network HAL starts (`/system/bin/dhcpcd wlan0 -AdLK`, `/init.mt8163_amazon.rc:270`). The AOSP-style `dhcpcd_wlan0` carries no interface and exits 1 within 57 ms; this init refuses `dhcpcd_wlan0:wlan0`.

  On KNET-IOT the lease came 0.6 s after association, and names resolve through `netmgrd`'s `dnsproxyd`. Success is judged by an IPv4 address on `wlan0`, never by init state. A restart flushes the old address first, because init SIGKILLs dhcpcd and leaves the address and routes behind.
- **Clock.** The RTC (`mt-rtc`) seeds the clock at boot and keeps counting across warm reboots. A never-set RTC reads 2010-01-01. Amazon's `sntpd` can never sync here: it waits for ACE NetMgr, which `wifisvc` serves (`/dev/aipc/1`), exits 1 after 20 s, and init restarts it every ~20 s forever. It is on the denylist. `internal/timesync` replaces it:
  - Once a default route exists, it runs Amazon's own client in one-shot mode (`sntp -o`, same servers, no NetMgr), then writes the RTC with `hwclock -w -u`.
  - It retries with backoff (10 s up to 10 min) and resyncs daily.

  Proven from a forced 2010 clock: NTP set the clock 3.7 s after the lease, and a warm reboot then came up on the right time. Nothing in EchoMuse depends on the wall clock: TLS verification is floored at the build time (`client.tlsNow`), and alarms run on controller UTC mapped to CLOCK_MONOTONIC. On a network with no NTP egress the clock stays at the RTC or saved value; setting it from the controller's trusted UTC would close that, and is not done.

  The step itself is not harmless to capture (2026-10-11). A newly added Dot on Fire OS 6574.1 booted 3m23s slow. The first sync stepped the clock forward 11 s after `micAsr` opened, and the stream never carried data again: one `ETIMEDOUT`, then status 14 on every read, with logcat `Mixer_DataTrans:InCapture-GetReadBuff:check bababeef state 0` and `MixerReleaseBufRec:Release Failed`. The firmware handled only 0, 110 and 112, so it logged 14 in a hot loop (15 MB in 4 min) and never re-opened. The wake model scored nothing, button turns carried no audio, and a restarted process captured normally. Its first boot, a 2010 → 2026 step, scored 3 wake hops in a whole session. A Fire OS 6.5.6.9 Dot ran through a 15h46m step unharmed, so it depends on build or timing. The firmware now opens `micAsr` only after `timesync.Settled` (bounded at 30 s, `cmd/server.go`), and the recorder re-opens on any status but 0 and 110, paced at 500 ms (`mixerapi`, "Wall-clock steps"). On the same Dot with that firmware (`fw-3554c2daa5e1`), `date -u @<now + 203 s>` reproduced the failure: `ETIMEDOUT`, then status 14, then one re-open and data again within the same second. Stepping back with `sntp -o` caused no error.
- **No `/tmp`, no `/sdcard`.**
  - `/tmp` does not exist. `start_server.sh` redirects the server into `/tmp/server.log`, so without it the server never runs. A plain `mkdir /tmp` is refused even from boot-root's permissive domain, because the new inode would be labelled `rootfs`, which policy will not associate with the ext4 system root (`avc: denied { associate } … permissive=0`). `install_boot_hook` therefore creates it as `mkdir -Z u:object_r:system_file:s0 /tmp`, and `start_server.sh` mounts a 32 MB tmpfs on it.
  - `/sdcard` is a dangling link to `/storage/self/primary`. With no framework, emulated storage is never mounted. The wizard stages Fire OS 6 pushes in `/data/local/tmp`.
- **SELinux domain.** The service starts in `adbd`, the only domain init will enter. There, `wpa_supplicant` (`wpa`, enforcing) may not send its control-socket replies (`avc: denied { sendto } … tcontext=u:r:adbd:s0`), so the firmware could not drive Wi-Fi. boot-root allows `adbd` to dyntransition into `su`, the root adb shell's domain, where every test program of §2.1 and of this section ran. `start_server.sh` makes that move in its own single-threaded shell (`echo -n u:r:su:s0 > /proc/self/attr/current`), and the server inherits it. The Go runtime cannot do it itself, because it is multithreaded.
- **Boot result.** A cold boot from the wizard's install brings `echomuse` up as `uid=0 gid=2901` in `u:r:su:s0` with the mixer backend. Wi-Fi joins on its own, NTP sets the clock, and mDNS finds the controller. The §4.1 denylist is stopped, and MemAvailable reads 415 MB.
