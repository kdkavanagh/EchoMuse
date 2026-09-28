"""Controller-owned device state and v1 link admission (SPEC §11, §16.1).

A ``Device`` outlives transport sessions.  The link owns framing and liveness;
this module owns admission, message routing, retained state, button policy, and
projection of actor state onto the controller LED layer.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import math
import time
from dataclasses import dataclass
from typing import Any, Awaitable, Callable, Iterable, Protocol

import em_button
import em_capture
import em_db
import em_device_link
import em_linkauth
import em_render
import em_scenes
import em_tap_burst
import em_volume

log = logging.getLogger("em_device")

PROTOCOL = 1
REQUIRED_CAPABILITIES = frozenset({
    "audio_timeline_v1",
    "uplink_leases_v1",
    "device_wake_v1",
    "render_reference_v1",
    "render_progress_v1",
    "focus_leases_v1",
    "alert_cache_v1",
    "turn_protocol_v1",
})
ACTOR_MESSAGES = frozenset({
    "wake.candidate", "wake.candidate_end", "uplink.ended", "stream.open",
    "stream.end", "privacy.changed", "command.ack", "alert.state",
})
DEVICE_CONFIG_KEYS = ("startupVolume", "duckDb", "bleProxyEnabled")
PING_INTERVAL_SECONDS = 5.0
PING_TIMEOUT_SECONDS = 60.0
RTT_EXCURSION_MS = 200
LED_RENEW_SECONDS = 10.0
LED_TTL_RENEW_MAX_S = 30
BUTTON_HOLD_MS = 750
NUM_LEDS = 12


class Actor(Protocol):
    state: str
    awaiting_intent: bool
    turn_active: bool

    async def start(self) -> None: ...
    async def close(self) -> None: ...
    def attach(self, link: em_device_link.DeviceLink, render: em_render.RenderClient) -> None: ...
    def detach(self, reason: str) -> None: ...
    def on_message(self, msg_type: str, envelope: dict) -> None: ...
    def on_audio(self, frame: bytes) -> None: ...
    def button_turn(self, action: dict) -> None: ...
    def cancel_turn(self, reason: str = "interrupted") -> None: ...
    def add_listener(self, cb: Callable[[Any], None]) -> Callable[[], None]: ...
    async def open_diagnostic(self, on_mic: Callable[[int, Any], None]) -> None: ...
    async def close_diagnostic(self) -> None: ...


class Alerts(Protocol):
    async def on_session_hello(self, endpoint_id: str, alerts: dict, *,
                               capabilities: Iterable[str] = ()) -> None: ...
    def on_session_lost(self, endpoint_id: str) -> None: ...
    async def on_alert_ack(self, endpoint_id: str, body: dict) -> None: ...
    def on_alert_state(self, endpoint_id: str, body: dict) -> None: ...
    def on_alert_ring_ended(self, endpoint_id: str, body: dict) -> None: ...
    def on_command_ack(self, endpoint_id: str, body: dict) -> None: ...
    async def on_local_operation(self, endpoint_id: str, body: dict) -> None: ...
    def timers(self, endpoint_id: str) -> list[dict]: ...


class Host(Protocol):
    """Effects routed back into ``em_controller`` to keep Device testable."""

    alerts: Alerts

    def make_actor(self, device_id: str) -> Actor: ...
    def make_render(self, device: "Device", link: em_device_link.DeviceLink) -> em_render.RenderClient: ...
    async def connected(self, device: "Device") -> None: ...
    async def disconnected(self, device: "Device") -> None: ...
    async def pending(self, device_id: str, ip: str) -> None: ...
    async def push_state(self, device: "Device", state: dict) -> None: ...
    async def push_log(self, device_id: str, level: str, message: str) -> None: ...
    def button_event(self, device_id: str, event_type: str) -> None: ...
    def ambient_lux(self, device_id: str, lux: int | None) -> None: ...
    def volume(self, device_id: str, value: float) -> None: ...
    def ble_adverts(self, device_id: str, adverts: list) -> None: ...
    def ble_stats(self, device_id: str, stats: dict) -> None: ...
    def player_busy(self, device_id: str) -> bool: ...
    def player_gone(self, device_id: str) -> None: ...
    def dialog_released(self, device_id: str) -> Awaitable[None]: ...
    def wifi_result(self, device_id: str, ok: bool, ssid: str, error: str) -> dict: ...


class Store(Protocol):
    def device(self, device_id: str): ...
    def token(self, device_id: str) -> str | None: ...
    def approval_mode(self, default: str) -> str: ...
    def register(self, device_id: str, ip: str, version: str | None) -> None: ...
    def approve(self, device_id: str, label: str) -> None: ...
    def seen(self, device_id: str, ip: str, version: str | None) -> None: ...
    def config(self, device_id: str) -> dict: ...
    def set_config(self, device_id: str, config: dict) -> None: ...
    def log(self, device_id: str, level: str, source: str, message: str) -> None: ...
    def stats(self, device_id: str, stats: dict) -> None: ...
    def wake_stats(self, device_id: str, body: dict) -> None: ...


class DbStore:
    """Thin synchronous adapter; callers use ``asyncio.to_thread``."""

    def device(self, device_id: str):
        return em_db.get_device(device_id)

    def token(self, device_id: str) -> str | None:
        return em_db.get_device_token(device_id)

    def approval_mode(self, default: str) -> str:
        return em_db.get_config("device_approval", default) or default

    def register(self, device_id: str, ip: str, version: str | None) -> None:
        em_db.register_new_device(device_id, ip, version)

    def approve(self, device_id: str, label: str) -> None:
        em_db.approve_device(device_id, label, None)

    def seen(self, device_id: str, ip: str, version: str | None) -> None:
        em_db.upsert_device_seen(device_id, ip, version)

    def config(self, device_id: str) -> dict:
        return em_db.get_effective_device_config(device_id)

    def set_config(self, device_id: str, config: dict) -> None:
        em_db.set_device_config(device_id, config)

    def log(self, device_id: str, level: str, source: str, message: str) -> None:
        em_db.log_device(device_id, level, source, message)

    def stats(self, device_id: str, stats: dict) -> None:
        em_db.record_device_stats(device_id, stats)
        em_db.touch_device_seen(device_id)

    def wake_stats(self, device_id: str, body: dict) -> None:
        episodes = body.get("near_misses") or []
        peaks = [e.get("peak") for e in episodes if isinstance(e, dict) and isinstance(e.get("peak"), (int, float))]
        em_db.bump_wake_counters(
            device_id,
            near_misses=len(episodes),
            near_miss_max=max(peaks) if peaks else None,
            dev_hops=int(body.get("hops_scored") or 0),
            dev_drops=int(body.get("hops_dropped") or 0),
            dev_crossings=int(body.get("candidates_opened") or 0),
            dev_max_score=_number(body.get("peak_smoothed")),
            dev_max_infer_ms=_integer(body.get("infer_max_ms")),
        )


@dataclass(frozen=True)
class Registration:
    result: str
    row: Any | None
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
    approval_default: str,
) -> Registration:
    """Authenticate and apply the retained registration/approval policy."""
    expected = await asyncio.to_thread(store.token, device_id)
    verdict = em_linkauth.decide(
        presented=token, expected=expected, secure=secure, require_tls=require_tls)
    if not verdict.ok:
        log.warning("[%s] link rejected: %s", device_id, verdict.reason)
        return Registration("unauthorized", None, None)
    if verdict.stale_token:
        log.warning("[%s] stale device token; treating it as unregistered", device_id)

    row = await asyncio.to_thread(store.device, device_id)
    approval = await asyncio.to_thread(store.approval_mode, approval_default)
    if row is None:
        await asyncio.to_thread(store.register, device_id, ip, version)
        if approval != "auto":
            await host.pending(device_id, ip)
            return Registration("pending_approval", None, None)
        label = f"Unknown {device_id[:8]}"
        await asyncio.to_thread(store.approve, device_id, label)
        row = await asyncio.to_thread(store.device, device_id)
    if row is None or not bool(row["approved"]):
        await asyncio.to_thread(store.seen, device_id, ip, version)
        await host.pending(device_id, ip)
        return Registration("pending_approval", row, None)

    await asyncio.to_thread(store.seen, device_id, ip, version)
    return Registration("ok", row, row["label"] or f"EchoMuse {device_id[-8:]}")


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
        self.legacy_ws: Any | None = None
        self.capabilities: frozenset[str] = frozenset()
        self.missing_capabilities: frozenset[str] = frozenset()
        self.firmware_version: str | None = None
        self.ip: str | None = None
        self.secure = False
        self.upgrade_required = False
        self.stats: dict | None = None
        self.wake_stats: dict | None = None
        self.alert_state: dict | None = None
        self.alerts_wakeup: str | None = None
        self.volume: int | None = None
        self.muted: bool | None = None
        self.ambient: dict | None = None
        self.wifi_scan_future: asyncio.Future | None = None
        self.config: dict = {}
        self.dialog_active = False
        self.led_scene = em_scenes.resolve({})
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
        self._ping_task: asyncio.Task | None = None
        self._session_serial = 0
        self._physical_seq = -1
        self._boot_id: str | None = None
        self._actor_remove = actor.add_listener(self._on_actor_event)
        self._led_event = asyncio.Event()
        self._led_task: asyncio.Task | None = None
        self._last_led: dict | None = None
        self.wake_model_sha256: str | None = None   # graph named in this session's ready
        self._cue: str | None = None
        self._timer_fraction: float | None = None
        self.preview_playback: em_render.Playback | None = None
        self._diagnostic = False
        self.collect_mode = False
        self.collect_seg = None
        self.collect_clips = 0
        self.collect_last_ms: int | None = None
        self.collect_led_task = None
        self.ambient_mode = False
        self.ambient_rec = None
        self.ambient_files = 0
        self.ambient_led_task = None
        self.capture_mode = False
        self.capture_webhook: str | None = None
        self.capture_idle_s = em_capture.DEFAULT_IDLE_S
        self.capture_window = None
        self.capture_last_activity = 0.0
        self.capture_queue = None
        self.capture_sender_task = None
        self.capture_watchdog_task = None
        self.capture_led_task = None
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
        return "led_anim" in self.capabilities

    @property
    def button_hold_capable(self) -> bool:
        return "button_hold" in self.capabilities

    @property
    def ambient_light_capable(self) -> bool:
        return "ambient_light" in self.capabilities

    @property
    def alert_cache_capable(self) -> bool:
        return "alert_cache_v1" in self.capabilities

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

    async def disconnect(self, reason: str = "closed") -> None:
        link, legacy = self.link, self.legacy_ws
        if link is not None:
            await link.close(reason)
        if legacy is not None:
            with contextlib.suppress(Exception):
                await legacy.close()

    async def send(self, msg_type: str, body: dict, *, generation: int = 0) -> str:
        link = self.link
        if link is not None and not link.closed:
            return await link.send(msg_type, body, generation=generation)
        if self.legacy_ws is not None and msg_type in {"shell_open", "shell_close"}:
            await self.legacy_ws.send(json.dumps({"type": msg_type, **body}))
            return "legacy"
        raise em_device_link.LinkClosed(f"{self.device_id} is offline")

    async def apply_config(self, cfg: dict) -> None:
        """Apply effective config and push only retained device-owned keys."""
        self.config = dict(cfg)
        self.led_scene = em_scenes.resolve(cfg)
        self.button_single_tap_event = bool(cfg.get("buttonSingleTapEvent", False))
        self.button_multi_tap_ms = int(cfg.get("buttonMultiTapMs", 0))
        if self.link is not None and not self.link.closed:
            await self.send("config", {k: cfg[k] for k in DEVICE_CONFIG_KEYS if k in cfg})
            # The wake graph is named only in session.ready (§16.1): a changed
            # selection renegotiates the session so device and controller agree.
            if cfg.get("wakeModel") not in (None, self.wake_model_sha256):
                log.info("[%s] wake model changed; renegotiating the session", self.device_id)
                await self.link.close("closed")
        self._led_event.set()

    async def attach_legacy(self, ws: Any, *, ip: str, version: str | None,
                            capabilities: list[str], secure: bool) -> None:
        await self.disconnect("closed")
        self.legacy_ws = ws
        self.upgrade_required = True
        self.ip, self.firmware_version, self.secure = ip, version, secure
        self.capabilities = frozenset(capabilities)
        self.missing_capabilities = REQUIRED_CAPABILITIES - self.capabilities
        await self.host.connected(self)

    async def detach_legacy(self, ws: Any) -> None:
        if self.legacy_ws is not ws:
            return
        self.legacy_ws = None
        await self.host.disconnected(self)

    async def _ready(self, link: em_device_link.DeviceLink) -> None:
        self._session_serial += 1
        self.link = link
        self.render = self.host.make_render(self, link)
        self.legacy_ws = None
        self.upgrade_required = False
        self.capabilities = link.capabilities
        self.missing_capabilities = REQUIRED_CAPABILITIES - self.capabilities
        self.firmware_version = _text(link.hello.get("firmware_version"))
        self.ip, self.secure = link.peer_ip, link.secure
        self.alerts_wakeup = (link.hello.get("alerts") or {}).get("wakeup")
        self.ambient = link.hello.get("ambient_light_status")
        self._boot_id = _text(link.hello.get("boot_id"))
        self._physical_seq = -1
        privacy = link.hello.get("privacy") or {}
        self.muted = privacy.get("muted") if isinstance(privacy.get("muted"), bool) else None
        volume = link.hello.get("volume") or {}
        self.volume = _integer(volume.get("level"))
        self.actor.attach(link, self.render)
        await self.host.alerts.on_session_hello(self.device_id, link.hello.get("alerts") or {},
                                                capabilities=self.capabilities)
        await self.apply_config(self.config)
        await self.host.connected(self)
        self._start_ping()
        self._led_event.set()

    def _message(self, msg_type: str, envelope: dict) -> None:
        if self.render is not None and self.render.on_message(msg_type, envelope):
            return
        body = envelope.get("body") or {}
        if msg_type in ACTOR_MESSAGES:
            self.actor.on_message(msg_type, envelope)
        if msg_type == "button.action":
            self._spawn(self._button(body), "button")
        elif msg_type == "privacy.changed":
            self.muted = body.get("muted") if isinstance(body.get("muted"), bool) else None
            self._spawn(self.host.push_state(self, {"muted": self.muted}), "privacy push")
        elif msg_type == "wake.stats":
            self.wake_stats = {**body, "received_ms": time.time_ns() // 1_000_000}
            self._spawn(asyncio.to_thread(self.store.wake_stats, self.device_id, body), "wake stats")
            self._spawn(self.host.push_state(self, {"wake_stats": self.wake_stats}), "wake stats push")
        elif msg_type == "alert.ack":
            self._spawn(self.host.alerts.on_alert_ack(self.device_id, body), "alert ack")
        elif msg_type == "alert.state":
            self.alert_state = body
            self.host.alerts.on_alert_state(self.device_id, body)
            self._led_event.set()
            self._spawn(self.host.push_state(self, {"alert_state": body}), "alert state push")
        elif msg_type == "alert.ring_ended":
            self.host.alerts.on_alert_ring_ended(self.device_id, body)
        elif msg_type == "alert.local_operation":
            self._spawn(self.host.alerts.on_local_operation(self.device_id, body), "alert operation")
        elif msg_type == "command.ack":
            self.host.alerts.on_command_ack(self.device_id, body)
        elif msg_type == "volume_state":
            self._spawn(self._volume(body), "volume state")
        elif msg_type == "ambient_light":
            self._ambient(body)
        elif msg_type == "stats":
            self._spawn(self._stats(body), "device stats")
        elif msg_type == "wifi_result":
            self._spawn(self._wifi(body), "wifi result")
        elif msg_type == "wifi_scan_result":
            future = self.wifi_scan_future
            if future is not None and not future.done():
                future.set_result(body)
        elif msg_type == "ble_adverts":
            self.host.ble_adverts(self.device_id, body.get("adverts") or [])
        elif msg_type == "log":
            self._spawn(self.host.push_log(self.device_id, body.get("level", "info"),
                                           str(body.get("message", ""))), "device log")
        elif msg_type == "pong":
            self._pong(body)

    def _audio(self, frame: bytes) -> None:
        self.actor.on_audio(frame)

    def _lost(self, link: em_device_link.DeviceLink | None, reason: str) -> None:
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

    async def _button(self, body: dict) -> None:
        sequence = body.get("physical_seq")
        if isinstance(sequence, int):
            if sequence <= self._physical_seq:
                return
            self._physical_seq = sequence
        if body.get("down", False):
            return
        active_id = body.get("occurrence_id") if body.get("handled") == "alert_stopped" else None
        action = em_button.decide(
            held_ms=int(body.get("held_ms") or 0),
            hold_ms=BUTTON_HOLD_MS,
            muted=bool(body.get("muted", self.muted)),
            turn_active=self.actor.turn_active,
            tap_event=self.button_single_tap_event and self.button_hold_capable,
            active_occurrence_id=active_id,
        )
        if action == em_button.ALERT_STOPPED:
            return
        if action == em_button.HOLD:
            self.host.button_event(self.device_id, "long")
        elif action == em_button.TAP_EVENT:
            if self.button_multi_tap_ms > 0:
                self.tap_burst.tap(self.button_multi_tap_ms)
            else:
                self.host.button_event(self.device_id, "single")
        elif action == em_button.CANCEL:
            self.actor.cancel_turn()
        elif action == em_button.TURN:
            self.actor.button_turn(body)

    async def _volume(self, body: dict) -> None:
        level = _integer(body.get("level"))
        if level is None:
            return
        self.volume = level
        cfg = await asyncio.to_thread(self.store.config, self.device_id)
        cfg["startupVolume"] = level
        await asyncio.to_thread(self.store.set_config, self.device_id, cfg)
        self.host.volume(self.device_id, em_volume.device_level_to_ha(level))
        await self.host.push_state(self, {"volume": level})

    def _ambient(self, body: dict) -> None:
        lux = body.get("lux")
        if lux is not None and not isinstance(lux, int):
            return
        if self.stats is None:
            self.stats = {}
        self.stats["ambientLux"] = lux
        self.host.ambient_lux(self.device_id, lux)
        self._spawn(self.host.push_state(self, {"stats": self.stats}), "ambient push")

    async def _stats(self, body: dict) -> None:
        keys = (
            "cpuPct", "memUsedMb", "memTotalMb", "storageUsedMb", "storageTotalMb",
            "wifiRssi", "wifiSsid", "linkSpeedMbps", "wifiFreqMhz", "wifiBssid",
            "txBytes", "rxBytes", "txErrors", "txDropped", "rxCrcErrors", "ble",
            "cpuTempC", "maxTempC", "coresOnline", "coresTotal", "thermalCoreLimit",
            "ambientLux",
        )
        self.stats = {key: body.get(key) for key in keys}
        if body.get("ble"):
            self.host.ble_stats(self.device_id, body["ble"])
        if "ambientLux" in body:
            self.host.ambient_lux(self.device_id, body.get("ambientLux"))
        await asyncio.to_thread(self.store.stats, self.device_id,
                                {**self.stats, **self._drain_rtt()})
        await self.host.push_state(self, {"stats": self.stats})

    async def _wifi(self, body: dict) -> None:
        ok, ssid = bool(body.get("ok")), str(body.get("ssid", ""))
        state = self.host.wifi_result(self.device_id, ok, ssid, str(body.get("error") or ""))
        await self.send("wifi_commit", {})
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
                await self.send("ping", {"id": self._ping_seq})
        except (asyncio.CancelledError, em_device_link.LinkClosed):
            pass

    def _pong(self, body: dict) -> None:
        item = self._ping_sent.pop(body.get("id"), None)
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

    def _drain_rtt(self) -> dict:
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

    def _on_actor_event(self, event: Any) -> None:
        if event.kind == "cue":
            self._cue = event.reason
        elif event.kind == "dialog_focus":
            self.dialog_active = bool(event.dialog_active)
            if not self.dialog_active:
                self._spawn(self.host.dialog_released(self.device_id), "dialog release")
        self._led_event.set()
        self._spawn(self.host.push_state(self, {
            "actor_state": event.state,
            "dialog_active": self.dialog_active,
            "terminal_reason": event.reason if event.kind == "terminal" else None,
        }), "actor push")

    def update_timer_projection(self, timers: list[dict]) -> None:
        self._timer_fraction = None
        if timers:
            timer = timers[0]
            total = timer.get("total_seconds")
            remaining = timer.get("remaining_seconds")
            if isinstance(total, (int, float)) and total > 0 and isinstance(remaining, (int, float)):
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
                    await asyncio.sleep(1.0)
                    self._last_led = None   # the cue replaced the ring; restore it
            spec = self._project_led()
            if spec != self._last_led or (due and _needs_renewal(spec)):
                await self._send_led(spec)
                self._last_led = spec

    def _project_led(self) -> dict:
        if self.diagnostic:
            return {"pattern": "pulse", "colors": [[180, 0, 200]], "periodMs": 2600,
                    "ttlSec": 20}
        state = self.actor.state
        if state in {"ARMED", "LISTENING", "END_PENDING", "EXPECT_REPLY"}:
            return self.led_scene["listening_anim"]
        if state in {"COMMITTED", "THINKING"}:
            return self.led_scene["spin_anim"]
        if state == "SPEAKING":
            return self.led_scene["meter_anim"]
        if self._timer_fraction is not None and self._timer_fraction > 0:
            count = max(1, min(NUM_LEDS, math.ceil(NUM_LEDS * self._timer_fraction)))
            base = self.led_scene["listening"]
            leds = [dict(base[i]) if i < count else {"id": i, "r": 0, "g": 0, "b": 0}
                    for i in range(NUM_LEDS)]
            return {"pattern": "static", "leds": leds, "ttlSec": 20}
        return {"pattern": "off"}

    async def _send_led(self, spec: dict) -> None:
        try:
            if self.led_anim_capable:
                if spec.get("pattern") == "static":
                    await self.send("leds", {"leds": spec["leds"]})
                else:
                    await self.send("led_anim", {"anim": spec})
            else:
                if spec.get("pattern") == "off":
                    leds = [{"id": i, "r": 0, "g": 0, "b": 0} for i in range(NUM_LEDS)]
                elif spec.get("pattern") == "static":
                    leds = spec["leds"]
                else:
                    leds = self.led_scene["listening"]
                await self.send("leds", {"leds": leds})
        except em_device_link.LinkClosed:
            pass

    def _spawn(self, awaitable: Awaitable[Any], what: str) -> None:
        task = asyncio.create_task(awaitable, name=f"{what}:{self.device_id}")
        task.add_done_callback(_task_error)


class _Sink:
    def __init__(self, device: Device):
        self.device = device
        self.link: em_device_link.DeviceLink | None = None

    async def on_ready(self, link: em_device_link.DeviceLink) -> None:
        self.link = link
        await self.device._ready(link)

    def on_message(self, msg_type: str, envelope: dict) -> None:
        self.device._message(msg_type, envelope)

    def on_audio(self, frame: bytes) -> None:
        self.device._audio(frame)

    def on_lost(self, reason: str) -> None:
        self.device._lost(self.link, reason)


class LinkHub:
    """``em_device_link.LinkHub`` implementation and Device registry owner."""

    def __init__(self, devices: dict[str, Device], host: Host, registry: Any, assets: Any,
                 *, store: Store | None = None, require_tls: bool = False,
                 approval_default: str = "strict"):
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

    async def admit(self, hello: dict, *, device_id: str, peer_ip: str, secure: bool,
                    token: str | None) -> em_device_link.Admitted | em_device_link.Rejected:
        protocols = hello.get("protocols")
        capabilities = hello.get("capabilities")
        if (not isinstance(protocols, list) or PROTOCOL not in protocols
                or not isinstance(capabilities, list)
                or any(not isinstance(cap, str) for cap in capabilities)):
            return em_device_link.Rejected("protocol")
        missing = REQUIRED_CAPABILITIES - set(capabilities)
        if missing:
            log.warning("[%s] missing v1 capabilities: %s", device_id, sorted(missing))
            return em_device_link.Rejected("protocol")
        registration = await register_device(
            self.store, self.host, device_id=device_id, ip=peer_ip,
            version=_text(hello.get("firmware_version")), secure=secure, token=token,
            require_tls=self.require_tls, approval_default=self.approval_default)
        if registration.result != "ok":
            return em_device_link.Rejected(registration.result)
        device = await self.ensure(device_id, registration.label or device_id)
        device.config = await asyncio.to_thread(self.store.config, device_id)
        model = self.registry.for_config(device.config)
        device.wake_model_sha256 = model.graph_sha256
        ready = {
            "capture_permitted": device.capture_permitted,
            "assets": self.assets.speech_assets(model).wire(),
            "detector": {
                "thresholds": {
                    "idle": model.thresholds.idle,
                    "playback": model.thresholds.playback,
                    "near_miss": model.thresholds.near_miss,
                },
                "hop_blocks": 2,
                "smoothing": 3,
                "clear_after_unscored": 6,
                "provisional_duck": {
                    "duck_db": float(device.config.get("duckDb", -18.0)),
                    "max_per_window": 2,
                    "window_ms": 5000,
                },
            },
        }
        return em_device_link.Admitted(ready=ready, sink=_Sink(device))

    async def admit_legacy(self, *, device_id: str, ip: str, version: str | None,
                           capabilities: list[str], secure: bool, token: str | None) -> Registration:
        return await register_device(
            self.store, self.host, device_id=device_id, ip=ip, version=version,
            secure=secure, token=token, require_tls=self.require_tls,
            approval_default=self.approval_default)


def _needs_renewal(spec: dict) -> bool:
    """Short-TTL layers (≤ LED_TTL_RENEW_MAX_S) are re-sent before expiry."""
    ttl = spec.get("ttlSec")
    return spec.get("pattern") != "off" and (ttl is None or ttl <= LED_TTL_RENEW_MAX_S)


def _number(value: object) -> float | None:
    return float(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _integer(value: object) -> int | None:
    return int(value) if isinstance(value, (int, float)) and not isinstance(value, bool) else None


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _task_error(task: asyncio.Task) -> None:
    if task.cancelled():
        return
    try:
        error = task.exception()
    except asyncio.CancelledError:
        return
    if error is not None:
        log.error("background task %s failed: %s", task.get_name(), error,
                  exc_info=(type(error), error, error.__traceback__))
