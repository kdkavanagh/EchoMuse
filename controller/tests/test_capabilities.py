"""
Device/controller compatibility is negotiated by CAPABILITY, not version.

The two halves of this project ship on independent version schemes (device
`v*`, controller `controller-v*`), so any given moment can pair new firmware
with an old controller or the reverse. Version comparison would mean encoding
release history into the controller and getting it wrong the first time
someone runs a dev build; a capability is the device stating what it
implements.

That only works if both sides spell the capability identically. A typo makes
the feature permanently unavailable and looks exactly like a device that does
not support it — silent, and the sort of thing you debug from the wrong end.
So the strings are asserted to match across the two languages, the same way
CONFIG_SECTIONS is mirrored between Python and dashboard.jsx.
"""

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
CONTROL_GO = ROOT / "device" / "internal" / "client" / "control.go"
CONTROLLER = ROOT / "controller" / "em_controller.py"
API = ROOT / "controller" / "em_api.py"
ESPHOME = ROOT / "controller" / "em_esphome.py"


def device_capabilities() -> list[str]:
    """
    Every capability the firmware can announce.

    Read from the whole capabilities() function rather than a single literal:
    the list is no longer fixed — "ambient_light" is appended only when the
    hardware actually has a readable sensor, because the controller advertises
    an HA entity off the back of it. A parser that only understood one literal
    would silently stop covering the conditional ones, which is the direction
    that hides a typo rather than surfacing it.
    """
    src = CONTROL_GO.read_text()
    m = re.search(r'func capabilities\(\) \[\]string \{(.*?)\n\}', src, re.S)
    assert m, "could not find func capabilities() in control.go"
    return re.findall(r'"([a-z_]+)"', m.group(1))


def test_device_announces_expected_capabilities():
    caps = device_capabilities()
    for expected in ("mic", "speaker", "leds", "led_anim", "buttons", "oww_shadow"):
        assert expected in caps, f"firmware no longer announces {expected!r}"


def test_every_capability_the_controller_checks_is_one_the_device_sends():
    """
    A controller checking for a capability string the device never sends is a
    feature that is silently off forever. This catches the typo direction that
    the device-side test cannot.
    """
    caps = set(device_capabilities())
    # Two idioms, both scanned: `"<cap>" in (self.capabilities or [])` on
    # Device in em_controller, and _device_has("<cap>") on the ESPHome
    # satellite, which decides which HA entities get advertised. A typo in
    # either is an entity that never appears or never fires, with no error
    # anywhere — exactly what this test exists to catch.
    checked = set(re.findall(r'"([a-z_]+)"\s+in\s+\(self\.capabilities',
                             CONTROLLER.read_text()))
    checked |= set(re.findall(r'_device_has\(\s*"([a-z_]+)"\s*\)',
                              ESPHOME.read_text()))
    assert checked, "no capability checks found — has the idiom changed?"
    unknown = checked - caps
    assert not unknown, (
        f"controller checks capabilities the firmware never announces: {sorted(unknown)}. "
        f"Device sends: {sorted(caps)}"
    )


def test_shadow_capability_is_surfaced_to_the_dashboard():
    """
    The dashboard must be able to tell "cannot" from "off", or it offers a
    toggle that silently does nothing on older firmware — which reads as a
    broken feature rather than an unsupported one.
    """
    assert "oww_shadow_capable" in CONTROLLER.read_text(), \
        "em_controller must expose the shadow capability as a property"
    assert "owwShadowCapable" in API.read_text(), \
        "/api/devices must surface the shadow capability"
    jsx = (ROOT / "controller" / "static" / "dashboard.jsx").read_text()
    assert "owwShadowCapable" in jsx, \
        "the dashboard must gate the on-device toggle on the capability"


def test_triggering_is_a_separate_capability_from_scoring():
    """
    Shadow shipped first, so there is firmware in the field that scores the
    wake word and reports it while having no code to act on it. Gating "on"
    behind oww_shadow alone would offer those devices a mode that leaves them
    scoring perfectly and never answering — the "I enabled it and nothing
    happened" the capability rule exists to prevent.
    """
    caps = device_capabilities()
    assert "oww_trigger" in caps, "firmware no longer announces oww_trigger"
    assert "oww_shadow" in caps, \
        "oww_trigger must not replace oww_shadow — shadow is still a mode"
    assert "oww_trigger_capable" in CONTROLLER.read_text(), \
        "em_controller must expose the trigger capability as a property"
    assert "owwTriggerCapable" in API.read_text(), \
        "/api/devices must surface the trigger capability"
    assert "owwTriggerCapable" in (ROOT / "controller" / "static" / "dashboard.jsx").read_text(), \
        "the dashboard must gate the 'On device' option on the capability"


def test_the_wake_chime_is_gated_on_the_firmware_that_carries_it():
    """
    The chime is embedded in the firmware and played from a `wake_sound`
    control message, so a device that predates it ignores the message
    entirely — no error, no sound, nothing in any log the user reads. That is
    exactly the shape a version comparison gets wrong and a capability gets
    right.

    Both halves are pinned: the device announcing it, and the controller
    refusing to send unless it did. The message being harmless to an old
    device is not a reason to send it blind — "the chime is off" and "this
    Echo cannot chime" are different answers, and only one of them is fixed
    by turning the setting on.
    """
    caps = device_capabilities()
    assert "wake_sound" in caps, "firmware no longer announces wake_sound"
    assert "wake_sound_capable" in CONTROLLER.read_text(), \
        "em_controller must expose the wake-sound capability as a property"
    assert "wakeSoundCapable" in API.read_text(), \
        "/api/devices must surface the wake-sound capability"
    jsx = (ROOT / "controller" / "static" / "dashboard.jsx").read_text()
    assert "wakeSoundCapable" in jsx, \
        "the dashboard must gate the wake chime toggle on the capability"


def test_the_wake_chime_is_audio_the_device_already_has():
    """
    The whole point of this feature is that the sound does not travel: the
    clip lives in the firmware (device/internal/cue) and only the DECISION
    crosses the link, so the feedback never waits on a link with measured
    1.1–2.6s RTT excursions.

    Pinned because the tempting "fix" for any future change to the sound is
    to stream it from the controller like em_sounds does for a timer ring —
    which would work, and would quietly turn the one piece of instant
    feedback in the system into another thing that can arrive late.
    """
    pcm = ROOT / "device" / "internal" / "cue" / "wake_word_triggered.pcm"
    assert pcm.exists(), "the embedded wake chime is missing"
    assert pcm.stat().st_size > 1000, "the embedded wake chime looks like a stub"

    control = CONTROL_GO.read_text()
    assert 'case "wake_sound":' in control, \
        "the device must handle the wake_sound control message"

    controller = CONTROLLER.read_text()
    m = re.search(r'async def play_wake_sound\(self\):(.*?)\n    async def',
                  controller, re.S)
    assert m, "em_controller.Device must still have play_wake_sound"
    body = m.group(1)
    assert '"type": "wake_sound"' in body, \
        "play_wake_sound must send the wake_sound control message"
    assert "send_data" not in body and "stream_speaker" not in body, \
        "the chime must not be streamed as audio — it is already on the device"


# Keys the device's audio HAL owns, which the controller therefore must not
# offer as settings. Each was a real config key until the pipeline moved to
# Android's audio HAL (docs/native-afe-migration.md); the firmware no longer
# reads any of them, and its AFE.cfg tuning lives on a read-only partition.
_HAL_OWNED_KEYS = (
    "micGainDb", "adcMicpga", "adcDigitalGain",
    "beamformingEnabled", "beamAngle",
    "aecEnabled", "aecDelayMs", "aecTailMs",
    "agcEnabled",
)


def test_the_mic_chain_is_not_offered_as_config():
    """
    The one direction this can fail silently.

    A control the device cannot honour is the exact thing the capability rule
    exists to prevent, and these nine are the ones most likely to come back:
    they read like ordinary audio settings, they have obvious dashboard
    widgets, and re-adding one produces a slider that moves, saves, pushes and
    changes nothing about the sound — indistinguishable from a setting that
    simply does not help.

    Asserted against DEFAULT_DEVICE_CONFIG and the dashboard rather than the
    Go source: the firmware ignoring an unknown field is the safe half. The
    unsafe half is the controller believing it is a setting.
    """
    import em_db
    present = [k for k in _HAL_OWNED_KEYS if k in em_db.DEFAULT_DEVICE_CONFIG]
    assert not present, (
        f"{present} is owned by the device's audio HAL and cannot be set from "
        f"here — the firmware does not read it"
    )
    jsx = (ROOT / "controller" / "static" / "dashboard.jsx").read_text()
    shown = [k for k in _HAL_OWNED_KEYS if k in jsx]
    assert not shown, f"dashboard still renders HAL-owned control(s): {shown}"


def test_the_toggle_control_actually_honours_disabled():
    """
    "Disabled with the reason, never a control that silently does nothing" is
    the rule every capability-gated control in this dashboard relies on — and
    for a release, Toggle did not take a `disabled` prop at all (only Slider
    did). Three toggles therefore rendered greyed with their reason, read as
    off, and WROTE THE OPPOSITE VALUE into config when clicked: worse than
    doing nothing, because the setting silently disagreed with what the
    control showed.

    Asserted against the component rather than the call sites, because the
    call sites already looked correct while the bug was live.
    """
    jsx = (ROOT / "controller" / "static" / "dashboard.jsx").read_text()
    m = re.search(r'function Toggle\(\{(.*?)\}\)', jsx, re.S)
    assert m, "dashboard.jsx must still define a Toggle component"
    assert "disabled" in m.group(1), \
        "Toggle must accept a `disabled` prop — every caller that passes one " \
        "is relying on it to refuse the write, not merely to grey the switch"

    body = jsx[m.end():jsx.index("\n}", m.end())]
    assert re.search(r'if\s*\(!disabled\)|disabled\s*\?\s*undefined|disabled\s*\|\|', body), \
        "Toggle's click handler must check `disabled` before calling onChange — " \
        "styling it grey while still writing the value is the bug this pins"


def test_there_is_no_audio_pipeline_switch_left_to_half_remove():
    """
    There was an opt-in that chose between EchoMuse's own audio pipeline and
    the HAL's: a marker file, a capability pair, an endpoint and a dashboard
    panel. All four are gone, and they have to go together — a surviving
    marker check or endpoint would offer a device a backend the firmware no
    longer contains, and the observable failure is a reboot into a device
    whose audio never starts.
    """
    assert "native_afe" not in CONTROLLER.read_text(), \
        "em_controller still references the removed native-AFE capability"
    api = API.read_text()
    assert "native_afe" not in api and "nativeAfe" not in api, \
        "em_api still exposes the removed native-AFE endpoint or capability"
    jsx = (ROOT / "controller" / "static" / "dashboard.jsx").read_text()
    assert "nativeAfe" not in jsx, \
        "the dashboard still renders the removed audio-pipeline panel"
    script = (ROOT / "controller" / "device_payloads" / "start_server.sh").read_text()
    assert "EM_NATIVE_AFE" not in script, \
        "start_server.sh still checks the removed opt-in marker"


def test_capabilities_reported_before_the_server_exists_are_not_lost():
    """
    A device registers BEFORE its ESPHome server is created — the listener
    only comes up once the device is present — so the capability push finds
    no server. Dropping it there built the entity list from an empty set, and
    that list is a ONE-SHOT at ListEntities time: HA caches it and the sensor
    is absent for the life of the connection.

    Observed on Retreat, 2026-08-03: registered 05:25:33, server created
    05:25:34, no ambient light entity in HA afterwards. The same race resolves
    differently on each controller restart, which is why the graph came and
    went rather than simply never working.

    Read as source, not imported: em_esphome pulls in zeroconf and aiohttp,
    which this suite deliberately does without.
    """
    src = ESPHOME.read_text()
    setter = re.search(r"def set_device_capabilities\(.*?\n(?=\n\ndef |\Z)", src, re.S)
    assert setter, "could not find set_device_capabilities"
    body = setter.group(0)
    # The store must happen unconditionally — not inside the "if server" arm,
    # which is exactly what dropped it.
    store = re.search(r"^\s{4}_pending_caps\[device_id\]\s*=", body, re.M)
    assert store, (
        "set_device_capabilities must hold the capabilities unconditionally; "
        "pushing them only when a server already exists loses them"
    )


def test_the_pending_capabilities_are_applied_when_the_server_is_built():
    """
    Guard against the two halves drifting: holding the value is only useful
    if server creation applies it to the same attribute the ListEntities gate
    reads (_device_has → srv.capabilities).
    """
    src = ESPHOME.read_text()
    create = re.search(r"async def _register_device_server\(.*?\n(?=\n\nasync def |\n\ndef |\Z)",
                       src, re.S)
    assert create, "could not find _register_device_server"
    body = create.group(0)
    assert "_pending_caps" in body, \
        "server creation must seed capabilities from the pending map"
    assert "set_capabilities" in body, \
        "and must apply them via the same setter the entity gate reads"


def test_a_capability_that_changes_later_rebuilds_the_entity_list():
    """
    The other half of the one-shot problem, and `_pending_caps` does not reach
    it: when capabilities change AFTER HA has enumerated, the server has the
    new list but HA read the entity list once at connect and never asks again.

    Reachable in normal operation rather than only in theory. `als.resolve()`
    deliberately does not cache a negative result, because the first lookup
    happens moments after a cold boot when sysfs is least likely to be
    complete — so a device can register without `ambient_light` and acquire it
    on a later scan. Before this, that device kept the entity list from the
    registration that missed the sensor, and the only cure was a controller
    restart that happened to win the race (#90).

    Bouncing the HA connection is the documented remedy; `update_oww_model`
    does the same for the wake word configuration, and HA redials in seconds.
    """
    src = ESPHOME.read_text()
    setter = re.search(r"def set_device_capabilities\(.*?\n(?=\n\ndef |\Z)", src, re.S)
    assert setter, "could not find set_device_capabilities"
    body = setter.group(0)

    assert "disconnect()" in body, (
        "a capability change after ListEntities must bounce the HA connection, "
        "or the new entity never appears for the life of that connection"
    )
    # It must be conditional on an actual change. Bouncing on every register
    # would drop HA's connection on every device reconnect.
    assert re.search(r"if\s+set\(caps\)\s*==\s*before", body), (
        "the bounce must be gated on the capability set actually changing; "
        "bouncing unconditionally disconnects HA on every device reconnect"
    )


def test_the_api_can_tell_no_sensor_from_no_reading():
    """
    0 lux is a real reading from a covered sensor, so absence cannot be
    expressed as a value: whether the device HAS the sensor has to be a
    separate field from what it read.

    Asserted on the API rather than the dashboard deliberately. A first
    attempt put this on the Status tab, which pushed the panel past its
    height and gave the page a scrollbar — what the device panel should show
    is its own question, tracked separately. The API field stands on its own
    merits regardless of who renders it.
    """
    api = (Path(__file__).resolve().parent.parent / "em_api.py").read_text()
    assert "ambientLightCapable" in api, \
        "/api/devices must report whether the device found its ALS"
