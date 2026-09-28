# Playback matrix — recording a corpus through the Echo

**Status:** built and deployed 2026-08-18. Not yet run against hardware. The
controller half has 40 tests; the driver script's pure parts were smoke-tested
locally. The first real run is the thing that has not happened.

Full technical reference: [`docs/playback-capture.md`](docs/playback-capture.md).
This file is the summary of the effort — what it is for, what was built, and
what to watch when it runs.

## The problem

Three sources of wake-word training data exist in this repo, and none of them
produces the fourth thing that is wanted:

| | what it gives | what it cannot give |
|---|---|---|
| `oww_forge` | synthetic TTS positives, thousands of them | a real microphone, a real room |
| `em_samples` (collect mode) | this speaker, this room, this array | volume, quantity, labels |
| `collect_device_clips.py` | verified wake phrases from a button press | anything at scale |
| **this** | 1.9M already-labelled clips, re-recorded through the device | positives — Common Voice has no wake word in it |

Common Voice on disk is clean studio speech. Played into the room and captured
off the Echo's own mic array, the same clips carry the array, the beamformer,
`micGainDb`, the room's reverb and noise floor, the distance, and whatever the
playing speaker does to the spectrum — every part of the path the model is
scored on in service, and none of which augmentation can invent from a clean
file. That makes it **negative** training material of exactly the right
character, plus room/channel material for augmenting positives.

The matrix is what turns one corpus into several. Each clip is played from
each media player, at each volume for that player. Position gives direction,
distance and reverb; volume gives SNR.

## What was built

**`controller/em_capture.py`** (new) — a recording *window*: the caller opens
it, plays something, closes it, and gets exactly one WAV POSTed back to a
webhook. Pure logic, no I/O.

**`controller/em_controller.py`** — the mode itself. Frame tap beside the
collect-mode tap in `wake_word_listener`, so the recorded audio is
byte-for-byte what the wake model scores. Suspends voice turns, lights the
magenta ring, runs a delivery task and a watchdog.

**`controller/em_api.py`** — four admin-only routes: arm/disarm the mode, open
a window, stop a window, read state.

**`oww_forge/tools/playback_matrix.py`** — the driver. Owns the matrix, serves
each source clip to Home Assistant over HTTP, plays it, receives the
recording, trims it and files it.

Plus: `CAPTURING` badge on the dashboard, `Dockerfile` COPY,
`controller/tests/test_capture.py` (40 tests), and the reference doc.

**No database migration.** Deliberately — see below.

## The decisions worth knowing

### It is a window, not the sample segmenter

The obvious implementation was "collect mode, but POST the clip instead of
saving it". That does not work, and the reason is worth stating because it
looks like it should.

`em_samples.Segmenter` cuts an unattended stream at the silences, because
nobody knows when the speech is coming. Here the driver knows exactly — it is
the thing playing it — and it has to pair each recording with the file that
produced it. Run a 5s Common Voice clip through the segmenter and:

- `silence_ms=400` splits the sentence at its own internal pauses into two or
  three clips;
- `max_clip_ms=6000` truncates anything longer, then refuses to reopen until
  the room is quiet again, discarding the remainder;
- a clip played at the bottom of the volume matrix may never clear
  `open_margin_db` and produce nothing at all.

One playback becomes 0..N files with no way to tell which. A window opened and
closed by the caller yields one, always.

### The mode is not persisted — the opposite call to collect mode

`collect_mode` is a database column, re-armed on connect, because someone
walking the house saying the wake word should not lose their session to a
`docker compose up`.

A webhook is the address of a *running process*. A controller that came back
up still suspended, POSTing at a socket nobody holds, would be a device
answering nothing for a reason nothing on screen explains. So capture state
lives on the connection only. That is why there is no schema change, nothing
to add to the support-bundle allowlist, and why the mode requires a
**connected** device where collect mode does not.

### Three ways it refuses to strand a device

Each of these was the obvious way to get it wrong:

1. **`idle_s` dead-man.** No window and no API call for 5 minutes and the mode
   clears itself, ring out, voice turns back. Never expires underneath an open
   window — a caller may legitimately open 30s and say nothing.
2. **A window closes on the wall clock as well as on audio time.** A device
   that stops sending frames stops advancing audio time, so the cap alone
   would hold a window open forever and the driver would wait for a delivery
   that never comes. Same reasoning as `em_endpoint` checking `maxSpeechMs`
   outside the frame path.
3. **The disarm never cancels the task it is running on.** The watchdog calls
   `set_capture_mode(False)` itself, so a blind `task.cancel()` raises
   `CancelledError` at the next await and abandons the rest of the disarm —
   webhook and queue left set, no log line, no state push. Found in review,
   pinned by test.

Behind all three: the driver's own cleanup runs on every exit path (normal,
exception, Ctrl+C, SIGTERM) with signals ignored while it happens, and prints
the exact `curl` to undo by hand if it still fails.

### The mic path never waits on HTTP

The frame tap appends bytes and returns. Delivery runs on a per-device task
off a bounded queue, so a slow or absent webhook degrades to dropped
*windows* — counted, logged, and reported to the next successful delivery as
`X-EM-Dropped` — never to a stuttering microphone. Same rule as
`shadow.Scorer.Push` on the device.

An **empty window is a failure, not a delivery.** A zero-length recording read
as success is how a caller ends up with a corpus of silence it believes in.

### Files outer, matrix inner

The run has no end — it is killed by hand when enough has been collected. With
the loops the other way round (one player/volume at a time, which is cheaper
in `volume_set` calls) that would leave every recording at a single setting.
So each source clip is finished across the whole matrix before the next is
started, and whenever it stops the set is balanced.

### Small things that would each have cost an evening

- **Playback end is judged by the source duration from `ffprobe`,** never by
  polling `media_player` state — Music Assistant flow players lag and
  misreport their transitions.
- **The window's `max_ms` is a backstop, not the mechanism.** The driver stops
  the window itself. Letting the cap do it would mark every recording
  `truncated` and throw away the signal that a playback actually *overran*.
- **A delivery for any other tag is discarded, not returned.** A late arrival
  from the previous cell would pair a recording with the wrong source file —
  and from there every file in the run is mislabelled by one.
- **Sources are streamed with `os.scandir`; resume is one `stat` per
  candidate.** The Common Voice clips directory holds 1,932,437 entries, so
  anything that lists, globs or sorts it costs more than the recording does.
- **Volume in the filename is an integer percent** (`_35`, not `_0.35`) so the
  name carries exactly one dot. One formatter and one regex, shared by the
  writer and the resume check — two copies of a filename convention is how a
  resumed run silently re-records everything.

## Running it

Recordings land in `clips_recorded/` beside the source directory, named
`common_voice_en_100000_recorded_office_35.wav`, with `manifest.tsv` (source,
player, volume, durations, peak/floor dB, truncated) and `failures.tsv`
alongside.

Ctrl+C whenever. It finishes the current recording, restores the players'
volumes, gives the Echo back. Re-run later and it skips whatever is already on
disk.

## What to watch on the first run

- **The first `✓` line.** It reports the kept duration and the peak/floor dB.
  A peak within 6dB of the floor is silence, and three of those in a row abort
  the run rather than burning hours on a muted player.
- **`TRUNCATED` on the `✓` line** means the playback outlasted the window —
  raise `--post-ms`.
- **`no recording arrived`** means the controller never delivered. Check the
  device is on the same network path as the machine running the script, and
  that `--advertise` picked the right address (it guesses from the route to
  HA).
- **Recordings that are too short after the trim** are discarded rather than
  written. If that happens a lot, `--silence-db` is too aggressive for the
  volume being played, or the player is too quiet at that matrix cell.

## What this does not do

- **No positives.** Common Voice contains no wake word. This is negative and
  channel material; positives still come from `oww_forge` and collect mode.
- **No parallelism.** The Echo hears the whole house, so two players at once
  is one recording of both.
- **No device config changes.** Unlike `collect_device_clips.py` this needs
  none — the always-on wake stream is already the signal it wants, so there is
  no config to save and restore.
