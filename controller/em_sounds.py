"""
em_sounds.py — the alert sound catalog and its exported alert assets.

Users upload sounds (MP3, WAV, FLAC, OGG, M4A) into `sounds/` beside the SQLite
database, so they persist with the data volume. Catalog IDs are the upload
stems: they are the user-facing names and the values of the `timerSound` and
`alarmSound` config keys (SPEC §16.7, §18.4).

Devices never decode uploads. Each sound is exported as an **alert asset**
(SPEC §16.5): 48 kHz mono PCM16 WAV, at most its first 10 s with a 50 ms
fade-out, named by the SHA-256 of the WAV file (`sounds/assets/<sha256>.wav`).
The alert engine names assets by hash in `alert.ring` and calendar events; the
device fetches them over `/device/v1/assets`. An ID that does not resolve to
an asset is `builtin:fallback`, the tone embedded in the firmware.

Invariants:
- An asset file's name is the SHA-256 of its bytes; assets are never deleted,
  because calendar events may name a hash after its sound was replaced.
- Each export is recorded in `<id>.alert.json` against the upload's
  mtime/size, so a sound is decoded once per upload, and a failed decode is
  not retried until the file changes.
- Everything here is synchronous (ffmpeg via subprocess); async callers use an
  executor.
"""

from __future__ import annotations

import hashlib
import io
import json
import logging
import os
import re
import subprocess
import tempfile
import wave
from dataclasses import dataclass
from pathlib import Path

import numpy as np

log = logging.getLogger("echomuse.sounds")

SOUNDS_SUBDIR = "sounds"
ASSETS_SUBDIR = "assets"

# §16.5 alert asset format.
SAMPLE_RATE = 48_000
CHANNELS = 1
SAMPLE_BYTES = 2
ALERT_MAX_SECONDS = 10
ALERT_MAX_SAMPLES = ALERT_MAX_SECONDS * SAMPLE_RATE      # 960,000 PCM bytes
FADE_OUT_SAMPLES = SAMPLE_RATE * 50 // 1000              # 50 ms

# The device-embedded tone (§16.5) used for any ID that does not resolve.
FALLBACK = "builtin:fallback"

# The fleet-wide default sound's ID: what an empty timerSound/alarmSound rings.
DEFAULT_ID = "default"

# Rejected at upload rather than truncated, so the user learns immediately.
MAX_UPLOAD_BYTES = 10 * 1024 * 1024

# Accepted upload extensions: the formats listened to on this hardware.
ALLOWED_SUFFIXES = (".mp3", ".wav", ".flac", ".ogg", ".m4a")

# Sound IDs are filename stems and URL segments.
_STEM_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$")
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")

_SIDECAR_SUFFIX = ".alert.json"
_FFMPEG_TIMEOUT_S = 60


class ExportError(Exception):
    """The sound has no upload, or its upload does not decode to audio."""


@dataclass(frozen=True)
class AlertExport:
    sha256: str       # of the WAV file bytes; the asset's name
    seconds: float    # exported length, ≤ ALERT_MAX_SECONDS
    shortened: bool   # the upload was longer and was cut (dashboard warning)


def sounds_dir(db_path: str | None = None) -> Path:
    """`sounds/` beside the SQLite DB (DB_PATH env by default), absolute."""
    if db_path is None:
        db_path = os.environ.get("DB_PATH", "echomuse.db")
    return Path(db_path).resolve().parent / SOUNDS_SUBDIR


def assets_dir(directory: Path | None = None) -> Path:
    """Where exported alert assets live: `<sounds>/assets/`."""
    return (directory if directory is not None else sounds_dir()) / ASSETS_SUBDIR


def safe_sound_id(sound_id: str) -> str | None:
    """The ID unchanged if acceptable, else None. Never rewritten: a renamed
    ID would not match the one stored in config."""
    sid = (sound_id or "").strip()
    if os.path.basename(sid) != sid or not _STEM_RE.match(sid):
        return None
    return sid


def safe_upload_suffix(filename: str) -> str | None:
    """The accepted lowercase extension of an upload, or None."""
    suffix = Path(os.path.basename(filename or "")).suffix.lower()
    return suffix if suffix in ALLOWED_SUFFIXES else None


def source_path(sound_id: str, directory: Path | None = None) -> Path | None:
    """The stored upload for this ID in whatever container it came, or None."""
    sid = safe_sound_id(sound_id)
    if sid is None:
        return None
    directory = directory if directory is not None else sounds_dir()
    for suffix in ALLOWED_SUFFIXES:
        p = directory / f"{sid}{suffix}"
        if p.is_file():
            return p
    return None


def _sidecar_path(directory: Path, sound_id: str) -> Path:
    return directory / f"{sound_id}{_SIDECAR_SUFFIX}"


def _stamp(source: Path) -> str:
    st = source.stat()
    return f"{source.name}:{st.st_mtime_ns}:{st.st_size}"


def _atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "wb") as f:
            f.write(data)
        os.replace(tmp, path)
    except BaseException:
        Path(tmp).unlink(missing_ok=True)
        raise


def _fresh_record(source: Path, directory: Path, sound_id: str) -> dict | None:
    """The sidecar record if it describes this exact upload, else None."""
    try:
        record = json.loads(_sidecar_path(directory, sound_id).read_text())
        return record if record.get("stamp") == _stamp(source) else None
    except (OSError, ValueError, AttributeError):
        return None


def _decode(source: Path) -> np.ndarray:
    """Decode an upload to 48 kHz mono int16, bounded to one second past the
    alert limit (enough to know it was longer)."""
    try:
        proc = subprocess.run(
            ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(source),
             "-t", str(ALERT_MAX_SECONDS + 1),
             "-f", "s16le", "-acodec", "pcm_s16le",
             "-ar", str(SAMPLE_RATE), "-ac", str(CHANNELS), "-"],
            capture_output=True, timeout=_FFMPEG_TIMEOUT_S, check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        raise ExportError(f"ffmpeg failed: {e}") from e
    if proc.returncode != 0:
        tail = proc.stderr.decode("utf-8", "replace").strip().splitlines()
        raise ExportError(tail[-1] if tail else f"ffmpeg exited {proc.returncode}")
    pcm = np.frombuffer(proc.stdout[: len(proc.stdout) // SAMPLE_BYTES * SAMPLE_BYTES],
                        dtype="<i2")
    if pcm.size == 0:
        raise ExportError("decoded to zero samples — is this really an audio file?")
    return pcm


def render_alert_wav(pcm: np.ndarray) -> tuple[bytes, bool]:
    """
    §16.5 alert asset from 48 kHz mono int16 PCM: its first ALERT_MAX_SECONDS
    with a linear 50 ms fade-out ending at zero, as a canonical WAV (same PCM
    → same bytes → same SHA-256). Returns (wav, shortened).
    """
    if pcm.size == 0:
        raise ExportError("no audio")
    shortened = pcm.size > ALERT_MAX_SAMPLES
    x = pcm[:ALERT_MAX_SAMPLES].astype(np.float64)
    n = min(x.size, FADE_OUT_SAMPLES)
    x[x.size - n:] *= np.linspace(1.0, 0.0, n)
    out = np.clip(np.rint(x), -32768, 32767).astype("<i2")
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(CHANNELS)
        w.setsampwidth(SAMPLE_BYTES)
        w.setframerate(SAMPLE_RATE)
        w.writeframes(out.tobytes())
    return buf.getvalue(), shortened


def export(sound_id: str, directory: Path | None = None) -> AlertExport:
    """
    The alert asset for a catalog sound, exporting it when the upload is new
    or changed. Raises ExportError when the ID has no upload or the upload
    does not decode (the failure is remembered until the file changes).
    OSError from writing the asset propagates.
    """
    directory = directory if directory is not None else sounds_dir()
    source = source_path(sound_id, directory)
    if source is None:
        raise ExportError(f"no sound {sound_id!r}")
    record = _fresh_record(source, directory, sound_id)
    if record is not None:
        if record.get("error"):
            raise ExportError(record["error"])
        if (assets_dir(directory) / f"{record['sha256']}.wav").is_file():
            return AlertExport(record["sha256"], record["seconds"], record["shortened"])

    stamp = _stamp(source)
    try:
        wav, shortened = render_alert_wav(_decode(source))
    except ExportError as e:
        _atomic_write(_sidecar_path(directory, sound_id),
                      json.dumps({"stamp": stamp, "error": str(e)}).encode())
        raise
    sha = hashlib.sha256(wav).hexdigest()
    seconds = round((len(wav) - 44) / (SAMPLE_RATE * SAMPLE_BYTES), 3)
    asset = assets_dir(directory) / f"{sha}.wav"
    if not asset.is_file():
        _atomic_write(asset, wav)
    _atomic_write(_sidecar_path(directory, sound_id), json.dumps({
        "stamp": stamp, "sha256": sha, "seconds": seconds, "shortened": shortened,
        "error": None,
    }).encode())
    log.info(f"[sounds] Exported {source.name} → {sha[:12]}… ({seconds}s"
             f"{', shortened' if shortened else ''})")
    return AlertExport(sha, seconds, shortened)


def alert_asset(sound_id: str | None, directory: Path | None = None) -> tuple[str, bool]:
    """
    Resolve a configured sound to (asset sha256 | FALLBACK, flagged).

    Empty means "no choice": the fleet `default` upload if there is one, else
    FALLBACK unflagged. A named ID that has no upload or does not decode is
    FALLBACK and flagged (§16.7); so is a `default` upload that fails.
    """
    directory = directory if directory is not None else sounds_dir()
    sid = sound_id or DEFAULT_ID
    try:
        return export(sid, directory).sha256, False
    except (ExportError, OSError) as e:
        explicit = bool(sound_id) or source_path(DEFAULT_ID, directory) is not None
        if explicit:
            log.warning(f"[sounds] {sid!r} does not resolve ({e}) — using {FALLBACK}")
        return FALLBACK, explicit


def preview_asset(sound_id: str | None, directory: Path | None = None) -> str:
    """The asset a dashboard preview of this sound plays (sha256 or FALLBACK)."""
    return alert_asset(sound_id, directory)[0]


def asset_path(sha256: str, directory: Path | None = None) -> Path | None:
    """The exported asset file for a hash, or None."""
    if not _SHA256_RE.match(sha256 or ""):
        return None
    p = assets_dir(directory) / f"{sha256}.wav"
    return p if p.is_file() else None


def scan(directory: Path | None = None) -> list[dict]:
    """
    The catalog: [{id, file, size, mtime, seconds, sha256, shortened}], id-
    sorted. The last three describe the current upload's alert asset and are
    None until it has been exported successfully.
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
        record = _fresh_record(f, directory, f.stem) or {}
        ok = bool(record.get("sha256"))
        out.append({
            "id": f.stem,
            "file": f.name,
            "size": st.st_size,
            "mtime": int(st.st_mtime),
            "seconds": record.get("seconds") if ok else None,
            "sha256": record.get("sha256") if ok else None,
            "shortened": record.get("shortened") if ok else None,
        })
    return out


def store(sound_id: str, suffix: str, data: bytes,
          directory: Path | None = None) -> Path:
    """
    Write an upload, replacing any existing sound with this ID in any
    container (otherwise source_path could keep resolving the old file).
    Call export() afterwards.
    """
    directory = directory if directory is not None else sounds_dir()
    directory.mkdir(parents=True, exist_ok=True)
    for existing in ALLOWED_SUFFIXES:
        (directory / f"{sound_id}{existing}").unlink(missing_ok=True)
    _sidecar_path(directory, sound_id).unlink(missing_ok=True)
    dest = directory / f"{sound_id}{suffix}"
    _atomic_write(dest, data)
    return dest


def delete(sound_id: str, directory: Path | None = None) -> bool:
    """Remove a sound and its export record (not its asset). False if absent."""
    directory = directory if directory is not None else sounds_dir()
    source = source_path(sound_id, directory)
    if source is None:
        return False
    source.unlink(missing_ok=True)
    _sidecar_path(directory, source.stem).unlink(missing_ok=True)
    return True


def in_use_by(sound_id: str, configs: dict[str, dict]) -> list[str]:
    """Config scopes (label → config) naming this sound as timerSound or
    alarmSound, so the dashboard can warn before a delete."""
    return [
        scope for scope, cfg in configs.items()
        if sound_id in ((cfg or {}).get("timerSound"), (cfg or {}).get("alarmSound"))
    ]
