"""Controller-owned device state and v1 link admission (SPEC §11, §16.1).

A ``Device`` outlives transport sessions.  The link owns framing and liveness;
this module owns admission, message routing, retained state, button policy, and
projection of actor state onto the controller LED layer.
"""

from __future__ import annotations

import asyncio
import contextlib
import enum
import json
import logging
import math
import time
from collections.abc import Awaitable, Callable, Iterable, Mapping
from dataclasses import dataclass
from typing import Any, Protocol

import numpy as np
from websockets.asyncio.server import ServerConnection

import em_ambient
import em_button
import em_capture
import em_config_sections
import em_db
import em_device_assets
import em_device_link
import em_linkauth
import em_render
import em_samples
import em_scenes
import em_session
import em_speech_worker
import em_tap_burst
import em_volume
import em_wake_registry
from em_device_link import Capability, CloseReason, CommandAck, MessageType, RejectReason

log = logging.getLogger("em_device")

REQUIRED_CAPABILITIES: frozenset[Capability] = frozenset({
    Capability.AUDIO_TIMELINE,
    Capability.UPLINK_LEASES,
    Capability.DEVICE_WAKE,
    Capability.RENDER_REFERENCE,
    Capability.RENDER_PROGRESS,
    Capability.FOCUS_LEASES,
    Capability.ALERT_CACHE,
    Capability.TURN_PROTOCOL,
})
ACTOR_MESSAGES: frozenset[MessageType] = frozenset({
    MessageType.WAKE_CANDIDATE, MessageType.WAKE_CANDIDATE_END, MessageType.UPLINK_ENDED,
    MessageType.STREAM_OPEN, MessageType.STREAM_END, MessageType.PRIVACY_CHANGED,
    MessageType.COMMAND_ACK, MessageType.ALERT_STATE,
})
# Retained device stats (§4.8 `stats`), in the order the dashboard reads them.
STATS_KEYS = (
    "cpuPct", "memUsedMb", "memTotalMb", "storageUsedMb", "storageTotalMb",
    "wifiRssi", "wifiSsid", "linkSpeedMbps", "wifiFreqMhz", "wifiBssid",
    "txBytes", "rxBytes", "txErrors", "txDropped", "rxCrcErrors", "ble",
    "cpuTempC", "maxTempC", "coresOnline", "coresTotal", "thermalCoreLimit",
    "ambientLux",
)
DEVICE_CONFIG_KEYS = ("startupVolume", "duckDb", "bleProxyEnabled")
DEFAULT_DUCK_DB = -18.0
PING_INTERVAL_SECONDS = 5.0
PING_TIMEOUT_SECONDS = 60.0
RTT_EXCURSION_MS = 200
LED_RENEW_SECONDS = 10.0
LED_TTL_RENEW_MAX_S = 30
LED_CUE_SECONDS = 1.0
BUTTON_HOLD_MS = 750
NUM_LEDS = 12
ALERT_STOPPED = "alert_stopped"          # `button.action.handled` (WIRE §4.6)

LedSpec = dict[str, Any]                 # one em_scenes layer: the `led_anim.anim` wire object


class ApprovalMode(enum.StrEnum):
    """The `device_approval` system setting."""

    STRICT = "strict"
    AUTO = "auto"


class Actor(Protocol):
    @property
    def state(self) -> em_session.ActorState: ...
    @property
    def awaiting_intent(self) -> bool: ...
    @property
    def turn_active(self) -> bool: ...

    async def start(self) -> None: ...
    async def close(self) -> None: ...
    def attach(self, link: em_device_link.DeviceLink, render: em_render.RenderClient) -> None: ...
    def detach(self, reason: CloseReason) -> None: ...
    def on_message(self, envelope: em_device_link.Envelope) -> None: ...
    def on_audio(self, frame: bytes) -> None: ...
    def on_observation(self, obs: em_speech_worker.Observation) -> None: ...
    def button_turn(self, action: Mapping[str, object]) -> None: ...
    def cancel_turn(self) -> None: ...
    def add_listener(self, cb: Callable[[em_session.ActorEvent], object]) -> Callable[[], None]: ...
    async def announce(self, url: str, *, preannounce_url: str | None,
                       start_conversation: bool) -> None: ...
    async def open_diagnostic(self, on_mic: Callable[[int, np.ndarray], None]) -> None: ...
    async def close_diagnostic(self) -> None: ...


class Alerts(Protocol):
    async def on_session_hello(self, endpoint_id: str, alerts: Mapping[str, object], *,
                               capabilities: Iterable[str] = ()) -> None: ...
    def on_session_lost(self, endpoint_id: str) -> None: ...
    async def on_alert_ack(self, endpoint_id: str, body: Mapping[str, object]) -> None: ...
    def on_alert_state(self, endpoint_id: str, body: Mapping[str, object]) -> None: ...
    def on_alert_ring_ended(self, endpoint_id: str, body: Mapping[str, object]) -> None: ...
    def on_command_ack(self, endpoint_id: str, ack: CommandAck) -> None: ...
    async def on_local_operation(self, endpoint_id: str, body: Mapping[str, object]) -> None: ...


class Host(Protocol):
    """Effects routed back into ``em_controller`` to keep Device testable."""

    @property
    def alerts(self) -> Alerts: ...

    def make_actor(self, device_id: str) -> Actor: ...
    def make_render(self, device: Device, link: em_device_link.DeviceLink) -> em_render.RenderClient: ...
    async def connected(self, device: Device) -> None: ...
    async def disconnected(self, device: Device) -> None: ...
    async def pending(self, device_id: str, ip: str) -> None: ...
    async def push_state(self, device: Device, state: Mapping[str, object]) -> None: ...
    async def push_log(self, device_id: str, level: em_db.LogLevel, message: str) -> None: ...
    def button_event(self, device_id: str, event_type: em_button.ButtonEvent) -> None: ...
    def ambient_lux(self, device_id: str, lux: int | None) -> None: ...
    def voice_phase(self, device_id: str, phase: em_session.VoicePhase) -> None: ...
    def volume(self, device_id: str, value: float) -> None: ...
    def ble_adverts(self, device_id: str, adverts: list[object]) -> None: ...
    def ble_stats(self, device_id: str, stats: object) -> None: ...
    def player_busy(self, device_id: str) -> bool: ...
    def player_gone(self, device_id: str) -> None: ...
    def dialog_released(self, device_id: str) -> Awaitable[None]: ...
    def wifi_result(self, device_id: str, ok: bool, ssid: str, error: str) -> dict[str, object]: ...


class Store(Protocol):
    def device(self, device_id: str) -> em_db.DeviceRow | None: ...
    def token(self, device_id: str) -> str | None: ...
    def approval_mode(self, default: ApprovalMode) -> ApprovalMode: ...
    def register(self, device_id: str, ip: str, version: str | None) -> None: ...
    def approve(self, device_id: str, label: str) -> None: ...
    def seen(self, device_id: str, ip: str, version: str | None) -> None: ...
    def config(self, device_id: str) -> em_config_sections.DeviceConfig: ...
    def set_config(self, device_id: str, config: em_config_sections.DeviceConfig) -> None: ...
    def log(self, device_id: str, level: em_db.LogLevel, source: em_db.LogSource,
            message: str) -> None: ...
    def stats(self, device_id: str, stats: Mapping[str, object]) -> None: ...
    def wake_stats(self, device_id: str, body: Mapping[str, object]) -> None: ...


class WakeModels(Protocol):
    def for_config(self, config: Mapping[str, object]) -> em_wake_registry.WakeModel: ...


class SpeechAssetNamer(Protocol):
    def speech_assets(self, model: em_wake_registry.WakeModel) -> em_device_assets.SpeechAssets: ...


class DbStore:
    """Thin synchronous adapter; callers use ``asyncio.to_thread``."""

    def device(self, device_id: str) -> em_db.DeviceRow | None:
        return em_db.get_device(device_id)

    def token(self, device_id: str) -> str | None:
        return em_db.get_device_token(device_id)

    def approval_mode(self, default: ApprovalMode) -> ApprovalMode:
        stored = em_db.get_config(em_db.SystemConfigKey.DEVICE_APPROVAL, None)
        if not stored:
            return default
        return ApprovalMode.AUTO if stored == ApprovalMode.AUTO else ApprovalMode.STRICT

    def register(self, device_id: str, ip: str, version: str | None) -> None:
        em_db.register_new_device(device_id, ip, version)

    def approve(self, device_id: str, label: str) -> None:
        em_db.approve_device(device_id, label, None)

    def seen(self, device_id: str, ip: str, version: str | None) -> None:
        em_db.upsert_device_seen(device_id, ip, version)

    def config(self, device_id: str) -> em_config_sections.DeviceConfig:
        return em_db.get_effective_device_config(device_id)

    def set_config(self, device_id: str, config: em_config_sections.DeviceConfig) -> None:
        em_db.set_device_config(device_id, config)

    def log(self, device_id: str, level: em_db.LogLevel, source: em_db.LogSource,
            message: str) -> None:
        em_db.log_device(device_id, level, source, message)

    def stats(self, device_id: str, stats: Mapping[str, object]) -> None:
        em_db.record_device_stats(device_id, stats)
        em_db.touch_device_seen(device_id)

    def wake_stats(self, device_id: str, body: Mapping[str, object]) -> None:
        near = body.get("near_misses")
        episodes = near if isinstance(near, list) else []
        peaks = [peak for e in episodes if isinstance(e, dict)
                 if (peak := _number(e.get("peak"))) is not None]
        em_db.bump_wake_counters(
            device_id,
            near_misses=len(episodes),
            near_miss_max=max(peaks) if peaks else None,
            dev_hops=_integer(body.get("hops_scored")) or 0,
            dev_drops=_integer(body.get("hops_dropped")) or 0,
            dev_crossings=_integer(body.get("candidates_opened")) or 0,
            dev_max_score=_number(body.get("peak_smoothed")),
            dev_max_infer_ms=_integer(body.get("infer_max_ms")),
        )


@dataclass(frozen=True)
class Registration:
    """Admitted (`rejected` None, with the label) or refused."""

    rejected: RejectReason | None
    label: str | None


async def register_device(
    store: Store,
    host: Host,
    *,
    device_id: str,
    ip: str,
    version: str | None,
    secure: bool,
    token: str | None,
    require_tls: bool,
    approval_default: ApprovalMode,
) -> Registration:
    """Authenticate and apply the retained registration/approval policy."""
    expected = await asyncio.to_thread(store.token, device_id)
    verdict = em_linkauth.decide(
        presented=token, expected=expected, secure=secure, require_tls=require_tls)
    if not verdict.ok:
        log.warning("[%s] link rejected: %s", device_id, verdict.reason)
        return Registration(RejectReason.UNAUTHORIZED, None)
    if verdict.stale_token:
        log.warning("[%s] stale device token; treating it as unregistered", device_id)

    row = await asyncio.to_thread(store.device, device_id)
    approval = await asyncio.to_thread(store.approval_mode, approval_default)
    if row is None:
        await asyncio.to_thread(store.register, device_id, ip, version)
        if approval is not ApprovalMode.AUTO:
            await host.pending(device_id, ip)
            return Registration(RejectReason.PENDING_APPROVAL, None)
        label = f"Unknown {device_id[:8]}"
        await asyncio.to_thread(store.approve, device_id, label)
        row = await asyncio.to_thread(store.device, device_id)
    if row is None or not row.approved:
        await asyncio.to_thread(store.seen, device_id, ip, version)
        await host.pending(device_id, ip)
        return Registration(RejectReason.PENDING_APPROVAL, None)

    await asyncio.to_thread(store.seen, device_id, ip, version)
    return Registration(None, row.label or f"EchoMuse {device_id[-8:]}")


class Device:
    """One registered endpoint, retaining state while it is offline."""

    def __init__(self, device_id: str, label: str, actor: Actor, host: Host,
                 store: Store | None = None):
        self.device_id = device_id
        self.label = label
        self.actor = actor
        self.host = host
        self.store = store or DbStore()
        self.link: em_device_link.DeviceLink | None = None
        self.render: em_render.RenderClient | None = None
        self.legacy_ws: ServerConnection | None = None
        self.capabilities: frozenset[str] = frozenset()
        self.missing_capabilities: frozenset[str] = frozenset()
        self.firmware_version: str | None = None
        self.ip: str | None = None
        self.secure = False
        self.upgrade_required = False
        self.stats: dict[str, object] | None = None
        self.wake_stats: dict[str, object] | None = None
        self.alert_state: dict[str, object] | None = None
        self.alerts_wakeup: str | None = None
        self.volume: int | None = None
        self.muted: bool | None = None
        self.ambient: dict[str, object] | None = None
        self.wifi_scan_future: asyncio.Future[dict[str, object]] | None = None
        self.config: em_config_sections.DeviceConfig = {}
        self.dialog_active = False
        self.led_scene: LedSpec = em_scenes.resolve({})
        self.rtt_last_ms: int | None = None
        self._rtt_sum_ms = 0
        self._rtt_count = 0
        self._rtt_min_ms: int | None = None
        self._rtt_max_ms: int | None = None
        self._rtt_excursions = 0
        self._rtt_excursions_idle = 0
        self._rtt_samples_idle = 0
        self._ping_seq = 0
        self._ping_sent: dict[int, tuple[float, bool]] = {}
        self._ping_task: asyncio.Task[None] | None = None
        self._physical_seq = -1
        self._background: set[asyncio.Future[object]] = set()
        self._actor_remove = actor.add_listener(self._on_actor_event)
        self._led_event = asyncio.Event()
        self._led_task: asyncio.Task[None] | None = None
        self._last_led: LedSpec | None = None
        self.wake_model_sha256: str | None = None   # graph named in this session's ready
        self._cue: em_session.Cue | None = None
        self._timer_fraction: float | None = None
        self.preview_playback: em_render.Playback | None = None
        self._diagnostic = False
        # Recording modes (§18.1); em_controller drives them over the
        # diagnostic lease, whose mic blocks feed `diag_queue`.
        self.diag_queue: asyncio.Queue[np.ndarray] | None = None
        self.diag_task: asyncio.Task[None] | None = None
        self.collect_mode = False
        self.collect_seg: em_samples.Segmenter | None = None
        self.collect_clips = 0
        self.collect_last_ms: int | None = None
        self.ambient_mode = False
        self.ambient_rec: em_ambient.Recorder | None = None
        self.ambient_files = 0
        self.capture_mode = False
        self.capture_webhook: str | None = None
        self.capture_idle_s = em_capture.DEFAULT_IDLE_S
        self.capture_window: em_capture.Window | None = None
        self.capture_last_activity = 0.0
        self.capture_queue: asyncio.Queue[em_capture.CaptureResult] | None = None
        self.capture_sender_task: asyncio.Task[None] | None = None
        self.capture_watchdog_task: asyncio.Task[None] | None = None
        self.capture_delivered = 0
        self.capture_failed = 0
        self.capture_dropped = 0
        self.capture_dropped_pending = 0
        self.capture_store = em_capture.ResultStore()
        self.capture_ready = asyncio.Event()
        self.button_single_tap_event = False
        self.button_multi_tap_ms = 0
        self.tap_burst = em_tap_burst.TapCoalescer(
            lambda name: host.button_event(self.device_id, name),
            enabled=lambda: self.button_single_tap_event,
            on_error=_task_error,
        )

    @property
    def online(self) -> bool:
        return ((self.link is not None and not self.link.closed) or self.legacy_ws is not None)

    @property
    def diagnostic(self) -> bool:
        """A diagnostic uplink lease is armed (recording modes, §18.1)."""
        return self._diagnostic

    @diagnostic.setter
    def diagnostic(self, value: bool) -> None:
        self._diagnostic = bool(value)
        self._led_event.set()

    @property
    def led_anim_capable(self) -> bool:
        return Capability.LED_ANIM in self.capabilities

    @property
    def button_hold_capable(self) -> bool:
        return Capability.BUTTON_HOLD in self.capabilities

    @property
    def ambient_light_capable(self) -> bool:
        return Capability.AMBIENT_LIGHT in self.capabilities

    @property
    def alert_cache_capable(self) -> bool:
        return Capability.ALERT_CACHE in self.capabilities

    @property
    def capture_permitted(self) -> bool:
        return not self.diagnostic

    async def start(self) -> None:
        await self.actor.start()
        self._led_task = asyncio.create_task(self._led_loop(), name=f"led:{self.device_id}")

    async def close(self) -> None:
        self.tap_burst.cancel()
        await self.disconnect()
        if self._led_task is not None:
            self._led_task.cancel()
            await asyncio.gather(self._led_task, return_exceptions=True)
            self._led_task = None
        self._actor_remove()
        await self.actor.close()

    async def disconnect(self, reason: CloseReason = CloseReason.CLOSED) -> None:
        link, legacy = self.link, self.legacy_ws
        if link is not None:
            await link.close(reason)
        if legacy is not None:
            with contextlib.suppress(Exception):
                await legacy.close()

    async def send(self, msg_type: MessageType, body: Mapping[str, object], *,
                   generation: int = 0) -> str:
        link = self.link
        if link is not None and not link.closed:
            return await link.send(msg_type, body, generation=generation)
        if self.legacy_ws is not None and msg_type in {MessageType.SHELL_OPEN, MessageType.SHELL_CLOSE}:
            await self.legacy_ws.send(json.dumps({"type": msg_type, **body}))
            return "legacy"
        raise em_device_link.LinkClosed(f"{self.device_id} is offline")

    async def apply_config(self, cfg: em_config_sections.DeviceConfig) -> None:
        """Apply effective config and push only retained device-owned keys."""
        self.config = dict(cfg)
        self.led_scene = em_scenes.resolve(cfg)
        self.button_single_tap_event = bool(cfg.get("buttonSingleTapEvent", False))
        self.button_multi_tap_ms = int(cfg.get("buttonMultiTapMs", 0))
        link = self.link
        if link is not None and not link.closed:
            await self.send(MessageType.CONFIG, {k: cfg[k] for k in DEVICE_CONFIG_KEYS if k in cfg})
            # The wake graph is named only in session.ready (§16.1): a changed
            # selection renegotiates the session so device and controller agree.
            if cfg.get("wakeModel") not in (None, self.wake_model_sha256):
                log.info("[%s] wake model changed; renegotiating the session", self.device_id)
                await link.close(CloseReason.CLOSED)
        self._led_event.set()

    async def attach_legacy(self, ws: ServerConnection, *, ip: str, version: str | None,
                            capabilities: Iterable[str], secure: bool) -> None:
        await self.disconnect(CloseReason.CLOSED)
        self.legacy_ws = ws
        self.upgrade_required = True
        self.ip, self.firmware_version, self.secure = ip, version, secure
        self.capabilities = frozenset(capabilities)
        self.missing_capabilities = REQUIRED_CAPABILITIES - self.capabilities
        await self.host.connected(self)

    async def detach_legacy(self, ws: ServerConnection) -> None:
        if self.legacy_ws is not ws:
            return
        self.legacy_ws = None
        await self.host.disconnected(self)

    async def _ready(self, link: em_device_link.DeviceLink) -> None:
        hello = link.hello
        self.link = link
        render = self.render = self.host.make_render(self, link)
        self.legacy_ws = None
        self.upgrade_required = False
        self.capabilities = link.capabilities
        self.missing_capabilities = REQUIRED_CAPABILITIES - self.capabilities
        self.firmware_version = hello.firmware_version
        self.ip, self.secure = link.peer_ip, link.secure
        self.alerts_wakeup = hello.alerts_wakeup
        self.ambient = hello.ambient_light_status
        self._physical_seq = -1
        self.muted = hello.muted
        self.volume = hello.volume_level
        self.actor.attach(link, render)
        await self.host.alerts.on_session_hello(self.device_id, hello.alerts,
                                                capabilities=self.capabilities)
        await self.apply_config(self.config)
        await self.host.connected(self)
        self._start_ping()
        self._led_event.set()

    def _message(self, envelope: em_device_link.Envelope) -> None:
        if self.render is not None and self.render.on_message(envelope):
            return
        msg_type, body = envelope.type, envelope.body
        if msg_type in ACTOR_MESSAGES:
            self.actor.on_message(envelope)
        match msg_type:
            case MessageType.BUTTON_ACTION:
                self._spawn(self._button(body), "button")
            case MessageType.PRIVACY_CHANGED:
                muted = body.get("muted")
                self.muted = muted if isinstance(muted, bool) else None
                self._spawn(self.host.push_state(self, {"muted": self.muted}), "privacy push")
            case MessageType.WAKE_STATS:
                self.wake_stats = {**body, "received_ms": time.time_ns() // 1_000_000}
                self._spawn(asyncio.to_thread(self.store.wake_stats, self.device_id, body), "wake stats")
                self._spawn(self.host.push_state(self, {"wake_stats": self.wake_stats}), "wake stats push")
            case MessageType.ALERT_ACK:
                self._spawn(self.host.alerts.on_alert_ack(self.device_id, body), "alert ack")
            case MessageType.ALERT_STATE:
                self.alert_state = body
                self.host.alerts.on_alert_state(self.device_id, body)
                self._led_event.set()
                self._spawn(self.host.push_state(self, {"alert_state": body}), "alert state push")
            case MessageType.ALERT_RING_ENDED:
                self.host.alerts.on_alert_ring_ended(self.device_id, body)
            case MessageType.ALERT_LOCAL_OPERATION:
                self._spawn(self.host.alerts.on_local_operation(self.device_id, body), "alert operation")
            case MessageType.COMMAND_ACK:
                self.host.alerts.on_command_ack(self.device_id, CommandAck.parse(body))
            case MessageType.VOLUME_STATE:
                self._spawn(self._volume(body), "volume state")
            case MessageType.AMBIENT_LIGHT:
                self._ambient(body)
            case MessageType.STATS:
                self._spawn(self._stats(body), "device stats")
            case MessageType.WIFI_RESULT:
                self._spawn(self._wifi(body), "wifi result")
            case MessageType.WIFI_SCAN_RESULT:
                future = self.wifi_scan_future
                if future is not None and not future.done():
                    future.set_result(body)
            case MessageType.BLE_ADVERTS:
                adverts = body.get("adverts")
                self.host.ble_adverts(self.device_id, adverts if isinstance(adverts, list) else [])
            case MessageType.LOG:
                level = body.get("level")
                self._spawn(self.host.push_log(
                    self.device_id,
                    (em_db.LogLevel(level) if isinstance(level, str) and level in em_db.LogLevel
                     else em_db.LogLevel.INFO),
                    str(body.get("message", ""))), "device log")
            case MessageType.PONG:
                self._pong(body)

    def _audio(self, frame: bytes) -> None:
        self.actor.on_audio(frame)

    def _lost(self, link: em_device_link.DeviceLink | None, reason: CloseReason) -> None:
        if link is not None and self.link is not link:
            return
        self.link = None
        if self.render is not None:
            self.render.fail_all(reason)
        self.render = None
        self.actor.detach(reason)
        self.host.alerts.on_session_lost(self.device_id)
        self.host.player_gone(self.device_id)
        self.tap_burst.cancel()
        if self._ping_task is not None:
            self._ping_task.cancel()
            self._ping_task = None
        self._last_led = None
        self._spawn(self.host.disconnected(self), "disconnect")

    async def _button(self, body: Mapping[str, object]) -> None:
        sequence = body.get("physical_seq")
        if isinstance(sequence, int):
            if sequence <= self._physical_seq:
                return
            self._physical_seq = sequence
        if body.get("down", False):
            return
        occurrence_id = body.get("occurrence_id")
        active_id = (occurrence_id if body.get("handled") == ALERT_STOPPED
                     and isinstance(occurrence_id, str) else None)
        action = em_button.decide(
            held_ms=_integer(body.get("held_ms")) or 0,
            hold_ms=BUTTON_HOLD_MS,
            muted=bool(body.get("muted", self.muted)),
            turn_active=self.actor.turn_active,
            tap_event=self.button_single_tap_event and self.button_hold_capable,
            active_occurrence_id=active_id,
        )
        match action:
            case em_button.ButtonAction.ALERT_STOPPED:
                return
            case em_button.ButtonAction.HOLD:
                self.host.button_event(self.device_id, em_button.ButtonEvent.LONG)
            case em_button.ButtonAction.TAP_EVENT:
                if self.button_multi_tap_ms > 0:
                    self.tap_burst.tap(self.button_multi_tap_ms)
                else:
                    self.host.button_event(self.device_id, em_button.ButtonEvent.SINGLE)
            case em_button.ButtonAction.CANCEL:
                self.actor.cancel_turn()
            case em_button.ButtonAction.TURN:
                self.actor.button_turn(body)

    async def _volume(self, body: Mapping[str, object]) -> None:
        level = _integer(body.get("level"))
        if level is None:
            return
        self.volume = level
        cfg = await asyncio.to_thread(self.store.config, self.device_id)
        cfg["startupVolume"] = level
        await asyncio.to_thread(self.store.set_config, self.device_id, cfg)
        self.host.volume(self.device_id, em_volume.device_level_to_ha(level))
        await self.host.push_state(self, {"volume": level})

    def _ambient(self, body: Mapping[str, object]) -> None:
        lux = body.get("lux")
        if lux is not None and not isinstance(lux, int):
            return
        if self.stats is None:
            self.stats = {}
        self.stats["ambientLux"] = lux
        self.host.ambient_lux(self.device_id, lux)
        self._spawn(self.host.push_state(self, {"stats": self.stats}), "ambient push")

    async def _stats(self, body: Mapping[str, object]) -> None:
        stats = self.stats = {key: body.get(key) for key in STATS_KEYS}
        ble = body.get("ble")
        if ble:
            self.host.ble_stats(self.device_id, ble)
        if "ambientLux" in body:
            lux = body.get("ambientLux")
            self.host.ambient_lux(self.device_id, lux if isinstance(lux, int) else None)
        await asyncio.to_thread(self.store.stats, self.device_id,
                                {**stats, **self._drain_rtt()})
        await self.host.push_state(self, {"stats": stats})

    async def _wifi(self, body: Mapping[str, object]) -> None:
        ok, ssid = bool(body.get("ok")), str(body.get("ssid", ""))
        state = self.host.wifi_result(self.device_id, ok, ssid, str(body.get("error") or ""))
        await self.send(MessageType.WIFI_COMMIT, {})
        await self.host.push_state(self, {"wifi": state})

    def _start_ping(self) -> None:
        if self._ping_task is not None:
            self._ping_task.cancel()
        self._ping_task = asyncio.create_task(self._ping_loop(), name=f"ping:{self.device_id}")

    async def _ping_loop(self) -> None:
        try:
            while self.link is not None:
                await asyncio.sleep(PING_INTERVAL_SECONDS)
                now = asyncio.get_running_loop().time()
                self._ping_sent = {key: val for key, val in self._ping_sent.items()
                                   if now - val[0] <= PING_TIMEOUT_SECONDS}
                self._ping_seq += 1
                self._ping_sent[self._ping_seq] = (
                    now, self.actor.turn_active or self.host.player_busy(self.device_id))
                await self.send(MessageType.PING, {"id": self._ping_seq})
        except (asyncio.CancelledError, em_device_link.LinkClosed):
            pass

    def _pong(self, body: Mapping[str, object]) -> None:
        ping_id = body.get("id")
        item = self._ping_sent.pop(ping_id, None) if isinstance(ping_id, int) else None
        if item is None:
            return
        sent, busy = item
        ms = int((asyncio.get_running_loop().time() - sent) * 1000)
        self.rtt_last_ms = ms
        self._rtt_sum_ms += ms
        self._rtt_count += 1
        self._rtt_min_ms = ms if self._rtt_min_ms is None else min(ms, self._rtt_min_ms)
        self._rtt_max_ms = ms if self._rtt_max_ms is None else max(ms, self._rtt_max_ms)
        if not busy:
            self._rtt_samples_idle += 1
        if ms >= RTT_EXCURSION_MS:
            self._rtt_excursions += 1
            if not busy:
                self._rtt_excursions_idle += 1

    def _drain_rtt(self) -> dict[str, int | None]:
        if not self._rtt_count:
            return {}
        result = {
            "rttSumMs": self._rtt_sum_ms, "rttSamples": self._rtt_count,
            "rttMinMs": self._rtt_min_ms, "rttMaxMs": self._rtt_max_ms,
            "rttExcursions": self._rtt_excursions,
            "rttExcursionsIdle": self._rtt_excursions_idle,
            "rttSamplesIdle": self._rtt_samples_idle,
        }
        self._rtt_sum_ms = self._rtt_count = self._rtt_excursions = 0
        self._rtt_excursions_idle = self._rtt_samples_idle = 0
        self._rtt_min_ms = self._rtt_max_ms = None
        return result

    def _on_actor_event(self, event: em_session.ActorEvent) -> None:
        kind = em_session.ActorEventKind
        if event.kind is kind.CUE and isinstance(event.reason, em_session.Cue):
            self._cue = event.reason
        elif event.kind is kind.DIALOG_FOCUS:
            self.dialog_active = bool(event.dialog_active)
            if not self.dialog_active:
                self._spawn(self.host.dialog_released(self.device_id), "dialog release")
        elif event.kind is kind.STATE:
            self.host.voice_phase(self.device_id, event.state.phase)
        self._led_event.set()
        self._spawn(self.host.push_state(self, {
            "actor_state": event.state,
            "dialog_active": self.dialog_active,
            "terminal_reason": event.reason if event.kind is kind.TERMINAL else None,
        }), "actor push")

    def update_timer_projection(self, timers: Iterable[Mapping[str, object]]) -> None:
        self._timer_fraction = None
        timer = next(iter(timers), None)
        if timer is not None:
            total = _number(timer.get("total_seconds"))
            remaining = _number(timer.get("remaining_seconds"))
            if total is not None and total > 0 and remaining is not None:
                self._timer_fraction = min(1.0, max(0.0, remaining / total))
        self._led_event.set()

    async def _led_loop(self) -> None:
        # Send on change; renew TTL-bounded layers every LED_RENEW_SECONDS
        # before they expire (§11.2). The renewal clock runs from the last
        # check, so frequent events (the 1 s timer tick) cannot starve it.
        loop = asyncio.get_running_loop()
        next_renew = loop.time() + LED_RENEW_SECONDS
        while True:
            try:
                await asyncio.wait_for(self._led_event.wait(),
                                       timeout=max(0.0, next_renew - loop.time()))
            except TimeoutError:
                pass
            self._led_event.clear()
            due = loop.time() >= next_renew
            if due:
                next_renew = loop.time() + LED_RENEW_SECONDS
            if self.link is None or self.link.closed:
                continue
            if self._cue:
                cue, self._cue = self._cue, None
                anim = self.led_scene.get(cue)
                if anim:
                    await self._send_led(anim)
                    await asyncio.sleep(LED_CUE_SECONDS)
                    self._last_led = None   # the cue replaced the ring; restore it
            spec = self._project_led()
            if spec != self._last_led or (due and _needs_renewal(spec)):
                await self._send_led(spec)
                self._last_led = spec

    def _project_led(self) -> LedSpec:
        if self.diagnostic:
            return {"pattern": "pulse", "colors": [[180, 0, 200]], "periodMs": 2600,
                    "ttlSec": 20}
        match self.actor.state.phase:
            case em_session.VoicePhase.LISTENING:
                return self.led_scene["listening_anim"]
            case em_session.VoicePhase.THINKING:
                return self.led_scene["spin_anim"]
            case em_session.VoicePhase.SPEAKING:
                return self.led_scene["meter_anim"]
        if self._timer_fraction is not None and self._timer_fraction > 0:
            count = max(1, min(NUM_LEDS, math.ceil(NUM_LEDS * self._timer_fraction)))
            base = self.led_scene["listening"]
            leds = [dict(base[i]) if i < count else {"id": i, "r": 0, "g": 0, "b": 0}
                    for i in range(NUM_LEDS)]
            return {"pattern": "static", "leds": leds, "ttlSec": 20}
        return {"pattern": "off"}

    async def _send_led(self, spec: LedSpec) -> None:
        try:
            if self.led_anim_capable:
                if spec.get("pattern") == "static":
                    await self.send(MessageType.LEDS, {"leds": spec["leds"]})
                else:
                    await self.send(MessageType.LED_ANIM, {"anim": spec})
            else:
                if spec.get("pattern") == "off":
                    leds = [{"id": i, "r": 0, "g": 0, "b": 0} for i in range(NUM_LEDS)]
                elif spec.get("pattern") == "static":
                    leds = spec["leds"]
                else:
                    leds = self.led_scene["listening"]
                await self.send(MessageType.LEDS, {"leds": leds})
        except em_device_link.LinkClosed:
            pass

    def _spawn(self, awaitable: Awaitable[object], what: str) -> None:
        # The loop holds tasks weakly: keep each one until it finishes.
        task = asyncio.ensure_future(awaitable)
        task.set_name(f"{what}:{self.device_id}")
        self._background.add(task)
        task.add_done_callback(self._background.discard)
        task.add_done_callback(_task_error)


class _Sink:
    def __init__(self, device: Device):
        self.device = device
        self.link: em_device_link.DeviceLink | None = None

    async def on_ready(self, link: em_device_link.DeviceLink) -> None:
        self.link = link
        await self.device._ready(link)

    def on_message(self, envelope: em_device_link.Envelope) -> None:
        self.device._message(envelope)

    def on_audio(self, frame: bytes) -> None:
        self.device._audio(frame)

    def on_lost(self, reason: CloseReason) -> None:
        self.device._lost(self.link, reason)


class LinkHub:
    """``em_device_link.LinkHub`` implementation and Device registry owner."""

    def __init__(self, devices: dict[str, Device], host: Host, registry: WakeModels,
                 assets: SpeechAssetNamer, *, store: Store | None = None,
                 require_tls: bool = False, approval_default: ApprovalMode = ApprovalMode.STRICT):
        self.devices = devices
        self.host = host
        self.registry = registry
        self.assets = assets
        self.store = store or DbStore()
        self.require_tls = require_tls
        self.approval_default = approval_default

    async def ensure(self, device_id: str, label: str) -> Device:
        device = self.devices.get(device_id)
        if device is None:
            device = Device(device_id, label, self.host.make_actor(device_id), self.host, self.store)
            self.devices[device_id] = device
            await device.start()
        else:
            device.label = label
        return device

    async def admit(self, hello: em_device_link.SessionHello, *, device_id: str, peer_ip: str,
                    secure: bool, token: str | None) -> em_device_link.Admitted | em_device_link.Rejected:
        missing = REQUIRED_CAPABILITIES - hello.capabilities
        if missing:
            log.warning("[%s] missing v1 capabilities: %s", device_id, sorted(missing))
            return em_device_link.Rejected(RejectReason.PROTOCOL)
        registration = await register_device(
            self.store, self.host, device_id=device_id, ip=peer_ip,
            version=hello.firmware_version, secure=secure, token=token,
            require_tls=self.require_tls, approval_default=self.approval_default)
        if registration.rejected is not None:
            return em_device_link.Rejected(registration.rejected)
        device = await self.ensure(device_id, registration.label or device_id)
        device.config = await asyncio.to_thread(self.store.config, device_id)
        model = self.registry.for_config(device.config)
        device.wake_model_sha256 = model.graph_sha256
        duck_db = _number(device.config.get("duckDb"))
        ready = em_device_link.ReadyGrant(
            capture_permitted=device.capture_permitted,
            assets=self.assets.speech_assets(model),
            detector=em_device_link.DetectorConfig(
                thresholds=em_device_link.DetectorThresholds(
                    idle=model.thresholds.idle,
                    playback=model.thresholds.playback,
                    near_miss=model.thresholds.near_miss,
                ),
                provisional_duck=em_device_link.ProvisionalDuck(
                    duck_db=DEFAULT_DUCK_DB if duck_db is None else duck_db),
            ),
        )
        return em_device_link.Admitted(ready=ready, sink=_Sink(device))

    async def admit_legacy(self, *, device_id: str, ip: str, version: str | None,
                           secure: bool, token: str | None) -> Registration:
        return await register_device(
            self.store, self.host, device_id=device_id, ip=ip, version=version,
            secure=secure, token=token, require_tls=self.require_tls,
            approval_default=self.approval_default)


def _needs_renewal(spec: LedSpec) -> bool:
    """Short-TTL layers (≤ LED_TTL_RENEW_MAX_S) are re-sent before expiry."""
    ttl = spec.get("ttlSec")
    return spec.get("pattern") != "off" and (ttl is None or ttl <= LED_TTL_RENEW_MAX_S)


def _number(value: object) -> float | None:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _integer(value: object) -> int | None:
    return int(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _task_error(task: asyncio.Task[Any]) -> None:
    if task.cancelled():
        return
    try:
        error = task.exception()
    except asyncio.CancelledError:
        return
    if error is not None:
        log.error("background task %s failed: %s", task.get_name(), error,
                  exc_info=(type(error), error, error.__traceback__))
