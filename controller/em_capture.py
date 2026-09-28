"""
em_capture.py — script-driven recording windows, pushed to a webhook
=====================================================================

Sample collection (`em_samples`) records an unattended room and has to guess
where the speech is, so it cuts at the silences. This module is for the
opposite situation: something *else* knows exactly when the audio starts,
because it is the thing playing it.

The case it was built for is re-recording a corpus through the device. Common
Voice on disk is clean studio speech; the same clips played from a media
player in a real room and captured off the Echo's own mic array carry the
array, the HAL's beam selection and gain, the room, the distance and the
speaker —
every part of the path the wake model is scored on in service, and none of
which augmentation can invent. Multiplied across a matrix of players and
volumes, one corpus becomes many.

**A segmenter cannot do that job, and it is worth saying why rather than
finding out.** Run a 5s Common Voice clip through `em_samples.Segmenter` and
what comes out is nondeterministic: `silence_ms=400` splits the sentence at
its internal pauses into two or three clips, `max_clip_ms=6000` truncates
anything longer and then refuses to reopen until the room is quiet, and a
clip played at the bottom of a volume matrix may never clear
`open_margin_db` at all and produce nothing. One playback becomes 0..N files
with no way for the caller to know which it got. A driver script that must
pair each recording with the file it played needs exactly one file per
playback, so the window is opened and closed by the caller and the audio is
handed back whole.

Trimming is deliberately NOT done here. The caller has ffmpeg and the source
clip to compare against; the controller has neither, and a trim rule baked in
at this end is one the caller cannot change without a redeploy.

What this module is: a buffer with a cap, a peak/floor measurement, and the
metadata that lets a caller correlate. Pure — no aiohttp, no db, no numpy,
and the caller passes the frame RMS it has already computed, exactly as
`em_samples` does. The transport and the mode's lifecycle live in
`em_controller`; the routes live in `em_api`.
"""

from __future__ import annotations

import logging
import time
import uuid
from collections import OrderedDict
from dataclasses import dataclass
from urllib.parse import urlparse

from em_samples import (          # one definition of the wire format
    DB_FLOOR,
    db_of,
    duration_ms,
    encode_wav,
)

log = logging.getLogger("echomuse.capture")

__all__ = [
    "MAX_WINDOW_MS", "DEFAULT_WINDOW_MS", "DEFAULT_IDLE_S", "MAX_IDLE_S",
    "WINDOW_GRACE_MS", "MAX_TAG_LEN",
    "CaptureResult", "Window", "ResultStore",
    "clamp_max_ms", "clamp_idle_s", "clean_tag",
    "valid_webhook", "headers", "decide_idle_expiry", "window_overdue",
]

# Hard ceiling on a window, whatever was asked for. 30s of the wire format is
# ~960kB held in memory per device — comfortable, and small enough that a
# caller looping on a bad tag cannot grow a buffer without bound. It is also
# well past any single corpus clip: Common Voice tops out around 10s.
MAX_WINDOW_MS     = 30_000
DEFAULT_WINDOW_MS = 12_000

# How long capture mode survives with nothing happening. This is a dead-man,
# not a convenience: the mode is not persisted precisely because it belongs
# to a running script, and a script that is killed, loses the network, or is
# Ctrl+C'd twice must not leave a device with its assistant switched off and
# no visible reason. The script's own cleanup is the first line of defence;
# this is what covers the case where the script is no longer there to run it.
DEFAULT_IDLE_S = 300.0
MAX_IDLE_S     = 3600.0

# Slack past `max_ms` before the watchdog closes a window on the wall clock.
# The frame path closes a full window as soon as the frame that fills it
# arrives; this covers the case where frames STOP arriving — a link stall, a
# device that dropped its mic stream — where waiting on audio time would hold
# the window open forever. A wall-clock backstop for the case where audio
# time stops advancing.
WINDOW_GRACE_MS = 2_000

# Tags are opaque to the controller and echoed into a header, so they are
# bounded and stripped of anything that cannot ride one.
MAX_TAG_LEN = 200


@dataclass
class CaptureResult:
    """One closed window, ready to deliver."""

    pcm:       bytes
    tag:       str
    session:   str
    peak_db:   float
    floor_db:  float
    frames:    int
    truncated: bool          # `max_ms` closed it rather than the caller
    opened_ms: int           # wall clock at open, epoch ms (for the caller's log)

    @property
    def ms(self) -> int:
        return duration_ms(len(self.pcm))

    def wav(self) -> bytes:
        return encode_wav(self.pcm)


class Window:
    """
    Frames in, one recording out.

    `push` returns True on the frame that fills the window, which is the
    caller's cue to close it — the window does not close itself, because the
    close is what triggers delivery and delivery is not this module's job.
    """

    def __init__(self, tag: str = "", max_ms: int = DEFAULT_WINDOW_MS,
                 session: str | None = None, opened_ms: int | None = None,
                 opened_mono: float | None = None):
        self.tag     = clean_tag(tag)
        self.max_ms  = clamp_max_ms(max_ms)
        self.session = session or uuid.uuid4().hex
        # Wall clock for the caller's records, monotonic for our own timing —
        # the same split every other clock in this controller makes.
        self.opened_ms   = int(time.time() * 1000) if opened_ms is None else int(opened_ms)
        self.opened_mono = time.monotonic() if opened_mono is None else float(opened_mono)
        self._frames: list[bytes] = []
        self._ms      = 0
        self._count   = 0
        self._peak_db = DB_FLOOR
        # The floor at open, measured from the first few frames rather than
        # tracked: a window is seconds long and the caller is about to play
        # audio into it, so a tracker would follow the playback rather than
        # the room. This is the number that answers "was the room quiet when
        # this started", which is what a silent capture needs to be told
        # apart from a muted player.
        self._floor_db: float | None = None
        self._floor_frames = 0

    # How many frames at the head of the window contribute to the floor.
    # ~400ms at the 80ms wire frame: long enough not to be one unlucky
    # frame, short enough to end before any sane playback latency.
    FLOOR_FRAMES = 5

    @property
    def ms(self) -> int:
        return self._ms

    @property
    def frames(self) -> int:
        return self._count

    @property
    def floor_db(self) -> float:
        return DB_FLOOR if self._floor_db is None else self._floor_db

    @property
    def peak_db(self) -> float:
        return self._peak_db

    def push(self, pcm: bytes, rms: float) -> bool:
        """
        Consume one frame. Returns True once the window is full.

        Nothing here allocates beyond holding the frame, so it is safe on the
        controller's event loop — as with em_samples, it is the DELIVERY that
        needs to be off this path.
        """
        if not pcm:
            return False
        db = db_of(rms)
        self._frames.append(pcm)
        self._ms    += duration_ms(len(pcm))
        self._count += 1
        self._peak_db = max(self._peak_db, db)
        if self._floor_frames < self.FLOOR_FRAMES:
            self._floor_frames += 1
            self._floor_db = db if self._floor_db is None else min(self._floor_db, db)
        return self._ms >= self.max_ms

    def overdue(self, now_mono: float | None = None) -> bool:
        """
        True when the wall clock says this window should have finished.

        Audio time cannot answer this: a device that stops sending frames
        stops advancing `ms`, so a stalled link would hold the window open
        indefinitely and the caller would wait for a delivery that never
        comes. See WINDOW_GRACE_MS.
        """
        now = time.monotonic() if now_mono is None else now_mono
        return (now - self.opened_mono) * 1000.0 >= self.max_ms + WINDOW_GRACE_MS

    def close(self, truncated: bool | None = None) -> CaptureResult:
        """
        Freeze the window into a result.

        `truncated` defaults to whether the cap was actually reached, so the
        watchdog and the frame path agree without either having to work it
        out: a window closed early by the caller is not truncated, and one
        the cap ended is, however it was noticed.
        """
        if truncated is None:
            truncated = self._ms >= self.max_ms
        pcm = b"".join(self._frames)
        self._frames = []
        return CaptureResult(
            pcm=pcm,
            tag=self.tag,
            session=self.session,
            peak_db=round(self._peak_db, 1),
            floor_db=round(self.floor_db, 1),
            frames=self._count,
            truncated=bool(truncated),
            opened_ms=self.opened_ms,
        )


# ─── validation ───────────────────────────────────────────────────────────────

def clamp_max_ms(value) -> int:
    """A caller's window length, made safe. Never raises."""
    try:
        ms = int(value)
    except (TypeError, ValueError):
        return DEFAULT_WINDOW_MS
    if ms <= 0:
        return DEFAULT_WINDOW_MS
    return min(ms, MAX_WINDOW_MS)


def clamp_idle_s(value) -> float:
    """The dead-man's timeout, made safe. Never raises, never returns 0 —
    a zero here would disarm the one thing that un-strands the device."""
    try:
        s = float(value)
    except (TypeError, ValueError):
        return DEFAULT_IDLE_S
    if s <= 0.0:
        return DEFAULT_IDLE_S
    return min(s, MAX_IDLE_S)


def clean_tag(tag) -> str:
    """
    A caller's tag, made safe to put in a header.

    Opaque here on purpose — correlation is the caller's business, and a tag
    format the controller understood would be a second copy to drift. All
    this does is bound the length and drop anything that cannot ride an HTTP
    header value (control characters, newlines, non-latin-1).
    """
    if tag is None:
        return ""
    text = str(tag)[:MAX_TAG_LEN]
    return "".join(c for c in text if 0x20 <= ord(c) < 0x7F)


def valid_webhook(url) -> bool:
    """
    True if `url` is something we are willing to POST to.

    The caller is an authenticated admin, so this is not an authorisation
    check — it is the cheap half of not building an SSRF gadget out of a
    controller that sits on a home LAN. Scheme is restricted to http/https
    and a host is required; the OTHER half is refusing redirects at the
    request itself, which is em_controller's job.
    """
    if not url or not isinstance(url, str):
        return False
    try:
        parts = urlparse(url)
    except Exception:
        return False
    return parts.scheme in ("http", "https") and bool(parts.hostname)


# ─── delivery metadata ────────────────────────────────────────────────────────

def headers(result: CaptureResult, device_id: str, dropped: int = 0) -> dict:
    """
    The metadata that rides the POST.

    Headers rather than multipart so both ends stay dependency-free: the
    receiver writes the body straight to disk as a WAV and reads the rest off
    the response headers, with no parser in between.

    `peak_db` and `floor_db` are the pair that matter operationally. A caller
    driving a long unattended run needs to notice that it has been recording
    silence — because the wrong entity was addressed, or a player was muted —
    and the two numbers together say so, where either alone does not.
    """
    return {
        "Content-Type":   "audio/wav",
        "X-EM-Device":    device_id,
        "X-EM-Session":   result.session,
        "X-EM-Tag":       result.tag,
        "X-EM-Ms":        str(result.ms),
        "X-EM-Peak-Db":   f"{result.peak_db:.1f}",
        "X-EM-Floor-Db":  f"{result.floor_db:.1f}",
        "X-EM-Frames":    str(result.frames),
        "X-EM-Dropped":   str(int(dropped)),
        "X-EM-Truncated": "1" if result.truncated else "0",
        "X-EM-Opened-Ms": str(result.opened_ms),
    }


class ResultStore:
    """
    Closed windows waiting to be collected, for the PULL delivery mode.

    Push is not always available and on this fleet is not available at all:
    the controller runs on a macvlan network, and a macvlan container cannot
    reach its own Docker host — so a webhook served by the machine driving
    the run times out every time, with the recording captured perfectly and
    nowhere to go. Pull inverts that. The driver is already an HTTP client of
    the controller, so it needs no route back and works on any network where
    the rest of the API does.

    Bounded and FIFO: a driver that stops collecting must not grow this
    without limit, and the OLDEST is what gets dropped because the newest is
    the one somebody is currently waiting for. An eviction is counted — a
    caller seeing them knows it is falling behind rather than losing
    recordings to a fault.
    """

    def __init__(self, limit: int = 8):
        self.limit    = max(1, int(limit))
        self.evicted  = 0
        self._items: "OrderedDict[str, CaptureResult]" = OrderedDict()

    def __len__(self) -> int:
        return len(self._items)

    @property
    def sessions(self) -> list[str]:
        return list(self._items)

    def put(self, result: CaptureResult) -> None:
        self._items[result.session] = result
        while len(self._items) > self.limit:
            self._items.popitem(last=False)
            self.evicted += 1

    def take(self, session: str | None = None) -> CaptureResult | None:
        """
        Remove and return one result: the named session, or the oldest.

        Consumed on read — leaving it
        behind would let one recording be collected twice and paired with two
        different source files.
        """
        if session:
            return self._items.pop(session, None)
        if not self._items:
            return None
        return self._items.popitem(last=False)[1]

    def clear(self) -> None:
        self._items.clear()


def decide_idle_expiry(now_mono: float, last_activity_mono: float,
                       idle_s: float, window_open: bool) -> bool:
    """
    Should capture mode clear itself?

    Pure so the rule is testable without a running controller — the same
    reason `em_button.decide` and `em_linkauth.decide` are functions.

    An OPEN window is never idle. A caller can legitimately open a 30s window
    and say nothing else for its duration, and expiring underneath it would
    discard the recording it is waiting for.
    """
    if window_open:
        return False
    return (now_mono - last_activity_mono) >= idle_s


def window_overdue(window: "Window | None", now_mono: float | None = None) -> bool:
    """`window.overdue`, tolerating None — the watchdog's shape."""
    return window is not None and window.overdue(now_mono)
