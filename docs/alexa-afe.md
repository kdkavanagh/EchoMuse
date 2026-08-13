# Amazon's audio front end on the Echo Dot Gen 2

Reverse-engineering notes on how the stock Alexa stack does acoustic echo
cancellation and beamforming on the same hardware EchoMuse runs on, and an
assessment of what — if anything — EchoMuse can reuse.

Investigated 2026-08-12 against a fielded Dot (FireOS image dated 2022-11-18).

**Nothing from Amazon is vendored into this repo.** The tuning files and
libraries described here are Amazon proprietary and stay on the device. Every
rooted Dot already has its own copy at the paths below; extract from your own
hardware. This is the same line `echo-dot-2-playground` draws.

## Summary

The AEC is not in the audio HAL, not in `libpryon.so`, and not in any Java
app. It is `/system/lib/libasp.so` — Amazon "ASP" (Audio Signal Processing) —
which contains the whole AFE and runs **inside `/system/bin/mediaserver`**,
the same process as AudioFlinger.

The complete tuning is on disk in plain commented JSON at
`/system/vendor/etc/audio-algorithms/`, including the beamformer coefficients
designed for this exact array. That directory is the valuable find; the binary
mostly just tells you how to read it.

## Where everything is

| Path | What |
|---|---|
| `/system/lib/libasp.so` | 1,083,168 B. The AFE. clang 3.5, ARMv7, built Feb 24 2022 |
| `/system/lib/libaspclient.so` | 30,016 B. Binder proxy only |
| `/system/vendor/etc/audio-algorithms/` | 28 files, 761 KB. All tuning |
| `/system/lib/libpryon.so` | 21.6 MB. Wake word + ASR decoder. Consumes AFE output |
| `/system/lib/libwakewordserver_jni.so` | JNI bridge. NEEDED = `libaspclient.so` + `libpryon.so` |
| binder service | `audiosignalprocessor` → `com.amazon.asp.IAudioSignalProcessor` |
| host process | `/system/bin/mediaserver` (confirmed via `/proc/<pid>/maps`) |

Chain: **mediaserver/libasp (AEC + beamform) → binder → wakeword server JNI →
libpryon (WW/ASR)**.

`dumpsys audiosignalprocessor` works on a stock-ish device and prints live AFE
debug JSON. With EchoMuse holding the mic there is no AFE pipeline
instantiated, so it only reports the playback-side volume leveller.

`libpryon` is the decoder only. It consumes AFE **metadata** alongside the
audio — its strings include `ERLE ... from AFE metadata metrics calculator` and
`AFE LSB metadata doesn't support fields of Sub-Base DTD flag or playback
status. Skipping evaluation of DTD for the purposes of NTT self-wake
suppression`. So the AFE hands the decoder ERLE, a double-talk flag and
playback status, and the decoder uses them to suppress self-wake. Channel types
include `BEAMFORMED`; strings `pre-aec` and `post-aec` exist.

### Extracting it

```bash
export PATH="$HOME/.local/bin:$PATH"
adb connect 192.168.3.71:5555
adb pull /system/vendor/etc/audio-algorithms .     # the tuning — start here
adb pull /system/lib/libasp.so .
adb shell su -c 'dumpsys audiosignalprocessor'
```

## The tuning directory

```
AFE.cfg                                 41,622   master config, commented JSON
coefs_FBF.cfg                          460,701   fixed beamformer coefficients
coefs_FilterBank_640.cfg                12,513   ASR analysis/synthesis prototype
coefs_FilterBank_AnalysisSynthesis_1024.cfg 19,974   VoIP filterbank prototype
coefs_FilterBank_160.cfg                 3,231
EQ_{50..100}.cfg, VOIP_EQ_*, MBCL*, UserEQ.cfg, VOIP*ParametricEQ.cfg
```

`AFE.cfg` opens with `// Comments are accepted in JSON configuration files. Use
cJSON_Minify to strip them prior to parsing.` It is written for a human, with
units and rationale in the comments.

```json
"Hardware Definition": {
    "Name"                  : "Biscuit",
    "Num Mics"              : 7,
    "Num Speakers"          : 2,
    "Mics SamplingRate"     : 16000,
    "ASR Path SamplingRate" : 16000,
    "Speakers SamplingRate" : 48000
}
```

Same rates EchoMuse uses. `coefs_FBF.cfg` self-describes as
`coefs_FBF_6beams_64bands_Biscuit`, generated Mar 6 2019.

## Pipeline

The ASR path, in the order `AFE.cfg` states frames are processed:

```
Downsampler IIR → HPF 80Hz @ 16K (mic in AND ref in) → FilterBank
  → AEC → ARA → ABF → VAD → RefBeamSelector → SNRBeamSelector
  → False WW Prevention → Output Gain → EspFeatureExtraction
```

The VoIP path differs and is where the nonlinear stages live:

```
... FilterBank → AEC → Frequency Masking RES → Recursive Magnitude Estimation NR
  → Random Filler CNG → FB AGC → parametric IIR → GainRampUp → limiter
```

Global policy:

```json
// If there is a reference signal, we will use AEC, otherwise we will use ANC
// FBF is always enabled
"Enable AEC/ABF according to ref level" : true,
"Enable ABF" : true,
"Enable AEC" : true
```

### Filterbank

```json
"ASR FilterBank": { "FFT Len": 128, "Decimation Rate": 64,
                    "FilterLen": 640, "AnaSynFilterCoefs": "coefs_FilterBank_640.cfg" }
```

A uniform DFT (WOLA) filterbank: 128-point FFT, hop 64, 640-tap prototype
window (supplied as 640 floats). Hop 64 at 16 kHz = **4 ms frames**, which
matches the `Each number represents average ERLE number of %d frames(4ms)`
string in the binary. 64 usable bands, 125 Hz apart.

The NEON kernels that escaped stripping confirm the implementation:
`neonAllpass2X2F32_DF2T` and `neonBiquad{1X4,2X2}F32_DF2T` for the filterbank,
`neonV4cplxMAC32f` / `neonV4cplxMACCircular32f_optimized` for a **4-wide
complex** MAC, and `neonAdaptAndComputeRefEstimate1` /
`...Circular1` for the adaptive filter update. Float32 throughout. The
coefficient file is laid out 4 bands at a time (`4REAL-4IMAG`), matching the
4-wide SIMD.

### AEC

```json
"ASR AcousticEchoCanceler": {
    "Num Refs Per Input"    : 2,
    "TailLen"               : 2560,
    "MaxStepSize"           : 0.2,
    "adaptBandLoHz"         : 0,
    "adaptBandHiHz"         : 8000,
    "leakyFactor"           : 1.0,
    "DivFactorThd"          : 1.0,
    "RegFactorScale"        : 1E-5,
    "bandBasedTailLen"      : true,
    "bandBasedStepSize"     : true,
    "stepSizeRednScale"     : 5.0,
    "stepSizeErrorScale"    : 5.0,
    "RefSigEnThresh"        : 0,
    "Enable VSS"            : true,
    "Ord2 to Ord1 ratio Percent" : 50   // round robin
}
```

Notes that matter:

- **`TailLen` 2560 samples = 160 ms** at 16 kHz. EchoMuse's `AEC_TAIL_MS`
  defaults to **300 ms** — nearly twice as long. Longer is not better: tail
  length trades convergence speed and steady-state misadjustment against the
  reverberation you can actually cancel.
- One AEC **per microphone**, before beamforming — `SerialAEC: AEC_%d`,
  `NumAECs`, `m_nInputsToAdapt`, and the commented-out
  `"Adapted Inputs Indexes": [0,1,2,3,4,5,6]` (set at runtime to the mic count).
- `bandBasedTailLen` / `bandBasedStepSize` — per-band tail and step size, not
  one global value.
- The VoIP AEC is tuned differently: 1 ref, `TailLen` 3840 (240 ms),
  `MaxStepSize` 0.5, `adaptBandLoHz` 100, and a non-zero `RefSigEnThresh` of
  `3.1623E-6` with the comment *"corresponds to wideband level of -55 dB (ref
  signal)"* — i.e. it refuses to adapt on a reference quieter than −55 dB.

### Beamformer

```json
"ASR FixedBeamFormer": { "Num Source Beams": 6, "Num Coefficients": 4,
                         "BeamFormingCoefs": "coefs_FBF.cfg" }
```

`coefs_FBF.cfg` header:

```
/* It consists of 64 bands 6 beams 4 coefs 7 mics -> 64*6*4*7 = 10752 complex numbers */
```

Parsed and confirmed: exactly 2688 groups of 8 floats (4 real + 4 imag,
covering 4 consecutive bands). So the FBF is a **subband filter-and-sum** — per
band, per beam, a 4-tap complex filter on each of the 7 mics. Not delay-and-sum.

What the coefficients show directly, without needing to pin the convention:

- Each beam weights an **opposite pair** of perimeter mics highest and uses all
  7. At 1000 Hz, normalised per beam, the on-axis pair sits at 1.00/0.99, the
  other four at 0.59, and the centre mic at 0.41. The six beams fall into three
  axis pairs, consistent with six look directions 60° apart aligned to the mic
  axes.
- Phase is symmetric about each beam's axis (mics either side of the axis carry
  equal phase), which is what a beam steered along that axis looks like.
- Of the 4 taps, **coefficient index 2 carries 94.7% of the energy** — a short
  filter with a dominant centre tap, the rest shaping.
- Weighting the whole array rather than the on-axis mic, with a centre-mic
  term, is the signature of a superdirective/MVDR-style design rather than
  delay-and-sum.

On top of the fixed beams sits a **beamspace GSC**: each beam is cleaned using
two *other beams* as noise references.

```json
"ASR AdaptiveBeamFormer": {
    "FixedBeamFormer" : "ASR FixedBeamFormer",
    "Num Beams"       : 6,
    "Num Refs Per Input" : 2,            // 2 nulls per beam
    "Input-References Info" : [ [2,4],[3,5],[4,0],[5,1],[0,2],[1,3] ],
    "TailLen"         : 1536,
    "MaxStepSize"     : 0.1,
    "adaptBandLoHz"   : 200,
    "adaptBandHiHz"   : 7000,
    "leakyFactor"     : 1.0,
    "RegFactorScale"  : 2.5E-4,
    "Enable RoundRobin" : true,
    "VSSLoHz" : 1000, "VSSHiHz" : 6000
}
```

Beam *i* is adapted against beams *i*+2 and *i*+4 (mod 6) — the two beams
120° and 240° away. Adaptation is band-limited to 200–7000 Hz, unlike the AEC
which adapts 0–8000 Hz.

Then a beam is **selected**, not merged:

```json
"ASR SNRBeamSelector": {
    "energyAdaptationFactorFast" : 0.95,   "energyAdaptationFactorSlow" : 0.987,
    "energyRatio" : 1.2,                   "noiseAdaptationFactor" : 1.001,
    "bufferSize" : 10, "hangoverPeriod" : 15, "SNRThreshold" : 6.5
}
```

That top layer is the same idea as EchoMuse's beamformer — a fast/slow energy
ratio per direction with hysteresis. Amazon just selects among six
superdirective beams instead of among six raw mics.

### Residual echo suppression and double-talk

This is the part EchoMuse has no equivalent of. `Frequency Masking RES` carries
**ten columns of tuning, one per volume step**:

```json
"Frequency Masking RES": {
  "numVolume" : 10,
  "aecVssSumTh" : 60,
  "internal": {
    //                    Vol1     Vol2     Vol3     Vol4     Vol5    Vol6 ...
    "erleAecFactor" : [    2.2,     2.1,     2.0,    1.95,    1.85,    1.8, ...],
    "erleAecAttX"   : [   0.08,    0.07,    0.06,    0.05,   0.035,   0.03, ...],
    "dtdSmoothUp"   : [   0.45,    0.45,    0.45,    0.45,    0.45,   0.45, ...],
    "dtdSmoothDown" : [   0.49,    0.49,    0.49,    0.49,    0.49,   0.48, ...],
    "dtdDecisionTh" : [   0.45,    0.45,    0.45,    0.45,    0.45,   0.33, ...],
    ...
  },
  "lineout": { ... same shape, separate tuning ... },
  "default speaker mode": "internal"
}
```

Separate tables for the internal speaker and for lineout — which is exactly the
jack case EchoMuse handles in `internal/bindings/jack`.

`Karush-Kuhn-Tucker RES` attenuates only the lowest 3 of 16 bands by 0.001
during double talk (`attenuationPerBand`), and nothing at all when it is echo
only (`attenuationPerBandEchoOnly` all 1.0).

### False wake word prevention

A dedicated module, `"enable": true`, whose thresholds are indexed by playback
volume — and note it distinguishes single talk from double talk at each volume:

```json
"Double talk threshold volume 10" : 1.0,   "Single talk threshold volume 10" : 4.0,
"Double talk threshold volume 40" : 11.0,  "Single talk threshold volume 40" : 14.0,
"Double talk threshold volume 100": 20.0,  "Single talk threshold volume 100": 24.0
```

This is the module whose job is precisely the risk CLAUDE.md flags for the
timer ring: *"a chime whose residual scores as the wake word and silences its
own alarm"*. Amazon's answer is a volume-indexed variance/hold test on the
post-AEC signal, not a threshold change.

### Filters, verbatim

Both are given normalised (divided through by a0) and are directly usable.

`HPF 80Hz @ 16K` — three biquads, applied to **both the mic input and the
reference input**:

```
a1 = [-1.978964742877471, -1.920823752121581, -1.995031240858690]
a2 = [ 0.980148787818626,  0.923195725460049,  0.995935784603137]
b0 = [ 2.061764283274676, 12.481101519210988,  0.036465860122281]
b1 = [-4.12286229274,    -24.9615622297,       -0.07291290428   ]
b2 = [ 2.061764283274676, 12.481101519210988,  0.036465860122281]
```

`Downsampler IIR`, 48 kHz → 16 kHz — three biquads plus a first-order section
(the 4th column). The comment says *"taken from Knight
(lpfIIRFilterCoeffs16Kat48K.h)"*:

```
a1 = [-1.35381400585174560547, -1.16845381259918212891, -1.09131717681884765625, -0.74153685569763183594]
a2 = [ 0.67048937082290649414,  0.85192829370498657227,  0.96150147914886474609,  0.0]
b0 = [ 0.03503995016217231750,  0.48894411325454711914,  0.74522399902343750000,  0.50676357746124267578]
b1 = [ 0.01067659538239240646, -0.29441374540328979492, -0.62026363611221313477,  0.50676357746124267578]
b2 = [ 0.03503995016217231750,  0.48894411325454711914,  0.74522399902343750000,  0.0]
```

### Gain staging

```json
"Voice Activity Detector": {
    "System Gain Available" : true,
    "PGA Gain"   : 20.0,   "ADC Gain"   : 0.0,
    "AFE In Gain": 0.0,    "AFE Out Gain": 7.2,
    "referenceAudioLevelOutputInDb" : -53
}
```

PGA 20 dB matches `audio_init.sh`, which sets `ADC_x MICPGA Volume Ctrl` to 40
in 0.5 dB steps. Then +7.2 dB after the AFE. EchoMuse's `micGainDb` default of
+24 dB sits in a different place in the chain (pre-truncation, on the 24-bit
sample) and is not directly comparable.

## The reference, and why it has a whole subsystem

Amazon never assumes the far-end reference is aligned. From the binary:

```
Loopback signal is zero. Cannot synchronize yet
Synchronized. Slice %d. Removing %d samples from the buffer
Out of sync, frames do not match
Reference buffer is too big. Trimming to %d frames
Failed to read echo reference. Sending silence.
asp_set_ext_ref_sync_status          ← the host tells ASP whether the ref is synced
sync_timeout / repeat_sync_timeout / sync_time_in_us / MIN_DELAY_DELTA_NS
EchoReferenceBufferMatching / Reference Delay / AEC upload samples delay
```

The hardware is why. Measured live on a fielded device:

- mic `pcm24c`: 9 ch, S24_3LE, **16 kHz**, period 512 — 4× TLV320AIC3101 over SPI
- spk `pcm23p`: 2 ch, S16_LE, **48 kHz**, period 2048 — TLV320AIC3204 over I2S

Two codecs, two rates, two buses. Relevant mixer controls and PCMs:

| Control / PCM | State | Note |
|---|---|---|
| `Audio_I2S0dl1_get_timestamp` | BYTE[8], increments between reads | hardware playback timestamp |
| `Audio_ExtCodec_EchoRef_Switch` | Off | external-codec echo reference route |
| `Codec_Loopback_Setting` | OFF | |
| `AP_Loopback_Select` | AP_LOOPBACK_NONE | |
| `00-09 DL1_AWB_Record` | capture | MTK audio-write-back of the playback path |
| `00-16 I2S0AWB_Capture` | capture | loopback of I2S0 out |
| `LineIn ADC` | Off | `persist.adc.linein` unset |

Adaptation is also gated on content state rather than run blind:
`Enable AEC/ABF according to ref level`, `Automatic AEC/ABF Selection according
to reference power`, `Alarm : set AEC adaptation %d`, `Enable For TTS`, plus
per-band ERLE (`ERLEComputeLowBand`/`HighBand`) and a divergence counter
(`AECD: AIC IS DIVERGING`).

## Can EchoMuse use any of this?

Four routes, in decreasing order of how well they work out.

### 1. Reuse the tuning data — **yes, and this is where the value is**

`AFE.cfg` and the coefficient files are data, they live on every rooted Dot,
and they were designed against this exact array and enclosure. Reading them
costs nothing and commits to nothing.

Immediately usable with no new DSP:

- **The `Downsampler IIR` coefficients.** EchoMuse currently 3:1 **box
  decimates** the 48 kHz reference to 16 kHz (`device/internal/aec/aec.go`). A
  box filter is a poor anti-alias filter; everything above 8 kHz folds back into
  the reference band. Aliasing in the reference is uncorrelated with the mic
  signal by construction, so it directly caps achievable ERLE. Swapping in
  Amazon's 4-section IIR is ~20 constants and a biquad cascade.
- **The `HPF 80Hz @ 16K` coefficients.** EchoMuse has no high-pass anywhere on
  either path (grepped: no `highpass`/`hpf`/`dcblock` in `device/` or
  `controller/`). Amazon high-passes **both** mic and reference before the AEC.
  Low-frequency rumble inflates the reference power estimate and so shrinks the
  effective step size at the frequencies that carry speech.
- **`TailLen` 2560 = 160 ms** against EchoMuse's 300 ms default. Worth an A/B;
  the shorter filter converges faster and misadjusts less.
- The volume-indexed **DTD and RES tables** as a starting point if a residual
  suppressor is ever built.

The FBF coefficients are usable in principle but not for free — see the
caveats below.

### 2. Adopt the design decisions — **yes, and cheaper than porting**

Two structural gaps stand out, both independent of the beamformer:

- **A residual echo suppressor after the linear AEC.** Amazon ships four kinds
  and none of the paths runs without one. This corroborates rather than
  contradicts the existing "AEC ceiling is hardware" finding: Amazon hit the
  same linear ceiling and answered it with a nonlinear spectral post-filter,
  not with a better adaptive filter. A frequency-masking RES on the existing
  speex output is additive and does not touch the AEC.
- **A double-talk detector whose thresholds depend on playback volume.** Every
  volume-sensitive parameter in Amazon's config is a 10-entry table, never a
  constant.

Both would also give the timer-ring barge-in path (`Ring listening (chime
audible, AEC)`) something better than a fixed `bargeInThreshold`.

### 3. dlopen `libasp.so` and drive it directly — **possible, but pointless given route 5**

The precedent exists: `internal/wakeword/ort/` already dlopens ONNX Runtime
rather than linking it. Every `NEEDED` library is present on the device
(verified: `libaudioutils`, `libbinder`, `libc++`, `libcutils`, `liblog`,
`libmedia`, `libspeexresampler`, `libutils`), and the config sits at a fixed
read-only path.

A disassembly pass says the ABI work is **moderate, not forbidding**. The
exported functions are tiny — 6 to 180 bytes — because they are a thin C shim
over a C++ object with a vtable:

```
asp_init                6 B    movs r0,#0 ; b asp_parameterized_init   -> init(0)
asp_parameterized_init 84 B    takes a lock, calls internal 0x1d39c, sets a
                               "already initialised" byte, returns int
asp_create_pipeline   180 B    rejects type >= 0x13 (19 pipeline types),
                               forwards (r0..r3) to internal 0x1da60,
                               returns 0 on failure -> returns a handle
asp_process            58 B    ldr r6,[r0] / ldr r6,[r6,#0x18] / blx r6
                               = vtable slot 6.  8 incoming args (r0-r3 +
                               4 stack), forwards them plus a trailing 0
asp_process_ext        58 B    identical, but passes a real 9th arg
asp_process_3p          6 B    mvn r0,#0x25 ; bx lr  -- STUBBED, always -38
asp_set_device         44 B    vtable slot 3
asp_set_ext_ref_sync_status 16 B  vtable slot 8
```

So the handle from `asp_create_pipeline` is a C++ object, `asp_process` takes
roughly `(ctx, mic**, nMic, ref**, nRef, out**, nOut, nSamples)` — eight
arguments whose types are unknowable from the shim, since all the real work is
behind `vtable[6]`. Recovering them means reversing the internal
implementation, and every wrong guess corrupts memory inside the process that
owns the audio hardware. Note also `asp_process_3p` is a hard-coded `-38`
stub, so at least one advertised entry point does nothing.

Verdict: doable, days of work, ships nothing (Amazon proprietary — it could
only ever be dlopen'd from the device's own copy), and route 5 gets the same
DSP through a documented API for a fraction of the effort.

### 4. Talk to the `audiosignalprocessor` binder service — **no, and it is the wrong door**

The service is registered and `dumpsys audiosignalprocessor` responds. But the
tap that would let a client read processed audio is fused off in retail builds
(`ASP capture is disabled for ship build` / `ASP injection is disabled for ship
build`), and the real audio transport is not this service at all.

`libwakewordserver_jni.so` links `libaudiostream.so`, which implements
`amazon.speech.audio.IAudioStreamService` — a binder service handing out
**shared-memory ring buffers** (`amazon::ShmAudioStreamIOBase`,
`allocateSharedMemory`, `openReader`/`openWriter`, `available`,
`getPosition`/`setPosition`). That is how processed audio actually reaches the
wake word engine. It is **not registered on an EchoMuse device**: its host is
`amazon.speech.sim`, which EchoMuse's own debloat step puts in the `pm hide`
list. Un-hiding it to get the stream back would restore the Alexa stack this
project exists to remove.

Releasing the microphone would not change either fact.

### 5. Open the AFE through the standard Android capture API — **this is the real answer**

The AFE is not behind an Amazon service. **It is inside the audio HAL**:

```
$ readelf -d audio.primary.mt8163.so | grep NEEDED
   NEEDED  libasp.so
$ nm -D --undefined-only audio.primary.mt8163.so | grep asp_
   U asp_init   U asp_create_pipeline   U asp_process
   U asp_set_device   U asp_command   U asp_destroy_pipeline
```

The HAL runs ASP on both directions — `AudioALSAPlaybackHandlerNormal::asp_open`
/ `doProcessAsp` / `setASPDevice` on the way out, and
`AudioALSACaptureDataClient::getAspPipelineType` on the way in. That last
function is 44 bytes and decides everything. Disassembled, it is a switch on
`audio_source_t`:

| `input_source` | value | pipeline | `AFE.cfg` path |
|---|---|---|---|
| `AUDIO_SOURCE_VOICE_RECOGNITION` | 6 | **0** | **ASR** — per-mic AEC, FBF, ABF, beam selection, false-WW prevention |
| `AUDIO_SOURCE_HOTWORD` | 1999 | **0** | same |
| `AUDIO_SOURCE_VOICE_COMMUNICATION` | 7 | 1 | Voice/VoIP — AEC, RES, NR, CNG, AGC |
| `AUDIO_SOURCE_MIC` | 1 | 2 | `"Mic": { "Algorithms": {} }` — empty, passthrough |
| anything else | — | −1 | no ASP pipeline at all |

So **an ordinary `AudioRecord` opened with `VOICE_RECOGNITION` gets Amazon's
full ASR front end** — seven per-mic echo cancellers, the superdirective
beamformer, the adaptive beamformer, SNR beam selection and +7.2 dB output
gain — with no Amazon service running, no Alexa packages un-hidden, and no
reversing of `libasp`.

The device binary is Go and does not link the Android framework, but it does
not have to. `/system/lib/libOpenSLES.so` is present, and OpenSL ES on API 22
exposes recording with `SL_ANDROID_RECORDING_PRESET_VOICE_RECOGNITION`, which
maps to exactly this source. That is a documented, stable C API, dlopen'd the
same way `internal/wakeword/ort` already dlopens ONNX Runtime.

**The catch, and it is not small: the echo reference comes from the HAL's
playback path.** ASP takes its far-end reference where the HAL writes output.
EchoMuse currently runs `stop media` in `Init()` and then owns `pcm23p` and
`pcm24c` directly with tinyalsa for the life of the process — audio written
that way never passes through the HAL, so the AFE would have no reference for
it and would cancel nothing. Getting the AEC means routing **both** capture and
playback through AudioFlinger, which inverts a load-bearing part of the current
device design (see the jack section of CLAUDE.md for why `stop media` is there).

What it would cost, beyond that inversion:

- Raw 7-channel access is gone. One processed channel comes back, so
  `internal/beamformer`, `internal/aec`, `internal/processor` (AGC) and the
  controller's `em_ns` denoiser are all bypassed — as is the direction estimate
  the LED ring overlay uses.
- 16-bit through the framework instead of S24_3LE, so the `micGainDb`
  pre-truncation trick no longer applies; gain staging moves to the HAL's PGA
  (20 dB) plus the AFE's +7.2 dB output boost.
- AudioFlinger buffering is added on both paths, against a mic pipeline that
  already has a hard 160 ms deadline.
- The device stops being sovereign over its own audio hardware, which affects
  the mute guarantee — currently the device rejects `mic_start` while muted and
  mutes the ADC. That reasoning would need revisiting.

**Status: static analysis only, not yet confirmed on hardware.** The whole
chain above is read out of the HAL binary; nothing has been recorded through it
yet. The experiment that would settle it is to open an `AudioRecord` at
`VOICE_RECOGNITION` while playing a known signal through AudioFlinger, and
measure ERLE against the same capture at `AUDIO_SOURCE_MIC` (pipeline 2, which
the config says is empty and should therefore show none). That needs a small
ARM binary built in the `echomuse-compiler` image; it does not need EchoMuse
stopped for the analysis, only for the measurement.

### Caveats on the beamformer coefficients

Do not treat the FBF coefficients as drop-in. Using them requires the matching
64-band WOLA analysis **and** synthesis filterbank (128-pt FFT, hop 64,
640-tap prototype — the prototype is supplied), which does not exist in
EchoMuse today. Cost is roughly 10,752 complex MACs per 4 ms hop for the
beamformer plus the filterbank on 7 channels; plausible on this SoC with NEON
but a real addition to a mic pipeline already at 18–20% of a core, on a device
that may also be running on-device wake word at ~38%.

And a correctness caveat: **I could not pin the coefficient convention.** The
per-mic magnitude and phase structure is unambiguous (six beams, three axis
pairs, all 7 mics used, dominant centre tap), but attempts to recover each
beam's look direction against EchoMuse's empirically-confirmed geometry left a
constant ~64° offset, and the resulting white-noise-gain and directivity
figures came out mutually inconsistent (+8 dB WNG alongside 16 dB DI is not
physically possible for 7 elements). That means the mic-index mapping, the
delay sign, or the interpretation of the 4 taps is wrong — not that the design
is. **No DI or WNG number should be quoted from this document.** Settling it
needs either a measured impulse response per mic or a match against a known
reference, and it must be settled before the coefficients are trusted, because
the existing white-noise-gain objection to superdirective beamforming on
unmatched capsules across four ADCs is exactly what those numbers would
answer.

## Open questions

- The coefficient convention above.
- Whether `Audio_ExtCodec_EchoRef_Switch` or `LineIn ADC` routes a real
  electrical loopback into one of the two capture channels SETUP.md records as
  unconnected (ch7, ch8). A reference on the same converter and clock as the
  mics is a different problem from the one the current software tap has.
- Whether `00-16 I2S0AWB_Capture` is openable alongside EchoMuse's own playback
  and what its alignment to `pcm24c` actually is.
- `ARA` is configured identically to the AEC and runs after it. Its expansion
  is unknown; `SerialARA_BeamMerger`, `setAraReferenceBeam` and `ARA ON`/`OFF`
  suggest a second canceller stage operating in beam space.

## Also in the AFE, not AEC

- **Tap detection is acoustic**, in the AFE, using inter-mic level difference
  and coherence, and it reuses the AEC filters: `ILD Tap Detector`,
  `DNN Tap Detector`, `Tap Mic Indices`, `Tap Coherence Threshold`,
  `Tap HF Coherence Start/End Freq Hz`, `Tap AEC Filters`,
  `Double Tap Window In Seconds`, `Button/Screen Press Lockout In Seconds`.
  Relevant to #115: Amazon does not time taps on a wire at all.
- Ultrasound presence detection (`Device Side Polling Ultrasound On/Off
  Duration`, `Major`/`Minor Movement Threshold`).
- Low-power sound detector (`LPMSDAPI`), acoustic event detection
  (`libaed.so`), spatial audio / crosstalk canceller, multi-room (WHA cluster),
  automatic volume levelling driven by `/vendor/smartvolume/*.csv`.

## Prior art

`github.com/albertoZurini/echo-dot-2-playground` covers `libpryon.so` and
`libwakewordserver_jni.so` — wake word and speech interaction, with decompiled
Java and JNI probes. It does not cover `libasp.so`, the AFE, the AEC or the
tuning directory.
