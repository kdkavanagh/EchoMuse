"""Shell-plane rendezvous between API sessions and device-dialled `/shell` sockets.

A shell session is requested over the device link (`shell_open`) and arrives
as a separate socket the device dials back to `/shell/{device_id}`. The two
halves meet here: em_api registers a `ShellRequest` before asking, and
em_controller's `handle_shell` claims it when the socket arrives.

A request is programmatic (em_api's `_device_shell`: handed the socket) or
interactive (the dashboard terminal: `handle_shell` proxies the socket to the
dashboard's WebSocket and ends the request when the proxy stops). Programmatic
sessions are serialised per device by `lock`; the interactive terminal
deliberately never takes it and only refuses to start while it is held.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
from typing import Protocol

from aiohttp import web


class ShellConnection(Protocol):
    """The device-dialled `/shell` socket a programmatic session is handed
    (a websockets ServerConnection)."""

    async def send(self, message: str, /) -> None: ...
    async def recv(self) -> str | bytes: ...
    async def close(self) -> None: ...


@dataclass(frozen=True, slots=True, eq=False)
class ShellRequest:
    """One requested session, waiting for the device to dial `/shell`.

    `dashboard` is the socket an interactive session is proxied to; None
    marks a programmatic one. The future resolves with the dialled socket
    (programmatic) or with None once the session ends (interactive).
    """

    future: asyncio.Future[ShellConnection | None]
    dashboard: web.WebSocketResponse | None

    @property
    def open(self) -> bool:
        """Still answerable: not yet answered, ended or abandoned."""
        return not self.future.done()

    def answer(self, conn: ShellConnection) -> None:
        """Hand a programmatic requester its socket."""
        self.future.set_result(conn)

    def end(self) -> None:
        """The session is over (idempotent)."""
        if not self.future.done():
            self.future.set_result(None)

    async def wait(self, timeout: float | None = None) -> ShellConnection | None:
        """The answered socket, or None once the session ended. On timeout the
        request is cancelled, so a late dial finds it closed."""
        return await asyncio.wait_for(self.future, timeout=timeout)


class ShellBroker:
    """The per-device shell rendezvous: at most one registered request each."""

    def __init__(self) -> None:
        self._requests: dict[str, ShellRequest] = {}
        self._locks: dict[str, asyncio.Lock] = {}

    def request(self, device_id: str,
                dashboard: web.WebSocketResponse | None = None) -> ShellRequest:
        """Register a session request, replacing any earlier one."""
        req = ShellRequest(asyncio.get_running_loop().create_future(), dashboard)
        self._requests[device_id] = req
        return req

    def claim(self, device_id: str) -> ShellRequest | None:
        """The open request a dialled shell answers, if any. It stays
        registered until its requester releases it."""
        req = self._requests.get(device_id)
        return req if req is not None and req.open else None

    def release(self, device_id: str, req: ShellRequest) -> None:
        """Drop `req` — only if it is still the registered one, so a finished
        session never unregisters a newer requester's."""
        if self._requests.get(device_id) is req:
            del self._requests[device_id]

    def lock(self, device_id: str) -> asyncio.Lock:
        """The lock serialising programmatic sessions on one device."""
        lock = self._locks.get(device_id)
        if lock is None:
            lock = self._locks[device_id] = asyncio.Lock()
        return lock

    def busy(self, device_id: str) -> bool:
        """A programmatic session holds the device's shell."""
        lock = self._locks.get(device_id)
        return lock is not None and lock.locked()
