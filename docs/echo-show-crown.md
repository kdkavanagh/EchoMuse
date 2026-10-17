# Echo Show 8 (1st gen, `crown`): findings

**Status:** exploration, 2026-10-16, updated 2026-10-17. Everything below was read from one rooted unit, G6G16D12143203LD. Sections 1–7, 9 and 10 describe the unit on Fire OS 6574.1; the probes there were temporary, and the Phase 3 state of §9.5 was kept until the flash. **Since 2026-10-17 the unit runs LineageOS 18.1**, first v0.6 (§8.4–8.5), now a local build (`18.1-20261017-UNOFFICIAL-crown`) of lineage-18.1 that ships the NS6571 `libasp.so` with `libtensorflowlite_c.so` and `libedgeflow_core_neon.so` (amazon-oss/android_device_amazon_crown draft PR #2). On it `VOICE_RECOGNITION` carries v3.3 metadata again: 37,467 of 37,470 frames valid over 5 min. Root adb is seeded in `/system/build.prop` and `/adb_keys`; the Fire OS state, including §9.5, was wiped. NS6570 or NS6571 through TWRP is the way back (§8.5).
**Purpose:** compare the Show with the Echo Dot Gen 2 as documented in [fireos6-port.md](fireos6-port.md), [alexa-afe.md](alexa-afe.md), [afe-metadata.md](afe-metadata.md) and the other `alexa-*.md` notes. This feeds [echo-show-port-plan.md](echo-show-port-plan.md).
**Notation:** [INFERENCE] marks a conclusion that was not observed directly.

## 0. Summary

1. **The Show is Fire OS 6 with the full Android framework.** zygote, system_server, `audioserver` and surfaceflinger all run. The Dot's Fire OS 6 is Amazon's headless "Puffin" image. Architecturally, the Show is the Fire OS 5 Dot on Android 7.1.
2. **There is no `mixer` daemon and no `libmixerAPI.so`.** Amazon's AFE (`libasp.so`) runs inside `audioserver`, behind Amazon's audio HAL. Today's firmware has no audio backend that can run here.
3. **OpenSL ES with the `VOICE_RECOGNITION` preset reaches Amazon's ASR pipeline from a root native process.**
   - It delivers mono 16 kHz S16 with v3.3 metadata in bit 0, in the same layout the firmware's `internal/audio/afe` decoder already reads.
   - Playback through Android becomes the AEC reference automatically.
   - Measured echo reduction: about 43 dB on a tone, at least 28 dB on noise (limited by the room floor), 34–41 dB on bursts, and about 12 dB on a fast sweep (§4.4).
4. **Hardware differs from the Dot:**
   - 4 mics on two TLV320AIC3101 ADCs, read through a 6-channel stream whose channels 4–5 are a loopback of the DAC output.
   - An RT5616 playback codec.
   - No LED ring, no action button.
   - Mute is `amazon-gating`; ambient light is an `stk3x1x`.
   - A touchscreen and two cameras.
5. **Wi-Fi, Bluetooth, time and volume belong to the Android framework**, not to Amazon daemons.
6. **The device is unregistered.** Alexa's wake engine (`amazon.speech.sim`) waits for account registration, so nothing held the microphone in OOBE. Since §9.5 the OOBE is hidden, Fully Kiosk is HOME, and SIM stays installed (KnightSystemUI needs it) with its microphone denied through appops. Section 9 lists what is hidden and why.
7. **LineageOS 18.1 v0.6 for crown ships an older Amazon audio HAL and `libasp.so` than stock, with the current tuning set** (§8). Measured on this unit (§8.4, §8.5): `VOICE_RECOGNITION` from uid 0 reaches the ASR pipeline, but carries **no bit-0 metadata** and cancels 5–11 dB less than stock. uid 0 is the audio policy's assistant, so no app can silence it. AAudio's timestamps leave a constant echo offset of 88 ms, where OpenSL's lag moves 50–60 ms between opens. An init service in `u:r:su:s0` records at boot and survives updates through addon.d. The mute button gates the mic in the kernel, but a tap clears the light without restoring the mic.

## 1. Identity and boot

| | Echo Dot Gen 2 (Fire OS 6) | Echo Show 8 `crown` |
|---|---|---|
| Model / product | `biscuit_puffin` | `AEOCW`, product `crown`, `ro.build.lab126.project=crown_fireos_ship_6574.1` |
| Build | Fire OS 6.5.6.9 / 6574.1, API 25 | `Fire OS 6574.1 (NS65741/8146)`, incremental `0013222531716`, API 25, built 2026-09-15, security patch 2026-03-01 |
| Kernel | 3.18.19 armv7l | `4.9.77-ga3ad36a2995d-dirty`, armv7l, gcc 6.3.1 |
| SoC / RAM | MT8163 | MT8163, 4 cores (`possible 0-3`), 1,936,456 kB RAM; MediaTek hotplug (`/proc/hps`, §2.2) |
| Layout | A/B, system-as-root | Single slot. `root=/dev/ram` (ramdisk rootfs, mounted ro). `/system` is `mmcblk0p12` ext4 ro, `/data` is `mmcblk0p16`. Separate `boot`, `recovery`, `lk`, `tee1/2`, `persist`, `metadata` partitions |
| Root | boot-root.zip: adbd uid 0 in permissive `u:r:su:s0`, no `su` | Same pattern: adbd uid 0 in `u:r:su:s0` (`permissive=1` in avc), `ro.secure=0`, `ro.debuggable=1`, no `su` binary. SELinux enforcing otherwise |
| Boot state | `veritymode=disabled` | `veritymode=disabled`, `verifiedbootstate=yellow`, `unlocked_kernel=true`, `lk_build_desc=44072a3-20240709`, `pl_build_desc=1fce15f-20190917` |
| `/tmp`, `/sdcard` | neither exists | no `/tmp` (rootfs is a read-only ramdisk); `/sdcard` works (FUSE emulated storage) |
| OTA | neutered by boot-root | `com.amazon.device.software.ota` and `.ota.override` are disabled (state 2) |
| State | — | OOBE: `device_provisioned=0`, `user_setup_complete=0`, `persist.boot.oobe=1`, no Amazon account (MAP `accounts 0`). Offline until 2026-10-17; since then on Wi-Fi (§5) and still in OOBE |

## 2. System

### 2.1 Framework and services

- **Framework:** full Android 7.1 framework, with 181 packages, 130 of them Amazon or Ring.
  - Init defines 205 services; 50 were running.
  - The Dot's Fire OS 6 denylist ([fireos6-port.md §4.1](fireos6-port.md)) mostly does not apply. Of its stop and keep names, only `ahe`, `avahi-daemon`, `dbus`, `dha_service`, `p2p_supplicant`, `perfmonitord`, `perfrecoveryd`, `servicemanager`, `time_update`, `trackerd` and `wmt_launcher` exist here.
  - `puffin`, `mixer`, `wifisvc`, `sntpd`, `ledcontroller`, `acebuttond`, `btmanagerd` and the rest are absent.
- **Alexa is APKs**, as on Fire OS 5. Keeping it away from the hardware is a package problem (§9), not an init-service problem.
- **Memory:** MemAvailable was 986,780 kB with every stock app running. The Dot reads 316,600 kB stock and 420,852 kB after its denylist.

### 2.2 CPU hotplug: why only two cores are online

The kernel takes cores offline itself: MediaTek's `hps` governor is on (`/proc/hps/enabled=1`).

- **Settings:** `num_base_perf_serv=2` keeps two cores minimum. Another core comes online when online-core utilisation exceeds `up_threshold=80` for `up_times=2` samples, or on a rush boost at 98. Every `num_limit_*` is 4, so nothing thermal or power-related is capping it.
- **Utilisation during OOBE:** about 38% averaged over cpu0 and cpu1, nearly all of it `com.amazon.ds2.oobe.efd` (37% in `top`).
- **Thermals:** 26–36 °C across 12 zones.
- **Load average is misleading:** it read about 7, but six MediaTek kernel threads (`GCPU`, `hang_detect`, `display_esd_check`, `dynamic_boost`, `cpu_sched_thread`, `cpu_usage_thread`) sit in uninterruptible sleep permanently and count towards it without using CPU.

## 3. Hardware

| | Dot | Show |
|---|---|---|
| Mics | 7 mics on four TLV320ADC3101 ADCs, read as 9 ch `pcm24c` | 4 mics on two `tlv320aic3101` (`0-0018`, `0-0019`) behind an FPGA (`i2s_to_spi_6ch`). `pcmC0D22c` "TLV320AIC3101 Capture": 6 ch S24_3LE, 16 kHz. Ch 0–3 are mics, ch 4–5 a sample-aligned DAC loopback (measured by jxlarrea, §8.2). `/proc/idme/miccal.0-3` per-mic calibration |
| Speaker | TLV320AIC3204, `pcm23p` | `rt5616` (`2-001b`), `pcmC0D23p` "RT5616_Playback", 2 ch 48 kHz; external amp (`Ext_Speaker_Amp_Switch On`) |
| tinymix | 239–241 controls, DAC volume is control 61 | 181 controls, different indices (61 is `Audio FM I2S Volume`). Only `ADC_A`/`ADC_B`, MICPGA 40 (+20 dB). `start_server.sh`'s indices (5, 56, 61, 64, 88, 89…146) hit unrelated controls here |
| Lights | `is31fl3236` 12-LED ring | none; `/sys/class/leds` has only `lcd-backlight` |
| Display / touch | — | ILI9881C 800×1280 panel, rotated 90° (`ro.sf.hwrotation=90`); `fts_ts` and `amazon_touch` |
| Cameras | — | `camera_main` (`0-002e`) and `camera_sub` (`0-0021`), `cameraserver` running. Physical shutter: `SW_CAMERA_LENS_COVER` on `gpio-keys` |
| Buttons | `mtk-kpd`, `keys` (action, volume, mute) | `gpio-keys`: `KEY_VOLUMEDOWN`, `KEY_VOLUMEUP`, `SW_CAMERA_LENS_COVER`. No action button |
| Mute | `amz_privacy`, `privacy_trigger` | `amazon-gating` platform driver: `state` readable (0), `enable` write-only. The button reports key 116 (`KEY_POWER`) on `event0` "gating". A long press latches camera power off (jxlarrea, §8.2) |
| Ambient light | `tsl2584tsv` over IIO | `stk3x1x` at `0-0044`; `cat /sys/bus/platform/drivers/als_ps/lux` → `2 lux`. Not IIO, so `bindings/als` finds nothing. Also exposed through the Android sensor HAL (LIGHT, and an ACCELEROMETER) |
| Headphone | `h2w` | `h2w` (state 0) and `ACCDET` |
| Thermal | — | three `tmp103` sensors |
| Bluetooth | `/dev/stpbt`, owned by `btmanagerd` | `/dev/stpbt` held by `com.android.bluetooth` |

## 4. Audio software

### 4.1 Path

**Show:** app ⇄ AudioFlinger ⇄ `audio.primary.mt8163.so` + `libasp.so`, all inside `audioserver` (pid 261).
**Fire OS 6 Dot:** firmware ⇄ libmixerAPI ⇄ `mixer` ⇄ `audio.primary.mt8163_headless.so` + ASP.

- **Library:** `libasp.so` is 2,041,912 B, clang 3.8, built 2025-04-11; manager class `AspMgrEchoShowNougat`. It NEEDs `libtensorflowlite_c.so` and `libedgeflow_core_neon.so`, and loads `libAFEMetadataDecoder.so`. It registers the `audiosignalprocessor` binder service.
- **HAL source → ASP pipeline mapping** (`getAspPipelineType`, HAL 0xa1fb4):

  | Source | Pipeline |
  |---|---|
  | 6 `VOICE_RECOGNITION` | 0 `asr` (or 9 `asr_custom` with a stream-attribute flag) |
  | 1999 `HOTWORD` | 0 `asr` |
  | 7 `VOICE_COMMUNICATION` | 1 `voice` |
  | 1 `MIC` | 2 `mic` (`asp.cfg` defines no Mic pipeline) |
  | 2999 | 6 `swwref` |
  | anything else | −1 |

- **Echo reference:** playback goes through `asp_create_pipeline(3 = speaker)` from `PlaybackHandlerNormal`. That is the echo-reference writer, so anything played through AudioFlinger is the AEC reference. `asp.cfg` sets `"ERB settings": {"Matching type": "none"}`; the Dot used loopback correlation.
- **Raw capture:** the 6-channel PCM is opened by `audioserver` only while something records. All PCMs were closed at idle.
- **Framework limits:** the audio policy allows only mono or stereo input, so a framework client gets one ASR channel.
- **OpenSL:** `libOpenSLES.so` is the AOSP shim over `libwilhelm.so`.
- **Bit-0 metadata hook:** Amazon's `libaudioflinger.so` carries a decoder hook (`RecordThread_fosExt_metadata_decoder_report`), gated on source 6.

### 4.2 AFE tuning (`/system/vendor/etc/audio-algorithms/`, 35 files)

`AFE.cfg` `"Hardware Definition"`: `"Crown"`, 4 mics, 2 speakers; mics and ASR at 16 kHz, speakers at 48 kHz. There is no `Channel Map`.

| Item | Show | Dot (Fire OS 6 unless noted) |
|---|---|---|
| Filterbank | FFT 256 / hop 128 / 768 (8 ms) | same |
| AEC | `AEC_V2`, tail 160 ms, 2 references | same |
| Beamformer | `FBF_V2` 8 beams × 4 taps, `coefs_FBFV2_LowLatency_8beams.cfg` | same structure |
| `ARA_V2` pairing | beams i+2, i+4 | i+3, i+5 |
| Beam merger | `GroupBeamMergerV2` 3-beam groups, margin 1.26, hold 12 | same |
| VAD | DNN VAD (`vad_lite.tflite`, via TFLite/Edgeflow), then VAD | DNN VAD then VAD |
| False WW Prevention | absent (no block at all) | commented out (`AUDIOALG-52224`) |
| ASR output gain | +4.75 dB | +9.56 dB |
| Post-AEC mic gain | +1.9 dB | +4 dB |
| `selectAECOutputDelaySamples` | 168 | 168 |
| Per-volume tables in the ASR path | none | (Fire OS 5: False WW thresholds per volume) |
| Extra | `SpatialAudioProcessing` (disabled), `NDVC` (bypassed), `hamming512_earcon.cfg` | — |

The full stage order is `MicPreProcess → Downsampler IIR → HPF 80 Hz → FilterBank → AEC_V2 → NDVC → ARA_V2 → FBF_V2 → AECARAUtilsV2 → GroupBeamMergerV2 → DNN VAD → VAD → MicEnergy → OutputGain → EspFeatureExtraction → MicsPostAECOutputGain → selectAecOutputDelay → UED Fusion`. jxlarrea's timing measurements put the four mics within about 1.2 cm of each other along the top edge (§8.2), so on this unit the AFE's value is mainly AEC and NR, not beamforming gain [INFERENCE].

### 4.3 Recording presets (measured)

| OpenSL preset | logcat | Output |
|---|---|---|
| `VOICE_RECOGNITION` (3) | `ASP/Pipeline: Pipeline() mPipelineId = 0`; input 6 ch 24-bit 16 kHz, reference 48 kHz stereo, `ref_sync: 1`, `ReadValidateMicCal() Succeeded` | AEC'd mono; v3.3 metadata in bit 0 |
| `GENERIC` (1) | no capture pipeline (only speaker pipeline 3 on playback) | raw, no metadata |
| `UNPROCESSED` (5) | `getAspPipelineType(), Invalid input source (9)` | raw, no metadata |
| `VOICE_COMMUNICATION` (4) | `mPipelineId = 1` | heavy suppression (about −72 dBFS overall during playback), no metadata |

A root process (uid 0, `u:r:su:s0`) opened the recorder and delivered audio with no further privilege changes.

### 4.4 Echo cancellation and timing (measured)

**Method.**
- **Capture program:** a throwaway C program, built with the firmware's NDK (21.4, `armv7a-linux-androideabi22-clang`, `-lOpenSLES`). It records 16 kHz mono S16 through `SLDataLocator_IODevice` → `SLAndroidSimpleBufferQueue` (8 × 256-frame buffers) with `SL_ANDROID_KEY_RECORDING_PRESET`. It logs `CLOCK_MONOTONIC` at every buffer completion.
- **Playback:** the same program plays a 48 kHz stereo S16 stimulus through an OpenSL buffer-queue player (4 × 20 ms) on a chosen `SL_ANDROID_KEY_STREAM_TYPE`, starting 2 s after capture.
- **Stimulus:** 1 s silence, 4 s white noise at −18 dBFS RMS, 1 s silence, 3 s 1 kHz tone at −12 dBFS, 1 s silence, 4 s log sweep 100 Hz–7 kHz at −12 dBFS, 1 s silence, three 20 ms 1 kHz bursts at −6 dBFS spaced 1 s, 1.5 s tail.
- **Stream volume:** mostly the alarm stream (index 18/30, −22 dB in AudioFlinger), so the echo is measurable without changing any volume setting. One run used the music stream at its current index 12/30 (−36 dB).
- **Level measurement:** segment levels are RMS over each segment, offset by the measured 258 ms acoustic latency and trimmed 150 ms at each edge. The noise segment uses its last 2 s, after convergence.
- **Metadata decoding:** the firmware decoder's rule (`internal/audio/afe`): sync `0xA5`, version `0x63`, payload 104, popcount checksum.

**Levels (dBFS).**

| Segment | `GENERIC` (no ASP) | `VOICE_RECOGNITION` | Reduction | `ERLE_RAW` mean (max) |
|---|---|---|---|---|
| room, idle | −71.4 | −68.8 | (ASR output is about 2.6 dB hotter) | 0 |
| white noise (converged) | −44.4 | −70.0 | ≥ 28 dB, floor-limited | 16 (24) |
| 1 kHz tone | −36.5 | −76.5 | about 43 dB | 35 (43) |
| bursts | −31.2 | −63 to −70 | 34–41 dB | — |
| 100 Hz–7 kHz sweep | −29.9 | −39.0 | about 12 dB | 10 (27) |

- **Music stream run:** `VOICE_RECOGNITION` read −75.0 (noise), −75.6 (tone) and −60.4 (sweep).
- **Why the sweep is low:** the AEC adapts per band and the sweep keeps moving into bands that haven't converged.

**Metadata.**
- **Framing:** 2,725 of 2,726 frames valid. Every frame starts at a multiple of 128 samples from the first sample read, and `FRAME_COUNTER` steps +1 every time.
- **Flags:**
  - `PLAYBACK_ACTIVE`: 1.0 in every segment with energy, 0.7–0.85 in 1 s gaps (hangover), 0 before and after.
  - `VOLUME`: 40 in every frame. The Dot reads 70. 40 matches `dumpsys audiosignalprocessor` `"VolIndex":40`.
  - `DEVICE_MUTE`: 0. No divergence or clipping flags fired.
  - `DNN_VAD_PROB` peaked at 0.5 on the sweep.
- **Not exercised:** a talker.

**Timing.**
- **Playback to mic:** OpenSL buffer handoff to mic onset was 258.2 ms (`GENERIC`) and 258.6 ms (`UNPROCESSED`), identical for all three bursts. The Fire OS 6 Dot with a 3-block mixer queue measured 321–385 ms; the Fire OS 5 Dot 356–371 ms ([fireos6-port.md §6](fireos6-port.md)).
- **Capture:** the first buffer completed 203 ms after `SetRecordState(RECORDING)`. Buffer gaps were 14 ms median, 31 ms at p99.
- **Timestamps:** OpenSL provides no capture timestamp, but `AFE_TIMESTAMP` (`CLOCK_MONOTONIC` ms mod 65,536) does. A frame's buffer completed 7–41 ms (median 18) after the frame's `AFE_TIMESTAMP`. That gives the firmware a monotonic anchor for capture, the role the mixer's `ts` plays on the Dot.
- **Re-read for the firmware's render clock (2026-10-17, same logs, capture completions fitted against the sample count):**
  - The 258 ms is a fresh player's first sample. Its handoff was the prefill of all four buffers before `SetPlayState(PLAYING)`. The track took three at once, then nothing for 184–186 ms (the output leaving standby), then about one per 16 ms FastMixer cycle (`HAL frame count` 768). So the mixer first read sample 0 about 168 ms after the handoff, and it was heard about 92 ms after that.
  - In steady state a buffer enqueued behind three others was heard 221–230 ms after its enqueue. That is 44–60 ms of queue and the track's 60–80 ms ring (it took three buffers at once, not four), leaving 81–126 ms.
  - In the two `VOICE_RECOGNITION` captures with playback (5,450 frames) a frame's `AFE_TIMESTAMP` sat 0–27 ms (median 15) before the fitted completion of its last sample, and 7–42 ms before the raw completion. Against a straight line, `AFE_TIMESTAMP` scattered −12 to +15 ms.
  - `internal/opensl`'s `playPipelineNs` (40 ms) is derived from these figures.

## 5. Network, clock, Bluetooth

- **Wi-Fi:** the framework's WifiService drives it.
  - `wpa_supplicant` runs as `wifi` with sockets in `/data/misc/wifi/sockets`. No networks were saved during the exploration. On 2026-10-17 the user joined one from the device (`wlan0` 192.168.3.159) while it stayed in OOBE.
  - There is no `wifisvc`. `ip`, `iw`, `wpa_cli`, `ndc` and `ifconfig` all exist.
  - Interfaces: `wlan0`, `p2p0`, `ap0`. `com.amazon.wifi.sync` can push Wi-Fi credentials from the Amazon account.
- **Clock:**
  - There is no `sntpd`. `auto_time=1`, but `config.disable_networktime=true`.
  - The RTC (`mt-rtc`) read the build date while offline. Android floors the clock to the build time, which is why the device showed 2026-09-15.
  - After joining Wi-Fi, the clock read the correct date (2026-10-17 03:11 GMT). The time source was not identified.
- **Bluetooth:** `com.android.bluetooth` (bluedroid) holds `/dev/stpbt`.

## 6. Stock Alexa voice stack

| Item | Show | Dot docs |
|---|---|---|
| Wake models | On-device ONNX under `/system/local/models` (21 `.onnx`), plus `NTT/` and `PMA/`. en-US ALEXA is one shared encoder (`primed_encoder.int8-streaming.onnx`) with decoders `dvad`, `sid`, `ww_stop`, `ww_alexa`; 64 LFBE bins, 80–7200 Hz | Kaldi/NeMoRt models, `final.mdl`, 20 bins |
| en-US ALEXA thresholds | accept 0.92, notify 0.25, AlarmState 0.65, AudioPlayerState/audio_playback 0.84 | 0.7 / 0.1 / 0.5 / 0.5 |
| Natural Turn Taking | shipped and enabled (`supports_ntt`, `preload_ntt_decoder`), including a camera active-speaker mode | not shipped |
| Wake host | `amazon.speech.sim` via `libGalactus_jni.so` (AVS Device SDK). `AudioInputManager.configure(6, 16000, …)` on the beamformed channel | `SpeechInteractionManager` + `libwakewordserver_jni` |
| Endpointing | server for wake-word turns, device for button turns | same |
| Alerts | `com.amazon.alarms` (`RingingService`); alarm sounds bundled in the APK | HeadlessBeacon, `/system/local/share/earcon` |
| "Light bar" | on-screen Lottie animations inside SIM | 284-file LED corpus |
| Capture today | none: SIM logs `AccountRegisteredCondition: isMet: false` | — |

## 7. Display stack

- **HOME candidates:** `com.amazon.ds2.oobe.efd/.MainActivity` (priority 101), `com.amazon.paladin/.app.MMLauncherActivity` (50), `com.android.settings/.FallbackHome` (−1000).
- **Screensaver:** `com.amazon.paladin/com.amazon.displayclockface.dream.ClockDream` (`screensaver_enabled=1`, activates on sleep and dock).
- **WebView:** `com.amazon.webview.chromium` 118.0.5993.155. It won't update while Amazon OTA is disabled.
- **Keyboard:** the only IME is Amazon's `com.amazon.bluestone.keyboard/.DictationIME`, which holds `RECORD_AUDIO`. There is no AOSP keyboard in the image.
- **Settings and SystemUI** are Amazon builds (`KnightSettings`, `KnightSystemUI`).

## 8. LineageOS 18.1 for crown

### 8.1 What the port ships

- **Repos and release:** `amazon-oss/android_device_amazon_crown` (lineage-18.1), `android_hardware_amazon` (lineage-18.1) and `android_device_amazon_mt8163-common`. Release `lineage-18.1-crown-v0.6` came out 2026-10-14.
- **Kernel:** a 4.9 kernel taken from Amazon's Android 9 MT8163 tablets, with Android 9 blobs.
- **Audio blobs:** `proprietary-files.txt:3-65` lists Amazon's `audio.primary.mt8163.so` (installed as `audio.primary_amazon.mt8163.so`), `libasp.so`, `libaspclient.so`, `libmediaextension.so` and all of `audio-algorithms/`. The HAL and the three libraries are **older** than this unit's stock: device a5b8429 and vendor 393ae56 pinned them to a Fire OS from before Alexa Plus. That `libasp.so` is 1,120,316 B with no TFLite dependency; stock's is 2,041,912 B. Only the `audio-algorithms` configs are NS6571 files. §8.4 has the hashes read on the device.
- **Audio wrapper:** `vendor.prop` sets `ro.hardware.audio.primary=amazon_wrapper`, and `hardware/amazon/audio/hal/legacy/` wraps the Amazon HAL.
  - `5dca03c` (2026-09-27), "Run Amazon's capture processing for mic captures": remaps `MIC`/`CAMCORDER` to `VOICE_COMMUNICATION`, because "the HAL has no pipeline for plain mics". It is in v0.6 and cannot be switched off there: the switch, `amazon_noise_suppression=off`, came with `80a06c0` (2026-10-15), and v0.6's wrapper has no such key (§8.4).
  - `27b5346` (committed 2026-10-14 19:01Z, after the v0.6 release at 15:40:58Z) passes the voice-call volume to ASP, but only in the Android 9 (`v28`) wrapper; crown builds the legacy one. `VOICE_RECOGNITION` is not remapped.
  - The same batch reverted the +40 dB mic PGA boost (`d7cb939`). v0.6's raw mic level equals stock's (§8.4).
- **Camera:** configured in the device trees since `3b57c27` / `a2c1cb1` (2026-09-04).
- **Port notes:** SELinux permissive, deep sleep disabled, the mute button doubles as the power button, the battery is faked at 100%.

### 8.2 jxlarrea's measurements (`jxlarrea/lineageos-echo-show-camera`)

Measured on `lineage-18.1-crown-v0.5`, before the 09-27 wrapper change:
- **Mic stream:** the FPGA stream is 6 ch S24_3LE, 16 kHz, period 257. Ch 0–3 are mics, ch 4–5 a DAC loopback at sample alignment that drops to exactly zero when playback stops.
- **No echo cancellation on MIC captures:** the HAL kept channel 0 only and applied no AEC to `MIC` captures. His claim that ASP is "only called from the playback handler" was made with recorder apps, which use the `MIC` source. That source has no ASP pipeline on stock either (§4.3).
- **His fix:** a WebRTC AEC shim that uses ch 4–5 as the far end removed 38–44 dB of echo, at a cost of about 9.5 dB of talker loss during double talk.
- **Mic gain:** a +40 dB mic PGA clipped 24% of samples during loud playback.
- **Array geometry:** the four mics are within about 1.2 cm in projection, and beamforming gave no SNR gain.
- **Mute latch:** a long press on the privacy button latches camera power off until mains is removed (patches 0015/0016).

### 8.3 Open

§8.4 answers the old questions (ASR routing, metadata, root capture under Android 11's rules, the ASR path without Fire OS's volume plumbing). Still open:
- Double talk with a real talker over playback, and how to detect it without `DTD` (no metadata on v0.6).
- Capture policy with an assistant role holder set (§8.4's table holds only while none is), and beside a privacy-sensitive capture such as a WebView `getUserMedia`.
- AAudio `setPrivacySensitive(true)`, and the two long-run hazards AudioPolicy11 read in the sources: the HAL's 32-bit capture counter wraps after about 74.6 h of capture (AAudio stamps then go wrong until the stream is recreated), and audioserver aborts on `mRsmpInRear` overflow after about 37 h unless patch `frameworks/av/0004` is in. Neither long run was made.
- Whether a later build (80a06c0, newer Amazon blobs) brings back the v3.3 metadata or stock's AEC depth.
- Suspend with the screen off and no capture, off USB.

### 8.4 Measured on lineage-18.1-crown-v0.6 (2026-10-17)

Same unit, after the flash: `18.1-20261014-UNOFFICIAL-crown`, API 30, userdebug, SELinux permissive, adb root (uid 0, `u:r:su:s0`), setup wizard on screen, no Wi-Fi.

**Method.**
- **Probes:** `slprobe` (OpenSL, 8 × 256 frames; also 4 × 1,280), `aaprobe` (AAudio, `PERFORMANCE_MODE_NONE`, I16, 1,280-frame reads) and `sl_latency`. Stimuli, segments and trims as in §4.4 and §10.1. Besides `decode.py`, bit 0 was scanned for sync `0xA5` at every phase, with a histogram of the version and payload that follow.
- **Clients:** uid 0 from the adb shell. uid 1000 and the Recorder's uid 10127 came from a throwaway `setgroups`/`setgid`/`setuid` + `exec` helper built with the NDK clang (not in the repo), because uid 0's non-VR captures are silenced (Q3).
- **Volume:** set with `cmd media_session volume --stream 3 --set N`, read back with `service call audio 16 i32 3`. `sl_latency` reads the index with Fire OS code 8, which is `adjustSuggestedStreamVolume` here, so it ran with a `PATH` shim that forwarded only `service call audio 8` to code 16 and refused everything else.
- **Cleanup:** `/data/local/tmp` was empty before and after. `STREAM_MUSIC` went back to 5, and `stay_on_while_plugged_in` back to 0 (it was set to 1 for the Recorder test). The two test recordings and their MediaStore rows were deleted, and so were the Recorder's `shared_prefs`, which the test launch had created. Logcat was cleared between runs.

**Blobs on the device** (correction to §8.1):

| File | Size, B | sha1 |
|---|---|---|
| `/system/vendor/lib/hw/audio.primary_amazon.mt8163.so` | 1,902,676 | `bb4f93e1…`, the pinned blob |
| `/system/lib/libasp.so` | 1,120,316 | `f72d0891…`, the pinned blob |
| `/system/lib/libaspclient.so` | 38,544 | `81df145c…` |
| `/system/lib/libmediaextension.so` | 17,940 | `296480dc…` |
| `/system/vendor/lib/hw/audio.primary.amazon_wrapper.so` | 10,140 | `1d081f37…` |
| `audio-algorithms/AFE.cfg`, `asp.cfg` | — | `efd88d62…`, `b86a635f…` |

- **`libasp.so`:** it NEEDs no TFLite or Edgeflow library, and the image has none. Its only metadata strings belong to the `MetaDataFile` debug dumps (`meta_*.wav`). No `audiosignalprocessor` binder service is registered.
- **The wrapper** has no `amazon_noise_suppression` key and no "noise" string at all. So `GENERIC` was recorded as remapped, and `UNPROCESSED` from uid 1000 is the raw reference.

**Q1/Q2: routing, format and metadata.** Every request was 16 kHz mono S16 and was granted as asked; thread and track were both 16 kHz mono PCM16, so no converter ran.

| Preset (client) | AudioFlinger input | HAL source → ASP (logcat) | Output |
|---|---|---|---|
| `vr`, OpenSL, 256 or 1,280 frames (uid 0) | `AUDIO_INPUT_FLAG_FAST`, HAL frame count 80 (5 ms), FastCapture yes, `Timestamp stats: n=0` | 6 → `ASP/ASR: PipelineAsr … in 6 6 16000 out 1 1 16000 ref delay 0`, `ASP/ER: RegisterReader (asr)` | AEC'd mono. First buffer 202–210 ms after `RECORDING` (stock 197–206); gaps p50 14, p99 34–35, max 44 ms |
| `vr`, AAudio (uid 0) | flags 0, HAL frame count 320 (20 ms), FastCapture no, timestamps n > 0 | same | same; first read after 250–255 ms |
| `generic` (`MIC`) | as above, per API | **7** (the wrapper's remap) → `asp_command code=5` (`Current volume 30`), `code=3` (VoIP mode), `ASP/Conversion: PipelineVoice … ref 1` | uid 0: all zeros (`Sil s`, Q3). uid 1000: the VoIP output |
| `vc` | as above | 7 → `PipelineVoice` | as `generic` |
| `unprocessed` | as above | 9 → `ASP: Invalid pipeline type: -1` | uid 0: zeros. uid 1000: raw mono, no ASP; first buffer after 457–521 ms |

- **`VOICE_RECOGNITION` reaches the ASR pipeline,** pipeline 0 by the HAL's mapping. This libasp has no `mPipelineId` log line; it logs the pipeline class instead.
- **There is no bit-0 metadata of any version.**
  - `decode.py` found 0 valid frames in every capture: 0/1,226 in 10 s, 0/23,726 in 190 s, 0/1,220 through AAudio.
  - The scan found no phase where more than 13 of about 1,220 frames start with `0xA5`; chance alone gives about 10 at the best of 128 phases. `0xA5` sat at 584 bit offsets, against 613 expected from random bits, and bit 0 has no periodicity (autocorrelation ≤ 0.017). Bit 0 is audio.
  - So `AFE_TIMESTAMP`, `PLAYBACK_ACTIVE`, `ERLE_RAW`, `DTD`, `DNN_VAD_PROB`, the clip flags and `VOLUME` do not exist on v0.6. The firmware's `afe` decoder and `slmic`'s anchoring have nothing to read (`sl_latency`: `anchored=0 at-completion=440`).
- **The echo reference is aligned by loopback correlation,** although `asp.cfg` says `"Matching type": "none"`. On every VR or VC open with playback, the log runs `ASP/ER: Loopback signal is zero. Cannot synchronize yet`, then `No match found`, then `Synchronized. Slice n. Removing 7–453 samples`, 1.2–3.2 s after playback starts.
- **ASP volume:** `setSpeakerVolumeIndex : 30` (`Vol_dB = -43 dB`) on every VC or `MIC` open, and nothing on `STREAM_MUSIC` changes. `ASP_CMD_GET_AFE_PLAYBACK_LATENCY` reports 36.2 ms for the speaker pipeline (`PipelineSpeaker in 1 2 48000`).

**Q4: echo cancellation and mic level.**
- **Method:** `stimulus.py loud` through an OpenSL player on `STREAM_MUSIC`, with §10.1's segments and trims. Burst onsets came 219–228 ms (raw) and 279–291 ms (VR) after enqueue in slprobe's completion convention; stock's were 226 and 285.
- **Table:** levels in dBFS, as raw (`UNPROCESSED`, uid 1000) / residual (`VOICE_RECOGNITION`, uid 0) / reduction in dB.

| Index (AudioFlinger dB) | Pink, converged | 1 kHz tone | Sweep top | Gated pink |
|---|---|---|---|---|
| 6 (−28) | −33.9 / −57.4 / 24 | −35.4 / −73.0 / 38 (floor) | −39.2 / −51.4 / 12 | −34.9 / −61.6 / 27 |
| 9 (−17) | −22.6 / −41.2 / 19 | −23.4 / −59.2 / 36 | −27.8 / −28.3 / 1 | −24.8 / −47.0 / 22 |
| 12 (−8.5) | −21.0 / −41.6 / 21 | −15.5 / −59.8 / 44 | −19.6 / −13.5 / −6 | −23.8 / −41.9 / 18 |
| 15 (0) | −19.5 / −39.2 / 20 | −12.9 / −56.6 / 44 | −15.3 / −8.2 / −7 | −23.0 / −37.1 / 14 |

- **AudioFlinger's attenuation** (`Stream volumes in dB`, stream 3) follows AOSP's speaker curve, (1, −58) (20, −40) (60, −17) (100, 0), over 0–15, as Platform11 predicted. Stock's curve is on record at one point only, 12/30 = −36 dB (§4.4), so the comparisons below match raw acoustic level instead.
- **The mic gain and the speaker path at full scale are stock's.**
  - Raw at 15/15 against stock at 30/30: pink −19.5 / −20.0, tone −12.9 / −12.4, sweep top −15.3 / −15.8, gated pink −23.0 / −23.1 dBFS.
  - The limiter above −8.5 dB is there too: from 12 to 15, pink rose 1.5 dB and the tone 2.6 dB.
  - The mic never clipped: raw peaks were −14.4, −9.4, −4.4 and −2.0 dBFS, with no full-scale sample. jxlarrea's +40 dB PGA (§8.2) is not in v0.6.
- **Cancellation is weaker than stock: the residual sits 5–11 dB higher at equal acoustic level.**
  - Lineage 12 against stock 24 (raw pink −21.0 / −21.9): residual pink −41.6 / −48.4, tone −59.8 / −66.4, gated pink −41.9 / −52.7.
  - Lineage 15 against stock 30: pink −39.2 / −48.6, tone −56.6 / −61.8, gated pink −37.1 / −41.9.
  - Reduction: pink 19–24 dB (stock 27–29), tone 36–44 (stock 37–49), gated pink 14–27 (stock 19–29).
- **The uncancelled sweep comes out hotter:** at 15 the VR sweep top sits 7 dB over raw (stock +2 at 30), and the VR output hits full scale on 1,029 samples at 12 and 5,473 at 15, all in the sweep (stock: 189 at 30).
- **Idle floor:** raw −59 to −68 dBFS (the room varied during the session), VR −69 to −73 (stock: −69 raw, −69 to −72 residual).
- **`VOICE_COMMUNICATION`** (`PipelineVoice`, uid 1000; what the Recorder and every app `MIC` capture get since 5dca03c):
  - idle −47 to −50 dBFS: its AGC lifts the room 10–20 dB over raw;
  - during the programme it ducks: pink −60/−52/−40/−73, tone −48/−42/−43/−46, gated pink −59/−65/−66/−65 dBFS at 6/9/12/15;
  - that is 13–53 dB under raw and not monotonic in volume. Against VR it is lower on the noise programmes at 9–15 (gated pink −65 against −37 to −47) but higher on the tone (−42 to −48 against −57 to −73) and at idle: suppression with AGC rather than cancellation [INFERENCE].
- **Volume keys:** `KEYCODE_VOLUME_UP`/`DOWN` with nothing playing are dropped (`MediaSessionService: Nothing is playing on the music stream. Skipping volume event`).

**Q3: capture policy.**
- **(a) A uid 0 `VOICE_RECOGNITION` recorder is allowed,** with no AppOps or permission step.
- **(b) uid 0 is the audio policy's assistant.**
  - No app holds `android.app.role.ASSISTANT` (`dumpsys role`), and `voice_interaction_service` and `assistant` are unset.
  - So `updateAssistantUId` hands its initial `assistantUid = 0` to `setAssistantUid` (LineageOS frameworks_base lineage-18.1 `AudioService.java:1684-1735`); `UidPolicy` also starts at 0.
  - `updateUidStates_l` (`AudioPolicyService.cpp:474-704`) then treats our recorder as the assistant on TOP. It silences every other capture that is not the assistant's or an accessibility service's, and it allows the assistant only `VOICE_RECOGNITION` and `HOTWORD`.

| First (holding the input) | Second | Second gets | First gets |
|---|---|---|---|
| our VR (uid 0) | uid 0 VR | audio | audio, untouched |
| our VR | uid 0 `generic` | zeros (100 %, `Sil s`) | untouched; the source stays 6 |
| our VR | app uid 10127 with the Recorder on top: OpenSL `generic`, then `vr` | zeros, both | untouched: 2,737 buffers in 44.0 s |
| our VR | the Lineage Recorder recording (MediaRecorder, `MIC`, AAC 8 kHz) | a 9.5 s file of digital zero | untouched: 2,489 buffers in 40.0 s |
| the Lineage Recorder recording | our VR | zeros while ours runs, audio again after it stops (the file: 7 s audio, 9 s zero, 7 s audio) | audio, but from **`PipelineVoice`** (−33 to −57 dBFS, AGC'd): the HAL stream was opened as `VOICE_COMMUNICATION` |
| app uid 10127, OpenSL `generic` (FAST) | our VR | dead: our start re-patched the input (source 7 → 6), and its `AudioRecord` logged `restoreRecord_l … dead IAudioRecord` three times, then `processAudioBuffer: Error -32 … giving up`. It stayed stalled after ours stopped | audio, from `PipelineVoice` (−39 to −47 dBFS) |

- **This is the opposite of AudioPolicy11's prediction** that a later foreground app silences us. Nothing an app did silenced our VR, and ours silenced every app in either start order. It holds only while no assistant is set [INFERENCE: with an assistant role holder, uid 0 is an ordinary TOP client and that prediction applies].
- **What does go wrong is sharing.** The ASP pipeline is fixed when the HAL stream opens; the re-patch at each start (`input_source=7` or `6`) does not recreate it. So a VR client that joins an input an app opened with `MIC` gets the VoIP pipeline, without the ASR output, until the input goes to standby. A FAST app recorder can be killed by the re-patch.
- **Other uids:** uid 0 cannot record `MIC`, `VOICE_COMMUNICATION` or `UNPROCESSED` at all (silenced as the assistant). As uid 2000, OpenSL fails with `Request denied by app op: 27`, because the shell uid is not in the foreground. uid 1000 works.
- **Silencing gives zeros, not an error:** no callback and no status; `dumpsys media.audio_flinger` shows `Sil s`, and bit 0 is lost. Android 7.1's "the second recorder gets no buffers" (§10.2) no longer applies.
- **(c) Screen off**, with §10.3's method: one 190 s VR capture, `KEYCODE_SLEEP` at 6 s, `KEYCODE_WAKEUP` at 166 s.

| | Screen on | Screen off (160 s) |
|---|---|---|
| `mWakefulness`, display | `Awake`, `ON` | `Asleep`, `OFF` |
| Capture | running | continuous: 11,863 buffers, 189.81 s of audio in 190.04 s, max gap 44.4 ms, 16,000.1 samples/s |
| Wake locks | `PARTIAL_WAKE_LOCK 'AudioIn' (uid=1041 ws=WorkSource{0})`, `mHoldingWakeLockSuspendBlocker=true` | same |
| `suspend_stats` success | 0 | 0 |

Then 60 s with the screen off and no capture: no wake locks and `mHoldingWakeLockSuspendBlocker=false`, but still no suspend (`success: 0`; on mains and connected over USB).

**Q6: timestamps.**
- **OpenSL has none,** and on LineageOS it takes the FAST path (`flags 0x1`, FastCapture, `Timestamp stats: n=0`), so the HAL's capture position is never read. Without metadata there is no `AFE_TIMESTAMP` either.
- **AAudio capture** (`PERFORMANCE_MODE_NONE`, `VOICE_RECOGNITION`):
  - `getTimestamp` succeeded on 747 of 747 reads in 60 s; the implied rate is 16,000.13 Hz (+8 ppm). AudioFlinger's own stats for the thread: n 1,307, err 183.
  - Each read's stamp scatters 15–16 ms p-p (sd 3.6–4.2 ms) around a straight line, which is one 256-frame ALSA period [INFERENCE]. A fit removes it.
  - Completion lag (read return − stamped capture time of the read's last frame): min −7.9, median −4.9, max +12.7 ms. A negative lag is physically impossible, so the stamp runs about 5 ms late. The median was −4.5 to −5.0 ms over five opens; `UNPROCESSED` +4.2 ms, `MIC` and `VC` −8.8 ms.
  - The record track's `Latency` column read −9 to +19 ms with `t`, so the HAL clock is CLOCK_MONOTONIC, as predicted.
- **AAudio render:**
  - The first timestamp after start is 72–96 ms off; every later one lies on a single line at exactly 48,000 Hz (the anchor stays constant to 0.1 ms over 19 s).
  - The 10 priming writes before start return `AAUDIO_ERROR_INVALID_STATE`.
  - aaprobe's `-m` render rate (−137 to −168 ppm) is an artefact of that first stamp.
- **Acoustic lag left after both timestamps:** `stimulus.py echo` through AAudio `USAGE_MEDIA` at index 9, 1 kHz burst onsets, both timestamp streams fitted.

| Capture | Run 1, ms | Run 2, ms | Run 3, ms |
|---|---|---|---|
| `UNPROCESSED` (uid 1000) | 52.3–52.8 | 52.7–53.1 | 53.0–53.3 |
| `VOICE_RECOGNITION` (uid 0) | 87.5–88.0 (101.7) | 87.6–89.3 (102.7) | 86.5–89.1 |

In brackets: the third burst, detected late at 21–24 dB SNR once the AEC had converged on 1 kHz. Using each read's own stamp instead of the fit scatters the raw lag by 5–9 ms.

- **The firmware's OpenSL backend** (`sl_latency`; four runs with a tone at index 9, one with noise at index 12):
  - Every period was stamped at completion, since there is no anchor.
  - `pcm` lag median per run: +183.7, +128.2, +174.9 and +125.2 ms with the tone, +180.6 with noise. Within a run the bursts spread 20–65 ms; the empty-queue burst read +111 to +130.
  - Stock (§10.7): +110.4 median (95–137) with a tone, +158.6 with noise.
- **Recommendation: AAudio for both directions.**
  - Settings: `PERFORMANCE_MODE_NONE`, I16, `dlopen("libaaudio.so")` (the firmware targets API 22).
  - With render and capture timestamps, the echo sits at a constant offset from predicted playout: 53 ms raw, 88 ms VR, within 1 ms (raw) and 3 ms (VR) across three opens once both streams are fitted.
  - OpenSL has lost both of its anchors here (FAST capture without timestamps, and no `AFE_TIMESTAMP`), and its lag moved 50–60 ms between opens.
  - The 88 ms needs one loopback calibration per build [INFERENCE: it holds ASP's reported 36 ms speaker latency, the amp, the air path and the ASR pipeline].

### 8.5 Platform on lineage-18.1-crown-v0.6 (measured 2026-10-17)

**Flash.** `amonet-crown-v2.0.1`'s kaeru LK boots recovery from `swdl` (p11), where the unit already had TWRP 3.7.0_9-0 (gabontz "OTA-guard" build, SHA-1 `73d91718…`); the `recovery` partition (p10) holds Amazon's NS65741 recovery and is not used. `adb reboot recovery`, then `twrp wipe cache|dalvik|system|data`, `twrp install` of the release zip (SHA-256 `fd25da98…5891e`; it writes only `system` and `boot`). TWRP rebooted itself once, about 20 s after first entering it.

**adb at first boot without the screen.** The build is userdebug with `ro.adb.secure=1` and `persist.sys.usb.config=none`. Before the first boot, from TWRP: `persist.sys.usb.config=adb` and `service.adb.root=1` appended to `/system/system/build.prop`, and the host's `adbkey.pub` installed as `/adb_keys` (`adb_keys_file`). First boot came up with root adb (uid 0, `u:r:su:s0`). adbd also listens on TCP 5555 (`init.mt8163.rc`), key-authenticated. Without these edits a person must enable USB debugging and "Rooted debugging" on screen, and `adb root` must be repeated after each reboot.

**Detection.** `ro.product.device=crown` on both images. LineageOS: `ro.lineage.version=18.1-20261014-UNOFFICIAL-crown`, `ro.build.version.sdk=30`, `ro.build.version.name` empty; `/system/lib/libOpenSLES.so` and `libaaudio.so` present, no `libmixerAPI.so`, ABI list `armeabi-v7a,armeabi`. **Today's `platform.Detect` would classify LineageOS as Fire OS crown** (device name plus OpenSL), and the Fire OS volume check's `service call audio 10 i32 3` is `setStreamVolume` here [INFERENCE from the transaction table; not run]. Detection must test `ro.lineage.version` or the SDK level first.

**Volume.**

| | Fire OS crown (§10.5) | LineageOS |
|---|---|---|
| `STREAM_MUSIC` range | 0–30 | 0–15 (AOSP default; no `ro.config.media_vol_steps`) |
| get / set / max codes | 8 / 3 / 10 | 16 / 10 / 18 (`setStreamVolume(int,int,int,String)`) |
| Round trip | — | `service call audio 10 i32 3 i32 7 i32 0 s16 com.android.shell` → `service call audio 16 i32 3` read 7 |
| Shell alternative | — | `cmd media_session volume --stream 3 --set N` works; `--get` prints only to logcat (`System.out`: `volume is 5 in range [0..15]`) |
| `settings get` cost | ~1 s | 30 ms |
| `settings_system.xml` | plain XML | plain XML; `volume_music_speaker` and the mtime follow a set within 3 s, a `cmd media_session` set not yet after 1 s |
| Volume keys | adjust `STREAM_MUSIC` | adjust `STREAM_MUSIC` with the screen awake or while music plays; ignored with the screen off and nothing playing (6 → 5 on `KEYCODE_VOLUME_DOWN` while playing) |
| ASP `VOLUME` | follows the index | no metadata on v0.6 (§8.4) |

**Boot hook.** A test service in `/system/etc/init/emtest.rc` (`user root`, `group root audio`, `seclabel u:r:su:s0`, started on `sys.boot_completed=1`) ran in `u:r:su:s0` with gid `audio` and recorded `VOICE_RECOGNITION` through AAudio at boot (60 reads, 0 xruns, room floor −68 dBFS). No `adbd` → `su` move is needed: `su` is a defined, permissive domain in this userdebug policy. `/` was remounted rw with `mount -o remount,rw /` (no verity, no overlayfs).

**Surviving an update.** With an addon.d v2 script `/system/addon.d/50-emtest.sh` (Lineage's `backup_file`/`restore_file` for `etc/init/emtest.rc`, plus copying `/adb_keys` and re-appending the two adb properties in `post-restore`), a dirty flash of the same zip in TWRP (`backuptool.sh` backup → `block_image_update` → restore) kept the `.rc`, the addon.d script, `/adb_keys` and the properties; the service ran again on the next boot and adb stayed root. A clean flash (system wiped) keeps nothing. Test files were removed afterwards; the adb properties and `/adb_keys` remain, unprotected by any addon.d script.

**Mute button** (pressed by the user, `VOICE_RECOGNITION` capture of 4 s after each step, `/sys/devices/platform/amazon-gating/state`, gating dmesg and `getevent`).

| Step | Light | `state` | Kernel | Capture |
|---|---|---|---|---|
| Baseline | off | 0 | — | −67.9 dBFS, 6.8% zero samples |
| Hold 1.9 s | red | 1 | `disable MIC with gating mode callback`; pinctrl `mic-disable` | −84.4 dBFS, 97.3% zero |
| Tap 0.13 s | off | 0 | press events only, no callback | **100% zero** |
| Hold 1.5 s | off | 0 | `enable MIC`; pinctrl `mic-enable` | −57.3 dBFS, 3.6% zero |
| Hold, then `adb reboot` | red | 1 after boot | `Failed to enable CAM, GATING Mode is ON`; `cam_pwr_gate_pin` out high | 100% zero |
| Hold after the reboot | stayed red | 1 | no interrupt counted on the `gating` IRQ | 100% zero |
| Tap, then hold 1.5 s | off | 0 | tap: events only; hold: `enable MIC` | −59.3 dBFS, 4.0% zero |

- **Muting is real and survives a reboot:** the latch holds and the capture is digital silence.
- **A tap un-mutes the light but not the mic.** The latch clears in hardware on any press, but the kernel restores the mic GPIO only on a long press, so after a tap the light is off and capture stays at zeros. A firmware on LineageOS must treat `state=0` with a zero-signal capture as muted, or the user must be told to hold the button.
- The button sends `KEY_POWER` (input device `gating`, `Generic.kl`): a tap sleeps or wakes the screen, a hold opens the power menu.
- One hold after the reboot registered no interrupt at all; the retry worked. Unexplained.
- After the reboot `cam_pwr_gate_pin` stayed high (camera power off) once un-muted, jxlarrea's camera symptom (§8.2); not investigated.

**Ambient light.** The Android sensor list has one sensor, "Light Sensor" (`amazon-oss`). The stock sysfs path exists: `/sys/bus/platform/drivers/als_ps/lux`, format `N lux`. With nothing listening it read `0 lux` three times; after `echo 1 > enable_als_polling` it read `15 lux`, and the value stayed cached at 15 after `echo 0`. A reader on LineageOS has to enable polling (or register for the sensor); today's reader would report 0.

**Screen off.** §8.4: 190 s of `VOICE_RECOGNITION` capture, 160 s of it with the screen off, had no gap over 44.4 ms; the device did not suspend (on mains and USB).

## 9. Alexa takeover surface: what to hide

The list comes from `dumpsys package` (granted permissions, persistent and privileged flags), `cmd package query-receivers -a BOOT_COMPLETED`/`LOCKED_BOOT_COMPLETED`, `cmd package query-activities -c HOME`, and init service state. Roles marked [INFERENCE] come from package names, not behaviour. Tier 1 was applied on 2026-10-17 and corrected by what it broke; §9.5 has the measurements. Tier 2 has not been applied.

**Ways Amazon can take something back:**
- the **microphone** (any `RECORD_AUDIO` or `CAPTURE_AUDIO_HOTWORD` holder that starts recording; Android 7.1 allows one active input);
- the **speaker** (alarms, multi-room, media);
- the **screen** (OOBE, launcher, screensaver);
- **undoing our changes**: packages holding `CHANGE_COMPONENT_ENABLED_STATE`, `INSTALL_PACKAGES`/`DELETE_PACKAGES` or `MASTER_CLEAR`, plus remote device management;
- **radios**: BLE mesh, Sidewalk, Wi-Fi sync.

### 9.1 Tier 1: must hide

| Package | Flags | Why |
|---|---|---|
| `amazon.speech.sim` | persistent, boot; `CAPTURE_AUDIO_HOTWORD`, `RECORD_AUDIO`, `WRITE_SECURE_SETTINGS`, `SYSTEM_ALERT_WINDOW` | **Not hidden; microphone denied instead** (§9.5). Alexa wake engine and on-screen attention UI; captures once registered |
| `com.amazon.comms.multimodaltachyonarm` | boot; `RECORD_AUDIO`, `CAMERA`, `WRITE_SECURE_SETTINGS` | calling / Drop In [INFERENCE] |
| `com.amazon.zordon` | boot; `RECORD_AUDIO`, `CAMERA`, `WRITE_SECURE_SETTINGS` | Alexa visual / camera features [INFERENCE] |
| `com.amazon.rtcsessioncontroller`, `com.amazon.a4b.conferencing.chime`, `com.amazon.comms.knightmessaging`, `com.amazon.obscura` | `RECORD_AUDIO`, `CAMERA` | calls, messaging, photo [INFERENCE] |
| `com.amazon.device.gadgetscontrolmanager` | `RECORD_AUDIO`, `CAPTURE_AUDIO_OUTPUT`, `BLUETOOTH_PRIVILEGED` | Alexa Gadgets |
| `com.amazon.ds2.oobe.efd` | HOME priority 101; boot | setup; owns the screen until provisioned. (`com.amazon.kindle.otter.oobe` was listed here; it has no HOME activity and Settings needs it, §9.5) |
| `com.amazon.paladin` | HOME priority 50, boot, `CHANGE_COMPONENT_ENABLED_STATE`; screensaver `ClockDream` | Amazon launcher |
| `com.amazon.bishop` | persistent, boot | Amazon home UI [INFERENCE] |
| `com.amazon.ambienthome` | boot, `CHANGE_COMPONENT_ENABLED_STATE`, `WRITE_SECURE_SETTINGS`, `SYSTEM_ALERT_WINDOW` | photos/ambient screen; can re-enable components |
| `com.amazon.spotlight` | boot | home content [INFERENCE] |
| `com.amazon.alarms` | boot | Alexa alerts ringer; EchoMuse owns alarms |
| `com.amazon.whad` | persistent | whole-home (multi-room) audio [INFERENCE]; with init service `whad_cc` |
| `com.amazon.alexa.externalmediaplayer.fireos` | — | Alexa media playback |
| `com.amazon.device.rdmapplication` | boot; `INSTALL_PACKAGES`, `DELETE_PACKAGES`, `CHANGE_COMPONENT_ENABLED_STATE` | remote device management |
| `com.amazon.d3` | boot; `INSTALL_PACKAGES`, `REBOOT` | — |
| `com.amazon.venezia` | boot; `INSTALL_PACKAGES`, `DELETE_PACKAGES` | Appstore / app updates |
| `com.amazon.adep`, `com.amazon.alta.h2clientservice` | boot; `CHANGE_COMPONENT_ENABLED_STATE` | — |
| `com.amazon.diode`, `com.amazon.settings.windfall` | boot; `MASTER_CLEAR`, `REBOOT` | could factory-reset `/data`, and with it EchoMuse |
| `com.amazon.csapp` | `REBOOT`, `WRITE_SECURE_SETTINGS` | — |
| `com.amazon.device.software.ota`, `.ota.override`, `com.amazon.device.smarthome.ota` | — | keep OTA disabled (the first two already are) |

### 9.2 Tier 2: recommended

These free memory and stop cloud, telemetry and radio use. Hide only after a Tier 1 baseline passes §9.4.

- **Alexa account and cloud:** `com.amazon.alexa.identity`, `com.amazon.alexa.datastore.app`, `com.amazon.alexa.beaconbroadcaster` (persistent), `com.amazon.alexa.timeoutmanagerapp` (persistent), `com.amazon.tcomm` (persistent), `com.amazon.device.messaging`, `com.amazon.imp` (persistent), `com.amazon.neodelegate` (persistent), `com.amazon.banyan.core` (persistent), `amazon.speech.davs.davcservice` (model downloads), `com.amazon.assetsync.service`, `com.amazon.a4b.mcc`, `com.amazon.ods.kindleconnect`, `com.amazon.mlv`, `com.amazon.smarthomemapviewapp`, `com.amazon.afe.app` ("Tap to Alexa", not the AFE).
- **Smart home, Matter, BLE mesh, Sidewalk, provisioning:** every `com.amazon.device.smarthome.*` (`adapters.ble` and `adapters.echo` are persistent), `com.amazon.whisperjoin.*`, `com.amazon.wifi.sync`, `com.ring.halo.messaging`, `com.amazon.gloria.smarthome`. These are Amazon cloud features layered on top of Android's Wi-Fi and Bluetooth stack: `wifi.sync` pushes credentials from the Amazon account, `whisperjoin` does Frustration-Free Setup over BLE, and the rest use the radios for Amazon's mesh. Hiding them should leave the device's own Wi-Fi and Bluetooth in charge [INFERENCE]; §9.4 verifies that.
- **Telemetry:** `com.amazon.device.metrics`, `com.amazon.device.logmanager`, `com.amazon.device.crashmanager`, `com.amazon.logan`, `com.fireos.usagestats.proxy`, `com.fireos.arcus.proxy` [INFERENCE: remote config].
- **Native init services, stopped per boot:**

  | Service | Binary or role |
  |---|---|
  | `ahe` | `AlexaHybridExecutionControllerLitespeed`, maps `libpryon.so`, 38 MB |
  | `shchipd`, `shblemeshd`, `shlocalskillsd`, `smarthomewifid` | smart home |
  | `halo-bssid-scan`, `halo-hub-broker` | Ring / Sidewalk |
  | `meshmgrservice` | — |
  | `neo-coordinator`, `neo-init` | — |
  | `inlocservice` | — |
  | `trackerd`, `touchlogger`, `dha_service`, `wifi_log_levels` | telemetry |
  | `whad_cc` | with `com.amazon.whad` |

  `sidewalk-init` is already stopped.

### 9.3 Keep

| Keep | Why |
|---|---|
| `android`, `com.android.systemui`, `com.android.settings`, the providers, `com.android.packageinstaller`, `com.android.shell` | framework |
| `com.amazon.webview.chromium`, `com.amazon.webview`, `com.amazon.knightwebview` | the kiosk's web engine |
| `audioserver`, `media`, `cameraserver`, `aal` (MediaTek display brightness from `aal_als`) | audio, camera, display |
| Wi-Fi: framework WifiService, init `p2p_supplicant`/`wpa_supplicant`, `netd`, `dhcpcd`, `wmt_launcher`, and the Wi-Fi pages of `com.android.settings` | the device owns Wi-Fi in the EchoMuse configuration; nothing here is stopped or replaced |
| Bluetooth: `com.android.bluetooth` on `/dev/stpbt`, and its Settings pages | the device owns Bluetooth and BLE; EchoMuse's BLE proxy does not run on the Show |
| `com.amazon.knight.ecs` (persistent), `com.amazon.knight.hds`, `com.amazon.device.echoaudioservice` | unknown role on a framework the Knight builds of Settings and SystemUI may depend on; test before hiding |
| `com.amazon.NativeAccessorProxyServices` | a SIM client; crashes with an on-screen dialog when `amazon.speech.sim` is hidden (§9.5). Harmless with SIM present |
| `amazon.speech.sim` | KnightSystemUI binds to it and crash-loops without it (§9.5). Kept installed with `RECORD_AUDIO` denied through appops |
| `com.amazon.kindle.otter.oobe` | KnightSettings' `SettingsApplication` reads its launched-countries resource at start-up; hidden, every Settings page crashes (§9.5) |
| `com.amazon.bluestone.keyboard` | Amazon's dictation IME; keep installed, `RECORD_AUDIO` denied through appops. A replacement keyboard (Simple Keyboard, F-Droid `rkr.simplekeyboard.inputmethod` 6.7) is the default IME |
| `com.amazon.knight.btavrcp` (persistent, `RECORD_AUDIO`) | Bluetooth AVRCP / A2DP sink [INFERENCE]; Tier 2 candidate once its mic use is understood |
| `perfmonitord`, `perfrecoveryd`, `securetime`, `kisd`, `healthd` | `perfmonitord` stalled the mixer on the Dot when stopped; keep until measured here |
| `com.ivona.*`, `com.svox.pico` | TTS engines, inert unless called |

### 9.4 How and in what order

- **Package hiding:** use `pm hide <package>` (root). It persists in `/data/system/users/0/package-restrictions.xml`, makes a package invisible to resolvers including persistent restarts [INFERENCE], and is undone with `pm unhide`. It is the Fire OS 5 debloat precedent.
- **Native services:** stop them at every boot from the start script, so `/system` keeps no edits for them.

**Order**, which matters because removing every HOME handler leaves only `FallbackHome`:
1. Install the kiosk app (Fully Kiosk) and a replacement keyboard.
2. Set `settings put global device_provisioned 1` and `settings put secure user_setup_complete 1`.
3. Make the kiosk the preferred HOME, and confirm with `cmd package resolve-activity -c android.intent.category.HOME -a android.intent.action.MAIN`.
4. Hide Tier 1, reboot, verify.
5. Then Tier 2, reboot, verify.

**Verification after each tier:**
- the §4.4 `VOICE_RECOGNITION` probe still yields pipeline 0 and metadata;
- the kiosk comes up as HOME after a cold boot;
- the kiosk's camera motion wake works;
- Settings and the volume keys work;
- the device still owns connectivity: Settings joins another Wi-Fi network and back, pairs a Bluetooth device, and both pages stay reachable from the kiosk;
- root adb stays reachable;
- `dumpsys media.audio_flinger` shows no input other than ours;
- `ps` shows no hidden package running after 10 minutes.

### 9.5 Tier 1 applied (measured 2026-10-17)

**What was done**, in §9.4's order: Fully Kiosk Browser 1.61.4 (free, sha256 `42df2da1…3dec7`, from fully-kiosk.com) and Simple Keyboard 6.7 (F-Droid, hash checked against the index) installed with `adb install`; the keyboard enabled and set (`ime enable`/`ime set`); the provisioned flags set; Tier 1 hidden with `controller/device_payloads/crown_hide_packages.txt`; rebooted with `adb reboot`.

**Fully's HOME activity ships disabled.** `de.ozerov.fully/.LauncherReplacement` does not appear in `cmd package query-activities -c HOME` until it is enabled (Fully's own launcher setting, or `pm enable de.ozerov.fully/.LauncherReplacement`); then it is offered at priority 0. `cmd package set-home-activity` prints `Success`, but `resolve-activity` keeps returning the OOBE (a priority-101 system activity wins over a preferred activity) until the OOBE and `paladin` are hidden. After hiding, HOME resolves to Fully and Fully is in front after every reboot.

**What the first Tier 1 broke, and the fix:**

| Symptom | Cause | Fix |
|---|---|---|
| `com.android.settings` dies at start (`NameNotFoundException: com.amazon.kindle.otter.oobe`, from `oobe.commons` `CountryHelper` via `PFMHelper` in `ConfigurationBundleManager`) | `otter.oobe` hidden | keep `otter.oobe` (it has no HOME activity) |
| Every Settings activity dies: `FileNotFoundException: null/preference-settings.json` | an unregistered device never installs KnightSettings' on-device config bundle; `bundle_location` is missing from `/data/data/com.android.settings/shared_prefs/com.amazon.settings.CONFIG_BUNDLE_STORE.xml`. Not caused by hiding: the OOBE never got that far | `am force-stop com.android.settings`, then `am broadcast -a com.amazon.dcp.sso.broadcast.CORPFMHasChanged -n com.android.settings/com.amazon.settings.AccountPropertyChangeReceiver`. It logged `DeviceBundle Combination for PFM: ATVPDKIKX0DER and language: en-US is now available` and wrote `bundle_location=/data/user/0/com.android.settings/files/bundle_a`. Without the force-stop, a Settings process left crashed makes the broadcast time out after 60 s |
| KnightSystemUI crashes exactly 10 min after boot: `RuntimeException: SimClient was never got created` (`com.amazon.systemui.common.SimConnectionListerner.onFinish`, a `CountDownTimer`) | `amazon.speech.sim` hidden | keep SIM installed; deny its microphone (below) |
| On-screen crash dialogs: `NativeAccessorProxyServices`, `smarthome.dshs.services`, `smarthomemapviewapp`, `alexa.datastore.app` (`Client already torn down`) | SIM clients, with SIM hidden | none needed once SIM is present |

**Danger.** The same `AccountPropertyChangeReceiver` handles `com.amazon.dcp.sso.action.account.removed`: with `device_provisioned=1` it factory-resets the device (`DeviceUtils.resetDevice`, read from the decompiled KnightSettings). Only the exact `CORPFMHasChanged` action above may be sent to it.

**Microphone denial replaces hiding SIM.** `appops set <pkg> RECORD_AUDIO ignore` makes AudioFlinger refuse a recorder even when the permission is granted. Tested with a uid holding a granted `RECORD_AUDIO` (Fully's, granted and revoked again for the test): allowed, the `VOICE_RECOGNITION` recorder delivered audio; with the op ignored, `Realize` failed with `SLresult 0x9` and logcat read `Request denied by app op OP_RECORD_AUDIO`. Applied to `amazon.speech.sim` and `com.amazon.bluestone.keyboard`; it persists in `/data/system/appops.xml` and is undone with `appops set <pkg> RECORD_AUDIO default`. The `HOTWORD` source goes through the same `recordingAllowed` check [INFERENCE from AOSP 7.1].

**The Wi-Fi and Bluetooth pages.** Implicit `am start -a android.settings.WIFI_SETTINGS` does not resolve on this build. `am start -n com.android.settings/.Settings\$WifiSettingsActivity` shows the network list (connected to the user's network, others listed) and `.Settings\$BluetoothSettingsActivity` the pairing page, both after the bundle fix.

**Result after the corrected Tier 1** (26 packages hidden, SIM and the keyboard mic-denied), rebooted and left 25 minutes:

| Check | Result |
|---|---|
| HOME and screen | Fully in front; `resolve-activity` returns `.LauncherReplacement` |
| Crashes | none in `dumpsys dropbox` after boot; SystemUI kept its boot pid for 25 min |
| Hidden packages running | none (`ps` against `pm list packages -u` minus visible) |
| Audio inputs | `dumpsys media.audio_flinger` shows no input thread at idle |
| Volume keys | `input keyevent 24`/`25` move `STREAM_MUSIC` 12 → 13 → 12 |
| MemAvailable | 1,015,752 kB stock in OOBE; 1,168,080 kB at 25 min |
| `VOICE_RECOGNITION` with metadata | yes, through the firmware's own backend (§10.7) |
| Not checked here | Fully's camera motion wake (PLUS licence), joining a different Wi-Fi network and pairing a Bluetooth device (need a person at the screen), a power-cycle cold boot |

## 10. Phase 0 answers (measured 2026-10-17)

**Method.**
- **Probe:** `device/tools/slprobe` (C, NDK 21.4 `armv7a-linux-androideabi22-clang`, `-lOpenSLES`; built by `device/tools/build_tools.sh`, documented in `device/tools/README.md`). It is the §4.4 program rebuilt: `VOICE_RECOGNITION` capture, 16 kHz mono S16, 8 × 256-frame buffers, `CLOCK_MONOTONIC` per completion, optional OpenSL buffer-queue player on a chosen stream. `stimulus.py` writes the stimuli, `decode.py` decodes bit 0 with the firmware's rule.
- **Identity:** uid 0 in `u:r:su:s0` from the adb shell, except item 5.
- **Every `VOICE_RECOGNITION` run** (10 s to 185 s):
  - metadata valid in all frames but the first, `FRAME_COUNTER` continuous;
  - first buffer 197–206 ms after `SetRecordState(RECORDING)` when no other recorder held the input; buffer gaps p50 14 ms, p99 33–35 ms, max 35–45 ms;
  - buffer completion − `AFE_TIMESTAMP`: median 17–19 ms, range 7–42 ms. §4.4's 7–41 ms (median 18) holds, so decision 4 of the plan stands.
- **Cleanup:** every file written to `/data/local/tmp` was deleted (the directory was empty before and after). `emprobe.rc` was deleted from `/system/etc/init` (remount rw, `rm`, remount ro), then the device was rebooted and showed no `init.svc.emprobe`. `STREAM_MUSIC` was set back to 12 and the screen woken. `settings list system/global/secure` matched the pre-test snapshot except `boot_count` (4 → 6, the two reboots), the time-zone provider's timestamp, a `crlset_framework_version` that appeared after boot, and `keep_screen_off`. That one read 0 at the start, then 1 in the snapshot taken seconds later before any test had run, and 0 at the end [INFERENCE: the device toggles it with the screen state].

### 10.1 Loud playback (item 1)

**Method.**
- **Stimulus** (`stimulus.py loud`, 24.5 s, 48 kHz): pink noise at −12 dBFS RMS (4 s), 1 kHz at −3 dBFS (3 s), log sweep 100 Hz–7 kHz at −6 dBFS (4 s), the pink noise gated at 4 Hz (4 s, a speech-rhythm programme), three 20 ms 1 kHz bursts at 0 dBFS, with 1 s gaps.
- **Playback:** OpenSL player on `STREAM_MUSIC` at indices 12 (the unit's setting), 18, 24 and 30, each recorded once with `VOICE_RECOGNITION` and once with `GENERIC` (no ASP: the raw reference).
- **Segments:** trimmed 150 ms at each edge. "Pink" is its last 2 s (converged), "sweep" its top 1.7 s (about 2.5–7 kHz).

**Levels, dBFS: raw (`GENERIC`) / residual (`VOICE_RECOGNITION`) / reduction.** Idle floor: −69 raw, −69 to −72 residual.

| Index (`VOLUME`) | Pink, converged | 1 kHz tone | Sweep top | Gated pink |
|---|---|---|---|---|
| 12 (40) | −43.1 / −72.1 / 29 | −41.4 / −78.2 / 37 (floor) | −47.2 / −68.9 / 22 | −44.0 / −73.4 / 29 |
| 18 (60) | −29.5 / −58.3 / 29 | −27.6 / −76.7 / 49 | −33.3 / −40.4 / 7 | −30.2 / −57.2 / 27 |
| 24 (80) | −21.9 / −48.4 / 27 | −19.0 / −66.4 / 47 | −24.4 / −24.2 / 0 | −25.0 / −52.7 / 28 |
| 30 (100) | −20.0 / −48.6 / 29 | −12.4 / −61.8 / 49 | −15.8 / −13.5 / −2 | −23.1 / −41.9 / 19 |

**Metadata over the whole stimulus, no talker present.**

| Index | `MIC_CLIPPED` | `OUTPUT_CLIPPED` | `AEC_DIVERGED` | `DTD` max (where) | `DNN_VAD_PROB` max | `UED` max |
|---|---|---|---|---|---|---|
| 12 | 0 | 0 | 0 | 0 | 0.25 (single frames in gaps) | 0.25 |
| 18 | 0 | 0 | 0 | 0.42 (sweep, 69% of frames) | 0.50 (sweep) | 0.38 |
| 24 | 0 | 0 | 0 | 1.00 (sweep, mean 0.47; 0.06 at the pink onset) | 0.75 (sweep) | 0.59 |
| 30 | 0 | 18 frames (sweep top) | 0 | 1.00 (sweep, mean 0.84; 0.45 gated pink; 0.16 pink onset) | 0.75 | 0.50 |

- **`MIC_CLIPPED` never fired, and the mic never clipped:** the raw `GENERIC` peak was −1.9 dBFS (sweep at index 30). jxlarrea's 24% clipping (§8.2) was at +40 dB PGA on LineageOS; stock runs +20 dB.
- **`OUTPUT_CLIPPED` is the ASR output clipping:** at index 30 the `VOICE_RECOGNITION` output reached full scale on 189 samples, all in the sweep, where the AEC removed nothing.
- **`DTD` and `DNN_VAD_PROB` fire on the device's own echo from index 18 up:** `DTD` read 1.0 for most of the sweep at 24 and 30. Neither is evidence of a near-end talker at volume, as on the Dot ([afe-metadata.md](afe-metadata.md) §1.6–1.7).
- **`PLAYBACK_ACTIVE`:** 1.0 in every segment with energy, 0.54–1.0 in the 1 s gaps (hangover), 0 before and after.
- **`VOLUME` = round(index × 100 / 30)** (12 → 40, 13 → 43, 18 → 60, 24 → 80, 30 → 100). SystemUI pushes it to the ASP on every `STREAM_MUSIC` change: `AlexaVolumeUpdater: Updating ASP, deviceVolume 13 heimdallVolume 43`, then ASP `setSpeakerVolumeIndex : 43`. It depends on `com.android.systemui`, which stays (§9.3).
- **Cancellation:** 27–29 dB on stationary wideband noise at every index, 47–49 dB on a tone from index 18 (floor-limited at 12), and nothing on the fast sweep from index 24. At 24–30 the residual of the noise programmes sits at −42 to −53 dBFS, 17–28 dB above the idle floor.
- **The raw level saturates above index 24 on wideband programme:** pink rose 13.6, 7.6 and 1.9 dB over the steps 12 → 18 → 24 → 30, the tone 13.8, 8.6 and 6.6 dB [INFERENCE: a limiter in the speaker path, since the mic did not clip].
- **Double talk: not measured.** No talker was available.

### 10.2 A second recorder (item 2)

| Started first (holding the input) | Started second | Second recorder | First recorder |
|---|---|---|---|
| `VOICE_RECOGNITION` | `GENERIC` (`MIC`) | 0 buffers in 12 s | untouched: no gap over 45 ms, 9,349 of 9,350 frames valid, counter continuous, level unchanged over 75 s |
| `VOICE_RECOGNITION` | `VOICE_RECOGNITION` | 0 buffers in 12 s | untouched (same run) |
| `VOICE_RECOGNITION` | `VOICE_COMMUNICATION` | 0 buffers in 12 s | untouched (same run) |
| `GENERIC` (16 s) | `VOICE_RECOGNITION` | 0 buffers for all 25 s, including the 12 s after the holder stopped | — |
| `GENERIC` (16 s) | `VOICE_RECOGNITION`, cycling the recorder every 2 s while stalled (`STOPPED`, `Clear`, re-enqueue, `RECORDING`) | audio 200 ms after the first cycle after the holder stopped, with metadata (1,341 of 1,342 frames valid) | — |

- **The first input wins; this is AOSP 7.1's policy, unchanged.** The later start fails (`APM_AudioPolicyManager: startInput(182) failed: other input 166 already started`, `AudioRecord: start() status -38`). The holder is never silenced or preempted.
- **OpenSL hides the failure.** `SetRecordState(RECORDING)` returns `SL_RESULT_SUCCESS`, no record event fires, and buffers simply never complete. A recorder that lost the race never recovers by itself; stopping and restarting it does.
- **Consequence for the firmware:** "no buffer completion within a deadline after `RECORDING`" is a capture failure to recover from by cycling or re-opening (Phase 1's "re-open on recorder errors"). Whoever starts first locks the other out silently: while EchoMuse holds the input, Fully's acoustic detection, an in-page Assist mic, the Amazon dictation IME or a registered Alexa cannot record, and if one of them starts first, EchoMuse gets nothing until it stops.
- **WebView `getUserMedia`: pending.** Fully is not installed. Silk (`com.amazon.cloud9`, `.browsing.BrowserActivity`) is present but holds no `RECORD_AUDIO`, so it cannot exercise the path. A framework `AudioRecord` goes through the same `AudioPolicyManager::startInput` [INFERENCE], so the same rule is expected.

### 10.3 Screen off (item 3)

**Method.** One 185 s `VOICE_RECOGNITION` capture. 6 s in, `input keyevent 223` (`KEYCODE_SLEEP`); 160 s later, `input keyevent 224` (`KEYCODE_WAKEUP`). `dumpsys power`, `/sys/power/wake_lock` and `/sys/kernel/debug/suspend_stats` were read before, at 4, 30, 60, 120 and 160 s, and after waking; then 60 s screen off without capture.

| | Screen on | Screen off (160 s) |
|---|---|---|
| `mWakefulness`, display, backlight | `Awake`, `ON`, 1 | `Asleep`, `OFF`, 0 |
| Capture | running (6 s) | continuous: 11,551 buffers for 184.8 s of audio in 185.05 s, no gap over 45 ms, 23,101 of 23,102 frames valid, counter continuous, level unchanged (−69 dBFS) |
| Wake locks | `PARTIAL_WAKE_LOCK 'AudioIn' (uid=1041 ws=WorkSource{0})` from capture start to stop | same; `/sys/power/wake_lock` held `PowerManagerService.WakeLocks` |
| Suspend (`suspend_stats` success) | 0 | 0 |

- **Capture continues with the screen off, and an active input keeps the device awake:** AudioFlinger takes the `AudioIn` partial wake lock for the record thread, attributed to the client's uid (0). EchoMuse's always-open capture therefore blocks suspend by itself.
- **Whether the device suspends without a capture was not established.** `suspend_stats` success stayed 0 through both screen-off runs, but USB was connected and Prime Video (`com.amazon.avod`) held a WorkManager partial wake lock for more than 7 minutes, including the 60 s screen-off baseline.
- **The screensaver keeps the screen on.** In `Dreaming` (`ClockDream`) the window manager holds `SCREEN_BRIGHT_WAKE_LOCK` for `com.amazon.paladin`. A 185 s capture run through it delivered 184.8 s of audio in 185.0 s.
- **`input keyevent 26` does not turn the screen off on crown.** `KEYCODE_POWER` is the mute button's key (§3), and the injected press went to Amazon's privacy path: `ECS_ECDeviceModeEventProvider: Received broadcast intent: amazon.intent.action.PRIVACY_MODE_OFF`. `amazon-gating` `state` stayed 0 and the screen stayed on. Use `KEYCODE_SLEEP` (223) and `KEYCODE_WAKEUP` (224).
- **Seen in passing:** while in OOBE, `HDS_CameraDataProvider` (`com.amazon.knight.hds`) ran the camera at about 10 fps, and ECS logged `PresenceDetection enabled (CV or HDS): true`. That is a role for the §9.3 entry that lists `knight.hds` as unknown.

### 10.4 Service domain (item 5)

**Method.** `/system/etc/init/emprobe.rc` (removed afterwards), started by init after `sys.boot_completed=1` with SELinux enforcing:

```
service emprobe /system/bin/sh /data/local/tmp/emprobe.sh
    class late_start
    user root
    group audio system inet net_raw wakelock
    seclabel u:r:adbd:s0
    disabled
    oneshot

on property:sys.boot_completed=1
    start emprobe
```

The script ran each check in `adbd`, then made the Dot's move (`echo -n u:r:su:s0 > /proc/self/attr/current`) and ran them again.

| Check | `u:r:adbd:s0` | after the move, `u:r:su:s0` |
|---|---|---|
| init starts the service | yes, at uptime 26 s; uid 0, gid `audio`, groups `audio system inet net_raw wakelock`; `ANDROID_ROOT`, `ANDROID_DATA`, `BOOTCLASSPATH` set | — |
| The move (`/proc/self/attr/current`) | write succeeds | — |
| OpenSL `VOICE_RECOGNITION` capture | **fails**: recorder `Realize` returns `SLresult 0x9`; kernel `binder: 264:… transaction failed 29201/-1` and `send failed reply for transaction … to <probe pid>` | works, with metadata |
| `service call audio 8 i32 3` | works | works |
| Read `settings_system.xml`; write `/data/local/tmp` | works | works |
| `echo EchoMuse > /sys/power/wake_lock` | works (`adbd` is a permissive domain: the avc denial logs `permissive=1`) | works |
| `pm list packages`, `pm path android` | fail (no output) | work (183 packages) |

- **The move is required on crown, as on the Dot.** In `adbd`, audioserver's binder reply to the client is refused (`BR_FAILED_REPLY`, `EPERM`), so no recorder can be created. No avc line names the check. The boot-time audit log was rate-limited, but the failure also reproduced in steady state: a root shell moved into `u:r:adbd:s0` by hand failed the same way, with the same kernel lines and no denial logged [INFERENCE: an enforced check on audioserver's side, such as `binder transfer` to `adbd`].
- **Install path:** `mount -o rw,remount /system` and `mount -o ro,remount /system` both exit 0 from the adb root shell. A file copied into `/system/etc/init/` takes `u:object_r:system_file:s0` from the directory.
- **The plan's decision 11 (the domain is decided in Phase 0) is now settled:** `seclabel u:r:adbd:s0` plus the Dot's `start_server.sh` move to `su`.

### 10.5 Volume (item 6)

**`IAudioService` transaction codes are AOSP 7.1's.** They were read from the `TRANSACTION_*` constants of `android.media.IAudioService$Stub` in `/system/framework/arm/boot-framework.oat` (`framework.jar` is a 304-byte stub), and each was verified by a round trip.

| Code | Method |
|---|---|
| 3 | `setStreamVolume(int streamType, int index, int flags, String callingPackage)` |
| 4 | `isStreamMute(int)` |
| 8 | `getStreamVolume(int)` |
| 9 | `getStreamMinVolume(int)` → 0 for `STREAM_MUSIC` |
| 10 | `getStreamMaxVolume(int)` → 30 for `STREAM_MUSIC` |
| 11 | `getLastAudibleStreamVolume(int)` |

| Command (uid 0) | Reply |
|---|---|
| `service call audio 8 i32 3` | `Result: Parcel(00000000 0000000c   '........')`: first word the exception code (0), second the index in hex (12) |
| `service call audio 3 i32 3 i32 13 i32 0 s16 com.android.shell` | `Result: Parcel(00000000    '....')`: void, no exception |
| `service call audio 8 i32 99` (bad stream) | multi-line `Result: Parcel(` dump starting `fffffffd 00000012 …'Bad stream type 99'`: an exception code, then its UTF-16 message |

- **`setStreamVolume` works from uid 0** with `s16 com.android.shell`, with `s16 root`, and with the package omitted. In all three cases `getStreamVolume`, `dumpsys audio` (speaker index) and the setting followed. `flags` 0 shows no UI.
- **Parse rule:** one line, first word `00000000`, second word the value. Anything else (a multi-line dump, a non-zero first word) is a failure.
- **Setting:** `volume_music_speaker` (system namespace) tracks the speaker index. `volume_music` without the suffix is stale (11 while the index was 12).
- **Volume keys:** `input keyevent 24`/`25` move `STREAM_MUSIC` by one (12 ↔ 13). `getStreamVolume` reflects the press at once; `volume_music_speaker` follows 0.5–0.6 s later (AudioService's persist delay).
- **Cost per read:** `service call audio 8 i32 3` takes about 27 ms wall and 18 ms CPU. `settings get system volume_music_speaker` starts a JVM: 1.27 s wall, 1.03 s user CPU. `/data/system/users/0/settings_system.xml` is plain XML (mode 0600, `system`), with the line `<setting id="395" name="volume_music_speaker" value="12" package="android" />`; grepping it takes about 10 ms, and its mtime changes on every persist.
- **Correction to the plan's decision 8:** poll `getStreamVolume` through `service call` (or watch the XML's mtime), not `settings get`, which costs a second of CPU per call.
- **Side effect:** every change also reaches the ASP (`VOLUME`, §10.1).

### 10.6 Still open

- Double talk with a real talker 1–2 m away: `DTD`, `DNN_VAD_PROB` and `UED` under near-end speech over playback (item 1).
- A second recorder from Fully's acoustic detection and from a WebView `getUserMedia` (item 2). Fully 1.61.4 is installed now, but both features need the PLUS licence.
- Whether the device suspends with the screen off and no capture, off USB (item 3). It does not matter while EchoMuse captures, since the `AudioIn` wake lock blocks suspend.
- Whether `amazon-gating` mute silences the ASR output at the source (item 4). Deferred with mute ([plan](echo-show-port-plan.md) Phase 7).
- Everything in §8.3.

### 10.7 The firmware's own backend (Phase 1, measured 2026-10-17)

Run by hand over adb on the Tier 1 state of §9.5, firmware `fw-09147d08f418`.

**Render-to-capture lag, `device/tools/sl_latency`.** The tool opens `slmic.OpenOpenSL` and `slspeaker.NewOpenSL` exactly as the firmware does and plays 20 ms bursts through ordinary 2,048-frame sink writes, 2.5 s apart, plus one burst into a queue left to run dry. The lag is the firmware's capture stamp of the echo minus the render accounting's stamp of the burst's first frame; a negative lag would mean render stamps are late.

| Run | `pcm` lag (echo in the AEC residual), ms | `PLAYBACK_ACTIVE` lag, ms | Empty-queue burst `pcm`, ms |
|---|---|---|---|
| 1 kHz tone, index 18, 11 bursts | +95.5 / +110.4 / +137.1 (min / median / max) | +48.9 / +58.0 / +69.4 | +95.5 |
| white noise, index 24, 7 bursts | +97.2 / +158.6 / +172.4 | +41.4 / +98.1 / +104.5 | +97.2 |

- **No late stamp.** Every lag is positive, so `opensl.playPipelineNs` = 40 ms holds as a lower bound; the stamps run 95–172 ms early, inside the controller's 0–500 ms lag search.
- **Capture stamps:** every period was anchored on `AFE_TIMESTAMP` (439 of 439 and 314 of 314). Read completion came 7.2–18.8 ms after it (median 8 ms), tighter than §4.4's 7–41 ms with 256-frame buffers.
- **Metadata:** 4,388 and 3,138 frames, 0 invalid, 1 sync, 0 gaps, 0 lost. No mic drops; sink writes and completions matched (767/767, 531/531).

**Firmware run, 75 s,** with TCP 8767/8770 rejected by `iptables` so that it could not register with the LAN controller (removed afterwards, together with the `/data/local/etc`, `/data/local/share` and `/data/local/tmp` directories the run created):
- `Platform: fireos6_crown (crown, "Fire OS 6574.1 (NS65741/8146)")`; the Android volume check passed (no `[volume] volume control disabled` line); `[als] ambient light sensor at /sys/bus/platform/drivers/als_ps/lux`.
- `[afe] 30s: 3698/3700 frames valid, 0 invalid, 1 syncs, 0 gaps, 0 lost … volume 40`, then `3750/3750`. `VOLUME` 40 is index 12 (§10.1).
- RSS 29.6 MB, 17 goroutines. The Show path makes no `tinymix`, Wi-Fi or Bluetooth calls [INFERENCE from the code; not traced on the device].
- Not exercised without a controller session: wake candidates, the chime, a turn through HA, `render.progress`.
