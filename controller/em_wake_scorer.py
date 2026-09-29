"""BCResNet audio-in scorer used for reference scoring and registry probes.

The only accepted wake model is the §5.1 audio-in graph plus sidecar. This
module intentionally contains no openWakeWord, feature-in, denoising, or model
family dispatch path.
"""

from __future__ import annotations

import collections
import enum
import json
import math
import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

import numpy as np

TYPE_BCRESNET = "bcresnet"
SAMPLE_RATE = 16_000
WINDOW_SAMPLES = 22_400
HOP_SAMPLES = 2_560
SMOOTHING_WINDOWS = 3
CLEAR_AFTER_UNSCORED = 6
PEAK_EPS = 1e-4
SILENCE_RMS = 1e-4
REFERENCE_THRESHOLD = 0.30
CANDIDATE_KEEP_RATIO = 0.50

Infer = Callable[[np.ndarray], np.ndarray]   # float32 [batch, window] audio → logits [batch, labels]


@dataclass(frozen=True, slots=True)
class BcresnetSpec:
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


def parse_spec(raw: object) -> BcresnetSpec:
    """Parse a §5.1 sidecar, rejecting values the consumer cannot honor."""
    if not isinstance(raw, Mapping):
        raise ValueError(f"sidecar must be a JSON object, got {type(raw).__name__}")
    if raw.get("type") != TYPE_BCRESNET:
        raise ValueError(f"sidecar type must be {TYPE_BCRESNET!r}, got {raw.get('type')!r}")
    labels = raw.get("labels")
    if not isinstance(labels, list) or not labels or not all(isinstance(x, str) and x for x in labels):
        raise ValueError(f"sidecar labels must be a non-empty list of strings, got {labels!r}")
    wake_index = raw.get("wakeIndex")
    if isinstance(wake_index, bool) or not isinstance(wake_index, int) or not 0 <= wake_index < len(labels):
        raise ValueError(f"sidecar wakeIndex {wake_index!r} is not valid for {len(labels)} labels")
    window = raw.get("window")
    if window != WINDOW_SAMPLES:
        raise ValueError(f"sidecar window must be {WINDOW_SAMPLES}, got {window!r}")
    sample_rate = raw.get("sampleRate")
    if sample_rate != SAMPLE_RATE:
        raise ValueError(f"sidecar sampleRate must be {SAMPLE_RATE}, got {sample_rate!r}")
    norm_peak = raw.get("normPeak")
    if not isinstance(norm_peak, (int, float)) or isinstance(norm_peak, bool) or float(norm_peak) != 0.8:
        raise ValueError(f"sidecar normPeak must be 0.8, got {norm_peak!r}")
    n_mels = raw.get("nMels")
    if isinstance(n_mels, bool) or not isinstance(n_mels, int) or n_mels <= 0:
        raise ValueError(f"sidecar nMels must be a positive int, got {n_mels!r}")
    clip_seconds = raw.get("clipSeconds")
    if isinstance(clip_seconds, bool) or not isinstance(clip_seconds, (int, float)) \
            or not math.isclose(float(clip_seconds), window / sample_rate):
        raise ValueError(f"sidecar clipSeconds must be {window / sample_rate:g}, got {clip_seconds!r}")
    return BcresnetSpec(window, sample_rate, tuple(labels), wake_index,
                        float(norm_peak), n_mels, float(clip_seconds))


def sidecar_path(model_path: str | os.PathLike[str]) -> Path:
    return Path(model_path).with_suffix(".json")


def load_spec(model_path: str | os.PathLike[str], sidecar: str | os.PathLike[str] | None = None) -> BcresnetSpec:
    path = sidecar_path(model_path) if sidecar is None else Path(sidecar)
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        raise FileNotFoundError(f"{path.name} not found beside {Path(model_path).name}") from None
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f"{path.name}: {exc}") from None
    try:
        return parse_spec(raw)
    except ValueError as exc:
        raise ValueError(f"{path.name}: {exc}") from None


def softmax(logits: np.ndarray) -> np.ndarray:
    values = np.asarray(logits)
    shifted = values - values.max(axis=-1, keepdims=True)
    exp = np.exp(shifted)
    return exp / exp.sum(axis=-1, keepdims=True)


def prepare_window(pcm: np.ndarray, spec: BcresnetSpec, *, allow_silence: bool = False) -> np.ndarray | None:
    """Exact §5.1 int16→float32 preparation. Returns None below RMS floor."""
    samples = np.asarray(pcm)
    if samples.dtype != np.int16 or samples.ndim != 1 or samples.size != spec.window:
        raise ValueError(f"window must be int16 [{spec.window}], got {samples.dtype} {samples.shape}")
    x = samples.astype(np.float32) / np.float32(32768.0)
    rms = float(np.sqrt(np.mean(x.astype(np.float64) ** 2)))
    if rms < SILENCE_RMS and not allow_silence:
        return None
    peak = float(np.abs(x).max(initial=0.0))
    if peak > PEAK_EPS:
        x *= np.float32(spec.norm_peak / peak)
    return np.ascontiguousarray(x)


def wake_probability(infer: Infer, spec: BcresnetSpec,
                     pcm: np.ndarray, *, allow_silence: bool = False) -> float | None:
    x = prepare_window(pcm, spec, allow_silence=allow_silence)
    if x is None:
        return None
    logits = np.asarray(infer(x[None, :]), dtype=np.float32)
    if logits.shape != (1, len(spec.labels)):
        raise ValueError(f"model emits {logits.shape}, expected (1, {len(spec.labels)})")
    if not np.all(np.isfinite(logits)):
        raise ValueError("model emitted non-finite logits")
    p = float(softmax(logits[0])[spec.wake_index])
    if not math.isfinite(p):
        raise ValueError("model emitted a non-finite probability")
    return p


@dataclass(frozen=True, slots=True)
class HopScore:
    end_sample: int
    raw: float | None
    smoothed: float | None


class CandidateClose(enum.StrEnum):
    """Why a reference candidate closed (§5.2)."""

    BELOW = "below"
    GAP = "gap"
    RESET = "reset"


@dataclass(frozen=True, slots=True)
class ReferenceCandidate:
    """A reference-scorer candidate event in reference-epoch samples.

    Emitted once when it opens (`close_reason` None) and once when it closes.
    `support_end` is the end of the last window whose raw probability reached
    the threshold so far."""

    first_crossing_end: int
    support_start: int
    support_end: int | None
    peak_smoothed: float
    close_reason: CandidateClose | None = None


@dataclass(slots=True)
class _OpenCandidate:
    first: int
    start: int
    support_end: int | None
    peak: float


class BcresnetScorer:
    """§5.2 scorer state: three scored probabilities, six-unscored clearing.

    Windows and their hop-grid end samples are supplied by the caller so this
    state works on reference timeline snapshots without maintaining a second
    PCM ring. `score_window` returns both raw and smoothed values.
    """

    def __init__(self, infer: Infer, spec: BcresnetSpec,
                 smoothing: int = SMOOTHING_WINDOWS, clear_after_unscored: int = CLEAR_AFTER_UNSCORED):
        if smoothing < 1 or clear_after_unscored < 1:
            raise ValueError("smoothing and clear_after_unscored must be positive")
        self.infer = infer
        self.spec = spec
        self.smoothing = smoothing
        self.clear_after_unscored = clear_after_unscored
        self._history: collections.deque[tuple[int, float]] = collections.deque(maxlen=smoothing)
        self._unscored = 0

    def reset(self) -> None:
        self._history.clear()
        self._unscored = 0

    @property
    def history(self) -> tuple[tuple[int, float], ...]:
        return tuple(self._history)

    def invalid(self, end_sample: int) -> HopScore:
        """A gap/discontinuity window: immediately clear history."""
        self.reset()
        return HopScore(end_sample, None, None)

    def score_window(self, end_sample: int, pcm: np.ndarray,
                     *, digital_silence: bool = False) -> HopScore:
        if end_sample % HOP_SAMPLES:
            raise ValueError(f"hop end {end_sample} is not a multiple of {HOP_SAMPLES}")
        if digital_silence:
            p = None
        else:
            p = wake_probability(self.infer, self.spec, pcm)
        if p is None:
            self._unscored += 1
            if self._unscored >= self.clear_after_unscored:
                self._history.clear()
            return HopScore(end_sample, None, None)
        self._unscored = 0
        self._history.append((end_sample, p))
        smoothed = float(sum(v for _, v in self._history) / len(self._history))
        return HopScore(end_sample, p, smoothed)


class ReferenceDetector:
    """Reference candidate policy (§5.2) over a `BcresnetScorer`."""

    def __init__(self, scorer: BcresnetScorer, threshold: float = REFERENCE_THRESHOLD):
        if not 0.0 < threshold < 1.0:
            raise ValueError("threshold must be in (0, 1)")
        self.scorer = scorer
        self.threshold = threshold
        self._open: _OpenCandidate | None = None
        self._below = 0

    def reset(self, reason: CandidateClose = CandidateClose.RESET) -> ReferenceCandidate | None:
        closed = self._close(reason)
        self.scorer.reset()
        return closed

    def _close(self, reason: CandidateClose) -> ReferenceCandidate | None:
        if self._open is None:
            self._below = 0
            return None
        o = self._open
        c = ReferenceCandidate(o.first, o.start, o.support_end, o.peak, reason)
        self._open = None
        self._below = 0
        return c

    def push(self, score: HopScore, *, invalid: bool = False) -> tuple[ReferenceCandidate, ...]:
        events = []
        if invalid:
            closed = self._close(CandidateClose.GAP)
            if closed is not None:
                events.append(closed)
            return tuple(events)
        if score.smoothed is None:
            if self._open is not None and not self.scorer.history:
                closed = self._close(CandidateClose.RESET)
                if closed is not None:
                    events.append(closed)
            return tuple(events)
        reached = score.raw is not None and score.raw >= self.threshold
        if self._open is None and score.smoothed >= self.threshold:
            history = self.scorer.history
            start = history[0][0] - self.scorer.spec.window
            self._open = _OpenCandidate(score.end_sample, start,
                                        score.end_sample if reached else None, score.smoothed)
            events.append(ReferenceCandidate(score.end_sample, start,
                                             self._open.support_end, score.smoothed))
            return tuple(events)
        if self._open is None:
            return ()
        self._open.peak = max(self._open.peak, score.smoothed)
        if reached:
            self._open.support_end = score.end_sample
        if score.smoothed >= self.threshold * CANDIDATE_KEEP_RATIO:
            self._below = 0
        else:
            self._below += 1
            if self._below >= 2:
                closed = self._close(CandidateClose.BELOW)
                if closed is not None:
                    events.append(closed)
        return tuple(events)


def onnx_infer(model_path: str | os.PathLike[str], threads: int = 1,
               *, spec: BcresnetSpec | None = None) -> tuple[Infer, str]:
    """Load one CPU ORT session (one intra-op thread, sequential execution).

    Static and dynamic graph IO is checked against the sidecar before the
    callable is returned. Imported lazily so pure tests need only numpy.
    """
    import onnxruntime as ort

    path = Path(model_path)
    spec = spec or load_spec(path)
    opts = ort.SessionOptions()
    opts.intra_op_num_threads = threads
    opts.inter_op_num_threads = 1
    opts.execution_mode = ort.ExecutionMode.ORT_SEQUENTIAL
    sess = ort.InferenceSession(str(path), opts, providers=["CPUExecutionProvider"])
    if len(sess.get_inputs()) != 1 or len(sess.get_outputs()) != 1:
        raise ValueError("BCResNet graph must have exactly one input and one output")
    inp, out = sess.get_inputs()[0], sess.get_outputs()[0]
    if inp.name != "audio" or inp.type != "tensor(float)":
        raise ValueError(f"BCResNet input must be float32 'audio', got {inp.name!r} {inp.type}")
    if len(inp.shape) != 2 or (isinstance(inp.shape[1], int) and inp.shape[1] != spec.window):
        raise ValueError(f"BCResNet audio input shape must be [batch,{spec.window}], got {inp.shape}")
    if out.name != "logits" or out.type != "tensor(float)":
        raise ValueError(f"BCResNet output must be float32 'logits', got {out.name!r} {out.type}")
    if len(out.shape) != 2 or (isinstance(out.shape[1], int) and out.shape[1] != len(spec.labels)):
        raise ValueError(f"BCResNet output shape disagrees with {len(spec.labels)} labels: {out.shape}")

    def infer(x: np.ndarray) -> np.ndarray:
        logits: np.ndarray = sess.run([out.name], {inp.name: np.ascontiguousarray(x, dtype=np.float32)})[0]
        return logits

    # A real run proves dynamic output count and catches a graph that opens but
    # cannot execute on this runtime.
    logits = np.asarray(infer(np.zeros((1, spec.window), dtype=np.float32)))
    if logits.shape != (1, len(spec.labels)) or not np.all(np.isfinite(logits)):
        raise ValueError(f"graph probe emitted invalid logits {logits.shape}")
    return infer, f"onnxruntime {ort.__version__}, {path.name}"


def probe_model(infer: Infer, spec: BcresnetSpec) -> dict[str, float]:
    """Deterministic silence/noise/tone load probe used by registry upload.

    Signals are int16 because deployed input is canonical PCM. Silence bypasses
    the stream RMS gate to exercise the graph. A healthy deployed model scores
    all three far below its 0.17 near-miss floor (§5.4).
    """
    n = spec.window
    silence = np.zeros(n, dtype=np.int16)
    rng = np.random.default_rng(0)
    noise = np.clip(np.rint(rng.normal(0.0, 0.10, n) * 32767.0), -32768, 32767).astype(np.int16)
    t = np.arange(n, dtype=np.float64) / spec.sample_rate
    tone = np.rint(0.5 * np.sin(2 * np.pi * 1000.0 * t) * 32767.0).astype(np.int16)
    scores: dict[str, float] = {}
    for name, signal in (("silence", silence), ("noise", noise), ("tone", tone)):
        p = wake_probability(infer, spec, signal, allow_silence=True)
        if p is None:     # allow_silence scores every window
            raise ValueError(f"{name} probe window was not scored")
        scores[name] = p
    return scores


def validate_probe(scores: dict[str, float], near_miss: float) -> None:
    if set(scores) != {"silence", "noise", "tone"}:
        raise ValueError("probe did not produce silence, noise, and tone scores")
    for name, value in scores.items():
        if not math.isfinite(value) or not 0.0 <= value < near_miss:
            raise ValueError(f"{name} probe score {value!r} is not finite and below {near_miss}")
