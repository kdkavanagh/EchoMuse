"""Upgrade-only handler for the old ``/control`` path (SPEC §12).

The handler authenticates and registers an old endpoint, marks it
``upgrade_required``, answers the registration handshake, and otherwise only
keeps the socket available for ``shell_open``/``shell_close`` sent by the
controller.  No report received here can change controller state or cause an
action.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
from typing import Any

import websockets.exceptions

import em_device

log = logging.getLogger("em_legacy")
REGISTER_TIMEOUT_SECONDS = 10.0


def _header(ws: Any, name: str) -> str | None:
    try:
        return ws.request.headers.get(name)
    except AttributeError:
        return None


async def serve_control(ws: Any, *, secure: bool, hub: em_device.LinkHub) -> None:
    remote = ws.remote_address
    peer_ip = str(remote[0]) if remote else ""
    try:
        raw = await asyncio.wait_for(ws.recv(), timeout=REGISTER_TIMEOUT_SECONDS)
        if not isinstance(raw, str):
            await ws.close()
            return
        message = json.loads(raw)
        if not isinstance(message, dict) or message.get("type") != "register":
            await ws.close()
            return
        device_id = message.get("device_id")
        capabilities = message.get("capabilities") or []
        if (not isinstance(device_id, str) or not device_id
                or not isinstance(capabilities, list)
                or any(not isinstance(cap, str) for cap in capabilities)):
            await ws.close()
            return
        ip = message.get("ip") if isinstance(message.get("ip"), str) else peer_ip
        version = message.get("version") if isinstance(message.get("version"), str) else None
        result = await hub.admit_legacy(
            device_id=device_id, ip=ip, version=version,
            capabilities=capabilities, secure=secure,
            token=_header(ws, "X-EM-Token"),
        )
        if result.result == "pending_approval":
            await ws.send(json.dumps({"type": "pending"}))
            await ws.close()
            return
        if result.result != "ok":
            await ws.close()
            return
        device = await hub.ensure(device_id, result.label or device_id)
        await device.attach_legacy(
            ws, ip=ip, version=version, capabilities=capabilities, secure=secure)
        await ws.send(json.dumps({"type": "ack", "device_id": device_id}))
        log.info("[%s] legacy firmware connected; upgrade required", device_id)
        try:
            async for _report in ws:
                # SPEC §12: every report from the legacy device is ignored.
                pass
        finally:
            await device.detach_legacy(ws)
    except (asyncio.TimeoutError, json.JSONDecodeError):
        await ws.close()
    except websockets.exceptions.ConnectionClosed:
        pass
    except Exception:
        log.exception("legacy control handler failed for %s", peer_ip)
        with contextlib.suppress(Exception):
            await ws.close()
