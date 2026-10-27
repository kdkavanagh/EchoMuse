"""
The wake model's windows over each saved clip, scored again on the controller.

A wake clip or a near-miss recording runs as long as the Echo's score stayed
up: its support (every 1.4 s window behind the score, from the oldest of the
three it averaged when the episode opened to the last hop at or above the
floor) plus 300 ms either side. So the clips vary in length, from about 2 s to
3.6 s, while BCResNet trains on and scores one fixed 1.4 s window. Training's
loaders keep only the first 1.4 s of a longer clip, which is rarely the part
that scored: when the first 479 saved near misses were re-scored, the best
window started 0.48–0.96 s into eight in ten recordings, and at the start of
only one.

So the controller scores every saved clip again, once: one 1.4 s window every
160 ms as the Dot does (and one more ending at the clip's end when that grid
stops short of it), with the graph that scored the clip on the Echo while the
registry still holds it and the fleet's active graph otherwise. It keeps the
result on the clip's row as ClipWindows JSON (`turns.wake_windows`,
`near_misses.windows`; schema v40). Activity shows a clip's windows and plays
the best one, and the archives cut crops around them (plan, cut).

ClipWindowsService does the scoring in the background. Every
SWEEP_INTERVAL_S it lists the clips on disk, asks the database which of them
have no windows yet, and scores those, newest first; `pending` says how many
are left. An archive scores on the spot any clip the sweep has not reached
yet, and keeps the result the same way.

The scores are the controller's: the Dot's graph and window preparation
(em_wake_scorer) on the microphone audio the Echo uploaded, so close to the
Echo's own window scores but not bit-identical, and never smoothed (a near
miss's peak is the mean of three windows).

Everything that scores, and everything that touches the service's state,
runs on EXECUTOR: one thread, with one ORT intra-op thread, so scoring takes
at most one core from the realtime path. A clip costs about 70 ms there.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import enum
import json
import logging
import wave
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import TypedDict

import numpy as np

import em_db as db
import em_recordings
import em_wake_registry
import em_wake_scorer
import em_wakeclips
from em_wake_scorer import BcresnetSpec, Infer

log = logging.getLogger(__name__)

SAMPLE_RATE = em_recordings.SAMPLE_RATE
HOP = em_wake_scorer.HOP_SAMPLES                  # 160 ms, the Dot's hop
WINDOW = em_wake_scorer.WINDOW_SAMPLES            # 1.4 s, the graph's input
MIN_CROP_SECONDS = 0.1
MAX_CROP_SECONDS = 10.0
# How often the background sweep looks for clips saved since the last one.
SWEEP_INTERVAL_S = 10.0

EXECUTOR = concurrent.futures.ThreadPoolExecutor(max_workers=1, thread_name_prefix="clip-windows")


class ClipKind(enum.StrEnum):
    WAKE = "wake"             # a turn's wake clip (em_wakeclips); turns.wake_windows
    NEAR_MISS = "near_miss"   # a near miss's recording (em_recordings NEAR_MISS); near_misses.windows


@dataclass(frozen=True, slots=True)
class WindowScore:
    start: int      # the window's first sample in the clip
    score: float    # its wake probability, unsmoothed


class ClipWindowsJson(TypedDict):
    """ClipWindows as the database stores it and the dashboard reads it."""

    graph_sha256: str
    samples: int
    window: int
    scores: list[tuple[int, float]]   # (start sample, wake probability), in clip order


@dataclass(frozen=True, slots=True)
class ClipWindows:
    """The windows scored over one clip: `graph_sha256` scored them, the clip is `samples`
    long, each window `window` samples, and `scores` holds every window that was scored, in
    clip order (a window below the RMS floor is not)."""

    graph_sha256: str
    samples: int
    window: int
    scores: tuple[WindowScore, ...]

    @property
    def best(self) -> WindowScore | None:
        return max(self.scores, key=lambda s: s.score) if self.scores else None

    def wire(self) -> ClipWindowsJson:
        return {"graph_sha256": self.graph_sha256, "samples": self.samples, "window": self.window,
                "scores": [(s.start, round(s.score, 4)) for s in self.scores]}

    def to_json(self) -> str:
        return json.dumps(self.wire(), separators=(",", ":"))

    @classmethod
    def from_json(cls, raw: str) -> ClipWindows:
        """ValueError when `raw` is not ClipWindows JSON."""
        data = json.loads(raw)
        if not isinstance(data, dict):
            raise ValueError("not an object")
        graph, samples, window, scores = (data.get(k) for k in ("graph_sha256", "samples", "window", "scores"))
        if not isinstance(graph, str) or not isinstance(samples, int) or not isinstance(window, int) \
                or not isinstance(scores, list):
            raise ValueError("graph_sha256, samples, window or scores missing")
        parsed = []
        for pair in scores:
            if not (isinstance(pair, list) and len(pair) == 2 and isinstance(pair[0], int)
                    and isinstance(pair[1], (int, float))):
                raise ValueError(f"bad window {pair!r}")
            parsed.append(WindowScore(pair[0], float(pair[1])))
        return cls(graph, samples, window, tuple(parsed))


def parse_stored(raw: str | None, what: str) -> ClipWindows | None:
    """A row's stored windows; None when it has none, or (logged) when they cannot be read."""
    if raw is None:
        return None
    try:
        return ClipWindows.from_json(raw)
    except ValueError as exc:
        log.warning("[windows] %s: unreadable windows ignored: %s", what, exc)
        return None


@dataclass(frozen=True, slots=True)
class CropSpec:
    """What an archive cuts from each clip: `samples` long, centred on the window that
    scored highest (`window_min` None), or on every window scoring at or above `window_min`."""

    samples: int
    window_min: float | None = None


@dataclass(frozen=True, slots=True)
class Crop:
    start: int      # [start, end) samples of the clip
    end: int
    score: float    # the score of the window it is centred on


@dataclass(frozen=True, slots=True)
class Clip:
    """One saved clip: what kind and whose row it is, its download name when whole (it ends in
    .wav), its file, the graph that scored it on the Echo (None: not recorded), and its
    windows (None: not scored yet)."""

    kind: ClipKind
    row_id: int
    name: str
    path: Path
    graph_sha256: str | None
    windows: ClipWindows | None = None


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


def plan(windows: ClipWindows, crop: CropSpec) -> list[Crop]:
    """The crops cut from a clip with these windows, in clip order: one centred on the best
    window, or one centred on each window scoring at or above `crop.window_min`. A crop that
    would run past an end of the clip moves inside it, and a clip shorter than the crop is
    taken whole. Two windows whose crops land on the same samples give one crop, with the
    higher score."""
    n = windows.samples
    if crop.window_min is None:
        chosen = [windows.best] if windows.best is not None else []
    else:
        chosen = [s for s in windows.scores if s.score >= crop.window_min]
    length = min(crop.samples, n)
    crops: dict[int, Crop] = {}
    for s in chosen:
        start = min(max(s.start + windows.window // 2 - length // 2, 0), n - length)
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
    """Scores saved clips with the registry's graphs, keeping each graph's session."""

    def __init__(self, registry: em_wake_registry.WakeRegistry,
                 load: Callable[[Path, BcresnetSpec], Infer] = _load_onnx) -> None:
        self.registry = registry
        self._load = load
        self._models: dict[str, tuple[Infer, BcresnetSpec]] = {}

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

    def score(self, path: Path, graph_sha256: str | None) -> ClipWindows:
        """A clip's windows, with the graph graph_for picks."""
        sha = self.graph_for(graph_sha256)
        pcm = read_pcm(path)
        infer, spec = self._model(sha)
        return ClipWindows(sha, len(pcm), spec.window, score_windows(pcm, infer, spec))


# What a clip that cannot be scored raises, short of a missing graph: gone since it was
# listed, unreadable, or a graph that fails on it.
_UNSCORABLE = (OSError, EOFError, ValueError, wave.Error)


class ClipWindowsService:
    """Scores every saved clip once and keeps its windows on its row (`run`), and cuts the
    archives' crops (`plan_archive`). Its methods that touch files, graphs or its own state
    run on EXECUTOR."""

    def __init__(self, registry: em_wake_registry.WakeRegistry,
                 load: Callable[[Path, BcresnetSpec], Infer] = _load_onnx,
                 db_path: str | None = None) -> None:
        self.scorer = ClipScorer(registry, load)
        self._db_path = db_path
        # Clips on disk whose rows need no more looking at: scored, without a row, or
        # unscorable (tried again after a restart).
        self._checked: set[tuple[ClipKind, int]] = set()
        self._missing_graph_logged = False
        # Saved clips the current sweep has still to score; None before the first sweep.
        self.pending: int | None = None

    async def run(self, interval: float = SWEEP_INTERVAL_S) -> None:
        while True:
            try:
                await self.sweep()
            except Exception:
                log.exception("[windows] sweep failed")
            await asyncio.sleep(interval)

    async def sweep(self) -> int:
        """Score every saved clip on disk that has no windows yet, newest first, and keep the
        windows on its row. Returns how many it scored."""
        loop = asyncio.get_running_loop()
        todo = await loop.run_in_executor(EXECUTOR, self._todo)
        self.pending = len(todo)
        scored = 0
        for clip in todo:
            try:
                scored += await loop.run_in_executor(EXECUTOR, self._score_and_keep, clip)
            except em_wake_registry.RegistryError as exc:
                if not self._missing_graph_logged:
                    log.warning("[windows] no wake model to score saved clips with: %s", exc)
                    self._missing_graph_logged = True
                return scored             # tried again next sweep
            self.pending -= 1
        return scored

    def _on_disk(self) -> dict[tuple[ClipKind, int], tuple[str, Path]]:
        """Every saved clip on disk: (kind, row id) → (its device's safe id, its file)."""
        found: dict[tuple[ClipKind, int], tuple[str, Path]] = {}
        near = em_recordings.recordings_dir(self._db_path, kind=em_recordings.RecordingKind.NEAR_MISS)
        if near.is_dir():
            for child in near.iterdir():
                if (parsed := em_recordings.parse_filename(child.name)) is not None:
                    found[(ClipKind.NEAR_MISS, parsed[1])] = (parsed[0], child)
        wakes = em_wakeclips.wakes_dir(self._db_path)
        if wakes.is_dir():
            for device in wakes.iterdir():
                if device.is_dir():
                    for child in device.iterdir():
                        if (turn := em_wakeclips.parse_filename(child.name)) is not None:
                            found[(ClipKind.WAKE, turn)] = (device.name, child)
        return found

    def _todo(self) -> list[Clip]:
        """The clips on disk not yet checked whose rows have no windows, newest first."""
        new = {key: v for key, v in self._on_disk().items() if key not in self._checked}
        rows = [(ClipKind.WAKE, r) for r in db.get_unscored_wake_clips(
                    [i for kind, i in new if kind is ClipKind.WAKE])] \
            + [(ClipKind.NEAR_MISS, r) for r in db.get_unscored_near_misses(
                    [i for kind, i in new if kind is ClipKind.NEAR_MISS])]
        unscored = {(kind, r.row_id) for kind, r in rows}
        self._checked.update(key for key in new if key not in unscored)   # scored, or no row
        todo: list[tuple[float, Clip]] = []
        for kind, r in rows:
            safe, path = new[(kind, r.row_id)]
            if em_recordings.safe_device_id(r.device_id) != safe:         # another device's file
                self._checked.add((kind, r.row_id))
                continue
            todo.append((r.ts, Clip(kind, r.row_id, path.name, path, r.wake_model_sha256)))
        return [clip for _, clip in sorted(todo, key=lambda t: t[0], reverse=True)]

    def _keep(self, clip: Clip, windows: ClipWindows) -> None:
        if clip.kind is ClipKind.WAKE:
            db.set_turn_wake_windows(clip.row_id, windows.to_json())
        else:
            db.set_near_miss_windows(clip.row_id, windows.to_json())

    def _score_and_keep(self, clip: Clip) -> bool:
        """Score one clip and keep its windows; False (logged unless it was pruned) when it
        cannot be scored. RegistryError when there is no graph to score it with."""
        try:
            windows = self.scorer.score(clip.path, clip.graph_sha256)
        except FileNotFoundError:
            windows = None
        except _UNSCORABLE as exc:
            log.warning("[windows] %s %d not scored: %s", clip.kind, clip.row_id, exc)
            windows = None
        self._checked.add((clip.kind, clip.row_id))
        if windows is None:
            return False
        self._keep(clip, windows)
        return True

    def plan_archive(self, clips: Sequence[Clip], crop: CropSpec) -> list[tuple[str, Path, Crop]]:
        """(download name, clip file, crop) of every crop `crop` cuts from `clips`, in their
        order. A clip not scored yet is scored now, and its windows kept. A clip pruned since
        it was listed is left out, and so is one that cannot be scored (logged).
        RegistryError when a clip has no graph to score it with. Run it on EXECUTOR."""
        planned: list[tuple[str, Path, Crop]] = []
        for clip in clips:
            windows = clip.windows
            if windows is None:
                try:
                    windows = self.scorer.score(clip.path, clip.graph_sha256)
                except FileNotFoundError:
                    continue
                except _UNSCORABLE as exc:
                    log.warning("[windows] %s %d not scored: %s", clip.kind, clip.row_id, exc)
                    continue
                self._keep(clip, windows)
            planned.extend((crop_name(clip.name, c), clip.path, c) for c in plan(windows, crop))
        return planned
