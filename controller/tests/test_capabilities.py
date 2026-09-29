"""Capability negotiation (SPEC §11.1): admission requires the exact v1 set,
and every UI capability derives from the announced set, never a version."""

import re
from pathlib import Path

import pytest

pytest.importorskip("websockets")

import em_audio_timeline as tl  # noqa: E402
import em_device  # noqa: E402
from _device_fakes import ALL_V1, FakeLink, FakeStore, hello, hello_body, make_hub, run  # noqa: E402
from em_device_link import Capability, SessionHello  # noqa: E402

ROOT = Path(__file__).resolve().parents[2]
PROTO_GO = ROOT / "device" / "internal" / "proto" / "proto.go"
SPEC = ROOT / "docs" / "post-afe-audio-architecture.md"


def _go_capabilities() -> set[str]:
    return set(re.findall(r'\bCap\w+\s*=\s*"([a-z0-9_]+)"', PROTO_GO.read_text()))


def test_required_set_is_the_spec_list_and_the_firmware_declares_it():
    spec = SPEC.read_text()
    block = spec[spec.index("### 11.1 Capability cutover"):]
    block = block[block.index("```text") + 7:]
    listed = set(block[:block.index("```")].split())
    assert em_device.REQUIRED_CAPABILITIES == listed
    assert em_device.REQUIRED_CAPABILITIES <= _go_capabilities()


def test_every_capability_the_controller_knows_is_one_the_firmware_can_send():
    assert {c.value for c in Capability} <= _go_capabilities()


def _admit(hello_body):
    store = FakeStore()
    store.add("DEV1", approved=True)
    hub, _, _ = make_hub(store)
    return run(hub.admit(hello_body, device_id="DEV1", peer_ip="10.0.0.5",
                         secure=True, token=None))


@pytest.mark.parametrize("missing", ALL_V1)
def test_admission_rejects_a_hello_missing_any_v1_capability(missing):
    caps = [c for c in ALL_V1 if c != missing] + ["leds", "led_anim"]
    assert _admit(hello(caps)) == em_device.em_device_link.Rejected("protocol")


def test_admission_accepts_the_full_set_whatever_the_version_string():
    result = _admit(hello(firmware_version="dev-20260101-0000"))
    assert isinstance(result, em_device.em_device_link.Admitted)


@pytest.mark.parametrize("bad", [{"protocols": [2]}, {"protocols": None},
                                 {"capabilities": "audio_timeline_v1"}])
def test_a_malformed_or_unsupported_hello_is_refused(bad):
    body = hello_body()
    body.update(bad)
    with pytest.raises(tl.ProtocolError):
        SessionHello.parse(body)


@pytest.mark.parametrize("prop, cap", [
    ("led_anim_capable", "led_anim"),
    ("button_hold_capable", "button_hold"),
    ("ambient_light_capable", "ambient_light"),
])
def test_ui_capability_properties_follow_the_announced_set(prop, cap):
    async def scenario():
        store = FakeStore()
        store.add("DEV1", approved=True)
        hub, _, _ = make_hub(store)
        device = await hub.ensure("DEV1", "Office")
        for caps, version, expected in ((ALL_V1, "v99.0.0", False),
                                        (ALL_V1 + [cap], "v0.0.1", True)):
            link = FakeLink(hello(sorted(set(caps)), firmware_version=version))
            await device._ready(link)
            assert getattr(device, prop) is expected
            device._lost(link, "closed")
        await device.close()
    run(scenario())
