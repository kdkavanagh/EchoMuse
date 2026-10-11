# How the native stack handles barge-in, in-turn listening and continuation

Companion to `alexa-afe.md` (the signal processing) and `alexa-endpointing.md`
(when a turn ends). This one is about **what happens while the device is
already talking or already in a turn**: interrupting a response, recognising
"stop" mid-turn, and deciding whether follow-up speech was addressed to the
device at all.

Everything here was recovered from the stock binaries and configs still
present on a rooted Dot Gen 2 (`biscuit`, build `Nov 18 2022`), with Alexa
disabled. Nothing here is running; it is all dormant on `/system`.

## Summary

**Amazon does not solve "someone is talking over me" by lowering the wake
threshold.** The threshold does move during playback, but only from **0.7 to
0.5** — a factor of 0.71, configured as a data-driven override table keyed on
*which kind of audio is playing*. The heavy lifting is done by a stack of
independent **suppression** tests that run *after* a keyword has already been
accepted and can each veto it.

Four decisions that shape the whole design:

- **The best mechanism is not acoustic at all.** When the device's own
  response contains the wake word, it knows from the **TTS speech marks**
  exactly when that word will be spoken, maps that instant onto the
  microphone's sample index, and discards any detection within **1100 ms** of
  it. No echo canceller is involved, and the code is recovered verbatim
  (`EnumeratedPolicy.shouldSuppressWakeword`). The mapping is cruder than it
  sounds — the mic's read position is sampled when the mark is *delivered* —
  which is why the window is 1100 ms, and which is also its precondition:
  the marks must be emitted in real time relative to audible playback.
- **The device scores its own output.** A second, separately tuned keyword
  decoder (`WakeWordService_playback_decoder`) runs on the playback/loopback
  stream. A mic detection within the same window of a loopback detection is
  discarded. That detector is deliberately tuned **more sensitive** than the
  mic one.
- **In-turn "stop" is not a second detector.** It is one extra FST compiled
  into the same keyword model, made active only in an `awake` state by a
  small state machine, with a timeout back to `sleep`.
- **Continuation has two forms.** The cloud-driven one is an `ExpectSpeech`
  directive with a timeout — the direct analogue of HA's
  `continue_conversation`. The device-driven one is "Natural Turn Taking", its
  own decoder fusing acoustic device-directedness, sound-source direction,
  self-reference and an embedding of the **dialog act just performed**. NTT is
  present in the library and *not shipped* on this SKU.

Everything acoustic in the rest of this document exists for the cases the
first two cannot cover: audio the device did not generate — a television, an
advertisement, another Echo.

## Where the evidence is

| Path on device | Size | What it establishes |
|---|---|---|
| `/system/lib/libpryon.so` | 21,588,948 B | Decoder. Spotter, suppression, NTT, endpointing |
| `/system/lib/libwakewordserver_jni.so` | 112,168 B | Service layer. Two audio paths, playback monitoring |
| `/system/lib/libasp.so` | 1,083,168 B | AFE. SB-DTD, ERLE, metadata encoding |
| `/system/local/models/keyword/<locale>/<WORD>/` | — | **Tuned values.** `pryon.config`, `kw.cfg.json`, `op.cfg.json` |
| `/system/local/models/keyword/en-US/AMAZON/playback.config` | — | The self-wake detector's config. Only copy on the device |
| `/system/vendor/etc/audio-algorithms/AFE.cfg` | 41,622 B | `False WW Prevention` block (see `alexa-afe.md`) |
| `SpeechInteractionManager.apk` → `classes59.dex` | 404,508 B | **`EnumeratedPolicy`, `MetricsManager`** — the suppression decision and its instrumentation |
| … → `classes136.dex` | 64,320 B | `WakeWordManager`: `suppressDetection`, `DEFAULT_SUPPRESS_TIME_MS`, `usesLoopback` |
| … → `classes62.dex` | 164,628 B | `TtsSpeechMarksEmitter`, `AspPlaybackReporter` |

Archived off-device at
`~/.local/share/echomuse-native/biscuit-2022-11-18/` (`lib/`, `dex/` with
jadx output in `dex/out*/`) — outside the repo deliberately. These are
Amazon's proprietary binaries, models and tuning; they are reference material
for reading, not redistributable, and must not be committed.

Re-probing was cheap and did not need the 21 MB transferred — `busybox
strings` on the device and grep there. That was the Fire OS 5 image, which
EchoMuse no longer supports; Fire OS 6 has no busybox and not these binaries,
so the archive above is now the reference. The recipe as it ran:

```bash
adb connect 192.168.3.71:5555
adb shell 'busybox strings /system/lib/libpryon.so | busybox grep -iE "suppress|erle|ntt" | busybox sort -u'
adb shell 'cat /system/local/models/keyword/en-US/ALEXA/kw.cfg.json'
```

**The APK is 8.1 MB of bytecode in 180 dex files, and this link cannot move
that** — measured throughput for bulk transfer is 1–2 KB/s, so a 118 MB tar
managed 2.3 MB before being abandoned (this is #139's packet loss showing up
as throughput rather than latency). The way through is to unzip on the device
and let a dex string table act as its own index, because DEX string tables are
plain text:

```bash
adb shell 'cd /data/local/tmp && mkdir simdex && cd simdex && \
  busybox unzip -o -q /system/priv-app/SpeechInteractionManager/SpeechInteractionManager.apk "classes*.dex"'
adb shell 'cd /data/local/tmp/simdex && busybox grep -la suppressDetection *.dex'   # names the dex
adb pull /data/local/tmp/simdex/classes59.dex .                                     # pull only that one
jadx -d out59 --no-res classes59.dex
```

Three 64–404 KB dex files answered every policy question here. Note a
compile-time constant (`public static final String`) is inlined by javac, so
grepping for a property *literal* finds its readers, not just its declaration
— which is how `EnumeratedPolicy` was located from `persist.ww.speechmarks.
suppress`.

## The ASR chain: how it hears you over its own output

This is the part that makes a wake word detectable while music is playing, and
it is separate from the self-wake question. From `AFE.cfg`'s
`Path Definition → ASR → Algorithms`, with the file's own note that "frames
are processed roughly in this order":

```
Downsampler IIR                48 kHz → 16 kHz
HPF 80 Hz @ 16 K               applied to mic in AND ref in
ASR FilterBank                 FFT 128, decimation 64, filter len 640
ASR AcousticEchoCanceler       per mic × 7, Num Refs Per Input = 2, TailLen 2560,
                               adapt 0–8000 Hz, VSS on, band-based tail + step size
ASR ARA                        identical parameters — a second adaptive stage in series
ASR AdaptiveBeamFormer         FixedBeamFormer (6 beams, coefs_FBF.cfg) + adaptive nulls,
                               2 nulls/beam, TailLen 1536, adapt 200–7000 Hz,
                               VSS 1000–6000 Hz, RoundRobin
Voice Activity Detector
ASR RefBeamSelector            ratio threshold 0.4, smoothing 0.83/0.87, 80 frames to transition
ASR SNRBeamSelector            SNRThreshold 6.5, buffer 10, hangover 15,
                               energy adapt fast 0.95 / slow 0.987, energyRatio 1.2,
                               noiseAdaptationFactor 1.001
False WW Prevention            (the self-wake stage — covered above)
ASR Output Gain                +7.2 dB
EspFeatureExtraction           not bypassed
Mics Post AEC Output Gain      0 dB, "To be calibrated"
```

Five things about that ordering matter.

**1. The interferer is a known signal, so it is cancelled, not suppressed.**
`Num Refs Per Input = 2` against `Num Speakers = 2`: both speaker channels are
references, for every one of the 7 mics, with a long adaptive filter across
the full 0–8000 Hz band. Music the device is playing is perfectly known, so
this is a linear subtraction problem, and it runs **before** any spatial
processing. Two cancellation stages in series (`AEC` then `ARA`, same
parameters).

**2. Beamforming happens after cancellation, and the beams can null.** Six
fixed beams designed for this exact array (`coefs_FBF_6beams_64bands_Biscuit`,
see `alexa-afe.md`), each with an adaptive null-former carrying 2 nulls —
so whatever residual survives cancellation can be steered against, in the
200–7000 Hz band.

**3. Beam choice is by SNR, with deliberate hysteresis.** Not a fixed angle
and not per-frame: `SNRThreshold 6.5` dB with `hangoverPeriod 15`, a 10-deep
buffer, two-rate energy tracking (fast 0.95 / slow 0.987) against a
slowly-rising noise floor (`noiseAdaptationFactor 1.001`), and the reference
selector needs **80 frames** to transition. Switching beams mid-word would be
worse than picking a slightly wrong one, so it is damped.

**4. The chain reconfigures on whether anything is playing.** From
`Global Definition`: `"Enable AEC/ABF according to ref level" : true`, with the
comment *"If there is a reference signal, we will use AEC, otherwise we will
use ANC. FBF is always enabled."* So the same pipeline is echo-cancelling
during playback and noise-cancelling in silence, and the fixed beamformer
never goes away.

**5. The ASR path is deliberately LINEAR — and this is the transferable
insight.** Compare what the `Voice` (VoIP) path has and the ASR path does not:

| Stage | Voice path | ASR path |
|---|---|---|
| Residual echo suppressor | `Frequency Masking RES` | **absent** |
| Noise reduction | `Recursive Magnitude Estimation NR` | **absent** |
| Comfort noise | `Random Filler CNG` | **absent** |
| AGC | `FB Automatic Gain Control` | **absent** |
| Output gain | limiter + EQ | fixed **+7.2 dB** |

Every per-volume tuning array in `AFE.cfg` — `Frequency Masking RES`'s
`erleAecFactor`, `dtdDecisionTh`, `avgGainLowTh`, `statHigh` and the rest,
indexed Vol1…Vol10 — belongs to the **VoIP** path, not the ASR path. For wake
word and speech recognition Amazon does linear echo cancellation and spatial
filtering, applies one fixed gain, and hands the decoder audio that **still
contains residual echo** rather than audio that has been nonlinearly scrubbed.
The self-wake problem is then solved with metadata — speech marks, ERLE flags,
a loopback decoder — precisely so that the audio the model sees is never
distorted to get rid of it.

### What the VAD block says about levels

```json
"Voice Activity Detector": {
    // Device side bias factors always set to 1.0f. Bias factors are applied on the cloud.
    "Ambient Gain": 1.0,  "Voice Gain": 1.0,
    "System Gain Available": true,
    "PGA Gain": 20.0, "ADC Gain": 0.0, "AFE In Gain": 0.0, "AFE Out Gain": 7.2,
    "referenceAudioLevelOutputInDb": -53,
    "referenceOneMicAudioLevelOutputInDb": 0   // To be calibrated
}
```

The device-side VAD carries **no decision bias at all** — both gains are 1.0
and the comment says the bias is applied in the cloud. What it does carry is
the **whole gain structure**, so a level can be referred to an absolute
calibrated scale (`referenceAudioLevelOutputInDb: -53`) rather than to
whatever the ADC happened to produce. A device that knows its own gain chain
can report "this was 55 dB SPL" instead of "this was 0.02 RMS".

### ESP, and why it is in this chain

`EspFeatureExtraction` sits in the ASR path and is **not bypassed**. ESP is
Echo Spatial Perception — the feature set Amazon uses to decide **which Echo
in a house should answer**. It is computed in the AFE, alongside the echo
canceller that already knows the reference signal, and shipped upward with the
utterance. EchoMuse's `em_arbiter` answers the same question with
first-detector-wins and no acoustic feature at all; the repo's own note that
SNR-at-detection was indistinguishable across devices (0.9 / 1.15 / 0.93) is a
measurement of how little a naive energy proxy carries, not of whether the
question is answerable.

### And on the decoder side

Three things in `pryon.config` / `kw.cfg.json` help under a music bed:

- **Continuous mean normalisation that bridges utterances** —
  `fe.feat.continuous_norm_estimation = 1`, `current_frame_decay_weight =
  0.003`, `do_mean_norm = 1`, `do_variance_norm = 0`. A steady bed is adapted
  *out* as background over a long time constant. There is no per-window
  rescaling anywhere in this front end.
- **Graceful degradation under backlog rather than falling behind** —
  `scorer.score_upsampling`: default `skip_frame_count 2`, and once the
  backlog exceeds `fallback_backlog_threshold_msec = 300` it switches to
  `fallback_skip_frame_count = 5`. The detector thins its own work instead of
  accumulating latency.
- **The accept threshold drops 0.7 → 0.5** while `AudioPlayerState` is set
  (see the override table above), on top of all of the above — not instead of
  it.

## Layer 1 — the AFE decides whether it is hearing itself

Inside `mediaserver`, `libasp.so` runs a dedicated **`False WW Prevention`**
stage, sitting in the ASR chain after beam selection and before output gain.
Its tuning is in `AFE.cfg` and its thresholds are indexed by playback volume
— `alexa-afe.md` already covers that block, including the key detail that it
distinguishes *single talk* (echo only) from *double talk* (echo plus a
person) at each of ten volume steps.

What matters here is the vocabulary it exports upward, which is what every
later decision consumes:

- **SB-DTD** — "Sub-Base DTD", a double-talk detector. `libasp.so` carries
  `SB-DTD Algorithm`, `"enable DTD": true`, and per-volume
  `dtdDecisionTh` / `dtdSmoothUp` / `dtdSmoothDown` arrays (0.45 at low
  volume falling to 0.2 at high volume). Errors name its shape:
  `Error: While parsing False WW Prevention block, expected 3 parameters for
  SB-DTD, received %d` and `Error: Decision Hangover Frames for SB-DTD should
  be at least 1.`
- **ERLE** — echo return loss enhancement, computed in two bands
  (`ERLEComputeLowBand`, `ERLEComputeHighBand`), with `ERLEThresholddB`,
  `erleUpTh`, `erleAecFactor`/`erleAecAttX` per volume, and diagnostics that
  average it over 4 ms frames (`Each number represents average ERLE number of
  %d frames(4ms)`, `current ERLE: %f  Last Good ERLE: %f  Corner: %f
  Wall: %f  Freespace: %f`). Note the last one: ERLE is tracked against
  *placement* classes.
- **An encoding state** — `Audio metadata encoding state : %d`, and in
  `AFE.cfg` the `False WW Prevention` block opens with
  `"defaultEncodingState" : false`. The AFE can annotate the audio it hands
  upward, and does not by default.

## Layer 2 — the metadata rides inside the audio

The transport is the interesting part: **the per-frame flags travel in the
least significant bits of the PCM samples.** `libpryon.so` carries
`pryon/cpp/signalprocessing/audio_metadata_decoder.cpp`, the classes
`AudioMetadataDecoder` and `AudioLSBMetadata`, the property
`recognizer.audioMetadataDecoderEnabled` ("Enables Audio Metadata Decoder by
default."), and a per-frame flag `has_audio_lsb_metadata`.

It is treated as a lossy channel, with accounting to match:
`CorruptedLSBFrameCount`, `CorruptedLSBFrameCountPer`,
`NoLSBMetadataObserved`, and a heartbeat line `Peculiar audio frames seen
since last short heartbeat message. Corrupted LSB frames count:`.

Two things follow that are easy to get wrong:

- **ERLE forces the decoder on.** `recognizer.erle.enabled is set when
  recognizer.audioMetadataDecoderEnabled is not; Overriding
  recognizer.audioMetadataDecoderEnabled to True because ERLE requires it`.
  The suppression logic is not independent of the transport.
- **The LSB channel on this build carries ERLE but not the rest.** Verbatim:
  `AFE LSB metadata doesn't support fields of Sub-Base DTD flag or playback
  status. Skipping evaluation of DTD for the purposes of NTT self-wake
  suppression`. So the richest self-wake test degrades on this hardware for
  lack of transport, and says so rather than guessing.

The decoder keeps history rather than acting per frame: `AfeMetadataHistory`,
`AfeMetadataMetricsCalculator`, `AfeMetadataMetricsEvent`, and a range query
over it (`Invalid start and end indices provided when trying to get ERLE
stats from AFE metadata metrics calculator`). That is what lets a suppression
decision be made over *the span of the wake word* rather than at one instant.

There is a second, explicit transport as well: the wake word server writes a
metadata blob into its own stream (`metadataStream writer`, `cannot write
metadata with null reference of metadataWriter or blobSize`, `Expected to
write metadata size %d, but only write %d`).

## Layer 3 — the keyword spotter

### It is two stages, and only the second one has a tunable threshold

From `en-US/ALEXA/pryon.config`:

```
search.decoder_type            = "kaldi-key-phrase"
search.trans_filepath          = "final.mdl"
scorer.model_filepath          = "dnn_quantized.mlp"
scorer.acoustic_scale          = 0.05
keyword_spotter.config_filepath    = "kw.cfg.json"
keyword_spotter.op_config_filepath = "op.cfg.json"
keyword_spotter.emit_nearmiss  = 1
fingerprinter.enabled          = 1
fe.type                        = "LFBE"
fe.mel_fbank.num_bins          = 20        # 100–6000 Hz
fe.feat.stack_left             = 20
fe.feat.stack_right            = 10
```

Stage one is a Kaldi HMM search over compiled FSTs (`ALEXA.fg.hclg.pfst` /
`ALEXA.bg.hclg.pfst`, `beam 25.0`, `max-depth 200`, `block-size 1024`,
`window-size 6`). Its own thresholds are **wide open** —
`hmm-thresholds: {accept 0.0, notify 1e+37, escalate 1e+37}` — so it proposes,
it does not decide.

Stage two re-scores the proposal. For ALEXA it is a DNN
(`nemodnn_2ndStageV1.mlp`, normalised by `cmvn_2ndStage.mat`); for AMAZON it
is an SVM (`AMAZON.psvm` + `AMAZON.scales`, `probabilistic: false`). Its
input is a fixed feature selection, from `kw.cfg.json`:

```json
"vector-space-mapper": { "type": "version1", "version1": {
  "add-context-features": true,
  "feature-indices": [0,1,2,3,5,6,7,9,10, ... ,63,65,66,67,68,69,70],
  "log-energy-feature-begin-index": 400,
  "log-energy-feature-end-index": 420,
  "sns-smoothing-context": 5,
  "sns-speech-prior": 0.5
}}
```

66 of indices 0–70 (4, 8 and 64 are dropped), plus a 20-wide log-energy block
at 400–420. So the verifier sees posteriors *and* absolute level.

The handoff between stages is named "escalation":
`escalation-period` (500 for ALEXA, 200 for AMAZON), `NearMissAndEscalation`,
`DEBUG_PRYON_KEYWORD_START_ESCALATION_PERIOD` /
`..._END_ESCALATION_PERIOD` / `..._DETECTION_ESCALATION`, and the guard
`Escalation is not enabled for single stage spotters`.

Rate limiting is explicit and separate from thresholding:
`lock-period: 40` (a refractory period), `max-per-window: 6` within
`window-size: 100`, `cleanup-period: 6000`, `stickiness: 0`.

`notify-threshold` is the near-miss report level (`emit_nearmiss = 1`) — the
same idea as EchoMuse's `nearMissThreshold`, but built into the decoder.

### The barge-in mechanism: an override table keyed on client properties

This is the part worth reading verbatim. From `en-US/ALEXA/kw.cfg.json`:

```json
"classification-thresholds": {
  "accept-threshold":   0.7,
  "notify-threshold":   0.1,
  "escalate-threshold": 1e+37,
  "overrides": [
    { "accept-threshold": 0.5, "notify-threshold": 0.1,
      "clientProperties": [ { "name": "AudioPlayerState", "equals": 1 } ] },
    { "accept-threshold": 0.5, "notify-threshold": 0.1,
      "clientProperties": [ { "name": "audio_playback",   "equals": 1 } ] },
    { "accept-threshold": 0.5, "notify-threshold": 0.1,
      "clientProperties": [ { "name": "AlarmState",       "equals": 1 } ] }
  ]
}
```

Four observations:

- **The move is 0.7 → 0.5.** Not an order of magnitude. Whatever depression
  speech-over-playback suffers, Amazon's answer is a modest relaxation plus
  the suppression stack below — not a low bar on its own.
- **It is keyed on what is playing, not on "is playing".** The properties are
  distinct per stream type.
- **`TTSPlayerState` is monitored but is not an override key here.** The
  device knows when it is speaking its own response, and this table does not
  relax the bar for that case — only for media, generic playback, and alarms.
  Whether `audio_playback` subsumes TTS in practice is not answerable from the
  config alone. *[INFERENCE — the property names are matched by the decoder;
  which client sets which is a Java-layer question, still open.]*
- **Scales differ per model and are not comparable.** ALEXA's 0.7/0.5 are
  second-stage DNN scores; AMAZON's are SVM margins (`accept -0.75`,
  `notify -3.25`). A number from one model means nothing against the other.

#### The client-property system underneath it

The override table is one consumer of a general mechanism. `libpryon.so`
carries the default map as an embedded JSON literal, which is the clearest
statement of what the decoder expects to be told:

```json
{"clientProperties":{
  "AudioPlaybackState": -1, "AlarmState":        -1,
  "MediaPlayerState":   -1, "EarconPlayerState": -1,
  "TtsPlayerState":     -1, "WorkoutMode":       -1,
  "SVMode":             -1, "AutomotiveMode":    -1
}}
```

Five things follow from that literal:

- **Unknown is −1, not 0.** Every property initialises to a sentinel that is
  neither "playing" nor "not playing" — the same NULL-not-zero discipline
  CLAUDE.md requires of `playback_stats` and `turns.dev_shadow`. A decoder
  that had defaulted these to 0 would silently assert "nothing is playing"
  about a client that never reported.
- **Earcons are their own state.** `EarconPlayerState` is tracked separately
  from TTS and from media — an independent convergence with EchoMuse's wake
  chime being a third audio plane that deliberately must not make
  `IsStreaming()` true.
- **Properties expire.** `OutsideClientPropertyTimeout` exists, so a stale
  property does not pin an override open indefinitely. Combined with the
  timestamp on every `pushClientEventToPryon` call, the decoder treats client
  state as time-bounded evidence rather than a latch.
- **There are two generations of naming**, and both appear in the binaries:
  `AudioPlaybackState` / `TtsPlayerState` in this literal against
  `AudioPlayerState` / `TTSPlayerState` in `libwakewordserver_jni.so` and in
  the override table. The names are matched as **strings** supplied by the
  client, not enumerated by the decoder, which is why a config can name
  `audio_playback` alongside `AudioPlayerState` and both work — and why a
  typo would fail silently as an override that never applies.
- **Overrides are a general facility, not a barge-in special case.** Beyond
  the keyword thresholds there are `client_properties_overrides`,
  `cv_client_property_overrides`, `dvad_client_property_overrides`, a
  `client_property_operator` (so matching is not limited to `equals`), and
  watermark tuning keyed the same way: `Watermark config overrides must
  contain a 'client_property' key to indicate which client property should be
  used in loading overriden values.` Mute arrives through the same door
  (`recognizer.mute_state.client_property_name`).

So the shape is: *the decoder is told, with timestamps and expiry, what the
device is doing; and every tunable can carry a table of alternative values
selected by that state.* The barge-in threshold is one row of one such table.

### The second decoder: the device scores its own output

`en-US/AMAZON/playback.config` is three lines and is the whole idea:

```
INCLUDE "pryon.inc"
keyword_spotter.config_filepath = "kw.cfg_playback.json"
fingerprinter.enabled = 0
```

Same front end and acoustic model, a different spotter config, and
fingerprinting **off** — fingerprinting is for identifying known media
arriving at the mic, which is meaningless on a stream you generated.

The tuning is asymmetric, and in the direction that matters:

| | accept | notify |
|---|---|---|
| mic path (`kw.cfg.json`) | −0.75 | −3.25 |
| playback path (`kw.cfg_playback.json`) | **−0.95** | −1.55 |

The self-detector's accept threshold is *lower* — more willing to fire. A
missed self-detection costs the user a false wake; an over-eager one costs at
most a suppressed detection that the SNR band can still rescue. So the
suppressor is biased toward over-detection while the mic detector is biased
toward precision.

`playback.config` exists only under `en-US/AMAZON` on this device, so the
self-wake detector runs the cheaper SVM model regardless of which wake word
the user selected. *[INFERENCE from the file layout.]*

The service layer wires it (all from `libwakewordserver_jni.so`):

```
amazon::WakeWordService::detectAudioDataFromAudioInput(void*, unsigned, unsigned long long)
amazon::WakeWordService::detectAudioDataFromPlayback (void*, unsigned, unsigned long long)
amazon::WakeWordService::enablePlaybackDetector()
amazon::WakeWordService::setPushDataToPlaybackDetectorEnabled(bool)
amazon::WakeWordService::getInputFrameCount(long long&)
amazon::WakeWordService::getPlaybackFrameCount(long long&)
amazon::PlaybackMonitor::threadLoop()
```

`PlaybackMonitor` is a binder client with its own thread
(`getService`, `onServiceConnected`, `onServiceDisconnected`, `binderDied`)
that reads the render stream as a media source (`Error there is no media
source`, `Error reading sample rate from media source`). Two separate frame
counters exist because the two streams must be aligned in time for a
span-based comparison to mean anything; `logFrameCountBoundaries()` reports
them. The playback path can be declined at startup: `Will NOT initialize
playback as it is suppressed at ww service initiation time`.

### Suppression: four vetoes, all probabilistic, all with a shadow mode

A detection that clears the (possibly overridden) accept threshold is then
subject to independent vetoes. These four are the **decoder's own**, evaluated
inside `libpryon` from AFE metadata; the Java layer adds two more of its own
on top (speech marks and the loopback-detection delta — see Layer 5), so a
detection has to survive both sets.

**1. ERLE over the wake-word span.** `recognizer.erle.enabled` is tri-state —
`Enables ERLE Suppression. 0: disabled (default) 1: enabled 2: shadow mode
(collects metrics but ignores suppressions)`. The threshold is a *fraction of
frames*, not a level: `recognizer.erle.suppress_threshold` is documented in
the binary as "The percentage of frames in the wakeword span that must have
the ERLEState flag set to true in order to suppress a keyword detection
event". Outcome `Suppressing detection event due to ERLE`, with types in
`pryon::ErleSuppressionDecisionType` and stats `erleMean`, `erleStdDev`,
`erleNonZero`, `erleThreshold`, `prev_erle_count`.

**2. SB-DTD self-reference.** `ntt.self_wake_prevention`, "Path to the self
wake suppression config.", `self_wake_eval_duration_msec`,
`decision_hangover_afe_frame_count`, `highDTDThreshold` / `lowDTDThreshold`,
`dtdEventCounter`, `Max Dtd event count is`. Evidence is accumulated as
`pryon::ntt::SelfWakeInfo` records consumed by a
`pryon::ntt::SelfReferenceCalculator`; outcome `Suppressing NTT detection due
to DTD self-reference heuristic`.

Its degradation rule is worth copying on principle: `No self wake info has
been stored yet. Unable to assess DTD stats for the purposes of
self-reference wake prevention. **Accept the candidate event by default**`,
and separately `Collected self wake info count doesn't meet the minimum self
wake evaluation frame count`. Missing evidence never becomes a rejection.

**3. Fingerprint match** — `fingerprinter.enabled = 1` on the mic path,
`PryonDecoder_SetFingerprintDatabase`, exposed as
`WakeWordService::setFingerprintDatabase(const char*)`. This is the "a TV ad
said Alexa" case: match the arriving audio against a database of known media.

**4. Watermark detection** — `WatermarkProcessor::getSnrThresholdForKeyHash
returned:`, `wmSnrThreshold`. An inaudible marker in Amazon's own
advertising, with a **per-key SNR threshold** rather than one global bar.

Three properties are shared across all four:

- **They are probabilistic on purpose.** `fingerprinter.keyword_event_suppress
  _probability`, `recognizer.erle_suppress_probability`, documented as
  "Probability that a … will suppress a corresponding keyword event, where 0.0
  is never suppress and 1.0 is always suppress. **Defaults to 995 in 1000**",
  with `fingerprinter.keyword_event_suppress_rand_seed` ("Random seed for
  probabilistic suppression. If set to 0, current time is used as the seed").
  So ~0.5% of suppressible events are deliberately let through — that is the
  only way to keep measuring what suppression is costing in the field.
  Log lines: `Found an ERLE should be suppressed but probabilistically not
  suppressing based on a probability of`, and the same for fingerprint and
  watermark.
- **Shadow mode is a first-class state**, not a debug flag: `Detected DTD
  suppression event, but it is not suppressed because of the Shadow Mode`,
  `not suppressing because Shadow Mode is set`, outcome label
  `Accepted_SelfWakeShadow`. The same pattern EchoMuse uses for on-device wake
  word, arrived at independently.
- **There is an SNR rescue band.** `snr_threshold` plus
  `barely_suppressed_snr_threshold`, with the constraint `Barely suppressed
  SNR threshold must be equal to or greater than SNR threshold`. Loud enough
  speech overrides suppression; the band between the two thresholds is the
  "only just suppressed" population you would want to inspect.

Terminal labels: `Rejected_SelfWake`, `Rejected_SSL`,
`Rejected_SelfWake_and_SSL`, `Accepted_SelfWakeShadow`, reported through
`PryonSuppressDecision:` / `SuppressDetection:` / `erle_suppression_decision`.

### In-turn listening: a state machine over keywords

`op.cfg.json` (`recognizer.override_behavior.json_filename`,
`keyword_spotter.op_config_filepath`) is the entire in-turn interruption
mechanism, and it is 25 lines. From `en-US/ALEXA`:

```json
{
  "object_type_name": "keyword-spotter-machine-config",
  "initial": "sleep",
  "sleep": { "rules": [ { "name": "ALEXA", "next": "awake" } ] },
  "awake": {
    "rules": [ { "name": "ALEXA", "next": "awake" },
               { "name": "STOP",  "next": "sleep" } ],
    "timeout": { "duration": 175, "next": "sleep" }
  }
}
```

Implemented by `pryon::KeywordSpotterMachine` (`Graph`, `State`,
`DetectionEventHandler`, from `pryon/cpp/spotter/keyword_spotter_machine.cpp`).

What this buys, and why it is cheap:

- **`STOP` is a second FST in the same model**, not a second detector.
  `STOP.fg.hclg.pfst` / `STOP.bg.hclg.pfst` sit beside `ALEXA.*` in every
  keyword directory, sharing one front end, one acoustic model and one search.
- **`STOP` gets no second stage.** Its classifier is
  `{"trivial": {"score": 0.0}}` and every threshold is `0.0` — HMM-only. It is
  a bare-word spotter whose precision comes from *when it is allowed to fire*,
  not from a verifier.
- **It can only fire while `awake`**, which is entered by the wake word and
  left by `STOP` or by timeout. So "stop" never triggers from ambient
  conversation; it is armed by the interaction itself.
- `duration: 175` — *[INFERENCE: frames. At the 10 ms
  `fe.audio_analysis.frame_shift_milli`, ≈1.75 s. `lock-period: 40` ≈ 400 ms
  and `cleanup-period: 6000` ≈ 60 s are consistent with that reading, but no
  string in the binary states the unit.]*

Above this sits the local-command path already documented in
`ring-stop-word.md` and `alexa-endpointing.md` — `LOCAL_COMMAND`,
`is-local-command`, `KeywordLocalCommandTimeout`, and the three shipped
actions `com.amazon.speech.LocalCommand.{Stop,Snooze,Cancel}`. The
`LocalCommandEnumeratedResult_Callback` in the `WakeWordService` constructor
is the delivery path, separate from the ordinary wake callback.

### Continuation: Natural Turn Taking

There are two continuations in this stack and they answer different questions.
The cloud-driven one is an `ExpectSpeech` directive — "open the mic now,
expect a reply, time out after N" — carried as
`amazon/speech/model/speechrecognizer/ExpectSpeechDirective` with an
`Initiator`/`InitiatorPayload` and an `ExpectSpeechTimedOutEvent` sent back
when nobody speaks. NTT is the other one: **nobody asked the user to speak,
and they did anyway — was it for me?**

NTT is not an open microphone with a timer. It is a **separate decoder
instance** with its own model, its own audio push and its own result channel:

```
amazon::WakeWordService::setNttModel(const char*)
amazon::WakeWordService::setNttDetectionMode(bool, const char*)
amazon::WakeWordService::pushAudioToNttDecoder(void*, size_t, uint64_t)
amazon::WakeWordService::isNttDecoderReady()
amazon::WakeWordService::destroyNttModel()
                         NttDecoderStatus_Callback     (constructor arg)
```

Internals: `pryon::NaturalTurnTakingPipelineBuilder`,
`pryon/cpp/core/ntt_pipeline_builder.cpp`, and a fusion stage —
`pryon::NttFusionProcessor` (`ntt_fusion_processor.cpp`), publishing
`NttResultPayload` to `pryon::NttResultNotifier`. It emits per-frame state
(`NTT_state: FRAME:`, `NTT_RESULT_METADATA:`) and requires a single-threaded
search (`NTT pipeline does not support search.use_separate_thread=1`).

The fused inputs are each visible:

- **Acoustic device-directedness** — `DeviceDirectednessVerifier`,
  `acousticEmbeddingForDeviceDirectedness`,
  `recognizer.directedness_score_lower_bound` / `_upper_bound`,
  `recognizer.confidence_device_directedness_interval_frame_count`. Covered in
  `alexa-endpointing.md`; this is where its output is consumed.
- **Sound-source localisation** — `pryon::ntt::SSLAssessor`,
  `SoundSourceLocalizationNttClientEvent` (`ssl_ntt_client_event.cpp`),
  `pryon_ssl_event_cadence_msec`, `NTT SSL Assessment:`, `SSL metadata:`,
  and a versioned wire format (`Support for given version of SSL NTT event is
  not implemented. Version:`). **This is not computed by the wake word engine
  at all.** `SpeakerLocalizationClientManager` binds a separate system service
  (`com.amazon.alexa.ms.speakerlocalization`), receives
  `SpeakerDirectionEvent`s carrying a JSON payload plus **start and end
  timestamps**, and forwards them via
  `WakeWordServiceCore.onSpeakerLocalizationEvent(json, startMs, endMs)`.
  Direction of arrival is somebody else's product, pushed in as a timestamped
  client event.
- **Self-reference** — `pryon::ntt::SelfReferenceCalculator` +
  `SelfWakeInfo` + the DTD stats above, i.e. the same machinery that guards
  the wake word, reused to stop a continuation triggering on the device's own
  speech.
- **CV metadata — it is facial analysis, confirmed.** `CvKinexClientManager`
  binds `com.amazon.alexa.ms.kinex.client.NttFacialAnalysisClient` and
  forwards its `AnalysisResult` into the decoder as timestamped JSON
  (`sendCvEventToPryon`), arriving at `WakeWordService::onCVEvent(const
  char*, long)` / `pryon/cpp/pryon_ntt/cv_metadata_calculator.cpp`. Another
  external service, and one a Dot cannot have — it has no camera.
- **The dialog act the assistant just performed** — and this is the most
  interesting input in the whole stack:

  ```
  PryonApi_SetDialogActEmbeddingCallback   (EmbeddingReturn (*)(const char*))
  dialog_act_embedding / dialog_act_embedding_dim / dialog_act_input
  "Concatenate input specified but no dialog act embedding found"
  ```

  The model is **conditioned on what was just said to the user**, supplied by
  the client as an embedding looked up by name. "Which one did you mean?"
  and "Playing jazz" imply completely different priors on whether the next
  sound in the room is addressed to the device, and this is how that reaches
  the classifier. It is a semantic input to an acoustic decision.

Playback interacts with NTT explicitly: `First frame of override OP during
NTT evaluation due to audio playback` — the override state machine is applied
inside NTT evaluation too.

**None of this runs on a Dot Gen 2, and the model was never on any disk we
can read.** `NttModelManager` names it explicitly:

```java
private static final String ARTIFACT_ID       = "NTT";
private static final String ARTIFACT_KEY_VALUE = "wakeword-free";   // Amazon's own name for it
public  static final String DEFAULT_MODEL_DIR  = "/system/local/models/NTT";
private static final String DISABLE_DOWNLOAD_PROPERTY_KEY = "persist.debug.davs.ntt.disable";
```

It is a **DAVS download**, not a firmware asset (`ClientProcessTemplateV2`,
`checksum.txt`, an `engineCompatibilityIdList`, per-locale). On this unit
`/system/local/models/NTT` does not exist, `/data/local/models` does not
exist, `setNttDetectionMode` answers `NTT not supported on this build type.`,
`ntt.config_filepath` has no value, and the runtime consequence is
`Ignoring PushNttEvent; Decoder does not support NTT events`.

So the design is recoverable and the weights are not — and even with weights,
two of the four fusion inputs are separate Amazon services (speaker
localisation, facial analysis), one of which needs a camera. What remains
portable is the *shape*: a per-frame decision, fused from a directedness
score, a self-reference test, and semantic context about what was just said.

### Mute reaches the decoder as data

Not a gate in front of it: `recognizer.mute_state.client_property_name`,
`recognizer.mute_state.digital_silence_buffer_num_frames`, plus
`recognizer.drop_digital_silence_frames` (valid only with
`search.decoder_type="kaldi-key-phrase"`) and
`recognizer.min_digital_silence_frame_percent`. The decoder is told the mic
is muted, and separately knows how to recognise and drop digital silence
rather than normalising into it — the same hazard `em_samples`'
`ABS_FLOOR_DB` clamp exists for.

## Layer 4 — the service, and how state gets into the decoder

`libwakewordserver_jni.so` exports 97 `amazon::` symbols (`T`/`W`), so this
is a usable C++ surface, not an internal detail. The parts relevant here:

**Playback state is watched on a thread and pushed as a callback.**
`amazon::PlaybackStatusNotifier` (constructed with `void (*)(void*, bool,
int)`) runs `threadLoop()` calling `checkPlaybackStatus(amazon::streamState&)`
and logging `State change stream[%d] state[%d]`. The four streams it knows
are, verbatim:

```
AudioPlayerState    TTSPlayerState    AlarmState    ASPState
```

which lands at `WakeWordService::processPlaybackNotifierCallback(bool, int)`
— a boolean and a stream id, matching the `clientProperties` names in the
override table.

**Decoder configuration is runtime and by string name**, which is what makes
the override table usable at all:

```
WakeWordService::setPryonProperty(const char*, int64_t)
WakeWordService::setPryonAcousticProperty(const char*, int64_t)
WakeWordService::pushClientEventToPryon(const char*, int64_t)
WakeWordService::pushClientEventToPryon(const char*, int64_t, PryonString)
```

underpinned by `PryonDecoder_PushClientEvents` / `Unknown PryonClientEvent
type:`. Note every push carries an explicit **timestamp** — the decoder
correlates client state with audio frames rather than assuming "now".

**Other surface worth knowing exists**: `switchPryonModel` /
`setNewPryonModel` (hot model swap), `injectWaveFile(const char*, bool)`
(offline audio injection — a test harness), `isAudioSlience` (sic),
`resetAudioInput`, `setBacklogWatermarks(int, int)`,
`setPryonPriorities(int, int)` / `setPryonDecoderThreadPriority`,
`setAudioEventDetectionMode(bool)` + `setAedStaticModel`/`setAedCustomModel`
(with `/system/local/models/AED/`, a 770 KB MLP), `getAudioPresenceData` /
`detectAudioPresenceData` (presence detection, `PryonApi_SetPresenceDetection
ResultCallback`, `SPCH-WW: Presence DetectionType: %d`), and federated
learning callbacks.

### The Java boundary, and two primitives that are not thresholds

`SpeechInteractionManager`'s own JNI library
(`/system/priv-app/SpeechInteractionManager/lib/arm/libwakewordmanager.so`,
17,944 B — stubs only, the logic is in the bytecode) exports the API the
policy layer actually drives:

```
WakeWordManager_startDetection / stopDetection
WakeWordManager_pauseDetection / resumeDetection
WakeWordManager_suppressDetection      WakeWordManager_getSuppressTimeMs
WakeWordManager_usesLoopback
WakeWordManager_getMicStreamPosition   WakeWordManager_getLoopbackStreamPosition
WakeWordManager_setMicDataStream       WakeWordManager_pushAudioEvent
WakeWordManager_injectAudio            WakeWordManager_restartApe
WakeWordManager_nSetWakeWord / nSetLocale / addLocaleFallback
artifact_ArtifactManager_shouldDownload / onNewModelDownloaded / setBundledManifests
```

Two of those are mechanisms the config files do not reveal:

- **`suppressDetection` with `getSuppressTimeMs`** — a *time-bounded* mute of
  the detector, requested by the client, with the duration readable back. That
  is a third lever alongside the threshold override and the post-detection
  vetoes, and it is the blunt one: not "score this differently" but "do not
  detect for the next N ms". An earcon about to play, or the first
  milliseconds of the device's own response, are the obvious uses. *[INFERENCE
  — the callers are in the bytecode.]*
- **`usesLoopback`, `getMicStreamPosition`, `getLoopbackStreamPosition`** —
  the far-end stream is called *loopback* here, it is a capability the device
  may or may not have, and **both streams expose a position**. That is the
  time alignment the self-wake comparison depends on, surfaced all the way up
  to Java rather than hidden in the service. `PlaybackMonitor`'s
  `logFrameCountBoundaries()` is the same information one layer down.

`restartApe` is worth noting for its blast radius: the client can restart the
audio processing engine underneath the detector, which is the kind of recovery
path EchoMuse handles by `log.Fatalf` and an A/B slot flip.

## Layer 5 — the policy layer, and the mechanism that needs no acoustics

The layers above are mechanism. The policy — *when* to set
`AudioPlayerState`, what counts as self-wake, what to do with a `STOP` — is
Java, in `SpeechInteractionManager.apk`. Three dex files out of 180 answered
all of it; see "Where the evidence is" for how they were located without
moving the APK. This is also where the most surprising mechanism in the stack
lives.

### TTS tells the AFE it is speaking

`amazon/speech/simclient/asp/AspPlaybackReporter` (with `AspWrapper` /
`IAspWrapper` over `com/amazon/asp/AudioSignalProcessor`) exposes
`reportAspPlayback`, and the command it sends is named in the string table:

```
ASP_CMD_NOTIFY_TTS_STATUS
AudioSignalProcessor client not found on this device
```

So playback status is pushed **into the AFE**, not only into the decoder. That
closes the chain this document started with: the TTS player tells the ASP it is
speaking → the ASP's `False WW Prevention` stage uses it alongside SB-DTD and
ERLE → the verdict is encoded into the audio's LSBs → Pryon's
`AudioMetadataDecoder` reads it back and the suppressors act on it. The
"playback status" field named in the LSB-limitation string has an identified
producer.

### Self-wake suppression using TTS word timings

This is the mechanism no amount of config-reading would have revealed, and it
is decompiled rather than inferred:
`amazon/speech/wakewordservice/EnumeratedPolicy` (`classes59.dex`).

**Speech marks are the TTS engine's word-level timing metadata** — they exist
to drive lip-sync and karaoke-style highlighting. Amazon reuses them to decide
whether a wake-word detection was its own voice, and the decision costs no
acoustics at all. When Alexa's own response contains the word "Alexa", the
device does not work that out by listening; it already knows from the
synthesiser when that word will leave the speaker. Every acoustic mechanism
in this document — the ERLE span test, SB-DTD, the loopback decoder — is for
the cases where it *cannot* know: media it did not generate, a television, an
advertisement.

The two decoders are named here, which also confirms the loopback design:

```java
public boolean shouldSuppressedResult(String decoderId, String enumResult,
                                      int detectionType, long startSampleIndex) {
    if ("WakeWordService_playback_decoder".equals(decoderId))
        return processResultFromPlayback(detectionType, getPlaybackSuppressionThreshold(enumResult));
    if ("WakeWordService_input_decoder".equals(decoderId))
        return processResultFromInput(enumResult, detectionType, startSampleIndex);
    return true;
}
```

#### Ingest: a TTS word mark becomes a mic sample index

```java
public int onSpeechMark(String speechMarks, InputFrameCountProvider provider) {
    if (shouldDisableSpeechMarks()) { … return 0; }            // persist.ww.speechmarks.disable
    SpeechMarksContainer c = SpeechMarkBundleParser.parseSpeechMarkBundle(speechMarks, this.mWakewordMatcher);
    setLatestSpeechMarkInputFrameCount(c, provider);
    return c.getWakewordCount();
}
```

Three things happen in those four lines:

- The marks are matched against a **wake-word regex matcher**
  (`mWakewordMatcher`, configured from `mWakewordRegexJson` via
  `setWakeWordMatchingJson`), so only marks whose *text* is a wake word
  matter. `getWakewordCount()` is how many were found.
- The mark's playback time is converted into a position on the **microphone**
  stream via `InputFrameCountProvider`, and stored as
  `mLatestSpeechMarkInputFrameCount`. This is the cross-domain step: playback
  time → mic frame index.
- There is a second entry point, `onWakeWordSpeechMarks(Bundle, provider)`,
  reading a `long[]` under the key `wakewordTimestampsMs` — so the TTS
  pipeline can hand over wake-word timestamps directly instead of full marks.

Conversion is a fixed ratio, with the sample rate spelled out:

```java
private static final long SAMPLE_RATE = 16000;
private static final long SAMPLE_RATE_TO_MILLIS_RATIO = 16;
static long millisToSampleFrames(long millis)      { return 16 * millis; }
static long sampleFramesToMillis(long sampleFrames){ return sampleFrames / 16; }
```

#### The decision, and the recovered window: 1100 ms

```java
static final long DEFAULT_WAKEWORD_SUPPRESSION_THRESHOLD = 1100;              // ms
static final long CELEBRITY_WW_PLAYBACK__SUPPRESSION_THRESHOLD = 1000;        // ms

public boolean shouldSuppressWakeword(long startSampleIndex, String wakeword) {
    boolean bySpeechMarks = shouldSuppressBySpeechMarks(startSampleIndex, DEFAULT_WAKEWORD_SUPPRESSION_THRESHOLD);
    boolean byPlayback    = shouldSuppressByPlayback(now(), getPlaybackSuppressionThreshold(wakeword));
    return bySpeechMarks || byPlayback;
}
```

Both tests are the same shape — an absolute delta against a threshold, with a
sentinel guard so "never seen" is not "delta huge":

```java
// speech marks: distance on the MIC stream, in samples, converted to ms
long millisDelta = sampleFramesToMillis(startSampleIndex - mLatestSpeechMarkInputFrameCount);
return Math.abs(millisDelta) < threshold && mLatestSpeechMarkInputFrameCount >= 0;

// playback decoder: distance in WALL CLOCK from the last loopback detection
long delta = currentTimestamp - mLatestPlaybackTimeStamp;
return Math.abs(delta) < threshold && mLatestPlaybackTimeStamp >= 0;
```

Details worth keeping:

- **`Math.abs`, so the window is symmetric.** A detection slightly *before*
  the mark it belongs to still suppresses — correct, because the two clocks
  are estimates of each other.
- **`>= 0` sentinels.** Both trackers initialise to `-1`, and the guard means
  an unarmed mechanism suppresses nothing. Same discipline as the decoder's
  `-1` client properties.
- **Thresholds are per wake word**, loaded from a database cursor into
  `mPlaybackSuppressionThresholds` keyed on `WakeWordModel.sanitize(name)`,
  defaulting to 1100 ms — and **celebrity wake words get 1000 ms**
  (`WakeWordDescriptionsProvider.ModelType.CELEBRITY`), so the window is a
  per-model property, not a constant.
- **A suppressed wake word also suppresses the `STOP` that follows it:**

  ```java
  boolean suppressedStopAfterSuppressingWW = isStop(enumResult) && mPreviousInputWWSuppressed;
  ```

  `isStop` is `"STOP".equals(enumResult)`. If the device's own speech said the
  wake word, the "stop" that trails it in the same utterance must not stop
  anything — and `onInputSuppressed` sets the latch only for non-`STOP`
  results, so it is armed by a wake word and consumed by the next stop.

#### The instrument: it records the suppressions it *nearly* made

`MetricsManager` keeps a second, wider window purely to measure tuning:

```java
static final long WAKEWORD_MISSED_SUPPRESSION_THRESHOLD_DELTA = 1000;  // ms
long wakewordMissedSuppressionThreshold = wakewordSuppressionThreshold + 1000;
```

A detection that fell *outside* the 1100 ms window but inside 2100 ms is
recorded as `SPEECHMARK_SUPPRESS_FAILED` / `PLAYBACK_DETECTOR_SUPPRESS_FAILED`
with the **miss distance** (`delta - threshold`), alongside
`*_SUPPRESS_SUCCEED` with the catch distance. So the fleet continuously
reports how close the 1100 ms is to being wrong in both directions — the same
instinct as Pryon's `barely_suppressed_snr_threshold` band and its 995/1000
probabilistic let-through, implemented independently one layer up.

`PLAYBACK_DETECTOR_DETECT_WITHOUT_INPUT_DETECT` is the happiest counter in the
system: the loopback decoder heard the device say the wake word and the
microphone never reported it at all.

Enabling is two-sided, both readable at runtime:

```java
// WakeWordService
boolean enabled = policy.shouldEnableWwSpeechMarksSuppression()
               || systemPropertiesHelper.getBoolean("persist.ww.speechmarks.suppress", false);
// EnumeratedPolicy — test hook, forces marks off
private static final String DISABLE_SPEECH_MARKS = "persist.ww.speechmarks.disable";
```

Separately, the detector can simply be blinded for a period:
`WakeWordManager.suppressDetection(long)` with
`DEFAULT_SUPPRESS_TIME_MS = 500` (`classes136.dex`), passed into
`nInitialize` at startup and readable back through `getSuppressTimeMs()`. That
is the blunt lever — "do not detect for the next N ms" — beside the two
evidence-based tests above.

#### How a playback instant becomes a mic index — and why the window is 1100 ms

This is the part that decides whether the mechanism is portable, and it is
simpler and cruder than expected.

The transport is an **Android broadcast**. The TTS side's
`SpeechMarkBroadcaster` sends `com.amazon.speech.SPEECH_MARK` with the mark
JSON in `com.amazon.speech.SPEECH_MARK_STRING`; `WakeWordService` registers a
receiver for it (gated on `mShouldBroadcastSpeechMarks`) and, on each mark,
calls straight into the policy:

```java
int currentWakeWordsFound = mEnumeratedPolicy.onSpeechMark(speechMarkStr, WakeWordService.this);
if (mPolicy.supportsNtt())
    mTtsBargeInCalculator.onSpeechMark(speechMarkStr);
for (int i = 0; i < currentWakeWordsFound; i++)
    dispatchCurrentWakeWordSpeechMarkReceived();
```

`WakeWordService` **is** the `InputFrameCountProvider` — it implements the
interface and forwards to the native core:

```java
public long getInputFrameCount()    { return mWakeWordServiceCore.getInputFrameCount(); }
public long getPlaybackFrameCount() { return mWakeWordServiceCore.getPlaybackFrameCount(); }
```

So the alignment is established by **sampling the microphone's read position
at the moment the mark is delivered**. `TtsBargeInOffsetCalculator` makes that
explicit:

```java
long firstSpeechMarkMillis = SpeechMarkBundleParser.getFirstTimestamp(speechMarks);
long lastSpeechMarkMillis  = SpeechMarkBundleParser.getLastTimestamp(speechMarks);
long lastAudioPushMillis   = sampleFramesToMillis(mInputFrameCountProvider.getInputFrameCount());
mSpeechOffsetsPojo = SpeechOffsetsPojo.builder()
    .firstSpeechMarkMillis(firstSpeechMarkMillis)
    .lastSpeechMarkMillis(lastSpeechMarkMillis)
    .speechMarkStartOffsetMillis(lastAudioPushMillis - firstSpeechMarkMillis)   // ← the mapping
    .build();
```

`speechMarkStartOffsetMillis` is the constant that converts TTS time into mic
time, and it is just *(mic position now) − (first mark's timestamp)*. There is
no measurement of output latency, no clock recovery, no correlation.

**That is why the window is 1100 ms and not 100 ms.** The mark is only in the
right place if it is delivered while the word is actually leaving the speaker,
which holds because `TtsSpeechMarksEmitter` schedules each mark with
`postDelayed(mark, markTime - now)` against the instant playback started. The
1100 ms is therefore a tolerance absorbing emitter scheduling slop, broadcast
delivery jitter, and the output and capture latencies nobody measured — not an
estimate of how long the wake word takes to say.

It also states the precondition for reproducing this anywhere else: **the
marks must be emitted in real time relative to audible playback.** On a stack
that hands a whole TTS stream to a device buffering seconds ahead, "now" at
the sender is not "now" at the speaker, and this exact construction would be
wrong by the size of the buffer.

#### Barge-in is also *measured*, not just detected

The same offset runs in reverse for a genuine interruption:

```java
long startSampleMillis   = sampleFramesToMillis(startSampleIndex);
long bargeInOffsetMillis = startSampleMillis - speechOffsetsPojo.getSpeechMarkStartOffsetMillis();
if (bargeInOffsetMillis > mSpeechOffsetsPojo.getLastSpeechMarkMillis())
    Log.w(TAG, "A barge-in occurred after last known speech mark.");
```

`bargeInOffsetMillis` is **where in its own response the device had got to
when the user cut in**, in the TTS's own timeline. It returns `null` — not
zero — when no marks have been seen (`"No speech marks have been observed
yet."`), it is broadcast via `BargeInOffsetBroadcaster`, and it is carried
into the speech session as the `nttBargeInOffset` argument of
`pushAudioStream`. EchoMuse records that a barge happened and at what score;
it does not know what the user had heard.

Note the calculator's mark ingest is gated on `mPolicy.supportsNtt()`, so on
this SKU it never runs — the *suppression* path (`EnumeratedPolicy`) is not
gated and does.

Note also there are **two copies of the TTS stack** in the APK —
`amazon/speech/tts/` (`classes133.dex`) and `amazon/speech/simclient/`
(`classes62.dex`) — with the same emitter, listener and property names in
both. Which one is live is unresolved.

#### What a detection carries when it is accepted

`onEnumeratedResult` asks the policy first and counts the answer either way:

```java
boolean shouldBeSuppressed = mEnumeratedPolicy.shouldSuppressedResult(decoderId, enumResult, detectionType, startSampleIndex);
mActivityStats.incrementCounter(ActivityStats.getWakeWordDetectionCounterName(decoderId, enumResult, detectionType, shouldBeSuppressed));
if (!shouldBeSuppressed) { … }
```

— so the counter name itself is keyed on decoder, result, detection type *and*
whether it was suppressed. The verdict is also attached to the utterance's AFE
metadata as `WWS_IS_SUPPRESSED` (`onUtteranceJsonAvailable`), so downstream
consumers see it rather than inferring it.

What gets handed to the speech client is worth reading as a data model:

```java
pushAudioStream(wwDetectedId, audioSources, wakeWord,
                indexPreRoll, indexStart, indexEnd,      // sample indices, not durations
                wakeWordDetectionTime,
                metaStartByteIndex, metaBlobByteSize,    // the AFE metadata blob
                nttBargeInOffset,                        // nullable
                AudioSessionData.Initiator.WAKE_WORD);   // or NATURAL_TURN
```

Three details: the preroll is an **index** derived from a time offset
(`calculateIndexFromTimeOffset(startByteIndex, -prerollDurationMillis)`) rather
than a fixed frame count, so the wake word's own span is explicit and a
consumer can trim or keep it; the AFE metadata travels as a byte range
alongside the audio; and the initiator is an enum, with NTT results pushed
through the *same* provider as `NATURAL_TURN`.

And one line that reframes `alexa-endpointing.md`: this provider declares

```java
private static final AudioSessionData.Endpointer PROVIDER_ENPOINTER = AudioSessionData.Endpointer.Server;
```

On this SKU, wake-word-initiated audio is endpointed **server-side**. The
five endpointing modes documented in `alexa-endpointing.md` are real and in
`libpryon`, but the shipping path for a Dot Gen 2 declares endpointing to be
the server's job — which is the same division EchoMuse arrives at by letting
Home Assistant's VAD end the turn.

#### Turning the detector off is refcounted, not a boolean

`state/DetectorStateManager` is the other half of the policy, and it is about
*whether the detector runs at all*. Requests are keyed by caller token:

```java
setDetectorStateWithTokenLocked(IBinder token, int state)   // 1 = enable, 2 = disable
String caller = getPackageManager().getNameForUid(Binder.getCallingUid());
// re-enable is refused while anyone else still holds a disable:
if (1 == state && mDisableDetectorCallerMap.size() != 0)
    Log.i(TAG, String.format("(%d) other clients still disable wakeword (%s)",
          mDisableDetectorCallerMap.size(), mDisableDetectorCallerMap.keySet()));
```

So "off" is a set of holders, not a flag, and the log names which packages are
holding it. Above that sit the conditions that recompute the state
(`updateWakeWordEnabled` → `shouldEnableDetectorState`): **privacy mode**
(`ALEXA_PRIVACY_MODE_SETTING_KEY`), OOBE in progress, parental controls
(`mAlexaAvailability & 2`), keyguard/lockscreen access
(`ALEXA_HANDSFREE_LOCKSCREEN_ACCESS_KEY`, only where the device has
`FEATURE_FOS_KEYGUARD_MANAGER`), screen state and AC/USB power (tablets only,
`shouldConsiderPowerSaveMode`), device mode, and thermal mitigation
(`THERMAL_MITIGATION_TECHNIQUE_SECONDARY_WAKEWORD`). The playback detector has
its own gate for setup (`shouldSuppressPlaybackDetectorOnOobe` /
`SUPPRESS_PLAYBACK_AT_OOBE_PROPERTY`).

EchoMuse's equivalent is one device-sovereign mute plus `collect_mode` /
`ambient_mode` / `capture_mode` refusals at `_run_voice_locked` — the same
problem solved with independent booleans rather than a holder set. Worth
noting the shape, because the failure it prevents is the one EchoMuse's
capability work keeps running into: two subsystems disabling the same thing
and the first one to finish re-enabling it.

Above it, `WakeWordStateController` is the piece worth stealing conceptually:
conditions are **data**, and the decision is a policy function over the whole
set.

```java
private final Map<String, Boolean> mStateMap = mPolicy.getDefaultStateMap();
private final Binder mToken = new Binder();          // it is one holder among many

public void onConditionUpdated(String condition, boolean state) {
    if (!mStateMap.containsKey(condition)) { …warn…; return; }   // unknown condition = ignored
    if (mStateMap.get(condition).equals(state))   { …unchanged…; return; }
    mStateMap.put(condition, state);
    refreshWakeWordState();
}

private void refreshWakeWordState() {
    mDetectorStateManager.setDetectorStateLocal(mToken, mPolicy.shouldEnableWakeWord(mStateMap) ? 1 : 2);
    if (mPolicy.shouldHoldMicExclusive(mStateMap)) mService.acquireAudioSourceLocal(mToken, 1);
    else                                            mService.releaseAudioSourceLocal(mToken);
}
```

No condition knows the rule. A condition reports itself, the map is
re-evaluated whole, and the same evaluation drives a *second* decision —
whether to hold the microphone exclusively. An unrecognised condition is
logged and dropped rather than defaulting, and an unchanged value short
circuits rather than churning.

### Audio dialog focus, per namespace

```
addAudioDialogFocus        releaseAudioDialogFocus
"Releasing audio dialog focus for namespace: "
"Failure sending addAudioDialogFocus"
```

A dialog-scoped audio focus claim, keyed by **namespace** rather than by
stream. This is the same job EchoMuse does with `em_player.interrupt()` /
`resume_interrupted()` and the "voice turns OWN the speaker" rule — except
claims are named, so what took focus is visible rather than implied by
whichever code path ran.

### The turn state machine, decompiled

`amazon/speech/sim/state/` (`classes171.dex`) is the turn state machine, and
the state classes name themselves:

```
ReadyState          idle, wake word armed
ListenState         capturing the utterance
ThinkState          waiting on the cloud
ResponseReceivedState
MultiTurnRequestState   ← continuation
SconeReadyState / SconeListenState / SconeCanceledState   ← the button-initiated flow
SmartSuspendState / InertState / PendingUnlockState / IgnoreWhile{Connected,Disconnected,SmartSuspend}State
```

Each state returns a **list of commands** rather than mutating anything
(`StateTransitionCommand`, `VuiTransitionCommand`, `CancelSpeechCommand`,
`ChangeAudioFocusCommand`, `SendAlexaEventCommand`, `SettingsTransitionCommand`
…), so a transition is a value that can be inspected before it is applied.

#### Wake-word turns endpoint in the cloud; button turns endpoint on the device

This is the sharpest thing in the file, and EchoMuse arrived at it
independently. `WakeWordAudioProvider` declares:

```java
private static final AudioSessionData.Endpointer PROVIDER_ENPOINTER = AudioSessionData.Endpointer.Server;
```

while the button path, in `MultiTurnRequestState.handleEvent(SconeStartSpeechEvent)`,
asks for the opposite:

```java
new StartRecordingCommand(..., AudioSessionData.Endpointer.Device,
                               AudioSessionData.Initiator.MICROPHONE_BUTTON, ...)
```

**Device endpointing for the button, server endpointing for the wake word.**
That is exactly EchoMuse's split — the device VAD gate applies only to
`lock_mic` (button) turns and the wake stream is ungated with Home Assistant
deciding when to stop. Two independent designs landing on the same division is
worth more than either one alone.

`ListenState` accordingly has three distinct ways to end, not one:

```java
handleEvent(ServerEndpointEvent)  → EndpointAudioCommand                     // the normal path
handleEvent(LocalEndpointEvent)   → SendDirectiveSequenceEvent.Action.CANCELLED, then endpoint
handleEvent(SpeechEndEvent)       → → ThinkState + StartSpeechSessionTimeoutCommand
handleEvent(TimeoutEvent)         → cancel
```

Note a **local** endpoint is reported upstream as a *cancellation* of the
directive sequence — the cloud is told its turn was cut short, rather than
being left to infer it.

#### Continuation is a Ready state with the wake word bypassed

`MultiTurnRequestState extends ReadyState`, with `getClearState()` returning
false so the turn's context survives, and:

```java
public List<SimCommand> handleEvent(SpeechStartEvent event) {
    return super.handleSpeechStartEvent(event, false);   // ← no wake word required
}
```

Its exit is a timeout that is **reported to the cloud**, and the event chosen
is capability-versioned rather than assumed:

```java
Float version = parseFloat(capabilityContract.getInterfaceVersionImpl("SpeechRecognizer"));
return version < 1.0f ? new ListenTimeoutEvent() : new ExpectSpeechTimedOutEvent();
// NumberFormatException → ListenTimeoutEvent, the older one
```

Negotiate by capability, degrade to the older behaviour on a parse failure —
the same rule CLAUDE.md states for this repo, in Amazon's code.

On timeout it emits the event, cancels the speech session, cancels pending
voice directives, returns to `ReadyState`, clears the VUI and clears settings.
Five commands, explicit, in order.

#### Two gates on a bare "stop", and the second one is a log line worth quoting

`LocalStopEvent` in `ListenState` is gated on **audio focus**, not on what is
ringing:

```java
AlexaFocusContract focusContract = mStateMachine.getAlexaFocusContract();
if (!focusContract.containsChannel(1) || focusContract.getChannelState(1) != Channel.State.ACTIVE) { … }
…
if (allowedDirectives.length == 0) { Log.i(TAG, "LocalStopEvent found no allowed directives"); return commands; }
```

So "permission to act" — which `ring-stop-word.md` describes from the alarms
end — is implemented here as *does an active focus channel admit a directive
this stop could route to*.

And the answer to the question the configs could not settle, verbatim:

```java
public List<SimCommand> handleEvent(LocalCommandEvent event) {
    Log.i(TAG, "Dropping local command from Pryon because the device is already listening, "
             + "not a wakewordless local command");
    return Collections.emptyList();
}
```

Local commands from Pryon are **wakewordless by design** and are dropped when
a turn is already in progress. So the `op.cfg.json` `awake` gate is not the
whole story: the spotter may report a bare "stop", and the SIM decides whether
the current state is one in which a wakewordless command means anything.

#### The ASP is told when the stream ends, too

`ListenState.handleEvent(SpeechEndEvent)` adds, gated on the device actually
having an AFE:

```java
if (DeviceFeaturesHelper.hasAsp(mContext)) commands.add(new NotifyStreamEndCommand(false, mContext));
```

which is the third leg of the AFE conversation, beside
`ASP_CMD_NOTIFY_TTS_STATUS` and the `isListening` notification.

## How this compares with EchoMuse today

Descriptive. Nothing below is a proposal; it is what the two stacks actually
do, with the EchoMuse side read out of the current tree rather than from
memory.

| Question | Native stack | EchoMuse today |
|---|---|---|
| Bar during playback | `accept` 0.7 → **0.5** via an override table keyed per stream type | `owwThreshold` **0.5 → `bargeInThreshold` 0.05** (`em_db.DEFAULT_DEVICE_CONFIG`), used as-is |
| Who supplies the playback state | AFE + service push it as timestamped client properties | Controller infers it: `playback_started` event, `em_player.is_playing()`, `device.timer_ringing` |
| TTS vs music vs alarm | Four tracked stream states; three are override keys; `TTSPlayerState` is **not** one | One `barge_threshold` for all three |
| Post-detection veto | Four independent tests (ERLE span, SB-DTD, fingerprint, watermark) | None — `fired = score >= threshold` |
| Scores its own output | Second decoder on the far-end stream, tuned *more* sensitive (accept −0.95 vs −0.75) | Not done |
| Per-frame AEC telemetry | ERLE / DTD / playback status, carried in the audio LSBs | None reaches the controller; the HAL hands up one finished mono channel |
| Response that says the wake word | Discarded if within **1100 ms** of a wake-word TTS speech mark, mapped onto the mic sample index — known, not heard | Nothing; the TTS is scored like any other audio, at the 0.05 barge bar |
| Playback status to the echo canceller | `ASP_CMD_NOTIFY_TTS_STATUS` into the AFE via binder | No channel exists — the AFE is inside the HAL and takes no input from us |
| Speaker ownership | Named claims: `addAudioDialogFocus` / `releaseAudioDialogFocus` per namespace | `em_player.interrupt()` / `resume_interrupted()`, ownership implied by code path |
| Detection cadence | 10 ms frame shift, decision per frame | 1.4 s window on a 160 ms hop, score = mean of last 3 windows ≈ 1.7 s of audio (`em_wake_scorer.BcresnetScorer`) |
| Level handling | Continuous per-bin mean normalisation bridging utterances + fixed +7.2 dB; no per-window rescaling | Every window **peak-normalised to 0.8** — a loud bed divides the speech down before inference |
| Feature normalisation across turns | Continuous, deliberately bridges utterances (`continuous_norm_estimation = 1`) | `model.reset()` per turn — and on BC-ResNet that means **no score at all for a full 1.4 s** |
| Verification of a candidate | Second-stage DNN/SVM after the HMM proposes ("escalation") | One classifier; 3-window mean smoothing, plus a two-scored-frame low tier during thinking |
| Rate limiting | `lock-period 40`, `max-per-window 6` in `window-size 100` | Implicit: `voice_lock` plus `model.reset()` after a detection |
| Near-miss reporting | `notify-threshold` per model, per override (0.1), `emit_nearmiss = 1` | `nearMissThreshold` 0.05, one global value |
| In-turn "stop" | An FST inside the same keyword model, armed only in the `awake` state | Full wake word re-detection at `bargeInThreshold`; same for silencing a ring |
| Continuation gating | NTT fusion model: directedness + SSL + self-reference + dialog-act embedding | HA's `continue_conversation` re-opens the mic with no directedness test at all |
| Mute | A decoder client property, plus digital-silence frame dropping | Device-sovereign: `mic_start` refusal + hardware ADC mute |
| Deliberate measurement holes | 995/1000 probabilistic suppression, shadow mode on every suppressor | `oww_shadow` / `turns.dev_shadow` for on-device wake word — same pattern, different subject |
| Turning the detector off | A refcounted set of holder tokens; re-enable refused while any holder remains, caller package logged | Independent booleans: mute, `collect_mode`, `ambient_mode`, `capture_mode` |
| Who endpoints a turn | **Split**: `Endpointer.Server` for wake-word turns, `Endpointer.Device` for button turns | The same split — HA's VAD for wake turns (with `em_endpoint` as a ceiling), device VAD gate for `lock_mic` button turns |
| What a detection hands over | Explicit `indexPreRoll`/`indexStart`/`indexEnd`, the AFE metadata byte range, a nullable barge offset, an `Initiator` enum | A PCM stream plus `VOICE_PREROLL_DISCARD` frames dropped by the sender |

Six of those are worth more than a table row.

### 1. The bar moves 10× for us and 1.4× for them

`bargeInThreshold` defaults to **0.05** against `owwThreshold` **0.5**
(`em_db.py`), and `_barge_watcher` uses it as-is during playback —
deliberately unfloored, with the reasoning recorded in the code: self-echo
measured 0.004 converged / 0.055 unconverged, real speech over TTS 0.118+.

Amazon reaches the same problem and moves its bar by a factor of 0.71. The
difference is not that one number is braver: it is that a detection clearing
0.5 in Pryon then has to survive four suppressors that know whether the AEC
was converged over exactly the frames that scored, and an EchoMuse detection
at 0.05 survives nothing. The low bar is doing the entire job alone.

The scales are not directly comparable (a BC-ResNet softmax probability,
smoothed over three windows, against a second-stage DNN score), so
0.05-vs-0.5 is not a like-for-like ratio. What is comparable is the
*structure*: one number, versus one number plus a veto stack fed by echo
telemetry.

### 2. EchoMuse resets the context every turn; Amazon never does

`_barge_watcher` calls `model.reset()` before scoring, once per turn, and the
code already documents the price — a genuine barge attempt "over the
watcher's cold-started model can plateau below the wake threshold (observed
2026-07-12: 0.240/0.242 on consecutive frames vs threshold 0.50 — missed, and
the unwanted answer played in full)". The two-consecutive-frame low tier
exists to recover exactly those turns.

Amazon's front end is configured to do the opposite, with a comment saying so
in `pryon.config`:

```
# CONTINUOUSLY DECAYING cmn processing--bridges across all utts
fe.feat.continuous_norm_estimation = 1
fe.feat.current_frame_decay_weight = 0.003
# Buffer until we have at least min_frames data to start streaming (only affects 1st utt)
fe.feat.min_frames_norm            = 100
```

`min_frames_norm = 100` is 1 s of 10 ms frames spent settling normalisation —
and "only affects 1st utt", because the state is never thrown away. One
decoder runs forever across wake, turn, playback and the next wake.

So the depressed-score problem the low barge threshold compensates for is, at
least in part, self-inflicted: it is measured on a model that was handed a
cold context at the moment the response started playing.

### 3. Decision cadence, and the 1.4 s the reset costs

Pryon decides every 10 ms (`fe.audio_analysis.frame_shift_milli = 10`) with a
400 ms-scale refractory on top. The BC-ResNet scorer slides a **1.4 s window**
on a **2-chunk (160 ms) hop**, returns `None` between hops, and reports the
**mean of the last 3 window scores** (`SMOOTHING_WINDOWS = 3`) — so a reported
score summarises roughly 1.4 + 2×0.16 ≈ **1.7 s** of audio.

The smoothing is a real evidence-pooling mechanism and closer to Amazon's
N-of-M shape than a single frame would be. What it costs is latency: the
barge watcher's two-consecutive-scored-frame low tier therefore needs two
hops on top of a full window.

And `reset()` is not a cheap operation on this scorer. It sets `_filled = 0`
and clears the smoothing deque, and the class documents the consequence
plainly: *"a window is never scored until it has been refilled from
scratch"* — which the docstring frames as a deliberate "refractory of exactly
one window". So after `_barge_watcher` calls `model.reset()`, **no score of
any kind exists for a full 1.4 s of fresh audio**, and the first scores after
that are means over a partially-filled deque. If Home Assistant answers
quickly, playback can begin before the barge scorer has produced its first
number at all.

### 4. Two stages versus one, and what peak normalisation does under music

Amazon's precision comes from a verifier: the HMM search proposes with its
thresholds wide open (`accept 0.0`), and a second-stage DNN or SVM decides,
over a feature vector that includes an explicit log-energy block. BC-ResNet is
one classifier over a window, with 3-window mean smoothing standing in for a
verifier.

The sharper difference is what happens to the audio before inference. Every
window is **peak-normalised to `normPeak` (0.8)** before it reaches the graph —
`em_wake_scorer._score` and `device/internal/wakeword/bcresnet` do this
identically, pinned by a parity fixture, and it is the model's definition of
level invariance:

```python
peak = float(np.abs(window).max())
if peak > PEAK_EPS:
    window = window * (self._spec.norm_peak / peak)
```

**Under a music bed that is a divisor on the target speech.** The loudest
sample anywhere in the 1.4 s window sets the scale, so a bed peaking above the
speech scales the speech down by exactly that ratio before the model sees it.
That is a mechanism for depressed speech-over-playback scores which is
specific to this scorer and has nothing to do with the echo canceller.

Amazon's front end does the opposite at every point: continuously-adapting
**mean** normalisation per bin with a long decay
(`continuous_norm_estimation = 1`, `current_frame_decay_weight = 0.003`,
`do_variance_norm = 0`), bridging utterances, plus one fixed +7.2 dB — and no
per-window rescaling anywhere. A steady bed is adapted out as background
rather than becoming the denominator.

Two smaller BC-ResNet behaviours worth holding alongside: a window below
`SILENCE_RMS` (1e-4) is **not scored at all** and deliberately does not dilute
the smoothing history either side of it; and `PEAK_EPS` exists because
normalising a near-silent window would otherwise amplify it by up to ~8000×,
which is also what protects a muted device's zero-filled frames.

### 5. The far-end signal exists here — it is just not looked at

Amazon's playback detector needs a binder client and a thread
(`PlaybackMonitor`) because the render stream belongs to someone else.
EchoMuse is in a structurally *easier* position: the controller generates the
TTS PCM itself and hands it to `stream_speaker`, so the far-end signal is
already in the same process as the wake scorer, at the same sample rate,
before it is ever sent. Nothing currently scores it, and the device's own
`IsStreaming()` reports only a boolean.

The comparison Amazon makes — "was the wake word present in what I was
playing?" — therefore needs no new hardware access on this stack. What it
would need is the time alignment Amazon maintains explicitly with two frame
counters (`getInputFrameCount` / `getPlaybackFrameCount`), which is non-trivial
here: a TTS stream is sent well ahead of being heard (the device holds
`primePeriods` ≈ 1 s before starting and buffers `audioChanDepth` ≈ 5.5 s),
music runs a deliberate `LEAD_S` = 4.0 s ahead of realtime, and the HAL adds
its own buffering on top. What was *sent* at a given instant is not what was
*audible*.

### 6. Continuation is the widest gap

EchoMuse's continuation, read from `_run_voice_locked`: on HA's
`continue_conversation`, call `device.mic_start()`, drain stale frames, re-arm
the listening ring, and re-enter `trigger_voice_turn` with
`preroll_discard = 0` and `turn_label = "continuation"`. There is no test of
whether the next sound in the room was addressed to the device — the mic is
simply open, and whatever arrives becomes the next command.

It is also anchorless. `device.last_wake_db` is read and **consumed** at the
first turn (`em_esphome`, "a later button turn … must not inherit the level of
whoever last spoke"), so a continuation turn gets `asr_gain_for(None)` —
nominal gain — and an unseeded `em_endpoint.Endpointer`, which then has to
converge its own maximum from the follow-up speech itself. The same code path
that makes the *first* turn's endpointing well-placed leaves the continuation
turn to bootstrap.

Amazon has both halves. The cloud-driven one is an **`ExpectSpeech` directive**
(`amazon/speech/model/speechrecognizer/ExpectSpeechDirective`, with an
`Initiator`/`InitiatorPayload`, and `ExpectSpeechTimedOutEvent` reported back
when nobody speaks) — that is the same shape as `continue_conversation`, so
the mechanism EchoMuse has is the one Amazon has too. The difference is what
guards it: NTT, an entire separate decoder whose inputs are direction of
arrival, an acoustic directedness embedding, a self-reference test, and an
embedding of the dialog act just performed. None
of that is portable — the model is not on this SKU — but it does establish
that "open the mic and hope" is not a shortcut Amazon took and we skipped; it
is a problem they spent a model on.

### A correction to `ring-stop-word.md`

That doc states the stop model "runs continuously alongside the wake word
model" and that "what is gated is not the listening, it is the *permission to
act*". The configs show the first half is not how this SKU is built:

- `STOP` is not a separate model. `STOP.fg.hclg.pfst` / `STOP.bg.hclg.pfst`
  sit inside every keyword directory and are spotters in the same
  `keyword-orchestra`, sharing the front end, acoustic model and search.
- Its listening **is** gated, at the decoder: `op.cfg.json` only makes `STOP`
  a rule in the `awake` state, which is entered by the wake word and left by
  `STOP` or by timeout.

The permission-to-act gate is real and additional — the alarms subsystem
discarding a stop when nothing is ringing, as that doc records.

This raises a question the configs alone cannot answer: as shipped, bare
"stop" can only fire after a wake word, which is not how a stock Echo behaves
when an alarm is ringing. Something must be driving the machine into `awake`
without a wake word, and `AlarmState` being both a tracked stream state and an
override key makes the client the obvious candidate — the file is named
`op.cfg` for *override* behaviour, and NTT logs `First frame of override OP
during NTT evaluation due to audio playback`, showing the override machine is
driven by playback state elsewhere. *[INFERENCE — unconfirmed; it is a
Java-layer question.]*

## What else is in there

A survey pass over all 180 dex files, so the remaining territory is mapped
rather than guessed at. The package landscape inside `amazon.speech.sim`:

```
amazon/speech/{sim,model,ap,vui,tts,io,scl,playback,audio,
               devicecapability,conditionals,dual,requestid,
               wakeword,wakewordapi,options,nexusclient,util}
```

`sim` is the speech client, `ap` the audio-provider framework, `vui` the voice
UX layer, `scl` the speech client library, `conditionals` the condition map
`WakeWordStateController` evaluates, `devicecapability` the capability gating.

### The dialog layer — now pulled and decompiled

`classes171.dex` (1.0 MB), with `classes77`, `classes55`, `classes114`,
`classes107`, `classes154`, is a full dialog-management engine. Located by
grepping for `dialogRequestId`, `superbowltypes/directives` and
`StopCapture`; all six are now archived and decompiled, and the turn state
machine is written up under Layer 5. What the strings establish, beyond what
that section covers:

- **A per-turn state machine keyed by dialog id.** `amazon/speech/sim/state/
  SimStateMachine` with `BlockSpecificEventConfig` and
  `DeviceCapabilityBlockConfig`, driven by explicit commands
  (`StateTransitionCommand`, `VuiTransitionCommand`,
  `SettingsTransitionCommand`). The failure log is
  `Cannot find utterance state machine for dialogRequestId `, so machines are
  per-utterance and looked up by id.
- **`StopCaptureDirective`** (`com.amazon.superbowltypes.directives.
  speechrecognizer.StopCaptureDirective`) — the server telling the device to
  stop capturing, which is the other half of the `Endpointer.Server`
  declaration above. Critically it is **instrumented for not arriving**:
  `STOPCAPTURE_TIMEOUT_METRIC` and
  `UserPerceivedLatency.VoiceRequest.StopCaptureTimeout`. That is exactly
  EchoMuse's "turn ran to the 20 s cap" failure, with a metric on it.
- **`NewDialogRequest` directive** — server-initiated dialog
  (`NewDialogRequest Directive Received; eventId (%s)`).
- **Barge-in is a first-class property of a directive sequence**:
  `onDirectiveSequenceCompleted ID (%s) cancelled (%s) bargeIn (%s)`, plus
  `TRANSITION_OUT_OF_THINKING_DIRECTIVES_IGNORE_LIST` — a set of directives
  deliberately ignored while leaving the thinking state.
- **The barge offset crosses the process boundary** as a broadcast
  (`com.amazon.speech.BARGE_IN_OFFSET`, `_LONG`), is received by a
  `BargeInOffsetBroadcastReceiver`, and is attached upward as
  `ATTRIBUTE_NTT_BARGE_IN_OFFSET`. So how far into its own response the device
  got is carried into the request, not merely logged.
- **The AFE is told when the device is LISTENING too**, not only when TTS is
  playing: `Notify ASP that isListening is: `, `aspListeningNotification`,
  `shouldNotifyAspOnCommsMessage`. Together with `ASP_CMD_NOTIFY_TTS_STATUS`
  that is a two-way conversation with the echo canceller about turn state.
- Turn states are broadcast (`LISTENING_START` / `LISTENING_STOP`,
  `THINKING`, `ACTION_SPEAKING`, `STATE_IS_SPEAKING`,
  `com.amazon.speech.EXTRA_IS_SPEAKING`).

### Adaptive Listening — a named, user-facing endpointing control

```
Alexa.Accessibility.AdaptiveListening.enablement
content://com.amazon.settings.provider.settingsproxy/settings/Alexa.Accessibility.AdaptiveListening.enablement
KEY_ADAPTIVE_LISTENING_MODE   DEFAULT_ADAPTIVE_LISTENING_MODE
ATTENTION_SPAN                buildTrueSettingWithListeningReason / getListeningReason
```

An **accessibility setting that changes how long the device keeps listening**,
with `ATTENTION_SPAN` as a named quantity and a "listening reason" attached to
the setting. This is the closest thing in the stack to EchoMuse's
`endpointSilenceMs` / `maxSpeechMs`, and it is framed as accessibility rather
than tuning — worth reading before anyone designs a user-facing control for
the same thing.

### The VUI layer names follow-up explicitly

`amazon/speech/vui` (`classes154`, `classes119`, `classes149`) carries
`ISimVuiManager`, `VuiAgent`, `VuiCommand`, `SimVuiConstants`,
`IVuiCommandListener`, `VuiShortPressResult` — and among its constants,
**`FOLLOW_UP`** and **`MULTI_TURN`** beside `LISTENING`. So the two
continuation modes are distinct named states at the UX layer, not an
emergent consequence of a flag.

### Earcons are configurable, downloaded, and there are two of them

```
PROP_EARCON_WAKEWORD_CONFIRMATION_KEY    mWakeWordConfirmationEarconEnabled
PROP_EARCON_SPEECH_CONFIRMATION_KEY      mSpeechConfirmationEarconEnabled
shouldEnableSpeechConfirmationEarcon     com/amazon/uxcontroller/Earcon
DAVSEarconDownloadCompleteReceiver       /data/securedStorageLocation/earcons
EARCON_WAKEWORD_DOWNLOAD_RETRY_COUNT     earconConfigString
```

Two independently-gated cues: **wake word confirmation** (the chime EchoMuse
has as `wakeSound`) and **speech confirmation** — an end-of-speech cue
EchoMuse does not have. Both are DAVS artefacts with a config blob and a
retry count, not compiled in. Note `aedEarcon` as well: acoustic event
detection has its own.

### Local voice control has its own state machine

`amazon/speech/sim/scl/hybridproxy/` — `HPStateMachine`,
`HPUtteranceState`, `HPCommandStateTransition`, `StateTransition`. A parallel
utterance state machine for the offline path, which is the same
`LocalCommand` territory `ring-stop-word.md` covers from the other end.

### Adjacent APKs, untouched

`EchoAudioService` (`com.amazon.device.echoaudioservice`),
`com.amazon.mediaplayeragent`,
`com.amazon.alexa.externalmediaplayer.fireos`, `com.amazon.echo.csm.oobe`,
`com.amazon.headlessbeacon`, `amazon.speech.davs.davcservice`. The first three
are where media playback and ducking policy would live.

### The `libpryon.so` skeleton — recovered without transferring it

21.6 MB never left the device. `.dynstr` and the RTTI type names are string
tables, so `strings` on the device recovers the structure; only the extracted
text (45 KB gzipped) was pulled. It lives in
`~/.local/share/echomuse-native/biscuit-2022-11-18/skel/` as `rtti.txt`
(3204 type names → **1064 distinct `pryon::` classes** after `c++filt -t`),
`src.txt` (536 embedded source paths), `api.txt` (205 `PryonApi_*` /
`PryonDecoder_*` symbols), `params.txt` (307 config parameters) and
`exports.txt`.

**The embedded source paths are the skeleton.** File counts per directory:

```
decoder 75   config 48   signalprocessing 34   core 34   spotter 29
util 28      scorer 26   utterance_detection 24   pryon_e2e_speakerid 18
adaptation 16   results 15   fstext 15   precompute_lookahead_utils 11
e2e_decoder 10   pryon_whisper 9   speaker_id 8   metrics 8   confidence 8
audio 7   sm_store 6   pryon_ntt 6   external 6   pryon_models 5
feproc_framework 5   e2e_encoder 5   decoder_factory 5   fingerprinter 4
input_context 3   system 2   svm 2   serialization 2   dnn_runtime 2
smc_model 1   sm_commons 1   reflection 1   oov_handling 1   inference 1
fl_toolkit_interface 1   file_caching 1   dynamic_pruning 1   dynamic_models 1   api 1
```

**And the `Processor<In,Out>` template instantiations give the dataflow**,
because each one names an edge:

```
AudioPayload  → AudioPayload          audio-domain stages (LSB metadata decode)
AudioPayload  → FramePayload          front end / featurisation
FramePayload  → FramePayload          feature-domain stages (CMN, stacking)
FramePayload  → KeywordPayload        keyword spotter      → DetectionEvent → ResultPayload
FramePayload  → NttResultPayload      NTT fusion
FramePayload  → acr::FingerprintPayload   ACR (automatic content recognition)
FramePayload  → DeviceAedResultPayload    acoustic event detection
FramePayload  → LargeVocabResultPayload   large-vocabulary ASR
Stage1Result  → LargeVocabResultPayload   two-stage lattice ASR
e2e_decoder::RnntDecoderResultPayload → LargeVocabResultPayload
```

So one library, one front end, and **seven parallel consumers of the same
feature stream**: keyword spotting, AED, presence, NTT, content
fingerprinting, lattice ASR and an end-to-end RNN-T decoder
(`joint_network.cpp`, `prediction_network.cpp`, `subword_transducer.h`,
`rnnt_encoder_time_reduction_layer.cpp`, `ep_promoter.cpp`, `nnlu_processor.cpp`).

Two things in that list change conclusions elsewhere in this repo:

- **`alexa-endpointing.md` says the strong (search-based) endpointer "needs a
  decoder we do not have".** The decoder is *in the library* — 75 files under
  `decoder/`, plus `fstext/`, `e2e_decoder/` and `decoder_factory/`. What is
  absent is the **models**, which are DAVS downloads and not on this SKU. The
  obstacle is artefacts and licensing, not capability.
- **Speaker ID is implemented, in the exact shape that doc proposes as its
  tractable future work.** `pryon_e2e_speakerid/` (18 files) plus
  `speaker_id/` and `adaptation/speaker_id_model.cpp`, with config providers
  for **text-independent**, **wakeword-based** and a **fusion** of the two
  (`e2e_speakerid_fusion_config.cpp`,
  `e2e_speakerid_text_independent_scorer_config.cpp`,
  `e2e_speakerid_wakeword_split_model_config.cpp`). That is the
  speaker-similarity gate, already designed upstream.

Also present and previously unrecorded: `pryon_whisper/` (9 files —
`whisper_detector`, `whisper_decider`, `whisper_confidence_config`), i.e.
**whispered-speech detection**, which is Alexa's Whisper Mode; `adaptation/`;
`confidence/`; `oov_handling/`; `dynamic_pruning/`; and
`fl_toolkit_interface/` for federated learning.

### What is still not worth doing

- **Full disassembly of `libpryon.so`.** The skeleton above cost one `strings`
  pass; recovering actual algorithms from stripped, boost-heavy ARM C++ is a
  different order of effort, and the parameter names carry embedded
  documentation that usually answers the question first.
  `arm-objdump` exists (`~/.local/bin/arm-objdump`, NDK llvm-objdump in the
  compiler image) if one specific function ever needs it; host binutils
  cannot read these ELFs.
- **The models.** `dnn_quantized.mlp`, `finalQuant.mlp`, `*.psvm`, the HCLG
  FSTs — binary weights in Pryon's own formats, unloadable by anything we
  run, and not redistributable. `phones.txt` and `words.shrunk.txt` are text
  if the keyword vocabulary is ever interesting.

## Timers and alarms

EchoMuse does not own timers — Home Assistant runs the countdown and pushes
`FINISHED` through the controller, which is why `em_sounds.py` synthesises the
ring locally (see the wake-chime section for why that call differs from the
wake cue). The stock stack owns the whole lifecycle, and its wire model is in
`classes55.dex`.

### The data model

`com.amazon.superbowltypes.directives.alerts.SetAlertDirective`, namespace
`Alerts`, name `SetAlert`:

```java
public enum AlertType { ALARM, TIMER, REMINDER }

SetAlertDirective(String token,            // identity; DeleteAlert takes the same
                  AlertType type,
                  String scheduledTime,    // when it fires
                  String earcon,
                  AlertsAsset[] assets,    // {assetId, url} — downloadable audio
                  String shortAlertAsset,
                  int loopCount,
                  int loopPauseInMilliSeconds,
                  String originalTime,     // distinct from scheduledTime
                  String label)            // user-facing name
```

Four things are worth reading off that signature:

- **One type, three behaviours.** `ALARM`, `TIMER` and `REMINDER` share the
  entire directive; they differ only by enum value. Whatever distinguishes
  them is policy in the runtime, not structure on the wire.
- **The ring is explicitly a loop with a gap**, `loopCount` ×
  `loopPauseInMilliSeconds`. EchoMuse has the same shape — a burst, a quiet
  window, repeat — and the repo already documents that the quiet gap must stay
  long enough for the 1.4 s scorer to produce a result, or the alarm cannot be
  stopped by voice.
- **Two assets, one short.** `assets[]` carries `{assetId, url}` pairs and
  `shortAlertAsset` is separate — so there is a full ring and an abbreviated
  cue, and the audio is *referenced by URL*, i.e. downloaded rather than
  shipped. A timer sound can therefore be changed server-side per alert.
- **`originalTime` beside `scheduledTime`.** The alert remembers what it was
  originally set for as well as when it will next fire — the field you need
  for snooze and for a timer that was paused or extended.

Alerts carry their own volume controls as separate directives — `SetVolume`,
`AdjustVolume`, `SetMute`, with a `VolumeChangedEvent` back — so alert loudness
is a channel of its own, not the device volume.

### The event vocabulary

```
SetAlertSucceeded / SetAlertFailed        DeleteAlertSucceeded / DeleteAlertFailed
DeleteAlertsSucceeded / DeleteAlertsFailed
AlertStarted / AlertStopped
AlertEnteredForeground / AlertEnteredBackground
```

Every mutation is acknowledged in both directions, and — the interesting pair
— an alert can be in the **foreground or background** while ringing. That is
the same audio-focus notion `ListenState` gates a bare "stop" on: a ringing
alert that has been pushed to the background by a voice turn is still ringing,
but is no longer the thing a "stop" would address.

### How this compares with EchoMuse's ring

The *cadence* model is the same shape, arrived at independently:

| | Stock | EchoMuse |
|---|---|---|
| Burst length | `shortAlertAsset` vs full `assets[]`; bundled fallback | `timerRingBurstSeconds` (a short sound repeats to fill it) |
| Gap between bursts | directive `loopPauseInMilliSeconds`; 10 s for the bundled fallback | `timerRingGapSeconds` |
| Repeat limit | directive `loopCount`; hard 60 min safety cap | `timerRingSeconds` (a wall-clock cap) |
| Sound | asset URL/cache plus bundled fallbacks | `timerSound`, an `em_sounds` id stored controller-side |
| Alert volume | its own `SetVolume`/`AdjustVolume`/`SetMute` channel + 20 s ascending ramp | device volume |

The **ownership** model is where they diverge, and not by choice. A stock
alert has a `token`, acknowledged create and delete, `AlertStarted` /
`AlertStopped`, and foreground/background states — a full lifecycle the cloud
can address at any point. EchoMuse's ring has none of that because there is
nothing left to address: Home Assistant's `TimerManager._timer_finished` pops
the timer from its registry *as* it fires, so by the time `FINISHED` reaches
the controller the timer no longer exists anywhere in HA. Nothing upstream can
cancel it and no "stop the timer" utterance can route back.

That single fact explains the design `_run_timer_ring` ended up with, and it
is worth stating next to the stock model because it makes the differences
deliberate rather than deficient:

- **`timerRingSeconds` is a safety cap, not a preference.** With no external
  authority able to stop the ring, a device in an empty house would otherwise
  ring until power-cycled. Stock has `loopCount` for cadence *and* a cloud
  that can still send `DeleteAlert`.
- **Stopping is entirely local** — wake word, action button or mute button —
  which is why the mic stays streaming for the whole ring and why the gap
  between bursts doubles as the listening window. Stock can stop a ring from
  the cloud, so its gap is free to be pure cadence.
- **The foreground/background distinction has no equivalent here.** A stock
  alert that keeps ringing while a voice turn takes focus is a state the
  system tracks; EchoMuse's ring holds `voice_lock` outright, which is why
  `em_button.TAP_RING_STOP` has to call `stop_timer_ring` rather than the
  turn-cancel path — cancelling the "turn" would silence the burst while
  leaving the ring holding the lock.

### The runtime is on the device — in HeadlessBeacon, not SIM

The earlier conclusion here was **wrong**. It searched the 180 dex files in
`SpeechInteractionManager`, found only wire types, and stopped. The actual
runtime is the separate, installed
`/system/priv-app/com.amazon.headlessbeacon/com.amazon.headlessbeacon.apk`
(80 dex files). `RegularUberButtonHandler` in SIM names it:

```java
private static final String ALARM_APP_PKG = "com.amazon.headlessbeacon";
if (amazonAudioManager.getPackageInFocus().equals(ALARM_APP_PKG))
    context.sendBroadcast(new Intent("amazon.alexa.alerts.ACTION_ALARM_BUTTON_STOP"));
else
    startSpeechRecognition();
```

That is the physical-button half of “stop is gated on active audio focus”:
while the alarm app owns focus, a dot press stops an alarm rather than starting
a voice turn.

| Job | Stock implementation |
|---|---|
| Schedule | `amazon.alexa.alerts.engine.AlarmsEngine` |
| Persist | `TimedEventsDb` / `AlarmDBHelper`, `/data/data/com.amazon.headlessbeacon/databases/alarms.db` |
| Restore after reboot | `BootCompleteReceiver` → `AlarmService.onBootComplete()` → `restoreAndroidAlarmsFromDB()` |
| Generic ringer | `RingingService` |
| Headless-Dot ringer | `com.amazon.headlessbeacon.service.HeadlessRingingService` |
| Alert policy | `AlertsStateMachine` |
| Local stop/snooze | `LocalCommandReceiver` |
| Asset cache | `FileStorageManager` |

**Scheduling is stronger than ordinary exact alarms.** `AlarmsEngine` uses
`AlarmManager.setAlarmClock(new AlarmClockInfo(triggerTime, showIntent), …)`,
not `setExact` / `setExactAndAllowWhileIdle`; `AlarmClock` is exempt from
Android Doze/App-Standby deferral. It also arms a **pre-intent 30 s early**
(`WAKEUP_PRE_MILLIS = 30_000`) to pre-warm the ringing path. Recurring alarms
are computed from `originalTime` and `RecurrencePattern` in local wall-clock
time, then re-armed, so DST does not slide a 07:00 alarm.

`alarms.db` is real and contains eight tables: `timed_events`, `assets`,
`recurring_alarms`, `offline_support_timed_event`, `next_instance`,
`retained_timed_events`, `notification_event_cache` and Android metadata. The
primary row holds the directive fields plus state (`isRinging`,
`instanceState`, `localTimedEventState`), recurrence, the short asset, loop
parameters, next-instance identity and music-alarm metadata. On this device it
is a valid **zero-row** 61,440 B database — correct for a Dot never signed
into Alexa — not evidence that the runtime is absent.

On boot, `restoreAndroidAlarmsFromDB()` re-arms future entries. A persisted
ring less than **30 min** old (`RING_AFTER_REBOOT_WINDOW`) is handled rather
than blindly replayed; stale duplicates are dropped using the event's
`createdDate` against the reconstructed boot wall clock, and expired
non-recurring entries are deleted with a `TriggerTimeInThePast` reliability
metric.

The headless ringer is a foreground service with an `AlertsWakeLock`, a queue
for simultaneous alerts (they serialize; they do not overlap), a **60 min**
hard cap (`ALERT_RINGING_TIMEOUT` /
`CANCEL_RINGING_EVENT_DELAY_MINUTE`), LED animations
`ready-{alarm,timer}{,-short}`, and a default-earcon fallback on playback
failure. It registers the exact `ACTION_ALARM_BUTTON_STOP` broadcast above.
`HeadlessRingingService` is the path actually wired for this Dot — it refers
to installed `com.amazon.mediaplayeragent`; generic `RingingService` instead
hardcodes the absent `com.amazon.bishop`.

The full runtime, audio focus, loop, ramp, recovery and assets are documented
in [alexa-alerts-and-leds.md](alexa-alerts-and-leds.md).

## Could we use any of this code directly?

The tempting shapes are: load our own ONNX model into Pryon, or take Amazon's
on-device ASR and intercept its text output. Both are **no**, for concrete
reasons rather than caution. What is left over is narrower and more useful.

### Plugging our ONNX model into Pryon — no

Two independent blockers.

**There is no ONNX loader.** `libpryon.so` contains no `onnxruntime`, no
`OrtGetApiBase`, no `OrtApi` — the probe returns nothing. The model runtime is
Amazon's own, visible in the embedded source paths:

```
pryon/cpp/dnn_runtime/dnnrt_cache.cpp        pryon/cpp/scorer/dnnrt-scorer.cpp
pryon/cpp/dnn_runtime/dnnrt_disk_source.cpp  pryon/cpp/scorer/kaldi-mlp-scorer.cpp
src/nemort/kaldi/dnnrt/components/{dnnrt-affine-transform,dnnrt-batch-norm,
  dnnrt-conv2d,dnnrt-fullyconnected_recurrent,dnnrt-length-norm,
  dnnrt-lstm-dyn,dnnrt-lstmp}.{h,cc}
```

"NemoRT" — a Kaldi-derived runtime with its own component set — plus a
`kaldi-mlp` scorer. The shipped models match: `dnn_quantized.mlp`,
`finalQuant.mlp`, `*.psvm`. So the two accepted formats are undocumented
Amazon ones and our `.onnx` is neither.

**And the interface is the wrong shape anyway.** Pryon's scorer produces
**frame-level acoustic posteriors** at a 10 ms shift, consumed by an HMM search
over compiled HCLG FSTs (`search.decoder_type = "kaldi-key-phrase"`,
`beam 25.0`), with the classifier threshold applied to a *second-stage*
verifier. BC-ResNet produces one class probability per **1.4 s window**. There
is no socket in this pipeline shaped like that: substituting it would mean
replacing the front end, the search and the spotter — i.e. everything except
the parts we would be borrowing.

Converting the other way (our model → NemoRT) would need the format, the
quantisation and a matching phone set, none of which is documented or
redistributable.

### Intercepting the STT output — no models to intercept

The on-device recognisers genuinely exist in the library: 75 files under
`decoder/`, `e2e_decoder/` with an RNN-T (`joint_network.cpp`,
`prediction_network.cpp`, `subword_transducer.h`), `fstext/`,
`large_vocab_asr*` pipeline builders. But **the models are not on this SKU**.
`/system/local/models/` contains only `AED/`, `keyword/` and
`applicable_wakewords.json`; there is no `NTT/`, no large-vocabulary
directory, and `/data/local/models` does not exist. Local Voice Control models
are DAVS artefacts, fetched per-device after account registration
(`NttModelManager`'s `ClientProcessTemplateV2`, `checksum.txt`,
`engineCompatibilityIdList`). An unregistered, Alexa-disabled Dot never
downloaded them.

So the obstacle is artefacts and licensing, not capability — which is worth
stating precisely, because it also corrects `alexa-endpointing.md`'s "needs a
decoder we do not have". We have the decoder. We have no acoustic or language
model to put in it, and no lawful way to obtain one.

### What *is* all present, and why it is still a poor fit

One path has every artefact on the device already: run **Pryon with Amazon's
own keyword models**. `libwakewordserver_jni.so` exports 97 `amazon::` symbols
as `T`/`W`, so `dlopen` + `dlsym` is mechanically possible, and
`/system/local/models/keyword/<locale>/{ALEXA,AMAZON,ECHO,COMPUTER}/` ships
the models, configs and FSTs.

It is still the wrong trade:

- **It only knows Amazon's wake words.** EchoMuse's entire point is a custom
  one. Adding a keyword means compiling new HCLG FSTs against Amazon's phone
  set with Kaldi tooling *and* training a matching second-stage verifier —
  a research project, and the result would still be built on their lexicon.
- **Licensing.** The models and the engine are Amazon's. Reading them to
  understand the design is one thing; shipping a product that loads them is
  another, and EchoMuse distributes firmware.
- **It would re-acquire the coupling we deliberately removed.** The engine
  wants the AFE's client properties, the loopback stream and the metadata
  channel to work well; without those it is a strictly worse BC-ResNet.

`injectWaveFile(const char*, bool)` on `WakeWordService` is worth noting for a
different reason: it is an offline audio-injection entry point, i.e. the stock
stack's own test harness. If anyone ever wants to A/B our detector against
Amazon's on identical audio **on this hardware**, that is the hook, and it is
a measurement exercise rather than a shipping dependency.

### The genuinely reusable thing is not code, it is a channel — and it is closed

We already use the AFE — that is what the `VOICE_RECOGNITION` capture
path is (OpenSL ES on Fire OS 5 then; the mixer's `micAsr` on Fire OS 6 now). What we do not have is the **metadata** it produces: the per-frame
ERLE flag, the sub-base double-talk flag and the playback status that every
suppression decision in Pryon is built on. That is the one piece whose absence
changes EchoMuse's behaviour today, and unlike a model it needs no licence —
only a way to ask for it.

**Surveyed, and the answer is no.** A non-Amazon process cannot obtain a
continuous per-frame ERLE / double-talk / playback-status feed from the ASP
through any interface reachable on this device. The detail matters, though,
because two *other* things turned out to be reachable.

#### The interface, and why symbol names tell you nothing

The service is live **while EchoMuse is running and holding the microphone**:

```
84  audiosignalprocessor: [com.amazon.asp.IAudioSignalProcessor]
```

`libaspclient.so` has 134 dynamic symbols, of which all 43 exported are Binder
and RTTI boilerplate for two AIDL interfaces — `asp::IAudioSignalProcessor`
(a `BpAudioSignalProcessor` proxy) and `asp::IAudioEventListener` (with
`BnAudioEventListener::onTransact`, a callback the ASP pushes into). There are
no per-command function bodies to find: the whole surface is
`command(int cmdCode, byte[] in, byte[] out)`, a generic integer dispatch.

The Java client, `com.amazon.asp.AudioSignalProcessor` from
`/system/framework/aspclient.jar`, gives the real list:

```java
command(int, byte[], byte[])          registerListener/unregisterListener(IAudioEventListener)
setActiveInputSource(int, int, byte[]) startCapture(int, fdIn, fdOut, fdRef[, fdAecOut])
startInjection(...) / stopInjection(int) / stopCapture(int)
inVoiceMessaging(boolean)             startIrCodeDetection(boolean)
```

Almost every named `cmdCode` is one-way client→ASP: `ASP_CMD_NOTIFY_TTS_STATUS
= 1`, `ASP_CMD_NOTIFY_MUTED = 4`, `ASP_CMD_NOTIFY_VOICE_MESSAGING_MODE = 12`
(the real `isListening` notify), `ASP_CMD_NOTIFY_ASR_STREAM_STOPPED = 14` /
`_ERROR = 15`. Only four are GETs — `GET_AFE_DEBUG_INFO = 109`,
`GET_AUDIO_FORMAT = 91`, `GET_AUDIO_NORMALIZATION_CONFIG = 65`,
`GET_WHA_LOCAL_INFO = 26`.

#### What the diagnostic command *could* return

`GenericAFE::getAFEDiagnosticsData` in `libasp.so` can emit, behind
`ASP_CMD_GET_AFE_DEBUG_INFO = 109`:

```
AFEDiagAverageERLE          AFEDiagAECDivergenceCount   AFEDiagBeamChangeCount
AFEDiagSNRatWW              AFEDiagRoomDetection(Confidence)
AFEDiagMusicState / TTSState / AlarmState                AFEDiagSpeakerVolume
AFEDiagSelectedBeam*        AFEDiagMicCompensation*
```

That is a real instrument list — `SNRatWW`, average ERLE, AEC divergence
count, beam-change count — and EchoMuse currently has none of it. But three
caveats, in descending order of seriousness: it is **averaged, never
per-frame** (`libasp.so` logs "Each number represents average ERLE number of
%d frames(4ms)"); it is **size-capped** ("AFE debug info is too long"); and on
this device, idle, it populated **only** the playback-side leveller:

```json
{"AutomaticVolumeLeveling":{"Enabled":false,"ContentType":0,"InTTS":false,
 "InAlarm":false,"VolIndex":0,"DeviceIndex":0,"Volume dB":0,"GainOffset dB":0}}
```

No gating flag or property was found by string search, so whether the capture
side populates during an active session is **undetermined** — it needs either
a live Alexa voice session or disassembly of the native handler.

Worth noting independently: `InTTS` and `InAlarm` are exactly the flags
`ASP_CMD_NOTIFY_TTS_STATUS` sets. Nothing tells our ASP that we are speaking,
so they are permanently false — **the AFE is running without the playback-state
input its own tuning expects.**

#### Two things that *are* reachable, and one correction

- **`ASP_CMD_REQUEST_ARBITRATION_JSON = 23`** returns `{"voiceEnergy": …}`
  into a 256-byte buffer, fired once per wake-word recognition by
  `DeviceArbitrationHelper.doDeviceArbitration`. That is a single snapshot at
  wake time, not telemetry — but it is precisely the quantity `em_arbiter`
  currently has no acoustic substitute for, computed by the AFE that holds the
  reference signal.
- **The push callback carries beam direction.** `IAudioEventListener.onEvent
  (int what, byte[] data)` handles exactly two codes: `EVENT_BEAM_DIRECTION =
  1` and `EVENT_LINE_INPUT_DETECTION = 10`. **This corrects a claim in
  CLAUDE.md** — the LED direction overlay was removed on the basis that "the
  HAL selects its beam internally and reports nothing per frame". It reports
  nothing per *frame*, but it does emit a beam-direction *event*, and there is
  a registered listener interface for it. Whether an unprivileged process may
  register is untested.
- Nothing about echo, AEC or playback state exists in that callback at all.

#### The two per-frame paths, and why neither helps

**`startCapture` writes frame-synchronous metadata — but not the useful
fields.** A capture session emits auxiliary WAV channels beside the audio:

```
meta_numRefChannels.wav  meta_playbackDevice.wav  meta_playbackVolume.wav
meta_inPlayback.wav      meta_inTTS.wav           meta_inAlarm.wav
meta_inMicMuted.wav      meta_inVOIP.wav          meta_inVoiceMessaging.wav
meta_inVolumeAttenuated.wav
```

Genuine per-frame playback status — and **no `meta_erle.wav`, no
`meta_dtd.wav`**. It is also permission-gated: `libasp.so` contains "Caller
has permission. ASP is starting capturing", proving a native check runs before
`startCapture`/`startInjection` are honoured. Which UIDs pass is undetermined
without disassembling the native `onTransact`.

**And the PCM-LSB channel is narrower than its name suggests.**
`"defaultEncodingState" : false` sits at `AFE.cfg:367`, *inside the*
`False WW Prevention` *block*, beside the per-volume double/single-talk
thresholds. It toggles whether the SB-DTD suppressor's per-frame
single/double-talk decision is stuffed into the mic PCM's low bits for Pryon's
`AudioMetadataDecoder` to read. It is **not** a general ERLE/playback
telemetry channel, it is off by default, and its consumer is Pryon's own
decoder rather than any public getter.

One caveat on all of the above: the Java `IAudioSignalProcessor.Stub.onTransact`
performs **no** permission or UID check before dispatching `command()` or
`registerListener()`. Any gate on those lives in the native `onTransact` and
could not be determined from strings. `dumpsys` worked as root, which proves
nothing about an unprivileged caller — `dump()` is a different code path from
`transact()`.

## Open questions

- **Most of the Java layer is still unread** — 171 of 180 dex files; nine are
  archived and decompiled. What is now known: the suppression decision, the
  speech-mark ingest, the ASP notifications, and the turn state machine. What
  is not: who calls `suppressDetection(500)` and on what
  event, which component sets the audio-focus channel for a ringing alert,
  and which dialog act is handed to NTT. `EchoAudioService`,
  `com.amazon.headlessbeacon` and `amazon.speech.davs.davcservice` are
  adjacent APKs, unexamined.
- **Which TTS stack is live**, `amazon/speech/tts/` or
  `amazon/speech/simclient/` — both carry the same emitter, listener and
  property names.
- **Whether `suppressDetection(500)` is used at all on this SKU.** The API and
  the constant are in `WakeWordManager` (`classes136.dex`) and the constant is
  passed to `nInitialize`, but no caller was found in the three dex files
  read. It may be a tablet/Fire TV path, or it may be driven from an adjacent
  APK.
- ~~**How bare "stop" is armed for a ringing alarm.**~~ **Answered** by
  `ListenState`: local commands from Pryon are wakewordless by design and are
  dropped only when a turn is already in progress ("*not a wakewordless local
  command*"), and `LocalStopEvent` is gated on an active audio-focus channel
  admitting a directive. The `op.cfg.json` `awake` state is one gate, not the
  whole story. What remains unread is which component sets the focus channel
  for a ringing alert.
- **Units of the spotter machine periods** (`duration 175`, `lock-period 40`,
  `escalation-period 500`) are inferred from the 10 ms frame shift, not stated.
- **Whether anything reaches us in the LSBs today.** Now narrower than it
  looked: the encoding is scoped to the SB-DTD single/double-talk decision,
  not ERLE, and is off by default. Still measurable — look for structure in
  bit 0 of captured PCM with playback active — but the ceiling on what it
  could carry is lower than assumed.
- **Does `ASP_CMD_GET_AFE_DEBUG_INFO = 109` populate its capture-side keys
  during an active session?** `AFEDiagAverageERLE`, `AFEDiagSNRatWW`,
  `AFEDiagAECDivergenceCount` and `AFEDiagBeamChangeCount` exist in
  `libasp.so`; on an idle device only `AutomaticVolumeLeveling` came back. No
  gating property was found by string search. Averaged rather than per-frame
  either way, but it would still be more AFE telemetry than EchoMuse has.
- **Can an unprivileged process register an `IAudioEventListener`?** The Java
  `Stub.onTransact` applies no permission check; a native one may exist. If it
  can, `EVENT_BEAM_DIRECTION = 1` is reachable — and that **corrects
  CLAUDE.md's basis for deleting the LED direction overlay**, which assumed
  the HAL reports nothing about beam selection.
- **Is `ASP_CMD_REQUEST_ARBITRATION_JSON = 23` callable by us?** It returns
  `{"voiceEnergy": …}` at wake time, which is the acoustic quantity
  `em_arbiter` currently lacks.
- **Which UIDs pass the native `startCapture` permission check?** That path
  carries genuine per-frame playback metadata (`meta_inTTS.wav`,
  `meta_inPlayback.wav`, `meta_playbackVolume.wav` …) though not ERLE or DTD.
- **ERLE on this hardware has never been measured.** CLAUDE.md already flags
  this: `device/tools/afe_probe` existed (removed with Fire OS 5 support) and was never run, and every
  barge-in figure in the repo predates the move to the native AFE.
- **`ASPState`** is a tracked stream state whose meaning is unclear — the AFE
  signalling its own activity is the obvious reading, but unconfirmed.
