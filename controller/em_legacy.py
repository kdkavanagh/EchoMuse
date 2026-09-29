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

import websockets.exceptions
from websockets.asyncio.server import ServerConnection

import em_device
import em_device_link
from em_device_link import RejectReason

log = logging.getLogger("em_legacy")
REGISTER_TIMEOUT_SECONDS = 10.0


async def serve_control(ws: ServerConnection, *, secure: bool, hub: em_device.LinkHub) -> None:
    peer_ip = em_device_link.peer_ip(ws)
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
        reported_ip = message.get("ip")
        ip = reported_ip if isinstance(reported_ip, str) else peer_ip
        reported_version = message.get("version")
        version = reported_version if isinstance(reported_version, str) else None
        result = await hub.admit_legacy(
            device_id=device_id, ip=ip, version=version, secure=secure,
            token=em_device_link.request_header(ws, em_device_link.TOKEN_HEADER),
        )
        if result.rejected is RejectReason.PENDING_APPROVAL:
            await ws.send(json.dumps({"type": "pending"}))
            await ws.close()
            return
        if result.rejected is not None:
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
