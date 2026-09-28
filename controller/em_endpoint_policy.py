"""Sample-time endpoint policy reducer (SPEC §8.3, §9.1, §16.2, §16.6).

The session actor calls `step()` after each 80 ms mic block only when level,
VAD and ASR observations have reached that block's end. Every duration is
therefore acquired-audio sample time; wall time is never an input.
"""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass
from typing import Callable, Literal, Sequence

from em_attribution import (
    BACKGROUND_SPEECH,
    GAP,
    NON_SPEECH,
    SELF_OUTPUT,
    ClassifiedCell,
)
from em_wake_phrase import normalize_command, words

SAMPLE_RATE = 16_000

# §16.6 endpoint values.
NO_INPUT = 5 * SAMPLE_RATE
TOO_LONG = 15 * SAMPLE_RATE
TOO_LONG_EXTENDED = 30 * SAMPLE_RATE
LOOKAHEAD = 3_072  # 192 ms
REVOCATION_CELLS = 4
MIN_COMMAND_CELLS = 6  # 192 ms
PAUSE_WINDOW_CELLS = 19
PAUSE_QUIET_CELLS = 15
MIN_TRAILING_BLANKS = 10  # one blank frame = 40 ms
ROUTE_A_PAUSE = {
    "complete": 9_728,  # 608 ms
    "extendable": 19_456,  # 1,216 ms
    "needs_more": 28_672,  # 1,792 ms
    "unknown": 28_672,
}
ROUTE_B_STABLE = 9_728  # 608 ms
ROUTE_B_BACKGROUND_FRACTION = 0.80
ROUTE_R_PAUSE = 16_384  # 1,024 ms
NO_PROGRESS = 3 * SAMPLE_RATE
STABILITY_SPAN = 3_840  # 240 ms

# §16.2 local-command precondition.
LOCAL_PRE_ROLL = 7_680  # 480 ms

Completeness = Literal["complete", "extendable", "needs_more", "unknown"]
CompletenessFn = Callable[[str], Completeness]

LISTENING = "LISTENING"
END_PENDING = "END_PENDING"
COMMITTED = "COMMITTED"
CLOSED = "CLOSED"


@dataclass(frozen=True, slots=True)
class StableText:
    """Current stable prefix and its sample clocks (§16.6)."""

    prefix: str
    prefix_tokens: tuple[str, ...]
    tail: str
    latest_text: str
    prefix_sample: int | None
    progress_sample: int
    trailing_blank_frames: int
    through_sample: int


def _tokens(text: str) -> tuple[str, ...]:
    return tuple(w.text for w in words(text))


def _common_prefix(results: Sequence[tuple[str, ...]]) -> tuple[str, ...]:
    if not results:
        return ()
    n = min(map(len, results))
    i = 0
    while i < n and all(tokens[i] == results[0][i] for tokens in results[1:]):
        i += 1
    return results[0][:i]


class TextStability:
    """Longest normalized token prefix unchanged across ≥3 results spanning ≥240 ms.

    `prefix_sample` is the through-sample of the result in which the current
    stable prefix became stable; `progress_sample` starts at the trigger and is
    updated to that same sample whenever the stable prefix changes.
    """

    def __init__(
        self,
        trigger_sample: int,
        normalize_tokens: Callable[[str], Sequence[str]] | None = None,
    ) -> None:
        # Wake-turn ASR starts in the pre-roll, so results may precede the trigger.
        self._normalize_tokens = normalize_tokens or _tokens
        self._results: deque[tuple[int, tuple[str, ...], str, int]] = deque()
        self._prefix: tuple[str, ...] = ()
        self._prefix_sample: int | None = None
        self._progress_sample = trigger_sample
        self._through: int | None = None
        self._trailing_blanks = 0
        self._latest_tokens: tuple[str, ...] = ()
        self._latest_text = ""
        self._has_nonempty_prefix = False

    @property
    def current(self) -> StableText:
        tail = " ".join(self._latest_tokens[len(self._prefix) :])
        return StableText(
            prefix=" ".join(self._prefix),
            prefix_tokens=self._prefix,
            tail=tail,
            latest_text=self._latest_text,
            prefix_sample=self._prefix_sample,
            progress_sample=self._progress_sample,
            trailing_blank_frames=self._trailing_blanks,
            through_sample=self._progress_sample if self._through is None else self._through,
        )

    def push(self, text: str, through_sample: int, trailing_blank_frames: int) -> StableText:
        if self._through is not None and through_sample < self._through:
            raise ValueError("ASR results must be in sample order")
        tokens = tuple(self._normalize_tokens(text))
        self._results.append((through_sample, tokens, text, trailing_blank_frames))
        self._through = through_sample
        self._trailing_blanks = trailing_blank_frames
        self._latest_tokens = tokens
        self._latest_text = text

        history = list(self._results)
        eligible = [i for i, r in enumerate(history) if through_sample - r[0] >= STABILITY_SPAN]
        if eligible:
            window = history[eligible[-1] :]
            if len(window) >= 3:
                prefix = _common_prefix([r[1] for r in window])
                if prefix != self._prefix:
                    self._prefix = prefix
                    self._prefix_sample = through_sample if prefix else None
                    if prefix or self._has_nonempty_prefix:
                        self._progress_sample = through_sample
                    self._has_nonempty_prefix |= bool(prefix)
        # Only samples needed to establish a future 240 ms span are retained.
        while len(self._results) > 1 and through_sample - self._results[1][0] >= STABILITY_SPAN:
            self._results.popleft()
        return self.current


@dataclass(frozen=True, slots=True)
class Pending:
    boundary: int
    since: int
    route: Literal["A", "B", "R"]
    text: str


@dataclass(frozen=True, slots=True)
class Commit:
    """Immutable committed span. `end` is boundary + 192 ms, clipped to valid audio."""

    start: int
    boundary: int
    end: int
    route: Literal["A", "B", "R", "fallback"]
    text: str
    redecode_required: bool


CloseReason = Literal["muted", "session_lost", "interrupted", "audio_overrun", "no_input", "too_long", "retry"]


@dataclass(frozen=True, slots=True)
class Close:
    """Terminal reason without dispatch (§7 terminal feedback)."""

    reason: CloseReason


@dataclass(frozen=True, slots=True)
class Decision:
    state: str
    pending: Pending | None = None
    revoked: Pending | None = None
    commit: Commit | None = None
    close: Close | None = None


class EndpointReducer:
    """§16.6 endpoint state machine for one utterance."""

    def __init__(
        self,
        *,
        utterance_start: int,
        trigger_sample: int,
        completeness: CompletenessFn,
        extended_utterances: bool = False,
        esphome_reply: bool = False,
    ) -> None:
        if utterance_start > trigger_sample:
            raise ValueError("utterance_start must not follow the trigger")
        self.utterance_start = utterance_start
        self.trigger_sample = trigger_sample
        self.completeness = completeness
        self.extended_utterances = extended_utterances
        self.esphome_reply = esphome_reply
        self.state = LISTENING
        self.pending: Pending | None = None
        self._cells: list[ClassifiedCell] = []
        self._command_ends: list[int] = []
        self._frontier = utterance_start
        self._commit: Commit | None = None
        self._close: Close | None = None

    @property
    def commit(self) -> Commit | None:
        return self._commit

    @property
    def close(self) -> Close | None:
        return self._close

    def observe(self, cells: Sequence[ClassifiedCell]) -> None:
        """Add newly classified cells in sample order before the next evidence-frontier `step()`."""
        for cell in cells:
            if self._cells and cell.start <= self._cells[-1].start:
                raise ValueError("cells must be observed once in sample order")
            self._cells.append(cell)
            if cell.command:
                self._command_ends.append(cell.end)

    def _close_once(self, reason: CloseReason) -> Decision:
        self._close = Close(reason)
        self.state = CLOSED
        return Decision(self.state, close=self._close)

    def finalize_once(
        self,
        boundary: int,
        *,
        route: Literal["A", "B", "R", "fallback"],
        text: str,
        valid_audio_end: int,
    ) -> Commit:
        """Commit exactly once; a second commit is an internal error (§16.2 invariant 2)."""
        if self._commit is not None:
            raise RuntimeError("utterance already committed")
        if self._close is not None:
            raise RuntimeError("closed utterance cannot commit")
        if not self.utterance_start <= boundary <= valid_audio_end:
            raise ValueError("boundary is outside valid acquired audio")
        self._commit = Commit(
            start=self.utterance_start,
            boundary=boundary,
            end=min(boundary + LOOKAHEAD, valid_audio_end),
            route=route,
            text=text,
            redecode_required=route in ("B", "fallback"),
        )
        self.pending = None
        self.state = COMMITTED
        return self._commit

    def _cells_through(self, frontier: int) -> list[ClassifiedCell]:
        return [c for c in self._cells if c.end <= frontier]

    def _command_boundary_at(self, sample: int) -> int | None:
        return next((end for end in reversed(self._command_ends) if end <= sample), None)

    def _route_a(self, frontier: int, stable: StableText, cells: list[ClassifiedCell]) -> Pending | None:
        if len(self._command_ends) < MIN_COMMAND_CELLS or not stable.prefix or len(cells) < PAUSE_WINDOW_CELLS:
            return None
        if sum(c.cls in (NON_SPEECH, SELF_OUTPUT) for c in cells[-PAUSE_WINDOW_CELLS:]) < PAUSE_QUIET_CELLS:
            return None
        if stable.trailing_blank_frames < MIN_TRAILING_BLANKS:
            return None
        kind = self.completeness(stable.prefix)
        if kind not in ROUTE_A_PAUSE:
            raise ValueError(f"invalid completeness result: {kind!r}")
        if frontier - self._command_ends[-1] < ROUTE_A_PAUSE[kind]:
            return None
        return Pending(self._command_ends[-1], frontier, "A", stable.prefix)

    def _route_b(self, frontier: int, stable: StableText, cells: list[ClassifiedCell]) -> Pending | None:
        if not stable.prefix or stable.prefix_sample is None:
            return None
        if self.completeness(stable.prefix) != "complete":
            return None
        if frontier - stable.prefix_sample < ROUTE_B_STABLE:
            return None
        combined = stable.prefix if not stable.tail else f"{stable.prefix} {stable.tail}"
        if stable.tail and self.completeness(combined) != "unknown":
            return None
        after = [c for c in cells if c.end > stable.prefix_sample and c.speech_positive]
        if not after or sum(c.cls == BACKGROUND_SPEECH for c in after) < ROUTE_B_BACKGROUND_FRACTION * len(after):
            return None
        boundary = self._command_boundary_at(stable.prefix_sample)
        if boundary is None:
            return None
        return Pending(boundary, frontier, "B", stable.prefix)

    def _route_r(self, frontier: int, stable: StableText) -> Pending | None:
        if not self._command_ends or frontier - self._command_ends[-1] < ROUTE_R_PAUSE:
            return None
        return Pending(self._command_ends[-1], frontier, "R", stable.prefix)

    def step(
        self,
        *,
        frontier: int,
        stable: StableText,
        valid_audio_end: int,
        muted: bool = False,
        session_lost: bool = False,
        gap: bool = False,
        overrun: bool = False,
    ) -> Decision:
        """Evaluate one 80 ms block after the evidence frontier reaches `frontier`."""
        if self.state in (COMMITTED, CLOSED):
            raise RuntimeError("terminal utterance was evaluated again")
        if frontier < self._frontier:
            raise ValueError("evidence frontier moved backwards")
        if valid_audio_end > frontier:
            raise ValueError("valid audio cannot extend beyond the evidence frontier")
        self._frontier = frontier

        # Actor priority (§16.2) and reducer close order (§16.6).
        if muted:
            return self._close_once("muted")
        if session_lost:
            return self._close_once("session_lost")
        cells = self._cells_through(frontier)
        if gap or any(c.cls == GAP for c in cells):
            return self._close_once("interrupted")
        if overrun:
            return self._close_once("audio_overrun")
        if not self._command_ends and frontier - self.trigger_sample >= NO_INPUT:
            return self._close_once("no_input")
        maximum = TOO_LONG_EXTENDED if self.extended_utterances else TOO_LONG
        if frontier - self.trigger_sample >= maximum:
            return self._close_once("too_long")

        if self.pending is not None:
            pending = self.pending
            resumed = sum(end > pending.since for end in self._command_ends) >= REVOCATION_CELLS
            if resumed:
                self.pending = None
                self.state = LISTENING
                return Decision(self.state, revoked=pending)
            if frontier - pending.since >= LOOKAHEAD:
                commit = self.finalize_once(
                    pending.boundary,
                    route=pending.route,
                    text=pending.text,
                    valid_audio_end=valid_audio_end,
                )
                return Decision(self.state, commit=commit)
            return Decision(self.state, pending=pending)

        pending = self._route_r(frontier, stable) if self.esphome_reply else self._route_a(frontier, stable, cells)
        if pending is None and not self.esphome_reply:
            pending = self._route_b(frontier, stable, cells)
        if pending is not None:
            self.pending = pending
            self.state = END_PENDING
            return Decision(self.state, pending=pending)

        if self._command_ends and frontier - stable.progress_sample >= NO_PROGRESS:
            boundary = self._command_boundary_at(stable.prefix_sample) if stable.prefix_sample is not None else None
            if stable.prefix and self.completeness(stable.prefix) == "complete" and boundary is not None:
                commit = self.finalize_once(
                    boundary,
                    route="fallback",
                    text=stable.prefix,
                    valid_audio_end=valid_audio_end,
                )
                return Decision(self.state, commit=commit)
            return self._close_once("retry")
        return Decision(self.state)


def redecode_differs(committed: str, redecoded: str, normalize: Callable[[str], Sequence[str]] | None = None) -> bool:
    """Whether a fresh span decode differs except by completion of the final word (§16.6).

    Equality is accepted. So is exactly one unfinished final token becoming a
    longer token: `"turn off the lig"` → `"turn off the lights"`. Added words,
    deletions, substitutions, or completing a non-final token differ.
    """
    normalize = normalize or _tokens
    before = tuple(normalize(committed))
    after = tuple(normalize(redecoded))
    if before == after:
        return False
    return not (
        len(before) == len(after)
        and len(before) > 0
        and before[:-1] == after[:-1]
        and after[-1].startswith(before[-1])
        and len(after[-1]) > len(before[-1])
    )


def local_command_preconditions(
    *,
    normalized_text: str,
    stable: StableText,
    first_command_token_sample: int | None,
    commit_boundary: int,
    cells: Sequence[ClassifiedCell],
) -> bool:
    """§16.2 local-command gates after normal endpoint commit.

    The full command text must have been stable for at least 240 ms. From
    480 ms before the first command token's emission through the commit
    boundary, fewer than half of speech-positive cells may be `self_output`.
    """
    text = normalize_command(normalized_text)
    if not text or stable.prefix_sample is None or first_command_token_sample is None:
        return False
    if stable.prefix != text or stable.tail:
        return False
    if stable.through_sample - stable.prefix_sample < STABILITY_SPAN:
        return False
    start = first_command_token_sample - LOCAL_PRE_ROLL
    relevant = [c for c in cells if c.speech_positive and c.end > start and c.end <= commit_boundary]
    if not relevant:
        return False
    self_output = sum(c.cls == SELF_OUTPUT for c in relevant)
    return 2 * self_output < len(relevant)


@dataclass(frozen=True, slots=True)
class ReplyChoice:
    value: str
    aliases: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class ChoiceResult:
    status: Literal["selected", "reprompt", "abandon"]
    value: str | None = None


def resolve_reply_choice(text: str, choices: Sequence[ReplyChoice], *, already_reprompted: bool) -> ChoiceResult:
    """§9.1/§16.6 exact normalized alias selection and one-reprompt policy."""
    normalized = normalize_command(text)
    for choice in choices:
        if any(normalized == normalize_command(alias) for alias in choice.aliases):
            return ChoiceResult("selected", choice.value)
    return ChoiceResult("abandon" if already_reprompted else "reprompt")
