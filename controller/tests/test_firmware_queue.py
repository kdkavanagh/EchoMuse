"""An install of the bundled firmware queued for a device's next connect.

An offline Dot cannot be updated, so the dashboard queues the install
(POST /api/devices/{id}/update/queue). It is a database row, so it outlives
the controller, and the connect path acts on it against whatever firmware is
bundled at that moment: install, or clear it when the Dot is already current.
The install itself is replaced by a recorder; nothing here reaches a device.
"""

import asyncio
import json
from types import SimpleNamespace

import pytest

pytest.importorskip("aiohttp")
pytest.importorskip("websockets")

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import make_mocked_request  # noqa: E402

import em_api  # noqa: E402
import em_db  # noqa: E402

DEVICE = "OFFICE"
_real_sleep = asyncio.sleep


def _firmware(version: str) -> SimpleNamespace:
    return SimpleNamespace(version=version)


def _live(version: str) -> SimpleNamespace:
    """A connected device as em_api reads one: online and its reported version."""
    return SimpleNamespace(online=True, firmware_version=version)


@pytest.fixture
def fleet(tmp_path, monkeypatch):
    path = str(tmp_path / "t.db")
    em_db.init(path)
    em_db.register_new_device(DEVICE, "10.0.0.9", "fw-old", None)
    em_db.approve_device(DEVICE, "Office")

    devices: dict[str, SimpleNamespace] = {}
    monkeypatch.setattr(em_api, "_devices", devices)
    installs: list[tuple[str, str]] = []

    async def record_install(shell, device_id, firmware):
        installs.append((device_id, firmware.version))
        em_api._updates_in_progress.discard(device_id)

    monkeypatch.setattr(em_api, "_run_update", record_install)

    async def no_wait(_seconds):
        return None

    monkeypatch.setattr(em_api.asyncio, "sleep", no_wait)

    state = SimpleNamespace(path=path, devices=devices, installs=installs,
                            firmware=_firmware("fw-new"))
    yield state
    em_api._updates_in_progress.clear()
    if em_db._conn is not None:
        em_db._conn.close()


async def _drain() -> None:
    """Run every background task em_api spawned, and the ones they spawn."""
    while em_api._background_tasks:
        await asyncio.gather(*list(em_api._background_tasks))
        await _real_sleep(0)


async def _call(handler, method: str, fleet) -> tuple[int, dict]:
    app = web.Application()
    app[em_api.SERVICES] = SimpleNamespace(firmware=lambda: fleet.firmware, shell=None)
    request = make_mocked_request(method, f"/api/devices/{DEVICE}/update/queue",
                                  match_info={"id": DEVICE}, app=app)
    response = await handler.__wrapped__(request)
    return response.status, json.loads(response.text)


async def _queue(fleet) -> tuple[int, dict]:
    return await _call(em_api._post_device_update_queue, "POST", fleet)


async def _cancel(fleet) -> tuple[int, dict]:
    return await _call(em_api._delete_device_update_queue, "DELETE", fleet)


async def _connect(fleet, live: SimpleNamespace) -> None:
    fleet.devices[DEVICE] = live
    await em_api.notify_device_connected(None, DEVICE, live.firmware_version,
                                         firmware=fleet.firmware)
    await _drain()


def _queued_at() -> int | None:
    row = em_db.get_device(DEVICE)
    assert row is not None
    return row.update_queued_at


def test_queued_on_an_offline_device_installs_when_it_connects(fleet):
    async def scenario():
        status, body = await _queue(fleet)
        assert (status, body["status"], body["version"]) == (202, "queued", "fw-new")
        assert _queued_at() == body["queued_at"]
        assert fleet.installs == []                     # nothing to reach yet

        await _connect(fleet, _live("fw-old"))
        assert fleet.installs == [(DEVICE, "fw-new")]
        assert _queued_at() is None                     # ran once, then cleared

        await _connect(fleet, _live("fw-old"))          # e.g. back after an auto-rollback
        assert fleet.installs == [(DEVICE, "fw-new")], "a cleared queue never re-runs"
    asyncio.run(scenario())


def test_the_queue_installs_what_is_bundled_at_connect_not_at_queue_time(fleet):
    async def scenario():
        await _queue(fleet)
        fleet.firmware = _firmware("fw-newer")          # controller updated meanwhile
        await _connect(fleet, _live("fw-old"))
        assert fleet.installs == [(DEVICE, "fw-newer")]
    asyncio.run(scenario())


def test_a_device_already_current_on_connect_is_cleared_without_an_install(fleet):
    async def scenario():
        await _queue(fleet)
        await _connect(fleet, _live("fw-new"))          # updated some other way
        assert fleet.installs == []
        assert _queued_at() is None
    asyncio.run(scenario())


def test_a_cancelled_queue_does_not_run(fleet):
    async def scenario():
        await _queue(fleet)
        assert await _cancel(fleet) == (200, {"cancelled": True})
        assert _queued_at() is None
        await _connect(fleet, _live("fw-old"))
        assert fleet.installs == []
        assert await _cancel(fleet) == (200, {"cancelled": False})
    asyncio.run(scenario())


def test_the_queue_survives_a_fresh_database_handle(fleet):
    async def scenario():
        _, body = await _queue(fleet)
        em_db._conn.close()
        em_db.init(fleet.path)                           # a controller restart
        assert _queued_at() == body["queued_at"]
        await _connect(fleet, _live("fw-old"))
        assert fleet.installs == [(DEVICE, "fw-new")]
    asyncio.run(scenario())


def test_a_device_online_when_queued_installs_now(fleet):
    async def scenario():
        fleet.devices[DEVICE] = _live("fw-old")
        status, body = await _queue(fleet)
        await _drain()
        assert (status, body["status"], body["queued_at"]) == (202, "started", None)
        assert fleet.installs == [(DEVICE, "fw-new")]
        assert _queued_at() is None
    asyncio.run(scenario())


def test_a_device_on_the_bundled_firmware_or_unapproved_cannot_be_queued(fleet):
    async def scenario():
        fleet.firmware = _firmware("fw-old")             # what it last reported
        status, body = await _queue(fleet)
        assert (status, body["code"]) == (409, "already_current")

        em_db.register_new_device("PENDING", "10.0.0.10", "fw-ancient", None)
        app = web.Application()
        app[em_api.SERVICES] = SimpleNamespace(firmware=lambda: fleet.firmware, shell=None)
        request = make_mocked_request("POST", "/api/devices/PENDING/update/queue",
                                      match_info={"id": "PENDING"}, app=app)
        response = await em_api._post_device_update_queue.__wrapped__(request)
        assert (response.status, json.loads(response.text)["code"]) == (409, "not_approved")
        assert _queued_at() is None
    asyncio.run(scenario())


def test_fleet_deploy_reports_a_device_offline_since_the_controller_started(fleet):
    """It has no live entry at all — only its row — and must still be listed,
    or the dashboard has nothing to offer the queue on."""
    async def scenario():
        app = web.Application()
        app[em_api.SERVICES] = SimpleNamespace(firmware=lambda: fleet.firmware, shell=None)
        request = make_mocked_request("POST", "/api/firmware/deploy", app=app)
        response = await em_api._post_deploy_firmware.__wrapped__(request)
        body = json.loads(response.text)
        assert body["started"] == []
        assert body["skipped"] == [{"device_id": DEVICE, "reason": "offline"}]
    asyncio.run(scenario())
