"""
Re-scoring a saved clip to cut out the stretch the wake model scored highest.

A wake clip or a near-miss recording runs as long as the Echo's score stayed
up: its support (every 1.4 s window behind the score, from the oldest of the
three it averaged when the episode opened to the last hop at or above the
floor) plus 300 ms either side. So the clips vary in length, from about 2 s to
3.6 s, while BCResNet trains on and scores one fixed 1.4 s window. Training's
loaders keep only the first 1.4 s of a longer clip, which is rarely the part
that scored: when the first 479 saved near misses were re-scored, the best
window started 0.48–0.96 s into eight in ten recordings, and at the start of
only one.

The Activity view's archives can hold crops instead. The controller scores
each clip again, one 1.4 s window every 160 ms as the Dot does (and one more
ending at the clip's end when the grid stops short of it), with the graph
that scored it on the Echo while the registry still holds it and the fleet's
active graph otherwise. Then it cuts a crop of the requested length centred on
the best window, or on every window scoring at or above a threshold.

The scores are the controller's: the Dot's graph and window preparation
(em_wake_scorer) on the microphone audio the Echo uploaded, so close to the
Echo's own window scores but not bit-identical, and never smoothed (a near
miss's peak is the mean of three windows).

Scoring runs on EXECUTOR: one thread, with one ORT intra-op thread, so an
export takes at most one core from the realtime path and exports run one at a
time. A window costs about 7 ms there, so a few hundred clips take tens of
seconds the first time. The scores are kept per file and graph, so exporting
the same clips again at another length or threshold only reads the files.
"""

from __future__ import annotations

import collections
import concurrent.futures
import logging
import wave
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path

import numpy as np

import em_recordings
import em_wake_registry
import em_wake_scorer
from em_wake_scorer import BcresnetSpec, Infer

log = logging.getLogger(__name__)

SAMPLE_RATE = em_recordings.SAMPLE_RATE
HOP = em_wake_scorer.HOP_SAMPLES                  # 160 ms, the Dot's hop
WINDOW = em_wake_scorer.WINDOW_SAMPLES            # 1.4 s, the graph's input
MIN_CROP_SECONDS = 0.1
MAX_CROP_SECONDS = 10.0
# Clips whose scores are kept. An entry is a few dozen floats, so this holds
# every clip the controller keeps (500 wake clips and 200 near misses per
# device, plus the marked ones) for a fleet of a dozen Echos.
SCORE_CACHE_SIZE = 8_192

EXECUTOR = concurrent.futures.ThreadPoolExecutor(max_workers=1, thread_name_prefix="clip-crop")


@dataclass(frozen=True, slots=True)
class CropSpec:
    """What an archive cuts from each clip: `samples` long, centred on the window that
    scored highest (`window_min` None), or on every window scoring at or above `window_min`."""

    samples: int
    window_min: float | None = None


@dataclass(frozen=True, slots=True)
class WindowScore:
    start: int      # the window's first sample in the clip
    score: float    # its wake probability, unsmoothed


@dataclass(frozen=True, slots=True)
class Crop:
    start: int      # [start, end) samples of the clip
    end: int
    score: float    # the score of the window it is centred on


@dataclass(frozen=True, slots=True)
class Clip:
    """One saved clip an archive holds: `name` is its download name when whole (it ends in
    .wav), and `graph_sha256` the graph that scored it on the Echo (None: not recorded)."""

    name: str
    path: Path
    graph_sha256: str | None


def window_starts(n: int, window: int = WINDOW) -> list[int]:
    """The first sample of each window scored over an `n`-sample clip: every HOP from its
    start, and one more ending at its end when that grid stops short of it. A clip shorter
    than one window has none."""
    if n < window:
        return []
    starts = list(range(0, n - window + 1, HOP))
    if starts[-1] != n - window:
        starts.append(n - window)
    return starts


def score_windows(pcm: np.ndarray, infer: Infer, spec: BcresnetSpec) -> tuple[WindowScore, ...]:
    """Each window's wake probability. A window below the RMS floor is left out, as the Dot
    leaves it unscored."""
    scores = []
    for start in window_starts(len(pcm), spec.window):
        p = em_wake_scorer.wake_probability(infer, spec, pcm[start:start + spec.window])
        if p is not None:
            scores.append(WindowScore(start, p))
    return tuple(scores)


def plan(n: int, scores: Sequence[WindowScore], crop: CropSpec, window: int = WINDOW) -> list[Crop]:
    """The crops cut from an `n`-sample clip whose windows scored `scores`, in clip order:
    one centred on the best window, or one centred on each window scoring at or above
    `crop.window_min`. A crop that would run past an end of the clip moves inside it, and
    a clip shorter than the crop is taken whole. Two windows whose crops land on the same
    samples give one crop, with the higher score."""
    if crop.window_min is None:
        chosen = [max(scores, key=lambda s: s.score)] if scores else []
    else:
        chosen = [s for s in scores if s.score >= crop.window_min]
    length = min(crop.samples, n)
    crops: dict[int, Crop] = {}
    for s in chosen:
        start = min(max(s.start + window // 2 - length // 2, 0), n - length)
        if start not in crops or crops[start].score < s.score:
            crops[start] = Crop(start, start + length, s.score)
    return [crops[start] for start in sorted(crops)]


def read_pcm(path: Path) -> np.ndarray:
    """A saved clip's samples. Every clip is 16 kHz mono 16-bit, as em_recordings.encode_wav
    writes it; ValueError for any other format."""
    with wave.open(str(path), "rb") as w:
        shape = (w.getframerate(), w.getnchannels(), w.getsampwidth())
        if shape != (SAMPLE_RATE, em_recordings.CHANNELS, em_recordings.SAMPLE_WIDTH):
            raise ValueError(f"{path.name} is {shape[0]} Hz, {shape[1]} channels, {shape[2] * 8}-bit")
        return np.frombuffer(w.readframes(w.getnframes()), dtype=np.int16)


def cut(path: Path, start: int, end: int) -> bytes:
    """Samples [start, end) of a saved clip, as a WAV."""
    return em_recordings.encode_wav(read_pcm(path)[start:end].tobytes())


def crop_name(clip_name: str, crop: Crop) -> str:
    """A crop's download name: its clip's, then where in the clip it starts and the score of
    the window it is centred on (`kitchen-nearmiss446-0.64s-0.41.wav`). The clip's name stays
    at the front, so a training split can keep one clip's crops together."""
    return f"{clip_name.removesuffix('.wav')}-{crop.start / SAMPLE_RATE:.2f}s-{crop.score:.2f}.wav"


def _load_onnx(path: Path, spec: BcresnetSpec) -> Infer:
    return em_wake_scorer.onnx_infer(path, threads=1, spec=spec)[0]


class ClipScorer:
    """Scores saved clips with the registry's graphs. It keeps each graph's session and each
    clip's scores between exports. Not thread-safe: use it on EXECUTOR only."""

    def __init__(self, registry: em_wake_registry.WakeRegistry,
                 load: Callable[[Path, BcresnetSpec], Infer] = _load_onnx) -> None:
        self.registry = registry
        self._load = load
        self._models: dict[str, tuple[Infer, BcresnetSpec]] = {}
        self._scores: collections.OrderedDict[tuple[str, int, int, str], tuple[int, tuple[WindowScore, ...]]] \
            = collections.OrderedDict()

    def graph_for(self, graph_sha256: str | None) -> str:
        """The graph that scores a clip: the one that scored it on the Echo while the registry
        holds it, else the fleet's active graph. RegistryError when there is neither."""
        if graph_sha256 is not None:
            try:
                return self.registry.get(graph_sha256).graph_sha256
            except em_wake_registry.RegistryError:
                pass
        return self.registry.active().graph_sha256

    def _model(self, graph_sha256: str) -> tuple[Infer, BcresnetSpec]:
        if graph_sha256 not in self._models:
            graph = self.registry.graph_path(graph_sha256)
            spec = em_wake_scorer.load_spec(graph, self.registry.sidecar_path(graph_sha256))
            self._models[graph_sha256] = (self._load(graph, spec), spec)
        return self._models[graph_sha256]

    def scores(self, path: Path, graph_sha256: str | None) -> tuple[int, tuple[WindowScore, ...]]:
        """A clip's length in samples and its windows' scores, with the graph graph_for picks."""
        sha = self.graph_for(graph_sha256)
        st = path.stat()
        key = (str(path), st.st_mtime_ns, st.st_size, sha)
        if (kept := self._scores.get(key)) is not None:
            self._scores.move_to_end(key)
            return kept
        pcm = read_pcm(path)
        infer, spec = self._model(sha)
        result = (len(pcm), score_windows(pcm, infer, spec))
        self._scores[key] = result
        if len(self._scores) > SCORE_CACHE_SIZE:
            self._scores.popitem(last=False)
        return result

    def plan_archive(self, clips: Sequence[Clip], crop: CropSpec) -> list[tuple[str, Path, Crop]]:
        """(download name, clip file, crop) of every crop `crop` cuts from `clips`, in their
        order. A clip pruned since it was listed is left out, and so is one that cannot be read
        or scored (logged). RegistryError when a clip has no graph to score it with."""
        planned: list[tuple[str, Path, Crop]] = []
        for clip in clips:
            try:
                n, scores = self.scores(clip.path, clip.graph_sha256)
            except FileNotFoundError:
                continue
            except (OSError, EOFError, ValueError, wave.Error) as exc:
                log.warning("[crop] %s not scored: %s", clip.path.name, exc)
                continue
            planned.extend((crop_name(clip.name, c), clip.path, c) for c in plan(n, scores, crop))
        return planned
