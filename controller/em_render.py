"""Controller-side render client: playbacks on the device mixer (§4.4, §16.1;
WIRE §4.2).

Network sources (`content`, `dialog_output`) stream kind-3 packets on the
audio socket; local sources (`earcon`, `alert_preview`) name a device asset.

Invariants:
- Kind-3 audio for a playback is sent only after the device accepted its
  `render.start` (control and audio are separate sockets, so an earlier
  packet could name an epoch the device does not know yet).
- Frames sent minus the device's `completed_frames` never exceed the device
  FIFO minus one packet (§4.4: 128 writes × 2,048 frames).
- `render.progress`/`render.finished` whose envelope generation is not the
  playback's are dropped (generation fencing).
- `finished` resolves exactly once; a playback that ends before it started
  fails `started` with `PlaybackEnded`.
"""

from __future__ import annotations

import asyncio
import enum
import logging
import secrets
import uuid
from collections.abc import Mapping
from typing import AsyncIterator, Callable, NotRequired, Protocol, TypedDict

import numpy as np

import em_audio_timeline as tl
from em_device_link import AckStatus, CommandAck, DeviceLink, Envelope, LinkClosed, MessageType

log = logging.getLogger("em_render")


class SourceClass(enum.StrEnum):
    """`render.start` source classes (WIRE §4.2)."""

    CONTENT = "content"
    DIALOG_OUTPUT = "dialog_output"
    EARCON = "earcon"
    ALERT_PREVIEW = "alert_preview"


class FinishReason(enum.StrEnum):
    """`render.finished` reasons (WIRE §4.2)."""

    DRAINED = "drained"
    CANCELLED = "cancelled"
    FAILED = "failed"
    UNDERRUN = "underrun"


NETWORK_SOURCES = frozenset({SourceClass.CONTENT, SourceClass.DIALOG_OUTPUT})
LOCAL_SOURCES = frozenset({SourceClass.EARCON, SourceClass.ALERT_PREVIEW})
PACKET_FRAMES = tl.MAX_FRAMES[tl.Kind.RENDER]      # 3,840 frames = 80 ms at 48 kHz
FIFO_FRAMES = 128 * 2_048                          # device FIFO, §4.4
MAX_IN_FLIGHT = FIFO_FRAMES - PACKET_FRAMES        # sent − completed bound
START_ACK_TIMEOUT_S = 2.0


class RenderFinished(TypedDict):
    """How a playback ended: the device's `render.finished` body, or the same shape for a
    playback that ended locally (`detail` says why)."""

    playback_id: str
    last_completed_frame: str | None     # u64 decimal string
    timing_quality: str | None
    reason: FinishReason
    detail: NotRequired[str]


class StreamFilter(Protocol):
    """A fresh stateful filter over S16_LE mono PCM (`em_eq.StreamingEQ`)."""

    def process(self, pcm: bytes) -> bytes: ...


class _RenderStartBody(TypedDict):
    playback_id: str
    source_class: SourceClass
    epoch: NotRequired[str]              # network sources only
    gain_db: float
    format: int
    local_asset: str | None
    announcement: bool


class PlaybackEnded(Exception):
    """Set on `started` when the playback finished without starting."""

    def __init__(self, finished: RenderFinished):
        super().__init__(f"playback ended before start: {finished['reason']}")
        self.finished = finished


def _new_epoch() -> int:
    while True:
        epoch = secrets.randbits(64)
        if epoch:
            return epoch


class Playback:
    """One `render.start` on the device."""

    def __init__(self, client: RenderClient, source_class: SourceClass, generation: int):
        loop = asyncio.get_running_loop()
        self.playback_id = str(uuid.uuid4())
        self.generation = generation
        self.source_class = source_class
        self.started: asyncio.Future[None] = loop.create_future()
        self.finished: asyncio.Future[RenderFinished] = loop.create_future()
        self.last_progress: Mapping[str, object] | None = None
        self.completed_frames = 0
        self.sent_frames = 0          # frames of a network source sent to the device
        self._client = client
        self._start_sent = False
        self._cancel_sent = False
        self._progressed = asyncio.Event()
        self._pump: asyncio.Task[object] | None = None

    def __repr__(self) -> str:
        return f"<Playback {self.source_class} {self.playback_id} gen={self.generation}>"

    @property
    def done(self) -> bool:
        return self.finished.done()

    async def cancel(self, reason: str) -> None:
        """Idempotent `render.cancel`. `finished` resolves on the device's
        `render.finished` (`cancelled`), or at once if nothing reached it."""
        if self.done or self._cancel_sent:
            return
        self._cancel_sent = True
        self._stop_pump()
        if not self._start_sent:
            self._finish(FinishReason.CANCELLED)
            return
        try:
            await self._client._send(MessageType.RENDER_CANCEL,
                                     {"playback_id": self.playback_id, "reason": reason}, self)
        except LinkClosed:
            self._finish(FinishReason.FAILED, "session lost")

    def _stop_pump(self) -> None:
        if self._pump is not None and self._pump is not asyncio.current_task():
            self._pump.cancel()

    def _result(self, reason: FinishReason) -> RenderFinished:
        """The finish record as far as the controller knows it: progress it saw, no timing quality."""
        return RenderFinished(
            playback_id=self.playback_id,
            last_completed_frame=(tl.format_u64(self.completed_frames)
                                  if self.last_progress is not None else None),
            timing_quality=None,
            reason=reason,
        )

    def _finish(self, reason: FinishReason, detail: str | None = None) -> None:
        """End locally: nothing more will come from the device."""
        result = self._result(reason)
        if detail is not None:
            result["detail"] = detail
        self._settle(result)

    def _on_finished(self, body: Mapping[str, object]) -> None:
        """The device's `render.finished`; its fields override what the controller saw."""
        reason = body.get("reason")
        if not isinstance(reason, str) or reason not in FinishReason:
            log.warning(f"{self}: render.finished with reason {reason!r}: counted as failed")
            reason = FinishReason.FAILED
        result = self._result(FinishReason(reason))
        if "last_completed_frame" in body:
            frame = body["last_completed_frame"]
            result["last_completed_frame"] = frame if isinstance(frame, str) else None
        if "timing_quality" in body:
            quality = body["timing_quality"]
            result["timing_quality"] = quality if isinstance(quality, str) else None
        detail = body.get("detail")
        if isinstance(detail, str):
            result["detail"] = detail
        self._settle(result)

    def _settle(self, result: RenderFinished) -> None:
        if self.done:
            return
        self.finished.set_result(result)
        if not self.started.done():
            self.started.set_exception(PlaybackEnded(result))
            self.started.exception()     # retrieved: awaiting it is optional
        self._progressed.set()
        self._stop_pump()
        self._client._forget(self)

    def _on_progress(self, body: Mapping[str, object]) -> None:
        completed = body.get("completed_frames")
        if completed is not None:
            try:
                self.completed_frames = max(self.completed_frames,
                                            tl.parse_u64(completed, "completed_frames"))
            except tl.ProtocolError as err:
                log.warning(f"{self}: bad progress: {err}")
        self.last_progress = body
        if body.get("event") == "start" and not self.started.done():
            self.started.set_result(None)
        self._progressed.set()


class RenderClient:
    """Playbacks on one `DeviceLink`. Create a new one per session."""

    def __init__(self, link: DeviceLink, *, eq: Callable[[], StreamFilter | None] = lambda: None):
        """`eq()` is called once per network playback and returns a fresh
        stateful filter with `process(bytes) -> bytes` (`em_eq.StreamingEQ`)
        or None."""
        self._link = link
        self._eq = eq
        self._playbacks: dict[str, Playback] = {}
        # message_id → playback, for this client's render.end/render.cancel acks.
        self._own_messages: dict[str, Playback] = {}

    @property
    def playbacks(self) -> tuple[Playback, ...]:
        return tuple(self._playbacks.values())

    async def _send(self, msg_type: MessageType, body: Mapping[str, object], playback: Playback) -> None:
        mid = await self._link.send(msg_type, body, generation=playback.generation)
        self._own_messages[mid] = playback

    def _forget(self, playback: Playback) -> None:
        self._playbacks.pop(playback.playback_id, None)

    def _register(self, source_class: SourceClass, generation: int) -> Playback:
        if not 0 <= generation <= tl.U32_MAX:
            raise ValueError(f"generation {generation} is not a uint32")
        playback = Playback(self, source_class, generation)
        self._playbacks[playback.playback_id] = playback
        return playback

    async def play_stream(self, source_class: SourceClass, pcm48: AsyncIterator[np.ndarray], *,
                          generation: int, gain_db: float = 0.0,
                          announcement: bool = False) -> Playback:
        """Stream 48 kHz mono int16 audio as a network source. Every failure,
        including a source exception, resolves `finished` with `failed`."""
        if source_class not in NETWORK_SOURCES:
            raise ValueError(f"{source_class!r} is not a network source")
        playback = self._register(source_class, generation)
        epoch = _new_epoch()
        start = _RenderStartBody(
            playback_id=playback.playback_id,
            source_class=source_class,
            epoch=tl.format_u64(epoch),
            gain_db=float(gain_db),
            format=tl.FORMAT_PCM16,
            local_asset=None,
            announcement=bool(announcement),
        )
        playback._pump = asyncio.create_task(
            self._pump(playback, start, epoch, pcm48), name=f"render-{playback.playback_id}")
        return playback

    async def play_local(self, source_class: SourceClass, asset: str, *, generation: int,
                         gain_db: float = 0.0) -> Playback:
        """Play a device-local asset (`builtin:*` or a SHA-256)."""
        if source_class not in LOCAL_SOURCES:
            raise ValueError(f"{source_class!r} is not a local source")
        playback = self._register(source_class, generation)
        start = _RenderStartBody(
            playback_id=playback.playback_id,
            source_class=source_class,
            gain_db=float(gain_db),
            format=tl.FORMAT_PCM16,
            local_asset=asset,
            announcement=False,
        )
        playback._pump = asyncio.create_task(
            self._start(playback, start), name=f"render-{playback.playback_id}")
        return playback

    async def _start(self, playback: Playback, body: _RenderStartBody) -> bool:
        """Send `render.start`; True once the device accepted it."""
        playback._start_sent = True
        try:
            ack = await self._link.request(MessageType.RENDER_START, body, generation=playback.generation,
                                           timeout=START_ACK_TIMEOUT_S)
        except LinkClosed:
            playback._finish(FinishReason.FAILED, "session lost")
            return False
        except TimeoutError:
            await self._abort(playback, "render.start not acknowledged")
            return False
        if ack.status == AckStatus.REJECTED:
            playback._finish(FinishReason.FAILED, f"rejected: {ack.error}")
            return False
        return True

    async def _abort(self, playback: Playback, detail: str) -> None:
        """Local failure after `render.start`: cancel on the device, fail now."""
        log.warning(f"{playback}: {detail}")
        playback._cancel_sent = True
        playback._finish(FinishReason.FAILED, detail)
        try:
            await self._send(MessageType.RENDER_CANCEL,
                             {"playback_id": playback.playback_id, "reason": "failed"}, playback)
        except LinkClosed:
            pass

    async def _pump(self, playback: Playback, start: _RenderStartBody, epoch: int,
                    source: AsyncIterator[np.ndarray]) -> None:
        eq = self._eq()
        try:
            if not await self._start(playback, start):
                return
            sequence = 0
            sent = 0
            pending = np.empty(0, dtype=np.int16)
            eof = False
            while not eof or pending.size:
                if not eof and pending.size < PACKET_FRAMES:
                    try:
                        chunk = await anext(source)
                    except StopAsyncIteration:
                        eof = True
                        continue
                    chunk = np.asarray(chunk)
                    if chunk.dtype != np.int16 or chunk.ndim != 1:
                        raise TypeError(f"source yielded {chunk.dtype} {chunk.shape}, want 1-D int16")
                    if eq is not None and chunk.size:
                        chunk = np.frombuffer(eq.process(chunk.tobytes()), dtype=np.int16)
                    pending = np.concatenate((pending, chunk)) if pending.size else chunk
                    continue
                n = min(PACKET_FRAMES, pending.size)
                while sent + n - playback.completed_frames > MAX_IN_FLIGHT:
                    playback._progressed.clear()
                    await playback._progressed.wait()
                    if playback.done:
                        return
                frame = tl.build_packet(tl.Kind.RENDER, epoch=epoch, sequence=sequence,
                                        first_sample=sent, pcm=pending[:n],
                                        generation=playback.generation)
                pending = pending[n:]
                await self._link.send_audio(frame)
                sequence += 1
                sent += n
                playback.sent_frames = sent
            # The tail may still be in flight on the audio socket when render.end
            # arrives on the control socket: end_frame says how much audio to wait for.
            await self._send(MessageType.RENDER_END, {"playback_id": playback.playback_id,
                                                      "end_frame": tl.format_u64(sent)}, playback)
        except asyncio.CancelledError:
            raise
        except LinkClosed:
            playback._finish(FinishReason.FAILED, "session lost")
        except Exception as err:
            await self._abort(playback, f"source failed: {err!r}")
        finally:
            aclose = getattr(source, "aclose", None)
            if aclose is not None:
                try:
                    await aclose()
                except Exception:
                    log.exception(f"{playback}: closing the source failed")

    def on_message(self, envelope: Envelope) -> bool:
        """Consume `render.progress`/`render.finished` and acks of this
        client's own render commands. True if consumed."""
        body = envelope.body
        if envelope.type == MessageType.COMMAND_ACK:
            ack = CommandAck.parse(body)
            playback = self._own_messages.pop(ack.message_id, None)
            if playback is None:
                return False
            if ack.status == AckStatus.REJECTED:
                # The device holds no such playback (its render.start never
                # arrived or it already ended): nothing more will be reported.
                log.info(f"{playback}: render command rejected: {ack.error}")
                playback._finish(FinishReason.CANCELLED if playback._cancel_sent else FinishReason.FAILED,
                                 f"rejected: {ack.error}")
            return True
        if envelope.type not in (MessageType.RENDER_PROGRESS, MessageType.RENDER_FINISHED):
            return False
        playback_id = body.get("playback_id")
        playback = self._playbacks.get(playback_id) if isinstance(playback_id, str) else None
        if playback is None or envelope.generation != playback.generation:
            return True
        if envelope.type == MessageType.RENDER_PROGRESS:
            playback._on_progress(body)
        else:
            playback._on_finished(body)
        return True

    def fail_all(self, reason: str) -> None:
        """Session lost: every open playback finishes `failed`."""
        for playback in list(self._playbacks.values()):
            playback._finish(FinishReason.FAILED, reason)
        self._own_messages.clear()
