# capture_mics — Biscuit Mic Array Channel Mapping Tool

Standalone tool for mapping the 9 ALSA channels to physical mic positions
on the Echo Dot Gen 2 (biscuit) mic PCB.

## What this is for

The biscuit has a 7-mic array (6 perimeter + 1 centre) on 4× TLV320ADC3101
ADCs giving 8 channels, plus 1 unknown channel = 9 total. Amazon's firmware
mapping is closed-source. This tool captures raw 9-channel audio and an
analysis script identifies which channel corresponds to which physical mic
by playing a tone from known angles.

## Build

`device/tools/build_tools.sh` (run from `device/`) builds every tool. By hand,
inside the `echomuse-compiler` image, which is the firmware toolchain stage of
`controller/Dockerfile`:

```bash
docker build -f ../../controller/Dockerfile --target compiler -t echomuse-compiler ../..
```

then:

```bash
docker run --rm \
  -v "$(pwd)":/capture \
  -v "$(pwd)/../GoTinyAlsa":/GoTinyAlsa \
  echomuse-compiler \
  bash -c "cd /capture && go build -tags server -o capture_mics ."
```

## Deploy

The Fire OS 6 adb shell is already root:

```bash
adb push capture_mics /data/local/bin/capture_mics
adb shell chmod 755 /data/local/bin/capture_mics
```

**Note:** Stop EchoMuse first — both tools need exclusive access to ALSA device 24,
and they stop Amazon's `mixer` daemon to get it, so start it again afterwards:

```bash
adb shell stop echomuse
# ... run capture ...
adb shell start mixer
adb shell start echomuse
```

## Test procedure

You need a consistent tone source — a phone playing a 440Hz sine wave works well.
Mark the front of the Echo Dot as the action button side. That's 0°.

For each of the 6 positions (0°, 60°, 120°, 180°, 240°, 300°):

1. Place tone source ~50cm directly in front of the device at the current angle
2. Capture:
   ```bash
   adb shell /data/local/bin/capture_mics 5
   adb pull /data/local/tmp/capture.raw capture_0deg.raw   # adjust name per angle
   ```
3. Analyse:
   ```bash
   python3 analyse_capture.py capture_0deg.raw --label "0deg_action_button"
   ```
4. Note the loudest channel

After 3–4 positions the mapping is unambiguous. The centre mic (ch8 most likely)
will be consistently loud regardless of angle.

## Output

`capture.raw` — raw interleaved S24_3LE, 9 channels, 16kHz.

Format: `[ch0_b0][ch0_b1][ch0_b2][ch1_b0]...[ch8_b2]` per frame, 16000 frames/sec.

File size for 5 seconds: `5 × 16000 × 9 × 3 = 2,160,000 bytes` (~2.1MB)

## Analysis

```bash
# Basic RMS table
python3 analyse_capture.py capture_0deg.raw

# With waveform plot (requires matplotlib)
python3 analyse_capture.py capture_0deg.raw --plot --label "0deg"
```

Install deps if needed:
```bash
pip3 install numpy matplotlib
```

# slprobe — OpenSL ES capture/playback probe (Echo Show 8, `crown`)

C rebuild of the probe in `docs/echo-show-crown.md` §4.4, used for the Phase 0
measurements of `docs/echo-show-port-plan.md` (results in findings §10). It
records through Android's AudioFlinger, the way the Show's firmware will, so
nothing has to be stopped first; `audioserver` must stay running.

- **Capture:** `SLDataLocator_IODevice` → `SLAndroidSimpleBufferQueue`, 16-bit
  mono, `SL_ANDROID_KEY_RECORDING_PRESET` set before `Realize`. Each buffer
  completion is stamped with `CLOCK_MONOTONIC` in the callback.
- **Playback (optional):** an OpenSL buffer-queue player of a WAV (PCM16) or raw
  S16LE file on a chosen `SL_ANDROID_KEY_STREAM_TYPE`, started `-D` ms after
  `SetRecordState(RECORDING)`; each enqueue is stamped.
- **Output:** `PREFIX.raw` (S16LE mono) and `PREFIX.ts` (text: `T0`, one `R` line
  per completed buffer, `P` per player enqueue, `E` record events, `X` stalls;
  format in the source header). A status line per second shows completions,
  RMS and the digital-zero fraction, and `STALL`/`capture resumed` when buffers
  stop and restart. `-x MS` cycles a stalled recorder (stop, clear, record)
  every MS. `-d 0` runs until SIGINT/SIGTERM; `-o none` writes no files.

`build_tools.sh` builds it with the image's NDK clang (`$CC`,
`armv7a-linux-androideabi22-clang`, `-lOpenSLES`). Options: `slprobe -h`.

```bash
adb push build/slprobe /data/local/tmp/slprobe
python3 tools/slprobe/stimulus.py loud loud.wav        # also writes loud.seg
adb push loud.wav /data/local/tmp/
adb shell /data/local/tmp/slprobe -p vr -d 29 -i /data/local/tmp/loud.wav -s music -o /data/local/tmp/cap
adb pull /data/local/tmp/cap.raw /data/local/tmp/cap.ts .
python3 tools/slprobe/decode.py cap --stim-seg loud.seg  # or 1 s windows without --stim-seg
adb shell 'rm /data/local/tmp/slprobe /data/local/tmp/loud.wav /data/local/tmp/cap.*'
```

`decode.py` and `stimulus.py` need numpy. `decode.py` recovers the v3.3 bit-0 metadata with the firmware
decoder's rule (`internal/audio/afe`: sync `0xA5`, version `0x63`, payload 104,
popcount checksum) and prints capture timing plus, per segment, the level,
`PLAYBACK_ACTIVE`, `ERLE_RAW`, `DTD`, `DNN_VAD_PROB`, `UED`, the clip, divergence
and mute flags, `VOLUME`, and how long after each frame's `AFE_TIMESTAMP` its
buffer completed. `--csv` dumps every frame. `stimulus.py` writes the
`docs/echo-show-crown.md` §4.4 stimulus (`echo`) or the louder Phase 0
programme (`loud`).

# sl_latency — render-to-capture lag through the firmware's own audio path (Echo Show 8, `crown`)

Measures what `internal/opensl`'s `playPipelineNs` is a bound for
(`docs/echo-show-port-plan.md` decision 4, Phase 1): how the firmware's render
stamps line up with its capture stamps. It is Go inside the firmware module
and opens audio exactly as the firmware does (`slmic.OpenOpenSL`,
`slspeaker.NewOpenSL`): one `VOICE_RECOGNITION` recorder, capture periods
stamped from `AFE_TIMESTAMP`, and an OpenSL player on `STREAM_MUSIC` fed
`render.SinkFrames` writes, stamped from the queue depth.

- **Run:** a silent lead-in, then `-n` bursts (`-burst` long, a `-freq` Hz
  tone or white noise at `-level` dBFS peak), each starting a write, `-gap`
  apart; then, unless `-stall 0`, one more burst after the writer has stalled
  `-stall` so the queue ran dry; then a silent tail.
- **Reference onset:** the burst's first frame as the render accounting
  stamps it, the completion stamp of its write less one write. `fit-ref` is
  the supervisor's render clock fit for that frame at the moment it was
  written, against it.
- **Capture onsets,** on the firmware's capture stamps, within 300 ms before
  to 1.5 s after the reference onset:
  - `pa`: the AFE frame where `PLAYBACK_ACTIVE` rose. The AFE computes it on
    its input timeline, so it leads the echo in its output (51.8 ms on the
    Dot, `docs/afe-metadata.md` §1.3); 8 ms resolution.
  - `erle`: the frame where `ERLE_RAW` rose.
  - `pcm`: the burst in the AEC residual, by matched filter (energy at
    `-freq`, or total energy for noise, over a burst-length window). This is
    the lag the controller's echo comparison sees. It counts only `-min-snr`
    dB over the room: `VOICE_RECOGNITION` cancels tones by 37–49 dB, so use a
    higher `-index` or `-wave noise`.
- **Report:** one row per burst (lags in ms; `pcm_lag` must never be
  negative, which the report flags), the lag summaries, `pcm − pa`, the AFE
  decoder's totals (periods, frames, invalid, syncs, gaps, lost), how many
  periods were stamped from `AFE_TIMESTAMP` or at completion, the mic's
  drops, and the distribution of read completion − `AFE_TIMESTAMP`.
- **`-index N`** sets `STREAM_MUSIC` for the run through `service call
  audio` and restores it afterwards (also on Ctrl-C). **`-o PREFIX`** writes
  `PREFIX.raw` (the capture as delivered, bit 0 included),
  `PREFIX.periods.csv` (per period: first sample, stamp, completion, anchor)
  and `PREFIX.writes.csv` (per write: burst, stall, completion stamp).

Stop `echomuse` first: on Android 7.1 the first recorder holds the input and
a later one gets no buffers (findings §10.2), and the tool gives up after
5 s without one. Its pure-Go half, `tools/sl_latency/latency`, has host tests
(`go test ./tools/...`).

```bash
adb shell stop echomuse
adb push build/sl_latency /data/local/tmp/sl_latency
adb shell /data/local/tmp/sl_latency -n 10 -index 18 -o /data/local/tmp/echomuse/lat
adb pull /data/local/tmp/echomuse/lat.raw /data/local/tmp/echomuse/lat.periods.csv /data/local/tmp/echomuse/lat.writes.csv .
adb shell 'rm -r /data/local/tmp/sl_latency /data/local/tmp/echomuse/lat.*'
adb shell start echomuse
```
