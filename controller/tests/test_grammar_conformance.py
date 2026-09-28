"""§16.6 grammar conformance: every line of echomuse_grammar/conformance.tsv."""

from __future__ import annotations

from collections import Counter
from pathlib import Path

import pytest

from echomuse_grammar import (
    AMPM_CHOICES,
    AlarmParse,
    AlarmQuery,
    Choice,
    CommandContext,
    TEMPLATE_VERSION,
    TimerCancel,
    classify,
    family_result,
    match_choice,
    match_local_command,
    normalize,
    parse_alarm,
    parse_alarm_query,
    parse_timer_cancel,
)

CORPUS = Path(__file__).resolve().parents[1] / "echomuse_grammar" / "conformance.tsv"

# Vocabulary snapshot and reply choices the hand-written corpus was written against.
VOCABULARY = (
    "kitchen", "Kitchen Lights", "desk lamp", "office", "office fan", "bedroom",
    "living room", "kitchen counter",
)
CHOICES = (
    Choice("yes", ("yes", "yeah", "yep", "sure")),
    Choice("no", ("no", "nope")),
    Choice("first", ("first", "one", "the first one")),
    Choice("second", ("second", "two", "the second one")),
    Choice("tea", ("tea", "earl grey")),
) + AMPM_CHOICES


def _cases() -> list[tuple[str, str, str]]:
    rows = []
    for line in CORPUS.read_text(encoding="utf-8").splitlines():
        if line.startswith("#"):
            continue
        text, family, klass = line.split("\t")
        rows.append((text, family, klass))
    return rows


CASES = _cases()


def test_corpus_shape() -> None:
    header = CORPUS.read_text(encoding="utf-8").splitlines()[:2]
    assert f"home-assistant-intents {TEMPLATE_VERSION}" in header[1]
    counts = Counter(family for _, family, _ in CASES)
    assert set(counts) == {"timer", "alarm", "alarm_query", "local", "home", "reply"}
    assert all(counts[family] >= 40 for family in counts)


def test_every_corpus_line_reproduces() -> None:
    failures = [
        (text, family, expected, got)
        for text, family, expected in CASES
        if (got := family_result(family, text, VOCABULARY, CHOICES).klass) != expected
    ]
    assert failures == []


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("Set an alarm for 7:30 PM.", "set an alarm for 7 30 pm"),
        ("  Wake me   up at SEVEN oh FIVE a.m.! ", "wake me up at 7 oh 5 am"),
        ("twenty-five minutes", "25 minutes"),
        ("ninety nine", "99"),
        ("Don't stop.", "don't stop"),
        ("a timer for 1/2 an hour", "a timer for 1/2 an hour"),
        ("When\u2019s my alarm?", "when's my alarm"),
    ],
)
def test_normalize(text: str, expected: str) -> None:
    assert normalize(text) == expected


@pytest.mark.parametrize(
    ("text", "parse"),
    [
        ("set an alarm for 7 am", AlarmParse("set", 7, 0, None, None, 7, "am")),
        ("set an alarm for 12 am", AlarmParse("set", 0, 0, None, None, 12, "am")),
        ("set an alarm for 12 pm", AlarmParse("set", 12, 0, None, None, 12, "pm")),
        ("wake me at 6 45 in the evening tomorrow",
         AlarmParse("set", 18, 45, "tomorrow", None, 6, "pm")),
        ("set an alarm for seven oh five a m every day",
         AlarmParse("set", 7, 5, frozenset({"mon", "tue", "wed", "thu", "fri", "sat", "sun"}), None, 7, "am")),
        ("make alarm at midnight on weekends", AlarmParse("set", 0, 0, frozenset({"sat", "sun"}), None, 12, "am")),
        ("set alarm for 6 am on monday, wednesday and friday",
         AlarmParse("set", 6, 0, frozenset({"mon", "wed", "fri"}), None, 6, "am")),
        ("set an alarm for seven", AlarmParse("set", None, 0, None, "ampm", 7, None)),
        ("cancel my 7 am alarm", AlarmParse("cancel", 7, 0, None, None, 7, "am")),
        ("turn off the alarm", AlarmParse("cancel", None, None, None)),
        ("delete all my alarms", AlarmParse("cancel_all", None, None, None)),
        ("set an alarm for 13 pm", None),
        ("set an alarm for 7 am monday monday", None),
        ("don't set an alarm for 7 am", None),
    ],
)
def test_parse_alarm(text: str, parse: AlarmParse | None) -> None:
    assert parse_alarm(text) == parse


@pytest.mark.parametrize(
    ("text", "parse"),
    [
        ("Cancel the timer.", TimerCancel()),
        ("stop my timer", TimerCancel()),
        ("turn off the timer", TimerCancel()),
        ("cancel the five-minute timer", TimerCancel((0, 5, 0))),
        ("cancel the hour and a half timer", TimerCancel((1, 30, 0))),
        ("cancel the 10 minute pasta timer", TimerCancel((0, 10, 0), "pasta")),
        ("cancel the timer called chicken wings", TimerCancel(None, "chicken wings")),
        ("cancel the timer for 10 minutes", TimerCancel((0, 10, 0))),
        ("cancel all of my timers", TimerCancel(all=True)),
        ("cancel both timers", TimerCancel(all=True)),
        ("cancel every timer", TimerCancel(all=True)),
        ("can you cancel the timers please", TimerCancel(all=True)),
        ("cancel", None),
        ("cancel the alarm", None),
        ("cancel the 5 minute", None),
        ("cancel the timer and turn on the lights", None),
        ("set a timer for 5 minutes", None),
    ],
)
def test_parse_timer_cancel(text: str, parse: TimerCancel | None) -> None:
    assert parse_timer_cancel(text) == parse


@pytest.mark.parametrize(
    ("text", "parse"),
    [
        ("what alarms are set", AlarmQuery()),
        ("when's my next alarm", AlarmQuery(next_only=True)),
        ("what time is my alarm set for", AlarmQuery()),
        ("do I have any alarms", AlarmQuery()),
        # Timer questions are HA's local HassTimerStatus intent.
        ("How much time is left?", None),
        ("what timers and alarms do I have", None),
        ("what time is it", None),
    ],
)
def test_parse_alarm_query(text: str, parse: AlarmQuery | None) -> None:
    assert parse_alarm_query(text) == parse


PASTA_TIMER = CommandContext("alert", "timer", "pasta")
WAKE_ALARM = CommandContext("alert", "alarm", "wake up")
DIALOG = CommandContext("dialog")


@pytest.mark.parametrize(
    ("text", "context", "command"),
    [
        ("stop", PASTA_TIMER, "dismiss"),
        ("please cancel", WAKE_ALARM, "dismiss"),
        ("stop", DIALOG, "stop"),
        ("cancel please", DIALOG, "stop"),
        ("stop", None, None),
        ("snooze", WAKE_ALARM, "snooze"),
        ("snooze", PASTA_TIMER, None),
        ("snooze", DIALOG, None),
        ("stop the timer", PASTA_TIMER, "dismiss"),
        ("stop the pasta timer", PASTA_TIMER, "dismiss"),
        ("stop the tea timer", PASTA_TIMER, None),
        ("stop the alarm", PASTA_TIMER, None),
        ("turn off the wake up alarm please", WAKE_ALARM, "dismiss"),
        ("dismiss the alarm", WAKE_ALARM, "dismiss"),
        ("stop the timer", DIALOG, None),
        ("don't stop", PASTA_TIMER, None),
        ("stop the music", PASTA_TIMER, None),
        ("snooze for", WAKE_ALARM, None),
    ],
)
def test_match_local_command(text: str, context: CommandContext | None, command: str | None) -> None:
    assert match_local_command(text, context) == command


def test_match_choice() -> None:
    assert match_choice("In the evening.", AMPM_CHOICES) == "pm"
    assert match_choice("a m", AMPM_CHOICES) == "am"
    assert match_choice("night", AMPM_CHOICES) is None
    ambiguous = (Choice("x", ("one",)), Choice("y", ("one",)))
    assert match_choice("one", ambiguous) is None


@pytest.mark.parametrize(
    ("text", "klass", "family"),
    [
        ("set a pasta timer for five minutes", "complete", "timer"),
        ("set a timer for", "needs_more", "timer"),
        ("timer for five", "needs_more", "timer"),
        ("set an alarm for seven", "needs_more", "alarm"),
        ("turn off kitchen", "extendable", "home"),
        ("turn off the", "needs_more", "home"),
        ("stop", "complete", "local"),
        ("yes", "complete", "reply"),
        ("what is the weather", "unknown", None),
        ("", "unknown", None),
        ("switch off the kitchen", "extendable", "home"),
        ("kitchen lights on", "complete", "home"),
        ("stop the timer", "complete", "timer"),
        ("what alarms do i have", "complete", "alarm_query"),
        ("how much time is left", "complete", "timer"),
    ],
)
def test_classify_across_families(text: str, klass: str, family: str | None) -> None:
    result = classify(text, VOCABULARY, CHOICES)
    assert (result.klass, result.family if result.klass != "unknown" else None) == (klass, family)


def test_reply_needs_supplied_choices() -> None:
    assert classify("yes", VOCABULARY).klass == "unknown"
