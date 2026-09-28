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

NUM_LEDS = 12


def _solid(r: int, g: int, b: int) -> list:
    return [(r, g, b)] * NUM_LEDS


def _hsv(h: float, s: float, v: float) -> tuple:
    """h in degrees; returns 8-bit RGB."""
    import colorsys
    r, g, b = colorsys.hsv_to_rgb((h % 360) / 360.0, s, v)
    return (int(r * 255), int(g * 255), int(b * 255))


# One hue per LED around the wheel — the pride ring. Value capped below
# full so white-ish hues don't visually swamp the saturated ones.
_RAINBOW = [_hsv(i * 360 / NUM_LEDS, 1.0, 0.75) for i in range(NUM_LEDS)]

# Each preset: listening palette (12 triples), spinner head/trail colours,
# and rotate=True to spin the listening palette itself instead of a
# head+trail dot (pride's rotating rainbow).
_PRESETS = {
    "standard": {
        "listening":  _solid(0, 180, 0),
        "spin_head":  (0, 200, 0),
        "spin_trail": (0, 60, 0),
        "rotate":     False,
    },
    "airy": {
        # Pale sky blue — calm, low-saturation.
        "listening":  _solid(80, 150, 200),
        "spin_head":  (150, 205, 255),
        "spin_trail": (25, 45, 70),
        "rotate":     False,
    },
    "malevolent": {
        # Deep crimson-magenta with an ember spinner. Deliberately NOT pure
        # red (180,0,0) — that's the mute ring and must stay unambiguous.
        "listening":  _solid(110, 0, 45),
        "spin_head":  (210, 45, 0),
        "spin_trail": (55, 8, 0),
        "rotate":     False,
    },
    "pride": {
        "listening":  list(_RAINBOW),
        "spin_head":  None,
        "spin_trail": None,
        "rotate":     True,
    },
}


def _hex_to_rgb(value, default: tuple) -> tuple:
    try:
        s = str(value).lstrip("#")
        if len(s) != 6:
            return default
        return (int(s[0:2], 16), int(s[2:4], 16), int(s[4:6], 16))
    except (ValueError, TypeError):
        return default


def _leds(palette: list) -> list:
    return [{"id": i, "r": r, "g": g, "b": b} for i, (r, g, b) in enumerate(palette)]


# Dead-man TTLs (§11.2): remote dialog indicators clear on controller loss.
# em_device re-sends any layer with a TTL <= 30 s every 10 s while it holds,
# so these bound how long a ring outlives a dead controller.
METER_TTL = 20
SPIN_TTL = 20

# Meter response-curve config keys → the wire field the device reads, with
# the range the dashboard offers. The device clamps independently
# (resolveMeter in animator.go) — this is the UI range, not the guard.
_METER_KEYS = {
    "meterAttack": ("attack", 0.05, 1.0),
    "meterDecay":  ("decay",  0.02, 1.0),
    "meterFloor":  ("floor",  0.0,  0.6),
    "meterGamma":  ("gamma",  1.0,  3.5),
    "meterRef":    ("ref",    0.02, 1.0),
    "meterCurve":  ("curve",  0.3,  2.0),
}


def _meter_curve(config: dict) -> dict:
    """
    Pull the meter response-curve overrides out of a device config.

    Only keys actually present are emitted: an absent field means "use the
    firmware default", which keeps the wire spec small and lets the device
    move its defaults without every stored config pinning the old ones.
    """
    out = {}
    for cfg_key, (wire_key, lo, hi) in _METER_KEYS.items():
        if cfg_key not in config or config[cfg_key] is None:
            continue
        try:
            out[wire_key] = min(hi, max(lo, float(config[cfg_key])))
        except (TypeError, ValueError):
            continue
    return out


def resolve(config: dict) -> dict:
    """
    Turn a device config dict into a render-ready scene. Unknown scene
    names fall back to standard, so a stale config never breaks LEDs.
    """
    name = (config or {}).get("ledScene", "standard")
    if name == "custom":
        listen = _hex_to_rgb(config.get("ledListenColor"), (0, 180, 0))
        think  = _hex_to_rgb(config.get("ledThinkColor"), (0, 200, 0))
        preset = {
            "listening":  _solid(*listen),
            "spin_head":  think,
            "spin_trail": tuple(c // 3 for c in think),
            "rotate":     False,
        }
    else:
        preset = _PRESETS.get(name, _PRESETS["standard"])

    listening_leds = _leds(preset["listening"])

    # led_anim specs for the device's own ticker (capability "led_anim").
    # ttlSec is a dead-man switch: if the controller dies mid-turn the ring
    # self-clears instead of spinning forever.
    if preset["rotate"]:
        spin_anim = {
            "pattern":  "rotate",
            "colors":   [list(c) for c in preset["listening"]],
            "periodMs": 80,
            "ttlSec":   SPIN_TTL,
        }
    else:
        spin_anim = {
            "pattern":  "spin",
            "colors":   [list(preset["spin_head"]), list(preset["spin_trail"])],
            "periodMs": 80,
            "ttlSec":   SPIN_TTL,
        }
    listening_anim = {
        "pattern": "solid",
        "colors":  [list(c) for c in preset["listening"]],
        "ttlSec":  30,
    }
    # Playback: the ring throbs with the live speaker level (device-side
    # RMS at the write point). Solid scenes throb the spinner head colour;
    # pride throbs the whole rainbow.
    meter_palette = (preset["listening"] if preset["rotate"]
                     else [preset["spin_head"]])
    meter_anim = {
        "pattern": "meter",
        "colors":  [list(c) for c in meter_palette],
        "ttlSec":  METER_TTL,
        **_meter_curve(config or {}),
    }

    # A brief self-clearing cue played at turn end so outcomes are
    # distinguishable. Rhythm carries the meaning, not colour: adding a new
    # colour would collide with red (mute) / orange (link) / cyan (volume).
    outcome_colors = ([list(preset["spin_head"])] if not preset["rotate"]
                      else [list(c) for c in preset["listening"]])

    return {
        "name":           name,
        "listening":      listening_leds,
        "listening_anim": listening_anim,
        "spin_anim":      spin_anim,
        "meter_anim":     meter_anim,
        # One slow throb — "I was listening and heard nothing."
        "nospeech_anim":  {
            "pattern": "pulse", "colors": outcome_colors,
            "periodMs": 900, "ttlSec": 1,
        },
        # Fast agitated blinks — "something went wrong."
        "error_anim":     {
            "pattern": "pulse", "colors": outcome_colors,
            "periodMs": 220, "ttlSec": 1,
        },
    }

