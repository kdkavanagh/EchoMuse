"""POST /api/devices/{id}/kiosk_login (docs/echo-show-companion-plan.md §11
decision 3): the Kiosk Satellite admin password becomes an API token at the
device's own remote API, the controller keeps only the token, and a
`kiosk_plugin` device gets it in `config`. Kiosk Satellite is a fake aiohttp
server with its login endpoint's three answers (200 {token}, 401, 429)."""

import asyncio
import json
import logging
import socket

import pytest

pytest.importorskip("aiohttp")
pytest.importorskip("websockets")

from aiohttp import web  # noqa: E402
from aiohttp.test_utils import TestServer, make_mocked_request  # noqa: E402

import em_api  # noqa: E402
import em_db  # noqa: E402
from _device_fakes import ALL_V1, FakeLink, FakeStore, hello, make_hub  # noqa: E402

DEVICE = "SHOW1"
PASSWORD = "correct horse battery"
TOKEN = "ks-token-0123456789"
LINEAGE = ALL_V1 + ["screen_wake", "screen_overlay", "turn_view_v1", "timer_view_v1", "kiosk_plugin"]


class FakeKioskSatellite:
    """Kiosk Satellite's `POST /api/login` (remote_manager.dart `_login`)."""

    def __init__(self) -> None:
        self.logins: list[dict] = []
        self.throttled = False

    async def login(self, request: web.Request) -> web.Response:
        body = await request.json()
        self.logins.append(body)
        if self.throttled:
            return web.json_response({"error": "too many attempts"}, status=429)
        if body.get("password") != PASSWORD:
            return web.json_response({"error": "invalid password"}, status=401)
        return web.json_response({"token": TOKEN})


@pytest.fixture
def db(tmp_path, monkeypatch):
    em_db.init(str(tmp_path / "t.db"))
    em_db.register_new_device(DEVICE, "127.0.0.1", "fw-1", None)
    monkeypatch.setattr(em_api, "_devices", {})
    yield
    em_db._conn.close()
    em_db._conn = None


def _kiosk_port(port: int) -> None:
    em_db.set_global_device_config({**em_db.get_global_device_config_raw(), "kioskPort": port})


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def _login(monkeypatch, password: str) -> tuple[int, dict]:
    async def body(_request):
        return {"password": password}

    monkeypatch.setattr(em_api, "_json_body", body)
    request = make_mocked_request("POST", f"/api/devices/{DEVICE}/kiosk_login",
                                  match_info={"id": DEVICE}, app=web.Application())
    response = await em_api._post_kiosk_login.__wrapped__(request)
    return response.status, json.loads(response.text)


async def _kiosk() -> tuple[FakeKioskSatellite, TestServer]:
    fake = FakeKioskSatellite()
    app = web.Application()
    app.router.add_post("/api/login", fake.login)
    server = TestServer(app, host="127.0.0.1")
    await server.start_server()
    _kiosk_port(server.port)
    return fake, server


async def _online_show(monkeypatch):
    """The Show connected, approved, its session up from 127.0.0.1."""
    em_db.approve_device(DEVICE, "Kitchen")
    store = FakeStore()
    store.add(DEVICE, approved=True)
    hub, _, _ = make_hub(store)
    session_hello = hello(LINEAGE, platform="lineage18_crown")
    admitted = await hub.admit(session_hello, device_id=DEVICE, peer_ip="127.0.0.1", secure=True, token=None)
    link = FakeLink(session_hello, DEVICE)
    link.peer_ip = "127.0.0.1"
    await admitted.sink.on_ready(link)
    device = hub.devices[DEVICE]
    monkeypatch.setattr(em_api, "_devices", {DEVICE: device})
    return device, link


def test_a_login_stores_only_the_token_and_pushes_it_to_the_device(db, monkeypatch, caplog):
    caplog.set_level(logging.DEBUG)

    async def scenario():
        fake, server = await _kiosk()
        device, link = await _online_show(monkeypatch)
        link.sent.clear()
        port = server.port
        try:
            status, body = await _login(monkeypatch, PASSWORD)
        finally:
            await server.close()
        assert (status, body["pushed"]) == (200, True)
        assert fake.logins == [{"password": PASSWORD, "ttl_days": 3650}]
        assert em_db.get_device(DEVICE).kiosk_token == TOKEN
        config = next(b for kind, b in link.sent if kind == "config")
        assert config["kioskToken"] == TOKEN and config["kioskPort"] == port
        # Write-only: the dashboard learns that a token is set, never the token.
        shown = em_api._merge_device(em_db.get_device(DEVICE))
        assert shown["kioskTokenSet"] is True
        assert TOKEN not in json.dumps(shown, default=str)
        await device.close()
    asyncio.run(scenario())
    assert PASSWORD not in caplog.text and TOKEN not in caplog.text


def test_a_pending_device_logs_in_at_its_last_address_and_keeps_the_token_for_admission(db, monkeypatch):
    async def scenario():
        fake, server = await _kiosk()
        try:
            status, body = await _login(monkeypatch, PASSWORD)
        finally:
            await server.close()
        assert (status, body["pushed"]) == (200, False)
        assert em_db.get_device(DEVICE).kiosk_token == TOKEN
    asyncio.run(scenario())


def test_a_wrong_password_is_reported_and_nothing_is_stored(db, monkeypatch):
    async def scenario():
        fake, server = await _kiosk()
        try:
            status, body = await _login(monkeypatch, "hunter2")
            fake.throttled = True
            throttled, _ = await _login(monkeypatch, PASSWORD)
        finally:
            await server.close()
        # Never the API's own 401, which the dashboard reads as its session expiring.
        assert (status, body["code"]) == (400, "kiosk_password_rejected")
        assert "rejected the password" in body["error"]
        assert throttled == 429
        assert em_db.get_device(DEVICE).kiosk_token is None
    asyncio.run(scenario())


def test_an_unreachable_kiosk_satellite_names_the_prerequisite(db, monkeypatch):
    async def scenario():
        port = _free_port()
        _kiosk_port(port)
        status, body = await _login(monkeypatch, PASSWORD)
        assert (status, body["code"]) == (502, "kiosk_unreachable")
        assert f"127.0.0.1:{port}" in body["error"]
        assert "Remote Administration must be on, with a password set" in body["error"]
        assert em_db.get_device(DEVICE).kiosk_token is None
    asyncio.run(scenario())
