"""What EchoMuse says about timers and alarms (§10.5, §10.8).

Pure formatting. Timers are HA's ``HassTimerStatus`` entries (its ``speech_slots``);
EchoMuse speaks only the timer cancels it runs itself. Alarms are the alert engine's
``list_alarms`` entries and ``set_alarm`` results.
"""

from __future__ import annotations

from datetime import datetime
from typing import Iterable, Sequence

from echomuse_grammar import AlarmParse
from em_alert_wire import DEFAULT_LABEL, KIND_SNOOZE
from em_timers import start_of

WEEKDAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")
DAY_NAMES = dict(zip(WEEKDAYS, ("Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday")))
MONTHS = ("January", "February", "March", "April", "May", "June", "July", "August",
          "September", "October", "November", "December")
MAX_SPOKEN = 5              # timers or alarms read out one by one; the rest are counted

LINE_ALARMS_UNAVAILABLE = "I couldn't check your alarms."
LINE_NO_TIMERS = "You don't have any timers."
LINE_TIMER_NOT_CANCELLED = "I couldn't cancel that timer."
LINE_WHICH_TIMER_AGAIN = "Which timer? Say its length, like the 5 minute timer, or say all of them."


def _join(items: list[str], word: str = "and") -> str:
    if len(items) <= 2:
        return f" {word} ".join(items)
    return ", ".join(items[:-1]) + f", {word} " + items[-1]


def _capital(text: str) -> str:
    return text[:1].upper() + text[1:]


def _length(timer: dict) -> str:
    """ "5 minute", "1 hour 30 minute pasta": a timer's length (and name) as an adjective."""
    parts = [f"{n} {unit}" for n, unit in zip(start_of(timer), ("hour", "minute", "second")) if n]
    return " ".join(parts + ([str(timer["name"]).strip()] if timer.get("name") else []))


def timer_label(timer: dict) -> str:
    """ "5 minute timer", "10 minute pasta timer"."""
    return f"{_length(timer)} timer".strip()


def timer_cancelled_line(timer: dict) -> str:
    """ "5 second timer cancelled.", as HA's own cancel sentences answer."""
    return _capital(f"{timer_label(timer)} cancelled.")


def timers_cancelled_line(cancelled: Sequence[dict], total: int) -> str:
    """ "5 minute and 10 minute timers cancelled." for every timer of the speaker."""
    if total == 0:
        return LINE_NO_TIMERS
    if not cancelled:
        return LINE_TIMER_NOT_CANCELLED
    if len(cancelled) == 1:
        line = timer_cancelled_line(cancelled[0])
    elif len(cancelled) > MAX_SPOKEN:
        line = f"{len(cancelled)} timers cancelled."
    else:
        line = _capital(f"{_join([_length(t) for t in cancelled])} timers cancelled.")
    return line if len(cancelled) == total else f"{line} {total - len(cancelled)} could not be cancelled."


def timer_gone_line(timer: dict) -> str:
    return f"Your {timer_label(timer)} already finished."


def which_timer_line(timers: Sequence[dict]) -> str:
    """ "Which one? Your 5 minute timer or your 10 minute timer?" """
    if len(timers) > MAX_SPOKEN:
        return f"You have {len(timers)} timers. Which one? Say its length, like the 5 minute timer."
    return f"Which one? {_capital(_join([f'your {timer_label(t)}' for t in timers], 'or'))}?"


def timers_indistinct_line(timer: dict, count: int) -> str:
    """Several of this speaker's timers share a length and name: HA cannot pick one."""
    return (f"You have {count} {timer_label(timer)}s, and I can't tell them apart. "
            "Say cancel all timers to cancel them.")


def clock_words(hour24: int, minute: int) -> str:
    """ "7 AM", "6:30 PM"."""
    return f"{hour24 % 12 or 12}{f':{minute:02d}' if minute else ''} {'AM' if hour24 < 12 else 'PM'}"


def days_words(days: Iterable[str]) -> str:
    wanted = set(days)
    if wanted == set(WEEKDAYS):
        return "every day"
    if wanted == set(WEEKDAYS[:5]):
        return "on weekdays"
    if wanted == set(WEEKDAYS[5:]):
        return "on weekends"
    return "on " + _join([DAY_NAMES[d] + "s" for d in WEEKDAYS if d in wanted])


def date_words(due: datetime, now: datetime) -> str:
    """ "today", "tomorrow", "on Saturday" within the week, else "on October 4"."""
    days = (due.date() - now.astimezone(due.tzinfo).date()).days
    if days == 0:
        return "today"
    if days == 1:
        return "tomorrow"
    if 1 < days < 7:
        return f"on {DAY_NAMES[WEEKDAYS[due.weekday()]]}"
    return f"on {MONTHS[due.month - 1]} {due.day}"


def _when(due: datetime, repeats: Iterable[str], now: datetime) -> str:
    repeats = list(repeats)
    return days_words(repeats) if repeats else date_words(due, now)


def alarm_set_line(parsed: AlarmParse, result: dict, now: datetime) -> str:
    """ "Alarm set for 7 AM tomorrow.": the first due time the alert engine computed."""
    days = result.get("days") or []
    if result.get("first_due"):
        due = datetime.fromisoformat(result["first_due"])
        return f"Alarm set for {clock_words(due.hour, due.minute)} {_when(due, days, now)}."
    # Still pending in HA: the parse is all there is.
    when = f" {days_words(days)}" if days else ""
    return f"Alarm set for {clock_words(parsed.hour24, parsed.minute or 0)}{when}."


def alarm_cancel_line(result: dict) -> str:
    if not result.get("ok"):
        return "Which alarm? Say its time." if result.get("error") == "which alarm" \
            else "I couldn't find that alarm."
    count = len(result.get("cancelled") or [])
    return "Alarm cancelled." if count <= 1 else f"Cancelled {count} alarms."


def _alarm_item(alarm: dict, now: datetime) -> str:
    due = datetime.fromisoformat(alarm["due"])
    at = f"{clock_words(due.hour, due.minute)} {_when(due, alarm.get('repeats') or [], now)}"
    if alarm.get("kind") == KIND_SNOOZE:
        return f"a snoozed alarm at {at}"
    name = str(alarm.get("name") or "").strip()
    return at if not name or name == DEFAULT_LABEL else f"{name} at {at}"


def alarms_line(alarms: list[dict], now: datetime, *, next_only: bool = False) -> str:
    """The speaker's alarms, one entry per schedule (a repeating alarm is listed once, by
    its days), soonest first."""
    schedules: dict[str, dict] = {}
    for alarm in sorted(alarms, key=lambda a: datetime.fromisoformat(a["due"])):
        schedules.setdefault(alarm.get("schedule_id") or alarm["due"], alarm)
    items = [_alarm_item(a, now) for a in schedules.values()]
    if not items:
        return "You don't have any alarms."
    if next_only:
        return f"Your next alarm is {items[0]}."
    spoken = items[:MAX_SPOKEN]
    extra = len(items) - len(spoken)
    return (f"You have {len(items)} alarm{'' if len(items) == 1 else 's'}: "
            + _join(spoken + ([f"{extra} more"] if extra else [])) + ".")
