"""
Config sections — the unit of fleet-vs-device scoping.

A device stores the SET of sections it overrides; its effective config is the
fleet config with the device's own values layered over it for those sections
only. Everything else follows the fleet.

This module is the single source of truth for which key belongs to which
section (SPEC §18.4). `controller/static/dashboard.jsx` mirrors SECTIONS for
rendering; tests/test_config_sections.py fails if the two drift or if any
config key belongs to no section.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import StrEnum
from typing import Any, Iterable, Mapping
from urllib.parse import urlsplit

# A config scope as stored (JSON object): values are whatever each key holds.
DeviceConfig = dict[str, Any]


class SectionId(StrEnum):
    """Section ids. They are stored in the DB, so they are API surface:
    renaming one needs a migration."""
    PLAYBACK = "playback"
    WAKEWORD = "wakeword"
    MICROPHONES = "microphones"
    RING = "ring"
    ADVANCED = "advanced"
    BLUETOOTH = "bluetooth"
    TIMERS = "timers"
    SCREEN = "screen"


@dataclass(frozen=True, slots=True)
class Section:
    label: str              # display only; matches the dashboard Stage title
    keys: tuple[str, ...]   # the config keys this section scopes


SECTIONS: dict[SectionId, Section] = {
    SectionId.PLAYBACK: Section("Playback", ("eqBands", "eqLoudness", "duckDb")),
    # wakeModel is the active registry graph's SHA-256 (§5.1); its
    # thresholds belong to the registry entry, not to config. The open and
    # shadow rules (em_wake_rules) add to that entry's baseline rule.
    SectionId.WAKEWORD: Section("Wake word", ("wakeModel", "saveWakeClips", "wakeArbitrationMs", "wakeSound",
                                              "wakeOpenRules", "wakeShadowRules")),
    # Everything the controller decides about the STT copy and who transcribes
    # the utterance at a pause (§16.6). Gain, beamforming, AEC and AGC belong
    # to the native AFE (§4.1).
    SectionId.MICROPHONES: Section("Speech", ("nsAsr", "saveUtterances", "extendedUtterances", "pauseAsr")),
    SectionId.RING: Section("Ring", (
        "ledScene", "ledListenColor", "ledThinkColor",
        "meterAttack", "meterDecay", "meterFloor",
        "meterGamma", "meterRef", "meterCurve",
    )),
    SectionId.ADVANCED: Section("Button", ("buttonSingleTapEvent", "buttonMultiTapMs")),
    SectionId.BLUETOOTH: Section("Bluetooth", ("bleProxyEnabled",)),
    # Separate from "ring" (the LED ring) so a device can take its own alert
    # sounds without forking its LED scene.
    SectionId.TIMERS: Section("Timers", ("timerSound", "timerRingSeconds", "timerRingGapSeconds", "alarmSound")),
    # The kiosk screen of a `screen_wake` device (the Echo Show): lit through
    # the kiosk's local REST API when a wake candidate opens.
    SectionId.SCREEN: Section("Screen", ("screenWake", "screenWakeUrl", "screenWakePassword")),
}

# Keys that live in the config dict but are device STATE, not settings: they
# belong to no section and are never fleet-inherited.
#
# startupVolume: the controller writes it from every volume report and the
# device re-applies it on its first config per run, which is how volume
# survives a reboot. The fleet value 85 is the start for a device that has
# never reported.
STATE_KEYS: frozenset[str] = frozenset({"startupVolume"})

SECTION_IDS: tuple[SectionId, ...] = tuple(SectionId)


def keys_for(section_ids: Iterable[object] | None) -> set[str]:
    """Every config key belonging to the given sections. Unknown ids ignored."""
    out: set[str] = set()
    for sid in normalise(section_ids):
        out.update(SECTIONS[sid].keys)
    return out


def normalise(section_ids: Iterable[object] | None) -> list[SectionId]:
    """
    Filter to known section ids, in canonical SECTIONS order.

    Stored values survive a controller downgrade/upgrade, so an id we no
    longer recognise must be dropped rather than propagated — otherwise a
    stale id would silently widen or narrow an override.
    """
    if not section_ids:
        return []
    given = set(section_ids)
    return [sid for sid in SECTION_IDS if sid in given]


def merge(global_cfg: Mapping[str, object], device_cfg: Mapping[str, object],
          section_ids: Iterable[object] | None) -> DeviceConfig:
    """
    Effective config: fleet, with the device's values layered over it for the
    sections it overrides.

    STATE_KEYS always come from the device when present, whatever the section
    scoping says — they are that device's own hardware state, and inheriting
    one from the fleet would mean a device coming back at another room's
    volume.
    """
    effective: DeviceConfig = dict(global_cfg)
    overridden = keys_for(section_ids)
    for key in overridden:
        if key in device_cfg:
            effective[key] = device_cfg[key]
    for key in STATE_KEYS:
        if key in device_cfg:
            effective[key] = device_cfg[key]
    return effective


def summarise(section_ids: Iterable[object] | None) -> str:
    """Dashboard-facing one-liner for the Status panel's Config row."""
    n = len(normalise(section_ids))
    if n == 0:
        return "Fleet"
    return f"Local override ({n} of {len(SECTION_IDS)})"


def wake_sound(cfg: Mapping[str, object]) -> bool:
    """Whether wakeSound is on, for both chime gates: the controller's chime
    on acceptance and the `wakeSound` pushed to a `local_wake_chime` device.
    Only JSON true counts: the device decodes the key as a strict boolean."""
    return cfg.get("wakeSound") is True


# Fully Kiosk's local REST API on the device itself: the kiosk runs on the
# Echo Show it lights, so loopback reaches it whatever the network.
SCREEN_WAKE_URL_DEFAULT = "http://127.0.0.1:2323"


def screen_wake(cfg: Mapping[str, object]) -> dict[str, object]:
    """The keys pushed to a `screen_wake` device in `config`: `screenWake` a
    strict boolean as for wakeSound, and the kiosk API's URL and password as
    strings, the defaults standing in for a missing or mistyped value."""
    url, password = cfg.get("screenWakeUrl"), cfg.get("screenWakePassword")
    return {
        "screenWake": cfg.get("screenWake") is True,
        "screenWakeUrl": url if isinstance(url, str) and url else SCREEN_WAKE_URL_DEFAULT,
        "screenWakePassword": password if isinstance(password, str) else "",
    }


def screen_wake_error(values: Mapping[str, object]) -> str | None:
    """Why the screen-wake keys in a config write are invalid, or None. The
    device requests `{screenWakeUrl}/?cmd=screenOn…`, so the URL names the
    kiosk API's root: http(s), a host, and no path, query or fragment."""
    if "screenWake" in values and not isinstance(values["screenWake"], bool):
        return "screenWake must be boolean"
    if "screenWakeUrl" in values and not _kiosk_root(values["screenWakeUrl"]):
        return ("screenWakeUrl must be the kiosk API's http:// or https:// root, "
                f"with no path or trailing slash, e.g. {SCREEN_WAKE_URL_DEFAULT}")
    if "screenWakePassword" in values and not isinstance(values["screenWakePassword"], str):
        return "screenWakePassword must be a string"
    return None


def _kiosk_root(url: object) -> bool:
    """An http(s) URL naming a host (and port) and nothing below the root."""
    if not isinstance(url, str) or any(ch.isspace() for ch in url):
        return False
    try:
        parts = urlsplit(url)
        port = parts.port   # ValueError: not a number, or outside 0-65535
    except ValueError:
        return False
    return (parts.scheme in ("http", "https") and bool(parts.hostname) and port != 0
            and not (parts.path or parts.query or parts.fragment))
