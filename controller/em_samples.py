"""
em_samples.py — continuous capture, cut into clips, for wake-word training
===========================================================================

Training a custom wake word (`oww_forge/`) on synthetic TTS positives gets a
model that works in the room the recordings were never made in. What it
cannot supply is *this* speaker at *this* distance through *this* mic array,
which is exactly the material that fixes a model that near-misses at 0.4 all
evening. Collecting that used to mean sitting in front of a laptop with
`arecord`, which records the wrong microphone.

Sample collection mode records from the Echo instead. It needs nothing new
from the device: the always-on wake stream is already continuous, ungated and
AGC-free (see the mic pipeline notes in CLAUDE.md), so every 80ms frame of it
already arrives at the controller whether anyone said anything or not. The
whole feature is therefore what this module does with those frames — chop
them into clips at the silences and write each clip to disk — plus a mode
flag that stops the same frames from starting a voice turn.

**The audio is exactly what the wake model scores**, byte for byte, because
it is tapped at the same point in `wake_word_listener`. That is the property
that makes the clips worth training on: a sample captured through a different
gain, denoiser or resampler teaches the model about a path the model will
never see in service.

Segmentation is energy-relative, for the reason `em_endpoint` is: an absolute
RMS threshold is a room's property, not a setting, and one that works in a
quiet study is deaf in a kitchen. The difference is which end it is measured
from. The endpointer measures DOWN from a tracked maximum, because it already
knows the speaker is talking and is asking when they stop. Here there is no
wake word to anchor on and the question is the opposite one — *did anything
louder than this room just happen* — so the threshold is measured UP from a
tracked noise floor.

Both halves of that need bounding:

  - The floor is FROZEN while a clip is open. Otherwise a few seconds of
    speech drags it up and the clip closes on the speaker's own level.
  - The floor is CLAMPED at `ABS_FLOOR_DB` for thresholding. A muted device
    sends zero-filled frames (hardware mute still produces frames), and a
    floor tracking true digital silence would put the open threshold at
    -168dB, where thermal noise in the ADC is a shout. The tracker keeps its
    real value; only the comparison is clamped.

Pure logic and filesystem work — no numpy, no aiohttp, no db import, and the
caller passes the frame RMS it has already computed. Storage mirrors
`em_recordings`: files beside the SQLite DB so they sit inside the persisted
Docker volume. Retention is a hard per-device file count, an order of
magnitude larger than the recordings one because a training set is the point
here rather than a diagnostic sample.
"""

from __future__ import annotations

import io
import logging
import math
import os
import re
import time
import wave
from dataclasses import dataclass
from pathlib import Path

log = logging.getLogger("echomuse.samples")

SAMPLES_SUBDIR = "samples"

# Wire format of the wake stream (em_controller.CHUNK_BYTES is 80ms of it).
SAMPLE_RATE  = 16000
SAMPLE_WIDTH = 2
CHANNELS     = 1

# How many clips to keep per device before the oldest are dropped. Sized for
# a training set rather than a diagnostic: openWakeWord's own docs suggest a
# few hundred real positives is where a custom model starts behaving, and a
# 1s clip is ~32kB, so the whole cap is ~64MB per device.
KEEP_PER_DEVICE = 2000

# dBFS reported for a digitally silent frame. Silence must read as a number,
# never -inf — the same reason em_endpoint.DB_FLOOR exists.
DB_FLOOR = -100.0

# Lowest noise floor used for thresholding, whatever the tracker believes.
# See the module docstring: a muted device streams zeroes, and an unclamped
# floor turns the quantisation noise that follows an unmute into "speech".
# Measured room floors on this fleet sit around -66dBFS after micGainDb, so
# this is well below anything real.
ABS_FLOOR_DB = -75.0


@dataclass
class SegmentConfig:
    """
    How the stream is cut. Every value is a duration or a dB margin — there
    is no absolute level anywhere, so the same config works in a study and a
    kitchen.
    """

    # How far above the noise floor a frame must sit to START a clip, and how
    # far above it must stay to keep one open. The gap between them is
    # hysteresis: with one threshold, a frame sitting on the boundary opens
    # and closes a clip every 80ms.
    open_margin_db:  float = 12.0
    close_margin_db: float = 6.0

    # Audio kept from BEFORE the frame that crossed the open threshold. A
    # wake word's first phoneme is its quietest — "Jarvis" opens on the "ar",
    # not the "J" — and a model trained on clips missing their onset learns
    # to want a word that nobody says. Non-negotiable, not a nicety.
    preroll_ms: int = 320

    # Below-threshold audio that ends a clip, and how much of it is kept.
    # Shorter than a turn endpointer's silence window on purpose: this is
    # cutting single words apart, not deciding when a sentence finished, and
    # a long window welds "hey jarvis / what's the weather" into one clip.
    silence_ms: int = 400
    tail_ms:    int = 240

    # Clips whose SPEECH span is shorter than this are discarded — a door
    # click, a chair, one syllable of a cough. Measured on the speech span
    # rather than the file so preroll+tail cannot make 40ms of nothing look
    # like a 600ms clip.
    min_clip_ms: int = 300

    # A clip is cut here whatever the level is doing. A wake word is ~1s; a
    # cap this size holds a short sentence, and its real job is to stop a
    # television or a running tap producing one clip that never ends.
    max_clip_ms: int = 6000

    # How fast the noise floor tracks. Asymmetric for the reason the wake
    # listener's own floor is: it must fall to a newly quiet room quickly and
    # rise slowly, so a burst of speech cannot drag it up. Per frame; at
    # 12.5 frames/s the rise is a ~4s time constant.
    fall_alpha: float = 0.3
    rise_alpha: float = 0.02

    def __post_init__(self) -> None:
        if self.close_margin_db > self.open_margin_db:
            raise ValueError("close_margin_db must not exceed open_margin_db")
        if self.min_clip_ms > self.max_clip_ms:
            raise ValueError("min_clip_ms must not exceed max_clip_ms")
        if not 0.0 < self.fall_alpha <= 1.0:
            raise ValueError("fall_alpha must be in (0, 1]")
        if not 0.0 < self.rise_alpha <= 1.0:
            raise ValueError("rise_alpha must be in (0, 1]")


DEFAULT_CONFIG = SegmentConfig()


@dataclass
class Clip:
    """One cut segment, ready to write."""
    pcm:       bytes
    speech_ms: int      # onset → last above-threshold frame (what min_clip_ms judges)
    peak_db:   float
    truncated: bool     # hit max_clip_ms rather than a silence

    @property
    def duration_ms(self) -> int:
        return duration_ms(len(self.pcm))


@dataclass
class SegmentStats:
    """
    What the segmenter has done since it was created, for the dashboard.

    `dropped_short` is the interesting one: a device reporting hundreds of
    them is picking up a mechanical noise rather than speech, which is a
    placement problem no amount of collecting will fix.
    """
    clips:         int = 0
    dropped_short: int = 0
    truncated:     int = 0
    frames:        int = 0


def db_of(rms: float) -> float:
    """dBFS of a normalised RMS (0.0–1.0). Silence reads DB_FLOOR."""
    if rms <= 0.0:
        return DB_FLOOR
    return max(DB_FLOOR, 20.0 * math.log10(rms))


def duration_ms(pcm_len: int) -> int:
    """Playing time of `pcm_len` bytes of the wire format, in ms."""
    return int(pcm_len / (SAMPLE_RATE * SAMPLE_WIDTH * CHANNELS) * 1000)


class Segmenter:
    """
    Frames in, clips out.

    Feed every frame of the wake stream through `push()`; it returns a Clip
    on the frame that closes one and None otherwise. Nothing here blocks or
    allocates per frame beyond the frame itself, so it is safe on the
    controller's event loop — the WRITE is what needs an executor.
    """

    def __init__(self, config: SegmentConfig | None = None):
        self.cfg   = config or DEFAULT_CONFIG
        self.stats = SegmentStats()
        # None until the first frame: seeding the floor at a constant would
        # spend the first seconds of every session converging away from it.
        self._floor: float | None = None
        self._preroll: list[bytes] = []
        self._preroll_ms = 0
        self._open  = False
        self._frames: list[bytes] = []
        self._ms          = 0     # audio held in _frames
        self._pad_ms      = 0     # of that, preroll captured before the onset
        self._speech_ms   = 0     # start of _frames → last above-close frame
        self._silence_ms  = 0     # run of below-close-threshold audio
        self._peak_db     = DB_FLOOR
        # After a truncated clip, refuse to open again until the level has
        # actually dropped. Without it a continuous source (a television, a
        # tap) emits a back-to-back clip every max_clip_ms forever, each one
        # cut mid-word and none of them worth training on.
        self._armed = True

    # ── the floor ────────────────────────────────────────────────────────

    @property
    def floor_db(self) -> float:
        """The tracker's current value, unclamped. Diagnostics only."""
        return DB_FLOOR if self._floor is None else self._floor

    def _threshold_floor(self) -> float:
        return max(ABS_FLOOR_DB, self.floor_db)

    @property
    def open_db(self) -> float:
        """Level a frame must reach to start a clip, right now."""
        return self._threshold_floor() + self.cfg.open_margin_db

    @property
    def close_db(self) -> float:
        """Level a frame must hold to keep a clip open, right now."""
        return self._threshold_floor() + self.cfg.close_margin_db

    # ── the loop ─────────────────────────────────────────────────────────

    def push(self, pcm: bytes, rms: float) -> Clip | None:
        """
        Consume one frame. Returns a Clip if this frame closed one.

        `rms` is the caller's already-computed normalised RMS for `pcm` —
        the wake listener has it for its own noise-floor tracking, and
        recomputing it here would mean numpy in a module that otherwise
        needs none.
        """
        if not pcm:
            return None
        cfg     = self.cfg
        frame_ms = duration_ms(len(pcm))
        db      = db_of(rms)
        self.stats.frames += 1

        # The floor is frozen while a clip is open — see the module docstring.
        if self._floor is None:
            self._floor = db
        elif not self._open:
            alpha = cfg.fall_alpha if db < self._floor else cfg.rise_alpha
            self._floor += alpha * (db - self._floor)

        if not self._open:
            self._remember(pcm, frame_ms)
            if db < self.close_db:
                # Quiet again: a truncated clip's cooldown is over.
                self._armed = True
            if self._armed and db >= self.open_db:
                self._start(db, frame_ms)
            return None

        # Open.
        self._frames.append(pcm)
        self._ms += frame_ms
        self._peak_db = max(self._peak_db, db)
        if db >= self.close_db:
            self._speech_ms  = self._ms
            self._silence_ms = 0
        else:
            self._silence_ms += frame_ms

        if self._silence_ms >= cfg.silence_ms:
            return self._close(truncated=False)
        if self._ms >= cfg.max_clip_ms:
            self._armed = False
            return self._close(truncated=True)
        return None

    def flush(self) -> Clip | None:
        """
        Close whatever is open, as if the silence had arrived.

        Called when collection is turned off or the device disconnects: the
        alternative is discarding a clip that is complete except for its
        trailing silence, which on a device switched off mid-word is the
        clip someone just recorded on purpose.
        """
        if not self._open:
            return None
        return self._close(truncated=False)

    # ── internals ────────────────────────────────────────────────────────

    def _remember(self, pcm: bytes, frame_ms: int) -> None:
        """Hold the last preroll_ms of pre-onset audio."""
        self._preroll.append(pcm)
        self._preroll_ms += frame_ms
        while self._preroll and self._preroll_ms - duration_ms(len(self._preroll[0])) >= self.cfg.preroll_ms:
            self._preroll_ms -= duration_ms(len(self._preroll[0]))
            self._preroll.pop(0)

    def _start(self, db: float, frame_ms: int) -> None:
        # The crossing frame is already the last entry in _preroll (push
        # remembers before it decides), so the clip starts with it and
        # everything held before it becomes the pad.
        self._open       = True
        self._frames     = list(self._preroll)
        self._ms         = sum(duration_ms(len(f)) for f in self._frames)
        self._pad_ms     = max(0, self._ms - frame_ms)
        self._speech_ms  = self._ms
        self._silence_ms = 0
        self._peak_db    = db
        self._preroll    = []
        self._preroll_ms = 0

    def _close(self, truncated: bool) -> Clip | None:
        cfg = self.cfg
        # Keep tail_ms past the last frame that was above the close
        # threshold, and drop the rest of the silence. Trimming by TIME
        # rather than frame count so a caller feeding a different frame
        # size gets the same clip.
        keep_ms  = self._speech_ms + cfg.tail_ms
        kept: list[bytes] = []
        so_far   = 0
        for frame in self._frames:
            if so_far >= keep_ms:
                break
            kept.append(frame)
            so_far += duration_ms(len(frame))

        # The speech span, not the file: preroll and tail are padding and
        # must not be able to promote a click into a clip. The pad actually
        # captured is subtracted, not the configured preroll — the first
        # clip of a session has less of it, and assuming otherwise would
        # discard exactly the clip someone recorded to test the feature.
        speech_span = max(0, self._speech_ms - self._pad_ms)
        peak        = self._peak_db
        self._reset_open()

        if speech_span < cfg.min_clip_ms:
            self.stats.dropped_short += 1
            return None
        self.stats.clips += 1
        if truncated:
            self.stats.truncated += 1
        return Clip(
            pcm=b"".join(kept),
            speech_ms=int(speech_span),
            peak_db=round(peak, 1),
            truncated=truncated,
        )

    def _reset_open(self) -> None:
        self._open       = False
        self._frames     = []
        self._ms         = 0
        self._pad_ms     = 0
        self._speech_ms  = 0
        self._silence_ms = 0
        self._peak_db    = DB_FLOOR
        self._preroll    = []
        self._preroll_ms = 0


# ─── storage ──────────────────────────────────────────────────────────────────

# `<epoch_ms>.wav`, inside a per-device directory. The device id is a
# directory rather than a filename prefix because a collecting device
# produces thousands of these and one flat directory would be listed in full
# on every write.
_NAME_RE   = re.compile(r"^(?P<ts>\d{10,17})\.wav$")
_DEVICE_RE = re.compile(r"[A-Za-z0-9_.-]{1,64}")


def samples_dir(db_path: str | None = None) -> Path:
    """`samples/` beside the SQLite DB. Absolute, so cwd cannot move it."""
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / SAMPLES_SUBDIR


def safe_device_id(device_id: str) -> str | None:
    """The device id as a path component, or None if it isn't one."""
    if device_id and _DEVICE_RE.fullmatch(device_id):
        return device_id
    return None


def device_dir(device_id: str, db_path: str | None = None) -> Path | None:
    safe = safe_device_id(device_id)
    if safe is None:
        return None
    return samples_dir(db_path) / safe


def filename(when_ms: int) -> str:
    return f"{int(when_ms)}.wav"


def parse_filename(name: str) -> int | None:
    """The clip's epoch-ms timestamp, or None if the name is not ours."""
    m = _NAME_RE.match(name)
    return int(m.group("ts")) if m else None


def encode_wav(pcm: bytes) -> bytes:
    """PCM frames → a WAV container, in memory."""
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(CHANNELS)
        w.setsampwidth(SAMPLE_WIDTH)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(pcm)
    return buf.getvalue()


def save_clip(device_id: str, clip: Clip, when_ms: int | None = None,
              db_path: str | None = None,
              keep: int = KEEP_PER_DEVICE) -> str | None:
    """
    Write one clip and prune the device back to `keep` files.

    Returns the filename, or None if nothing was written. Blocking — call it
    in an executor.

    `when_ms` defaults to now. The wall clock lives here rather than at the
    call site on purpose: em_controller deliberately keeps to monotonic
    clocks (device timestamps are worthless before NTP), and a filename is
    the one place a real date is wanted.

    A clip landing in the same millisecond as an existing one takes the next
    free millisecond rather than overwriting it. Clips are hundreds of ms
    apart by construction, so this only fires when the clock has been
    stepped backwards under us, which is precisely when silently losing the
    older file would be worst.
    """
    directory = device_dir(device_id, db_path)
    if directory is None or not clip.pcm:
        return None
    directory.mkdir(parents=True, exist_ok=True)
    when = int(time.time() * 1000) if when_ms is None else int(when_ms)
    while (directory / filename(when)).exists():
        when += 1
    name = filename(when)
    path = directory / name
    # Write-then-rename: a partially written WAV that the API then serves is
    # worse than no clip at all.
    tmp = path.with_suffix(".wav.part")
    tmp.write_bytes(encode_wav(clip.pcm))
    tmp.replace(path)
    prune(device_id, db_path=db_path, keep=keep)
    return name


def list_for(device_id: str, db_path: str | None = None) -> list[dict]:
    """
    This device's clips, newest first: name, epoch ms, bytes and duration.

    Duration comes from the file size rather than the WAV header — every
    file here is written by encode_wav, and opening thousands of them to
    read a header the writer already fixed would make listing a directory
    scan plus a syscall storm.
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
            # 44 bytes of RIFF header. A negative would mean a truncated
            # file, which reads as 0 rather than as nonsense.
            "ms":    max(0, duration_ms(size - 44)),
        })
    out.sort(key=lambda c: c["ts"], reverse=True)
    return out


def usage(device_id: str, db_path: str | None = None) -> dict:
    """Clip count and total bytes for a device."""
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
            log.warning(f"[samples] Could not prune {clip['name']}: {e}")
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
