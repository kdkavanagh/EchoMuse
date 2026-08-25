#!/home/kyle/.venv/bin/python
"""
playback_matrix.py — re-record a corpus THROUGH the Echo, across a matrix of
media players and volumes.

Why
---

`oww_forge` trains on synthetic TTS positives; `collect_device_clips.py`
collects real speech a person walks up and says. Neither produces the third
thing a wake model wants: a large, already-labelled corpus carrying **this
device's signal chain**. Common Voice on disk is clean studio speech. The
same clips played into the room and captured off the Echo's mic array carry
the array, the HAL's beam selection and gain, the room's reverb and floor,
the distance, and whatever the playing speaker does to the spectrum — every
part of the path the model is scored on in service, and none of which
augmentation can invent from the clean file.

The matrix is what turns one corpus into several. Each source clip is played
from each media player, at each volume configured for that player, and every
combination is a separate recording. Position gives you direction, distance
and reverb; volume gives you SNR.

How it runs
-----------

The controller side is `em_capture`: a recording window opened and closed by
this script, whose audio is POSTed back here as a WAV. That is deliberately
NOT the sample collector — a segmenter cuts one playback into 0..N clips
depending on where the pauses fall, and this script has to pair each
recording with the exact file that produced it.

So per (file, player, volume):

    volume_set -> open window -> play_media -> wait out the clip
               -> stop window -> await the POST -> trim -> write

**Files are the OUTER loop and the matrix is the inner one.** That costs a
`volume_set` per cell instead of one per pass, and it buys the property that
matters when the run has no end: this script is meant to be killed with
Ctrl+C whenever enough has been collected, and with the loops the other way
round that would leave every recording at one player and one volume. Each
source clip is finished across the whole matrix before the next is started,
so whenever it is stopped the set is balanced.

Resume is free and comes from the filenames: a cell whose output file already
exists is skipped. Note the source directory holds ~1.9M files, so it is
streamed with `os.scandir` and never listed, sorted or globbed, and the
resume check is one `stat` per candidate rather than anything that reads the
directory.

Requires: ffmpeg/ffprobe on PATH, and tqdm for the progress bar (optional —
without it the bar degrades to the same information on plain stderr). Runs on
~/.venv, which is where tqdm lives.
"""

from __future__ import annotations

import argparse
import contextlib
import http.server
import json
import os
import queue
import re
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import wave
from dataclasses import dataclass
from pathlib import Path

# ─── Constants ────────────────────────────────────────────────────────────────

# Source extensions worth playing. Common Voice ships mp3; the rest are here
# because a corpus assembled by hand rarely is one format.
DEFAULT_EXTS = (".mp3", ".wav", ".flac", ".ogg", ".opus", ".m4a")

# The controller's wire format for captured audio.
SAMPLE_RATE = 16000

# Where the recordings go, relative to the source directory's parent.
DEFAULT_OUT_NAME = "clips_recorded"

# Settle after a volume change before playing. Music Assistant and shairport
# both apply volume asynchronously, and a clip that starts before the change
# lands is recorded at the PREVIOUS cell's level while being filed under this
# one — a mislabelled sample, which is worse than a missing one.
DEFAULT_SETTLE_MS = 500

# Kept after the source clip's own duration before the window is stopped:
# playback start latency, network buffering in the player, and the tail of
# the room. Generous on purpose — the trim pass takes the excess back off,
# and audio that was never captured cannot be recovered.
DEFAULT_PRE_MS  = 400
DEFAULT_POST_MS = 300

# Pre-roll for an Alexa. Playback is driven from Amazon's cloud — Home
# Assistant hands the utterance to Amazon, which fetches the URL and pushes it
# to the device — so the delay between the service call and a sound in the
# room is seconds, and varies per call. Recording through it and letting the
# trim find the audio is far cheaper than trying to predict it.
# Measured on this fleet: the gap between the notify service call and sound
# in the room was 1.98s / 2.30s / 2.36s across consecutive takes. 5s carries
# roughly double the worst observed, and overshooting only costs run time —
# undershooting loses the clip entirely, since the window would close before
# Amazon got round to playing it. Re-measure from the manifest's onset_s
# column if the pace ever needs tuning.
ALEXA_PRE_MS = 2000

# Silence prepended to the audio handed to Alexa, to separate the word from
# the earcon Alexa plays when it starts an external clip.
#
# The earcon fires on EVERY clip, not only the first. It only sounded like
# the first because it landed ~0.3s ahead of the word and merged with it;
# separating them is what made each one audible. So this is not a margin
# against an occasional event, it is the thing that keeps a ~230ms tone out
# of every recording in the corpus — sized well past the tone plus its decay
# plus the 0.25s locate pad, since the cost of being wrong is an artefact the
# real wake word never has, trained in.
ALEXA_LEAD_MS = 400

# How long past the expected end to wait for the recording to arrive.
DEFAULT_GRACE_S = 25.0

# A capture whose peak sits less than this above its own noise floor recorded
# silence, not audio. One is noise; a run of them means the wrong entity, a
# muted player, or a device that cannot hear it.
SILENT_MARGIN_DB   = 6.0
DEFAULT_MAX_SILENT = 3

# Trim: silence off both ends. Copied deliberately from
# collect_device_clips.py rather than shared — that script's trim is tuned for
# a wake phrase, this one keeps a whole sentence, and a common helper would
# couple two things that want to drift apart. The 0.15s/0.25s floors leave a
# breath either side: a hard cut at the first sample over threshold clips the
# leading phoneme, which is exactly the artefact worth not training in.
SILENCE_FILTER = (
    "silenceremove=start_periods=1:start_threshold={thr}:start_silence=0.15,"
    "areverse,"
    "silenceremove=start_periods=1:start_threshold={thr}:start_silence=0.25,"
    "areverse"
)
# Trim threshold. AUTO by default, and that is not laziness: captured speech
# lands around -45dBFS through this mic path against a room floor near
# -75dBFS, so an absolute default sits at the recording's own MEAN and cuts
# the word in half — measured, a 1.47s clip came back 0.67s. The threshold is
# therefore derived per clip from the two numbers the controller already
# reports: comfortably above the room, comfortably below the speech.
DEFAULT_SILENCE_DB = None
TRIM_ABOVE_FLOOR_DB = 8.0     # never trim anything this close to the room
TRIM_BELOW_PEAK_DB  = 30.0    # never trim anything this close to the speech


def trim_threshold_db(peak_db: float, floor_db: float) -> int:
    """
    Where to put the trim gate for one recording.

    Whichever of the two bounds is HIGHER: a clip with an unusually loud room
    must not have the room kept, and a clip whose speech is unusually quiet
    must not have the speech cut. Both are relative, so the same rule holds
    at every cell of a volume matrix.
    """
    return int(round(max(floor_db + TRIM_ABOVE_FLOOR_DB,
                         peak_db - TRIM_BELOW_PEAK_DB)))


def envelope(pcm: bytes, frame_ms: int = 20) -> list[float]:
    """Mean-square energy per frame_ms of a captured WAV."""
    import array as _array
    samples = _array.array("h")
    samples.frombytes(pcm[44:] if pcm[:4] == b"RIFF" else pcm)
    step = max(1, SAMPLE_RATE * frame_ms // 1000)
    out: list[float] = []
    for i in range(0, len(samples) - step + 1, step):
        chunk = samples[i:i + step]
        out.append(sum(float(x) * x for x in chunk) / len(chunk))
    return out


def speech_span(path: Path, margin_db: float = 12.0) -> float | None:
    """
    How much of a source file is actually sound, in seconds.

    locate_clip searches for a window of this width, and the FILE duration is
    the wrong number to give it: a TTS clip carries its own leading and
    trailing silence, so a search that wide spans whatever else is nearby —
    measured, it swallowed Alexa's start-of-audio tone along with the word
    and both landed in the output. The speech span is the thing being
    located, so it is the thing to search for.

    Decoded through ffmpeg rather than parsed, so it works for every source
    format the matrix accepts.
    """
    cmd = ["ffmpeg", "-nostdin", "-loglevel", "error", "-i", str(path),
           "-ar", str(SAMPLE_RATE), "-ac", "1", "-f", "s16le", "-"]
    try:
        done = subprocess.run(cmd, capture_output=True, timeout=60)
    except (subprocess.TimeoutExpired, OSError):
        return None
    if done.returncode != 0 or not done.stdout:
        return None
    env = envelope(done.stdout)
    if not env:
        return None
    import math as _math
    db = [10.0 * _math.log10(e) if e > 0 else -200.0 for e in env]
    quiet = sorted(db)[len(db) // 4]
    loud  = [i for i, d in enumerate(db) if d > quiet + margin_db]
    if not loud:
        return None
    return (loud[-1] - loud[0] + 1) * 0.02


def locate_clip(pcm: bytes, want_s: float, pad_s: float = 0.25,
                frame_ms: int = 20) -> tuple[float, float] | None:
    """
    Find where the played clip sits inside the captured window.

    A silence gate is the wrong instrument here and measurably so: the window
    is ~11s of room with ~1.3s of speech in it, captured speech peaks around
    -47dBFS against a -75dBFS floor, and any threshold low enough not to clip
    the word's quiet phonemes is also low enough to latch onto a door or a
    chair three seconds earlier. Measured, that kept 2.53s of a 1.32s clip and
    reported an onset of 0.00s on every take.

    The length is KNOWN — it is the source file's duration — so this asks the
    question that actually has one answer: which stretch of exactly that
    length holds the most energy. Deterministic, threshold-free, and it
    returns the same answer at every cell of a volume matrix.

    Returns (start_s, end_s) including `pad_s` either side, clamped to the
    window, or None if the window is shorter than the clip.
    """
    env = envelope(pcm, frame_ms)
    width = max(1, int(round(want_s * 1000 / frame_ms)))
    if len(env) < width:
        return None
    # Sliding sum over a fixed width — one pass, no allocation per position.
    total = sum(env[:width])
    best, best_at = total, 0
    for i in range(1, len(env) - width + 1):
        total += env[i + width - 1] - env[i - 1]
        if total > best:
            best, best_at = total, i
    start = best_at * frame_ms / 1000.0
    end   = start + want_s
    span  = len(env) * frame_ms / 1000.0
    return max(0.0, start - pad_s), min(span, end + pad_s)


def onset_seconds(pcm: bytes, floor_db: float, peak_db: float) -> float | None:
    """
    Where the audio starts inside the captured window, in seconds.

    Recorded in the manifest because it is the only measurement of how long
    the player actually took to start — for an Alexa that is a cloud round
    trip that nothing else reports, and it is what --pre-ms has to cover. A
    run that logs it can be paced from its own data instead of a guess.
    """
    import array as _array
    samples = _array.array("h")
    samples.frombytes(pcm[44:] if pcm[:4] == b"RIFF" else pcm)
    if not samples:
        return None
    gate = 10.0 ** (max(floor_db + TRIM_ABOVE_FLOOR_DB,
                        peak_db - TRIM_BELOW_PEAK_DB) / 20.0) * 32768.0
    step = SAMPLE_RATE // 50          # 20ms
    for i in range(0, len(samples), step):
        chunk = samples[i:i + step]
        if chunk and max(abs(x) for x in chunk) >= gate:
            return i / float(SAMPLE_RATE)
    return None

# Below this, whatever survived the trim was room tone.
MIN_KEEP_SECONDS = 0.40

# Audio kept either side of the located clip.
#
# Not just breathing room: BC-ResNet scores a 1.4s WINDOW, and a ~0.9s word
# with 0.25s either side comes to 1.40s — exactly on the boundary. Measured,
# those clips score 0.000 raw and 0.969 once padded, so a consumer that does
# not pad reads a perfectly good recording as a total miss. Padding here
# makes each file long enough to score on its own.
DEFAULT_PAD_S = 0.45

# `{stem}_recorded_{player}_{pct}.wav`. One formatter and one regex, used by
# both the writer and the resume check — two copies of a filename convention
# is how a resumed run silently re-records everything.
NAME_RE = re.compile(r"^(?P<stem>.+)_recorded_(?P<player>[a-z0-9-]+)_(?P<pct>\d{1,3})\.wav$")

# Window ceiling asked of the controller, past which it closes on its own.
# This is a backstop, not the mechanism: the script stops the window itself
# once the clip has played. Leaving it as the mechanism would mark every
# recording truncated and throw away the signal that a playback OVERRAN.
WINDOW_SLACK_S = 8.0


# ─── Site defaults ────────────────────────────────────────────────────────────
#
# So the common case is `playback_matrix.py` with no arguments. Every one of
# these is still overridable by a flag, and most by an environment variable;
# they are constants rather than a config file because there is one site and
# a second file to find would be one more thing that can be missing.
#
# Edit these, not the argument parser.

DEFAULT_DEVICE     = os.environ.get("EM_DEVICE", "Office")
DEFAULT_CONTROLLER = os.environ.get("EM_CONTROLLER", "http://192.168.3.211:8768")
DEFAULT_SRC        = Path(os.environ.get(
    "EM_SRC", "/media/nfsShare/downloads/oph_positives"))
DEFAULT_PLAYERS    = ["media_player.kyle_s_echo_dot:100"]
DEFAULT_ALEXA      = True

# Home Assistant's URL and token are read from the dashboard project's .env
# rather than duplicated here — one copy, and it is the one that is already
# kept current.
DEFAULT_HA_ENV = Path.home() / "git/hass/react/knet-hass-ui/.env"
HA_ENV_URL_KEY   = "VITE_HA_URL"
HA_ENV_TOKEN_KEY = "VITE_HA_TOKEN"

# Where a controller session token is cached. Dashboard sessions last 30
# days, so this makes a no-argument run work for a month at a time without a
# password anywhere on disk — and when it does expire the script logs in
# again and rewrites it.
TOKEN_CACHE = Path(os.environ.get(
    "EM_TOKEN_FILE", Path.home() / ".config/echomuse/token"))
DEFAULT_USER = os.environ.get("EM_USER", "admin")


def read_env_file(path: Path) -> dict:
    """Parse a KEY=value file. Missing file is not an error."""
    out: dict[str, str] = {}
    try:
        text = path.read_text()
    except OSError:
        return out
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, value = line.split("=", 1)
        out[key.strip()] = value.strip().strip('"').strip("'")
    return out


def cached_token() -> str | None:
    try:
        token = TOKEN_CACHE.read_text().strip()
    except OSError:
        return None
    return token or None


def cache_token(token: str) -> None:
    """Store a session token, readable only by this user."""
    try:
        TOKEN_CACHE.parent.mkdir(parents=True, exist_ok=True)
        TOKEN_CACHE.write_text(token + "\n")
        TOKEN_CACHE.chmod(0o600)
    except OSError as e:
        log(f"  ! could not cache the token at {TOKEN_CACHE}: {e}")


def authenticate(api: Api, user: str) -> bool:
    """
    Get the controller API into a usable state, asking for as little as
    possible.

    Env token, then the cache, then a password. A cached token that has
    expired is indistinguishable from a wrong one until it is used, so it is
    verified with one cheap call rather than trusted — discovering it at the
    first window would abandon the run after arming the device.
    """
    for token in (api.token, cached_token()):
        if not token:
            continue
        api.token = token
        try:
            api.get("/api/devices")
            return True
        except ApiError:
            api.token = None
    import getpass
    try:
        password = os.environ.get("EM_PASSWORD") or getpass.getpass(
            f"Password for {user} at {api.base}: ")
    except (EOFError, KeyboardInterrupt):
        return False
    try:
        role = api.login(user, password)
    except ApiError as e:
        print(f"login failed: {e}", file=sys.stderr)
        return False
    if role != "admin":
        print("Capture mode is admin-only.", file=sys.stderr)
        return False
    cache_token(api.token)
    return True


class Progress:
    """
    A progress bar over CELLS, which is the unit of work — files times matrix.

    tqdm when it is importable, and an equivalent line of stderr when it is
    not. The script is otherwise standard-library-only and a corpus run is the
    last thing that should fail for want of a decoration.

    The total arrives LATE, on purpose. It needs a count of the source
    directory, and that directory can hold nearly two million entries; doing
    it up front would stall the run before the first clip for something that
    only affects the ETA. So counting happens on a background thread and the
    total is filled in when it lands — the bar shows a count and a rate until
    then, and gains an estimate afterwards.
    """

    def __init__(self, enabled: bool = True):
        self.enabled = enabled and sys.stderr.isatty()
        self.n = 0
        self.total: int | None = None
        self.started = time.monotonic()
        self._bar = None
        self._counts = {"rec": 0, "skip": 0, "fail": 0}
        if not self.enabled:
            return
        try:
            from tqdm import tqdm
        except ImportError:
            return
        self._bar = tqdm(total=None, unit="cell", dynamic_ncols=True,
                         smoothing=0.05,
                         bar_format="{l_bar}{bar}| {n_fmt}/{total_fmt} "
                                    "[{elapsed}<{remaining}, {rate_fmt}]{postfix}")

    def set_total(self, total: int) -> None:
        self.total = total
        if self._bar is not None:
            self._bar.total = total
            self._bar.refresh()

    def advance(self, kind: str, n: int = 1) -> None:
        self.n += n
        if kind in self._counts:
            self._counts[kind] += n
        if self._bar is not None:
            self._bar.set_postfix(**self._counts, refresh=False)
            self._bar.update(n)
        elif self.enabled:
            self._plain()

    def _plain(self) -> None:
        done = time.monotonic() - self.started
        rate = self.n / done if done > 0 else 0.0
        eta = ""
        if self.total and rate > 0:
            left = max(0, self.total - self.n) / rate
            eta = f" eta {int(left // 3600)}h{int(left % 3600 // 60):02d}m"
        total = self.total if self.total is not None else "?"
        sys.stderr.write(
            f"\r{self.n}/{total} cells  {rate * 3600:.0f}/h"
            f"  rec {self._counts['rec']} skip {self._counts['skip']}"
            f" fail {self._counts['fail']}{eta}   ")
        sys.stderr.flush()

    def write(self, line: str) -> None:
        """Emit a log line without tearing the bar."""
        if self._bar is not None:
            self._bar.write(line)
        else:
            if self.enabled:
                sys.stderr.write("\r" + " " * 100 + "\r")
            print(line, flush=True)

    def close(self) -> None:
        if self._bar is not None:
            self._bar.close()
        elif self.enabled:
            sys.stderr.write("\n")


PROGRESS = Progress(enabled=False)      # replaced in main()


def log(*parts) -> None:
    PROGRESS.write(" ".join([time.strftime("%H:%M:%S"), *(str(p) for p in parts)]))


def count_cells(src: Path, exts: tuple[str, ...], skip: int,
                per_file: int, progress: Progress) -> None:
    """Count the work, on a background thread. Failure is silent by design —
    an unknown total costs an ETA, not the run."""
    try:
        files = sum(1 for _ in iter_sources(src, exts, skip))
        progress.set_total(files * per_file)
    except Exception:
        pass


# ─── Controller API ───────────────────────────────────────────────────────────

class ApiError(RuntimeError):
    pass


class Api:
    """Minimal Bearer-auth JSON client. Same shape as collect_device_clips."""

    def __init__(self, base: str, token: str | None = None):
        self.base  = base.rstrip("/")
        self.token = token

    def _request(self, method: str, path: str, body=None, timeout=30):
        url  = f"{self.base}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req  = urllib.request.Request(url, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                payload = resp.read()
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")
            try:
                detail = json.loads(detail).get("error", detail)
            except Exception:
                pass
            raise ApiError(f"{method} {path} -> {e.code}: {detail}") from None
        except urllib.error.URLError as e:
            raise ApiError(f"{method} {path} -> {e.reason}") from None
        return json.loads(payload or b"null")

    def get(self, path):        return self._request("GET", path)
    def post(self, path, body): return self._request("POST", path, body)

    def get_recording(self, path: str, timeout: float):
        """
        GET that returns (body, headers), or None on a 404.

        The pull transport. A 404 here is an ordinary answer — "nothing
        arrived in the time you allowed" — not an error, so it is not raised.
        """
        req = urllib.request.Request(f"{self.base}{path}", method="GET")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return resp.read(), dict(resp.headers)
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            detail = e.read().decode("utf-8", "replace")[:200]
            raise ApiError(f"GET {path} -> {e.code}: {detail}") from None
        except urllib.error.URLError as e:
            raise ApiError(f"GET {path} -> {e.reason}") from None

    def login(self, username: str, password: str) -> str:
        self.token = None
        out = self.post("/api/auth/login",
                        {"username": username, "password": password})
        self.token = out["token"]
        return out.get("role", "")


class Ha:
    """Home Assistant REST client — three service calls and one state read."""

    def __init__(self, base: str, token: str):
        self.base  = base.rstrip("/")
        self.token = token

    def _request(self, method: str, path: str, body=None, timeout=20):
        req = urllib.request.Request(
            f"{self.base}{path}",
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
        )
        req.add_header("Authorization", f"Bearer {self.token}")
        req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                return json.loads(resp.read() or b"null")
        except urllib.error.HTTPError as e:
            detail = e.read().decode("utf-8", "replace")[:300]
            raise ApiError(f"HA {method} {path} -> {e.code}: {detail}") from None
        except urllib.error.URLError as e:
            raise ApiError(f"HA {method} {path} -> {e.reason}") from None

    def call(self, domain: str, service: str, data: dict):
        return self._request("POST", f"/api/services/{domain}/{service}", data)

    def state(self, entity_id: str) -> dict:
        return self._request("GET", f"/api/states/{entity_id}")

    def volume_set(self, entity_id: str, level: float):
        self.call("media_player", "volume_set",
                  {"entity_id": entity_id, "volume_level": round(level, 3)})

    def play_url(self, entity_id: str, url: str):
        # Deliberately no `announce` or `enqueue`: both are optional service
        # fields that HA rejects outright on a player lacking the matching
        # feature flag, and the defaults are already what this wants (play
        # now, replacing whatever was going). A rejected call would fail
        # every cell on that player for a field neither end needed.
        self.call("media_player", "play_media", {
            "entity_id":          entity_id,
            "media_content_id":   url,
            "media_content_type": "music",
        })

    def stop(self, entity_id: str):
        self.call("media_player", "media_stop", {"entity_id": entity_id})


# ─── Players ──────────────────────────────────────────────────────────────────
#
# Two ways to get a clip out of a speaker, because the two kinds of player
# available here work in opposite directions.


class LocalPlayer:
    """
    A player that fetches the clip from us: Music Assistant, shairport,
    Chromecast — anything driven on the LAN.

    `media_player.play_media` with a URL this process serves. Nothing is
    transcoded and nothing leaves the network, so what comes out of the
    speaker is the source file.
    """

    kind = "local"

    def __init__(self, ha: Ha, serve: "Serve", media_base: str):
        self.ha = ha
        self.serve = serve
        self.media_base = media_base

    def prepare(self, source: Path, token: str) -> str:
        self.serve.arm(source, token)
        return f"{self.media_base}/media/{token}"

    def play(self, entity: str, url: str) -> None:
        self.ha.play_url(entity, url)

    def cleanup(self) -> None:
        self.serve.disarm()

    def stop(self, entity: str) -> None:
        with contextlib.suppress(ApiError):
            self.ha.stop(entity)


class AlexaPlayer:
    """
    An Echo, driven through Alexa Media Player.

    Alexa cannot fetch anything from the LAN — playback is driven from
    Amazon's cloud — so the direction is inverted: the clip is transcoded and
    dropped into Home Assistant's `www/`, which is served publicly over
    HTTPS, and Alexa is told to play that URL.

    **`media_player.play_media` does not work and cannot be made to.** It
    routes `music` into the same TTS call used below, but derives a local
    filename from `media_content_id` — so a plain URL builds a path with
    slashes in it and the download 404s. The supported form needs a
    `media-source://` id, which in a Docker install means the file must live
    under the container's `/media`. `notify.alexa_media` reaches the same
    final call with none of that in the way.

    Three constraints from Amazon, all load-bearing:

    - **The URL must be public HTTPS with a real certificate.** Amazon's
      cloud fetches it, not the device, so a LAN address is unreachable
      however well it works from here.
    - **The MP3 must be MPEG-2: 48 kbps at 24 kHz.** These are the same
      ffmpeg arguments alexa_media itself uses on its own path.
    - **Alexa attenuates SSML audio** relative to its own speaking voice, by
      enough to matter. `speechnorm` recovers a few dB; the rest has to come
      from the device's volume, which is the matrix axis anyway.
    """

    kind = "alexa"

    def __init__(self, ha: Ha, www_dir: Path, public_base: str,
                 subdir: str = "em_capture", normalise: bool = True,
                 lead_ms: int = ALEXA_LEAD_MS):
        self.ha = ha
        self.dir = www_dir / subdir
        self.dir.mkdir(parents=True, exist_ok=True)
        self.public_base = public_base.rstrip("/")
        self.subdir = subdir
        self.normalise = normalise
        self.lead_ms = max(0, int(lead_ms))
        self._current: Path | None = None

    def prepare(self, source: Path, token: str) -> str:
        dst = self.dir / f"{token}.mp3"
        filters = ["speechnorm=e=12.5:r=0.0001:l=1"] if self.normalise else []
        if self.lead_ms:
            # Alexa plays a short tone when it starts external audio — a pure
            # ~350Hz burst, measured, and absent from plain-text TTS, so it
            # belongs to the <audio> element rather than to speech. It cannot
            # be turned off from here, so the clip is pushed clear of it: the
            # tone happens at playback start, the word arrives a beat later,
            # and locate_clip then has two well-separated bursts to choose
            # between instead of one blob.
            filters.append(f"adelay={self.lead_ms}:all=1")
        cmd = ["ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-i", str(source)]
        if filters:
            cmd += ["-af", ",".join(filters)]
        cmd += ["-ac", "2", "-codec:a", "libmp3lame", "-b:a", "48k",
                "-ar", "24000", "-write_xing", "0", str(dst)]
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
        if done.returncode != 0 or not dst.is_file():
            raise ApiError(f"transcode failed: {(done.stderr or '').strip()[:200]}")
        self._current = dst
        return f"{self.public_base}/local/{self.subdir}/{dst.name}"

    def play(self, entity: str, url: str) -> None:
        # Single quotes inside the SSML, matching alexa_media's own construction.
        self.ha.call("notify", "alexa_media", {
            "message": f"<audio src='{url}'/>",
            "target":  [entity],
            "data":    {"type": "tts"},
        })

    def cleanup(self) -> None:
        # Every clip leaves a file in the user's www/ directory, and a corpus
        # run is thousands of clips. Removed as soon as the recording is in
        # hand — Amazon has already fetched it by then.
        if self._current is not None:
            with contextlib.suppress(OSError):
                self._current.unlink()
            self._current = None


    def stop(self, entity: str) -> None:
        pass    # a TTS utterance is not a stream; there is nothing to stop


# ─── The local server: serves the source, receives the recording ──────────────

@dataclass
class Delivery:
    body:      bytes
    tag:       str
    ms:        int
    peak_db:   float
    floor_db:  float
    truncated: bool
    dropped:   int


class Serve:
    """
    One HTTP server doing two jobs.

    `GET /media/<token>` hands the current source file to the media player —
    HA needs a URL and this process already has the bytes, so standing up a
    separate file server would be a second thing to configure and keep alive.
    The token changes per cell and anything else 404s, so the route is not a
    way to read arbitrary paths off this host.

    `POST /clip` is the controller's webhook. The body is the WAV; everything
    else rides `X-EM-*` headers.
    """

    def __init__(self, bind_host: str, bind_port: int):
        self.deliveries: "queue.Queue[Delivery]" = queue.Queue()
        self._armed_token: str | None = None
        self._armed_path:  Path | None = None
        self._armed_type:  str = "application/octet-stream"
        self._lock = threading.Lock()
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):        # noqa: N802 — quiet by default
                pass

            def do_GET(self):                 # noqa: N802
                outer._serve_media(self)

            def do_HEAD(self):                # noqa: N802
                # Some players HEAD before they GET, to size the stream.
                outer._serve_media(self, head_only=True)

            def do_POST(self):                # noqa: N802
                outer._take_clip(self)

        self.httpd = http.server.ThreadingHTTPServer((bind_host, bind_port), Handler)
        self.httpd.daemon_threads = True
        self.port = self.httpd.server_address[1]
        self._thread = threading.Thread(target=self.httpd.serve_forever,
                                        kwargs={"poll_interval": 0.2}, daemon=True)

    def start(self):
        self._thread.start()

    def close(self):
        try:
            self.httpd.shutdown()
            self.httpd.server_close()
        except Exception:
            pass

    # ── media ────────────────────────────────────────────────────────────

    def arm(self, path: Path, token: str) -> None:
        with self._lock:
            self._armed_token = token
            self._armed_path  = path
            self._armed_type  = _content_type(path)

    def disarm(self) -> None:
        with self._lock:
            self._armed_token = None
            self._armed_path  = None

    def _serve_media(self, req, head_only: bool = False) -> None:
        parsed = urllib.parse.urlparse(req.path)
        token  = parsed.path.rsplit("/", 1)[-1]
        with self._lock:
            armed, path, ctype = self._armed_token, self._armed_path, self._armed_type
        if not token or token != armed or path is None or not path.is_file():
            req.send_error(404, "not armed")
            return
        try:
            data = path.read_bytes()
        except OSError as e:
            req.send_error(500, str(e))
            return

        # Range support: several players ask for one, and a server that
        # ignores it either gets the whole file re-fetched or, worse, gets a
        # player that gives up. Only the single-range form is handled — that
        # is all any of them send.
        start, end = 0, len(data) - 1
        status = 200
        rng = req.headers.get("Range", "")
        m = re.match(r"bytes=(\d*)-(\d*)$", rng.strip()) if rng else None
        if m and (m.group(1) or m.group(2)):
            if m.group(1):
                start = int(m.group(1))
                if m.group(2):
                    end = min(int(m.group(2)), end)
            else:                                   # suffix range
                start = max(0, len(data) - int(m.group(2)))
            if start > end:
                req.send_error(416, "bad range")
                return
            status = 206

        chunk = data[start:end + 1]
        req.send_response(status)
        req.send_header("Content-Type", ctype)
        req.send_header("Content-Length", str(len(chunk)))
        req.send_header("Accept-Ranges", "bytes")
        if status == 206:
            req.send_header("Content-Range", f"bytes {start}-{end}/{len(data)}")
        req.end_headers()
        if not head_only:
            try:
                req.wfile.write(chunk)
            except (BrokenPipeError, ConnectionResetError):
                pass    # the player stopped reading; not our problem

    # ── webhook ──────────────────────────────────────────────────────────

    def _take_clip(self, req) -> None:
        try:
            length = int(req.headers.get("Content-Length") or 0)
        except ValueError:
            length = 0
        body = req.rfile.read(length) if length else b""
        h = req.headers
        self.deliveries.put(Delivery(
            body=body,
            tag=h.get("X-EM-Tag", ""),
            ms=_as_int(h.get("X-EM-Ms"), 0),
            peak_db=_as_float(h.get("X-EM-Peak-Db"), -100.0),
            floor_db=_as_float(h.get("X-EM-Floor-Db"), -100.0),
            truncated=h.get("X-EM-Truncated") == "1",
            dropped=_as_int(h.get("X-EM-Dropped"), 0),
        ))
        req.send_response(204)
        req.send_header("Content-Length", "0")
        req.end_headers()

    def await_tag(self, tag: str, timeout: float) -> Delivery | None:
        """
        Wait for the recording belonging to `tag`.

        Deliveries for any other tag are discarded rather than returned: a
        late arrival from the previous cell is the one thing that would pair
        a recording with the wrong source file, and from there every file in
        the run is mislabelled by one.
        """
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            try:
                item = self.deliveries.get(timeout=remaining)
            except queue.Empty:
                return None
            if item.tag == tag:
                return item
            log(f"  ! discarding a late delivery for {item.tag!r}")


def _content_type(path: Path) -> str:
    return {
        ".mp3":  "audio/mpeg",
        ".wav":  "audio/wav",
        ".flac": "audio/flac",
        ".ogg":  "audio/ogg",
        ".opus": "audio/ogg",
        ".m4a":  "audio/mp4",
    }.get(path.suffix.lower(), "application/octet-stream")


def _as_int(v, default: int) -> int:
    try:
        return int(v)
    except (TypeError, ValueError):
        return default


def _as_float(v, default: float) -> float:
    try:
        return float(v)
    except (TypeError, ValueError):
        return default


# ─── Matrix, names, sources ───────────────────────────────────────────────────

@dataclass(frozen=True)
class Cell:
    entity: str
    pct:    int            # 0-100, the integer that appears in the filename

    @property
    def level(self) -> float:
        return self.pct / 100.0

    @property
    def slug(self) -> str:
        return player_slug(self.entity)

    def __str__(self) -> str:
        return f"{self.entity}@{self.pct}%"


def player_slug(entity: str) -> str:
    """`media_player.lounge_speaker` -> `lounge-speaker`."""
    name = entity.split(".", 1)[-1] if "." in entity else entity
    return re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-") or "player"


def output_name(stem: str, cell: Cell) -> str:
    """
    The one place a recording's filename is built.

    Volume is an integer percent rather than the 0-1 float HA takes, so the
    name carries exactly one dot — the one before the extension. `parse_name`
    is its inverse and the resume check uses the same formatter, so the two
    cannot disagree about what has already been recorded.
    """
    return f"{stem}_recorded_{cell.slug}_{cell.pct}.wav"


def parse_name(name: str) -> tuple[str, str, int] | None:
    m = NAME_RE.match(name)
    if not m:
        return None
    return m.group("stem"), m.group("player"), int(m.group("pct"))


def parse_player_arg(spec: str) -> list[Cell]:
    """
    `media_player.lounge:20,35,60` -> three cells.

    Volumes are accepted as percents (`35`) or fractions (`0.35`) because
    both are natural to type and HA takes the latter; anything <= 1 is read
    as a fraction. `100` therefore means full and `1` means one percent,
    which is the reading that cannot silently blast a room.
    """
    if ":" not in spec:
        raise argparse.ArgumentTypeError(
            f"--player wants ENTITY:VOL[,VOL...], got {spec!r}")
    entity, vols = spec.split(":", 1)
    entity = entity.strip()
    if not entity.startswith("media_player."):
        entity = f"media_player.{entity}"
    cells: list[Cell] = []
    for raw in vols.split(","):
        raw = raw.strip()
        if not raw:
            continue
        try:
            value = float(raw)
        except ValueError:
            raise argparse.ArgumentTypeError(f"bad volume {raw!r} in {spec!r}") from None
        pct = int(round(value * 100)) if value <= 1.0 else int(round(value))
        if not 0 < pct <= 100:
            raise argparse.ArgumentTypeError(f"volume out of range: {raw!r}")
        cells.append(Cell(entity=entity, pct=pct))
    if not cells:
        raise argparse.ArgumentTypeError(f"no volumes in {spec!r}")
    return cells


def iter_sources(src: Path, exts: tuple[str, ...], skip: int):
    """
    Stream the source directory.

    `os.scandir`, never `glob`/`sorted`/`listdir`: Common Voice's clips
    directory holds ~1.9M entries, and materialising that list costs hundreds
    of megabytes to produce an order nothing depends on. Directory order is
    arbitrary but stable, which is all the resume needs.
    """
    seen = 0
    with os.scandir(src) as it:
        for entry in it:
            try:
                if not entry.is_file():
                    continue
            except OSError:
                continue
            if Path(entry.name).suffix.lower() not in exts:
                continue
            if parse_name(entry.name):
                continue        # one of ours, if the output dir is the source dir
            seen += 1
            if seen <= skip:
                continue
            yield Path(entry.path)


# ─── Media inspection and trimming ────────────────────────────────────────────

def require_tools() -> None:
    missing = [t for t in ("ffmpeg", "ffprobe") if shutil.which(t) is None]
    if missing:
        raise RuntimeError(
            f"{', '.join(missing)} not on PATH — needed to measure and trim "
            f"the clips")


def source_seconds(path: Path) -> float | None:
    """Duration of a source clip, or None if ffprobe cannot say."""
    cmd = ["ffprobe", "-v", "error", "-show_entries", "format=duration",
           "-of", "default=noprint_wrappers=1:nokey=1", str(path)]
    try:
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    except (subprocess.TimeoutExpired, OSError):
        return None
    try:
        seconds = float(done.stdout.strip())
    except ValueError:
        return None
    return seconds if seconds > 0 else None


def wav_seconds(path: Path) -> float:
    try:
        with wave.open(str(path), "rb") as w:
            return w.getnframes() / float(w.getframerate() or 1)
    except Exception:
        return 0.0


def cut(src: Path, dst: Path, start_s: float, end_s: float) -> tuple[bool, str]:
    """Cut [start_s, end_s) out of a captured window. Returns (ok, note)."""
    cmd = [
        "ffmpeg", "-nostdin", "-loglevel", "error", "-y",
        "-ss", f"{start_s:.3f}", "-t", f"{max(0.05, end_s - start_s):.3f}",
        "-i", str(src),
        "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(dst),
    ]
    try:
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    except subprocess.TimeoutExpired:
        return False, "ffmpeg timed out"
    if done.returncode != 0 or not dst.is_file():
        note = (done.stderr or "").strip().splitlines()
        return False, note[-1] if note else "ffmpeg failed"
    return True, ""


# ─── The run ──────────────────────────────────────────────────────────────────

class Run:
    """Owns the device's capture mode, the players' volumes, and the loop."""

    def __init__(self, args, api: Api, ha: Ha, serve: Serve | None,
                 player, device_id: str, cells: list[Cell], out_dir: Path):
        self.args      = args
        self.api       = api
        self.ha        = ha
        self.serve     = serve
        self.player    = player
        self.device_id = device_id
        self.cells     = cells
        self.out_dir   = out_dir
        # None means PULL: the controller holds each finished recording and
        # we fetch it. That is the default because push needs a route back
        # from the controller to this host, which a controller on a macvlan
        # network does not have — it cannot reach its own Docker host, so a
        # webhook here times out with the audio captured and nowhere to go.
        self.webhook = (f"http://{args.advertise}:{serve.port}/clip"
                        if (serve is not None and args.webhook) else None)
        self.current_level: dict[str, int] = {}
        self._spans: dict[Path, float] = {}
        self.prior_level:   dict[str, float | None] = {}
        self.recorded = 0
        self.skipped  = 0
        self.failed   = 0
        self.silent_streak = 0
        self.stopping = False
        self.manifest = out_dir / "manifest.tsv"
        self.failures = out_dir / "failures.tsv"

    # ── device ───────────────────────────────────────────────────────────

    def arm(self) -> None:
        """
        Put the device into capture mode. Idempotent, and safe to re-call.

        Re-armed rather than assumed on every cell: the mode lives on the
        device's connection and is deliberately not persisted, so a device
        that bounced mid-run comes back with it off. Noticing that here costs
        one GET and turns a silent run of failures into a hiccup.
        """
        body = {
            "enabled": True,
            # Comfortably longer than one cell, so the controller's dead-man
            # only fires when this script has genuinely stopped driving.
            "idle_s":  300,
        }
        if self.webhook:
            body["webhook"] = self.webhook
        self.api.post(f"/api/devices/{self.device_id}/capture", body)

    def ensure_armed(self) -> bool:
        try:
            state = self.api.get(f"/api/devices/{self.device_id}/capture")
        except ApiError as e:
            log(f"  ! controller unreachable: {e}")
            return False
        if state.get("enabled") and state.get("webhook") == self.webhook:
            return True
        log("  · capture mode not armed (device reconnected?) — re-arming")
        try:
            self.arm()
            return True
        except ApiError as e:
            log(f"  ! could not arm capture mode: {e}")
            return False

    def disarm(self) -> None:
        try:
            self.api.post(f"/api/devices/{self.device_id}/capture",
                          {"enabled": False})
            log("Capture mode off — the device answers again.")
        except ApiError as e:
            print(f"\n!! CAPTURE MODE NOT CLEARED: {e}", file=sys.stderr)
            print(f"!! {self.device_id} will not start voice turns until it "
                  f"is. It clears itself after ~5 minutes idle; to do it now:",
                  file=sys.stderr)
            print(f"!!   curl -X POST -H 'Authorization: Bearer <token>' "
                  f"-H 'Content-Type: application/json' -d '{{\"enabled\":false}}' "
                  f"{self.api.base}/api/devices/{self.device_id}/capture",
                  file=sys.stderr)

    # ── players ──────────────────────────────────────────────────────────

    def remember_volume(self, entity: str) -> None:
        if entity in self.prior_level:
            return
        try:
            state = self.ha.state(entity)
            self.prior_level[entity] = state.get("attributes", {}).get("volume_level")
        except ApiError:
            self.prior_level[entity] = None

    def restore_volumes(self) -> None:
        if self.args.no_restore_volume:
            return
        for entity, level in self.prior_level.items():
            if level is None:
                continue
            try:
                self.ha.volume_set(entity, float(level))
            except ApiError as e:
                log(f"  ! could not restore {entity} volume: {e}")

    def set_cell_volume(self, cell: Cell) -> None:
        if self.current_level.get(cell.entity) == cell.pct:
            return
        self.remember_volume(cell.entity)
        self.ha.volume_set(cell.entity, cell.level)
        self.current_level[cell.entity] = cell.pct
        time.sleep(self.args.settle_ms / 1000.0)

    def stop_players(self) -> None:
        for entity in self.current_level:
            self.player.stop(entity)

    # ── one cell ─────────────────────────────────────────────────────────

    def record(self, source: Path, cell: Cell, seconds: float) -> bool:
        """Play one file at one cell and write the recording. True on success."""
        stem = source.stem
        dst  = self.out_dir / output_name(stem, cell)
        tag  = f"{stem}|{cell.entity}|{cell.pct}"
        token = f"{abs(hash((stem, cell.entity, cell.pct, time.time()))):x}"
        if self.player.kind == "local":
            token += source.suffix

        play_s = (self.args.pre_ms / 1000.0 + seconds
                  + getattr(self.player, "lead_ms", 0) / 1000.0
                  + self.args.post_ms / 1000.0)
        window_s = play_s + WINDOW_SLACK_S

        if not self.ensure_armed():
            return False
        self.set_cell_volume(cell)
        session = None
        try:
            url = self.player.prepare(source, token)
            opened = self.api.post(
                f"/api/devices/{self.device_id}/capture/window",
                {"tag": tag, "max_ms": int(window_s * 1000)},
            )
            session = opened.get("session")
            self.player.play(cell.entity, url)
            # Wall clock, not player state: Music Assistant flow players lag
            # and misreport their transitions, and an Alexa reports nothing
            # about a TTS utterance at all. The source duration is known
            # exactly; --pre-ms covers however long the player takes to
            # start, and the trim finds the audio wherever inside it landed.
            time.sleep(play_s)
            self.api.post(f"/api/devices/{self.device_id}/capture/window/stop",
                          {"session": session})
        except ApiError as e:
            log(f"  ! {cell}: {e}")
            self.player.cleanup()
            return False

        got = self.collect(tag, session)
        self.player.cleanup()
        if got is None:
            log(f"  ! {cell}: no recording arrived within {self.args.grace_s:.0f}s")
            self._fail(stem, cell, "no_delivery")
            return False
        if got.dropped:
            log(f"  ! {got.dropped} earlier recording(s) were dropped by the "
                f"controller queue — the webhook is not keeping up")

        margin = got.peak_db - got.floor_db
        if margin < SILENT_MARGIN_DB:
            self.silent_streak += 1
            log(f"  ! {cell}: silence (peak {got.peak_db:.1f}dBFS, floor "
                f"{got.floor_db:.1f}dBFS) — streak {self.silent_streak}")
            self._fail(stem, cell, "silent")
            return False
        self.silent_streak = 0

        return self._write(source, dst, cell, got, seconds)

    def _span(self, source: Path, fallback: float) -> float:
        """The source's speech span, measured once per file."""
        cached = self._spans.get(source)
        if cached is None:
            cached = speech_span(source) or fallback
            self._spans[source] = cached
        return cached

    def collect(self, tag: str, session: str | None) -> Delivery | None:
        """
        Get the recording for this cell, by whichever transport is in use.

        Pull long-polls the controller, which is the default and the only one
        that works on a macvlan deployment. Push waits on the local server.
        Either way a recording belonging to a DIFFERENT cell is never
        returned — pull asks for this window's session by name, and push
        discards anything whose tag does not match.
        """
        if self.webhook:
            return self.serve.await_tag(tag, timeout=self.args.grace_s)
        try:
            got = self.api.get_recording(
                f"/api/devices/{self.device_id}/capture/recording"
                f"?session={session}&wait={self.args.grace_s:.0f}",
                timeout=self.args.grace_s + 10,
            )
        except ApiError as e:
            log(f"  ! collect failed: {e}")
            return None
        if got is None:
            return None
        body, h = got
        return Delivery(
            body=body,
            tag=h.get("X-EM-Tag", ""),
            ms=_as_int(h.get("X-EM-Ms"), 0),
            peak_db=_as_float(h.get("X-EM-Peak-Db"), -100.0),
            floor_db=_as_float(h.get("X-EM-Floor-Db"), -100.0),
            truncated=h.get("X-EM-Truncated") == "1",
            dropped=_as_int(h.get("X-EM-Dropped"), 0),
        )

    def _write(self, source: Path, dst: Path, cell: Cell,
               got: Delivery, seconds: float) -> bool:
        raw = dst.with_suffix(".raw.wav")
        # Located even when the cut is skipped: --no-trim keeps the whole
        # window precisely so the clip can be seen in context, and where the
        # clip sits is the thing being looked at.
        want  = self._span(source, seconds)
        where = locate_clip(got.body, want, self.args.pad_s)
        try:
            raw.write_bytes(got.body)
            if self.args.no_trim:
                raw.replace(dst)
                kept = wav_seconds(dst)
            else:
                tmp = dst.with_suffix(".part.wav")
                if where is None:
                    log(f"  ! {cell}: window shorter than the clip — keeping whole")
                    raw.replace(dst)
                    kept = wav_seconds(dst)
                    self._finish(source, dst, cell, got, seconds, kept, None)
                    return True
                ok, note = cut(raw, tmp, *where)
                if not ok:
                    log(f"  ! {cell}: cut failed ({note}) — keeping whole window")
                    raw.replace(dst)
                    kept = wav_seconds(dst)
                else:
                    kept = wav_seconds(tmp)
                    if kept < MIN_KEEP_SECONDS:
                        log(f"  ! {cell}: only {kept:.2f}s survived the trim — "
                            f"discarding")
                        tmp.unlink(missing_ok=True)
                        raw.unlink(missing_ok=True)
                        self._fail(source.stem, cell, "cut_to_nothing")
                        return False
                    tmp.replace(dst)
                    raw.unlink(missing_ok=True)
        except OSError as e:
            log(f"  ! {cell}: write failed: {e}")
            self._fail(source.stem, cell, "write_failed")
            return False
        finally:
            raw.unlink(missing_ok=True)

        onset = None if where is None else where[0] + self.args.pad_s
        self._finish(source, dst, cell, got, seconds, kept, onset)
        return True

    def _finish(self, source: Path, dst: Path, cell: Cell, got: Delivery,
                seconds: float, kept: float, onset: float | None) -> None:
        self._manifest(source, dst, cell, got, seconds, kept, onset)
        log(f"  ✓ {dst.name}  {kept:.2f}s  peak {got.peak_db:.1f}dBFS  "
            f"floor {got.floor_db:.1f}dBFS"
            f"{'' if onset is None else f'  onset {onset:.1f}s'}"
            f"{'  TRUNCATED' if got.truncated else ''}")

    # ── bookkeeping ──────────────────────────────────────────────────────

    def _manifest(self, source: Path, dst: Path, cell: Cell,
                  got: Delivery, seconds: float, kept: float,
                  onset: float | None) -> None:
        new = not self.manifest.exists()
        with self.manifest.open("a") as f:
            if new:
                f.write("file\tsource\tplayer\tvolume_pct\tsource_s\t"
                        "captured_ms\tonset_s\tkept_s\tpeak_db\tfloor_db\t"
                        "truncated\n")
            f.write(f"{dst.name}\t{source.name}\t{cell.entity}\t{cell.pct}\t"
                    f"{seconds:.2f}\t{got.ms}\t"
                    f"{'' if onset is None else f'{onset:.2f}'}\t"
                    f"{kept:.2f}\t{got.peak_db:.1f}\t"
                    f"{got.floor_db:.1f}\t{int(got.truncated)}\n")

    def _fail(self, stem: str, cell: Cell, why: str) -> None:
        self.failed += 1
        new = not self.failures.exists()
        with self.failures.open("a") as f:
            if new:
                f.write("when\tsource\tplayer\tvolume_pct\treason\n")
            f.write(f"{time.strftime('%Y-%m-%dT%H:%M:%S')}\t{stem}\t"
                    f"{cell.entity}\t{cell.pct}\t{why}\n")

    # ── the loop ─────────────────────────────────────────────────────────

    def run(self) -> int:
        args = self.args
        log(f"Transport {'webhook ' + self.webhook if self.webhook else 'pull'}")
        log(f"Player    {self.player.kind}")
        log(f"Output    {self.out_dir}")
        log(f"Matrix    {len(self.cells)} cell(s): "
            f"{', '.join(str(c) for c in self.cells)}")
        log("Ctrl+C when you have enough — each source file is finished "
            "across the whole matrix before the next is started.")

        self.arm()
        started = time.monotonic()
        for source in iter_sources(args.src, args.exts, args.skip):
            if self.stopping:
                break
            todo = [c for c in self.cells
                    if not (self.out_dir / output_name(source.stem, c)).exists()]
            done_already = len(self.cells) - len(todo)
            if done_already:
                self.skipped += done_already
                PROGRESS.advance("skip", done_already)
            if not todo:
                continue

            seconds = source_seconds(source)
            if seconds is None:
                log(f"  ! {source.name}: ffprobe could not read it — skipping")
                PROGRESS.advance("fail", len(todo))
                continue
            if seconds > args.max_source_s:
                log(f"  ! {source.name}: {seconds:.1f}s is longer than "
                    f"--max-source-s — skipping")
                PROGRESS.advance("skip", len(todo))
                continue

            if not PROGRESS.enabled:
                log(f"{source.name} ({seconds:.1f}s) — {len(todo)} cell(s) to do")
            for cell in todo:
                if self.stopping:
                    break
                if self.record(source, cell, seconds):
                    self.recorded += 1
                    PROGRESS.advance("rec")
                else:
                    PROGRESS.advance("fail")
                if self.silent_streak >= args.max_silent:
                    log(f"!! {self.silent_streak} silent captures in a row — "
                        f"stopping. Check the player is audible and the Echo "
                        f"can hear it.")
                    self.stopping = True

        elapsed = time.monotonic() - started
        log(f"Done: {self.recorded} recorded, {self.skipped} already present, "
            f"{self.failed} failed, in {elapsed / 60:.1f} min")
        return 0 if self.recorded or self.skipped else 1


# ─── Device resolution ────────────────────────────────────────────────────────

def resolve_device(api: Api, wanted: str) -> dict:
    devices = api.get("/api/devices") or []
    if isinstance(devices, dict):
        devices = devices.get("devices", [])
    matches = [d for d in devices
               if wanted.lower() in (str(d.get("label") or "").lower(),
                                     str(d.get("device_id") or "").lower())]
    if not matches:
        names = ", ".join(f"{d.get('label')} ({d.get('device_id')})"
                          for d in devices) or "none"
        raise SystemExit(f"No device matching {wanted!r}. Known: {names}")
    return matches[0]


def local_ip_towards(url: str) -> str:
    """
    The address this host has on the route to `url`.

    HA and the media players have to reach the server this script runs, and
    the right interface is whichever one reaches HA — a host with several
    (docker bridges, VPNs) has no single obvious answer otherwise.
    """
    host = urllib.parse.urlparse(url).hostname or "8.8.8.8"
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.connect((host, 80))
        return sock.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        sock.close()


# ─── main ─────────────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__.split("Why", 1)[0].strip(),
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
example:
  playback_matrix.py

    Everything has a site default (see "Site defaults" near the top of this
    file): the Office device, the oph_positives corpus, and the Echo Dot at
    three volumes, recorded through Alexa. Output lands in clips_recorded/
    beside the source directory.

  playback_matrix.py --player media_player.bedroom_echo_dot:100,60

    A different Echo.

  playback_matrix.py --local --player media_player.office_2:35,60 \\
    --src /media/nfsShare/common_voice/cv-corpus-26.0-2026-06-12/en/clips

    A Music Assistant player, which fetches the clip from this host with no
    transcode and no cloud round trip.
""")
    ap.add_argument("-d", "--device", default=DEFAULT_DEVICE, metavar="NAME",
                    help="device label or id to record with")
    ap.add_argument("--controller", default=DEFAULT_CONTROLLER)
    ap.add_argument("--token", default=os.environ.get("EM_TOKEN"),
                    help="dashboard API token (or use --user)")
    ap.add_argument("--user", default=DEFAULT_USER,
                    help="dashboard username (admin role required)")
    ap.add_argument("--ha-url", default=os.environ.get("HA_URL"),
                    help=f"Home Assistant base URL (default: {HA_ENV_URL_KEY} "
                         f"from --ha-env)")
    ap.add_argument("--ha-token", default=os.environ.get("HA_TOKEN"),
                    help=f"HA long-lived token (default: {HA_ENV_TOKEN_KEY} "
                         f"from --ha-env)")
    ap.add_argument("--ha-env", type=Path, default=DEFAULT_HA_ENV, metavar="FILE",
                    help="KEY=value file to read the HA URL and token from")
    ap.add_argument("--src", type=Path, default=DEFAULT_SRC,
                    help="directory of source clips to play")
    ap.add_argument("-o", "--out", type=Path, default=None,
                    help=f"where recordings go (default: ../{DEFAULT_OUT_NAME} "
                         f"beside --src)")
    ap.add_argument("--player", action="append", metavar="ENTITY:VOLS",
                    help=f"media player and its volumes, e.g. "
                         f"media_player.lounge:20,35,60 (repeatable; "
                         f"default: {' '.join(DEFAULT_PLAYERS)})")
    ap.add_argument("--bind", default="0.0.0.0:0", metavar="HOST:PORT",
                    help="where to serve media / listen for a webhook "
                         "(default: an ephemeral port on all interfaces)")
    ap.add_argument("--webhook", action="store_true",
                    help="have the controller PUSH each recording here "
                         "instead of pulling it. Needs the controller to be "
                         "able to reach this host, which it cannot on a "
                         "macvlan network")
    ap.add_argument("--advertise", metavar="HOST",
                    help="address HA should reach this host on "
                         "(default: auto-detected from the route to HA)")
    ap.add_argument("--media-base", metavar="URL",
                    help="override the base URL given to the media players")
    ap.add_argument("--ext", default=",".join(DEFAULT_EXTS), metavar="LIST",
                    help="source extensions to play")
    ap.add_argument("--skip", type=int, default=0, metavar="N",
                    help="skip the first N source files")
    ap.add_argument("--max-source-s", type=float, default=20.0, metavar="S",
                    help="ignore source clips longer than this")
    ap.add_argument("--no-progress", action="store_true",
                    help="plain log lines instead of a progress bar")

    a = ap.add_argument_group("alexa players")
    a.add_argument("--alexa", action="store_true", default=DEFAULT_ALEXA,
                   help="drive the players as Amazon Echos through "
                        "notify.alexa_media. Transcodes each clip into Home "
                        "Assistant's www/ and plays it by public URL, since "
                        "Alexa playback is driven from Amazon's cloud and "
                        "cannot reach the LAN")
    a.add_argument("--hass-www", type=Path,
                   default=Path.home() / "git/hass/www", metavar="DIR",
                   help="Home Assistant's www/ directory")
    a.add_argument("--public-base", default=None, metavar="URL",
                   help="public HTTPS base that serves www/ (default: the "
                        "--ha-url host). Must be reachable from the internet "
                        "with a valid certificate — Amazon fetches it")
    a.add_argument("--local", dest="alexa", action="store_false",
                   help="the players are ordinary LAN players (Music "
                        "Assistant, shairport, Chromecast) that fetch the "
                        "clip from this host directly")
    a.add_argument("--lead-ms", type=int, default=ALEXA_LEAD_MS, metavar="MS",
                   help="silence prepended to the clip, to separate it from "
                        "Alexa's start-of-audio tone")
    a.add_argument("--no-normalise", action="store_true",
                   help="skip the speechnorm pass on the transcode")

    t = ap.add_argument_group("timing")
    t.add_argument("--settle-ms", type=int, default=DEFAULT_SETTLE_MS)
    t.add_argument("--pre-ms", type=int, default=DEFAULT_PRE_MS,
                   help="recorded before playback is expected to start")
    t.add_argument("--post-ms", type=int, default=DEFAULT_POST_MS,
                   help="recorded after the clip's duration has elapsed")
    t.add_argument("--grace-s", type=float, default=DEFAULT_GRACE_S,
                   help="how long to wait for the recording to arrive")

    q = ap.add_argument_group("quality")
    q.add_argument("--no-trim", action="store_true",
                   help="keep the raw window instead of trimming room tone")
    q.add_argument("--pad-s", type=float, default=DEFAULT_PAD_S, metavar="S",
                   help="audio kept either side of the located clip")
    q.add_argument("--max-silent", type=int, default=DEFAULT_MAX_SILENT,
                   metavar="N", help="stop after N silent captures in a row")
    q.add_argument("--no-restore-volume", action="store_true",
                   help="leave the players at the last matrix volume")

    args = ap.parse_args()

    if not args.player:
        args.player = list(DEFAULT_PLAYERS)
    if not args.ha_url or not args.ha_token:
        env = read_env_file(args.ha_env)
        args.ha_url   = args.ha_url   or env.get(HA_ENV_URL_KEY)
        args.ha_token = args.ha_token or env.get(HA_ENV_TOKEN_KEY)
    if not args.ha_url or not args.ha_token:
        ap.error(f"no Home Assistant URL/token — pass --ha-url/--ha-token, set "
                 f"HA_URL/HA_TOKEN, or put {HA_ENV_URL_KEY}/{HA_ENV_TOKEN_KEY} "
                 f"in {args.ha_env}")
    if not args.src.is_dir():
        ap.error(f"--src is not a directory: {args.src}")

    try:
        require_tools()
    except RuntimeError as e:
        print(e, file=sys.stderr)
        return 2

    args.exts = tuple(e if e.startswith(".") else f".{e}"
                      for e in (x.strip().lower() for x in args.ext.split(","))
                      if e)
    out_dir = args.out or args.src.parent / DEFAULT_OUT_NAME
    out_dir.mkdir(parents=True, exist_ok=True)

    cells: list[Cell] = []
    for spec in args.player:
        cells.extend(parse_player_arg(spec))

    api = Api(args.controller, args.token)
    if not authenticate(api, args.user):
        return 2

    device = resolve_device(api, args.device)
    device_id = device["device_id"]
    if not device.get("connected"):
        print(f"{device.get('label') or device_id} is not connected — capture "
              f"mode needs a live device.", file=sys.stderr)
        return 2

    ha = Ha(args.ha_url, args.ha_token)
    if not args.advertise:
        args.advertise = local_ip_towards(args.ha_url)

    # The local server exists for two jobs, and an Alexa run pulling its
    # recordings needs neither: the clip goes out through Home Assistant's
    # www/ and the recording is fetched from the controller.
    serve = None
    if args.webhook or not args.alexa:
        host, _, port = args.bind.rpartition(":")
        serve = Serve(host or "0.0.0.0", int(port or 0))
        serve.start()

    if args.alexa:
        base = args.public_base or args.ha_url
        player = AlexaPlayer(ha, args.hass_www, base,
                             normalise=not args.no_normalise,
                             lead_ms=args.lead_ms)
        # Amazon's round trip is seconds and varies per call, so the default
        # pre-roll (sized for a LAN player starting near-instantly) would
        # close the window before the audio ever plays.
        if args.pre_ms == DEFAULT_PRE_MS:
            args.pre_ms = ALEXA_PRE_MS
    else:
        media_base = args.media_base or f"http://{args.advertise}:{serve.port}"
        player = LocalPlayer(ha, serve, media_base)

    global PROGRESS
    PROGRESS = Progress(enabled=not args.no_progress)
    # Counting the corpus can take a while on a directory of millions, so it
    # runs alongside the work rather than in front of it.
    threading.Thread(target=count_cells, daemon=True,
                     args=(args.src, args.exts, args.skip, len(cells),
                           PROGRESS)).start()

    run = Run(args, api, ha, serve, player, device_id, cells, out_dir)

    def _stop(_sig, _frm):
        # First Ctrl+C asks the loop to finish the cell it is on; the cleanup
        # in `finally` is what actually matters, and it must run.
        if run.stopping:
            return
        run.stopping = True
        log("Stopping after this cell…")

    signal.signal(signal.SIGINT, _stop)
    signal.signal(signal.SIGTERM, _stop)

    try:
        return run.run()
    except KeyboardInterrupt:
        return 130
    finally:
        # Cleanup is the part that must not be interruptible: the cost of
        # skipping it is a device that answers nobody and players left at a
        # matrix volume. Same discipline as collect_device_clips' config
        # restore, and for the same reason.
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            run.stop_players()
            run.restore_volumes()
            run.disarm()
        finally:
            player.cleanup()
            if serve is not None:
                serve.close()
            PROGRESS.close()


if __name__ == "__main__":
    sys.exit(main())
