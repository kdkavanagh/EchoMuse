#!/usr/bin/env python3
"""
collect_device_clips.py — wake-phrase clips captured through the Echo's own
mic pipeline, for oww_forge's positive training set.

The README's strongest accent/pronunciation lever is "real recordings", and
the recordings that matter most are the ones the *device* makes: its mic
array, its gain, its room. A phone recording of the same voice trains the
model on a microphone that will never hear the wake word in service.

The device already has the capture path — `saveUtterances` keeps the mic
audio of a voice turn as a 16kHz mono WAV, which is exactly the format
`positive_train/` wants. Two things stop it being usable as-is, and this
script exists to handle both.

**The wake word is not in a wake-triggered turn.** Wake turns discard
VOICE_PREROLL_DISCARD frames of wake-word tail and drain the stale queue
before streaming (em_esphome._stream_mic_audio), so the saved audio starts
*after* the phrase. Button turns pass preroll_discard=0 and keep the whole
utterance, plus the device's 512ms preroll ring for the leading phoneme —
so the collection gesture is: press the action button, say the phrase.

**The turn stream is not quite the wake stream.** The mic chain itself is no
longer ours to differ on — Android's audio HAL hands back one processed mono
channel and every turn gets the identical signal, whatever triggered it — but
the utterance tap sits *below* the denoiser, and `nsAsr` is a controller-side
pass that the wake stream never sees. Turning it off is what makes the file on
disk the same audio the wake model is scored on in service. The device's own
gain, echo cancellation and beam selection are part of that path and are not
adjustable from here at all, which is exactly what you want represented.

**Every clip is trimmed and checked before it is kept.** A turn runs to Home
Assistant's VAD end, so the raw audio carries a second or more of room tone
that positives should not be trained on; ffmpeg takes it off both ends. Then
faster-whisper transcribes what is left and the clip is DISCARDED unless it
really contains the phrase (`--phrase`). That check exists because the mistake
is invisible otherwise: a mistimed press, a false start or the wrong words
produce a WAV that looks exactly like a good one in a directory listing, and
mislabelled positives are worse than fewer positives — they teach the model
that the wrong sound is the wake word. The transcriber is a genuinely
independent second opinion: a bigger model than the pipeline's, run on the
finished artefact, and never primed with the phrase it is checking for.

The config is restored on exit, including on Ctrl+C — that is the whole
reason this is a script and not a paragraph of instructions. `saveUtterances`
is the one setting in the system that writes recognisable speech to disk, so
leaving it on by accident is the failure worth engineering against; if the
restore itself fails, the body needed to undo it by hand is printed.

Everything here happens over the LAN, against the controller's HTTP API: the
config push rides the device's existing /control WebSocket, and the recording
is made controller-side (em_esphome buffers the ASR-bound stream, em_recordings
writes the WAV beside the DB). No USB, no adb, no firmware change — any device
already in the fleet will do.

Runs on the HOST (stdlib only, no aiohttp), unlike controller/tools/*.py
which run inside the container — it needs to write clips somewhere you can
feed to the trainer.

Needs `ffmpeg` on PATH and `pip install faster-whisper`; both are checked
before the device is reconfigured, so a missing one costs nothing.

    python3 oww_forge/tools/collect_device_clips.py --controller http://…:8768
    python3 oww_forge/tools/collect_device_clips.py -d Lounge \
        --phrase "hey clara" --out data/wakewords/hey_clara/…/positive_train

The first form lists the fleet; the second collects. Devices are named by
their dashboard label, not their serial. Then: press the dot, say the phrase,
pause. Repeat. Ctrl+C when done.
"""

from __future__ import annotations

import argparse
import difflib
import getpass
import io
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unicodedata
import urllib.error
import urllib.request
import wave
from pathlib import Path

# Sections that own the keys below (em_config_sections.SECTIONS). A device
# must OVERRIDE a section before its values can be set, so these are added to
# whatever the device already overrides and removed again on restore.
CAPTURE_SECTIONS = ("microphones",)

# What makes a button turn's recording match the stream openwakeword scores.
# See the module docstring for why each one.
CAPTURE_OVERRIDES = {
    "saveUtterances": True,   # the recording itself
    "nsAsr":          False,  # tap is BELOW the denoiser
}

# em_config_sections.STATE_KEYS — device state wearing a config key's
# clothes. Always in scope, so it rides every config POST; re-read at restore
# time rather than replayed, or a volume change made during collection would
# be silently rolled back with the rest.
STATE_KEYS = ("startupVolume",)

# Only the button gesture puts the wake phrase in the recording (see the
# docstring). em_controller passes trigger_label="button"; wake turns pass
# "wakeword(0.522)".
BUTTON_TRIGGER = "button"

# em_recordings.KEEP_PER_DEVICE — the retention window we are racing. Polling
# faster than a person can say ten phrases is the whole mitigation.
KEEP_PER_DEVICE = 10

# Trim: silence off both ends (the second pass runs reversed, since
# silenceremove only trims leading silence). A turn runs to Home Assistant's
# VAD end, so the tail is typically a second or more of room tone; positives
# want the phrase and little else. The 0.1s/0.2s floors leave a breath either
# side rather than splicing at the first sample over threshold — a hard cut on
# the leading phoneme is exactly the artefact the device's preroll ring exists
# to avoid, and re-introducing it here would train it in.
SILENCE_FILTER = (
    "silenceremove=start_periods=1:start_threshold={thr}:start_silence=0.1,"
    "areverse,"
    "silenceremove=start_periods=1:start_threshold={thr}:start_silence=0.2,"
    "areverse"
)

# Below this, whatever survived the trim was silence, not speech.
MIN_KEEP_SECONDS = 0.30

# A turn row is inserted before set_turn_audio names its WAV, so audio_file is
# briefly NULL on a turn that will have one. Give up on a turn after this many
# polls: no_speech turns and cancelled turns never get audio at all.
AUDIO_WAIT_POLLS = 8


class ApiError(RuntimeError):
    pass


class Api:
    """Minimal Bearer-auth JSON client for the controller HTTP API."""

    def __init__(self, base: str, token: str | None = None):
        self.base  = base.rstrip("/")
        self.token = token

    def _request(self, method: str, path: str, body=None, raw=False):
        url  = f"{self.base}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req  = urllib.request.Request(url, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
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
        return payload if raw else json.loads(payload or b"null")

    def get(self, path):            return self._request("GET", path)
    def get_bytes(self, path):      return self._request("GET", path, raw=True)
    def post(self, path, body):     return self._request("POST", path, body)

    def login(self, username: str, password: str) -> str:
        self.token = None
        out = self.post("/api/auth/login",
                        {"username": username, "password": password})
        self.token = out["token"]
        return out.get("role", "")


# ─── Config: save, apply, restore ─────────────────────────────────────────────

def read_config(api: Api, device_id: str) -> dict:
    """The device's effective config plus the sections it overrides."""
    out = api.get(f"/api/devices/{device_id}/config")
    return {
        "config":          dict(out["config"]),
        "config_sections": list(out.get("config_sections") or []),
    }


def _post_config(api: Api, device_id: str, values: dict, sections) -> None:
    """
    Write config. The whole effective dict is sent every time: POSTs REPLACE
    rather than merge (em_api._dropped_keys refuses a partial body with 409),
    and the handler filters to the keys actually in scope — so sending
    everything is both accepted and exact, and needs no client-side copy of
    the section->key map to drift out of date.
    """
    body = dict(values)
    body["config_sections"] = list(sections)
    api.post(f"/api/devices/{device_id}/config", body)


def apply_capture_config(api: Api, device_id: str, prior: dict) -> list[str]:
    sections = list(prior["config_sections"])
    for sid in CAPTURE_SECTIONS:
        if sid not in sections:
            sections.append(sid)
    values = dict(prior["config"])
    values.update(CAPTURE_OVERRIDES)
    _post_config(api, device_id, values, sections)

    changed = [f"{k}: {prior['config'].get(k)!r} -> {v!r}"
               for k, v in CAPTURE_OVERRIDES.items()
               if prior["config"].get(k) != v]
    return changed


def restore_config(api: Api, device_id: str, prior: dict) -> None:
    """
    Put the device back exactly as it was, and verify it took.

    Sections dropped here are pruned by set_device_config_sections before any
    value is written, so a section we added hands its keys back to the fleet
    rather than leaving shadow values behind.
    """
    values = dict(prior["config"])
    try:
        current = read_config(api, device_id)["config"]
        for key in STATE_KEYS:
            if key in current:
                values[key] = current[key]
    except ApiError:
        pass  # restoring the settings matters more than a stale volume

    _post_config(api, device_id, values, prior["config_sections"])

    after = read_config(api, device_id)
    bad = [k for k in CAPTURE_OVERRIDES
           if after["config"].get(k) != prior["config"].get(k)]
    if bad or after["config_sections"] != prior["config_sections"]:
        raise ApiError(f"config did not restore cleanly: {', '.join(bad) or 'sections'}")


def manual_restore_body(prior: dict) -> str:
    body = dict(prior["config"])
    body["config_sections"] = prior["config_sections"]
    return json.dumps(body, indent=2, sort_keys=True)


def _restore_or_explain(api: Api, device_id: str, base_url: str,
                        label: str, prior: dict) -> None:
    """
    Restore, retrying a couple of times, and say exactly what to do by hand if
    it still fails.

    SIGINT is ignored for the duration: this runs from the Ctrl+C path, and a
    second Ctrl+C landing here is precisely how saveUtterances would be left
    on. A transient network error gets three attempts for the same reason —
    the cost of not restoring is speech continuing to be written to disk.
    """
    previous = signal.getsignal(signal.SIGINT)
    signal.signal(signal.SIGINT, signal.SIG_IGN)
    try:
        last: Exception | None = None
        for attempt in range(3):
            try:
                restore_config(api, device_id, prior)
                print(f"Config restored on {label} (saveUtterances back to "
                      f"{prior['config'].get('saveUtterances')!r}).")
                return
            except ApiError as e:
                last = e
                if attempt < 2:
                    time.sleep(2)
        print(f"\n!! CONFIG NOT RESTORED: {last}", file=sys.stderr)
        print("!! saveUtterances may still be ON — the device may still be "
              "writing recognisable speech to disk.", file=sys.stderr)
        print(f"!! Undo by hand: POST {base_url.rstrip('/')}"
              f"/api/devices/{device_id}/config with:\n"
              f"{manual_restore_body(prior)}", file=sys.stderr)
    finally:
        signal.signal(signal.SIGINT, previous)


# ─── Collection ───────────────────────────────────────────────────────────────

def wav_seconds(data_or_path) -> float:
    try:
        src = (io.BytesIO(data_or_path) if isinstance(data_or_path, bytes)
               else str(data_or_path))
        with wave.open(src, "rb") as w:
            return w.getnframes() / float(w.getframerate() or 1)
    except Exception:
        return 0.0


# ─── Trim ─────────────────────────────────────────────────────────────────────

def require_ffmpeg() -> None:
    if shutil.which("ffmpeg") is None:
        raise RuntimeError(
            "ffmpeg is not on PATH — it is needed to trim the clips. "
            "Install it, or pass --no-trim to keep the raw turn audio.")


def trim_clip(src: Path, dst: Path, threshold_db: int) -> tuple[bool, str]:
    """
    Trim silence off both ends of a clip. Returns (ok, note).

    The output format is pinned to 16kHz mono s16 even though the input
    already is: this is the file that goes into positive_train/, and stating
    the contract costs nothing next to an ffmpeg invocation.
    """
    cmd = [
        "ffmpeg", "-nostdin", "-loglevel", "error", "-y", "-i", str(src),
        "-af", SILENCE_FILTER.format(thr=f"{threshold_db}dB"),
        "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(dst),
    ]
    try:
        done = subprocess.run(cmd, capture_output=True, text=True, timeout=60)
    except subprocess.TimeoutExpired:
        return False, "ffmpeg timed out"
    if done.returncode != 0 or not dst.is_file():
        return False, (done.stderr or "ffmpeg failed").strip().splitlines()[-1:][0] \
            if done.stderr.strip() else "ffmpeg failed"
    return True, ""


# ─── Speech verification ──────────────────────────────────────────────────────

_PUNCT = re.compile(r"[^\w\s]", re.UNICODE)
_WS    = re.compile(r"\s+")


def normalise_text(text: str) -> str:
    """
    Fold a transcript and a target phrase onto common ground.

    Whisper punctuates and capitalises ("Hey, Clara."), the target does not,
    and neither difference is a mismatch. Accents are stripped for the same
    reason — a model that writes "Clára" has still heard the phrase.
    """
    text = unicodedata.normalize("NFKD", text or "")
    text = "".join(c for c in text if not unicodedata.combining(c))
    return _WS.sub(" ", _PUNCT.sub(" ", text.lower())).strip()


def phrase_match(transcript: str, targets: list[str],
                 min_ratio: float) -> tuple[bool, float, str]:
    """
    Did the speaker say (one of) the target phrase(s)?

    Exact containment first, then a sliding window over the transcript's
    words, so a near-miss spelling ("hey clara" heard as "hey claira") passes
    while an unrelated sentence does not. The window is the target's own word
    count ±1: comparing a 2-word phrase against a 20-word transcript ratios to
    nearly zero however clearly the phrase was said.

    A same-length window is scored **word by word, taking the WORST pair**,
    not as one string. Wake phrases are a common word plus a distinctive one,
    and whole-string similarity lets the common half carry the wrong half over
    the bar: "hey sarah" scores 0.78 against "hey clara" — above any threshold
    loose enough to accept "hey claira" (0.95) — purely on the shared "hey"
    and a common "ara". Per word, the name is judged on its own and the
    imposter drops to 0.60 while the near miss stays at 0.91. Windows of a
    different length have no pairing to do and fall back to whole-string,
    which is what catches a run-together "heyclara".

    Returns (matched, best_ratio, which_target).
    """
    heard = normalise_text(transcript)
    best  = (0.0, targets[0] if targets else "")
    if not heard:
        return False, 0.0, best[1]

    def ratio(a: str, b: str) -> float:
        return difflib.SequenceMatcher(None, a, b).ratio()

    words = heard.split()
    for target in targets:
        want = normalise_text(target)
        if not want:
            continue
        if want in heard:
            return True, 1.0, target
        want_words = want.split()
        n = len(want_words)
        for size in {max(1, n - 1), n, n + 1}:
            for i in range(0, max(1, len(words) - size + 1)):
                window = words[i:i + size]
                if len(window) == n:
                    score = min(ratio(a, b) for a, b in zip(want_words, window))
                else:
                    score = ratio(want, " ".join(window))
                if score > best[0]:
                    best = (score, target)
    return best[0] >= min_ratio, best[0], best[1]


class Transcriber:
    """
    faster-whisper, held open for the session.

    Loaded BEFORE the device config is touched (see main): a missing wheel or
    a failed model download must not be discovered after an hour of
    collecting, with saveUtterances still on.

    The target phrase is deliberately NOT passed as `initial_prompt`. Biasing
    the decoder toward the words we are checking for would make it agree with
    us, which is the one thing this must not do — the entire point is a
    second opinion that did not come from the wake pipeline.
    """

    def __init__(self, model_name: str, device: str, compute_type: str):
        try:
            from faster_whisper import WhisperModel
        except ImportError:
            raise RuntimeError(
                "faster-whisper is not installed — it is what checks each clip "
                "really contains the phrase.\n"
                "  pip install faster-whisper\n"
                "Or pass --no-verify to keep every clip unchecked.") from None
        print(f"Loading faster-whisper '{model_name}' "
              f"({device}/{compute_type}) — first run downloads the model…")
        t0 = time.monotonic()
        self._model = WhisperModel(model_name, device=device,
                                   compute_type=compute_type)
        print(f"  ready in {time.monotonic() - t0:.1f}s")

    def transcribe(self, path: Path, language: str) -> str:
        segments, _info = self._model.transcribe(
            str(path),
            language=language or None,
            beam_size=5,
            condition_on_previous_text=False,
        )
        return " ".join(seg.text.strip() for seg in segments).strip()


def slug(text: str) -> str:
    out = re.sub(r"[^A-Za-z0-9]+", "-", (text or "").strip()).strip("-").lower()
    return out or "device"


_print_lock = threading.Lock()


def log(*parts) -> None:
    """Single writer for stdout — the processor thread prints too."""
    with _print_lock:
        print(*parts, flush=True)


class Processor:
    """
    Trim, transcribe and judge each clip off the collection loop.

    On its own thread because the download must not wait for it. Only ten
    recordings per device survive on the controller
    (em_recordings.KEEP_PER_DEVICE), so a clip not fetched promptly is gone
    for good, while a clip already on local disk can be processed at leisure.
    Whisper 'medium' takes seconds per clip; a few of those in a row would be
    long enough to lose one.
    """

    _STOP = object()

    def __init__(self, out_dir: Path, work_dir: Path, rejects_dir: Path | None,
                 stem: str, transcriber: Transcriber | None,
                 targets: list[str], min_ratio: float, threshold_db: int,
                 do_trim: bool, language: str):
        self.out_dir     = out_dir
        self.work_dir    = work_dir
        self.rejects_dir = rejects_dir
        self.stem        = stem
        self.transcriber = transcriber
        self.targets     = targets
        self.min_ratio   = min_ratio
        self.threshold_db = threshold_db
        self.do_trim     = do_trim
        self.language    = language

        self.kept: list[Path] = []
        self.rejected = 0
        self.failed   = 0
        # Ratios of clips rejected for not matching, so the summary can tell
        # "nobody said the phrase" apart from "the bar is a shade too high".
        # Whisper's spelling is not the target's — it wrote "Mr. Quilter" for
        # a ground-truth "MISTER QUILTER", scoring 0.74 against a 0.75 bar —
        # and that is a threshold question, not a bad recording.
        self.near_ratios: list[float] = []

        self._q      = queue.Queue()
        self._thread = threading.Thread(target=self._run, daemon=True,
                                        name="clip-processor")
        self._thread.start()

    # — public —

    def submit(self, turn_id: int, raw: Path) -> None:
        self._q.put((turn_id, raw))

    def pending(self) -> int:
        return self._q.qsize()

    def close(self) -> None:
        self._q.put(self._STOP)
        self._thread.join()

    # — worker —

    def _run(self) -> None:
        while True:
            item = self._q.get()
            if item is self._STOP:
                return
            turn_id, raw = item
            try:
                self._process(turn_id, raw)
            except Exception as e:                      # never kill the thread
                self.failed += 1
                log(f"  ! turn {turn_id}: processing failed ({e}) — clip kept "
                    f"at {raw}")

    def _process(self, turn_id: int, raw: Path) -> None:
        final = self.work_dir / f"final_{turn_id}.wav"

        if self.do_trim:
            ok, note = trim_clip(raw, final, self.threshold_db)
            if not ok:
                self.failed += 1
                log(f"  ! turn {turn_id}: trim failed ({note}) — using untrimmed")
                final = raw
        else:
            final = raw

        raw_s, cut_s = wav_seconds(raw), wav_seconds(final)
        if cut_s < MIN_KEEP_SECONDS:
            self._reject(turn_id, raw,
                         f"nothing but silence after trim ({raw_s:.1f}s -> "
                         f"{cut_s:.2f}s)")
            return

        heard = ""
        if self.transcriber is not None:
            heard = self.transcriber.transcribe(final, self.language)
            matched, ratio, target = phrase_match(heard, self.targets,
                                                  self.min_ratio)
            if not matched:
                self.near_ratios.append(ratio)
                self._reject(
                    turn_id, raw,
                    f'heard "{heard or "(nothing)"}" — no match for '
                    f'"{target}" (best {ratio:.2f} < {self.min_ratio:.2f})')
                return

        dest = self.out_dir / f"{self.stem}_turn{turn_id}.wav"
        shutil.move(str(final), str(dest))
        self.kept.append(dest)
        detail = f'  "{heard}"' if heard else ""
        log(f"  ✓ {len(self.kept):>3}  turn {turn_id}  "
            f"{raw_s:.1f}s -> {cut_s:.1f}s  {dest.name}{detail}")

    def _reject(self, turn_id: int, raw: Path, why: str) -> None:
        self.rejected += 1
        where = ""
        if self.rejects_dir is not None:
            self.rejects_dir.mkdir(parents=True, exist_ok=True)
            kept_at = self.rejects_dir / f"{self.stem}_turn{turn_id}.wav"
            shutil.copyfile(raw, kept_at)
            where = f" (kept at {kept_at.name})"
        log(f"  ✗ turn {turn_id} DISCARDED: {why}{where}")


def collect(api: Api, device_id: str, work_dir: Path, poll: float,
            any_trigger: bool, processor: Processor) -> None:
    """
    Poll for new turns and pull their audio until interrupted.

    Turn ids are monotonic rowids, so a baseline taken now is all that is
    needed to ignore everything already on the device.
    """
    existing = api.get(f"/api/devices/{device_id}/turns?limit=200")
    baseline = max((t["turn_id"] for t in existing), default=0)

    attempts: dict[int, int] = {}
    done: set[int] = set()
    pruned_warned = False

    while True:
        try:
            turns = api.get(f"/api/devices/{device_id}/turns?limit=50")
        except ApiError as e:
            log(f"  ! poll failed ({e}) — retrying")
            time.sleep(poll)
            continue

        for turn in turns:
            tid = turn["turn_id"]
            if tid <= baseline or tid in done:
                continue
            trigger = (turn.get("trigger") or "")
            if not any_trigger and trigger != BUTTON_TRIGGER:
                done.add(tid)
                continue
            if not turn.get("audio_file"):
                attempts[tid] = attempts.get(tid, 0) + 1
                if attempts[tid] >= AUDIO_WAIT_POLLS:
                    done.add(tid)
                    log(f"  · turn {tid} ({turn.get('outcome') or '?'}) — "
                        f"no audio, skipped")
                continue

            try:
                data = api.get_bytes(f"/api/devices/{device_id}/turns/{tid}/audio")
            except ApiError as e:
                done.add(tid)
                if "404" in str(e):
                    if not pruned_warned:
                        pruned_warned = True
                        log(f"  ! turn {tid}'s WAV was already pruned "
                            f"(only {KEEP_PER_DEVICE} are kept per device) — "
                            f"leave a beat between phrases")
                else:
                    log(f"  ! turn {tid}: {e}")
                continue

            # Straight to local disk, then handed off: the controller keeps
            # only KEEP_PER_DEVICE recordings, so fetching must never wait on
            # trimming or a whisper pass.
            raw = work_dir / f"raw_turn{tid}.wav"
            raw.write_bytes(data)
            done.add(tid)
            processor.submit(tid, raw)

        time.sleep(poll)


def _describe(device: dict) -> str:
    state = "connected" if device.get("connected") else "offline"
    if device.get("muted"):
        state += ", muted"
    return (f"{device.get('label') or '(unlabelled)'}  "
            f"[{state}]  {device.get('device_id') or ''}")


def print_fleet(devices: list[dict], stream=sys.stdout) -> None:
    print("Devices (-d <name>):", file=stream)
    for d in sorted(devices, key=lambda x: (not x.get("connected"),
                                            (x.get("label") or "").lower())):
        print(f"  {_describe(d)}", file=stream)


def list_devices(api: Api) -> int:
    try:
        devices = api.get("/api/devices")
    except ApiError as e:
        print(f"Cannot list devices: {e}", file=sys.stderr)
        return 1
    if not devices:
        print("No devices registered on this controller.")
        return 1
    print_fleet(devices)
    return 0


def resolve_device(api: Api, wanted: str) -> dict:
    """
    Find a device by its NAME — the label shown in the dashboard.

    Names are what people have; ro.serialno is what the API keys on, and
    nobody remembers which Echo is G090LF00000000AA. Matching is
    case-insensitive and falls back to a unique substring ("lounge" for
    "Lounge Dot"), so the name typed at the shell is the name on screen.

    An id is still accepted — a label could in principle look like one, so
    labels are matched FIRST and the id is the fallback, never the reverse.

    Ambiguity is an error rather than a guess: two devices called "Bedroom"
    and "Bedroom 2" both match "bedroom", and picking one silently would
    reconfigure the wrong Echo.
    """
    devices = api.get("/api/devices")
    if not devices:
        raise ApiError("no devices are registered on this controller")

    key = wanted.strip().lower()

    def unique(matches: list[dict], how: str) -> dict | None:
        if len(matches) == 1:
            return matches[0]
        if len(matches) > 1:
            lines = "\n".join(f"  {_describe(d)}" for d in matches)
            raise ApiError(f"{how} matches {len(matches)} devices:\n{lines}\n"
                           f"Use a longer name, or the device id.")
        return None

    for how, matches in (
        ("name",            [d for d in devices
                             if (d.get("label") or "").strip().lower() == key]),
        ("device id",       [d for d in devices
                             if (d.get("device_id") or "").lower() == key]),
        ("partial name",    [d for d in devices
                             if key in (d.get("label") or "").lower()]),
    ):
        found = unique(matches, how)
        if found:
            return found

    buf = io.StringIO()
    print_fleet(devices, buf)
    raise ApiError(f"no device named {wanted!r}.\n{buf.getvalue().rstrip()}")


# ─── Entry point ──────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser(
        description="Collect wake-phrase clips through an Echo's own mic pipeline.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="Press the action button, say the phrase, pause. Ctrl+C when done.",
    )
    ap.add_argument("-d", "--device", metavar="NAME",
                    help="device name, as labelled in the dashboard "
                         "(case-insensitive; a unique part of the name will "
                         "do, and a device id is also accepted). Omit to list "
                         "the fleet and exit.")
    ap.add_argument("--controller", default=os.environ.get("EM_CONTROLLER",
                                                           "http://localhost:8768"),
                    help="controller base URL (env EM_CONTROLLER)")
    ap.add_argument("--token", default=os.environ.get("EM_TOKEN"),
                    help="existing session token (env EM_TOKEN); otherwise log in")
    ap.add_argument("--user", help="dashboard username (admin role required)")
    ap.add_argument("-o", "--out", metavar="DIR",
                    help="directory to write the WAVs into, created if needed "
                         "(default ./wake_clips/<label>-<date>). Point it "
                         "straight at a wake word's positive_train/ to skip "
                         "the copy step; clips are named after the device, "
                         "not the directory.")
    ap.add_argument("--poll", type=float, default=2.0,
                    help="seconds between turn polls (default 2.0)")
    ap.add_argument("--any-trigger", action="store_true",
                    help="also keep wake-triggered turns — their audio starts "
                         "AFTER the wake word, so they are not positives")
    ap.add_argument("--no-config", action="store_true",
                    help="collect only; do not touch (or restore) device config")

    v = ap.add_argument_group("verification")
    v.add_argument("-p", "--phrase", metavar="TEXT",
                   help="the wake phrase. Every clip is transcribed with "
                        "faster-whisper and DISCARDED unless it matches. "
                        "Comma-separate spelling variants ('hey clara, hey "
                        "clarra') to accept any of them, matching the "
                        "trainer's own target_phrase field.")
    v.add_argument("--no-verify", action="store_true",
                   help="keep every clip without transcribing it")
    v.add_argument("--rejects", metavar="DIR",
                   help="keep discarded clips here (default: delete them, "
                        "the reason is always logged)")
    v.add_argument("--match-ratio", type=float, default=0.75, metavar="0-1",
                   help="similarity needed to accept a near miss (default "
                        "0.75); exact containment always passes")
    v.add_argument("--stt-model", default="medium",
                   help="faster-whisper model (default medium; large-v3 is "
                        "stricter and slower, small is the other way)")
    v.add_argument("--stt-device", default="auto",
                   help="auto | cpu | cuda (default auto)")
    v.add_argument("--stt-compute", default="int8",
                   help="compute type: int8 (CPU default), float16 (GPU)")
    v.add_argument("--stt-language", default="en",
                   help="language hint, '' to autodetect (default en)")

    t = ap.add_argument_group("trimming")
    t.add_argument("--no-trim", action="store_true",
                   help="keep the full turn audio instead of trimming silence "
                        "off both ends")
    t.add_argument("--silence-db", type=int, default=-45, metavar="DB",
                   help="silence threshold for the trim (default -45)")
    args = ap.parse_args()

    if args.phrase and args.no_verify:
        ap.error("--phrase and --no-verify contradict each other")
    targets = [p.strip() for p in (args.phrase or "").split(",") if p.strip()]
    verify  = bool(targets) and not args.no_verify
    if not verify and not args.no_verify and args.device:
        print("Note: no --phrase given, so nothing is checked — every clip is "
              "kept. Pass --phrase 'hey clara' to discard the misfires, or "
              "--no-verify to silence this.", file=sys.stderr)

    api = Api(args.controller, args.token)
    if not api.token:
        user = args.user or input("Username [admin]: ").strip() or "admin"
        pw   = os.environ.get("EM_PASSWORD") or getpass.getpass(f"Password for {user}: ")
        try:
            role = api.login(user, pw)
        except ApiError as e:
            print(f"Login failed: {e}", file=sys.stderr)
            return 1
        if role != "admin" and not args.no_config:
            print("Warning: config changes need the admin role; this session is "
                  f"'{role}'. Use --no-config to collect without them.", file=sys.stderr)

    if not args.device:
        return list_devices(api)

    try:
        device = resolve_device(api, args.device)
    except ApiError as e:
        print(f"{e}", file=sys.stderr)
        return 1
    device_id = device["device_id"]
    label     = device.get("label") or device_id
    print(f"Device: {_describe(device)}")
    if not device.get("connected"):
        print(f"Warning: {label} is not connected — nothing will be captured "
              f"until it is.", file=sys.stderr)
    if device.get("muted"):
        print(f"Warning: {label} is muted — a dot press while muted refuses "
              f"the voice turn. Unmute before collecting.", file=sys.stderr)

    stem    = slug(label)
    out_dir = Path(args.out).expanduser() if args.out else (
        Path("wake_clips") / f"{stem}-{time.strftime('%Y%m%d-%H%M')}")
    try:
        out_dir.mkdir(parents=True, exist_ok=True)
    except OSError as e:
        print(f"Cannot use output directory {out_dir}: {e}", file=sys.stderr)
        return 1
    # Resolved once and used everywhere below: a relative --out printed back
    # relative is ambiguous the moment the shell that ran this has moved on.
    out_dir = out_dir.resolve()
    pre_existing = {p.name for p in out_dir.glob("*.wav")}
    if pre_existing:
        print(f"Note: {out_dir} already holds {len(pre_existing)} wav(s) — "
              f"they are left alone and not counted.")

    rejects_dir = Path(args.rejects).expanduser().resolve() if args.rejects else None

    # Everything that can fail on this machine fails HERE, before the device's
    # config is touched: a missing ffmpeg or an unavailable whisper model
    # discovered later would leave saveUtterances on while the operator went
    # to install something.
    transcriber = None
    try:
        if not args.no_trim:
            require_ffmpeg()
        if verify:
            transcriber = Transcriber(args.stt_model, args.stt_device,
                                      args.stt_compute)
    except RuntimeError as e:
        print(f"{e}", file=sys.stderr)
        return 1

    prior = None
    if not args.no_config:
        try:
            prior = read_config(api, device_id)
            changed = apply_capture_config(api, device_id, prior)
        except ApiError as e:
            print(f"Could not apply capture config: {e}", file=sys.stderr)
            return 1
        print(f"Capture config applied to {label}:")
        for line in changed:
            print(f"  {line}")
        if not changed:
            print("  (both already set)")
        print(f"  sections overridden: {', '.join(CAPTURE_SECTIONS)} "
              f"(was: {', '.join(prior['config_sections']) or 'fleet'})")
    else:
        print(f"Collecting from {label} with config untouched (--no-config).")

    print()
    print(f"Saving to {out_dir}")
    if not args.no_trim:
        print(f"Trimming silence off both ends at {args.silence_db}dB.")
    if verify:
        print(f"Verifying with faster-whisper '{args.stt_model}' against "
              f"{' / '.join(repr(t) for t in targets)} — anything else is "
              f"discarded" + (f", copies kept in {rejects_dir}." if rejects_dir
                              else " (use --rejects DIR to keep copies)."))
    print("Mute must be OFF — a dot press while muted refuses the turn.")
    print("Home Assistant answers every press ('sorry, I don't understand'). Expected.")
    print("Press the action button, say the wake phrase, pause. Ctrl+C when done.")
    print()

    # SIGTERM through the same exit as Ctrl+C, so a kill still restores the
    # config rather than leaving saveUtterances on.
    def _term(_signum, _frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, _term)

    work_dir  = Path(tempfile.mkdtemp(prefix="em-clips-"))
    processor = Processor(
        out_dir=out_dir, work_dir=work_dir, rejects_dir=rejects_dir, stem=stem,
        transcriber=transcriber, targets=targets, min_ratio=args.match_ratio,
        threshold_db=args.silence_db, do_trim=not args.no_trim,
        language=args.stt_language,
    )
    try:
        collect(api, device_id, work_dir, args.poll, args.any_trigger, processor)
    except KeyboardInterrupt:
        print()
    finally:
        # Config first, processing second: the device is reconfigured on
        # someone's shelf and a whisper backlog is local work, so the shorter
        # the window with saveUtterances on the better.
        if prior is not None:
            _restore_or_explain(api, device_id, args.controller, label, prior)
        left = processor.pending()
        if left:
            print(f"Finishing {left} queued clip(s)…")
        processor.close()
        shutil.rmtree(work_dir, ignore_errors=True)

    # Globbed rather than counted from the processor: --out may already have
    # held clips, and only what landed there this run should be reported.
    saved = sorted(p for p in out_dir.glob("*.wav") if p.name not in pre_existing)

    print()
    tally = [f"{len(saved)} kept"]
    if processor.rejected:
        tally.append(f"{processor.rejected} discarded")
    if processor.failed:
        tally.append(f"{processor.failed} failed")
    print(", ".join(tally) + ".")

    # A cluster of rejects just under the bar is a threshold problem, not a
    # collection problem, and the two want opposite responses. Say which.
    near = [r for r in processor.near_ratios if r >= args.match_ratio - 0.15]
    if near:
        suggest = max(0.5, round(min(near) - 0.02, 2))
        print(f"{len(near)} clip(s) missed by less than 0.15 (best "
              f"{max(near):.2f}, threshold {args.match_ratio:.2f}). If those "
              f"transcripts look right to you, re-run with "
              f"--match-ratio {suggest} — whisper's spelling is not "
              f"necessarily yours.")

    if not saved:
        if processor.rejected:
            print("Every clip was rejected. The transcripts above show what "
                  "whisper heard; a consistent near miss wants --match-ratio "
                  "lowered or the spelling adding to --phrase.")
        else:
            print("No clips collected.")
        return 0

    what = " and ".join(
        ([] if args.no_trim else ["trimmed"]) + (["verified"] if verify else [])
    ) or "unprocessed"
    print(f"{what.capitalize()}, in:")
    print(f"  {out_dir}")
    if rejects_dir is not None and processor.rejected:
        print(f"Discarded clips kept in:\n  {rejects_dir}")
    print()
    print("  These are ready for training. Add them via the forge UI's")
    print("  \"+ Recordings…\", or copy into data/wakewords/<name>/…/")
    print("  positive_train/ directly. Hold a few back as an eval set:")
    print("    forge test <name> --wav <dir>")
    return 0


if __name__ == "__main__":
    sys.exit(main())
