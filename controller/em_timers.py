"""This speaker's HA timers: picking them out by voice and targeting one through HA's intents (§10.8).

Timers are HA's ``HassTimerStatus`` entries (``speech_slots.timers``). HA's status and
cancel intents see every timer in HA, whichever device started it, so EchoMuse filters
to the speaker itself and only sends a ``HassCancelTimer`` that picks out exactly the
timer the user meant.
"""

from __future__ import annotations

from enum import StrEnum
from typing import Sequence

from echomuse_grammar import Choice, tokens
from em_ha_client import HaTimer

Start = tuple[int, int, int]      # (hours, minutes, seconds) the timer started with


def start_of(timer: HaTimer) -> Start:
    return (int(timer.get("start_hours") or 0), int(timer.get("start_minutes") or 0),
            int(timer.get("start_seconds") or 0))


def _seconds(start: Start) -> int:
    return start[0] * 3600 + start[1] * 60 + start[2]


def _norm(name: object) -> str:
    return " ".join(tokens(str(name or "")))


def matches(timer: HaTimer, start: Start | None, name: str | None) -> bool:
    """Whether "the <start> <name> timer" means `timer`. A start matches by length, so
    "the 90 second timer" is also the one started as "1 minute 30 seconds"."""
    if start is not None and _seconds(start_of(timer)) != _seconds(start):
        return False
    return name is None or _norm(timer.get("name")) == _norm(name)


def cancel_slots(target: HaTimer, every: Sequence[HaTimer], device_id: str | None) -> dict[str, object] | None:
    """`HassCancelTimer` slots under which HA picks exactly `target` from `every` timer in
    HA, or None when HA cannot tell it apart (another timer of this speaker with the same
    length and name).

    Mirrors HA's `_find_timer`: filter by name, then by the exact start units, returning a
    lone match at each step, then prefer the requesting device's timers. HA stores only
    the units a start gave, and EchoMuse's own starts give only non-zero ones."""
    slots: dict[str, object] = {unit: value for unit, value in zip(
        ("start_hours", "start_minutes", "start_seconds"), start_of(target)) if value}
    if target.get("name"):
        slots["name"] = str(target["name"])
    left = [t for t in every if not target.get("name") or _norm(t.get("name")) == _norm(target.get("name"))]
    if len(left) > 1:
        left = [t for t in left if start_of(t) == start_of(target)]
    if len(left) > 1:
        left = [t for t in left if t.get("device_id") == device_id]
    return slots if [t.get("id") for t in left] == [target.get("id")] else None


_ORDINALS = ("first", "second", "third", "fourth", "fifth")


class TimerChoice(StrEnum):
    """The "which timer?" replies that are not one timer's index."""
    ALL = "all"
    NONE = "none"


def choices(timers: Sequence[HaTimer]) -> tuple[Choice, ...]:
    """Reply choices for "which timer?": each timer by its length, its name, or its place
    in the question ("the second one"); all of them; or none. Choice values are indexes
    into `timers`. An alias two timers share selects neither."""
    items = []
    for index, timer in enumerate(timers):
        h, m, s = start_of(timer)
        length = " ".join(f"{n} {unit}" for n, unit in ((h, "hour"), (m, "minute"), (s, "second")) if n)
        names = [length] if length else []
        if timer.get("name"):
            names += [str(timer["name"]), f"{length} {timer['name']}".strip()]
        aliases = []
        for base in names:
            aliases += [base, f"the {base}", f"{base} timer", f"the {base} timer", f"the {base} one",
                        f"{base} one", f"my {base} timer"]
        if length:
            aliases += [f"{length}s", f"the {length}s"]
        if index < len(_ORDINALS):
            ordinal = _ORDINALS[index]
            aliases += [ordinal, f"the {ordinal}", f"the {ordinal} one", f"the {ordinal} timer"]
        items.append(Choice(index, tuple(aliases)))
    items.append(Choice(TimerChoice.ALL, ("all", "both", "all of them", "both of them", "all timers", "both timers",
                              "all of them please", "cancel all", "cancel both", "every timer")))
    items.append(Choice(TimerChoice.NONE, ("none", "neither", "none of them", "neither of them", "never mind",
                               "nevermind", "no", "nothing", "leave them", "leave it")))
    return tuple(items)
