"""
LED ring scenes — palettes for the controller-rendered ring states.

A scene decides what "listening" (solid ring) and "thinking" (spinner)
look like. The controller renders every animation frame and the device
just paints it, so scenes are entirely controller-side; device-local
displays keep fixed colours on purpose — the mute ring stays red in every
scene (it's a privacy indicator, not decoration) and the volume arc stays
cyan.

Config keys (global or per-device, pushed live like any other config):
  ledScene        — "standard" | "airy" | "malevolent" | "pride" | "custom"
  ledListenColor  — "#RRGGBB", custom scene only
  ledThinkColor   — "#RRGGBB", custom scene only

resolve(config) returns what em_device's LED projection sends:
  listening   — ready-to-send list of 12 {id,r,g,b} dicts (static frames)
  *_anim      — led_anim specs the device animates on its own ticker
"""

from __future__ import annotations

import colorsys
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from enum import StrEnum
from typing import Any, Literal, NotRequired, TypedDict

NUM_LEDS = 12

Rgb = tuple[int, int, int]

# resolve()'s result: layer name → wire object (`leds` frames or `led_anim.anim`).
# em_device picks layers by cue name, so the keys stay a JSON-shaped mapping.
SceneSpec = dict[str, Any]


class SceneName(StrEnum):
    """`ledScene` config values."""
    STANDARD = "standard"
    AIRY = "airy"
    MALEVOLENT = "malevolent"
    PRIDE = "pride"
    CUSTOM = "custom"


class AnimPattern(StrEnum):
    """`led_anim.anim.pattern` values the device animates."""
    SOLID = "solid"
    SPIN = "spin"
    ROTATE = "rotate"
    METER = "meter"
    PULSE = "pulse"


class LedPixel(TypedDict):
    id: int
    r: int
    g: int
    b: int


class LedAnim(TypedDict):
    """One `led_anim.anim` wire object; meter curve fields only when configured."""
    pattern: AnimPattern
    colors: list[list[int]]
    ttlSec: int
    periodMs: NotRequired[int]
    attack: NotRequired[float]
    decay: NotRequired[float]
    floor: NotRequired[float]
    gamma: NotRequired[float]
    ref: NotRequired[float]
    curve: NotRequired[float]


def _solid(r: int, g: int, b: int) -> list[Rgb]:
    return [(r, g, b)] * NUM_LEDS


def _hsv(h: float, s: float, v: float) -> Rgb:
    """h in degrees; returns 8-bit RGB."""
    r, g, b = colorsys.hsv_to_rgb((h % 360) / 360.0, s, v)
    return (int(r * 255), int(g * 255), int(b * 255))


# One hue per LED around the wheel — the pride ring. Value capped below
# full so white-ish hues don't visually swamp the saturated ones.
_RAINBOW = [_hsv(i * 360 / NUM_LEDS, 1.0, 0.75) for i in range(NUM_LEDS)]


@dataclass(frozen=True, slots=True)
class _Preset:
    """Listening palette (12 triples) and spinner head/trail colours; `rotate`
    spins the listening palette itself instead of a head+trail dot (pride's
    rotating rainbow), which then has no head/trail."""
    listening: list[Rgb]
    spin_head: Rgb | None
    spin_trail: Rgb | None
    rotate: bool


_PRESETS = {
    SceneName.STANDARD: _Preset(_solid(0, 180, 0), (0, 200, 0), (0, 60, 0), False),
    # Pale sky blue — calm, low-saturation.
    SceneName.AIRY: _Preset(_solid(80, 150, 200), (150, 205, 255), (25, 45, 70), False),
    # Deep crimson-magenta with an ember spinner. Deliberately NOT pure
    # red (180,0,0) — that's the mute ring and must stay unambiguous.
    SceneName.MALEVOLENT: _Preset(_solid(110, 0, 45), (210, 45, 0), (55, 8, 0), False),
    SceneName.PRIDE: _Preset(list(_RAINBOW), None, None, True),
}


def _hex_to_rgb(value: object, default: Rgb) -> Rgb:
    try:
        s = str(value).lstrip("#")
        if len(s) != 6:
            return default
        return (int(s[0:2], 16), int(s[2:4], 16), int(s[4:6], 16))
    except (ValueError, TypeError):
        return default


def _leds(palette: Sequence[Rgb]) -> list[LedPixel]:
    return [{"id": i, "r": r, "g": g, "b": b} for i, (r, g, b) in enumerate(palette)]


def _colors(palette: Sequence[Rgb | None]) -> list[list[int]]:
    return [list(c) for c in palette if c is not None]


# Dead-man TTLs (§11.2): remote dialog indicators clear on controller loss.
# em_device re-sends any layer with a TTL <= 30 s every 10 s while it holds,
# so these bound how long a ring outlives a dead controller.
METER_TTL = 20
SPIN_TTL = 20
LISTENING_TTL = 30
SPIN_PERIOD_MS = 80
NOSPEECH_PERIOD_MS = 900    # one slow throb
ERROR_PERIOD_MS = 220       # fast agitated blinks
OUTCOME_TTL = 1

MeterField = Literal["attack", "decay", "floor", "gamma", "ref", "curve"]

# Meter response-curve config keys → the wire field the device reads, with
# the range the dashboard offers. The device clamps independently
# (resolveMeter in animator.go) — this is the UI range, not the guard.
_METER_KEYS: tuple[tuple[str, MeterField, float, float], ...] = (
    ("meterAttack", "attack", 0.05, 1.0),
    ("meterDecay",  "decay",  0.02, 1.0),
    ("meterFloor",  "floor",  0.0,  0.6),
    ("meterGamma",  "gamma",  1.0,  3.5),
    ("meterRef",    "ref",    0.02, 1.0),
    ("meterCurve",  "curve",  0.3,  2.0),
)


def _add_meter_curve(anim: LedAnim, config: Mapping[str, object]) -> None:
    """
    Copy the meter response-curve overrides of a device config into `anim`.

    Only keys actually present are emitted: an absent field means "use the
    firmware default", which keeps the wire spec small and lets the device
    move its defaults without every stored config pinning the old ones.
    """
    for cfg_key, wire_key, lo, hi in _METER_KEYS:
        value = config.get(cfg_key)
        if not isinstance(value, (int, float, str)):
            continue
        try:
            anim[wire_key] = min(hi, max(lo, float(value)))
        except ValueError:
            continue


def resolve(config: Mapping[str, object] | None) -> SceneSpec:
    """
    Turn a device config dict into a render-ready scene. Unknown scene
    names fall back to standard, so a stale config never breaks LEDs.
    """
    config = config or {}
    name = config.get("ledScene", SceneName.STANDARD)
    if name == SceneName.CUSTOM:
        listen = _hex_to_rgb(config.get("ledListenColor"), (0, 180, 0))
        think  = _hex_to_rgb(config.get("ledThinkColor"), (0, 200, 0))
        preset = _Preset(_solid(*listen), think, (think[0] // 3, think[1] // 3, think[2] // 3), False)
    else:
        preset = (_PRESETS[SceneName(name)] if isinstance(name, str) and name in _PRESETS
                  else _PRESETS[SceneName.STANDARD])

    # led_anim specs for the device's own ticker (capability "led_anim").
    # ttlSec is a dead-man switch: if the controller dies mid-turn the ring
    # self-clears instead of spinning forever.
    spin_anim: LedAnim = {
        "pattern":  AnimPattern.ROTATE if preset.rotate else AnimPattern.SPIN,
        "colors":   _colors(preset.listening if preset.rotate else [preset.spin_head, preset.spin_trail]),
        "periodMs": SPIN_PERIOD_MS,
        "ttlSec":   SPIN_TTL,
    }
    listening_anim: LedAnim = {
        "pattern": AnimPattern.SOLID,
        "colors":  _colors(preset.listening),
        "ttlSec":  LISTENING_TTL,
    }
    # Playback: the ring throbs with the live speaker level (device-side
    # RMS at the write point). Solid scenes throb the spinner head colour;
    # pride throbs the whole rainbow.
    meter_anim: LedAnim = {
        "pattern": AnimPattern.METER,
        "colors":  _colors(preset.listening if preset.rotate else [preset.spin_head]),
        "ttlSec":  METER_TTL,
    }
    _add_meter_curve(meter_anim, config)

    # A brief self-clearing cue played at turn end so outcomes are
    # distinguishable. Rhythm carries the meaning, not colour: adding a new
    # colour would collide with red (mute) / orange (link) / cyan (volume).
    outcome_colors = _colors([preset.spin_head] if not preset.rotate else preset.listening)
    nospeech_anim: LedAnim = {"pattern": AnimPattern.PULSE, "colors": outcome_colors,
                              "periodMs": NOSPEECH_PERIOD_MS, "ttlSec": OUTCOME_TTL}
    error_anim: LedAnim = {"pattern": AnimPattern.PULSE, "colors": outcome_colors,
                           "periodMs": ERROR_PERIOD_MS, "ttlSec": OUTCOME_TTL}

    return {
        "name":           name,
        "listening":      _leds(preset.listening),
        "listening_anim": listening_anim,
        "spin_anim":      spin_anim,
        "meter_anim":     meter_anim,
        # One slow throb — "I was listening and heard nothing."
        "nospeech_anim":  nospeech_anim,
        # Fast agitated blinks — "something went wrong."
        "error_anim":     error_anim,
    }
