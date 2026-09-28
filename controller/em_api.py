"""EchoMuse dashboard and authenticated HTTP API.

The device protocol sockets are owned by em_controller; this module serves the
SPA, fleet/configuration/diagnostic APIs, update and provisioning infrastructure,
and the dashboard event and shell WebSockets.
"""

import asyncio
import hashlib
import html as _html
import io
import json
import logging
import os
import platform
import re
import shutil
import sqlite3 as _sqlite3
import tempfile
import time
import zipfile
from datetime import date
from pathlib import Path
from typing import Any, Optional

import aiohttp
from aiohttp import web

import em_db as db
import em_auth as auth
import em_ble_proxy
import em_config_sections as sections_mod
import em_pki
import em_player
import em_recordings
import em_capture
import em_samples
import em_wakeclips
import em_ambient
import em_volume
import em_sounds
import em_device_assets
import em_wake_registry
import em_support
from version import VERSION as CONTROLLER_VERSION
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
# List endpoint, not /releases/latest: device firmware releases (v* tags with
# a `server` asset) share the repo with controller releases (controller-v*
# tags, GHCR image only). /releases/latest returns whichever was published
# most recently — _fetch_latest_release filters the list for the newest
# release that is actually a device firmware release.
GITHUB_API_URL = "https://api.github.com/repos/{repo}/releases?per_page=10"

# How long to cache GitHub release info in memory (seconds).
# DB is the persistent cache; this avoids hitting the DB on every
# /api/releases/latest request.
_release_cache: dict = {}
_release_cache_ts: float = 0.0
RELEASE_CACHE_TTL = 60  # seconds

# Controller releases are `controller-v*` TAGS with no GitHub Release behind
# them — controller-release.yml publishes a GHCR image and nothing else (see
# "Versioning / releases" in CLAUDE.md). So the notes come from the tag's own
# annotation: matching-refs lists the tags, and an annotated tag's object
# carries the message.
#
# Deliberately NOT solved by publishing GitHub Releases for controller tags.
# The releases list is the DEVICE firmware's update feed and
# _fetch_latest_release scans it for the newest v* tag carrying a `server`
# asset; adding controller rows puts non-firmware entries in front of that
# scan for no gain, when the annotation we already write says the same thing.
GITHUB_TAGS_URL = (
    "https://api.github.com/repos/{repo}/git/matching-refs/tags/controller-v"
)
GITHUB_TAG_OBJECT_URL = "https://api.github.com/repos/{repo}/git/tags/{sha}"

_controller_cache: dict = {}
_controller_cache_ts: float = 0.0

# Reference to the live devices dict from em_controller — set by init().
_devices: dict = {}


def _online(device_id: str):
    """The connected Device (v1 or upgrade-only legacy), otherwise None."""
    device = _devices.get(device_id)
    return device if device is not None and device.online else None


def _v1(device_id: str):
    """A connected v1 Device, otherwise None; diagnostics/alerts need v1."""
    device = _online(device_id)
    return device if device is not None and device.link is not None else None


def _parse_free_mb(df_line: str) -> int | None:
    """Available MiB from a BusyBox `df -m` row; wrapped device names make
    numeric field indexes unstable, so anchor on the percentage field."""
    fields = (df_line or "").split()
    for i, field in enumerate(fields):
        if field.endswith("%") and i > 0 and fields[i - 1].isdigit():
            return int(fields[i - 1])
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
_updates_in_progress: set[str] = set()

# Last OTA failure per device, surfaced as `update_error` in /api/devices so
# the dashboard (fleet deploy modal + per-device update log) can show *why* a
# tile stopped progressing instead of sitting at "updating…" forever. Set by
# _update_failed on every _run_update/_run_rollback failure path; cleared when
# a new update starts and on confirmed success. In-memory by design — a
# controller restart clears stale errors along with the update tasks
# themselves.
_update_errors: dict[str, str] = {}

# Pending local binary uploads — keyed by UUID token, expire after 10 minutes.
_pending_uploads: dict[str, bytes] = {}

# WiFi change state per device_id — {"pending": {...}|None, "last_result":
# {...}|None}. Deliberately NOT on the live Device object: the connection
# (and with it the Device) dies when the network switches, and the outcome
# arrives on the replacement connection. In-memory only — a controller
# restart mid-change just means the result event is lost, not the change
# itself (the device self-manages commit/rollback).
_wifi_states: dict[str, dict] = {}

# A change whose result never arrived (device bricked its network AND
# rollback failed, or controller restarted) must not block retries forever.
_WIFI_PENDING_TTL = 240  # device gates total ≤ ~135s + margin


def wifi_state(device_id: str) -> dict:
    """Current wifi change state for a device, with stale pending expiry."""
    st = _wifi_states.setdefault(device_id, {"pending": None, "last_result": None})
    pending = st.get("pending")
    if pending and time.time() - pending["started_at"] > _WIFI_PENDING_TTL:
        st["pending"] = None
        st["last_result"] = {
            "ok": False, "ssid": pending["ssid"],
            "error": "no result from device — change timed out (device may "
                     "be offline, or its rollback failed)",
            "at": time.time(),
        }
    return st


def wifi_record_result(device_id: str, ok: bool, ssid: str, error: str
                       ) -> tuple[dict, bool]:
    """
    Store a wifi_result reported by the device.

    Returns (state, duplicate). The device re-sends its result until the
    wifi_commit ack lands, so re-arrivals of the same outcome are flagged
    (duplicate=True) and don't refresh the timestamp — callers ack every
    arrival but log/record only the first.
    """
    st = wifi_state(device_id)
    last = st.get("last_result")
    if (last and last.get("ok") == ok and last.get("ssid") == ssid
            and last.get("error") == error and st.get("pending") is None):
        return st, True
    st["pending"] = None
    st["last_result"] = {"ok": ok, "ssid": ssid, "error": error, "at": time.time()}
    return st, False

# ─── Initialisation ───────────────────────────────────────────────────────────

_shell_pending:   dict = {}
_shell_dashboard: dict = {}
_shell_ws:        dict = {}   # device_id → live ws for programmatic sessions
_shell_lock:      dict = {}   # device_id → asyncio.Lock (one session at a time)

def init(devices_ref: dict, shell_pending_ref: dict, shell_dashboard_ref: dict) -> None:
    """
    Bind live shared state from em_controller.

    Must be called before create_app().
    """
    global _devices, _shell_pending, _shell_dashboard
    _devices         = devices_ref
    _shell_pending   = shell_pending_ref
    _shell_dashboard = shell_dashboard_ref


async def create_app() -> web.Application:
    """
    Build and return the aiohttp Application.

    Routes are registered here. The app is not started — the caller
    creates an AppRunner and TCPSite.
    """
    app = web.Application(middlewares=[_ingress_only_middleware, _error_middleware])

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
    app.router.add_get("/api/devices/{id}/turns/{turn}/audio", _get_turn_audio)
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
    app.router.add_post("/api/devices/{id}/rollback",     _post_device_rollback)
    app.router.add_post("/api/releases/upload",           _post_upload_binary)

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

    # Releases
    app.router.add_get("/api/releases/latest",   _get_latest_release)
    app.router.add_get("/api/releases/controller", _get_controller_release)
    app.router.add_post("/api/releases/check",   _post_check_release)
    app.router.add_post("/api/releases/deploy",  _post_deploy_all)

    # Global device config
    app.router.add_get("/api/global/config",   _get_global_config)
    app.router.add_post("/api/global/config",  _post_global_config)

    # System
    app.router.add_get("/api/support/bundle",  _get_support_bundle)
    app.router.add_get("/api/system/status",    _get_system_status)
    app.router.add_get("/api/system/config",    _get_system_config)
    app.router.add_patch("/api/system/config",  _patch_system_config)
    app.router.add_get("/api/ha/status",        _get_ha_status)

    # Provisioning
    app.router.add_get("/api/provision/start_script", _get_provision_start_script)
    app.router.add_get("/api/provision/debloat_script",   _get_provision_debloat_script)
    app.router.add_get("/api/provision/debloat_packages", _get_provision_debloat_packages)
    app.router.add_get("/api/provision/magisk_db",    _get_provision_magisk_db)
    app.router.add_get("/api/provision/latest_binary", _get_provision_latest_binary)
    app.router.add_post("/api/provision/tls_credentials", _post_provision_tls_credentials)
    app.router.add_post("/api/provision/diagnostics",     _post_provision_diagnostics)
    app.router.add_post("/api/devices/{id}/secure_link",  _post_secure_link)
    app.router.add_post("/api/devices/{id}/debloat",      _post_debloat)

    # Live events WebSocket
    app.router.add_get("/api/events", _ws_events)

    return app


async def create_runner(devices_ref: dict, shell_pending_ref: dict,
                        shell_dashboard_ref: dict) -> web.AppRunner:
    """Convenience wrapper — init + create_app + AppRunner."""
    init(devices_ref, shell_pending_ref, shell_dashboard_ref)
    app = await create_app()
    return web.AppRunner(app)


# ─── Middleware ───────────────────────────────────────────────────────────────

@web.middleware
async def _ingress_only_middleware(request: web.Request, handler):
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
async def _error_middleware(request: web.Request, handler):
    """
    Catch unhandled exceptions and return a consistent error shape.

    AuthError from em_auth is also caught here so route handlers don't
    need to handle it explicitly.
    """
    try:
        return await handler(request)
    except auth.AuthError as e:
        return e.to_response()
    except web.HTTPException:
        raise  # let aiohttp handle its own HTTP exceptions normally
    except Exception:
        log.exception(f"Unhandled error in {request.method} {request.path}")
        return _error("internal_error", "An internal error occurred", 500)


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
    user = request["user"]
    return _ok({
        "id":       user["id"],
        "username": user["username"],
        "role":     user["role"],
    })


# ─── Devices ──────────────────────────────────────────────────────────────────

@auth.require_auth
async def _get_devices(request: web.Request) -> web.Response:
    """GET /api/devices — all devices, live state merged with DB."""
    loop = asyncio.get_event_loop()
    rows = await loop.run_in_executor(None, db.get_all_devices)
    return _ok([_merge_device(row) for row in rows])


@auth.require_auth
async def _get_pending(request: web.Request) -> web.Response:
    """GET /api/devices/pending — unapproved devices."""
    loop = asyncio.get_event_loop()
    rows = await loop.run_in_executor(None, db.get_pending_devices)
    return _ok([_merge_device(row) for row in rows])


@auth.require_auth
async def _get_device(request: web.Request) -> web.Response:
    """GET /api/devices/{id}"""
    device_id = request.match_info["id"]
    loop = asyncio.get_event_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
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
        since = request.query.get("since")
        since = float(since) if since is not None else None
    except ValueError:
        return _error("bad_request", "limit/since must be numeric", 400)
    loop  = asyncio.get_event_loop()
    turns = await loop.run_in_executor(
        None, lambda: db.get_turns(device_id, limit, since)
    )
    return _ok(turns)


@auth.require_auth
async def _get_turn_audio(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/turns/{turn}/audio — the saved mic audio for
    one voice turn, as a downloadable WAV.

    Only turns captured while saveUtterances was on have one, and only the
    newest em_recordings.KEEP_PER_DEVICE per device survive — a turn row
    older than that window still carries the filename but the file is gone,
    so a 404 here is an ordinary outcome, not an error state.

    The filename is derived from (device, turn) rather than taken from the
    row: em_recordings.resolve then re-checks that the file belongs to the
    device in the URL, so a turn id from another device can't be used to
    reach its audio."""
    device_id = request.match_info["id"]
    try:
        turn_id = int(request.match_info["turn"])
    except ValueError:
        return _error("bad_request", "turn must be an integer", 400)

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    name = em_recordings.filename(device_id, turn_id)
    path = em_recordings.resolve(device_id, name) if name else None
    if path is None:
        return _error("no_recording",
                      "No saved audio for this turn", 404)

    label = _slug(row["label"] or device_id)
    return web.FileResponse(
        path,
        headers={
            "Content-Type":        "audio/wav",
            "Content-Disposition": f'attachment; filename="{label}-turn{turn_id}.wav"',
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
    try:
        body = await request.json()
    except Exception:
        body = {}
    enabled = bool(body.get("enabled"))

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    if not row["approved"]:
        return _error("not_approved",
                      "Approve this device before collecting from it", 409)
    # The mirror of the guard in _post_device_ambient: the two modes want the
    # same frames for opposite purposes, so they are mutually exclusive at
    # the API rather than resolved by a precedence rule nobody can see.
    if enabled and bool(row["ambient_mode"]):
        return _error("recording_ambient",
                      "Stop ambient recording on this device first", 409)

    connected = _online(device_id)
    if enabled and connected is not None and connected.link is None:
        return _error("upgrade_required", "Device firmware must be upgraded", 409)
    await loop.run_in_executor(None, db.set_collect_mode, device_id, enabled)

    live = connected if connected is not None and connected.link is not None else None
    if live is not None:
        # Lazy import — em_controller imports em_api at module level. It
        # writes the device log line itself, since it is the thing that
        # knows the mode actually took effect (and how many clips a session
        # produced on the way out).
        import em_controller
        await em_controller.set_collect_mode(live, enabled)
    else:
        await _push_log_event(
            device_id, "info", "controller",
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
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    clips = await loop.run_in_executor(None, em_samples.list_for, device_id)
    live  = _online(device_id)
    # What the segmenter is hearing, while it is hearing it. Without this,
    # "I turned it on and got nothing" has no answer short of a log tail:
    # a room whose floor sits 3dB under the open threshold and one where the
    # mic is muted produce the same empty list. `dropped_short` separates a
    # third case — something IS crossing the threshold, and it is a click.
    seg = getattr(live, "collect_seg", None) if live else None
    return _ok({
        "enabled":  bool(row["collect_mode"]),
        "clips":    clips,
        "count":    len(clips),
        "bytes":    sum(c["bytes"] for c in clips),
        "ms":       sum(c["ms"] for c in clips),
        "keep":     em_samples.KEEP_PER_DEVICE,
        # Session counters live on the connection, so they reset when the
        # device does — the file count above is the durable number.
        "session":  getattr(live, "collect_clips", 0) if live else 0,
        "live": None if seg is None else {
            "floor_db":      round(seg.floor_db, 1),
            "open_db":       round(seg.open_db, 1),
            "frames":        seg.stats.frames,
            "dropped_short": seg.stats.dropped_short,
            "truncated":     seg.stats.truncated,
        },
    })


@auth.require_auth
async def _get_sample_audio(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/samples/{name} — one clip, as a WAV.

    em_samples.resolve re-checks that the name belongs to the device in the
    URL: both come from the path, so without it a name from one device
    would reach another's audio."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_samples.resolve(device_id, name)
    if path is None:
        return _error("no_sample", "No such sample", 404)
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
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    label = _slug(row["label"] or device_id)

    def _build() -> bytes | None:
        clips = em_samples.list_for(device_id)
        if not clips:
            return None
        buf = io.BytesIO()
        # ZIP_STORED: WAV of speech does not compress meaningfully and
        # deflating 64MB would hold a worker for seconds.
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
            for clip in clips:
                path = em_samples.resolve(device_id, clip["name"])
                if path is None:
                    continue      # pruned between listing and reading
                z.write(path, arcname=f"{label}/{clip['name']}")
        return buf.getvalue()

    blob = await loop.run_in_executor(None, _build)
    if blob is None:
        return _error("no_samples", "No samples collected yet", 404)
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
        return _error("no_sample", "No such sample", 404)
    loop = asyncio.get_event_loop()
    await loop.run_in_executor(None, path.unlink)
    return _ok({"deleted": name})


@auth.require_admin
async def _delete_samples(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/samples — drop every clip for this device."""
    device_id = request.match_info["id"]
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    removed = await loop.run_in_executor(None, em_samples.delete_all, device_id)
    await _push_log_event(
        device_id, "info", "controller",
        f"Deleted {removed} collected sample(s)",
    )
    return _ok({"deleted": removed})


# ─── Wake clips ───────────────────────────────────────────────────────────────
#
# Accepted-candidate support audio kept per turn when saveWakeClips is on.
# Sample collection gathers prompted wake words; this captures production
# detections for model evaluation and false-positive triage.


@auth.require_auth
async def _get_turn_wake_audio(request: web.Request) -> web.Response:
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
        return _error("bad_request", "turn must be an integer", 400)

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    path = em_wakeclips.resolve(device_id, em_wakeclips.filename(turn_id))
    if path is None:
        return _error("no_wake_clip",
                      "No saved wake clip for this turn", 404)

    label = _slug(row["label"] or device_id)
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
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    label = _slug(row["label"] or device_id)

    def _build() -> bytes | None:
        clips = em_wakeclips.list_for(device_id)
        if not clips:
            return None
        buf = io.BytesIO()
        # ZIP_STORED: WAV of speech does not compress meaningfully and
        # deflating tens of MB would hold a worker for seconds.
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_STORED) as z:
            for clip in clips:
                path = em_wakeclips.resolve(device_id, clip["name"])
                if path is None:
                    continue      # pruned between listing and reading
                z.write(path, arcname=f"{label}/{clip['name']}")
        return buf.getvalue()

    blob = await loop.run_in_executor(None, _build)
    if blob is None:
        return _error("no_wake_clips", "No wake clips saved yet", 404)
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
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    removed = await loop.run_in_executor(None, em_wakeclips.delete_all, device_id)
    await _push_log_event(
        device_id, "info", "controller",
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
    try:
        body = await request.json()
    except Exception:
        body = {}
    enabled = bool(body.get("enabled"))

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    if not row["approved"]:
        return _error("not_approved",
                      "Approve this device before recording from it", 409)
    if enabled and bool(row["collect_mode"]):
        return _error("collecting",
                      "Stop wake-word sample collection on this device first",
                      409)

    connected = _online(device_id)
    if enabled and connected is not None and connected.link is None:
        return _error("upgrade_required", "Device firmware must be upgraded", 409)
    await loop.run_in_executor(None, db.set_ambient_mode, device_id, enabled)

    live = connected if connected is not None and connected.link is not None else None
    if live is not None:
        # Lazy import — em_controller imports em_api at module level. It
        # writes the device log line itself: it is the thing that knows
        # whether a file was opened, and which one was kept on the way out.
        import em_controller
        await em_controller.set_ambient_mode(live, enabled)
    else:
        await _push_log_event(
            device_id, "info", "controller",
            f"Ambient recording {'armed' if enabled else 'disarmed'} — "
            f"device offline, takes effect on its next connect",
        )
    return _ok({
        "enabled":   enabled,
        "connected": live is not None,
        # A live device reports what actually happened: arming can fail if
        # the file cannot be opened, and reporting the request back would be
        # a dashboard showing a recording that is not running.
        "recording": bool(getattr(live, "ambient_mode", False)) if live else False,
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
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    items = await loop.run_in_executor(None, em_ambient.list_for, device_id)
    live  = _online(device_id)
    rec   = getattr(live, "ambient_rec", None) if live else None
    return _ok({
        "enabled":  bool(row["ambient_mode"]),
        "clips":    items,
        "count":    len(items),
        "bytes":    sum(i["bytes"] for i in items),
        "ms":       sum(i["ms"] for i in items),
        "keep":     em_ambient.KEEP_PER_DEVICE,
        "maxMs":    em_ambient.MAX_RECORDING_MS,
        "session":  getattr(live, "ambient_files", 0) if live else 0,
        "live": None if rec is None else {
            "ms":         rec.duration_ms,
            "bytes":      rec.data_bytes,
            "startedMs":  rec.started_ms,
        },
    })


@auth.require_auth
async def _get_ambient_audio(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/ambient/{name} — one recording, as a WAV.

    Served with FileResponse rather than read into memory: these are tens of
    megabytes each, which is exactly the size that must not be buffered per
    request. em_ambient.resolve re-checks that the name belongs to the device
    in the URL — both come from the path."""
    device_id = request.match_info["id"]
    name      = request.match_info["name"]
    path = em_ambient.resolve(device_id, name)
    if path is None:
        return _error("no_recording", "No such recording", 404)
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
        return _error("no_recording", "No such recording", 404)
    loop = asyncio.get_event_loop()
    await loop.run_in_executor(None, path.unlink)
    return _ok({"deleted": name})


@auth.require_admin
async def _delete_ambient_all(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id}/ambient — drop every recording for a device.

    The recording currently open is untouched: it is not one of these files
    yet, and stopping the mode is how you end it."""
    device_id = request.match_info["id"]
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    removed = await loop.run_in_executor(None, em_ambient.delete_all, device_id)
    await _push_log_event(
        device_id, "info", "controller",
        f"Deleted {removed} ambient recording(s)",
    )
    return _ok({"deleted": removed})


@auth.require_auth
async def _get_device_activity(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/activity?days=7 — turn and device-wake rollups."""
    device_id = request.match_info["id"]
    try:
        days = min(max(int(request.query.get("days", 7)), 1), 180)
    except ValueError:
        return _error("bad_request", "days must be an integer", 400)
    since = time.time() - days * 86400
    loop = asyncio.get_running_loop()
    turns, raw_counters, metrics = await asyncio.gather(
        loop.run_in_executor(None, db.get_turns, device_id, 50_000, since),
        loop.run_in_executor(None, db.get_wake_counters, device_id, since),
        loop.run_in_executor(None, db.get_device_metrics, device_id, since),
    )
    counters = [dict(row) for row in raw_counters]

    def pct(sorted_values, p):
        if not sorted_values:
            return None
        return sorted_values[min(len(sorted_values) - 1,
                                 int(len(sorted_values) * p))]

    day_buckets: dict[str, list[dict]] = {}
    for turn in turns:
        day = time.strftime("%Y-%m-%d", time.localtime(turn["ts"]))
        day_buckets.setdefault(day, []).append(turn)

    days_out = []
    for day in sorted(day_buckets):
        bucket = day_buckets[day]
        ok = [turn for turn in bucket if turn.get("outcome") == "ok"]
        totals = sorted(turn["total_ms"] for turn in ok if (turn.get("total_ms") or 0) > 0)
        scores = [turn["wake_score"] for turn in bucket
                  if turn.get("wake_score") is not None]
        outcomes: dict[str, int] = {}
        terminal: dict[str, int] = {}
        routes: dict[str, int] = {}
        attribution: dict[str, int] = {}
        for turn in bucket:
            for value, dest in ((turn.get("outcome") or "?", outcomes),
                                (turn.get("terminal_reason") or "?", terminal),
                                (turn.get("commit_route") or "?", routes),
                                (turn.get("wake_attribution") or "?", attribution)):
                dest[value] = dest.get(value, 0) + 1
        days_out.append({
            "date": day,
            "turns": len(bucket),
            "ok": len(ok),
            "outcomes": outcomes,
            "terminal_reasons": terminal,
            "commit_routes": routes,
            "attributions": attribution,
            "total_ms_p50": pct(totals, 0.50),
            "total_ms_p95": pct(totals, 0.95),
            "wake_score_avg": round(sum(scores) / len(scores), 3) if scores else None,
            "wake_score_min": round(min(scores), 3) if scores else None,
            "underruns": sum(turn.get("underruns") or 0 for turn in bucket),
        })

    model_rollups: dict[str, dict] = {}
    for turn in turns:
        graph = turn.get("wake_model_sha256")
        if not graph:
            continue
        model = model_rollups.setdefault(graph, {"turns": 0, "scores": []})
        model["turns"] += 1
        if turn.get("wake_score") is not None:
            model["scores"].append(turn["wake_score"])
    models_out = {
        graph: {
            "turns": model["turns"],
            "score_avg": (round(sum(model["scores"]) / len(model["scores"]), 3)
                          if model["scores"] else None),
            "score_min": min(model["scores"]) if model["scores"] else None,
        }
        for graph, model in model_rollups.items()
    }
    device_wake = {
        "hops": sum(row.get("dev_hops") or 0 for row in counters),
        "overruns": sum(row.get("dev_drops") or 0 for row in counters),
        "candidates": sum(row.get("dev_crossings") or 0 for row in counters),
        "near_misses": sum(row.get("near_misses") or 0 for row in counters),
        "near_miss_max": max((row.get("near_miss_max") or 0 for row in counters), default=0),
        "max_infer_ms": max((row.get("dev_max_infer_ms") or 0 for row in counters), default=0),
        "max_score": max((row.get("dev_max_score") or 0 for row in counters), default=0),
    }
    return _ok({"days": days_out, "wake_models": models_out,
                "wake_counters": counters, "device_wake": device_wake,
                "metrics": metrics})


@auth.require_admin
async def _patch_device(request: web.Request) -> web.Response:
    """PATCH /api/devices/{id} — update label."""
    device_id = request.match_info["id"]
    body  = await _json_body(request)
    label = _require_str(body, "label")

    loop = asyncio.get_event_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    await loop.run_in_executor(None, db.set_device_label, device_id, label)
    device = _devices.get(device_id)
    if device is not None:
        device.label = label
    await _push_event({"type": "device_update", "device_id": device_id,
                       "state": {"label": label}})
    return _ok({"device_id": device_id, "label": label})


@auth.require_admin
async def _delete_device(request: web.Request) -> web.Response:
    """DELETE /api/devices/{id} — remove from registry."""
    device_id = request.match_info["id"]
    loop = asyncio.get_event_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    await loop.run_in_executor(None, db.delete_device, device_id)
    import em_controller
    await em_controller.remove_device(device_id)
    # Row gone → reconcile tears down any BT proxy listener/mDNS for it.
    await em_ble_proxy.reconcile(device_id)
    await _push_event({"type": "device_deleted", "device_id": device_id})
    return _ok({})


@auth.require_admin
async def _post_approve(request: web.Request) -> web.Response:
    """Approve a pending device, assign its label, and optionally set config."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    label = _require_str(body, "label")
    config = body.get("config")
    if config is not None and not isinstance(config, dict):
        return _error("bad_request", "config must be an object", 400)
    if config:
        error = _validate_config(config)
        if error is not None:
            return error
    loop = asyncio.get_running_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    if row["approved"]:
        return _error("already_approved", "Device is already approved", 409)
    await loop.run_in_executor(None, db.approve_device, device_id, label, config)
    await _push_event({"type": "device_approved", "device_id": device_id,
                       "label": label})
    return _ok({"device_id": device_id, "label": label})


async def _apply_live_config(device_id: str, device, effective: dict) -> bool:
    """Apply a full effective config through the Device-owned fanout."""
    if not device.online:
        return False
    try:
        await device.apply_config(effective)
    except Exception:
        # A session can close between the online check and send. Persisted
        # config is still authoritative and will apply at the next admission.
        if not device.online:
            log.info("[api] %s disconnected during config apply", device_id)
            return False
        raise
    return True


def _validate_config(values: dict) -> web.Response | None:
    """Validate config keys and post-AFE keys before any persistence."""
    for key in values:
        if key in db.REMOVED_CONFIG_KEYS:
            return _error("removed_config_key",
                          f"Configuration key '{key}' was removed", 400)
        if key not in db.DEFAULT_DEVICE_CONFIG:
            return _error("unknown_config_key",
                          f"Unknown configuration key: {key}", 400)
    if "wakeModel" in values:
        value = values["wakeModel"]
        if not isinstance(value, str):
            return _error("invalid_config", "wakeModel must be a graph SHA-256", 400)
        try:
            _wake_registry().get(value)
        except em_wake_registry.RegistryError:
            return _error("unknown_wake_model",
                          f"wakeModel is not registered: {value}", 400)
    if "extendedUtterances" in values and not isinstance(values["extendedUtterances"], bool):
        return _error("invalid_config", "extendedUtterances must be boolean", 400)
    for key in _SOUND_KEYS:
        if key not in values:
            continue
        value = values[key]
        if value in (None, ""):
            continue
        if not isinstance(value, str) or em_sounds.safe_sound_id(value) is None:
            return _error("invalid_config", f"{key} must be a valid sound id or empty", 400)
    if "timerRingSeconds" in values:
        value = values["timerRingSeconds"]
        if isinstance(value, bool) or not isinstance(value, int) or not 1 <= value <= 3600:
            return _error("invalid_config",
                          "timerRingSeconds must be an integer from 1 through 3600", 400)
    return None


@auth.require_auth
async def _get_device_config(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/config — effective config and section scoping."""
    device_id = request.match_info["id"]
    loop = asyncio.get_running_loop()
    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
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
    if await loop.run_in_executor(None, db.get_device, device_id) is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    sections_body = body.pop("config_sections", None)
    use_global = body.pop("use_global_config", None)
    explicit_replace = bool(body.pop("replace", False))
    error = _validate_config(body)
    if error is not None:
        return error

    if sections_body is None and use_global is not None:
        if not isinstance(use_global, bool):
            return _error("bad_request", "use_global_config must be boolean", 400)
        sections_body = [] if use_global else list(sections_mod.SECTION_IDS)
    if sections_body is None:
        new_sections = await loop.run_in_executor(
            None, db.get_device_config_sections, device_id)
    else:
        if not isinstance(sections_body, list):
            return _error("bad_request", "config_sections must be a list", 400)
        unknown = [section for section in sections_body
                   if section not in sections_mod.SECTIONS]
        if unknown:
            return _error("bad_request",
                          f"Unknown config section(s): {', '.join(map(str, unknown))}", 400)
        new_sections = sections_mod.normalise(sections_body)

    in_scope = sections_mod.keys_for(new_sections) | sections_mod.STATE_KEYS
    stored = await loop.run_in_executor(None, db.get_device_config, device_id)
    stored_in_scope = {key: value for key, value in stored.items() if key in in_scope}
    dropped = _dropped_keys(body, stored_in_scope)
    if dropped and not explicit_replace:
        return _error("would_drop_keys",
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
        return _error("invalid_credentials",
                      "SSID/passphrase cannot contain double-quote or "
                      "backslash characters (wpa_supplicant.conf cannot "
                      "represent them safely)", 400)
    if psk and not 8 <= len(psk) <= 63:
        return _error("invalid_credentials",
                      f"WPA passphrase must be 8–63 characters (got {len(psk)})", 400)

    live = _online(device_id)
    if live is None:
        return _error("device_offline", "Device is not connected", 409)

    st = wifi_state(device_id)
    if st["pending"]:
        return _error("wifi_change_in_progress",
                      f"A change to \"{st['pending']['ssid']}\" is already "
                      f"in progress", 409)

    st["pending"] = {"ssid": ssid, "started_at": time.time()}
    st["last_result"] = None
    await live.send("wifi_change", {"ssid": ssid, "psk": psk})
    db.log_device(device_id, "info", "controller", f'WiFi change to "{ssid}" requested')
    await _push_event({"type": "device_update", "device_id": device_id,
                       "state": {"wifi": st}})
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
    live = _online(device_id)
    if live is None:
        return _error("device_offline", "Device is not connected", 409)
    if getattr(live, "wifi_scan_future", None) is not None:
        return _error("scan_in_progress", "A scan is already running", 409)

    fut = asyncio.get_event_loop().create_future()
    live.wifi_scan_future = fut
    try:
        await live.send("wifi_scan", {})
        msg = await asyncio.wait_for(fut, timeout=20)
    except asyncio.TimeoutError:
        return _error("scan_timeout",
                      "Device did not return scan results within 20s "
                      "(old firmware without WiFi support?)", 504)
    finally:
        live.wifi_scan_future = None
    if msg.get("error"):
        return _error("scan_failed", msg["error"], 502)
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
    loop = asyncio.get_event_loop()

    row = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    try:
        limit = int(request.rel_url.query.get("limit", "100"))
    except ValueError:
        return _error("invalid_param", "limit must be an integer", 400)

    before_param = request.rel_url.query.get("before")
    before_ts = None
    if before_param:
        try:
            before_ts = int(before_param)
        except ValueError:
            return _error("invalid_param", "before must be a unix ms timestamp", 400)

    rows = await loop.run_in_executor(
        None, db.get_device_logs, device_id, limit, before_ts
    )
    entries = [
        {
            "id":        r["id"],
            "ts":        r["ts"],
            "level":     r["level"],
            "source":    r["source"],
            "message":   r["message"],
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


def _live_capture_device(device_id: str):
    """The connected v1 Device, or None."""
    return _v1(device_id)


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
    try:
        body = await request.json()
    except Exception:
        body = {}
    enabled = bool(body.get("enabled"))

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    live = _live_capture_device(device_id)
    import em_controller

    if not enabled:
        if live is not None:
            await em_controller.set_capture_mode(live, False)
        return _ok(em_controller.capture_state(live))

    if not row["approved"]:
        return _error("not_approved",
                      "Approve this device before capturing from it", 409)
    if live is None:
        if _online(device_id) is not None:
            return _error("upgrade_required", "Device firmware must be upgraded", 409)
        return _error("not_connected",
                      "Capture mode needs a connected device — it is not "
                      "persisted and cannot be armed in advance", 409)

    # Absent webhook means PULL: the controller holds each finished recording
    # and the caller collects it from /capture/recording. That is the mode
    # that works everywhere — push needs a route from the controller back to
    # the caller, which a controller on a macvlan network does not have.
    webhook = body.get("webhook") or None
    if webhook is not None and not em_capture.valid_webhook(webhook):
        return _error("bad_webhook",
                      "webhook must be an http(s) URL with a host", 400)

    await em_controller.set_capture_mode(
        live, True, webhook=webhook, idle_s=body.get("idle_s"),
    )
    return _ok(em_controller.capture_state(live))


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
    try:
        body = await request.json()
    except Exception:
        body = {}

    live = _live_capture_device(device_id)
    if live is None:
        return _error("not_connected", f"Device not connected: {device_id}", 409)
    if not getattr(live, "capture_mode", False):
        return _error("not_capturing",
                      "Arm capture mode before opening a window", 409)

    import em_controller
    window = await em_controller.open_capture_window(
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
    try:
        body = await request.json()
    except Exception:
        body = {}

    live = _live_capture_device(device_id)
    if live is None:
        return _error("not_connected", f"Device not connected: {device_id}", 409)

    import em_controller
    closed = await em_controller.close_capture_window(
        live, session=(body.get("session") or None)
    )
    return _ok({"closed": closed, **em_controller.capture_state(live)})


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
    live = _live_capture_device(device_id)
    if live is None:
        return _error("not_connected", f"Device not connected: {device_id}", 409)
    if not getattr(live, "capture_mode", False):
        return _error("not_capturing", "Capture mode is not armed", 409)

    session = request.query.get("session") or None
    try:
        wait_s = float(request.query.get("wait", 30))
    except ValueError:
        wait_s = 30.0
    wait_s = max(0.0, min(wait_s, 120.0))

    import em_controller
    result = await em_controller.take_recording(live, session, wait_s)
    if result is None:
        return _error("no_recording", "No recording available", 404)
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
    import em_controller
    return _ok(em_controller.capture_state(_live_capture_device(device_id)))


# ─── OTA: update + rollback ───────────────────────────────────────────────────

@auth.require_admin
async def _post_device_update(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/update

    Deploys a new binary to the device using A/B slots.
    Accepts an optional JSON body with {"upload_token": "..."} to deploy a
    locally uploaded binary instead of the latest GitHub release.

    Returns 202 Accepted — update runs in the background.
    """
    device_id = request.match_info["id"]

    body = {}
    try:
        body = await request.json()
    except Exception:
        pass

    upload_token = body.get("upload_token")
    binary_override = None
    release = None

    if upload_token:
        binary_override = _pending_uploads.pop(upload_token, None)
        if binary_override is None:
            return _error("invalid_token", "Upload token not found or expired", 404)
        _embedded = _extract_binary_version(binary_override)
        _ver      = _embedded or f"local-{time.strftime('%Y%m%d-%H%M')}"
        release   = {"version": _ver, "url": None}
    else:
        release = await _get_cached_release()
        if release is None:
            return _error("no_release", "No release information available", 409)

    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    live = _online(device_id)
    if live is None:
        return _error("device_offline", "Device is not connected", 409)

    if device_id in _updates_in_progress:
        return _error("update_in_progress", "An update is already in progress", 409)

    asyncio.create_task(_run_update(device_id, release, binary_override))
    return _ok({"status": "started", "version": release["version"]}, status=202)


@auth.require_admin
async def _post_device_rollback(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/rollback

    Flips the inactive A/B slot back to active. Instant — no binary transfer.
    Requires firmware_previous to be set.
    Returns 202 Accepted.
    """
    device_id = request.match_info["id"]
    loop = asyncio.get_event_loop()
    row  = await loop.run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    if not row["firmware_previous"]:
        return _error("no_rollback_available",
                      "No previous version recorded — cannot roll back", 404)

    live = _online(device_id)
    if live is None:
        return _error("device_offline", "Device is not connected", 409)

    if device_id in _updates_in_progress:
        return _error("update_in_progress", "An update is already in progress", 409)

    asyncio.create_task(_run_rollback(device_id, row["firmware_previous"]))
    return _ok({"status": "started", "rolling_back_to": row["firmware_previous"]}, status=202)


@auth.require_admin
async def _post_upload_binary(request: web.Request) -> web.Response:
    """
    POST /api/releases/upload (multipart: field name "binary")

    Upload a local binary for deployment. Returns an upload_token valid for
    10 minutes. Pass the token to /api/devices/{id}/update or
    /api/releases/deploy to deploy it.
    """
    import uuid as _uuid
    try:
        reader = await request.multipart()
        field  = await reader.next()
        if field is None or field.name != "binary":
            return _error("invalid_upload", "Expected multipart field 'binary'", 400)
        binary = await field.read()
        if not binary:
            return _error("empty_upload", "Uploaded binary is empty", 400)
        if len(binary) > 50 * 1024 * 1024:
            return _error("too_large", "Binary exceeds 50 MB limit", 413)

        token = str(_uuid.uuid4())
        _pending_uploads[token] = binary
        log.info(f"[api] Binary uploaded: {len(binary):,} bytes token={token[:8]}…")

        async def _expire():
            await asyncio.sleep(600)
            _pending_uploads.pop(token, None)
        asyncio.create_task(_expire())

        return _ok({"upload_token": token, "size": len(binary)})
    except Exception as e:
        log.error(f"[api] Upload error: {e}")
        return _error("upload_failed", str(e), 500)


# ─── Wake model registry ─────────────────────────────────────────────────────

_wake_registry_lock = asyncio.Lock()


def _wake_registry():
    # Lazy import: em_controller imports this module during startup.
    import em_controller
    return em_controller.registry()


def _wake_model_users(graph_sha256: str) -> list[str]:
    """Fleet and effective device scopes that select this graph."""
    users: list[str] = []
    if db.get_global_device_config().get("wakeModel") == graph_sha256:
        users.append("global")
    for row in db.get_all_devices():
        device_id = row["device_id"]
        if db.get_effective_device_config(device_id).get("wakeModel") == graph_sha256:
            users.append(device_id)
    return users


def _wake_model_json(model, active: str | None, users: list[str]) -> dict:
    return {**model.to_dict(), "active": model.graph_sha256 == active,
            "in_use_by": users}


@auth.require_auth
async def _get_wake_models(request: web.Request) -> web.Response:
    """GET /api/wake_models — registered BCResNet pairs and active graph."""
    registry = _wake_registry()
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


async def _read_part(field, limit: int) -> bytes | None:
    """Read one multipart part without retaining bytes beyond its limit."""
    out = bytearray()
    while True:
        block = await field.read_chunk()
        if not block:
            return bytes(out)
        out.extend(block)
        if len(out) > limit:
            return None


@auth.require_admin
async def _post_wake_model_upload(request: web.Request) -> web.Response:
    """POST /api/wake_models/upload — validate, probe, and register a graph pair."""
    try:
        reader = await request.multipart()
    except Exception:
        return _error("invalid_upload", "Expected multipart form data", 400)

    raw: dict[str, bytes | None] = {}
    text: dict[str, str] = {}
    while True:
        field = await reader.next()
        if field is None:
            break
        if field.name == "graph":
            raw["graph"] = await _read_part(field, em_wake_registry.MAX_GRAPH_BYTES)
            if raw["graph"] is None:
                return _error("too_large", "Graph exceeds the upload limit", 413)
        elif field.name == "sidecar":
            raw["sidecar"] = await _read_part(field, em_wake_registry.MAX_SIDECAR_BYTES)
            if raw["sidecar"] is None:
                return _error("too_large", "Sidecar exceeds the upload limit", 413)
        elif field.name in {"idle", "playback", "reference", "near_miss",
                            "wake_phrase", "verify_core"}:
            value = await _read_part(field, 1024)
            if value is None:
                return _error("invalid_upload", f"Field {field.name} is too long", 400)
            text[field.name] = value.decode("utf-8", "replace").strip()
        else:
            await field.release()

    required = {"graph", "sidecar"}
    missing = sorted(required - raw.keys())
    missing += sorted({"idle", "playback", "reference", "near_miss",
                       "wake_phrase", "verify_core"} - text.keys())
    if missing:
        return _error("invalid_upload", f"Missing multipart field(s): {', '.join(missing)}", 400)
    if not raw["graph"] or not raw["sidecar"]:
        return _error("invalid_upload", "Graph and sidecar must not be empty", 400)
    try:
        thresholds = em_wake_registry.Thresholds(
            idle=float(text["idle"]), playback=float(text["playback"]),
            reference=float(text["reference"]), near_miss=float(text["near_miss"]),
        )
    except (TypeError, ValueError, em_wake_registry.RegistryError) as exc:
        return _error("invalid_model", str(exc), 400)

    registry = _wake_registry()

    def register():
        with tempfile.TemporaryDirectory(prefix="wake-upload-") as directory:
            graph = Path(directory) / "graph.onnx"
            sidecar = Path(directory) / "sidecar.json"
            graph.write_bytes(raw["graph"])
            sidecar.write_bytes(raw["sidecar"])
            return registry.register(graph, sidecar, thresholds,
                                     text["wake_phrase"], text["verify_core"])

    try:
        async with _wake_registry_lock:
            model = await asyncio.get_running_loop().run_in_executor(None, register)
    except em_wake_registry.RegistryError as exc:
        return _error("invalid_model", str(exc), 400)

    users = await asyncio.get_running_loop().run_in_executor(
        None, _wake_model_users, model.graph_sha256)
    return _ok({"model": _wake_model_json(model, registry.active_sha256, users)})


@auth.require_admin
async def _delete_wake_model(request: web.Request) -> web.Response:
    """DELETE /api/wake_models/{sha256}; selected graphs cannot be removed."""
    graph_sha256 = request.match_info["sha256"]
    registry = _wake_registry()
    try:
        registry.get(graph_sha256)
    except em_wake_registry.RegistryError:
        return _error("not_found", "No such wake model", 404)

    async with _wake_registry_lock:
        users = await asyncio.get_running_loop().run_in_executor(
            None, _wake_model_users, graph_sha256)
        if users:
            return _error("model_in_use",
                          f"Wake model is selected by: {', '.join(users)}", 409)
        try:
            await asyncio.get_running_loop().run_in_executor(
                None, registry.delete, graph_sha256)
        except em_wake_registry.RegistryError as exc:
            return _error("model_in_use", str(exc), 409)
    return _ok({"deleted": graph_sha256})


@auth.require_auth
async def _get_speech_assets(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/speech_assets — named, installed, and wake state."""
    device_id = request.match_info["id"]
    row = await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)

    import em_controller
    named = None
    named_error = None
    try:
        effective = await asyncio.get_running_loop().run_in_executor(
            None, db.get_effective_device_config, device_id)
        named = em_controller.device_assets().speech_assets(
            em_controller.registry().for_config(effective)).wire()
    except em_wake_registry.RegistryError as exc:
        # No active registered graph: the named set is unavailable (null),
        # never guessed (§8.1).
        named_error = str(exc)

    device = _devices.get(device_id)
    live = device if device is not None and device.online else None
    hello = live.link.hello if live is not None and live.link is not None else None
    wake_stats = device.wake_stats if device is not None else None
    installed = (em_device_assets.installed_speech_assets(hello.get("assets") or [], named, wake_stats)
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

_SOUND_KEYS = ("timerSound", "alarmSound")


def _sound_configs() -> dict[str, dict]:
    configs = {"global": db.get_global_device_config_raw()}
    for row in db.get_all_devices():
        configs[row["device_id"]] = db.get_device_config(row["device_id"])
    return configs


def _unresolved_sounds(configs: dict[str, dict]) -> list[dict]:
    unresolved = []
    for scope, config in configs.items():
        for key in _SOUND_KEYS:
            sound_id = config.get(key)
            if not sound_id:
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
        return _error("invalid_upload", "Expected multipart form data", 400)
    sound_id = None
    suffix = None
    data = None
    while True:
        field = await reader.next()
        if field is None:
            break
        if field.name == "id":
            sound_id = (await field.read()).decode("utf-8", "replace").strip()
        elif field.name == "sound":
            suffix = em_sounds.safe_upload_suffix(field.filename or "")
            if suffix is None:
                return _error("invalid_filename",
                              "Sound must be one of: " + ", ".join(em_sounds.ALLOWED_SUFFIXES), 400)
            data = await _read_part(field, em_sounds.MAX_UPLOAD_BYTES)
            if data is None:
                return _error("too_large", "Sound exceeds the upload limit", 413)
        else:
            await field.release()
    if data is None:
        return _error("invalid_upload", "Expected multipart field 'sound'", 400)
    if not data:
        return _error("empty_upload", "Uploaded sound is empty", 400)
    sid = em_sounds.safe_sound_id(sound_id or em_sounds.DEFAULT_ID)
    if sid is None:
        return _error("invalid_id", "Sound id must be letters, digits, _ - . only", 400)

    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, em_sounds.store, sid, suffix, data)
    try:
        exported = await loop.run_in_executor(None, em_sounds.export, sid)
    except em_sounds.ExportError as exc:
        await loop.run_in_executor(None, em_sounds.delete, sid)
        return _error("decode_failed", str(exc), 400)
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
        return _error("invalid_id", "Bad sound id", 400)
    loop = asyncio.get_running_loop()
    sounds, configs = await asyncio.gather(
        loop.run_in_executor(None, em_sounds.scan),
        loop.run_in_executor(None, _sound_configs),
    )
    if not any(item["id"] == sid for item in sounds):
        return _error("not_found", "No such sound", 404)
    users = await loop.run_in_executor(None, em_sounds.in_use_by, sid, configs)
    if users:
        return _error("sound_in_use", f"Sound is selected by: {', '.join(users)}", 409)
    await loop.run_in_executor(None, em_sounds.delete, sid)
    return _ok({"deleted": sid})


def _v1_response(device_id: str):
    """(device, error response) for a route that needs a v1 session."""
    device = _online(device_id)
    if device is None:
        return None, _error("device_offline", "Device is not connected", 409)
    if device.link is None:
        return None, _error("upgrade_required", "Device firmware must be upgraded", 409)
    return device, None


@auth.require_auth
async def _post_sound_preview(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/sounds/preview — alert-class local preview."""
    body = await _json_body(request)
    sound_id = _require_str(body, "sound_id")
    if em_sounds.safe_sound_id(sound_id) is None:
        return _error("invalid_id", "Bad sound id", 400)
    device, error = _v1_response(request.match_info["id"])
    if error is not None:
        return error
    import em_controller
    result = await em_controller.preview_sound(device, sound_id)
    if result.get("error") == "offline":
        return _error("device_offline", "Device disconnected", 409)
    return _ok(result)


@auth.require_auth
async def _post_sound_stop(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/sounds/stop — stop the current preview."""
    device, error = _v1_response(request.match_info["id"])
    if error is not None:
        return error
    import em_controller
    await em_controller.stop_preview(device)
    return _ok({"stopped": True})


# ─── Alerts panel ─────────────────────────────────────────────────────────────

def _alert_status(endpoint_id: str) -> dict | None:
    import em_controller
    status = em_controller.alerts().status(endpoint_id)
    if status is None:
        return None
    status = {**status}
    occurrences = []
    for occurrence in status.get("occurrences") or []:
        armed = bool(occurrence.get("armed_on_endpoint"))
        occurrences.append({**occurrence, "stored_in_ha": True,
                            "delivery_pending": not armed})
    status["occurrences"] = occurrences
    return status


@auth.require_auth
async def _get_device_alerts(request: web.Request) -> web.Response:
    """GET /api/devices/{id}/alerts — one endpoint's full alert panel."""
    device_id = request.match_info["id"]
    row = await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id)
    if row is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    import em_controller
    return _ok({"status": _alert_status(device_id), "ha": em_controller.ha_status()})


@auth.require_admin
async def _post_alarm(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/alarms — dashboard entry to AlertEngine."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    alarm_time = _require_str(body, "time")
    days = body.get("days", [])
    if not isinstance(days, list) or not all(isinstance(day, str) for day in days):
        return _error("bad_request", "days must be a list of weekday names", 400)
    name = body.get("name") or ""
    if not isinstance(name, str):
        return _error("bad_request", "name must be a string", 400)
    on_date = None
    if body.get("date") not in (None, ""):
        try:
            on_date = date.fromisoformat(body["date"])
        except (TypeError, ValueError):
            return _error("bad_request", "date must be YYYY-MM-DD", 400)
    if await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id) is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    import em_controller
    result = await em_controller.alerts().set_alarm(
        device_id, alarm_time, days, name, on_date=on_date, source="dashboard")
    return _ok(result)


@auth.require_admin
async def _post_alarm_cancel(request: web.Request) -> web.Response:
    """POST /api/devices/{id}/alarms/cancel — cancel matching schedules."""
    device_id = request.match_info["id"]
    body = await _json_body(request)
    name = body.get("name") or ""
    alarm_time = body.get("time") or ""
    if not isinstance(name, str) or not isinstance(alarm_time, str):
        return _error("bad_request", "name and time must be strings", 400)
    if await asyncio.get_running_loop().run_in_executor(None, db.get_device, device_id) is None:
        return _error("device_not_found", f"No device: {device_id}", 404)
    import em_controller
    result = await em_controller.alerts().cancel_alarm(
        device_id, name=name, time=alarm_time, all_alarms=bool(body.get("all")),
        source="dashboard")
    return _ok(result)


# ─── OTA background tasks ─────────────────────────────────────────────────────


def _extract_binary_version(binary: bytes) -> str | None:
    """
    Scan a compiled Go binary for its embedded EchoMuse version string.
    The version is compiled in via -ldflags "-X ...Version=YYYYMMDD-HHMM-suffix".
    Pattern matches e.g. 20260614-1152-dev, 20260614-0513-release, etc.
    Falls back to None if not found, caller generates a local-YYYYMMDD-HHMM label.
    """
    import re as _re
    match = _re.search(rb'20\d{6}-\d{4}-[a-z][a-z0-9]*', binary)
    return match.group(0).decode("ascii") if match else None


async def _update_failed(device_id: str, reason: str) -> None:
    """
    Record and broadcast an OTA failure: device log line, in-memory
    update_error (read back via /api/devices), and a device_update_failed
    event for the dashboard WS. Every abort path in _run_update/_run_rollback
    must come through here — a log-only failure leaves the dashboard tile at
    "updating…" indefinitely.
    """
    _update_errors[device_id] = reason
    await _push_log_event(device_id, "error", "controller", reason)
    await _push_event({
        "type":      "device_update_failed",
        "device_id": device_id,
        "error":     reason,
    })


async def _run_update(device_id: str, release: dict,
                      binary_override: bytes | None = None) -> None:
    """
    Background task: A/B slot update.

    1. Fetch binary (GitHub or pre-uploaded).
    2. Detect active slot via readlink; migrate legacy layout if needed.
    3. Stream binary to inactive slot.
    4. Flip symlink atomically.
    5. Restart service and monitor reconnect.
    6. Detect auto-rollback (start_server.sh retry exhausted).
    """
    _updates_in_progress.add(device_id)
    _update_errors.pop(device_id, None)  # fresh attempt clears the last failure
    loop = asyncio.get_event_loop()
    version = release["version"]

    try:
        await _push_log_event(device_id, "info", "controller",
                              f"OTA update starting → {version}")

        # Fetch binary
        if binary_override is not None:
            binary = binary_override
            await _push_log_event(device_id, "info", "controller",
                                  f"Using uploaded binary ({len(binary):,} bytes)")
        else:
            binary = await _fetch_binary(release["url"])
            if binary is None:
                await _update_failed(device_id,
                                     "Failed to fetch binary from GitHub")
                return

        # Record current version as previous before anything changes
        row = await loop.run_in_executor(None, db.get_device, device_id)
        current_ver = row["firmware_ver"] if row else None
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
        detect_result = await _shell_run(live, detect_cmd, timeout=60.0)
        log.info(f"[api] Slot detect result for {device_id}: {detect_result!r}")

        if "MIGRATE_FAILED" in detect_result:
            await _update_failed(device_id,
                                 "A/B migration failed — aborting update")
            return

        active_slot = None
        for line in detect_result.splitlines():
            if "SLOT:" in line:
                candidate = line.split("SLOT:")[-1].strip().split()[0]
                if candidate in ("server_a", "server_b"):
                    active_slot = candidate
                    break

        if active_slot is None:
            await _update_failed(device_id,
                                 f"Could not determine active slot — output: {detect_result!r}")
            return

        if "MIGRATED" in detect_result:
            await _push_log_event(device_id, "info", "controller",
                                  "A/B migration complete — active slot: server_a")

        # Sync the startup script while we're here — OTA is the only update
        # path existing devices have for it (see _sync_start_script).
        await _sync_start_script(live, device_id)
        # Payload drift is not limited to the start script — the debloat
        # halves had no update path at all until 2026-07-30.
        await _sync_debloat(live, device_id)

        inactive_slot = "server_b" if active_slot == "server_a" else "server_a"

        # Free space, checked BEFORE writing anything. The transfer needs room
        # for the new binary alongside its .part, and running /data out of
        # space mid-write is a bad way to find out. Read with parse_free_mb,
        # never an awk field index — busybox wraps a long filesystem name onto
        # its own line, so $4 is the percentage on these devices.
        need_mb  = (len(binary) * 2) // 1048576 + 8   # binary + .part + slack
        free_out = await _shell_run(
            live, 'echo "FREE $(busybox df -m /data | busybox tail -1)"')
        free_mb = None
        for line in (free_out or "").splitlines():
            if line.startswith("FREE"):
                free_mb = _parse_free_mb(line[5:])
                break
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

        await _push_log_event(device_id, "info", "controller",
                              f"Deploying to slot {inactive_slot} (active: {active_slot})")

        # Stream binary to inactive slot. Verified by md5 before it is renamed
        # into place, so a corrupt transfer leaves the slot as it was and never
        # reaches the symlink flip below (#76).
        ok = await _stream_binary_to_slot(live, binary, inactive_slot)
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
        await _push_log_event(device_id, "info", "controller",
                              f"Flipping symlink → {inactive_slot} and restarting")
        await _shell_run(live,
            f"ln -sf {inactive_slot} /data/local/bin/server && "
            f"kill $PPID"
        )
        # Shell dies when the server process is killed — FLIP_OK will never arrive.
        # _monitor_reconnect below detects whether the restart succeeded.

        # Wait for device to come back
        confirmed = await _monitor_reconnect(device_id, version, previous_version=current_ver, timeout=90)

        if confirmed:
            _update_errors.pop(device_id, None)
            await _push_log_event(device_id, "info", "controller",
                                  f"✓ Update confirmed: {version}")
            await _push_event({
                "type":      "device_updated",
                "device_id": device_id,
                "version":   version,
            })
        else:
            row     = await loop.run_in_executor(None, db.get_device, device_id)
            running = row["firmware_ver"] if row else "unknown"

            if running == current_ver:
                # Device came back on old version — auto-rollback by start_server.sh
                await loop.run_in_executor(
                    None, db.set_firmware_previous, device_id, None
                )
                _update_errors[device_id] = (
                    f"auto-rolled back to {running} — new binary failed to start"
                )
                _supervisor_log_wanted.add(device_id)
                await _push_log_event(device_id, "warn", "controller",
                    f"Device auto-rolled back to {running} "
                    f"— new binary failed {3} start attempts")
                await _push_event({
                    "type":      "device_auto_rolled_back",
                    "device_id": device_id,
                    "version":   running,
                })
            else:
                _update_errors[device_id] = (
                    f"timed out — device running {running}"
                )
                _supervisor_log_wanted.add(device_id)
                await _push_log_event(device_id, "warn", "controller",
                    f"Update timed out — device running: {running}")
                await _push_event({
                    "type":      "device_update_failed",
                    "device_id": device_id,
                    "error":     _update_errors[device_id],
                    "running":   running,
                })

    except Exception as e:
        log.exception(f"[api] OTA update error for {device_id}: {e}")
        await _update_failed(device_id, f"OTA exception: {e}")
    finally:
        _updates_in_progress.discard(device_id)


async def _run_rollback(device_id: str, target_version: str) -> None:
    """
    Background task: flip to inactive A/B slot.

    No binary transfer needed — the old binary is already in the inactive slot.
    """
    _updates_in_progress.add(device_id)
    _update_errors.pop(device_id, None)  # fresh attempt clears the last failure
    try:
        await _push_log_event(device_id, "info", "controller",
                              f"Rolling back to {target_version}")

        live = _online(device_id)
        if live is None:
            await _update_failed(device_id,
                                 "Device disconnected before rollback")
            return

        active_slot = None
        detect_result = await _shell_run(live,
            "CURRENT=$(readlink /data/local/bin/server 2>/dev/null); "
            "if [ \"$CURRENT\" = \"server_a\" ] || [ \"$CURRENT\" = \"server_b\" ]; then "
            "  echo \"SLOT:$CURRENT\"; "
            "else echo \"SLOT_UNKNOWN\"; fi"
        )
        for line in detect_result.splitlines():
            if "SLOT:" in line:
                candidate = line.split("SLOT:")[-1].strip().split()[0]
                if candidate in ("server_a", "server_b"):
                    active_slot = candidate
                    break

        if active_slot is None:
            await _update_failed(device_id,
                                 "Cannot determine active slot — is A/B set up?")
            return

        inactive_slot = "server_b" if active_slot == "server_a" else "server_a"
        await _push_log_event(device_id, "info", "controller",
                              f"Flipping {active_slot} → {inactive_slot}")

        await _shell_run(live,
            f"ln -sf {inactive_slot} /data/local/bin/server && "
            f"kill $PPID"
        )
        # Shell dies when the server process is killed — ROLLBACK_OK will never arrive.

        loop = asyncio.get_event_loop()
        row_pre = await loop.run_in_executor(None, db.get_device, device_id)
        current_fw = row_pre["firmware_ver"] if row_pre else None
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
            await _push_log_event(device_id, "info", "controller",
                                  f"✓ Rollback confirmed: {target_version}")
            await _push_event({
                "type":      "device_rolled_back",
                "device_id": device_id,
                "version":   target_version,
            })
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
    Poll until the device reconnects on a new version, or timeout elapses.

    Accepts success if the device reports expected_version exactly (GitHub
    releases where the tag matches the binary's embedded version), OR any
    version that differs from previous_version (local uploads where the
    binary reports its own version string, not the controller's local-YYYYMMDD
    tracking string).
    """
    loop     = asyncio.get_event_loop()
    deadline = time.monotonic() + timeout
    await asyncio.sleep(8)  # give device time to stop and restart

    while time.monotonic() < deadline:
        if _online(device_id) is not None:
            row = await loop.run_in_executor(None, db.get_device, device_id)
            if row:
                running = row["firmware_ver"]
                if running == expected_version:
                    return True
                if previous_version is not None and running != previous_version:
                    return True
        await asyncio.sleep(2)

    return False


# ─── Shell helpers ────────────────────────────────────────────────────────────

async def _get_device_shell_ws(live) -> object:
    """
    Request a programmatic shell connection from the device.

    Acquires a per-device lock so sessions are strictly sequential.
    handle_shell resolves the future with the ws, then waits for ws.close()
    before returning — so the connection stays alive while we use it.
    """
    device_id = live.device_id
    loop      = asyncio.get_event_loop()

    if device_id not in _shell_lock:
        _shell_lock[device_id] = asyncio.Lock()

    try:
        await asyncio.wait_for(_shell_lock[device_id].acquire(), timeout=20.0)
    except asyncio.TimeoutError:
        raise RuntimeError(f"Shell lock acquisition timed out for {device_id}")

    future = loop.create_future()
    _shell_pending[device_id] = future
    # Deliberately do NOT set _shell_dashboard — signals programmatic mode.

    await live.send("shell_open", {})
    try:
        ws = await asyncio.wait_for(future, timeout=15.0)
        _shell_ws[device_id] = ws
        return ws
    except asyncio.TimeoutError:
        _shell_pending.pop(device_id, None)
        _shell_ws.pop(device_id, None)
        _shell_lock[device_id].release()
        raise


async def _release_shell_ws(device_id: str, live=None) -> None:
    """
    Close the programmatic shell session.

    Closing ws wakes handle_shell's ws.wait_closed(), which then returns
    and lets the device clean up its side too.
    """
    ws = _shell_ws.pop(device_id, None)
    if ws:
        try:
            await ws.close()
        except Exception:
            pass
    _shell_pending.pop(device_id, None)
    if live is not None:
        await live.send("shell_close", {})
    lock = _shell_lock.get(device_id)
    if lock and lock.locked():
        try:
            lock.release()
        except RuntimeError:
            pass


async def _shell_run(live, cmd: str, timeout: float = 30.0) -> str:
    """
    Run a shell command on the device and return its stdout as a string.

    Appends a sentinel marker to detect when output is complete.
    """
    SENTINEL = "__CMD_DONE_9f3a__"
    device_id = live.device_id
    output: list[str] = []
    try:
        ws = await _get_device_shell_ws(live)
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
    finally:
        await _release_shell_ws(device_id, live)


async def _stream_binary_to_slot(live, binary: bytes, slot: str) -> "TransferResult":
    """
    Transfer a firmware binary to /data/local/bin/{slot}.

    require_verify=True: firmware is the one payload where an unverifiable
    transfer must fail rather than proceed. A corrupt binary and a genuinely
    broken one produce the same observable — three fast exits and a rollback —
    so shipping one to the slot we are about to boot costs a reboot and a
    rollback to learn nothing (#76).
    """
    return await _stream_file_to_device(live, binary, f"/data/local/bin/{slot}",
                                        require_verify=True)


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

    __slots__ = ("ok", "stage", "detail")

    def __init__(self, ok: bool, stage: str = "ok", detail: str = ""):
        self.ok     = ok
        self.stage  = stage
        self.detail = detail

    def __bool__(self) -> bool:
        return self.ok

    def __str__(self) -> str:
        return self.detail or self.stage


# Stage detail text. Phrased for someone reading a device log who is deciding
# what to try next, so each one says whether any data left the controller —
# that is the difference between "retry, the link was bad" and "the payload or
# the device is wrong".
_TRANSFER_STAGES = {
    "shell":   "could not open a shell session on the device — no data was sent",
    "decoder": "no base64 decoder found on the device — no data was sent",
    "md5tool": "device has no md5 tool, refusing to send unverified",
    "send":    "timed out part-way through sending",
    "verify":  "sent, but timed out waiting for md5 verification",
    "corrupt": "arrived corrupt — md5 did not match what was sent",
    "error":   "transfer error",
}


def _transfer_failed(stage: str, extra: str = "") -> TransferResult:
    detail = _TRANSFER_STAGES.get(stage, stage)
    if extra:
        detail = f"{detail} ({extra})"
    return TransferResult(False, stage, detail)


async def _stream_file_to_device(live, data: bytes, dest: str,
                                 mode: str = "755",
                                 require_verify: bool = False) -> TransferResult:
    """
    Transfer a file to `dest` on the device via shell heredoc (default mode 755).

    Detects available base64 decoder (busybox base64, python3, python) before
    transferring, since 'base64' is not always in PATH on Android/FireOS.
    Uses a heredoc so no intermediate .b64 file is needed.
    The heredoc delimiter contains '_' which is not in the base64 alphabet.

    **md5 decides success, not the shell's exit status.** Bytes land in
    `{dest}.part` and are renamed only once their md5 matches what we sent, so
    a bad transfer leaves whatever was at `dest` untouched. `TRANSFER_OK` alone
    only ever proved that the decode pipeline and chmod exited 0 — not that the
    bytes arrived intact (#76). The verification rides the SAME shell session as
    the transfer, so it costs a round trip on an open socket, not a new session.

    `require_verify` decides what happens when the device has no md5 tool at
    all. Callers default to False, which warns and accepts — the same behaviour
    they had before this existed, and the base64 detection below already treats
    a busybox-less device as a contemplated state. Firmware passes True.
    """
    import base64 as _b64

    device_id     = live.device_id
    DELIM         = "__END_B64_42__"
    DETECT_MARKER = "__DETECT_DONE__"

    try:
        try:
            ws = await _get_device_shell_ws(live)
        except Exception as e:
            log.error(f"[api] Could not open a shell to {device_id} for "
                      f"{dest}: {e}")
            return _transfer_failed("shell", str(e))

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

        # ── Detect available base64 decoder ──────────────────────────────────
        # Try busybox first (Magisk provides it), then python3/python.
        # We run a round-trip sanity test so we know the decode flag works.
        # The md5 tool is detected in the SAME round trip, not a second one.
        # busybox first for the same reason as the decoder (Magisk provides
        # it); bare md5sum as a fallback since some SKUs have it in PATH.
        await ws.send(
            "if echo dGVzdA== | busybox base64 -d >/dev/null 2>&1; then echo DECODER:busybox; "
            "elif python3 -c 'import base64,sys; sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))' </dev/null >/dev/null 2>&1; then echo DECODER:python3; "
            "elif python  -c 'import base64,sys; sys.stdout.write(base64.b64decode(sys.stdin.read()))' </dev/null >/dev/null 2>&1; then echo DECODER:python; "
            "else echo DECODER:none; fi; "
            "if echo x | busybox md5sum >/dev/null 2>&1; then echo MD5:busybox; "
            "elif echo x | md5sum >/dev/null 2>&1; then echo MD5:plain; "
            f"else echo MD5:none; fi; echo {DETECT_MARKER}\n"
        )

        detect_buf = ""
        detect_dl  = time.monotonic() + 15
        while time.monotonic() < detect_dl:
            try:
                msg  = await asyncio.wait_for(ws.recv(), timeout=2)
                text = msg.decode("utf-8", errors="replace") if isinstance(msg, bytes) else msg
                detect_buf += text
                if DETECT_MARKER in detect_buf:
                    break
            except asyncio.TimeoutError:
                continue

        if "DECODER:busybox" in detect_buf:
            decode_cmd = "busybox base64 -d"
        elif "DECODER:python3" in detect_buf:
            decode_cmd = ("python3 -c "
                          "'import sys,base64; "
                          "sys.stdout.buffer.write(base64.b64decode(sys.stdin.read()))'")
        elif "DECODER:python" in detect_buf:
            decode_cmd = ("python -c "
                          "'import sys,base64; "
                          "sys.stdout.write(base64.b64decode(sys.stdin.read()))'")
        else:
            # Two very different things reach here. DETECT_MARKER present means
            # the device answered and genuinely has no decoder — a property of
            # that device, which retrying will not change. Absent means the
            # round trip produced nothing in 15s, i.e. the shell plane is not
            # carrying output, which is a link problem and IS worth retrying.
            # Reporting both as "no base64 decoder" sent #121 looking at the
            # wrong half.
            if DETECT_MARKER not in detect_buf:
                log.error(f"[api] Shell produced no output in 15s while probing "
                          f"{device_id} for a decoder — link problem, not a "
                          f"missing tool. Output so far: {detect_buf!r}")
                return _transfer_failed(
                    "shell", "device shell produced no output within 15s")
            log.error(f"[api] No base64 decoder found on device. "
                      f"Detection output: {detect_buf!r}")
            return _transfer_failed("decoder")

        log.info(f"[api] Decoder: {decode_cmd.split()[0]} {decode_cmd.split()[1]}")

        if "MD5:busybox" in detect_buf:
            md5_cmd = "busybox md5sum"
        elif "MD5:plain" in detect_buf:
            md5_cmd = "md5sum"
        else:
            md5_cmd = None
            if require_verify:
                log.error(f"[api] No md5 tool on device — refusing to transfer "
                          f"{dest} unverified. Detection output: {detect_buf!r}")
                return _transfer_failed("md5tool")
            log.warning(f"[api] No md5 tool on device — {dest} will be "
                        f"transferred WITHOUT verification")

        # ── Heredoc transfer ─────────────────────────────────────────────────
        lines = _b64.encodebytes(data).decode("ascii").splitlines(keepends=True)
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
            return _transfer_failed("send")

        if md5_cmd is None:
            log.info(f"[api] Transfer to {dest} confirmed (unverified)")
            return TransferResult(True, "unverified")

        # ── Verify, then promote ─────────────────────────────────────────────
        # Same shell session, so this is a round trip on an open socket rather
        # than a new session. A mismatch removes the .part and leaves dest as
        # it was — for firmware that means the rollback slot keeps its previous
        # binary instead of being replaced by a broken one.
        # No `cut`: md5sum prints "<hash>  <path>", and the busybox-less branch
        # is exactly where `busybox cut` would not be there either. A case glob
        # needs no external tool at all.
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
                    return _transfer_failed("corrupt",
                                            f"device reported {got[0] if got else '(none)'}")
            except asyncio.TimeoutError:
                continue

        log.error(f"[api] Transfer to {dest} timed out waiting for md5 verification "
                  f"— {dest} left untouched")
        return _transfer_failed("verify")

    except Exception as e:
        log.error(f"[api] File transfer to {dest} failed: {e}")
        return _transfer_failed("error", str(e))
    finally:
        await _release_shell_ws(device_id, live)



async def _sync_start_script(live, device_id: str) -> None:
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
    """
    path = "/data/local/bin/start_server.sh"
    try:
        script = (PAYLOADS_DIR / "start_server.sh").read_bytes()
    except OSError as e:
        log.error(f"[api] start_server.sh payload unreadable — skipping sync: {e}")
        return
    want = hashlib.md5(script).hexdigest()

    out = await _shell_run(live, f"busybox md5sum {path} 2>/dev/null")
    if want in out:
        return  # in sync — the common case
    await asyncio.sleep(1.0)  # let the md5 shell session close cleanly

    await _push_log_event(device_id, "info", "controller",
                          "start_server.sh out of date — syncing canonical version")
    tmp = path + ".new"
    res = await _stream_file_to_device(live, script, tmp)
    if not res:
        await _push_log_event(device_id, "warn", "controller",
                              f"start_server.sh sync failed: {res} — continuing OTA")
        return
    await asyncio.sleep(1.0)

    res = await _shell_run(live,
        f'NEW=$(busybox md5sum {tmp} | busybox cut -d" " -f1); '
        f'if [ "$NEW" = "{want}" ]; then '
        f'mv {tmp} {path} && chmod 755 {path} && echo SCRIPT_SYNCED; '
        f'else rm -f {tmp}; echo SCRIPT_MD5_MISMATCH:$NEW; fi')
    if "SCRIPT_SYNCED" in res:
        await _push_log_event(device_id, "info", "controller",
                              "start_server.sh synced — takes effect on next device reboot")
    else:
        await _push_log_event(device_id, "warn", "controller",
                              f"start_server.sh sync failed ({res.strip() or 'no output'}) — continuing OTA")
    await asyncio.sleep(1.0)


# Magisk service.d location of the boot-time debloat script. Installed by the
# provisioning wizard; synced from here afterwards.
DEBLOAT_SCRIPT_PATH = "/sbin/.core/img/.core/service.d/echomuse-debloat.sh"


def _debloat_packages() -> list[str]:
    """The pm-hide list from the canonical payload, comments stripped."""
    try:
        raw = (PAYLOADS_DIR / "debloat_packages.txt").read_text()
    except OSError as e:
        log.error(f"[api] debloat_packages.txt unreadable: {e}")
        return []
    return [ln.strip() for ln in raw.splitlines()
            if ln.strip() and not ln.lstrip().startswith("#")]


async def _sync_debloat(live, device_id: str) -> None:
    """
    Heal debloat drift on a device that is already in the field.

    The debloat has two halves and neither had an update path. The boot script
    was installed once by the provisioning wizard, and the pm-hide list was
    applied once at the same time — so a device provisioned before a list grew
    never receives the addition. Found 2026-07-30 when round 2 added
    com.amazon.whad and every existing device needed a manual push.

    Both halves are reconciled here, idempotently, so this is safe to run as
    often as you like:

      * the script by md5 against the canonical payload, replaced by rename so
        the running shell keeps reading the old inode (same reasoning as
        _sync_start_script — it takes effect on the next device reboot);
      * the hide list by asking the device which of those packages are still
        VISIBLE and hiding only those. One `pm list packages` costs about a
        second; `pm hide` is slow enough per call that hiding all 32
        unconditionally would add half a minute for nothing.

    Best-effort throughout: a failure logs and returns, and never blocks the
    firmware update this normally rides along with.

    Note what hiding does NOT do: com.amazon.whad is PERSISTENT, so hiding it
    leaves the running instance alive until the next reboot (am force-stop is a
    no-op on it — measured). That matches the rest of the debloat's semantics
    rather than being a shortcoming of this function, and it is why the log
    line says "next reboot".
    """
    # ── half 1: the boot script ──────────────────────────────────────────────
    try:
        script = (PAYLOADS_DIR / "echomuse-debloat.sh").read_bytes()
    except OSError as e:
        log.error(f"[api] echomuse-debloat.sh payload unreadable — skipping sync: {e}")
        script = None

    if script is not None:
        want = hashlib.md5(script).hexdigest()
        out = await _shell_run(live, f"busybox md5sum {DEBLOAT_SCRIPT_PATH} 2>/dev/null")
        if want not in out:
            # An empty result also lands here — a device provisioned before the
            # script existed has no file at all, and installing it is right.
            await asyncio.sleep(1.0)
            await _push_log_event(device_id, "info", "controller",
                                  "debloat script out of date — syncing canonical version")
            tmp = DEBLOAT_SCRIPT_PATH + ".new"
            pushed = await _stream_file_to_device(live, script, tmp)
            if pushed:
                await asyncio.sleep(1.0)
                res = await _shell_run(live,
                    f'NEW=$(busybox md5sum {tmp} | busybox cut -d" " -f1); '
                    f'if [ "$NEW" = "{want}" ]; then '
                    f'mv {tmp} {DEBLOAT_SCRIPT_PATH} && chmod 755 {DEBLOAT_SCRIPT_PATH} '
                    f'&& echo DEBLOAT_SYNCED; '
                    f'else rm -f {tmp}; echo DEBLOAT_MD5_MISMATCH:$NEW; fi')
                await _push_log_event(
                    device_id,
                    "info" if "DEBLOAT_SYNCED" in res else "warn", "controller",
                    "debloat script synced — daemon stops take effect on next device reboot"
                    if "DEBLOAT_SYNCED" in res else
                    f"debloat script sync failed ({res.strip() or 'no output'})")
            else:
                await _push_log_event(device_id, "warn", "controller",
                                      f"debloat script sync failed: {pushed}")
            await asyncio.sleep(1.0)

    # ── half 2: the pm-hide list ─────────────────────────────────────────────
    pkgs = _debloat_packages()
    if not pkgs:
        return
    # Built as a file rather than a long inline list: 30-odd package names is
    # over a kilobyte of command line, and a shell command that is *usually*
    # short enough is the kind of thing that breaks on the day someone adds the
    # thirty-third package.
    listing = ("\n".join(pkgs) + "\n").encode()
    remote_list = "/data/local/tmp/em_debloat_pkgs.txt"
    pushed = await _stream_file_to_device(live, listing, remote_list, mode="644")
    if not pushed:
        await _push_log_event(device_id, "warn", "controller",
                              f"debloat hide-list sync failed: {pushed}")
        return
    await asyncio.sleep(1.0)

    # Two details here were learned by getting them wrong (2026-07-30).
    #
    # The list is iterated with `for` over a variable, NOT `while read < file`:
    # `pm` is a wrapper that starts app_process, and a command inside a read
    # loop can consume the loop's own stdin, silently ending it early. `pm hide`
    # also gets </dev/null for the same reason.
    #
    # And the end state is VERIFIED by re-listing rather than trusting the
    # return code of each hide, so a partial failure cannot read as success.
    #
    # The match is `grep -qx`, ANCHORED to the whole line, and that is the
    # important part. A shell `case "$VIS" in *"package:$p"*)` looks equivalent
    # and is not: `package:com.amazon.tcomm` is also a substring of
    # `package:com.amazon.tcomm.client`, so three packages appeared un-hidden
    # because a *different*, longer-named package was visible. That produced a
    # confident warning about a FireOS limitation that did not exist — `pm hide`
    # had worked, and dumpsys said hidden=true throughout.
    res = await _shell_run(live,
        f'PKGS=$(cat {remote_list}); VIS=$(pm list packages); N=0; '
        f'for p in $PKGS; do '
        f'if echo "$VIS" | busybox grep -qx "package:$p"; then '
        f'pm hide "$p" >/dev/null 2>&1 </dev/null && N=$((N+1)); fi; '
        f'done; '
        f'VIS2=$(pm list packages); LEFT=""; '
        f'for p in $PKGS; do '
        f'if echo "$VIS2" | busybox grep -qx "package:$p"; then LEFT="$LEFT $p"; fi; '
        f'done; '
        f'rm -f {remote_list}; '
        f'echo HIDDEN_APPLIED:$N; echo STILL_VISIBLE:$LEFT', timeout=180.0)

    applied = 0
    for tok in res.split():
        if tok.startswith("HIDDEN_APPLIED:"):
            try:
                applied = int(tok.split(":", 1)[1])
            except ValueError:
                pass
    still = ""
    for line in res.splitlines():
        if line.strip().startswith("STILL_VISIBLE:"):
            still = line.split(":", 1)[1].strip()
    if applied:
        await _push_log_event(device_id, "info", "controller",
                              f"debloat: hid {applied} newly-listed package(s) — "
                              f"persistent ones stop at the next device reboot")
    if still:
        await _push_log_event(device_id, "warn", "controller",
                              f"debloat: {len(still.split())} package(s) could not be "
                              f"hidden and are still active: {still}")
    await asyncio.sleep(1.0)


async def _exec_shell(live, cmd: str) -> None:
    """Send a command to the device shell and return immediately (fire-and-forget)."""
    try:
        ws = await _get_device_shell_ws(live)
        await ws.send(cmd + "\n")
        await asyncio.sleep(0.5)
    except Exception as e:
        log.warning(f"[api] Shell exec failed ({cmd!r}): {e}")
    finally:
        await _release_shell_ws(live.device_id, live)


# ─── Shell WebSocket proxy (interactive dashboard terminal) ───────────────────

async def _ws_shell(request: web.Request) -> web.WebSocketResponse:
    """
    WS /api/devices/{id}/shell — interactive shell terminal for dashboard.

    Auth is handled via ws_resolve_session (checks cookie then ?token= query
    param) because browser WebSocket clients cannot set custom headers.
    Do NOT add @auth.require_admin here — _extract_token doesn't read query
    params and would reject every connection before this function runs.

    Sets _shell_dashboard so handle_shell proxies in interactive mode.
    """
    device_id = request.match_info["id"]

    user = await auth.ws_resolve_session(request)
    if user is None:
        raise web.HTTPUnauthorized()
    if user["role"] != "admin":
        raise web.HTTPForbidden()

    live = _online(device_id)
    if live is None:
        raise web.HTTPConflict(reason="Device is not connected")

    # Refuse if a programmatic shell session (e.g. OTA transfer) is in progress.
    # Opening a terminal mid-transfer sends shell_open to the device, which cancels
    # the current shell context and kills the transfer.
    lock = _shell_lock.get(device_id)
    if lock and lock.locked():
        raise web.HTTPConflict(reason="Device shell is busy — an OTA update is in progress")

    ws = web.WebSocketResponse()
    await ws.prepare(request)

    log.info(f"[api] Shell session requested: {device_id} by {user['username']}")
    await _push_log_event(device_id, "info", "controller",
                          f"Shell session opened by {user['username']}")

    loop = asyncio.get_event_loop()
    done_future = loop.create_future()
    _shell_pending[device_id]   = done_future
    _shell_dashboard[device_id] = ws
    # Do NOT set _shell_ws or acquire _shell_lock — interactive sessions
    # bypass the programmatic shell mechanism entirely.

    try:
        # pty:true — interactive terminal wants a real PTY (mksh prompt,
        # line editing, top/vi, resize). Old firmware ignores the field and
        # opens the legacy pipe; handle_shell reports the established mode
        # to the dashboard via shell_meta. Programmatic sessions
        # (_get_device_shell_ws) deliberately do not set it.
        await live.send("shell_open", {"pty": True})
        await done_future
    except Exception as e:
        log.warning(f"[api] Shell session error ({device_id}): {e}")
    finally:
        _shell_pending.pop(device_id, None)
        _shell_dashboard.pop(device_id, None)
        await live.send("shell_close", {})
        log.info(f"[api] Shell session closed: {device_id}")
        await _push_log_event(device_id, "info", "controller",
                              f"Shell session closed by {user['username']}")

    return ws


# ─── Releases ─────────────────────────────────────────────────────────────────

@auth.require_auth
async def _get_latest_release(request: web.Request) -> web.Response:
    """GET /api/releases/latest — latest GitHub release, from cache."""
    release = await _get_cached_release()
    if release is None:
        return _error("no_release", "No release information available", 404)
    return _ok(release)


@auth.require_admin
async def _post_check_release(request: web.Request) -> web.Response:
    """POST /api/releases/check — force re-poll GitHub."""
    release = await _fetch_latest_release(force=True)
    if release is None:
        return _error("no_release", "Could not fetch release from GitHub", 502)
    return _ok(release)


@auth.require_admin
async def _post_deploy_all(request: web.Request) -> web.Response:
    """
    POST /api/releases/deploy

    Deploy to all connected, approved, non-current devices.
    Accepts optional {"upload_token": "..."} to deploy a local binary
    to the whole fleet instead of the latest GitHub release.
    """
    body = {}
    try:
        body = await request.json()
    except Exception:
        pass

    upload_token = body.get("upload_token")
    binary_override = None
    release = None

    if upload_token:
        binary_override = _pending_uploads.pop(upload_token, None)
        if binary_override is None:
            return _error("invalid_token", "Upload token not found or expired", 404)
        release = {"version": f"local-{time.strftime('%Y%m%d-%H%M')}", "url": None}
    else:
        release = await _get_cached_release()
        if release is None:
            return _error("no_release", "No release information available", 409)

    started = []
    skipped = []
    loop = asyncio.get_event_loop()

    for device_id, device in list(_devices.items()):
        if not device.online:
            skipped.append({"device_id": device_id, "reason": "offline"})
            continue
        row = await loop.run_in_executor(None, db.get_device, device_id)
        if row is None or not row["approved"]:
            skipped.append({"device_id": device_id, "reason": "not_approved"})
            continue
        if not upload_token and row["firmware_ver"] == release["version"]:
            skipped.append({"device_id": device_id, "reason": "already_current"})
            continue
        if device_id in _updates_in_progress:
            skipped.append({"device_id": device_id, "reason": "update_in_progress"})
            continue

        asyncio.create_task(_run_update(device_id, release, binary_override))
        started.append(device_id)

    return _ok({
        "version": release["version"],
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
async def _get_provision_debloat_script(request: web.Request) -> web.Response:
    """GET /api/provision/debloat_script — the Magisk service.d boot script
    that re-stops init-launched daemons each boot (Debloat wizard step)."""
    return web.Response(
        text=_read_payload("echomuse-debloat.sh"),
        content_type='text/plain',
        headers={'Content-Disposition': 'attachment; filename="echomuse-debloat.sh"'},
    )


@auth.require_admin
async def _get_provision_debloat_packages(request: web.Request) -> web.Response:
    """GET /api/provision/debloat_packages — the pm-hide package list as JSON.

    Parsed server-side (comments/blank lines stripped) so the wizard never
    has to understand the file format and list edits ship without a
    dashboard rebuild.
    """
    packages = [
        line.strip()
        for line in _read_payload("debloat_packages.txt").splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    return _ok({"packages": packages})


@auth.require_admin
async def _get_provision_latest_binary(request: web.Request) -> web.Response:
    """
    GET /api/provision/latest_binary — serves the bytes of the latest
    GitHub release binary, for the provisioning wizard's "Install latest
    from GitHub" step.

    Distinct from /api/releases/latest (metadata only: {version, url}) —
    this route does the actual download from GitHub on the server side
    and streams the binary back, since a freshly-flashed device isn't
    registered in _devices yet and can't go through the
    /api/devices/{id}/update fleet-OTA path (that requires a live
    WebSocket session). Reuses the same cache/fetch machinery as OTA.
    """
    release = await _get_cached_release()
    if release is None:
        return _error("no_release", "No release information available", 404)

    binary = await _fetch_binary(release["url"])
    if binary is None:
        return _error("fetch_failed", "Could not download binary from GitHub", 502)

    return web.Response(
        body=binary,
        content_type='application/octet-stream',
        headers={
            'Content-Disposition': 'attachment; filename="server"',
            'X-Release-Version': release["version"],
        },
    )


@auth.require_admin
async def _get_provision_magisk_db(request: web.Request) -> web.Response:
    """GET /api/provision/magisk_db — generates a pre-seeded Magisk grant DB.

    Grants uid 2000 (adb shell) and uid 0 (root) unconditional su access so
    the screenless Echo Dot never shows a grant dialog.
    """
    def _build_db() -> bytes:
        fd, path = tempfile.mkstemp(suffix='.db')
        os.close(fd)
        try:
            con = _sqlite3.connect(path)
            # Schema confirmed against a real Magisk v17.3 device dump
            # (sqlite> .schema on a working /data/adb/magisk.db) — NOT
            # guessed. magiskd queries settings and strings on every su
            # request regardless of whether anything's stored in them;
            # the previous version of this function only created
            # `policies` (and with the wrong columns — no package_name in
            # the real schema, PRIMARY KEY is uid alone). Missing
            # settings/strings meant every single su call hit
            # "sqlite3_exec: no such table" on each of those two tables
            # and got hard-rejected — which looked like a hang from the
            # wizard side because su was taking up to ~60s per rejection
            # cycle, far longer than the wizard's retry loop accounted for.
            con.execute(
                "CREATE TABLE policies ("
                "  uid INT,"
                "  policy INT,"
                "  until INT,"
                "  logging INT,"
                "  notification INT,"
                "  PRIMARY KEY(uid)"
                ")"
            )
            con.execute(
                "CREATE TABLE settings (key TEXT, value INT, PRIMARY KEY(key))"
            )
            con.execute(
                "CREATE TABLE strings (key TEXT, value TEXT, PRIMARY KEY(key))"
            )
            con.execute(
                "CREATE TABLE denylist (package_name TEXT, process TEXT, "
                "PRIMARY KEY(package_name, process))"
            )
            # policy=2 → always grant. Matches the confirmed real schema:
            # uid, policy, until, logging, notification — no package_name.
            con.execute("INSERT INTO policies (uid, policy, until, logging, notification) VALUES (2000, 2, 0, 1, 1)")
            con.execute("INSERT INTO policies (uid, policy, until, logging, notification) VALUES (0, 2, 0, 1, 1)")
            con.commit()
            con.close()
            return Path(path).read_bytes()
        finally:
            os.unlink(path)

    loop = asyncio.get_event_loop()
    data = await loop.run_in_executor(None, _build_db)
    return web.Response(
        body=data,
        content_type='application/octet-stream',
        headers={'Content-Disposition': 'attachment; filename="magisk.db"'},
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
    if _tls_dir is None:
        return _error("tls_unavailable",
                      "Device-link TLS is not active on this controller", 503)
    body      = await _json_body(request)
    device_id = _require_str(body, "device_id")

    loop  = asyncio.get_event_loop()
    token = await loop.run_in_executor(None, db.ensure_device_token, device_id)
    return _ok({
        "ca_pem": em_pki.ca_pem(_tls_dir),
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
    if _tls_dir is None:
        return _error("tls_unavailable",
                      "Device-link TLS is not active on this controller", 503)
    device_id = request.match_info["id"]
    live = _online(device_id)
    if live is None:
        return _error("device_offline", f"Device not connected: {device_id}", 409)

    task = asyncio.create_task(_run_secure_link(device_id))
    task.add_done_callback(_log_task_exception_api)
    return _ok({"started": True})


@auth.require_admin
async def _post_debloat(request: web.Request) -> web.Response:
    """
    POST /api/devices/{id}/debloat

    Re-apply the debloat payloads to a live device: sync the boot script and
    hide any newly-listed packages.

    This exists because the OTA-time sync cannot reach every device. A device
    already running the latest firmware will not be updated again, so it would
    never receive a payload change — which is exactly the situation the first
    device hit (Lounge was current when round 2 landed). Idempotent, so
    pressing it twice costs a `pm list packages` and nothing else.
    """
    device_id = request.match_info["id"]
    live = _online(device_id)
    if live is None:
        return _error("device_offline", f"Device not connected: {device_id}", 409)

    # No explicit shell release here: _shell_run and _stream_file_to_device each
    # acquire and release the session in their own finally, which is why
    # _sync_start_script does not either. Releasing it from out here could close
    # a session a concurrent caller had opened.
    task = asyncio.create_task(_sync_debloat(live, device_id))
    task.add_done_callback(_log_task_exception_api)
    return _ok({"started": True})


def _log_task_exception_api(task: asyncio.Task) -> None:
    if task.cancelled():
        return
    exc = task.exception()
    if exc is not None:
        log.error(f"[api] Unhandled exception in background task: {exc}", exc_info=exc)


async def _run_secure_link(device_id: str) -> None:
    """Background task: install TLS credentials on a live device."""
    loop = asyncio.get_event_loop()
    live = _online(device_id)
    if live is None:
        return
    try:
        await _push_log_event(device_id, "info", "controller",
                              "Secure link: pushing TLS credentials")
        token = await loop.run_in_executor(None, db.ensure_device_token, device_id)
        ca    = em_pki.ca_pem(_tls_dir)

        await _shell_run(live, f"mkdir -p {DEVICE_TLS_DIR}")
        await asyncio.sleep(1.0)  # let the shell session close cleanly

        ok = await _stream_file_to_device(
            live, ca.encode("ascii"), f"{DEVICE_TLS_DIR}/ca.pem", mode="644")
        if ok:
            await asyncio.sleep(1.0)
            ok = await _stream_file_to_device(
                live, token.encode("ascii"), f"{DEVICE_TLS_DIR}/token", mode="600")
        if not ok:
            await _push_log_event(device_id, "error", "controller",
                                  f"Secure link: credential transfer failed: {ok}")
            return

        await _push_log_event(
            device_id, "info", "controller",
            "Secure link: credentials installed — bouncing connection to switch to wss")
        # The device reloads credentials on every dial, so a reconnect is
        # enough to move to the TLS listener.
        await live.disconnect()
    except Exception as e:
        log.exception(f"[api] Secure link failed for {device_id}: {e}")
        await _push_log_event(device_id, "error", "controller",
                              f"Secure link failed: {e}")


# ─── System ───────────────────────────────────────────────────────────────────

@auth.require_auth
async def _get_system_status(request: web.Request) -> web.Response:
    """GET /api/system/status"""
    loop = asyncio.get_event_loop()
    all_rows = await loop.run_in_executor(None, db.get_all_devices)
    release = await _get_cached_release()

    # Lazy import — em_controller imports em_api at module level.
    import em_controller as _ctrl

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
        "loop_lag_peak_ms": round(_ctrl._loop_lag_peak_ms, 1),
        "connected":      sum(1 for d in list(_devices.values()) if d.online),
        "total_devices":  len(all_rows),
        "pending":        sum(1 for r in all_rows if not r["approved"]),
        "approval_mode":  db.get_config("device_approval", "strict"),
        "latest_release": release["version"] if release else None,
        # Controller update, surfaced alongside the firmware one so the header
        # can badge it without a second round trip. Read-only by design: the
        # controller runs as a container the user owns, and updating it is a
        # `docker compose pull` they perform — there is deliberately no action
        # here, only the information needed to decide to take it.
        "controller_update": _controller_cache.get("version")
            if _controller_cache.get("available") else None,
        "updates_available": sum(
            1 for r in all_rows
            if r["firmware_ver"] and release
            and r["firmware_ver"] != release["version"]
        ),
        "speech": _ctrl.speech_worker_status(),
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
    import em_controller
    return _ok({"features": em_controller.ha_status(), "calendar_url": _ha_calendar_url()})


@auth.require_admin
async def _get_system_config(request: web.Request) -> web.Response:
    """GET /api/system/config — full system_config table."""
    loop = asyncio.get_event_loop()
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
    MUTABLE_KEYS = {
        "device_approval",
        "session_expiry_days",
        "update_check_interval",
        "github_repo",
    }
    body = await _json_body(request)
    loop = asyncio.get_event_loop()

    updated = {}
    unknown = []
    for key, value in body.items():
        if key not in MUTABLE_KEYS:
            unknown.append(key)
            continue
        await loop.run_in_executor(None, db.set_config, key, str(value))
        updated[key] = value

    if unknown:
        return _error(
            "unknown_config_key",
            f"Unknown or immutable config key(s): {', '.join(unknown)}",
            400,
        )
    return _ok(updated)


# ─── Global device config ─────────────────────────────────────────────────────

@auth.require_auth
async def _get_global_config(request: web.Request) -> web.Response:
    """GET /api/global/config — fleet-wide effective defaults."""
    config = await asyncio.get_running_loop().run_in_executor(
        None, db.get_global_device_config)
    return _ok(config)


def _dropped_keys(incoming: dict, stored: dict) -> list[str]:
    """Stored keys a replacement body would delete."""
    return sorted(set(stored) - set(incoming))


@auth.require_admin
async def _post_global_config(request: web.Request) -> web.Response:
    """Replace fleet defaults and apply each connected device's effective config."""
    config = await _json_body(request)
    explicit_replace = bool(config.pop("replace", False))
    error = _validate_config(config)
    if error is not None:
        return error
    loop = asyncio.get_running_loop()
    stored = await loop.run_in_executor(None, db.get_global_device_config_raw)
    dropped = _dropped_keys(config, stored)
    if dropped and not explicit_replace:
        return _error("would_drop_keys",
                      "This body would delete existing setting(s): " + ", ".join(dropped), 409)
    await loop.run_in_executor(None, db.set_global_device_config, config)
    saved = await loop.run_in_executor(None, db.get_global_device_config)

    pushed = []
    for device_id, device in list(_devices.items()):
        if not device.online:
            continue
        effective = await loop.run_in_executor(
            None, db.get_effective_device_config, device_id)
        if await _apply_live_config(device_id, device, effective):
            pushed.append(device_id)

    all_rows = await loop.run_in_executor(None, db.get_all_devices)
    for row in all_rows:
        if row["approved"]:
            await em_ble_proxy.reconcile(row["device_id"])
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
    user = request["user"]
    body = await _json_body(request)
    current_password = _require_str(body, "current_password")
    new_password     = _require_str(body, "new_password")

    if len(new_password) < 8:
        return _error("invalid_input", "New password must be at least 8 characters", 400)

    loop = asyncio.get_event_loop()
    db_user = await loop.run_in_executor(None, db.get_user_by_id, user["id"])
    if db_user is None:
        return _error("user_not_found", "User not found", 404)

    if not await auth.verify_password_async(current_password, db_user["password_hash"]):
        return _error("invalid_credentials", "Current password is incorrect", 401)

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
        loop = asyncio.get_event_loop()
        rows = await loop.run_in_executor(None, db.get_all_devices)
        await ws.send_str(json.dumps({
            "type":    "snapshot",
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


async def _push_event(event: dict) -> None:
    """
    Broadcast a JSON event to all connected /api/events clients.

    Called by route handlers and background tasks whenever device
    state changes.
    """
    if not _event_clients:
        return
    payload = json.dumps(event)
    dead = set()
    for ws in _event_clients:
        try:
            await ws.send_str(payload)
        except Exception:
            dead.add(ws)
    _event_clients.difference_update(dead)


async def _push_log_event(
    device_id: str,
    level: str,
    source: str,
    message: str,
) -> None:
    """
    Persist a controller-generated log entry and push it to event clients.
    """
    loop = asyncio.get_event_loop()
    await loop.run_in_executor(None, db.log_device, device_id, level, source, message)
    await _push_event({
        "type":      "device_log",
        "device_id": device_id,
        "entry": {
            "ts":      int(time.time() * 1000),
            "level":   level,
            "source":  source,
            "message": message,
        },
    })


async def push_device_update(device_id: str, state: dict) -> None:
    """Broadcast a partial device JSON (same keys as _merge_device)."""
    await _push_event({"type": "device_update", "device_id": device_id, "state": state})


async def push_alerts(device_id: str, kind: str, data: dict) -> None:
    """Broadcast an AlertEngine notification (`kind` = its notify code)."""
    await _push_event({"type": "alerts", "device_id": device_id, "kind": kind, "data": data})


async def push_ha_status(features: dict) -> None:
    """Broadcast a changed em_controller.ha_status()."""
    await _push_event({"type": "ha_status", "features": features})


# ─── GitHub release fetching ──────────────────────────────────────────────────

async def _get_cached_release() -> Optional[dict]:
    """
    Return the latest release info, using the in-memory cache if fresh.
    Falls back to the DB cache if the in-memory cache is cold.
    Triggers a background fetch if the DB cache is stale.
    """
    global _release_cache, _release_cache_ts

    # In-memory cache hit
    if _release_cache and (time.monotonic() - _release_cache_ts) < RELEASE_CACHE_TTL:
        return _release_cache

    # Load from DB cache
    version = db.get_config("latest_version")
    url     = db.get_config("latest_binary_url")
    last_check = db.get_config("last_update_check")

    if version and url:
        _release_cache = {
            "version":      version,
            "url":          url,
            "notes":        db.get_config("latest_notes", "") or "",
            "release_url":  db.get_config("latest_release_url", "") or "",
            "published_at": db.get_config("latest_published_at", "") or "",
        }
        _release_cache_ts = time.monotonic()

        # If the DB cache has aged out, AWAIT the refresh rather than firing it
        # into the background and returning the stale value.
        #
        # Returning stale here is why "there's an update" showed up
        # inconsistently and why an OTA could push the previous release: the
        # caller — dashboard or update endpoint — got the old version and the
        # fresh one only landed in the cache afterwards, for whoever asked
        # next. It cost one wrong OTA (v2.9.9 pushed while v2.9.10 was
        # current, 2026-07-30).
        #
        # The cost is a single GitHub round trip, bounded by the 10s timeout in
        # _fetch_latest_release, and only on the first request after the
        # interval lapses — release_poll_loop normally refreshes ahead of any
        # caller. A failed refresh falls through to the stale cache, which is
        # better than no answer.
        interval = int(db.get_config("update_check_interval", "3600") or 3600)
        if not last_check or (time.time() - float(last_check)) > interval:
            fresh = await _fetch_latest_release()
            if fresh:
                return fresh

        return _release_cache

    # No cache at all — fetch synchronously
    return await _fetch_latest_release()


async def _fetch_latest_release(force: bool = False) -> Optional[dict]:
    """
    Poll the GitHub releases API and update the DB cache.

    Returns the release dict or None on failure.
    """
    global _release_cache, _release_cache_ts

    repo = db.get_config("github_repo", "wilbowes/EchoMuse")
    url  = GITHUB_API_URL.format(repo=repo)

    log.info(f"[api] Polling GitHub releases: {url}")
    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(
                url,
                headers={"Accept": "application/vnd.github.v3+json"},
                timeout=aiohttp.ClientTimeout(total=10),
            ) as resp:
                if resp.status != 200:
                    log.warning(f"[api] GitHub API returned {resp.status}")
                    return None
                releases = await resp.json()

        # Newest device firmware release: plain v* tag (controller releases
        # use controller-v* and ship no binary), published, with the compiled
        # `server` asset attached. The list is newest-first.
        tag = None
        binary = None
        # Initialised explicitly: it is only assigned inside the loop, and
        # while the `binary is None` return below happens to cover that today,
        # relying on one guard to protect another variable is how a later edit
        # introduces a NameError on a path nobody runs in testing.
        release: dict = {}
        for data in releases:
            if data.get("draft") or data.get("prerelease"):
                continue
            candidate_tag = data.get("tag_name", "")
            if not candidate_tag.startswith("v"):
                continue
            candidate_binary = next(
                (a for a in data.get("assets", []) if a.get("name") == "server"),
                None,
            )
            if candidate_binary is None:
                continue
            tag, binary = candidate_tag, candidate_binary
            release = data
            break

        if binary is None:
            log.warning("[api] No device firmware release with a 'server' asset found")
            return None

        download_url = binary["browser_download_url"]

        # Release notes, so the dashboard can show WHAT an update changes
        # rather than only that one exists. Deciding whether to push firmware
        # to a device you rely on, from a version number alone, is not a
        # decision — it is a guess. The body comes from the annotated tag (see
        # .github/workflows/release.yml), which is why tags are annotated.
        notes = (release.get("body") or "").strip()

        previous_tag = db.get_config("latest_version")

        # Persist to DB
        db.set_config("latest_version",    tag)
        db.set_config("latest_binary_url", download_url)
        db.set_config("latest_notes",      notes)
        db.set_config("latest_release_url", release.get("html_url") or "")
        db.set_config("latest_published_at", release.get("published_at") or "")
        db.set_config("last_update_check", str(time.time()))

        # Update in-memory cache
        _release_cache    = {
            "version":      tag,
            "url":          download_url,
            "notes":        notes,
            "release_url":  release.get("html_url") or "",
            "published_at": release.get("published_at") or "",
        }
        _release_cache_ts = time.monotonic()

        log.info(f"[api] Latest release: {tag}")
        if tag != previous_tag:
            # Tell any open dashboard, so a tab that is already showing the
            # Updates panel does not sit on the old version until someone
            # reloads or presses Check now.
            log.info(f"[api] Release changed {previous_tag or '(none)'} -> {tag}")
            await _push_event({
                "type":         "release_update",
                "version":      tag,
                "notes":        notes,
                "release_url":  release.get("html_url") or "",
                "published_at": release.get("published_at") or "",
            })
        return _release_cache

    except Exception as e:
        log.error(f"[api] GitHub release fetch failed: {e}")
        return None


async def _fetch_controller_release(force: bool = False) -> Optional[dict]:
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
    global _controller_cache, _controller_cache_ts

    if (not force and _controller_cache
            and (time.monotonic() - _controller_cache_ts) < RELEASE_CACHE_TTL):
        return _controller_cache

    repo = db.get_config("github_repo", "wilbowes/EchoMuse")
    headers = {"Accept": "application/vnd.github+json"}
    timeout = aiohttp.ClientTimeout(total=10)

    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(
                GITHUB_TAGS_URL.format(repo=repo), headers=headers, timeout=timeout
            ) as resp:
                if resp.status != 200:
                    log.warning(f"[api] Controller tag list returned {resp.status}")
                    return _controller_cache or None
                refs = await resp.json()

            newest = None
            for ref in refs or []:
                tag = (ref.get("ref") or "").removeprefix("refs/tags/")
                parsed = _parse_version(tag)
                if parsed is None:
                    continue
                if newest is None or parsed > newest[0]:
                    newest = (parsed, tag, ref.get("object") or {})

            if newest is None:
                log.info("[api] No controller-v* tags published yet")
                return None

            _, tag, obj = newest
            notes = ""
            published_at = ""
            if obj.get("type") == "tag" and obj.get("sha"):
                async with session.get(
                    GITHUB_TAG_OBJECT_URL.format(repo=repo, sha=obj["sha"]),
                    headers=headers, timeout=timeout,
                ) as resp:
                    if resp.status == 200:
                        tag_obj = await resp.json()
                        notes = (tag_obj.get("message") or "").strip()
                        published_at = (tag_obj.get("tagger") or {}).get("date", "")

        version = tag.removeprefix("controller-")
        previous = db.get_config("latest_controller_version")

        db.set_config("latest_controller_version", version)
        db.set_config("latest_controller_notes", notes)
        db.set_config("latest_controller_published_at", published_at)

        _controller_cache = {
            "version":      version,
            "current":      CONTROLLER_VERSION,
            "notes":        notes,
            "published_at": published_at,
            "release_url":  f"https://github.com/{repo}/releases/tag/{tag}",
            **_compare_versions(CONTROLLER_VERSION, version),
        }
        _controller_cache_ts = time.monotonic()

        log.info(f"[api] Latest controller release: {version} "
                 f"(running {CONTROLLER_VERSION}, {_controller_cache['status']})")
        if version != previous:
            # Same live-push as device releases: a dashboard left open should
            # not sit on stale information until someone reloads.
            await _push_event({
                "type":         "controller_update",
                "version":      version,
                "notes":        notes,
                "published_at": published_at,
                **_compare_versions(CONTROLLER_VERSION, version),
            })
        return _controller_cache

    except Exception as e:
        log.error(f"[api] Controller release fetch failed: {e}")
        return _controller_cache or None


async def _get_controller_release(request: web.Request) -> web.Response:
    """GET /api/releases/controller"""
    data = await _fetch_controller_release()
    if data is None:
        # Fall back to the DB cache so a GitHub outage does not blank the
        # panel — offline is not the same as "no update exists".
        version = db.get_config("latest_controller_version", "") or ""
        if not version:
            return _ok({"version": None, "current": CONTROLLER_VERSION,
                        "status": "unknown", "available": False})
        data = {
            "version":      version,
            "current":      CONTROLLER_VERSION,
            "notes":        db.get_config("latest_controller_notes", "") or "",
            "published_at": db.get_config("latest_controller_published_at", "") or "",
            "release_url":  "",
            **_compare_versions(CONTROLLER_VERSION, version),
        }
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


def _controller_stats() -> dict:
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
    stats: dict[str, Any] = {}
    try:
        import em_controller as _ctrl
        stats["loop_lag_peak_ms"] = round(_ctrl._loop_lag_peak_ms, 1)
    except Exception:
        pass

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
        rec_dir = em_recordings.recordings_dir()
        total = sum(f.stat().st_size for f in rec_dir.iterdir() if f.is_file())
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
    loop = asyncio.get_event_loop()
    rows = await loop.run_in_executor(None, db.get_all_devices)

    since = time.time() - 24 * 3600
    turns, metrics, counters = [], [], []
    device_configs, live_state, logs = {}, {}, []

    for row in rows:
        did = row["device_id"]
        device_configs[did] = await loop.run_in_executor(
            None, db.get_effective_device_config, did)
        device = _devices.get(did)
        live = device if device is not None and device.online else None
        hello = live.link.hello if live is not None and live.link is not None else {}
        live_state[did] = {
            "connected":        live is not None,
            "upgrade_required": bool(live is not None and live.upgrade_required),
            # Capabilities decide which HA entities and controls exist.
            "capabilities":     sorted(live.capabilities) if live is not None else [],
            # Why ambient_light is absent: no_chip, no_attribute, or ok (#90).
            "ambient_light_status": hello.get("ambient_light_status"),
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
        counters += await loop.run_in_executor(None, db.get_wake_counters, did, since)
        # Fetch deep and thin, rather than fetching 100 and shipping noise:
        # 89% of this table is [mem] heap dumps, so a flat 100 was ~11 lines
        # of evidence. Newest first, which is what thin_noise expects.
        raw = await loop.run_in_executor(None, db.get_device_logs, did, 500, None)
        pairs = [(lg["ts"], f"{lg['ts']} [{lg['level']}] {lg['source']}: {lg['message']}")
                 for lg in raw]
        # Sorted oldest-first at the end: a log someone reads should run
        # forwards, and per-device blocks in reverse order do not.
        logs += em_support.thin_noise(pairs, key=lambda p: p[1])[:DEVICE_LOG_LINES]

    bundle = em_support.build(
        controller_version=CONTROLLER_VERSION,
        devices=rows,
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
        accounts={u["username"]: u["role"] for u in
                  await loop.run_in_executor(None, db.get_all_users)},
        controller_stats=await loop.run_in_executor(None, _controller_stats),
    )
    body = em_support.to_json(bundle)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    return web.Response(
        body=body.encode(),
        content_type="application/json",
        headers={"Content-Disposition":
                 f'attachment; filename="echomuse-support-{stamp}.json"'},
    )


async def _fetch_binary(download_url: str) -> Optional[bytes]:
    """Download the binary from a GitHub release asset URL."""
    log.info(f"[api] Fetching binary: {download_url}")
    try:
        async with aiohttp.ClientSession() as session:
            async with session.get(
                download_url,
                timeout=aiohttp.ClientTimeout(total=120),
            ) as resp:
                if resp.status != 200:
                    log.error(f"[api] Binary download failed: HTTP {resp.status}")
                    return None
                return await resp.read()
    except Exception as e:
        log.error(f"[api] Binary download exception: {e}")
        return None


# ─── Periodic background tasks ────────────────────────────────────────────────

async def release_poll_loop() -> None:
    """
    Periodically poll GitHub for new releases.

    Runs as an asyncio task started from em_controller.main().
    Interval is read from system_config each iteration so it can be
    changed at runtime without restart.
    """
    # Initial delay — let the controller finish starting up
    await asyncio.sleep(30)

    while True:
        try:
            await _fetch_latest_release()
        except Exception as e:
            log.error(f"[api] Release poll loop error: {e}")

        # Same cadence, separate failure domain: a GitHub hiccup on one must
        # not cost the other its poll.
        try:
            await _fetch_controller_release(force=True)
        except Exception as e:
            log.error(f"[api] Controller release poll error: {e}")

        interval = int(db.get_config("update_check_interval", "3600") or 3600)
        await asyncio.sleep(interval)


async def session_prune_loop() -> None:
    """Prune expired sessions hourly."""
    while True:
        await asyncio.sleep(3600)
        try:
            loop = asyncio.get_event_loop()
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


async def _collect_supervisor_log(device_id: str) -> None:
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
    out = await _shell_run(live, f"busybox tail -c 4096 {SUPERVISOR_LOG}", timeout=30.0)
    text = (out or "").strip()
    if not text:
        await _push_log_event(device_id, "warn", "controller",
            "Update failed, and the device has no supervisor log — firmware "
            "predating it, or start_server.sh has not been synced yet "
            "(it takes effect on the next device reboot).")
        return
    await _push_log_event(device_id, "warn", "controller",
        "Supervisor log from the failed update:\n" + text)


async def notify_device_connected(device_id: str, version: str | None = None) -> None:
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
    event: dict = {"type": "device_connected", "device_id": device_id}
    if version is not None:
        event["firmware_ver"] = version
    else:
        loop = asyncio.get_event_loop()
        row = await loop.run_in_executor(None, db.get_device, device_id)
        if row:
            event["firmware_ver"] = row["firmware_ver"]
    await _push_event(event)

    # Owed an explanation from a failed update? Collect it now the device is
    # reachable again. Removed from the set on the way in, so a flapping
    # device cannot queue repeated fetches, and scheduled rather than awaited
    # so a slow shell never delays the connect path.
    if device_id in _supervisor_log_wanted:
        _supervisor_log_wanted.discard(device_id)

        async def _collect_soon(_id=device_id):
            # The device has just registered; give its shell plane a moment
            # before demanding a session on it.
            await asyncio.sleep(3.0)
            try:
                await _collect_supervisor_log(_id)
            except Exception as e:
                log.warning(f"[api] supervisor log fetch failed for {_id}: {e}")

        asyncio.create_task(_collect_soon())


async def notify_device_disconnected(device_id: str) -> None:
    """Called by em_controller when a device disconnects."""
    await _push_event({"type": "device_disconnected", "device_id": device_id})


async def notify_device_pending(device_id: str, ip: str) -> None:
    """Called by em_controller when an unapproved device attempts connection."""
    await _push_event({
        "type":      "device_pending",
        "device_id": device_id,
        "ip":        ip,
    })


# ─── Response helpers ─────────────────────────────────────────────────────────

def _ok(data, status: int = 200) -> web.Response:
    return web.Response(
        status=status,
        content_type="application/json",
        body=json.dumps(data),
    )


def _error(code: str, message: str, status: int) -> web.Response:
    return web.Response(
        status=status,
        content_type="application/json",
        body=json.dumps({"error": message, "code": code}),
    )


# ─── Request helpers ──────────────────────────────────────────────────────────

async def _json_body(request: web.Request) -> dict:
    """The request body as a JSON object; 400 when missing, invalid, or not an object."""
    try:
        body = await request.json()
    except Exception:
        body = None
    if not isinstance(body, dict):
        raise web.HTTPBadRequest(
            content_type="application/json",
            body=json.dumps({
                "error": "Request body must be a JSON object",
                "code":  "invalid_json",
            }),
        )
    return body


def _require_str(body: dict, key: str) -> str:
    """Extract a required string field from a parsed JSON body."""
    value = body.get(key)
    if not isinstance(value, str) or not value.strip():
        raise web.HTTPBadRequest(
            content_type="application/json",
            body=json.dumps({
                "error": f"Missing or empty required field: {key}",
                "code":  "missing_field",
            }),
        )
    return value.strip()


# ─── Device state merge ───────────────────────────────────────────────────────

def _stored_volume(row):
    """Last-known volume as an HA 0..1 float, from the persisted config."""
    try:
        level = json.loads(row["config"] or "{}").get("startupVolume")
    except (json.JSONDecodeError, TypeError):
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


def _row_sections(row) -> list:
    """
    Overridden config sections from a device row, tolerant of a row that
    predates the v8 column or carries unparseable JSON — either way the safe
    reading is "overrides nothing", which shows the device as fleet-scoped
    rather than inventing overrides it does not have.
    """
    try:
        raw = row["config_sections"]
    except (IndexError, KeyError):
        return []
    try:
        return sections_mod.normalise(json.loads(raw or "[]"))
    except (json.JSONDecodeError, TypeError):
        return []


# Actor states (em_session.SessionActor.state) the dashboard shows as listening
# or thinking; SPEAKING is shown as speaking.
_LISTENING_STATES = frozenset({"ARMED", "LISTENING", "END_PENDING", "EXPECT_REPLY"})
_THINKING_STATES = frozenset({"COMMITTED", "THINKING"})


def _merge_device(row) -> dict:
    """
    One device's dashboard JSON: the DB row plus live Device state.

    Live fields describe the current connection and are null/false/empty while
    the device is offline; `wake_stats` is the last report received (it carries
    `received_ms`), so it survives a disconnect.
    """
    device_id = row["device_id"]
    device = _devices.get(device_id)
    live = device if device is not None and device.online else None
    turn_state = live.actor.state if live is not None and live.link is not None else None
    alert_state = live.alert_state if live is not None else None
    sections = _row_sections(row)

    return {
        # Persistent
        "device_id":          device_id,
        "label":              row["label"],
        "approved":           bool(row["approved"]),
        "ip":                 row["ip"],
        "firmware_ver":       row["firmware_ver"],
        "firmware_previous":  row["firmware_previous"],
        "first_seen":         row["first_seen"],
        "last_seen":          row["last_seen"],
        "config":             json.loads(row["config"] or "{}"),
        "config_sections":    sections,
        "use_global_config":  not sections,
        "esphome_port":       row["esphome_api_port"],
        "ble_proxy_port":     row["ble_proxy_port"],
        # Live connection
        "connected":          live is not None,
        "upgrade_required":   bool(live is not None and live.upgrade_required),
        # Capability names (SPEC §11.1): a control whose capability is absent
        # is shown disabled with the reason, never as a silent no-op.
        "capabilities":       sorted(live.capabilities) if live is not None else [],
        "missing_capabilities": sorted(live.missing_capabilities) if live is not None else [],
        "firmware_version":   live.firmware_version if live is not None else None,
        "turn_state":         turn_state,
        "speaking":           turn_state == "SPEAKING",
        "listening":          turn_state in _LISTENING_STATES,
        "thinking":           turn_state in _THINKING_STATES,
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
        # A diagnostic uplink lease is held, so the device answers no wake (§4.4).
        "diagnostic":         bool(live is not None and live.diagnostic),
        "stats":              live.stats if live is not None else None,
        # Recording modes. collect/ambient are persisted device-row columns
        # (armed whether or not the device is up); counters are per connection.
        "collectMode":        bool(row["collect_mode"]),
        "collectClips":       getattr(live, "collect_clips", 0) if live is not None else 0,
        "collectLastMs":      getattr(live, "collect_last_ms", None) if live is not None else None,
        "ambientMode":        bool(row["ambient_mode"]),
        "ambientFiles":       getattr(live, "ambient_files", 0) if live is not None else 0,
        "ambientMs":          (getattr(live.ambient_rec, "duration_ms", 0)
                               if live is not None and getattr(live, "ambient_rec", None) is not None
                               else 0),
        # Script-driven capture is live-only; nothing is persisted.
        "captureMode":        bool(getattr(live, "capture_mode", False)) if live is not None else False,
        "captureDelivered":   getattr(live, "capture_delivered", 0) if live is not None else 0,
        # Controller-measured control-plane round trip.
        "rttMs":              live.rtt_last_ms if live is not None else None,
        "bleProxy":           em_ble_proxy.get_status(device_id),
        "linkTokenIssued":    bool(row["token"]),
        "linkTls":            bool(live is not None and live.secure),
        "wifi":               wifi_state(device_id),
        "update_in_progress": device_id in _updates_in_progress,
        # Last OTA/rollback failure; None when the last attempt succeeded.
        "update_error":       _update_errors.get(device_id),
    }
