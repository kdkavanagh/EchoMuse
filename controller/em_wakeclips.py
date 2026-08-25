"""
em_wakeclips.py — the audio that woke the device, kept for retraining
=====================================================================

A false wake has no evidence. The Activity row records that a turn started,
which model scored it and how high (`wake_score`), and — if `saveUtterances`
is on — the audio streamed to Home Assistant *afterwards*. None of that is
the sound that actually crossed the threshold: the wake word ends before the
detection, `_stream_mic_audio` then discards `VOICE_PREROLL_DISCARD` frames
precisely to drop the tail of it, and the model's own rolling context is
overwritten by `model.reset()` on the same iteration. So the one recording
that would let a false positive be fixed — feed it back to `oww_forge` as a
negative and retrain — was the one recording that could not be obtained.

This module stores it. `wake_word_listener` keeps the last `CLIP_MS` of the
chunks it is scoring in a bounded deque; on a detection that starts a turn it
joins them onto the Device, and `em_esphome._save_wake_clip` writes them here
once the turn's rowid exists. Consequences of that shape worth stating:

  * **The clip is exactly what the model scored**, byte for byte, tapped at
    the same point as `em_samples` and for the same reason: a negative
    captured through a different gain or denoiser teaches the model about a
    path it will never see in service.
  * **It ends at the detection and contains no command audio.** The causal
    window is entirely before the crossing, the turn's own recording already
    covers what follows, and a training negative that carries somebody's
    request is a privacy cost with no modelling benefit.
  * **Nothing is retained in memory unless the feature is on.** The deque is
    only fed while `saveWakeClips` is set for the device, so the default is
    an empty ring rather than a rolling two seconds of the room.
  * **The size bound is structural.** The deque's `maxlen` is the only limit
    the buffer needs, so unlike `em_recordings` there is no byte cap here to
    keep in step with a stream that has no length of its own.

Storage follows `em_samples` rather than `em_recordings`: a per-device
directory, because a device that false-triggers is going to produce hundreds
of these and a flat directory would be listed in full on every write, and a
retention cap sized for a training set rather than a diagnostic handful. The
filename is the turn id, as in `em_recordings` — rowids are monotonic, so
the ordering pruning depends on is exact even after a restore from a backup
that flattened every timestamp, and the Activity row that shows the false
positive links straight to the clip that caused it.

Pure path/filesystem logic (no aiohttp, no db import) so it can be unit
tested; em_esphome writes through it and em_api serves from it.
"""

from __future__ import annotations

import logging
import os
import re
from pathlib import Path

from em_samples import (          # one definition of the wire format
    CHANNELS,
    SAMPLE_RATE,
    SAMPLE_WIDTH,
    duration_ms,
    encode_wav,
)

log = logging.getLogger("echomuse.wakeclips")

WAKES_SUBDIR = "wakes"

# How much pre-detection audio a clip holds. openWakeWord's own context is
# ~1.4s and BC-ResNet scores a 1.4s window, so this covers the whole of what
# either model actually looked at with a little room in front of it — the
# margin matters because a false positive is usually a word or two, and a
# negative cut flush against the crossing trains the model on a fragment.
CLIP_MS = 2000

# The wake stream arrives as 80ms chunks (em_controller.CHUNK_BYTES), which
# is what the listener's ring holds. 25 frames = CLIP_MS.
FRAME_MS    = 80
CLIP_FRAMES = CLIP_MS // FRAME_MS

# Clips kept per device before the oldest go. A false-positive corpus is the
# point here, not a diagnostic sample, so this is two orders of magnitude
# above em_recordings.KEEP_PER_DEVICE — but well below em_samples, because
# these accumulate unattended and a device with a badly-tuned threshold
# should not be able to fill a volume on its own. 2s is ~64kB, so the cap is
# ~32MB per device.
KEEP_PER_DEVICE = 500

# `<turn_id>.wav`, inside a per-device directory. turn_id is a rowid and
# device_id is ro.serialno, but both come back off disk or out of a URL, so
# they are validated as path components rather than trusted.
_NAME_RE   = re.compile(r"^(?P<turn>\d{1,19})\.wav$")
_DEVICE_RE = re.compile(r"[A-Za-z0-9_.-]{1,64}")


def wakes_dir(db_path: str | None = None) -> Path:
    """`wakes/` beside the SQLite DB. Absolute, so cwd cannot move it."""
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / WAKES_SUBDIR


def safe_device_id(device_id: str) -> str | None:
    """The device id as a path component, or None if it isn't one."""
    if device_id and _DEVICE_RE.fullmatch(device_id):
        return device_id
    return None


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


def list_for(device_id: str, db_path: str | None = None) -> list[dict]:
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
    out: list[dict] = []
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


def usage(device_id: str, db_path: str | None = None) -> dict:
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
