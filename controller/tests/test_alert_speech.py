"""What the speaker says about timers and alarms, and which HA timer a cancel targets."""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import em_alert_speech as speech
import em_timers
from echomuse_grammar import match_choice

EDT = timezone(timedelta(hours=-4))
NOW = datetime(2026, 9, 28, 22, 0, tzinfo=EDT)            # a Monday evening


def timer(id_="t", name="", left=60, active=True, start=(0, 5, 0), device="office"):
    return {"id": id_, "name": name, "device_id": device, "total_seconds_left": left, "is_active": active,
            "start_hours": start[0], "start_minutes": start[1], "start_seconds": start[2]}


def test_a_cancel_names_the_timers_it_cancelled():
    five_sec = timer("s", start=(0, 0, 5))
    assert speech.timer_cancelled_line(five_sec) == "5 second timer cancelled."
    assert speech.timer_cancelled_line(timer("p", "pasta", start=(1, 5, 0))) == "1 hour 5 minute pasta timer cancelled."
    both = [timer("a"), timer("b", start=(0, 10, 0))]
    assert speech.timers_cancelled_line(both, 2) == "5 minute and 10 minute timers cancelled."
    assert speech.timers_cancelled_line(both[:1], 2) == "5 minute timer cancelled. 1 could not be cancelled."
    assert speech.timers_cancelled_line([], 0) == "You don't have any timers."


def test_a_cancel_targets_one_timer_only_when_ha_can_tell_it_apart():
    five, ten, pasta = timer("a"), timer("b", start=(0, 10, 0)), timer("c", "pasta")
    kitchen = timer("k", device="kitchen")               # another speaker's 5 minute timer
    every = [five, ten, pasta, kitchen]
    assert em_timers.cancel_slots(ten, every, "office") == {"start_minutes": 10}
    assert em_timers.cancel_slots(pasta, every, "office") == {"start_minutes": 5, "name": "pasta"}
    # HA filters by length before device: another speaker's 5 minute timer does not matter...
    assert em_timers.cancel_slots(five, [five, ten, kitchen], "office") == {"start_minutes": 5}
    # ...but this speaker's 5 minute pasta timer does: HA would refuse, so nothing is sent.
    assert em_timers.cancel_slots(five, every, "office") is None
    assert em_timers.cancel_slots(five, [five, timer("a2")], "office") is None
    # "the 90 second timer" is the one started as 1 minute 30 seconds.
    assert em_timers.matches(timer(start=(0, 1, 30)), (0, 0, 90), None)


def test_which_timer_is_answered_by_length_name_or_place():
    timers = [timer("a"), timer("b", start=(0, 10, 0)), timer("c", "pasta", start=(0, 11, 0))]
    assert speech.which_timer_line(timers) == \
        "Which one? Your 5 minute timer, your 10 minute timer, or your 11 minute pasta timer?"
    choices = em_timers.choices(timers)
    assert [match_choice(r, choices) for r in ("the ten minute one", "The pasta timer.", "the first one",
                                               "both", "never mind", "the 12 minute one")] == \
        [1, 2, 0, em_timers.ALL, em_timers.NONE, None]
    # A length two offered timers share picks neither.
    assert match_choice("the 5 minute one", em_timers.choices([timer("a"), timer("a2")])) is None


def alarm(due, schedule, name="Alarm", repeats=(), kind="alarm"):
    return {"name": name, "kind": kind, "due": due, "repeats": list(repeats), "schedule_id": schedule}


def test_a_repeating_alarm_is_listed_once_by_its_days_among_the_others_by_due_time():
    weekdays = ("mon", "tue", "wed", "thu", "fri")
    alarms = [
        alarm("2026-10-10T09:15:00-04:00", "d"),
        alarm("2026-09-29T06:30:00-04:00", "w", "Work", weekdays),
        alarm("2026-09-30T06:30:00-04:00", "w", "Work", weekdays),
        alarm("2026-09-28T22:09:00-04:00", "s", kind="snooze"),
        alarm("2026-10-03T08:00:00-04:00", "sat"),
    ]
    assert speech.alarms_line(alarms, NOW) == (
        "You have 4 alarms: a snoozed alarm at 10:09 PM today, Work at 6:30 AM on weekdays, "
        "8 AM on Saturday, and 9:15 AM on October 10.")
    assert speech.alarms_line(alarms, NOW, next_only=True) == "Your next alarm is a snoozed alarm at 10:09 PM today."
    assert speech.alarms_line([], NOW) == "You don't have any alarms."
