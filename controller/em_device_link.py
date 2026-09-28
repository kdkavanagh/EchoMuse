"""Controller end of the device link: three WebSockets per session.

SPEC §4.4 and §16.1; field spellings are the WIRE (`docs/protocol-v1.md`
§1, §2, §4.1, §6). Core's listener dispatches the three paths to
`serve_control`, `serve_audio`, and `serve_assets`, passes `process_request`
and `compression=None` to `websockets.serve`, and supplies the `LinkHub`
that admits sessions.

Invariants:
- One live `DeviceLink` per device and per session ID. A newly admitted
  hello for a device closes its previous link before the new `on_ready`.
- Audio and assets sockets bind to the device's current session by
  `X-EM-Session` and must present the control socket's `X-EM-Token` over the
  same listener (TLS or plain); anything else is refused.
- `LinkSink.on_lost` is called exactly once per link, after which every
  send raises `LinkClosed` and every pending `request` fails with it.
- Control and audio are separate sockets: audio backpressure never delays a
  control send.
"""

from __future__ import annotations

import asyncio
import hmac
import http
import json
import logging
import time
import uuid
from dataclasses import dataclass
from typing import Any, Protocol

import websockets

import em_audio_timeline as tl
import em_device_assets

log = logging.getLogger("em_device_link")

PROTOCOL = 1
CONTROL_PATH = "/device/v1/control"
AUDIO_PATH = "/device/v1/audio"
ASSETS_PATH = "/device/v1/assets"
TOKEN_HEADER = "X-EM-Token"
SESSION_HEADER = "X-EM-Session"
MAX_CONTROL_BYTES = 256 * 1024            # §16.1 control frame limit
MAX_ASSET_CHUNK = em_device_assets.MAX_CHUNK_BYTES   # WIRE §6: ≤64 KiB
REJECT_REASONS = ("pending_approval", "unauthorized", "protocol")
ACK_STATUSES = ("accepted", "applied", "durable", "rejected")

# Identifies this controller process in `session.ready` (§16.1).
SERVER_BOOT_ID = str(uuid.uuid4())

# Link-internal message types the sink never sees.
_LINK_TYPES = frozenset({"heartbeat", "clock.request"})


@dataclass(frozen=True)
class Timing:
    """Link timers in seconds (§16.1). Tests shorten them."""

    heartbeat_s: float = 1.0
    degraded_s: float = 2.0
    lost_s: float = 3.0
    hello_timeout_s: float = 10.0       # WIRE §1: the device waits ≤10 s for ready
    audio_attach_s: float = 10.0        # send_audio waits this long for the audio socket


DEFAULT_TIMING = Timing()


class LinkClosed(Exception):
    """The session is over; nothing more can be sent on it."""


@dataclass(frozen=True)
class Rejected:
    reason: str                          # pending_approval | unauthorized | protocol


@dataclass(frozen=True)
class Admitted:
    ready: dict                          # session.ready body minus the link-filled keys
    sink: "LinkSink"


class LinkHub(Protocol):
    async def admit(self, hello: dict, *, device_id: str, peer_ip: str, secure: bool,
                    token: str | None) -> Admitted | Rejected: ...


class LinkSink(Protocol):
    async def on_ready(self, link: "DeviceLink") -> None: ...
    def on_message(self, msg_type: str, envelope: dict) -> None: ...
    def on_audio(self, frame: bytes) -> None: ...
    def on_lost(self, reason: str) -> None: ...


# ---------------------------------------------------------------------------
# Envelopes (WIRE §2)
# ---------------------------------------------------------------------------

def _is_count(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


# WIRE §4 uint64 fields (decimal strings) of D→C bodies, by message type.
# A path step "*" walks every list item / dict value. Absent or null values
# are the owning module's concern; present ones must be decimal strings.
_U64_PATHS: dict[str, tuple[tuple[str, ...], ...]] = {
    "heartbeat": (("mono_ns",),),
    "stream.open": (("epoch",),),
    "stream.end": (("epoch",), ("final_sample",)),
    "render.progress": (("submitted_frames",), ("completed_frames",), ("mono_ns",),
                        ("seek_frame",), ("missing_from",), ("missing_to",)),
    "render.finished": (("last_completed_frame",),),
    "wake.candidate": (("capture_epoch",), ("first_crossing_end",), ("support_start",),
                       ("mono_ns",), ("hops", "*", "end_sample")),
    "wake.candidate_end": (("support_end",),),
    "wake.stats": (("near_misses", "*", "mono_ns"),),
    "uplink.ended": (("last_sample", "*"), ("clipped_start", "*")),
    "privacy.changed": (("capture_epoch",),),
    "button.action": (("mono_ns",), ("capture_epoch",), ("capture_sample",)),
}


def _walk(value: object, path: tuple[str, ...], name: str):
    if not path:
        yield name, value
        return
    step, rest = path[0], path[1:]
    if step == "*":
        items = (value.items() if isinstance(value, dict)
                 else enumerate(value) if isinstance(value, list) else ())
        for key, child in items:
            yield from _walk(child, rest, f"{name}[{key}]")
    elif isinstance(value, dict) and step in value:
        yield from _walk(value[step], rest, f"{name}.{step}")


def _validate_u64_fields(msg_type: str, body: dict) -> None:
    for path in _U64_PATHS.get(msg_type, ()):
        for name, value in _walk(body, path, msg_type):
            if value is not None:
                tl.parse_u64(value, name)


def _validate_link_body(msg_type: str, body: dict) -> None:
    if msg_type == "heartbeat" and "mono_ns" not in body:
        raise tl.ProtocolError("malformed_message", "heartbeat.mono_ns is required")
    if msg_type == "clock.request":
        if not isinstance(body.get("nonce"), str) or not body["nonce"]:
            raise tl.ProtocolError("malformed_message", "clock.request.nonce must be a string")
    if msg_type == "command.ack":
        if not isinstance(body.get("message_id"), str) or not body["message_id"]:
            raise tl.ProtocolError("malformed_message", "command.ack.message_id must be a string")
        if body.get("status") not in ACK_STATUSES:
            raise tl.ProtocolError("malformed_message", "invalid command.ack.status")
        if body.get("error") is not None and not isinstance(body["error"], str):
            raise tl.ProtocolError("malformed_message", "command.ack.error must be a string or null")


def parse_envelope(raw: str | bytes) -> dict:
    """Decode and validate one D→C control frame. Raises `ProtocolError`.

    Session and device identity are checked by the caller, which knows them.
    """
    if isinstance(raw, bytes):
        raise tl.ProtocolError("malformed_message", "control frames are text")
    if len(raw.encode("utf-8")) > MAX_CONTROL_BYTES:
        raise tl.ProtocolError("oversize", f"control frame exceeds {MAX_CONTROL_BYTES} bytes")
    try:
        env = json.loads(raw)
    except ValueError as err:
        raise tl.ProtocolError("malformed_message", f"invalid JSON: {err}") from None
    if not isinstance(env, dict):
        raise tl.ProtocolError("malformed_message", "envelope is not an object")
    if env.get("protocol") != PROTOCOL or isinstance(env.get("protocol"), bool):
        raise tl.ProtocolError("unsupported_protocol", f"protocol {env.get('protocol')!r}")
    for key in ("type", "message_id", "device_id"):
        if not isinstance(env.get(key), str) or not env[key]:
            raise tl.ProtocolError("malformed_message", f"{key} must be a non-empty string")
    sid = env.get("session_id")
    if sid is not None and not isinstance(sid, str):
        raise tl.ProtocolError("malformed_message", "session_id must be a string or null")
    if not _is_count(env.get("generation")) or env["generation"] > tl.U32_MAX:
        raise tl.ProtocolError("malformed_message", "generation must be a uint32")
    body = env.get("body")
    if not isinstance(body, dict):
        raise tl.ProtocolError("malformed_message", "body must be an object")
    _validate_u64_fields(env["type"], body)
    _validate_link_body(env["type"], body)
    return env


def _encode(msg_type: str, body: dict, *, session_id: str | None, device_id: str,
            generation: int) -> tuple[str, str]:
    if not _is_count(generation) or generation > tl.U32_MAX:
        raise ValueError("generation must be a uint32")
    if not isinstance(body, dict):
        raise TypeError("body must be a dict")
    message_id = str(uuid.uuid4())
    text = json.dumps({
        "protocol": PROTOCOL,
        "type": msg_type,
        "session_id": session_id,
        "message_id": message_id,
        "device_id": device_id,
        "generation": generation,
        "body": body,
    }, separators=(",", ":"))
    if len(text.encode("utf-8")) > MAX_CONTROL_BYTES:
        raise ValueError(f"{msg_type} exceeds the {MAX_CONTROL_BYTES}-byte control limit")
    return message_id, text


def _utc_ms() -> str:
    return tl.format_u64(time.time_ns() // 1_000_000)


def _mono_ns() -> str:
    return tl.format_u64(time.monotonic_ns())


# ---------------------------------------------------------------------------
# Session registry
# ---------------------------------------------------------------------------

_by_device: dict[str, "DeviceLink"] = {}
_by_session: dict[str, "DeviceLink"] = {}


def current(device_id: str) -> "DeviceLink | None":
    """The device's live link, if any."""
    return _by_device.get(device_id)


def _header(headers: Any, name: str) -> str | None:
    value = headers.get(name)
    return value or None


def _binding(headers: Any) -> "DeviceLink | None":
    """The live link an audio/assets upgrade may bind to, or None."""
    sid = _header(headers, SESSION_HEADER)
    link = _by_session.get(sid) if sid else None
    if link is None or link.closed:
        return None
    presented = _header(headers, TOKEN_HEADER)
    if presented is None or link._token is None:
        return link if presented == link._token else None
    return link if hmac.compare_digest(presented, link._token) else None


def _path(request: Any) -> str:
    return request.path.split("?", 1)[0]


def process_request(connection: Any, request: Any) -> Any:
    """`websockets.serve(process_request=…)` hook: HTTP 403 for an audio or
    assets upgrade not bound to a live session (WIRE §1); None otherwise."""
    if _path(request) not in (AUDIO_PATH, ASSETS_PATH):
        return None
    if _binding(request.headers) is None:
        return connection.respond(http.HTTPStatus.FORBIDDEN, "not the current session\n")
    return None


# ---------------------------------------------------------------------------
# The link
# ---------------------------------------------------------------------------

class DeviceLink:
    """One device session: the control socket plus its audio/assets sockets."""

    def __init__(self, ws: Any, *, device_id: str, session_id: str, hello: dict,
                 peer_ip: str, secure: bool, token: str | None, sink: LinkSink,
                 timing: Timing):
        self.device_id = device_id
        self.session_id = session_id
        self.hello = hello
        caps = hello.get("capabilities")
        self.capabilities: frozenset[str] = frozenset(
            c for c in caps if isinstance(c, str)) if isinstance(caps, list) else frozenset()
        self.peer_ip = peer_ip
        self.secure = secure
        self.degraded = False
        self.closed = False
        self._token = token
        self._sink = sink
        self._timing = timing
        self._control = ws
        self._audio: Any = None
        self._assets: Any = None
        self._audio_attached = asyncio.Event()
        self._waiters: dict[str, asyncio.Future[dict]] = {}
        self._ready_done = False
        self._queued: list[tuple[str, dict]] = []
        self._tasks: list[asyncio.Task] = []           # heartbeat + watchdog
        self._ready_task: asyncio.Task | None = None   # on_ready; never cancelled
        self._last_rx = asyncio.get_running_loop().time()

    def __repr__(self) -> str:
        return f"<DeviceLink {self.device_id} session={self.session_id}>"

    # -- sending ------------------------------------------------------------

    async def _send_text(self, text: str) -> None:
        try:
            await self._control.send(text)
        except websockets.ConnectionClosed:
            raise LinkClosed(f"{self.device_id}: control socket closed") from None

    async def send(self, msg_type: str, body: dict, *, generation: int = 0) -> str:
        """Send one C→D envelope; returns its message_id."""
        if self.closed:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        message_id, text = _encode(msg_type, body, session_id=self.session_id,
                                   device_id=self.device_id, generation=generation)
        await self._send_text(text)
        return message_id

    async def request(self, msg_type: str, body: dict, *, generation: int = 0,
                      timeout: float = 2.0) -> dict:
        """Send and await the device's `command.ack` body. Raises
        `TimeoutError` when no ack arrives, `LinkClosed` on session loss."""
        if self.closed:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        message_id, text = _encode(msg_type, body, session_id=self.session_id,
                                   device_id=self.device_id, generation=generation)
        fut: asyncio.Future[dict] = asyncio.get_running_loop().create_future()
        self._waiters[message_id] = fut
        try:
            await self._send_text(text)
            async with asyncio.timeout(timeout):
                return await fut
        finally:
            self._waiters.pop(message_id, None)

    async def ack(self, message_id: str, status: str, error: str | None = None) -> None:
        if status not in ACK_STATUSES:
            raise ValueError(f"invalid ack status {status!r}")
        await self.send("command.ack", {"message_id": message_id, "status": status, "error": error})

    async def send_audio(self, frame: bytes) -> None:
        """Send one kind-3 EMA1 frame, awaiting the audio socket's drain."""
        if self.closed:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        if self._audio is None:
            try:
                async with asyncio.timeout(self._timing.audio_attach_s):
                    await self._audio_attached.wait()
            except TimeoutError:
                raise LinkClosed(f"{self.device_id}: audio socket never opened") from None
            if self.closed or self._audio is None:
                raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        try:
            await self._audio.send(frame)
        except websockets.ConnectionClosed:
            raise LinkClosed(f"{self.device_id}: audio socket closed") from None

    async def protocol_error(self, code: str, detail: str) -> None:
        """Report a protocol violation and end the session (§16.1)."""
        log.warning(f"[link] {self.device_id}: protocol error {code}: {detail}")
        try:
            await self.send("protocol.error", {"code": code, "detail": detail})
        except LinkClosed:
            pass
        await self.close("protocol")

    # -- lifecycle ----------------------------------------------------------

    async def close(self, reason: str) -> None:
        """End the session: all three sockets close, `on_lost(reason)` once."""
        if self.closed:
            return
        self.closed = True
        self._audio_attached.set()
        if _by_device.get(self.device_id) is self:
            del _by_device[self.device_id]
        _by_session.pop(self.session_id, None)
        me = asyncio.current_task()
        tasks = [task for task in (*self._tasks, self._ready_task)
                 if task is not None and task is not me]
        for task in tasks:
            task.cancel()
        for fut in self._waiters.values():
            if not fut.done():
                fut.set_exception(LinkClosed(f"{self.device_id}: session ended ({reason})"))
        await asyncio.gather(*tasks, return_exceptions=True)
        self._waiters.clear()
        log.info(f"[link] {self.device_id}: session {self.session_id} ended ({reason})")
        try:
            self._sink.on_lost(reason)
        except Exception:
            log.exception(f"[link] {self.device_id}: on_lost failed")
        socks = [s for s in (self._control, self._audio, self._assets) if s is not None]
        await asyncio.gather(*(s.close() for s in socks), return_exceptions=True)

    def _start(self) -> None:
        self._tasks.append(asyncio.create_task(self._heartbeat_loop(), name=f"hb-{self.device_id}"))
        self._tasks.append(asyncio.create_task(self._watchdog_loop(), name=f"wd-{self.device_id}"))

    async def _heartbeat_loop(self) -> None:
        while not self.closed:
            try:
                await self.send("heartbeat", {"mono_ns": _mono_ns()})
            except LinkClosed:
                return
            await asyncio.sleep(self._timing.heartbeat_s)

    async def _watchdog_loop(self) -> None:
        """2 s without a control message = degraded; 3 s = session lost."""
        loop = asyncio.get_running_loop()
        t = self._timing
        while not self.closed:
            idle = loop.time() - self._last_rx
            if idle >= t.lost_s:
                await self.close("session_lost")
                return
            if idle >= t.degraded_s:
                if not self.degraded:
                    log.info(f"[link] {self.device_id}: degraded ({idle:.1f} s silent)")
                self.degraded = True
                await asyncio.sleep(t.lost_s - idle)
            else:
                await asyncio.sleep(t.degraded_s - idle)

    async def _run_ready(self) -> None:
        try:
            await self._sink.on_ready(self)
        except LinkClosed:
            return
        except Exception:
            log.exception(f"[link] {self.device_id}: on_ready failed")
            await self.close("closed")
            return
        queued, self._queued = self._queued, []
        self._ready_done = True
        for msg_type, env in queued:
            self._deliver(msg_type, env)

    # -- receiving ----------------------------------------------------------

    def _deliver(self, msg_type: str, env: dict) -> None:
        if self.closed:
            return
        if not self._ready_done:
            self._queued.append((msg_type, env))
            return
        try:
            self._sink.on_message(msg_type, env)
        except Exception:
            log.exception(f"[link] {self.device_id}: on_message({msg_type}) failed")

    async def _on_control(self, raw: str | bytes) -> None:
        try:
            env = parse_envelope(raw)
            if env["session_id"] != self.session_id:
                raise tl.ProtocolError("wrong_session", f"session_id {env['session_id']!r}")
            if env["device_id"] != self.device_id:
                raise tl.ProtocolError("wrong_device", f"device_id {env['device_id']!r}")
            if env["type"] == "session.hello":
                raise tl.ProtocolError("unexpected_message", "session.hello on a live session")
        except tl.ProtocolError as err:
            await self.protocol_error(err.code, err.detail)
            return
        self._last_rx = asyncio.get_running_loop().time()
        self.degraded = False
        msg_type, body = env["type"], env["body"]
        if msg_type == "heartbeat":
            return
        if msg_type == "clock.request":
            nonce = body.get("nonce")
            if isinstance(nonce, str):
                try:
                    await self.send("clock.reply", {"nonce": nonce, "utc_ms": _utc_ms()})
                except LinkClosed:
                    pass
            return
        if msg_type == "command.ack":
            fut = self._waiters.pop(body.get("message_id"), None) if isinstance(
                body.get("message_id"), str) else None
            if fut is not None:
                if not fut.done():
                    fut.set_result(body)
                return
        self._deliver(msg_type, env)

    async def _on_audio_frame(self, frame: bytes | str) -> None:
        try:
            if not isinstance(frame, bytes):
                raise tl.ProtocolError("malformed_frame", "audio frames are binary")
            self._sink.on_audio(frame)
        except tl.ProtocolError as err:
            await self.protocol_error(err.code, err.detail)
        except Exception:
            log.exception(f"[link] {self.device_id}: on_audio failed")


# ---------------------------------------------------------------------------
# Socket handlers
# ---------------------------------------------------------------------------

async def _reject(ws: Any, device_id: str, reason: str) -> None:
    _, text = _encode("session.rejected", {"reason": reason}, session_id=None,
                      device_id=device_id, generation=0)
    try:
        await ws.send(text)
    except websockets.ConnectionClosed:
        pass
    await ws.close()


async def serve_control(ws: Any, *, secure: bool, hub: LinkHub,
                        timing: Timing = DEFAULT_TIMING) -> None:
    """`/device/v1/control`: hello → admit → ready, then the session."""
    peer_ip = str(ws.remote_address[0]) if ws.remote_address else ""
    try:
        async with asyncio.timeout(timing.hello_timeout_s):
            raw = await ws.recv()
    except (TimeoutError, websockets.ConnectionClosed):
        await ws.close()
        return

    device_id = ""
    try:
        env = parse_envelope(raw)
        device_id = env["device_id"]
        if env["type"] != "session.hello":
            raise tl.ProtocolError("unexpected_message", f"first message is {env['type']!r}")
        if env["session_id"] is not None:
            raise tl.ProtocolError("wrong_session", "session.hello carries a session_id")
        protocols = env["body"].get("protocols")
        if (not isinstance(protocols, list)
                or any(not isinstance(p, int) or isinstance(p, bool) for p in protocols)
                or PROTOCOL not in protocols):
            raise tl.ProtocolError("unsupported_protocol", f"protocols {protocols!r}")
        capabilities = env["body"].get("capabilities")
        if (not isinstance(capabilities, list)
                or any(not isinstance(capability, str) for capability in capabilities)):
            raise tl.ProtocolError("malformed_message", "capabilities must be a string array")
        if env["generation"] != 0:
            raise tl.ProtocolError("malformed_message", "session.hello generation must be 0")
    except tl.ProtocolError as err:
        log.warning(f"[link] {peer_ip} {device_id or '?'}: hello refused: {err}")
        await _reject(ws, device_id, "protocol")
        return

    hello = env["body"]
    token = _header(ws.request.headers, TOKEN_HEADER)
    try:
        verdict = await hub.admit(hello, device_id=device_id, peer_ip=peer_ip,
                                  secure=secure, token=token)
    except Exception:
        log.exception(f"[link] {device_id}: admit failed")
        await ws.close()
        return
    if isinstance(verdict, Rejected):
        log.info(f"[link] {device_id} from {peer_ip}: rejected ({verdict.reason})")
        await _reject(ws, device_id, verdict.reason)
        return

    old = _by_device.get(device_id)
    if old is not None:
        await old.close("closed")
    session_id = str(uuid.uuid4())
    link = DeviceLink(ws, device_id=device_id, session_id=session_id, hello=hello,
                      peer_ip=peer_ip, secure=secure, token=token, sink=verdict.sink,
                      timing=timing)
    _by_device[device_id] = link
    _by_session[session_id] = link
    ready = {**verdict.ready, "protocol": PROTOCOL, "session_id": session_id,
             "server_boot_id": SERVER_BOOT_ID, "utc_ms": _utc_ms()}
    try:
        await link.send("session.ready", ready)
    except LinkClosed:
        await link.close("closed")
        return
    log.info(f"[link] {device_id} at {peer_ip}: session {session_id} ready "
             f"({'tls' if secure else 'plain'})")
    link._start()
    link._ready_task = asyncio.create_task(link._run_ready(), name=f"ready-{device_id}")
    try:
        async for raw in ws:
            await link._on_control(raw)
            if link.closed:
                break
    except websockets.ConnectionClosed:
        pass
    finally:
        await link.close("closed")


def _bind(ws: Any, secure: bool, plane: str) -> DeviceLink | None:
    link = _binding(ws.request.headers)
    if link is None or link.secure != secure:
        log.warning(f"[link] {plane} upgrade from {ws.remote_address} not bound to a live session")
        return None
    return link


async def serve_audio(ws: Any, *, secure: bool) -> None:
    """`/device/v1/audio`: uplink EMA1 frames to the sink; downlink via
    `DeviceLink.send_audio`. Losing it ends the session."""
    link = _bind(ws, secure, "audio")
    if link is None:
        await ws.close(code=1008, reason="not the current session")
        return
    previous, link._audio = link._audio, ws
    link._audio_attached.set()
    if previous is not None:
        await previous.close()
    try:
        async for frame in ws:
            await link._on_audio_frame(frame)
            if link.closed:
                break
    except websockets.ConnectionClosed:
        pass
    finally:
        if link._audio is ws:
            link._audio = None
            link._audio_attached.clear()
            await link.close("closed")


async def serve_assets(ws: Any, *, secure: bool,
                       assets: em_device_assets.DeviceAssets) -> None:
    """`/device/v1/assets` (WIRE §6): one request in flight; a second request
    before the answer ends closes the socket."""
    link = _bind(ws, secure, "assets")
    if link is None:
        await ws.close(code=1008, reason="not the current session")
        return
    previous, link._assets = link._assets, ws
    if previous is not None:
        await previous.close()
    try:
        await _assets_loop(ws, link.device_id, assets)
    except websockets.ConnectionClosed:
        pass
    except Exception:
        log.exception(f"[assets] {link.device_id}: serving failed")
    finally:
        if link._assets is ws:
            link._assets = None
        await ws.close()


def _parse_asset_request(raw: str | bytes) -> tuple[object, int]:
    if isinstance(raw, bytes):
        raise ValueError("asset requests are text")
    req = json.loads(raw)
    if not isinstance(req, dict):
        raise ValueError("asset request is not an object")
    offset = req.get("offset")
    if not _is_count(offset):
        raise ValueError(f"offset {offset!r} is not a non-negative integer")
    return req.get("sha256"), offset


async def _assets_loop(ws: Any, device_id: str, assets: Any) -> None:
    pending: asyncio.Task | None = asyncio.ensure_future(ws.recv())
    while pending is not None:
        raw = await pending
        try:
            sha, offset = _parse_asset_request(raw)
        except ValueError as err:
            log.warning(f"[assets] {device_id}: bad request: {err}")
            return
        answer = asyncio.create_task(_answer(ws, assets, sha, offset))
        pending = asyncio.ensure_future(ws.recv())
        done, _ = await asyncio.wait({answer, pending}, return_when=asyncio.FIRST_COMPLETED)
        if pending in done:
            answer.cancel()
            await asyncio.gather(answer, return_exceptions=True)
            if pending.exception() is None:
                log.warning(f"[assets] {device_id}: second request before the answer ended")
            return
        exc = answer.exception()
        if exc is not None:
            pending.cancel()
            raise exc
        log.info(f"[assets] {device_id}: served {sha} from {offset}")


async def _answer(ws: Any, assets: Any, sha: object, offset: int) -> None:
    """Chunks from `offset`, then the completion; not_found otherwise. An
    offset at or past the end yields no chunks, so the device's size check
    restarts a stale partial file."""
    loop = asyncio.get_running_loop()
    try:
        info = await loop.run_in_executor(None, assets.resolve, sha)
    except em_device_assets.AssetNotFound:
        await ws.send(json.dumps({"error": "not_found"}))
        return
    with open(info.path, "rb") as source:
        pos = offset
        while pos < info.size:
            block = await loop.run_in_executor(None, _read_at, source, pos)
            if not block:
                break
            await ws.send(block)
            pos += len(block)
    await ws.send(json.dumps({"sha256": info.sha256, "size": info.size, "done": True}))


def _read_at(source: Any, pos: int) -> bytes:
    source.seek(pos)
    return source.read(MAX_ASSET_CHUNK)
