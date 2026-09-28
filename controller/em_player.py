"""Media playback sessions: the HA media_player entity's `content` source.

`play_media` URLs (Music Assistant, the media browser, radio) are decoded by
ffmpeg to 48 kHz mono int16 and streamed to the device as one `content`
playback through `RenderClient.play_stream` (SPEC §16.1 `render.*`).

Invariants:
- At most one content playback per device; each gets a fresh generation.
- The device owns focus (SPEC §6.2): it attenuates content under any dialog
  lease and pauses it under an alert foreground. This module sends nothing
  for turns.
- The bookmark is measured, not estimated: the playback's media offset plus
  the device-reported completed source frames (`render.finished`
  `last_completed_frame`, else the latest `render.progress`
  `completed_frames`).
- Pause cancels the playback and keeps the bookmark; resume starts a new
  playback whose `em_media.stream_url` decoder input-seeks (`ffmpeg -ss`) to
  it. A first-block deadline detects live streams that cannot seek.
- Deferred-command rule (SPEC §18.1): while `dialog_active(device)` holds, a
  user pause/resume/stop is recorded (last wins) and applied by
  `dialog_released`; HA is told the intended state at once. `play` is not
  deferred: new content starts attenuated under dialog focus like any other
  (SPEC §6.2) and supersedes a recorded command.
"""

from __future__ import annotations

import asyncio
import contextlib
import itertools
import logging
from typing import TYPE_CHECKING, AsyncIterator, Awaitable, Callable

import em_media

if TYPE_CHECKING:
    import numpy as np
    from em_render import Playback, RenderClient

log = logging.getLogger("player")

RATE = 48000
# A seek must yield its first block within this long, or the stream is taken
# as unseekable: ffmpeg's input seek on a live stream decodes and discards up
# to the target instead of failing, which is silence for the whole bookmark.
SEEK_STALL_S = 5.0
CANCEL_WAIT_S = 2.0        # render.cancel → render.finished before using last progress

IDLE, PLAYING, PAUSED = "idle", "playing", "paused"
PAUSE, RESUME, STOP = "pause", "resume", "stop"

_get_render: Callable[[str], "RenderClient | None"] = lambda _device_id: None
_notify_state: Callable[[str, str], Awaitable[None]] | None = None
_dialog_active: Callable[[str], bool] = lambda _device_id: False

_sessions: dict[str, "MediaSession"] = {}
_generations = itertools.count(1)
_background: set[asyncio.Task] = set()


def init(get_render: Callable[[str], "RenderClient | None"],
         notify_state: Callable[[str, str], Awaitable[None]] | None,
         dialog_active: Callable[[str], bool]) -> None:
    """Inject the device's render client lookup, the HA state push, and the
    dialog-focus query (any dialog lease held on that device)."""
    global _get_render, _notify_state, _dialog_active
    _get_render = get_render
    _notify_state = notify_state
    _dialog_active = dialog_active


def _session(device_id: str) -> "MediaSession":
    s = _sessions.get(device_id)
    if s is None:
        s = _sessions[device_id] = MediaSession(device_id)
    return s


def state(device_id: str) -> str:
    s = _sessions.get(device_id)
    return s.state if s else IDLE


def is_playing(device_id: str) -> bool:
    return state(device_id) == PLAYING


def reported_state(device_id: str) -> str:
    """The state HA should show: a deferred command's intended outcome, else
    the actual state. Ducked content is still playing."""
    s = _sessions.get(device_id)
    return s.intended_state() if s else IDLE


async def play(device_id: str, url: str) -> None:
    await _session(device_id).play(url)


async def pause(device_id: str) -> None:
    await _command(device_id, PAUSE)


async def resume(device_id: str) -> None:
    await _command(device_id, RESUME)


async def stop(device_id: str) -> None:
    await _command(device_id, STOP)


async def dialog_released(device_id: str) -> None:
    """Dialog focus ended: apply the user command recorded during it, if any."""
    s = _sessions.get(device_id)
    if s is None or s.deferred is None:
        return
    command, s.deferred = s.deferred, None
    await s.run(command)


def device_gone(device_id: str) -> None:
    """The device's session is gone: drop its playback without wire traffic."""
    s = _sessions.pop(device_id, None)
    if s is not None:
        s.abandon()


async def _command(device_id: str, command: str) -> None:
    s = _session(device_id)
    if _dialog_active(device_id):
        s.deferred = command
        log.info(f"[{device_id}] Media {command} deferred until dialog focus ends")
        await s.push_state()
        return
    await s.run(command)


def _spawn(coro) -> None:
    task = asyncio.ensure_future(coro)
    _background.add(task)
    task.add_done_callback(_background.discard)


async def _cancel_quietly(playback: "Playback", reason: str) -> None:
    try:
        await playback.cancel(reason)
    except Exception as e:
        log.warning(f"render.cancel {playback.playback_id} failed: {e}")


async def _prepend(first: np.ndarray, rest: AsyncIterator[np.ndarray]) -> AsyncIterator[np.ndarray]:
    try:
        yield first
        async for block in rest:
            yield block
    finally:
        await rest.aclose()


async def _start_playback(render: "RenderClient", source: AsyncIterator[np.ndarray]) -> "Playback":
    """`play_stream('content')` that never leaks a device playback: if the
    caller is cancelled while it is in flight, the playback is cancelled as
    soon as it exists."""
    started = asyncio.ensure_future(
        render.play_stream("content", source, generation=next(_generations)))

    def reap(fut: asyncio.Future) -> None:
        if fut.cancelled():
            return
        if fut.exception() is not None:
            _spawn(source.aclose())
        else:
            _spawn(_cancel_quietly(fut.result(), "stopped"))

    try:
        return await asyncio.shield(started)
    except asyncio.CancelledError:
        started.add_done_callback(reap)
        raise
    except Exception:
        await source.aclose()
        raise


def _completed_frames(playback: "Playback", finished: dict | None) -> int | None:
    """Source frames the device reports as rendered, or None if unknown."""
    if finished is not None and finished.get("last_completed_frame") is not None:
        return int(finished["last_completed_frame"])
    if playback.last_progress is not None:
        return playback.completed_frames
    return None


class MediaSession:
    """One device's content playback. Commands are serialized by `_lock`;
    `_task` starts the current playback and settles its `render.finished`."""

    def __init__(self, device_id: str):
        self.device_id = device_id
        self.state = IDLE
        self.url: str | None = None
        self.position_s = 0.0              # media seconds: bookmark / current playback start
        self.deferred: str | None = None   # PAUSE | RESUME | STOP recorded under dialog focus
        self._seekable = True              # learned per URL; an unseekable resume rejoins the live edge
        self._base_s = 0.0                 # media position of the current playback's frame 0
        self._playback: Playback | None = None
        self._task: asyncio.Task | None = None
        self._lock = asyncio.Lock()

    def intended_state(self) -> str:
        if self.deferred == STOP:
            return IDLE
        if self.deferred == PAUSE and self.state != IDLE:
            return PAUSED
        if self.deferred == RESUME and self.state != IDLE:
            return PLAYING
        return self.state

    async def run(self, command: str) -> None:
        await {PAUSE: self.pause, RESUME: self.resume, STOP: self.stop}[command]()

    # ── commands ──────────────────────────────────────────────────────────

    async def play(self, url: str) -> None:
        async with self._lock:
            self.deferred = None
            await self._halt("replaced")
            self.url = url
            self.position_s = 0.0
            self._seekable = True
            await self._begin()

    async def pause(self) -> None:
        async with self._lock:
            if self.state != PLAYING:
                return
            finished = await self._halt("paused")
            if finished is not None and finished.get("reason") != "cancelled":
                await self._finish(finished.get("reason"))
                return
            self.state = PAUSED
            log.info(f"[{self.device_id}] Media paused at {self.position_s:.2f}s")
            await self.push_state()

    async def resume(self) -> None:
        async with self._lock:
            if self.state != PAUSED or self.url is None:
                return
            await self._begin()

    async def stop(self) -> None:
        async with self._lock:
            was_active = self.state != IDLE
            await self._halt("stopped")
            self.state = IDLE
            self.url = None
            self.position_s = 0.0
            self.deferred = None
            if was_active:
                log.info(f"[{self.device_id}] Media stopped")
                await self.push_state()

    def abandon(self) -> None:
        """Drop the playback without wire traffic (the render client already
        failed it with the session)."""
        task, self._task = self._task, None
        self._playback = None
        if task is not None:
            task.cancel()
        self.state = IDLE
        self.deferred = None

    async def push_state(self) -> None:
        if _notify_state is None:
            return
        try:
            await _notify_state(self.device_id, self.intended_state())
        except Exception as e:
            log.warning(f"[{self.device_id}] media state push failed: {e}")

    # ── playback lifecycle ────────────────────────────────────────────────

    async def _begin(self) -> None:
        render = _get_render(self.device_id)
        if render is None:
            log.warning(f"[{self.device_id}] Media play: device has no render session")
            self.state = IDLE
            self.url = None
            self.position_s = 0.0
            await self.push_state()
            return
        self.state = PLAYING
        self._task = asyncio.create_task(self._run(render, self.url, self.position_s))

    async def _halt(self, reason: str) -> dict | None:
        """End the current playback; bookmark what the device rendered.
        Returns its `render.finished` body, or None if there was no playback
        or the device did not answer within CANCEL_WAIT_S."""
        task, self._task = self._task, None
        playback, self._playback = self._playback, None
        finished = None
        if playback is not None:
            if not playback.finished.done():
                await _cancel_quietly(playback, reason)
            try:
                finished = await asyncio.wait_for(asyncio.shield(playback.finished), CANCEL_WAIT_S)
            except TimeoutError:
                log.warning(f"[{self.device_id}] no render.finished {CANCEL_WAIT_S:.0f}s after cancel")
            frames = _completed_frames(playback, finished)
            if frames is not None:
                self.position_s = self._base_s + frames / RATE
        if task is not None and not task.done():
            task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await task
        return finished

    async def _open(self, url: str, start_s: float) -> tuple[AsyncIterator[np.ndarray] | None, float]:
        """Decoder source from `start_s`, and the media position its first
        frame really has. None when the media ends at or before `start_s`."""
        if start_s > 0 and not self._seekable:
            log.info(f"[{self.device_id}] Stream is not seekable: resuming at the live edge")
            start_s = 0.0
        blocks = em_media.stream_url(url, rate=RATE, start_s=start_s)
        if start_s <= 0:
            return blocks, 0.0
        try:
            first = await asyncio.wait_for(anext(blocks), SEEK_STALL_S)
        except TimeoutError:
            await blocks.aclose()
            log.warning(f"[{self.device_id}] No audio {SEEK_STALL_S:.0f}s after seeking to "
                        f"{start_s:.1f}s: stream is not seekable; rejoining the live edge")
            self._seekable = False
            return em_media.stream_url(url, rate=RATE, start_s=0.0), 0.0
        except StopAsyncIteration:
            return None, start_s
        except BaseException:
            await blocks.aclose()
            raise
        return _prepend(first, blocks), start_s

    async def _run(self, render: "RenderClient", url: str, start_s: float) -> None:
        me = asyncio.current_task()
        try:
            source, start_s = await self._open(url, start_s)
            self._base_s = self.position_s = start_s
            if source is None:
                self._task = None
                await self._finish("drained")
                return
            playback = await _start_playback(render, source)
            self._playback = playback
            log.info(f"[{self.device_id}] Media playing {url!r} from {start_s:.2f}s "
                     f"(playback {playback.playback_id}, generation {playback.generation})")
            await self.push_state()
            finished = await asyncio.shield(playback.finished)
        except asyncio.CancelledError:
            raise
        except Exception as e:
            if self._task is me:
                log.error(f"[{self.device_id}] Media playback error: {e}")
                self._task = None
                self._playback = None
                await self._finish("failed")
            return
        if self._playback is not playback:
            return
        self._task = None
        self._playback = None
        frames = _completed_frames(playback, finished)
        if frames is not None:
            self.position_s = self._base_s + frames / RATE
        await self._finish(finished.get("reason"))

    async def _finish(self, reason: str | None) -> None:
        """The playback ended by itself (or could not start): idle."""
        if reason == "drained":
            log.info(f"[{self.device_id}] Media finished at {self.position_s:.2f}s")
        else:
            log.warning(f"[{self.device_id}] Media playback ended ({reason}) at {self.position_s:.2f}s")
        self.state = IDLE
        self.url = None
        self.position_s = 0.0
        self.deferred = None
        await self.push_state()
