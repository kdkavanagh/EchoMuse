"""Per-cell attribution and reference comparison (SPEC §6.1, §8.2, §16.6).

Positions are capture-epoch sample indices at 16 kHz. Cells are the VAD's
non-overlapping 512-sample (32 ms) cells: cell k covers [512k, 512k + 512).
PCM is canonical native-AFE int16 (never the evidence copy). Levels are `E`
in dB of `x = int16 / 32768`. Unavailable measurements are None (NaN inside
float arrays), never 0.

Reference arrays are in mic time under the clock estimate and start MAX_LAG
samples before the mic array they are compared with: `reference[i]` is the
final-mix sample the clock estimate places at mic sample `mic_first - MAX_LAG + i`.
"""

from __future__ import annotations

import enum
from collections import deque
from dataclasses import dataclass
from typing import Sequence

import numpy as np

SAMPLE_RATE = 16_000
CELL = 512


class CellClass(enum.StrEnum):
    """Cell classes, §8.2 order."""

    GAP = "gap"
    SELF_OUTPUT = "self_output"
    NON_SPEECH = "non_speech"
    BACKGROUND_SPEECH = "background_speech"
    COMMAND_SPEECH = "command_speech"
    UNKNOWN = "unknown"


class Rule(enum.StrEnum):
    """Deciding rules recorded in the decision trace (§8.2); gap cells record why they are gaps."""

    GAP = "gap"
    MUTE = "mute"
    ECHO = "echo"
    VAD = "vad"
    LEVEL = "level"
    HYSTERESIS = "hysteresis"


class EchoResult(enum.StrEnum):
    """Reference comparison results (§16.6); NO_REFERENCE is per-cell only."""

    ECHO_ONLY = "echo_only"
    NEAR_END_PRESENT = "near_end_present"
    NO_REFERENCE = "no_reference"
    UNKNOWN = "unknown"


class Coverage(enum.StrEnum):
    """render.progress reference_coverage values."""

    FULL = "full"
    PARTIAL = "partial"


class PlaybackVerdict(enum.StrEnum):
    """Why a mic candidate is rejected while the device produces sound (§6.1 steps 1–2)."""

    SELF_OUTPUT = "self_output"
    ECHO_ONLY = "echo_only"


# §16.6 activity and level.
BACKGROUND_WINDOW = 10 * SAMPLE_RATE
BACKGROUND_MIN_CELLS = -(-SAMPLE_RATE // CELL)  # 1 s of support, whole cells
BACKGROUND_PERCENTILE = 20.0
BACKGROUND_VAD_EXCLUDE = 0.35
SPEECH_POSITIVE_VAD = 0.50
FOREGROUND_PERCENTILE = 80.0
FOREGROUND_MIN_CELLS = 10
SEED_VAD = 0.35

# §8.2 / §16.6 classes.
NON_SPEECH_MAX_VAD = 0.35
RUN_OPEN_VAD = 0.65
RUN_OPEN_R = 0.55
BACKGROUND_MARGIN_DB = 10.0
R_MIN_SPAN_DB = 12.0

# §16.6 reference comparison.
MAX_LAG = SAMPLE_RATE // 2  # 0–500 ms beyond the clock estimate
PEAK_EXCLUSION = SAMPLE_RATE // 50  # ±20 ms
PEAK_MARGIN = 0.10
MIN_REFERENCE_CELLS = 6
SILENT_REFERENCE_DB = -60.0
MIN_VALID_FRACTION = 0.80
ECHO_CORRELATION = 0.92
ECHO_UNEXPLAINED_RATIO = 0.20
ECHO_CELL_FRACTION = 0.90
NEAR_END_MARGIN_DB = 6.0

# §16.6 per-cell echo comparison.
ECHO_WINDOW_CELLS = 6
LAG_ESTIMATE_CELLS = -(-SAMPLE_RATE // CELL)  # 1 s, whole cells
LAG_REESTIMATE = SAMPLE_RATE

# §16.6 early answer and reply onset.
EARLY_ANSWER_CELLS = 15
EARLY_ANSWER_VAD = 0.85
EARLY_ANSWER_MARGIN_DB = 12.0
ONSET_CELLS = 8
ONSET_VAD = 0.85
ONSET_QUIET_CELLS = 10
ONSET_SCAN_BACK = SAMPLE_RATE  # scan from drain − 1,000 ms
ONSET_EARLIEST_BACK = 7_680  # run starts no earlier than drain − 480 ms

_EPS = 1e-12


@dataclass(frozen=True, slots=True)
class Cell:
    """Evidence for one 32 ms cell.

    `level` is the device's `E` (None for a gap cell), `vad` the controller's
    Silero probability (None before the lease), `echo` the per-cell echo result
    (None when not computed).
    """

    start: int
    level: float | None
    vad: float | None = None
    echo: EchoResult | None = None
    gap: bool = False
    muted: bool = False

    @property
    def end(self) -> int:
        return self.start + CELL

    @property
    def valid(self) -> bool:
        return self.valid_level is not None

    @property
    def valid_level(self) -> float | None:
        """`level` when the cell is valid evidence (not a gap, not muted, measured), else None."""
        return None if self.gap or self.muted else self.level

    @property
    def speech_positive(self) -> bool:
        return self.valid and self.vad is not None and self.vad >= SPEECH_POSITIVE_VAD


@dataclass(frozen=True, slots=True)
class ClassifiedCell:
    """One classified cell. `command` is command speech proper: `command_speech` ending after the trigger."""

    start: int
    cls: CellClass
    rule: Rule
    speech_positive: bool
    command: bool
    background: float | None = None
    foreground: float | None = None
    ratio: float | None = None

    @property
    def end(self) -> int:
        return self.start + CELL


@dataclass(frozen=True, slots=True)
class TraceSegment:
    """Run-length decision-trace segment [start, end) (§8.2, §11.3)."""

    cls: CellClass
    rule: Rule
    start: int
    end: int

    @property
    def cells(self) -> int:
        return (self.end - self.start) // CELL


@dataclass(frozen=True, slots=True)
class WakeHop:
    """One scored hop of a device wake candidate (WIRE `wake.candidate.hops[]`); None = not scored."""

    end_sample: int
    raw: float | None
    smoothed: float | None = None


def wake_trigger_sample(hops: Sequence[WakeHop], threshold: float) -> int:
    """§16.6 trigger: end of the last WIRE hop whose raw value reaches the latched threshold."""
    ends: list[int] = []
    for hop in hops:
        if hop.raw is not None and hop.raw >= threshold:
            ends.append(hop.end_sample)
    if not ends:
        raise ValueError("no hop reached the latched threshold")
    return ends[-1]


class BackgroundTracker:
    """Rolling room floor `B` for one lease's cell stream (§16.6).

    Query `value(cell.start)` then `observe(cell)` for every cell in order, once
    that cell's VAD is final (None for cells uploaded from before the lease).
    Reset on capture-epoch change, discontinuity, or mute.
    """

    def __init__(self) -> None:
        self._cells: deque[tuple[int, float]] = deque()
        self._next: int | None = None

    def reset(self) -> None:
        self._cells.clear()
        self._next = None

    def value(self, start: int) -> float | None:
        """B for the cell starting at `start`, over the valid cells of the preceding 10 s."""
        lo = start - BACKGROUND_WINDOW
        while self._cells and self._cells[0][0] < lo:
            self._cells.popleft()
        if len(self._cells) < BACKGROUND_MIN_CELLS:
            return None
        return float(np.percentile([level for _, level in self._cells], BACKGROUND_PERCENTILE))

    def observe(self, cell: Cell) -> None:
        if self._next is not None and cell.start < self._next:
            raise ValueError("cells must be observed in sample order")
        self._next = cell.end
        level = cell.valid_level
        if level is None:
            return
        if cell.vad is not None and cell.vad >= BACKGROUND_VAD_EXCLUDE:
            return
        self._cells.append((cell.start, float(level)))


class Attributor:
    """Classifies one utterance's cells in sample order (§8.2, §16.6).

    `trigger_sample` is the utterance's trigger (§16.6). `seed_start` is the
    candidate's `support_start` for a wake turn and None for button and reply
    turns. Each cell is classified with the `F` in force before it; earlier
    cells are never relabelled.
    """

    def __init__(self, *, trigger_sample: int, seed_start: int | None = None) -> None:
        self._trigger = trigger_sample
        self._seed_start = seed_start
        self._seed: list[float] | None = [] if seed_start is not None else None
        self._foreground: list[float] = []
        self._f: float | None = None
        self._in_run = False
        self._next: int | None = None
        self._trace: list[TraceSegment] = []

    @property
    def foreground(self) -> float | None:
        """Current `F`, or None until it has support."""
        return self._f

    @property
    def trace(self) -> list[TraceSegment]:
        return list(self._trace)

    def _add_foreground(self, level: float) -> None:
        self._foreground.append(level)
        if len(self._foreground) >= FOREGROUND_MIN_CELLS:
            self._f = float(np.percentile(self._foreground, FOREGROUND_PERCENTILE))

    def _close_seed(self) -> None:
        seed, self._seed = self._seed, None
        if seed is not None and len(seed) >= FOREGROUND_MIN_CELLS:
            for level in seed:
                self._add_foreground(level)

    def classify(self, cell: Cell, background: float | None) -> ClassifiedCell:
        if self._next is not None and cell.start != self._next:
            raise ValueError("cells must be contiguous and in sample order")
        self._next = cell.end
        if self._seed is not None and cell.end > self._trigger:
            self._close_seed()

        f_before = self._f
        level = cell.valid_level
        ratio = None
        if level is not None and background is not None and f_before is not None:
            ratio = (level - background) / max(f_before - background, R_MIN_SPAN_DB)

        cls, rule = self._decide(cell, background)
        command = cls == CellClass.COMMAND_SPEECH and cell.end > self._trigger
        if command and level is not None:
            self._add_foreground(float(level))
        if (
            self._seed is not None
            and self._seed_start is not None
            and level is not None
            and cell.start >= self._seed_start
            and cell.end <= self._trigger
            and cell.vad is not None
            and cell.vad >= SEED_VAD
        ):
            self._seed.append(float(level))

        last = self._trace[-1] if self._trace else None
        if last is not None and last.cls == cls and last.rule == rule and last.end == cell.start:
            self._trace[-1] = TraceSegment(cls, rule, last.start, cell.end)
        else:
            self._trace.append(TraceSegment(cls, rule, cell.start, cell.end))
        return ClassifiedCell(
            cell.start,
            cls,
            rule,
            cell.speech_positive,
            command,
            background,
            f_before,
            ratio,
        )

    def _decide(self, cell: Cell, background: float | None) -> tuple[CellClass, Rule]:
        f = self._f
        level = cell.valid_level
        if level is None:
            self._in_run = False
            return CellClass.GAP, Rule.MUTE if cell.muted else Rule.GAP
        if cell.echo == EchoResult.ECHO_ONLY:
            self._in_run = False
            return CellClass.SELF_OUTPUT, Rule.ECHO
        vad = cell.vad
        if vad is None:
            self._in_run = False
            return CellClass.UNKNOWN, Rule.VAD
        if vad <= NON_SPEECH_MAX_VAD:
            self._in_run = False
            return CellClass.NON_SPEECH, Rule.VAD
        if vad >= SPEECH_POSITIVE_VAD and f is not None and level <= f - BACKGROUND_MARGIN_DB:
            self._in_run = False
            return CellClass.BACKGROUND_SPEECH, Rule.LEVEL
        if self._in_run:
            return CellClass.COMMAND_SPEECH, Rule.HYSTERESIS
        if vad >= RUN_OPEN_VAD:
            r = None
            if background is not None and f is not None:
                r = (level - background) / max(f - background, R_MIN_SPAN_DB)
            if r is None:
                self._in_run = True
                return CellClass.COMMAND_SPEECH, Rule.VAD
            if r >= RUN_OPEN_R:
                self._in_run = True
                return CellClass.COMMAND_SPEECH, Rule.LEVEL
            return CellClass.UNKNOWN, Rule.LEVEL
        return CellClass.UNKNOWN, Rule.VAD


# --- Reference comparison ---------------------------------------------------


def _pcm(samples: np.ndarray) -> np.ndarray:
    return np.asarray(samples, dtype=np.float64) / 32768.0


def _optional(values: Sequence[float | None] | np.ndarray, n: int, name: str) -> np.ndarray:
    out = np.array([np.nan if v is None else v for v in values], dtype=np.float64)
    if out.shape != (n,):
        raise ValueError(f"{name} must have one entry per cell")
    return out


def _check_layout(mic: np.ndarray, reference: np.ndarray, cell_valid: np.ndarray) -> int:
    n = len(cell_valid)
    if len(mic) != n * CELL:
        raise ValueError("mic must hold exactly one 512-sample block per cell")
    if len(reference) != n * CELL + MAX_LAG:
        raise ValueError("reference must start MAX_LAG samples before the mic")
    return n


def _reference_valid(reference_valid: np.ndarray | None, length: int) -> np.ndarray:
    if reference_valid is None:
        return np.ones(length, dtype=bool)
    out = np.asarray(reference_valid, dtype=bool)
    if out.shape != (length,):
        raise ValueError("reference_valid must match reference")
    return out


def _level_db(x: np.ndarray, axis: int | None = None) -> np.ndarray:
    return 10.0 * np.log10(np.mean(x * x, axis=axis) + _EPS)


def _xcorr(a: np.ndarray, b: np.ndarray) -> np.ndarray:
    """c[o] = Σ_i a[i]·b[i+o] for o in [0, len(b) − len(a)]."""
    size = 1 << int(len(a) + len(b)).bit_length()
    c = np.fft.irfft(np.conj(np.fft.rfft(a, size)) * np.fft.rfft(b, size), size)
    return c[: len(b) - len(a) + 1]


@dataclass(frozen=True)
class CellStats:
    """Per-cell comparison at one lag (arrays, one entry per cell)."""

    valid: np.ndarray
    reference_active: np.ndarray
    correlation: np.ndarray
    unexplained_ratio: np.ndarray
    unexplained_db: np.ndarray


def _lagged(reference: np.ndarray, lag: int, n: int) -> np.ndarray:
    offset = MAX_LAG - lag
    return reference[offset : offset + n * CELL]


def estimate_lag(
    mic: np.ndarray,
    reference: np.ndarray,
    cell_valid: Sequence[bool] | np.ndarray,
    reference_valid: np.ndarray | None = None,
) -> int | None:
    """Echo lag in samples beyond the clock estimate (0…MAX_LAG), or None when not accepted.

    Mean-subtracted normalized cross-correlation over valid mic/reference
    samples. Accepts the peak only with ≥6 non-silent reference cells at that
    lag and a 0.10 margin over the best lag outside ±20 ms.
    """
    cell_valid = np.asarray(cell_valid, dtype=bool)
    n = _check_layout(mic, reference, cell_valid)
    if cell_valid.sum() < MIN_REFERENCE_CELLS:
        return None

    x = _pcm(mic)
    y = _pcm(reference)
    m = np.repeat(cell_valid, CELL).astype(np.float64)
    rv = _reference_valid(reference_valid, len(reference)).astype(np.float64)

    # Sliding sufficient statistics let every lag use exactly the reference
    # samples valid at that lag without an 8,001-lag Python loop.
    count = _xcorr(m, rv)
    sx = _xcorr(x * m, rv)
    sxx = _xcorr(x * x * m, rv)
    sy = _xcorr(m, y * rv)
    syy = _xcorr(m, y * y * rv)
    sxy = _xcorr(x * m, y * rv)
    with np.errstate(invalid="ignore", divide="ignore"):
        cov = sxy - sx * sy / count
        var_x = sxx - sx * sx / count
        var_y = syy - sy * sy / count
        ncc = np.where(
            (count >= MIN_REFERENCE_CELLS * CELL) & (var_x > _EPS) & (var_y > _EPS),
            cov / np.sqrt(np.maximum(var_x, _EPS) * np.maximum(var_y, _EPS)),
            np.nan,
        )
    ncc = ncc[::-1]  # index by lag: offset o ↔ lag MAX_LAG − o
    if np.all(np.isnan(ncc)):
        return None
    lag = int(np.nanargmax(ncc))
    peak = ncc[lag]
    lags = np.arange(len(ncc))
    outside = ncc[np.abs(lags - lag) > PEAK_EXCLUSION]
    second = np.nanmax(outside) if outside.size and not np.all(np.isnan(outside)) else -1.0
    if peak - second < PEAK_MARGIN:
        return None

    lagged = _lagged(y, lag, n).reshape(n, CELL)
    lagged_ok = _lagged(rv.astype(bool), lag, n).reshape(n, CELL).all(axis=1)
    active = _level_db(lagged, axis=1) >= SILENT_REFERENCE_DB
    if (active & cell_valid & lagged_ok).sum() < MIN_REFERENCE_CELLS:
        return None
    return lag


def compare_cells(
    mic: np.ndarray,
    reference: np.ndarray,
    lag: int,
    cell_valid: Sequence[bool] | np.ndarray,
    reference_valid: np.ndarray | None = None,
) -> CellStats:
    """Per-cell fit `a = dot(mic, ref)/dot(ref, ref)` at `lag`, with correlation and unexplained energy."""
    if not 0 <= lag <= MAX_LAG:
        raise ValueError("lag outside 0…MAX_LAG")
    cell_valid = np.asarray(cell_valid, dtype=bool)
    n = _check_layout(mic, reference, cell_valid)
    ref_ok = _lagged(_reference_valid(reference_valid, len(reference)), lag, n).reshape(n, CELL).all(axis=1)
    x = _pcm(mic).reshape(n, CELL)
    r = _lagged(_pcm(reference), lag, n).reshape(n, CELL)

    rr = (r * r).sum(axis=1)
    a = np.divide((x * r).sum(axis=1), rr, out=np.zeros(n), where=rr > 0)
    resid = x - a[:, None] * r
    ratio = (resid * resid).sum(axis=1) / np.maximum((x * x).sum(axis=1), _EPS)
    xc = x - x.mean(axis=1, keepdims=True)
    rc = r - r.mean(axis=1, keepdims=True)
    denom = np.sqrt((xc * xc).sum(axis=1) * (rc * rc).sum(axis=1))
    corr = np.divide((xc * rc).sum(axis=1), denom, out=np.full(n, np.nan), where=denom > 0)
    return CellStats(
        valid=cell_valid & ref_ok,
        reference_active=_level_db(r, axis=1) >= SILENT_REFERENCE_DB,
        correlation=corr,
        unexplained_ratio=ratio,
        unexplained_db=_level_db(resid, axis=1),
    )


def judge(
    stats: CellStats,
    vad: Sequence[float | None] | np.ndarray,
    background: Sequence[float | None] | np.ndarray,
    coverage: Coverage,
) -> EchoResult:
    """The §16.6 three-way result over compared cells at an accepted lag."""
    n = len(stats.valid)
    vad_a = _optional(vad, n, "vad")
    b = _optional(background, n, "background")
    valid = stats.valid
    if coverage != Coverage.FULL or n == 0 or valid.sum() < MIN_VALID_FRACTION * n:
        return EchoResult.UNKNOWN
    if np.isnan(b[valid]).any():
        return EchoResult.UNKNOWN
    with np.errstate(invalid="ignore"):
        speech = valid & (vad_a >= SPEECH_POSITIVE_VAD)
        loud = speech & (stats.unexplained_db >= b + NEAR_END_MARGIN_DB)
        explained = speech & (stats.correlation >= ECHO_CORRELATION) & (
            stats.unexplained_ratio <= ECHO_UNEXPLAINED_RATIO
        )
    if (loud[:-1] & loud[1:]).any():
        return EchoResult.NEAR_END_PRESENT
    if explained.sum() >= ECHO_CELL_FRACTION * speech.sum():
        return EchoResult.ECHO_ONLY
    return EchoResult.UNKNOWN


@dataclass(frozen=True, slots=True)
class Comparison:
    result: EchoResult
    lag: int | None


def compare_reference(
    mic: np.ndarray,
    reference: np.ndarray,
    *,
    cell_valid: Sequence[bool] | np.ndarray,
    vad: Sequence[float | None] | np.ndarray,
    background: Sequence[float | None] | np.ndarray,
    coverage: Coverage,
    reference_valid: np.ndarray | None = None,
) -> Comparison:
    """Reference comparison over a mic candidate's support cells (§16.6)."""
    lag = estimate_lag(mic, reference, cell_valid, reference_valid)
    if lag is None:
        return Comparison(EchoResult.UNKNOWN, None)
    stats = compare_cells(mic, reference, lag, cell_valid, reference_valid)
    return Comparison(judge(stats, vad, background, coverage), lag)


def reference_candidate_overlaps(
    reference_start: int, reference_end: int, support_start: int, support_end: int, uncertainty: int
) -> bool:
    """Whether a reference-scorer candidate, mapped to mic time by the clock estimate, can overlap the mic support.

    The mapped interval widens by the 0–500 ms lag search and ±`uncertainty` samples.
    """
    lo = reference_start - uncertainty
    hi = reference_end + MAX_LAG + uncertainty
    return lo < support_end and support_start < hi


def self_playback_verdict(*, reference_candidate_overlaps: bool, comparison: EchoResult) -> PlaybackVerdict | None:
    """§6.1 steps 1–2 for a mic candidate while the device produces sound.

    Returns the rejection reason (`self_output`, `echo_only`), or None to continue
    to wake verification. `near_end_present` rescues double-talk in step 1 only.
    """
    if reference_candidate_overlaps and comparison != EchoResult.NEAR_END_PRESENT:
        return PlaybackVerdict.SELF_OUTPUT
    if comparison == EchoResult.ECHO_ONLY:
        return PlaybackVerdict.ECHO_ONLY
    return None


class EchoTracker:
    """Per-cell echo labels while an utterance or reply lease is open (§16.6).

    Lag is estimated at open from the preceding 1 s when the lease holds it,
    otherwise from the first 1 s of the lease, then re-estimated every second.
    A failed re-estimate keeps the established lag; without one, cells are `unknown`.
    """

    def __init__(self, *, open_sample: int, lease_mic_start: int) -> None:
        first = open_sample - open_sample % CELL
        span = LAG_ESTIMATE_CELLS * CELL
        self._end = first if first - span >= lease_mic_start else lease_mic_start + span
        self.lag: int | None = None

    @staticmethod
    def _following(end: int) -> int:
        """First cell boundary at least one second after `end`."""
        return -(-(end + LAG_REESTIMATE) // CELL) * CELL

    def estimate_window(self, frontier: int) -> tuple[int, int] | None:
        """Mic range [start, end) of whole cells to estimate the lag from, or None when none is due.

        A frontier that skipped several due points estimates only the latest.
        """
        if frontier < self._end:
            return None
        while self._following(self._end) <= frontier:
            self._end = self._following(self._end)
        return self._end - LAG_ESTIMATE_CELLS * CELL, self._end

    def update_lag(self, lag: int | None) -> None:
        """Record the estimate for the window last returned; None keeps the established lag."""
        if lag is not None:
            self.lag = lag
        self._end = self._following(self._end)

    def label(
        self,
        mic: np.ndarray,
        reference: np.ndarray,
        *,
        cell_valid: Sequence[bool] | np.ndarray,
        vad: Sequence[float | None] | np.ndarray,
        background: Sequence[float | None] | np.ndarray,
        coverage: Coverage,
        reference_valid: np.ndarray | None = None,
    ) -> EchoResult:
        """Label the last of 1–6 trailing cells.

        `no_reference` when the reference over that cell's 0–500 ms lag span is
        below −60 dBFS; `unknown` when that silence cannot be trusted (partial
        coverage or missing reference, §3.2 inv. 7) or no lag is established.
        """
        cell_valid = np.asarray(cell_valid, dtype=bool)
        n = _check_layout(mic, reference, cell_valid)
        if not 1 <= n <= ECHO_WINDOW_CELLS:
            raise ValueError("label takes the trailing 1–6 cells")
        ref_ok = _reference_valid(reference_valid, len(reference))
        span = slice((n - 1) * CELL, n * CELL + MAX_LAG)
        if not ref_ok[span].all():
            return EchoResult.UNKNOWN
        if _level_db(_pcm(reference[span])) < SILENT_REFERENCE_DB:
            return EchoResult.NO_REFERENCE if coverage == Coverage.FULL else EchoResult.UNKNOWN
        if self.lag is None:
            return EchoResult.UNKNOWN
        stats = compare_cells(mic, reference, self.lag, cell_valid, reference_valid)
        return judge(stats, vad, background, coverage)


class EarlyAnswerDetector:
    """§16.6 early answer during a prompt carrying a reply expectation.

    Push every cell of the reply lease with its `B`; returns the onset (first
    cell of 15 consecutive qualifying cells) once, when the run completes.
    """

    def __init__(self) -> None:
        self._run_start: int | None = None
        self._run = 0
        self._next: int | None = None
        self.onset: int | None = None

    def push(self, cell: Cell, background: float | None) -> int | None:
        if self.onset is not None:
            return None
        if self._next is not None and cell.start != self._next:
            self._run = 0
        self._next = cell.end
        level = cell.valid_level
        qualifies = (
            level is not None
            and cell.vad is not None
            and cell.vad >= EARLY_ANSWER_VAD
            and background is not None
            and level >= background + EARLY_ANSWER_MARGIN_DB
            and cell.echo in (EchoResult.NEAR_END_PRESENT, EchoResult.NO_REFERENCE)
        )
        if not qualifies:
            self._run = 0
            return None
        if self._run == 0:
            self._run_start = cell.start
        self._run += 1
        if self._run >= EARLY_ANSWER_CELLS:
            self.onset = self._run_start
            return self.onset
        return None


class ReplyOnsetScanner:
    """§16.6 reply onset after the guarded drain (or expectation start without prompt audio).

    Push retained cells in order; cells starting before drain − 1,000 ms are
    ignored. The onset is the first run of 8 consecutive cells with VAD ≥0.85
    that are not `self_output`, starting no earlier than drain − 480 ms, and
    immediately preceded by ≥10 `non_speech`/`self_output` cells.
    """

    def __init__(self, drain_sample: int) -> None:
        self._scan_from = drain_sample - ONSET_SCAN_BACK
        self._earliest = drain_sample - ONSET_EARLIEST_BACK
        self._next: int | None = None
        self._quiet = 0
        self._speech = 0
        self._speech_start = 0
        self._quiet_before = 0
        self.onset: int | None = None

    def push(self, cell: Cell) -> int | None:
        if self.onset is not None or cell.start < self._scan_from:
            return None
        if self._next is not None and cell.start != self._next:
            self._quiet = self._speech = 0
        self._next = cell.end
        own_echo = cell.echo == EchoResult.ECHO_ONLY
        speech = cell.valid and not own_echo and cell.vad is not None and cell.vad >= ONSET_VAD
        quiet = cell.valid and (own_echo or (cell.vad is not None and cell.vad <= NON_SPEECH_MAX_VAD))
        if speech:
            if self._speech == 0:
                self._speech_start = cell.start
                self._quiet_before = self._quiet
            self._speech += 1
            self._quiet = 0
            if (
                self._speech == ONSET_CELLS
                and self._speech_start >= self._earliest
                and self._quiet_before >= ONSET_QUIET_CELLS
            ):
                self.onset = self._speech_start
                return self.onset
        elif quiet:
            self._quiet += 1
            self._speech = 0
        else:
            self._quiet = self._speech = 0
        return None
