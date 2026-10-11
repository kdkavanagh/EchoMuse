# Amazon's audio front end on the Echo Dot Gen 2

Reverse-engineering notes on how the stock Alexa stack does acoustic echo
cancellation and beamforming on the same hardware EchoMuse runs on, and an
assessment of what — if anything — EchoMuse can reuse.

Investigated 2026-08-12 against a fielded Dot (FireOS image dated 2022-11-18).
Everything up to [Fire OS 6: the mixer's ASR streams](#fire-os-6-the-mixers-asr-streams)
describes Fire OS 5. Fire OS 6 (`biscuit_puffin`) ships a different AFE
generation behind Amazon's `mixer` daemon; that section covers it. EchoMuse
has since dropped Fire OS 5 and runs only on Fire OS 6; the Fire OS 5
sections are kept as the investigation record.

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
`amazon.speech.sim`, which EchoMuse's Fire OS 5 debloat step put in the `pm hide`
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

## Fire OS 6: the mixer's ASR streams

Investigated 2026-10-07 on G090LF0965260F1J (Fire OS 6.5.6.9, `NS6569/6009`)
with EchoMuse stopped. Every measurement below comes from one small C capture
program run on the Dot and from host-side analysis of what it recorded. Both
are specified in [How the measurements were made](#how-the-measurements-were-made),
closely enough to rebuild them. The program, the raw captures and the analysis
scripts were not kept. Static-analysis claims name the library, symbol and
offset in this build, so they can be re-derived from the libraries on any
`NS6569/6009` unit. The libmixerAPI prototypes are those in
[fireos6-port.md](fireos6-port.md) §2.1, plus the two pinned below.

**Short answer: `micMultiChAsr` does not mean Alexa beamforms.** All spatial
processing (8 fixed beams, a beam-space canceller and a beam-group merger) is
in `libasp` inside the `mixer`. `micMultiChAsr` carries the same single beam
as `micAsr`, and optionally up to 4 post-AEC microphones and the far-end
reference beside it. Alexa asks for the beam plus one post-AEC microphone. The
wake engine reads only the beam, as a mono `beamformed` channel. The
microphone channel goes to a cloud upload ("OneMic"). Self-wake suppression
scores playback audio, not this stream; a separate `loopback` record stream
carries the playback mix [INFERENCE: that it feeds the suppression decoders].
No interface exposes the individual beams or locks a beam.

The hardware is 4 × Cortex-A53 (`/proc/cpuinfo` CPU part `0xd03`, 1.3 GHz),
running 32-bit. It is not a dual-core Cortex-A7.

### How the measurements were made

**Device state.** The adb shell of boot-root runs as uid 0 in `u:r:su:s0`
([fireos6-port.md](fireos6-port.md) §2). Before each run: `stop echomuse` and
`stop puffin`, because PuffinApp takes the mixer's single ASR mode back
(fireos6-port.md §2.1), and `logcat -b all -c`. After it the run's logcat was
read with `logcat -d -b all`, filtered on the `mixer` daemon's pid.

**Capture program.** One C file, compiled in the firmware's compiler image
with `armv7a-linux-androideabi22-clang -O2 -Wall -o capture capture.c -ldl -lm`
(NDK 21.4.7075529). It was pushed to `/data/local/tmp`, run from there and
deleted afterwards. Arguments: output name, duration, stream type, channel
count, an optional setup `mics:asr:ref` (for example `0,1,2,3:1:1`), an optional
stimulus file and a playback delay. It does this, in order:

1. `setgroups(2, {2901, 1005})` and `setresgid(2901, 2901, 2901)`, so the
   process runs with egid `aipc` and group `audio`, uid still 0. The mixer
   cannot read a stream descriptor created under any other group, and the
   stream then never carries data (fireos6-port.md §2.1).
2. `dlopen("libmixerAPI.so")` and `dlsym` of `MixerOpenRecCh`,
   `MixerGetBufRecTimed`, `MixerReleaseBufRec`, `MixerOpenPlay`,
   `MixerGetBufPlay`, `MixerReleaseBufPlay`, `MixerClose`, `MixerGetRate`,
   `MixerGetNumCh`, `MixerGetSampleSizeBits`, `MixerGetBufSize`,
   `MixerGetStreamName` and `MixerMultiChannelAsrSetup`.
3. With a setup, `MixerMultiChannelAsrSetup(mics, n, asr, ref)`, printing the
   return value. Then `MixerOpenRecCh(type, nch)`, printing rate, channels,
   bits, buffer size and stream name.
4. With a stimulus, a playback thread. It sleeps for the delay, opens
   `MixerOpenPlay(48000, 1, 16, 0)` (a `MUSIC` stream), then loops:
   `MixerGetBufPlay` (4,800-byte chunks), copy the next chunk of the file
   (zero-padded at the end), `MixerReleaseBufPlay(h, size)`. It logs each
   chunk's index, byte offset and the `CLOCK_MONOTONIC` time taken after the
   release returned. This document calls that time the chunk's *handoff*. At
   end of file it waits 1 s and calls `MixerClose`.
5. Until the duration has elapsed, or after 20 failed reads:
   `MixerGetBufRecTimed(h, &status, &size, &ts)`, then at once
   `clock_gettime(CLOCK_MONOTONIC)` and `clock_gettime(CLOCK_BOOTTIME)`. A
   status-0 block is appended to the raw file (interleaved S16LE). Every read
   gets one log line: block number, status, size, `ts`, both clocks and the
   cumulative byte count. `MixerReleaseBufRec(h)` follows every read.
6. Rate, channels and bits are printed again, then `MixerClose`.

**Stimuli.** Both are 48 kHz mono S16LE, written as
`round(clip(x, −1, 1) × 32767)`, with noise from numpy's `default_rng(seed)`.
For analysis they were resampled to 16 kHz with
`scipy.signal.resample_poly(x, 1, 3)`.

| Stimulus | Content, in order |
|---|---|
| A, 11.02 s | 1 s silence; a 20 ms 1 kHz sine at amplitude 0.5 (the *click*, 1.00–1.02 s); 0.98 s silence; 3 s white Gaussian noise scaled to RMS 0.1, −20 dBFS (seed 1234; 2–5 s); 1 s silence; a 3 s exponential sweep 100 → 7,500 Hz at amplitude 0.3, phase 2π·100·(e^(kt) − 1)/k with k = ln(75)/3, 20 ms linear fades (6–9 s); 1 s silence; the click (10.00–10.02 s); 1 s silence |
| B, 22 s | 1 s silence; 20 s white Gaussian noise at −35 dBFS RMS (seed 7); 1 s silence |

**Captures.** Uptime is `CLOCK_BOOTTIME` at the first block. Between boots 1
and 2 the Dot was rebooted with `adb reboot`, to watch Alexa start (below).

| Capture | Stream (`nch`) | Setup | Playback (start in capture) | Length | Uptime | Notes |
|---|---|---|---|---|---|---|
| `p9` | `micMultiChAsr` (9) | none | A at 1.0 s | 14 s | 4,092 s, boot 1 | unconfigured layout |
| `q9` | `micMultiChAsr` (9) | none | none | 10 s | 4,107 s | idle |
| `pa` | **`micAsr`** (1), EchoMuse's stream | — | A at 1.0 s | 14 s | 4,118 s | |
| `s9` | `micMultiChAsr` (9) | `0,1,2,3,4,5,6:1:1`, rejected | none | 3 s | 4,174 s | setup limits |
| `s6` | `micMultiChAsr` (6) | `0,1,2,3:1:1` | A at 1.0 s | 14 s | 4,194 s | |
| `n1`, `n2`, `n3` | `micMultiChAsr` (6) | `0,1,2,3:1:1` | B at 1.0 s | 24 s each | 4,544 / 4,569 / 4,595 s | three consecutive streams |
| channel-map set, 14 runs | `micMultiChAsr` | see [Channel layout](#channel-layout-identified-empirically) | B at 0.5 s | 4 s each | ≈4,650–4,760 s (wall clock; no block logs kept) | |
| `lb` | `micMultiChAsr` (3) | `0:1:1` | A at 8.0 s | 30 s | 4,913 s | tonal room sound at 21.7–25.8 s; `beam_index` events logged |
| `lm` | `micMultiChAsr` (3) | `0:1:1` | A at 1.5 s | 15 s | 231 s, **boot 2** | `lp` recorded in parallel |
| `lp` | `loopback` (1) | — | none of its own | 16 s | 231 s, boot 2 | the playback mix |
| `rw` | `micRaw` (asked 9, got 1) | — | A at 1.0 s | 14 s | 272 s, boot 2 | |

Every capture opened a new record stream and, when it played, a new play
stream.

**Analysis.** Host-side, numpy and scipy, with samples scaled by 1/32768.
Levels are 20·log10(RMS) in dBFS. Where the metadata bit could bias a
per-frame level, bit 0 was cleared first.

- *Channel roles.* A channel is the **reference** when at least 99 % of its
  samples in the first 0.8 s, before playback, are exactly zero, and it is
  above −80 dBFS during playback. Two reference channels had 98 % zeros and
  were placed by level instead: about −107 dBFS before playback and −36.8 dBFS
  under stimulus B. A channel **carries the metadata** when at least 90 % of its
  128-sample frames begin with the 16 bits `0xA563` in bit 0. The rest are
  **microphones**. Beam and post-AEC microphone are then told apart by position
  and level: with `ASR_out` true, channel 0 idled at −73.5 to −77.8 dBFS and the
  microphone channels at −65 to −70 dBFS.
- *Stimulus-relative windows.* The first click is the maximum of a 160-sample
  moving mean of x² within 0.5–4 s, and the stimulus starts 1.0 s before it.
  Floor = [start − 0.9 s, start − 0.1 s], noise = [start + 2.2 s, start + 4.8 s],
  sweep = [start + 6.2 s, start + 8.8 s].
- *GCC-PHAT* from the reference channel to each other channel: cross-spectrum
  over a zero-padded power-of-two FFT, zeroed outside 200–7,000 Hz, normalised
  to unit magnitude, inverse FFT, then the peak of |cc| within ±3,200 samples.
  A positive lag means the channel follows the reference. It was run over whole
  segments (`s6` sweep 6.9–9.8 s, noise 2.9–5.8 s) and over sliding 0.5 s
  windows, skipping windows whose reference RMS is below 10⁻⁴.
- *Reference against handoff.* Windows of 4,096 samples of the 16 kHz stimulus,
  every 1 s from 1.2 s and in a second pass every 0.25 s, were located in the
  reference channel by FFT cross-correlation. A located sample index becomes
  capture time by linear interpolation of each block's `ts` against the
  cumulative frame count, taking `ts` as the end of its block. Subtracting the
  handoff of the 4,800-byte play chunk that held the window's first stimulus
  sample gives the latency. In the 0.25 s pass, windows with normalised
  correlation ≤0.08 or a result outside 600–1,000 ms were dropped: one per
  capture.
- *FIR fit.* For 2,048-sample windows, a least-squares 32-tap FIR from the
  16 kHz stimulus, centred on the located offset, to the reference channel. The
  residual is reported relative to the window's energy.
- *Metadata frames:* see [AFE metadata in bit 0](#afe-metadata-in-bit-0-v33).
- *`mixer` CPU.* `utime + stime` (fields 14 and 15 of `/proc/<mixer pid>/stat`,
  100 Hz ticks) read 4 s after a 26 s capture opened and again 20 s later, with
  no playback. 2,000 ticks is one core. With no stream open the mixer used 0.

**Static analysis.** The libraries were pulled with `adb pull` from
`/system/lib/` and PuffinApp from `/system/bin/PuffinApp` (the `puffin` init
service). They were disassembled with Capstone (Thumb-2 and ARM) and pyelftools,
resolving PLT calls to imported names and PC-relative literal loads to strings,
and read with `nm -C`, `strings` and `readelf`. Addresses are file offsets in
these builds. `nm` prints a Thumb function's address with the low bit set, for
example `0000ec7d T MixerMultiChannelAsrSetup` for 0xec7c.

### Opening it

Two calls, pinned from the libmixerAPI disassembly and confirmed by behaviour:

```c
int   MixerMultiChannelAsrSetup(const int *mic_channels, int n, bool asr_out, int ref_out); /* returned 0 on every call made */
void *MixerOpenRecCh(const char *type, unsigned nch);   /* MixerOpenRec(t) == MixerOpenRecCh(t, 1) */
```

`MixerMultiChannelAsrSetup` (libmixerAPI 0xec7c) calls
`mixer::UtilMultiChAsrSetup(int*, int, bool, int)` (libmixerAPI 0x12548) and
returns −22 only when that call reports failure. It returned 0 on every call
made here, including the setups the HAL rejected. `UtilMultiChAsrSetup` writes
one line to `/data/mixer_streams//MultiChSetup`, built from the literals
`mic_channels`, `=`, `[`, `,`, `];`, `ASR_out`, `true`/`false` and `ref_out`.
That file was never present when listed after a setup (`cat` found nothing), so
its contents are known only from the disassembly. The mixer forwards the line
to the HAL as parameters, and the HAL turns them into ASP command 46. Logcat
for `s6`:

```
Mixer_HalIO:SetPrimaryHalParameters:Setting Parameters mic_channels=[0,1,2,3];ASR_out=true;ref_out=1:
ASP/Pipeline: Pipeline() mPipelineId = 9            (micAsr opens pipeline 0)
ASP/Pipeline: out_buf: format: 1 sample_depth: 2 channel_count: 6 sample_rate: 16000 ...
ASP/AspManagerBase: set AEC out config: num_mics 4 asr 1 ref 1
AUDIOALG: TimeDelay Config: m_numChannels = 4, m_nSamplesToDelay = 168, m_frameLen = 128
```

With seven offsets (`s9`) the HAL logged `setParameters(), still have
param.size() = 3` and the mixer logged
`SetHalParams:params mic_channels=[0,1,2,3,4,5,6];ASR_out=true;ref_out=1 HAL Error -22`.
The mixer logs the same `HAL Error -22` for the `multi_ch_asr_disable=1` it
sends when `micAsr` opens, and that one does take effect
([fireos6-port.md](fireos6-port.md) §2.1), so the message alone does not mean
the parameters were ignored. The limits come from the HAL's own messages in the
`s9` and channel-map runs:

- `mic_channels` holds **1–4 offsets in [0, 3]**.
  `selectAECOutChannels: numMics specified is 7, must be in range [1,4].` and
  `Mic offset 4 (element 0 of array) not in range [0,3].` The offsets index the
  four microphones the Fire OS 6 AFE uses (below), not the seven on the board.
- `mic_channels=[]` is not "no microphones". It parsed as
  `num_mics 1 asr 0 ref 0`.
- **A rejected setup leaves the previous AEC-out config in force.** The next
  open got the previous layout. Set a valid config before every open.
- `ref_out` is a channel count. With `2`, both speaker channels came back,
  identical for mono content.
- Opening `micMultiChAsr` with no setup (`p9`, `q9`) gives pipeline 0. Channel 0
  is the beam and channel 1 is zero. Channels 2–8 hold a pattern that repeats
  every 128 samples (bit-0 autocorrelation 0.997 at lag 128), which looks like
  stale buffers [INFERENCE].

### Stream format

| | `micAsr` | `micMultiChAsr` |
|---|---|---|
| HAL open | `input_source=6`, `AUDIO_INPUT_FLAG_PREFIX_TS`, ASP pipeline 0 | the same, pipeline 9 once configured |
| Samples | 16 kHz S16LE mono | 16 kHz S16LE, interleaved, `nch` channels |
| Read block | 256 frames (16 ms), 512 B | 256 frames, 512 B × `nch` (1536/3072/4608 B for 3/6/9 ch) |
| `ts` step (block logs) | 16.00 ms mean (12.5–21.0) | 16.00 ms mean (12.4–21.0) |
| read completion − `ts` (block logs) | 77.3 ms mean (71.9–79.4) | 76.6–77.1 ms mean (71.7–93.1) |
| `GetRate`/`GetNumCh`/`GetSampleSizeBits` | `0xFFFFFFFF` right after open, 16000/1/16 by close | the same race (1 of 14 opens), correct by close |
| `ASR path latency` (ASP cmd 151, logcat at open) | 50.19 ms | 50.19 ms |
| `mixer` CPU, utime+stime over 20 s, no playback | 1356 ticks, **67.8 %** of one core | 1373 (3 ch, `0:1:1`) / 1365 (6 ch, `0,1,2,3:1:1`), **68.7 / 68.3 %** |

So switching streams costs the `mixer` nothing measurable. The AFE runs either
way; the AEC-out channels are copies it already has. The client's extra cost is
reading 32 kB/s per added channel. That cost was not measured.

### Channel layout, identified empirically

Layout: **[beam if `ASR_out`] + [post-AEC mic for each `mic_channels` offset] +
[reference × `ref_out`]**. The AFE LSB metadata (next subsection) is always in
bit 0 of channel 0, whatever channel 0 is. The rows below come from `s6` and
the 14 channel-map runs, classified as in
[How the measurements were made](#how-the-measurements-were-made). The m*k*
labels follow the order of the requested `mic_channels` list. Nothing else
told the post-AEC channels apart.

| Setup (`mic_channels`; `ASR_out`; `ref_out`), `nch` | Observed channels |
|---|---|
| `[0,1,2,3]; true; 1`, 6 | beam, m0, m1, m2, m3, ref |
| `[3]; true; 1`, 3 | beam, m3, ref |
| `[2]; true; 2`, 4 | beam, m2, ref, ref |
| `[0,1]; true; 0`, 3 | beam, m0, m1 |
| `[0]; true; 0`, 2 | beam, m0 — **Alexa's configuration** |
| `[0,1,2,3]; false; 1`, 5 | m0 (carrying the metadata), m1, m2, m3, ref |
| `[0]; false; 1`, 2 | m0 (carrying the metadata), ref |

- **The beam is the same signal as `micAsr`.** Stimulus A was recorded through
  each (`pa`, then `s6` 76 s later). Floor −75.7 vs −75.2 dBFS, echo residual under noise
  −69.1 vs −68.6, and under the sweep −55.6 vs −55.8. The beam is in the same
  ASP path, has the same 50.19 ms latency and carries the same v3.3 metadata.
  Recording both at once is impossible because the mixer serves one ASR mode,
  so "same processing" rests on this match [INFERENCE, strong].
- **The m*k* channels are post-AEC, not raw.** `AFE.cfg` names them
  `MicsPostAECOutputGain` (+4 dB) and `selectAecOutputDelay` (168 samples).
  During −20 dBFS noise playback (`s6`) they sat at −60.6 to −63.1 dBFS, against a
  −66 to −69 floor. That is the same few dB of residual as the beam, not a full
  echo. No raw channel was available to measure them against. The AFE's own
  `ERLE_RAW` read 4–26 during playback.
- **The reference** is digital zero while nothing plays, and −22 dBFS for the
  −20 dBFS noise. It is the 16 kHz speaker feed after the mixer's playback
  chain. In `s6` the sweep fits a 32-tap FIR of the stimulus to −30 to −58 dB
  residual (2,048-sample windows every 0.5 s; −20 dB in the window straddling
  its onset). The −20 dBFS white noise does not fit (−0.4 dB). No 2,048-sample
  window of the reference during the noise matches the played noise anywhere
  in the stimulus: the best normalised correlation is 0.41–0.44, at an
  unrelated position in the sweep. That is consistent with the `MBCL` limiter
  in `AFE.cfg`'s `Playback` path [INFERENCE].
- **There are no raw microphones anywhere in libmixerAPI.** Asking for `micRaw`
  with 9 channels (`rw`) opened `t micRaw r 16000 c 1` with `input_source=6`, ASP
  pipeline 0 and 1 output channel. The audio had beam-like residuals and v3.3
  metadata, so on this build `micRaw` is the beam too. The 9-channel `pcm24c`
  is held by the mixer, and `post-afe-audio-architecture.md` §4.1 rules raw ALSA
  out anyway. Mic geometry therefore cannot be inferred. Against the centred
  speaker, the four post-AEC channels' echo arrival differs by at most
  3 samples.

### The reference is sample-aligned to the microphones

This is the one property `micAsr` cannot give. GCC-PHAT (see
[How the measurements were made](#how-the-measurements-were-made)) was run from
the ref channel to every other channel, in 0.5 s windows over the first 2–4 s
of playback, before the AEC converges, while the echo is still visible. For
`s6` it was also run over the whole sweep (6.9–9.8 s):

| Capture | post-AEC mics | beam |
|---|---|---|
| `s6` (sweep + noise) | +836 to +839 samples | +829 |
| `n1` (−35 dBFS noise) | +836, +837 | +829 |
| `n3` (same, new stream) | +834 to +837 | +828 |
| `lm` (after a reboot) | +837 to +839 | +829 to +831 |

The echo reaches the beam **51.8 ms** after the same samples appear on the
reference channel, and the post-AEC mics 52.3 ms after. That is the 50.19 ms
ASR path latency plus about 2 ms of acoustics and converters. So the reference
channel is the AFE's input-aligned reference, and the output channels carry the
pipeline latency on top of it [INFERENCE from the match]. ASP synchronises its
reference against the I2S loopback once per stream (`ASP/ER: Synchronized.
Slice N. Removing M samples`, M = 726, 227, 456 and 5 at 48 kHz in four
streams).

After 2–4 s of playback, the coherent part of what remains in every
output channel moved to +573 to +584 samples, in all four captures. The
reference did not move. Located against the handoff log (see
[How the measurements were made](#how-the-measurements-were-made)), its content
landed 785.0 ms after the `MixerReleaseBufPlay` handoff of the same chunk before
the change (windows in the first 4 s of stimulus B) and 786.8 ms after it
(windows from 5 s on). For `n1` the sd was 6.0 and 6.3 ms over 11 and 63
windows; `n3` gave 785.7 and 787.0 ms. So the residual
changed composition, not timing. Individual chunks ranged 773–791 ms after
handoff with the full 16-block queue, the handoff jitter the first release's
780 ms re-anchor in `mixerapi/playclock.go` estimated around. The Player now
keeps the queue to 3 blocks and stamps from its depth (fireos6-port.md §6).

### AFE metadata in bit 0 (v3.3)

Bit 0 of channel 0 carries one 128-bit frame per 128-sample AFE batch (8 ms),
phase-locked to the stream start. It is present in `micAsr`, so **EchoMuse's
current Fire OS 6 capture already carries it**. The layout comes from
`libAFEMetadataDecoder.so` (`decodeMetadataVersion3_0` @0x8618,
`decodeMetadataVersion3_3` @0x8964) and the field names from `libpryon`'s
`AfeMetadataType` table (getter 0x6c224c, table 0x6c2b30). Every bit was
checked against the device: 11,984 of 11,984 frames in five captures (`lb`
3,722, `pa` 1,722, `s6` 1,722, `n1` 2,972, `lm` 1,846; both stream types, two
boots) passed the checksum, which is the popcount of bits 127…8. The mixer logs
`AudioMetadataDecoder: ... setting decoder to version
3.3`. Amazon's decoder (`processAudio` @0x7654) writes `sample & ~1` back as
it reads, stripping the bit [INFERENCE: before Pryon scores the audio].

**How the frames were found.** Bit 0 of channel 0 is shifted, sample by
sample, into a 128-bit register, the first-received bit becoming bit 127. A
frame ends wherever bits 127:120 are `0xA5` and bits 7:0 equal the popcount of
bits 127:8 modulo 256. The layout was first located without the library:

- **Phase.** Over all 128 phases, the frame start is the phase at which the
  most bit positions hold the same value in every frame. It was phase 0 in
  every capture: frames start at multiples of 128 samples from the first
  sample read.
- **Counter.** Searching every field of 4–16 bits for one that steps +1
  (mod 2ⁿ) in at least 98 % of frames found the 6-bit counter.
- **Header.** The first two bytes, read MSB-first, are `A5 63`.

Field boundaries and names then came from the disassembly above. Two excerpts
of this bit stream are in the repository as the decoder's test fixtures. Both
are packed MSB-first, with golden per-period records beside them:

- `device/internal/audio/afe/testdata/rw.bits`: frames 160–1,119 of `rw`.
- `lm.bits`: frames 900–939 of `lm`, where `OUTPUT_CLIPPED` fired.

| Bits (MSB first) | Field | Observed (`lb`, `pa`, `s6`, `n1`, `lm`) |
|---|---|---|
| 127:120 | sync `0xA5` | constant |
| 119:112 | version (3 bits major, 5 minor) | `0x63` = 3.3 |
| 111:105 | `PAYLOAD_SIZE` | 104 |
| 104:99 | `FRAME_COUNTER` | +1 per frame |
| 98:83 | `AFE_TIMESTAMP` (ms) | device `CLOCK_MONOTONIC` ms mod 65,536, about 5 ms before read completion. 8.000 ms per frame over the long run, but frames are processed in pairs (steps of ~5 and ~11 ms, within ±8 ms of the 8 ms grid), so it marks processing time, not sample time; the decoder derives `FRAME_DROPS` from it |
| 82:75 | `ERLE_RAW` | 0 idle, 4–26 during playback |
| 74 / 73:66 | `COMPUTE_RMS` / `RMS` (value − 256 dB) | 1 / −86 to −38 |
| 65, 64 | `LPM_FLAG`, `LPM_V2_FLAG` | 0 |
| 63, 62, 61 | `AEC_DIVERGED`, `MIC_CLIPPED`, `OUTPUT_CLIPPED` | 0, 0, occasionally 1 |
| 60 | `PLAYBACK_ACTIVE` | 1 while the play stream ran |
| 59:53 | `VOLUME` | 70 (`persist.mixer.init.main.volume`) |
| 52 | `DEVICE_MUTE` | 0 (mute not exercised) |
| 51:44 | `ARA_VSS` (/256) | ≈0.99 idle, 0 with sound |
| 43:39 | `DTD` (/31) | 0 when idle and through converged playback; 0–0.065 at a playback onset (`pa`); 0–0.68 in `lm`, at its first click, which the AEC had not yet cancelled; 0 throughout `lb`, `s6` and `n1` |
| 38:34, 33:29 | `UED_GR`, `UED` (/32) | 0.156–0.312, 0–0.84 |
| 28:25 | `ULTRA_PROX_DISTANCE`, `_CONF` | 0 |
| 24:20 | `SSL_ANGLE` (×45°), `SSL_PROB` (×0.25) | **constant 0 / 0.5** |
| 19:15 | `FD_ANGLE` (×45°), `FD_PROB` (×0.25) | **constant 0 / 0.5** |
| 14:13 | `DNN_VAD_PROB` (×0.25) | 0–0.75 |
| 12:8 | `SIGNAL_TO_ECHO_RATIO` | 0 |
| 7:0 | checksum | valid in every frame |

There is no beam-index field. The two direction fields stayed constant in all
five captures, including through `lb`'s room sound, which drove `DNN_VAD_PROB`
to 0.75. Direction exists only as the lipc event `beam_index` (origin
`com.doppler.lasp`). The mixer logs it as
`I lipc:evts:name=beam_index, origin=com.doppler.lasp, fparam=<value>:Event sent`.
Read with `logcat -d -v monotonic -b all | grep beam_index`, it fired 54 times
during `lb` and 3 times at a 9-channel open. It is emitted on change, with
back-to-back changes 0.486–0.492 s apart and gaps of up to 5.4 s. Values 0, 1,
3, 5, 6, 7, 9 and 11 were observed. Its consumer is
`ledcontroller` (`AnimationFrame::onBeamIndex`, `BeamDirection`), so it drives
the ring's direction animation [INFERENCE: one value per LED position].

### What Alexa does with it

Observed live on boot 2. After `adb reboot`, the host polled
`getprop init.svc.echomuse` every 0.3 s and ran `stop echomuse` as soon as it
read `running`, at 13.4 s of uptime. That is inside the 5 s `start_server.sh`
waits after `init.svc.mixer=running` before it stops its denylist, so every
stock service kept running. While they ran, the Wi-Fi configuration
(`wpa_supplicant.conf`) was rewritten; it was restored from a copy afterwards.
At 40 s `/data/mixer_streams/` held PuffinApp's two stream descriptors,
`micMultiChAsr` and `loopback`. For the log below, puffin was restarted with
`logcat -b all -c; stop puffin; sleep 2; start puffin; sleep 30`, and the
mixer's lines were read with `logcat -d -b all | grep " <mixer pid> "`. The unit is
not registered with Amazon (`HeadlessSetupManager:Device is not registered,
starting registration`), but `PuffinApp` still announced `wakewordChanged
["ALEXA"]` and opened both streams:

```
Mixer_HalIO:SetPrimaryHalParameters:Setting Parameters mic_channels=[0];ASR_out=true;ref_out=0:
Mixer_Connect:AddNewStream:Add Record /mstream1951_78601172234 t micMultiChAsr r 16000 c 2:
ASP/AspManagerBase: set AEC out config: num_mics 1 asr 1 ref 0
Mixer_Record:AddLoopback:Loopback Start :
AudioStreamRecord: initialize(): AUDIOINFO record stream successfully created, source: loopback
```

`persist.puffin.ONE_MIC_ENABLED` is `1`. `persist.puffin.MCASRMode` is unset.
Static analysis of `PuffinApp`'s `MixerRecordWrapper::initialize` shows three
paths. The function starts around 0x143c5c and selects the path at 0x143d7a,
on a flag at `this+0xbc`. The `MixerMultiChannelAsrSetup` calls are at 0x143ef6
(`[1,2]`) and 0x144606 (`[0]`). The OneMic branch is at 0x14436a and its
de-interleave at 0x147c0a–0x147c20. The PLT stubs are `MixerMultiChannelAsrSetup`
0x8772c and `MixerOpenRecCh` 0x87810.

| Path | Setup | Channels | Where they go |
|---|---|---|---|
| Multichannel ASR (`MCASRMode` = 1, `MC_ASR_MODE_160Kbps`, en-US only) | `[1,2]; true; 0` | beam, m1, m2 | channel 0 to the mono ASR stream; the whole buffer to a multichannel stream for cloud upload [INFERENCE: its reader is in `libAIP.so`, not pulled] |
| OneMic (this unit) | `[0]; true; 0` | beam, m0 | channel 0 to `audioStreamWriter` (wake engine and cloud ASR); channel 1 to `oneMicStreamWriter`, uploaded as a separate Opus `OneMicAudio` message |
| Otherwise | — | `micAsr` | the beam |

`ref_out` is 0 at every call site. On the consumer side:

- **The wake engine is single-channel.** `libWakeWordManager` builds every
  spotter decoder from `PryonDecoder_NewPryonMultichannelAudioFormat_Default`
  (libpryon 0x3d3cf0): {16000 Hz, 16 bit, 1 channel, type `beamformed`}. It
  asserts `only mono sources supported` (libWakeWordManager 0x32902). The ALEXA
  model's `nemort_scorer_config.json` (under `/system/local/models/keyword/`)
  maps its one input to `channel_type: beamformed`.
  `PryonDecoder_NewMultichannelAudioDecoder` is reached only through
  `libPryonDetector` (`createPryonDecoder` 0x35cc, call at 0x3606) for
  acoustic event detection (AED), also with the one-channel format. `libpryon`
  knows the channel types `beamformed`, `post-aec`, `pre-aec` and
  `playbackencoder` (names at 0x96e8c8), and can choose among a model's
  channel arrangements. No on-device model here declares more than one.
- **There is no beamforming on the consumer side.** `libpryon`,
  `libWakeWordManager` and `PuffinApp` contain no steering, DOA,
  delay-and-sum, MVDR or FBF code. Every "beam" in `libpryon` is the Viterbi
  search beam. Spatial processing exists only in `libasp`.
- **Self-wake suppression runs on playback, not on the reference channel.**
  WWM runs a second spotter decoder named `playback` and a `PMA` decoder.
  `PMA` is automatic content recognition, fingerprint plus watermark
  (`models/PMA/pryon.config`: `recognizer.client_type = "acr"`). The mic model
  also enables watermark detection. The `loopback` record stream is the playback
  mix: 16 kHz mono, 1024-byte (32 ms) reads, `ts = 0`. It was recorded as `lp`
  alongside `lm`. Five 2,048-sample windows of stimulus A (at 6.5–8.5 s) were
  located in `lp` at normalised correlation 1.00 (0.96–1.00 in `lm`'s reference
  channel). Timing `lp` by read completion, since its `ts` is 0, and the
  reference by `ts`, the loopback content arrives 23–37 ms before the same
  content's reference-channel `ts`. The stream admits one client at a time
  (`Loopback stream already exists`) [INFERENCE: it feeds the
  `playback`/`PMA` decoders].
- **The metadata is read, but not for beams.** `libpryon` decodes it for ERLE,
  NTT and wake-word timing. After a wake, `PuffinApp` sends
  `LASP_CMD_SET_WAKEWORD_METADATA` with `timestamp_before_ww_start/end` in AFE
  milliseconds. The code 0x72 comes from `libgenericaspclient`'s LASP command
  map (around 0xf236; first reference 0x102d6). ASP then computes wake-word
  ERLE/SNR, ESP spectral features and user localisation over that span
  (`libasp.so` strings `parseWakewordMetadata`,
  `Invalid ASP_CMD_SET_WAKEWORD_METADATA data/size`).
- **No beam lock exists.** No beam-select or beam-lock command appears in the
  client libraries, the mixer or the HAL. `GroupBeamMergerV2` holds a beam
  group for 12 frames (96 ms) on its own. `asp.cfg` gives the ASR pipeline one
  output channel, and AEC-out adds only post-AEC mics and references. No
  configuration outputs the beams.
- **Cloud audio is one channel unless MC-ASR is on.** The acsdk
  `OpusAudioEncoder` (`libacsdkOpusAudioEncoder.so` 0x23ec, 0x2b2c; created
  from PuffinApp at 0x1a9084) is 32 kbps hard-CBR, 20 ms frames,
  `OPUS_APPLICATION_VOIP`, 16 kHz, and the local engine gets
  `AUDIO_L16_RATE_16000_CHANNELS_1`.

### The Fire OS 6 AFE is not the one described above

From `/system/vendor/etc/audio-algorithms/AFE.cfg` on this unit:

| | Fire OS 5 (this document) | Fire OS 6 (`AFE.cfg` line) |
|---|---|---|
| Mics used | 7 | **4 of 7**: `"Channel Map" : [1,2,4,5] //This selects 4 mics out of 7` (:123) |
| Filterbank | FFT 128, hop 64 (4 ms), 640 taps | FFT 256, hop 128 (**8 ms**), 768 taps (:126-132) |
| AEC | per mic, `TailLen` 2560 (160 ms) | `AEC_V2`, 160 ms tail, 2 refs |
| Fixed beams | 6, `coefs_FBF_6beams_64bands_Biscuit` | **8**, `FBF_V2`, 4 taps, `coefs_FBFV2_LowLatency_8beams.cfg` for both noise models, EVD off (:255-265) |
| After the beams | ABF (beam-space GSC), Ref/SNR beam selectors | `ARA_V2` (target beam *i* against *i*+3, *i*+5), `GroupBeamMergerV2`: 8 groups of 3 adjacent beams, hold 12 frames, margin 1.26 (:266-436) |
| False WW Prevention | in the path | commented out: `//Disabled as this feature is not used anymore on Biscuit - AUDIOALG-52224` (:54) |
| VAD | `Voice Activity Detector` | `DNN VAD` (`vad_lite.tflite`), then `Voice Activity Detector` |
| Output gain | 7.2 dB | 9.5586 dB (:523-526) |
| Path latency | not recorded | `ASR Path Latency` 50.19 ms; AEC-out delay 168 samples |

The beamformer caveats above apply to the Fire OS 5 six-beam file. The Fire OS 6
eight-beam file was not parsed. Its 32,768 floats fit 128 bands × 8 beams ×
4 taps × 4 mics × re/im [INFERENCE].

### Answer

**Amazon's wake engine does not beamform.** The AFE does all of it. On Fire OS 6
that is 8 fixed beams over 4 mics, a beam-space canceller and a group merger
whose output is one channel. Pryon scores that channel as `beamformed`, mono.
The other channels in `micMultiChAsr` exist for:

- the cloud: OneMic uploads one post-AEC mic; MC-ASR uploads two post-AEC mics
  beside the beam;
- per-frame AFE metadata, which rides in the beam's bit 0 and goes to metrics,
  ERLE/NTT and wake-word timing.

Self-wake suppression is not one of these uses. It scores playback audio, and
Alexa holds a separate `loopback` record stream of the playback mix for that
[INFERENCE: the stream's consumer]. Alexa does not even request the reference
channel.

### What EchoMuse should do, ranked

1. **Decode the metadata already in the `micAsr` stream (Fire OS 6).**
   **Implemented** as the optional capability `afe_metadata_v1`
   ([protocol-v1.md](protocol-v1.md) §3 and §4.1, `post-afe-audio-architecture.md`
   §4.5). The decoder is `device/internal/audio/afe`. Per-80 ms records ride
   under uplink leases, and the controller keeps them as evidence on turns. No
   decision reads them. On the device every steady 30 s window validated
   3,750 of 3,750 frames, idle and during playback (3,719 of 3,720 in the
   first window after start, where one frame confirms the lock). A forced
   stream re-open kept the lock and was counted as 72 lost frames. How those
   were measured, what each field can and cannot be trusted for, what
   EchoMuse could build on it and the experiments that would decide it are in
   [afe-metadata.md](afe-metadata.md). One correction to the table above:
   `FRAME_COUNTER` is the exact continuity check, and `AFE_TIMESTAMP` only
   resolves its modulo-64 wraps.
   - *Benefit:* per-8 ms `PLAYBACK_ACTIVE`, `ERLE_RAW`, `DTD`, AFE `RMS`,
     `DNN_VAD_PROB`, `OUTPUT_CLIPPED` and `VOLUME`, all from the AFE that
     processed the very samples. `FRAME_COUNTER`/`AFE_TIMESTAMP` are also an
     independent check on capture discontinuities.
   - *Cost:* shifting one bit per sample plus a popcount every 128 samples.
     No stream change and no mixer cost. A per-80 ms summary is a few bytes of
     uplink.
   - *Risks:* both direction fields were constant in every capture, so the
     metadata is no DOA source. `DTD` moved only during playback, and it was
     not tested against real near-end speech. Field semantics come from
     names and the observations above, not from a specification. Carrying
     native fields was itself a measured protocol change: `afe_metadata_v1`,
     opt-in on both sides (`post-afe-audio-architecture.md` §4.5). Clearing
     bit 0 before scoring, as Amazon does, is optional hygiene: the metadata
     is a ≤1-LSB signal 90 dB down.
   - *Uses* beyond evidence wait for a consumer (attribution, the playback
     profile) and for the field behaviour to hold on more units
     ([afe-metadata.md](afe-metadata.md) §3, §5).
   - *Fire OS 5:* frames are present, in an older layout. 439 saved office-Dot
     clips (canonical PCM from before the Fire OS 6 port) carry sync `0xA5`,
     version `0x41` (2.1) and `PAYLOAD_SIZE` 86 every 128 samples. The checksum
     (popcount of bits 127…26) held on all 1,878,292 frames. There is an
     ERLE-like 8-bit field at 104:97, non-zero only during the Dot's own
     playback. The flags never set, so there is no `PLAYBACK_ACTIVE`. There is
     no frame counter, and the timestamp slot holds a constant `0xBEEF`. Field
     names are unresolved [INFERENCE: the 8-bit field is `ERLE_RAW` by position
     and behaviour], so EchoMuse never decoded it, and no longer supports Fire OS 5. The clips, the layout and
     how it was derived are in [afe-metadata.md](afe-metadata.md) §1.12.
2. **Keep `micAsr` as the wake and ASR source.**
   - The beam in `micMultiChAsr` measured identical, so switching gains no
     wake accuracy by itself.
   - This is the right state until item 3's condition is met.
3. **Switch to `micMultiChAsr` `[0]; true; 1` (beam, m0, ref) only for an
   AFE-aligned reference.**
   - *Benefit:* a reference in the capture timeline at a fixed offset, with no
     clock estimate between the two. It is the speaker feed after the mixer's
     own EQ/limiter, from all mixer clients. The echo in the beam follows it by
     829 samples. That held in four captures across two boots, and the
     reference timeline moved by less than 2 ms. It could replace the 48 kHz
     render tap the firmware decimates into kind 2 reference packets, which
     today are mapped into mic time through `render.progress`'s `estimated`
     clock. In `n1` and `n3` the reference landed 773–791 ms after its chunk's
     `MixerReleaseBufPlay` handoff, and `fireos6-port.md` §6 records +54 to
     89 ms against the estimate in steady state.
   - *Cost:* the mixer 68.7 % vs 67.8 % of a core (unmeasurable). The firmware
     reads 3 channels (96 kB/s) and de-interleaves them. Uplink is unchanged,
     because a kind 2 reference is uploaded under leases either way.
   - *Risks and requirements:*
     - At least one mic channel is mandatory (`[]` is invalid).
     - The AEC-out config is global HAL state, and a rejected setup silently
       keeps the old one.
     - The residual's coherent lag moves from +829 to about +575 samples after
       the AEC converges, so attribution must search a window.
     - It is a contract change: the reference would join the capture epoch, in
       place of the render epoch (`post-afe-audio-architecture.md` §4.3, §16.1).
     - Behaviour across mute was not tested.
   - *Worth doing only if* attribution or self-output decisions are measured
     failing because of reference timing. The architecture already avoids
     relying on estimated timing alone.
   - *Fire OS 5* (no longer supported): no. OpenSL `VOICE_RECOGNITION` returns
     one channel. Whether Fire OS 5's HAL honours `mic_channels`/`ASR_out`/`ref_out`
     was never tested, and OpenSL cannot pass them anyway.
4. **Score wake on several beams: not possible, and not worth emulating.**
   - No configuration outputs beams.
   - Beamforming the four post-AEC mics in firmware would duplicate the AFE's
     8-beam superdirective design on the same four capsules.
   - Each extra scored stream costs another BCResNet: 43 % of one core
     (`post-afe-audio-architecture.md` §5.2), next to the mixer's 68 %.
   - Both platforms: no.
5. **Beam lock for a turn: not available.**
   - No command exists in the clients, the mixer or the HAL.
   - Amazon does not lock either. It reports wake-word boundaries
     (`SET_WAKEWORD_METADATA`) for metrics and localisation.
   - Doing it would mean patching `libasp`. Both platforms: no.

What Amazon does with playback is already in EchoMuse's design. Its
`playback`/`PMA` decoders score the playback mix; EchoMuse's controller-side
reference scorer scores the final-mix reference (`post-afe-audio-architecture.md`
§5.2). The `loopback` stream would only matter for scoring on the device.

### Not established

- Who reads PuffinApp's multichannel stream in MC-ASR mode, and what codec it
  uses (`libAIP.so` was not pulled).
- Whether `DEVICE_MUTE`, `AEC_DIVERGED`, `MIC_CLIPPED`, `SSL_*` or `FD_*` ever
  change on this unit, and what `DTD` does under real double talk. A wake, a
  mute and speech over playback were not exercised.
- The exact `beam_index` mapping, and whether the post-AEC offsets 0–3 are
  physical mics 1, 2, 4 and 5 in that order [INFERENCE from `Channel Map`].
  Nothing beyond the requested order labels the m*k* channels: no test played
  a source near one microphone to tell them apart.
- Whether the `loopback` stream is what feeds the `playback`/`PMA` decoders.
- The Fire OS 5 HAL's support for AEC-out parameters, and the names of the
  Fire OS 5 v2.1 metadata fields (item 1). Both are moot now that EchoMuse
  no longer supports Fire OS 5.

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
  suggest a second canceller stage operating in beam space. Fire OS 6's
  `ARA_V2` pairs every target beam with two reference beams
  (`"ARA_V2 Target to Ref Beams"`), which supports that reading.

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
