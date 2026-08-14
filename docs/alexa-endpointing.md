# How Alexa decides you have stopped talking

Reverse-engineering notes on end-of-utterance detection in the stock Echo Dot
Gen 2 stack, and what EchoMuse can take from it — written against the specific
failure of a turn that never ends because a television is talking in the
background.

Investigated 2026-08-13 against the fielded Dot (FireOS image 2022-11-18),
`/system/lib/libpryon.so` (21.6 MB) and `/system/lib/libasp.so`.

Companion to [alexa-afe.md](alexa-afe.md), which covers the audio front end.
That document ends where this one starts: the AFE hands one cleaned channel to
the decoder, and the decoder decides when the utterance is over.

**Nothing from Amazon is vendored into this repo.** Extract from your own
rooted hardware, as with the AFE notes.

## Summary

**Alexa does not endpoint with a voice activity detector.** A VAD is one of
five available modes and it is the weakest one. The mode that ships is
decoder-driven: the ASR search itself reports whether its current best
hypothesis has settled into non-speech, and in the strongest mode the language
model predicts *how long a pause is expected at this point in the sentence*
before deciding a pause is a stop.

That is why an Echo endpoints correctly with a TV on and EchoMuse does not.
The TV is speech. Any detector whose question is "is there speech?" answers
yes forever. Amazon's question is "has **this utterance** finished?", which
background speech cannot answer in the affirmative.

The second half of the answer is that everything is **relative and
range-normalised**, never an absolute threshold. Even Amazon's fallback energy
VAD tracks a running maximum *and* minimum energy and sets its thresholds as
fractions of the observed range. EchoMuse has no maximum tracker anywhere.

## Where it lives

| Path | What |
|---|---|
| `/system/lib/libpryon.so` | ASR decoder + wake word. Contains the whole utterance-detection framework |
| `/system/lib/libasp.so` | AFE. Runs a VAD of its own, upstream (see alexa-afe.md) |

Source paths survive in the binary and map the subsystem out:

```
pryon/cpp/utterance_detection/uttdet_endpoint_features_calculator.cpp
pryon/cpp/utterance_detection/uttdet_vad_feature_extractor.cpp
pryon/cpp/utterance_detection/contextual_endpoint_decision.cpp
pryon/cpp/utterance_detection/eos_vad_voting_queue.cpp
pryon/cpp/utterance_detection/speculative_detection_logic.cpp
pryon/cpp/core/ep_backporch_estimator.h
pryon/cpp/config/utterance_detection_spec.h
pryon/cpp/config/elastic_uttdet_logic_config_provider.cpp
pryon/cpp/config/eos_estimation_config.cpp
```

**No tuned numbers were recovered, and none exist on this device.** What
transfers is the **design**, not a parameter set. Every parameter description
quoted below is verbatim from the binary's own help text.

That is not a case of "the model is here but the weights are missing". It is
three separate absences, each independently sufficient:

**1. `libpryon.so` carries no weights at all.** Its 21.6 MB is code:

```
.text     15,250,496      .rodata   2,585,984
.data          5,352      .bss        175,296
```

15 MB of ARM `.text` is a Kaldi-derived WFST decoder, lattice rescoring, a DNN
runtime, FST composition, boost and a regex engine compiled in — not a model.
Every model is loaded from a path at runtime
(`search.model_filepath`, `search.endpoint.model_filepath`,
`recognizer.uttdet.json_filename`, `ep_dnn_path`).

**2. There are no model files anywhere on the device.** No `.fst`, `.mdl`,
`.nnet` or `.pryon` under `/system` or `/vendor`; no file over 500 KB anywhere
under `/data`; and `SpeechInteractionManager.apk` ships
`res/raw/applicable_models.json` containing exactly:

```json
{ "configurations": { "wakewordModels": [ ] } }
```

Empty. Even the *wake word* model — the one thing this device certainly ran
locally — is not in the image. It arrives through DAVS after account sign-in,
which is why a debloated unit has nothing.

**3. This device would not run the endpointer even with the weights.** The
only consumer of `libpryon` here is `amazon::WakeWordService`, and the symbols
it imports are the **spotter** surface, not a recognizer:

```
PryonDecoder_NewSpotterAudioDecoder     PryonModelSet_New
PryonDecoder_NewMultichannelAudioDecoder    PryonApi_SetEnumeratedResultCallback
PryonDecoder_NewPcmInt16                PryonApi_SetPresenceDetectionResultCallback
setNewPryonModel(const char*, const char**, int)
```

Wake word, acoustic event detection and presence. There is no transcript API
and no endpointer configuration surface. Nothing on the device — not the JNI,
not any APK — sets a single `recognizer.*`, `search.*` or `fe.*` parameter.
DAVS's artifact types on this build are `ARTIFACT_TYPE_WAKEWORD`,
`ARTIFACT_KEY_CID_DATABASE` and `ARTIFACT_KEY_PERSONALIZED_AED` — no ASR,
language model or endpointer artifact exists to fetch. And `AFE.cfg` says it
outright: *"Device side bias factors always set to 1.0f. Bias factors are
applied on the cloud."*

So a Dot Gen 2 spots the wake word locally and endpoints **in the cloud**. The
endpointing machinery is in `libpryon` because `libpryon` is Amazon's one
speech library, built once and linked everywhere including server-side; on
this hardware it is dead code.

The practical consequence: there is no version of "just run Amazon's
endpointer locally". Not the weights, not the models, not the API, not the
architecture. Everything below is a design to learn from, not an asset to
recover.

### DAVS, and why the tuning is not on the device

`amazon.speech.davs.davcservice` (`/system/priv-app/`, 2.1 MB, version
`1.0.251.0-fos_1058210`) is Amazon's **Device Artifact Vending Service**
client — an OTA channel for model blobs, entirely separate from firmware
updates. From its dex:

```
ARTIFACT_TYPE_WAKEWORD          ARTIFACT_KEY_CID_DATABASE
ARTIFACT_KEY_PERSONALIZED_AED   addWakewordLocaleParam / addWakewordNameParam
registerArtifactWithDavs        checkAndDownloadImmediately
ARTIFACT_TTL_KEY                scheduleCheckAndDownloadForArtifact
switchWakeWordModel             DAVS using Android DownloadManager:
```

Artifacts are keyed by **type + key**, where the key carries the wake word
name and the **locale**; the client registers what it wants, polls on a TTL,
downloads via Android's `DownloadManager`, caches under its own data
directory, and hot-swaps the running model (`switchWakeWordModel`). Regioned
(`DavsRegion`), account-authenticated (`AmazonAccountUtils`).

So the endpointer's tuning is **cloud-vended, per-locale, per-account**, which
is why it is absent here twice over: EchoMuse's debloat leaves the package
`enabled=2` (DISABLED), and the unit was never signed in to an Amazon account
anyway. Its files directory is empty. Nothing was lost by disabling it — those
artifacts are wake word models and a contact-ID database for Alexa's own
stack, not something EchoMuse could consume.

## The five endpointing modes

`recognizer.endpoint.mode`, described in the binary as:

> `"not_set"` indicates to use the JSON configuration if provided otherwise we
> default to `all_speech` mode. `"all_speech"` indicates all audio be treated
> as a speech utterance. `"search_based"` asks the decoder to handle the end
> pointing based on information about the current state of the decode.
> `"energy_vad"` indicates using energy based voice activity detection
> algorithm to determine the end points. `"hybrid"` indicates using the
> combination of `energy_vad` and `search_based`. `"dynamic"` uses the expected
> pause duration over all active hypotheses. The parameter
> `fe.uttdet.max_speech_seconds` will take effect in all of these modes.

Ranked by how well they survive a television:

- **`dynamic`** — the strongest. Endpointing is a *prediction*, not a
  measurement: over all active hypotheses in the trellis the decoder computes
  an expected pause duration, and compares it against
  `search.endpoint.dynamic.min_pause_duration` ("Not endpoint will be detected
  if the expected pause duration is smaller than this threshold") and
  `.max_pause_duration` ("Endpoint will be detected **without considering final
  states** if the expected pause duration is larger than this threshold").
  After "set a timer for" the model expects more words and will not endpoint;
  after "set a timer for five minutes" it endpoints immediately. Background
  speech contributes nothing to the hypothesis set, so it cannot delay this.
- **`search_based`** — endpoint on the decoder's 1-best alignment landing on
  non-speech phones, counted over a window (below). Robust because it asks
  about the phone alignment of the *decoded utterance*, not about acoustic
  energy.
- **`hybrid`** — both, combined.
- **`energy_vad`** — the fallback. Still range-normalised (below), which is
  more than EchoMuse has.
- **`all_speech`** — treat everything as speech; endpoint only on
  `max_speech_seconds`. The documented way to build a hard cut-off.

Supporting bounds, both first-class parameters rather than panic guards:

- `recognizer.endpoint.min_speech_seconds` — "Minimum length in seconds of a
  speech utterance. There will not be any EndOfUtterance detected until the
  minimum length constraint satisfied or session_end reached."
- `fe.uttdet.max_speech_seconds` — "Maximum length in seconds of a speech
  utterance. Setting it to 0 sets maximum length to infinity."

## The decision objects

`libpryon` builds one endpoint decision per strategy and combines them. The
full set, from the RTTI symbols:

```
AllSpeechEndpointDecision          AllNonSpeechEndpointDecision
VadEndpointDecision                DecoderEndpointSignalDecision
PauseDurationEndpointDecision      OneBestFeaturesEndpointDecision
ContextualEndpointDecision         SampleRangeEndpointDecision
UnderMinEndpointDecision
```

`UnderMinEndpointDecision` is `min_speech_seconds` as a first-class veto — a
decision object whose job is to refuse. `ContextualEndpointDecision` endpoints
on a *phrase*: `Contextual endpointing phrase is `, `contextual-choice-list`,
`contextualFinalPauseDurationThreshold`, and a fallback message `failed to use
contextual endpointing, underlying fst (`. When the grammar knows what
completes the utterance, finishing it is the endpoint.

There is also a **speculative** endpointer running in parallel
(`SpeculativeEndDetectionLogic`, `endpointSettingType == "normal" ||
"speculative"`, metrics `pryon_speculative_ep_reduction_msec`,
`pryon_speculative_ep_time_before_eos_msec`). It endpoints early and starts
building a result before the normal endpointer commits, to buy latency back —
which is only safe because the normal endpointer is the authority.

## `search_based`, concretely

Three parameters, all in decoder frames (`fe.audio_analysis.frame_shift_milli`
× `fe.lfr_frame_count`):

| Parameter | Binary's own description |
|---|---|
| `search.endpoint.maximum_frames` | "Number of frames to be examined in the best path to determine if to start or end an utterance. Must be greater than start_count and stop_count." |
| `search.endpoint.start_count` | "Number of speech frames (out of the maximum_frames frames) threshold to start an utterance." |
| `search.endpoint.stop_count` | "Number of none-speech frames (out of the prior maximum_frames frames) threshold to end an utterance." |

Note the shape: **N-out-of-M voting over a sliding window**, not "K consecutive
frames of silence". A voting window tolerates a stray speech-scoring frame
inside a genuine pause, which is exactly what a TV produces, without needing
the timer to be long.

### What "a non-speech frame" actually means here

The decoder is Kaldi — `pryon/cpp/decoder/kaldi_decoder.cpp`,
`LatticeFasterDecoder`, HCLG, transition-ids and pdfs throughout. So the
non-speech test is not a heuristic on the audio; it is a **set of pdf-ids in
the acoustic model**:

```
nonSpeechPdf   non-speech-pdf   NONSPEECH_CLASS
Could not find nonspeech transition ID to assign as failsafe emitting label
```

A frame counts as non-speech when the transition-id it took on the winning
path maps into that class — silence and noise models the acoustic model was
*trained* to recognise, distinguished from speech phones by the same network
that does the recognition. There is even a failsafe for a model that ships
without one.

Two properties fall out, and both are things a VAD structurally cannot do.

**The count is over the best token's traceback, not a per-frame flag.**
The feature is `bestTokNonSpeechCount` — non-speech frames along the current
best hypothesis's history. It is recomputed from the best path every frame, so
when the decoder revises what it thinks was said, the *labelling of frames
already past* changes with it. A VAD writes its history down once and cannot
take it back; the endpointer's view of "was that a pause" is retrospective and
keeps improving while the utterance is still in flight.

**And it is not limited to the 1-best.** The per-frame record also carries
`latticeNonSpeechPosterior` and `latticeSpeechPosterior` — the posterior mass
across the whole lattice. The question stops being "does the winning path say
non-speech" and becomes "what fraction of the probability says non-speech",
which survives a temporarily wrong 1-best.

Alongside those, `search.require_endstate` and the assertion
`!mEndpointFeatureConfig || mFinalWord != SYMBOL_NOT_FOUND` say the path must
also be sitting in a **grammar-final state** — a complete utterance, not just
a quiet one. `dynamic` mode is defined by when it is allowed to skip that
check (`max_pause_duration`: "Endpoint will be detected **without considering
final states** if the expected pause duration is larger than this threshold").

### The endpointer that actually ships is a fusion network

`search.endpoint.model_filepath` is "a file containing the model for
model-based endpointing", and `DnnEndpointDecision` /
`VadAssistedDnnEndpointDecision` / `getDynamicEndpointDnnConfig` /
`epDnnPosteriors` say the decision is a DNN posterior per frame. What that
network eats is pinned by one assertion:

```
vadDnnEpEmbeddingOutputDim + EP_DNN_INPUT_EXTRA_FEATURES_NUM
    == network.InputDim(mDnnInputTag)
```

Its input is **the VAD DNN's embedding output concatenated with a small fixed
number of extra features**. So the shipped endpointer is neither "a VAD" nor
"the decoder" — it is a learned fusion of an acoustic embedding and decoder
state, and `search_based` names one family of its inputs rather than the whole
mechanism. (`EP_DNN_INPUT_EXTRA_FEATURES_NUM` is a compile-time constant and
its value is not in the strings.)

The candidate feature set is enumerated in the per-frame endpointing record,
which is worth reading in full because it is the clearest statement anywhere
of what Amazon thinks the question depends on:

```
activeToksCount        searchBeam            bestTokNonSpeechCount
oneBestCost            totalCost             logTotalCostOffset
bestTotalScoreToken    bestAcousticScoreToken    bestGraphScoreToken
bestPathPhones         bestPathTransitions
tokenLatticeGraphScore     tokenLatticeAcousticScore
hypothesisGraphScore       hypothesisAcousticScore
latticeNonSpeechPosterior  latticeSpeechPosterior
expectedPauseDuration      expectedFinalPauseDuration
expectedFinalPauseWithBestNSfinal   expectedFinalPauseWithContext
foregroundEnergy       backgroundEnergy
epDnnPosteriors        endpointerInSpecialMode
acousticEmbeddingForDeviceDirectedness    embeddingVector
```

Three observations for EchoMuse.

- **`foregroundEnergy` / `backgroundEnergy` is our max/min tracker.** Amazon
  carries a foreground-vs-background energy split into the endpoint feature
  vector as one input among ~25. That is a direct corroboration of
  `em_endpoint`'s design rather than a coincidence: the level split is a real
  feature, it is just not sufficient on its own, which is exactly the caveat
  already documented for P1.
- **`activeToksCount` and `searchBeam` are trellis breadth.** A decoder that
  has narrowed to few surviving hypotheses is one that has decided; breadth
  collapsing is an endpoint cue with no acoustic analogue at all.
- **The directedness embedding rides in the same record.** "Is this finished"
  and "was this addressed to me" are computed from shared state, which is why
  the stock device gets both right in a room with a television and why a
  bolt-on VAD gets neither.

`Static` vs `Elastic EndpointUttdetLogicConfigProvider` closes it out: the
thresholds themselves need not be constant for the length of an utterance.

## The vote-queue VAD

`UttdetVoteQueueVadFeatureExtractor` / `eos_vad_voting_queue.cpp`. Its
config validates four fields:

```
maxQueueSize > 0
votesNeeded > 0
maxQueueSize >= votesNeeded
rightContextCount >= 0 && rightContextCount < maxQueueSize
nonSpeechThreshold >= 0.0f && nonSpeechThreshold < 1.0f
```

`nonSpeechThreshold` being a probability in [0,1) says the VAD emits a
posterior, not an energy comparison. `rightContextCount` is **lookahead**: the
decision for a frame is deferred until that many later frames are in the
queue, so a pause is confirmed against what came after it. `votesNeeded` out of
`maxQueueSize` is the same N-of-M voting as the search endpointer.

## Backporch

Four estimators — `PhoneAlignmentBackporch`, `VadAssistedBackPorch`,
`VadQueueOnlyBackPorch`, `EndpointerEstimatedBackPorch` — plus
`GetExpectedBackporchFrame` and an `Insufficient data detected while
calculating expected backporch time.` diagnostic.

The backporch is the audio *after* the endpoint decision that is still part of
the utterance. Amazon does not merely keep a fixed tail; it estimates it, from
the phone alignment or from the VAD queue, and reports the error as a metric
(`x_expected_backporch_msec`, `metrics_calculator_estimated_backporch.cpp`).

The reason this exists is the reason it matters here: an endpointer aggressive
enough to beat a TV will clip trailing words unless something puts the tail
back. Any change that makes EchoMuse endpoint sooner needs a backporch in the
same change.

## The other half: device directedness

Separate from endpointing, `libpryon` carries a **device-directedness
classifier** — "was this speech addressed to me at all":

```
DeviceDirectednessVerifier      DeviceDirectedAcousticEmbedding
directednessScore / directednessConfidence / directednessStable
recognizer.directedness_score_lower_bound / _upper_bound
recognizer.confidence_device_directedness_interval_frame_count
acousticEmbeddingForDeviceDirectedness      directedness_binning
```

It scores continuously over a frame interval, from an acoustic embedding, and
carries a stability flag. This is the mechanism by which a stock Echo ignores
the TV in the middle of your sentence rather than transcribing it — and it is
a per-frame *acoustic embedding*, i.e. a speaker/channel identity, not a
grammar test.

`recognizer.erle.enabled`, `recognizer.erle.suppress_threshold` and
`recognizer.erle_suppress_probability` sit alongside it: the decoder consumes
the AFE's ERLE metric and suppresses results probabilistically when the echo
canceller says it is listening to itself.

## What the AFE contributes before any of this

From `AFE.cfg` (see alexa-afe.md for the full pipeline):

```
... AEC → ARA → ABF → VAD → RefBeamSelector → SNRBeamSelector
    → False WW Prevention → Output Gain → EspFeatureExtraction
```

Two things bear on the TV case.

- **ABF** puts two adaptive nulls on every beam
  (`"Num Refs Per Input": 2`, beam *i* adapted against beams *i*+2 and *i*+4).
  A spatially separated interferer is attenuated before the decoder sees it.
  This is the part EchoMuse gets for free on the native-AFE path and does not
  have on the tinyalsa path.
- **`ASR SNRBeamSelector` re-selects continuously**, with
  `"hangoverPeriod": 15` and `"energyRatio": 1.2`. It is not locked for the
  duration of an utterance. During the user's mid-sentence pause the TV is the
  best-SNR direction, so the selector can swing to it and *boost* it into the
  ASR stream. That is survivable for Amazon because the decoder is endpointing
  on hypothesis state rather than on what is loud. **It is not survivable for
  EchoMuse, which is endpointing on exactly what is loud** — and EchoMuse's
  own beamformer, by contrast, locks the selected mic at turn start.

The `"Voice Activity Detector"` block in `AFE.cfg` is gain staging only, and
says so:

```json
// Device side bias factors always set to 1.0f. Bias factors are applied on the cloud.
"Ambient Gain" : 1.0,  "Voice Gain" : 1.0,
```

The device-side VAD deliberately does not decide. It measures, and the
decision is made where the language model is.

## The energy VAD, and why it is the transferable part

Even Amazon's fallback is not a threshold on RMS. `fe.energy_vad.*`, verbatim:

| Parameter | Description |
|---|---|
| `max_hysteresis` | "Number of consecutive frames with dB energy above the current maximum needed to set a new maximum." |
| `min_hysteresis` | "Number of consecutive frames with dB energy below the current minimum needed to set a new minimum." |
| `max_hunt` | "Amount, in dB, to reduce the estimate of the maximum dB energy tracker on each frame that is below the current maximum. Must be less than 0." |
| `min_hunt` | "Amount, in dB, to increase the estimate of the minimum dB energy tracker on each frame that is above the current minimum. Must be greater than 0." |
| `min_range` | "Minimum dB energy range used for thresholding. Roughly, should approximate a lower bound of the signal-to-noise ratio. Must be greater than 0." |
| `high_per_mil` | "Normalized utterance-starting (high) threshold factor, in thousandths. Must be greater than or equal to low_per_mil." |
| `low_per_mil` | "Normalized utterance-ending (low) threshold factor, in thousandths. Must be less than or equal to high_per_mil." |
| `start_window` / `start_count` | N-of-M above `high` to start |
| `stop_window` / `stop_count` | N-of-M below `low` to stop |

Restated as an algorithm:

```
range  = max(maxDb - minDb, min_range)
high   = minDb + (high_per_mil / 1000) * range
low    = minDb + (low_per_mil  / 1000) * range
stop when stop_count of the last stop_window frames are below `low`
```

Three properties EchoMuse's endpointing has none of:

1. **A maximum tracker.** The thresholds are anchored to the loudest thing in
   this utterance — the speaker — and hunt down slowly. Room noise that rises
   cannot pull the stop threshold up with it, because the stop threshold is a
   fraction of the distance from the floor to *the user's own level*.
2. **Separate start and stop thresholds** (`high_per_mil` ≥ `low_per_mil`) —
   hysteresis in level, on top of hysteresis in time.
3. **N-of-M voting rather than consecutive-frame counting** on both edges.

This is the piece that needs no model, no decoder and no new dependency, and
it is the piece that directly addresses "the TV is 10 dB below me and holds the
gate open".

## What EchoMuse does today

For a **wake-word** turn, honestly stated: **EchoMuse has no endpointer.**

- The device's VAD gate and AGC apply only to `lock_mic` (button) turns. The
  always-on wake stream is ungated by design, so no device sentinel is ever
  sent for a wake turn (`em_esphome._stream_mic_audio`'s comment says so:
  the 0x05 sentinel "only fires when lock_mic=True — a condition P0-1
  eliminates").
- The controller's `_is_speech` (3 × `device.noise_floor`) only ever *disarms*
  the no-speech timeout. It never ends a turn.
- `em_turnclock.no_speech_verdict` closes a turn where nobody spoke. Once
  `speech_seen` is true it returns `False` forever, by construction.
- So end-of-turn is **entirely** HA's `STT_VAD_END`, with
  `asyncio.wait_for(..., timeout=20.0)` as a panic guard that logs a warning
  and marks the outcome `stream_timeout`.

HA's Assist VAD is an absolute speech/no-speech detector followed by a fixed
silence timer. Continuous background speech means it never sees the silence.
The turn therefore runs to the 20 s cap, and HA is handed 20 s of the user's
command with a television mixed into it — a bad transcript on top of a 20 s
wait. Both symptoms are the reported one.

The `native_afe.enabled` marker is set on the fielded Dot, so its ASR stream is
already Amazon's ABF output. That helps the interferer level and does nothing
for the endpointing decision, which is entirely controller-side.

## Proposals

Ranked by value over cost. They are additive and independent; none requires a
decoder.

**P1, P2 and P3 shipped together** as `controller/em_endpoint.py`, wired into
`em_esphome._stream_mic_audio` and covered by `tests/test_endpoint.py`. Five
device-scoped config keys under Microphones, all tunable from the dashboard:

| Key | Default | The symptom it answers |
|---|---|---|
| `endpointRelative` | on | — |
| `endpointLowPerMil` | 400 | background speech still holds turns open (raise) / a soft talker gets cut (lower) |
| `endpointSilenceMs` | 1200 | it cut me off mid-sentence (raise) |
| `endpointBackporchMs` | 250 | the answer missed my last word (raise) |
| `maxSpeechMs` | 12000 | how long a stuck turn may run before it is answered anyway |

See CLAUDE.md's "Ending the turn" section for the invariants. **P4 is moot** —
this fleet is native-AFE only. P5 remains open.

One thing changed on contact with the tests. The threshold is measured **down
from the maximum**, not up from the minimum as `fe.energy_vad`'s parameter
names imply. The two are identical whenever the observed range exceeds
`min_range` — the ordinary case — and differ only when the floor clamps it,
where anchoring from the minimum is actively wrong: a steady tone converges
both trackers onto its own level, and `min + 0.4 x 12 dB` then sits 4.8 dB
*above* the signal, so a speaker holding a constant level endpoints
themselves. Found by
`test_a_talker_who_stays_at_their_own_level_is_never_cut`, which fails against
the min-anchored form. Amazon presumably avoids this with a much slower
minimum hunt at their 10 ms frame rate; anchoring on the maximum avoids it by
construction, and the maximum is the speaker.

### P1 — A range-normalised, wake-anchored endpointer in the controller

Port `fe.energy_vad`'s design into `em_turnclock` as a pure function, and run
it in `_stream_mic_audio` alongside the existing HA VAD check.

- Per 80 ms frame, dB energy. `maxDb` / `minDb` trackers with `*_hunt` and
  `*_hysteresis`; `range = max(maxDb − minDb, min_range)`; stop threshold
  `minDb + low_per_mil/1000 × range`; **N-of-M** stop voting.
- **Seed `maxDb` from the wake-word window**, not from the turn's own audio.
  The wake word is a known-good sample of the target speaker at their real
  distance, available at frame zero, and it is the anchor that makes a TV
  10 dB down read as non-speech from the first frame rather than after the
  trackers have converged.
- Respect `min_speech_seconds` before any stop decision can fire.
- **It races HA's VAD, it does not replace it.** The loop already exits on
  whichever fires first, so this is a ceiling on turn length, not a new
  authority. Wrong-but-early is bounded by P3; wrong-and-late is what we have.

### P2 — `max_speech_seconds` as a real parameter

The 20 s `wait_for` is `all_speech` mode with a badly chosen constant and a
warning log. Make it a config key (device-scoped, `microphones` section),
default ~10–12 s, and treat firing it as an ordinary endpoint rather than
`stream_timeout` — HA still gets `end=True` and still produces a result, so
the outcome label is wrong today as well as the value.

Cheapest possible relief, independent of everything else: it converts "20 s of
dead air then a garbage transcript" into "a truncated command, answered".

### P3 — A backporch

Once P1 or P2 can end a turn early, keep sending ~200–300 ms after the stop
decision before `end=True`. Fixed, not estimated — the estimators need a phone
alignment we do not have. Must land in the same change as P1.

### P4 — ~~Settle whether native AFE helps or hurts this specific case~~ (moot)

**Dropped: this fleet is native-AFE only, so there is no A/B to run.** The
observation stands and is worth carrying into P1's tuning rather than being
forgotten: `ASR SNRBeamSelector` re-selects mid-utterance with a 15-frame
hangover and `energyRatio` 1.2, so during the user's pause the television is
the best-SNR direction and gets *boosted* into the ASR stream by that beam's
gain. Amazon survives this because it endpoints on hypothesis state rather than
on what is loud.

For P1 this means the interferer's level in the stream is not stable — it steps
up when the selector swings. It does not defeat the design, because the swing
lifts the TV by a beam's worth of gain rather than to parity with a near-field
speaker, but it does argue for leaving headroom in `endpointLowPerMil` rather
than tuning it to the edge of whatever a single quiet room produces.

### A decoder on the device — considered, and it does not answer this

`echo-dot-2-playground/custom-voice-assistant` runs local speech-to-text on
this hardware. Worth being precise about what that would and would not buy,
because the shape of it looks like `search_based` and is not.

It uses **sherpa-onnx**, not `libpryon` — its wake-word half references the
Pryon detector but the STT is sherpa. sherpa-onnx does carry a real
endpointing API on its streaming transducer, and rule 2 (trailing silence
measured since the last decoded symbol) is genuinely decoder-driven rather
than energy-driven, which is a real step up on a VAD.

**It still does not solve a television.** The endpointer fires on trailing
*blank* symbols, and a transducer fed background speech emits non-blank
symbols for it — it transcribes the TV and keeps going, exactly as HA's
Whisper does. The property that makes Amazon's `dynamic` mode immune is that
the pause prediction comes from hypotheses about **the utterance**, and the
property that makes the stock device ignore the TV in the middle of a sentence
is directedness. Neither arrives with a general-purpose ASR.

Against that it costs a second ONNX model permanently resident on a device
already at 18–20% of a core for the mic pipeline, plus ~38% if on-device wake
word is enabled. The cheap version of the same idea is P5, which reuses the
ONNX Runtime `internal/wakeword/ort` already dlopens and scores one embedding
per second rather than decoding continuously.

Keep local STT in mind as a latency play (it is a good one), not as the fix
for this.

### P5 — Directedness

Amazon's actual answer, and out of reach cheaply. The tractable subset is a
speaker-similarity gate: embed the wake-word window, score each second of the
turn against it, and treat dissimilar speech as non-speech for endpointing
purposes. A small ONNX speaker-verification model on the controller, in the
same thread executor as openwakeword. Worth doing only if P1 proves
insufficient — note that P1 already captures most of the same signal in the one
dimension (level) that correlates with distance.

## Open questions

- No tuned values were recovered, so P1's constants must be derived by
  measurement in a real room with a real TV. `saveUtterances` recordings are
  the instrument — a turn that failed this way, replayed offline through a
  candidate endpointer, is the whole test rig.
- Whether HA's Assist pipeline exposes anything better than its
  `VadSensitivity` silence-seconds. If HA ever streams partial STT results,
  `search_based` becomes approximable controller-side.
- `ARA` (alexa-afe.md's open question) runs between AEC and ABF and is still
  unexpanded. `setAraReferenceBeam` suggests it is beam-space, which would make
  it relevant to interferer rejection and not only to echo.
- Whether `libasp`'s own VAD output is reachable at all through the OpenSL ES
  path. If it is, it is a free non-speech posterior computed on the 7-channel
  signal, which is strictly better information than the controller can derive
  from one downmixed channel.
