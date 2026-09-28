#!/usr/bin/env python3
"""Compile HA 2026.7.30 English timer sentences and conformance cases."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from echomuse_grammar.normalize import normalize  # noqa: E402

VERSION = "2026.7.30"
WHEEL_SHA256 = "9e9f6cb787b47db87de7266656932d270135b7be2ef4c491e02086c874ed08a0"
INTENTS = (
    "HassStartTimer",
    "HassCancelTimer",
    "HassCancelAllTimers",
    "HassIncreaseTimer",
    "HassDecreaseTimer",
    "HassPauseTimer",
    "HassUnpauseTimer",
    "HassTimerStatus",
)
DURATIONS = (1, 2, 5, 10, 15, 30, 45, 90)
NAMES = ("pasta", "kitchen", "tea")


@dataclass(frozen=True)
class Node:
    kind: str
    value: object


def _source(wheel: Path | None) -> tuple[dict, str]:
    if wheel is None:
        with tempfile.TemporaryDirectory() as directory:
            subprocess.run(
                [sys.executable, "-m", "pip", "download", f"home-assistant-intents=={VERSION}",
                 "--no-deps", "-d", directory], check=True, stdout=subprocess.DEVNULL,
            )
            wheel = next(Path(directory).glob("home_assistant_intents-*.whl"))
            return _read_wheel(wheel)
    return _read_wheel(wheel)


def _read_wheel(wheel: Path) -> tuple[dict, str]:
    raw = wheel.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if digest != WHEEL_SHA256:
        raise SystemExit(f"unexpected source SHA-256: {digest}")
    with zipfile.ZipFile(wheel) as archive:
        data = json.loads(archive.read("home_assistant_intents/data/en.json"))
    return data, digest


class Parser:
    def __init__(self, text: str, rules: dict[str, str]) -> None:
        self.text = re.sub(r"([A-Za-z]+)\[s\]", r"(\1|\1s)", text).replace("( |-)", " ")
        self.rules = rules
        self.pos = 0

    def parse(self, end: str = "") -> Node:
        alternatives: list[Node] = []
        sequence: list[Node] = []
        literal: list[str] = []

        def flush() -> None:
            if literal:
                words = normalize("".join(literal)).split()
                sequence.extend(Node("word", word) for word in words)
                literal.clear()

        while self.pos < len(self.text):
            char = self.text[self.pos]
            if char in end:
                break
            if char == "|":
                flush()
                alternatives.append(_sequence(sequence))
                sequence = []
                self.pos += 1
            elif char in "([":
                flush()
                close = ")" if char == "(" else "]"
                self.pos += 1
                child = self.parse(close)
                if self.pos >= len(self.text) or self.text[self.pos] != close:
                    raise ValueError(f"unclosed {char!r} in {self.text!r}")
                self.pos += 1
                sequence.append(Node("optional", child) if char == "[" else child)
            elif char == "<":
                flush()
                stop = self.text.index(">", self.pos)
                name = self.text[self.pos + 1 : stop]
                self.pos = stop + 1
                sequence.append(Parser(self.rules[name], self.rules).parse())
            elif char == "{":
                flush()
                stop = self.text.index("}", self.pos)
                name = self.text[self.pos + 1 : stop].split(":", 1)[0]
                sequence.append(Node("slot", name))
                self.pos = stop + 1
            else:
                literal.append(char)
                self.pos += 1
        flush()
        alternatives.append(_sequence(sequence))
        return alternatives[0] if len(alternatives) == 1 else Node("alt", tuple(alternatives))


def _sequence(items: list[Node]) -> Node:
    if not items:
        return Node("seq", ())
    return items[0] if len(items) == 1 else Node("seq", tuple(items))


def _expand(node: Node) -> list[tuple[str, ...]]:
    if node.kind == "word":
        return [(str(node.value),)]
    if node.kind == "slot":
        return [("{" + str(node.value) + "}",)]
    if node.kind == "optional":
        return [()] + _expand(node.value)  # type: ignore[arg-type]
    if node.kind == "alt":
        return [item for child in node.value for item in _expand(child)]  # type: ignore[union-attr]
    result: list[tuple[str, ...]] = [()]
    for child in node.value:  # type: ignore[union-attr]
        result = [left + right for left in result for right in _expand(child)]
    return result


def _plain(node: Node) -> tuple:
    if node.kind in {"word", "slot"}:
        return (node.kind, node.value)
    if node.kind == "optional":
        return ("optional", _plain(node.value))
    return (node.kind, tuple(_plain(child) for child in node.value))


# §16.6 table forms that the HA 2026.7.30 English templates lack; compiled
# with the same expansion rules so their completeness matches HA's own forms.
SPEC_SENTENCES = (
    ("HassStartTimer", "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_hours:hours}( |-)hour[s]"),
    ("HassStartTimer", "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_minutes:minutes}( |-)minute[s]"),
    ("HassStartTimer", "<timer_set> [a|an|the|my] {timer_name:name} timer for {timer_seconds:seconds}( |-)second[s]"),
    ("HassTimerStatus", "how much time [is] left"),
)


def _patterns(data: dict) -> list[tuple[str, Node]]:
    rules = data["expansion_rules"]
    sentences = [
        (intent, sentence)
        for intent in INTENTS
        for block in data["intents"][intent]["data"]
        for sentence in block["sentences"]
    ]
    sentences.extend(SPEC_SENTENCES)
    return [(intent, Parser(sentence, rules).parse()) for intent, sentence in sentences]


def _fill(sequence: tuple[str, ...], serial: int) -> tuple[str, ...]:
    duration = DURATIONS[serial % len(DURATIONS)]
    name = NAMES[serial % len(NAMES)]
    result: list[str] = []
    for token in sequence:
        if token.startswith("{timer_") and token not in {"{timer_name}", "{timer_command}"}:
            if token == "{timer_half}":
                result.append("half")
            else:
                result.append(str(duration))
        elif token == "{timer_name}":
            result.append(name)
        elif token == "{timer_command}":
            result.extend(("turn", "on", "lights"))
        elif token == "{area}":
            result.append("kitchen")
        else:
            result.append(token)
    return tuple(result)


_WILDCARD_SLOTS = frozenset({"{timer_name}", "{timer_command}", "{area}"})


def _shape(sequence: tuple[str, ...]) -> tuple[str, ...]:
    """Sequence as the matcher sees it: numbers and word slots lose their slot identity."""

    return tuple(
        "{n}" if token.startswith("{timer_") and token not in _WILDCARD_SLOTS and token != "{timer_half}"
        else "{w}" if token in _WILDCARD_SLOTS else token
        for token in sequence
    )


def _timer_cases(patterns: Iterable[tuple[str, Node]]) -> dict[str, str]:
    """Every expansion is complete; each proper prefix ending before a required
    slot or inside a duration (number without its unit) is needs_more, unless
    that prefix is itself the shape of a complete expansion."""

    sequences: list[tuple[str, ...]] = []
    seen: set[tuple[str, ...]] = set()
    for _, node in patterns:
        for sequence in _expand(node):
            if sequence not in seen:
                seen.add(sequence)
                sequences.append(sequence)
    complete_shapes = {_shape(sequence) for sequence in sequences}
    cases: dict[str, str] = {}
    for serial, sequence in enumerate(sequences):
        cases[" ".join(_fill(sequence, serial))] = "complete"
        for index, symbol in enumerate(sequence):
            if not symbol.startswith("{"):
                continue
            prefixes = [sequence[:index]]
            if symbol not in _WILDCARD_SLOTS:
                prefixes.append(sequence[: index + 1])
            for prefix in prefixes:
                if prefix and _shape(prefix) not in complete_shapes:
                    cases.setdefault(" ".join(_fill(prefix, serial)), "needs_more")
    return cases


HAND_CASES = {
    "alarm": [
        ("set an alarm for 7 am", "complete"), ("set an alarm for seven", "needs_more"),
        ("set an alarm for 7 a m tomorrow", "complete"), ("set a alarm at 12 pm", "complete"),
        ("create alarm for noon", "complete"), ("make an alarm at midnight", "complete"),
        ("wake me at 6 am", "complete"), ("wake me up for 6 30 pm", "complete"),
        ("wake me up at", "needs_more"), ("set alarm for", "needs_more"),
        ("set alarm for 7 oh 5 am", "complete"), ("set alarm for seven five am", "complete"),
        ("set alarm for 1 in the morning", "complete"), ("set alarm for 1 in the afternoon", "complete"),
        ("set alarm for 1 in the evening", "complete"), ("set alarm for 1 at night", "complete"),
        ("set alarm for 12 at night", "complete"), ("set alarm for 12 in the morning", "complete"),
        ("wake me", "needs_more"), ("cancel my", "unknown"),
        ("set alarm for 7 in the", "needs_more"), ("set an alarm for 7 am on every monday", "unknown"),
        ("Set an alarm for 7:30 p.m. tomorrow.", "complete"), ("wake me up at 6 30", "needs_more"),
        ("set alarm for 7 am today", "complete"), ("set alarm for 7 am tomorrow", "complete"),
        ("set alarm for 7 am every day", "complete"), ("set alarm for 7 am daily", "complete"),
        ("set alarm for 7 am weekdays", "complete"), ("set alarm for 7 am on weekends", "complete"),
        ("set alarm for 7 am monday", "complete"), ("set alarm for 7 am on monday and friday", "complete"),
        ("set alarm for 7 am every monday wednesday friday", "complete"),
        ("set alarm for 7 am on", "needs_more"), ("set alarm for 7 am every", "needs_more"),
        ("set alarm for 7 am monday and", "needs_more"), ("set alarm for 0 am", "unknown"),
        ("set alarm for 13 pm", "unknown"), ("set alarm for 7 60 am", "unknown"),
        ("cancel my 7 am alarm", "complete"), ("cancel the alarm", "complete"),
        ("delete my midnight alarm", "complete"), ("remove noon alarm", "complete"),
        ("turn off the 8 pm alarm", "complete"), ("cancel all alarms", "complete"),
        ("delete all my alarms", "complete"), ("remove all the alarms", "complete"),
        ("cancel all", "needs_more"), ("cancel my 7 alarm", "needs_more"),
        ("don't set an alarm for 7 am", "unknown"), ("set alarm for 7 am please", "unknown"),
    ],
    "local": [
        ("stop", "complete"), ("cancel", "complete"), ("snooze", "complete"),
        ("please stop", "complete"), ("stop please", "complete"), ("please cancel", "complete"),
        ("cancel please", "complete"), ("please snooze", "complete"), ("snooze please", "complete"),
        ("stop the timer", "complete"), ("cancel timer", "complete"),
        ("turn off the timer", "complete"), ("dismiss timer", "complete"),
        ("stop the alarm", "complete"), ("cancel alarm", "complete"),
        ("turn off the alarm", "complete"), ("dismiss the alarm", "complete"),
        ("stop the pasta timer", "complete"), ("cancel the tea timer", "complete"),
        ("dismiss kitchen alarm", "complete"), ("turn off the wake up alarm", "complete"),
        ("stop the timer please", "complete"), ("please stop the timer", "unknown"),
        ("please stop please", "unknown"), ("stop please please", "unknown"),
        ("dismiss the alarm please", "complete"), ("snooze for", "needs_more"),
        ("please snooze for", "needs_more"), ("snooze for 5", "needs_more"),
        ("snooze for five minutes", "unknown"), ("don't stop", "unknown"),
        ("do not stop", "unknown"), ("stop the music", "unknown"),
        ("stop talking", "unknown"), ("cancel the kitchen timer now", "unknown"),
        ("unstoppable", "unknown"), ("stopwatch", "unknown"),
        ("please don't cancel", "unknown"), ("snooze timer", "unknown"),
        ("turn the alarm off", "unknown"), ("stop and cancel", "unknown"),
        ("Stop.", "complete"), ("  Cancel,  please! ", "complete"),
        ("stop the", "needs_more"), ("turn off the", "needs_more"),
        ("dismiss the", "needs_more"), ("cancel the", "needs_more"),
    ],
    "home": [
        ("turn on kitchen lights", "complete"), ("turn off kitchen lights", "complete"),
        ("turn on the kitchen lights", "complete"), ("turn off the kitchen lights", "complete"),
        ("switch on kitchen lights", "complete"), ("switch off kitchen lights", "complete"),
        ("kitchen lights on", "complete"), ("kitchen lights off", "complete"),
        ("turn on desk lamp", "complete"), ("turn off office fan", "complete"),
        ("switch on the bedroom", "complete"), ("bedroom off", "complete"),
        ("turn on kitchen", "extendable"), ("turn off kitchen", "extendable"),
        ("switch on kitchen", "extendable"), ("kitchen on", "extendable"),
        ("kitchen off", "extendable"), ("turn on office", "extendable"),
        ("office on", "extendable"), ("turn off the", "needs_more"),
        ("turn on the", "needs_more"), ("switch off the", "needs_more"),
        ("turn off", "needs_more"), ("switch on", "needs_more"),
        ("turn off living", "needs_more"), ("living room", "needs_more"),
        ("don't turn off kitchen lights", "unknown"), ("turn kitchen lights on", "unknown"),
        ("switch kitchen lights off", "unknown"), ("turn up kitchen lights", "unknown"),
        ("turn off unknown light", "unknown"), ("turn off kitchen lights please", "unknown"),
        ("kitchen lights", "needs_more"), ("desk lamp", "needs_more"),
        ("the kitchen lights on", "unknown"), ("turn on on kitchen lights", "unknown"),
        ("turn", "unknown"), ("switch", "unknown"), ("on", "unknown"),
        ("off", "unknown"), ("kitchen lights and", "unknown"),
        ("turn off kitchen counter", "complete"), ("kitchen counter off", "complete"), ("", "unknown"), ("please", "unknown"),
    ],
    "reply": [
        ("yes", "complete"), ("yeah", "complete"), ("yep", "complete"),
        ("sure", "complete"), ("no", "complete"), ("nope", "complete"),
        ("first", "complete"), ("one", "complete"), ("the first one", "complete"),
        ("second", "complete"), ("two", "complete"), ("the second one", "complete"),
        ("am", "complete"), ("a m", "complete"), ("morning", "complete"),
        ("in the morning", "complete"), ("pm", "complete"), ("p m", "complete"),
        ("afternoon", "complete"), ("evening", "complete"),
        ("in the evening", "complete"), ("at night", "complete"),
        ("tea", "complete"), ("earl grey", "complete"),
        ("yes please", "unknown"), ("please yes", "unknown"),
        ("not yes", "unknown"), ("maybe", "unknown"), ("okay", "unknown"),
        ("third", "unknown"), ("three", "unknown"), ("the third one", "unknown"),
        ("night", "unknown"), ("in the", "needs_more"), ("at", "needs_more"),
        ("earl", "needs_more"), ("the first", "needs_more"),
        ("the second", "needs_more"), ("the", "needs_more"), ("pm please", "unknown"),
        ("", "unknown"), ("tea or coffee", "unknown"), ("no thank you", "unknown"),
    ],
    "alarm_query": [
        ("what alarms do i have", "complete"), ("what alarms are set", "complete"),
        ("which alarms are set", "complete"), ("what alarms have i got", "complete"),
        ("do i have any alarms", "complete"), ("do i have any alarms set", "complete"),
        ("have i got any alarms", "complete"), ("are there any alarms set", "complete"),
        ("how many alarms do i have", "complete"), ("how many alarms are set", "complete"),
        ("list my alarms", "complete"), ("show me my alarms", "complete"), ("tell me my alarms", "complete"),
        ("check my alarms", "complete"), ("read me all my alarms", "complete"), ("what are my alarms", "complete"),
        ("what's my alarm set for", "needs_more"), ("alarm status", "complete"), ("status of my alarms", "complete"),
        ("when is my alarm", "complete"), ("when's my next alarm", "complete"), ("whens my alarm", "complete"),
        ("what time is my alarm", "complete"), ("what time is my next alarm set", "complete"),
        ("is my alarm set", "complete"), ("is there an alarm set", "complete"),
        ("what time does my alarm go off", "complete"), ("when will my next alarm go off", "complete"),
        ("do i have an alarm set", "complete"), ("what alarms do i have please", "complete"),
        ("what time is my alarm set for", "needs_more"), ("can you tell me my alarms", "complete"),
        ("how much time is left", "unknown"), ("what timers do i have", "unknown"),
        ("what timers and alarms do i have", "unknown"), ("set an alarm for 7 am", "unknown"),
        ("cancel my alarm", "unknown"), ("what time is it", "unknown"), ("when is my appointment", "unknown"),
        ("", "unknown"),
    ],
}


def _write_data(patterns: list[tuple[str, Node]], data: dict, digest: str) -> None:
    ranges = {
        name: (data["lists"][name]["range"]["from"], data["lists"][name]["range"]["to"])
        for name in ("timer_hours", "timer_minutes", "timer_seconds")
    }
    lines = [
        '"""Generated by tools/gen_timer_grammar.py from home-assistant-intents; do not edit.',
        "",
        "Each pattern is (intent, node); a node is ('word', w), ('slot', name),",
        "('optional', node), ('alt', nodes) or ('seq', nodes), with words normalized.",
        '"""',
        "",
        f"TEMPLATE_VERSION = {VERSION!r}",
        f"SOURCE_SHA256 = {digest!r}",
        "SOURCE_MEMBER = 'home_assistant_intents/data/en.json'",
        "",
        f"SKIP_WORDS = {tuple(data['skip_words'])!r}",
        f"SLOT_RANGES = {ranges!r}",
        "",
        "TIMER_PATTERNS = (",
    ]
    for intent, node in patterns:
        lines.append(f"    ({intent!r}, {_plain(node)!r}),")
    lines.extend((")", ""))
    (ROOT / "echomuse_grammar" / "timer_data.py").write_text("\n".join(lines), encoding="utf-8")


# §16.6 table timer examples, verbatim (the generator's fills never produce these spellings).
TIMER_TABLE = (
    ("set a timer for five minutes", "complete"),
    ("set a pasta timer for five minutes", "complete"),
    ("timer for 1 hour and 5 minutes", "complete"),
    ("add 2 minutes to the pasta timer", "complete"),
    ("pause the timer", "complete"),
    ("cancel the pasta timer", "complete"),
    ("how much time is left", "complete"),
    ("set a timer for", "needs_more"),
    ("timer for five", "needs_more"),
    ("cancel the", "needs_more"),
)


def _write_corpus(timer_cases: dict[str, str]) -> None:
    lines = ["# utterance\tfamily\texpected class", f"# timer source home-assistant-intents {VERSION} sha256 {WHEEL_SHA256}"]
    lines.extend(f"{text}\ttimer\t{klass}" for text, klass in sorted(timer_cases.items()))
    lines.append("# SPEC §16.6 table timer examples")
    lines.extend(f"{text}\ttimer\t{klass}" for text, klass in TIMER_TABLE)
    for family, cases in HAND_CASES.items():
        lines.append(f"# hand-written {family}")
        lines.extend(f"{text}\t{family}\t{klass}" for text, klass in cases)
    (ROOT / "echomuse_grammar" / "conformance.tsv").write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wheel", type=Path, help="local home_assistant_intents wheel (else pip download)")
    args = parser.parse_args()
    data, digest = _source(args.wheel)
    patterns = _patterns(data)
    _write_data(patterns, data, digest)
    _write_corpus(_timer_cases(patterns))


if __name__ == "__main__":
    main()
