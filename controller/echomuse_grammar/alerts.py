"""Authoritative parses of the timer cancels and alarm questions EchoMuse answers itself
(§10.8, §16.6).

Every other timer request is HA's: its own timer intents, matched locally by the custom
sentences in the HA configuration. HA's cancel answer cannot say which timer it
cancelled, so a bare "cancel the timer" and "cancel all timers" are resolved here
against HA's ``HassTimerStatus`` and run as HA's ``HassCancelTimer``. Alarms live in
the alert engine, so alarm questions are answered here too.
"""

from __future__ import annotations

import re
from dataclasses import dataclass

from .normalize import tokens

_UNITS = {
    "hour": "hours", "hours": "hours", "hr": "hours", "hrs": "hours",
    "minute": "minutes", "minutes": "minutes", "min": "minutes", "mins": "minutes",
    "second": "seconds", "seconds": "seconds", "sec": "seconds", "secs": "seconds",
}
_UNIT_RANK = {"hours": 0, "minutes": 1, "seconds": 2}
# Half of a unit, in the next smaller one.
_HALF = {"hours": ("minutes", 30), "minutes": ("seconds", 30)}

_NAME_TAIL = frozenset({"called", "named", "labeled", "labelled"})
_LEADS = (("can", "you", "please"), ("could", "you", "please"), ("would", "you", "please"),
          ("can", "you"), ("could", "you"), ("would", "you"), ("will", "you"), ("please",))
# Words that never form a timer name: grammar words, units, and verbs of other timer intents.
_NOT_NAME = frozenset({
    "a", "an", "the", "my", "another", "new", "for", "of", "to", "in", "on", "at", "and", "or",
    "with", "timer", "timers", "alarm", "alarms", "half", "called", "named", "labeled", "labelled",
    "set", "start", "create", "make", "begin", "cancel", "stop", "pause", "resume", "unpause",
    "continue", "add", "remove", "subtract", "delete", "extend", "restart", "reset", "check",
    "what", "which", "how", "is", "are", "please", "up",
}) | frozenset(_UNITS)
_MAX_NAME_WORDS = 3


def _strip_polite(words: tuple[str, ...]) -> tuple[str, ...]:
    for lead in _LEADS:
        if words[: len(lead)] == lead:
            words = words[len(lead):]
            break
    return words[:-1] if words[-1:] == ("please",) else words


def _prefix(words: tuple[str, ...], pos: int, options: tuple[tuple[str, ...], ...]) -> int | None:
    """End of the first option starting at ``pos``, else None."""
    for option in options:
        if words[pos : pos + len(option)] == option:
            return pos + len(option)
    return None


def _name(words: tuple[str, ...]) -> str | None:
    if not 1 <= len(words) <= _MAX_NAME_WORDS:
        return None
    if any(word in _NOT_NAME or word.isdigit() or word == "1/2" for word in words):
        return None
    return " ".join(words)


def _count(word: str) -> int | None:
    if word.isdigit():
        return int(word)
    return 1 if word in ("a", "an") else None


def _duration(words: tuple[str, ...]) -> dict[str, int] | None:
    """Parse all of ``words`` as a duration: parts in descending units, each unit once.

    ``N unit``, ``a|an unit``, ``N and a half unit``, ``N 1/2 unit``, ``… unit and a half``,
    ``half an|a unit`` and ``a half unit``, joined by optional ``and``. A half
    hour is 30 minutes and a half minute 30 seconds; half a second is not a timer.
    """
    parts: dict[str, int] = {}

    def add(unit: str, value: int) -> bool:
        if unit in parts or any(_UNIT_RANK[u] >= _UNIT_RANK[unit] for u in parts):
            return False
        parts[unit] = value
        return True

    def add_half(unit: str) -> bool:
        half = _HALF.get(unit)
        return half is not None and add(*half)

    i = 0
    while i < len(words):
        if parts and words[i] == "and" and words[i + 1 : i + 3] != ("a", "half"):
            i += 1
            if i == len(words):
                return None                # "1 hour and": the rest was not said
        rest = words[i:]
        if rest[:2] in (("half", "an"), ("half", "a"), ("a", "half")) and len(rest) > 2 and rest[2] in _UNITS:
            if not add_half(_UNITS[rest[2]]):
                return None
            i += 3
            continue
        count = _count(rest[0]) if rest else None
        if count is None:
            return None
        if rest[1:4] == ("and", "a", "half") and len(rest) > 4 and rest[4] in _UNITS:
            unit = _UNITS[rest[4]]
            if not (add(unit, count) and add_half(unit)):
                return None
            i += 5
            continue
        if rest[1:2] == ("1/2",) and len(rest) > 2 and rest[2] in _UNITS:
            unit = _UNITS[rest[2]]
            if not (add(unit, count) and add_half(unit)):
                return None
            i += 3
            continue
        if len(rest) < 2 or rest[1] not in _UNITS:
            return None
        unit = _UNITS[rest[1]]
        if not add(unit, count):
            return None
        i += 2
        if words[i : i + 3] == ("and", "a", "half"):
            if not add_half(unit):
                return None
            i += 3
    if not parts or sum(parts.values()) == 0:
        return None
    return parts


# ── timer references: "the 5 minute timer", "the pasta timer" ────────────────

Start = tuple[int, int, int]      # a timer's starting duration as spoken: (hours, minutes, seconds)


def _start_of(duration: dict[str, int]) -> Start:
    return duration.get("hours", 0), duration.get("minutes", 0), duration.get("seconds", 0)


def _timer_ref(words: tuple[str, ...]) -> tuple[Start | None, str | None] | None:
    """The words naming one timer before ``timer``: a starting duration, a name, or a
    duration then a name ("10 minute pasta"). A bare unit counts one ("the hour timer").
    ``(None, None)`` for no words; None when the words are neither."""
    if not words:
        return None, None
    if words[0] in _UNITS:
        words = ("1",) + words
    for split in range(len(words), 0, -1):
        duration = _duration(words[:split])
        if duration is None:
            continue
        rest = words[split:]
        name = _name(rest) if rest else None
        return (_start_of(duration), name) if not rest or name is not None else None
    name = _name(words)
    return (None, name) if name is not None else None


@dataclass(frozen=True)
class TimerCancel:
    """A voice timer cancel: every timer (``all``), or the one ``start`` and/or ``name``
    pick out; neither given means "the timer", which needs exactly one."""

    start: Start | None = None
    name: str | None = None
    all: bool = False


_CANCEL_VERBS = (("turn", "off"), ("cancel",), ("stop",), ("delete",), ("remove",), ("end",), ("clear",))
_ALL_TIMERS = (
    ("all", "of", "my"), ("all", "of", "the"), ("all", "my"), ("all", "the"), ("all",),
    ("both", "of", "my"), ("both", "my"), ("both", "the"), ("both",), ("my",), ("the",),
)
_THE = (("the",), ("my",), ("this",), ("that",))


def parse_timer_cancel(text: str) -> TimerCancel | None:
    """Authoritative parse of a voice timer cancel; None when ``text`` is not one.

    "cancel the timer", "cancel the 5 minute timer", "stop the pasta timer",
    "cancel the timer called pasta", "cancel the timer for 10 minutes",
    "cancel all (my) timers", "cancel every timer", "cancel both timers".
    """
    words = tokens(text)
    if not words:
        return None
    words = _strip_polite(words)
    pos = _prefix(words, 0, _CANCEL_VERBS)
    if pos is None:
        return None
    rest = words[pos:]
    if rest in (("every", "timer"), ("each", "timer")):
        return TimerCancel(all=True)
    if rest[-1:] == ("timers",):
        lead = rest[:-1]
        return TimerCancel(all=True) if lead == () or lead in _ALL_TIMERS else None
    if "timer" not in rest:
        return None
    det = _prefix(rest, 0, _THE) or 0
    at = rest.index("timer", det)
    ref = _timer_ref(rest[det:at])
    if ref is None:
        return None
    start, name = ref
    tail = rest[at + 1 :]
    if not tail:
        return TimerCancel(start, name)
    if start is not None or name is not None:
        return None
    if tail[0] in _NAME_TAIL:
        name = _name(tail[1:])
        return TimerCancel(None, name) if name is not None else None
    if tail[0] == "for":
        after = tail[1:]
        duration = _duration(after)
        if duration is not None:
            return TimerCancel(_start_of(duration))
        if after[:1] in (("the",), ("my",)):
            after = after[1:]
        name = _name(after)
        return TimerCancel(None, name) if name is not None else None
    return None


# ── alarm questions ──────────────────────────────────────────────────────────
# Timer questions are HA's own local intents (HassTimerStatus, custom sentences in
# the HA configuration); alarms live in EchoMuse's alert engine, so EchoMuse answers.

@dataclass(frozen=True)
class AlarmQuery:
    """A question about this speaker's alarms; ``next_only`` asks for the next one."""

    next_only: bool = False


_STATE = r"(?: (?:set|running|going|active|scheduled|set up))?"
_ALARM_QUESTIONS = tuple(re.compile(pattern) for pattern in (
    rf"(?:what|which) alarms?(?: (?:are|are there|do i have|have i|have i got|did i))?{_STATE}",
    rf"(?:do|have) i (?:have|got) any alarms?{_STATE}",
    rf"(?:are|is) there any alarms?{_STATE}",
    rf"how many alarms?(?: (?:do i have|are there|are|have i))?{_STATE}",
    r"(?:list|show|check|tell me|read)(?: me)?(?: all)?(?: of)?(?: (?:my|the))? alarms?",
    r"(?:what are|what's|whats|what is) (?:my|the) alarms",
    r"(?:status of|check on)(?: (?:my|the))? alarms?",
    r"alarms? status",
    r"(?:when is|when's|whens) my(?P<next> next)? alarm(?: set for| for)?",
    r"what time is my(?P<next> next)? alarm(?: set for| for| set)?",
    r"(?:what is|what's|whats) my(?P<next> next)? alarm(?: set)?(?: for)?",
    r"is (?:my|the|an|there an)(?P<next> next)? alarm set",
    r"(?:what time|when) (?:does|will) my(?P<next> next)? alarm go off",
    r"(?:do|have) i (?:have|got) an alarm(?: set)?",
))


def parse_alarm_query(text: str) -> AlarmQuery | None:
    """Authoritative parse of a question about this speaker's alarms; None when ``text``
    is not one."""
    words = tokens(text)
    if not words:
        return None
    sentence = " ".join(_strip_polite(words))
    for pattern in _ALARM_QUESTIONS:
        match = pattern.fullmatch(sentence)
        if match is not None:
            return AlarmQuery(bool(match.groupdict().get("next")))
    return None
