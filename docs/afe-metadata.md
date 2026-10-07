# What EchoMuse can do with the native AFE metadata

**Status:** exploration of 2026-10-06 and 2026-10-07, updated for what was built. Decoding, transport and per-turn evidence are implemented as the optional capability `afe_metadata_v1` (section 2). Everything else here is a proposal or an experiment that has not run. No decision reads any field.
**Evidence:** Fire OS 6 captures from G090LF0965260F1J (Fire OS 6.5.6.9, `NS6569/6009`); Fire OS 5 frames from saved clips of the office Dot. Experiment E0 ran on 2026-10-07 (section 5).
**Companion:** [alexa-afe.md](alexa-afe.md), "Fire OS 6: the mixer's ASR streams", documents the stream, the frame layout and the capture program. Its "How the measurements were made" defines the capture names, stimuli A and B and the analysis methods used here. The wire format is [protocol-v1.md](protocol-v1.md) §3 and §4; the contract is [post-afe-audio-architecture.md](post-afe-audio-architecture.md) §4.5.

---

## 0. Summary

1. **The metadata measures the AEC state and the playback timeline. It does not detect who is talking.**
   - `PLAYBACK_ACTIVE` (PA) is exact and input-aligned. It rises in the frame that contains the reference onset, 51.8 ms before the echo reaches the beam, and has a 0.5–0.8 s hangover.
   - `ERLE_RAW` tracks cancellation in dB-like steps. It is 0 whenever the reference is silent, and it is capped by how far the echo stands above the noise floor.
   - `FRAME_COUNTER` and `AFE_TIMESTAMP` give independent capture-continuity checks.
   - `DTD`, `DNN_VAD_PROB` and `UED` all fired on the device's **own** echo with no talker in the room: DTD reached 0.68 and VAD 0.75. None of them has been tested against a real near-end talker.
2. **Built** (section 2). The device decodes every frame and uplinks one 14-byte record per 80 ms capture period under uplink leases. For every wake turn and rejected candidate, the controller keeps summaries of the second before the wake (`pre`) and of the wake window, plus the last PA rise before the wake (`playback_onset`); a turn also gets a summary of its utterance. Decoder health travels in `wake.stats.afe`. All of it is evidence only.
3. **Ranked uses** (detail in section 3):

   | Rank | Use | Status |
   |---|---|---|
   | 1 | Self-echo evidence on every producing-sound wake candidate: pre-wake ERLE and PA history, playback age, DTD. Experiments E1 and E2 settle whether it separates self-echo from a talker | Evidence **implemented** (`pre`, `wake`, `playback_onset`); a decision only after measurement |
   | 2 | Capture-integrity accounting: counter and timestamp gaps against the capture timeline's `Missing` ranges and the unconsumed `Drops()` | Decoder counters **implemented**; the cross-check is a proposal |
   | 3 | The AFE as a measured playback timeline: PA rise as an onset anchor, end of ERLE as the drain point. It measures the error of `ClockMap`, which today sits inside a 0–500 ms lag search | Proposal (logging) |
   | 4 | Offline decode of clips already saved, which needs no protocol change. This includes the four Fire OS 5 barge-in clips behind the documented self-wakes, and training-data labels | E0 ran on Fire OS 5 clips, inconclusive (section 5); otherwise a proposal |
   | 5 | Per-device echo-cancellation health telemetry | Proposal |

   Ranks 6–9 (a PA-based `producing_sound` window, double-talk support, mute verification, clipping alarms) are decision changes or small checks that wait on experiments.
4. **Would not do:** VAD or endpointing from `DNN_VAD_PROB`, DTD or UED as near-end acceptance, ERLE-based rejection before E1/E2, noise floor from `RMS`, direction fields, `VOLUME` as a volume read-back, and `AFE_TIMESTAMP` as a sample clock. Reasons are in section 4.
5. **Fire OS 5 has metadata too.** It is v2.1, not v3.3, and was verified on 1.88 M frames from 439 saved office-Dot clips with no checksum failures. It carries ERLE-like fields only: no playback flag, no frame counter, and a constant `0xBEEF` where the timestamp should be. Its field names are unresolved, and EchoMuse does not decode it.
6. **Device experiments in run order:** E1 (AEC convergence and persistence) → E5/E3/E4 (VOLUME, mute and clipping, about 25 min in the same sitting) → E2 (double-talk corpus) → E6 (Fire OS 5 field validation on the office Dot) → E7 (passive field study). E0 needs no device. It ran on 2026-10-07 on an export that lacks the four barge-in clips, so it decided nothing. Specs are in section 5.

---

## 1. Field validation from the captures

### 1.1 Data and decoding

Every capture was made with the capture program described in alexa-afe.md ("How the measurements were made"). It is 16 kHz, with the metadata in bit 0 of channel 0. The stimuli are 48 kHz mono S16LE, played through a `MUSIC` play stream (`MixerOpenPlay(48000, 1, 16, 0)`).

Stimulus A, 11.02 s:

| Segment | Duration |
|---|---|
| silence | 1 s |
| 1 kHz sine "click", amplitude 0.5 | 20 ms |
| silence | 0.98 s |
| white Gaussian noise, RMS 0.1 (−20 dBFS), seed 1234 | 3 s |
| silence | 1 s |
| exponential sweep 100 → 7,500 Hz, amplitude 0.3, 20 ms fades | 3 s |
| silence | 1 s |
| click | 20 ms |
| silence | 1 s |

Stimulus B, 22 s: 1 s of silence, 20 s of white Gaussian noise at −35 dBFS RMS (seed 7), then 1 s of silence.

| Capture | Stream / setup (`mic_channels:ASR_out:ref_out`) | Playback | Uptime at start | Notes |
|---|---|---|---|---|
| `q9`, `s9` | `micMultiChAsr`, 9 ch, no setup / rejected setup `0,1,2,3,4,5,6:1:1` | none | 4,107 s / 4,174 s | idle |
| `p9` | 9 ch, no setup | A at 1.0 s | 4,092 s | |
| `pa` | **`micAsr` (EchoMuse's stream)** | A at 1.0 s | 4,118 s | |
| `s6` | 6 ch `0,1,2,3:1:1` (beam, m0–m3, ref) | A at 1.0 s | 4,194 s | |
| `n1`–`n3` | 6 ch, same setup | B at 1.0 s | 4,544–4,595 s | three consecutive streams |
| `lb` | 3 ch `0:1:1` (beam, m0, ref) | A at 8.0 s | 4,913 s | tonal room sound at 21.7–25.8 s |
| `lm` | 3 ch `0:1:1`, `loopback` recorded in parallel | A at 1.5 s | **231 s, after a reboot** | |
| `rw` | `micRaw` (which is the beam) | A at 1.0 s | **272 s, after the reboot** | |
| channel-map set | 14 × 4 s, `micMultiChAsr` with the setups of alexa-afe.md "Channel layout" | B at 0.5 s | ≈4,650–4,760 s | |
| `lp` | `loopback` | none of its own (A, from `lm`) | 231 s | no metadata |

**Decoding.** For each capture, bit 0 of channel 0 was shifted sample by sample into a 128-bit register, with the first-received bit as bit 127. A frame was taken wherever bits 127:120 are `0xA5` and bits 7:0 equal the popcount of bits 127:8 modulo 256, and the fields were sliced per the v3.3 table in alexa-afe.md. Only frames at phase 0 (starting at a multiple of 128 samples) were kept; section 1.2 says why. For every frame and channel the RMS and peak of its 128 samples were computed with bit 0 cleared. In the multichannel captures, the reference channel gives the playback timeline: a *reference segment* is a run of non-zero reference samples, ended by more than 400 consecutive zeros.

Every capture opened a new record stream and a new play stream. A "first playback" below is therefore the first in a fresh stream pair. EchoMuse keeps both streams open continuously:
- the render mixer's `Run` (`device/internal/render/mixer.go`) writes silence when idle;
- the mixer `Recorder` (`device/internal/mixerapi`) reopens the record stream only on errors (`EHOSTDOWN`, repeated `ETIMEDOUT`).

### 1.2 Framing and continuity

- **`FRAME_COUNTER`** advanced +1 (mod 64) across all **29,538** phase-0 frames of 25 captures (every capture except `lp`). No frame was missing against samples/128, and every capture starts at phase 0.
- **Chance matches.** Hunting sync + checksum at every sample finds false frames:
  - `lp` (loopback, which carries no metadata) gave 4 checksum-valid "frames" in 229,888 samples, at phases 16, 61, 82 and 123.
  - `n3` gave one valid "frame" at phase 30 (2,973 found against 2,972 expected), which made `FRAME_COUNTER` appear to jump 32/33.
  - That is roughly one false frame per 3–4 s of audio, so a decoder must phase-lock. The firmware's decoder does: sync, version, `PAYLOAD_SIZE` and checksum, plus a confirming frame, and no re-hunting while locked (section 2.1).
- **`AFE_TIMESTAMP` is device `CLOCK_MONOTONIC` (equal to `CLOCK_BOOTTIME` here) in ms, mod 65,536.** The 16-bit timestamps were unwrapped and compared with the capture program's per-block log. Each clock was interpolated at the frame's last sample against the cumulative sample count.
  - AFE_TS minus the read-completion monotonic time: median −4.5 to −5.1 ms, sd 2.0–2.4 ms, in 6 captures (`lb`, `n1`, `pa`, `q9`, `lm`, `s6`).
  - AFE_TS minus the mixer block `ts`: +72.3 to +72.4 ms (sd 1.9–2.1).
  - Long-run slope (linear fit against frame index, `lb`, `n1`, `pa`, `q9`): 7.9997–8.0001 ms per frame; mean step 7.9995–8.0012 ms.
  - Per-frame steps alternate ~5 and ~11 ms (even and odd frames average 4.87 and 11.13 ms) because frames are processed in pairs. The residual against the linear fit has sd 2.5–2.7 ms and ranges from −7.9 to +5.2 ms.
  - So it marks **processing time, not sample time**. It is good for resolving counter wraps (64 frames = 512 ms) and for spotting input-side stalls. It is not a sample clock.

### 1.3 Alignment: which audio each field describes

- **`RMS` describes exactly the 128 output samples it rides on.**
  - The field was correlated against the computed level of the beam samples it rides on, at frame lags −40 to +40. The best lag is 0 in all 9 captures tested (`lb`, `lm`, `s6`, `n1`–`n3`, `pa`, `p9`, `rw`), with r = 0.991–1.000.
  - The median of (RMS − computed level) is −0.19 to −0.27 dB, and the 90th percentile of |error| is ≤0.48 dB.
  - Values are integer dB, observed from −90 to −17 dBFS. `COMPUTE_RMS` is 1 in every frame.
- **`PLAYBACK_ACTIVE`, `ERLE_RAW`, `DTD` and `DNN_VAD_PROB` are computed on the AFE's input timeline, and they lead the beam audio they ride on.**
  - PA rises in the frame that contains the reference onset. In 14 of the 15 reference segments of `lb`, `lm`, `s6` and `n1`–`n3`, the PA-rise frame starts 71–116 samples before the segment. The faded sweep start in `lb` (13.83 s) was one frame late (+8 samples).
  - The echo then reaches the beam 829 samples (51.8 ms, 6.5 frames) later (GCC-PHAT; alexa-afe.md, "The reference is sample-aligned to the microphones").
  - On the first click in `lm` (PA rise at frame 287), VAD and VSS responded at frame 289 and DTD at frame 293. All three responded before the echo appeared in the beam at frame 294.
  - On converged clicks, ERLE peaked 3–6 frames after PA, also before the beam echo (6–7 frames after PA). On the unconverged first click, ERLE first moved at frame 295.
  - At an 80 ms period this lead is under one period. `em_afe`'s module doc records it, and spans are summarised without a shift.

### 1.4 `PLAYBACK_ACTIVE`

- **It follows reference energy, not stream state.** In `lb` the play stream wrote 1 s of zeros before the first click, and PA stayed 0 until the click.
- **The rise is frame-exact** (section 1.3).
- **The fall is level-dependent.** Measured from the end of the reference segment to the PA fall:
  - 492–499 ms for −37 dBFS noise (`n1`–`n3`)
  - 631–716 ms for clicks
  - 688–691 ms for −22 dBFS noise
  - 790–792 ms for the −3 dBFS sweep
- It was 0 in every idle frame.
- Because it is the AFE's own reference, it covers **every mixer client**, not only EchoMuse.

### 1.5 `ERLE_RAW`

- **It is 0 in every frame without reference energy.**
  - In captures with a reference channel it is non-zero in 83–95 % of frames with reference energy.
  - It is non-zero in only 0.00–0.01 of the frames where PA is 1 but the reference is digitally silent.
  - It therefore drops to 0 within a frame when playback stops, and in every pause inside playback. A plain 80 ms mean of it is meaningless, so the records carry the max, the mean of non-zero values, and the non-zero count.
- **Its scale behaves like dB** [INFERENCE].
  - On a converged click it peaks at 16–23.
  - The beam's echo fell 12.5–32 dB between the first click and the last click of the same capture.
  - Values seen run from 0 to 26.
- **Convergence on a fresh AFE instance.** Each PA rise starts a playback. The table gives the first long playback (−20 dBFS noise) of each capture:
  - when ERLE first reaches 5 and 10, in ms after the rise;
  - the beam residual at 0.1, 1.0 and 2.5 s, as the mean per-frame level over ±6 frames.

  | Capture | ERLE ≥ 5 after | ERLE ≥ 10 after | Beam residual at 0.1 / 1.0 / 2.5 s |
  |---|---|---|---|
  | `lb` | 416 ms | 1952 ms | −60.1 / −70.6 / −75.0 dBFS |
  | `s6` | 312 | 952 | −59.0 / −70.4 / −71.8 |
  | `pa` (`micAsr`) | 128 | 632 | −59.6 / −68.4 / −73.3 |
  | `p9` | 224 | 584 | −55.8 / −65.9 / −71.8 |
  | `lm` (post-reboot) | 128 | 408 | −47.1 / −55.1 / −63.9 |
  | `rw` (post-reboot, the click had pre-trained it) | 24 | 56 | −51.6 / −60.8 / −65.3 |

  The residual falls 8–11 dB between 0.1 and 1.0 s, and another 1.4–9 dB by 2.5 s. **The first ~1–2 s of the first playback in a fresh AFE carries residual echo several dB above its converged level.** The documented Fire OS 5 self-wakes fell 1.2–1.55 s into an answer (post-afe-audio-architecture.md §5.3, [D3]).
- **Convergence persists across silence within one stream pair.** The final click came after 4 s of other content and 1 s of silence. ERLE reached ≥10 within 16–24 ms, and its residual was 12.5–32 dB below the first click of the same capture.
- **The first click of every capture was poorly cancelled**: beam −45 to −39 dBFS before the reboot, −23 to −26 after it.
  - This includes `pa`, recorded about 12 s after `p9` ended, with `q9` (10 s idle) in between.
  - So the AEC state did not survive one of these: new streams, ≥12 s of idle, or both. **The captures cannot tell which.** E1 is designed to answer it.
- **ERLE is capped by the echo's level above the noise floor.**
  - At a −37 dBFS reference (`n1`–`n3`), ERLE stayed at 2–5 (p50 2–3, p90 3–5) for 20 s, while the beam sat at the idle floor (−76.8 vs −76.3 dBFS).
  - Low ERLE therefore also means "quiet echo", not only "unconverged".
  - After the reboot the echo was 13–23 dB louder (first click −23/−26 vs −39 to −46 dBFS at the same reference level), and converged ERLE was higher (p50 18 vs 11).

### 1.6 `DTD` (raw/31)

- 0 in every idle frame and throughout converged playback of noise and clicks.
- Non-zero only in two situations, neither with any talker present:
  - **At the first, uncancelled click:**
    - `lm` 0.68, with the beam echo at −17 dBFS. It rose 6 frames after PA and decayed 0.68 → 0.19 over ~0.27 s.
    - `rw` 0.35
    - `pa` 0.06
  - **Weakly (≤0.06) on loud sweep residual** (`lm`, `rw`).
- The RES section of the device's `AFE.cfg` (`dtdDecisionTh`, lines 634–636 of `/system/vendor/etc/audio-algorithms/AFE.cfg`) sets the decision threshold at 0.33 for volume steps 6–7, so **the AFE declared double talk on its own echo**.
- DTD under real near-end speech is unmeasured.

### 1.7 `DNN_VAD_PROB` (2 bits: 0, .25, .5, .75; never 1)

- **0 in every idle frame**: `q9` for 10 s, and `lb` for the 8 s before its playback.
- **It fired on:**
  - click onsets: 0.5–0.75 for 0.1–0.27 s in 6 captures
  - noise onset: 0.25, briefly
  - loud sweep residual: `lm` 0.75 for 1.1 s with the beam at −43 to −50 dBFS and nobody talking; `rw` 0.5
  - the onset of `lb`'s room sound
- **The room sound** was tonal, with energy at 0.3–3 kHz and peaks at 0.86–1.08 kHz. It started at −49 dBFS and held −60 dBFS (15–25 dB over the floor) for 4 s. VAD was 0.75 for 0.32 s at its onset and then 0 for the rest of the sound.
- So the VAD is onset- and novelty-driven and not echo-robust. No capture contains speech, so its speech sensitivity is unknown.

### 1.8 `UED`, `UED_GR`, `ARA_VSS`

- **`UED`** is the output of `AFE.cfg`'s `UED Fusion` graphical model (lines 1031–1049): hidden `UE` and `ES` nodes, with observed `PBA` (playback active), `VSS`, `DTD` and `VAD`.
  - It reached 0.70 on sweep residual and 0.84 on the room-sound onset; idle it reads 0–0.03.
  - It is not echo-robust either.
- **`ARA_VSS`** is ≈1 in silence and 0 with any sound, recovering over 0.3–1.7 s.
  - "VSS" is variable step size [INFERENCE from `AEC_V2 VSS Mode` and `aecVssSumTh`].
- None of the three has a consumer.

### 1.9 Clipping and divergence

- **`OUTPUT_CLIPPED`** was 1 on exactly 13 frames in each of `lb`, `lm` and `s6` (frames 1,734–1,746, 921–933 and 860–872).
  - In each of those frames the `ref_out` channel peaked at ≥32,482 (the smallest flagged peak: 32,482 in `lm` and `s6`, 32,516 in `lb`). Peaks of unflagged frames were not checked, so 32,482 is only an upper bound on the threshold.
  - It was **0 in `pa`/`p9`/`rw`**, which played the same stimulus but had no reference channel in their output (beam peaks 1,366, 842 and 7,270).
  - So it flags clipping in the AFE output buffer [INFERENCE from the coincidence]. On `micAsr` that buffer holds only the beam, which never exceeded 8,110.
  - Separately, the sweep's 100–200 Hz start drives the reference to full scale (−3 dBFS RMS from a −13.5 dBFS sine). The playback chain boosts bass by about 10 dB, but `micAsr` cannot see that clipping.
- **`MIC_CLIPPED`** and **`AEC_DIVERGED`** were 0 in every frame, including the uncancelled −17 dBFS echo after the reboot.

### 1.10 `VOLUME`, `DEVICE_MUTE` and constant fields

- **`VOLUME`** was 70 in every frame, which is `persist.mixer.init.main.volume`.
  - It read 70 both after the reboot and before it, although the uncancelled echo was 13–23 dB louder after the reboot at the same reference level.
  - [INFERENCE] It does not follow the codec DAC (tinymix control 61) that EchoMuse uses for volume (`device/internal/server/volume.go`). E5 confirms or refutes this.
- **`DEVICE_MUTE`** was 0. Mute was never exercised.
- **Constant fields:** `SSL_ANGLE`/`SSL_PROB` 0/0.5 and `FD_ANGLE`/`FD_PROB` 0/0.5 in every frame. `ULTRA_PROX_*`, `SIGNAL_TO_ECHO_RATIO`, `LPM_FLAG` and `LPM_V2_FLAG` were 0.

### 1.11 Trust table

| Field | Trust for | Do not trust for |
|---|---|---|
| `FRAME_COUNTER` | Sample-exact continuity of the AFE output EchoMuse received; gap size mod 64 | Anything without phase-locking (chance matches) |
| `AFE_TIMESTAMP` | Resolving counter wraps; spotting AFE input-side stalls (a timestamp jump while the counter advances +1); a cross-check of `ts` (+72 ms) | A sample clock (±7 ms in pairs) |
| `PLAYBACK_ACTIVE` | "The speaker emitted audio from any client", on the capture timeline; the onset frame (≤8 ms) | The exact end of playback (0.5–0.8 s level-dependent hangover); quiet sources below its unknown threshold |
| `ERLE_RAW` | Whether the AEC was cancelling while the reference was active; convergence trends on loud content | Convergence on its own: quiet echo also gives low ERLE. Any frame or period mean, because of the zeros in pauses. Double talk |
| `RMS` | Integrity check of the PCM path (matches the 128-sample level within 0.5 dB) | New information: it duplicates the device's per-cell `E` |
| `DTD` | AFE-internal "unexplained mic energy during playback" | Telling a real talker from the device's own unconverged echo (0.68 with no talker) |
| `DNN_VAD_PROB`, `UED` | Nothing yet | Speech presence during playback (0.75 on own residual); endpointing |
| `OUTPUT_CLIPPED` | Clipping of the beam on `micAsr` | Speaker or reference clipping (that needs `ref_out`) |
| `MIC_CLIPPED`, `AEC_DIVERGED`, `DEVICE_MUTE` | Unvalidated: never non-zero | – |
| `VOLUME` | The mixer main volume (70) | EchoMuse's speaker volume [INFERENCE] |
| Direction, proximity, SER, LPM | – | Everything (constant) |

### 1.12 Fire OS 5: v2.1 frames are present

- **Data.** A labelled export of the office Dot's saved clips: 556 WAV files, 16 kHz mono S16, 247 positives and 309 negatives.
  - 339 are wake clips: 242 controller wake clips named by epoch milliseconds (saved 2026-08-15 to 08-23), 87 named `office-<date><time>-wake.wav` (the newest 2026-09-20 19:08) and 10 others.
  - The other 217 are STT clips (`…_stt-stt.faster_whisper.wav`).
  - The clips are canonical PCM from before the Fire OS 6 port.
- **Presence test.** For each clip, take bit 0 of every sample. Then, over phases 0–127, find the phase at which the most 128-bit frames start with `0xA541`. A clip carries metadata when that fraction is at least 90 %.
  - 439 of the 556 do: all 339 wake clips and 100 of the 217 STT clips.
  - They hold 1,878,292 frames of **sync `0xA5`, version `0x41` (2.1)**.
  - The checksum held on every frame: an 8-bit popcount of bits 127:26, stored in bits 25:18. Bits 17:0 were zero in every frame.
- **The 117 clips without frames are all STT clips.** [INFERENCE: they were saved after the controller's ASR gain (`em_stt_copy`), which rescales samples, so bit 0 does not survive.] This was not tested.
- **Layout**, MSB-first. It comes from `libAFEMetadataDecoder.so` `decodedMetadataBaselineVersion2_x` (0x9330; `nm` lists 0x9331), whose third argument is the minor version.
  - The minor ≤3 path reads two 6-bit fields where minor >3 reads an 8-bit RMS-like value and `COMPUTE_RMS`. Minor 0 stops after the 16-bit field, and a non-zero minor adds three single bits and a 10-bit field. The minor-1 path was used.
  - The slots were matched to the v3.0 decoder's struct: it stores `AFE_TIMESTAMP` at +0x9a and `VOLUME` at +0x9c, and this path writes its 16-bit field and its second 7-bit field to the same offsets.
  - The widths sum to 86, the `PAYLOAD_SIZE` of every frame, which is the cross-check.

  | Bits | Field | Observed |
  |---|---|---|
  | 127:120 | sync | `0xA5` |
  | 119:112 | version | `0x41` (2.1) |
  | 111:105 | `PAYLOAD_SIZE` | 86 |
  | 104:97 | 8-bit, ERLE-like | 0–34 |
  | 96 | 1 bit | 0 |
  | 95:92 | 4 flags | always 0 |
  | 91:85 | 7 bits | 60, constant |
  | 84:78 | the `VOLUME` slot (struct +0x9c) | 0 |
  | 77:72, 71:66 | two 6-bit fields | smoothed ERLE-like |
  | 65:56 | 10 bits | slow, 0–510 |
  | 55:40 | the `AFE_TIMESTAMP` slot (struct +0x9a) | **`0xBEEF`, constant** |
  | 39:26 | four single bits, a 10-bit field | 0 |
  | 25:18 | checksum | popcount(127:26) |
  | 17:0 | padding | 0 |

- **The 8-bit field was non-zero only in 63 STT clips, all in the negatives set.** It was active during the Dot's own playback, such as the chime or a reply. No wake clip showed it; two more clips moved only the 10-bit field.
  - In one example the 8-bit field climbed from 0 to 25 over 0.35 s, sagged to 3–5 during quieter stretches, then rose again.
  - The two 6-bit fields track it smoothly.
- **What Fire OS 5 lacks:**
  - no `PLAYBACK_ACTIVE` (the flags never set, even while the ERLE-like field was active)
  - no frame counter
  - no live timestamp (the `0xBEEF` placeholder)
  - no RMS
  - no VOLUME value
- Field names are unresolved [INFERENCE: the 8-bit field is `ERLE_RAW`, by position and behaviour]. EchoMuse documents Fire OS 5 as "v2.1 present, not decoded" (post-afe-audio-architecture.md §4.5) and never announces the capability there.

---

## 2. What is built, and where each field would plug in

### 2.1 `afe_metadata_v1`

The contract is post-afe-audio-architecture.md §4.5; the wire format is protocol-v1.md §3 (kind 5 record), §4.1 (`afe_metadata_v1`, `session.ready` `afe_metadata`) and §4.4 (`wake.stats.afe`).

- **Device** (`device/internal/audio/afe`, called from the mixer capture binding):
  - It decodes bit 0 of each 80 ms capture period without modifying a sample.
  - A frame counts only with sync `0xA5`, version 3.3, `PAYLOAD_SIZE` 104 and a matching checksum. Lock needs a second valid frame 128 bits later with `FRAME_COUNTER` + 1, and the decoder does not re-hunt while locked.
  - Lost frames are counted from `FRAME_COUNTER`, with `AFE_TIMESTAMP` used only to resolve its modulo-64 wraps. A frame belongs to the period in which its last sample arrives.
  - Each period becomes one 14-byte record: valid frames; flags (`gap`, `sync`, `output_clipped`, `mic_clipped`, `aec_diverged`, `device_mute`); PA frame count; ERLE max, non-zero mean and non-zero count; DTD max and count; RMS max and mean; VAD max and count; `VOLUME`; lost frames. Records sit in an 8 s ring on the capture timeline.
- **Transport.**
  - Records ride kind 5 under uplink leases, and only after the controller opts in with `session.ready` `afe_metadata`.
  - A candidate lease takes them from its reference start, mic start − 40,000 samples (2.5 s), so the AEC state before the wake word is visible. A button turn also takes them from its reference start, and a reply lease takes them live.
  - The stream costs 975 B/s while a lease is live.
  - `wake.stats.afe` carries only decoder health per 30 s window: periods, frames, invalid, syncs, gaps, lost frames.
- **Controller** (`controller/em_afe.py`):
  - `AfeEvidence` is anchored on the candidate's `support_start`. `pre` summarises `[support_start − 1 s, support_start)`, `wake` the support window and `utterance` the committed or closed utterance. `playback_onset` is the capture sample of the last PA rise at or before `support_start`.
  - A rise is placed at the period start + (10 − PA count) × 128 samples, which keeps 8 ms resolution for a clean rise. A playback run whose start was not received has no known rise.
  - It is stored in `turns.afe_evidence` (migration 28) and in the decision trace, for accepted turns and rejected candidates alike. SQL NULL means unavailable; a null part means no data.
  - The dashboard shows it per turn ("Native AFE"), and the decoder counters beside wake health. `wake.stats.afe` is kept for the latest window only and is not persisted.
- **No decision reads any of it.** Wake acceptance, the self-playback verdicts, attribution and endpointing are unchanged.

These choices came from section 1:
- phase lock (1.2);
- ERLE as max + mean of non-zero values + non-zero count, and DTD and VAD as max + count (1.5);
- the per-period PA count for an 8 ms rise;
- the 2.5 s lead on candidate leases;
- the `pre` span and `playback_onset` (rank 1);
- Fire OS 5 as "present, not decoded" (1.12).

### 2.2 On-device validation

The decoder was checked on G090LF0965260F1J running firmware `20261006-2220-dev` (then `20261006-2231-dev`), from the firmware's own `[afe]` line per 30 s stats window in its log. No controller session was involved: the device sat at pending approval.

- **Idle:** every steady window read 3,750 of 3,750 frames valid, with 0 invalid, 0 syncs, 0 gaps and 0 lost. The first window after each start read 3,719 of 3,720 with 1 sync, one frame being spent on confirming the lock.
- **Playback:** stimulus B came from a second mixer client. The capture program recorded the `loopback` stream while it played, so the firmware's `micAsr` was untouched. The window read 3,750 of 3,750 valid, PA in 2,582 frames (20.66 s: the 20 s of noise plus hangover), ERLE max 11 over 2,475 frames, DTD 0, VAD max 0.25 in 1 frame, RMS −85 to −67 dB and volume 70.
- **Forced re-open:** the capture program opened `micAsr` itself. The mixer tore down the firmware's stream, and the firmware logged `mixerapi: micAsr: EHOSTDOWN (stream torn down), reopening`. The capture program received no data and exited. The next window read 3,710 of 3,710 valid, 0 syncs (the lock was kept), 1 gap and 72 lost frames (576 ms). That window's 3,710 + 72 frames exceed the nominal 3,750; nothing recorded explains the difference.

### 2.3 Consumer map

| Problem / consumer | Current mechanism | AFE field that would feed it |
|---|---|---|
| Self-echo on producing-sound candidates | Device: `producing_sound` is the final-mix mask through the clock fits over `[support_start − 2 s, open]` (`supervisor.producingSound`, `wakeword/detector`). Controller: `em_session._decide_candidate` runs the reference-scorer overlap (`_reference_overlap`, `em_attribution.reference_candidate_overlaps`), then the echo comparison (`_schedule_candidate_comparison`, `em_attribution.judge`), then the Kroko verifier (0.40, 700 ms), with `em_attribution.self_playback_verdict`. The trace is `em_session._trace`; rejected rows go through `_persist_candidate`. Both now carry `afe_evidence` | PA, ERLE history, DTD, RMS vs `B`; playback age from the PA rise |
| Echo attribution | `em_attribution.estimate_lag`/`judge`: `MAX_LAG` = 0–500 ms past the clock estimate, `PEAK_MARGIN` 0.10, `ECHO_CORRELATION` 0.92; `EchoTracker`. Mapping: `em_audio_timeline.StreamClock`/`ClockMap` from per-packet `mono_ns`. Fire OS 6 render stamps are modelled (`mixerapi/playclock.go`: 20 ms / 780 ms), with a measured error of +8–11 / +54–89 / +68–88 ms (fireos6-port.md §6) | PA rise as an onset anchor; last ERLE > 0 frame as the drain point |
| Early answer / reply | `em_attribution.EarlyAnswerDetector`: 15 cells, VAD ≥0.85, level ≥ `B` + 12, echo ∈ {near_end_present, no_reference}. `ReplyOnsetScanner`; `em_session._watch_reply` | DTD/ERLE as double-talk evidence (needs E2) |
| Endpointing / VAD | Silero v5 on the ×10 evidence copy (`em_speech_worker`); thresholds in `em_attribution`; reducer `em_endpoint_policy` | `DNN_VAD_PROB` (not recommended) |
| Capture gaps | `capture.Timeline.Add` (`device/internal/audio/capture`) infers lost periods from elapsed time. `mixerapi.Recorder.Drops()` is **not consumed by the supervisor**. Decoder counters: `supervisor/afe.go`, `proto.AFEStats` (`wake.stats.afe`) | `FRAME_COUNTER`, `AFE_TIMESTAMP` |
| Clipping / gain | No mic clipping detection. `em_stt_copy.apply_asr_gain` counts only ASR-gain clips. The render mixer's `saturate` is silent. DAC control 61 (`server/volume.go`). The mixer's 70 is never read back (`mixerapi` package doc, "Gain staging") | `MIC_CLIPPED`, `OUTPUT_CLIPPED`, `VOLUME` |
| Mute | `server/mute.go` (ADC mute through tinymix, `server/hardware.go` `adcMuteControls`); `supervisor.setPrivacy`; muted blocks are discarded in the capture path; controller `em_session._privacy` | `DEVICE_MUTE` |
| AEC health / telemetry | `wake.stats` → `DbStore.wake_stats` → `wake_counters`; `em_device.STATS_KEYS`; dashboard `WakeHealth`; `em_db.MIGRATIONS` (append-only; the latest, v28, added `turns.afe_evidence`) | ERLE distribution during PA, DTD fraction, clip and diverge counts |
| Ambient noise | `em_attribution.BackgroundTracker` (`B`, only inside leases, not persisted) | `RMS`, `DNN_VAD_PROB` (not recommended) |
| Wake clips / training | `em_session._wake_clip` (canonical PCM, support − 300 ms … support_end; bit 0 survives in Fire OS 5 clips, §1.12, and is expected to on Fire OS 6, rank 4); `em_wakeclips.save`, `KEEP_PER_DEVICE` 500. Rejected candidates keep no audio | Every field, decoded offline from the clip itself |

---

## 3. Ranked catalogue

Each entry has these parts:
- **Benefit**
- **Evidence**: what it rests on
- **Unknown**: what is still open
- **Settle**: the measurement that would decide it
- **Risk**: the risk of acting on it
- **Effort**: S, M or L
- **Class**: evidence-only (safe now), or a decision change (post-afe-audio-architecture.md §4.5 requires measurement first)
- **Status**: what is built

Each also notes whether it applies to Fire OS 5.

### Rank 1. Self-echo evidence on producing-sound wake candidates

- **Benefit.** It directly targets the documented failure: both logged TTS barge-ins were the device waking on its own answer 1.2–1.55 s in (post-afe-audio-architecture.md §5.3, [D3]). Today the defence is the Kroko verifier. It rejected 4 of the 4 barge-in clips, but it costs a decision deadline of 700 ms.
  - The metadata adds what the controller cannot see: the AEC's state just before the wake word, and the playback age on the AFE's own timeline.
  - Amazon's own suppressor tests exactly this, as an ERLE fraction over the wake-word span with shadow and probabilistic modes (alexa-turn-control.md, "Suppression: four vetoes, all probabilistic, all with a shadow mode").
- **Evidence.**
  - On a fresh AFE the residual sits 8–11 dB above its 1 s level at 0.1 s, and 1.4–9 dB above its 2.5 s level at 1 s. ERLE reaches 10 only after 0.4–2 s (section 1.5).
  - DTD and VAD fire on the device's own unconverged echo (section 1.6).
  - PA gives an exact playback onset, so playback age = `support_start` − last PA rise.
- **Status: implemented as evidence** (section 2.1). Each wake turn and rejected candidate stores `pre`, `wake` and `playback_onset`. The summaries carry ERLE max, non-zero mean and count, PA frames, DTD max and count, RMS and VAD. Derived features still to compute in the analysis: playback age = `support_start` − `playback_onset`, and (RMS mean − `B`) during PA (`B` is not persisted).
- **Unknown.**
  - Whether EchoMuse's always-open stream pair ever presents an unconverged AEC at answer start. The candidate causes are AFE reset on reopen, decay over long idle, a volume change, or movement (E1).
  - Whether any feature separates self-echo from a real talker (E2).
  - The self-wakes were on Fire OS 5, a different AFE: 7 mics, 4 ms hop.
- **Settle.**
  - E0 (Fire OS 5 barge clips)
  - E1 (convergence conditions)
  - E2 (separability, AUC ≥0.95 needed)
  - E7 (field: features against verdicts and user-confirmed barge-ins)
- **Risk.**
  - None while it stays evidence.
  - As a decision, ERLE alone would reject genuine barge-ins: double talk lowers ERLE, and quiet playback has low ERLE anyway (`n1`: 2–5).
  - DTD alone would accept self-echo (0.68).
  - post-afe-audio-architecture.md §6.1 already forbids near-end evidence from accepting on its own.
- **Effort.** S for the evidence (done); M for the analysis.
- **Class.** Evidence-only now. A later shadow rule inside `self_playback_verdict` is a decision change: shadow first, Amazon-style.
- **Fire OS 5.** Partial at most. The ERLE-like field exists but is unvalidated (E6), there is no PA, and the decoder does not handle v2.1.

### Rank 2. Capture-integrity accounting

- **Benefit.** It is the only sample-exact, clock-independent check of the capture timeline. Every sample-indexed contract rests on that timeline:
  - the wake grid at multiples of 2,560 (post-afe-audio-architecture.md §5.2)
  - lease backfill
  - reference alignment and the echo comparison

  Today two things are blind:
  - `Timeline.Add` infers `Missing` ranges from elapsed time, so a wrongly inferred gap shifts every later sample.
  - `mixerapi.Recorder.Drops()` is never read.
- **Evidence.** The counter was +1 over 29,538 frames, and `AFE_TIMESTAMP` is monotonic ms (section 1.2). The forced re-open in section 2.2 was counted as one gap of 72 frames.
- **Status: partly implemented.** The decoder's counters (`periods`, `frames`, `invalid`, `syncs`, `gaps`, `lost_frames`) travel in `wake.stats.afe`, and the dashboard shows them beside wake health, disabled with a reason when the capability is absent. Each record carries its `gap` flag and `lost` count.
- **Still to add:**
  - Device: per stats window, compare the counter-lost frames with the frames `Timeline.Add` declared `Missing`, plus `Drops()`. Report a disagreement count. Also report "timestamp jump with counter +1", which means an AFE input-side stall.
  - Controller: persist `wake.stats.afe` in `wake_counters` (a new migration) so rates survive beyond the latest window.
- **Unknown.** Field loss rates, and whether the mixer's `ts` already reveals every drop.
- **Settle.** E7: a passive week of counters across Dots.
- **Risk.** None as telemetry. Turning a counter gap into `FlagDiscontinuity` (resets VAD/ASR, closes the utterance as `interrupted`) is a decision change that needs the E7 rates first.
- **Effort.** S.
- **Class.** Evidence-only.
- **Fire OS 5.** No: there is no counter and no timestamp.

### Rank 3. The AFE as the measured playback timeline

- **Benefit.** The reference is mapped into mic time through `ClockMap`, built from the modelled render stamps. Its error was measured at +8–11 / +54–89 / +68–88 ms (fireos6-port.md §6). The design absorbs it two ways: a 0–500 ms lag search (`em_attribution.MAX_LAG`), and reference-overlap windows widened by `MAX_LAG` + 2 × uncertainty (`reference_candidate_overlaps`).
  - **A PA rise is a measured anchor once per response,** at ≤8 ms resolution: the reference onset lands in the PA-rise frame, and the echo follows 829 samples later.
  - **The last ERLE > 0 frame is a measured drain point.** That bears on the 150 ms drain guard (post-afe-audio-architecture.md §17, "Presentation timing?").
- **Evidence.** Sections 1.3–1.5. The PA rise sits 71–116 samples before the onset (within one frame). The beam's offset of +828 to +831 samples was stable over 4 captures and 2 boots (alexa-afe.md, "The reference is sample-aligned to the microphones"). ERLE is 0 in silent-reference frames.
- **What to log.** The controller would log, per response:
  - `pa_rise_capture` − `ClockMap.reference_to_capture(first non-silent kind 2 reference sample)`
  - the same for the drain
  - the lag `estimate_lag` chose

  Later, if the error is stable, PA rises could feed `StreamClock` as anchors. That would allow narrowing `MAX_LAG` and the overlap windows, which means fewer `self_output` overlaps with genuine wakes and quicker lag acceptance.
- **Status: proposal.** `playback_onset` (rank 1) already places the last PA rise before a wake; nothing compares it with `ClockMap`.
- **Unknown.**
  - The PA threshold for soft TTS onsets (a fade delayed the rise by one frame).
  - Whether the residual's coherent lag (829 → ~575 samples after convergence) breaks the lag accounting.
  - Field drift.
- **Settle.** E7 study of the logged offsets. E1 also yields offsets for real TTS onsets.
- **Risk.** None as logging. As a clock source, a missed or late PA rise would misplace the reference, so keep the existing fit and only add anchors.
- **Effort.** M.
- **Class.** Evidence-only now; feeding anchors to the clock is a decision change.
- **Fire OS 5.** No PA. [INFERENCE] The ERLE-like field's onset could serve as an anchor, but it is unvalidated.

### Rank 4. Offline decode of clips already saved

- **Benefit.** No protocol change and no device. Canonical-PCM wake clips keep bit 0: on Fire OS 5 this is proven (section 1.12). On Fire OS 6 [INFERENCE: no step from the mixer capture to `_wake_clip` changes a sample; no saved Fire OS 6 clip has been decoded yet].
  - **(a) E0.** Decode the four Fire OS 5 barge-in clips (`data/wakes/G090LF10728426PR/{421,426,322,356}.wav` on the controller host, [D3]). This tests the architecture's "AEC plausibly still converging" [INFERENCE] directly on the clips that motivated it.
  - **(b) Training labels for BCResNet's playback-leak data** (post-afe-audio-architecture.md §5.4 asks for device-channel playback-leak negatives). Per clip: playback present, ERLE profile, clipping.
  - **(c) Hygiene.** Clear bit 0 before features if desired. Amazon strips it before scoring [INFERENCE in alexa-afe.md]. The metadata is a ≤1-LSB signal 90 dB down, so this is optional.
- **Evidence.** Section 1.12, and the v3.3 checksum on `micAsr`.
- **Status:** E0 ran on 2026-10-07 on an export without the four barge-in clips (section 5). Nothing decodes saved clips routinely.
- **Unknown.** The Fire OS 5 field names and semantics (E6). Rejected candidates save no audio, so for them only `afe_evidence` survives.
- **Settle.** E0 and E6.
- **Risk.** Over-reading unvalidated Fire OS 5 fields.
- **Effort.** S: the decodes of sections 1.1 and 1.12 are a few dozen lines of numpy on the controller host.
- **Class.** Evidence-only.
- **Fire OS 5.** Yes; this is the one use that covers it today.

### Rank 5. Echo-cancellation health per device

- **Benefit.** Placement, volume and hardware diagnostics. Amazon tracks ERLE against placement classes (corner, wall, free space), plus divergence counts (`AFEDiagAverageERLE`, `AFEDiagAECDivergenceCount`; alexa-turn-control.md, "What the diagnostic command *could* return").
  - A low steady-state ERLE at normal volume, or a high DTD-during-playback fraction, flags a Dot that is likely to self-wake.
- **Content.** The device already knows its render RMS from the final-mix tap (`supervisor/render.go`), so ERLE can be gated by it. Per stats window, report:
  - the ERLE p50/p90 over periods with ≥2 s of continuous PA and reference RMS above a level to be set from E5
  - DTD frames during PA
  - `MIC_CLIPPED`/`OUTPUT_CLIPPED`/`AEC_DIVERGED` counts
- **Status: proposal.**
- **Unknown.**
  - ERLE depends on echo level (section 1.5), so cross-device comparison needs a level gate, and E5 gives the dependence.
  - The flags were never seen to fire.
- **Settle.** E5 and E4, then E7.
- **Risk.** A misleading "bad AEC" verdict if it is not level-normalised.
- **Effort.** S–M: a `wake.stats.afe` extension, a new `wake_counters` migration, and a dashboard card disabled with a reason when the capability is absent.
- **Class.** Evidence-only.
- **Fire OS 5.** Partial (ERLE-like field only), after E6.

### Rank 6. A PA-based `producing_sound` window

- **Benefit.** The device decides `producing_sound` over a fixed 2 s lookback (`wakeword/detector`). Replacing that with "PA or its hangover overlaps the support" sends wakes said just after a response down the idle path. That avoids the verifier's latency and its rejections; the verifier passed 23 of 24 idle wakes (post-afe-audio-architecture.md §16.6).
- **Evidence.** PA follows reference energy, and its 0.5–0.8 s hangover plausibly covers the room tail [INFERENCE].
- **Status: proposal.**
- **Unknown.** The residual echo after PA falls. It would show in the E1 tails.
- **Settle.** E1: beam residual in the 0–1.5 s after the reference ends, against PA.
- **Risk.** Self-wakes on the reverberant tail if the hangover is too short.
- **Effort.** M.
- **Class.** Decision change.
- **Fire OS 5.** No.

### Rank 7. Double-talk support for early answer and the near-end rescue

- **Consumer.** `EarlyAnswerDetector` and the rescue in post-afe-audio-architecture.md §6.1 step 1. DTD and ERLE drops could corroborate `near_end_present`.
- **Evidence.** None for real double talk. DTD has positive evidence only for the device's **own** echo.
- **Status: proposal.**
- **Settle.** E2.
- **Risk.** It could accept own-residual as an answer, which would cancel the device's question.
- **Effort.** M after E2.
- **Class.** Decision change.
- **Fire OS 5.** No.

### Rank 8. Mute verification

- **Idea.** Compare `DEVICE_MUTE` with the privacy state (`server/mute.go`) on the device, before muted blocks are discarded, as a counter.
- **Benefit.** Diagnosing a deaf Dot whose mute state disagrees.
- **Status: proposal.** Records already carry the `device_mute` flag.
- **Unknown.** Whether the field tracks EchoMuse's ADC mute or only Amazon's `amz_privacy` path.
- **Settle.** E3.
- **Effort.** S.
- **Class.** Evidence-only.
- **Fire OS 5.** No.

### Rank 9. Clipping alarms and VOLUME read-back

- **Benefit.** `MIC_CLIPPED` during loud playback would warn of nonlinear echo (self-wake risk) and could justify a volume ceiling. `VOLUME` would confirm the mixer's 70, the scalar the DAC cannot reach.
- **Evidence.** No clip flag fired on the microphone side; `VOLUME` stayed constant across a 13–23 dB change in echo level.
- **Status: proposal.** Records already carry the clip flags and `VOLUME`.
- **Settle.** E4 and E5.
- **Effort.** S.
- **Class.** Evidence-only. A volume ceiling would be a decision change.
- **Fire OS 5.** No.

---

## 4. Would not do, and why

- **`DNN_VAD_PROB` as a VAD second opinion, for endpointing, early answer, or the speech exclusion in `B`.**
  - It is 2-bit, and it gave 0.75 for 1.1 s on own-echo residual with nobody talking. It fired only on onsets: 0.32 s of a 4 s sound sitting 15–25 dB over the floor.
  - Silero already runs on the same samples, and `B` is a 20th percentile that tolerates speech (post-afe-audio-architecture.md §16.6).
  - Its speech sensitivity was never measured.
- **`DTD` or `UED` as near-end acceptance or rescue.** DTD reached 0.68 and UED 0.70–0.84 with no talker. §6.1 of the architecture says near-end evidence never accepts on its own, and nothing here changes that until E2.
- **ERLE-based rejection now.** Low ERLE also comes from quiet playback (2–5 after 20 s at −37 dBFS) and from real double talk. Rejecting on it would cut genuine barge-ins; it needs E1 and E2 plus shadow time.
- **`RMS` as a noise floor or for wake-threshold context.** It is the same samples' level within 0.5 dB, so it duplicates `E`. A device floor (for example the p20 of `E` while PA = 0) needs no metadata. Only the PA gating is new, and that is a minor gain.
- **`SSL`/`FD` direction, `ULTRA_PROX`, `SIGNAL_TO_ECHO_RATIO`, `LPM`.** All constant.
- **`VOLUME` as EchoMuse's speaker volume.** It stayed at 70 across a 13–23 dB change in echo level [INFERENCE: the DAC differed after the reboot]. EchoMuse already owns the DAC value (`session.hello.volume`).
- **`AFE_TIMESTAMP` as a sample or render clock.** It is processing time with ±7 ms of paired jitter. Use it only to de-alias counter gaps.
- **Any Fire OS 5 decision.** There is no PA, counter or timestamp, and the field names are unresolved. Use only offline evidence (rank 4) until E6.
- **Switching to `micMultiChAsr` for the AFE-aligned reference** (alexa-afe.md, "What EchoMuse should do, ranked", item 3). That is a separate decision, and rank 3 gets most of the timing value from `micAsr` alone.

---

## 5. Experiment specs, in run order

Common tools:
- **Capture program:** the C program of alexa-afe.md ("How the measurements were made"): egid `aipc`, `MixerOpenRecCh` and an optional `MUSIC` play thread, raw S16LE output plus per-block and per-chunk logs.
- **v3.3 decode:** section 1.1.
- **v2.1 decode:** section 1.12. For each clip, find the phase at which ≥90 % of frames start with `0xA541`, then slice the fields per the layout table.

### E0. Decode the Fire OS 5 barge-in clips (controller host, no device, about 15 min)

- **Input.**
  - `data/wakes/G090LF10728426PR/{421,426,322,356}.wav` on the controller host (the [D3] barge-in clips)
  - all other saved wake clips for that device
  - for comparison, the Fire OS 5 STT clips that show playback activity
- **Method.** v2.1-decode each clip. Per 80 ms (10 frames), print the 8-bit field's max, both 6-bit fields, the 10-bit field and the level of the samples (bit 0 cleared). Place each clip in its answer from the turn's row and the controller log timestamps.
- **Decides.** Compare the field with the playback level at the same instant: a reading ≤5 is routine whenever the echo is quiet (the 2026-10-07 run below shows it).
  - If the field is low over the barge clips while the playback level is high, "AEC still converging" gains direct support (conditional on E6).
  - If it is high (≥15) while the answer plays, the self-wakes happened despite a converged AEC: the residual itself is speech-like. Ranks 1 and 6 then lose value, and the verifier stays the defence.

**Result, 2026-10-07.** Run on the labelled export of section 1.12 (556 clips), not on the controller host's `data/wakes`.

- **The four barge-in clips are not in the export.** Its newest wake clip is `office-2026-09-20190803-wake.wav` (2026-09-20 19:08; the filename's time zone is not established). The [D2] log window that holds turns 419–427 runs 2026-09-20 21:51 to 09-23 UTC (post-afe-audio-architecture.md §14). Turns 420 and 425 therefore come after the export [INFERENCE: their own timestamps were not read]. Turns 322 and 356 may be in it, but none of the 339 wake clips shows the 8-bit field active. If they are there, the field read 0 throughout.
- **Every active run starts the clip.** 63 STT clips show the field, and in 60 it is active within the first 0.3 s. Run lengths: median 0.58 s, p90 1.90 s, max 6.40 s. 10 runs last past 1.55 s.
- **The eight clips whose run lasted at least 1.6 s**, in 0.25 s bins from the run's start. The field value is the bin's median. The level is 20·log10 of the RMS of the bin's 4,000 samples (bit 0 included).

  | Clip | 8-bit field per bin | Level per bin (dBFS) |
  |---|---|---|
  | 4110627168963611 | 16, 8, 0, 0, 0, 2, 1, 1, 0, 0 | −53, −44, −43, −61, −62, −61, −62, −61, −62, −61 |
  | 4195774854164465 | 4, 7, 8, 10, 8, 7, 10 | −58, −53, −64, −75, −76, −75, −75 |
  | 4196051344377784 | 22, 8, 12, 5, 4, 4, 0, 2, 3, 5, 3, 1 | −65, −53, −65, −74, −73, −74, −73, −72, −75, −75, −74, −74 |
  | 4196075978899061 | 19, 6, 1, 10, 3, 0, 8, 6, 8, 1 | −67, −56, −51, −73, −75, −75, −74, −75, −76, −76 |
  | 4196084529162561 | 19, 20, 0, 2, 5, 9, 7, 9, 7, 8, 9, 8 | −48, −52, −48, −67, −73, −73, −74, −73, −73, −73, −74, −74 |
  | 4196314719309722 | 15, 15, 0, 1, 3, 3, 0, 7, 12, 3, 1, 0 | −73, −54, −46, −63, −70, −74, −63, −71, −75, −69, −73, −74 |
  | 4201557672511686 | 16, 19, 0, 4, 1, 2, 5 | −48, −49, −47, −70, −74, −70, −74 |
  | 4201585995445559 | 20, 4, 5, 3, 3, 13, 6 | −63, −51, −54, −73, −75, −74, −76 |

  - In 7 of the 8 the field reads 15–22 in the first bin, during the loud start (each clip's loudest bin is −43 to −53 dBFS). [INFERENCE: the loud start is the wake chime, the first thing the Dot plays after a wake.]
  - Once the level is down at the floor (−73 to −76 dBFS; −61 to −62 in the first clip), the field reads 0–13.
  - No clip in the export covers 1.2–1.55 s into a reply.
- **The field is already 15–22 within the first 0.25 s of playback that follows silence** [INFERENCE: the clip starts within about 0.3 s of the chime]. If the 8-bit field is ERLE (unvalidated: E6), Fire OS 5's AEC does not restart from zero after idle, which weakens "still converging". E1 settles that on Fire OS 6.
- **The decision rule as first written cannot discriminate.** A reading ≤5 is routine whenever the echo is quiet (here, the chime's tail). The rule above therefore compares the field with the playback level at the same instant, not with fixed thresholds of 5 and 15.
- **Still needed:** `data/wakes/G090LF10728426PR/{322,356,421,426}.wav` themselves (from the controller host, or an export that covers 2026-09-20 to 09-23), and E6 to validate the field.

### E1. AEC convergence and persistence (Fire OS 6 Dot, about 60 min)

- **Setup, preferred.** EchoMuse firmware with `afe_metadata_v1`, with playback through EchoMuse's own render path (an HA announcement, or a dashboard test sound).
  - It needs a lease that carries mic + afe + reference for the whole run. Today's `diagnostic` lease (collect mode) wants only mic, so it must also want `afe` and `reference`, a controller change.
  - Alternatively, decode the diagnostic mic PCM offline, after first checking that the saved samples keep bit 0.
- **Setup, alternative.** The capture program, changed to open `micAsr` and one `MUSIC` play stream once and hold both open for the whole run, writing zeros while idle, as the render mixer's `Run` does.
- **Record** the DAC control 61 value, and stop Amazon services as usual.
- **Stimulus.**
  - (a) A real HA TTS reply of about 4 s at the normal volume, ideally the answer to "How many tablespoons is eight ounces?" (turns 420/425).
  - (b) Stimulus A as a broadband check.
- **Schedule.** Play (a) after:
  - idle gaps of {1, 5, 15, 60, 180} s, 3 repetitions each in random order
  - then blocks of 3 repetitions, each with a 3 s pause before playback, after: (i) closing and reopening `micAsr`; (ii) closing and reopening the play stream; (iii) DAC +8 steps (+4 dB); (iv) DAC −8 steps; (v) optionally, moving the Dot 20 cm
- **Log.**
  - per-frame metadata, all fields
  - beam PCM
  - render handoff stamps and the TTS file offset
  - condition markers
- **Metrics,** per playback from the PA rise:
  - ERLE max and the mean of non-zero values per 80 ms over 0–3 s
  - the beam residual in 100 ms bins, against the converged reference (the median of the 1 s-gap repetitions)
  - DTD max and frames, VAD frames, UED
  - residual and PA in 0–1.5 s after the reference ends (for rank 6)
- **Decision rules.**
  - A condition with residual ≥6 dB above converged over 0.3–1.6 s **and** ERLE max <8 is an "unconverged window".
    - The pre-wake ERLE history (rank 1) is then a valid marker for that condition.
    - Operational fixes then become options: avoid unnecessary reopens, and treat N s after a volume step or reopen as high-risk.
  - If every condition starts converged (ERLE ≥10 within 100 ms, residual within 3 dB), self-echo on Fire OS 6 is not an AEC-convergence effect. ERLE gating then drops and the verifier remains the defence.
  - Tail data decides rank 6: the residual must be at `B` + ≤3 dB by the time PA falls.

### E5, E3, E4. Quick checks in the same sitting (about 25 min)

**E5. VOLUME (5 min).**
- **Run.** Capture `micAsr` for 60 s while stepping DAC control 61 through {47, 79, 103, 127}, with a click + 3 s noise burst (the first 5 s of stimulus A) at each level.
- **Log.** VOLUME, ERLE, residual.
- **Decides.**
  - If VOLUME is constant, it is the mixer's main volume and not a speaker read-back (rank 9).
  - The ERLE and residual against DAC level give the level dependence that rank 5 needs for normalisation.

**E3. Mute (10 min).**
- **Run.** Use the capture program, not EchoMuse, which discards muted blocks. Capture `micAsr` for 60 s:
  - at 10 s, press the physical mute button (Amazon's `amz_privacy` path) for 10 s
  - at 30 s, set the ADC mute controls EchoMuse uses (`tinymix -D 0 <ctl> 1` for controls 105, 106, 123, 124, 141, 142, 159 and 160; `server/hardware.go` `adcMuteControls`) for 10 s
- **Log.** `DEVICE_MUTE`, RMS, counter continuity (does the stream keep flowing?), mixer logcat.
- **Decides.** Rank 8 is worth doing only if `DEVICE_MUTE` tracks the mute EchoMuse applies.

**E4. Clipping (10 min).**
- **Run.**
  - (a) 5 claps and a shout at 10 cm
  - (b) a bass-heavy music track at DAC 127
  - (c) the sweep of stimulus A at DAC 127
- **Log.** `MIC_CLIPPED`, `OUTPUT_CLIPPED`, `AEC_DIVERGED`, beam peaks.
- **Decides.**
  - If the flags never fire, keep them as counters only, with no UI.
  - If `MIC_CLIPPED` fires during loud playback, rank 9 becomes a gain-staging alarm and a candidate volume ceiling (a decision).

### E2. Double-talk corpus (Fire OS 6 Dot plus a second loudspeaker or a person, about 90 min)

- **Setup.**
  - EchoMuse with `afe_metadata_v1`, decision traces including `afe_evidence`, and saved wake clips.
  - A second speaker 1.5 m away playing recorded positives of the household's wake word (for example the controller's saved wake clips, `data/wakes/<device>/`) at about 60 dBA at 1 m, standing in for the talker.
- **Conditions.**
  - (A) A self-only answer, 20 runs, including the turn 420/425 answers.
  - (B) Talker only, no playback, 20 runs.
  - (C) Answer plus talker at offsets {0.3, 0.8, 1.5, 3.0} s into the answer, 5 runs each, at 2 DAC levels.
  - (D) An answer right after a stream reopen (unconverged, if E1 shows that state exists) plus a talker at 1.2 s, 5 runs.
- **Log.** Candidates, verdicts, AFE per-frame data (or the 80 ms records), mic, reference.
- **Metrics.**
  - The DTD distribution over the wake span for A, C and D: the first measurement of DTD under real double talk.
  - Pre-span ERLE, playback age, VAD and UED frames, and (RMS − `B`).
  - AUC for self (A, and the unconverged runs) against talker (C, D).
- **Decides.** Only a feature with AUC ≥0.95, and a threshold losing ≤1 % of genuine barge-ins, may become a **shadow** rule in `self_playback_verdict`. Otherwise the metadata stays evidence for barge-in, and rank 7 is dropped.

### E6. Fire OS 5 v2.1 field validation (office Dot, Fire OS 5, about 20 min)

- **Run.** Hold an EchoMuse `diagnostic` lease (collect mode) and play stimulus A through EchoMuse's render path, at two volumes.
- **Method.** v2.1-decode the recorded mic PCM. Compare the 8-bit, 6-bit and 10-bit fields with render onsets and the beam residual, as section 1.5 does for v3.3.
- **Decides.**
  - "8-bit = ERLE_RAW" holds if the field is 0 without reference energy, rises during the first playback, and peaks at 15–30 when converged. Fire OS 5 could then carry ERLE-only evidence (no PA or counter), and E0's reading becomes trustworthy.
  - Otherwise Fire OS 5 stays "present, not decoded".

### E7. Passive field study (after ranks 1–3 deploy; 1–2 weeks; no device time)

- **Analysis.** Decision traces and `wake.stats.afe` across Dots:
  - (a) Capture-integrity rates and disagreements (rank 2).
  - (b) PA-rise and drain offsets against `ClockMap` (rank 3).
  - (c) Candidate features against verdicts and against user-confirmed barge-ins, meaning a repeated question as with turns 420/425 (rank 1).
- **Decides.** Whether counter gaps should become `FlagDiscontinuity`, and whether PA anchors should feed `StreamClock` and narrow `MAX_LAG`.

---

## 6. Not established

- Behaviour of `DTD`, `DNN_VAD_PROB` and `UED` under real near-end speech; no capture contains a talker.
- Whether `DEVICE_MUTE`, `MIC_CLIPPED` or `AEC_DIVERGED` ever fire on this unit, and the exact `OUTPUT_CLIPPED` threshold.
- Whether the AEC state survives a stream reopen or long idle (E1).
- What `VOLUME` follows, if anything (E5).
- The Fire OS 5 v2.1 field names and semantics (E6), and whether any Fire OS 6 wake clip saved by the controller keeps bit 0.
- Field behaviour on Dots other than G090LF0965260F1J.
