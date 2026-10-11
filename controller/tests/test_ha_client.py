"""HA client against an in-process stock-HA transcript replayer.

The JSON below is not invented by this fake: responses and expected requests
come from tests/fixtures/ha, whose `source` fields identify the installed HA
2026.8.1 code that defines each shape.
"""

from __future__ import annotations

import asyncio
import json
from contextlib import asynccontextmanager
from datetime import datetime
from pathlib import Path

import pytest

aiohttp = pytest.importorskip("aiohttp")
from aiohttp import web

import em_ha_client as ha_mod
from echomuse_grammar import GrammarClass
from em_endpoint_policy import recognized_completeness, recognizer_sentences
from em_ha_client import (
    FeatureStatus,
    HaClient,
    HaEndpoint,
    HaError,
    IntentEnded,
    PipelineRun,
    RunEnded,
    RunFailed,
    RunLost,
    TtsReady,
    VAD_RELAXED,
)

FIXTURE_DIR = Path(__file__).parent / "fixtures" / "ha"
FIXTURES = {p.stem: json.loads(p.read_text()) for p in FIXTURE_DIR.glob("*.json")}
# conversation/agent/homeassistant/debug as recorded, by sentence; HA recognizes nothing else (None).
RECOGNITIONS = {sentence: result
                for key, case in FIXTURES["conversation"].items() if key != "source"
                for sentence, result in zip(case["request"]["sentences"], case["result"]["results"])}
PIPELINE_ID = "01hnpec9vt52ec4dkkxqa89z46"
DEVICE_ID = "b285ec5e94b98f4c1cbeabf926be4b77"
CALENDAR_ID = "calendar.echomuse_office"


def _without_id(msg: dict) -> dict:
    return {k: v for k, v in msg.items() if k != "id"}


def _result(msg_id: int, result=None) -> dict:
    return {"id": msg_id, "type": "result", "success": True, "result": result}


def _error(msg_id: int, error: dict) -> dict:
    return {"id": msg_id, "type": "result", "success": False, "error": error}


def _event(msg_id: int, event: dict) -> dict:
    return {"id": msg_id, "type": "event", "event": event}


class FakeHA:
    """Small HA 2026.8.1 websocket/REST shape replayer."""

    def __init__(self) -> None:
        self.app = web.Application()
        self.app.router.add_get("/api/websocket", self.websocket)
        self.app.router.add_get("/api/calendars/{entity}", self.calendar_rest)
        self.app.router.add_route("*", "/api/config/script/config/{object_id}", self.script_rest)
        self.app.router.add_post("/api/config/config_entries/flow", self.flow_start)
        self.app.router.add_post("/api/config/config_entries/flow/{flow_id}", self.flow_submit)
        self.app.router.add_get("/api/config/config_entries/flow_handlers", self.flow_handlers)
        self.app.router.add_get("/api/states/{entity}", self.state_rest)
        self.app.router.add_post("/api/intent/handle", self.intent_rest)
        self.runner: web.AppRunner | None = None
        self.base_url = ""
        self.websockets: set[web.WebSocketResponse] = set()
        self.auth_messages: list[dict] = []
        self.requests: list[dict] = []
        self.rest_requests: list[tuple[str, str, object]] = []
        self.binary: list[bytes] = []
        self.subscriptions: dict[str, list[tuple[web.WebSocketResponse, int]]] = {}
        self.connection_count = 0
        self.local_calendar_found = True
        self.created_calendar = False
        self.script_missing = False
        self.script_value = FIXTURES["script_config"]["get"]["response"]
        self.fail_types: set[str] = set()
        self.fail_stt_once = False
        self._stt_fail_consumed = False
        self.timers_running = False
        self._stt: dict[web.WebSocketResponse, tuple[int, int, str]] = {}
        self._correlate: list[tuple[web.WebSocketResponse, dict]] = []

    async def start(self) -> None:
        self.runner = web.AppRunner(self.app)
        await self.runner.setup()
        site = web.TCPSite(self.runner, "127.0.0.1", 0)
        await site.start()
        port = site._server.sockets[0].getsockname()[1]
        self.base_url = f"http://127.0.0.1:{port}"

    async def close(self) -> None:
        for ws in list(self.websockets):
            await ws.close()
        if self.runner:
            await self.runner.cleanup()

    async def drop_connections(self) -> None:
        for ws in list(self.websockets):
            await ws.close()

    async def emit(self, event_type: str, data: dict) -> None:
        event = {"event_type": event_type, "data": data, "origin": "LOCAL",
                 "time_fired": "2026-09-23T11:30:00.123456+00:00",
                 "context": {"id": "01K5", "parent_id": None, "user_id": None}}
        for ws, sub_id in list(self.subscriptions.get(event_type, [])):
            if not ws.closed:
                await ws.send_json(_event(sub_id, event))

    async def websocket(self, request: web.Request) -> web.WebSocketResponse:
        ws = web.WebSocketResponse()
        await ws.prepare(request)
        self.websockets.add(ws)
        self.connection_count += 1
        await ws.send_json(FIXTURES["auth"]["auth_required"])
        auth = await ws.receive_json()
        self.auth_messages.append(auth)
        if auth.get("access_token") != "token":
            await ws.send_json(FIXTURES["auth"]["auth_invalid"])
            await ws.close()
            return ws
        await ws.send_json(FIXTURES["auth"]["auth_ok"])
        try:
            async for frame in ws:
                if frame.type == aiohttp.WSMsgType.TEXT:
                    msg = json.loads(frame.data)
                    self.requests.append(msg)
                    await self._handle_json(ws, msg)
                elif frame.type == aiohttp.WSMsgType.BINARY:
                    await self._handle_binary(ws, bytes(frame.data))
        finally:
            self.websockets.discard(ws)
            self._stt.pop(ws, None)
        return ws

    async def _handle_json(self, ws: web.WebSocketResponse, msg: dict) -> None:
        msg_id, kind = msg["id"], msg["type"]
        if kind in self.fail_types:
            await ws.send_json(_error(msg_id, {"code": "forced_failure", "message": f"{kind} failed"}))
            return
        if kind == "test/correlate":
            self._correlate.append((ws, msg))
            if len(self._correlate) == 2:
                for reply_ws, request in reversed(self._correlate):
                    await reply_ws.send_json(_result(request["id"], request["value"] * 10))
                self._correlate.clear()
            return
        if kind == "test/error":
            await ws.send_json(_error(msg_id, {"code": "bad_test", "message": "deliberate"}))
            return
        if kind == "get_config":
            await ws.send_json(_result(msg_id, FIXTURES["core"]["get_config"]["result"]))
            return
        if kind == "assist_pipeline/pipeline/list":
            await ws.send_json(_result(msg_id, FIXTURES["assist_pipeline"]["pipeline_list"]["result"]))
            return
        if kind == "assist_pipeline/run":
            await self._pipeline_run(ws, msg)
            return
        if kind == "subscribe_events":
            event_type = msg.get("event_type", "*")
            self.subscriptions.setdefault(event_type, []).append((ws, msg_id))
            await ws.send_json(_result(msg_id))
            return
        if kind == "unsubscribe_events":
            sub_id = msg["subscription"]
            for subscribers in self.subscriptions.values():
                subscribers[:] = [(sws, sid) for sws, sid in subscribers if sid != sub_id]
            await ws.send_json(_result(msg_id))
            return
        if kind == "fire_event":
            await ws.send_json(_result(msg_id, FIXTURES["core"]["fire_event"]["result"]))
            return
        if kind == "call_service":
            if msg.get("service") == "nope":
                await ws.send_json(_error(msg_id, FIXTURES["core"]["call_service_not_found"]))
            elif msg.get("return_response"):
                await ws.send_json(_result(msg_id, FIXTURES["core"]["call_service_response"]["result"]))
            else:
                await ws.send_json(_result(msg_id, FIXTURES["core"]["call_service"]["result"]))
            return
        if kind.startswith("calendar/event/"):
            await self._calendar_ws(ws, msg)
            return
        if kind == "config_entries/get":
            key = "result_found" if self.local_calendar_found or self.created_calendar else "result_empty"
            await ws.send_json(_result(msg_id, FIXTURES["local_calendar"]["config_entries_get"][key]))
            return
        if kind == "config/entity_registry/list":
            if self.created_calendar or self.local_calendar_found:
                result = FIXTURES["local_calendar"]["entity_registry_list"]["result"]
            else:
                result = FIXTURES["registries"]["entity_registry_list"]["result"]
            # The real registry contains both calendar and select entries.
            result = list(result) + list(FIXTURES["registries"]["entity_registry_list"]["result"])
            await ws.send_json(_result(msg_id, result))
            return
        if kind == "config/device_registry/list":
            await ws.send_json(_result(msg_id, FIXTURES["registries"]["device_registry_list"]["result"]))
            return
        if kind == "homeassistant/expose_entity":
            await ws.send_json(_result(msg_id))
            return
        if kind == "homeassistant/expose_entity/list":
            await ws.send_json(_result(msg_id, FIXTURES["registries"]["expose_entity_list"]["result"]))
            return
        if kind == "config/entity_registry/get_entries":
            await ws.send_json(_result(msg_id, FIXTURES["registries"]["get_entries"]["result"]))
            return
        if kind == "config/area_registry/list":
            await ws.send_json(_result(msg_id, FIXTURES["registries"]["area_registry_list"]["result"]))
            return
        if kind == "config/floor_registry/list":
            await ws.send_json(_result(msg_id, FIXTURES["registries"]["floor_registry_list"]["result"]))
            return
        if kind == "conversation/agent/homeassistant/debug":
            await ws.send_json(_result(msg_id, {"results": [RECOGNITIONS.get(s) for s in msg["sentences"]]}))
            return
        raise AssertionError(f"unhandled websocket request: {msg}")

    async def _pipeline_run(self, ws: web.WebSocketResponse, msg: dict) -> None:
        msg_id = msg["id"]
        await ws.send_json(_result(msg_id))
        if msg.get("input", {}).get("text") == "lose connection":
            await ws.close()
            return
        start = msg["start_stage"]
        if start == "stt":
            if self.fail_stt_once and not self._stt_fail_consumed:
                self._stt_fail_consumed = True
                await ws.close()
                return
            fixture = FIXTURES["assist_pipeline"]["stt_run"]
            for event in fixture["events_before_audio"]:
                await ws.send_json(_event(msg_id, event))
            self._stt[ws] = (msg_id, 1, "ok")
            return
        if start == "tts":
            events = FIXTURES["assist_pipeline"]["tts_run"]["events"]
        elif msg["input"]["text"] == "tell me a story":
            events = FIXTURES["assist_pipeline"]["intent_tts_streaming_run"]["events"]
        elif msg["input"]["text"] == "turn on the lights":
            events = FIXTURES["assist_pipeline"]["intent_tts_acknowledge_run"]["events"]
        elif msg["input"]["text"] == "make this fail":
            events = FIXTURES["assist_pipeline"]["intent_error_run"]["events"]
        else:
            events = FIXTURES["assist_pipeline"]["intent_tts_run"]["events"]
        for event in events:
            await ws.send_json(_event(msg_id, event))

    async def _handle_binary(self, ws: web.WebSocketResponse, data: bytes) -> None:
        self.binary.append(data)
        state = self._stt.get(ws)
        assert state is not None, "binary without an STT run"
        msg_id, handler, mode = state
        assert data[0] == handler
        if len(data) == 1:
            fixture = FIXTURES["assist_pipeline"]["stt_run"]
            events = fixture["events_after_audio"] if mode == "ok" else FIXTURES["assist_pipeline"]["stt_error_run"]["events_after_audio"]
            for event in events:
                await ws.send_json(_event(msg_id, event))
            self._stt.pop(ws, None)

    async def _calendar_ws(self, ws: web.WebSocketResponse, msg: dict) -> None:
        msg_id, kind = msg["id"], msg["type"]
        if kind == "calendar/event/delete" and msg["uid"] == "gone-uid":
            await ws.send_json(_error(msg_id, FIXTURES["calendar"]["delete_not_found"]))
            return
        await ws.send_json(_result(msg_id))
        if kind == "calendar/event/subscribe":
            await ws.send_json(_event(msg_id, FIXTURES["calendar"]["subscribe"]["push"]))
            await ws.send_json(_event(msg_id, FIXTURES["calendar"]["subscribe"]["push_failed"]))

    @staticmethod
    def _assert_rest_auth(request: web.Request) -> None:
        assert request.headers.get("Authorization") == "Bearer token"

    async def calendar_rest(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        self.rest_requests.append((request.method, request.path, dict(request.query)))
        return web.json_response(FIXTURES["calendar"]["rest_events"]["response"])

    async def script_rest(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        body = await request.json() if request.method == "POST" else None
        self.rest_requests.append((request.method, request.path, body))
        if request.method == "GET":
            if self.script_missing:
                return web.json_response(FIXTURES["script_config"]["get_missing"]["response"], status=404)
            return web.json_response(self.script_value)
        self.script_value = body
        return web.json_response(FIXTURES["script_config"]["post"]["response"])

    async def flow_start(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        body = await request.json()
        self.rest_requests.append((request.method, request.path, body))
        return web.json_response(FIXTURES["local_calendar"]["flow_init"]["response"])

    async def flow_submit(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        body = await request.json()
        self.rest_requests.append((request.method, request.path, body))
        self.created_calendar = True
        return web.json_response(FIXTURES["local_calendar"]["flow_submit"]["response"])

    async def flow_handlers(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        self.rest_requests.append((request.method, request.path, None))
        return web.json_response(["local_calendar", "esphome", "hue"])

    async def state_rest(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        self.rest_requests.append((request.method, request.path, None))
        return web.json_response(FIXTURES["registries"]["entity_state"]["response"])

    async def intent_rest(self, request: web.Request) -> web.Response:
        self._assert_rest_auth(request)
        body = await request.json()
        self.rest_requests.append((request.method, request.path, body))
        fixture = FIXTURES["intent"]
        if body["name"] == "HassStartTimer":
            key = "start" if body.get("device_id") else "start_unsupported"
        else:
            key = "status" if self.timers_running else "status_empty"
        return web.json_response(fixture[key]["response"])


@asynccontextmanager
async def running(fake: FakeHA | None = None):
    fake = fake or FakeHA()
    await fake.start()
    client = HaClient(HaEndpoint.from_url(fake.base_url, "token"))
    await client.start()
    await client.wait_connected(2)
    try:
        yield fake, client
    finally:
        await client.close()
        await fake.close()


def run(coro):
    return asyncio.run(coro)


async def collect(handle: PipelineRun):
    return [event async for event in handle]


async def barrier(client: HaClient) -> None:
    """One command round trip: HA's socket is ordered, so every message the
    fake sent before this reply has been dispatched when it returns."""
    await client.command({"type": "get_config"})


def test_endpoint_selection_for_direct_and_add_on_modes():
    direct = HaEndpoint.from_env({"HA_URL": "https://ha.example:8123/", "HA_TOKEN": "direct"})
    assert direct == HaEndpoint("wss://ha.example:8123/api/websocket", "https://ha.example:8123", "direct")
    addon = HaEndpoint.from_env({"SUPERVISOR_TOKEN": "supervisor"})
    assert addon == HaEndpoint(
        "ws://supervisor/core/websocket", "http://supervisor/core", "supervisor")
    assert addon.absolute("/api/tts_proxy/token.flac") == \
           "http://supervisor/core/api/tts_proxy/token.flac"


def test_auth_command_correlation_time_zone_and_errors():
    async def scenario():
        async with running() as (fake, client):
            one, two = await asyncio.gather(
                client.command({"type": "test/correlate", "value": 1}),
                client.command({"type": "test/correlate", "value": 2}),
            )
            assert (one, two) == (10, 20)
            assert await client.time_zone() == "America/Chicago"
            assert fake.auth_messages == [{"type": "auth", "access_token": "token"}]
            with pytest.raises(HaError) as raised:
                await client.command({"type": "test/error"})
            assert (raised.value.code, raised.value.message) == ("bad_test", "deliberate")
            assert client.connected
        assert not client.connected
    run(scenario())


def test_stt_binary_framing_chunking_terminator_and_transport_retry():
    async def scenario():
        async with running() as (fake, client):
            pcm = bytes((i % 251 for i in range(ha_mod.STT_CHUNK_BYTES * 2 + 6)))
            text = await client.run_stt(PIPELINE_ID, DEVICE_ID, pcm)
            assert text == "Ophelia, turn on the kitchen lights."
            chunks = fake.binary
            assert len(chunks) == 4
            assert all(len(c) <= 64 * 1024 for c in chunks)
            assert all(c[0] == 1 for c in chunks)
            assert chunks[-1] == b"\x01"
            assert b"".join(c[1:] for c in chunks[:-1]) == pcm
            assert _without_id(next(r for r in fake.requests if r["type"] == "assist_pipeline/run")) == \
                   FIXTURES["assist_pipeline"]["stt_run"]["request"]
        retry_fake = FakeHA()
        retry_fake.fail_stt_once = True
        async with running(retry_fake) as (fake, client):
            assert await client.run_stt(PIPELINE_ID, DEVICE_ID, b"\x00\x00" * 8) == \
                   "Ophelia, turn on the kitchen lights."
            assert fake.connection_count >= 2
            assert len([r for r in fake.requests if r["type"] == "assist_pipeline/run"]) == 2
    run(scenario())


def test_intent_tts_typed_events_streaming_error_and_abandonment():
    async def scenario():
        async with running() as (_fake, client):
            normal = await collect(await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "turn on the kitchen lights", None))
            assert normal == [
                IntentEnded("Turned on the kitchen lights.", "01K5ZCB000CONV", False,
                            "action_done", True),
                TtsReady(_fake.base_url + "/api/tts_proxy/Vn6uQ8.flac", False),
                RunEnded(),
            ]
            # A streamable reply is fetched only once intent-progress says HA will
            # stream it; that arrives before intent-end.
            streaming = await collect(await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "tell me a story", "old-conversation"))
            assert streaming[0] == TtsReady(_fake.base_url + "/api/tts_proxy/Vn6uQ8.flac", True)
            assert isinstance(streaming[1], IntentEnded)
            assert streaming[1].continue_conversation
            assert streaming[-1] == RunEnded()
            # Run-start announces a streamable URL but HA never starts streaming (a
            # local intent whose result HA may override with the acknowledge sound):
            # nothing is fetched before tts-end.
            acknowledged = await collect(await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "turn on the lights", None))
            assert [type(e) for e in acknowledged] == [IntentEnded, TtsReady, RunEnded]
            assert acknowledged[1] == TtsReady(_fake.base_url + "/api/tts_proxy/Qp2Lx7.flac", False)
            failed = await collect(await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "make this fail", None))
            assert failed == [RunFailed("intent-failed", "Unexpected error during intent recognition")]
            abandoned = await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "turn on the kitchen lights", None)
            abandoned.abandon()
            assert await collect(abandoned) == []
            lost = await collect(await client.run_intent_tts(
                PIPELINE_ID, DEVICE_ID, "lose connection", None))
            assert lost == [RunLost()]
    run(scenario())


def test_tts_only_and_pipeline_resolution():
    async def scenario():
        async with running() as (fake, client):
            pipelines = await client.list_pipelines()
            assert pipelines.preferred_id == PIPELINE_ID
            assert await client.resolve_pipeline("STT") == PIPELINE_ID
            assert await client.resolve_pipeline("preferred") == PIPELINE_ID
            assert await client.resolve_pipeline("renamed or absent") == PIPELINE_ID
            url = await client.run_tts(PIPELINE_ID, "Alarm set for 7 AM.", DEVICE_ID)
            assert url == fake.base_url + "/api/tts_proxy/Vn6uQ8.flac"
            sent = [r for r in fake.requests if r["type"] == "assist_pipeline/run"][-1]
            assert _without_id(sent) == FIXTURES["assist_pipeline"]["tts_run"]["request"]
    run(scenario())


def test_intents_run_for_the_speakers_device_and_an_intent_failure_raises():
    async def scenario():
        async with running() as (fake, client):
            fixture = FIXTURES["intent"]
            start = fixture["start"]["request"]["body"]
            assert await client.handle_intent(start["name"], start["data"], DEVICE_ID) == \
                fixture["start"]["response"]
            assert fake.rest_requests[-1] == ("POST", "/api/intent/handle", start)
            fake.timers_running = True
            status = await client.handle_intent("HassTimerStatus", {}, DEVICE_ID)
            assert fake.rest_requests[-1][2] == fixture["status"]["request"]["body"]
            assert [t["name"] for t in status["speech_slots"]["timers"]] == ["probe", ""]
            # HA answers a failed intent with HTTP 200 and an error response.
            with pytest.raises(HaError) as raised:
                await client.handle_intent("HassStartTimer", {"minutes": 1}, None)
            assert raised.value.code == "failed_to_handle"
            assert "does not support timers" in raised.value.message
    run(scenario())


def test_calendar_crud_subscription_and_rest_backlog():
    async def scenario():
        async with running() as (fake, client):
            fixture = FIXTURES["calendar"]
            create = fixture["create"]["request"]
            await client.calendar_create(CALENDAR_ID, create["event"])
            update = fixture["update"]["request"]
            await client.calendar_update(CALENDAR_ID, update["uid"], update["event"], update["recurrence_id"])
            assert await client.calendar_delete(
                CALENDAR_ID, fixture["delete"]["request"]["uid"], fixture["delete"]["request"]["recurrence_id"])
            assert not await client.calendar_delete(CALENDAR_ID, "gone-uid")
            pushes = []
            start = datetime.fromisoformat("2026-09-23T11:00:00+00:00")
            end = datetime.fromisoformat("2026-09-30T11:30:00+00:00")
            sub = await client.calendar_subscribe(CALENDAR_ID, start, end, pushes.append)
            await barrier(client)
            assert len(pushes) == 1  # HA's subsequent events:null fetch failure was not read as deletion
            assert set(pushes[0][1]) == set(ha_mod.CALENDAR_EVENT_KEYS)
            assert pushes[0][1]["description"] is None
            assert pushes[0][2]["all_day"] is True
            backlog = await client.calendar_events(CALENDAR_ID, start, end)
            assert backlog[0]["start"] == "2026-07-01T06:30:00-05:00"
            assert backlog[1]["start"] == "2026-07-04"
            assert backlog[1]["all_day"] is True
            await sub.unsubscribe()
            sent = [_without_id(r) for r in fake.requests]
            assert create in sent
            assert update in sent
            assert fixture["delete"]["request"] in sent
            assert fixture["subscribe"]["request"] in sent
    run(scenario())


def test_event_subscription_fire_service_response_and_select():
    async def scenario():
        async with running() as (fake, client):
            got = []
            sub = await client.subscribe_events("echomuse_alert_request", got.append)
            await fake.emit("echomuse_alert_request", {"request_id": "r1", "action": "set_alarm", "args": {}})
            await barrier(client)
            assert got == [{"request_id": "r1", "action": "set_alarm", "args": {}}]
            await client.fire_event("echomuse_alert_result", {"request_id": "r1", "result": {"ok": True}})
            response = await client.call_service(
                "script", "echomuse_list_alarms", {"speaker": "Office"}, return_response=True)
            assert response == {"ok": True, "alarms": []}
            with pytest.raises(HaError) as raised:
                await client.call_service("script", "nope")
            assert raised.value.code == "not_found"
            await client.set_select_option("select.office_finished_speaking_detection", VAD_RELAXED)
            select_msg = [_without_id(r) for r in fake.requests
                          if r["type"] == "call_service" and r["domain"] == "select"][-1]
            assert select_msg == FIXTURES["core"]["select_option"]["request"]
            await sub.unsubscribe()
    run(scenario())


def test_local_calendar_provisioning_found_and_created():
    async def scenario():
        async with running() as (_fake, client):
            assert await client.ensure_local_calendar("EchoMuse Office") == \
                   ("01K5LOCALCAL0FFICE", CALENDAR_ID)
            assert not _fake.rest_requests
        fake = FakeHA()
        fake.local_calendar_found = False
        async with running(fake) as (fake, client):
            assert await client.ensure_local_calendar("EchoMuse Office") == \
                   ("01K5LOCALCAL0FFICE", CALENDAR_ID)
            assert ("POST", "/api/config/config_entries/flow", {"handler": "local_calendar"}) \
                   in fake.rest_requests
            assert ("POST", "/api/config/config_entries/flow/4c1b9b4bd2d1",
                    {"calendar_name": "EchoMuse Office", "import": "create_empty"}) \
                   in fake.rest_requests
    run(scenario())


def test_scripts_exposure_and_satellite_selects():
    async def scenario():
        async with running() as (fake, client):
            current = await client.get_script_config("echomuse_set_alarm")
            assert current == FIXTURES["script_config"]["get"]["response"]
            new = {**current, "alias": "Changed"}
            await client.put_script_config("echomuse_set_alarm", new)
            assert await client.get_script_config("echomuse_set_alarm") == new
            fake.script_missing = True
            assert await client.get_script_config("missing") is None
            await client.expose_entities(
                ["script.echomuse_set_alarm", "script.echomuse_list_alarms"], ["conversation"])
            expose = [_without_id(r) for r in fake.requests if r["type"] == "homeassistant/expose_entity"][-1]
            assert expose == FIXTURES["registries"]["expose_entity"]["request"]
            selects = await client.satellite_entities(DEVICE_ID)
            assert selects.pipeline_select == "select.office_assistant"
            assert selects.vad_sensitivity_select == "select.office_finished_speaking_detection"
            assert await client.satellite_pipeline_id(DEVICE_ID) == PIPELINE_ID
    run(scenario())


def test_vocabulary_build_and_registry_event_refresh():
    async def scenario():
        old_debounce = ha_mod.VOCAB_DEBOUNCE_S
        ha_mod.VOCAB_DEBOUNCE_S = 0.01
        try:
            async with running() as (fake, client):
                vocab = await client.refresh_vocabulary()
                assert {e.entity_id for e in vocab.entities} == {
                    "fan.ceiling", "light.kitchen", "switch.porch"}
                assert {
                    "kitchen lights", "cooking lights", "ceiling fan", "front porch", "porch",
                    "kitchen", "cookhouse", "office", "ground floor", "downstairs",
                } <= vocab.targets()
                refreshed = []
                client.track_vocabulary(refreshed.append)
                # The immediate tracker connect refresh comes first.
                for _ in range(50):
                    if refreshed:
                        break
                    await asyncio.sleep(0.01)
                assert refreshed
                before = len(refreshed)
                await fake.emit("entity_registry_updated", {"action": "update",
                                                            "entity_id": "switch.porch", "changes": {}})
                for _ in range(50):
                    if len(refreshed) > before:
                        break
                    await asyncio.sleep(0.01)
                assert len(refreshed) > before
        finally:
            ha_mod.VOCAB_DEBOUNCE_S = old_debounce
    run(scenario())


def test_reconnect_hook_reestablishes_subscription():
    async def scenario():
        async with running() as (fake, client):
            connected = 0
            subscriptions = []
            disconnected = []

            async def on_connect():
                nonlocal connected
                connected += 1
                subscriptions.append(await client.subscribe_events("hook_event", lambda _data: None))

            client.add_connect_listener(on_connect)
            client.add_disconnect_listener(lambda: disconnected.append(True))
            await on_connect()  # listener was registered after the initial connect
            await fake.drop_connections()
            for _ in range(80):
                hook_requests = [r for r in fake.requests
                                 if r["type"] == "subscribe_events" and r["event_type"] == "hook_event"]
                if connected >= 2 and len(hook_requests) >= 2:
                    break
                await asyncio.sleep(0.05)
            assert connected >= 2
            assert fake.connection_count >= 2
            assert disconnected
            assert len([r for r in fake.requests
                        if r["type"] == "subscribe_events" and r["event_type"] == "hook_event"]) >= 2
    run(scenario())


def test_probe_reports_per_feature_status_without_cascade():
    async def scenario():
        fake = FakeHA()
        fake.fail_types.add("config/area_registry/list")
        async with running(fake) as (_fake, client):
            status = await client.probe()
            assert status["vocabulary"].ok is False
            assert "forced_failure" in status["vocabulary"].detail
            assert status == {
                "voice": FeatureStatus(True),
                "calendar": FeatureStatus(True),
                "scripts": FeatureStatus(True),
                "vocabulary": status["vocabulary"],
                "timers": FeatureStatus(True),
                "recognizer": FeatureStatus(True),
            }
    run(scenario())


@pytest.mark.parametrize("case, expected", [
    ("complete", GrammarClass.COMPLETE),              # "what time is it": nothing extends it
    ("extendable", GrammarClass.EXTENDABLE),          # "what's the weather" [{when}]
    ("extendable_short", GrammarClass.EXTENDABLE),    # "play" unpauses, but "(play) {query}" takes more
    ("needs_more", GrammarClass.NEEDS_MORE),          # "set a timer for five": minutes unfilled
    ("unknown", GrammarClass.UNKNOWN),                # "what time is": nothing recognized
])
def test_recognized_completeness_of_recorded_answers(case, expected):
    async def scenario():
        async with running() as (fake, client):
            text = FIXTURES["conversation"][case]["request"]["sentences"][0]
            said, probe = await client.recognize(recognizer_sentences(text), DEVICE_ID)
            sent = [r for r in fake.requests if r["type"] == "conversation/agent/homeassistant/debug"]
            assert _without_id(sent[-1]) == FIXTURES["conversation"][case]["request"]
            assert recognized_completeness(said, probe) == expected
    run(scenario())


def test_warm_up_sentence_is_new_each_time():
    """HA caches recognition by text: a repeated warm-up sentence would not run the matcher."""
    async def scenario():
        async with running() as (fake, client):
            await client.warm_recognizer(DEVICE_ID)
            await client.warm_recognizer(DEVICE_ID)
            sent = [r["sentences"] for r in fake.requests if r["type"] == "conversation/agent/homeassistant/debug"]
            assert len(sent) == 2 and sent[0] != sent[1]
    run(scenario())
