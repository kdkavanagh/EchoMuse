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
  prompt.
- `local_action` is the §16.2 local-command gate plus grammar match.
"""

from __future__ import annotations

import enum
import uuid
from collections import deque
from dataclasses import dataclass, field
from typing import Sequence, TypedDict

import echomuse_grammar
from echomuse_grammar import Choice, CommandContext, GrammarClass, LocalAction
from em_attribution import (
    CELL,
    Attributor,
    BackgroundTracker,
    Cell,
    CellClass,
    ClassifiedCell,
    EarlyAnswerDetector,
    EchoResult,
    Rule,
)
from em_audio_timeline import CELL_FLAG_GAP, CELL_FLAG_MUTED
from em_endpoint_policy import Commit as ReducerCommit
from em_endpoint_policy import (
    CloseReason,
    EndpointReducer,
    Route,
    StableText,
    TextStability,
    local_command_preconditions,
)
from em_pause_asr import PauseDecode, WyomingServer
from em_wake_phrase import (
    LOOKAHEAD_SAMPLES,
    StreamingTranscript,
    Window,
    command_text,
    final_command_text,
    locate_streaming,
    normalize_command,
)

SAMPLE_RATE = 16_000
BLOCK = 1_280          # one 80 ms capture block; the reducer's evaluation grid (§16.6)
PREROLL = 4_800        # 300 ms before the trigger/onset (§8.1)
RETAIN_CELLS = 10 * SAMPLE_RATE // CELL + 1   # retained evidence for B, a reply's lead-in and echo windows


class UtteranceKind(enum.StrEnum):
    """What opened the utterance: a wake word, the button, a no-wake reply (websocket or ESPHome path)."""

    WAKE = "wake"
    BUTTON = "button"
    REPLY = "reply"
    HA_REPLY = "ha_reply"


REPLY_KINDS = frozenset({UtteranceKind.REPLY, UtteranceKind.HA_REPLY})


class TextSource(enum.StrEnum):
    """Whose words a judged text is (§16.6): Kroko's live stream, Kroko's fresh decode at a
    pause, or the pause server's transcript of that pause (config `pauseAsr`)."""

    STREAMING = "streaming"
    KROKO = "kroko"
    SERVER = "server"


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
        self._echo: dict[int, EchoResult] = {}
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

    def add_echo(self, first_cell_sample: int, results: Sequence[EchoResult]) -> None:
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
        out: list[Evidence] = []
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
    route: Route


@dataclass(frozen=True, slots=True)
class Revoked:
    boundary: int
    route: Route


@dataclass(frozen=True, slots=True)
class Commit:
    """The immutable committed span [start, end) and what it carries (§7 COMMITTED).

    `text` is the stable streaming command text (after the wake cut for wake
    turns), `source` whose words it is (the pause server's when it equals any text judged
    on them, else Kroko's pause decode's, else the live stream's);
    `wake_window` locates the wake phrase in the streaming transcript for
    the final-transcript strip; `local_action` is the §6.3 local command, if any.
    `decided_at` is the evidence frontier (sample) the reducer committed at, so
    `decided_at − boundary` is the silence it waited; `completeness` is the
    grammar class of `text` at that moment.
    """

    commit_id: str
    start: int
    boundary: int
    end: int
    route: Route
    text: str
    redecode_required: bool
    wake_window: Window | None
    local_action: LocalAction | None
    decided_at: int
    completeness: GrammarClass
    source: TextSource


@dataclass(frozen=True, slots=True)
class Close:
    reason: CloseReason


Decision = Pending | Revoked | Commit | Close


class EndpointEvent(TypedDict, total=False):
    """One entry of the §11.3 endpoint decision trace."""

    event: str                     # pending | revoked | commit | close
    route: Route
    boundary: int
    since: int
    at: int
    end: int
    commit_id: str
    local_action: LocalAction | None
    source: TextSource             # commit: whose words the committed text is
    reason: CloseReason


class RecognitionTrace(TypedDict):
    """One answer of HA's sentence matcher for a stable prefix (§16.6). It applies to the blocks
    after `at`; `klass` is None when HA gave no answer (`error`). `ms`: wall-clock round trip."""

    text: str
    at: int
    klass: GrammarClass | None
    ms: int
    error: str | None


class PauseTranscript(TypedDict):
    """One transcriber's words at one pause (§16.6). `text` is its transcript of the
    utterance through the pause, wake word included, and `ms` the wall clock from the pause
    to it; both None while it gave none: `error` says why (`superseded`, `timeout`, a
    failure), and with no error the decision came first. `judged` is the text judged on
    those words (wake phrase cut, normalized) and `at` the block from which it was; both
    None when they never were: empty, dropped, after the decision, or, for Kroko, the
    server's words already in when Kroko's decode was reported."""

    text: str | None
    ms: int | None
    judged: str | None
    at: int | None
    error: str | None


class PauseTrace(TypedDict):
    """One pause (§16.6): Kroko's fresh decode of the utterance through `through` and the
    pause server's transcript of the same audio, sent at the same moment; `server` None
    when Kroko transcribes alone."""

    through: int
    kroko: PauseTranscript
    server: PauseTranscript | None


class UtteranceTrace(TypedDict):
    """§11.3 decision-trace fields for one utterance (logged with the turn row)."""

    utterance_id: str
    kind: UtteranceKind
    start: int
    trigger: int
    evidence_frontier: int
    segments: list[tuple[CellClass, Rule, int, int]]
    foreground_db: float | None
    background_available: int
    cells: int
    command_cells: int
    stable_prefix: str
    streaming_text: str
    heard: str
    pauses: list[PauseTrace]
    recognizer: list[RecognitionTrace]
    endpoint: list[EndpointEvent]


@dataclass(frozen=True, slots=True)
class UtteranceSpec:
    """What one utterance is and how it is judged.

    `start` is the pre-roll start (first sample of the ASR stream and of the
    committed span); `trigger` the §16.6 trigger sample. Wake turns give
    `seed_start` (candidate support start), `wake_open` (the opening hop's end)
    and `wake_phrase`. `context` is the §6.3 command context, only for the first
    utterance of a wake turn. `choices` are EchoMuse reply choices (§9.1).
    `pause_server` also transcribes the utterance at each pause (config `pauseAsr`).
    `no_input_at` is where a reply reopened after a gap closes `no_input`: its
    window's original end (None: the reducer's own, from `trigger`).
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
    pause_server: WyomingServer | None = None
    no_input_at: int | None = None


@dataclass(frozen=True, slots=True)
class _AsrResult:
    through: int
    transcript: StreamingTranscript
    blanks: int
    pause_text: str | None
    finalized: bool


def _heard(text: str | None, ms: int | None, error: str | None = None) -> PauseTranscript:
    return PauseTranscript(text=text, ms=ms, judged=None, at=None, error=error)


def streaming_command(spec: UtteranceSpec, transcript: StreamingTranscript) -> tuple[str, Window | None, int | None]:
    """The text judged for stability and completeness (§16.6 step 4 for wake turns).

    Returns (text, wake window, first command token sample). Non-wake turns
    never strip anything.
    """
    if spec.kind == UtteranceKind.WAKE and spec.wake_phrase and spec.wake_open is not None:
        window = locate_streaming(transcript, spec.wake_phrase, spec.wake_open + LOOKAHEAD_SAMPLES)
        cut = command_text(transcript, window)
        return cut.text, window, cut.first_token_sample
    cut = command_text(transcript, None)
    return cut.text, None, cut.first_token_sample


def pause_command(spec: UtteranceSpec, text: str, window: Window | None) -> str:
    """The judged text of a pause server's transcript, which has no word timings: the
    wake phrase is cut as from HA's final transcript, near where the streaming
    transcript placed it (§16.6 step 3), then normalized like the streaming text."""
    if spec.kind == UtteranceKind.WAKE and spec.wake_phrase and spec.wake_open is not None:
        text = final_command_text(text, wake_initiated=True, streaming_window=window, wake_phrase=spec.wake_phrase)
    return normalize_command(text)


def local_action(
    *,
    text: str,
    stable: StableText,
    first_token_sample: int | None,
    boundary: int,
    cells: Sequence[ClassifiedCell],
    context: CommandContext | None,
) -> LocalAction | None:
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

    `recognizer_query` names a stable prefix to ask HA's sentence matcher about
    and `recognized` records its answer; the answer counts from the next block
    on, so a replay of the trace makes the same decisions.
    """

    def __init__(self, spec: UtteranceSpec) -> None:
        if spec.start > spec.trigger:
            raise ValueError("utterance start must not follow its trigger")
        self.spec = spec
        reply = spec.kind in REPLY_KINDS
        self._reducer = EndpointReducer(
            utterance_start=spec.start,
            trigger_sample=spec.trigger,
            completeness=self._completeness,
            extended_utterances=spec.extended,
            esphome_reply=spec.kind == UtteranceKind.HA_REPLY,
            reply=reply,
            no_input_at=spec.no_input_at,
        )
        self._attributor = Attributor(
            trigger_sample=spec.trigger,
            seed_start=spec.seed_start if spec.kind == UtteranceKind.WAKE else None,
            reply=reply,
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
        self._events: list[EndpointEvent] = []
        self._pauses: list[PauseTrace] = []
        self._kroko_texts: set[str] = set()    # judged texts that came from Kroko's pause decodes
        self._server_texts: set[str] = set()   # judged texts that came from the pause server
        self._asked: set[str] = set()
        self._recognized: dict[str, GrammarClass] = {}
        self._recognitions: list[RecognitionTrace] = []
        self._pending_since: int | None = None
        self.decision: Commit | Close | None = None
        self.has_command_speech = False
        self._silent_gap: int | None = None   # a reply's gap before any command speech: its last cell's end
        self._heard_after_gap = False

    def _local_completeness(self, text: str) -> GrammarClass:
        return echomuse_grammar.classify(text, self.spec.vocabulary, self.spec.choices).klass

    def _completeness(self, text: str) -> GrammarClass:
        """The local grammar's class; HA's answer for exactly this text only where that is `unknown` (§16.6)."""
        local = self._local_completeness(text)
        if local != GrammarClass.UNKNOWN:
            return local
        return self._recognized.get(text, local)

    def recognizer_query(self) -> str | None:
        """The stable prefix to ask HA's sentence matcher about, each text once: only text the
        local grammar leaves `unknown`, and never for an ESPHome-path reply, whose route R
        reads no completeness (§16.6)."""
        text = self._stability.current.prefix
        if (self.done or self.spec.kind == UtteranceKind.HA_REPLY or not text or text in self._asked
                or self._local_completeness(text) != GrammarClass.UNKNOWN):
            return None
        self._asked.add(text)
        return text

    def recognized(self, text: str, klass: GrammarClass | None, *, ms: int, error: str | None = None) -> None:
        """HA's answer for `text`: it counts from the block after the last one evaluated.
        An answer after the decision changes nothing and is not recorded."""
        if self.done:
            return
        self._recognitions.append(RecognitionTrace(text=text, at=self._evaluated, klass=klass, ms=ms, error=error))
        if klass is not None:
            self._recognized[text] = klass

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

    @property
    def answer_under_way(self) -> bool:
        """A command-speech run that opened by the trigger is open at the newest cell: for a
        reply, an answer begun over the question's end (§16.6), which the reply chime would cover."""
        start = self._attributor.answer_start
        return start is not None and start <= self.spec.trigger

    @property
    def reopen_at(self) -> int | None:
        """A reply that met a gap before any command speech (§16.6): where to reopen it, once
        audio after the gap has been heard, else None. Its window does not end at the gap: the
        utterance evaluates nothing past it, and the actor reopens the reply after it."""
        return self._silent_gap if self._heard_after_gap else None

    @property
    def no_input_at(self) -> int:
        """The sample at which, without command speech, the utterance closes `no_input`."""
        return self._reducer.no_input_at

    def push_cell(self, ev: Evidence) -> None:
        if self.done or ev.end <= self._first_cell:
            return
        if ev.start < self._cells_through:
            return
        classified = self._attributor.classify(ev.cell, ev.background)
        self._cells.append(classified)
        self._cells_through = ev.end
        if classified.cls == CellClass.GAP:
            if self.spec.kind in REPLY_KINDS and not self.has_command_speech:
                self._silent_gap, self._heard_after_gap = classified.end, False
        elif self._silent_gap is not None and ev.cell.vad is not None:
            self._heard_after_gap = True
        if classified.command:
            self.has_command_speech = True

    def push_asr(self, tokens: Sequence[str], emission_samples: Sequence[int], trailing_blanks: int,
                 through: int, *, finalize_ms: int | None = None, pause_text: str | None = None,
                 pause_decode: PauseDecode | None = None) -> None:
        """One ASR observation; `finalize_ms` marks Kroko's fresh decode at a pause and is
        its time (§16.6). `pause_text` is the pause server's transcript standing for its
        text, and `pause_decode` reports a server request on the result where it was
        answered or dropped, never before its pause's finalized result."""
        if self.done:
            return
        if through < self._asr_through:
            raise ValueError("ASR results must be in sample order")
        transcript = StreamingTranscript.from_tokens(tokens, emission_samples)
        self._asr.append(_AsrResult(through, transcript, trailing_blanks, pause_text, finalize_ms is not None))
        self._asr_through = through
        if finalize_ms is not None:
            self._pauses.append(PauseTrace(
                through=through, kroko=_heard(transcript.text.strip(), finalize_ms),
                server=None if self.spec.pause_server is None else _heard(None, None)))
        if pause_decode is not None:
            pause = next((p for p in reversed(self._pauses) if p["through"] == pause_decode.through), None)
            if pause is None:
                raise ValueError("a pause server's answer must follow its pause's finalized result")
            pause["server"] = _heard(pause_decode.text, pause_decode.ms, pause_decode.error)

    def advance(self, valid_audio_end: int) -> list[Decision]:
        """Evaluate every block end the evidence frontier has passed.

        `valid_audio_end` is the end of contiguous known mic audio; commits clip
        their 192 ms tail to it. A reply that met a gap before any command speech
        evaluates nothing more (see `reopen_at`).
        """
        out: list[Decision] = []
        frontier = self.frontier
        while not self.done and self._silent_gap is None and self._evaluated + BLOCK <= frontier:
            block_end = self._evaluated + BLOCK
            self._evaluated = block_end
            while self._asr and self._asr[0].through <= block_end:
                self._push_text(self._asr.popleft())
            new_cells: list[ClassifiedCell] = []
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
                revoked = Revoked(decision.revoked.boundary, decision.revoked.route)
                self._events.append(EndpointEvent(event="revoked", route=revoked.route, boundary=revoked.boundary,
                                                  at=block_end))
                out.append(revoked)
            elif decision.commit is not None:
                out.append(self._commit(decision.commit, block_end))
            elif decision.close is not None:
                close = Close(decision.close.reason)
                self.decision = close
                self._events.append(EndpointEvent(event="close", reason=close.reason, at=block_end))
                out.append(close)
            elif decision.pending is not None and decision.pending.since != self._pending_since:
                p = decision.pending
                self._pending_since = p.since
                self._events.append(EndpointEvent(event="pending", route=p.route, boundary=p.boundary,
                                                  since=p.since))
                out.append(Pending(p.boundary, p.since, p.route))
        return out

    def _push_text(self, result: _AsrResult) -> None:
        text, window, first = streaming_command(self.spec, result.transcript)
        heard = result.transcript.text.strip()
        # The pause these words belong to: a finalized result opens it, and the server's
        # transcript rides only the latest finalize's results.
        pause = next((p for p in reversed(self._pauses) if p["through"] <= result.through), None)
        if result.finalized and result.pause_text is None and pause is not None:
            pause["kroko"]["judged"], pause["kroko"]["at"] = text, result.through
            self._kroko_texts.add(text)
        if result.pause_text is not None:
            # Words from the pause server; Kroko still places the wake phrase and the command's start.
            text, heard = pause_command(self.spec, result.pause_text, window), result.pause_text.strip()
            self._server_texts.add(text)
            server = pause["server"] if pause is not None else None
            if server is not None and server["at"] is None:
                server["judged"], server["at"] = text, result.through
        self._text, self._window, self._first_token, self._heard = text, window, first, heard
        self._stability.push(text, result.through, result.blanks)

    def _source(self, text: str) -> TextSource:
        """Whose words a judged text is; the server's first, as its re-decode follows from that."""
        if text in self._server_texts:
            return TextSource.SERVER
        return TextSource.KROKO if text in self._kroko_texts else TextSource.STREAMING

    def _commit(self, commit: ReducerCommit, decided_at: int) -> Commit:
        action = None
        if self.spec.kind == UtteranceKind.WAKE:
            action = local_action(
                text=self._text,
                stable=self._stability.current,
                first_token_sample=self._first_token,
                boundary=commit.boundary,
                cells=self._cells,
                context=self.spec.context,
            )
        committed = Commit(
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
            source=self._source(commit.text),
        )
        self.decision = committed
        self._events.append(EndpointEvent(event="commit", route=commit.route, boundary=commit.boundary,
                                          end=commit.end, commit_id=committed.commit_id, local_action=action,
                                          source=committed.source))
        return committed

    def trace(self) -> UtteranceTrace:
        """§11.3 decision-trace fields for this utterance."""
        return UtteranceTrace(
            utterance_id=self.spec.utterance_id,
            kind=self.spec.kind,
            start=self.spec.start,
            trigger=self.spec.trigger,
            evidence_frontier=self.frontier,
            segments=[(s.cls, s.rule, s.start, s.end) for s in self._attributor.trace],
            foreground_db=self._attributor.foreground,
            background_available=sum(c.background is not None for c in self._cells),
            cells=len(self._cells),
            command_cells=sum(c.cls == CellClass.COMMAND_SPEECH and c.command for c in self._cells),
            stable_prefix=self._stability.current.prefix,
            streaming_text=self._text,
            heard=self._heard,
            pauses=[PauseTrace(through=p["through"], kroko=p["kroko"].copy(),
                               server=None if p["server"] is None else p["server"].copy())
                    for p in self._pauses],
            recognizer=list(self._recognitions),
            endpoint=list(self._events),
        )


def redecoded_command(spec: UtteranceSpec, tokens: Sequence[str], seconds: Sequence[float]) -> str:
    """Command text of a fresh decode of the committed span, cut like the streaming text."""
    emissions = [spec.start + round(t * SAMPLE_RATE) for t in seconds]
    return streaming_command(spec, StreamingTranscript.from_tokens(tokens, emissions))[0]


# --- Reply expectations --------------------------------------------------------------


@dataclass
class ReplyWatch:
    """Finds an early answer (§16.6) while a question carrying a reply expectation plays.

    Push every released cell of the reply lease until the question drains;
    returns the onset sample once. The reply turn's trigger is the onset, and
    its utterance starts 300 ms before it.
    """

    _early: EarlyAnswerDetector = field(default_factory=EarlyAnswerDetector)

    def push(self, ev: Evidence) -> int | None:
        return self._early.push(ev.cell, ev.background)
