"""EchoMuse dashboard and authenticated HTTP API.

The device protocol sockets are owned by em_controller; this module serves the
SPA, fleet/configuration/diagnostic APIs, update and provisioning infrastructure,
and the dashboard event and shell WebSockets.
"""

import asyncio
import base64
import contextlib
import enum
import hashlib
import html as _html
import io
import json
import logging
import os
import platform
import re
import shutil
import tempfile
import time
import zipfile
from collections.abc import AsyncIterator, Callable, Coroutine, Mapping, Sequence
from dataclasses import asdict, dataclass
from datetime import date
from pathlib import Path
from typing import Generic, NotRequired, Protocol, TypedDict, TypeVar

import aiohttp
from aiohttp import web
from aiohttp.typedefs import Handler

import em_afe
import em_alerts
import em_db as db
import em_auth as auth
import em_ble_proxy
import em_config_sections as sections_mod
import em_device
import em_ha_client
import em_pki
import em_player
import em_recordings
import em_capture
import em_samples
import em_wakeclips
import em_ambient
import em_pause_asr
import em_volume
import em_sounds
import em_device_assets
import em_device_link
import em_firmware
import em_wake_registry
import em_wake_rules
import em_shell
import em_support
from em_device_link import MessageType
from em_session import ActorState, VoicePhase
from version import VERSION as CONTROLLER_VERSION
from version import UpdateStatus
from version import compare as _compare_versions
from version import parse as _parse_version

log = logging.getLogger("echomuse.api")

# Import time, which is startup: em_controller imports this module before it
# serves anything. Close enough to process start for "how long has it been up",
# and it needs no procfs.
_PROCESS_START = time.time()

# CPU over 1m/5m/1h, fed by the event-loop lag monitor (see sample_cpu). The
# ring is bounded by its longest window; nothing here runs per request.
_cpu_history = em_support.CpuHistory()
CPU_SAMPLE_INTERVAL_S = em_support.CpuHistory.INTERVAL_S

# The controller's own recent log, kept in memory for support bundles. Every
# line that would have explained #62 goes to stdout, and stdout was not in
# the bundle at all.
_log_ring = em_support.LogRing()


def install_log_ring(fmt: str) -> None:
    """
    Attach the in-memory log ring to the root logger.

    Called from em_controller once logging is configured, with the same
    format string, so a bundle reads exactly like the console does.
    """
    _log_ring.setFormatter(logging.Formatter(fmt))
    logging.getLogger().addHandler(_log_ring)


def sample_cpu() -> None:
    """
    Take one CPU sample. Called from em_controller's existing 1s ticker, not
    on a task of its own — the cost is one os.times() every INTERVAL_S.
    """
    _cpu_history.add(time.monotonic(), sum(os.times()[:2]))

# ─── Config ───────────────────────────────────────────────────────────────────

STATIC_DIR = Path(__file__).parent / "static"
# Set when running as a Home Assistant add-on (config.yaml's `environment`
# block) — gates the ingress-only middleware below. Unset for every other
# deployment (docker-compose, bare python), which keeps serving the
# dashboard directly exactly as before.
INGRESS_ONLY = os.environ.get("ECHOMUSE_HOME_ASSISTANT_INGRESS") == "true"
# Home Assistant Supervisor's ingress reverse proxy always calls in from this
# fixed address on the internal hassio Docker network.
INGRESS_GATEWAY_IP = "172.30.32.2"

T = TypeVar("T")


@dataclass(slots=True)
class _Cached(Generic[T]):
    """One in-memory value and when it was fetched (monotonic)."""

    value: T | None = None
    at: float = 0.0

    def fresh(self, ttl: float) -> T | None:
        """The value if it is younger than `ttl` seconds, else None."""
        if self.value is not None and time.monotonic() - self.at < ttl:
            return self.value
        return None

    def put(self, value: T) -> T:
        self.value, self.at = value, time.monotonic()
        return value


class ControllerRelease(TypedDict):
    """/api/releases/controller. Without any known release only version (None),
    current, status and available are present."""

    version: str | None
    current: str
    notes: NotRequired[str]
    published_at: NotRequired[str]
    release_url: NotRequired[str]
    status: UpdateStatus
    available: bool


# Controller releases are `controller-v*` TAGS with no GitHub Release behind
# them — controller-release.yml publishes a GHCR image and nothing else (see
# "Versioning / releases" in CLAUDE.md). So the notes come from the tag's own
# annotation: matching-refs lists the tags, and an annotated tag's object
# carries the message.
GITHUB_TAGS_URL = (
    "https://api.github.com/repos/{repo}/git/matching-refs/tags/controller-v"
)
GITHUB_TAG_OBJECT_URL = "https://api.github.com/repos/{repo}/git/tags/{sha}"

# A controller release lookup is served from memory this long (seconds); the
# DB holds the last one for when GitHub cannot be reached.
_controller_cache: _Cached[ControllerRelease] = _Cached()
RELEASE_CACHE_TTL = 60

# Reference to the live devices dict from em_controller — set by init().
_devices: dict[str, em_device.Device] = {}


def _online(device_id: str) -> em_device.Device | None:
    """The connected Device, otherwise None."""
    device = _devices.get(device_id)
    return device if device is not None and device.online else None


# Free space on /data. toybox df has no -m but prints 1K blocks by default.
# The unquoted $(df) folds the header and row onto the one marker line; no
# `tail`, which toybox may not link.
FREE_SPACE_PROBE = 'echo FREE_KB $(df /data 2>/dev/null)'


def _df_available(row: str) -> int | None:
    """The Available figure of a df row. Wrapped device names make numeric
    field indexes unstable, so anchor on the Use% field; a header's `Use%`
    follows a word, never a number, so a folded header is skipped."""
    fields = row.split()
    for i, field in enumerate(fields):
        if field.endswith("%") and i > 0 and fields[i - 1].isdigit():
            return int(fields[i - 1])
    return None


def _parse_free_mb(probe_out: str) -> int | None:
    """MiB free on /data from FREE_SPACE_PROBE's output; None when unreadable."""
    for line in (probe_out or "").splitlines():
        unit, _, row = line.strip().partition(" ")
        if unit == "FREE_KB":
            kb = _df_available(row)
            return None if kb is None else kb // 1024
    return None

# Device-link TLS material directory — set by em_controller.main() once
# em_pki.ensure_pki() succeeds. None = TLS listener not running (no
# cryptography package / setup failure); credential endpoints then 503.
_tls_dir: str | None = None


def set_tls_dir(tls_dir: str) -> None:
    global _tls_dir
    _tls_dir = tls_dir

# Set of connected /api/events WebSocket clients.
_event_clients: set[web.WebSocketResponse] = set()

# Track in-progress OTA updates per device_id to enforce one-at-a-time.
# Claimed by the request handler, before the task is scheduled, so two
# requests cannot both pass the check; released by the task when it ends.
_updates_in_progress: set[str] = set()

# Background tasks started from here. The event loop keeps only a weak
# reference to a task, so one nobody holds can be collected mid-flight.
_background_tasks: set[asyncio.Task[None]] = set()


def _spawn(coro: Coroutine[object, object, None], name: str) -> asyncio.Task[None]:
    """Run `coro` in the background, held until done; a crash is logged."""
    task = asyncio.create_task(coro, name=name)
    _background_tasks.add(task)
    task.add_done_callback(_task_done)
    return task


def _task_done(task: asyncio.Task[None]) -> None:
    _background_tasks.discard(task)
    if task.cancelled():
        return
    exc = task.exception()
    if exc is not None:
        log.error(f"[api] Unhandled exception in background task {task.get_name()}: {exc}",
                  exc_info=exc)


# Last OTA failure per device, surfaced as `update_error` in /api/devices so
# the dashboard (fleet deploy modal + per-device update log) can show *why* a
# tile stopped progressing instead of sitting at "updating…" forever. Set by
# _update_failed on every _run_update/_run_rollback failure path; cleared when
# a new update starts and on confirmed success. In-memory by design — a
# controller restart clears stale errors along with the update tasks
# themselves.
_update_errors: dict[str, str] = {}


@dataclass(frozen=True, slots=True)
class _WifiPending:
    ssid: str
    started_at: float


@dataclass(frozen=True, slots=True)
class _WifiResult:
    ok: bool
    ssid: str
    error: str
    at: float


@dataclass(slots=True)
class _WifiChange:
    """
    One device's WiFi switch: the one in flight, and how the last one ended.

    Deliberately NOT on the live Device object: the connection (and with it
    the Device) dies when the network switches, and the outcome arrives on
    the replacement connection. In-memory only — a controller restart
    mid-change just means the result event is lost, not the change itself
    (the device self-manages commit/rollback).
    """

    pending: _WifiPending | None = None
    last_result: _WifiResult | None = None

    def wire(self) -> dict[str, object]:
        """The device JSON's `wifi` field."""
        return {
            "pending": None if self.pending is None else asdict(self.pending),
            "last_result": None if self.last_result is None else asdict(self.last_result),
        }


_wifi_changes: dict[str, _WifiChange] = {}

# A change whose result never arrived (device bricked its network AND
# rollback failed, or controller restarted) must not block retries forever.
_WIFI_PENDING_TTL = 240  # device gates total ≤ ~135s + margin


def _wifi_change(device_id: str) -> _WifiChange:
    """Current wifi change state for a device, with stale pending expiry."""
    st = _wifi_changes.setdefault(device_id, _WifiChange())
    pending = st.pending
    if pending is not None and time.time() - pending.started_at > _WIFI_PENDING_TTL:
        st.pending = None
        st.last_result = _WifiResult(
            ok=False, ssid=pending.ssid,
            error="no result from device — change timed out (device may "
                  "be offline, or its rollback failed)",
            at=time.time(),
        )
    return st


def wifi_record_result(device_id: str, ok: bool, ssid: str, error: str
                       ) -> tuple[dict[str, object], bool]:
    """
    Store a wifi_result reported by the device.

    Returns (state JSON, duplicate). The device re-sends its result until the
    wifi_commit ack lands, so re-arrivals of the same outcome are flagged
    (duplicate=True) and don't refresh the timestamp — callers ack every
    arrival but log/record only the first.
    """
    st = _wifi_change(device_id)
    last = st.last_result
    if (last is not None and last.ok == ok and last.ssid == ssid
            and last.error == error and st.pending is None):
        return st.wire(), True
    st.pending = None
    st.last_result = _WifiResult(ok=ok, ssid=ssid, error=error, at=time.time())
    return st.wire(), False

# ─── Initialisation ───────────────────────────────────────────────────────────

class FeatureStatusWire(TypedDict):
    """One HA feature's probe result as the dashboard reads it."""

    ok: bool
    detail: str | None


class SpeechWorkerStatus(TypedDict, total=False):
    """The speech worker as `/api/system/status` reports it."""

    started: bool
    available: bool
    policy: str


class ControllerServices(Protocol):
    """What the API asks of the running controller.

    em_controller implements it and hands it to `create_runner`; handlers
    read it from `request.app[SERVICES]`. The accessors raise before the
    controller has created what they return.
    """

    @property
    def shell(self) -> em_shell.ShellBroker: ...
    def alerts(self) -> em_alerts.AlertEngine: ...
    def registry(self) -> em_wake_registry.WakeRegistry: ...
    def device_assets(self) -> em_device_assets.DeviceAssets: ...
    def firmware(self) -> em_firmware.BundledFirmware: ...
    def ha_status(self) -> dict[em_ha_client.HaFeature, FeatureStatusWire]: ...
    def speech_worker_status(self) -> SpeechWorkerStatus: ...
    def loop_lag_peak_ms(self) -> float: ...
    async def remove_device(self, device_id: str) -> None: ...
    async def set_collect_mode(self, device: em_device.Device, enabled: bool) -> None: ...
    async def set_ambient_mode(self, device: em_device.Device, enabled: bool) -> None: ...
    def capture_state(self, device: em_device.Device | None) -> dict[str, object]: ...
    async def set_capture_mode(self, device: em_device.Device, enabled: bool,
                               webhook: str | None = None,
                               idle_s: float | None = None) -> None: ...
    async def open_capture_window(self, device: em_device.Device, tag: str,
                                  max_ms: int) -> em_capture.Window: ...
    async def close_capture_window(self, device: em_device.Device,
                                   session: str | None = None) -> bool: ...
    async def take_recording(self, device: em_device.Device, session: str | None,
                             wait_s: float) -> em_capture.CaptureResult | None: ...
    async def preview_sound(self, device: em_device.Device, sound_id: str) -> dict[str, object]: ...
    async def stop_preview(self, device: em_device.Device) -> None: ...


SERVICES: web.AppKey[ControllerServices] = web.AppKey("services")


def init(devices_ref: dict[str, em_device.Device]) -> None:
    """
    Bind the live devices dict from em_controller.

    Must be called before create_app().
    """
    global _devices
    _devices = devices_ref


async def create_app(services: ControllerServices) -> web.Application:
    """
    Build and return the aiohttp Application, serving `services`.

    Routes are registered here. The app is not started — the caller
    creates an AppRunner and TCPSite.
    """
    app = web.Application(middlewares=[_ingress_only_middleware, _error_middleware])
    app[SERVICES] = services

    # Static / setup
    app.router.add_get("/",           _serve_spa)
    # /setup predates the state-aware landing page — / now shows the
    # first-run form itself when setup is pending, so just send people there.
    app.router.add_get("/setup",      _redirect_root)
    app.router.add_get("/dashboard",  _serve_dashboard)
    app.router.add_static("/static",  STATIC_DIR)
    app.router.add_post("/api/setup", _post_setup)
    # Public (pre-auth) — the landing page needs to know which form to show.
    # Exposes only the boolean; the bootstrap token itself stays in the logs.
    app.router.add_get("/api/system/setup-state", _get_setup_state)

    # Auth
    app.router.add_post("/api/auth/login",           _post_login)
    app.router.add_post("/api/auth/logout",          _post_logout)
    app.router.add_get("/api/auth/me",               _get_me)
    app.router.add_post("/api/auth/change-password", _post_change_password)

    # Devices — order matters: specific paths before parameterised ones
    app.router.add_get("/api/devices",                    _get_devices)
    app.router.add_get("/api/devices/pending",            _get_pending)
    app.router.add_get("/api/devices/{id}",               _get_device)
    app.router.add_patch("/api/devices/{id}",             _patch_device)
    app.router.add_delete("/api/devices/{id}",            _delete_device)
    app.router.add_post("/api/devices/{id}/approve",      _post_approve)
    app.router.add_get("/api/devices/{id}/config",        _get_device_config)
    app.router.add_post("/api/devices/{id}/config",       _post_device_config)
    app.router.add_get("/api/devices/{id}/logs",          _get_device_logs)
    app.router.add_get("/api/devices/{id}/turns",         _get_device_turns)
    app.router.add_get("/api/devices/{id}/activity",      _get_device_activity)
    app.router.add_get("/api/devices/{id}/wake_shadow",   _get_device_wake_shadow)
    app.router.add_get("/api/devices/{id}/response_latency", _get_device_response_latency)
    app.router.add_get("/api/devices/{id}/turns/{turn}/audio", _get_turn_audio)
    app.router.add_get("/api/devices/{id}/turns/{turn}/recording", _get_turn_recording)
    app.router.add_get("/api/devices/{id}/turns/{turn}/afe",   _get_turn_afe_series)
    app.router.add_get("/api/devices/{id}/turns/{turn}/trace", _get_turn_decision_trace)
    # Wake clips — the pre-detection audio that crossed the threshold. The
    # per-turn WAV sits beside the turn's utterance because that is the pair
    # you look at together; the archive and the purge are device-wide.
    # wakeclips.zip before wakeclips for the file's ordering rule, though
    # here they are two distinct literal segments and cannot collide.
    app.router.add_get("/api/devices/{id}/turns/{turn}/wake", _get_turn_wake_audio)
    app.router.add_get("/api/devices/{id}/wakeclips.zip",  _get_wakeclips_zip)
    app.router.add_delete("/api/devices/{id}/wakeclips",   _delete_wakeclips)
    # Wake-word sample collection. samples.zip before samples/{name} — the
    # two cannot collide (different segment counts), but the file's ordering
    # rule is worth keeping honest.
    app.router.add_post("/api/devices/{id}/collect",      _post_device_collect)
    app.router.add_get("/api/devices/{id}/samples.zip",   _get_samples_zip)
    app.router.add_get("/api/devices/{id}/samples",       _get_samples)
    app.router.add_delete("/api/devices/{id}/samples",    _delete_samples)
    app.router.add_get("/api/devices/{id}/samples/{name}", _get_sample_audio)
    app.router.add_delete("/api/devices/{id}/samples/{name}", _delete_sample)
    # Ambient recording — the mic held open, one file per session. `/ambient`
    # before `/ambient/{name}` for the same ordering reason as above.
    app.router.add_post("/api/devices/{id}/ambient",          _post_device_ambient)
    app.router.add_get("/api/devices/{id}/ambient",           _get_ambient)
    app.router.add_delete("/api/devices/{id}/ambient",        _delete_ambient_all)
    app.router.add_get("/api/devices/{id}/ambient/{name}",    _get_ambient_audio)
    app.router.add_delete("/api/devices/{id}/ambient/{name}", _delete_ambient)
    # Script-driven capture. The mode, then windows inside it — see em_capture.
    # /window/stop before /window: aiohttp matches in registration order and
    # the two would otherwise be ambiguous only by luck.
    app.router.add_get("/api/devices/{id}/capture",             _get_device_capture)
    app.router.add_post("/api/devices/{id}/capture",            _post_device_capture)
    app.router.add_post("/api/devices/{id}/capture/window/stop", _post_capture_window_stop)
    app.router.add_get("/api/devices/{id}/capture/recording",   _get_capture_recording)
    app.router.add_post("/api/devices/{id}/capture/window",     _post_capture_window)
    app.router.add_post("/api/devices/{id}/wifi",         _post_device_wifi)
    app.router.add_post("/api/devices/{id}/wifi/scan",    _post_device_wifi_scan)
    app.router.add_post("/api/devices/{id}/update",       _post_device_update)
    # A firmware install queued for the device's next connect (offline now).
    app.router.add_post("/api/devices/{id}/update/queue",   _post_device_update_queue)
    app.router.add_delete("/api/devices/{id}/update/queue", _delete_device_update_queue)
    app.router.add_post("/api/devices/{id}/rollback",     _post_device_rollback)

    # Content-addressed BCResNet registry and per-device named speech assets.
    app.router.add_get("/api/wake_models",             _get_wake_models)
    app.router.add_post("/api/wake_models/upload",     _post_wake_model_upload)
    app.router.add_delete("/api/wake_models/{sha256}", _delete_wake_model)
    app.router.add_get("/api/devices/{id}/speech_assets", _get_speech_assets)

    # Alert sound catalog and alert panel.
    app.router.add_get("/api/sounds",                    _get_sounds)
    app.router.add_post("/api/sounds/upload",            _post_sound_upload)
    app.router.add_delete("/api/sounds/{id}",            _delete_sound)
    app.router.add_post("/api/devices/{id}/sounds/preview", _post_sound_preview)
    app.router.add_post("/api/devices/{id}/sounds/stop",    _post_sound_stop)
    app.router.add_get("/api/devices/{id}/alerts",       _get_device_alerts)
    app.router.add_post("/api/devices/{id}/alarms",      _post_alarm)
    app.router.add_post("/api/devices/{id}/alarms/cancel", _post_alarm_cancel)
    app.router.add_get("/api/devices/{id}/shell",         _ws_shell)

    # Bundled device firmware, and the controller release advisory
    app.router.add_get("/api/firmware",            _get_firmware)
    app.router.add_post("/api/firmware/deploy",    _post_deploy_firmware)
    app.router.add_get("/api/releases/controller", _get_controller_release)

    # Global device config
    app.router.add_get("/api/global/config",   _get_global_config)
    app.router.add_post("/api/global/config",  _post_global_config)

    # System
    app.router.add_get("/api/support/bundle",  _get_support_bundle)
    app.router.add_get("/api/system/status",    _get_system_status)
    app.router.add_get("/api/system/config",    _get_system_config)
    app.router.add_patch("/api/system/config",  _patch_system_config)
    app.router.add_get("/api/ha/status",        _get_ha_status)
    app.router.add_get("/api/speech/wyoming",   _get_wyoming_info)

    # Provisioning
    app.router.add_get("/api/provision/start_script", _get_provision_start_script)
    app.router.add_get("/api/provision/firmware",     _get_provision_firmware)
    app.router.add_post("/api/provision/tls_credentials", _post_provision_tls_credentials)
    app.router.add_post("/api/provision/diagnostics",     _post_provision_diagnostics)
    app.router.add_post("/api/devices/{id}/secure_link",  _post_secure_link)
    app.router.add_post("/api/devices/{id}/debloat",      _post_debloat)

    # Live events WebSocket
    app.router.add_get("/api/events", _ws_events)

    return app


async def create_runner(devices_ref: dict[str, em_device.Device],
                        services: ControllerServices) -> web.AppRunner:
    """Convenience wrapper — init + create_app + AppRunner."""
    init(devices_ref)
    app = await create_app(services)
    return web.AppRunner(app)


# ─── Errors ───────────────────────────────────────────────────────────────────

class ErrorCode(enum.StrEnum):
    """The `code` of an API error body, `{"error": message, "code": code}`."""

    ALREADY_APPROVED = "already_approved"
    ALREADY_CURRENT = "already_current"
    BAD_REQUEST = "bad_request"
    BAD_WEBHOOK = "bad_webhook"
    COLLECTING = "collecting"
    DECODE_FAILED = "decode_failed"
    DEVICE_NOT_FOUND = "device_not_found"
    DEVICE_OFFLINE = "device_offline"
    EMPTY_UPLOAD = "empty_upload"
    INTERNAL_ERROR = "internal_error"
    INVALID_CONFIG = "invalid_config"
    INVALID_CREDENTIALS = "invalid_credentials"
    INVALID_FILENAME = "invalid_filename"
    INVALID_ID = "invalid_id"
    INVALID_INPUT = "invalid_input"
    INVALID_JSON = "invalid_json"
    INVALID_MODEL = "invalid_model"
    INVALID_PARAM = "invalid_param"
    INVALID_UPLOAD = "invalid_upload"
    MISSING_FIELD = "missing_field"
    MODEL_IN_USE = "model_in_use"
    NO_AFE_SERIES = "no_afe_series"
    NO_DECISION_TRACE = "no_decision_trace"
    NO_RECORDING = "no_recording"
    NO_ROLLBACK_AVAILABLE = "no_rollback_available"
    NO_SAMPLE = "no_sample"
    NO_SAMPLES = "no_samples"
    NO_WAKE_CLIP = "no_wake_clip"
    NO_WAKE_CLIPS = "no_wake_clips"
    NOT_APPROVED = "not_approved"
    NOT_CAPTURING = "not_capturing"
    NOT_CONNECTED = "not_connected"
    NOT_FOUND = "not_found"
    RECORDING_AMBIENT = "recording_ambient"
    REMOVED_CONFIG_KEY = "removed_config_key"
    SCAN_FAILED = "scan_failed"
    SCAN_IN_PROGRESS = "scan_in_progress"
    SCAN_TIMEOUT = "scan_timeout"
    SERVER_UNREACHABLE = "server_unreachable"
    SOUND_IN_USE = "sound_in_use"
    TLS_UNAVAILABLE = "tls_unavailable"
    TOO_LARGE = "too_large"
    UNKNOWN_CONFIG_KEY = "unknown_config_key"
    UNKNOWN_WAKE_MODEL = "unknown_wake_model"
    UPDATE_IN_PROGRESS = "update_in_progress"
    USER_NOT_FOUND = "user_not_found"
    WIFI_CHANGE_IN_PROGRESS = "wifi_change_in_progress"
    WOULD_DROP_KEYS = "would_drop_keys"


class ApiError(Exception):
    """A refused request, raised from a request helper and answered by
    `_error_middleware` with the API's error body."""

    def __init__(self, code: ErrorCode, message: str, status: int) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status


# ─── Middleware ───────────────────────────────────────────────────────────────

@web.middleware
async def _ingress_only_middleware(request: web.Request, handler: Handler) -> web.StreamResponse:
    """
    As a Home Assistant add-on, the dashboard/API must only be reachable
    through the authenticated ingress gateway — the add-on has no other
    auth in front of it on the LAN otherwise. No-op (INGRESS_ONLY unset)
    for every deployment that isn't the add-on.
    """
    if INGRESS_ONLY and request.remote != INGRESS_GATEWAY_IP:
        log.warning("Rejected non-ingress request from %s", request.remote)
        raise web.HTTPForbidden(text="Home Assistant Ingress is required")
    return await handler(request)


@web.middleware
async def _error_middleware(request: web.Request, handler: Handler) -> web.StreamResponse:
    """
    Catch unhandled exceptions and return a consistent error shape.

    AuthError from em_auth and ApiError from the request helpers are also
    caught here so route handlers don't need to handle them explicitly.
    """
    try:
        return await handler(request)
    except auth.AuthError as e:
        return e.to_response()
    except ApiError as e:
        return _error(e.code, e.message, e.status)
    except web.HTTPException:
        raise  # let aiohttp handle its own HTTP exceptions normally
    except Exception:
        log.exception(f"Unhandled error in {request.method} {request.path}")
        return _error(ErrorCode.INTERNAL_ERROR, "An internal error occurred", 500)


# ─── Static / setup ───────────────────────────────────────────────────────────

def _with_ingress_base(page: str, request: web.Request) -> str:
    """
    Inject a <base href> so the page's relative asset/API paths resolve
    under Home Assistant's generated ingress path (e.g.
    /api/hassio_ingress/<token>/) instead of the site root. A no-op string
    (base_path "/") outside ingress, where the page is already at root.
    """
    ingress_path = request.headers.get("X-Ingress-Path", "").rstrip("/")
    base_path = f"{ingress_path}/" if ingress_path else "/"
    base_tag = f'<base href="{_html.escape(base_path, quote=True)}">'
    return page.replace("<head>", f"<head>\n  {base_tag}", 1)


async def _serve_spa(request: web.Request) -> web.Response:
    """Serve index.html for all SPA routes."""
    index = STATIC_DIR / "index.html"
    if not index.exists():
        return web.Response(
            status=503,
            text="Dashboard not built — static/index.html not found",
        )
    return web.Response(
        text=_with_ingress_base(index.read_text(encoding="utf-8"), request),
        content_type="text/html",
        headers={"Cache-Control": "no-cache"},
    )


async def _serve_dashboard(request: web.Request) -> web.Response:
    """
    Serve dashboard.html for /dashboard, with the JS bundle cache-busted.

    add_static sends Last-Modified and ETag but no Cache-Control, so browsers
    apply HEURISTIC freshness and serve a cached dashboard.js without
    revalidating. The failure mode is nasty because it is invisible from the
    server side: the deploy is correct, the file on disk is correct, the
    compiled bundle is correct, and the browser shows the previous UI — which
    reads as "my change did not work" and sends you looking in the wrong place.
    It cost exactly that on 2026-07-30 when the new thermal row did not appear.

    So the bundle URL carries the file's mtime. That changes on every rebuild
    regardless of version numbering (controller_version is "dev" for local
    builds and would not bust between two dev deploys), and the wrapper itself
    is sent no-cache so the new URL is always seen — it is 3KB, revalidating it
    costs nothing.
    """
    dashboard = STATIC_DIR / "dashboard.html"
    if not dashboard.exists():
        return web.Response(status=503, text="dashboard.html not found in static/")
    page = _with_ingress_base(dashboard.read_text(encoding="utf-8"), request)
    bundle = STATIC_DIR / "dashboard.js"
    if bundle.exists():
        page = page.replace(
            "static/dashboard.js",
            f"static/dashboard.js?v={int(bundle.stat().st_mtime)}",
        )
    return web.Response(
        text=page,
        content_type="text/html",
        headers={"Cache-Control": "no-cache"},
    )


async def _redirect_root(request: web.Request) -> web.Response:
    # A relative Location preserves Home Assistant's generated ingress path
    # instead of bouncing the browser to the site root.
    raise web.HTTPFound(".")


async def _get_setup_state(request: web.Request) -> web.Response:
    """GET /api/system/setup-state — public: is first-run setup pending?"""
    return _ok({"needs_setup": auth.get_bootstrap_token() is not None})


async def _post_setup(request: web.Request) -> web.Response:
    """
    POST /api/setup — first-run admin account creation.

    Body: {token, username, password}
    Returns 201 + {token, role} on success so the client is immediately
    logged in after setup.
    """
    body = await _json_body(request)
    token    = _require_str(body, "token")
    username = _require_str(body, "username")
    password = _require_str(body, "password")

    await auth.create_first_admin(token, username, password)

    session_token, role = await auth.login(username, password)
    return _ok({"token": session_token, "role": role}, status=201)


# ─── Auth ─────────────────────────────────────────────────────────────────────

async def _post_login(request: web.Request) -> web.Response:
    """POST /api/auth/login — {username, password} → {token, role}"""
    body     = await _json_body(request)
    username = _require_str(body, "username")
    password = _require_str(body, "password")

    token, role = await auth.login(username, password)
    return _ok({"token": token, "role": role})


async def _post_logout(request: web.Request) -> web.Response:
    """POST /api/auth/logout — invalidate current session."""
    user = await auth.resolve_session(request)
    if user:
        await auth.logout(user["token"])
    return _ok({})


@auth.require_auth
async def _get_me(request: web.Request) -> web.Response:
    """GET /api/auth/me — current user info."""
    user: auth.SessionUser = request["user"]
    return _ok({
        "id":       user["id"],
        "username": user["username"],
        "role":     user["role"],
    })


# ─── Devices ──────────────────────────────────────────────────────────────────

@auth.require_auth
async def _get_devices(request: web.Request) -> web.Response:
    """GET /api/devices — all devices, live state merged with DB."""
    loop = asyncio.get_running_loop()
    rows = await loop.run_in_executor(None, db.get_all_devices)
    return _ok([_merge_device(row) for row in rows])


@auth.require_auth
async def _get_pending(request: web.Request) -> web.Response:
    """GET /api/devices/pending — unapproved devices."""
    loop = asyncio.get_running_loop()
    rows = await loop.run_in_executor(None, db.get_pending_devices)
    return _ok([_merge_device(row) for row in rows])


@auth.require_auth
async def _get_device(request: web.Request) -> web.Response:
    """GET /api/devices/{id}"""
    device_id = request.match_info["id"]
    row = await _device_row(device_id)
    return _ok(_merge_device(row))


@auth.require_auth
async def _get_device_turns(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/turns — recent voice-turn traces, newest last.
    Served from the persistent turns table (survives controller and device
    restarts). Powers the Activity tab's observability panel.

    Query params: limit (default 50, max 1000), since (epoch seconds)."""
    device_id = request.match_info["id"]
    try:
        limit = min(int(request.query.get("limit", 50)), 1000)
        raw_since = request.query.get("since")
        since = float(raw_since) if raw_since is not None else None
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "limit/since must be numeric", 400)
    loop  = asyncio.get_running_loop()
    turns = await loop.run_in_executor(
        None, lambda: db.get_turns(device_id, limit, since)
    )
    return _ok([_turn_json(t) for t in turns])


def _turn_json(turn: Mapping[str, object]) -> dict[str, object]:
    """A turns row for the dashboard, `afe_evidence` parsed from its stored
    JSON (em_afe.AfeEvidenceJson). Null stays null — unavailable: the device
    lacked afe_metadata_v1, or the row predates it."""
    raw = turn.get("afe_evidence")
    evidence: em_afe.AfeEvidenceJson | None = None
    if isinstance(raw, str):
        try:
            evidence = em_afe.AfeEvidence.loads(raw).wire()
        except ValueError as exc:
            log.warning("turn %s: unreadable afe_evidence ignored: %s", turn.get("turn_id"), exc)
    return {**turn, "afe_evidence": evidence}


@auth.require_auth
async def _get_turn_afe_series(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/turns/{turn}/afe — one turn's native AFE series
    (em_afe.AfeSeriesJson): every 80 ms period's record from the second
    before the wake through the utterance and the spoken answer, with the
    turn's recordings and processing stages placed on it, for the Activity
    tab's chart.

    A 404 is ordinary: the device lacked afe_metadata_v1, no record arrived,
    the row is older than the newest em_db.TRACE_RETENTION, or it predates
    schema 29. The stored JSON is the controller's own, served as written."""
    device_id = request.match_info["id"]
    try:
        turn_id = int(request.match_info["turn"])
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "turn must be an integer", 400)
    await _device_row(device_id)
    series = await asyncio.get_running_loop().run_in_executor(
        None, db.get_turn_afe_series, device_id, turn_id)
    if series is None:
        return _error(ErrorCode.NO_AFE_SERIES, "No native AFE series for this turn", 404)
    return web.Response(content_type="application/json", text=series)


@auth.require_auth
async def _get_turn_decision_trace(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/turns/{turn}/trace — one turn's §11.3 decision
    trace: wake, attribution, endpoint and, per pause, each transcriber's
    words and time (`utterance.pauses`) and whose words were committed (the
    `commit` event's `source`), for the Activity tab's turn detail.

    A 404 is ordinary: the row is older than the newest em_db.TRACE_RETENTION
    or predates schema 26. The stored JSON is the controller's own, served as
    written."""
    device_id = request.match_info["id"]
    try:
        turn_id = int(request.match_info["turn"])
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "turn must be an integer", 400)
    await _device_row(device_id)
    trace = await asyncio.get_running_loop().run_in_executor(
        None, db.get_turn_decision_trace, device_id, turn_id)
    if trace is None:
        return _error(ErrorCode.NO_DECISION_TRACE, "No decision trace for this turn", 404)
    return web.Response(content_type="application/json", text=trace)


@auth.require_auth
async def _get_turn_audio(request: web.Request) -> web.StreamResponse:
    """GET /api/devices/{id}/turns/{turn}/audio — the saved utterance (the
    STT copy) for one voice turn, as a downloadable WAV.

    Only turns captured while saveUtterances was on have one, and only the
    newest em_recordings.KEEP_PER_DEVICE per device survive — a turn row
    older than that window still carries the filename but the file is gone,
    so a 404 here is an ordinary outcome, not an error state."""
    return await _turn_recording_response(request, em_recordings.RecordingKind.UTTERANCE, "")


@auth.require_auth
async def _get_turn_recording(request: web.Request) -> web.StreamResponse:
    """GET /api/devices/{id}/turns/{turn}/recording — the turn recording: the
    whole turn's canonical mic, through the spoken answer, as a WAV placed by
    the turn's AFE series (`recording`).

    Only turns with an AFE chart captured while saveUtterances was on have
    one, and only the newest em_recordings.KEEP_TURN_PER_DEVICE per device
    survive, so a 404 here is an ordinary outcome, not an error state."""
    return await _turn_recording_response(request, em_recordings.RecordingKind.TURN, "-recording")


async def _turn_recording_response(request: web.Request, kind: em_recordings.RecordingKind,
                                   suffix: str) -> web.StreamResponse:
    """One turn's saved recording of `kind` as a WAV attachment.

    The filename is derived from (device, turn) rather than taken from the
    row: em_recordings.resolve then re-checks that the file belongs to the
    device in the URL, so a turn id from another device can't be used to
    reach its audio."""
    device_id = request.match_info["id"]
    try:
        turn_id = int(request.match_info["turn"])
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "turn must be an integer", 400)

    row = await _device_row(device_id)

    name = em_recordings.filename(device_id, turn_id)
    path = em_recordings.resolve(device_id, name, kind=kind) if name else None
    if path is None:
        return _error(ErrorCode.NO_RECORDING,
                      "No saved audio for this turn", 404)

    label = _slug(row.label or device_id)
    return web.FileResponse(
        path,
        headers={
            "Content-Type":        "audio/wav",
            "Content-Disposition": f'attachment; filename="{label}-turn{turn_id}{suffix}.wav"',
            # Recordings are immutable once written and their names are
            # unique per turn, but the retention window means a name can
            # stop resolving — so cache privately and briefly, never shared.
            "Cache-Control":       "private, max-age=60",
        },
    )


def _slug(text: str) -> str:
    """Lowercase ASCII slug, safe for a Content-Disposition filename."""
    out = re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-").lower()
    return out or "device"


# ─── Wake-word sample collection ──────────────────────────────────────────────
#
# Collection mode itself (em_samples, em_controller.set_collect_mode) plus the
# read side: list, play, download, delete. The mode is persisted on the device
# row rather than in its config — see the schema v18 migration.


@auth.require_admin
async def _post_device_collect(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/collect — body {"enabled": bool}

    Puts a device into (or out of) wake-word sample collection: its mic
    stream is cut into training clips on the controller and it starts no
    voice turns at all until this is switched off.

    Admin-only and one device at a time, because it SUSPENDS the assistant
    on that device. A device that answers nothing looks broken to everyone
    else in the house, so this is a deliberate act with a state the
    dashboard shows on every panel.

    Persisted even when the device is offline: arming a device that is
    rebooting is a reasonable thing to do, and the mode is re-applied by
    handle_control on its next connect.
    """
    device_id = request.match_info["id"]
    body = await _optional_json_body(request)
    enabled = bool(body.get("enabled"))

    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)
    if not row.approved:
        return _error(ErrorCode.NOT_APPROVED,
                      "Approve this device before collecting from it", 409)
    # The mirror of the guard in _post_device_ambient: the two modes want the
    # same frames for opposite purposes, so they are mutually exclusive at
    # the API rather than resolved by a precedence rule nobody can see.
    if enabled and bool(row.ambient_mode):
        return _error(ErrorCode.RECORDING_AMBIENT,
                      "Stop ambient recording on this device first", 409)

    live = _online(device_id)
    await loop.run_in_executor(None, db.set_collect_mode, device_id, enabled)

    if live is not None:
        # The controller writes the device log line itself, since it is the
        # thing that knows the mode actually took effect (and how many clips
        # a session produced on the way out).
        await request.app[SERVICES].set_collect_mode(live, enabled)
    else:
        await push_log_event(
            device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
            f"Sample collection {'armed' if enabled else 'disarmed'} — "
            f"device offline, takes effect on its next connect",
        )
    return _ok({
        "enabled":   enabled,
        "connected": live is not None,
        "samples":   await loop.run_in_executor(
            None, em_samples.usage, device_id
        ),
        # Both training corpora this device is filling, so the dashboard can
        # show what arming (or disarming) collection is costing the volume
        # without a second round trip. Wake clips accrue independently of
        # collect mode — they are here because this is the one response that
        # already reports per-feature disk use.
        "wakeclips": await loop.run_in_executor(
            None, em_wakeclips.usage, device_id
        ),
    })


@auth.require_auth
async def _get_samples(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/samples — the clips collected from this device,
    newest first, with the mode's own state alongside.

    Served from the filesystem rather than a table: the files ARE the
    record, and a DB row that disagreed with the volume (a restored backup,
    a hand-deleted file) would be a second source of truth for no gain.
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)
    clips = await loop.run_in_executor(None, em_samples.list_for, device_id)
    live  = _online(device_id)
    # What the segmenter is hearing, while it is hearing it. Without this,
    # "I turned it on and got nothing" has no answer short of a log tail:
    # a room whose floor sits 3dB under the open threshold and one where the
    # mic is muted produce the same empty list. `dropped_short` separates a
    # third case — something IS crossing the threshold, and it is a click.
    seg = live.collect_seg if live is not None else None
    return _ok({
        "enabled":  bool(row.collect_mode),
        "clips":    clips,
        "count":    len(clips),
        "bytes":    sum(c["bytes"] for c in clips),
        "ms":       sum(c["ms"] for c in clips),
        "keep":     em_samples.KEEP_PER_DEVICE,
        # Session counters live on the connection, so they reset when the
        # device does — the file count above is the durable number.
        "session":  live.collect_clips if live is not None else 0,
        "live": None if seg is None else {
            "floor_db":      round(seg.floor_db, 1),
            "open_db":       round(seg.open_db, 1),
            "frames":        seg.stats.frames,
            "dropped_short": seg.stats.dropped_short,
            "truncated":     seg.stats.truncated,
        },
    })


@auth.require_auth
async def _get_sample_audio(request: web.Request) -> web.StreamResponse:
    """GET /api/devices/{id}/samples/{name} — one clip, as a WAV.

    em_samples.resolve re-checks that the name belongs to the device in the
    URL: both come from the path, so without it a name from one device
    would reach another's audio."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_samples.resolve(device_id, name)
    if path is None:
        return _error(ErrorCode.NO_SAMPLE, "No such sample", 404)
    label = _slug(device_id)
    return web.FileResponse(
        path,
        headers={
            "Content-Type":        "audio/wav",
            "Content-Disposition": f'attachment; filename="{label}-{name}"',
            # Immutable once written, but the retention cap means a name can
            # stop resolving — private and brief, never shared.
            "Cache-Control":       "private, max-age=60",
        },
    )


@auth.require_auth
async def _get_samples_zip(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/samples.zip — every clip in one archive.

    This is what the feature is FOR: the clips are training input, and a
    training run wants the set, not one file at a time. Built in memory in
    an executor — the retention cap bounds it at ~64MB, and streaming a zip
    would mean either holding the response open across a prune or writing a
    temporary file on the same volume the clips live on.
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)

    label = _slug(row.label or device_id)

    blob = await loop.run_in_executor(None, lambda: _zip_clips(
        label, [clip["name"] for clip in em_samples.list_for(device_id)],
        lambda name: em_samples.resolve(device_id, name)))
    if blob is None:
        return _error(ErrorCode.NO_SAMPLES, "No samples collected yet", 404)
    return web.Response(
        body=blob,
        headers={
            "Content-Type":        "application/zip",
            "Content-Disposition": f'attachment; filename="{label}-samples.zip"',
            "Cache-Control":       "no-store",
        },
    )


@auth.require_admin
async def _delete_sample(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/samples/{name} — drop one clip.

    Triage: a clip that caught the dishwasher rather than the wake word is
    worse than no clip, because it trains the model toward it."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_samples.resolve(device_id, name)
    if path is None:
        return _error(ErrorCode.NO_SAMPLE, "No such sample", 404)
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, path.unlink)
    return _ok({"deleted": name})


@auth.require_admin
async def _delete_samples(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/samples — drop every clip for this device."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    await _device_row(device_id)
    removed = await loop.run_in_executor(None, em_samples.delete_all, device_id)
    await push_log_event(
        device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
        f"Deleted {removed} collected sample(s)",
    )
    return _ok({"deleted": removed})


# ─── Wake clips ───────────────────────────────────────────────────────────────
#
# Accepted-candidate support audio kept per turn when saveWakeClips is on.
# Sample collection gathers prompted wake words; this captures production
# detections for model evaluation and false-positive triage.


@auth.require_auth
async def _get_turn_wake_audio(request: web.Request) -> web.StreamResponse:
    """GET /api/devices/{id}/turns/{turn}/wake — the pre-detection audio that
    triggered one voice turn, as a downloadable WAV.

    Only turns detected while saveWakeClips was on have one, and only the
    newest em_wakeclips.KEEP_PER_DEVICE per device survive — that window is
    shorter than the turns table's, so a turn row can carry a wake_file whose
    file is already pruned. A 404 here is an ordinary outcome, not an error
    state.

    The filename is derived from (device, turn) rather than taken from the
    row: em_wakeclips.resolve then re-checks that the file belongs to the
    device in the URL, so a turn id from another device can't be used to
    reach its audio."""
    device_id = request.match_info["id"]
    try:
        turn_id = int(request.match_info["turn"])
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "turn must be an integer", 400)

    row = await _device_row(device_id)

    path = em_wakeclips.resolve(device_id, em_wakeclips.filename(turn_id))
    if path is None:
        return _error(ErrorCode.NO_WAKE_CLIP,
                      "No saved wake clip for this turn", 404)

    label = _slug(row.label or device_id)
    return web.FileResponse(
        path,
        headers={
            "Content-Type":        "audio/wav",
            "Content-Disposition": f'attachment; filename="{label}-wake{turn_id}.wav"',
            # Immutable once written and unique per turn, but the retention
            # window means a name can stop resolving — so cache privately and
            # briefly, never shared.
            "Cache-Control":       "private, max-age=60",
        },
    )


def _zip_clips(label: str, names: list[str],
               resolve: Callable[[str], Path | None]) -> bytes | None:
    """
    Every listed clip that still resolves, under `label/` in one in-memory
    zip; None when nothing is listed. Blocking — call it in an executor.

    ZIP_STORED: WAV of speech does not compress meaningfully, and deflating
    tens of MB would hold a worker for seconds.
    """
    if not names:
        return None
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
        for name in names:
            path = resolve(name)
            if path is None:
                continue      # pruned between listing and reading
            z.write(path, arcname=f"{label}/{name}")
    return buf.getvalue()


@auth.require_auth
async def _get_wakeclips_zip(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/wakeclips.zip — every wake clip in one archive.

    This is what the feature is FOR: the clips are training input, and a
    training run wants the set, not one false positive at a time. The
    per-turn endpoint above is for deciding whether a clip belongs in the
    set; this is how the set leaves the controller. Built in memory in an
    executor — KEEP_PER_DEVICE bounds it at ~23MB, and streaming a zip would
    mean either holding the response open across a prune or writing a
    temporary file on the same volume the clips live on.
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)

    label = _slug(row.label or device_id)

    blob = await loop.run_in_executor(None, lambda: _zip_clips(
        label, [clip["name"] for clip in em_wakeclips.list_for(device_id)],
        lambda name: em_wakeclips.resolve(device_id, name)))
    if blob is None:
        return _error(ErrorCode.NO_WAKE_CLIPS, "No wake clips saved yet", 404)
    return web.Response(
        body=blob,
        headers={
            "Content-Type":        "application/zip",
            "Content-Disposition": f'attachment; filename="{label}-wakeclips.zip"',
            "Cache-Control":       "no-store",
        },
    )


@auth.require_admin
async def _delete_wakeclips(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/wakeclips — drop every wake clip for this
    device.

    The turn rows keep their wake_file: the column records that a clip was
    written, and rewriting history to hide a file someone deleted on purpose
    would cost a write per turn to say nothing the 404 does not already."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    await _device_row(device_id)
    removed = await loop.run_in_executor(None, em_wakeclips.delete_all, device_id)
    await push_log_event(
        device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
        f"Deleted {removed} wake clip(s)",
    )
    return _ok({"deleted": removed})


# ─── Ambient recording ────────────────────────────────────────────────────────
#
# The mode (em_ambient, em_controller.set_ambient_mode) plus the read side.
# Same shape as sample collection above and persisted the same way (schema
# v19) — the difference is what comes out: one WAV covering the whole session
# rather than a set of clips, so there is no archive endpoint. One recording
# IS the artefact, and zipping ~350MB of them in memory from a dashboard
# click is a way to take the controller down.


@auth.require_admin
async def _post_device_ambient(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/ambient — body {"enabled": bool}

    Holds this device's mic open and writes everything it hears to one file;
    switching it off finalises that file and lists it. This is the room-noise
    half of a training set — the negatives a wake model is mixed against, and
    material for evaluating room-noise policy.

    Admin-only and one device at a time, for collect mode's reasons: it
    SUSPENDS the assistant on that device, and a device that answers nothing
    looks broken to everyone else in the house.

    Refused while sample collection is on, rather than silently sharing the
    stream: the two modes want the same frames for opposite purposes, and a
    user who armed both would get an ambient file full of the wake word they
    were saying for the segmenter.

    Persisted even when the device is offline — handle_control re-arms it on
    the next connect, in a new file.
    """
    device_id = request.match_info["id"]
    body = await _optional_json_body(request)
    enabled = bool(body.get("enabled"))

    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)
    if not row.approved:
        return _error(ErrorCode.NOT_APPROVED,
                      "Approve this device before recording from it", 409)
    if enabled and bool(row.collect_mode):
        return _error(ErrorCode.COLLECTING,
                      "Stop wake-word sample collection on this device first",
                      409)

    live = _online(device_id)
    await loop.run_in_executor(None, db.set_ambient_mode, device_id, enabled)

    if live is not None:
        # The controller writes the device log line itself: it is the thing
        # that knows whether a file was opened, and which one was kept on the
        # way out.
        await request.app[SERVICES].set_ambient_mode(live, enabled)
    else:
        await push_log_event(
            device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
            f"Ambient recording {'armed' if enabled else 'disarmed'} — "
            f"device offline, takes effect on its next connect",
        )
    return _ok({
        "enabled":   enabled,
        "connected": live is not None,
        # A live device reports what actually happened: arming can fail if
        # the file cannot be opened, and reporting the request back would be
        # a dashboard showing a recording that is not running.
        "recording": live.ambient_mode if live is not None else False,
        "usage":     await loop.run_in_executor(
            None, em_ambient.usage, device_id
        ),
    })


@auth.require_auth
async def _get_ambient(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/ambient — this device's finished recordings,
    newest first, with the mode's own state alongside.

    The recording currently open is NOT in the list — it is a `.part` until
    it is closed, and half a WAV is not something to hand a browser. Its
    elapsed length is reported separately as `live`, which is the only
    feedback a mode with one artefact at the end can give while it runs.
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)
    items = await loop.run_in_executor(None, em_ambient.list_for, device_id)
    live  = _online(device_id)
    rec   = live.ambient_rec if live is not None else None
    return _ok({
        "enabled":  bool(row.ambient_mode),
        "clips":    items,
        "count":    len(items),
        "bytes":    sum(i["bytes"] for i in items),
        "ms":       sum(i["ms"] for i in items),
        "keep":     em_ambient.KEEP_PER_DEVICE,
        "maxMs":    em_ambient.MAX_RECORDING_MS,
        "session":  live.ambient_files if live is not None else 0,
        "live": None if rec is None else {
            "ms":         rec.duration_ms,
            "bytes":      rec.data_bytes,
            "startedMs":  rec.started_ms,
        },
    })


@auth.require_auth
async def _get_ambient_audio(request: web.Request) -> web.StreamResponse:
    """GET /api/devices/{id}/ambient/{name} — one recording, as a WAV.

    Served with FileResponse rather than read into memory: these are tens of
    megabytes each, which is exactly the size that must not be buffered per
    request. em_ambient.resolve re-checks that the name belongs to the device
    in the URL — both come from the path."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_ambient.resolve(device_id, name)
    if path is None:
        return _error(ErrorCode.NO_RECORDING, "No such recording", 404)
    label = _slug(device_id)
    return web.FileResponse(
        path,
        headers={
            "Content-Type":        "audio/wav",
            "Content-Disposition": f'attachment; filename="{label}-ambient-{name}"',
            "Cache-Control":       "private, max-age=60",
        },
    )


@auth.require_admin
async def _delete_ambient(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/ambient/{name} — drop one recording.

    These are the largest artefacts the controller stores, and one that
    caught a houseful of guests rather than a quiet room is worth nothing
    but disk."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_ambient.resolve(device_id, name)
    if path is None:
        return _error(ErrorCode.NO_RECORDING, "No such recording", 404)
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, path.unlink)
    return _ok({"deleted": name})


@auth.require_admin
async def _delete_ambient_all(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/ambient — drop every recording for a device.

    The recording currently open is untouched: it is not one of these files
    yet, and stopping the mode is how you end it."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    await _device_row(device_id)
    removed = await loop.run_in_executor(None, em_ambient.delete_all, device_id)
    await push_log_event(
        device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
        f"Deleted {removed} ambient recording(s)",
    )
    return _ok({"deleted": removed})


def _number(value: object) -> int | float | None:
    """A numeric DB value as stored (int stays int on the wire), else None."""
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return value
    return None


def _pct(sorted_values: Sequence[int | float], p: float) -> int | float | None:
    if not sorted_values:
        return None
    return sorted_values[min(len(sorted_values) - 1, int(len(sorted_values) * p))]


class _DayRollup(TypedDict):
    date: str
    turns: int
    ok: int
    outcomes: dict[str, int]
    terminal_reasons: dict[str, int]
    commit_routes: dict[str, int]
    attributions: dict[str, int]
    total_ms_p50: int | float | None
    total_ms_p95: int | float | None
    wake_score_avg: float | None
    wake_score_min: float | None
    underruns: int | float


class _ModelRollup(TypedDict):
    turns: int
    score_avg: float | None
    score_min: int | float | None


def _day_rollups(turns: list[dict[str, object]]) -> list[_DayRollup]:
    """Per local-calendar-day turn counts, outcomes and latency/score figures."""
    day_buckets: dict[str, list[dict[str, object]]] = {}
    for turn in turns:
        ts = _number(turn.get("ts"))
        if ts is None:
            continue
        day = time.strftime("%Y-%m-%d", time.localtime(ts))
        day_buckets.setdefault(day, []).append(turn)

    days_out: list[_DayRollup] = []
    for day in sorted(day_buckets):
        bucket = day_buckets[day]
        ok = [turn for turn in bucket if turn.get("outcome") == "ok"]
        totals = sorted(total for turn in ok
                        if (total := _number(turn.get("total_ms"))) is not None and total > 0)
        scores = [score for turn in bucket
                  if (score := _number(turn.get("wake_score"))) is not None]
        outcomes: dict[str, int] = {}
        terminal: dict[str, int] = {}
        routes: dict[str, int] = {}
        attribution: dict[str, int] = {}
        for turn in bucket:
            for column, dest in (("outcome", outcomes), ("terminal_reason", terminal),
                                 ("commit_route", routes), ("wake_attribution", attribution)):
                value = turn.get(column)
                key = value if isinstance(value, str) and value else "?"
                dest[key] = dest.get(key, 0) + 1
        days_out.append({
            "date": day,
            "turns": len(bucket),
            "ok": len(ok),
            "outcomes": outcomes,
            "terminal_reasons": terminal,
            "commit_routes": routes,
            "attributions": attribution,
            "total_ms_p50": _pct(totals, 0.50),
            "total_ms_p95": _pct(totals, 0.95),
            "wake_score_avg": round(sum(scores) / len(scores), 3) if scores else None,
            "wake_score_min": round(min(scores), 3) if scores else None,
            "underruns": sum(_number(turn.get("underruns")) or 0 for turn in bucket),
        })
    return days_out


def _model_rollups(turns: list[dict[str, object]]) -> dict[str, _ModelRollup]:
    """Turn count and wake-score figures per wake graph."""
    scores_by_graph: dict[str, list[int | float]] = {}
    turns_by_graph: dict[str, int] = {}
    for turn in turns:
        graph = turn.get("wake_model_sha256")
        if not isinstance(graph, str) or not graph:
            continue
        turns_by_graph[graph] = turns_by_graph.get(graph, 0) + 1
        scores = scores_by_graph.setdefault(graph, [])
        score = _number(turn.get("wake_score"))
        if score is not None:
            scores.append(score)
    return {
        graph: {
            "turns": count,
            "score_avg": (round(sum(scores_by_graph[graph]) / len(scores_by_graph[graph]), 3)
                          if scores_by_graph[graph] else None),
            "score_min": min(scores_by_graph[graph]) if scores_by_graph[graph] else None,
        }
        for graph, count in turns_by_graph.items()
    }


def _device_wake_totals(counters: list[dict[str, object]]) -> dict[str, int | float]:
    """The device's own wake counters (wake.stats), summed or maxed over hours."""
    def total(column: str) -> int | float:
        return sum(_number(row.get(column)) or 0 for row in counters)

    def peak(column: str) -> int | float:
        return max((_number(row.get(column)) or 0 for row in counters), default=0)

    return {
        "hops": total("dev_hops"),
        "overruns": total("dev_drops"),
        "candidates": total("dev_crossings"),
        "near_misses": total("near_misses"),
        "near_miss_max": peak("near_miss_max"),
        "max_infer_ms": peak("dev_max_infer_ms"),
        "max_score": peak("dev_max_score"),
    }


@auth.require_auth
async def _get_device_activity(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/activity?days=7 — turn and device-wake rollups."""
    device_id = request.match_info["id"]
    try:
        days = min(max(int(request.query.get("days", 7)), 1), 180)
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "days must be an integer", 400)
    since = time.time() - days * 86400
    loop = asyncio.get_running_loop()
    turns, raw_counters, metrics = await asyncio.gather(
        loop.run_in_executor(None, db.get_turns, device_id, 50_000, since),
        loop.run_in_executor(None, db.get_wake_counters, device_id, since),
        loop.run_in_executor(None, db.get_device_metrics, device_id, since),
    )
    counters: list[dict[str, object]] = [asdict(row) for row in raw_counters]
    return _ok({"days": _day_rollups(turns), "wake_models": _model_rollups(turns),
                "wake_counters": counters, "device_wake": _device_wake_totals(counters),
                "metrics": metrics})


@auth.require_auth
async def _get_device_wake_shadow(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/wake_shadow?days=7 — shadow open rules (§5.2): per
    rule the would-be opens against the live rules, and the newest events."""
    device_id = request.match_info["id"]
    try:
        days = min(max(int(request.query.get("days", 7)), 1), 180)
    except ValueError:
        return _error(ErrorCode.BAD_REQUEST, "days must be an integer", 400)
    since = time.time() - days * 86400
    loop = asyncio.get_running_loop()
    totals, events, config = await asyncio.gather(
        loop.run_in_executor(None, db.get_wake_shadow, device_id, since),
        loop.run_in_executor(None, db.get_wake_shadow_events, device_id, since, 50),
        loop.run_in_executor(None, db.get_effective_device_config, device_id),
    )
    configured = em_wake_rules.RuleSet.from_config(config).shadow
    return _ok({"days": days, "rules": em_wake_rules.summarize(totals, configured),
                "events": [asdict(e) for e in events]})


# The windows, in hours, of the Status tab's response-latency percentiles.
RESPONSE_LATENCY_WINDOWS_H = (24, 7 * 24, 30 * 24)


class _LatencyWindow(TypedDict):
    hours: int
    turns: int                  # turns that measured a response latency
    p50: int | float | None     # None: no turn in the window measured one
    p90: int | float | None
    p95: int | float | None
    p99: int | float | None


def _latency_windows(samples: Sequence[tuple[float, int]], now: float) -> list[_LatencyWindow]:
    """Percentiles of `samples` ((ts, response_latency_ms)) per RESPONSE_LATENCY_WINDOWS_H."""
    out: list[_LatencyWindow] = []
    for hours in RESPONSE_LATENCY_WINDOWS_H:
        since = now - hours * 3600
        values = sorted(ms for ts, ms in samples if ts >= since)
        out.append({"hours": hours, "turns": len(values), "p50": _pct(values, 0.50),
                    "p90": _pct(values, 0.90), "p95": _pct(values, 0.95), "p99": _pct(values, 0.99)})
    return out


@auth.require_auth
async def _get_device_response_latency(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/response_latency — percentiles of the end of the
    user's last word → the response's first frame played (turns.response_latency_ms,
    the Dot's clock) over each RESPONSE_LATENCY_WINDOWS_H window. Turns that
    measured none are left out, never counted as zero."""
    device_id = request.match_info["id"]
    await _device_row(device_id)
    now = time.time()
    samples = await asyncio.get_running_loop().run_in_executor(
        None, db.get_response_latencies, device_id, now - max(RESPONSE_LATENCY_WINDOWS_H) * 3600)
    return _ok({"windows": _latency_windows(samples, now)})


@auth.require_admin
async def _patch_device(request: web.Request) -> web.Response:
    """PATCH /api/devices/{id} — update label."""
    device_id = request.match_info["id"]
    body  = await _json_body(request)
    label = _require_str(body, "label")

    loop = asyncio.get_running_loop()
    await _device_row(device_id)

    await loop.run_in_executor(None, db.set_device_label, device_id, label)
    device = _devices.get(device_id)
    if device is not None:
        device.label = label
    await push_device_update(device_id, {"label": label})
    return _ok({"device_id": device_id, "label": label})


@auth.require_admin
async def _delete_device(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id} — remove from registry."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    await _device_row(device_id)

    await loop.run_in_executor(None, db.delete_device, device_id)
    await request.app[SERVICES].remove_device(device_id)
    # Row gone → reconcile tears down any BT proxy listener/mDNS for it.
    await em_ble_proxy.reconcile(device_id)
    await _push_event(EventType.DEVICE_DELETED, device_id=device_id)
    return _ok({})


@auth.require_admin
async def _post_approve(request: web.Request) -> web.Response:
    """Approve a pending device, assign its label, and optionally set config."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    label = _require_str(body, "label")
    config = body.get("config")
    if config is not None and not isinstance(config, dict):
        return _error(ErrorCode.BAD_REQUEST, "config must be an object", 400)
    if config:
        error = _validate_config(config, request.app[SERVICES].registry())
        if error is not None:
            return error
    loop = asyncio.get_running_loop()
    row = await _device_row(device_id)
    if row.approved:
        return _error(ErrorCode.ALREADY_APPROVED, "Device is already approved", 409)
    await loop.run_in_executor(None, db.approve_device, device_id, label, config)
    await _push_event(EventType.DEVICE_APPROVED, device_id=device_id, label=label)
    return _ok({"device_id": device_id, "label": label})


async def _apply_live_config(device_id: str, device: em_device.Device,
                             effective: Mapping[str, object]) -> bool:
    """Apply a full effective config through the Device-owned fanout."""
    if not device.online:
        return False
    try:
        await device.apply_config(dict(effective))
    except Exception:
        # A session can close between the online check and send. Persisted
        # config is still authoritative and will apply at the next admission.
        if _online(device_id) is None:
            log.info("[api] %s disconnected during config apply", device_id)
            return False
        raise
    return True


def _validate_config(values: Mapping[str, object],
                     registry: em_wake_registry.WakeRegistry) -> web.Response | None:
    """Validate config keys and post-AFE keys before any persistence."""
    for key in values:
        if key in db.REMOVED_CONFIG_KEYS:
            return _error(ErrorCode.REMOVED_CONFIG_KEY,
                          f"Configuration key '{key}' was removed", 400)
        if key not in db.DEFAULT_DEVICE_CONFIG:
            return _error(ErrorCode.UNKNOWN_CONFIG_KEY,
                          f"Unknown configuration key: {key}", 400)
    if "wakeModel" in values:
        value = values["wakeModel"]
        if not isinstance(value, str):
            return _error(ErrorCode.INVALID_CONFIG, "wakeModel must be a graph SHA-256", 400)
        try:
            registry.get(value)
        except em_wake_registry.RegistryError:
            return _error(ErrorCode.UNKNOWN_WAKE_MODEL,
                          f"wakeModel is not registered: {value}", 400)
    if "extendedUtterances" in values and not isinstance(values["extendedUtterances"], bool):
        return _error(ErrorCode.INVALID_CONFIG, "extendedUtterances must be boolean", 400)
    if em_pause_asr.PAUSE_ASR_KEY in values:
        try:
            em_pause_asr.parse_pause_asr(values[em_pause_asr.PAUSE_ASR_KEY])
        except ValueError as err:
            return _error(ErrorCode.INVALID_CONFIG, f"{em_pause_asr.PAUSE_ASR_KEY}: {err}", 400)
    if em_wake_rules.OPEN_RULES_KEY in values or em_wake_rules.SHADOW_RULES_KEY in values:
        # The baseline of the model this write selects (else the fleet's); a
        # device's own model may differ, and session.ready drops any extra
        # rule equal to its baseline.
        try:
            model = registry.for_config(values)
            model_baseline = em_wake_rules.baseline(model.thresholds.idle, model.thresholds.playback)
        except em_wake_registry.RegistryError:
            model_baseline = None
        message = em_wake_rules.validate_config(values, model_baseline)
        if message is not None:
            return _error(ErrorCode.INVALID_CONFIG, message, 400)
    for key in em_sounds.SOUND_CONFIG_KEYS:
        if key not in values:
            continue
        value = values[key]
        if value in (None, ""):
            continue
        if not isinstance(value, str) or em_sounds.safe_sound_id(value) is None:
            return _error(ErrorCode.INVALID_CONFIG, f"{key} must be a valid sound id or empty", 400)
    if "timerRingSeconds" in values:
        value = values["timerRingSeconds"]
        if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= 3600:
            return _error(ErrorCode.INVALID_CONFIG,
                          "timerRingSeconds must be an integer from 1 through 3600", 400)
    return None


@auth.require_auth
async def _get_device_config(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/config — effective config and section scoping."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    await _device_row(device_id)
    config, section_ids = await asyncio.gather(
        loop.run_in_executor(None, db.get_effective_device_config, device_id),
        loop.run_in_executor(None, db.get_device_config_sections, device_id),
    )
    return _ok({"config": config, "config_sections": section_ids,
                "use_global_config": not section_ids})


@auth.require_admin
async def _post_device_config(request: web.Request) -> web.Response:
    """POST a device config. Values outside overridden sections are ignored;
    every accepted key must be part of the post-AFE schema."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    loop = asyncio.get_running_loop()
    await _device_row(device_id)

    sections_body = body.pop("config_sections", None)
    use_global = body.pop("use_global_config", None)
    explicit_replace = bool(body.pop("replace", False))
    error = _validate_config(body, request.app[SERVICES].registry())
    if error is not None:
        return error

    if sections_body is None and use_global is not None:
        if not isinstance(use_global, bool):
            return _error(ErrorCode.BAD_REQUEST, "use_global_config must be boolean", 400)
        sections_body = [] if use_global else list(sections_mod.SECTION_IDS)
    if sections_body is None:
        new_sections = await loop.run_in_executor(
            None, db.get_device_config_sections, device_id)
    else:
        if not isinstance(sections_body, list):
            return _error(ErrorCode.BAD_REQUEST, "config_sections must be a list", 400)
        unknown = [section for section in sections_body
                   if section not in sections_mod.SECTIONS]
        if unknown:
            return _error(ErrorCode.BAD_REQUEST,
                          f"Unknown config section(s): {', '.join(map(str, unknown))}", 400)
        new_sections = sections_mod.normalise(sections_body)

    in_scope = sections_mod.keys_for(new_sections) | sections_mod.STATE_KEYS
    stored = await loop.run_in_executor(None, db.get_device_config, device_id)
    stored_in_scope = {key: value for key, value in stored.items() if key in in_scope}
    dropped = _dropped_keys(body, stored_in_scope)
    if dropped and not explicit_replace:
        return _error(ErrorCode.WOULD_DROP_KEYS,
                      "This body would delete existing setting(s): " + ", ".join(dropped), 409)

    if sections_body is not None:
        await loop.run_in_executor(
            None, db.set_device_config_sections, device_id, new_sections)
    values = {key: value for key, value in body.items() if key in in_scope}
    if values:
        current = await loop.run_in_executor(None, db.get_device_config, device_id)
        await loop.run_in_executor(None, db.set_device_config, device_id,
                                   {**current, **values})
    config = await loop.run_in_executor(None, db.get_effective_device_config, device_id)

    device = _devices.get(device_id)
    pushed = bool(device is not None and await _apply_live_config(device_id, device, config))
    await em_ble_proxy.reconcile(device_id)
    await push_device_update(device_id, {
        "config": config, "config_sections": new_sections,
        "use_global_config": not new_sections,
    })
    return _ok({"device_id": device_id, "config": config,
                "config_sections": new_sections,
                "use_global_config": not new_sections, "pushed": pushed})


@auth.require_admin
async def _post_device_wifi(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/wifi — switch the device to a new WiFi network.

    Body: {"ssid": "...", "psk": "..."} (empty/absent psk = open network).

    Returns 202 immediately: the device owns the whole switch (associate →
    DHCP → reconnect gates, auto-rollback on any failure — see the device's
    internal/wifi package). The outcome arrives asynchronously as a
    wifi_result control message and is surfaced via the device_update
    event / the "wifi" field on the device object.
    """
    device_id = request.match_info["id"]
    body = await _json_body(request)
    ssid = _require_str(body, "ssid")
    psk  = str(body.get("psk") or "")

    # Mirror the device's own validation so obvious mistakes fail fast
    # with a readable message instead of a full switch/rollback cycle.
    if any(ch in ssid or ch in psk for ch in ('"', "\\")):
        return _error(ErrorCode.INVALID_CREDENTIALS,
                      "SSID/passphrase cannot contain double-quote or "
                      "backslash characters (wpa_supplicant.conf cannot "
                      "represent them safely)", 400)
    if psk and not 8 <= len(psk) <= 63:
        return _error(ErrorCode.INVALID_CREDENTIALS,
                      f"WPA passphrase must be 8–63 characters (got {len(psk)})", 400)

    live = _require_online(device_id)
    st = _wifi_change(device_id)
    if st.pending is not None:
        return _error(ErrorCode.WIFI_CHANGE_IN_PROGRESS,
                      f"A change to \"{st.pending.ssid}\" is already "
                      f"in progress", 409)

    previous = st.last_result
    st.pending = _WifiPending(ssid=ssid, started_at=time.time())
    st.last_result = None
    try:
        await live.send(MessageType.WIFI_CHANGE, {"ssid": ssid, "psk": psk})
    except em_device_link.LinkClosed:
        # Nothing reached the device, so nothing is pending: left set, the
        # change would block every retry until _WIFI_PENDING_TTL.
        st.pending, st.last_result = None, previous
        return _error(ErrorCode.DEVICE_OFFLINE, "Device is not connected", 409)
    await asyncio.get_running_loop().run_in_executor(
        None, db.log_device, device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
        f'WiFi change to "{ssid}" requested')
    await push_device_update(device_id, {"wifi": st.wire()})
    return _ok({"device_id": device_id, "ssid": ssid, "status": "switching"},
               status=202)


@auth.require_admin
async def _post_device_wifi_scan(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/wifi/scan — ask the device for visible networks.

    Synchronous from the dashboard's point of view: sends wifi_scan and
    awaits the wifi_scan_result control message (the device's scan itself
    takes ~5s).
    """
    device_id = request.match_info["id"]
    live = _require_online(device_id)
    if live.wifi_scan_future is not None:
        return _error(ErrorCode.SCAN_IN_PROGRESS, "A scan is already running", 409)

    fut: asyncio.Future[dict[str, object]] = asyncio.get_running_loop().create_future()
    live.wifi_scan_future = fut
    try:
        await live.send(MessageType.WIFI_SCAN, {})
        msg = await asyncio.wait_for(fut, timeout=20)
    except em_device_link.LinkClosed:
        return _error(ErrorCode.DEVICE_OFFLINE, "Device is not connected", 409)
    except asyncio.TimeoutError:
        return _error(ErrorCode.SCAN_TIMEOUT,
                      "Device did not return scan results within 20s "
                      "(old firmware without WiFi support?)", 504)
    finally:
        live.wifi_scan_future = None
    error = msg.get("error")
    if error:
        return _error(ErrorCode.SCAN_FAILED, str(error), 502)
    return _ok({"networks": msg.get("networks") or []})


@auth.require_auth
async def _get_device_logs(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/logs

    Query params:
      limit  — max rows (default 100, max 1000)
      before — cursor: return entries with ts < before (unix ms)
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()

    await _device_row(device_id)

    try:
        limit = int(request.rel_url.query.get("limit", "100"))
    except ValueError:
        return _error(ErrorCode.INVALID_PARAM, "limit must be an integer", 400)

    before_param = request.rel_url.query.get("before")
    before_ts = None
    if before_param:
        try:
            before_ts = int(before_param)
        except ValueError:
            return _error(ErrorCode.INVALID_PARAM, "before must be a unix ms timestamp", 400)

    rows = await loop.run_in_executor(
        None, db.get_device_logs, device_id, limit, before_ts
    )
    entries = [
        {
            "id":        r.id,
            "ts":        r.ts,
            "level":     r.level,
            "source":    r.source,
            "message":   r.message,
        }
        for r in rows
    ]
    return _ok(entries)


# ─── Script-driven capture ────────────────────────────────────────────────────
#
# The mode (em_controller.set_capture_mode) plus windows inside it. Unlike
# sample collection this is NOT persisted and cannot be armed on an offline
# device: the webhook is a running process's address, so there is nothing
# useful to remember about it — see em_capture's docstring.


@auth.require_admin
async def _post_device_capture(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/capture — body
        {"enabled": true, "webhook": "http://host:port/clip", "idle_s": 300}

    Puts a device into (or out of) script-driven capture: it starts no voice
    turns, and every recording window opened below is POSTed to `webhook` as
    a WAV.

    Admin-only for the same reason collect mode is — it SUSPENDS the
    assistant on that device — and additionally requires the device to be
    CONNECTED, which collect mode does not. Arming an offline device is
    meaningful there (the mode is persisted and re-applied on connect) and
    meaningless here: there is nothing to persist, and the caller is a script
    that is about to start playing audio at a device that is not listening.
    """
    device_id = request.match_info["id"]
    body = await _optional_json_body(request)
    enabled = bool(body.get("enabled"))

    row = await _device_row(device_id)

    live = _online(device_id)
    services = request.app[SERVICES]

    if not enabled:
        if live is not None:
            await services.set_capture_mode(live, False)
        return _ok(services.capture_state(live))

    if not row.approved:
        return _error(ErrorCode.NOT_APPROVED,
                      "Approve this device before capturing from it", 409)
    if live is None:
        return _error(ErrorCode.NOT_CONNECTED,
                      "Capture mode needs a connected device — it is not "
                      "persisted and cannot be armed in advance", 409)

    # Absent webhook means PULL: the controller holds each finished recording
    # and the caller collects it from /capture/recording. That is the mode
    # that works everywhere — push needs a route from the controller back to
    # the caller, which a controller on a macvlan network does not have.
    webhook = body.get("webhook") or None
    if webhook is not None and not em_capture.valid_webhook(webhook):
        return _error(ErrorCode.BAD_WEBHOOK,
                      "webhook must be an http(s) URL with a host", 400)
    idle_s = body.get("idle_s")

    await services.set_capture_mode(
        live, True, webhook=webhook,
        idle_s=None if idle_s is None else em_capture.clamp_idle_s(idle_s),
    )
    return _ok(services.capture_state(live))


@auth.require_admin
async def _post_capture_window(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/capture/window — body {"tag": "...", "max_ms": N}

    Opens a recording window. Returns its session id, which the caller can
    pass back to /capture/window/stop so a stop cannot land on the wrong
    window if the previous one already closed itself on `max_ms`.

    `tag` is opaque here and comes straight back on the delivery as
    `X-EM-Tag`: correlation is the caller's business, and a tag format the
    controller understood would be a second copy to drift.
    """
    device_id = request.match_info["id"]
    body = await _optional_json_body(request)

    live = _online(device_id)
    if live is None:
        return _error(ErrorCode.NOT_CONNECTED, f"Device not connected: {device_id}", 409)
    if not live.capture_mode:
        return _error(ErrorCode.NOT_CAPTURING,
                      "Arm capture mode before opening a window", 409)

    window = await request.app[SERVICES].open_capture_window(
        live,
        tag=str(body.get("tag") or ""),
        max_ms=em_capture.clamp_max_ms(body.get("max_ms")),
    )
    return _ok({
        "session": window.session,
        "tag":     window.tag,
        "max_ms":  window.max_ms,
    })


@auth.require_admin
async def _post_capture_window_stop(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/capture/window/stop — body {"session": "..."} 

    Closes the open window and queues it for delivery. An unknown or already
    finished session is not an error: the window closing itself on `max_ms`
    is a normal outcome, and the caller finds out from the delivery either
    way.
    """
    device_id = request.match_info["id"]
    body = await _optional_json_body(request)
    session = body.get("session")

    live = _online(device_id)
    if live is None:
        return _error(ErrorCode.NOT_CONNECTED, f"Device not connected: {device_id}", 409)

    services = request.app[SERVICES]
    closed = await services.close_capture_window(
        live, session=str(session) if session else None
    )
    return _ok({"closed": closed, **services.capture_state(live)})


@auth.require_admin
async def _get_capture_recording(request: web.Request) -> web.Response:
    """
    GET /api/devices/{id}/capture/recording?session=…&wait=…

    Collect one finished recording, as a WAV, with the same `X-EM-*` headers
    the webhook delivery carries — so a driver can use either transport
    without a second parser.

    This is the PULL half of delivery, and on some deployments it is the only
    half that can work: a controller on a macvlan network cannot reach its
    own Docker host, so a webhook served by the machine driving the run times
    out every time. Pull needs no route back.

    Long-polls up to `wait` seconds (default 30, capped at 120). 404 means
    nothing arrived in that time, which is an ordinary answer rather than an
    error — the driver decides whether to retry or record the cell as failed.
    """
    device_id = request.match_info["id"]
    live = _online(device_id)
    if live is None:
        return _error(ErrorCode.NOT_CONNECTED, f"Device not connected: {device_id}", 409)
    if not live.capture_mode:
        return _error(ErrorCode.NOT_CAPTURING, "Capture mode is not armed", 409)

    session = request.query.get("session") or None
    try:
        wait_s = float(request.query.get("wait", 30))
    except ValueError:
        wait_s = 30.0
    wait_s = max(0.0, min(wait_s, 120.0))

    result = await request.app[SERVICES].take_recording(live, session, wait_s)
    if result is None:
        return _error(ErrorCode.NO_RECORDING, "No recording available", 404)
    return web.Response(
        body=result.wav(),
        headers=em_capture.headers(result, device_id),
    )


@auth.require_auth
async def _get_device_capture(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/capture — the mode's live state.

    Everything here is per-connection, so an offline device reports the mode
    off rather than a stale sink."""
    device_id = request.match_info["id"]
    return _ok(request.app[SERVICES].capture_state(_online(device_id)))


# ─── OTA: update + rollback ───────────────────────────────────────────────────

@auth.require_admin
async def _post_device_update(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/update

    Installs the bundled firmware on the device using A/B slots.

    Returns 202 Accepted — update runs in the background.
    """
    device_id = request.match_info["id"]

    await _device_row(device_id)
    _require_online(device_id)
    if device_id in _updates_in_progress:
        return _error(ErrorCode.UPDATE_IN_PROGRESS, "An update is already in progress", 409)

    services = request.app[SERVICES]
    firmware = services.firmware()
    await _start_install(services.shell, device_id, firmware)
    return _ok({"status": UpdateQueueStatus.STARTED, "version": firmware.version}, status=202)


class UpdateQueueStatus(enum.StrEnum):
    """What asking for the bundled firmware did (`status` of the 202)."""

    STARTED = "started"   # the install is running now
    QUEUED = "queued"     # it runs when the device next connects


def _firmware_behind(row: db.DeviceRow, firmware: em_firmware.BundledFirmware) -> bool:
    """
    Whether the device is not on the bundled firmware: the last version it
    reported — kept on the row, so this holds offline too — differs, or was
    never reported.
    """
    return row.firmware_ver != firmware.version


async def _start_install(shell: em_shell.ShellBroker, device_id: str,
                         firmware: em_firmware.BundledFirmware) -> None:
    """
    Start the bundled-firmware install on a connected device, claiming its OTA
    slot synchronously (see _begin_ota). An install queued for its next
    connect is satisfied by this one, so it is cleared: left queued, a failed
    install would retry on the reconnect that follows its auto-rollback.
    """
    _begin_ota(device_id, _run_update(shell, device_id, firmware))
    await _clear_update_queue(device_id)


async def _clear_update_queue(device_id: str) -> int | None:
    """Clear the device's queued install; returns when it was queued, None
    when nothing was. Tells the dashboard only when something changed."""
    queued_at = await asyncio.get_running_loop().run_in_executor(
        None, db.take_update_queue, device_id)
    if queued_at is not None:
        await _push_event(EventType.DEVICE_UPDATE_QUEUE, device_id=device_id, queued_at=None)
    return queued_at


@auth.require_admin
async def _post_device_update_queue(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/update/queue

    Bring an approved device that is not on the bundled firmware onto it.
    Offline, the request is persisted and runs when the device next connects
    (see _reconcile_update_queue). It names no version: what installs is
    whatever this controller bundles at that connect. A device that is online
    by the time the request lands is installed now, as /update would.

    202 `{"status": "queued"|"started", "version", "queued_at"}`;
    409 not_approved / already_current / update_in_progress.
    """
    device_id = request.match_info["id"]
    row = await _device_row(device_id)
    if not row.approved:
        return _error(ErrorCode.NOT_APPROVED, "Device is not approved", 409)
    services = request.app[SERVICES]
    firmware = services.firmware()
    if not _firmware_behind(row, firmware):
        return _error(ErrorCode.ALREADY_CURRENT,
                      f"Device already runs the bundled firmware {firmware.version}", 409)
    if device_id in _updates_in_progress:
        return _error(ErrorCode.UPDATE_IN_PROGRESS, "An update is already in progress", 409)

    queued_at = int(time.time())
    await asyncio.get_running_loop().run_in_executor(
        None, db.set_update_queued, device_id, queued_at)
    await _push_event(EventType.DEVICE_UPDATE_QUEUE, device_id=device_id, queued_at=queued_at)
    await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                         "Firmware install queued for the next connect "
                         f"(bundled now: {firmware.version})")

    if await _reconcile_update_queue(services.shell, device_id, firmware) \
            is QueuedUpdateOutcome.STARTED:
        return _ok({"status": UpdateQueueStatus.STARTED, "version": firmware.version,
                    "queued_at": None}, status=202)
    return _ok({"status": UpdateQueueStatus.QUEUED, "version": firmware.version,
                "queued_at": queued_at}, status=202)


@auth.require_admin
async def _delete_device_update_queue(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/update/queue — cancel a queued install.
    Idempotent: `{"cancelled": false}` when nothing was queued."""
    device_id = request.match_info["id"]
    await _device_row(device_id)
    cancelled = await _clear_update_queue(device_id) is not None
    if cancelled:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             "Queued firmware install cancelled")
    return _ok({"cancelled": cancelled})


class QueuedUpdateOutcome(enum.StrEnum):
    """What _reconcile_update_queue did with a device's queued install."""

    NONE = "none"          # nothing queued
    WAITING = "waiting"    # stays queued: offline, not approved, or an OTA is running
    CURRENT = "current"    # already on the bundled firmware — cleared, nothing installed
    STARTED = "started"    # install started — cleared


async def _reconcile_update_queue(shell: em_shell.ShellBroker, device_id: str,
                                  firmware: em_firmware.BundledFirmware) -> QueuedUpdateOutcome:
    """
    Act on the device's queued install, if any, against the firmware bundled
    NOW (the queue is "bring this device current", not a pinned version).
    Run on every connect (notify_device_connected) and when a request queues
    one for a device that is already online.
    """
    loop = asyncio.get_running_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None or row.update_queued_at is None:
        return QueuedUpdateOutcome.NONE
    live = _online(device_id)
    if live is None or not row.approved or device_id in _updates_in_progress:
        return QueuedUpdateOutcome.WAITING
    if live.firmware_version == firmware.version:
        if await _clear_update_queue(device_id) is not None:
            await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                                 f"Queued firmware install cleared: device already runs "
                                 f"{firmware.version}")
        return QueuedUpdateOutcome.CURRENT

    queued_at = await loop.run_in_executor(None, db.take_update_queue, device_id)
    if queued_at is None:
        return QueuedUpdateOutcome.NONE        # cancelled meanwhile
    # The take awaited: re-check what it may have changed before claiming the
    # OTA slot, synchronously with the claim. Put the entry back if the device
    # is no longer installable — the request outlives a flapping connection.
    if _online(device_id) is None or device_id in _updates_in_progress:
        await loop.run_in_executor(None, db.set_update_queued, device_id, queued_at)
        return QueuedUpdateOutcome.WAITING
    _begin_ota(device_id, _run_update(shell, device_id, firmware))
    await _push_event(EventType.DEVICE_UPDATE_QUEUE, device_id=device_id, queued_at=None)
    await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                         f"Running queued firmware install → {firmware.version}")
    return QueuedUpdateOutcome.STARTED

@auth.require_admin
async def _post_device_rollback(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/rollback

    Flips the inactive A/B slot back to active. Instant — no binary transfer.
    Requires firmware_previous to be set.
    Returns 202 Accepted.
    """
    device_id = request.match_info["id"]
    row = await _device_row(device_id)
    previous: str | None = row.firmware_previous

    if not previous:
        return _error(ErrorCode.NO_ROLLBACK_AVAILABLE,
                      "No previous version recorded — cannot roll back", 404)

    _require_online(device_id)

    if device_id in _updates_in_progress:
        return _error(ErrorCode.UPDATE_IN_PROGRESS, "An update is already in progress", 409)

    _begin_ota(device_id, _run_rollback(request.app[SERVICES].shell, device_id, previous))
    return _ok({"status": "started", "rolling_back_to": previous}, status=202)


# ─── Wake model registry ─────────────────────────────────────────────────────

_wake_registry_lock = asyncio.Lock()


def _wake_model_users(graph_sha256: str) -> list[str]:
    """Fleet and effective device scopes that select this graph."""
    users: list[str] = []
    if db.get_global_device_config().get("wakeModel") == graph_sha256:
        users.append("global")
    for row in db.get_all_devices():
        device_id = row.device_id
        if db.get_effective_device_config(device_id).get("wakeModel") == graph_sha256:
            users.append(device_id)
    return users


def _wake_model_json(model: em_wake_registry.WakeModel, active: str | None,
                     users: list[str]) -> dict[str, object]:
    return {**model.to_dict(), "active": model.graph_sha256 == active,
            "in_use_by": users}


@auth.require_auth
async def _get_wake_models(request: web.Request) -> web.Response:
    """GET /api/wake_models — registered BCResNet pairs and active graph."""
    registry = request.app[SERVICES].registry()
    active = registry.active_sha256
    loop = asyncio.get_running_loop()
    models = registry.list()
    users = await asyncio.gather(*(
        loop.run_in_executor(None, _wake_model_users, model.graph_sha256)
        for model in models
    ))
    return _ok({
        "active": active,
        "models": [_wake_model_json(model, active, used)
                   for model, used in zip(models, users)],
        "max_graph_bytes": em_wake_registry.MAX_GRAPH_BYTES,
        "max_sidecar_bytes": em_wake_registry.MAX_SIDECAR_BYTES,
    })


async def _read_part(field: aiohttp.BodyPartReader, limit: int) -> bytes | None:
    """Read one multipart part without retaining bytes beyond its limit."""
    out = bytearray()
    while True:
        block = await field.read_chunk()
        if not block:
            return bytes(out)
        out.extend(block)
        if len(out) > limit:
            return None


# The multipart text fields of a wake model upload, all required.
_WAKE_MODEL_TEXT_FIELDS = frozenset({"idle", "playback", "reference", "near_miss",
                                     "wake_phrase", "verify_core"})


@auth.require_admin
async def _post_wake_model_upload(request: web.Request) -> web.Response:
    """POST /api/wake_models/upload — validate, probe, and register a graph pair."""
    try:
        reader = await request.multipart()
    except Exception:
        return _error(ErrorCode.INVALID_UPLOAD, "Expected multipart form data", 400)

    graph_bytes: bytes | None = None
    sidecar_bytes: bytes | None = None
    text: dict[str, str] = {}
    while True:
        field = await reader.next()
        if field is None:
            break
        if not isinstance(field, aiohttp.BodyPartReader):
            await field.release()      # a nested multipart is nothing we asked for
        elif field.name == "graph":
            graph_bytes = await _read_part(field, em_wake_registry.MAX_GRAPH_BYTES)
            if graph_bytes is None:
                return _error(ErrorCode.TOO_LARGE, "Graph exceeds the upload limit", 413)
        elif field.name == "sidecar":
            sidecar_bytes = await _read_part(field, em_wake_registry.MAX_SIDECAR_BYTES)
            if sidecar_bytes is None:
                return _error(ErrorCode.TOO_LARGE, "Sidecar exceeds the upload limit", 413)
        elif field.name in _WAKE_MODEL_TEXT_FIELDS:
            value = await _read_part(field, 1024)
            if value is None:
                return _error(ErrorCode.INVALID_UPLOAD, f"Field {field.name} is too long", 400)
            text[field.name] = value.decode("utf-8", "replace").strip()
        else:
            await field.release()

    missing = sorted(name for name, part in (("graph", graph_bytes), ("sidecar", sidecar_bytes))
                     if part is None)
    missing += sorted(_WAKE_MODEL_TEXT_FIELDS - text.keys())
    if missing:
        return _error(ErrorCode.INVALID_UPLOAD, f"Missing multipart field(s): {', '.join(missing)}", 400)
    if not graph_bytes or not sidecar_bytes:
        return _error(ErrorCode.INVALID_UPLOAD, "Graph and sidecar must not be empty", 400)
    try:
        thresholds = em_wake_registry.Thresholds(
            idle=float(text["idle"]), playback=float(text["playback"]),
            reference=float(text["reference"]), near_miss=float(text["near_miss"]),
        )
    except (TypeError, ValueError, em_wake_registry.RegistryError) as exc:
        return _error(ErrorCode.INVALID_MODEL, str(exc), 400)

    registry = request.app[SERVICES].registry()

    def register(graph_bytes: bytes, sidecar_bytes: bytes) -> em_wake_registry.WakeModel:
        with tempfile.TemporaryDirectory(prefix="wake-upload-") as directory:
            graph = Path(directory) / "graph.onnx"
            sidecar = Path(directory) / "sidecar.json"
            graph.write_bytes(graph_bytes)
            sidecar.write_bytes(sidecar_bytes)
            return registry.register(graph, sidecar, thresholds,
                                     text["wake_phrase"], text["verify_core"])

    try:
        async with _wake_registry_lock:
            model = await asyncio.get_running_loop().run_in_executor(
                None, register, graph_bytes, sidecar_bytes)
    except em_wake_registry.RegistryError as exc:
        return _error(ErrorCode.INVALID_MODEL, str(exc), 400)

    users = await asyncio.get_running_loop().run_in_executor(
        None, _wake_model_users, model.graph_sha256)
    return _ok({"model": _wake_model_json(model, registry.active_sha256, users)})


@auth.require_admin
async def _delete_wake_model(request: web.Request) -> web.Response:
    """DELETE /api/wake_models/{sha256}; selected graphs cannot be removed."""
    graph_sha256 = request.match_info["sha256"]
    registry = request.app[SERVICES].registry()
    try:
        registry.get(graph_sha256)
    except em_wake_registry.RegistryError:
        return _error(ErrorCode.NOT_FOUND, "No such wake model", 404)

    async with _wake_registry_lock:
        users = await asyncio.get_running_loop().run_in_executor(
            None, _wake_model_users, graph_sha256)
        if users:
            return _error(ErrorCode.MODEL_IN_USE,
                          f"Wake model is selected by: {', '.join(users)}", 409)
        try:
            await asyncio.get_running_loop().run_in_executor(
                None, registry.delete, graph_sha256)
        except em_wake_registry.RegistryError as exc:
            return _error(ErrorCode.MODEL_IN_USE, str(exc), 409)
    return _ok({"deleted": graph_sha256})


@auth.require_auth
async def _get_speech_assets(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/speech_assets — named, installed, and wake state."""
    device_id = request.match_info["id"]
    await _device_row(device_id)

    services = request.app[SERVICES]
    named: dict[str, str] | None = None
    named_error: str | None = None
    try:
        effective = await asyncio.get_running_loop().run_in_executor(
            None, db.get_effective_device_config, device_id)
        named = services.device_assets().speech_assets(
            services.registry().for_config(effective)).wire()
    except em_wake_registry.RegistryError as exc:
        # No active registered graph: the named set is unavailable (null),
        # never guessed (§8.1).
        named_error = str(exc)

    device = _devices.get(device_id)
    live = device if device is not None and device.online else None
    hello = live.link.hello if live is not None and live.link is not None else None
    wake_stats = device.wake_stats if device is not None else None
    installed = (em_device_assets.installed_speech_assets(hello.assets, named, wake_stats)
                 if hello is not None else None)
    missing = ([digest for digest in named.values() if digest not in set(installed)]
               if named is not None and installed is not None else None)
    return _ok({
        "device_id": device_id,
        "connected": live is not None,
        "named": named,
        "named_error": named_error,
        "installed": installed,
        "missing": missing,
        "wake_stats": wake_stats,
    })


# ─── Alert sound catalog ──────────────────────────────────────────────────────

class _UnresolvedSound(TypedDict):
    scope: str
    key: str
    sound_id: str


def _sound_configs() -> dict[str, dict[str, object]]:
    """Stored (not underlaid) config per scope: "global", then each device."""
    configs: dict[str, dict[str, object]] = {"global": db.get_global_device_config_raw()}
    for row in db.get_all_devices():
        configs[row.device_id] = db.get_device_config(row.device_id)
    return configs


def _unresolved_sounds(configs: Mapping[str, Mapping[str, object]]) -> list[_UnresolvedSound]:
    unresolved: list[_UnresolvedSound] = []
    for scope, config in configs.items():
        for key in em_sounds.SOUND_CONFIG_KEYS:
            sound_id = config.get(key)
            if not sound_id or not isinstance(sound_id, str):
                continue
            _, flagged = em_sounds.alert_asset(sound_id)
            if flagged:
                unresolved.append({"scope": scope, "key": key, "sound_id": sound_id})
    return unresolved


@auth.require_auth
async def _get_sounds(request: web.Request) -> web.Response:
    """GET /api/sounds — catalog entries and unresolved configured IDs."""
    loop = asyncio.get_running_loop()
    sounds, configs = await asyncio.gather(
        loop.run_in_executor(None, em_sounds.scan),
        loop.run_in_executor(None, _sound_configs),
    )
    unresolved = await loop.run_in_executor(None, _unresolved_sounds, configs)
    return _ok({
        "sounds": sounds,
        "dir": str(em_sounds.sounds_dir()),
        "default_id": em_sounds.DEFAULT_ID,
        "formats": list(em_sounds.ALLOWED_SUFFIXES),
        "max_bytes": em_sounds.MAX_UPLOAD_BYTES,
        "unresolved": unresolved,
    })


@auth.require_admin
async def _post_sound_upload(request: web.Request) -> web.Response:
    """POST /api/sounds/upload — store and export one alert asset (§16.5)."""
    try:
        reader = await request.multipart()
    except Exception:
        return _error(ErrorCode.INVALID_UPLOAD, "Expected multipart form data", 400)
    sound_id: str | None = None
    suffix: str | None = None
    data: bytes | None = None
    while True:
        field = await reader.next()
        if field is None:
            break
        if not isinstance(field, aiohttp.BodyPartReader):
            await field.release()      # a nested multipart is nothing we asked for
        elif field.name == "id":
            raw_id = await _read_part(field, 1024)
            if raw_id is None:
                return _error(ErrorCode.INVALID_ID, "Sound id must be letters, digits, _ - . only", 400)
            sound_id = raw_id.decode("utf-8", "replace").strip()
        elif field.name == "sound":
            suffix = em_sounds.safe_upload_suffix(field.filename or "")
            if suffix is None:
                return _error(ErrorCode.INVALID_FILENAME,
                              "Sound must be one of: " + ", ".join(em_sounds.ALLOWED_SUFFIXES), 400)
            data = await _read_part(field, em_sounds.MAX_UPLOAD_BYTES)
            if data is None:
                return _error(ErrorCode.TOO_LARGE, "Sound exceeds the upload limit", 413)
        else:
            await field.release()
    if data is None or suffix is None:
        return _error(ErrorCode.INVALID_UPLOAD, "Expected multipart field 'sound'", 400)
    if not data:
        return _error(ErrorCode.EMPTY_UPLOAD, "Uploaded sound is empty", 400)
    sid = em_sounds.safe_sound_id(sound_id or em_sounds.DEFAULT_ID)
    if sid is None:
        return _error(ErrorCode.INVALID_ID, "Sound id must be letters, digits, _ - . only", 400)

    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, em_sounds.store, sid, suffix, data)
    try:
        exported = await loop.run_in_executor(None, em_sounds.export, sid)
    except em_sounds.ExportError as exc:
        await loop.run_in_executor(None, em_sounds.delete, sid)
        return _error(ErrorCode.DECODE_FAILED, str(exc), 400)
    entry = next((item for item in await loop.run_in_executor(None, em_sounds.scan)
                  if item["id"] == sid), None)
    log.info("[api] Alert sound installed: %s (%d bytes, %.3fs%s)",
             sid, len(data), exported.seconds, ", shortened" if exported.shortened else "")
    return _ok({"sound": entry}, status=201)


@auth.require_admin
async def _delete_sound(request: web.Request) -> web.Response:
    """DELETE /api/sounds/{id}; refuse while any stored scope selects it."""
    sid = em_sounds.safe_sound_id(request.match_info["id"])
    if sid is None:
        return _error(ErrorCode.INVALID_ID, "Bad sound id", 400)
    loop = asyncio.get_running_loop()
    sounds, configs = await asyncio.gather(
        loop.run_in_executor(None, em_sounds.scan),
        loop.run_in_executor(None, _sound_configs),
    )
    if not any(item["id"] == sid for item in sounds):
        return _error(ErrorCode.NOT_FOUND, "No such sound", 404)
    users = await loop.run_in_executor(None, em_sounds.in_use_by, sid, configs)
    if users:
        return _error(ErrorCode.SOUND_IN_USE, f"Sound is selected by: {', '.join(users)}", 409)
    await loop.run_in_executor(None, em_sounds.delete, sid)
    return _ok({"deleted": sid})


@auth.require_auth
async def _post_sound_preview(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/sounds/preview — alert-class local preview."""
    body = await _json_body(request)
    sound_id = _require_str(body, "sound_id")
    if em_sounds.safe_sound_id(sound_id) is None:
        return _error(ErrorCode.INVALID_ID, "Bad sound id", 400)
    device = _require_online(request.match_info["id"])
    result = await request.app[SERVICES].preview_sound(device, sound_id)
    if result.get("error") == "offline":
        return _error(ErrorCode.DEVICE_OFFLINE, "Device disconnected", 409)
    return _ok(result)


@auth.require_auth
async def _post_sound_stop(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/sounds/stop — stop the current preview."""
    device = _require_online(request.match_info["id"])
    await request.app[SERVICES].stop_preview(device)
    return _ok({"stopped": True})


# ─── Alerts panel ─────────────────────────────────────────────────────────────

def _alert_status(engine: em_alerts.AlertEngine, endpoint_id: str) -> dict[str, object] | None:
    """AlertEngine's status, each occurrence marked with where it stands:
    stored in HA always, and pending delivery until the endpoint armed it."""
    status = engine.status(endpoint_id)
    if status is None:
        return None
    return {**status, "occurrences": [
        {**occurrence, "stored_in_ha": True,
         "delivery_pending": not bool(occurrence.get("armed_on_endpoint"))}
        for occurrence in status.get("occurrences") or []
    ]}


@auth.require_auth
async def _get_device_alerts(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/alerts — one endpoint's full alert panel."""
    device_id = request.match_info["id"]
    await _device_row(device_id)
    services = request.app[SERVICES]
    return _ok({"status": _alert_status(services.alerts(), device_id),
                "ha": services.ha_status()})


@auth.require_admin
async def _post_alarm(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/alarms — dashboard entry to AlertEngine."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    alarm_time = _require_str(body, "time")
    days = body.get("days", [])
    if not isinstance(days, list) or not all(isinstance(day, str) for day in days):
        return _error(ErrorCode.BAD_REQUEST, "days must be a list of weekday names", 400)
    name = body.get("name") or ""
    if not isinstance(name, str):
        return _error(ErrorCode.BAD_REQUEST, "name must be a string", 400)
    on_date = None
    raw_date = body.get("date")
    if raw_date not in (None, ""):
        try:
            on_date = date.fromisoformat(raw_date if isinstance(raw_date, str) else "")
        except ValueError:
            return _error(ErrorCode.BAD_REQUEST, "date must be YYYY-MM-DD", 400)
    await _device_row(device_id)
    result = await request.app[SERVICES].alerts().set_alarm(
        device_id, alarm_time, days, name, on_date=on_date,
        source=em_alerts.JournalSource.DASHBOARD)
    return _ok(result)


@auth.require_admin
async def _post_alarm_cancel(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/alarms/cancel — cancel matching schedules."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    name = body.get("name") or ""
    alarm_time = body.get("time") or ""
    if not isinstance(name, str) or not isinstance(alarm_time, str):
        return _error(ErrorCode.BAD_REQUEST, "name and time must be strings", 400)
    await _device_row(device_id)
    result = await request.app[SERVICES].alerts().cancel_alarm(
        device_id, name=name, time=alarm_time, all_alarms=bool(body.get("all")),
        source=em_alerts.JournalSource.DASHBOARD)
    return _ok(result)


# ─── OTA background tasks ─────────────────────────────────────────────────────

def _begin_ota(device_id: str, job: Coroutine[object, object, None]) -> None:
    """
    Claim the device's OTA slot and run `job` in the background.

    The claim happens here, synchronously with the caller's
    `_updates_in_progress` check: claimed inside the task instead, two
    requests that both arrive before it first runs would both pass the check
    and run two transfers into the same slot. The job releases it.
    """
    _updates_in_progress.add(device_id)
    _update_errors.pop(device_id, None)  # fresh attempt clears the last failure
    _spawn(job, f"ota:{device_id}")


class _Slot(enum.StrEnum):
    """The A/B firmware slots /data/local/bin/server links to."""

    A = "server_a"
    B = "server_b"

    @property
    def other(self) -> "_Slot":
        return _Slot.B if self is _Slot.A else _Slot.A


def _parse_active_slot(detect_output: str) -> _Slot | None:
    """The slot a `SLOT:<name>` line names, or None."""
    for line in detect_output.splitlines():
        if "SLOT:" in line:
            words = line.split("SLOT:")[-1].split()
            if words and words[0] in (_Slot.A, _Slot.B):
                return _Slot(words[0])
    return None


async def _update_failed(device_id: str, reason: str) -> None:
    """
    Record and broadcast an OTA failure: device log line, in-memory
    update_error (read back via /api/devices), and a device_update_failed
    event for the dashboard WS. Every abort path in _run_update/_run_rollback
    must come through here — a log-only failure leaves the dashboard tile at
    "updating…" indefinitely.
    """
    _update_errors[device_id] = reason
    await push_log_event(device_id, db.LogLevel.ERROR, db.LogSource.CONTROLLER, reason)
    await _push_event(EventType.DEVICE_UPDATE_FAILED, device_id=device_id, error=reason)


async def _run_update(shell: em_shell.ShellBroker, device_id: str,
                      firmware: em_firmware.BundledFirmware) -> None:
    """
    Background task (started by _begin_ota): A/B slot update to the bundled
    firmware.

    1. Read the bundled binary (checked against its startup hash).
    2. Detect active slot via readlink; migrate legacy layout if needed.
    3. Stream binary to inactive slot.
    4. Flip symlink atomically.
    5. Restart service and confirm it reconnects on the bundled version.
    6. Detect auto-rollback (start_server.sh retry exhausted).
    """
    loop = asyncio.get_running_loop()
    version = firmware.version

    try:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"OTA update starting → {version}")

        binary = await loop.run_in_executor(None, firmware.read)
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Installing bundled firmware ({len(binary):,} bytes)")

        # Record current version as previous before anything changes
        row = await loop.run_in_executor(None, db.get_device, device_id)
        current_ver = row.firmware_ver if row else None
        await loop.run_in_executor(None, db.set_firmware_previous, device_id, current_ver)

        live = _online(device_id)
        if live is None:
            await _update_failed(device_id,
                                 "Device disconnected before update could start")
            return

        # Detect active slot and migrate legacy layout if needed — single shell
        # session to avoid the race condition of two sequential open/close cycles.
        detect_cmd = (
            "CURRENT=$(readlink /data/local/bin/server 2>/dev/null); "
            "if [ \"$CURRENT\" = \"server_a\" ] || [ \"$CURRENT\" = \"server_b\" ]; then "
            "  echo \"SLOT:$CURRENT\"; "
            "else "
            "  cp /data/local/bin/server /data/local/bin/server_a 2>&1 && "
            "  chmod 755 /data/local/bin/server_a && "
            "  ln -sf server_a /data/local/bin/server && "
            "  echo \"SLOT:server_a MIGRATED\" || echo \"MIGRATE_FAILED\"; "
            "fi"
        )
        detect_result = await _shell_run(shell, live, detect_cmd, timeout=60.0)
        log.info(f"[api] Slot detect result for {device_id}: {detect_result!r}")

        if "MIGRATE_FAILED" in detect_result:
            await _update_failed(device_id,
                                 "A/B migration failed — aborting update")
            return

        active_slot = _parse_active_slot(detect_result)

        if active_slot is None:
            await _update_failed(device_id,
                                 f"Could not determine active slot — output: {detect_result!r}")
            return

        if "MIGRATED" in detect_result:
            await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                                 "A/B migration complete — active slot: server_a")

        # Sync the startup script while we're here — OTA is the only update
        # path existing devices have for it (see _sync_start_script).
        await _sync_start_script(shell, live, device_id)
        # Payload drift is not limited to the start script: re-apply the
        # debloat it carries.
        await _sync_debloat(shell, live, device_id)

        inactive_slot = active_slot.other

        # Free space, checked BEFORE writing anything. The transfer needs room
        # for the new binary alongside its .part, and running /data out of
        # space mid-write is a bad way to find out. Read with parse_free_mb,
        # never an awk field index — a long filesystem name can wrap onto
        # its own line, making $4 the percentage.
        need_mb  = (len(binary) * 2) // 1048576 + 8   # binary + .part + slack
        free_out = await _shell_run(shell, live, FREE_SPACE_PROBE)
        free_mb = _parse_free_mb(free_out)
        if free_mb is not None and free_mb < need_mb:
            await _update_failed(
                device_id,
                f"Not enough space on /data: {free_mb}MB free, needs ~{need_mb}MB")
            return
        if free_mb is None:
            # Unknown reads as "carry on" — a df we cannot parse is not
            # evidence of a full disk, and refusing on it would block updates
            # on any device whose df we have not seen.
            log.warning(f"[api] Could not read free space on {device_id} "
                        f"from {free_out!r} — proceeding with update")

        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Deploying to slot {inactive_slot} (active: {active_slot})")

        # Stream binary to inactive slot. Verified by md5 before it is renamed
        # into place, so a corrupt transfer leaves the slot as it was and never
        # reaches the symlink flip below (#76).
        ok = await _stream_binary_to_slot(shell, live, binary, inactive_slot)
        if not ok:
            # Name the stage. "failed or did not verify" covered everything
            # from a shell that never opened to a corrupt payload, and #121
            # was the former reported in the language of the latter.
            await _update_failed(device_id,
                                 f"Binary transfer to {inactive_slot} failed: "
                                 f"{ok} — {inactive_slot} left untouched")
            return

        # Brief pause so device can cleanly close the transfer shell before
        # we open a new one for the symlink flip.
        await asyncio.sleep(1.0)

        # Atomic symlink flip + service restart
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Flipping symlink → {inactive_slot} and restarting")
        await _shell_run(shell, live,
            f"ln -sf {inactive_slot} /data/local/bin/server && "
            f"kill $PPID"
        )
        # Shell dies when the server process is killed — FLIP_OK will never arrive.
        # _monitor_reconnect below detects whether the restart succeeded.

        # Wait for device to come back
        confirmed = await _monitor_reconnect(device_id, version, timeout=90)

        if confirmed:
            _update_errors.pop(device_id, None)
            await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                                 f"✓ Update confirmed: {version}")
            await _push_event(EventType.DEVICE_UPDATED, device_id=device_id, version=version)
        else:
            row     = await loop.run_in_executor(None, db.get_device, device_id)
            running = row.firmware_ver if row else "unknown"

            if running == current_ver:
                # Device came back on old version — auto-rollback by start_server.sh
                await loop.run_in_executor(
                    None, db.set_firmware_previous, device_id, None
                )
                _update_errors[device_id] = (
                    f"auto-rolled back to {running} — new binary failed to start"
                )
                _supervisor_log_wanted.add(device_id)
                await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                    f"Device auto-rolled back to {running} "
                    f"— new binary failed {3} start attempts")
                await _push_event(EventType.DEVICE_AUTO_ROLLED_BACK,
                                  device_id=device_id, version=running)
            else:
                _update_errors[device_id] = (
                    f"timed out — device running {running}"
                )
                _supervisor_log_wanted.add(device_id)
                await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                    f"Update timed out — device running: {running}")
                await _push_event(EventType.DEVICE_UPDATE_FAILED, device_id=device_id,
                                  error=_update_errors[device_id], running=running)

    except Exception as e:
        log.exception(f"[api] OTA update error for {device_id}: {e}")
        await _update_failed(device_id, f"OTA exception: {e}")
    finally:
        _updates_in_progress.discard(device_id)


async def _run_rollback(shell: em_shell.ShellBroker, device_id: str, target_version: str) -> None:
    """
    Background task (started by _begin_ota): flip to inactive A/B slot.

    No binary transfer needed — the old binary is already in the inactive slot.
    """
    loop = asyncio.get_running_loop()
    try:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Rolling back to {target_version}")

        live = _online(device_id)
        if live is None:
            await _update_failed(device_id,
                                 "Device disconnected before rollback")
            return

        detect_result = await _shell_run(shell, live,
            "CURRENT=$(readlink /data/local/bin/server 2>/dev/null); "
            "if [ \"$CURRENT\" = \"server_a\" ] || [ \"$CURRENT\" = \"server_b\" ]; then "
            "  echo \"SLOT:$CURRENT\"; "
            "else echo \"SLOT_UNKNOWN\"; fi"
        )
        active_slot = _parse_active_slot(detect_result)

        if active_slot is None:
            await _update_failed(device_id,
                                 "Cannot determine active slot — is A/B set up?")
            return

        inactive_slot = active_slot.other
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Flipping {active_slot} → {inactive_slot}")

        # The version running now, read before the flip restarts the server.
        row_pre = await loop.run_in_executor(None, db.get_device, device_id)
        current_fw = row_pre.firmware_ver if row_pre else None

        await _shell_run(shell, live,
            f"ln -sf {inactive_slot} /data/local/bin/server && "
            f"kill $PPID"
        )
        # Shell dies when the server process is killed — ROLLBACK_OK will never arrive.

        confirmed = await _monitor_reconnect(
            device_id, target_version,
            previous_version=current_fw,
            timeout=90,
        )

        if confirmed:
            _update_errors.pop(device_id, None)
            await loop.run_in_executor(
                None, db.set_firmware_previous, device_id, None
            )
            await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                                 f"✓ Rollback confirmed: {target_version}")
            await _push_event(EventType.DEVICE_ROLLED_BACK,
                              device_id=device_id, version=target_version)
        else:
            await _update_failed(device_id,
                                 "Rollback did not reconnect within 90s")

    except Exception as e:
        log.exception(f"[api] Rollback error for {device_id}: {e}")
        await _update_failed(device_id, f"Rollback exception: {e}")
    finally:
        _updates_in_progress.discard(device_id)


async def _monitor_reconnect(
    device_id: str,
    expected_version: str,
    previous_version: str | None = None,
    timeout: int = 90,
) -> bool:
    """
    Poll until the device reconnects on the expected version, or timeout elapses.

    Accepts success if the device reports expected_version exactly. A
    rollback also passes previous_version, since its target is only the
    label recorded before the last update: any version other than the one
    it flipped away from counts. An update passes none — the bundled
    version is the exact string compiled into the binary it installed.
    """
    loop     = asyncio.get_running_loop()
    deadline = time.monotonic() + timeout
    await asyncio.sleep(8)  # give device time to stop and restart

    while time.monotonic() < deadline:
        if _online(device_id) is not None:
            row = await loop.run_in_executor(None, db.get_device, device_id)
            if row:
                running = row.firmware_ver
                if running == expected_version:
                    return True
                if previous_version is not None and running != previous_version:
                    return True
        await asyncio.sleep(2)

    return False


# ─── Shell helpers ────────────────────────────────────────────────────────────

@contextlib.asynccontextmanager
async def _device_shell(shell: em_shell.ShellBroker,
                        live: em_device.Device) -> AsyncIterator[em_shell.ShellConnection]:
    """
    A programmatic shell session on the device, strictly one at a time.

    handle_shell answers the request with the socket, then waits for it to
    close before returning — so the connection stays alive until this exits.
    On the way out it releases exactly what it took: a caller that timed out
    waiting for the lock never reaches the session, so it can never close
    another caller's socket (an OTA transfer mid-flight) or free the lock
    that caller holds.
    """
    device_id = live.device_id
    lock = shell.lock(device_id)
    try:
        await asyncio.wait_for(lock.acquire(), timeout=20.0)
    except asyncio.TimeoutError:
        raise RuntimeError(f"Shell lock acquisition timed out for {device_id}") from None

    # No dashboard socket — signals programmatic mode.
    req = shell.request(device_id)
    ws: em_shell.ShellConnection | None = None
    try:
        await live.send(MessageType.SHELL_OPEN, {})
        ws = await req.wait(timeout=15.0)
        if ws is None:
            raise RuntimeError(f"Shell session for {device_id} ended before it opened")
        yield ws
    finally:
        # Closing ws wakes handle_shell's ws.wait_closed(), which then returns
        # and lets the device clean up its side too.
        if ws is not None:
            with contextlib.suppress(Exception):
                await ws.close()
        shell.release(device_id, req)
        try:
            await live.send(MessageType.SHELL_CLOSE, {})
        except em_device_link.LinkClosed:
            pass    # the session is gone (an OTA flip restarts the server): no shell left to close
        finally:
            lock.release()


async def _shell_run(shell: em_shell.ShellBroker, live: em_device.Device, cmd: str,
                     timeout: float = 30.0) -> str:
    """
    Run a shell command on the device and return its stdout as a string.

    Appends a sentinel marker to detect when output is complete.
    """
    SENTINEL = "__CMD_DONE_9f3a__"
    output: list[str] = []
    try:
        async with _device_shell(shell, live) as ws:
            await ws.send(f"{cmd} ; echo '{SENTINEL}'\n")
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    msg  = await asyncio.wait_for(ws.recv(), timeout=5.0)
                    text = msg.decode("utf-8", errors="replace") if isinstance(msg, bytes) else msg
                    if SENTINEL in text:
                        output.append(text[:text.index(SENTINEL)])
                        break
                    output.append(text)
                except asyncio.TimeoutError:
                    break
        return "".join(output).strip()
    except Exception as e:
        log.error(f"[api] shell_run failed ({cmd!r}): {e}")
        return ""


async def _stream_binary_to_slot(shell: em_shell.ShellBroker, live: em_device.Device,
                                 binary: bytes, slot: _Slot) -> "TransferResult":
    """
    Transfer a firmware binary to /data/local/bin/{slot}.

    require_verify=True: firmware is the one payload where an unverifiable
    transfer must fail rather than proceed. A corrupt binary and a genuinely
    broken one produce the same observable — three fast exits and a rollback —
    so shipping one to the slot we are about to boot costs a reboot and a
    rollback to learn nothing (#76).
    """
    return await _stream_file_to_device(shell, live, binary, f"/data/local/bin/{slot}",
                                        require_verify=True)


class TransferStage(enum.StrEnum):
    """Where a device file transfer ended."""

    OK = "ok"                    # verified by md5
    UNVERIFIED = "unverified"    # landed, but the device has no md5 tool
    SHELL = "shell"
    DECODER = "decoder"
    MD5TOOL = "md5tool"
    SEND = "send"
    VERIFY = "verify"
    CORRUPT = "corrupt"
    ERROR = "error"


@dataclass(frozen=True, slots=True)
class TransferResult:
    """
    The outcome of a device file transfer, carrying the STAGE it stopped at.

    Truthy on success, so every existing `if not await _stream_file_to_device(
    ...)` call site keeps working unchanged.

    The stage exists because one message covered five different outcomes, and
    the two that matter most are at opposite ends: "the bytes arrived corrupt"
    and "we never opened a shell, so no byte was ever sent". #121 reported the
    second and read as the first — three Dots failing with `failed or did not
    verify`, fifteen seconds after starting, which is far too fast to have
    attempted a 10MB payload. Nothing short of the controller's own stdout
    could tell them apart, and a user cannot be asked for that mid-update.
    """

    ok: bool
    stage: TransferStage = TransferStage.OK
    detail: str = ""

    def __bool__(self) -> bool:
        return self.ok

    def __str__(self) -> str:
        return self.detail or self.stage


# Stage detail text. Phrased for someone reading a device log who is deciding
# what to try next, so each one says whether any data left the controller —
# that is the difference between "retry, the link was bad" and "the payload or
# the device is wrong".
_TRANSFER_STAGES = {
    TransferStage.SHELL:   "could not open a shell session on the device — no data was sent",
    TransferStage.DECODER: "no base64 decoder found on the device — no data was sent",
    TransferStage.MD5TOOL: "device has no md5 tool, refusing to send unverified",
    TransferStage.SEND:    "timed out part-way through sending",
    TransferStage.VERIFY:  "sent, but timed out waiting for md5 verification",
    TransferStage.CORRUPT: "arrived corrupt — md5 did not match what was sent",
    TransferStage.ERROR:   "transfer error",
}


def _transfer_failed(stage: TransferStage, extra: str = "") -> TransferResult:
    detail = _TRANSFER_STAGES.get(stage, stage)
    if extra:
        detail = f"{detail} ({extra})"
    return TransferResult(False, stage, detail)


# One round trip names whether the device has toybox's base64 decoder and
# md5sum. The decoder must decode the test string back to `test`, not merely
# exit 0, so an unrelated `base64` on PATH cannot pass.
TOOL_PROBE_DONE = "__DETECT_DONE__"
TOOL_PROBE = (
    "if [ \"$(echo dGVzdA== | base64 -d 2>/dev/null)\" = test ]; then echo DECODER:base64; "
    "else echo DECODER:none; fi; "
    "if echo x | md5sum >/dev/null 2>&1; then echo MD5:md5sum; "
    f"else echo MD5:none; fi; echo {TOOL_PROBE_DONE}"
)


def _probe_decoder(detect_buf: str) -> str | None:
    """The decode command TOOL_PROBE's output names; None when it names none."""
    return "base64 -d" if "DECODER:base64" in detect_buf else None


def _probe_md5(detect_buf: str) -> str | None:
    """The md5 command TOOL_PROBE's output names; None when it names none."""
    return "md5sum" if "MD5:md5sum" in detect_buf else None


def _md5_of(path: str) -> str:
    """Shell printing `<md5>  <path>` through toybox's md5sum."""
    return f"md5sum {path} 2>/dev/null"


async def _stream_file_to_device(shell: em_shell.ShellBroker, live: em_device.Device,
                                 data: bytes, dest: str,
                                 mode: str = "755",
                                 require_verify: bool = False) -> TransferResult:
    """
    Transfer a file to `dest` on the device via shell heredoc (default mode 755).

    Detects the base64 decoder and md5 tool before transferring (TOOL_PROBE).
    Uses a heredoc so no intermediate .b64 file is needed.
    The heredoc delimiter contains '_' which is not in the base64 alphabet.

    **md5 decides success, not the shell's exit status.** Bytes land in
    `{dest}.part` and are renamed only once their md5 matches what we sent, so
    a bad transfer leaves whatever was at `dest` untouched. `TRANSFER_OK` alone
    only ever proved that the decode pipeline and chmod exited 0 — not that the
    bytes arrived intact (#76). The verification rides the SAME shell session as
    the transfer, so it costs a round trip on an open socket, not a new session.

    `require_verify` decides what happens when the device has no md5 tool at
    all. Callers default to False, which warns and accepts. Firmware passes
    True.
    """
    device_id     = live.device_id
    DELIM         = "__END_B64_42__"

    session = contextlib.AsyncExitStack()
    try:
        try:
            ws = await session.enter_async_context(_device_shell(shell, live))
        except Exception as e:
            log.error(f"[api] Could not open a shell to {device_id} for "
                      f"{dest}: {e}")
            return _transfer_failed(TransferStage.SHELL, str(e))

        # `dest` is NOT deleted here, and must not be. This used to open with
        # `rm -f {dest}` to clear a previous attempt, which for firmware means
        # deleting /data/local/bin/server_<inactive> — THE ROLLBACK SLOT —
        # before a single byte of the replacement had been sent. Every failed
        # OTA therefore left the device with a good active slot and an empty
        # partner, so a later crash-loop would flip the symlink onto nothing.
        # It also contradicted the message the user was shown, which promised
        # the slot was left untouched (#121). Nothing needs the delete: the
        # heredoc writes with `>`, which truncates, and a verified transfer
        # arrives by `mv` over whatever was there.

        # ── Detect available base64 decoder and md5 tool (TOOL_PROBE) ────────
        await ws.send(TOOL_PROBE + "\n")

        detect_buf = ""
        detect_dl  = time.monotonic() + 15
        while time.monotonic() < detect_dl:
            try:
                msg  = await asyncio.wait_for(ws.recv(), timeout=2)
                text = msg.decode("utf-8", errors="replace") if isinstance(msg, bytes) else msg
                detect_buf += text
                if TOOL_PROBE_DONE in detect_buf:
                    break
            except asyncio.TimeoutError:
                continue

        decode_cmd = _probe_decoder(detect_buf)
        if decode_cmd is None:
            # Two very different things reach here. TOOL_PROBE_DONE present means
            # the device answered and genuinely has no decoder — a property of
            # that device, which retrying will not change. Absent means the
            # round trip produced nothing in 15s, i.e. the shell plane is not
            # carrying output, which is a link problem and IS worth retrying.
            # Reporting both as "no base64 decoder" sent #121 looking at the
            # wrong half.
            if TOOL_PROBE_DONE not in detect_buf:
                log.error(f"[api] Shell produced no output in 15s while probing "
                          f"{device_id} for a decoder — link problem, not a "
                          f"missing tool. Output so far: {detect_buf!r}")
                return _transfer_failed(TransferStage.SHELL, "device shell produced no output within 15s")
            log.error(f"[api] No base64 decoder found on device. "
                      f"Detection output: {detect_buf!r}")
            return _transfer_failed(TransferStage.DECODER)

        log.info(f"[api] Decoder: {decode_cmd}")

        md5_cmd = _probe_md5(detect_buf)
        if md5_cmd is None:
            if require_verify:
                log.error(f"[api] No md5 tool on device — refusing to transfer "
                          f"{dest} unverified. Detection output: {detect_buf!r}")
                return _transfer_failed(TransferStage.MD5TOOL)
            log.warning(f"[api] No md5 tool on device — {dest} will be "
                        f"transferred WITHOUT verification")

        # ── Heredoc transfer ─────────────────────────────────────────────────
        lines = base64.encodebytes(data).decode("ascii").splitlines(keepends=True)
        log.info(f"[api] Transferring {len(data):,} bytes to {dest} "
                 f"({len(lines)} base64 lines via heredoc)")

        # Bytes land in .part; only a matching md5 promotes them to dest. With
        # no md5 tool there is nothing to promote against, so write straight to
        # dest and keep the old behaviour (require_verify already refused above
        # for the payloads where that is not acceptable).
        landing = f"{dest}.part" if md5_cmd else dest

        # Single shell command: decode heredoc → landing, set permissions,
        # confirm. chmod here rather than after the rename so the mode travels
        # with the file and dest is never briefly present with the wrong one.
        await ws.send(
            f"{decode_cmd} << '{DELIM}' > {landing} && "
            f"chmod {mode} {landing} && "
            f"echo TRANSFER_OK\n"
        )

        # Stream base64 data — each line already ends with \n from encodebytes
        for line in lines:
            await ws.send(line)

        # Close heredoc; shell now executes the decode pipeline
        await ws.send(f"{DELIM}\n")
        log.info("[api] Heredoc sent — waiting for TRANSFER_OK")

        # Wait for confirmation (decode of ~13 MB on ARM takes a few seconds)
        deadline    = time.monotonic() + 120
        transferred = False
        while time.monotonic() < deadline:
            try:
                msg  = await asyncio.wait_for(ws.recv(), timeout=5)
                text = msg.decode("utf-8", errors="replace") if isinstance(msg, bytes) else msg
                if "TRANSFER_OK" in text:
                    transferred = True
                    break
                if text.strip():
                    log.debug(f"[api] Shell output during transfer: {text!r}")
            except asyncio.TimeoutError:
                continue

        if not transferred:
            log.error(f"[api] Transfer to {dest} timed out waiting for TRANSFER_OK")
            return _transfer_failed(TransferStage.SEND)

        if md5_cmd is None:
            log.info(f"[api] Transfer to {dest} confirmed (unverified)")
            return TransferResult(True, TransferStage.UNVERIFIED)

        # ── Verify, then promote ─────────────────────────────────────────────
        # Same shell session, so this is a round trip on an open socket rather
        # than a new session. A mismatch removes the .part and leaves dest as
        # it was — for firmware that means the rollback slot keeps its previous
        # binary instead of being replaced by a broken one.
        # No `cut`: md5sum prints "<hash>  <path>", and a case glob needs no
        # external tool at all.
        want = hashlib.md5(data).hexdigest()
        await ws.send(
            f'GOT=$({md5_cmd} {landing} 2>/dev/null); '
            f'case "$GOT" in {want}*) mv {landing} {dest} && echo VERIFY_OK ;; '
            f'*) rm -f {landing}; echo "VERIFY_BAD:$GOT" ;; esac\n'
        )

        verify_buf = ""
        verify_dl  = time.monotonic() + 120
        while time.monotonic() < verify_dl:
            try:
                msg  = await asyncio.wait_for(ws.recv(), timeout=5)
                text = msg.decode("utf-8", errors="replace") if isinstance(msg, bytes) else msg
                verify_buf += text
                if "VERIFY_OK" in verify_buf:
                    log.info(f"[api] Transfer to {dest} confirmed "
                             f"({len(data):,} bytes, md5 {want})")
                    return TransferResult(True)
                if "VERIFY_BAD" in verify_buf:
                    got = verify_buf.split("VERIFY_BAD:")[-1].strip().split()
                    log.error(f"[api] Transfer to {dest} CORRUPT — md5 {want} "
                              f"expected, device reported {got[0] if got else '(none)'}. "
                              f"{dest} left untouched.")
                    return _transfer_failed(TransferStage.CORRUPT,
                                            f"device reported {got[0] if got else '(none)'}")
            except asyncio.TimeoutError:
                continue

        log.error(f"[api] Transfer to {dest} timed out waiting for md5 verification "
                  f"— {dest} left untouched")
        return _transfer_failed(TransferStage.VERIFY)

    except Exception as e:
        log.error(f"[api] File transfer to {dest} failed: {e}")
        return _transfer_failed(TransferStage.ERROR, str(e))
    finally:
        await session.aclose()


# Where the provisioning wizard installs the supervisor script; the echomuse
# init service runs it.
START_SCRIPT_PATH = "/data/local/bin/start_server.sh"


async def _sync_start_script(shell: em_shell.ShellBroker, live: em_device.Device,
                             device_id: str) -> bool:
    """
    OTA-time payload sync: heal /data/local/bin/start_server.sh drift.

    The startup script is installed at provisioning and — unlike the server
    binary — had no other update path, so fleet drift accumulates (found
    2026-07-11: Lounge was a script revision behind Office). Every OTA now
    compares the device's script against the canonical payload
    (controller/device_payloads/) and pushes it when they differ.

    Replacement is rename-based on purpose: the running script's shell keeps
    reading the OLD inode, so the update only takes effect at the next
    device reboot — safe to do while the script sits in its `wait` loop.
    Best-effort: a sync failure logs but never blocks the firmware update.

    True when the device's script is the canonical one on return — what
    Fire OS 6's debloat needs before it runs the script's `debloat` mode.
    """
    path = START_SCRIPT_PATH
    try:
        script = (PAYLOADS_DIR / "start_server.sh").read_bytes()
    except OSError as e:
        log.error(f"[api] start_server.sh payload unreadable — skipping sync: {e}")
        return False
    want = hashlib.md5(script).hexdigest()

    out = await _shell_run(shell, live, _md5_of(path))
    if want in out:
        return True  # in sync — the common case
    await asyncio.sleep(1.0)  # let the md5 shell session close cleanly

    await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                         "start_server.sh out of date — syncing canonical version")
    tmp = path + ".new"
    pushed = await _stream_file_to_device(shell, live, script, tmp)
    if not pushed:
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                             f"start_server.sh sync failed: {pushed}")
        return False
    await asyncio.sleep(1.0)

    # ${NEW%% *} keeps the hash: md5sum prints "<hash>  <path>". The
    # expansion needs no tool.
    res = await _shell_run(shell, live,
        f'NEW=$({_md5_of(tmp)}); NEW=${{NEW%% *}}; '
        f'if [ "$NEW" = "{want}" ]; then '
        f'mv {tmp} {path} && chmod 755 {path} && echo SCRIPT_SYNCED; '
        f'else rm -f {tmp}; echo SCRIPT_MD5_MISMATCH:$NEW; fi')
    synced = "SCRIPT_SYNCED" in res
    if synced:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             "start_server.sh synced — takes effect on next device reboot")
    else:
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                             f"start_server.sh sync failed ({res.strip() or 'no output'})")
    await asyncio.sleep(1.0)
    return synced


# `start_server.sh debloat`'s last line: how many listed services init no
# longer runs, and which (listed services, or dnsmasq) are still up.
_DENYLIST_RESULT = re.compile(r"DEBLOAT_STOPPED:(\d+) STILL_RUNNING:(\S*)")


@dataclass(frozen=True, slots=True)
class DenylistResult:
    """What `start_server.sh debloat` reported."""

    stopped: int
    still_running: tuple[str, ...]


def _parse_denylist_result(out: str) -> DenylistResult | None:
    """The result line in `start_server.sh debloat`'s output; None without one."""
    m = _DENYLIST_RESULT.search(out)
    if m is None:
        return None
    return DenylistResult(int(m.group(1)), tuple(n for n in m.group(2).split(",") if n))


async def _sync_debloat(shell: em_shell.ShellBroker, live: em_device.Device,
                        device_id: str) -> None:
    """
    Re-apply the debloat — from every firmware update and from
    POST /api/devices/{id}/debloat: the service denylist start_server.sh
    stops at every boot (FOS6_DENYLIST, the list's only copy), run on its own
    by the script's `debloat` mode. No reboot needed — the services stop now,
    and every boot stops them again.

    The script is synced first, and the mode runs only when the device's copy
    is then the canonical one: a script predating the mode ignores its
    argument and would run in full, starting a second supervisor beside the
    live one.
    """
    if not await _sync_start_script(shell, live, device_id):
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                             "debloat not applied: start_server.sh could not be brought "
                             "up to date, and an older copy would start a second server")
        return
    out = await _shell_run(shell, live, f"sh {START_SCRIPT_PATH} debloat 2>&1", timeout=60.0)
    result = _parse_denylist_result(out)
    if result is None:
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                             f"debloat: start_server.sh reported no result "
                             f"({out.strip() or 'no output'})")
    elif result.still_running:
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
                             f"debloat: {result.stopped} Amazon service(s) stopped; still "
                             f"running: {', '.join(result.still_running)}")
    else:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"debloat: Amazon's services stopped ({result.stopped} listed, "
                             f"none running)")
    await asyncio.sleep(1.0)


# ─── Shell WebSocket proxy (interactive dashboard terminal) ───────────────────

async def _ws_shell(request: web.Request) -> web.WebSocketResponse:
    """
    WS /api/devices/{id}/shell — interactive shell terminal for dashboard.

    Auth is handled via ws_resolve_session (checks cookie then ?token= query
    param) because browser WebSocket clients cannot set custom headers.
    Do NOT add @auth.require_admin here — _extract_token doesn't read query
    params and would reject every connection before this function runs.

    Registers a shell request carrying the dashboard socket, so handle_shell
    proxies in interactive mode.
    """
    device_id = request.match_info["id"]

    user = await auth.ws_resolve_session(request)
    if user is None:
        raise web.HTTPUnauthorized()
    if user["role"] != auth.Role.ADMIN:
        raise web.HTTPForbidden()

    live = _online(device_id)
    if live is None:
        raise web.HTTPConflict(reason="Device is not connected")

    # Refuse if a programmatic shell session (e.g. OTA transfer) is in progress.
    # Opening a terminal mid-transfer sends shell_open to the device, which cancels
    # the current shell context and kills the transfer.
    shell = request.app[SERVICES].shell
    if shell.busy(device_id):
        raise web.HTTPConflict(reason="Device shell is busy — an OTA update is in progress")

    ws = web.WebSocketResponse()
    await ws.prepare(request)

    log.info(f"[api] Shell session requested: {device_id} by {user['username']}")
    await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                         f"Shell session opened by {user['username']}")

    req = shell.request(device_id, dashboard=ws)
    # Do NOT take the shell lock — interactive sessions bypass the
    # programmatic shell mechanism entirely.

    try:
        # pty:true — interactive terminal wants a real PTY (mksh prompt,
        # line editing, top/vi, resize). The firmware falls back to a pipe
        # when PTY allocation fails; handle_shell reports the established
        # mode to the dashboard via shell_meta. Programmatic sessions
        # (_device_shell) deliberately do not set it.
        await live.send(MessageType.SHELL_OPEN, {"pty": True})
        await req.wait()
    except Exception as e:
        log.warning(f"[api] Shell session error ({device_id}): {e}")
    finally:
        shell.release(device_id, req)
        with contextlib.suppress(em_device_link.LinkClosed):
            await live.send(MessageType.SHELL_CLOSE, {})
        log.info(f"[api] Shell session closed: {device_id}")
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             f"Shell session closed by {user['username']}")

    return ws


# ─── Bundled firmware ─────────────────────────────────────────────────────────

@auth.require_auth
async def _get_firmware(request: web.Request) -> web.Response:
    """GET /api/firmware — the firmware build this controller installs."""
    firmware = request.app[SERVICES].firmware()
    return _ok({"version": firmware.version, "size": firmware.size,
                "sha256": firmware.sha256})


class DeploySkipReason(enum.StrEnum):
    """Why a fleet deploy passed a device over."""

    OFFLINE = "offline"
    NOT_APPROVED = "not_approved"
    ALREADY_CURRENT = "already_current"
    UPDATE_IN_PROGRESS = "update_in_progress"


@auth.require_admin
async def _post_deploy_firmware(request: web.Request) -> web.Response:
    """
    POST /api/firmware/deploy

    Install the bundled firmware on every connected, approved device not
    already on it.

    Walks every registered device, not only the ones connected since this
    controller started: an approved device that is offline and behind is
    reported `offline`, so the dashboard can offer to queue its install for
    its next connect (POST /api/devices/{id}/update/queue).
    """
    services = request.app[SERVICES]
    firmware = services.firmware()
    started: list[str] = []
    skipped: list[dict[str, str]] = []
    loop = asyncio.get_running_loop()

    for row in await loop.run_in_executor(None, db.get_all_devices):
        device_id = row.device_id
        live = _online(device_id)
        if not row.approved:
            skipped.append({"device_id": device_id, "reason": DeploySkipReason.NOT_APPROVED})
            continue
        if not _firmware_behind(row, firmware):
            skipped.append({"device_id": device_id, "reason": DeploySkipReason.ALREADY_CURRENT})
            continue
        if live is None:
            skipped.append({"device_id": device_id, "reason": DeploySkipReason.OFFLINE})
            continue
        if device_id in _updates_in_progress:
            skipped.append({"device_id": device_id,
                            "reason": DeploySkipReason.UPDATE_IN_PROGRESS})
            continue

        await _start_install(services.shell, device_id, firmware)
        started.append(device_id)

    return _ok({
        "version": firmware.version,
        "started": started,
        "skipped": skipped,
    }, status=202)


# ─── Provisioning ─────────────────────────────────────────────────────────────

# Device payloads — files the controller distributes to devices (provisioning
# wizard today; script/component OTA tomorrow). One canonical copy on disk,
# read per-request so edits ship without a restart. device/scripts/
# start_server.sh is a symlink into this directory.
PAYLOADS_DIR = Path(__file__).parent / "device_payloads"


def _read_payload(name: str) -> str:
    path = PAYLOADS_DIR / name
    if not path.is_file():
        raise web.HTTPInternalServerError(
            text=f"Payload {name} missing from {PAYLOADS_DIR} — broken install/image"
        )
    return path.read_text()


@auth.require_admin
async def _get_provision_start_script(request: web.Request) -> web.Response:
    """GET /api/provision/start_script — serves the EchoMuse startup script."""
    return web.Response(
        text=_read_payload("start_server.sh"),
        content_type='text/plain',
        headers={'Content-Disposition': 'attachment; filename="start_server.sh"'},
    )


@auth.require_admin
async def _get_provision_firmware(request: web.Request) -> web.Response:
    """
    GET /api/provision/firmware — the bytes of the bundled firmware, for the
    provisioning wizard's install step.

    A freshly-flashed device isn't registered in _devices yet and can't go
    through the /api/devices/{id}/update OTA path (that requires a live
    session), so the wizard takes the binary from here instead. Either way it
    is the same build: the one bundled with this controller.
    """
    firmware = request.app[SERVICES].firmware()
    binary = await asyncio.get_running_loop().run_in_executor(None, firmware.read)
    return web.Response(
        body=binary,
        content_type='application/octet-stream',
        headers={
            'Content-Disposition': 'attachment; filename="server"',
            'X-Firmware-Version': firmware.version,
            'X-Firmware-Sha256': firmware.sha256,
        },
    )


# ─── Device-link TLS credentials ──────────────────────────────────────────────

# Canonical on-device credential paths — coupled with
# device/internal/client/tlscreds.go. The Go client re-reads them on every
# dial attempt, so pushed credentials take effect on the next reconnect
# without a firmware restart.
DEVICE_TLS_DIR = "/data/local/etc/echomuse"

# Per-device log lines in a support bundle, after thinning. Deep enough that
# a startup line survives a day of chatter, small enough that six devices do
# not bury the controller's own log.
DEVICE_LOG_LINES = 120


@auth.require_admin
async def _post_provision_tls_credentials(request: web.Request) -> web.Response:
    """
    POST /api/provision/tls_credentials  {device_id}

    Provisioning-wizard path: returns the CA cert plus the device's link
    token (minting one — and a pending device row — if needed) so the
    wizard can install them over adb before the device's first contact.
    """
    tls_dir = _tls_dir
    if tls_dir is None:
        return _error(ErrorCode.TLS_UNAVAILABLE,
                      "Device-link TLS is not active on this controller", 503)
    body      = await _json_body(request)
    device_id = _require_str(body, "device_id")

    loop  = asyncio.get_running_loop()
    token = await loop.run_in_executor(None, db.ensure_device_token, device_id)
    ca    = await loop.run_in_executor(None, em_pki.ca_pem, tls_dir)
    return _ok({
        "ca_pem": ca,
        "token":  token,
        "dir":    DEVICE_TLS_DIR,
    })


@auth.require_admin
async def _post_secure_link(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/secure_link

    Fleet path for already-provisioned devices: pushes ca.pem + token to
    the device over the (still-plain) shell plane, then bounces the
    control connection so the device redials — over wss, now that the CA
    file exists. Requires the device to be connected.
    """
    tls_dir = _tls_dir
    if tls_dir is None:
        return _error(ErrorCode.TLS_UNAVAILABLE,
                      "Device-link TLS is not active on this controller", 503)
    device_id = request.match_info["id"]
    live = _require_online(device_id, f"Device not connected: {device_id}")

    _spawn(_run_secure_link(request.app[SERVICES].shell, live, tls_dir), f"secure-link:{device_id}")
    return _ok({"started": True})


@auth.require_admin
async def _post_debloat(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/debloat

    Re-apply the debloat to a live device (_sync_debloat): sync
    start_server.sh and stop its service denylist now.

    This exists because the OTA-time sync cannot reach every device. A device
    already running the latest firmware will not be updated again, so it would
    never receive a payload change. Idempotent, so pressing it twice costs a
    pass of `stop` calls on already-stopped services and nothing else.
    """
    device_id = request.match_info["id"]
    live = _require_online(device_id, f"Device not connected: {device_id}")

    # No explicit shell release here: _shell_run and _stream_file_to_device
    # each hold a _device_shell session only for their own duration.
    _spawn(_sync_debloat(request.app[SERVICES].shell, live, device_id), f"debloat:{device_id}")
    return _ok({"started": True})


async def _run_secure_link(shell: em_shell.ShellBroker, live: em_device.Device, tls_dir: str) -> None:
    """Background task: install TLS credentials on a live device."""
    loop = asyncio.get_running_loop()
    device_id = live.device_id
    try:
        await push_log_event(device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
                             "Secure link: pushing TLS credentials")
        token = await loop.run_in_executor(None, db.ensure_device_token, device_id)
        ca    = await loop.run_in_executor(None, em_pki.ca_pem, tls_dir)

        await _shell_run(shell, live, f"mkdir -p {DEVICE_TLS_DIR}")
        await asyncio.sleep(1.0)  # let the shell session close cleanly

        ok = await _stream_file_to_device(
            shell, live, ca.encode("ascii"), f"{DEVICE_TLS_DIR}/ca.pem", mode="644")
        if ok:
            await asyncio.sleep(1.0)
            ok = await _stream_file_to_device(
                shell, live, token.encode("ascii"), f"{DEVICE_TLS_DIR}/token", mode="600")
        if not ok:
            await push_log_event(device_id, db.LogLevel.ERROR, db.LogSource.CONTROLLER,
                                 f"Secure link: credential transfer failed: {ok}")
            return

        await push_log_event(
            device_id, db.LogLevel.INFO, db.LogSource.CONTROLLER,
            "Secure link: credentials installed — bouncing connection to switch to wss")
        # The device reloads credentials on every dial, so a reconnect is
        # enough to move to the TLS listener.
        await live.disconnect()
    except Exception as e:
        log.exception(f"[api] Secure link failed for {device_id}: {e}")
        await push_log_event(device_id, db.LogLevel.ERROR, db.LogSource.CONTROLLER,
                             f"Secure link failed: {e}")


# ─── System ───────────────────────────────────────────────────────────────────

@auth.require_auth
async def _get_system_status(request: web.Request) -> web.Response:
    """GET /api/system/status"""
    loop = asyncio.get_running_loop()
    all_rows = await loop.run_in_executor(None, db.get_all_devices)
    approval_mode = await loop.run_in_executor(
        None, db.get_config, db.SystemConfigKey.DEVICE_APPROVAL, "strict")

    services = request.app[SERVICES]
    firmware = services.firmware()

    controller = _controller_cache.value
    return _ok({
        "controller_version": CONTROLLER_VERSION,
        # True when running as a Home Assistant add-on behind Supervisor's
        # ingress proxy. Presentation only — the dashboard is the same
        # dashboard either way, with the same features, and nothing should
        # be gated on this. It exists so the SPA can stop drawing chrome
        # Home Assistant already draws (its own panel header and title) and
        # can avoid offering a theme toggle that fights HA's theme.
        "ha_ingress": INGRESS_ONLY,
        # Peak asyncio event-loop stall since start (ms). Non-trivial values
        # mean the controller itself delayed speaker frames and LED updates.
        "loop_lag_peak_ms": round(services.loop_lag_peak_ms(), 1),
        "connected":      sum(1 for d in list(_devices.values()) if d.online),
        "total_devices":  len(all_rows),
        "pending":        sum(1 for r in all_rows if not r.approved),
        "approval_mode":  approval_mode,
        "firmware_version": firmware.version,
        # Controller update, surfaced alongside the firmware one so the header
        # can badge it without a second round trip. Read-only by design: the
        # controller runs as a container the user owns, and updating it is a
        # `docker compose pull` they perform — there is deliberately no action
        # here, only the information needed to decide to take it.
        "controller_update": (controller["version"]
                              if controller is not None and controller["available"] else None),
        "updates_available": sum(
            1 for r in all_rows
            if r.firmware_ver and r.firmware_ver != firmware.version
        ),
        "speech": services.speech_worker_status(),
    })


def _ha_calendar_url() -> str | None:
    """HA's calendar panel: HA_URL when configured; under add-on ingress the
    dashboard shares HA's origin, so the panel path is absolute."""
    base = os.environ.get("HA_URL", "").strip().rstrip("/")
    if base:
        return f"{base}/calendar"
    return "/calendar" if INGRESS_ONLY else None


@auth.require_auth
async def _get_ha_status(request: web.Request) -> web.Response:
    """GET /api/ha/status — per-feature HA probe results (SPEC §16.7: a failure
    disables only the dependent feature) and where HA's calendar UI lives."""
    return _ok({"features": request.app[SERVICES].ha_status(),
                "calendar_url": _ha_calendar_url()})


@auth.require_admin
async def _get_wyoming_info(request: web.Request) -> web.Response:
    """GET /api/speech/wyoming?host=&port= — the ASR programs and models a Wyoming
    server offers, for the `pauseAsr` setting (§16.6). Admin only: it connects to
    the address it is given."""
    try:
        server = em_pause_asr.parse_pause_asr({
            "engine": em_pause_asr.PauseAsrEngine.WYOMING, "host": request.query.get("host", ""),
            "port": int(request.query.get("port", em_pause_asr.DEFAULT_WYOMING_PORT)),
            "model": "", "language": ""})
    except ValueError as err:
        return _error(ErrorCode.INVALID_PARAM, str(err), 400)
    if server is None:      # unreachable: the engine above is a server
        return _error(ErrorCode.INVALID_PARAM, "host is required", 400)
    loop = asyncio.get_running_loop()
    try:
        programs = await loop.run_in_executor(None, em_pause_asr.describe, server.host, server.port)
    except (OSError, em_pause_asr.WyomingError) as err:
        return _error(ErrorCode.SERVER_UNREACHABLE,
                      f"{server.host}:{server.port}: {str(err) or type(err).__name__}", 502)
    return _ok({"programs": [asdict(program) for program in programs]})


@auth.require_admin
async def _get_system_config(request: web.Request) -> web.Response:
    """GET /api/system/config — full system_config table."""
    loop = asyncio.get_running_loop()
    config = await loop.run_in_executor(None, db.get_all_config)
    # Don't expose schema_version — internal detail
    config.pop("schema_version", None)
    return _ok(config)


@auth.require_admin
async def _patch_system_config(request: web.Request) -> web.Response:
    """
    PATCH /api/system/config

    Body: {key: value, ...}
    Only known, mutable keys are accepted.
    """
    # The system settings the dashboard may change; everything else in
    # system_config is the controller's own bookkeeping.
    mutable_keys = {
        db.SystemConfigKey.DEVICE_APPROVAL,
        db.SystemConfigKey.SESSION_EXPIRY_DAYS,
        db.SystemConfigKey.UPDATE_CHECK_INTERVAL,
        db.SystemConfigKey.GITHUB_REPO,
    }
    body = await _json_body(request)
    loop = asyncio.get_running_loop()

    # Every key is checked before any is written: a body refused for one
    # unknown key must not have applied the others.
    unknown = [key for key in body if key not in mutable_keys]
    if unknown:
        return _error(
            ErrorCode.UNKNOWN_CONFIG_KEY,
            f"Unknown or immutable config key(s): {', '.join(unknown)}",
            400,
        )
    for key, value in body.items():
        await loop.run_in_executor(None, db.set_config, db.SystemConfigKey(key), str(value))
    return _ok(body)


# ─── Global device config ─────────────────────────────────────────────────────

@auth.require_auth
async def _get_global_config(request: web.Request) -> web.Response:
    """GET /api/global/config — fleet-wide effective defaults."""
    config = await asyncio.get_running_loop().run_in_executor(
        None, db.get_global_device_config)
    return _ok(config)


def _dropped_keys(incoming: dict[str, object], stored: dict[str, object]) -> list[str]:
    """Stored keys a replacement body would delete."""
    return sorted(set(stored) - set(incoming))


@auth.require_admin
async def _post_global_config(request: web.Request) -> web.Response:
    """Replace fleet defaults and apply each connected device's effective config."""
    config = await _json_body(request)
    explicit_replace = bool(config.pop("replace", False))
    error = _validate_config(config, request.app[SERVICES].registry())
    if error is not None:
        return error
    loop = asyncio.get_running_loop()
    stored = await loop.run_in_executor(None, db.get_global_device_config_raw)
    dropped = _dropped_keys(config, stored)
    if dropped and not explicit_replace:
        return _error(ErrorCode.WOULD_DROP_KEYS,
                      "This body would delete existing setting(s): " + ", ".join(dropped), 409)
    await loop.run_in_executor(None, db.set_global_device_config, config)
    saved = await loop.run_in_executor(None, db.get_global_device_config)

    pushed: list[str] = []
    for device_id, device in list(_devices.items()):
        if not device.online:
            continue
        effective = await loop.run_in_executor(
            None, db.get_effective_device_config, device_id)
        if await _apply_live_config(device_id, device, effective):
            pushed.append(device_id)

    all_rows = await loop.run_in_executor(None, db.get_all_devices)
    for row in all_rows:
        if row.approved:
            await em_ble_proxy.reconcile(row.device_id)
    return _ok({"config": saved, "pushed_to": pushed})


# ─── Auth — change password ───────────────────────────────────────────────────

@auth.require_auth
async def _post_change_password(request: web.Request) -> web.Response:
    """
    POST /api/auth/change-password

    Body: {current_password, new_password}
    Any authenticated user can change their own password.
    Verifies current password before accepting the new one.
    """
    user: auth.SessionUser = request["user"]
    body = await _json_body(request)
    current_password = _require_str(body, "current_password")
    new_password     = _require_str(body, "new_password")

    if len(new_password) < 8:
        return _error(ErrorCode.INVALID_INPUT, "New password must be at least 8 characters", 400)

    loop = asyncio.get_running_loop()
    db_user = await loop.run_in_executor(None, db.get_user_by_id, user["id"])
    if db_user is None:
        return _error(ErrorCode.USER_NOT_FOUND, "User not found", 404)

    if not await auth.verify_password_async(current_password, db_user.password_hash):
        return _error(ErrorCode.INVALID_CREDENTIALS, "Current password is incorrect", 401)

    new_hash = await auth.hash_password_async(new_password)
    await loop.run_in_executor(None, db.update_user_password, user["id"], new_hash)
    log.info(f"[api] Password changed for user: {user['username']}")
    return _ok({"ok": True})


# ─── Live events WebSocket ────────────────────────────────────────────────────

async def _ws_events(request: web.Request) -> web.WebSocketResponse:
    """
    WS /api/events

    Readonly access required. Dashboard connects once on load.
    Controller pushes device state changes, logs, and pending alerts
    in real time — no polling needed.
    """
    user = await auth.ws_resolve_session(request)
    if user is None:
        raise web.HTTPUnauthorized()

    ws = web.WebSocketResponse(heartbeat=30)
    await ws.prepare(request)
    _event_clients.add(ws)
    log.debug(f"[api] Events client connected ({user['username']}) "
              f"— {len(_event_clients)} total")

    try:
        # Send full device snapshot on connect so the dashboard has
        # immediate state without waiting for the first push event.
        loop = asyncio.get_running_loop()
        rows = await loop.run_in_executor(None, db.get_all_devices)
        await ws.send_str(json.dumps({
            "type":    EventType.SNAPSHOT,
            "devices": [_merge_device(r) for r in rows],
        }))

        async for msg in ws:
            # Client shouldn't send anything, but handle gracefully
            if msg.type in (aiohttp.WSMsgType.CLOSE, aiohttp.WSMsgType.ERROR):
                break

    finally:
        _event_clients.discard(ws)
        log.debug(f"[api] Events client disconnected — "
                  f"{len(_event_clients)} remaining")

    return ws


class EventType(enum.StrEnum):
    """`type` of a message on the dashboard's /api/events socket."""

    SNAPSHOT = "snapshot"
    DEVICE_UPDATE = "device_update"
    DEVICE_LOG = "device_log"
    DEVICE_CONNECTED = "device_connected"
    DEVICE_DISCONNECTED = "device_disconnected"
    DEVICE_PENDING = "device_pending"
    DEVICE_APPROVED = "device_approved"
    DEVICE_DELETED = "device_deleted"
    DEVICE_UPDATED = "device_updated"
    DEVICE_UPDATE_FAILED = "device_update_failed"
    DEVICE_AUTO_ROLLED_BACK = "device_auto_rolled_back"
    DEVICE_ROLLED_BACK = "device_rolled_back"
    # A firmware install queued for the next connect was set or cleared:
    # `queued_at` is the unix time it was queued, null once cleared.
    DEVICE_UPDATE_QUEUE = "device_update_queue"
    CONTROLLER_UPDATE = "controller_update"
    TURN_COMPLETE = "turn_complete"
    ALERTS = "alerts"
    HA_STATUS = "ha_status"


async def _push_event(event_type: EventType, /, **fields: object) -> None:
    """
    Broadcast `{"type": event_type, **fields}` to all connected /api/events
    clients.

    Called by route handlers and background tasks whenever device
    state changes.
    """
    if not _event_clients:
        return
    payload = json.dumps({"type": event_type, **fields})
    dead: set[web.WebSocketResponse] = set()
    # A snapshot: sending yields to the loop, and a client that connects or
    # drops meanwhile changes the set under a live iterator.
    for ws in list(_event_clients):
        try:
            await ws.send_str(payload)
        except Exception:
            dead.add(ws)
    _event_clients.difference_update(dead)


async def push_log_event(
    device_id: str,
    level: db.LogLevel,
    source: db.LogSource,
    message: str,
) -> None:
    """
    Persist a controller-generated log entry and push it to event clients.
    """
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, db.log_device, device_id, level, source, message)
    await _push_event(EventType.DEVICE_LOG, device_id=device_id, entry={
        "ts":      int(time.time() * 1000),
        "level":   level,
        "source":  source,
        "message": message,
    })


async def push_device_update(device_id: str, state: Mapping[str, object]) -> None:
    """Broadcast a partial device JSON (same keys as _merge_device)."""
    await _push_event(EventType.DEVICE_UPDATE, device_id=device_id, state=state)


async def push_turn_complete(device_id: str, turn: Mapping[str, object]) -> None:
    """Broadcast a persisted voice turn (a turns row, with its turn_id)."""
    await _push_event(EventType.TURN_COMPLETE, device_id=device_id, turn=_turn_json(turn))


async def push_alerts(device_id: str, kind: str, data: Mapping[str, object]) -> None:
    """Broadcast an AlertEngine notification (`kind` = its notify code)."""
    await _push_event(EventType.ALERTS, device_id=device_id, kind=kind, data=data)


async def push_ha_status(features: object) -> None:
    """Broadcast a changed em_controller.ha_status()."""
    await _push_event(EventType.HA_STATUS, features=features)


# ─── Controller release advisory ──────────────────────────────────────────────

# Fallback poll interval (s) when the stored update_check_interval is unusable.
DEFAULT_UPDATE_CHECK_INTERVAL = 3600
DEFAULT_GITHUB_REPO = "wilbowes/EchoMuse"


def _update_check_interval() -> int:
    """The configured controller release poll interval. Blocking (DB). A
    value that is not a positive integer — it is a free-text PATCH — falls
    back to the default rather than crashing the poll loop or spinning it."""
    raw = db.get_config(db.SystemConfigKey.UPDATE_CHECK_INTERVAL,
                        str(DEFAULT_UPDATE_CHECK_INTERVAL))
    try:
        interval = int(raw or DEFAULT_UPDATE_CHECK_INTERVAL)
    except ValueError:
        return DEFAULT_UPDATE_CHECK_INTERVAL
    return interval if interval > 0 else DEFAULT_UPDATE_CHECK_INTERVAL


def _github_repo() -> str:
    """Blocking (DB)."""
    return db.get_config(db.SystemConfigKey.GITHUB_REPO, DEFAULT_GITHUB_REPO) or DEFAULT_GITHUB_REPO


def _json_str(value: object) -> str:
    """A string field from GitHub's JSON, or "" when absent or not a string."""
    return value if isinstance(value, str) else ""


def _store_controller_release(version: str, notes: str, published_at: str) -> str | None:
    """Persist the controller release; returns the version it replaces. Blocking (DB)."""
    K = db.SystemConfigKey
    previous = db.get_config(K.LATEST_CONTROLLER_VERSION)
    db.set_config(K.LATEST_CONTROLLER_VERSION, version)
    db.set_config(K.LATEST_CONTROLLER_NOTES, notes)
    db.set_config(K.LATEST_CONTROLLER_PUBLISHED_AT, published_at)
    return previous


def _newest_controller_tag(refs: object) -> tuple[str, dict[str, object]] | None:
    """(tag, git object) of the highest-versioned controller-v* ref — by
    parsed version, since the API sorts refs lexically (v2.9.0 after v2.10.0)."""
    newest: tuple[tuple[int, ...], str, dict[str, object]] | None = None
    for ref in refs if isinstance(refs, list) else []:
        if not isinstance(ref, dict):
            continue
        tag = _json_str(ref.get("ref")).removeprefix("refs/tags/")
        parsed = _parse_version(tag)
        if parsed is None:
            continue
        if newest is None or parsed > newest[0]:
            obj = ref.get("object")
            newest = (parsed, tag, obj if isinstance(obj, dict) else {})
    return None if newest is None else (newest[1], newest[2])


async def _fetch_controller_release(force: bool = False) -> ControllerRelease | None:
    """
    Find the newest controller-v* tag and its annotation.

    Two requests, not one per tag: matching-refs returns every controller-v*
    ref, the newest is picked by parsed version (NOT by list order — the API
    sorts refs lexically, which puts v2.9.0 after v2.10.0), and only that one
    tag's object is dereferenced for its message.

    A lightweight tag has no annotation and no message; that is a degraded
    release, not a broken one, so it still reports the version with empty
    notes.
    """
    if not force:
        cached = _controller_cache.fresh(RELEASE_CACHE_TTL)
        if cached is not None:
            return cached

    loop = asyncio.get_running_loop()
    repo = await loop.run_in_executor(None, _github_repo)
    headers = {"Accept": "application/vnd.github+json"}
    timeout = aiohttp.ClientTimeout(total=10)

    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(
                GITHUB_TAGS_URL.format(repo=repo), headers=headers, timeout=timeout
            ) as resp:
                if resp.status != 200:
                    log.warning(f"[api] Controller tag list returned {resp.status}")
                    return _controller_cache.value
                refs = await resp.json()

            newest = _newest_controller_tag(refs)
            if newest is None:
                log.info("[api] No controller-v* tags published yet")
                return None

            tag, obj = newest
            notes = ""
            published_at = ""
            sha = obj.get("sha")
            if obj.get("type") == "tag" and isinstance(sha, str) and sha:
                async with session.get(
                    GITHUB_TAG_OBJECT_URL.format(repo=repo, sha=sha),
                    headers=headers, timeout=timeout,
                ) as resp:
                    if resp.status == 200:
                        tag_obj = await resp.json()
                        if isinstance(tag_obj, dict):
                            notes = _json_str(tag_obj.get("message")).strip()
                            tagger = tag_obj.get("tagger")
                            published_at = (_json_str(tagger.get("date"))
                                            if isinstance(tagger, dict) else "")

        version = tag.removeprefix("controller-")
        previous = await loop.run_in_executor(
            None, _store_controller_release, version, notes, published_at)

        check = _compare_versions(CONTROLLER_VERSION, version)
        release = _controller_cache.put({
            "version":      version,
            "current":      CONTROLLER_VERSION,
            "notes":        notes,
            "published_at": published_at,
            "release_url":  f"https://github.com/{repo}/releases/tag/{tag}",
            "status":       check["status"],
            "available":    check["available"],
        })

        log.info(f"[api] Latest controller release: {version} "
                 f"(running {CONTROLLER_VERSION}, {check['status']})")
        if version != previous:
            # Live-push: a dashboard left open should not sit on stale
            # information until someone reloads.
            await _push_event(EventType.CONTROLLER_UPDATE,
                              version=version, notes=notes, published_at=published_at,
                              status=check["status"], available=check["available"])
        return release

    except Exception as e:
        log.error(f"[api] Controller release fetch failed: {e}")
        return _controller_cache.value


def _stored_controller_release() -> ControllerRelease:
    """The controller release the DB last cached, for when GitHub cannot be
    reached — offline is not the same as "no update exists". Blocking (DB)."""
    K = db.SystemConfigKey
    version = db.get_config(K.LATEST_CONTROLLER_VERSION, "") or ""
    if not version:
        return {"version": None, "current": CONTROLLER_VERSION,
                "status": UpdateStatus.UNKNOWN, "available": False}
    check = _compare_versions(CONTROLLER_VERSION, version)
    return {
        "version":      version,
        "current":      CONTROLLER_VERSION,
        "notes":        db.get_config(K.LATEST_CONTROLLER_NOTES, "") or "",
        "published_at": db.get_config(K.LATEST_CONTROLLER_PUBLISHED_AT, "") or "",
        "release_url":  "",
        "status":       check["status"],
        "available":    check["available"],
    }


@auth.require_auth
async def _get_controller_release(request: web.Request) -> web.Response:
    """GET /api/releases/controller"""
    data = await _fetch_controller_release()
    if data is None:
        data = await asyncio.get_running_loop().run_in_executor(
            None, _stored_controller_release)
    return _ok(data)



def _read_first_line(path: str) -> str:
    with open(path) as fh:
        return fh.readline().strip()


def _proc_meminfo() -> dict[str, float]:
    """/proc/meminfo in MB. Empty on anything without procfs."""
    out: dict[str, float] = {}
    try:
        with open("/proc/meminfo") as fh:
            for ln in fh:
                parts = ln.split()
                if len(parts) >= 2 and parts[1].isdigit():
                    out[parts[0].rstrip(":")] = int(parts[1]) / 1024.0
    except OSError:
        pass
    return out


def _controller_stats(loop_lag_peak_ms: float) -> dict[str, object]:
    """
    The controller's own CPU, memory and storage.

    Read from /proc and the filesystem rather than psutil, which is not a
    dependency and is not worth becoming one for a handful of files. Every
    lookup degrades to an absent key: a bundle missing a stat is a nuisance,
    a bundle that 500s when the host is unusual is the failure that matters,
    since the bundle is what someone reaches for when things are wrong.

    Paths are deliberately never reported — on a bare-metal install the data
    directory carries the account name, which is the leak this same change
    fixes in the log tail.
    """
    stats: dict[str, object] = {"loop_lag_peak_ms": round(loop_lag_peak_ms, 1)}

    stats["python"] = platform.python_version()
    stats["platform"] = f"{platform.system()} {platform.machine()}"
    stats["cpu_count"] = os.cpu_count()
    stats["container"] = os.path.exists("/.dockerenv")

    try:
        load = os.getloadavg()
        stats["load_1"], stats["load_5"], stats["load_15"] = [round(x, 2) for x in load]
    except OSError:
        pass

    mem = _proc_meminfo()
    if "MemTotal" in mem:
        stats["mem_total_mb"] = round(mem["MemTotal"], 1)
    if "MemAvailable" in mem:
        stats["mem_available_mb"] = round(mem["MemAvailable"], 1)

    # cgroup v2 then v1: in a container MemTotal is the HOST's memory, which
    # reads as plenty of headroom while the container is being OOM-killed.
    for limit_path in ("/sys/fs/cgroup/memory.max",
                       "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        try:
            raw = _read_first_line(limit_path)
            if raw and raw != "max":
                val = int(raw) / 1048576.0
                # An unset v1 limit is a sentinel near 2^63, not a real cap.
                if val < 1024 * 1024:
                    stats["mem_limit_mb"] = round(val, 1)
            break
        except (OSError, ValueError):
            continue

    try:
        with open("/proc/self/status") as fh:
            for ln in fh:
                if ln.startswith("VmRSS:"):
                    stats["rss_mb"] = round(int(ln.split()[1]) / 1024.0, 1)
                    break
    except OSError:
        pass

    # Process CPU as a share of one core, averaged over the process lifetime.
    # A lifetime average, not an instant sample: a support bundle is taken
    # once, and a single 100ms sample of an asyncio process is noise.
    try:
        uptime = time.time() - _PROCESS_START
        stats["uptime_s"] = int(uptime)
        cpu_time = sum(os.times()[:2])
        # Below a few seconds the ratio is startup cost divided by almost
        # nothing — it reads as 1147% and looks like a controller on fire.
        # Omitted rather than reported wrong; a bundle taken in the first
        # seconds of a run has no CPU history worth having anyway.
        # Percent of ONE core, as `top` reports it — over 100 means more than
        # a core's worth. The windowed figures are what point anywhere: a
        # lifetime average cannot tell a controller busy right now from one
        # that was busy for an hour this morning.
        stats.update(_cpu_history.windows(time.monotonic(), cpu_time))
        if uptime >= 5.0:
            stats["cpu_pct_life"] = round(100.0 * cpu_time / uptime, 1)
    except Exception:
        pass

    # Same resolution as em_recordings, via its helper — the DB path lives in
    # one place and this must not become a second definition of it.
    db_path = Path(os.environ.get("DB_PATH", "echomuse.db")).resolve()
    try:
        usage = shutil.disk_usage(db_path.parent)
        stats["data_used_mb"] = round(usage.used / 1048576.0, 1)
        stats["data_free_mb"] = round(usage.free / 1048576.0, 1)
    except OSError:
        pass
    try:
        # The database is the thing that grows without anyone watching it.
        stats["db_mb"] = round(db_path.stat().st_size / 1048576.0, 1)
    except OSError:
        pass
    try:
        # Both kinds: the utterances and the turn recordings.
        total = sum(f.stat().st_size for kind in em_recordings.RecordingKind
                    if (rec_dir := em_recordings.recordings_dir(kind=kind)).is_dir()
                    for f in rec_dir.iterdir() if f.is_file())
        stats["recordings_mb"] = round(total / 1048576.0, 1)
    except OSError:
        pass
    try:
        # rglob, not iterdir: the wake store is nested one directory per
        # device, so a flat listing would count nothing but subdirectories.
        # Reported separately from recordings_mb because the two grow for
        # different reasons — utterance retention is a fixed handful per
        # device, while this fills up in proportion to how badly a wake
        # threshold is tuned, which is exactly the thing worth noticing.
        total = sum(f.stat().st_size
                    for f in em_wakeclips.wakes_dir().rglob("*.wav"))
        stats["wakeclips_mb"] = round(total / 1048576.0, 1)
    except OSError:
        pass

    return stats


@auth.require_admin
async def _post_provision_diagnostics(request: web.Request) -> web.Response:
    """
    POST /api/provision/diagnostics

    The wizard collects raw probe output when a step fails and posts it here;
    this returns the sanitised file to attach to an issue (#87).

    Packaged on the controller rather than in the browser on purpose. The
    redaction rules and their tests live in em_support, and a second copy in
    JavaScript would drift from them without anyone noticing until a file
    carried an SSID. This function only carries; em_support decides.

    Admin-only and a download rather than a display, the same call the support
    bundle makes: it is meant to be looked at before it is shared.
    """
    body = await _json_body(request)
    diag = em_support.build_provision_diagnostics(
        step=body.get("step") or "unknown",
        error=body.get("error") or "",
        probes=body.get("probes") or {},
        transcript=body.get("transcript") or None,
        # The wizard knows which network the operator picked; the file cannot
        # work it out once the names are gone, and "the one you wanted is
        # WPA3" is the whole answer on a #82-shaped failure.
        selected_ssid=body.get("selected_ssid") or None,
        controller_version=CONTROLLER_VERSION,
    )
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return web.Response(
        body=em_support.to_json(diag).encode(),
        content_type="application/json",
        headers={"Content-Disposition":
                 f'attachment; filename="echomuse-provision-{stamp}.json"'},
    )


@auth.require_admin
async def _get_support_bundle(request: web.Request) -> web.Response:
    """
    GET /api/support/bundle — one file to attach to an issue.

    Admin-only, and deliberately a download rather than a display: it is
    meant to be reviewed before it is shared. The privacy contract lives in
    em_support (allowlist, no speech, no labels, no network identifiers) and
    is enforced there, not here — this function only gathers.
    """
    loop = asyncio.get_running_loop()
    rows = await loop.run_in_executor(None, db.get_all_devices)

    since = time.time() - 24 * 3600
    turns: list[dict[str, object]] = []
    metrics: list[dict[str, object]] = []
    counters: list[dict[str, object]] = []
    device_configs: dict[str, dict[str, object]] = {}
    live_state: dict[str, dict[str, object]] = {}
    logs: list[tuple[int, str]] = []

    for row in rows:
        did = row.device_id
        device_configs[did] = await loop.run_in_executor(
            None, db.get_effective_device_config, did)
        device = _devices.get(did)
        live = device if device is not None and device.online else None
        hello = live.link.hello if live is not None and live.link is not None else None
        live_state[did] = {
            "connected":        live is not None,
            # Capabilities decide which HA entities and controls exist.
            "capabilities":     sorted(live.capabilities) if live is not None else [],
            # Why ambient_light is absent: no_chip, no_attribute, or ok (#90).
            "ambient_light_status": hello.ambient_light_status if hello is not None else None,
            "muted":        live.muted if live is not None else None,
            "rtt_ms":       live.rtt_last_ms if live is not None else None,
            "volume":       live.volume if live is not None else None,
            "media_state":  em_player.state(did),
            "stats":        em_support.redact_stats(live.stats if live is not None else None),
            "wake_stats":   device.wake_stats if device is not None else None,
            "alert_state":  live.alert_state if live is not None else None,
        }
        turns += [dict(t, device_id=did)
                  for t in await loop.run_in_executor(None, db.get_turns, did, 50, since)]
        # get_device_metrics resolves its own rows and does NOT carry the
        # device, so without this every device's hours pooled into one
        # anonymous list — six devices' CPU and memory with no way to tell
        # whose was whose. (`_pick` wants .keys(), which a dict already has.)
        metrics += [dict(m, device_id=did)
                    for m in await loop.run_in_executor(None, db.get_device_metrics, did, since)]
        counters += [asdict(c) for c in await loop.run_in_executor(None, db.get_wake_counters, did, since)]
        # Fetch deep and thin, rather than fetching 100 and shipping noise:
        # 89% of this table is [mem] heap dumps, so a flat 100 was ~11 lines
        # of evidence. Newest first, which is what thin_noise expects.
        raw = await loop.run_in_executor(None, db.get_device_logs, did, 500, None)
        pairs = [(lg.ts, f"{lg.ts} [{lg.level}] {lg.source}: {lg.message}")
                 for lg in raw]
        # Sorted oldest-first at the end: a log someone reads should run
        # forwards, and per-device blocks in reverse order do not.
        logs += em_support.thin_noise(pairs, key=lambda p: p[1])[:DEVICE_LOG_LINES]

    bundle = em_support.build(
        controller_version=CONTROLLER_VERSION,
        devices=[asdict(row) for row in rows],
        fleet_config=await loop.run_in_executor(None, db.get_global_device_config),
        schema_version=len(db.MIGRATIONS),
        turns=turns,
        metrics=metrics,
        counters=counters,
        device_configs=device_configs,
        live_state=live_state,
        controller_log=_log_ring.tail(),
        device_log=[ln for _, ln in sorted(logs)],
        # From the user table, not guessed at: an account name in log prose
        # has nothing to pattern-match on. Mapped to the ROLE it is replaced
        # with — "an admin opened a shell" is the diagnostic content, and
        # this is a single-operator system, so a positional alias would be a
        # one-to-one stand-in for a real person.
        accounts={u.username: u.role for u in
                  await loop.run_in_executor(None, db.get_all_users)},
        controller_stats=await loop.run_in_executor(
            None, _controller_stats, request.app[SERVICES].loop_lag_peak_ms()),
    )
    body = em_support.to_json(bundle)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return web.Response(
        body=body.encode(),
        content_type="application/json",
        headers={"Content-Disposition":
                 f'attachment; filename="echomuse-support-{stamp}.json"'},
    )


# ─── Periodic background tasks ────────────────────────────────────────────────

async def controller_release_poll_loop() -> None:
    """
    Periodically poll GitHub for a newer controller release (advisory only).

    Runs as an asyncio task started from em_controller.main().
    Interval is read from system_config each iteration so it can be
    changed at runtime without restart.
    """
    # Initial delay — let the controller finish starting up
    await asyncio.sleep(30)

    while True:
        try:
            await _fetch_controller_release(force=True)
        except Exception as e:
            log.error(f"[api] Controller release poll error: {e}")

        interval = await asyncio.get_running_loop().run_in_executor(None, _update_check_interval)
        await asyncio.sleep(interval)


async def session_prune_loop() -> None:
    """Prune expired sessions hourly."""
    while True:
        await asyncio.sleep(3600)
        try:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, db.prune_sessions)
        except Exception as e:
            log.error(f"[api] Session prune error: {e}")


# ─── Helpers shared across em_controller ─────────────────────────────────────

# Devices whose last update failed and whose supervisor log has not yet been
# collected. The fetch cannot happen at failure time — the device is gone,
# which is the whole problem — so it waits for the next connect.
_supervisor_log_wanted: set[str] = set()

# Supervisor decisions kept on the device, surviving the reboot that /tmp does
# not. Must match SUP_LOG in device_payloads/start_server.sh.
SUPERVISOR_LOG = "/data/local/etc/echomuse/supervisor.log"


async def _collect_supervisor_log(shell: em_shell.ShellBroker, device_id: str) -> None:
    """
    Pull the device's supervisor log after a failed update, on reconnect.

    A failed update destroys its own evidence: everything start_server.sh
    logs goes to /tmp, which is RAM-backed, so the power cycle used to
    recover wipes exactly the lines that would explain it (2026-08-01, a
    device that never came back and could not be diagnosed afterwards).

    The persistent log fixes the storage half. This is the other half: the
    controller notices it is owed an explanation and fetches it the moment
    the device is reachable again, pushing it into that device's log events —
    so the evidence arrives where someone would look, instead of sitting in a
    file nobody knows to read.
    """
    live = _online(device_id)
    if live is None:
        return
    # Shell builtins only (toybox may not link `tail`); the script caps the
    # file at SUP_MAX, and the last 4 KiB are kept here. The -f guard keeps a
    # missing file's redirect error out of the output.
    out = await _shell_run(
        shell, live,
        f'[ -f {SUPERVISOR_LOG} ] && while IFS= read -r l || [ -n "$l" ]; '
        f'do echo "$l"; done < {SUPERVISOR_LOG}',
        timeout=30.0)
    text = (out or "").strip()[-4096:]
    if not text:
        await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
            "Update failed, and the device has no supervisor log — firmware "
            "predating it, or start_server.sh has not been synced yet "
            "(it takes effect on the next device reboot).")
        return
    await push_log_event(device_id, db.LogLevel.WARN, db.LogSource.CONTROLLER,
        "Supervisor log from the failed update:\n" + text)


async def notify_device_connected(shell: em_shell.ShellBroker, device_id: str,
                                  version: str | None = None, *,
                                  firmware: em_firmware.BundledFirmware) -> None:
    """
    Called by em_controller when a device successfully registers.

    Includes firmware_ver in the event so the dashboard's device cache is
    updated immediately on reconnect — prevents a stale-cache false-positive
    where the frontend sees the old version during an OTA reconnect window
    and incorrectly shows an auto-rollback warning.

    Pass version directly from the device handshake (preferred — no DB round-trip).
    If omitted, falls back to a DB lookup; assumes em_controller has already
    written the new firmware_ver before calling this.
    """
    fields: dict[str, object] = {}
    if version is not None:
        fields["firmware_ver"] = version
    else:
        row = await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id)
        if row:
            fields["firmware_ver"] = row.firmware_ver
    await _push_event(EventType.DEVICE_CONNECTED, device_id=device_id, **fields)

    # Owed an explanation from a failed update? Collect it now the device is
    # reachable again. Removed from the set on the way in, so a flapping
    # device cannot queue repeated fetches. Then act on any install queued
    # for this connect. One task, scheduled rather than awaited so a slow
    # shell never delays the connect path, and sequential so the log fetch
    # and the install never compete for the device's shell.
    collect_log = device_id in _supervisor_log_wanted
    _supervisor_log_wanted.discard(device_id)
    _spawn(_after_connect(shell, device_id, firmware, collect_log), f"after-connect:{device_id}")


async def _after_connect(shell: em_shell.ShellBroker, device_id: str,
                         firmware: em_firmware.BundledFirmware, collect_log: bool) -> None:
    # The device has just registered; give its shell plane a moment
    # before demanding a session on it.
    await asyncio.sleep(3.0)
    if collect_log:
        try:
            await _collect_supervisor_log(shell, device_id)
        except Exception as e:
            log.warning(f"[api] supervisor log fetch failed for {device_id}: {e}")
    await _reconcile_update_queue(shell, device_id, firmware)


async def notify_device_disconnected(device_id: str) -> None:
    """Called by em_controller when a device disconnects."""
    await _push_event(EventType.DEVICE_DISCONNECTED, device_id=device_id)


async def notify_device_pending(device_id: str, ip: str) -> None:
    """Called by em_controller when an unapproved device attempts connection."""
    await _push_event(EventType.DEVICE_PENDING, device_id=device_id, ip=ip)


# ─── Response helpers ─────────────────────────────────────────────────────────

def _ok(data: object, status: int = 200) -> web.Response:
    return web.Response(
        status=status,
        content_type="application/json",
        body=json.dumps(data),
    )


def _error(code: ErrorCode, message: str, status: int) -> web.Response:
    return web.Response(
        status=status,
        content_type="application/json",
        body=json.dumps({"error": message, "code": code}),
    )


# ─── Request helpers ──────────────────────────────────────────────────────────

# A request body once it is known to be a JSON object; fields are narrowed
# where they are read.
JsonObject = dict[str, object]


async def _json_body(request: web.Request) -> JsonObject:
    """The request body as a JSON object; 400 when missing, invalid, or not an object."""
    try:
        body = await request.json()
    except Exception:
        body = None
    if not isinstance(body, dict):
        raise ApiError(ErrorCode.INVALID_JSON, "Request body must be a JSON object", 400)
    return body


async def _optional_json_body(request: web.Request) -> JsonObject:
    """For a body whose every field is optional: a missing, unparseable or
    non-object body reads as `{}`."""
    try:
        body = await request.json()
    except Exception:
        return {}
    return body if isinstance(body, dict) else {}


def _require_str(body: Mapping[str, object], key: str) -> str:
    """Extract a required string field from a parsed JSON body."""
    value = body.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ApiError(ErrorCode.MISSING_FIELD, f"Missing or empty required field: {key}", 400)
    return value.strip()


async def _device_row(device_id: str) -> db.DeviceRow:
    """The registered device's row; 404 device_not_found when there is none."""
    row = await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id)
    if row is None:
        raise ApiError(ErrorCode.DEVICE_NOT_FOUND, f"No device: {device_id}", 404)
    return row


def _require_online(device_id: str, message: str = "Device is not connected") -> em_device.Device:
    """The connected Device; 409 device_offline otherwise."""
    device = _online(device_id)
    if device is None:
        raise ApiError(ErrorCode.DEVICE_OFFLINE, message, 409)
    return device


# ─── Device state merge ───────────────────────────────────────────────────────

def _stored_volume(row: db.DeviceRow) -> float | None:
    """Last-known volume as an HA 0..1 float, from the persisted config."""
    try:
        level = json.loads(row.config or "{}").get("startupVolume")
    except (json.JSONDecodeError, TypeError, AttributeError):
        return None
    if level is None:
        return None
    try:
        # float() first, deliberately: em_volume swallows bad input and
        # returns 0.0, which is the right answer on the audio path and the
        # wrong one here — this function's None means "not known", and a
        # corrupt stored value must not report as "silent".
        return em_volume.device_level_to_ha(float(level))
    except (TypeError, ValueError):
        return None


def _row_sections(row: db.DeviceRow) -> list[sections_mod.SectionId]:
    """
    Overridden config sections from a device row, tolerant of unparseable
    JSON — the safe reading is "overrides nothing", which shows the device as
    fleet-scoped rather than inventing overrides it does not have.
    """
    try:
        return sections_mod.normalise(json.loads(row.config_sections or "[]"))
    except (json.JSONDecodeError, TypeError):
        return []


class DeviceJson(TypedDict):
    """One device as the dashboard receives it (/api/devices, the events
    snapshot). `device_update` events carry a partial one."""

    # Persistent
    device_id: str
    label: str | None
    approved: bool
    ip: str | None
    firmware_ver: str | None
    firmware_previous: str | None
    os_version: str | None
    first_seen: int | None
    last_seen: int | None
    config: dict[str, object]
    config_sections: list[sections_mod.SectionId]
    use_global_config: bool
    esphome_port: int | None
    ble_proxy_port: int | None
    # Live connection
    connected: bool
    capabilities: list[str]
    firmware_version: str | None
    turn_state: ActorState | None
    speaking: bool
    listening: bool
    thinking: bool
    muted: bool | None
    volume: float | None
    ambient: dict[str, object] | None
    alert_state: dict[str, object] | None
    ringing: bool | None
    wake_stats: dict[str, object] | None
    afe_stats: em_afe.AfeStatsJson | None
    diagnostic: bool
    stats: dict[str, object] | None
    collectMode: bool
    collectClips: int
    collectLastMs: int | None
    ambientMode: bool
    ambientFiles: int
    ambientMs: int
    captureMode: bool
    captureDelivered: int
    rttMs: int | None
    bleProxy: em_ble_proxy.BleProxyStatus | None
    linkTokenIssued: bool
    linkTls: bool
    wifi: dict[str, object]
    update_in_progress: bool
    update_queued_at: int | None
    update_error: str | None


def _merge_device(row: db.DeviceRow) -> DeviceJson:
    """
    One device's dashboard JSON: the DB row plus live Device state.

    Live fields describe the current connection and are null/false/empty while
    the device is offline; `wake_stats` is the last report received (it carries
    `received_ms`), so it survives a disconnect, and `afe_stats` is that
    report's native AFE decoder counters (null: it carried none).
    """
    device_id = row.device_id
    device = _devices.get(device_id)
    live = device if device is not None and device.online else None
    turn_state = live.actor.state if live is not None and live.link is not None else None
    phase = turn_state.phase if turn_state is not None else None
    alert_state = live.alert_state if live is not None else None
    sections = _row_sections(row)

    return {
        # Persistent
        "device_id":          device_id,
        "label":              row.label,
        "approved":           bool(row.approved),
        "ip":                 row.ip,
        "firmware_ver":       row.firmware_ver,
        "firmware_previous":  row.firmware_previous,
        # The Fire OS build the device last reported, beside firmware_ver,
        # the EchoMuse binary. Null: never reported (firmware predating it).
        "os_version":         row.os_version,
        "first_seen":         row.first_seen,
        "last_seen":          row.last_seen,
        "config":             json.loads(row.config or "{}"),
        "config_sections":    sections,
        "use_global_config":  not sections,
        "esphome_port":       row.esphome_api_port,
        "ble_proxy_port":     row.ble_proxy_port,
        # Live connection
        "connected":          live is not None,
        # Capability names (SPEC §11.1): a control whose capability is absent
        # is shown disabled with the reason, never as a silent no-op.
        "capabilities":       sorted(live.capabilities) if live is not None else [],
        "firmware_version":   live.firmware_version if live is not None else None,
        "turn_state":         turn_state,
        "speaking":           phase is VoicePhase.SPEAKING,
        "listening":          phase is VoicePhase.LISTENING,
        "thinking":           phase is VoicePhase.THINKING,
        "muted":              live.muted if live is not None else None,
        # Live level while connected, otherwise the last one the device
        # reported (persisted as startupVolume), as an HA 0..1 float.
        "volume":             (em_volume.device_level_to_ha(live.volume)
                               if live is not None and live.volume is not None
                               else _stored_volume(row)),
        "ambient":            live.ambient if live is not None else None,
        "alert_state":        alert_state,
        "ringing":            (alert_state.get("active") is not None
                               if alert_state is not None else None),
        "wake_stats":         device.wake_stats if device is not None else None,
        "afe_stats":          (device.afe_stats.wire()
                               if device is not None and device.afe_stats is not None else None),
        # A diagnostic uplink lease is held, so the device answers no wake (§4.4).
        "diagnostic":         bool(live is not None and live.diagnostic),
        "stats":              live.stats if live is not None else None,
        # Recording modes. collect/ambient are persisted device-row columns
        # (armed whether or not the device is up); counters are per connection.
        "collectMode":        bool(row.collect_mode),
        "collectClips":       live.collect_clips if live is not None else 0,
        "collectLastMs":      live.collect_last_ms if live is not None else None,
        "ambientMode":        bool(row.ambient_mode),
        "ambientFiles":       live.ambient_files if live is not None else 0,
        "ambientMs":          (live.ambient_rec.duration_ms
                               if live is not None and live.ambient_rec is not None else 0),
        # Script-driven capture is live-only; nothing is persisted.
        "captureMode":        live.capture_mode if live is not None else False,
        "captureDelivered":   live.capture_delivered if live is not None else 0,
        # Controller-measured control-plane round trip.
        "rttMs":              live.rtt_last_ms if live is not None else None,
        "bleProxy":           em_ble_proxy.get_status(device_id),
        "linkTokenIssued":    bool(row.token),
        "linkTls":            bool(live is not None and live.secure),
        "wifi":               _wifi_change(device_id).wire(),
        "update_in_progress": device_id in _updates_in_progress,
        # Unix time an install of the bundled firmware was queued for the
        # device's next connect; None when nothing is queued.
        "update_queued_at":   row.update_queued_at,
        # Last OTA/rollback failure; None when the last attempt succeeded.
        "update_error":       _update_errors.get(device_id),
    }
