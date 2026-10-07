"""em_legacy (SPEC §12): an old device is registered as upgrade_required, gets
`ack`/`pending`, can be sent shell_open/shell_close, and nothing it reports
has any effect."""

import asyncio
import json

import pytest

pytest.importorskip("websockets")

import em_device  # noqa: E402
import em_legacy  # noqa: E402
from _device_fakes import FakeStore, make_hub, run, settle  # noqa: E402


class FakeWs:
    def __init__(self, *messages: str) -> None:
        self.remote_address = ("10.0.0.9", 5555)
        self.request = type("Req", (), {"headers": {}})()
        self.inbox: asyncio.Queue = asyncio.Queue()
        for m in messages:
            self.inbox.put_nowait(m)
        self.sent: list[dict] = []
        self.closed = False

    async def recv(self):
        return await self.inbox.get()

    async def send(self, text: str) -> None:
        self.sent.append(json.loads(text))

    async def close(self) -> None:
        self.closed = True
        self.inbox.put_nowait(None)

    def __aiter__(self):
        return self

    async def __anext__(self):
        item = await self.inbox.get()
        if item is None:
            raise StopAsyncIteration
        return item


REGISTER = json.dumps({"type": "register", "device_id": "OLD1", "version": "v2.9.9",
                       "ip": "10.0.0.9", "capabilities": ["mic", "speaker", "leds", "oww_shadow"]})


def test_old_device_is_acked_marked_upgrade_required_and_ignored():
    async def scenario():
        store = FakeStore()
        store.add("OLD1", approved=True, label="Kitchen")
        hub, host, _ = make_hub(store)
        ws = FakeWs(REGISTER)
        task = asyncio.create_task(em_legacy.serve_control(ws, secure=False, hub=hub))
        await settle()
        device = hub.devices["OLD1"]
        assert ws.sent == [{"type": "ack", "device_id": "OLD1"}]
        assert device.upgrade_required and device.online and device.link is None
        assert device.firmware_version == "v2.9.9"
        assert "alert_cache_v1" in device.missing_capabilities
        # Pre-v1 firmware predates Fire OS 6: the firmware update and debloat
        # it can still receive are Fire OS 5's.
        assert device.platform is em_device.DevicePlatform.FIREOS5

        for report in ({"type": "button", "clickType": 138, "down": False},
                       {"type": "volume_state", "level": 10}, {"type": "stats", "cpuPct": 5}):
            ws.inbox.put_nowait(json.dumps(report))
        await settle()
        assert device.actor.messages == [] and device.actor.turns == []
        assert device.volume is None and device.stats is None and host.alerts.calls == []

        await device.send("shell_open", {"pty": True})
        assert ws.sent[-1] == {"type": "shell_open", "pty": True}
        with pytest.raises(em_device.em_device_link.LinkClosed):
            await device.send("leds", {"leds": []})

        await ws.close()
        await task
        assert not device.online and ("disconnected", "OLD1") in host.events
        await device.close()
    run(scenario())


def test_unapproved_old_device_gets_pending_and_is_closed():
    async def scenario():
        hub, host, store = make_hub(FakeStore(approval="strict"))
        ws = FakeWs(REGISTER)
        await em_legacy.serve_control(ws, secure=False, hub=hub)
        assert ws.sent == [{"type": "pending"}] and ws.closed
        assert store.registered == ["OLD1"] and "OLD1" not in hub.devices
    run(scenario())


@pytest.mark.parametrize("first", [json.dumps({"type": "hello"}), "not json",
                                   json.dumps({"type": "register", "device_id": ""})])
def test_anything_but_a_valid_register_is_closed(first):
    async def scenario():
        hub, _, _ = make_hub()
        ws = FakeWs(first)
        await em_legacy.serve_control(ws, secure=False, hub=hub)
        assert ws.closed and ws.sent == [] and hub.devices == {}
    run(scenario())
