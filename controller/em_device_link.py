"""Controller end of the device link: three WebSockets per session.

SPEC §4.4 and §16.1; field spellings are the WIRE (`docs/protocol-v1.md`
§1, §2, §4.1, §6). Core's listener dispatches the three paths to
`serve_control`, `serve_audio`, and `serve_assets` with one shared
`LinkRegistry`, passes `LinkRegistry.process_request` and `compression=None`
to `websockets.serve`, and supplies the `LinkHub` that admits sessions.

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
import enum
import hmac
import http
import json
import logging
import time
import uuid
from collections.abc import Iterator, Mapping
from dataclasses import dataclass, field
from typing import BinaryIO, Protocol

import websockets
from websockets.asyncio.server import ServerConnection
from websockets.datastructures import Headers
from websockets.http11 import Request, Response

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
POLICY_VIOLATION = 1008                   # WebSocket close code for an unbound upgrade

# Identifies this controller process in `session.ready` (§16.1).
SERVER_BOOT_ID = str(uuid.uuid4())


class MessageType(enum.StrEnum):
    """Envelope `type` of every control message (WIRE §4)."""

    # §4.1 session (heartbeat, command.ack and protocol.error go both ways)
    SESSION_HELLO = "session.hello"                  # D→C
    SESSION_READY = "session.ready"                  # C→D
    SESSION_REJECTED = "session.rejected"            # C→D
    HEARTBEAT = "heartbeat"
    CLOCK_REQUEST = "clock.request"                  # D→C
    CLOCK_REPLY = "clock.reply"                      # C→D
    COMMAND_ACK = "command.ack"
    PROTOCOL_ERROR = "protocol.error"
    # §4.2 streams and render
    STREAM_OPEN = "stream.open"                      # D→C
    STREAM_END = "stream.end"                        # D→C
    RENDER_START = "render.start"                    # C→D
    RENDER_END = "render.end"                        # C→D
    RENDER_CANCEL = "render.cancel"                  # C→D
    RENDER_PROGRESS = "render.progress"              # D→C
    RENDER_FINISHED = "render.finished"              # D→C
    # §4.3 focus (C→D)
    FOCUS_ACQUIRE = "focus.acquire"
    FOCUS_RENEW = "focus.renew"
    FOCUS_RELEASE = "focus.release"
    # §4.4 wake (D→C)
    WAKE_CANDIDATE = "wake.candidate"
    WAKE_CANDIDATE_END = "wake.candidate_end"
    WAKE_STATS = "wake.stats"
    # §4.5 uplink leases
    UPLINK_OPEN = "uplink.open"                      # C→D
    UPLINK_RENEW = "uplink.renew"                    # C→D
    UPLINK_CLOSE = "uplink.close"                    # C→D
    UPLINK_ENDED = "uplink.ended"                    # D→C
    # §4.6 physical events (D→C)
    PRIVACY_CHANGED = "privacy.changed"
    BUTTON_ACTION = "button.action"
    # §4.7 alerts
    ALERT_SNAPSHOT = "alert.snapshot"                # C→D
    ALERT_DELTA = "alert.delta"                      # C→D
    ALERT_ACK = "alert.ack"                          # D→C
    ALERT_LOCAL_OPERATION = "alert.local_operation"  # D→C
    ALERT_OP_RESULT = "alert.op_result"              # C→D
    ALERT_ACT = "alert.act"                          # C→D
    ALERT_RING = "alert.ring"                        # C→D
    ALERT_PREFETCH = "alert.prefetch"                # C→D
    ALERT_RING_ENDED = "alert.ring_ended"            # D→C
    ALERT_STATE = "alert.state"                      # D→C
    # §4.8 retained, C→D
    LEDS = "leds"
    LED_ANIM = "led_anim"
    VOLUME_SET = "volume_set"
    CONFIG = "config"
    SHELL_OPEN = "shell_open"
    SHELL_CLOSE = "shell_close"
    WIFI_CHANGE = "wifi_change"
    WIFI_COMMIT = "wifi_commit"
    WIFI_SCAN = "wifi_scan"
    PING = "ping"
    # §4.8 retained, D→C
    VOLUME_STATE = "volume_state"
    AMBIENT_LIGHT = "ambient_light"
    LOG = "log"
    WIFI_RESULT = "wifi_result"
    WIFI_SCAN_RESULT = "wifi_scan_result"
    BLE_ADVERTS = "ble_adverts"
    STATS = "stats"
    PONG = "pong"


class Capability(enum.StrEnum):
    """`session.hello.capabilities` names (SPEC §11.1, WIRE §4.1)."""

    AUDIO_TIMELINE = "audio_timeline_v1"
    UPLINK_LEASES = "uplink_leases_v1"
    DEVICE_WAKE = "device_wake_v1"
    RENDER_REFERENCE = "render_reference_v1"
    RENDER_PROGRESS = "render_progress_v1"
    FOCUS_LEASES = "focus_leases_v1"
    ALERT_CACHE = "alert_cache_v1"
    TURN_PROTOCOL = "turn_protocol_v1"
    LEDS = "leds"
    LED_ANIM = "led_anim"
    BUTTONS = "buttons"
    BUTTON_HOLD = "button_hold"
    AMBIENT_LIGHT = "ambient_light"
    ALERT_PREFETCH = "alert_prefetch"
    LOCAL_WAKE_CHIME = "local_wake_chime"


class RejectReason(enum.StrEnum):
    """`session.rejected.reason` (WIRE §4.1)."""

    PENDING_APPROVAL = "pending_approval"
    UNAUTHORIZED = "unauthorized"
    PROTOCOL = "protocol"


class AckStatus(enum.StrEnum):
    """`command.ack.status` (WIRE §4.1)."""

    ACCEPTED = "accepted"
    APPLIED = "applied"
    DURABLE = "durable"
    REJECTED = "rejected"


class CloseReason(enum.StrEnum):
    """Why a session ended; handed to `LinkSink.on_lost`."""

    CLOSED = "closed"
    SESSION_LOST = "session_lost"
    PROTOCOL = "protocol"


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


# ---------------------------------------------------------------------------
# Typed bodies (WIRE §2, §4.1)
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class Envelope:
    """One validated D→C control message of a type this controller implements."""

    type: MessageType
    session_id: str | None
    message_id: str
    device_id: str
    generation: int
    body: dict[str, object]


@dataclass(frozen=True, slots=True)
class UnknownEnvelope:
    """A well-formed envelope of a type this controller does not implement;
    it counts as liveness and is otherwise ignored (WIRE preamble)."""

    type: str
    session_id: str | None
    device_id: str


@dataclass(frozen=True, slots=True)
class CommandAck:
    """`command.ack` body (WIRE §4.1)."""

    message_id: str
    status: AckStatus
    error: str | None

    @classmethod
    def parse(cls, body: Mapping[str, object]) -> CommandAck:
        """Raises `ProtocolError` for a malformed body."""
        message_id = body.get("message_id")
        if not isinstance(message_id, str) or not message_id:
            raise tl.ProtocolError("malformed_message", "command.ack.message_id must be a string")
        status = body.get("status")
        if not isinstance(status, str) or status not in AckStatus:
            raise tl.ProtocolError("malformed_message", "invalid command.ack.status")
        error = body.get("error")
        if error is not None and not isinstance(error, str):
            raise tl.ProtocolError("malformed_message", "command.ack.error must be a string or null")
        return cls(message_id, AckStatus(status), error)

    def wire(self) -> dict[str, object]:
        return {"message_id": self.message_id, "status": self.status, "error": self.error}


def _object(value: object) -> dict[str, object] | None:
    return value if isinstance(value, dict) else None


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


@dataclass(frozen=True, slots=True)
class SessionHello:
    """`session.hello` body (WIRE §4.1), narrowed at the link boundary.

    `capabilities` keeps every announced name, known or not; compare against
    `Capability` members. `alerts` is the raw `alerts` object, which the
    alert engine owns.
    """

    protocols: tuple[int, ...]
    capabilities: frozenset[str]
    firmware_version: str | None = None
    boot_id: str | None = None
    ambient_light_status: dict[str, object] | None = None
    muted: bool | None = None
    volume_level: int | None = None
    alerts: dict[str, object] = field(default_factory=dict)
    assets: tuple[str, ...] = ()

    @classmethod
    def parse(cls, body: Mapping[str, object]) -> SessionHello:
        """Raises `ProtocolError` when `protocols` or `capabilities` is unusable."""
        protocols = body.get("protocols")
        if (not isinstance(protocols, list)
                or any(not isinstance(p, int) or isinstance(p, bool) for p in protocols)
                or PROTOCOL not in protocols):
            raise tl.ProtocolError("unsupported_protocol", f"protocols {protocols!r}")
        capabilities = body.get("capabilities")
        if (not isinstance(capabilities, list)
                or any(not isinstance(capability, str) for capability in capabilities)):
            raise tl.ProtocolError("malformed_message", "capabilities must be a string array")
        privacy = _object(body.get("privacy")) or {}
        muted = privacy.get("muted")
        volume = _object(body.get("volume")) or {}
        level = volume.get("level")
        assets = body.get("assets")
        return cls(
            protocols=tuple(protocols),
            capabilities=frozenset(capabilities),
            firmware_version=_text(body.get("firmware_version")),
            boot_id=_text(body.get("boot_id")),
            ambient_light_status=_object(body.get("ambient_light_status")),
            muted=muted if isinstance(muted, bool) else None,
            volume_level=(int(level) if isinstance(level, (int, float))
                          and not isinstance(level, bool) else None),
            alerts=_object(body.get("alerts")) or {},
            assets=(tuple(a for a in assets if isinstance(a, str))
                    if isinstance(assets, list) else ()),
        )

    @property
    def alerts_wakeup(self) -> str | None:
        """`alerts.wakeup`: `ok` or `alarm_wakeup_unavailable`."""
        wakeup = self.alerts.get("wakeup")
        return wakeup if isinstance(wakeup, str) else None


@dataclass(frozen=True, slots=True)
class DetectorThresholds:
    idle: float
    playback: float
    near_miss: float


@dataclass(frozen=True, slots=True)
class ProvisionalDuck:
    duck_db: float
    max_per_window: int = 2
    window_ms: int = 5000


@dataclass(frozen=True, slots=True)
class DetectorConfig:
    """`session.ready.detector` (WIRE §4.1)."""

    thresholds: DetectorThresholds
    provisional_duck: ProvisionalDuck
    hop_blocks: int = 2
    smoothing: int = 3
    clear_after_unscored: int = 6

    def wire(self) -> dict[str, object]:
        return {
            "thresholds": {
                "idle": self.thresholds.idle,
                "playback": self.thresholds.playback,
                "near_miss": self.thresholds.near_miss,
            },
            "hop_blocks": self.hop_blocks,
            "smoothing": self.smoothing,
            "clear_after_unscored": self.clear_after_unscored,
            "provisional_duck": {
                "duck_db": self.provisional_duck.duck_db,
                "max_per_window": self.provisional_duck.max_per_window,
                "window_ms": self.provisional_duck.window_ms,
            },
        }


@dataclass(frozen=True, slots=True)
class ReadyGrant:
    """The hub's part of `session.ready`; the link adds `protocol`,
    `session_id`, `server_boot_id` and `utc_ms`."""

    capture_permitted: bool
    assets: em_device_assets.SpeechAssets
    detector: DetectorConfig

    def wire(self) -> dict[str, object]:
        return {
            "capture_permitted": self.capture_permitted,
            "assets": self.assets.wire(),
            "detector": self.detector.wire(),
        }


@dataclass(frozen=True)
class Rejected:
    reason: RejectReason


@dataclass(frozen=True)
class Admitted:
    ready: ReadyGrant
    sink: "LinkSink"


class LinkHub(Protocol):
    async def admit(self, hello: SessionHello, *, device_id: str, peer_ip: str, secure: bool,
                    token: str | None) -> Admitted | Rejected: ...


class LinkSink(Protocol):
    async def on_ready(self, link: "DeviceLink") -> None: ...
    def on_message(self, envelope: Envelope) -> None: ...
    def on_audio(self, frame: bytes) -> None: ...
    def on_lost(self, reason: CloseReason) -> None: ...


class AssetSource(Protocol):
    def resolve(self, sha256: str) -> em_device_assets.AssetInfo: ...


# ---------------------------------------------------------------------------
# Envelopes (WIRE §2)
# ---------------------------------------------------------------------------

def _is_count(value: object) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


# WIRE §4 uint64 fields (decimal strings) of D→C bodies, by message type.
# A path step "*" walks every list item / dict value. Absent or null values
# are the owning module's concern; present ones must be decimal strings.
_U64_PATHS: dict[MessageType, tuple[tuple[str, ...], ...]] = {
    MessageType.HEARTBEAT: (("mono_ns",),),
    MessageType.STREAM_OPEN: (("epoch",),),
    MessageType.STREAM_END: (("epoch",), ("final_sample",)),
    MessageType.RENDER_PROGRESS: (("submitted_frames",), ("completed_frames",), ("mono_ns",),
                                  ("seek_frame",), ("missing_from",), ("missing_to",)),
    MessageType.RENDER_FINISHED: (("last_completed_frame",),),
    MessageType.WAKE_CANDIDATE: (("capture_epoch",), ("first_crossing_end",), ("support_start",),
                                 ("mono_ns",), ("hops", "*", "end_sample")),
    MessageType.WAKE_CANDIDATE_END: (("support_end",),),
    MessageType.WAKE_STATS: (("near_misses", "*", "mono_ns"),),
    MessageType.UPLINK_ENDED: (("last_sample", "*"), ("clipped_start", "*")),
    MessageType.PRIVACY_CHANGED: (("capture_epoch",),),
    MessageType.BUTTON_ACTION: (("mono_ns",), ("capture_epoch",), ("capture_sample",)),
}


def _walk(value: object, path: tuple[str, ...], name: str) -> Iterator[tuple[str, object]]:
    if not path:
        yield name, value
        return
    step, rest = path[0], path[1:]
    if step == "*":
        items: Iterator[tuple[object, object]] = (
            iter(value.items()) if isinstance(value, dict)
            else enumerate(value) if isinstance(value, list) else iter(()))
        for key, child in items:
            yield from _walk(child, rest, f"{name}[{key}]")
    elif isinstance(value, dict) and step in value:
        yield from _walk(value[step], rest, f"{name}.{step}")


def _validate_u64_fields(msg_type: MessageType, body: dict[str, object]) -> None:
    for path in _U64_PATHS.get(msg_type, ()):
        for name, value in _walk(body, path, msg_type):
            if value is not None:
                tl.parse_u64(value, name)


def _validate_link_body(msg_type: MessageType, body: dict[str, object]) -> None:
    if msg_type is MessageType.HEARTBEAT and "mono_ns" not in body:
        raise tl.ProtocolError("malformed_message", "heartbeat.mono_ns is required")
    if msg_type is MessageType.CLOCK_REQUEST:
        if not isinstance(body.get("nonce"), str) or not body["nonce"]:
            raise tl.ProtocolError("malformed_message", "clock.request.nonce must be a string")
    if msg_type is MessageType.COMMAND_ACK:
        CommandAck.parse(body)


def parse_envelope(raw: str | bytes) -> Envelope | UnknownEnvelope:
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
    raw_type: str = env["type"]
    message_id: str = env["message_id"]
    device_id: str = env["device_id"]
    sid = env.get("session_id")
    if sid is not None and not isinstance(sid, str):
        raise tl.ProtocolError("malformed_message", "session_id must be a string or null")
    generation = env.get("generation")
    if not isinstance(generation, int) or not _is_count(generation) or generation > tl.U32_MAX:
        raise tl.ProtocolError("malformed_message", "generation must be a uint32")
    body = env.get("body")
    if not isinstance(body, dict):
        raise tl.ProtocolError("malformed_message", "body must be an object")
    if raw_type not in MessageType:
        return UnknownEnvelope(raw_type, sid, device_id)
    msg_type = MessageType(raw_type)
    _validate_u64_fields(msg_type, body)
    _validate_link_body(msg_type, body)
    return Envelope(msg_type, sid, message_id, device_id, generation, body)


def _encode(msg_type: MessageType, body: Mapping[str, object], *, session_id: str | None,
            device_id: str, generation: int) -> tuple[str, str]:
    if not _is_count(generation) or generation > tl.U32_MAX:
        raise ValueError("generation must be a uint32")
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

def _header(headers: Headers, name: str) -> str | None:
    return headers.get(name) or None


def _path(request: Request) -> str:
    return request.path.split("?", 1)[0]


class LinkRegistry:
    """The live sessions of one controller's device listeners (plain and
    TLS share one registry): at most one link per device and per session."""

    def __init__(self) -> None:
        self._by_device: dict[str, DeviceLink] = {}
        self._by_session: dict[str, DeviceLink] = {}

    def _add(self, link: DeviceLink) -> None:
        self._by_device[link.device_id] = link
        self._by_session[link.session_id] = link

    def _remove(self, link: DeviceLink) -> None:
        if self._by_device.get(link.device_id) is link:
            del self._by_device[link.device_id]
        self._by_session.pop(link.session_id, None)

    def binding(self, headers: Headers) -> DeviceLink | None:
        """The live link an audio/assets upgrade may bind to, or None."""
        sid = _header(headers, SESSION_HEADER)
        link = self._by_session.get(sid) if sid else None
        if link is None or link.closed:
            return None
        presented = _header(headers, TOKEN_HEADER)
        if presented is None or link._token is None:
            return link if presented == link._token else None
        return link if hmac.compare_digest(presented, link._token) else None

    def process_request(self, connection: ServerConnection, request: Request) -> Response | None:
        """`websockets.serve(process_request=…)` hook: HTTP 403 for an audio or
        assets upgrade not bound to a live session (WIRE §1); None otherwise."""
        if _path(request) not in (AUDIO_PATH, ASSETS_PATH):
            return None
        if self.binding(request.headers) is None:
            return connection.respond(http.HTTPStatus.FORBIDDEN, "not the current session\n")
        return None


# ---------------------------------------------------------------------------
# The link
# ---------------------------------------------------------------------------

class DeviceLink:
    """One device session: the control socket plus its audio/assets sockets."""

    def __init__(self, ws: ServerConnection, *, registry: LinkRegistry, device_id: str,
                 session_id: str, hello: SessionHello, peer_ip: str, secure: bool,
                 token: str | None, sink: LinkSink, timing: Timing):
        self.device_id = device_id
        self.session_id = session_id
        self.hello = hello
        self.capabilities = hello.capabilities
        self.peer_ip = peer_ip
        self.secure = secure
        self.degraded = False
        self.closed = False
        self._registry = registry
        self._token = token
        self._sink = sink
        self._timing = timing
        self._control = ws
        self._audio: ServerConnection | None = None
        self._assets: ServerConnection | None = None
        self._audio_attached = asyncio.Event()
        self._waiters: dict[str, asyncio.Future[CommandAck]] = {}
        self._ready_done = False
        self._queued: list[Envelope] = []
        self._tasks: list[asyncio.Task[None]] = []           # heartbeat + watchdog
        self._ready_task: asyncio.Task[None] | None = None   # on_ready; never cancelled
        self._last_rx = asyncio.get_running_loop().time()

    def __repr__(self) -> str:
        return f"<DeviceLink {self.device_id} session={self.session_id}>"

    # -- sending ------------------------------------------------------------

    async def _send_text(self, text: str) -> None:
        try:
            await self._control.send(text)
        except websockets.ConnectionClosed:
            raise LinkClosed(f"{self.device_id}: control socket closed") from None

    async def send(self, msg_type: MessageType, body: Mapping[str, object], *,
                   generation: int = 0) -> str:
        """Send one C→D envelope; returns its message_id."""
        if self.closed:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        message_id, text = _encode(msg_type, body, session_id=self.session_id,
                                   device_id=self.device_id, generation=generation)
        await self._send_text(text)
        return message_id

    async def request(self, msg_type: MessageType, body: Mapping[str, object], *,
                      generation: int = 0, timeout: float = 2.0) -> CommandAck:
        """Send and await the device's `command.ack`. Raises `TimeoutError`
        when no ack arrives, `LinkClosed` on session loss."""
        if self.closed:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        message_id, text = _encode(msg_type, body, session_id=self.session_id,
                                   device_id=self.device_id, generation=generation)
        fut: asyncio.Future[CommandAck] = asyncio.get_running_loop().create_future()
        self._waiters[message_id] = fut
        try:
            await self._send_text(text)
            async with asyncio.timeout(timeout):
                return await fut
        finally:
            self._waiters.pop(message_id, None)

    async def ack(self, message_id: str, status: AckStatus, error: str | None = None) -> None:
        await self.send(MessageType.COMMAND_ACK, CommandAck(message_id, status, error).wire())

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
        audio = self._audio
        if self.closed or audio is None:
            raise LinkClosed(f"{self.device_id}: session {self.session_id} is closed")
        try:
            await audio.send(frame)
        except websockets.ConnectionClosed:
            raise LinkClosed(f"{self.device_id}: audio socket closed") from None

    async def protocol_error(self, code: str, detail: str) -> None:
        """Report a protocol violation and end the session (§16.1)."""
        log.warning(f"[link] {self.device_id}: protocol error {code}: {detail}")
        try:
            await self.send(MessageType.PROTOCOL_ERROR, {"code": code, "detail": detail})
        except LinkClosed:
            pass
        await self.close(CloseReason.PROTOCOL)

    # -- lifecycle ----------------------------------------------------------

    async def close(self, reason: CloseReason) -> None:
        """End the session: all three sockets close, `on_lost(reason)` once."""
        if self.closed:
            return
        self.closed = True
        self._audio_attached.set()
        self._registry._remove(self)
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
                await self.send(MessageType.HEARTBEAT, {"mono_ns": _mono_ns()})
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
                await self.close(CloseReason.SESSION_LOST)
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
            await self.close(CloseReason.CLOSED)
            return
        queued, self._queued = self._queued, []
        self._ready_done = True
        for env in queued:
            self._deliver(env)

    # -- receiving ----------------------------------------------------------

    def _deliver(self, env: Envelope) -> None:
        if self.closed:
            return
        if not self._ready_done:
            self._queued.append(env)
            return
        try:
            self._sink.on_message(env)
        except Exception:
            log.exception(f"[link] {self.device_id}: on_message({env.type}) failed")

    async def _on_control(self, raw: str | bytes) -> None:
        try:
            env = parse_envelope(raw)
            if env.session_id != self.session_id:
                raise tl.ProtocolError("wrong_session", f"session_id {env.session_id!r}")
            if env.device_id != self.device_id:
                raise tl.ProtocolError("wrong_device", f"device_id {env.device_id!r}")
            if env.type == MessageType.SESSION_HELLO:
                raise tl.ProtocolError("unexpected_message", "session.hello on a live session")
        except tl.ProtocolError as err:
            await self.protocol_error(err.code, err.detail)
            return
        self._last_rx = asyncio.get_running_loop().time()
        self.degraded = False
        if isinstance(env, UnknownEnvelope) or env.type is MessageType.HEARTBEAT:
            return
        if env.type is MessageType.CLOCK_REQUEST:
            nonce = env.body.get("nonce")
            if isinstance(nonce, str):
                try:
                    await self.send(MessageType.CLOCK_REPLY, {"nonce": nonce, "utc_ms": _utc_ms()})
                except LinkClosed:
                    pass
            return
        if env.type is MessageType.COMMAND_ACK:
            ack = CommandAck.parse(env.body)
            fut = self._waiters.pop(ack.message_id, None)
            if fut is not None:
                if not fut.done():
                    fut.set_result(ack)
                return
        self._deliver(env)

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

def peer_ip(ws: ServerConnection) -> str:
    """The remote IP of an accepted socket, or "" when unknown."""
    remote = ws.remote_address
    return str(remote[0]) if remote else ""


def request_header(ws: ServerConnection, name: str) -> str | None:
    """An upgrade request header, or None when absent or empty."""
    return _header(ws.request.headers, name) if ws.request is not None else None


async def _reject(ws: ServerConnection, device_id: str, reason: RejectReason) -> None:
    _, text = _encode(MessageType.SESSION_REJECTED, {"reason": reason}, session_id=None,
                      device_id=device_id, generation=0)
    try:
        await ws.send(text)
    except websockets.ConnectionClosed:
        pass
    await ws.close()


async def serve_control(ws: ServerConnection, *, secure: bool, hub: LinkHub,
                        links: LinkRegistry, timing: Timing = DEFAULT_TIMING) -> None:
    """`/device/v1/control`: hello → admit → ready, then the session."""
    remote_ip = peer_ip(ws)
    try:
        async with asyncio.timeout(timing.hello_timeout_s):
            raw = await ws.recv()
    except (TimeoutError, websockets.ConnectionClosed):
        await ws.close()
        return

    device_id = ""
    try:
        env = parse_envelope(raw)
        device_id = env.device_id
        if isinstance(env, UnknownEnvelope) or env.type is not MessageType.SESSION_HELLO:
            raise tl.ProtocolError("unexpected_message", f"first message is '{env.type}'")
        if env.session_id is not None:
            raise tl.ProtocolError("wrong_session", "session.hello carries a session_id")
        hello = SessionHello.parse(env.body)
        if env.generation != 0:
            raise tl.ProtocolError("malformed_message", "session.hello generation must be 0")
    except tl.ProtocolError as err:
        log.warning(f"[link] {remote_ip} {device_id or '?'}: hello refused: {err}")
        await _reject(ws, device_id, RejectReason.PROTOCOL)
        return

    token = request_header(ws, TOKEN_HEADER)
    try:
        verdict = await hub.admit(hello, device_id=device_id, peer_ip=remote_ip,
                                  secure=secure, token=token)
    except Exception:
        log.exception(f"[link] {device_id}: admit failed")
        await ws.close()
        return
    if isinstance(verdict, Rejected):
        log.info(f"[link] {device_id} from {remote_ip}: rejected ({verdict.reason})")
        await _reject(ws, device_id, verdict.reason)
        return

    old = links._by_device.get(device_id)
    if old is not None:
        await old.close(CloseReason.CLOSED)
    session_id = str(uuid.uuid4())
    link = DeviceLink(ws, registry=links, device_id=device_id, session_id=session_id,
                      hello=hello, peer_ip=remote_ip, secure=secure, token=token,
                      sink=verdict.sink, timing=timing)
    links._add(link)
    ready = {**verdict.ready.wire(), "protocol": PROTOCOL, "session_id": session_id,
             "server_boot_id": SERVER_BOOT_ID, "utc_ms": _utc_ms()}
    try:
        await link.send(MessageType.SESSION_READY, ready)
    except LinkClosed:
        await link.close(CloseReason.CLOSED)
        return
    log.info(f"[link] {device_id} at {remote_ip}: session {session_id} ready "
             f"({'tls' if secure else 'plain'})")
    link._start()
    link._ready_task = asyncio.create_task(link._run_ready(), name=f"ready-{device_id}")
    try:
        async for message in ws:
            await link._on_control(message)
            if link.closed:
                break
    except websockets.ConnectionClosed:
        pass
    finally:
        await link.close(CloseReason.CLOSED)


def _bind(ws: ServerConnection, links: LinkRegistry, secure: bool, plane: str) -> DeviceLink | None:
    link = links.binding(ws.request.headers) if ws.request is not None else None
    if link is None or link.secure != secure:
        log.warning(f"[link] {plane} upgrade from {ws.remote_address} not bound to a live session")
        return None
    return link


async def serve_audio(ws: ServerConnection, *, secure: bool, links: LinkRegistry) -> None:
    """`/device/v1/audio`: uplink EMA1 frames to the sink; downlink via
    `DeviceLink.send_audio`. Losing it ends the session."""
    link = _bind(ws, links, secure, "audio")
    if link is None:
        await ws.close(code=POLICY_VIOLATION, reason="not the current session")
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
            await link.close(CloseReason.CLOSED)


async def serve_assets(ws: ServerConnection, *, secure: bool, links: LinkRegistry,
                       assets: AssetSource) -> None:
    """`/device/v1/assets` (WIRE §6): one request in flight; a second request
    before the answer ends closes the socket."""
    link = _bind(ws, links, secure, "assets")
    if link is None:
        await ws.close(code=POLICY_VIOLATION, reason="not the current session")
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


def _parse_asset_request(raw: str | bytes) -> tuple[str | None, int]:
    """(sha256, offset); a non-string sha256 is None and answered not_found."""
    if isinstance(raw, bytes):
        raise ValueError("asset requests are text")
    req = json.loads(raw)
    if not isinstance(req, dict):
        raise ValueError("asset request is not an object")
    offset = req.get("offset")
    if not isinstance(offset, int) or not _is_count(offset):
        raise ValueError(f"offset {offset!r} is not a non-negative integer")
    sha = req.get("sha256")
    return (sha if isinstance(sha, str) else None), offset


async def _assets_loop(ws: ServerConnection, device_id: str, assets: AssetSource) -> None:
    pending: asyncio.Future[str | bytes] = asyncio.ensure_future(ws.recv())
    while True:
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


async def _answer(ws: ServerConnection, assets: AssetSource, sha: str | None, offset: int) -> None:
    """Chunks from `offset`, then the completion; not_found otherwise. An
    offset at or past the end yields no chunks, so the device's size check
    restarts a stale partial file."""
    loop = asyncio.get_running_loop()
    try:
        if sha is None:
            raise em_device_assets.AssetNotFound("not_found")
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


def _read_at(source: BinaryIO, pos: int) -> bytes:
    source.seek(pos)
    return source.read(MAX_ASSET_CHUNK)
