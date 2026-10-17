"""Capability negotiation (SPEC §11.1): admission requires the exact v1 set
and one of the supported images (the Dot's Fire OS 6, the Echo Show's Fire OS
6 or LineageOS 18.1), and every UI capability derives from the announced set
and the platform, never a version."""

import re
from pathlib import Path

import pytest

pytest.importorskip("websockets")

import em_audio_timeline as tl  # noqa: E402
import em_device  # noqa: E402
from _device_fakes import (ALL_V1, FakeHost, FakeLink, FakeStore, envelope, hello, hello_body,  # noqa: E402
                           make_hub, run, settle)
from em_device_link import Capability, DevicePlatform, SessionHello  # noqa: E402

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
    ("local_wake_chime_capable", "local_wake_chime"),
    ("no_input_chime_capable", "no_input_chime"),
    ("announcement_chime_capable", "announcement_chime"),
    ("open_rules_capable", "open_rules_v1"),
    ("afe_metadata_capable", "afe_metadata_v1"),
    ("leds_capable", "leds"),
    ("wifi_control_capable", "wifi_control"),
    ("ble_proxy_capable", "ble_proxy"),
    ("privacy_mute_capable", "privacy_mute"),
    ("screen_wake_capable", "screen_wake"),
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


def test_the_controller_knows_exactly_the_platforms_the_firmware_names():
    go = set(re.findall(r'\bPlatform\w+\s+Platform\s*=\s*"([a-z0-9_]+)"', PROTO_GO.read_text()))
    assert go and {p.value for p in DevicePlatform} == go


@pytest.mark.parametrize("extra, granted", [([], False), (["afe_metadata_v1"], True)])
def test_session_ready_grants_afe_metadata_only_to_a_device_announcing_it(extra, granted):
    # An older device must never see the key: it would not know the stream.
    result = _admit(hello(ALL_V1 + extra))
    assert isinstance(result, em_device.em_device_link.Admitted)
    assert result.ready.afe_metadata is granted
    wire = result.ready.wire()
    assert ("afe_metadata" in wire) is granted
    if granted:
        assert wire["afe_metadata"] is True


def test_the_actor_session_carries_the_afe_stream_exactly_when_ready_granted_it():
    async def scenario():
        store = FakeStore()
        store.add("DEV1", approved=True)
        hub, host, _ = make_hub(store)
        device = await hub.ensure("DEV1", "Office")
        for caps, expected in ((ALL_V1, False), (ALL_V1 + ["afe_metadata_v1"], True)):
            link = FakeLink(hello(sorted(caps)))
            await device._ready(link)
            assert host.actors["DEV1"].afe_metadata is expected
            device._lost(link, "closed")
        await device.close()
    run(scenario())


def _hello_without_platform() -> SessionHello:
    body = hello_body()
    del body["platform"]
    return SessionHello.parse(body)


@pytest.mark.parametrize("extra, expected", [
    ({"platform": "fireos6"}, DevicePlatform.FIREOS6),
    ({"platform": "fireos6_crown"}, DevicePlatform.FIREOS6_CROWN),
    ({"platform": "lineage18_crown"}, DevicePlatform.LINEAGE18_CROWN),
    ({"platform": "fireos5"}, None),
    ({"platform": "fireos7"}, None),
    ({"platform": None}, None),
    ({"platform": 6}, None),
])
def test_hello_platform(extra, expected):
    assert hello(**extra).platform is expected


def test_a_hello_naming_no_platform_parses_as_none():
    assert _hello_without_platform().platform is None


@pytest.mark.parametrize("platform", ["fireos6", "fireos6_crown", "lineage18_crown"])
def test_admission_accepts_every_supported_image(platform):
    assert isinstance(_admit(hello(platform=platform)), em_device.em_device_link.Admitted)


@pytest.mark.parametrize("platform, logged", [
    ("fireos5", "'fireos5'"), ("fireos7", "'fireos7'"), ("crown", "'crown'"),
    ("lineage18", "'lineage18'"), ("lineage19_crown", "'lineage19_crown'"),
    (None, "None"), (6, "6")])
def test_admission_refuses_any_other_platform(platform, logged, caplog):
    # Firmware from this tree runs audio only on the Dot's Fire OS 6 and the
    # Echo Show's Fire OS 6 or LineageOS 18.1: admitting another image would
    # only offer it an update that breaks it.
    assert _admit(hello(platform=platform)) == em_device.em_device_link.Rejected("protocol")
    assert f"unsupported platform: {logged}" in caplog.text


def test_admission_refuses_a_hello_naming_no_platform(caplog):
    assert _admit(_hello_without_platform()) == em_device.em_device_link.Rejected("protocol")
    assert "unsupported platform: absent" in caplog.text


# What each image's current firmware announces (decisions 6 and 7 of the
# Echo Show port plan, §3 of the LineageOS plan, decision 8 of the companion
# plan), a Dot on firmware predating the Dot-only three, and a LineageOS Show
# on firmware predating the companion app, which announced the Fire OS set.
DOT = ALL_V1 + ["leds", "led_anim", "buttons", "button_hold", "alert_prefetch",
                "local_wake_chime", "open_rules_v1", "ambient_light", "afe_metadata_v1",
                "wifi_control", "ble_proxy", "privacy_mute"]
DOT_OLDER = [c for c in DOT if c not in ("wifi_control", "ble_proxy", "privacy_mute")]
FIREOS_SHOW = ALL_V1 + ["alert_prefetch", "local_wake_chime", "open_rules_v1", "afe_metadata_v1",
                        "ambient_light", "screen_wake"]
COMPANION = ["leds", "led_anim", "buttons", "button_hold", "screen_overlay"]
LINEAGE_SHOW = FIREOS_SHOW + COMPANION
LINEAGE_SHOW_OLDER = FIREOS_SHOW
SHOWS = [("fireos6_crown", FIREOS_SHOW), ("lineage18_crown", LINEAGE_SHOW)]
SHOW_CAPS = dict(SHOWS)
DEVICES = [("fireos6", DOT), ("fireos6", DOT_OLDER), *SHOWS, ("lineage18_crown", LINEAGE_SHOW_OLDER)]


async def _session(caps, platform, store=None, host=None, **extra):
    store = store or FakeStore()
    store.add("DEV1", approved=True)
    hub, host, store = make_hub(store, host)
    admitted = await hub.admit(hello(caps, platform=platform, **extra), device_id="DEV1",
                               peer_ip="10.0.0.5", secure=True, token=None)
    link = FakeLink(hello(caps, platform=platform, **extra))
    await admitted.sink.on_ready(link)
    return hub.devices["DEV1"], link, host, admitted.sink


RING, BUTTON = "no LED ring on this device", "no action button on this device"
UPDATE, MANAGED = "firmware update available", "managed by the device"
NO_SCREEN = "no screen on this device"


SHOW_GAPS = {"wifi_control": MANAGED, "ble_proxy": MANAGED,
             "privacy_mute": "mute not supported on this device yet"}
FIREOS_SHOW_GAPS = {**SHOW_GAPS, "leds": RING, "led_anim": RING, "buttons": BUTTON,
                    "button_hold": BUTTON}
DOT_GAPS = {"screen_wake": NO_SCREEN, "screen_overlay": NO_SCREEN}


@pytest.mark.parametrize("platform, caps, gaps", [
    ("fireos6", DOT, DOT_GAPS),
    ("fireos6", DOT_OLDER, {**DOT_GAPS, "wifi_control": UPDATE, "ble_proxy": UPDATE,
                            "privacy_mute": UPDATE}),
    # A Fire OS Show has no ring or button, and never an app to paint them.
    ("fireos6_crown", FIREOS_SHOW, FIREOS_SHOW_GAPS),
    ("fireos6_crown", [c for c in FIREOS_SHOW if c != "screen_wake"],
     {**FIREOS_SHOW_GAPS, "screen_wake": UPDATE}),
    # A LineageOS Show's companion app brings them; older firmware lacks it.
    ("lineage18_crown", LINEAGE_SHOW, SHOW_GAPS),
    ("lineage18_crown", [c for c in LINEAGE_SHOW if c != "screen_wake"],
     {**SHOW_GAPS, "screen_wake": UPDATE}),
    ("lineage18_crown", LINEAGE_SHOW_OLDER, {**SHOW_GAPS, **{c: UPDATE for c in COMPANION}}),
    # Absent elsewhere, the dashboard's own per-capability reason applies.
    ("fireos6", [c for c in DOT if c != "led_anim"], DOT_GAPS),
])
def test_capability_gaps_name_the_reason_the_platform_gives(platform, caps, gaps):
    async def scenario():
        device, *_ = await _session(caps, platform)
        assert device.capability_gaps == gaps
        await device.close()
    run(scenario())


def test_no_session_no_gap():
    assert em_device.capability_gap(None, [], Capability.WIFI_CONTROL) is None


@pytest.mark.parametrize("platform, caps, reason", [
    *((show, caps, MANAGED) for show, caps in SHOWS), ("fireos6", DOT_OLDER, UPDATE)])
def test_wifi_change_and_scan_are_refused_with_the_reason(platform, caps, reason, monkeypatch):
    pytest.importorskip("aiohttp")
    from aiohttp import web
    from aiohttp.test_utils import make_mocked_request

    import em_api

    async def body(_request):
        return {"ssid": "Home", "psk": "correct horse"}

    async def scenario():
        device, link, *_ = await _session(caps, platform)
        monkeypatch.setattr(em_api, "_devices", {"DEV1": device})
        monkeypatch.setattr(em_api, "_json_body", body)
        for handler in (em_api._post_device_wifi, em_api._post_device_wifi_scan):
            request = make_mocked_request("POST", "/api/devices/DEV1/wifi",
                                          match_info={"id": "DEV1"}, app=web.Application())
            with pytest.raises(em_api.ApiError) as refused:
                await handler.__wrapped__(request)
            assert (refused.value.code, refused.value.status) == (em_api.ErrorCode.NOT_SUPPORTED, 409)
            assert refused.value.message == f"Wi-Fi changes unavailable: {reason}"
        assert not [t for t, _ in link.sent if t.startswith("wifi")]
        await device.close()
    run(scenario())


def test_a_dot_announcing_wifi_control_may_change_wifi(monkeypatch):
    pytest.importorskip("aiohttp")
    import em_api

    async def scenario():
        device, *_ = await _session(DOT, "fireos6")
        monkeypatch.setattr(em_api, "_devices", {"DEV1": device})
        assert em_api._require_wifi_control("DEV1") is device
        await device.close()
    run(scenario())


ADVERT = {"addr": "AA:BB:CC:DD:EE:FF", "addrType": 0, "rssi": -60, "data": "AgEG"}


@pytest.mark.parametrize("platform, caps", DEVICES)
def test_ble_adverts_reach_the_proxy_only_from_a_ble_proxy_device(platform, caps, caplog):
    async def scenario():
        host, forwarded = FakeHost(), []
        host.ble_adverts = lambda device_id, adverts: forwarded.append(adverts)
        device, _, _, sink = await _session(caps, platform, host=host)
        for _ in range(2):
            sink.on_message(envelope("ble_adverts", {"adverts": [ADVERT]}))
        capable = "ble_proxy" in caps
        assert forwarded == ([[ADVERT], [ADVERT]] if capable else [])
        # Dropped, and said so once per session rather than per batch.
        assert caplog.text.count("ble_adverts dropped") == (0 if capable else 1)
        await device.close()
    run(scenario())


@pytest.mark.parametrize("platform, caps", DEVICES)
def test_mute_is_shown_only_from_a_privacy_mute_device(platform, caps):
    async def scenario():
        device, _, host, sink = await _session(caps, platform)
        sink.on_message(envelope("privacy.changed",
                                 {"muted": True, "capture_epoch": None, "physical_seq": 1}))
        await settle()
        shown = True if "privacy_mute" in caps else None   # unavailable, never "off"
        assert device.muted is True                          # as the device reported it
        assert device.privacy_state is shown
        assert {"muted": shown} in host.pushed
        await device.close()
    run(scenario())


SCREEN = {"screenWake": True, "screenWakeUrl": "http://192.168.3.159:2323", "screenWakePassword": "pw"}


# The kiosk URL and password still go to a screen_overlay device: older
# LineageOS firmware, without the app, lights the screen through them.
@pytest.mark.parametrize("platform, caps, pushed", [
    *((show, caps, SCREEN) for show, caps in SHOWS), ("lineage18_crown", LINEAGE_SHOW_OLDER, SCREEN),
    ("fireos6", DOT, {}), ("fireos6", DOT_OLDER, {})])
def test_screen_wake_config_goes_only_to_a_screen_wake_device(platform, caps, pushed):
    async def scenario():
        store = FakeStore()
        store.configs["DEV1"] = {"duckDb": -18.0, **SCREEN}
        device, link, *_ = await _session(caps, platform, store)
        config = next(body for kind, body in link.sent if kind == "config")
        assert {k: v for k, v in config.items() if k.startswith("screen")} == pushed
        await device.close()
    run(scenario())


@pytest.mark.parametrize("platform, caps, painted", [
    ("fireos6", DOT, True), ("fireos6", DOT_OLDER, True), ("fireos6_crown", FIREOS_SHOW, False),
    # The companion app's light bar is the LineageOS Show's ring.
    ("lineage18_crown", LINEAGE_SHOW, True), ("lineage18_crown", LINEAGE_SHOW_OLDER, False)])
def test_the_ring_is_painted_only_on_a_device_with_leds(platform, caps, painted):
    async def scenario():
        device, link, *_ = await _session(caps, platform)
        await settle()
        assert any(kind in ("leds", "led_anim") for kind, _ in link.sent) is painted
        await device.close()
    run(scenario())


def _press(seq, **kw):
    return envelope("button.action", {"click_type": 138, "down": False, "held_ms": 120, "muted": False,
                                      "physical_seq": seq, "occurrence_id": None, "handled": None, **kw})


def test_a_lineageos_show_touch_follows_the_action_button_policy():
    # The companion app's touch zones reach the firmware's action-button
    # path, so the controller sees the Dot's button.action and decides alike.
    async def scenario():
        device, _, host, sink = await _session(LINEAGE_SHOW, "lineage18_crown")
        sink.on_message(_press(1))                                         # tap → turn
        sink.on_message(_press(2, handled="alert_stopped", occurrence_id="o"))  # stopped on the device
        sink.on_message(_press(3, held_ms=900))                            # hold → HA long press
        await settle()
        device.actor.turn_active = True
        sink.on_message(_press(4))                                         # tap in a turn → cancel
        await settle()
        assert [t["physical_seq"] for t in device.actor.turns] == [1]
        assert device.actor.cancels == ["interrupted"]
        assert [e for e in host.events if e[0] == "button_event"] == [("button_event", "long")]
        await device.close()
    run(scenario())


def test_a_lineageos_show_tap_may_fire_the_single_tap_event():
    # buttonSingleTapEvent needs button_hold, which the companion firmware announces.
    async def scenario():
        store = FakeStore()
        store.configs["DEV1"] = {"duckDb": -18.0, "buttonSingleTapEvent": True}
        device, _, host, sink = await _session(LINEAGE_SHOW, "lineage18_crown", store)
        sink.on_message(_press(1))
        await settle()
        assert device.actor.turns == []
        assert ("button_event", "single") in host.events
        await device.close()
    run(scenario())


COMPANION_HEALTH = {"version": "cmp-0123456789ab", "linked": True, "overlay": None, "error": None}


@pytest.mark.parametrize("platform, caps, body, companion", [
    ("lineage18_crown", LINEAGE_SHOW, {"cpuPct": 12.5, "companion": COMPANION_HEALTH}, COMPANION_HEALTH),
    # A device without the app sends no `companion`: the dashboard shows no row.
    ("fireos6", DOT, {"cpuPct": 12.5}, None)])
def test_companion_health_passes_through_stats(platform, caps, body, companion):
    async def scenario():
        device, _, host, sink = await _session(caps, platform)
        sink.on_message(envelope("stats", body))
        await settle()
        assert device.stats["companion"] == companion
        assert device.stats["cpuPct"] == 12.5
        assert {"stats": device.stats} in host.pushed
        await device.close()
    run(scenario())


@pytest.mark.parametrize("platform, debloat_gap", [
    ("fireos6_crown", None),
    ("lineage18_crown", "nothing to debloat: LineageOS has no Amazon apps or services"),
])
def test_the_dashboard_device_json_carries_the_platform_reasons(platform, debloat_gap, tmp_path,
                                                               monkeypatch):
    pytest.importorskip("aiohttp")
    import em_api
    import em_db

    em_db.init(str(tmp_path / "t.db"))
    em_db.register_new_device("DEV1", "10.0.0.5", "v3.0.0", None)
    em_db.approve_device("DEV1", "Kitchen")

    async def scenario():
        device, *_ = await _session(SHOW_CAPS[platform], platform)
        monkeypatch.setattr(em_api, "_devices", {"DEV1": device})
        out = em_api._merge_device(em_db.get_device("DEV1"))
        assert out["platform"] == platform
        assert out["muted"] is None                   # unavailable, not "off"
        assert out["capability_gaps"]["wifi_control"] == "managed by the device"
        assert out["capability_gaps"]["privacy_mute"] == "mute not supported on this device yet"
        assert out["debloat_gap"] == debloat_gap
        await device.close()
        # Offline: nothing live to gate on.
        monkeypatch.setattr(em_api, "_devices", {})
        out = em_api._merge_device(em_db.get_device("DEV1"))
        assert (out["platform"], out["capability_gaps"], out["muted"], out["debloat_gap"]) \
            == (None, {}, None, None)
    try:
        run(scenario())
    finally:
        em_db._conn.close()
        em_db._conn = None


@pytest.mark.parametrize("platform, caps, gap", [
    ("fireos6", DOT, None), ("fireos6_crown", FIREOS_SHOW, None),
    ("lineage18_crown", LINEAGE_SHOW, "nothing to debloat: LineageOS has no Amazon apps or services")])
def test_only_a_lineageos_show_has_nothing_to_debloat(platform, caps, gap):
    async def scenario():
        device, *_ = await _session(caps, platform)
        assert device.debloat_gap == gap
        await device.close()
    run(scenario())


def test_re_apply_debloat_is_refused_with_the_reason_on_lineageos(monkeypatch):
    pytest.importorskip("aiohttp")
    from aiohttp import web
    from aiohttp.test_utils import make_mocked_request

    import em_api

    spawned = []
    monkeypatch.setattr(em_api, "_spawn", lambda coro, name: spawned.append(name) or coro.close())

    async def scenario():
        device, link, *_ = await _session(LINEAGE_SHOW, "lineage18_crown")
        monkeypatch.setattr(em_api, "_devices", {"DEV1": device})
        request = make_mocked_request("POST", "/api/devices/DEV1/debloat",
                                      match_info={"id": "DEV1"}, app=web.Application())
        with pytest.raises(em_api.ApiError) as refused:
            await em_api._post_debloat.__wrapped__(request)
        assert (refused.value.code, refused.value.status) == (em_api.ErrorCode.NOT_SUPPORTED, 409)
        assert refused.value.message == ("Debloat unavailable: nothing to debloat: LineageOS has "
                                         "no Amazon apps or services")
        assert spawned == []
        await device.close()
    run(scenario())


async def _ha_server(monkeypatch, *, platform=DevicePlatform.FIREOS6, volume_control=True):
    """An ESPHome satellite server for DEV G090LF11 after device_connected,
    with no listener."""
    import em_esphome
    from em_session import VoicePhase

    async def no_listener(host):
        return None

    server = em_esphome.DeviceESPhomeServer("G090LF11", "Kitchen", "02:00:00:00:00:01", 16000)
    monkeypatch.setattr(server, "start", no_listener)
    monkeypatch.setitem(em_esphome._servers, "G090LF11", server)
    await em_esphome.device_connected(
        "G090LF11", label="Kitchen", capabilities=frozenset(ALL_V1), platform=platform,
        hooks=em_esphome.Hooks(), volume=0.5, volume_control=volume_control,
        voice_phase=VoicePhase.IDLE)
    return em_esphome.EchoMuseSatellite(server)


@pytest.mark.parametrize("platform, model", [
    (DevicePlatform.FIREOS6, "Echo Dot Gen 2 (biscuit)"),
    (DevicePlatform.FIREOS6_CROWN, "Echo Show 8 Gen 1 (crown)"),
    (DevicePlatform.LINEAGE18_CROWN, "Echo Show 8 Gen 1 (crown, LineageOS)"),
])
def test_home_assistant_is_told_which_echo_the_platform_is(platform, model, monkeypatch):
    pytest.importorskip("zeroconf")
    from esphome.vendor import api_pb2

    async def scenario():
        satellite = await _ha_server(monkeypatch, platform=platform)
        info = next(iter(satellite.handle_message(api_pb2.DeviceInfoRequest())))
        # HA shows the part of project_name after the dot as the Model.
        assert (info.model, info.project_name) == (model, f"EchoMuse.{model}")
    run(scenario())


# ─── Volume an Echo Show could not verify (hello `volume.level` null) ────────

NO_VOLUME = {"level": None, "seeded": False}
VOLUME_GAP = "volume control unavailable: Android volume check failed (see device log)"


@pytest.mark.parametrize("volume, level, available", [
    ({"level": 87, "seeded": True}, 87, True),
    (NO_VOLUME, None, False),
    ({"seeded": False}, None, True),    # no level: unknown, not "no volume control"
])
def test_a_null_volume_level_says_the_device_has_no_volume_control(volume, level, available):
    parsed = hello(volume=volume)
    assert (parsed.volume_level, parsed.volume_available) == (level, available)


@pytest.mark.parametrize("volume, available", [({"level": 40, "seeded": True}, True), (NO_VOLUME, False)])
def test_a_device_without_volume_control_is_sent_no_volume(volume, available, caplog):
    async def scenario():
        store = FakeStore()
        store.configs["DEV1"] = {"duckDb": -18.0, "startupVolume": 85}
        device, link, _, sink = await _session(FIREOS_SHOW, "fireos6_crown", store, volume=volume)
        assert device.volume_available is available
        assert device.volume_gap == (None if available else VOLUME_GAP)
        assert device.volume == (40 if available else None)
        config = next(body for kind, body in link.sent if kind == "config")
        assert ("startupVolume" in config) is available
        await device.set_volume(60)
        assert (("volume_set", {"level": 60}) in link.sent) is available
        assert ("volume_set 60 refused" in caplog.text) is not available
        # A stray volume_state cannot invent a level either.
        sink.on_message(envelope("volume_state", {"level": 50}))
        await settle()
        assert device.volume == (50 if available else None)
        assert store.configs["DEV1"]["startupVolume"] == (50 if available else 85)
        await device.close()
    run(scenario())


def test_the_dashboard_shows_no_volume_level_without_volume_control(tmp_path, monkeypatch):
    pytest.importorskip("aiohttp")
    import em_api
    import em_db

    em_db.init(str(tmp_path / "t.db"))
    em_db.register_new_device("DEV1", "10.0.0.5", "v3.0.0", None)   # stored startupVolume 85
    em_db.approve_device("DEV1", "Kitchen")

    async def scenario():
        device, *_ = await _session(FIREOS_SHOW, "fireos6_crown", volume=NO_VOLUME)
        monkeypatch.setattr(em_api, "_devices", {"DEV1": device})
        out = em_api._merge_device(em_db.get_device("DEV1"))
        assert (out["volume"], out["volume_gap"]) == (None, VOLUME_GAP)
        await device.close()
    try:
        run(scenario())
    finally:
        em_db._conn.close()
        em_db._conn = None


@pytest.mark.parametrize("volume_control", [True, False])
def test_home_assistant_gets_no_volume_slider_or_level_without_volume_control(volume_control, monkeypatch):
    pytest.importorskip("zeroconf")
    import math

    import em_esphome
    from esphome.vendor import api_pb2

    async def scenario():
        satellite = await _ha_server(monkeypatch, platform=DevicePlatform.FIREOS6_CROWN,
                                     volume_control=volume_control)
        player = next(m for m in satellite.handle_message(api_pb2.ListEntitiesRequest())
                      if isinstance(m, api_pb2.ListEntitiesMediaPlayerResponse))
        volume_set = int(em_esphome.MediaPlayerEntityFeature.VOLUME_SET)
        assert bool(player.feature_flags & volume_set) is volume_control
        state = satellite._media_state_msg()
        # NaN is what HA's ESPHome integration reports as an unknown level.
        assert math.isnan(state.volume) is not volume_control
        if volume_control:
            assert state.volume == pytest.approx(0.5)
    run(scenario())
