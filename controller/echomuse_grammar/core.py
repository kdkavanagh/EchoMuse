"""Pure grammar matching and authoritative alarm parsing (§16.6)."""

from __future__ import annotations

from dataclasses import dataclass, field
from functools import lru_cache
from typing import Any, Iterable, Literal, Mapping, Sequence

from .normalize import normalize, tokens
from .timer_data import SKIP_WORDS, SLOT_RANGES, TIMER_PATTERNS

COMPLETE = "complete"
EXTENDABLE = "extendable"
NEEDS_MORE = "needs_more"
UNKNOWN = "unknown"
_CLASSES = {COMPLETE, EXTENDABLE, NEEDS_MORE, UNKNOWN}
_TERMINAL = frozenset({"a", "an", "the", "for", "to", "in", "on", "at", "and", "or", "with"})
_WEEKDAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")
_DAY_NAMES = {
    "monday": "mon", "tuesday": "tue", "wednesday": "wed", "thursday": "thu",
    "friday": "fri", "saturday": "sat", "sunday": "sun",
}


@dataclass(frozen=True)
class Result:
    """A §16.6 completeness judgment.

    ``family`` is None only for ``unknown`` from :func:`classify`. ``parse`` is
    family-specific: timer → tuple of matching HA intent names, alarm →
    :class:`AlarmParse`, home → :class:`HomeParse`, local → :class:`LocalParse`,
    reply → the selected choice value; None when no parse exists.
    """

    klass: Literal["complete", "extendable", "needs_more", "unknown"]
    family: str | None = None
    parse: Any = None

    def __post_init__(self) -> None:
        if self.klass not in _CLASSES:
            raise ValueError(f"invalid grammar class: {self.klass}")


@dataclass(frozen=True)
class AlarmParse:
    """Authoritative alarm parse (§10.5, §9.1 ``pending_operation``).

    ``hour24``/``minute`` are local wall-clock time; ``hour24`` is None while
    ``missing == "ampm"``. ``days`` is a set of ``mon``..``sun`` for repeating
    alarms (``every day``/``daily`` = all seven), ``"today"``/``"tomorrow"``,
    or None for the next occurrence. Cancel parses carry no days; a cancel
    without a time has ``hour24 = minute = None``.
    """

    action: Literal["set", "cancel", "cancel_all"]
    hour24: int | None
    minute: int | None
    days: frozenset[str] | Literal["today", "tomorrow"] | None
    missing: Literal["ampm"] | None = None
    hour12: int | None = None
    ampm: Literal["am", "pm"] | None = None


@dataclass(frozen=True)
class HomeParse:
    action: Literal["on", "off"]
    target: str


@dataclass(frozen=True)
class LocalParse:
    """A §16.2 local command: bare ``stop|cancel|snooze`` (kind None) or a noun form."""

    action: Literal["stop", "cancel", "snooze", "turn off", "dismiss"]
    kind: Literal["timer", "alarm"] | None = None
    name: str | None = None


@dataclass(frozen=True)
class Choice:
    value: Any
    aliases: tuple[str, ...]


@dataclass(frozen=True)
class CommandContext:
    """Command context captured at wake acceptance (§6.3 step 1)."""

    kind: Literal["alert", "dialog"]
    alert_kind: Literal["timer", "alarm"] | None = None
    name: str | None = None


@dataclass
class _State:
    eps: list[int] = field(default_factory=list)
    words: list[tuple[str, int]] = field(default_factory=list)
    slots: list[tuple[str, int]] = field(default_factory=list)
    finals: set[str] = field(default_factory=set)
    pending_duration: bool = False


def _word_index(edges: list[tuple[str, int]]) -> dict[str, tuple[int, ...]]:
    index: dict[str, list[int]] = {}
    for word, target in edges:
        index.setdefault(word, []).append(target)
    return {word: tuple(targets) for word, targets in index.items()}


class _TimerAutomaton:
    """Nondeterministic automaton compiled from the generated timer patterns."""

    def __init__(self) -> None:
        self.states = [_State()]
        for intent, node in TIMER_PATTERNS:
            start = self._state()
            end = self._state()
            self.states[0].eps.append(start)
            self._compile(node, start, end)
            self.states[end].finals.add(intent)
        self.closures = tuple(frozenset(self._closure((state,))) for state in range(len(self.states)))
        self.word_edges: tuple[dict[str, tuple[int, ...]], ...] = tuple(
            _word_index(state.words) for state in self.states
        )

    def _state(self) -> int:
        self.states.append(_State())
        return len(self.states) - 1

    def _compile(self, node: tuple, start: int, end: int) -> None:
        kind, value = node
        if kind == "word":
            self.states[start].words.append((value, end))
        elif kind == "slot":
            self.states[start].slots.append((value, end))
            if value.startswith("timer_") and value not in {"timer_name", "timer_command"}:
                self.states[end].pending_duration = True
        elif kind == "optional":
            self.states[start].eps.append(end)
            self._compile(value, start, end)
        elif kind == "alt":
            for child in value:
                self._compile(child, start, end)
        elif kind == "seq":
            if not value:
                self.states[start].eps.append(end)
                return
            current = start
            for index, child in enumerate(value):
                following = end if index == len(value) - 1 else self._state()
                self._compile(child, current, following)
                current = following
        else:
            raise ValueError(f"unknown generated node: {kind}")

    def _closure(self, states: Iterable[int]) -> set[int]:
        result = set(states)
        stack = list(result)
        while stack:
            state = stack.pop()
            for target in self.states[state].eps:
                if target not in result:
                    result.add(target)
                    stack.append(target)
        return result

    def _slot_ends(self, slot: str, words: tuple[str, ...], pos: int,
                   vocabulary: tuple[tuple[str, ...], ...]) -> Iterable[int]:
        if pos >= len(words):
            return ()
        if slot in SLOT_RANGES:
            try:
                value = int(words[pos])
            except ValueError:
                return ()
            low, high = SLOT_RANGES[slot]
            return (pos + 1,) if low <= value <= high else ()
        if slot == "timer_half":
            return (pos + 1,) if words[pos] in {"half", "1/2"} else ()
        if slot == "area":
            return tuple(pos + len(item) for item in vocabulary if words[pos : pos + len(item)] == item)
        if slot in {"timer_name", "timer_command"}:
            if words[pos].isdigit() or words[pos] in _TERMINAL:
                return ()
            return tuple(
                end for end in range(pos + 1, len(words) + 1)
                if not words[end - 1].isdigit() and words[end - 1] not in _TERMINAL
            )
        return ()

    @lru_cache(maxsize=4096)
    def _step(self, states: frozenset[int]) -> tuple[dict[str, frozenset[int]], tuple[tuple[str, frozenset[int]], ...], bool, frozenset[str]]:
        """Transitions out of a closed state set: word edges, slot edges, needs-more flag, final intents."""

        words: dict[str, set[int]] = {}
        slots: dict[str, set[int]] = {}
        for state in states:
            for word, targets in self.word_edges[state].items():
                words.setdefault(word, set()).update(*(self.closures[target] for target in targets))
            for slot, target in self.states[state].slots:
                slots.setdefault(slot, set()).update(self.closures[target])
        pending = any(self.states[state].pending_duration for state in states) or bool(slots)
        finals = frozenset(intent for state in states for intent in self.states[state].finals)
        return (
            {word: frozenset(targets) for word, targets in words.items()},
            tuple((slot, frozenset(targets)) for slot, targets in sorted(slots.items())),
            pending,
            finals,
        )

    def match(self, words: tuple[str, ...], vocabulary: tuple[tuple[str, ...], ...]) -> Result:
        # Slot and word edges always advance, so positions can be settled in order.
        active: list[set[int]] = [set() for _ in range(len(words) + 1)]
        active[0].update(self.closures[0])
        for pos in range(len(words)):
            if not active[pos]:
                continue
            word_map, slot_edges, _, _ = self._step(frozenset(active[pos]))
            active[pos + 1].update(word_map.get(words[pos], ()))
            for slot, targets in slot_edges:
                for ending in self._slot_ends(slot, words, pos, vocabulary):
                    active[ending].update(targets)
        if not words or not active[-1]:
            return Result(UNKNOWN, "timer")
        _, _, pending, finals = self._step(frozenset(active[-1]))
        if finals:
            return Result(COMPLETE, "timer", tuple(sorted(finals)))
        return Result(NEEDS_MORE if pending else UNKNOWN, "timer")


_TIMER = _TimerAutomaton()
_SKIP = tuple(tuple(normalize(value).split()) for value in sorted(SKIP_WORDS, key=lambda item: (-len(item.split()), item)))


def _without_skip_words(words: tuple[str, ...]) -> tuple[str, ...]:
    result = list(words)
    index = 0
    while index < len(result):
        for phrase in _SKIP:
            if tuple(result[index : index + len(phrase)]) == phrase:
                del result[index : index + len(phrase)]
                break
        else:
            index += 1
    return tuple(result)


def _timer(text: str, vocabulary: Iterable[str]) -> Result:
    vocab = tuple(sorted({tokens(value) for value in vocabulary if normalize(value)}))
    return _TIMER.match(_without_skip_words(tokens(text)), vocab)


_AMPM_PHRASES: tuple[tuple[tuple[str, ...], Literal["am", "pm"]], ...] = (
    (("am",), "am"),
    (("pm",), "pm"),
    (("in", "the", "morning"), "am"),
    (("in", "the", "afternoon"), "pm"),
    (("in", "the", "evening"), "pm"),
    (("at", "night"), "pm"),
)


@dataclass(frozen=True)
class _Time:
    end: int
    hour12: int
    minute: int
    ampm: Literal["am", "pm"] | None
    hour24: int | None


def _int(word: str) -> int | None:
    return int(word) if word.isdigit() else None


def _hour24(hour12: int, ampm: str, phrase: tuple[str, ...]) -> int:
    # "12 at night" is midnight, not noon; every other pm phrase adds 12.
    if hour12 == 12:
        return 0 if ampm == "am" or phrase == ("at", "night") else 12
    return hour12 + (12 if ampm == "pm" else 0)


def _times(words: tuple[str, ...], pos: int) -> list[_Time]:
    """Every §16.6 ``time`` reading at ``pos``; ``ampm None`` marks a missing slot."""

    if pos >= len(words):
        return []
    if words[pos] == "noon":
        return [_Time(pos + 1, 12, 0, "pm", 12)]
    if words[pos] == "midnight":
        return [_Time(pos + 1, 12, 0, "am", 0)]
    hour = _int(words[pos])
    if hour is None or not 1 <= hour <= 12:
        return []
    minutes = [(pos + 1, 0)]
    following = _int(words[pos + 1]) if pos + 1 < len(words) else None
    if following is not None and 0 <= following <= 59:
        minutes.append((pos + 2, following))
    if words[pos + 1 : pos + 2] == ("oh",) and pos + 2 < len(words):
        oh_minute = _int(words[pos + 2])
        if oh_minute is not None and 1 <= oh_minute <= 9:
            minutes.append((pos + 3, oh_minute))
    readings: list[_Time] = []
    for end, minute in minutes:
        for phrase, ampm in _AMPM_PHRASES:
            if words[end : end + len(phrase)] == phrase:
                readings.append(_Time(end + len(phrase), hour, minute, ampm, _hour24(hour, ampm, phrase)))
        readings.append(_Time(end, hour, minute, None, None))
    return readings


def _days(words: tuple[str, ...], pos: int) -> frozenset[str] | str | None | bool:
    """Parse the §16.6 ``days`` tail from ``pos`` to the end; False if it is not one."""

    rest = words[pos:]
    if not rest:
        return None
    if rest in (("today",), ("tomorrow",)):
        return rest[0]
    if rest in (("every", "day"), ("daily",)):
        return frozenset(_WEEKDAYS)
    if rest in (("weekdays",), ("on", "weekdays")):
        return frozenset(_WEEKDAYS[:5])
    if rest in (("weekends",), ("on", "weekends")):
        return frozenset(_WEEKDAYS[5:])
    if rest[0] in {"every", "on"}:
        rest = rest[1:]
    found: list[str] = []
    expect_day = True
    for word in rest:
        if word in _DAY_NAMES:
            # Commas are erased by normalization, so adjacent days are a list too.
            if _DAY_NAMES[word] in found:
                return False
            found.append(_DAY_NAMES[word])
            expect_day = False
        elif word == "and" and not expect_day:
            expect_day = True
        else:
            return False
    return frozenset(found) if found and not expect_day else False


def _set_start(words: tuple[str, ...]) -> int | None:
    """Index of ``time`` after an ``alarm_set`` lead-in, else None."""

    if words[:1] in (("set",), ("create",), ("make",)):
        pos = 2 if words[1:2] in (("an",), ("a",)) else 1
        if words[pos : pos + 1] == ("alarm",) and words[pos + 1 : pos + 2] in (("for",), ("at",)):
            return pos + 2
        return None
    if words[:2] == ("wake", "me"):
        pos = 3 if words[2:3] == ("up",) else 2
        if words[pos : pos + 1] in (("at",), ("for",)):
            return pos + 1
    return None


def _parse_alarm_words(words: tuple[str, ...]) -> AlarmParse | None:
    if words[:1] in (("cancel",), ("delete",), ("remove",)) and words[1:2] == ("all",):
        pos = 3 if words[2:3] in (("my",), ("the",)) else 2
        return AlarmParse("cancel_all", None, None, None) if words[pos:] == ("alarms",) else None
    start = _set_start(words)
    if start is not None:
        for time in _times(words, start):
            days = _days(words, time.end)
            if days is not False:
                return AlarmParse("set", time.hour24, time.minute, days,
                                  None if time.ampm else "ampm", time.hour12, time.ampm)
        return None
    for verb in (("cancel",), ("delete",), ("remove",), ("turn", "off")):
        if words[: len(verb)] != verb:
            continue
        pos = len(verb) + (1 if words[len(verb) : len(verb) + 1] in (("my",), ("the",)) else 0)
        if words[pos:] == ("alarm",):
            return AlarmParse("cancel", None, None, None)
        for time in _times(words, pos):
            if words[time.end :] == ("alarm",):
                return AlarmParse("cancel", time.hour24, time.minute, None,
                                  None if time.ampm else "ampm", time.hour12, time.ampm)
    return None


def parse_alarm(text: str) -> AlarmParse | None:
    """Authoritative parse of the §16.6 alarm EBNF (§10.5).

    Returns None when ``text`` is not an alarm sentence. An hour spoken
    without am/pm parses with ``missing="ampm"`` and ``hour24=None``; the
    caller asks "AM or PM?" with ``AMPM_CHOICES`` (§9.1).
    """

    return _parse_alarm_words(tokens(text))


# Continuations that finish an alarm prefix; used only to detect needs_more.
_ALARM_CONTINUATIONS: tuple[tuple[str, ...], ...] = (
    ("alarm",), ("alarms",), ("am",), ("morning",), ("the", "morning"), ("night",),
    ("7", "am"), ("7", "am", "alarm"), ("for", "7", "am"), ("alarm", "for", "7", "am"),
    ("up", "at", "7", "am"), ("day",), ("weekdays",), ("monday",), ("friday",),
)


def _alarm_result(text: str) -> Result:
    words = tokens(text)
    parsed = _parse_alarm_words(words)
    if parsed is not None:
        return Result(NEEDS_MORE if parsed.missing else COMPLETE, "alarm", parsed)
    anchored = "alarm" in words or words[:2] == ("wake", "me") or words[1:2] == ("all",)
    if anchored and any(
        (completed := _parse_alarm_words(words + tail)) is not None and completed.missing is None
        for tail in _ALARM_CONTINUATIONS
    ):
        return Result(NEEDS_MORE, "alarm")
    return Result(UNKNOWN, "alarm")


def _local_syntax(text: str) -> Result:
    """§16.2 local-command syntax, independent of the command context."""

    words = tokens(text)
    bare = words[1:] if words[:1] == ("please",) else words[:-1] if words[-1:] == ("please",) else words
    if bare in (("stop",), ("cancel",), ("snooze",)):
        return Result(COMPLETE, "local", LocalParse(bare[0]))
    if bare[:2] == ("snooze", "for"):
        # "snooze for" (and a number awaiting its unit) needs more; any snooze
        # duration is not a local command and goes to HA.
        if len(bare) == 2 or (len(bare) == 3 and bare[2].isdigit()):
            return Result(NEEDS_MORE, "local")
        return Result(UNKNOWN, "local")
    body = words[:-1] if words[-1:] == ("please",) else words
    for action in (("turn", "off"), ("stop",), ("cancel",), ("dismiss",)):
        if body[: len(action)] != action:
            continue
        rest = body[len(action):]
        if rest[:1] == ("the",):
            rest = rest[1:]
        if not rest and body == words:
            return Result(NEEDS_MORE, "local")
        if rest[-1:] in (("timer",), ("alarm",)):
            name = " ".join(rest[:-1]) or None
            return Result(COMPLETE, "local", LocalParse(" ".join(action), rest[-1], name))
    return Result(UNKNOWN, "local")


def match_local_command(text: str, context: CommandContext | None) -> str | None:
    """Return the §6.3 local action for ``text`` under ``context``, else None.

    ``"dismiss"`` (alert context: stop/cancel, or a noun form naming the
    ringing kind and, if spoken, its exact name), ``"snooze"`` (alarm only), or
    ``"stop"`` (dialog context: close the turn). Anything else is an HA turn.
    """

    if context is None:
        return None
    result = _local_syntax(text)
    if result.klass != COMPLETE:
        return None
    parsed: LocalParse = result.parse
    if parsed.kind is None:
        if parsed.action in {"stop", "cancel"}:
            return "dismiss" if context.kind == "alert" else "stop"
        if parsed.action == "snooze" and context.kind == "alert" and context.alert_kind == "alarm":
            return "snooze"
        return None
    if context.kind != "alert" or parsed.kind != context.alert_kind:
        return None
    if parsed.name is not None and parsed.name != (normalize(context.name or "") or None):
        return None
    return "dismiss"


def _home(text: str, vocabulary: Iterable[str]) -> Result:
    words = tokens(text)
    targets = {tokens(value): normalize(value) for value in vocabulary if normalize(value)}
    if not targets or not words:
        return Result(UNKNOWN, "home")
    candidates: list[tuple[str, tuple[str, ...]]] = []
    if len(words) >= 2 and words[0] in {"turn", "switch"} and words[1] in {"on", "off"}:
        pos = 2
        if pos < len(words) and words[pos] == "the":
            pos += 1
        if pos == len(words):
            return Result(NEEDS_MORE, "home")
        candidates.append((words[1], words[pos:]))
    if len(words) >= 2 and words[-1] in {"on", "off"}:
        candidates.append((words[-1], words[:-1]))
    for action, target in candidates:
        if target in targets:
            extendable = any(len(other) > len(target) and other[: len(target)] == target for other in targets)
            klass = EXTENDABLE if extendable else COMPLETE
            return Result(klass, "home", HomeParse(action, targets[target]))
        if any(len(other) > len(target) and other[: len(target)] == target for other in targets):
            return Result(NEEDS_MORE, "home")
    if words in targets or any(len(words) < len(target) and target[: len(words)] == words for target in targets):
        # A bare target still needs its "on"/"off"; a strict target prefix needs the rest.
        return Result(NEEDS_MORE, "home")
    return Result(UNKNOWN, "home")


def _choice_items(choices: Sequence[Choice | Mapping[str, Any]] | None) -> tuple[Choice, ...]:
    if choices is None:
        return ()
    result = []
    for choice in choices:
        if isinstance(choice, Choice):
            result.append(choice)
        else:
            result.append(Choice(choice["value"], tuple(choice["aliases"])))
    return tuple(result)


def match_choice(text: str, choices: Sequence[Choice | Mapping[str, Any]]) -> Any | None:
    """Return the unique choice value whose alias exactly equals the reply."""

    wanted = normalize(text)
    matches = {choice.value for choice in _choice_items(choices) if wanted in {normalize(alias) for alias in choice.aliases}}
    return next(iter(matches)) if len(matches) == 1 else None


def _reply(text: str, choices: Sequence[Choice | Mapping[str, Any]] | None) -> Result:
    items = _choice_items(choices)
    matched = match_choice(text, items)
    if matched is not None:
        return Result(COMPLETE, "reply", matched)
    words = tokens(text)
    if words and any(words == tokens(alias)[: len(words)] and len(words) < len(tokens(alias))
                     for choice in items for alias in choice.aliases):
        return Result(NEEDS_MORE, "reply")
    return Result(UNKNOWN, "reply")


FAMILIES = ("reply", "alarm", "timer", "home", "local")
_RANK = {UNKNOWN: 0, NEEDS_MORE: 1, EXTENDABLE: 2, COMPLETE: 3}


def family_result(family: str, text: str, vocabulary: Iterable[str] = (),
                  choices: Sequence[Choice | Mapping[str, Any]] | None = None) -> Result:
    """Classify ``text`` against one family, including the terminal-token rule."""

    if family == "reply":
        result = _reply(text, choices)
    elif family == "alarm":
        result = _alarm_result(text)
    elif family == "timer":
        result = _timer(text, vocabulary)
    elif family == "home":
        result = _home(text, vocabulary)
    elif family == "local":
        result = _local_syntax(text)
    else:
        raise ValueError(f"unknown grammar family: {family}")
    words = tokens(text)
    if result.klass in {COMPLETE, EXTENDABLE} and words and words[-1] in _TERMINAL:
        # §16.6 table: "<exposed target> on/off" is complete; its on/off is the
        # command itself, not a dangling preposition.
        if not (isinstance(result.parse, HomeParse) and words[-1] == result.parse.action):
            return Result(NEEDS_MORE, result.family, result.parse)
    return result


def classify(text: str, vocabulary: Iterable[str] = (),
             choices: Sequence[Choice | Mapping[str, Any]] | None = None) -> Result:
    """Classify ``text`` across every §16.6 family; the strongest class wins.

    Ties go to the first family in ``FAMILIES``. Reply matching needs the
    expectation's ``choices``; ``vocabulary`` is the §16.7 target snapshot.
    """

    vocabulary = tuple(vocabulary)
    best = Result(UNKNOWN)
    for family in FAMILIES:
        result = family_result(family, text, vocabulary, choices)
        if _RANK[result.klass] > _RANK[best.klass]:
            best = result
    return best


AMPM_CHOICES = (
    Choice("am", ("am", "a m", "morning", "in the morning")),
    Choice("pm", ("pm", "p m", "evening", "in the evening", "afternoon", "at night")),
)
