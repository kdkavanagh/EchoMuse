"""Per-utterance evidence composition and endpoint decisions (SPEC §7, §8, §9, §16.2, §16.6).

Pure: no I/O, no clocks. The session actor feeds device cell records and
speech-worker observations in sample order; this module joins them into
classified cells, runs the endpoint reducer only at the evidence frontier, and
returns decisions. Positions are capture-epoch sample indices at 16 kHz.

- `CellAssembler` (one per uplink lease) joins device `E`/flags, controller VAD
  and per-cell echo labels into `em_attribution.Cell`s with the room floor `B`.
- `Utterance` (one per utterance) classifies cells (`Attributor`), tracks
  streaming-text stability over the wake-cut text, and evaluates
  `EndpointReducer` after each 80 ms block once cells and ASR both reached it.
- `ReplyWatch` (one per reply expectation) finds an early answer during the
  prompt or the reply onset after the guarded drain.
- `local_action` is the §16.2 local-command gate plus grammar match.
"""

from __future__ import annotations

import uuid
from collections import deque
from dataclasses import dataclass, field
from typing import Iterable, Literal, Sequence

import echomuse_grammar
from echomuse_grammar import Choice, CommandContext
from em_attribution import (
    CELL,
    COMMAND_SPEECH,
    Attributor,
    BackgroundTracker,
    Cell,
    ClassifiedCell,
    EarlyAnswerDetector,
    ReplyOnsetScanner,
)
from em_audio_timeline import CELL_FLAG_GAP, CELL_FLAG_MUTED
from em_endpoint_policy import (
    EndpointReducer,
    StableText,
    TextStability,
    local_command_preconditions,
)
from em_wake_phrase import (
    LOOKAHEAD_SAMPLES,
    StreamingTranscript,
    Window,
    command_text,
    locate_streaming,
)

SAMPLE_RATE = 16_000
BLOCK = 1_280          # one 80 ms capture block; the reducer's evaluation grid (§16.6)
PREROLL = 4_800        # 300 ms before the trigger/onset (§8.1)
RETAIN_CELLS = 10 * SAMPLE_RATE // CELL + 1   # retained evidence for B, onset and echo windows

UtteranceKind = Literal["wake", "button", "reply", "ha_reply"]


# --- Lease-level evidence ----------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Evidence:
    """One cell's joined evidence and the `B` in force before it (None = unavailable)."""

    cell: Cell
    background: float | None

    @property
    def start(self) -> int:
        return self.cell.start

    @property
    def end(self) -> int:
        return self.cell.end


class CellAssembler:
    """Joins one lease's device cells, VAD and echo labels in sample order (§8.1, §16.6).

    Invariants: cells are released strictly in order and exactly once; a cell is
    complete when its device record is known and, if it holds lease mic audio,
    its VAD is known, and, from `echo_from` on, its echo label is known. Gap and
    muted cells (device-flagged or marked by the actor for a transport hole) need
    neither. Cells before the lease's first mic sample have no VAD and still count
    toward `B` (§16.6).
    """

    def __init__(self) -> None:
        self._records: dict[int, tuple[float, int]] = {}
        self._vad: dict[int, float] = {}
        self._echo: dict[int, str] = {}
        self._gaps: list[tuple[int, int]] = []
        self._next: int | None = None
        self._vad_from: int | None = None
        self._echo_from: int | None = None
        self._echo_requested = -1   # end of the last cell handed out for echo labelling
        self._tracker = BackgroundTracker()
        self._staged: deque[Evidence] = deque()   # have E/VAD/B, may await echo
        self.history: deque[Evidence] = deque(maxlen=RETAIN_CELLS)

    @property
    def frontier(self) -> int | None:
        """End of the last released cell."""
        return self.history[-1].end if self.history else None

    @property
    def vad_from(self) -> int | None:
        return self._vad_from

    def set_vad_from(self, first_mic_sample: int) -> None:
        """First mic sample of the lease: VAD covers whole cells from here on."""
        if self._vad_from is None:
            self._vad_from = -(-first_mic_sample // CELL) * CELL

    def require_echo_from(self, sample: int | None) -> None:
        """Cells starting at or after `sample` wait for an echo label; None stops requiring it."""
        self._echo_from = None if sample is None else sample - sample % CELL

    def add_cells(self, first_sample: int, e_db: Sequence[float], flags: Sequence[int]) -> None:
        if first_sample % CELL:
            raise ValueError("cell records start on the 512-sample grid")
        for i, (e, f) in enumerate(zip(e_db, flags)):
            start = first_sample + i * CELL
            if self._next is None:
                self._next = start
            if start >= self._next:
                self._records.setdefault(start, (float(e), int(f)))

    def add_vad(self, first_cell_sample: int, probabilities: Sequence[float]) -> None:
        for i, p in enumerate(probabilities):
            self._vad[first_cell_sample + i * CELL] = float(p)

    def add_echo(self, first_cell_sample: int, results: Sequence[str]) -> None:
        for i, r in enumerate(results):
            self._echo[first_cell_sample + i * CELL] = r

    def mark_gap(self, start: int, end: int) -> None:
        """Unknown audio in [start, end): every overlapping cell becomes a gap (§3.2 inv. 5)."""
        if end > start:
            self._gaps.append((start, end))

    def _in_gap(self, start: int) -> bool:
        return any(a < start + CELL and start < b for a, b in self._gaps)

    def _stage(self) -> None:
        while self._next is not None and self._next in self._records:
            start = self._next
            level, flags = self._records[start]
            gap = bool(flags & CELL_FLAG_GAP) or self._in_gap(start)
            muted = bool(flags & CELL_FLAG_MUTED)
            vad = None
            if not gap and not muted:
                if self._vad_from is None:
                    return
                if start >= self._vad_from:
                    if start not in self._vad:
                        return
                    vad = self._vad[start]
            cell = Cell(start, None if gap or muted else level, vad, None, gap, muted)
            background = self._tracker.value(start)
            self._tracker.observe(cell)
            self._staged.append(Evidence(cell, background))
            del self._records[start]
            self._vad.pop(start, None)
            self._next = start + CELL

    def echo_pending(self) -> list[Evidence]:
        """Staged valid cells that need an echo label and were not yet requested, in order."""
        self._stage()
        return [ev for ev in self._staged
                if ev.end > self._echo_requested and self._echo_from is not None
                and ev.start >= self._echo_from and ev.cell.valid]

    def echo_requested(self, end: int) -> None:
        """Echo labels for cells ending at or before `end` were handed out."""
        self._echo_requested = max(self._echo_requested, end)

    def release(self) -> list[Evidence]:
        """Complete cells in order; each is released once and retained in `history`."""
        self._stage()
        out = []
        while self._staged:
            ev = self._staged[0]
            echo = None
            if self._echo_from is not None and ev.start >= self._echo_from and ev.cell.valid:
                if ev.start not in self._echo:
                    break
                echo = self._echo.pop(ev.start)
            self._staged.popleft()
            if echo is not None:
                ev = Evidence(Cell(ev.cell.start, ev.cell.level, ev.cell.vad, echo, ev.cell.gap, ev.cell.muted),
                              ev.background)
            self.history.append(ev)
            out.append(ev)
        if self._gaps and self.history:
            lo = self.history[0].start
            self._gaps = [(a, b) for a, b in self._gaps if b > lo]
        return out

    def window(self, start: int, end: int) -> list[Evidence]:
        """Released evidence for cells in [start, end), in order (may be partial)."""
        return [ev for ev in self.history if start <= ev.start and ev.end <= end]

    def lookup(self, start: int, end: int) -> list[Evidence]:
        """Released or staged evidence for cells in [start, end), in order (may be partial)."""
        return [ev for ev in (*self.history, *self._staged) if start <= ev.start and ev.end <= end]


# --- Decisions ---------------------------------------------------------------------


@dataclass(frozen=True, slots=True)
class Pending:
    boundary: int
    since: int
    route: str


@dataclass(frozen=True, slots=True)
class Revoked:
    boundary: int
    route: str


@dataclass(frozen=True, slots=True)
class Commit:
    """The immutable committed span [start, end) and what it carries (§7 COMMITTED).

    `text` is the stable streaming command text (after the wake cut for wake
    turns); `wake_window` locates the wake phrase in the streaming transcript for
    the final-transcript strip; `local_action` is the §6.3 local command, if any.
    `decided_at` is the evidence frontier (sample) the reducer committed at, so
    `decided_at − boundary` is the silence it waited; `completeness` is the
    grammar class of `text` at that moment.
    """

    commit_id: str
    start: int
    boundary: int
    end: int
    route: str
    text: str
    redecode_required: bool
    wake_window: Window | None
    local_action: str | None
    decided_at: int
    completeness: str


@dataclass(frozen=True, slots=True)
class Close:
    reason: str


Decision = Pending | Revoked | Commit | Close


@dataclass(frozen=True, slots=True)
class UtteranceSpec:
    """What one utterance is and how it is judged.

    `start` is the pre-roll start (first sample of the ASR stream and of the
    committed span); `trigger` the §16.6 trigger sample. Wake turns give
    `seed_start` (candidate support start), `wake_open` (the opening hop's end)
    and `wake_phrase`. `context` is the §6.3 command context, only for the first
    utterance of a wake turn. `choices` are EchoMuse reply choices (§9.1).
    """

    utterance_id: str
    kind: UtteranceKind
    start: int
    trigger: int
    seed_start: int | None = None
    wake_open: int | None = None
    wake_phrase: str | None = None
    vocabulary: tuple[str, ...] = ()
    choices: tuple[Choice, ...] | None = None
    context: CommandContext | None = None
    extended: bool = False


@dataclass(frozen=True, slots=True)
class _AsrResult:
    through: int
    transcript: StreamingTranscript
    blanks: int


def streaming_command(spec: UtteranceSpec, transcript: StreamingTranscript) -> tuple[str, Window | None, int | None]:
    """The text judged for stability and completeness (§16.6 step 4 for wake turns).

    Returns (text, wake window, first command token sample). Non-wake turns
    never strip anything.
    """
    if spec.kind == "wake" and spec.wake_phrase and spec.wake_open is not None:
        window = locate_streaming(transcript, spec.wake_phrase, spec.wake_open + LOOKAHEAD_SAMPLES)
        cut = command_text(transcript, window)
        return cut.text, window, cut.first_token_sample
    cut = command_text(transcript, None)
    return cut.text, None, cut.first_token_sample


def local_action(
    *,
    text: str,
    stable: StableText,
    first_token_sample: int | None,
    boundary: int,
    cells: Sequence[ClassifiedCell],
    context: CommandContext | None,
) -> str | None:
    """§16.2/§6.3: the local action (`dismiss`/`snooze`/`stop`) for a committed wake utterance, or None."""
    if context is None:
        return None
    if not local_command_preconditions(
        normalized_text=text,
        stable=stable,
        first_command_token_sample=first_token_sample,
        commit_boundary=boundary,
        cells=cells,
    ):
        return None
    return echomuse_grammar.match_local_command(text, context)


class Utterance:
    """One utterance's cells, text stability and endpoint reducer (§8, §16.6).

    Feed `push_cell` with released lease evidence and `push_asr` with the
    utterance's ASR results, then call `advance(valid_audio_end)`. The reducer is
    stepped once per 80 ms block end, and only for blocks both cells and ASR
    have reached. After a `Commit` or `Close` nothing further happens.
    """

    def __init__(self, spec: UtteranceSpec) -> None:
        if spec.start > spec.trigger:
            raise ValueError("utterance start must not follow its trigger")
        self.spec = spec
        vocabulary, choices = spec.vocabulary, spec.choices
        self._completeness = lambda text: echomuse_grammar.classify(text, vocabulary, choices).klass
        self._reducer = EndpointReducer(
            utterance_start=spec.start,
            trigger_sample=spec.trigger,
            completeness=self._completeness,
            extended_utterances=spec.extended,
            esphome_reply=spec.kind == "ha_reply",
        )
        self._attributor = Attributor(
            trigger_sample=spec.trigger,
            seed_start=spec.seed_start if spec.kind == "wake" else None,
        )
        self._stability = TextStability(spec.trigger)
        self._first_cell = spec.start - spec.start % CELL
        self._cells: list[ClassifiedCell] = []
        self._reducer_observed = 0
        self._cells_through = self._first_cell
        self._asr: deque[_AsrResult] = deque()
        self._asr_through = spec.start
        self._evaluated = self._first_cell - self._first_cell % BLOCK
        self._text = ""
        self._heard = ""
        self._window: Window | None = None
        self._first_token: int | None = None
        self._events: list[dict] = []
        self._pending_since: int | None = None
        self.decision: Commit | Close | None = None
        self.has_command_speech = False

    @property
    def utterance_id(self) -> str:
        return self.spec.utterance_id

    @property
    def done(self) -> bool:
        return self.decision is not None

    @property
    def frontier(self) -> int:
        """The evidence frontier: both cells and ASR reached it."""
        return min(self._cells_through, self._asr_through)

    @property
    def asr_through(self) -> int:
        return self._asr_through

    @property
    def stable(self) -> StableText:
        return self._stability.current

    @property
    def command_text(self) -> str:
        return self._text

    @property
    def heard(self) -> str:
        """The latest raw streaming transcript, wake word included."""
        return self._heard

    @property
    def cells(self) -> list[ClassifiedCell]:
        return list(self._cells)

    def push_cell(self, ev: Evidence) -> None:
        if self.done or ev.end <= self._first_cell:
            return
        if ev.start < self._cells_through:
            return
        classified = self._attributor.classify(ev.cell, ev.background)
        self._cells.append(classified)
        self._cells_through = ev.end
        if classified.command:
            self.has_command_speech = True

    def push_asr(self, tokens: Sequence[str], emission_samples: Sequence[int], trailing_blanks: int,
                 through: int) -> None:
        if self.done:
            return
        if through < self._asr_through:
            raise ValueError("ASR results must be in sample order")
        self._asr.append(_AsrResult(through, StreamingTranscript.from_tokens(tokens, emission_samples),
                                    trailing_blanks))
        self._asr_through = through

    def close(self, reason: str) -> Close | None:
        """Actor-imposed close (mute, session loss, worker error, overrun, preemption); once only."""
        if self.done:
            return None
        self.decision = Close(reason)
        self._events.append({"event": "close", "reason": reason})
        return self.decision

    def advance(self, valid_audio_end: int) -> list[Decision]:
        """Evaluate every block end the evidence frontier has passed.

        `valid_audio_end` is the end of contiguous known mic audio; commits clip
        their 192 ms tail to it.
        """
        out: list[Decision] = []
        frontier = self.frontier
        while not self.done and self._evaluated + BLOCK <= frontier:
            block_end = self._evaluated + BLOCK
            self._evaluated = block_end
            while self._asr and self._asr[0].through <= block_end:
                self._push_text(self._asr.popleft())
            new_cells = []
            while self._reducer_observed < len(self._cells):
                cell = self._cells[self._reducer_observed]
                if cell.end > block_end:
                    break
                new_cells.append(cell)
                self._reducer_observed += 1
            self._reducer.observe(new_cells)
            decision = self._reducer.step(
                frontier=block_end,
                stable=self._stability.current,
                valid_audio_end=min(valid_audio_end, block_end),
            )
            if decision.revoked is not None:
                self._pending_since = None
                event: Decision = Revoked(decision.revoked.boundary, decision.revoked.route)
                self._events.append({"event": "revoked", "route": event.route, "boundary": event.boundary,
                                     "at": block_end})
                out.append(event)
            elif decision.commit is not None:
                out.append(self._commit(decision.commit, block_end))
            elif decision.close is not None:
                self.decision = Close(decision.close.reason)
                self._events.append({"event": "close", "reason": decision.close.reason, "at": block_end})
                out.append(self.decision)
            elif decision.pending is not None and decision.pending.since != self._pending_since:
                p = decision.pending
                self._pending_since = p.since
                self._events.append({"event": "pending", "route": p.route, "boundary": p.boundary,
                                     "since": p.since})
                out.append(Pending(p.boundary, p.since, p.route))
        return out

    def _push_text(self, result: _AsrResult) -> None:
        text, window, first = streaming_command(self.spec, result.transcript)
        self._text, self._window, self._first_token = text, window, first
        self._heard = result.transcript.text.strip()
        self._stability.push(text, result.through, result.blanks)

    def _commit(self, commit, decided_at: int) -> Commit:
        action = None
        if self.spec.kind == "wake":
            action = local_action(
                text=self._text,
                stable=self._stability.current,
                first_token_sample=self._first_token,
                boundary=commit.boundary,
                cells=self._cells,
                context=self.spec.context,
            )
        self.decision = Commit(
            commit_id=str(uuid.uuid4()),
            start=commit.start,
            boundary=commit.boundary,
            end=commit.end,
            route=commit.route,
            text=commit.text,
            redecode_required=commit.redecode_required,
            wake_window=self._window,
            local_action=action,
            decided_at=decided_at,
            completeness=self._completeness(commit.text),
        )
        self._events.append({"event": "commit", "route": commit.route, "boundary": commit.boundary,
                             "end": commit.end, "commit_id": self.decision.commit_id, "local_action": action})
        return self.decision

    def trace(self) -> dict:
        """§11.3 decision-trace fields for this utterance."""
        return {
            "utterance_id": self.spec.utterance_id,
            "kind": self.spec.kind,
            "start": self.spec.start,
            "trigger": self.spec.trigger,
            "evidence_frontier": self.frontier,
            "segments": [[s.cls, s.rule, s.start, s.end] for s in self._attributor.trace],
            "foreground_db": self._attributor.foreground,
            "background_available": sum(c.background is not None for c in self._cells),
            "cells": len(self._cells),
            "command_cells": sum(c.cls == COMMAND_SPEECH and c.command for c in self._cells),
            "stable_prefix": self._stability.current.prefix,
            "streaming_text": self._text,
            "heard": self._heard,
            "endpoint": list(self._events),
        }


def redecoded_command(spec: UtteranceSpec, tokens: Sequence[str], seconds: Sequence[float]) -> str:
    """Command text of a fresh decode of the committed span, cut like the streaming text."""
    emissions = [spec.start + round(t * SAMPLE_RATE) for t in seconds]
    return streaming_command(spec, StreamingTranscript.from_tokens(tokens, emissions))[0]


# --- Reply expectations --------------------------------------------------------------


@dataclass
class ReplyWatch:
    """Finds where a no-wake reply starts (§16.6 early answer, reply onset after drain).

    Push every released cell of the reply lease. While the prompt plays, a run
    of qualifying near-end cells is an early answer; after `drained()`, the
    onset scan runs over the retained cells and then over new ones. Returns the
    onset sample once; the reply utterance starts at onset − 300 ms.
    """

    _early: EarlyAnswerDetector = field(default_factory=EarlyAnswerDetector)
    _scanner: ReplyOnsetScanner | None = None
    onset: int | None = None
    early: bool = False

    @property
    def draining(self) -> bool:
        return self._scanner is not None

    def push(self, ev: Evidence) -> int | None:
        if self.onset is not None:
            return None
        if self._scanner is None:
            onset = self._early.push(ev.cell, ev.background)
            if onset is not None:
                self.onset, self.early = onset, True
            return onset
        onset = self._scanner.push(ev.cell)
        if onset is not None:
            self.onset = onset
        return onset

    def drained(self, drain_sample: int, retained: Iterable[Evidence]) -> int | None:
        """Start the onset scan at the guarded drain (or expectation start without prompt audio)."""
        if self.onset is not None or self._scanner is not None:
            return None
        self._scanner = ReplyOnsetScanner(drain_sample)
        for ev in retained:
            onset = self._scanner.push(ev.cell)
            if onset is not None:
                self.onset = onset
                return onset
        return None
