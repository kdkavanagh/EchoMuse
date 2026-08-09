"""
em_sounds.py — user-uploaded notification sounds (timer ring)
==============================================================

A timer that fires has to make a noise, and the noise should be the user's
choice. Uploads land in `sounds/` beside the SQLite database — inside the
persisted data volume in Docker, so a sound survives an image upgrade, the
same arrangement `em_oww_models` uses for custom wake models.

**Why the sound lives here and not on the device.** Home Assistant owns the
countdown: it pushes a single `VoiceAssistantTimerEventResponse` FINISHED
over the ESPHome socket and immediately forgets the timer
(`homeassistant/components/intent/timers.py`, `_timer_finished` pops it from
the registry). That event reaches the device only by way of this controller,
so a device holding its own copy of the sound would still be unable to ring
without us — on-device storage would buy no autonomy, only a firmware
change. Decoding here also means the device never needs an MP3 decoder.

Decode is to 48kHz mono S16_LE — the device wire rate, so the PCM handed to
`stream_speaker` never resamples, matching what `_fetch_tts_audio` does for
TTS. ffmpeg does the work (it is already in the image for exactly that), so
whatever container the user uploads is accepted: MP3, WAV, FLAC, OGG, M4A.

Decoding a several-second sound costs ~100ms, which is too long to spend
while a timer is going off, so the PCM is cached beside the upload as
`<stem>.pcm` and reused until the source file's mtime/size changes. The
cache is a plain file rather than an in-memory blob so it survives a
controller restart — the first ring after a redeploy should not be the slow
one.

This module is pure path/filesystem/subprocess logic — no aiohttp, no db —
so the resolution and validation rules are unit-testable without a live
device. The HTTP endpoints in em_api.py are thin wrappers.
"""

from __future__ import annotations

import asyncio
import logging
import os
import re
from pathlib import Path

log = logging.getLogger("echomuse.sounds")

SOUNDS_SUBDIR = "sounds"

# Device wire format. Must match SPEAKER_RATE in em_controller — there is a
# test. Anything else means stream_speaker resamples, or worse, plays the
# sound at the wrong pitch.
SAMPLE_RATE = 48000
CHANNELS = 1
SAMPLE_BYTES = 2

# Generous for a ring tone; a whole song is not a notification sound, and
# the ring loops anyway. Rejected at upload rather than truncated so the
# user finds out immediately instead of at 6am.
MAX_UPLOAD_BYTES = 10 * 1024 * 1024

# Decoded PCM ceiling, enforced after ffmpeg. An MP3 is compressed ~10:1, so
# a file inside MAX_UPLOAD_BYTES can still decode to something absurd; at
# 96kB/s of PCM this caps a single ring at ~60s.
MAX_PCM_BYTES = 60 * SAMPLE_RATE * CHANNELS * SAMPLE_BYTES

# Accepted upload extensions. ffmpeg would happily take more, but every
# extra one is a format we have not listened to on this hardware.
ALLOWED_SUFFIXES = (".mp3", ".wav", ".flac", ".ogg", ".m4a")

# Sound ids are used as filename stems and appear in URLs, so they get the
# same treatment as wake-model stems.
_STEM_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")

# The fleet-wide default sound's id. A device with no sound of its own rings
# with this one; if it does not exist either, the ring falls back to the
# built-in tone (see builtin_pcm).
DEFAULT_ID = "default"

# Built-in fallback: a two-tone chime synthesised on demand, so a fresh
# install rings even before anyone uploads anything. Deliberately not a
# bundled audio file — a few lines of numpy beats carrying a binary blob in
# the repo, and it can never go missing.
_BUILTIN_TONE_HZ = (880.0, 660.0)
_BUILTIN_BEEP_S = 0.18
_BUILTIN_GAP_S = 0.12


def sounds_dir(db_path: str | None = None) -> Path:
    """
    Resolve the sounds directory: `sounds/` beside the SQLite DB (DB_PATH
    env, same default as em_controller). Absolute, so it stays valid
    regardless of the process cwd.
    """
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / SOUNDS_SUBDIR


def safe_sound_id(sound_id: str) -> str | None:
    """
    Validate a sound id. Returns it unchanged, or None if unacceptable.

    Rejects path separators, hidden files and exotic characters outright
    rather than rewriting them — a silently renamed id would not match the
    one stored in config.
    """
    sid = (sound_id or "").strip()
    if os.path.basename(sid) != sid:
        return None
    if not _STEM_RE.match(sid):
        return None
    return sid


def safe_upload_suffix(filename: str) -> str | None:
    """The accepted lowercase extension of an upload, or None."""
    suffix = Path(os.path.basename(filename or "")).suffix.lower()
    return suffix if suffix in ALLOWED_SUFFIXES else None


def source_path(sound_id: str, directory: Path | None = None) -> Path | None:
    """
    The stored upload for this id, whatever container it was uploaded in,
    or None if there isn't one.
    """
    sid = safe_sound_id(sound_id)
    if sid is None:
        return None
    directory = directory if directory is not None else sounds_dir()
    for suffix in ALLOWED_SUFFIXES:
        p = directory / f"{sid}{suffix}"
        if p.is_file():
            return p
    return None


def cache_path(source: Path) -> Path:
    """The decoded-PCM cache file for a given upload."""
    return source.with_suffix(".pcm")


def _cache_is_fresh(source: Path, cache: Path) -> bool:
    """
    Is the cached PCM still the decode of this exact source?

    Compares mtime and size rather than hashing: re-uploading a sound
    rewrites the file, and either field changing is enough to invalidate.
    The stamp lives in a sidecar because the PCM itself has nowhere to put
    metadata.
    """
    stamp = cache.with_suffix(".pcm.stamp")
    if not (cache.is_file() and stamp.is_file()):
        return False
    try:
        st = source.stat()
        return stamp.read_text().strip() == f"{int(st.st_mtime)}:{st.st_size}"
    except OSError:
        return False


def _write_cache_stamp(source: Path, cache: Path) -> None:
    try:
        st = source.stat()
        cache.with_suffix(".pcm.stamp").write_text(f"{int(st.st_mtime)}:{st.st_size}")
    except OSError as e:
        # A missing stamp only costs a re-decode next time.
        log.warning(f"[sounds] Could not write cache stamp for {source.name}: {e}")


async def decode(source: Path) -> bytes:
    """
    Decode an uploaded sound to 48kHz mono S16_LE PCM.

    Raises RuntimeError with ffmpeg's own stderr tail on failure — "decode
    failed" alone gives the user nothing to act on, and the usual cause
    (a file that is not really audio) is stated plainly by ffmpeg.
    """
    proc = await asyncio.create_subprocess_exec(
        "ffmpeg", "-nostdin", "-loglevel", "error",
        "-i", str(source),
        "-f", "s16le", "-acodec", "pcm_s16le",
        "-ar", str(SAMPLE_RATE), "-ac", str(CHANNELS),
        "-",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    pcm, err = await proc.communicate()
    if proc.returncode != 0:
        tail = (err or b"").decode("utf-8", "replace").strip().splitlines()
        raise RuntimeError(tail[-1] if tail else f"ffmpeg exited {proc.returncode}")
    if not pcm:
        raise RuntimeError("decoded to zero samples — is this really an audio file?")
    if len(pcm) > MAX_PCM_BYTES:
        # Truncate on a frame boundary rather than reject: the upload was
        # already accepted and stored, and a ring that loops does not need
        # the whole file. Reported so the surprise is visible.
        log.warning(
            f"[sounds] {source.name} decoded to {len(pcm):,}b — truncating to "
            f"{MAX_PCM_BYTES:,}b ({MAX_PCM_BYTES // (SAMPLE_RATE * SAMPLE_BYTES)}s)"
        )
        pcm = pcm[: MAX_PCM_BYTES - (MAX_PCM_BYTES % (CHANNELS * SAMPLE_BYTES))]
    return pcm


async def pcm_for(sound_id: str, directory: Path | None = None) -> bytes | None:
    """
    Decoded PCM for a sound id, via the on-disk cache. None if the id has
    no stored upload.

    Decode failures return None rather than raising: the caller is a timer
    going off, and a corrupt upload should degrade to the built-in tone,
    not silence the ring with a traceback.
    """
    source = source_path(sound_id, directory)
    if source is None:
        return None
    cache = cache_path(source)
    if _cache_is_fresh(source, cache):
        try:
            return cache.read_bytes()
        except OSError as e:
            log.warning(f"[sounds] Cache read failed for {source.name}: {e} — re-decoding")
    try:
        pcm = await decode(source)
    except Exception as e:
        log.error(f"[sounds] Decode failed for {source.name}: {e}")
        return None
    try:
        cache.write_bytes(pcm)
        _write_cache_stamp(source, cache)
    except OSError as e:
        log.warning(f"[sounds] Could not cache PCM for {source.name}: {e}")
    return pcm


def builtin_pcm() -> bytes:
    """
    The fallback ring: two descending tones, shaped so they don't click.

    Used when a device has no sound configured, the configured one has been
    deleted, or its file will not decode. A timer that fires silently is the
    worst possible outcome here, so this path has no way to fail.
    """
    import numpy as np

    parts = []
    for hz in _BUILTIN_TONE_HZ:
        n = int(SAMPLE_RATE * _BUILTIN_BEEP_S)
        t = np.arange(n, dtype=np.float64) / SAMPLE_RATE
        wave = np.sin(2 * np.pi * hz * t)
        # 8ms raised-cosine edges — a bare sine switched on and off pops,
        # and the pop is louder than the tone on this speaker.
        edge = max(1, int(SAMPLE_RATE * 0.008))
        ramp = 0.5 * (1 - np.cos(np.linspace(0, np.pi, edge)))
        wave[:edge] *= ramp
        wave[-edge:] *= ramp[::-1]
        parts.append((wave * 0.5 * 32767).astype(np.int16))
        parts.append(np.zeros(int(SAMPLE_RATE * _BUILTIN_GAP_S), dtype=np.int16))
    return np.concatenate(parts).tobytes()


async def ring_pcm(sound_id: str | None, directory: Path | None = None) -> bytes:
    """
    The PCM to ring with, resolving id → fleet default → built-in tone.

    Never returns empty: every fallback ends at the synthesised chime.
    """
    for candidate in (sound_id, DEFAULT_ID):
        if not candidate:
            continue
        pcm = await pcm_for(candidate, directory)
        if pcm:
            return pcm
    return builtin_pcm()


def duration_seconds(pcm_len: int) -> float:
    """Playback length of a PCM buffer, in seconds."""
    return pcm_len / float(SAMPLE_RATE * CHANNELS * SAMPLE_BYTES)


def scan(directory: Path | None = None) -> list[dict]:
    """
    List stored sounds: [{id, file, size, mtime, seconds}], id-sorted.

    `seconds` is present only when a decoded cache exists — computing it
    otherwise would mean decoding every sound just to render a list.
    Missing dir → empty list.
    """
    directory = directory if directory is not None else sounds_dir()
    if not directory.is_dir():
        return []
    out = []
    for f in sorted(directory.iterdir()):
        if f.suffix.lower() not in ALLOWED_SUFFIXES or not f.is_file():
            continue
        try:
            st = f.stat()
        except OSError:
            continue
        entry = {
            "id": f.stem,
            "file": f.name,
            "size": st.st_size,
            "mtime": int(st.st_mtime),
            "seconds": None,
        }
        cache = cache_path(f)
        if _cache_is_fresh(f, cache):
            try:
                entry["seconds"] = round(duration_seconds(cache.stat().st_size), 2)
            except OSError:
                pass
        out.append(entry)
    return out


def store(sound_id: str, suffix: str, data: bytes,
          directory: Path | None = None) -> Path:
    """
    Write an uploaded sound, replacing any existing one with this id.

    Removing the other-suffix spellings matters: uploading `ring.wav` over
    an existing `ring.mp3` would otherwise leave both on disk, and
    source_path would keep resolving to whichever ALLOWED_SUFFIXES lists
    first — the old one.
    """
    directory = directory if directory is not None else sounds_dir()
    directory.mkdir(parents=True, exist_ok=True)
    for existing in ALLOWED_SUFFIXES:
        old = directory / f"{sound_id}{existing}"
        if old.is_file():
            _unlink_quietly(old)
            _unlink_quietly(cache_path(old))
            _unlink_quietly(cache_path(old).with_suffix(".pcm.stamp"))
    dest = directory / f"{sound_id}{suffix}"
    dest.write_bytes(data)
    return dest


def delete(sound_id: str, directory: Path | None = None) -> bool:
    """Remove a sound and its decoded cache. False if it wasn't there."""
    source = source_path(sound_id, directory)
    if source is None:
        return False
    _unlink_quietly(source)
    _unlink_quietly(cache_path(source))
    _unlink_quietly(cache_path(source).with_suffix(".pcm.stamp"))
    return True


def _unlink_quietly(p: Path) -> None:
    try:
        p.unlink()
    except OSError:
        pass


def in_use_by(sound_id: str, configs: dict[str, dict]) -> list[str]:
    """
    Which config scopes reference this sound? `configs` maps a scope label
    (device id, or "global") to its config dict. Mirrors
    em_oww_models.in_use_by so the dashboard can warn before a delete.
    """
    return [
        scope for scope, cfg in configs.items()
        if (cfg or {}).get("timerSound") == sound_id
    ]
