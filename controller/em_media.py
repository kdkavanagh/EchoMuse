"""Audio decode for render: URLs to 48 kHz mono int16 via ffmpeg.

`stream_url` serves HA TTS URLs (unauthenticated capability URLs that expire
after ~300 s: fetched immediately, never cached, §16.7) and music. The HTTP
response is piped through one ffmpeg process and PCM is yielded as it is
decoded; neither the encoded response nor the decoded audio is accumulated.
"""

from __future__ import annotations

import asyncio
import contextlib
import logging
from collections.abc import AsyncGenerator

import numpy as np

log = logging.getLogger("em_media")

RATE = 48_000
READ_BYTES = 16 * 1024
CONNECT_TIMEOUT_S = 10
# HA's tts_proxy synthesises while the GET is open, so a read may wait for
# synthesis, not just the network.
READ_TIMEOUT_S = 60
FFMPEG_EXIT_TIMEOUT_S = 15
RETRY_DELAY_S = 0.5


def _ffmpeg_args(source: str, rate: int, start_s: float) -> list[str]:
    args = ["ffmpeg", "-nostdin", "-hide_banner", "-loglevel", "error"]
    if start_s > 0:
        args += ["-ss", f"{start_s:.3f}"]
    return args + ["-i", source, "-f", "s16le", "-acodec", "pcm_s16le",
                   "-ar", str(rate), "-ac", "1", "pipe:1"]


def _ffmpeg_error(stderr: bytes, returncode: int | None) -> RuntimeError:
    tail = stderr.decode("utf-8", "replace").strip().splitlines()
    return RuntimeError(f"ffmpeg: {tail[-1] if tail else f'exit {returncode}'}")


async def stream_url(url: str, *, rate: int = RATE, start_s: float = 0.0) -> AsyncGenerator[np.ndarray, None]:
    """Fetch `url` and yield decoded mono int16 chunks at `rate`.

    One retry, only before any PCM was yielded (a later retry would repeat
    audio already played). `start_s` skips into the source (ffmpeg input
    seek). Raises on HTTP or decode failure; closing the generator kills
    ffmpeg and the request before `aclose()` returns.
    """
    emitted = False
    for attempt in range(2):
        try:
            async with contextlib.aclosing(_stream_once(url, rate, start_s)) as chunks:
                async for pcm in chunks:
                    emitted = True
                    yield pcm
            return
        except Exception as err:
            if attempt or emitted:
                raise
            log.warning(f"stream_url: failed before audio ({err}); retrying once")
            await asyncio.sleep(RETRY_DELAY_S)


async def _stream_once(url: str, rate: int, start_s: float) -> AsyncGenerator[np.ndarray, None]:
    import aiohttp

    proc: asyncio.subprocess.Process | None = None
    tasks: list[asyncio.Task[object]] = []
    timeout = aiohttp.ClientTimeout(total=None, connect=CONNECT_TIMEOUT_S,
                                    sock_connect=CONNECT_TIMEOUT_S, sock_read=READ_TIMEOUT_S)
    try:
        async with aiohttp.ClientSession(timeout=timeout) as session:
            async with session.get(url) as resp:
                resp.raise_for_status()
                proc = await asyncio.create_subprocess_exec(
                    *_ffmpeg_args("pipe:0", rate, start_s),
                    stdin=asyncio.subprocess.PIPE,
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                if proc.stdin is None or proc.stdout is None or proc.stderr is None:
                    raise RuntimeError("ffmpeg: subprocess pipes were not opened")

                async def feed(sink: asyncio.StreamWriter) -> None:
                    try:
                        async for chunk in resp.content.iter_chunked(READ_BYTES):
                            sink.write(chunk)
                            await sink.drain()
                    except (BrokenPipeError, ConnectionResetError):
                        pass    # ffmpeg exited; its exit status reports why
                    finally:
                        if not sink.is_closing():
                            sink.close()

                feeder: asyncio.Task[None] = asyncio.create_task(feed(proc.stdin))
                # stderr drains concurrently: a full stderr pipe would stall
                # ffmpeg's stdout, and malformed input is what fills it.
                stderr: asyncio.Task[bytes] = asyncio.create_task(proc.stderr.read())
                tasks += [feeder, stderr]
                async for pcm in _pcm_chunks(proc.stdout):
                    yield pcm
                await feeder
                err = await stderr
                async with asyncio.timeout(FFMPEG_EXIT_TIMEOUT_S):
                    code = await proc.wait()
                if code != 0:
                    raise _ffmpeg_error(err, code)
    finally:
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        if proc is not None and proc.returncode is None:
            proc.kill()
            await proc.wait()


async def _pcm_chunks(stdout: asyncio.StreamReader) -> AsyncGenerator[np.ndarray, None]:
    """Whole int16 samples from a byte stream; an odd byte waits for its pair."""
    carry = b""
    while data := await stdout.read(READ_BYTES):
        data = carry + data
        whole = len(data) & ~1
        carry = data[whole:]
        if whole:
            yield np.frombuffer(data[:whole], dtype="<i2").astype(np.int16)
