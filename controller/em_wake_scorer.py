"""
Wake-word scoring behind one interface, so two model families can live behind
it without the wake listener knowing which it has.

Everything downstream of scoring in `em_controller` consumes exactly two things:
a single float `score` per mic chunk, and a `reset()` at the points where the
rolling context must be discarded. That was always the whole of openWakeWord's
surface here, so this module makes it explicit and puts both implementations
behind it:

  OwwScorer       wraps openwakeword's OWWModel; a thin adapter over what the
                  controller already did, and behaviour-preserving by design.
  BcresnetScorer  buffers a 1.4 s window, peak-normalizes it, runs one ONNX
                  graph that contains its own log-mel frontend, softmaxes the
                  three logits and smooths.

The two differ in one visible way, and callers must handle it: **push() may
return None**, meaning "no score for this chunk". openWakeWord scores every
80 ms chunk; BC-ResNet scores a whole window on a hop, so most chunks produce
nothing. A None is not a zero — feeding it to the near-miss counters or the
threshold compare would invent a low score for a frame that was never judged.

The buffering *is* the algorithm, so it is kept pure: BcresnetScorer takes an
injected `infer` callable and never imports onnxruntime, exactly as the device's
`wakeword.Detector` takes an `Inferer`. That is what lets it be unit-tested in
`controller/tests/`, which deliberately has neither openwakeword nor onnxruntime
installed.
"""

from __future__ import annotations

import collections
import json
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Protocol

import numpy as np

# The mic delivers 80 ms of 16 kHz mono per chunk. Both scorers are driven one
# chunk at a time, so every hop is a whole number of these.
CHUNK_SAMPLES = 1280

# Model families, as recorded in a sidecar's "type" and as returned by
# classify_model_shape.
TYPE_OWW = "oww"
TYPE_BCRESNET = "bcresnet"
# Not a model we can run: the log-mel-in export from train_qc.py's DEFAULT mode.
# Named separately from "unknown" because it is the file someone will actually
# reach for by mistake — it is called bcresnet.onnx and it is what a plain
# --export_only writes. See classify_model_shape.
TYPE_BCRESNET_FEATURES = "bcresnet-features"

# openWakeWord's classifier head: 16 embeddings of 96 features.
OWW_INPUT_SHAPE = (1, 16, 96)

# Peak below which a window is NOT rescaled. Matches scripts/eval.py's guard in
# the same units (floats in +/-1), which is the point: the reference
# implementation owns this constant and a second value here would be a second
# definition of the model's level invariance.
PEAK_EPS = 1e-4

# Windows quieter than this are not scored at all. Distinct from PEAK_EPS and
# needed *because* of it: a window peaking just above the guard is rescaled by
# up to ~8000x, so a room that is merely quiet becomes full-scale noise fed to a
# detector. The guard alone does not cover that — it only declines to amplify
# true digital silence. A muted device streams zero-filled frames and is caught
# here too.
SILENCE_RMS = 1e-4

# Mean of the last N window scores. Matches the reference streaming demo
# (rolyantrauts/bcresnet/microphone-streaming) and kills the single-window spike
# that an overlapping window otherwise produces on a transient.
SMOOTHING_WINDOWS = 3


class WakeScorer(Protocol):
    """One float per chunk, or None when this chunk produced no score."""

    def push(self, samples: np.ndarray) -> float | None: ...
    def reset(self) -> None: ...
    @property
    def info(self) -> str: ...


# --------------------------------------------------------------------------
# openWakeWord
# --------------------------------------------------------------------------

class OwwScorer:
    """Adapter over openwakeword's OWWModel.

    Deliberately thin and deliberately boring: this path is what every fielded
    device uses today, so the seam has to be behaviour-preserving rather than
    an improvement. It scores every chunk and therefore never returns None.

    `key` is openwakeword's prediction key, which for a custom model is the
    filename STEM and not the configured path — see em_oww_models.prediction_key.

    **Do not try to prove this equivalent by scoring the same audio through the
    old path and this one and diffing.** openwakeword is not deterministic:
    measured on 0.6.0, two freshly constructed OWWModels fed byte-identical
    chunks in the same order disagree by up to 0.024, and a single instance
    disagrees with ITSELF by up to 0.077 across a reset(). So an A/B produces a
    non-zero difference no matter how correct the wrapper is, and reading that
    as a regression sends you looking for a bug that is not here. What can be
    pinned is the contract — key lookup, 0.0 for a missing key, reset
    forwarding — and tests/test_wake_scorer.py does that against a fake.
    """

    def __init__(self, model, key: str, info: str = ""):
        self._model = model
        self._key = key
        self._info = info or f"openwakeword, model {key}"

    def push(self, samples: np.ndarray) -> float | None:
        return float(self._model.predict(samples).get(self._key, 0.0))

    def reset(self) -> None:
        self._model.reset()

    @property
    def info(self) -> str:
        return self._info

    @property
    def model(self):
        """The wrapped OWWModel, for the few callers that still need it."""
        return self._model


# --------------------------------------------------------------------------
# BC-ResNet
# --------------------------------------------------------------------------

@dataclass(frozen=True)
class BcresnetSpec:
    """The sidecar, parsed.

    Everything here is something the .onnx cannot state about itself and a
    consumer must not guess. wake_index especially: alphabetical ordering puts
    the wake word at whatever index its name sorts to, which is 1 for the
    "ohphelia" model (noise, ohphelia, unknown) and 0 for "hey tara". A default
    of 0 would score the wrong class and still look like a working detector.
    """

    window: int
    sample_rate: int
    labels: tuple[str, ...]
    wake_index: int
    norm_peak: float
    n_mels: int
    clip_seconds: float

    @property
    def wake_label(self) -> str:
        return self.labels[self.wake_index]


def parse_spec(raw: dict) -> BcresnetSpec:
    """Validate a sidecar dict into a BcresnetSpec.

    Every failure raises with the offending value named. A sidecar is written by
    the exporter and edited by nobody, so anything wrong here means the file was
    hand-made or belongs to a different model — both cases where refusing to
    load beats scoring something plausible against the wrong class.
    """
    if not isinstance(raw, dict):
        raise ValueError(f"sidecar must be a JSON object, got {type(raw).__name__}")
    kind = raw.get("type")
    if kind != TYPE_BCRESNET:
        raise ValueError(f"sidecar type must be {TYPE_BCRESNET!r}, got {kind!r}")

    labels = raw.get("labels")
    if not isinstance(labels, list) or not labels or not all(isinstance(x, str) for x in labels):
        raise ValueError(f"sidecar labels must be a non-empty list of strings, got {labels!r}")

    wake_index = raw.get("wakeIndex")
    if not isinstance(wake_index, int) or isinstance(wake_index, bool) \
            or not 0 <= wake_index < len(labels):
        raise ValueError(
            f"sidecar wakeIndex {wake_index!r} is not a valid index into "
            f"{len(labels)} labels"
        )

    window = raw.get("window")
    if not isinstance(window, int) or window <= 0:
        raise ValueError(f"sidecar window must be a positive int, got {window!r}")
    # NB the window is deliberately NOT required to be a whole number of mic
    # chunks, and the shipped model is not one: 1.4 s is 22400 samples against a
    # 1280-sample chunk, i.e. 17.5 of them. Only the HOP has to be chunk-aligned,
    # because that is what the caller counts in. The ring holds the last `window`
    # samples whatever boundary they arrived on.

    sample_rate = raw.get("sampleRate", 16000)
    if sample_rate != 16000:
        raise ValueError(
            f"sidecar sampleRate {sample_rate!r}: the wake stream is 16000 Hz and "
            f"is not resampled"
        )

    norm_peak = raw.get("normPeak", 0.8)
    if not isinstance(norm_peak, (int, float)) or not 0.0 < norm_peak <= 1.0:
        raise ValueError(f"sidecar normPeak must be in (0, 1], got {norm_peak!r}")

    return BcresnetSpec(
        window=window,
        sample_rate=int(sample_rate),
        labels=tuple(labels),
        wake_index=wake_index,
        norm_peak=float(norm_peak),
        n_mels=int(raw.get("nMels", 40)),
        clip_seconds=float(raw.get("clipSeconds", window / sample_rate)),
    )


def sidecar_path(model_path: str | os.PathLike) -> Path:
    """<stem>.onnx -> <stem>.json, alongside it."""
    return Path(model_path).with_suffix(".json")


def load_spec(model_path: str | os.PathLike) -> BcresnetSpec:
    path = sidecar_path(model_path)
    try:
        raw = json.loads(path.read_text())
    except FileNotFoundError:
        raise FileNotFoundError(
            f"{path.name} not found beside {Path(model_path).name} — a BC-ResNet "
            f"model needs its sidecar to say which logit is the wake word"
        ) from None
    try:
        return parse_spec(raw)
    except ValueError as e:
        raise ValueError(f"{path.name}: {e}") from None


def softmax(logits: np.ndarray) -> np.ndarray:
    """max-subtract, exp, normalize — scripts/eval.py:55.

    openWakeWord's output is already a probability; BC-ResNet's is not until you
    exponentiate, and a raw logit compared against a 0-1 threshold is a detector
    that runs and never fires (or always does).
    """
    e = np.exp(logits - logits.max(axis=-1, keepdims=True))
    return e / e.sum(axis=-1, keepdims=True)


class BcresnetScorer:
    """A 1.4 s window slid over the mic stream on a hop, scored by one graph.

    `infer` takes float32 `[1, window]` and returns float32 `[1, n_labels]` —
    the same shape of boundary as the device's `wakeword.Inferer`, and for the
    same reason: what is interesting here is the buffering, so it must be
    testable without a runtime.

    No refractory is implemented, and none is needed. Every controller path that
    ACTS on a wake already calls reset() — turn start, ring stopped, wake during
    an active turn, post-turn — and a reset empties the ring, so the next score
    cannot arrive until a full fresh window has been collected. That is a
    refractory of exactly one window, which is the value the design wanted,
    obtained by mapping reset() faithfully rather than by adding a second
    mechanism that could disagree with it.
    """

    def __init__(
        self,
        infer: Callable[[np.ndarray], np.ndarray],
        spec: BcresnetSpec,
        hop_chunks: int = 2,
        smoothing: int = SMOOTHING_WINDOWS,
        min_rms: float = SILENCE_RMS,
        info: str = "",
    ):
        if hop_chunks < 1:
            raise ValueError(f"hop_chunks must be >= 1, got {hop_chunks}")
        if smoothing < 1:
            raise ValueError(f"smoothing must be >= 1, got {smoothing}")
        self._infer = infer
        self._spec = spec
        self._hop_chunks = hop_chunks
        self._smoothing = smoothing
        self._min_rms = min_rms
        self._info = info or (
            f"bcresnet, wake '{spec.wake_label}'@{spec.wake_index} of "
            f"{len(spec.labels)}, {spec.clip_seconds:.2f}s window, "
            f"hop {hop_chunks * CHUNK_SAMPLES * 1000 // spec.sample_rate}ms"
        )
        self._buf = np.zeros(spec.window, dtype=np.int16)
        self._recent: collections.deque[float] = collections.deque(maxlen=smoothing)
        self.reset()

    # -- lifecycle ---------------------------------------------------------

    def reset(self) -> None:
        """Discard the rolling context: the ring and the smoothing history.

        The buffer's contents are not zeroed, only disowned — `_filled` is what
        makes them unreadable, and a window is never scored until it has been
        refilled from scratch.
        """
        self._filled = 0
        # So the first full window scores the moment it exists rather than one
        # hop later. "Score as soon as there is something to score, then every
        # hop" is the intended behaviour.
        self._since_hop = self._hop_chunks - 1
        self._recent.clear()

    @property
    def info(self) -> str:
        return self._info

    @property
    def spec(self) -> BcresnetSpec:
        return self._spec

    @property
    def ready(self) -> bool:
        """Whether a full window has been collected since the last reset."""
        return self._filled >= self._spec.window

    # -- scoring -----------------------------------------------------------

    def push(self, samples: np.ndarray) -> float | None:
        """Add one chunk. Returns a smoothed wake probability, or None.

        None means one of: the ring is not full yet, this chunk is not a hop
        boundary, or the window is below the silence floor. All three are "not
        judged", which is why they share a return value distinct from 0.0.
        """
        # No audio in, no state change — see the device's bcresnet.Detector.Push
        # for why this guard exists at both ends.
        if np.asarray(samples).size == 0:
            return None
        self._append(samples)
        if self._filled < self._spec.window:
            return None
        self._since_hop += 1
        if self._since_hop < self._hop_chunks:
            return None
        self._since_hop = 0
        return self._score()

    def _append(self, samples: np.ndarray) -> None:
        s = np.asarray(samples, dtype=np.int16).reshape(-1)
        n = s.size
        w = self._spec.window
        if n == 0:
            return
        if n >= w:
            # A chunk larger than the whole window: keep only its tail. Cannot
            # happen with an 80 ms mic chunk, but the buffer arithmetic below
            # would be wrong rather than merely wasteful if it did.
            self._buf[:] = s[-w:]
            self._filled = w
            return
        self._buf[: w - n] = self._buf[n:]
        self._buf[w - n:] = s
        self._filled = min(w, self._filled + n)

    def _score(self) -> float | None:
        window = self._buf.astype(np.float32) / 32768.0

        rms = float(np.sqrt(np.mean(window.astype(np.float64) ** 2)))
        if rms < self._min_rms:
            # Not scored, and the smoothing history is deliberately left alone:
            # a silent gap should not dilute the scores either side of it.
            return None

        peak = float(np.abs(window).max())
        if peak > PEAK_EPS:
            window = window * (self._spec.norm_peak / peak)

        logits = np.asarray(self._infer(window[None, :]), dtype=np.float32)
        if logits.ndim != 2 or logits.shape[0] != 1:
            raise ValueError(
                f"infer returned {logits.shape}, expected (1, n_labels)"
            )
        if logits.shape[1] != len(self._spec.labels):
            raise ValueError(
                f"model emits {logits.shape[1]} logits but the sidecar names "
                f"{len(self._spec.labels)} labels — mismatched .onnx/.json pair"
            )

        p = float(softmax(logits[0])[self._spec.wake_index])
        self._recent.append(p)
        return float(sum(self._recent) / len(self._recent))


# --------------------------------------------------------------------------
# Identifying an uploaded model
# --------------------------------------------------------------------------

def classify_model_shape(shape) -> str:
    """Which family a model belongs to, from its ONNX input shape alone.

    Pure so it can be tested without onnxruntime; the caller opens the session
    and hands over `session.get_inputs()[0].shape`, whose entries are ints or
    strings (a named dynamic axis) or None.

    The features-in case is called out by name because it is the mistake people
    will actually make: train_qc.py's DEFAULT export mode writes exactly that
    graph, to a file called bcresnet.onnx, which looks like something you would
    upload. Reporting it as merely "unrecognised" sends someone looking at the
    controller when the fix is --onnx_input audio in the training repo.
    """
    dims = [d if isinstance(d, int) else None for d in (shape or [])]

    if tuple(dims) == OWW_INPUT_SHAPE:
        return TYPE_OWW
    # audio [batch, window]: batch is dynamic, the window is baked in.
    if len(dims) == 2 and dims[1] and dims[1] >= CHUNK_SAMPLES:
        return TYPE_BCRESNET
    # log_mel [batch, 1, n_mels, frames]
    if len(dims) == 4 and dims[1] == 1 and dims[2] and dims[3]:
        return TYPE_BCRESNET_FEATURES
    return ""


def inspect_model_file(path: str | os.PathLike) -> tuple[str, list]:
    """(family, input_shape) for an .onnx on disk, by opening it.

    Lazy onnxruntime import so the module stays importable without it. Loading
    the graph rather than parsing the protobuf is deliberate: it answers "can
    this controller actually run the file" and not merely "is it well-formed",
    which is the question an upload is really asking.
    """
    import onnxruntime as ort

    opts = ort.SessionOptions()
    opts.intra_op_num_threads = 1
    # A model that ONLY fails at run time is still a model we can classify, so
    # a session that opens is enough; nothing is inferred here.
    sess = ort.InferenceSession(str(path), opts, providers=["CPUExecutionProvider"])
    shape = list(sess.get_inputs()[0].shape)
    return classify_model_shape(shape), shape


def describe_accepted_shapes() -> str:
    """The message an upload rejection should carry. One definition of it."""
    return (
        "expected an openWakeWord head with input [1, 16, 96], or a BC-ResNet "
        "audio-in model with input [batch, samples] plus a matching .json sidecar"
    )


# --------------------------------------------------------------------------
# Construction (the only part that touches a runtime)
# --------------------------------------------------------------------------

def onnx_infer(model_path: str | os.PathLike, threads: int = 1):
    """An `infer` callable backed by onnxruntime, plus a description of it.

    Imported lazily so this module stays importable in the unit-test
    environment, which has no onnxruntime. Single-threaded and sequential: this
    is duty-cycled work sharing a process with the audio pipeline, and letting
    ORT spin up its default thread pool costs far more CPU than it saves
    latency — the same conclusion the device reached in wakeword/ort.
    """
    import onnxruntime as ort

    opts = ort.SessionOptions()
    opts.intra_op_num_threads = threads
    opts.inter_op_num_threads = 1
    opts.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    sess = ort.InferenceSession(
        str(model_path), opts, providers=["CPUExecutionProvider"]
    )
    name = sess.get_inputs()[0].name

    def infer(x: np.ndarray) -> np.ndarray:
        return sess.run(None, {name: np.ascontiguousarray(x, dtype=np.float32)})[0]

    return infer, f"onnxruntime {ort.__version__}, {Path(model_path).name}"


def is_bcresnet_model(model_name: str) -> bool:
    """Whether a configured owwModel value names a BC-ResNet model.

    Decided by the sidecar existing beside the file, not by the filename: the
    name is user-chosen and the sidecar is what actually makes the model
    runnable. A bare path with no sidecar is an openWakeWord model, which is
    also the right answer for every model that predates this feature.
    """
    if not model_name or not str(model_name).endswith(".onnx"):
        return False
    return sidecar_path(model_name).is_file()


def build(model_name: str, *, speex_ns: bool = False, hop_chunks: int = 2) -> WakeScorer:
    """Construct the right scorer for a configured owwModel value.

    Blocking (it loads a model), so callers on the event loop must run it in an
    executor — as em_controller already did for OWWModel.
    """
    if is_bcresnet_model(model_name):
        spec = load_spec(model_name)
        infer, desc = onnx_infer(model_name)
        info = (
            f"bcresnet via {desc}, wake '{spec.wake_label}'@{spec.wake_index}, "
            f"{spec.clip_seconds:.2f}s window, hop {hop_chunks * 80}ms"
        )
        return BcresnetScorer(infer, spec, hop_chunks=hop_chunks, info=info)

    from openwakeword.model import Model as OWWModel  # lazy: not in the test env

    import em_oww_models
    model = OWWModel(
        wakeword_models=[model_name],
        enable_speex_noise_suppression=speex_ns,
    )
    key = em_oww_models.prediction_key(model_name)
    return OwwScorer(model, key, info=f"openwakeword, model {key} (speex_ns={speex_ns})")
