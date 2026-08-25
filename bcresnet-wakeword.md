# BC-ResNet wake word — spec and plan

**Status:** not started. Everything below is static analysis of EchoMuse plus
measurements taken on x86 — no code has been written and nothing has run on a
device. Phase 0 exists to produce the artifact; Phase 2's probe exists to kill
the whole idea cheaply if the device cannot afford it.

**Both gating unknowns were investigated on 2026-08-16 and neither is a
blocker** — see "The runtime question, settled" and "The cost question, bounded"
below. Short version: the fielded ARM runtime has the `STFT` kernel compiled in,
and the projected on-device cost is 26–40% of one core depending on hop, against
36.2% for openWakeWord today.

**Background:** the model is [`~/git/bcresnet`](../../bcresnet) — Qualcomm's
BC-ResNet trained on "hey tara", 26,699 parameters, exported to ONNX by
`scripts/train_qc.py --export_formats onnx`. This document is only about
running it *here*.

## What this is

EchoMuse's wake word is openWakeWord, top to bottom, in both places it runs:

- **Controller** — `em_controller.py:2056` constructs
  `OWWModel(wakeword_models=[name])` and scores 80 ms chunks at
  `em_controller.py:2262` (`model.predict(samples)` → a dict keyed by the
  filename *stem*).
- **Device** — `internal/wakeword` implements openWakeWord's streaming feature
  pipeline in Go: melspectrogram → mel ring → embedding → embedding ring →
  classifier, with the shapes as package constants (`stream.go:44-78`:
  `MelBins=32`, `MelWindow=76`, `MelStep=8`, `FeatDim=96`, `FeatWindow=16`) and
  again in `ort.go`'s `Embed`/`Classify`. `shadow.Open` loads exactly three
  files from `/data/local/share/echomuse/oww`.

BC-ResNet is not a different wake word in that pipeline — it is a different
pipeline. It takes 40-bin log-mel over a whole 1.4 s window (`[1,1,40,141]`)
and returns **three** logits (`hey_tara`, `noise`, `unknown`), not one score
over a 16×96 embedding window. Nothing in either path can feed it.

**The trap to close first:** `em_oww_models.safe_model_filename` validates the
filename, the `.onnx` suffix and a 20 MB cap — nothing about the graph. Upload
`bcresnet.onnx` through Config → Wake word → "+ Custom model" today and it
installs cleanly, deploys cleanly, and then fails at scoring time, because
openWakeWord derives its feature window from the model's own input shape and
will feed `[1, 1, 96]` to a graph that demands 22400 audio samples. Whatever
else happens, upload-time validation should reject or classify the file
(Phase 1).

## The decision that makes this small

**Put the feature frontend inside the ONNX graph.** The model then takes raw
audio and EchoMuse needs no mel implementation anywhere — no torchaudio on the
controller (which is not deployable), no DSP in Go on the device, and no second
definition of the frontend to drift out of step with training.

This was the open question, and it is now measured rather than assumed. Wrapping
`LogMel` + `BCResNets` and exporting with torch 2.11's dynamo exporter succeeds
at opset 20, lowering `torch.stft` to the standard ONNX `STFT` op (opset 17+):

```
input:   audio  [batch, 22400] float32
output:  logits [batch, 3]     float32
ops:     Conv 44, Reshape 29, Add 21, Relu 18, BatchNormalization 12,
         AveragePool 12, Sigmoid 12, Mul 12, Concat 11, …, STFT 1, MatMul 1, Log 1
size:    519 KB, weights inlined (single file)
parity:  max|torch − onnxruntime| = 1.7e-06 over a batch of 2
cost:    4.94 ms/invoke on ORT 1.19.2 (the device's version), 2.50 ms on 1.28 —
         1 intra-op thread, x86. See "The cost question, bounded".
```

For comparison the openWakeWord asset set is 3.3 MB across three files
(`melspectrogram` 1.1 MB + `embedding_model` 1.3 MB + head ~0.9 MB).

## The runtime question, settled

The risk that decision took was the `STFT` kernel on the *device's* runtime. The
controller pins `onnxruntime==1.20.1` (x86, PyPI); the device runs the vendored
`onnxruntime-android` **1.19.2** AAR (`controller/Dockerfile:111`). Checked
directly, two ways:

1. **The fielded ARM library has the kernel, not just the schema.** Downloaded
   the pinned AAR, sha256 verified against the Dockerfile
   (`84737dd1…c2a7b5f`, matches), extracted `jni/armeabi-v7a/libonnxruntime.so`
   — 12,290,332 bytes, ELF 32-bit ARM EABI5, Android 21, i.e. byte-for-byte the
   12.3 MB that ships to devices. It contains
   `virtual Status onnxruntime::STFT::Compute(OpKernelContext *) const` and the
   RTTI symbol `N11onnxruntime4STFTE` — those come from the compiled CPU kernel,
   not from the op schema (`STFT_Onnx_ver17`, also present). `MelWeightMatrix`,
   `HannWindow`, `BlackmanWindow` and the `dft_length`/`frame_step` kernel
   strings are all there too. The signal ops were not compiled out of this build.
2. **ORT 1.19.2 runs the graph.** The same version from PyPI (x86 build, shared
   kernel source) loads the opset-20 export and produces logits with no
   fallback and no warning.

What is still unverified is only what a probe on real hardware can answer:
XNNPACK will not take the `STFT` node, so it runs on the CPU EP there. That is a
performance question, and it is answered below.

**The conv-STFT export is therefore a performance option, not a compatibility
fallback.** Exporting the frontend as convolutions instead — an `nn.Conv1d`
whose weights are the DFT sin/cos basis, then the mel filterbank as a `MatMul`,
then `log` — produces the same numbers with only `Conv`/`MatMul`/`Log` in the
graph, all XNNPACK-eligible. It targets the 34% of the invoke that `STFT`
currently costs (measured below). Worth doing if Phase 2's probe lands near the
budget; not needed to start.

## The cost question, bounded

Both pipelines benchmarked on the same machine, under the *device's* ORT version
(1.19.2), one intra-op thread, sequential execution, driven exactly as
`stream.go` drives them — median of interleaved rounds:

| | per invoke (x86) |
|---|---|
| oWW melspectrogram (1×1760 samples) | 0.14 ms |
| oWW embedding (1×76×32×1) | 2.09 ms |
| oWW classifier (1×16×96) | 0.04 ms |
| **oWW total, per 80 ms frame** | **2.27 ms** |
| **BC-ResNet audio-in, per 1.4 s window** | **4.94 ms** |

BC-ResNet's graph breaks down as `STFT` 34% (1.70 ms), `AveragePool` 19%,
`Conv` 13%, `Add` 6% — the classifier itself is about two thirds of it.

EchoMuse has already measured the oWW pipeline on the Dot: 29 ms of work per
80 ms frame, 36.2% of one core (`ort.go:17-21`). That gives a device/x86 factor
of **12.8×** for a workload this machine also runs, which is a far better
projection basis than a generic ARM-vs-x86 guess:

| BC-ResNet hop | invocations/s | projected on-device | vs oWW today (36.2%) |
|---|---|---|---|
| 80 ms | 12.5 | ~63 ms/window, **79%** of one core | 2.2× |
| 160 ms | 6.25 | ~63 ms/window, **40%** of one core | 1.1× |
| 240 ms | 4.17 | ~63 ms/window, **26%** of one core | 0.7× |
| 320 ms | 3.13 | ~63 ms/window, **20%** of one core | 0.6× |

**Read this as an order-of-magnitude bound, not a measurement.** Two things bias
it: the 12.8× factor comes from a workload that is 92% one convolutional model,
applied here to a mix that is a third FFT; and on the device XNNPACK accelerates
the convolutions but not `STFT`, which pushes the STFT share — and the total —
up. Both push the same direction, so treat the table as optimistic and let the
probe settle it.

The conclusion that matters: **80 ms is out of budget, 160 ms is roughly par
with openWakeWord, 240 ms is comfortably under it.** Start on-device at 240 ms.

## The contract

One file plus one sidecar, both installed the way custom oWW models already are.

```
<stem>.onnx     audio [batch, 22400] float32  →  logits [batch, 3] float32
<stem>.json     {"type": "bcresnet", "sampleRate": 16000, "window": 22400,
                 "labels": ["hey_tara", "noise", "unknown"], "wakeIndex": 0,
                 "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8}
```

Rules that are not negotiable, because getting any of them wrong produces a
detector that runs, scores plausibly, and is quietly wrong:

1. **Peak-normalize each window to 0.8 before scoring**, on floats in ±1
   (`samples / 32768.0`), skipping the scale when the window peak is below
   `1e-4`. This is what `scripts/eval.py:46-52` does and what the model's
   level-invariance depends on — recall holds from full volume down to ×0.05
   *with* it. Do it outside the graph so the guard constant stays in the same
   units as the reference implementation.
   - Corollary: a near-silent window gets amplified noise. Gate scoring on
     `device.noise_floor` (already tracked at `em_controller.py:2251`) or a
     fixed RMS floor, or the quiet hours will generate false positives that
     look like model failures.
2. **Softmax the three logits and take `wakeIndex`.** oWW's score is already a
   probability; ours is not until you exponentiate. `max - exp - normalize`,
   as in `eval.py:55`.
3. **The window is baked into the graph.** A model trained with a different
   `--clip_duration` or `--n_mels` is a different file with a different input
   shape. The sidecar records both so a mismatch is a loud error at load, not
   a silent one at inference.
4. **Labels travel with the model.** `labels.json` from the training run is the
   only thing that says which logit is the wake word. Alphabetical ordering
   today puts `hey_tara` at 0 — do not rely on that, read the sidecar.

### Streaming parameters

| Parameter | Value | Why |
|---|---|---|
| Window | 22400 samples (1.4 s) | Fixed by the training clip; the graph's input shape |
| Hop | 160 ms on the controller (every 2nd mic frame), 240 ms on the device (every 3rd) | The mic delivers 80 ms frames, so the hop is a whole number of them. The controller has CPU to spare; the device does not — 240 ms projects to 26% of one core against oWW's 36.2%, and still puts ~6 windows over a 1.4 s utterance |
| Smoothing | mean of last 3 scores (~480 ms) | Matches the reference streaming demo (`rolyantrauts/bcresnet/microphone-streaming`); kills single-window spikes |
| Threshold | `owwThreshold`, retuned | Same semantics (probability), different operating point — see Phase 4 |
| Refractory | 1 window (1.4 s) after a detection | Overlapping windows would otherwise fire 8 times on one utterance. The existing `oww_paused` covers the turn itself |

The hop is the CPU dial and it is linear: halving it doubles the cost and buys
back that much worst-case latency. See "The cost question, bounded".

## Architecture

Both sides already have the right seam. Neither needs its ring/threshold/
barge-in logic touched.

### Controller

Everything downstream of scoring consumes a single float `score` and a
`model.reset()`. That is the whole interface openWakeWord provides here, so
introduce it explicitly and put both implementations behind it:

```python
# controller/em_wake_scorer.py  (new)
class WakeScorer(Protocol):
    def push(self, samples: np.ndarray) -> float | None: ...  # 1280 int16 in, score or None
    def reset(self) -> None: ...
    @property
    def info(self) -> str: ...

class OwwScorer:        # wraps OWWModel + em_oww_models.prediction_key
class BcresnetScorer:   # ring buffer + hop counter + normalize + infer + smooth
```

- `BcresnetScorer` takes an injected `infer(x: np.ndarray) -> np.ndarray`
  callable, exactly as the device's `wakeword.Detector` takes an `Inferer`
  (`stream.go:92`). The buffering — which *is* the algorithm — is then pure
  numpy and unit-testable in `controller/tests/`, which deliberately has neither
  openwakeword nor onnxruntime installed.
- `em_controller.py:2262`'s `model.predict(samples)` + `prediction.get(model_key)`
  becomes `scorer.push(samples)`; `None` means "no score this frame" (fewer than
  22400 samples buffered, or not a hop boundary) and skips the frame the same way
  the barge-in and ring guards already do.
- The barge-in watcher (`em_controller.py:1108`) gets the same treatment; it
  constructs its own model today.
- `owwSpeexNs` is an openWakeWord option with no BC-ResNet equivalent. Render it
  disabled-with-reason, the same pattern every capability-gated control uses. It
  is close to moot regardless — the audio HAL's ASP front end already denoises
  what it hands us.

### Device

`shadow.Scorer`'s public API (`Push`/`PushBytes`/`Ready`/`Reset`/`Drain`/
`SetThreshold`/`Info`/`Close`) is model-agnostic already; it is
`wakeword.Detector` underneath it that is openWakeWord-shaped. So:

- **`internal/wakeword/ort`** — add a generic single-model handle. `load()` and
  `run()` are already generic (`ort.go:138`, `ort.go:276`); only `NewInferer`
  insists on three models. Add `func (r *Runtime) NewModel(path, name string,
  o Options) (*Model, error)` with `Run(in []float32, shape []int64)
  ([]float32, error)` and leave the existing `Inferer` untouched.
- **`internal/wakeword/bcresnet`** (new package) — the 22400-sample ring, hop
  counter, ±1 conversion, peak normalization, softmax, smoothing window,
  refractory. Same split as the parent package: pure Go, an `Inferer` interface
  with one method, no cgo, host-testable.
- **`shadow`** — pick the detector by sidecar `type`. `ModelStem` already
  handles the path→stem mapping and stays as is; `Open` learns to read
  `<stem>.json` and to load one model instead of three.

### Assets

`em_oww_assets.desired_assets()` returns a fixed four-item set (runtime + two
shared feature models + the head). Make it type-aware: a bcresnet model wants
`libonnxruntime.so`, `<stem>.onnx`, `<stem>.json` — and *not* the 2.4 MB of
shared feature models. `plan_sync` already computes adds/deletes by md5, so the
shared models get cleaned off a device that no longer needs them for free.
`md5 is the only definition of success` (`em_oww_assets.py:30`) applies
unchanged.

## Phases

Each phase is separately shippable and separately abandonable. Nothing before
Phase 3 changes what a fielded device does.

### Phase 0 — the artifact (`bcresnet` repo, not here)

Add an audio-in export mode to `scripts/train_qc.py` alongside today's log-mel-in
export: `--onnx_input {features,audio}` producing `bcresnet_audio.onnx`, plus the
sidecar JSON. The existing post-export parity check (torch vs onnxruntime) covers
it; add a second check that scores a handful of real `hey_tara` test clips
through the audio-in graph and the `.tflite` and reports both, so the frontend
inside the graph is proven equivalent to the one `eval.py` computes outside it.

**Done when:** `bcresnet_audio.onnx` + `<stem>.json` exist, parity < 1e-4, and
scores on real clips match `eval.py`'s to three decimals.

### Phase 1 — controller (behind the scorer seam)

1. `em_wake_scorer.py` + unit tests with a fake `infer` (no ORT in the test env).
2. `em_controller` wake listener and barge-in watcher moved onto `WakeScorer`.
   `OwwScorer` must be byte-for-byte behaviour-preserving — same near-miss
   counters, same `model.reset()` points, same `wake_level` deque (that feeds
   `em_endpoint.Endpointer.seed` and must not change).
3. Upload validation: open the uploaded `.onnx` with onnxruntime (already a
   controller dependency, 1.20.1), read `get_inputs()[0].shape`, and classify —
   `[1, 16, 96]` → oWW head, `[batch, 22400]` → bcresnet, anything else →
   rejected with a message naming both accepted shapes. Write the sidecar at
   upload time. This closes the "installs fine, never fires" trap.

**Done when:** a bcresnet model selected in the dashboard wakes a device via the
controller path, and the oWW path is unchanged (existing tests green).

### Phase 2 — device

**2a. Probe before code.** Extend `device/tools/oww_probe` (or clone it as
`bcresnet_probe`): load `bcresnet_audio.onnx` through the dlopen'd runtime,
verify against a golden fixture generated by the Python scorer
(`testdata/gen_bcresnet_fixture.py`, mirroring `gen_fixture.py`), and pace
windows at real time to report process CPU from `getrusage` — the same two
questions, and the same reason: flat-out latency is the misleading number for a
duty-cycled stream.

**Kill criteria.** `STFT` support is no longer one of them — it is confirmed
present in the fielded library. What remains: if CPU at a 240 ms hop exceeds
what openWakeWord costs today (36.2% of one core, `ort.go:17-21`) with no hop,
conv-STFT export, or model-size (`--tau`) adjustment that fixes it, stop. The
controller path from Phase 1 already works, and this phase buys only offline
operation.

The probe's job is to confirm or refute the ~63 ms/window projection. If it
lands within ~1.5× of that, the hop table holds and the decision is a
straightforward budget choice. If it lands well above, try the conv-STFT export
first — `STFT` is 34% of the invoke and is the one part XNNPACK cannot help with.

**2b.** `ort.NewModel`, the `bcresnet` package, `shadow` type dispatch, host
tests against the fixture.

**Done when:** `fixture.Verify` passes on real ARM hardware and the probe's CPU
number is inside budget.

### Phase 3 — deployment

Type-aware `desired_assets`, dashboard wake-word tiles that show the model type
and the disabled-with-reason controls, `owwOnDevice` unchanged in meaning. No new
message types; no wire-protocol change (see below).

### Phase 4 — tuning

`owwThreshold`'s default of 0.5 is openWakeWord's recommendation and does not
transfer. The `bcresnet` README's held-out numbers (85.0% recall / 0.0% FP on
noise / 5.3% FP on other speech at 0.5) are for single 1.4 s clips, not for a
detector scoring an overlapping window 6.25 times a second — the effective false
positive rate per hour is a different quantity and must be measured in the
house. The near-miss telemetry already in the controller (counters, hourly
rollup, `wake_counters`) is exactly the instrument; use it, and set the default
from a week of real audio rather than from the test set.

## What does not change

Worth stating plainly, because it is why this is containable.

- **Wire protocol.** The device sends mono S16 16 kHz in 80 ms frames, before
  and after — the same format the audio HAL hands us
  (`native-afe-migration.md`, "Wire protocol: unchanged").
- **Endpointing.** `wake_level` and `em_endpoint.Endpointer.seed` are computed
  from raw chunk RMS, never from the model. Untouched.
- **Ring, barge-in, mute, speaking guards.** All of it sits above the score.
  `model.reset()` maps to `scorer.reset()` (clear the ring and the smoothing
  history) at exactly the same points.
- **openWakeWord.** Stays, works, remains the default. `oww_forge` remains the
  path for anyone who wants a new wake word without this machinery. This is a
  second backend, not a migration.

## Risks and open questions

| Risk | Mitigation |
|---|---|
| ~~`STFT` missing from the 1.19.2 Android AAR~~ | **Closed.** The kernel is compiled into the sha256-verified fielded library; ORT 1.19.2 runs the graph |
| `STFT` slower on ARM than the projection assumes (XNNPACK cannot take that node) | Probed in 2a; conv-STFT export targets exactly this 34% |
| On-device CPU beyond budget | Projected 26% of one core at a 240 ms hop vs oWW's 36.2%, but from a conv-dominated scaling factor — kill criteria in 2a; hop, conv-STFT and `--tau` are the dials; controller path still works |
| Peak normalization amplifies silence into false positives | RMS floor gate, reusing `device.noise_floor` |
| The audio HAL's front end (+7.2 dB, AGC, beamforming) changes the level distribution the model was trained on | Peak normalization absorbs gain, not spectral shaping — collect audio through the real path and re-check recall before trusting the fleet default |
| Threshold semantics silently differ from oWW | Sidecar `type` is visible in the dashboard; Phase 4 retunes from telemetry |
| Two scorers = two code paths to keep alive | The `WakeScorer`/`Inferer` seams are narrow by construction, and `OwwScorer` is a thin wrapper over what exists today |

**Open:** whether the wake word should also gate on the `unknown` class rather
than only on `hey_tara` ≥ threshold — the third logit is information oWW never
had (a near-miss phrase scores high on `unknown`), and a margin rule
(`p_wake − p_unknown > m`) may beat a bare threshold. Worth an offline
experiment against the same recordings Phase 4 collects; not worth blocking on.
