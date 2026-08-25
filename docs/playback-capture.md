# Playback capture: re-recording a corpus through the Echo

`oww_forge` trains on synthetic TTS positives; `em_samples` collects real
speech from the room. Neither produces the third thing a wake model needs —
**a large, already-labelled corpus carrying this device's signal chain**.
Common Voice on disk is clean studio speech. The same clips played into a
room and captured off the Echo's mic array carry the array, the beam the HAL
selected and the gain it applied, the room's reverb and noise floor, the
distance and whatever the
playing speaker does to the spectrum: every part of the path the model is
scored on in service, and none of which augmentation can invent from a clean
file.

The playback matrix turns one corpus into several. Each source clip is played
from each of several Home Assistant media players, at each of several
volumes. Position gives direction, distance and reverb; volume gives SNR.

Two pieces:

- **`controller/em_capture.py`** — a recording window opened and closed by
  whoever is driving, whose audio is POSTed to a webhook rather than written
  to disk.
- **`oww_forge/tools/playback_matrix.py`** — the driver: owns the matrix,
  plays the files through HA, receives the recordings and files them.

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

What it *does* reuse from collect mode, deliberately:

- **the frame tap**, at the same point in `wake_word_listener` — the audio is
  byte-for-byte what the wake model scores, which is the whole reason these
  recordings are worth training on;
- **the assistant suspension**, at `_run_voice_locked`, where wake word, dot
  button and HA's own `start_conversation` all meet;
- **the magenta throb ring**, because both modes mean "this device is
  recording and will not answer you", and a second colour for the same fact
  would have to be learned twice.

Trimming is **not** done on the controller. The driver has ffmpeg and the
source clip to compare against; the controller has neither, and a trim rule
baked in at that end is one the driver cannot change without a redeploy.

---

## The controller side

### API

```
POST   /api/devices/{id}/capture               {"enabled": true,
                                                "webhook": "http://host:port/clip",
                                                "idle_s": 300}
POST   /api/devices/{id}/capture               {"enabled": false}
POST   /api/devices/{id}/capture/window        {"tag": "...", "max_ms": 12000}
                                            -> {"session": "...", "tag": ..., "max_ms": ...}
POST   /api/devices/{id}/capture/window/stop   {"session": "..."}
GET    /api/devices/{id}/capture
```

Admin-only, and unlike collect mode it **requires a connected device**.
Arming an offline device is meaningful there (the mode is persisted and
re-applied on connect) and meaningless here.

### The mode is not persisted, and that is the opposite call to `collect_mode`

A collect flag survives a restart because the person walking the house saying
the wake word should not lose their session to a `docker compose up`. A
webhook is the address of a **running process**. A controller that came back
up still suspended, POSTing at a socket nobody holds, would be a device
answering nothing for a reason nothing on screen explains. Capture state
lives on the `Device` object only — hence no schema change, and nothing to
add to the support-bundle allowlist.

### Three things that stop it stranding a device

- **`idle_s` is a dead-man.** No window opened and no API call within it and
  the mode clears itself, the ring goes out, voice turns resume, and a device
  log line says why. It never expires underneath an open window — a caller
  may legitimately open a 30s window and say nothing for its duration.
- **A window closes two ways**: on audio time at `max_ms`, noticed by the
  frame path, and on the **wall clock** at `max_ms + WINDOW_GRACE_MS`,
  noticed by the watchdog. The second is not redundant: a device that stops
  sending frames stops advancing audio time, so a stalled link would
  otherwise hold the window open forever. Same reasoning as `em_endpoint`
  checking `maxSpeechMs` outside the frame path.
- **The disarm never cancels the task it is running on.** The watchdog calls
  `set_capture_mode(False)` itself, so a blind `task.cancel()` would raise
  `CancelledError` at the next await and abandon the rest of the disarm —
  webhook and queue left set, no log line, no state push. `tests/test_capture.py`
  pins it.

### Delivery

`POST <webhook>`, body = the WAV, metadata in headers (`X-EM-Tag`,
`X-EM-Ms`, `X-EM-Peak-Db`, `X-EM-Floor-Db`, `X-EM-Truncated`, `X-EM-Dropped`,
`X-EM-Frames`, `X-EM-Session`, `X-EM-Device`, `X-EM-Opened-Ms`). Headers
rather than multipart so both ends stay dependency-free: the receiver writes
the body straight to disk and reads the rest off the headers.

- **The mic path never waits on HTTP.** The frame tap appends bytes and
  returns; the POST runs on a per-device sender task off a bounded queue
  (`CAPTURE_QUEUE_MAX`). A slow webhook degrades to dropped *windows* —
  counted, logged, and reported to the next delivery as `X-EM-Dropped` —
  never to a stuttering microphone. Same rule as `shadow.Scorer.Push`.
- **Redirects are not followed** and the scheme is checked before the mode is
  armed (`em_capture.valid_webhook`). The caller is an authenticated admin,
  so this is not authorisation; it is the cheap half of not building an SSRF
  gadget out of a controller sitting on a home LAN.
- **An empty window is a failure, not a delivery.** A zero-length recording
  read as success is how a caller ends up with a corpus of silence it
  believes in.
- `tag` is opaque and echoed verbatim. Correlation is the caller's business,
  and a tag format the controller understood would be a second copy to drift.

### Precedence over collect mode

Where both are on, capture wins: it is ephemeral and was armed by something
actively driving the device right now, while collect mode is a persisted
standing order. Segmenting the same frames into a second set of clips
underneath a matrix run would fill the samples directory with fragments of a
corpus.

---

## The driver: `oww_forge/tools/playback_matrix.py`

Standard library plus `ffmpeg`/`ffprobe`. Reuses `collect_device_clips.py`'s
`Api` shape and its cleanup discipline.

```
playback_matrix.py -d Office \
  --controller http://192.168.3.211:8768 --user admin \
  --ha-url http://192.168.3.211:8123 --ha-token $HA_TOKEN \
  --src /media/nfsShare/common_voice/cv-corpus-26.0-2026-06-12/en/clips \
  --player media_player.lounge:20,35,60 \
  --player media_player.office:35,60
```

Volumes are accepted as percents (`35`) or fractions (`0.35`); anything ≤ 1
reads as a fraction, so `100` means full and `1` means one percent — the
reading that cannot silently blast a room.

### One server, two routes

- `GET /media/<token>` — serves the source clip currently armed. HA needs a
  URL and this process already has the bytes; a separate file server would be
  another thing to configure and keep alive. The token changes per cell and
  anything else 404s, so the route is not a way to read arbitrary paths off
  the host. Single-range `Range` requests are honoured, because several
  players send one.
- `POST /clip` — the controller's webhook.

`--advertise` defaults to whichever local address routes to HA (a host with
docker bridges and VPNs has no single obvious answer otherwise);
`--media-base` overrides the URL handed to the players.

### Loop order: files outer, matrix inner

This costs a `volume_set` per cell instead of one per pass, and it buys the
property that matters when the run has no end. The script is meant to be
killed with Ctrl+C whenever enough has been collected; with the loops the
other way round that would leave every recording at one player and one
volume. Each source clip is finished across the whole matrix before the next
is started, so whenever it stops the set is balanced.

### Per cell

```
volume_set -> settle -> open window -> play_media -> sleep(pre + duration + post)
           -> stop window -> await the POST for this tag -> trim -> write
```

- **Playback end is judged by the source duration, not by polling
  `media_player` state.** Music Assistant flow players lag and misreport
  their transitions; the duration is known exactly from `ffprobe`.
- **`max_ms` is a backstop, not the mechanism.** The script stops the window
  itself once the clip has played. Leaving the cap to do it would mark every
  recording `truncated` and throw away the signal that a playback *overran*.
- **A delivery for any other tag is discarded**, not returned. A late arrival
  from the previous cell is the one thing that would pair a recording with
  the wrong source file — and from there every file in the run is mislabelled
  by one.
- **Capture mode is re-checked every cell** (`ensure_armed`). The mode lives on
  the device's connection and is not persisted, so a device that bounced
  mid-run comes back with it off; noticing costs one GET and turns a silent
  run of failures into a hiccup.

### Output, resume and naming

Recordings go to `clips_recorded/` beside the source directory by default
(`--out` overrides). Names:

```
{stem}_recorded_{player_slug}_{pct}.wav
common_voice_en_100000_recorded_lounge_35.wav
```

`player_slug` is the entity id minus `media_player.`, non-alnum → `-`. The
volume is an integer percent so the name carries exactly one dot. One
formatter and one regex, used by both the writer and the resume check — two
copies of a filename convention is how a resumed run silently re-records
everything.

**Resume is one `stat` per candidate, never a directory listing.** The Common
Voice clips directory holds ~1.9M entries; it is streamed with `os.scandir`
and never globbed, listed or sorted. Files matching the output pattern are
skipped as sources, so pointing `--out` at the source directory still works.

Two sidecars in the output directory: `manifest.tsv` (one row per recording —
source, player, volume, durations, peak/floor dB, truncated) and
`failures.tsv`.

### Failure handling over a long unattended run

- **No delivery inside the grace window** → the cell is recorded as failed
  and the run continues.
- **Peak less than `SILENT_MARGIN_DB` above the window's own floor** → the
  capture was silence. `--max-silent` consecutive silent captures aborts the
  run, rather than discovering at 3am that six hours went to a muted entity.
- **Trimmed to nothing** → discarded rather than written, so the corpus never
  gains a file of room tone.
- **Exit — normal, exception or Ctrl+C** → players stopped, volumes restored,
  capture mode cleared, with signals ignored while it happens. If the disarm
  still fails it prints the exact `curl` to undo by hand. The device's own
  `idle_s` dead-man is the backstop behind that.

The first Ctrl+C asks the loop to finish the cell it is on; the cleanup is
what matters and it always runs.
