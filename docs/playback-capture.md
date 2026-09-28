# Playback capture: re-recording a corpus through the Echo

Sample collection (`em_samples`) records real speech from the room.
It does not produce the other thing a wake model or an endpoint qualification
set needs — **a large, already-labelled corpus carrying this device's signal
chain**. Common Voice on disk is clean studio speech. The same clips played
into a room and captured off the Echo carry the mic array, the native AFE's
beam selection and gain, the room's reverb and noise floor, the distance and
whatever the playing speaker does to the spectrum: every part of the path the
on-device BCResNet detector scores in service, and none of which augmentation
can invent from a clean file. BCResNet training itself lives outside this
repository (`~/git/bcresnet`); this is how its device-chain material is made.

A playback matrix turns one corpus into several. Each source clip is played
from each of several Home Assistant media players, at each of several
volumes. Position gives direction, distance and reverb; volume gives SNR.

The controller provides the recording half as **capture mode**
(`controller/em_capture.py`, lifecycle in `em_controller`, routes in
`em_api`): a recording window opened and closed by whoever is driving, handed
back whole. The driver — the thing that plays the files and files the
recordings — is yours; [Writing a driver](#writing-a-driver) lists what it
has to get right.

---

## Why a window and not the sample collector

`em_samples.Segmenter` cuts an unattended stream at the silences because
nobody knows when the speech is coming. Here the driver knows exactly — it is
the thing playing it — and it must pair each recording with the file that
produced it.

Run a 5s Common Voice clip through the segmenter and the result is
nondeterministic: `silence_ms=400` splits the sentence at its internal pauses
into two or three clips, `max_clip_ms=6000` truncates anything longer and
then refuses to reopen until the room is quiet, and a clip played at the
bottom of a volume matrix may never clear `open_margin_db` and produce
nothing at all. One playback becomes 0..N files with no way to tell which.

So capture mode records a window, and hands back exactly one file per window.

What it shares with the other recording modes (sample collection, ambient
recording):

- **the audio source.** While any recording mode is armed, the device's
  session actor holds one `diagnostic` uplink lease and every mode taps its
  live mic timeline — the native AFE's 16 kHz output, the same samples the
  on-device wake detector scores. The controller applies no gain, denoising
  or resampling before the tap. All armed modes receive the same blocks.
- **the assistant suspension.** The actor refuses wake candidates and button
  turns while a diagnostic lease is wanted (refusal reason `diagnostic`, with
  the error cue on the ring). Alerts still ring and physical stop still works.
- **the ring.** A slow magenta pulse (`em_device`) for as long as any
  recording mode is armed: "this device is recording and will not answer
  you".

Trimming is **not** done on the controller. The driver has ffmpeg and the
source clip to compare against; the controller has neither, and a trim rule
baked in at that end is one the driver cannot change without a redeploy.

---

## The controller side

### API

```
POST   /api/devices/{id}/capture               {"enabled": true,
                                                "webhook": "http://host:port/clip",   (optional)
                                                "idle_s": 300}
POST   /api/devices/{id}/capture               {"enabled": false}
POST   /api/devices/{id}/capture/window        {"tag": "...", "max_ms": 12000}
                                            -> {"session": "...", "tag": ..., "max_ms": ...}
POST   /api/devices/{id}/capture/window/stop   {"session": "..."}
GET    /api/devices/{id}/capture/recording?session=…&wait=…
GET    /api/devices/{id}/capture
```

Admin-only, and unlike sample collection it **requires a connected, approved
device running v1 firmware** (a device waiting for its firmware upgrade gets
`409 upgrade_required`). Arming an offline device is meaningful for
collection (the mode is persisted and re-applied on connect) and meaningless
here.

`max_ms` is clamped to `MAX_WINDOW_MS` (30 s; default 12 s). `idle_s`
defaults to 300 and is capped at 3600.

### Two delivery transports

- **Push** — arm with a `webhook`. Each finished window is POSTed to it.
- **Pull** — arm without one. The controller holds each finished recording
  and the driver collects it from `GET …/capture/recording`, which long-polls
  up to `wait` seconds (default 30, capped at 120). `404` means nothing
  arrived in that time — an ordinary answer, not an error. Pull needs no
  route from the controller back to the driver, which a controller on a
  macvlan network does not have to its own Docker host.

Both carry the same body and headers: the WAV (16 kHz mono PCM16), with
metadata in `X-EM-Tag`, `X-EM-Ms`, `X-EM-Peak-Db`, `X-EM-Floor-Db`,
`X-EM-Truncated`, `X-EM-Dropped`, `X-EM-Frames`, `X-EM-Session`,
`X-EM-Device`, `X-EM-Opened-Ms`. Headers rather than multipart so both ends
stay dependency-free: the receiver writes the body straight to disk and reads
the rest off the headers.

### The mode is not persisted, and that is the opposite call to sample collection

A collect flag survives a restart because the person walking the house saying
the wake word should not lose their session to a `docker compose up`. Capture
mode belongs to a **running process**. A controller that came back up still
suspended, holding recordings for a driver that no longer exists, would be a
device answering nothing for a reason nothing on screen explains. Capture
state lives on the `Device` object only and is cleared when the device's
session is lost — a driver must re-arm after a reconnect.

### Three things that stop it stranding a device

- **`idle_s` is a dead-man.** No window opened and no API call within it and
  the mode clears itself, the ring goes out, voice turns resume, and a device
  log line says why. It never expires underneath an open window — a caller
  may legitimately open a 30s window and say nothing for its duration.
- **A window closes two ways**: on audio time at `max_ms`, noticed by the
  frame path, and on the **wall clock** at `max_ms + WINDOW_GRACE_MS` (2 s),
  noticed by the watchdog. The second is not redundant: a device that stops
  sending audio stops advancing audio time, so a stalled link would otherwise
  hold the window open forever.
- **The disarm never cancels the task it is running on.** The watchdog calls
  `set_capture_mode(False)` itself, so a blind `task.cancel()` would raise
  `CancelledError` at the next await and abandon the rest of the disarm —
  webhook and queue left set, no log line, no state push.
  `tests/test_capture.py` pins it.

### Delivery rules

- **The mic path never waits on HTTP.** The tap appends bytes and returns;
  a push POST runs on a per-device sender task off a bounded queue
  (`CAPTURE_QUEUE_MAX`, 4). A slow webhook degrades to dropped *windows* —
  counted, logged, and reported to the next delivery as `X-EM-Dropped` —
  never to a stuttering microphone.
- **Redirects are not followed** and the scheme is checked before the mode is
  armed (`em_capture.valid_webhook`). The caller is an authenticated admin,
  so this is not authorisation; it is the cheap half of not building an SSRF
  gadget out of a controller sitting on a home LAN.
- **An empty window is a failure, not a delivery.** A zero-length recording
  read as success is how a caller ends up with a corpus of silence it
  believes in.
- **A result is consumed on read** in pull mode, so one recording cannot be
  collected twice and paired with two different source files.
- `tag` is opaque and echoed verbatim. Correlation is the caller's business,
  and a tag format the controller understood would be a second copy to drift.

---

## Writing a driver

The driver owns the matrix, plays the files through Home Assistant, and
files the recordings. Whatever it is written in, these are the properties
that decide whether a long unattended run produces a usable corpus.

### Per cell

```
volume_set -> settle -> open window -> play_media -> sleep(pre + duration + post)
           -> stop window -> collect the recording for this tag -> trim -> write
```

- **Loop files outer, matrix inner.** It costs a volume change per cell
  instead of one per pass, and buys the property that matters when the run
  has no fixed end: each source clip is finished across the whole matrix
  before the next starts, so whenever the run is stopped the set is balanced.
- **Judge playback end by the source duration** (from `ffprobe`), not by
  polling `media_player` state. Some players (Music Assistant flow players,
  for one) lag and misreport their transitions.
- **Stop the window yourself.** `max_ms` is a backstop; leaving it to close
  every window marks every recording `truncated` and throws away the signal
  that a playback *overran*. Pass the window's `session` to `…/window/stop`
  so a stop cannot land on a newer window.
- **Discard a recording for any other tag.** A late arrival from the previous
  cell is the one thing that would pair a recording with the wrong source
  file — and from there every file in the run is mislabelled by one.
- **Re-check the mode every cell** (`GET …/capture`). Capture mode does not
  survive a device reconnect; noticing costs one GET and turns a silent run
  of failures into a hiccup.
- **Serve the source clip to HA yourself** with a per-cell URL, so the media
  route cannot read arbitrary paths off the host. Honour single-range
  `Range` requests; several players send one.
- **Treat volumes ≤ 1 as fractions** if you accept both forms, so a typo
  cannot silently blast a room.

### Output and resume

- One filename formatter and one regex, shared by the writer and the resume
  check — two copies of a naming convention is how a resumed run silently
  re-records everything.
- For very large source directories (Common Voice's clips directory holds
  ~1.9M entries), resume with one `stat` per candidate and stream the
  directory with `os.scandir`; never glob, list or sort it.
- Keep a manifest row per recording (source, player, volume, durations,
  peak/floor dB, truncated) and a separate failures file.

### Failure handling over a long unattended run

- **No recording inside the grace window** → record the cell as failed and
  continue.
- **Peak barely above the window's own floor** (`X-EM-Peak-Db` vs
  `X-EM-Floor-Db`) → the capture was silence. Abort after a run of
  consecutive silent captures, rather than discovering at 3am that six hours
  went to a muted entity.
- **Trimmed to nothing** → discard rather than write, so the corpus never
  gains a file of room tone.
- **On exit — normal, exception or Ctrl+C** → stop the players, restore
  volumes and disarm capture mode, ignoring signals while it happens. The
  controller's `idle_s` dead-man is the backstop behind that.
