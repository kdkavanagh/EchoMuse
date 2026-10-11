"""Sample-time endpoint policy reducer (SPEC §8.3, §9.1, §16.2, §16.6).

The session actor calls `step()` after each 80 ms mic block only when level,
VAD and ASR observations have reached that block's end. Every duration is
therefore acquired-audio sample time; wall time is never an input.
"""

from __future__ import annotations

import enum
from dataclasses import dataclass
from typing import Callable, Protocol, Sequence

from echomuse_grammar import GrammarClass
from em_attribution import CellClass, ClassifiedCell
from em_wake_phrase import normalize_command, words

SAMPLE_RATE = 16_000

# §16.6 endpoint values.
NO_INPUT = 5 * SAMPLE_RATE
NO_INPUT_REPLY = 7 * SAMPLE_RATE  # a reply window: from where it opened (§9.1)
REPLY_SPAN_PREROLL = 4_800  # a reply's committed span: 300 ms before its answer's run
TOO_LONG = 15 * SAMPLE_RATE
TOO_LONG_EXTENDED = 30 * SAMPLE_RATE
LOOKAHEAD = 3_072  # 192 ms
REVOCATION_CELLS = 4
MIN_COMMAND_CELLS = 6  # 192 ms
PAUSE_WINDOW_CELLS = 19
PAUSE_QUIET_CELLS = 15
MIN_TRAILING_BLANKS = 10  # one blank frame = 40 ms

# Route A's 15-of-19 vote. `background_speech` joins it only for text whose
# pause is the long one: 1,792 ms outlasts any soft stretch inside a command,
# the complete/extendable pauses do not, and route B already ends a complete
# command under background speech.
PAUSE_QUIET = frozenset({CellClass.NON_SPEECH, CellClass.SELF_OUTPUT})
BACKGROUND_PAUSE_CLASSES = frozenset({GrammarClass.NEEDS_MORE, GrammarClass.UNKNOWN})

ROUTE_A_PAUSE = {
    GrammarClass.COMPLETE: 9_728,  # 608 ms
    GrammarClass.EXTENDABLE: 19_456,  # 1,216 ms
    GrammarClass.NEEDS_MORE: 28_672,  # 1,792 ms
    GrammarClass.UNKNOWN: 28_672,
}
ROUTE_B_STABLE = 9_728  # 608 ms
ROUTE_B_BACKGROUND_FRACTION = 0.80
ROUTE_R_PAUSE = 16_384  # 1,024 ms
NO_PROGRESS = 3 * SAMPLE_RATE

# §16.2 local-command preconditions.
LOCAL_PRE_ROLL = 7_680  # 480 ms
LOCAL_STABLE = 3_840  # 240 ms without newer text before the commit

CompletenessFn = Callable[[str], GrammarClass]   # `echomuse_grammar.classify(...).klass` of a text

# §16.6 completeness from Home Assistant's own sentence matcher (policy post_afe_4). The probe
# word is appended to ask whether a wildcard slot can take more words. It must never be an
# entity, area or list value, and it stays fixed so HA's recognition cache (keyed by text)
# answers a command said before.
RECOGNIZER_PROBE = "zqxj"


class Recognized(Protocol):
    """HA's recognition of one sentence (`em_ha_client.SentenceRecognition`)."""

    @property
    def match(self) -> bool: ...            # a sentence trigger, or an intent with every slot filled

    @property
    def unfilled_slots(self) -> bool: ...   # the closest intent left slots unmatched


def recognizer_sentences(text: str) -> tuple[str, str]:
    """What HA's recognizer is asked about a stable prefix: the text, then the text and the probe word."""
    return text, f"{text} {RECOGNIZER_PROBE}"


def recognized_completeness(text: Recognized | None, probe: Recognized | None) -> GrammarClass:
    """§16.6: HA's answer for a stable prefix as a completeness class.

    A full match is `complete`, unless the text plus a word nobody says also
    matches: then a wildcard (`(play) {query}`, `the weather [like] {when}`)
    can take more words and it is `extendable`. No match with slots left
    unfilled is `needs_more`; nothing recognized is `unknown`.
    """
    if text is None:
        return GrammarClass.UNKNOWN
    if text.match:
        return GrammarClass.EXTENDABLE if probe is not None and probe.match else GrammarClass.COMPLETE
    return GrammarClass.NEEDS_MORE if text.unfilled_slots else GrammarClass.UNKNOWN


class EndpointState(enum.StrEnum):
    LISTENING = "LISTENING"
    END_PENDING = "END_PENDING"
    COMMITTED = "COMMITTED"
    CLOSED = "CLOSED"


class Route(enum.StrEnum):
    """How an endpoint was found (§16.6): pause (A), background speech (B), ESPHome reply (R), no-progress fallback."""

    A = "A"
    B = "B"
    R = "R"
    FALLBACK = "fallback"


@dataclass(frozen=True, slots=True)
class StableText:
    """Current stable prefix and its sample clocks (§16.6)."""

    prefix: str
    prefix_tokens: tuple[str, ...]
    latest_text: str
    prefix_sample: int | None
    progress_sample: int
    trailing_blank_frames: int
    through_sample: int


def _tokens(text: str) -> tuple[str, ...]:
    return tuple(w.text for w in words(text))


class TextStability:
    """The stable prefix is the latest result's normalized tokens (§16.6).

    Kroko's greedy transducer only appends tokens, and only at its 1.28 s chunk
    edges, so between edges consecutive results are identical and waiting for a
    result to repeat checks nothing. `prefix_sample` is the through-sample of the
    result in which the prefix last changed (None while it is empty);
    `progress_sample` starts at the trigger and is updated to that same sample
    whenever the prefix changes once a non-empty prefix has existed.
    """

    def __init__(
        self,
        trigger_sample: int,
        normalize_tokens: Callable[[str], Sequence[str]] | None = None,
    ) -> None:
        # Wake-turn ASR starts in the pre-roll, so results may precede the trigger.
        self._normalize_tokens = normalize_tokens or _tokens
        self._prefix: tuple[str, ...] = ()
        self._prefix_sample: int | None = None
        self._progress_sample = trigger_sample
        self._through: int | None = None
        self._trailing_blanks = 0
        self._latest_text = ""
        self._has_nonempty_prefix = False

    @property
    def current(self) -> StableText:
        return StableText(
            prefix=" ".join(self._prefix),
            prefix_tokens=self._prefix,
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
        self._through = through_sample
        self._trailing_blanks = trailing_blank_frames
        self._latest_text = text
        if tokens != self._prefix:
            self._prefix = tokens
            self._prefix_sample = through_sample if tokens else None
            if tokens or self._has_nonempty_prefix:
                self._progress_sample = through_sample
            self._has_nonempty_prefix |= bool(tokens)
        return self.current


@dataclass(frozen=True, slots=True)
class Pending:
    boundary: int
    since: int
    route: Route
    text: str


@dataclass(frozen=True, slots=True)
class Commit:
    """Immutable committed span. `end` is boundary + 192 ms, clipped to valid audio."""

    start: int
    boundary: int
    end: int
    route: Route
    text: str
    redecode_required: bool


class CloseReason(enum.StrEnum):
    MUTED = "muted"
    SESSION_LOST = "session_lost"
    INTERRUPTED = "interrupted"
    AUDIO_OVERRUN = "audio_overrun"
    NO_INPUT = "no_input"
    TOO_LONG = "too_long"
    RETRY = "retry"


@dataclass(frozen=True, slots=True)
class Close:
    """Terminal reason without dispatch (§7 terminal feedback)."""

    reason: CloseReason


@dataclass(frozen=True, slots=True)
class Decision:
    state: EndpointState
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
        reply: bool = False,
        no_input_at: int | None = None,
    ) -> None:
        """`reply`: a no-wake reply whose trigger is where its window opened (§9.1). It
        closes `no_input` NO_INPUT_REPLY after the trigger, counts `too_long` from its first
        command speech, and commits a span from REPLY_SPAN_PREROLL before the run holding
        that speech (never before `utterance_start`), not the chime and the wait before it.
        `no_input_at` overrides where `no_input` falls: a reply reopened after a gap keeps
        its window's original end."""
        if utterance_start > trigger_sample:
            raise ValueError("utterance_start must not follow the trigger")
        self.utterance_start = utterance_start
        self.trigger_sample = trigger_sample
        self.completeness = completeness
        self.extended_utterances = extended_utterances
        self.esphome_reply = esphome_reply
        self.reply = reply
        self.no_input_at = (no_input_at if no_input_at is not None
                            else trigger_sample + (NO_INPUT_REPLY if reply else NO_INPUT))
        self.state = EndpointState.LISTENING
        self.pending: Pending | None = None
        self._cells: list[ClassifiedCell] = []
        self._command_ends: list[int] = []
        self._command_start: int | None = None   # first command_speech cell starting at/after the trigger
        self._answer_run: int | None = None      # first cell of the run holding the first command speech
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
            if cell.command and self._answer_run is None:
                # A run is the contiguous command_speech cells before it (§8.2 hysteresis).
                run = cell.start
                for prev in reversed(self._cells):
                    if prev.cls != CellClass.COMMAND_SPEECH or prev.end != run:
                        break
                    run = prev.start
                self._answer_run = run
            self._cells.append(cell)
            if cell.command:
                self._command_ends.append(cell.end)
                if self._command_start is None and cell.start >= self.trigger_sample:
                    self._command_start = cell.start

    def _close_once(self, reason: CloseReason) -> Decision:
        self._close = Close(reason)
        self.state = EndpointState.CLOSED
        return Decision(self.state, close=self._close)

    def finalize_once(
        self,
        boundary: int,
        *,
        route: Route,
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
        start = self.utterance_start
        if self.reply and self._answer_run is not None:
            start = max(start, self._answer_run - REPLY_SPAN_PREROLL)
        self._commit = Commit(
            start=start,
            boundary=boundary,
            end=min(boundary + LOOKAHEAD, valid_audio_end),
            route=route,
            text=text,
            redecode_required=route in (Route.B, Route.FALLBACK),
        )
        self.pending = None
        self.state = EndpointState.COMMITTED
        return self._commit

    def _cells_through(self, frontier: int) -> list[ClassifiedCell]:
        return [c for c in self._cells if c.end <= frontier]

    def _command_boundary_at(self, sample: int) -> int | None:
        return next((end for end in reversed(self._command_ends) if end <= sample), None)

    def _route_a(self, frontier: int, stable: StableText, cells: list[ClassifiedCell]) -> Pending | None:
        if len(self._command_ends) < MIN_COMMAND_CELLS or not stable.prefix or len(cells) < PAUSE_WINDOW_CELLS:
            return None
        window = cells[-PAUSE_WINDOW_CELLS:]
        quiet = sum(c.cls in PAUSE_QUIET for c in window)
        background = sum(c.cls == CellClass.BACKGROUND_SPEECH for c in window)
        if quiet + background < PAUSE_QUIET_CELLS:
            return None
        if stable.trailing_blank_frames < MIN_TRAILING_BLANKS:
            return None
        completeness = self.completeness(stable.prefix)
        if quiet < PAUSE_QUIET_CELLS and completeness not in BACKGROUND_PAUSE_CLASSES:
            return None
        if frontier - self._command_ends[-1] < ROUTE_A_PAUSE[completeness]:
            return None
        return Pending(self._command_ends[-1], frontier, Route.A, stable.prefix)

    def _route_b(self, frontier: int, stable: StableText, cells: list[ClassifiedCell]) -> Pending | None:
        if not stable.prefix or stable.prefix_sample is None:
            return None
        if self.completeness(stable.prefix) != GrammarClass.COMPLETE:
            return None
        if frontier - stable.prefix_sample < ROUTE_B_STABLE:
            return None
        after = [c for c in cells if c.end > stable.prefix_sample and c.speech_positive]
        background = sum(c.cls == CellClass.BACKGROUND_SPEECH for c in after)
        if not after or background < ROUTE_B_BACKGROUND_FRACTION * len(after):
            return None
        boundary = self._command_boundary_at(stable.prefix_sample)
        if boundary is None:
            return None
        return Pending(boundary, frontier, Route.B, stable.prefix)

    def _route_r(self, frontier: int, stable: StableText) -> Pending | None:
        if not self._command_ends or frontier - self._command_ends[-1] < ROUTE_R_PAUSE:
            return None
        return Pending(self._command_ends[-1], frontier, Route.R, stable.prefix)

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
        if self.state in (EndpointState.COMMITTED, EndpointState.CLOSED):
            raise RuntimeError("terminal utterance was evaluated again")
        if frontier < self._frontier:
            raise ValueError("evidence frontier moved backwards")
        if valid_audio_end > frontier:
            raise ValueError("valid audio cannot extend beyond the evidence frontier")
        self._frontier = frontier

        # Actor priority (§16.2) and reducer close order (§16.6).
        if muted:
            return self._close_once(CloseReason.MUTED)
        if session_lost:
            return self._close_once(CloseReason.SESSION_LOST)
        cells = self._cells_through(frontier)
        if gap or any(c.cls == CellClass.GAP for c in cells):
            return self._close_once(CloseReason.INTERRUPTED)
        if overrun:
            return self._close_once(CloseReason.AUDIO_OVERRUN)
        if not self._command_ends and frontier >= self.no_input_at:
            return self._close_once(CloseReason.NO_INPUT)
        maximum = TOO_LONG_EXTENDED if self.extended_utterances else TOO_LONG
        since = self._command_start if self.reply else self.trigger_sample
        if since is not None and frontier - since >= maximum:
            return self._close_once(CloseReason.TOO_LONG)

        if self.pending is not None:
            pending = self.pending
            resumed = sum(end > pending.since for end in self._command_ends) >= REVOCATION_CELLS
            if resumed:
                self.pending = None
                self.state = EndpointState.LISTENING
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

        found = self._route_r(frontier, stable) if self.esphome_reply else self._route_a(frontier, stable, cells)
        if found is None and not self.esphome_reply:
            found = self._route_b(frontier, stable, cells)
        if found is not None:
            self.pending = found
            self.state = EndpointState.END_PENDING
            return Decision(self.state, pending=found)

        # The no-progress clock starts no earlier than the command's first speech: a speaker who
        # waits after the wake word is still talking while the streaming text catches up (§16.6).
        progress = (stable.progress_sample if self._command_start is None
                    else max(stable.progress_sample, self._command_start))
        if self._command_ends and frontier - progress >= NO_PROGRESS:
            boundary = self._command_boundary_at(stable.prefix_sample) if stable.prefix_sample is not None else None
            if stable.prefix and self.completeness(stable.prefix) == GrammarClass.COMPLETE and boundary is not None:
                commit = self.finalize_once(
                    boundary,
                    route=Route.FALLBACK,
                    text=stable.prefix,
                    valid_audio_end=valid_audio_end,
                )
                return Decision(self.state, commit=commit)
            return self._close_once(CloseReason.RETRY)
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

    The full command text must be unchanged for at least 240 ms before the
    commit. From 480 ms before the first command token's emission through the
    commit boundary, fewer than half of speech-positive cells may be `self_output`.
    """
    text = normalize_command(normalized_text)
    if not text or stable.prefix_sample is None or first_command_token_sample is None:
        return False
    if stable.prefix != text:
        return False
    if stable.through_sample - stable.prefix_sample < LOCAL_STABLE:
        return False
    start = first_command_token_sample - LOCAL_PRE_ROLL
    relevant = [c for c in cells if c.speech_positive and c.end > start and c.end <= commit_boundary]
    if not relevant:
        return False
    self_output = sum(c.cls == CellClass.SELF_OUTPUT for c in relevant)
    return 2 * self_output < len(relevant)
