"""EchoMuse controller entry point (SPEC §3, §12, §16.7, §18.1).

Wires the stock Home Assistant client, speech worker, wake registry, speech
assets, alert engine and per-device session actors, and serves the device
listener:

- ``/device/v1/control|audio|assets`` → ``em_device_link`` (WIRE §1)
- ``/control``                         → ``em_legacy`` (upgrade-only, §12)
- ``/shell/{device_id}``               → the retained shell plane

Recording modes (sample collection, ambient recording, script-driven capture)
hold a ``diagnostic`` uplink lease through the device's actor; while one runs
the actor accepts no wake, alerts still ring and physical stop still works.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import socket
import sys
import time
from pathlib import Path
from typing import Any

if __name__ == "__main__":
    # em_api imports this module lazily by name; give it the running instance.
    sys.modules.setdefault("em_controller", sys.modules[__name__])

import aiohttp
import numpy as np
import websockets
from aiohttp import web
from zeroconf import ServiceInfo
from zeroconf.asyncio import AsyncZeroconf

import em_alerts
import em_ambient
import em_api as api
import em_arbiter
import em_auth as auth
import em_ble_proxy
import em_capture
import em_db as db
import em_device
import em_device_assets
import em_device_link
import em_eq
import em_esphome as esphome
import em_ha_client
import em_legacy
import em_linkauth
import em_pki
import em_player
import em_render
import em_samples
import em_session
import em_sounds
import em_speech_worker
import em_volume
import em_wake_registry

_LOG_FORMAT = "%(asctime)s [%(levelname)s] %(name)s — %(message)s"
logging.basicConfig(level=logging.INFO, format=_LOG_FORMAT)
log = logging.getLogger("echomuse")
api.install_log_ring(_LOG_FORMAT)
logging.getLogger("websockets.server").setLevel(logging.CRITICAL)

# ─── Configuration ────────────────────────────────────────────────────────────

SERVER_HOST = os.environ.get("SERVER_HOST", "0.0.0.0")
SERVER_PORT = int(os.environ.get("SERVER_PORT", "8767"))
# TLS device listener; 0 disables. Devices learn it from the tls_port TXT record.
SERVER_TLS_PORT = int(os.environ.get("SERVER_TLS_PORT", "8770"))
# Reject device sockets that are not TLS with a matching per-device token.
REQUIRE_DEVICE_TLS = os.environ.get("REQUIRE_DEVICE_TLS", "0") == "1"
API_PORT = int(os.environ.get("API_PORT", "8768"))
SERVER_IP = os.environ.get("SERVER_IP", "10.10.1.236")
MDNS_NAME = os.environ.get("MDNS_NAME", "echomuse")
DB_PATH = os.environ.get("DB_PATH", "echomuse.db")
DEVICE_APPROVAL = os.environ.get("DEVICE_APPROVAL", "strict")

MDNS_REFRESH_INTERVAL = 120
WS_MAX_SIZE = 10 * 1024 * 1024
DEPLOYED_GRAPH_NAME = "bcresnet_audio.onnx"
DEPLOYED_SIDECAR_NAME = "bcresnet_audio.json"
PREVIEW_GAIN_DB = 0.0
DIAGNOSTIC_QUEUE_BLOCKS = 256            # ~20 s of 80 ms blocks

# Script-driven capture delivery (em_capture).
CAPTURE_QUEUE_MAX = 4
CAPTURE_POST_TIMEOUT_S = 15.0
CAPTURE_POST_ATTEMPTS = 3
CAPTURE_POST_BACKOFF_S = 1.0
CAPTURE_WATCHDOG_S = 2.0

# ─── Process state ────────────────────────────────────────────────────────────

# Every approved device, online or not. em_api holds a reference.
_devices: dict[str, em_device.Device] = {}
_shell_pending: dict[str, asyncio.Future] = {}
_shell_dashboard: dict[str, object] = {}
_loop_lag_peak_ms: float = 0.0

_ha: em_ha_client.HaClient | None = None
_alerts: em_alerts.AlertEngine | None = None
_registry: em_wake_registry.WakeRegistry | None = None
_assets: em_device_assets.DeviceAssets | None = None
_worker: em_speech_worker.SpeechWorker | None = None
_hub: em_device.LinkHub | None = None
_arbiter = em_arbiter.WakeArbiter()
_ha_status: dict[str, dict] = {}
_script_warnings: list[str] = []
# device_id → (HA device id, HA area name), from the device registry by MAC.
_ha_identity: dict[str, tuple[str, str | None]] = {}
_diag_queues: dict[str, asyncio.Queue] = {}
_diag_tasks: dict[str, asyncio.Task] = {}
_preview_generation = 0
_timers_shown: dict[str, bool] = {}


def _log_task_exception(task: asyncio.Task) -> None:
    if task.cancelled():
        return
    exc = task.exception()
    if exc is not None:
        log.error("background task %s failed: %s", task.get_name(), exc, exc_info=exc)


def _spawn(coro, name: str) -> asyncio.Task:
    task = asyncio.create_task(coro, name=name)
    task.add_done_callback(_log_task_exception)
    return task


# ─── Accessors for em_api ─────────────────────────────────────────────────────

def get_device(device_id: str) -> em_device.Device | None:
    return _devices.get(device_id)


def alerts() -> em_alerts.AlertEngine:
    assert _alerts is not None, "controller not started"
    return _alerts


def registry() -> em_wake_registry.WakeRegistry:
    assert _registry is not None, "controller not started"
    return _registry


def device_assets() -> em_device_assets.DeviceAssets:
    assert _assets is not None, "controller not started"
    return _assets


def ha_status() -> dict:
    """Per-feature HA status from the latest startup/connect probe (§16.7)."""
    if _ha is None:
        return {}
    if not _ha.connected or not _ha_status:
        reason = _ha.last_error or "not connected"
        return {f: {"ok": False, "detail": reason} for f in em_ha_client.FEATURES}
    return dict(_ha_status)


def speech_worker_status() -> dict:
    if _worker is None:
        return {"started": False, "available": False}
    return {"started": _worker.models is not None, "available": bool(_worker.available),
            "policy": _worker.policy_hash}


async def remove_device(device_id: str) -> None:
    """Forget a deleted device: close its sockets and actor."""
    device = _devices.pop(device_id, None)
    if device is None:
        return
    await _diagnostic_teardown(device)
    await device.close()
    await esphome.device_disconnected(device_id)
    em_player.device_gone(device_id)


# ─── Home Assistant ───────────────────────────────────────────────────────────

async def _resolve_identities() -> None:
    """HA device id and area per endpoint, matched by the emulated MAC (§16.7)."""
    assert _ha is not None
    entries = await _ha.command({"type": "config/device_registry/list"})
    areas = {a["area_id"]: a.get("name") for a in await _ha.command({"type": "config/area_registry/list"})}
    by_mac: dict[str, dict] = {}
    for entry in entries:
        for kind, value in entry.get("connections") or []:
            if kind == "mac":
                by_mac[value.lower()] = entry
    for device_id in _devices:
        entry = by_mac.get(esphome._serialno_to_mac(device_id).lower())
        if entry is None:
            _ha_identity.pop(device_id, None)
        else:
            _ha_identity[device_id] = (entry["id"], areas.get(entry.get("area_id")))


async def _relax_satellite(device_id: str) -> None:
    """Set the satellite's VAD sensitivity select to `relaxed` (§16.7)."""
    identity = _ha_identity.get(device_id)
    if _ha is None or identity is None:
        return
    selects = await _ha.satellite_entities(identity[0])
    if selects.vad_sensitivity_select is None:
        return
    if await _ha.entity_state(selects.vad_sensitivity_select) != em_ha_client.VAD_RELAXED:
        await _ha.set_select_option(selects.vad_sensitivity_select, em_ha_client.VAD_RELAXED)
        log.info("[%s] satellite VAD sensitivity set to relaxed", device_id)


async def _provision(device: em_device.Device) -> None:
    """Local Calendar for the endpoint (§16.7); re-deliver if it is live."""
    if _ha is None or _alerts is None or not _ha.connected:
        return
    if not _ha_status.get("calendar", {}).get("ok", False):
        return
    fresh = _alerts.status(device.device_id) is None
    try:
        await _alerts.ensure_endpoint(device.device_id, device.label)
    except (em_ha_client.HaError, em_ha_client.HaUnavailable) as err:
        log.warning("[%s] calendar provisioning failed: %s", device.device_id, err)
        return
    if fresh and device.link is not None and not device.link.closed:
        await _alerts.on_session_hello(device.device_id, device.link.hello.get("alerts") or {},
                                       capabilities=device.capabilities)


async def _on_ha_connected() -> None:
    """Startup probe, identity, provisioning, relay and scripts (§12 order 2)."""
    global _ha_status, _script_warnings
    assert _ha is not None and _alerts is not None
    probe = await _ha.probe()
    _ha_status = {f: {"ok": s.ok, "detail": s.detail} for f, s in probe.items()}
    try:
        await _resolve_identities()
    except (em_ha_client.HaError, em_ha_client.HaUnavailable) as err:
        log.warning("HA device identity lookup failed: %s", err)
    for device in list(_devices.values()):
        await _provision(device)
    await _alerts.on_ha_connected()
    if _ha_status.get("scripts", {}).get("ok"):
        try:
            _script_warnings = await _alerts.install_scripts()
        except (em_ha_client.HaError, em_ha_client.HaUnavailable) as err:
            _ha_status["scripts"] = {"ok": False, "detail": f"install: {err}"}
        if _script_warnings:
            _ha_status["scripts"] = {"ok": True, "detail": "; ".join(_script_warnings)}
    for device_id in list(_devices):
        try:
            await _relax_satellite(device_id)
        except (em_ha_client.HaError, em_ha_client.HaUnavailable) as err:
            log.warning("[%s] VAD sensitivity not set: %s", device_id, err)
    await api.push_ha_status(ha_status())


def _on_ha_disconnected() -> None:
    assert _alerts is not None
    _alerts.on_ha_disconnected()
    _spawn(api.push_ha_status(ha_status()), "ha-status")


async def _satellite_attached(device_id: str) -> None:
    """HA's ESPHome integration subscribed; its device entry now exists."""
    if _ha is None or not _ha.connected:
        return
    try:
        if device_id not in _ha_identity:
            await _resolve_identities()
        await _relax_satellite(device_id)
    except (em_ha_client.HaError, em_ha_client.HaUnavailable) as err:
        log.warning("[%s] satellite setup failed: %s", device_id, err)


# ─── Alert engine wiring ──────────────────────────────────────────────────────

def _effective_config(device_id: str) -> dict:
    device = _devices.get(device_id)
    if device is not None and device.config:
        return device.config
    return db.get_effective_device_config(device_id)


def _sound_resolver(endpoint_id: str, kind: str) -> str | None:
    key = "alarmSound" if kind == "alarm" else "timerSound"
    sha, flagged = em_sounds.alert_asset(_effective_config(endpoint_id).get(key))
    return None if flagged else sha


async def _alert_send(device_id: str, msg_type: str, body: dict, generation: int = 0) -> str:
    device = _devices.get(device_id)
    if device is None:
        raise em_device_link.LinkClosed(f"unknown endpoint {device_id}")
    return await device.send(msg_type, body, generation=generation)


def _speakers() -> list[em_alerts.Speaker]:
    return [em_alerts.Speaker(did, d.label, (_ha_identity.get(did) or (None, None))[1])
            for did, d in _devices.items()]


def _awaiting_intent() -> list[str]:
    return [did for did, d in _devices.items() if d.actor.awaiting_intent]


def _alert_notify(endpoint_id: str, code: str, data: dict) -> None:
    if code == "alert_ringing":
        esphome.update_alert_ringing(endpoint_id, data.get("ringing"))
    elif code == "timers":
        device = _devices.get(endpoint_id)
        if device is not None:
            device.update_timer_projection(data.get("timers") or [])
    if code == "timers":
        # Ticks arrive every second; an empty list is pushed once, when it empties.
        had = _timers_shown.get(endpoint_id, False)
        _timers_shown[endpoint_id] = bool(data.get("timers"))
        if not (had or _timers_shown[endpoint_id]):
            return
    _spawn(api.push_alerts(endpoint_id, code, data), "alerts-push")


# ─── Device host ──────────────────────────────────────────────────────────────

class _Host:
    """Controller-side effects for ``em_device.Device`` (em_device.Host)."""

    @property
    def alerts(self) -> em_alerts.AlertEngine:
        return alerts()

    def make_actor(self, device_id: str) -> em_session.SessionActor:
        assert _ha is not None and _worker is not None and _registry is not None
        deps = em_session.ActorDeps(
            ha=_ha, worker=_worker, registry=_registry, alerts=alerts(), arbiter=_arbiter,
            config=lambda: _effective_config(device_id),
            ha_device_id=lambda: (_ha_identity.get(device_id) or (None, None))[0],
            pipeline_id=lambda: _pipeline_id(device_id),
            vocabulary=lambda: _ha.vocabulary,
            esphome_reply=lambda pcm: esphome.esphome_reply(device_id, pcm),
            persist_turn=lambda rec: _persist_turn(device_id, rec),
            record_continuation=lambda row_id, outcome: asyncio.to_thread(
                db.set_turn_continuation, row_id, outcome),
        )
        return em_session.SessionActor(device_id, deps)

    def make_render(self, device: em_device.Device,
                    link: em_device_link.DeviceLink) -> em_render.RenderClient:
        def eq():
            return em_eq.StreamingEQ(48000, device.config.get("eqBands"),
                                     bool(device.config.get("eqLoudness", False)))
        return em_render.RenderClient(link, eq=eq)

    async def connected(self, device: em_device.Device) -> None:
        did = device.device_id
        db.log_device(did, "info", "controller",
                      f"Connected from {device.ip} version={device.firmware_version}"
                      + (" (upgrade required)" if device.upgrade_required else ""))
        await api.notify_device_connected(did, device.firmware_version)
        if device.upgrade_required:
            return
        await esphome.device_connected(
            did, label=device.label, capabilities=device.capabilities,
            hooks=esphome.Hooks(
                announce=lambda url, pre, sc: device.actor.announce(
                    url, preannounce_url=pre, start_conversation=sc),
                set_volume=lambda level: device.send("volume_set", {"level": level}),
                stop_alert=lambda: _stop_alert(did),
                timer_event=lambda *a: alerts().on_timer_event(did, *a),
                attached=lambda: _satellite_attached(did),
            ),
            volume=None if device.volume is None else _ha_volume(device.volume),
            ambient_lux=(device.stats or {}).get("ambientLux"),
        )
        await em_ble_proxy.device_connected(did)
        await em_ble_proxy.reconcile(did)
        await _provision(device)
        await _restore_modes(device)

    async def disconnected(self, device: em_device.Device) -> None:
        did = device.device_id
        db.log_device(did, "info", "controller", "Disconnected")
        await asyncio.to_thread(db.touch_device_seen, did)
        await api.notify_device_disconnected(did)
        await esphome.device_disconnected(did)
        await em_ble_proxy.device_disconnected(did)
        await _session_teardown(device)

    async def pending(self, device_id: str, ip: str) -> None:
        db.log_device(device_id, "info", "controller", f"Pending approval ({ip})")
        await api.notify_device_pending(device_id, ip)

    async def push_state(self, device: em_device.Device, state: dict) -> None:
        await api.push_device_update(device.device_id, state)

    async def push_log(self, device_id: str, level: str, message: str) -> None:
        await api._push_log_event(device_id, level, "device", message)

    def button_event(self, device_id: str, event_type: str) -> None:
        esphome.send_button_event(device_id, event_type)

    def ambient_lux(self, device_id: str, lux: int | None) -> None:
        esphome.update_ambient_lux(device_id, lux)

    def volume(self, device_id: str, value: float) -> None:
        esphome.update_device_volume(device_id, value)

    def ble_adverts(self, device_id: str, adverts: list) -> None:
        em_ble_proxy.forward_adverts(device_id, adverts)

    def ble_stats(self, device_id: str, stats: dict) -> None:
        em_ble_proxy.update_stats(device_id, stats)

    def player_busy(self, device_id: str) -> bool:
        return em_player.is_playing(device_id)

    def player_gone(self, device_id: str) -> None:
        em_player.device_gone(device_id)

    def dialog_released(self, device_id: str):
        return em_player.dialog_released(device_id)

    def wifi_result(self, device_id: str, ok: bool, ssid: str, error: str) -> dict:
        state, duplicate = api.wifi_record_result(device_id, ok, ssid, error)
        if not duplicate:
            level = "info" if ok else "warning"
            text = f'WiFi changed to "{ssid}"' if ok else f'WiFi change to "{ssid}" failed: {error}'
            db.log_device(device_id, level, "device", text)
        return state


def _ha_volume(level: int) -> float:
    return em_volume.device_level_to_ha(level)


async def _pipeline_id(device_id: str) -> str:
    assert _ha is not None
    identity = _ha_identity.get(device_id)
    if identity is None:
        return await _ha.resolve_pipeline(None)
    return await _ha.satellite_pipeline_id(identity[0])


async def _persist_turn(device_id: str, record: dict) -> int:
    turn_id = await asyncio.to_thread(db.insert_turn, device_id, record)
    await api._push_event({"type": "turn_complete", "device_id": device_id,
                           "turn": {**record, "turn_id": turn_id}})
    return turn_id


async def _stop_alert(device_id: str) -> None:
    result = await alerts().dismiss_alert(device_id, source="entity")
    if not result.get("ok", False):
        log.info("[%s] Stop alert entity: %s", device_id, result.get("error"))


# ─── Sound preview (§18.1 em_api sounds/test) ────────────────────────────────

async def preview_sound(device: em_device.Device, sound_id: str) -> dict:
    """Play a catalog sound as an `alert_preview` local source."""
    global _preview_generation
    if device.render is None or device.link is None or device.link.closed:
        return {"ok": False, "error": "upgrade_required" if device.upgrade_required else "offline"}
    await stop_preview(device)
    sha = await asyncio.to_thread(em_sounds.preview_asset, sound_id)
    _preview_generation = (_preview_generation + 1) & 0xFFFFFFFF or 1
    playback = await device.render.play_local("alert_preview", sha, generation=_preview_generation,
                                              gain_db=PREVIEW_GAIN_DB)
    device.preview_playback = playback
    return {"ok": True, "sha256": sha, "playback_id": playback.playback_id}


async def stop_preview(device: em_device.Device) -> None:
    playback, device.preview_playback = device.preview_playback, None
    if playback is not None:
        with contextlib.suppress(em_device_link.LinkClosed):
            await playback.cancel("stopped")


# ─── Recording modes over the diagnostic lease ───────────────────────────────

async def _push_modes(device: em_device.Device) -> None:
    await api.push_device_update(device.device_id, {
        "collectMode": device.collect_mode,
        "collectClips": device.collect_clips,
        "collectLastMs": device.collect_last_ms,
        "ambientMode": device.ambient_mode,
        "ambientFiles": device.ambient_files,
        "ambientMs": device.ambient_rec.duration_ms if device.ambient_rec is not None else 0,
        "captureMode": device.capture_mode,
        "captureDelivered": device.capture_delivered,
        "diagnostic": device.diagnostic,
    })


async def _sync_diagnostic(device: em_device.Device) -> None:
    """Hold exactly one diagnostic lease while any recording mode is on."""
    want = device.collect_mode or device.ambient_mode or device.capture_mode
    if want and not device.diagnostic:
        queue = _diag_queues.setdefault(device.device_id, asyncio.Queue(DIAGNOSTIC_QUEUE_BLOCKS))
        task = _diag_tasks.get(device.device_id)
        if task is None or task.done():
            _diag_tasks[device.device_id] = _spawn(_diagnostic_tap(device, queue),
                                                    f"diagnostic:{device.device_id}")
        device.diagnostic = True
        await device.actor.open_diagnostic(lambda _first, pcm: _diagnostic_block(device, pcm))
    elif not want and device.diagnostic:
        device.diagnostic = False
        await device.actor.close_diagnostic()
        task = _diag_tasks.pop(device.device_id, None)
        if task is not None:
            task.cancel()
        _diag_queues.pop(device.device_id, None)


def _diagnostic_block(device: em_device.Device, pcm: np.ndarray) -> None:
    queue = _diag_queues.get(device.device_id)
    if queue is None:
        return
    try:
        queue.put_nowait(pcm)
    except asyncio.QueueFull:
        log.warning("[%s] recording tap fell behind; block dropped", device.device_id)


async def _diagnostic_tap(device: em_device.Device, queue: asyncio.Queue) -> None:
    while True:
        pcm = await queue.get()
        frame = np.asarray(pcm, dtype="<i2").tobytes()
        samples = np.asarray(pcm, dtype=np.float32) / 32768.0
        rms = float(np.sqrt(np.mean(samples * samples))) if samples.size else 0.0
        if device.collect_seg is not None:
            clip = device.collect_seg.push(frame, rms)
            if clip is not None:
                await _write_clip(device, clip)
        if device.ambient_rec is not None:
            await _ambient_frame(device, frame)
        if device.capture_window is not None and device.capture_window.push(frame, rms):
            await close_capture_window(device)


async def _restore_modes(device: em_device.Device) -> None:
    """Persisted modes survive restarts and reconnects (schema v18/v19)."""
    if await asyncio.to_thread(db.get_collect_mode, device.device_id):
        await set_collect_mode(device, True)
    if await asyncio.to_thread(db.get_ambient_mode, device.device_id):
        if device.ambient_mode and device.ambient_rec is None:
            await _ambient_open(device)
        await set_ambient_mode(device, True)
    else:
        await asyncio.to_thread(em_ambient.recover, device.device_id)


async def _session_teardown(device: em_device.Device) -> None:
    """Session lost: finish open files; the persisted modes stay armed."""
    seg = device.collect_seg
    if seg is not None:
        clip = seg.flush()
        device.collect_seg = em_samples.Segmenter() if device.collect_mode else None
        if clip is not None:
            await _write_clip(device, clip)
    await _ambient_close(device)
    await capture_teardown(device)
    await _sync_diagnostic(device)


async def _diagnostic_teardown(device: em_device.Device) -> None:
    device.collect_mode = device.ambient_mode = False
    await _session_teardown(device)


async def _write_clip(device: em_device.Device, clip: em_samples.Clip) -> None:
    try:
        name = await asyncio.to_thread(em_samples.save_clip, device.device_id, clip)
    except Exception as err:
        log.warning("[%s] sample write failed: %s", device.device_id, err)
        return
    if name is None:
        return
    device.collect_clips += 1
    device.collect_last_ms = clip.duration_ms
    await api.push_device_update(device.device_id, {
        "collectClips": device.collect_clips, "collectLastMs": device.collect_last_ms})


async def set_collect_mode(device: em_device.Device, enabled: bool) -> None:
    """Arm/disarm wake-word sample collection. Idempotent; disarm keeps the open clip."""
    enabled = bool(enabled)
    if device.collect_mode == enabled:
        return
    device.collect_mode = enabled
    if enabled:
        device.collect_seg = em_samples.Segmenter()
        device.collect_clips = 0
        device.collect_last_ms = None
        db.log_device(device.device_id, "info", "controller",
                      "Sample collection started (voice turns suspended)")
    else:
        seg, device.collect_seg = device.collect_seg, None
        clip = seg.flush() if seg is not None else None
        if clip is not None:
            await _write_clip(device, clip)
        db.log_device(device.device_id, "info", "controller",
                      f"Sample collection stopped ({device.collect_clips} clips)")
    await _sync_diagnostic(device)
    await _push_modes(device)


async def _ambient_open(device: em_device.Device) -> bool:
    def _open():
        em_ambient.recover(device.device_id)
        return em_ambient.start(device.device_id)
    try:
        device.ambient_rec = await asyncio.to_thread(_open)
    except Exception as err:
        device.ambient_rec = None
        log.warning("[%s] ambient recording could not start: %s", device.device_id, err)
    return device.ambient_rec is not None


async def _ambient_close(device: em_device.Device) -> str | None:
    rec, device.ambient_rec = device.ambient_rec, None
    if rec is None:
        return None

    def _finish():
        kept = rec.close()
        em_ambient.prune(device.device_id)
        return kept
    try:
        name = await asyncio.to_thread(_finish)
    except Exception as err:
        log.warning("[%s] ambient recording write failed: %s", device.device_id, err)
        return None
    if name is not None:
        device.ambient_files += 1
    return name


async def _ambient_frame(device: em_device.Device, frame: bytes) -> None:
    rec = device.ambient_rec
    if rec.push(frame):
        try:
            await asyncio.to_thread(rec.flush)
        except Exception as err:
            log.warning("[%s] ambient write failed: %s", device.device_id, err)
            await set_ambient_mode(device, False)
            return
    if rec.full:
        await _ambient_close(device)
        if not await _ambient_open(device):
            await set_ambient_mode(device, False)
            return
        await _push_modes(device)


async def set_ambient_mode(device: em_device.Device, enabled: bool) -> None:
    """Arm/disarm ambient recording (one rolling file). Idempotent."""
    enabled = bool(enabled)
    if device.ambient_mode == enabled:
        return
    if enabled:
        device.ambient_files = 0
        if not await _ambient_open(device):
            db.log_device(device.device_id, "error", "controller",
                          "Ambient recording could not open a file")
            await _push_modes(device)
            return
        device.ambient_mode = True
        db.log_device(device.device_id, "info", "controller",
                      "Ambient recording started (voice turns suspended)")
    else:
        device.ambient_mode = False
        name = await _ambient_close(device)
        db.log_device(device.device_id, "info", "controller",
                      f"Ambient recording stopped ({name})" if name
                      else "Ambient recording stopped (nothing recorded)")
    await _sync_diagnostic(device)
    await _push_modes(device)


def _capture_now() -> float:
    return asyncio.get_running_loop().time()


def _capture_touch(device: em_device.Device) -> None:
    device.capture_last_activity = _capture_now()


def _capture_cancel_tasks(device: em_device.Device) -> None:
    current = asyncio.current_task()
    for attr in ("capture_sender_task", "capture_watchdog_task"):
        task = getattr(device, attr)
        setattr(device, attr, None)
        if task is not None and task is not current:
            task.cancel()


async def _capture_deliver(device: em_device.Device, result: em_capture.CaptureResult,
                           webhook: str, dropped: int) -> bool:
    """POST one recording; redirects are refused (em_capture.valid_webhook)."""
    body = result.wav()
    headers = em_capture.headers(result, device.device_id, dropped)
    timeout = aiohttp.ClientTimeout(total=CAPTURE_POST_TIMEOUT_S)
    last: object = None
    for attempt in range(CAPTURE_POST_ATTEMPTS):
        try:
            async with aiohttp.ClientSession(timeout=timeout) as session:
                async with session.post(webhook, data=body, headers=headers,
                                        allow_redirects=False) as resp:
                    if 200 <= resp.status < 300:
                        return True
                    last = f"HTTP {resp.status}"
        except asyncio.CancelledError:
            raise
        except Exception as err:
            last = f"{type(err).__name__}: {err}"
        if attempt < CAPTURE_POST_ATTEMPTS - 1:
            await asyncio.sleep(CAPTURE_POST_BACKOFF_S * (attempt + 1))
    log.warning("[%s] capture delivery failed (tag=%r): %s", device.device_id, result.tag, last)
    return False


async def _capture_sender(device: em_device.Device, queue: asyncio.Queue) -> None:
    while True:
        result = await queue.get()
        try:
            webhook = device.capture_webhook
            if webhook is None:
                continue
            dropped, device.capture_dropped_pending = device.capture_dropped_pending, 0
            if await _capture_deliver(device, result, webhook, dropped):
                device.capture_delivered += 1
            else:
                device.capture_failed += 1
                db.log_device(device.device_id, "warn", "controller",
                              f"Capture delivery failed for tag {result.tag!r}")
            await _push_modes(device)
        finally:
            queue.task_done()


async def _capture_watchdog(device: em_device.Device) -> None:
    while device.capture_mode:
        await asyncio.sleep(CAPTURE_WATCHDOG_S)
        if not device.capture_mode:
            return
        now = _capture_now()
        if em_capture.window_overdue(device.capture_window, now):
            await close_capture_window(device)
            continue
        if em_capture.decide_idle_expiry(now, device.capture_last_activity, device.capture_idle_s,
                                         device.capture_window is not None):
            db.log_device(device.device_id, "info", "controller",
                          f"Capture mode cleared after {device.capture_idle_s:.0f}s idle")
            await set_capture_mode(device, False)
            return


async def open_capture_window(device: em_device.Device, tag: str, max_ms: int) -> em_capture.Window:
    """Start recording; an open window is closed first."""
    if device.capture_window is not None:
        await close_capture_window(device)
    window = em_capture.Window(tag=tag, max_ms=max_ms, opened_mono=_capture_now())
    device.capture_window = window
    _capture_touch(device)
    return window


async def close_capture_window(device: em_device.Device, session: str | None = None) -> bool:
    """Close the window (optionally only the named one) and hand it on."""
    window, device.capture_window = device.capture_window, None
    _capture_touch(device)
    if window is None:
        return False
    if session and session != window.session:
        device.capture_window = window
        return False
    result = window.close()
    if not result.pcm:
        device.capture_failed += 1
        await _push_modes(device)
        return False
    if device.capture_webhook is None:
        device.capture_store.put(result)
        device.capture_ready.set()
        await _push_modes(device)
        return True
    queue = device.capture_queue
    if queue is None:
        return False
    try:
        queue.put_nowait(result)
    except asyncio.QueueFull:
        device.capture_dropped += 1
        device.capture_dropped_pending += 1
        return False
    return True


async def set_capture_mode(device: em_device.Device, enabled: bool,
                           webhook: str | None = None, idle_s: float | None = None) -> None:
    """Arm/disarm script-driven capture. Re-arming may re-point the webhook."""
    if enabled:
        if not device.capture_mode or webhook is not None:
            device.capture_webhook = webhook
        if idle_s is not None:
            device.capture_idle_s = em_capture.clamp_idle_s(idle_s)
        _capture_touch(device)
        if device.capture_mode:
            return
        device.capture_mode = True
        device.capture_delivered = device.capture_failed = 0
        device.capture_dropped = device.capture_dropped_pending = 0
        device.capture_store.clear()
        device.capture_ready.clear()
        if device.capture_webhook is not None:
            device.capture_queue = asyncio.Queue(maxsize=CAPTURE_QUEUE_MAX)
            device.capture_sender_task = _spawn(_capture_sender(device, device.capture_queue),
                                                f"capture-sender:{device.device_id}")
        else:
            device.capture_queue = None
        device.capture_watchdog_task = _spawn(_capture_watchdog(device),
                                              f"capture-watchdog:{device.device_id}")
        db.log_device(device.device_id, "info", "controller",
                      "Capture mode started (voice turns suspended)")
    else:
        if not device.capture_mode:
            return
        await close_capture_window(device)
        if device.capture_queue is not None:
            with contextlib.suppress(asyncio.TimeoutError):
                await asyncio.wait_for(device.capture_queue.join(), timeout=30.0)
        device.capture_mode = False
        _capture_cancel_tasks(device)
        device.capture_queue = None
        device.capture_webhook = None
        db.log_device(device.device_id, "info", "controller",
                      f"Capture mode stopped ({device.capture_delivered} recordings delivered)")
    await _sync_diagnostic(device)
    await _push_modes(device)


async def capture_teardown(device: em_device.Device) -> None:
    """Session lost: capture is per-connection and is not re-armed."""
    device.capture_mode = False
    device.capture_window = None
    device.capture_queue = None
    device.capture_webhook = None
    _capture_cancel_tasks(device)


async def take_recording(device: em_device.Device, session: str | None,
                         wait_s: float) -> em_capture.CaptureResult | None:
    """Long-poll one finished recording (pull mode)."""
    deadline = _capture_now() + max(0.0, wait_s)
    while True:
        result = device.capture_store.take(session)
        if not device.capture_store:
            device.capture_ready.clear()
        if result is not None:
            _capture_touch(device)
            return result
        remaining = deadline - _capture_now()
        if remaining <= 0:
            return None
        try:
            await asyncio.wait_for(device.capture_ready.wait(), timeout=remaining)
        except asyncio.TimeoutError:
            return None


def capture_state(device: em_device.Device | None) -> dict:
    """The capture mode's state for the API; safe on an offline device."""
    if device is None or not device.capture_mode:
        return {"enabled": False}
    window = device.capture_window
    return {
        "enabled": True,
        "mode": "push" if device.capture_webhook else "pull",
        "webhook": device.capture_webhook,
        "ready": len(device.capture_store),
        "evicted": device.capture_store.evicted,
        "idle_s": device.capture_idle_s,
        "delivered": device.capture_delivered,
        "failed": device.capture_failed,
        "dropped": device.capture_dropped,
        "window": None if window is None else {
            "session": window.session, "tag": window.tag, "max_ms": window.max_ms,
            "ms": window.ms, "frames": window.frames,
            "peak_db": round(window.peak_db, 1), "floor_db": round(window.floor_db, 1),
        },
    }


# ─── Shell plane (retained) ───────────────────────────────────────────────────

async def _shell_auth_ok(ws: Any, device_id: str, secure: bool) -> bool:
    try:
        presented = ws.request.headers.get(em_device_link.TOKEN_HEADER)
    except AttributeError:
        presented = None
    expected = await asyncio.to_thread(db.get_device_token, device_id)
    verdict = em_linkauth.decide(presented=presented, expected=expected, secure=secure,
                                 require_tls=REQUIRE_DEVICE_TLS)
    if not verdict.ok:
        log.warning("[shell] %s: %s — rejecting", device_id, verdict.reason)
    return verdict.ok


async def handle_shell(ws: Any, path: str, secure: bool) -> None:
    """Proxy a device-dialled shell to the dashboard or a programmatic caller."""
    device_id, _, query = path.removeprefix("/shell/").partition("?")
    pty_mode = "pty=1" in query
    if not device_id or not await _shell_auth_ok(ws, device_id, secure):
        await ws.close()
        return
    done = _shell_pending.get(device_id)
    dashboard = _shell_dashboard.get(device_id)
    if done is None or done.done():
        await ws.close()
        return
    if dashboard is None:
        done.set_result(ws)
        with contextlib.suppress(Exception):
            await asyncio.wait_for(ws.wait_closed(), timeout=300.0)
        return
    with contextlib.suppress(Exception):
        await dashboard.send_str(json.dumps({"type": "shell_meta", "pty": pty_mode}))

    async def device_to_dashboard():
        with contextlib.suppress(Exception):
            async for msg in ws:
                if isinstance(msg, bytes):
                    await dashboard.send_bytes(msg)
                else:
                    await dashboard.send_str(msg)

    async def dashboard_to_device():
        with contextlib.suppress(Exception):
            async for msg in dashboard:
                if msg.type == aiohttp.WSMsgType.BINARY:
                    await ws.send(msg.data)
                elif msg.type == aiohttp.WSMsgType.TEXT:
                    await ws.send(msg.data.encode())
                elif msg.type in (aiohttp.WSMsgType.CLOSE, aiohttp.WSMsgType.ERROR):
                    break

    tasks = [asyncio.create_task(device_to_dashboard()), asyncio.create_task(dashboard_to_device())]
    try:
        await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
    finally:
        for task in tasks:
            task.cancel()
        if not done.done():
            done.set_result(None)


# ─── Router ───────────────────────────────────────────────────────────────────

async def _route(ws: Any, secure: bool) -> None:
    path = ws.request.path
    if path == em_device_link.CONTROL_PATH:
        await em_device_link.serve_control(ws, secure=secure, hub=_hub)
    elif path == em_device_link.AUDIO_PATH:
        await em_device_link.serve_audio(ws, secure=secure)
    elif path == em_device_link.ASSETS_PATH:
        await em_device_link.serve_assets(ws, secure=secure, assets=_assets)
    elif path == "/control":
        await em_legacy.serve_control(ws, secure=secure, hub=_hub)
    elif path.startswith("/shell/"):
        await handle_shell(ws, path, secure)
    else:
        log.warning("unknown WebSocket path %s from %s", path, ws.remote_address)
        await ws.close()


async def router(ws: Any) -> None:
    await _route(ws, secure=False)


async def router_tls(ws: Any) -> None:
    await _route(ws, secure=True)


# ─── mDNS and loop monitor (retained) ─────────────────────────────────────────

def _make_mdns_info(tls_active: bool) -> ServiceInfo:
    props = {"version": "1", "server": MDNS_NAME}
    if tls_active:
        props["tls_port"] = str(SERVER_TLS_PORT)
    return ServiceInfo(
        "_emcontroller._tcp.local.", f"{MDNS_NAME}._emcontroller._tcp.local.",
        addresses=[socket.inet_aton(SERVER_IP)], port=SERVER_PORT,
        properties=props, server=f"{MDNS_NAME}.local.",
    )


async def _mdns_refresh_loop(azc: AsyncZeroconf, info: ServiceInfo) -> None:
    while True:
        await asyncio.sleep(MDNS_REFRESH_INTERVAL)
        try:
            await azc.async_update_service(info)
        except Exception as err:
            log.warning("mDNS refresh failed: %s", err)


async def event_loop_lag_monitor(interval: float = 1.0, warn_ms: float = 250.0) -> None:
    """Record the peak asyncio stall; also drives the CPU sampler."""
    global _loop_lag_peak_ms
    loop = asyncio.get_running_loop()
    next_cpu = 0.0
    while True:
        t0 = loop.time()
        await asyncio.sleep(interval)
        lag_ms = (loop.time() - t0 - interval) * 1000
        _loop_lag_peak_ms = max(_loop_lag_peak_ms, lag_ms)
        if loop.time() >= next_cpu:
            next_cpu = loop.time() + api.CPU_SAMPLE_INTERVAL_S
            api.sample_cpu()
        if lag_ms >= warn_ms:
            log.warning("[loop] event loop stalled %.0fms", lag_ms)


# ─── Startup ──────────────────────────────────────────────────────────────────

def _deployed_model_files() -> tuple[Path, Path]:
    """Repository copy of the deployed BCResNet pair (§5.1): image or checkout."""
    here = Path(__file__).resolve().parent
    for directory in (here, here.parent):
        graph, sidecar = directory / DEPLOYED_GRAPH_NAME, directory / DEPLOYED_SIDECAR_NAME
        if graph.is_file() and sidecar.is_file():
            return graph, sidecar
    raise SystemExit(f"deployed wake model {DEPLOYED_GRAPH_NAME} not found beside {here}")


def _open_registry() -> em_wake_registry.WakeRegistry:
    reg = em_wake_registry.WakeRegistry(
        em_wake_registry.default_registry_dir(DB_PATH),
        active_getter=db.get_global_device_config)
    if em_wake_registry.DEPLOYED_GRAPH_SHA256 not in {m.graph_sha256 for m in reg.list()}:
        reg.install_deployed(*_deployed_model_files())
        log.info("pinned deployed wake model %s", em_wake_registry.DEPLOYED_GRAPH_SHA256[:12])
    return reg


def _route_observation(obs: em_speech_worker.Observation) -> None:
    device = _devices.get(obs.device_id)
    if device is not None:
        device.actor.on_observation(obs)


async def main() -> None:
    global _ha, _alerts, _registry, _assets, _worker, _hub
    log.info("EchoMuse Controller %s", api.CONTROLLER_VERSION)
    db.init(DB_PATH)
    auth.maybe_generate_bootstrap_token()

    endpoint = em_ha_client.HaEndpoint.from_env()
    if endpoint is None:
        raise SystemExit("Home Assistant is not configured: set HA_URL and HA_TOKEN "
                         "(or run as the add-on with homeassistant_api: true)")
    _ha = em_ha_client.HaClient(endpoint)

    _registry = _open_registry()
    _assets = em_device_assets.DeviceAssets(_registry, alert_lookup=em_sounds.asset_path)
    _worker = em_speech_worker.SpeechWorker(_registry, on_observation=_route_observation)
    await _worker.start()
    log.info("speech worker ready (policy %s)", _worker.policy_hash)

    _alerts = em_alerts.AlertEngine(
        db.connect_alerts(), _ha, _alert_send,
        sound_resolver=_sound_resolver, config_resolver=_effective_config,
        speakers=_speakers, awaiting_intent=_awaiting_intent,
        notify=_alert_notify, db_lock=db.alerts_lock,
    )
    host = _Host()
    _hub = em_device.LinkHub(_devices, host, _registry, _assets,
                             require_tls=REQUIRE_DEVICE_TLS, approval_default=DEVICE_APPROVAL)
    for row in await asyncio.to_thread(db.get_all_devices):
        if row["approved"]:
            await _hub.ensure(row["device_id"], row["label"] or f"EchoMuse {row['device_id'][-8:]}")

    em_player.init(
        get_render=lambda did: _devices[did].render if did in _devices else None,
        notify_state=esphome.push_media_state,
        dialog_active=lambda did: did in _devices and _devices[did].dialog_active,
    )

    _ha.add_connect_listener(_on_ha_connected)
    _ha.add_disconnect_listener(_on_ha_disconnected)
    _ha.track_vocabulary(lambda vocab: log.info("HA vocabulary: %d targets", len(vocab.targets())))
    await _ha.start()
    await _alerts.start()
    background = [
        _spawn(_alerts.run(), "alerts"),
        _spawn(api.release_poll_loop(), "release-poll"),
        _spawn(api.session_prune_loop(), "session-prune"),
        _spawn(event_loop_lag_monitor(), "loop-lag"),
    ]

    runner = await api.create_runner(_devices, _shell_pending, _shell_dashboard)
    await runner.setup()
    await web.TCPSite(runner, SERVER_HOST, API_PORT).start()
    log.info("Dashboard + API on http://%s:%d", SERVER_HOST, API_PORT)

    tls_ctx = None
    if SERVER_TLS_PORT:
        try:
            tls_dir = em_pki.ensure_pki(DB_PATH)
            if tls_dir:
                tls_ctx = em_pki.server_ssl_context(tls_dir)
                api.set_tls_dir(tls_dir)
        except Exception as err:
            log.error("device-link TLS setup failed — wss listener disabled: %s", err)

    azc = AsyncZeroconf()
    info = _make_mdns_info(tls_active=tls_ctx is not None)
    await azc.async_register_service(info, allow_name_change=True)
    background.append(_spawn(_mdns_refresh_loop(azc, info), "mdns"))

    serve_args = dict(compression=None, process_request=em_device_link.process_request,
                      ping_interval=20, ping_timeout=10, max_size=WS_MAX_SIZE)
    try:
        async with contextlib.AsyncExitStack() as stack:
            # Satellite servers exist before any device can connect.
            await esphome.start_esphome_servers(_devices, SERVER_HOST)
            await em_ble_proxy.start_ble_proxy_servers(SERVER_HOST)
            await stack.enter_async_context(
                websockets.serve(router, SERVER_HOST, SERVER_PORT, **serve_args))
            if tls_ctx is not None:
                await stack.enter_async_context(
                    websockets.serve(router_tls, SERVER_HOST, SERVER_TLS_PORT, ssl=tls_ctx,
                                     **serve_args))
            log.info("EchoMuse Controller ready on %s:%d%s", SERVER_IP, SERVER_PORT,
                     f" (tls {SERVER_TLS_PORT})" if tls_ctx else "")
            await asyncio.Future()
    finally:
        await em_ble_proxy.stop_ble_proxy_servers()
        await esphome.stop_esphome_servers()
        for task in background:
            task.cancel()
        for device in list(_devices.values()):
            await device.close()
        await _alerts.close()
        await _worker.close()
        await _ha.close()
        await azc.async_unregister_service(info)
        await azc.async_close()
        await runner.cleanup()
        log.info("EchoMuse Controller stopped (%s)", time.strftime("%H:%M:%S"))


if __name__ == "__main__":
    asyncio.run(main())
