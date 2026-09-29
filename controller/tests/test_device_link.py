import asyncio
import json
import time
from dataclasses import dataclass
from pathlib import Path

import pytest

websockets = pytest.importorskip("websockets")

import em_audio_timeline as tl
import em_device_assets
import em_device_link as dl


class Sink:
    def __init__(self):
        self.link = None
        self.ready = asyncio.Event()
        self.messages = []
        self.audio = []
        self.lost = []
        self.fail_audio = False

    async def on_ready(self, link):
        self.link = link
        self.ready.set()

    def on_message(self, envelope):
        self.messages.append((envelope.type, envelope))

    def on_audio(self, frame):
        if self.fail_audio:
            raise tl.ProtocolError("bad_test_frame", "test rejected the frame")
        self.audio.append(frame)

    def on_lost(self, reason):
        self.lost.append(reason)


class Hub:
    def __init__(self, verdict):
        self.verdict = verdict
        self.calls = []

    async def admit(self, hello, **connection):
        self.calls.append((hello, connection))
        return self.verdict


@dataclass
class Asset:
    sha256: str
    path: Path
    size: int


class Assets:
    def __init__(self, sha, path, *, delay=0.0):
        self.sha = sha
        self.path = path
        self.delay = delay

    def resolve(self, sha):
        if self.delay:
            time.sleep(self.delay)
        if sha != self.sha:
            raise em_device_assets.AssetNotFound("not_found")
        return Asset(self.sha, self.path, self.path.stat().st_size)


def envelope(msg_type, device_id="device-1", session_id=None, body=None,
             generation=0, message_id="device-message"):
    return json.dumps({
        "protocol": 1,
        "type": msg_type,
        "session_id": session_id,
        "message_id": message_id,
        "device_id": device_id,
        "generation": generation,
        "body": {} if body is None else body,
    })


async def recv_type(ws, wanted):
    while True:
        msg = json.loads(await ws.recv())
        if msg["type"] == wanted:
            return msg


def grant(capture_permitted=True):
    return dl.ReadyGrant(
        capture_permitted=capture_permitted,
        assets=em_device_assets.SpeechAssets("r", "g", "s"),
        detector=dl.DetectorConfig(dl.DetectorThresholds(0.9, 0.65, 0.17),
                                   dl.ProvisionalDuck(-18.0)))


async def start_server(hub, assets=None, timing=dl.DEFAULT_TIMING):
    links = dl.LinkRegistry()

    async def route(ws):
        path = ws.request.path.split("?", 1)[0]
        if path == dl.CONTROL_PATH:
            await dl.serve_control(ws, secure=False, hub=hub, links=links, timing=timing)
        elif path == dl.AUDIO_PATH:
            await dl.serve_audio(ws, secure=False, links=links)
        elif path == dl.ASSETS_PATH:
            await dl.serve_assets(ws, secure=False, links=links, assets=assets)
        else:
            await ws.close()

    server = await websockets.serve(
        route, "127.0.0.1", 0, compression=None,
        process_request=links.process_request, max_size=1024 * 1024,
    )
    port = server.sockets[0].getsockname()[1]
    return server, f"ws://127.0.0.1:{port}"


async def connect_control(base, *, token="token", device_id="device-1"):
    ws = await websockets.connect(
        base + dl.CONTROL_PATH, compression=None,
        additional_headers={dl.TOKEN_HEADER: token} if token is not None else None,
    )
    hello = {
        "protocols": [1], "capabilities": ["audio_timeline_v1", "render_progress_v1"],
        "firmware_version": "test", "boot_id": "boot",
    }
    await ws.send(envelope("session.hello", device_id=device_id, body=hello))
    ready = await recv_type(ws, "session.ready")
    return ws, ready, hello


def test_admit_ready_and_reject():
    async def run():
        sink = Sink()
        hub = Hub(dl.Admitted(grant(), sink))
        server, base = await start_server(hub)
        try:
            control, ready, hello = await connect_control(base)
            await sink.ready.wait()
            assert hub.calls == [(dl.SessionHello.parse(hello), {
                "device_id": "device-1", "peer_ip": "127.0.0.1",
                "secure": False, "token": "token",
            })]
            assert ready["body"]["protocol"] == 1
            assert ready["body"]["capture_permitted"] is True
            assert ready["body"]["detector"]["provisional_duck"] == {
                "duck_db": -18.0, "max_per_window": 2, "window_ms": 5000}
            assert ready["body"]["session_id"] == sink.link.session_id
            assert ready["body"]["server_boot_id"] == dl.SERVER_BOOT_ID
            tl.parse_u64(ready["body"]["utc_ms"], "utc_ms")
            assert sink.link.hello == dl.SessionHello.parse(hello)
            assert sink.link.capabilities == frozenset(hello["capabilities"])
            await control.close()

            rejected = Hub(dl.Rejected(dl.RejectReason.PENDING_APPROVAL))
            server.close()
            await server.wait_closed()
            server, base = await start_server(rejected)
            ws = await websockets.connect(base + dl.CONTROL_PATH, compression=None)
            await ws.send(envelope(
                "session.hello", body={"protocols": [1], "capabilities": []}))
            msg = await recv_type(ws, "session.rejected")
            assert msg["body"] == {"reason": "pending_approval"}
            await ws.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_heartbeat_degraded_and_session_lost():
    async def run():
        sink = Sink()
        timing = dl.Timing(heartbeat_s=0.01, degraded_s=0.04, lost_s=0.08,
                           hello_timeout_s=1.0, audio_attach_s=1.0)
        server, base = await start_server(
            Hub(dl.Admitted(grant(), sink)), timing=timing)
        try:
            control, _, _ = await connect_control(base)
            await sink.ready.wait()
            heartbeat = await recv_type(control, "heartbeat")
            tl.parse_u64(heartbeat["body"]["mono_ns"], "mono_ns")
            await asyncio.sleep(0.05)
            assert sink.link.degraded is True
            await asyncio.wait_for(control.wait_closed(), 0.2)
            assert sink.lost == ["session_lost"]
            assert sink.link.closed
        finally:
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_request_ack_correlation_and_clock_reply():
    async def run():
        sink = Sink()
        server, base = await start_server(Hub(dl.Admitted(grant(), sink)))
        try:
            control, ready, _ = await connect_control(base)
            await sink.ready.wait()
            session_id = ready["body"]["session_id"]
            pending = asyncio.create_task(
                sink.link.request(dl.MessageType.FOCUS_ACQUIRE, {"lease_id": "lease"}, generation=4))
            command = await recv_type(control, "focus.acquire")
            await control.send(envelope(
                "command.ack", session_id=session_id,
                body={"message_id": command["message_id"], "status": "accepted", "error": None},
            ))
            assert await pending == dl.CommandAck(command["message_id"], dl.AckStatus.ACCEPTED, None)
            assert sink.messages == []

            await control.send(envelope(
                "command.ack", session_id=session_id, message_id="unsolicited-envelope",
                body={"message_id": "other", "status": "applied", "error": None},
            ))
            for _ in range(100):
                if sink.messages:
                    break
                await asyncio.sleep(0.005)
            assert [msg_type for msg_type, _ in sink.messages] == ["command.ack"]

            await control.send(envelope(
                "clock.request", session_id=session_id, body={"nonce": "clock-nonce"},
            ))
            reply = await recv_type(control, "clock.reply")
            assert reply["body"]["nonce"] == "clock-nonce"
            tl.parse_u64(reply["body"]["utc_ms"], "utc_ms")
            # WIRE uint64 values are decimal strings, never JSON numbers.
            await control.send(envelope(
                "heartbeat", session_id=session_id, body={"mono_ns": 7}))
            error = await recv_type(control, "protocol.error")
            assert error["body"]["code"] == "malformed_message"
            await asyncio.wait_for(control.wait_closed(), 1.0)
            assert sink.lost == ["protocol"]
        finally:
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_audio_binding_and_protocol_error():
    async def run():
        sink = Sink()
        server, base = await start_server(Hub(dl.Admitted(grant(), sink)))
        try:
            control, ready, _ = await connect_control(base)
            await sink.ready.wait()
            session_id = ready["body"]["session_id"]
            with pytest.raises(websockets.InvalidStatus) as rejected:
                await websockets.connect(
                    base + dl.AUDIO_PATH, compression=None,
                    additional_headers={dl.SESSION_HEADER: "stale", dl.TOKEN_HEADER: "token"},
                )
            assert rejected.value.response.status_code == 403

            audio = await websockets.connect(
                base + dl.AUDIO_PATH, compression=None,
                additional_headers={dl.SESSION_HEADER: session_id, dl.TOKEN_HEADER: "token"},
            )
            await audio.send(b"accepted")
            for _ in range(20):
                if sink.audio:
                    break
                await asyncio.sleep(0.005)
            assert sink.audio == [b"accepted"]

            sink.fail_audio = True
            await audio.send(b"rejected")
            error = await recv_type(control, "protocol.error")
            assert error["body"] == {
                "code": "bad_test_frame", "detail": "test rejected the frame",
            }
            await asyncio.wait_for(control.wait_closed(), 1.0)
            assert sink.lost == ["protocol"]
        finally:
            server.close()
            await server.wait_closed()

    asyncio.run(run())


def test_assets_resume_not_found_and_second_request_closes(tmp_path):
    async def run():
        data = bytes(range(256)) * 400
        path = tmp_path / "asset.bin"
        path.write_bytes(data)
        sha = "a" * 64
        assets = Assets(sha, path)
        sink = Sink()
        server, base = await start_server(Hub(dl.Admitted(grant(), sink)), assets)
        try:
            control, ready, _ = await connect_control(base)
            session_id = ready["body"]["session_id"]
            headers = {dl.SESSION_HEADER: session_id, dl.TOKEN_HEADER: "token"}
            ws = await websockets.connect(
                base + dl.ASSETS_PATH, compression=None, additional_headers=headers)
            offset = 65_530
            await ws.send(json.dumps({"sha256": sha, "offset": offset}))
            chunks = []
            while True:
                reply = await ws.recv()
                if isinstance(reply, str):
                    done = json.loads(reply)
                    break
                chunks.append(reply)
            assert b"".join(chunks) == data[offset:]
            assert all(len(chunk) <= em_device_assets.MAX_CHUNK_BYTES for chunk in chunks)
            assert done == {"sha256": sha, "size": len(data), "done": True}

            await ws.send(json.dumps({"sha256": "b" * 64, "offset": 0}))
            assert json.loads(await ws.recv()) == {"error": "not_found"}
            await ws.close()

            assets.delay = 0.1
            ws = await websockets.connect(
                base + dl.ASSETS_PATH, compression=None, additional_headers=headers)
            await ws.send(json.dumps({"sha256": sha, "offset": 0}))
            await ws.send(json.dumps({"sha256": sha, "offset": 1}))
            await asyncio.wait_for(ws.wait_closed(), 1.0)
            assert ws.close_code is not None
            assert not sink.link.closed       # assets reconnects do not end the session
            await control.close()
        finally:
            server.close()
            await server.wait_closed()

    asyncio.run(run())
