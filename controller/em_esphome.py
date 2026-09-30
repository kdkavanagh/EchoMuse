"""Stock ESPHome satellite surface for one EchoMuse endpoint (SPEC §16.7).

The ESPHome voice path is deliberately narrow: only a committed reply to an
HA-started conversation enters it.  Ordinary wake/button turns use
``em_ha_client`` websocket pipeline runs owned by ``SessionActor``, so HA's
own assist-satellite state never sees them; the “Voice state” sensor shows
the actor's phase instead.
"""

from __future__ import annotations

import asyncio
import logging
import os
import socket
import uuid
from collections.abc import AsyncIterator, Callable, Coroutine, Iterator
from dataclasses import dataclass

from google.protobuf.message import Message
from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

import em_db as db
import em_ha_client
import em_player
import em_volume
from em_alerts import TimerEvent
from em_button import ButtonEvent
from em_device_link import Capability
from em_session import VoicePhase
from esphome.feature_flags import MediaPlayerEntityFeature, MediaPlayerState, VoiceAssistantFeature
from esphome.satellite_server import HANDLED, RejectProtocol, Reply, SatelliteServerProtocol, serve
from esphome.vendor import api_pb2
from version import VERSION as CONTROLLER_VERSION

log = logging.getLogger("echomuse.esphome")

ESPHOME_DEVICE_MODEL = "Echo Dot Gen 2 (biscuit)"
ESPHOME_PROJECT_VERSION = os.environ.get("ESPHOME_PROJECT_VERSION", CONTROLLER_VERSION)

MEDIA_PLAYER_KEY = 1
EVENT_KEY = 2
AMBIENT_LUX_KEY = 3
ALERT_RINGING_KEY = 4
STOP_ALERT_KEY = 5
VOICE_STATE_KEY = 6
BUTTON_EVENT_TYPES = [e.value for e in ButtonEvent]

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
    em_player.PlayerState.IDLE: MediaPlayerState.IDLE,
    em_player.PlayerState.PLAYING: MediaPlayerState.PLAYING,
    em_player.PlayerState.PAUSED: MediaPlayerState.PAUSED,
}
_TE = api_pb2.VoiceAssistantTimerEvent
_TIMER_EVENTS: dict[int, TimerEvent] = {
    _TE.VOICE_ASSISTANT_TIMER_STARTED: TimerEvent.STARTED,
    _TE.VOICE_ASSISTANT_TIMER_UPDATED: TimerEvent.UPDATED,
    _TE.VOICE_ASSISTANT_TIMER_CANCELLED: TimerEvent.CANCELLED,
    _TE.VOICE_ASSISTANT_TIMER_FINISHED: TimerEvent.FINISHED,
}
_ET = api_pb2.VoiceAssistantEvent

# A hook's coroutine is spawned (or, for announce, awaited); its result is unused.
HookCoro = Coroutine[object, object, object]


@dataclass(frozen=True, slots=True)
class Hooks:
    announce: Callable[[str, str | None, bool], HookCoro] | None = None
    set_volume: Callable[[int], HookCoro] | None = None
    stop_alert: Callable[[], HookCoro] | None = None
    timer_event: Callable[[TimerEvent, str, str, int, int, bool], HookCoro] | None = None
    attached: Callable[[], HookCoro] | None = None


class EchoMuseSatellite(SatelliteServerProtocol):
    """One HA connection. Voice audio is accepted only via ``esphome_reply``."""

    def __init__(self, server: "DeviceESPhomeServer") -> None:
        super().__init__(
            server_name=satellite_name(server.device_id),
            log_name=f"esphome.{server.device_id[-8:]}",
        )
        self.server = server
        self._run_queue: asyncio.Queue[em_ha_client.RunEvent] | None = None
        self._reply_lock = asyncio.Lock()
        self._tasks: set[asyncio.Task[object]] = set()

    @property
    def device_id(self) -> str:
        return self.server.device_id

    def _device_has(self, capability: Capability) -> bool:
        return capability in self.server.capabilities

    def connection_closed(self) -> None:
        self.server._on_satellite_disconnected(self)

    def handle_message(self, msg: Message) -> Iterator[Reply]:
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
            if self._device_has(Capability.BUTTON_HOLD):
                yield api_pb2.ListEntitiesEventResponse(
                    object_id="action_button", key=EVENT_KEY,
                    name=f"{self.server.label} Action Button", device_class="button",
                    event_types=BUTTON_EVENT_TYPES,
                )
            if self._device_has(Capability.AMBIENT_LIGHT):
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
            yield api_pb2.ListEntitiesTextSensorResponse(
                object_id="voice_state", key=VOICE_STATE_KEY,
                name=f"{self.server.label} Voice state", icon="mdi:account-voice",
            )
            yield api_pb2.ListEntitiesDoneResponse()
            return

        if isinstance(msg, (api_pb2.SubscribeStatesRequest,
                            api_pb2.SubscribeHomeAssistantStatesRequest)):
            yield self._media_state_msg()
            if self._device_has(Capability.AMBIENT_LIGHT):
                yield self._ambient_state_msg()
            yield self._alert_state_msg()
            yield self._voice_state_msg()
            return

        if isinstance(msg, api_pb2.SubscribeVoiceAssistantRequest):
            if msg.subscribe and self.server.hooks.attached is not None:
                self._spawn(self.server.hooks.attached(), "satellite-attached")
            yield HANDLED
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
            yield HANDLED
            return

        if isinstance(msg, api_pb2.SubscribeHomeassistantServicesRequest):
            yield HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantResponse):
            yield HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantEventResponse):
            self._handle_voice_event(msg)
            yield HANDLED
            return

        if isinstance(msg, api_pb2.VoiceAssistantAnnounceRequest):
            self._spawn(self._announce(msg.media_id, msg.preannounce_media_id or None,
                                       bool(msg.start_conversation)), "announce")
            yield api_pb2.MediaPlayerStateResponse(
                key=MEDIA_PLAYER_KEY, state=MediaPlayerState.ANNOUNCING,
                volume=self.server.volume, muted=False)
            return

        if isinstance(msg, api_pb2.VoiceAssistantTimerEventResponse):
            self._handle_timer(msg)
            yield HANDLED
            return

    def _handle_media_command(self, msg: api_pb2.MediaPlayerCommandRequest) -> None:
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

    def _handle_timer(self, msg: api_pb2.VoiceAssistantTimerEventResponse) -> None:
        event = _TIMER_EVENTS.get(msg.event_type)
        if event is None or self.server.hooks.timer_event is None:
            return
        self._spawn(self.server.hooks.timer_event(
            event, msg.timer_id, msg.name or "timer", int(msg.total_seconds),
            int(msg.seconds_left), bool(msg.is_active)), "timer-event")

    def _handle_voice_event(self, msg: api_pb2.VoiceAssistantEventResponse) -> None:
        queue = self._run_queue
        if queue is None:
            return
        data: dict[str, str] = {item.name: item.value for item in msg.data}
        typ = msg.event_type
        if typ == _ET.VOICE_ASSISTANT_STT_END:
            queue.put_nowait(em_ha_client.SttEnded(data.get("text", "")))
        elif typ == _ET.VOICE_ASSISTANT_INTENT_END:
            queue.put_nowait(em_ha_client.IntentEnded(
                speech=data.get("speech", ""),
                conversation_id=data.get("conversation_id") or None,
                continue_conversation=data.get("continue_conversation") == "1",
                response_type=None,
                processed_locally=None,
            ))
        elif typ in (_ET.VOICE_ASSISTANT_TTS_END, _ET.VOICE_ASSISTANT_RUN_START):
            if data.get("url"):
                queue.put_nowait(em_ha_client.TtsReady(
                    data["url"], typ == _ET.VOICE_ASSISTANT_RUN_START))
        elif typ == _ET.VOICE_ASSISTANT_ERROR:
            queue.put_nowait(em_ha_client.RunFailed(
                data.get("code", "unknown"), data.get("message", "")))
        elif typ == _ET.VOICE_ASSISTANT_RUN_END:
            queue.put_nowait(em_ha_client.RunEnded())

    async def esphome_reply(self, pcm: bytes) -> AsyncIterator[em_ha_client.RunEvent]:
        """Run STT→intent→TTS for a committed HA-started reply (§16.7)."""
        if not self._transport or self._transport.is_closing():
            yield em_ha_client.RunFailed("satellite_unavailable", "HA is not attached")
            yield em_ha_client.RunEnded()
            return
        async with self._reply_lock:
            queue: asyncio.Queue[em_ha_client.RunEvent] = asyncio.Queue()
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

    def _media_state_msg(self) -> Message:
        state = _MP_STATE.get(em_player.reported_state(self.device_id), MediaPlayerState.IDLE)
        return api_pb2.MediaPlayerStateResponse(
            key=MEDIA_PLAYER_KEY, state=state, volume=self.server.volume, muted=False)

    def _ambient_state_msg(self) -> Message:
        lux = self.server.ambient_lux
        return api_pb2.SensorStateResponse(
            key=AMBIENT_LUX_KEY, state=float(lux) if lux is not None else 0.0,
            missing_state=lux is None)

    def _alert_state_msg(self) -> Message:
        ringing = self.server.alert_ringing
        return api_pb2.BinarySensorStateResponse(
            key=ALERT_RINGING_KEY, state=bool(ringing), missing_state=ringing is None)

    def _voice_state_msg(self) -> Message:
        return api_pb2.TextSensorStateResponse(key=VOICE_STATE_KEY, state=self.server.voice_phase)

    def _spawn(self, coro: HookCoro, name: str) -> None:
        # Held until done: the loop keeps only weak references to tasks.
        task = asyncio.create_task(coro, name=f"{name}:{self.device_id}")
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        task.add_done_callback(_task_error)

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
        self.voice_phase = VoicePhase.IDLE
        self.hooks = Hooks()
        self._server: asyncio.AbstractServer | None = None
        self._active_satellite: EchoMuseSatellite | None = None
        self._mdns_info: ServiceInfo | None = None

    def _protocol_factory(self) -> SatelliteServerProtocol:
        if self._active_satellite is not None:
            return RejectProtocol()
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


_servers: dict[str, DeviceESPhomeServer] = {}
_mdns: Mdns | None = None
_host = "0.0.0.0"


@dataclass(frozen=True, slots=True)
class Mdns:
    """The mDNS responder (shared with the BT proxy, ``em_ble_proxy``) and the
    controller address its records advertise."""

    zc: AsyncZeroconf
    address: str

    def service_info(self, service_name: str, friendly_name: str, mac_address: str,
                     port: int) -> ServiceInfo:
        """``_esphomelib._tcp`` advertisement for one EchoMuse ESPHome node. The
        ``mac`` TXT is mandatory for HA discovery and must match the node's
        DeviceInfoResponse.mac_address."""
        return ServiceInfo(
            "_esphomelib._tcp.local.", f"{service_name}._esphomelib._tcp.local.",
            addresses=[socket.inet_aton(self.address)], port=port,
            properties={
                "version": ESPHOME_PROJECT_VERSION,
                "friendly_name": friendly_name,
                "mac": mac_address.replace(":", "").lower(),
                "network": "ethwifi",
                "project_name": f"EchoMuse.{ESPHOME_DEVICE_MODEL}",
                "project_version": ESPHOME_PROJECT_VERSION,
            },
            server=f"{service_name}.local.",
        )


def mdns() -> Mdns | None:
    """The running mDNS responder, or None before start / after stop."""
    return _mdns


async def start_esphome_servers(host: str, advertise_ip: str) -> None:
    """Register one stock ESPHome satellite for every approved endpoint,
    advertised over mDNS at `advertise_ip`."""
    global _mdns, _host
    _host = host
    _mdns = Mdns(AsyncZeroconf(), advertise_ip)
    rows = await asyncio.to_thread(db.get_all_devices)
    for row in rows:
        if row.approved:
            await _register_device_server(row.device_id, row.label)


async def _register_device_server(device_id: str, label: str | None) -> DeviceESPhomeServer:
    current = _servers.get(device_id)
    if current is not None:
        return current
    label = label or f"EchoMuse {device_id[-8:]}"
    port = await asyncio.to_thread(db.get_esphome_port, device_id)
    if port is None:
        port = await asyncio.to_thread(db.assign_esphome_port, device_id)
    server = DeviceESPhomeServer(device_id, label, serialno_to_mac(device_id), port)
    _servers[device_id] = server
    if _mdns is not None:
        info = _mdns.service_info(satellite_name(device_id), f"{label} Voice Assistant",
                                  server.mac_address, port)
        try:
            await _mdns.zc.async_register_service(info, allow_name_change=True)
            server._mdns_info = info
        except Exception as error:
            log.warning("[%s] ESPHome mDNS registration failed: %s", device_id, error)
    return server


async def stop_esphome_servers() -> None:
    global _mdns
    for server in list(_servers.values()):
        if server._mdns_info is not None and _mdns is not None:
            try:
                await _mdns.zc.async_unregister_service(server._mdns_info)
            except Exception as error:
                log.warning("[%s] ESPHome mDNS unregistration failed: %s",
                            server.device_id, error)
        await server.stop()
    _servers.clear()
    if _mdns is not None:
        await _mdns.zc.async_close()
        _mdns = None


async def device_connected(
    device_id: str,
    *,
    label: str,
    capabilities: frozenset[str],
    hooks: Hooks,
    volume: float | None = None,
    ambient_lux: int | None = None,
    voice_phase: VoicePhase,
) -> None:
    server = _servers.get(device_id)
    if server is None:
        row = await asyncio.to_thread(db.get_device, device_id)
        if row is None or not row.approved or _mdns is None:
            return
        server = await _register_device_server(device_id, label)
    before = server.capabilities
    server.label = label
    server.capabilities = frozenset(capabilities)
    server.hooks = hooks
    server.ambient_lux = ambient_lux
    server.voice_phase = voice_phase
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


def update_voice_phase(device_id: str, phase: VoicePhase) -> None:
    server = _servers.get(device_id)
    if server is None or server.voice_phase is phase:
        return
    server.voice_phase = phase
    satellite = server._active_satellite
    if satellite is not None:
        satellite._send_one(satellite._voice_state_msg())


async def push_media_state(device_id: str, _state: em_player.PlayerState) -> None:
    satellite = _satellite(device_id)
    if satellite is not None:
        satellite._send_one(satellite._media_state_msg())


async def esphome_reply(device_id: str, pcm: bytes) -> AsyncIterator[em_ha_client.RunEvent]:
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


def satellite_name(device_id: str) -> str:
    """The voice satellite's ESPHome node name (mDNS instance and HelloResponse)."""
    return f"echomuse-{device_id[-12:].lower()}"


def serialno_to_mac(device_id: str) -> str:
    chars = "".join(c for c in device_id if c in "0123456789abcdefABCDEF")[-12:].zfill(12)
    return ":".join(chars[i:i + 2].upper() for i in range(0, 12, 2))


def _task_error(task: asyncio.Task[object]) -> None:
    if task.cancelled():
        return
    try:
        error = task.exception()
    except asyncio.CancelledError:
        return
    if error is not None:
        log.error("task %s failed: %s", task.get_name(), error,
                  exc_info=(type(error), error, error.__traceback__))
