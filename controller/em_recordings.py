"""
em_recordings.py — utterance and turn audio for the Activity panel
==================================================================

Saves the mic audio of recent voice turns as WAV files so you can *listen*
to what the array actually captured, rather than inferring mic quality from
an STT transcript and a wake score. Asked for by users who wanted to judge
capture quality before spending an evening on the room and the placement.

Two kinds (`RecordingKind`), both kept only while `saveUtterances` is on:

  * **The utterance** (`recordings/`) is exactly the STT copy the controller
    uploaded to Home Assistant for the committed span — after ASR gain and,
    on a device with `nsAsr` on, DTLN noise suppression — not the raw mic.
    That is the point: a recording that isn't what STT heard can tell you
    the room was noisy but never why a transcript came back wrong.
  * **The turn recording** (`turn_recordings/`) is the whole turn as the
    native AFE output it: canonical mic PCM, no gain, from where the turn's
    chart starts through the spoken answer and a second past the turn's end
    (em_session's response lease). Only turns with an AFE chart (Fire OS 6,
    `afe_metadata_v1`) have one; the dashboard plays it against the chart.

Storage: files live beside the SQLite DB, so they sit inside the persisted
Docker volume and survive image upgrades. Retention is a hard per-device
file count per kind — these are the artefacts here that contain raw speech,
so a bounded window, then gone, is the point, not an optimisation. Pruning
is by turn id parsed out of the filename rather than mtime: ids are
monotonic rowids, so the order is exact even if the volume is restored from
a backup that flattened timestamps. 16kHz mono S16_LE.

Pure path/filesystem logic (no aiohttp, no db import) so it can be unit
tested; em_session writes through it and em_api serves from it.
"""

from __future__ import annotations

import enum
import io
import logging
import os
import re
import wave
from pathlib import Path

from em_samples import safe_device_id    # one definition of a device path component

log = logging.getLogger("echomuse.recordings")


class RecordingKind(enum.StrEnum):
    """What a recording holds. Each kind has its own directory and retention;
    the filename is `<device>_<turn>.wav` in both."""

    UTTERANCE = "utterance"   # the STT copy of the committed span
    TURN = "turn"             # the whole turn on its chart's timeline, canonical PCM


RECORDINGS_SUBDIR = "recordings"
TURN_RECORDINGS_SUBDIR = "turn_recordings"

# How many utterances to keep per device: enough for an endpoint and ASR
# evaluation corpus. The STT copy runs ~100–200 kB per turn, so 300 is
# ~30–60 MB per device (at most 300 × MAX_UTTERANCE_BYTES ≈ 290 MB).
KEEP_PER_DEVICE = 300

# How many turn recordings to keep per device: a diagnostic window, not a
# corpus. One runs ~10–25 s (~0.3–0.8 MB) — the wake, the request, the wait
# and the spoken answer — so 100 is ~30–80 MB per device.
KEEP_TURN_PER_DEVICE = 100

_SUBDIRS = {RecordingKind.UTTERANCE: RECORDINGS_SUBDIR, RecordingKind.TURN: TURN_RECORDINGS_SUBDIR}
_KEEP = {RecordingKind.UTTERANCE: KEEP_PER_DEVICE, RecordingKind.TURN: KEEP_TURN_PER_DEVICE}

# Format of both kinds (em_stt_copy, em_audio_timeline): 16 kHz mono PCM16.
SAMPLE_RATE  = 16000
SAMPLE_WIDTH = 2
CHANNELS     = 1

# Nominal cap on one utterance recording: the extendedUtterances utterance
# cap. Not enforced by save(); a committed span can exceed it by its pre-roll
# and tail. 30s at 16kHz mono = 960 kB.
MAX_UTTERANCE_SECONDS = 30
MAX_UTTERANCE_BYTES   = MAX_UTTERANCE_SECONDS * SAMPLE_RATE * SAMPLE_WIDTH

# device_id is ro.serialno (hex) and turn_id is a rowid, but this is the
# name that comes back off disk and out of the DB, so it gets validated
# like any other path component rather than trusted.
_NAME_RE = re.compile(r"^(?P<device>[A-Za-z0-9_.-]{1,64})_(?P<turn>\d{1,19})\.wav$")


def recordings_dir(db_path: str | None = None, *, kind: RecordingKind = RecordingKind.UTTERANCE) -> Path:
    """
    Resolve a kind's directory beside the SQLite DB (DB_PATH env, same
    default as em_controller): `recordings/` or `turn_recordings/`.
    Absolute, so it stays valid regardless of the process cwd.
    """
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return (Path(db_path).resolve().parent / _SUBDIRS[kind])


def filename(device_id: str, turn_id: int) -> str | None:
    """Canonical filename for a turn's recording (either kind), or None if unnameable."""
    safe = safe_device_id(device_id)
    if safe is None or turn_id is None or int(turn_id) < 0:
        return None
    return f"{safe}_{int(turn_id)}.wav"


def parse_filename(name: str) -> tuple[str, int] | None:
    """(device_id, turn_id) for a recording filename, or None if malformed."""
    m = _NAME_RE.match(name)
    if not m:
        return None
    return m.group("device"), int(m.group("turn"))


def encode_wav(pcm: bytes) -> bytes:
    """PCM frames → a WAV container, in memory."""
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(CHANNELS)
        w.setsampwidth(SAMPLE_WIDTH)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(pcm)
    return buf.getvalue()


def duration_ms(pcm_len: int) -> int:
    """Playing time of `pcm_len` bytes of the wire format, in ms."""
    return int(pcm_len / (SAMPLE_RATE * SAMPLE_WIDTH * CHANNELS) * 1000)


def save(device_id: str, turn_id: int, pcm: bytes, db_path: str | None = None,
         keep: int | None = None, *, kind: RecordingKind = RecordingKind.UTTERANCE) -> str | None:
    """
    Write one turn's recording of `kind` and prune the device back to `keep`
    files of that kind (the kind's retention when None).

    Returns the filename to store on the turn row, or None if nothing was
    written. Blocking (runs in an executor at the call site).
    """
    name = filename(device_id, turn_id)
    if name is None or not pcm:
        return None
    directory = recordings_dir(db_path, kind=kind)
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / name
    # Write-then-rename: a partially written WAV that the API then serves
    # is worse than no recording at all.
    tmp = path.with_suffix(".wav.part")
    tmp.write_bytes(encode_wav(pcm))
    tmp.replace(path)
    prune(device_id, db_path=db_path, keep=keep, kind=kind)
    return name


def list_for(device_id: str, db_path: str | None = None, *,
             kind: RecordingKind = RecordingKind.UTTERANCE) -> list[str]:
    """This device's recording filenames of `kind`, newest turn first."""
    safe = safe_device_id(device_id)
    if safe is None:
        return []
    directory = recordings_dir(db_path, kind=kind)
    if not directory.is_dir():
        return []
    entries: list[tuple[int, str]] = []
    for child in directory.iterdir():
        parsed = parse_filename(child.name)
        if parsed and parsed[0] == safe:
            entries.append((parsed[1], child.name))
    entries.sort(reverse=True)
    return [name for _, name in entries]


def prune(device_id: str, db_path: str | None = None, keep: int | None = None, *,
          kind: RecordingKind = RecordingKind.UTTERANCE) -> list[str]:
    """
    Delete all but the `keep` newest recordings of `kind` for a device (the
    kind's retention when None). Returns the filenames removed. Never
    raises — a failed unlink costs disk, not a turn.
    """
    keep = _KEEP[kind] if keep is None else keep
    directory = recordings_dir(db_path, kind=kind)
    removed: list[str] = []
    for name in list_for(device_id, db_path, kind=kind)[max(keep, 0):]:
        try:
            (directory / name).unlink()
            removed.append(name)
        except OSError as e:
            log.warning(f"[recordings] Could not prune {name}: {e}")
    return removed


def resolve(device_id: str, name: str, db_path: str | None = None, *,
            kind: RecordingKind = RecordingKind.UTTERANCE) -> Path | None:
    """
    Path of an existing recording of `kind`, or None. The filename must parse
    AND belong to `device_id` — the API takes both from the URL, and without
    the ownership check one device's turn id would serve another's audio.
    """
    parsed = parse_filename(name)
    safe   = safe_device_id(device_id)
    if parsed is None or safe is None or parsed[0] != safe:
        return None
    path = recordings_dir(db_path, kind=kind) / name
    return path if path.is_file() else None


def delete_device(device_id: str, db_path: str | None = None) -> int:
    """Remove every recording of every kind for a device. Returns the count deleted."""
    return sum(len(prune(device_id, db_path=db_path, keep=0, kind=kind)) for kind in RecordingKind)
