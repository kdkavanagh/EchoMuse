"""
em_ambient.py — hold the mic open, get one file back
====================================================

The sibling of `em_samples`, for the other half of a training set. Sample
collection answers "capture the wake word, wherever in this recording it
happens to be" and therefore has to *guess* where the speech is, cutting at
the silences. This module answers a question with no speech in it at all:
**what does this room sound like when nobody is talking to it** — the fridge,
the extractor fan, the TV two rooms away, the family having dinner. That is
the negative material `oww_forge` mixes under its synthetic positives, and
it is also what an endpointer or a threshold is tuned against.

For that job a segmenter is exactly wrong. Ambient noise has no onsets to cut
on, and a set of clips cut out of it has thrown away the one property that
made the recording worth taking: continuity. So the contract here is the
blunt one — **the mode opens the mic, and closing it hands back a single WAV
covering the whole time it was open**.

Like collect mode, the device needs nothing new: the wake stream is already
continuous, ungated and AGC-free, and the frames are tapped at the same point
in `wake_word_listener`, so the audio is byte-for-byte what the wake model
scores. Also like collect mode, the mode SUSPENDS voice turns — a device
whose mic is being recorded must not also be answering with it.

Three things differ from `em_samples`, and each one is why this is its own
module rather than a flag on that one:

  * **It streams to disk.** A clip is ~32kB and lives in memory until it is
    cut; an ambient recording is 1.92 MB per minute and has no natural end.
    Buffering one in a bytearray is a memory leak with a UI switch on it, so
    the file is opened when the mode is armed and frames are appended to it
    in ~2s batches (`FLUSH_BYTES`) — one write per 64kB rather than one per
    80ms frame, which keeps the executor hop off the busiest path in the
    controller without holding audio that a crash would lose.

  * **The header is patched at the end.** A streamed WAV cannot know its own
    length when it is opened, so the 44-byte header is written with zero
    sizes and rewritten on close. That makes crash recovery arithmetic
    rather than guesswork: `finalize()` derives both size fields from the
    file length, so a `.part` left behind by a killed controller is promoted
    to a playable WAV instead of discarded (`recover()`), and the audio
    someone left a device recording overnight for survives.

  * **Retention is a handful of files, not thousands.** `KEEP_PER_DEVICE`
    recordings of at most `MAX_RECORDING_MS` each bounds a device at
    ~350MB. The cap ROLLS rather than stops: an unattended overnight
    capture is a real use of this, and silently ending it at the cap would
    lose the rest of the night. A rolled recording is a new file with its
    own timestamp, which is visible in the list rather than implied.

No zip endpoint, deliberately, and that is the mirror of the samples one:
there the archive IS the feature (a training run wants the set), while here
one recording is one artefact and a 350MB in-memory zip would be a way to
make the controller fall over from the dashboard.

Pure logic and filesystem work — no numpy, no aiohttp, no db import. The
lifecycle (arming, the LED ring, rolling at the cap) lives in em_controller
and the routes in em_api, exactly as for `em_samples`.
"""

from __future__ import annotations

import logging
import os
import re
import struct
import time
from pathlib import Path

from em_samples import (          # one definition of the wire format
    CHANNELS,
    SAMPLE_RATE,
    SAMPLE_WIDTH,
    duration_ms,
    safe_device_id,
)

log = logging.getLogger("echomuse.ambient")

__all__ = [
    "AMBIENT_SUBDIR", "FLUSH_BYTES", "HEADER_BYTES", "KEEP_PER_DEVICE",
    "MAX_RECORDING_MS", "Recorder", "ambient_dir", "delete_all",
    "delete_device", "device_dir", "duration_ms", "filename", "finalize",
    "list_for", "parse_filename", "prune", "recover", "resolve", "start",
    "usage", "wav_header",
]

AMBIENT_SUBDIR = "ambient"

# Canonical PCM WAV header. Fixed size because the container is written by
# hand: `finalize` seeks back to byte 0 and rewrites exactly this many bytes,
# and `list_for` subtracts it to get a duration from a stat() alone.
HEADER_BYTES = 44

# How much audio is held before it is written. 64kB is 2s of the wire format:
# one write per 2s instead of 12.5 per second, and at most 2s of audio lost
# to a hard kill (a clean stop, a disconnect and a rolled cap all flush).
FLUSH_BYTES = 64 * 1024

# Hard cap on ONE file, and the reason the mode rolls at it rather than
# stopping. 30 minutes is 57.6MB — small enough to download over the LAN and
# to open in an editor, large enough that a normal "record the kitchen for a
# while" is one file, which is the contract this module exists to keep.
MAX_RECORDING_MS = 30 * 60 * 1000

# Recordings kept per device before the oldest are dropped. Six of the above
# is ~350MB per device on the same volume the DB and the samples live on —
# the ceiling is set by disk, not by what is useful, which is why it is six
# and not sixty.
KEEP_PER_DEVICE = 6

# `<epoch_ms>.wav`, in a per-device directory, matching em_samples so the two
# stores read the same way. `.wav.part` is a recording still open (or one
# orphaned by a crash) and is never listed, served or counted.
_NAME_RE = re.compile(r"^(?P<ts>\d{10,17})\.wav$")
_PART_RE = re.compile(r"^(?P<ts>\d{10,17})\.wav\.part$")


# ─── container ────────────────────────────────────────────────────────────────

def wav_header(data_bytes: int) -> bytes:
    """
    A 44-byte canonical PCM WAV header for `data_bytes` of payload.

    Written by hand rather than with `wave`, because the sizes have to be
    patchable after the fact: `wave` only fixes its header when it closes
    the file it owns, which is no use for recovering a `.part` whose writer
    was killed.
    """
    byte_rate  = SAMPLE_RATE * CHANNELS * SAMPLE_WIDTH
    block_size = CHANNELS * SAMPLE_WIDTH
    return b"".join((
        b"RIFF", struct.pack("<I", 36 + data_bytes), b"WAVEfmt ",
        struct.pack("<IHHIIHH", 16, 1, CHANNELS, SAMPLE_RATE, byte_rate,
                    block_size, SAMPLE_WIDTH * 8),
        b"data", struct.pack("<I", data_bytes),
    ))


def finalize(path: Path) -> bool:
    """
    Rewrite `path`'s header to match its actual length. True if it now holds
    playable audio.

    The whole of crash recovery: the payload is everything after the header,
    so both size fields are a subtraction. A file with no payload is not
    audio and is reported as such (the caller unlinks it).
    """
    try:
        size = path.stat().st_size
    except OSError:
        return False
    data = size - HEADER_BYTES
    if data <= 0:
        return False
    try:
        with open(path, "r+b") as fh:
            fh.seek(0)
            fh.write(wav_header(data))
    except OSError as e:
        log.warning(f"[ambient] Could not finalize {path.name}: {e}")
        return False
    return True


# ─── paths ────────────────────────────────────────────────────────────────────

def ambient_dir(db_path: str | None = None) -> Path:
    """`ambient/` beside the SQLite DB. Absolute, so cwd cannot move it."""
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / AMBIENT_SUBDIR


def device_dir(device_id: str, db_path: str | None = None) -> Path | None:
    safe = safe_device_id(device_id)
    if safe is None:
        return None
    return ambient_dir(db_path) / safe


def filename(when_ms: int) -> str:
    return f"{int(when_ms)}.wav"


def parse_filename(name: str) -> int | None:
    """The recording's epoch-ms timestamp, or None if the name is not ours."""
    m = _NAME_RE.match(name)
    return int(m.group("ts")) if m else None


# ─── the open recording ───────────────────────────────────────────────────────

class Recorder:
    """
    One ambient recording, open on disk.

    Frames go in with `push`, which does no I/O and returns True when a
    flush is due — the caller owns the executor hop, because it is the only
    thing that knows it is on an event loop. `close` patches the header and
    renames the `.part` into place, so nothing incomplete is ever listed.
    """

    def __init__(self, path: Path, started_ms: int,
                 flush_bytes: int = FLUSH_BYTES,
                 max_ms: int = MAX_RECORDING_MS):
        self.path        = path
        self.part        = path.with_suffix(".wav.part")
        self.started_ms  = int(started_ms)
        self.flush_bytes = int(flush_bytes)
        self.max_ms      = int(max_ms)
        self.written     = 0                 # payload bytes on disk
        self._pending: list[bytes] = []
        self._pending_bytes = 0
        self._closed = False
        self.part.parent.mkdir(parents=True, exist_ok=True)
        self._fh = open(self.part, "wb")
        # Placeholder sizes, rewritten by `close`/`finalize`. Flushed now so
        # the payload never starts at byte 0 on disk: recovery reads the
        # first 44 bytes as a header, and a file whose header was still in
        # a userspace buffer when the process died would have its opening
        # audio overwritten by one.
        self._fh.write(wav_header(0))
        self._fh.flush()

    # ── measurement ──
    @property
    def data_bytes(self) -> int:
        """Payload captured so far, on disk and in hand."""
        return self.written + self._pending_bytes

    @property
    def duration_ms(self) -> int:
        return duration_ms(self.data_bytes)

    @property
    def full(self) -> bool:
        """At the length cap. The caller rolls to a new file — see the
        module docstring for why it rolls rather than stops."""
        return self.duration_ms >= self.max_ms

    @property
    def flush_due(self) -> bool:
        return self._pending_bytes >= self.flush_bytes

    # ── audio ──
    def push(self, frame: bytes) -> bool:
        """Take one frame. No I/O; True when `flush` is due."""
        if self._closed or not frame:
            return False
        self._pending.append(frame)
        self._pending_bytes += len(frame)
        return self.flush_due

    def flush(self) -> int:
        """
        Write what is held. Returns the bytes written. Blocking.

        Pushed through to the OS rather than left in Python's buffer: the
        batch size is the stated bound on what a hard kill can lose, and a
        bound that only holds if the userspace buffer happens to be smaller
        is not a bound. Writes are strictly sequential, so the worst a kill
        can leave is a PREFIX of header-plus-audio — never a hole — which is
        what makes `finalize` arithmetic instead of a repair.
        """
        if self._closed or not self._pending:
            return 0
        blob, self._pending, self._pending_bytes = (
            b"".join(self._pending), [], 0
        )
        self._fh.write(blob)
        self._fh.flush()
        self.written += len(blob)
        return len(blob)

    def close(self) -> str | None:
        """
        Flush, patch the header, and put the file in place.

        Returns the final filename, or None if the recording held no audio
        (the mode switched on and straight off again) — in which case the
        part file is removed rather than left as a 44-byte WAV. Blocking.
        """
        if self._closed:
            return None
        self.flush()
        self._closed = True
        try:
            self._fh.seek(0)
            self._fh.write(wav_header(self.written))
        finally:
            self._fh.close()
        if self.written <= 0:
            self.part.unlink(missing_ok=True)
            return None
        # A name collision means the clock stepped back onto an existing
        # recording; take the next free millisecond rather than overwrite
        # half an hour of someone's audio.
        path = self.path
        while path.exists():
            path = path.with_name(filename(parse_filename(path.name) + 1))
        self.part.replace(path)
        self.path = path
        return path.name

    def abort(self) -> None:
        """Drop the recording and its file. For a failed start only."""
        if self._closed:
            return
        self._closed = True
        try:
            self._fh.close()
        finally:
            self.part.unlink(missing_ok=True)


def start(device_id: str, when_ms: int | None = None,
          db_path: str | None = None,
          flush_bytes: int = FLUSH_BYTES,
          max_ms: int = MAX_RECORDING_MS) -> Recorder | None:
    """
    Open a recording for a device. None if the id is not a path component.

    Blocking — call it in an executor. The wall clock lives here for the
    same reason it does in `em_samples.save_clip`: em_controller keeps to
    monotonic clocks, and a filename is the one place a real date is wanted.
    """
    directory = device_dir(device_id, db_path)
    if directory is None:
        return None
    when = int(time.time() * 1000) if when_ms is None else int(when_ms)
    while (directory / filename(when)).exists():
        when += 1
    return Recorder(directory / filename(when), when,
                    flush_bytes=flush_bytes, max_ms=max_ms)


def recover(device_id: str, db_path: str | None = None) -> list[str]:
    """
    Promote `.part` files left by a killed controller into real recordings.

    Returns the names recovered. An overnight ambient capture is hours of
    audio that only this makes survivable, and the cost is a header rewrite
    — see `finalize`. Never raises: recovery failing must not stop the
    recording that is about to start.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or not directory.is_dir():
        return []
    out: list[str] = []
    for child in sorted(directory.iterdir()):
        m = _PART_RE.match(child.name)
        if m is None:
            continue
        try:
            if not finalize(child):
                child.unlink()
                continue
            target = directory / filename(int(m.group("ts")))
            while target.exists():
                target = target.with_name(filename(parse_filename(target.name) + 1))
            child.replace(target)
            out.append(target.name)
            log.info(f"[ambient] Recovered {target.name} for {device_id} "
                     f"after an unclean shutdown")
        except OSError as e:
            log.warning(f"[ambient] Could not recover {child.name}: {e}")
    if out:
        prune(device_id, db_path=db_path)
    return out


# ─── the closed recordings ────────────────────────────────────────────────────

def list_for(device_id: str, db_path: str | None = None) -> list[dict]:
    """
    This device's recordings, newest first: name, epoch seconds, bytes and
    duration. Only closed ones — a `.part` is not a file anyone can play.

    Duration comes from the file size rather than the WAV header, as in
    em_samples: every file here was written by this module, and the header
    it would read is derived from that same size anyway.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or not directory.is_dir():
        return []
    out: list[dict] = []
    for child in directory.iterdir():
        ts = parse_filename(child.name)
        if ts is None:
            continue
        try:
            size = child.stat().st_size
        except OSError:
            continue
        out.append({
            "name":  child.name,
            "ts":    ts / 1000.0,
            "bytes": size,
            "ms":    max(0, duration_ms(size - HEADER_BYTES)),
        })
    out.sort(key=lambda r: r["ts"], reverse=True)
    return out


def usage(device_id: str, db_path: str | None = None) -> dict:
    """Recording count, total bytes and total duration for a device."""
    items = list_for(device_id, db_path)
    return {
        "count": len(items),
        "bytes": sum(i["bytes"] for i in items),
        "ms":    sum(i["ms"] for i in items),
    }


def prune(device_id: str, db_path: str | None = None,
          keep: int = KEEP_PER_DEVICE) -> list[str]:
    """
    Delete all but the `keep` newest recordings. Returns the names removed.
    Never raises — a failed unlink costs disk, not a recording.
    """
    directory = device_dir(device_id, db_path)
    if directory is None:
        return []
    removed: list[str] = []
    for item in list_for(device_id, db_path)[max(keep, 0):]:
        try:
            (directory / item["name"]).unlink()
            removed.append(item["name"])
        except OSError as e:
            log.warning(f"[ambient] Could not prune {item['name']}: {e}")
    return removed


def resolve(device_id: str, name: str, db_path: str | None = None) -> Path | None:
    """
    Path of an existing recording, or None.

    The name must parse as one of ours AND resolve inside this device's own
    directory: both come from the URL, so without the check a crafted name
    would reach another device's audio. A `.part` never parses, which is
    also what keeps an in-progress recording out of the API.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or parse_filename(name) is None:
        return None
    path = directory / name
    if not path.is_file():
        return None
    return path


def delete_all(device_id: str, db_path: str | None = None) -> int:
    """Remove every recording for a device. Returns the count deleted."""
    return len(prune(device_id, db_path=db_path, keep=0))


def delete_device(device_id: str, db_path: str | None = None) -> int:
    """
    Remove a device's recordings and its directory. Called from
    db.delete_device — nothing cascades from SQLite to the filesystem, and
    a deleted device's room audio is exactly the leftover that matters.
    """
    n = delete_all(device_id, db_path)
    directory = device_dir(device_id, db_path)
    if directory is not None and directory.is_dir():
        for child in list(directory.iterdir()):
            if _PART_RE.match(child.name):
                try:
                    child.unlink()
                except OSError:
                    pass
        try:
            directory.rmdir()
        except OSError:
            pass   # not empty, or gone already
    return n
