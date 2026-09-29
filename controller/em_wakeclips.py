"""
em_wakeclips.py — the audio that woke the device, kept for diagnosis
====================================================================

A false wake is only fixable if the sound that caused it can be heard. The
Activity row records which registry model accepted the wake and its peak
score, and — if `saveUtterances` is on — the committed utterance sent to Home
Assistant. Neither is the wake word itself as the device's BCResNet heard it.

This module stores that clip. When a turn started by an accepted
`wake.candidate` ends, `em_session` reads the candidate's support span from
the lease's mic timeline — support start −300 ms through `support_end` — and,
if `saveWakeClips` is set for the device, writes it here once the turn's
rowid exists. The clips serve two jobs: listening to why a device woke, and
false-positive/true-positive material for BCResNet training, which lives
outside this repository (`~/git/bcresnet`). Consequences of that shape:

  * **The clip is the audio the device scored.** The mic stream is the
    native AFE's output, the same samples the on-device detector consumed,
    with no controller gain or denoiser applied. Training material captured
    through a different path teaches a model about audio it never sees.
  * **It contains no command audio.** It ends where the candidate's support
    ends; the utterance recording (opt-in, separate) covers what follows.
  * **Nothing is written unless the feature is on** for that device.

Storage follows `em_samples` rather than `em_recordings`: a per-device
directory, because a device that false-triggers is going to produce hundreds
of these and a flat directory would be listed in full on every write, and a
retention cap sized for a corpus rather than a diagnostic handful. The
filename is the turn id, as in `em_recordings` — rowids are monotonic, so
the ordering pruning depends on is exact even after a restore from a backup
that flattened every timestamp, and the Activity row that shows the false
positive links straight to the clip that caused it.

Pure path/filesystem logic (no aiohttp, no db import) so it can be unit
tested; em_session writes through it and em_api serves from it.
"""

from __future__ import annotations

import logging
import os
import re
from pathlib import Path
from typing import TypedDict

from em_samples import (          # one definition of the wire format
    CHANNELS,
    SAMPLE_RATE,
    SAMPLE_WIDTH,
    StoreUsage,
    duration_ms,
    encode_wav,
    safe_device_id,
)

log = logging.getLogger("echomuse.wakeclips")

WAKES_SUBDIR = "wakes"

# A nominal clip: 18 wire frames of 80 ms (1.44 s), about the BCResNet
# window plus a little lead-in. Real clips are the candidate's support span
# −300 ms, so their length varies; this is the unit the retention estimate
# below and the tests use.
FRAME_MS    = 80
CLIP_FRAMES = 18
CLIP_MS     = CLIP_FRAMES * FRAME_MS      # 1440

# Clips kept per device before the oldest go. A false-positive corpus is the
# point here, not a diagnostic sample, so this is two orders of magnitude
# above em_recordings.KEEP_PER_DEVICE — but well below em_samples, because
# these accumulate unattended and a device with a badly-tuned threshold
# should not be able to fill a volume on its own. CLIP_MS is ~46kB, so the
# cap is ~23MB per device.
KEEP_PER_DEVICE = 500

# `<turn_id>.wav`, inside a per-device directory. turn_id is a rowid and
# device_id is ro.serialno, but both come back off disk or out of a URL, so
# they are validated as path components rather than trusted.
_NAME_RE = re.compile(r"^(?P<turn>\d{1,19})\.wav$")


class WakeClipEntry(TypedDict):
    """One stored wake clip as the API lists it."""

    name:    str
    turn_id: int
    bytes:   int
    ms:      int


def wakes_dir(db_path: str | None = None) -> Path:
    """`wakes/` beside the SQLite DB. Absolute, so cwd cannot move it."""
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / WAKES_SUBDIR

def device_dir(device_id: str, db_path: str | None = None) -> Path | None:
    safe = safe_device_id(device_id)
    if safe is None:
        return None
    return wakes_dir(db_path) / safe


def filename(turn_id: int) -> str:
    return f"{int(turn_id)}.wav"


def parse_filename(name: str) -> int | None:
    """The clip's turn id, or None if the name is not ours."""
    m = _NAME_RE.match(name)
    return int(m.group("turn")) if m else None


def save(device_id: str, turn_id: int, pcm: bytes,
         db_path: str | None = None,
         keep: int = KEEP_PER_DEVICE) -> str | None:
    """
    Write one wake clip and prune the device back to `keep` files.

    Returns the filename, or None if nothing was written. Blocking — call it
    in an executor.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or not pcm or int(turn_id) <= 0:
        return None
    directory.mkdir(parents=True, exist_ok=True)
    name = filename(turn_id)
    path = directory / name
    # Write-then-rename, as in em_samples: a partially written WAV that the
    # API then serves is worse than no clip at all.
    tmp = path.with_suffix(".wav.part")
    tmp.write_bytes(encode_wav(pcm))
    tmp.replace(path)
    prune(device_id, db_path=db_path, keep=keep)
    return name


def list_for(device_id: str, db_path: str | None = None) -> list[WakeClipEntry]:
    """
    This device's clips, newest turn first: name, turn id, bytes, duration.

    Duration comes from the file size rather than the WAV header, as in
    em_samples: every file here was written by encode_wav, and opening
    hundreds of them to read a header the writer already fixed would make
    listing a directory scan plus a syscall storm.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or not directory.is_dir():
        return []
    out: list[WakeClipEntry] = []
    for child in directory.iterdir():
        turn = parse_filename(child.name)
        if turn is None:
            continue
        try:
            size = child.stat().st_size
        except OSError:
            continue
        out.append({
            "name":    child.name,
            "turn_id": turn,
            "bytes":   size,
            # 44 bytes of RIFF header. A negative would mean a truncated
            # file, which reads as 0 rather than as nonsense.
            "ms":      max(0, duration_ms(size - 44)),
        })
    out.sort(key=lambda c: c["turn_id"], reverse=True)
    return out


def usage(device_id: str, db_path: str | None = None) -> StoreUsage:
    """Clip count, total bytes and total duration for a device."""
    clips = list_for(device_id, db_path)
    return {
        "count": len(clips),
        "bytes": sum(c["bytes"] for c in clips),
        "ms":    sum(c["ms"] for c in clips),
    }


def prune(device_id: str, db_path: str | None = None,
          keep: int = KEEP_PER_DEVICE) -> list[str]:
    """
    Delete all but the `keep` newest clips. Returns the names removed.
    Never raises — a failed unlink costs disk, not a clip.
    """
    directory = device_dir(device_id, db_path)
    if directory is None:
        return []
    removed: list[str] = []
    for clip in list_for(device_id, db_path)[max(keep, 0):]:
        try:
            (directory / clip["name"]).unlink()
            removed.append(clip["name"])
        except OSError as e:
            log.warning(f"[wakeclips] Could not prune {clip['name']}: {e}")
    return removed


def resolve(device_id: str, name: str, db_path: str | None = None) -> Path | None:
    """
    Path of an existing clip, or None.

    The name must parse as one of ours AND resolve inside this device's own
    directory — the API takes both from the URL, so without the check a
    crafted name would reach another device's audio (or anything else on
    the volume).
    """
    directory = device_dir(device_id, db_path)
    if directory is None or parse_filename(name) is None:
        return None
    path = directory / name
    if not path.is_file():
        return None
    return path


def delete_all(device_id: str, db_path: str | None = None) -> int:
    """Remove every clip for a device. Returns the count deleted."""
    return len(prune(device_id, db_path=db_path, keep=0))


def delete_device(device_id: str, db_path: str | None = None) -> int:
    """
    Remove a device's clips and its directory. Called from db.delete_device —
    nothing cascades from SQLite to the filesystem, and leaving a deleted
    device's speech on the volume is the one leftover that matters.
    """
    n = delete_all(device_id, db_path)
    directory = device_dir(device_id, db_path)
    if directory is not None and directory.is_dir():
        try:
            directory.rmdir()
        except OSError:
            pass   # not empty (a .part from a crash), or gone already
    return n
