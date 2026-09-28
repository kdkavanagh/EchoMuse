"""Stock ESPHome satellite surface for one EchoMuse endpoint (SPEC §16.7).

The ESPHome voice path is deliberately narrow: only a committed reply to an
HA-started conversation enters it.  Ordinary wake/button turns use
``em_ha_client`` websocket pipeline runs owned by ``SessionActor``.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
import os
import socket
import uuid
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass
from typing import Any

from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

import em_db as db
import em_ha_client
import em_player
import em_volume
from esphome.feature_flags import MediaPlayerEntityFeature, MediaPlayerState, VoiceAssistantFeature
from esphome.satellite_server import SatelliteServerProtocol, _HANDLED, serve
from esphome.vendor import api_pb2
from version import VERSION as CONTROLLER_VERSION

log = logging.getLogger("echomuse.esphome")

SERVER_IP = os.environ.get("SERVER_IP", "10.10.1.236")
ESPHOME_DEVICE_MODEL = "Echo Dot Gen 2 (biscuit)"
ESPHOME_PROJECT_VERSION = os.environ.get("ESPHOME_PROJECT_VERSION", CONTROLLER_VERSION)

MEDIA_PLAYER_KEY = 1
EVENT_KEY = 2
AMBIENT_LUX_KEY = 3
ALERT_RINGING_KEY = 4
STOP_ALERT_KEY = 5
BUTTON_EVENT_TYPES = ["long", "single", "double", "triple"]

MEDIA_PLAYER_FEATURES = int(
    MediaPlayerEntityFeature.PAUSE
    | MediaPlayerEntityFeature.STOP
    | MediaPlayerEntityFeature.PLAY
    | MediaPlayerEntityFeature.PLAY_MEDIA
    | MediaPlayerEntityFeature.BROWSE_MEDIA
    | MediaPlayerEntityFeature.VOLUME_SET
    | MediaPlayerEntityFeature.VOLUME_MUTE
    | MediaPlayerEntityFeature.MEDIA_ANNOUNCE
)
VOICE_ASSISTANT_FLAGS = int(
    VoiceAssistantFeature.VOICE_ASSISTANT
    | VoiceAssistantFeature.API_AUDIO
    | VoiceAssistantFeature.ANNOUNCE
    | VoiceAssistantFeature.TIMERS
    | VoiceAssistantFeature.START_CONVERSATION
)
VOICE_REQUEST_FLAGS_WAKE_WORD_DONE = 0   # start at STT; no HA wake-word stage
REPLY_CHUNK_BYTES = 640                  # 20 ms of 16 kHz PCM16 per VoiceAssistantAudio
_MP_STATE = {
    "idle": MediaPlayerState.IDLE,
    "playing": MediaPlayerState.PLAYING,
    "paused": MediaPlayerState.PAUSED,
}


@dataclass
class Hooks:
    announce: Callable[[str, str | None, bool], Awaitable[None]] | None = None
    set_volume: Callable[[int], Awaitable[None]] | None = None
    stop_alert: Callable[[], Awaitable[None]] | None = None
    timer_event: Callable[[str, str, str, int, int, bool], Awaitable[None]] | None = None
    attached: Callable[[], Awaitable[None]] | None = None


class EchoMuseSatellite(SatelliteServerProtocol):
    """One HA connection. Voice audio is accepted only via ``esphome_reply``."""

    def __init__(self, server: "DeviceESPhomeServer") -> None:
        super().__init__(
            server_name=f"echomuse-{server.device_id[-12:].lower()}",
            log_name=f"esphome.{server.device_id[-8:]}",
        )
        self.server = server
        self._disconnected_hook = server._on_satellite_disconnected
        self._run_queue: asyncio.Queue[object] | None = None
        self._reply_lock = asyncio.Lock()
        self._announce_tasks: set[asyncio.Task] = set()

    @property
    def device_id(self) -> str:
        return self.server.device_id

    def _device_has(self, capability: str) -> bool:
        return capability in self.server.capabilities

    def handle_message(self, msg):
        if isinstance(msg, api_pb2.DeviceInfoRequest):
            yield api_pb2.DeviceInfoResponse(
                uses_password=False,
                name=self.server_name,
                friendly_name=f"{self.server.label} Voice Assistant",
                mac_address=self.server.mac_address,
                manufacturer="EchoMuse",
                model=ESPHOME_DEVICE_MODEL,
                project_name=f"EchoMuse.{ESPHOME_DEVICE_MODEL}",
                project_version=ESPHOME_PROJECT_VERSION,
                voice_assistant_feature_flags=VOICE_ASSISTANT_FLAGS,
            )
            return

        if isinstance(msg, api_pb2.ListEntitiesRequest):
            fmt = dict(format="flac", sample_rate=48000, num_channels=1, sample_bytes=2)
            yield api_pb2.ListEntitiesMediaPlayerResponse(
                object_id="media_player", key=MEDIA_PLAYER_KEY, name=self.server.label,
                supports_pause=True, feature_flags=MEDIA_PLAYER_FEATURES,
                supported_formats=[
                    api_pb2.MediaPlayerSupportedFormat(
                        purpose=api_pb2.MEDIA_PLAYER_FORMAT_PURPOSE_DEFAULT, **fmt),
                    api_pb2.MediaPlayerSupportedFormat(
                        purpose=api_pb2.MEDIA_PLAYER_FORMAT_PURPOSE_ANNOUNCEMENT, **fmt),
                ],
            )
            if self._device_has("button_hold"):
                yield api_pb2.ListEntitiesEventResponse(
                    object_id="action_button", key=EVENT_KEY,
                    name=f"{self.server.label} Action Button", device_class="button",
                    event_types=BUTTON_EVENT_TYPES,
                )
            if self._device_has("ambient_light"):
                yield api_pb2.ListEntitiesSensorResponse(
                    object_id="ambient_light", key=AMBIENT_LUX_KEY,
                    name=f"{self.server.label} Ambient Light", unit_of_measurement="lx",
                    accuracy_decimals=0, device_class="illuminance", state_class=1,
                )
            yield api_pb2.ListEntitiesBinarySensorResponse(
                object_id="alert_ringing", key=ALERT_RINGING_KEY,
                name=f"{self.server.label} Alert ringing", device_class="sound",
            )
            yield api_pb2.ListEntitiesButtonResponse(
                object_id="stop_alert", key=STOP_ALERT_KEY,
                name=f"{self.server.label} Stop alert", device_class="restart",
            )
            yield api_pb2.ListEntitiesDoneResponse()
            return

        if isinstance(msg, (api_pb2.SubscribeStatesRequest,
                            api_pb2.SubscribeHomeAssistantStatesRequest)):
            yield self._media_state_msg()
            if self._device_has("ambient_light"):
                yield self._ambient_state_msg()
            yield self._alert_state_msg()
            return

        if isinstance(msg, api_pb2.SubscribeVoiceAssistantRequest):
            if msg.subscribe and self.server.hooks.attached is not None:
                self._spawn(self.server.hooks.attached(), "satellite-attached")
            yield _HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantConfigurationRequest):
            # BCResNet is device-owned; HA has no wake-word selector here (§18.1).
            yield api_pb2.VoiceAssistantConfigurationResponse()
            return

        if isinstance(msg, api_pb2.MediaPlayerCommandRequest):
            self._handle_media_command(msg)
            if msg.has_media_url and not (msg.has_announcement and msg.announcement):
                yield api_pb2.MediaPlayerStateResponse(
                    key=MEDIA_PLAYER_KEY, state=MediaPlayerState.PLAYING,
                    volume=self.server.volume, muted=False)
            else:
                yield self._media_state_msg()
            return

        if isinstance(msg, api_pb2.ButtonCommandRequest):
            if msg.key == STOP_ALERT_KEY and self.server.hooks.stop_alert is not None:
                self._spawn(self.server.hooks.stop_alert(), "stop-alert")
            yield _HANDLED
            return

        if isinstance(msg, api_pb2.SubscribeHomeassistantServicesRequest):
            yield _HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantResponse):
            yield _HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantEventResponse):
            self._handle_voice_event(msg)
            yield _HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantAnnounceRequest):
            task = self._spawn(self._announce(msg.media_id, msg.preannounce_media_id or None,
                                              bool(msg.start_conversation)), "announce")
            self._announce_tasks.add(task)
            task.add_done_callback(self._announce_tasks.discard)
            yield api_pb2.MediaPlayerStateResponse(
                key=MEDIA_PLAYER_KEY, state=MediaPlayerState.ANNOUNCING,
                volume=self.server.volume, muted=False)
            return

        if isinstance(msg, api_pb2.VoiceAssistantTimerEventResponse):
            self._handle_timer(msg)
            yield _HANDLED
            return

        log.debug("[%s] unhandled %s", self._log_name, type(msg).__name__)

    def _handle_media_command(self, msg: Any) -> None:
        if msg.has_volume and self.server.hooks.set_volume is not None:
            level = em_volume.ha_volume_to_device(msg.volume)
            self._spawn(self.server.hooks.set_volume(level), "volume")
        if msg.has_media_url:
            if msg.has_announcement and msg.announcement:
                if self.server.hooks.announce is not None:
                    self._spawn(self.server.hooks.announce(msg.media_url, None, False),
                                "media-announce")
            else:
                self._spawn(em_player.play(self.device_id, msg.media_url), "play-media")
            return
        if not msg.has_command:
            return
        if msg.command == api_pb2.MEDIA_PLAYER_COMMAND_PAUSE:
            self._spawn(em_player.pause(self.device_id), "pause-media")
        elif msg.command == api_pb2.MEDIA_PLAYER_COMMAND_PLAY:
            self._spawn(em_player.resume(self.device_id), "resume-media")
        elif msg.command == api_pb2.MEDIA_PLAYER_COMMAND_STOP:
            self._spawn(em_player.stop(self.device_id), "stop-media")

    async def _announce(self, media_id: str, preannounce: str | None,
                        start_conversation: bool) -> None:
        success = False
        try:
            if media_id and self.server.hooks.announce is not None:
                await self.server.hooks.announce(media_id, preannounce, start_conversation)
                success = True
        except Exception:
            log.exception("[%s] announcement failed", self._log_name)
        finally:
            if self._transport and not self._transport.is_closing():
                self._send_one(api_pb2.VoiceAssistantAnnounceFinished(success=success))
                self._send_one(self._media_state_msg())

    def _handle_timer(self, msg: Any) -> None:
        from esphome.vendor.api_pb2 import VoiceAssistantTimerEvent as TE
        mapping = {
            TE.VOICE_ASSISTANT_TIMER_STARTED: "started",
            TE.VOICE_ASSISTANT_TIMER_UPDATED: "updated",
            TE.VOICE_ASSISTANT_TIMER_CANCELLED: "cancelled",
            TE.VOICE_ASSISTANT_TIMER_FINISHED: "finished",
        }
        event = mapping.get(msg.event_type)
        if event is None or self.server.hooks.timer_event is None:
            return
        self._spawn(self.server.hooks.timer_event(
            event, msg.timer_id, msg.name or "timer", int(msg.total_seconds),
            int(msg.seconds_left), bool(msg.is_active)), "timer-event")

    def _handle_voice_event(self, msg: Any) -> None:
        queue = self._run_queue
        if queue is None:
            return
        from esphome.vendor.api_pb2 import VoiceAssistantEvent as ET
        data = {item.name: item.value for item in msg.data}
        typ = msg.event_type
        if typ == ET.VOICE_ASSISTANT_STT_END:
            queue.put_nowait(em_ha_client.SttEnded(data.get("text", "")))
        elif typ == ET.VOICE_ASSISTANT_INTENT_END:
            queue.put_nowait(em_ha_client.IntentEnded(
                speech=data.get("speech", ""),
                conversation_id=data.get("conversation_id") or None,
                continue_conversation=data.get("continue_conversation") == "1",
                response_type=None,
                processed_locally=None,
            ))
        elif typ in (ET.VOICE_ASSISTANT_TTS_END, ET.VOICE_ASSISTANT_RUN_START):
            if data.get("url"):
                queue.put_nowait(em_ha_client.TtsReady(
                    data["url"], typ == ET.VOICE_ASSISTANT_RUN_START))
        elif typ == ET.VOICE_ASSISTANT_ERROR:
            queue.put_nowait(em_ha_client.RunFailed(
                data.get("code", "unknown"), data.get("message", "")))
        elif typ == ET.VOICE_ASSISTANT_RUN_END:
            queue.put_nowait(em_ha_client.RunEnded())

    async def esphome_reply(self, pcm: bytes) -> AsyncIterator[object]:
        """Run STT→intent→TTS for a committed HA-started reply (§16.7)."""
        if not self._transport or self._transport.is_closing():
            yield em_ha_client.RunFailed("satellite_unavailable", "HA is not attached")
            yield em_ha_client.RunEnded()
            return
        async with self._reply_lock:
            queue: asyncio.Queue[object] = asyncio.Queue()
            self._run_queue = queue
            try:
                self._send_one(api_pb2.VoiceAssistantRequest(
                    start=True, conversation_id=str(uuid.uuid4()),
                    flags=VOICE_REQUEST_FLAGS_WAKE_WORD_DONE))
                for offset in range(0, len(pcm), REPLY_CHUNK_BYTES):
                    self._send_one(api_pb2.VoiceAssistantAudio(
                        data=bytes(pcm[offset:offset + REPLY_CHUNK_BYTES])))
                self._send_one(api_pb2.VoiceAssistantAudio(data=b"", end=True))
                progressed = False
                while True:
                    event = await queue.get()
                    # HA can emit a stray RUN_END before the run has progressed;
                    # only one after INTENT_END or ERROR is terminal.
                    if isinstance(event, em_ha_client.RunEnded) and not progressed:
                        continue
                    if isinstance(event, (em_ha_client.IntentEnded, em_ha_client.RunFailed)):
                        progressed = True
                    yield event
                    if isinstance(event, (em_ha_client.RunEnded, em_ha_client.RunLost)):
                        return
            finally:
                self._run_queue = None

    def _media_state_msg(self) -> Any:
        state = _MP_STATE.get(em_player.reported_state(self.device_id), MediaPlayerState.IDLE)
        return api_pb2.MediaPlayerStateResponse(
            key=MEDIA_PLAYER_KEY, state=state, volume=self.server.volume, muted=False)

    def _ambient_state_msg(self) -> Any:
        lux = self.server.ambient_lux
        return api_pb2.SensorStateResponse(
            key=AMBIENT_LUX_KEY, state=float(lux) if lux is not None else 0.0,
            missing_state=lux is None)

    def _alert_state_msg(self) -> Any:
        ringing = self.server.alert_ringing
        return api_pb2.BinarySensorStateResponse(
            key=ALERT_RINGING_KEY, state=bool(ringing), missing_state=ringing is None)

    def _spawn(self, awaitable: Awaitable[Any], name: str) -> asyncio.Task:
        task = asyncio.create_task(awaitable, name=f"{name}:{self.device_id}")
        task.add_done_callback(_task_error)
        return task

    def disconnected(self) -> None:
        if self._run_queue is not None:
            self._run_queue.put_nowait(em_ha_client.RunLost())


class DeviceESPhomeServer:
    """One persisted ESPHome port with a single active HA claimant."""

    def __init__(self, device_id: str, label: str, mac_address: str, port: int) -> None:
        self.device_id = device_id
        self.label = label
        self.mac_address = mac_address
        self.port = port
        self.capabilities: frozenset[str] = frozenset()
        self.volume = 1.0
        self.ambient_lux: int | None = None
        self.alert_ringing: bool | None = None
        self.hooks = Hooks()
        self._server: asyncio.AbstractServer | None = None
        self._active_satellite: EchoMuseSatellite | None = None
        self._mdns_info: ServiceInfo | None = None

    def _protocol_factory(self):
        if self._active_satellite is not None:
            return _RejectProtocol()
        satellite = EchoMuseSatellite(self)
        self._active_satellite = satellite
        return satellite

    def _on_satellite_disconnected(self, satellite: EchoMuseSatellite) -> None:
        satellite.disconnected()
        if self._active_satellite is satellite:
            self._active_satellite = None

    async def start(self, host: str) -> None:
        if self._server is None:
            self._server = await serve(self._protocol_factory, host, self.port)

    async def stop(self) -> None:
        satellite, self._active_satellite = self._active_satellite, None
        if satellite is not None:
            satellite.disconnected()
            satellite.close()
        server, self._server = self._server, None
        if server is not None:
            server.close()
            await server.wait_closed()


class _RejectProtocol(SatelliteServerProtocol):
    def __init__(self) -> None:
        super().__init__(server_name="reject", log_name="esphome.reject")
        self._rejected = False

    def handle_message(self, _msg):
        if not self._rejected:
            self._rejected = True
            yield api_pb2.DisconnectResponse()
            self.close()


_servers: dict[str, DeviceESPhomeServer] = {}
_pending_caps: dict[str, frozenset[str]] = {}
_azc: AsyncZeroconf | None = None
_host = "0.0.0.0"


async def start_esphome_servers(devices: dict, host: str = "0.0.0.0") -> None:
    """Register one stock ESPHome satellite for every approved endpoint."""
    del devices
    global _azc, _host
    _host = host
    _azc = AsyncZeroconf()
    rows = await asyncio.to_thread(db.get_all_devices)
    for row in rows:
        if row["approved"]:
            await _register_device_server(row["device_id"], row["label"])


async def _register_device_server(device_id: str, label: str | None) -> DeviceESPhomeServer:
    current = _servers.get(device_id)
    if current is not None:
        return current
    label = label or f"EchoMuse {device_id[-8:]}"
    port = await asyncio.to_thread(db.get_esphome_port, device_id)
    if port is None:
        port = await asyncio.to_thread(db.assign_esphome_port, device_id)
    server = DeviceESPhomeServer(device_id, label, _serialno_to_mac(device_id), port)
    server.capabilities = _pending_caps.get(device_id, frozenset())
    _servers[device_id] = server
    info = _make_device_mdns_info(device_id, label, port)
    if _azc is not None:
        try:
            await _azc.async_register_service(info, allow_name_change=True)
            server._mdns_info = info
        except Exception as error:
            log.warning("[%s] ESPHome mDNS registration failed: %s", device_id, error)
    return server


async def stop_esphome_servers() -> None:
    global _azc
    for server in list(_servers.values()):
        if server._mdns_info is not None and _azc is not None:
            with contextlib.suppress(Exception):
                await _azc.async_unregister_service(server._mdns_info)
        await server.stop()
    _servers.clear()
    if _azc is not None:
        await _azc.async_close()
        _azc = None


def get_server(device_id: str) -> DeviceESPhomeServer | None:
    return _servers.get(device_id)


async def device_connected(
    device_id: str,
    *,
    label: str,
    capabilities: frozenset[str],
    hooks: Hooks,
    volume: float | None = None,
    ambient_lux: int | None = None,
) -> None:
    server = _servers.get(device_id)
    if server is None:
        row = await asyncio.to_thread(db.get_device, device_id)
        if row is None or not row["approved"] or _azc is None:
            return
        server = await _register_device_server(device_id, label)
    before = server.capabilities
    server.label = label
    server.capabilities = frozenset(capabilities)
    _pending_caps[device_id] = server.capabilities
    server.hooks = hooks
    server.ambient_lux = ambient_lux
    if volume is not None:
        server.volume = max(0.0, min(1.0, volume))
    if before != server.capabilities and server._active_satellite is not None:
        server._active_satellite.close()
    await server.start(_host)


async def device_disconnected(device_id: str) -> None:
    server = _servers.get(device_id)
    if server is None:
        return
    server.hooks = Hooks()
    await server.stop()


def set_device_capabilities(device_id: str, caps: list[str] | frozenset[str]) -> None:
    capabilities = frozenset(caps or ())
    _pending_caps[device_id] = capabilities
    server = _servers.get(device_id)
    if server is not None:
        before = server.capabilities
        server.capabilities = capabilities
        if before != capabilities and server._active_satellite is not None:
            server._active_satellite.close()


def send_button_event(device_id: str, event_type: str) -> None:
    satellite = _satellite(device_id)
    if satellite is not None:
        satellite._send_one(api_pb2.EventResponse(key=EVENT_KEY, event_type=event_type))


def update_ambient_lux(device_id: str, lux: int | None) -> None:
    server = _servers.get(device_id)
    if server is None:
        return
    server.ambient_lux = lux
    satellite = server._active_satellite
    if satellite is not None:
        satellite._send_one(satellite._ambient_state_msg())


def update_device_volume(device_id: str, volume: float) -> None:
    server = _servers.get(device_id)
    if server is None:
        return
    server.volume = max(0.0, min(1.0, volume))
    satellite = server._active_satellite
    if satellite is not None:
        satellite._send_one(satellite._media_state_msg())


def update_alert_ringing(device_id: str, ringing: bool | None) -> None:
    server = _servers.get(device_id)
    if server is None:
        return
    server.alert_ringing = ringing
    satellite = server._active_satellite
    if satellite is not None:
        satellite._send_one(satellite._alert_state_msg())


async def push_media_state(device_id: str, _state: str) -> None:
    satellite = _satellite(device_id)
    if satellite is not None:
        satellite._send_one(satellite._media_state_msg())


async def esphome_reply(device_id: str, pcm: bytes) -> AsyncIterator[object]:
    """Committed HA-started reply path consumed by ``SessionActor``."""
    satellite = _satellite(device_id)
    if satellite is None:
        yield em_ha_client.RunFailed("satellite_unavailable", "HA is not attached")
        yield em_ha_client.RunEnded()
        return
    async for event in satellite.esphome_reply(pcm):
        yield event


def _satellite(device_id: str) -> EchoMuseSatellite | None:
    server = _servers.get(device_id)
    return server._active_satellite if server is not None else None


def _mdns_service_name(device_id: str) -> str:
    return f"echomuse-{device_id[-12:].lower()}"


def _make_device_mdns_info(device_id: str, label: str, port: int) -> ServiceInfo:
    name = _mdns_service_name(device_id)
    return ServiceInfo(
        "_esphomelib._tcp.local.", f"{name}._esphomelib._tcp.local.",
        addresses=[socket.inet_aton(SERVER_IP)], port=port,
        properties={
            "version": ESPHOME_PROJECT_VERSION,
            "friendly_name": f"{label} Voice Assistant",
            "mac": _serialno_to_mac(device_id).replace(":", "").lower(),
            "network": "ethwifi",
            "project_name": f"EchoMuse.{ESPHOME_DEVICE_MODEL}",
            "project_version": ESPHOME_PROJECT_VERSION,
        },
        server=f"{name}.local.",
    )


def _serialno_to_mac(device_id: str) -> str:
    chars = "".join(c for c in device_id if c in "0123456789abcdefABCDEF")[-12:].zfill(12)
    return ":".join(chars[i:i + 2].upper() for i in range(0, 12, 2))


def _task_error(task: asyncio.Task) -> None:
    if task.cancelled():
        return
    try:
        error = task.exception()
    except asyncio.CancelledError:
        return
    if error is not None:
        log.error("task %s failed: %s", task.get_name(), error,
                  exc_info=(type(error), error, error.__traceback__))
