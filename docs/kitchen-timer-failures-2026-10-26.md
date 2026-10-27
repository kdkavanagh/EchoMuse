# Kitchen timer failures, 2026-10-26: remaining work

On the evening of 2026-10-26 the Kitchen Dot (`G090LF0965260F1J`) failed to
cancel a timer (turn 925), failed once to set one (927), and could not stop
two ringing timers. The first one rang until the Dot was unplugged. The
investigation covered turns 925–939, near misses 470–476, controller and Dot
logs, Home Assistant pipeline debug runs, Netdata CPU data, and replays of the
saved audio through the deployed models.

This document keeps only what is still open. All times are local (CDT).
Sample numbers are capture samples at 16 kHz.

## Open issues

| Issue | Turns it broke | Planned fix |
|---|---|---|
| The wake verifier cannot answer within its 700 ms deadline, even when nothing else is running. | 929, 931, 934 | Decide on the verdict when it lands (capped), or make the verifier's decode cheaper. A priority thread would not help. |
| A slow pause re-decode aborts the turn, because it runs inside the VAD job's 750 ms deadline. | 927, 936 | Take the pause re-decode out of the VAD job's deadline. |
| An idle Dot in another room takes an utterance meant to stop a ringing Dot. | 933, 935 | Let a ringing Dot claim the utterance, and have idle Dots defer to it. |
| The controller's event loop thread saturates one CPU core when two Dots listen at once, and every deadline slips. A ringing alarm by itself costs nothing. | 927, 929, 931, 934, 936 | Find which work on the event loop costs 3–4 CPU-seconds per turn, and move it off that thread or make it cheaper. |
| A wake turn has no wall-clock limit. | 927 | Add one, like reply turns have. |
| The Dot and Kroko heard badly through the Kitchen alarm. | near misses 470 and 472, 932, 938 | No fix yet. The Living Room heard fine through its ring in the 2026-10-27 test. Collect clips with the new retention and study them. |

## 1. The verification deadline

### The rule

A wake heard while the Dot is producing sound (a ringing alert, music, a
reply) is accepted only if a fresh Kroko decode hears the wake word, and the
answer has to come within 700 ms of the controller receiving the candidate
(`WAKE_VERIFY_S`, architecture §16.6). The decode covers support start
− 300 ms through open + 480 ms. If the deadline passes, the candidate is
refused as `verifier_timeout`, and a late answer cannot accept it.

### What happened

| Time | Row | Dot score (threshold 0.65) | Result | What the verifier heard when replayed |
|---|---|---|---|---|
| 20:57:23.2 | 929 | 0.78 | `verifier_timeout` at 714 ms | "Apophilia": passes, 255 ms decode |
| 20:57:43.0 | 931 | 0.71 | `verifier_timeout` at 838 ms | "Ophilia.": passes, 298 ms decode |
| 20:57:48.8 | 934 | 0.78 | `verifier_timeout` at 713 ms | "Ophelia": passes (distance 0.0), 285 ms decode |

All three refused wakes contain a wake word that the verifier recognizes when
it gets enough time.

### Why the budget does not add up

The verifier cannot start until 480 ms of real-time audio after the opening
hop has arrived. That leaves about 220 ms for the network, the wait for an
executor thread, and the decode. The decode is flushed with 1.5 s of zeros,
and Kroko decodes in 1.28 s chunks, so its cost rises in steps. Measured on the
deployed bundle on an idle core:

| Verifier input (plus the 1.5 s flush) | Decode time |
|---|---|
| 1.0 s | ~100 ms |
| 1.5 s | ~190 ms |
| 2.0 s | ~190 ms |
| 2.5 s | 190–290 ms |

The architecture document budgeted about 70 ms for the decode, using a
real-time factor of 0.032 that left out the flush. With a typical input of
1.5–2.5 s, the decode alone uses most or all of the 220 ms. Any wait behind
other speech work, or any CPU contention, pushes it past the deadline.

### The test on 2026-10-27 confirms it

In a test on the Living Room Dot, three wakes were heard while a timer rang
(956, 958, 960). Only that Dot was active, the controller was otherwise idle,
and every verification passed. Their verification records show where the
time goes:

| Row | Input complete | Wait for a thread | Decode | Verdict | Margin to 700 ms |
|---|---|---|---|---|---|
| 956 | 416 ms | 0 ms | 286 ms | 703 ms | −3 ms |
| 958 | 411 ms | 1 ms | 269 ms | 682 ms | 18 ms |
| 960 | 383 ms | 1 ms | 222 ms | 605 ms | 95 ms |

- **The input alone takes 383–416 ms**, because the verifier must wait for
  480 ms of audio past the opening hop.
- **The decode takes 222–286 ms.**
- **Nothing queued**: each decode got a thread within 1 ms.

So on an idle controller the verdict already lands at the deadline. 956's
verdict came 3 ms late and still counted, only because the actor's periodic
tick, which enforces the deadline, had not run yet. Any load at all turns
these into `verifier_timeout`.

Before that evening this was rare: there were 4 `verifier_timeout` rows ever,
3 of them on that evening, against 61 verified wakes. That evening two Dots
were busy at once while the ring was being scored, and the controller process
was at full CPU (section 4).

### What we now record

Commit `d6815e3` keeps a verification record in the decision trace of every
row for a wake heard while playing, accepted or refused:

- what the verifier decoded, its distance from the wake word and its verdict,
  and whether the decoded text clears the match (`text_passes`), so a timeout
  shows what the verifier would have decided;
- when the mic covered the verifier's input (`input_ready_ms`), when the
  verdict arrived (`answered_ms`), and when the candidate was decided
  (`decided_ms`), all counted from when the controller received the
  candidate, against `deadline_ms`;
- how long the decode waited for a thread (`queued_ms`) and how long it ran
  (`decode_ms`);
- how much audio past the opening hop had arrived when the candidate was
  decided (`mic_past_open_ms`).

If the verifier has not answered when a candidate is refused, the row waits up
to 1.5 s and keeps the late verdict marked `late`. With Save wake clips on, a
refused wake keeps a clip of the verifier's whole input. The Activity turn
detail shows all of this in a Verification stage.

To pull the records:

```sql
SELECT id, device_id, terminal_reason,
       json_extract(decision_trace, '$.verification.text')           AS heard,
       json_extract(decision_trace, '$.verification.text_passes')    AS would_pass,
       json_extract(decision_trace, '$.verification.input_ready_ms') AS input_ready,
       json_extract(decision_trace, '$.verification.queued_ms')      AS queued,
       json_extract(decision_trace, '$.verification.decode_ms')      AS decode,
       json_extract(decision_trace, '$.verification.answered_ms')    AS answered,
       json_extract(decision_trace, '$.verification.decided_ms')     AS decided,
       json_extract(decision_trace, '$.verification.late')           AS late
FROM turns
WHERE json_extract(decision_trace, '$.verification') IS NOT NULL
ORDER BY id DESC;
```

### Next step

A separate executor thread with priority would not help, because the decode
already starts within 1 ms. The time is in the 480 ms of lookahead and the
decode itself. So choose one or both:

- Decide on the verdict whenever it lands, with a cap of 1.5–2 s, and hold the
  provisional duck while waiting.
- Make the decode cheaper, for example with a shorter flush than 1.5 s of
  zeros, after checking that the wake word is still recognized.

Keep collecting the verification records to see the spread under load.

## 2. A slow pause re-decode aborts the turn

At each pause, the speech worker re-decodes the whole utterance so far with a
fresh Kroko decode. That decode runs inside the mic lane's VAD job, and the
VAD job has a 750 ms deadline. When the decode runs long, the job misses its
deadline, the worker reports `speech worker vad failed: job deadline
exceeded`, and the session aborts the turn as `interrupted`.

- **927** (20:54:01.575): the turn had been listening for about 19 s, because
  of the dropped commit. On an idle core, a re-decode of 19 s of audio takes
  1,049 ms. The turn was aborted.
- **936** (20:57:58.4): a shorter utterance, but the controller process was at
  100–132% CPU at the time. The turn was aborted.

The dropped commit is fixed, so turns should no longer stay open that long.
But a long utterance, or a short one under CPU contention, will still miss the
deadline.

**Fix:** run the pause re-decode outside the VAD job's deadline. A late pause
transcript should cost evidence for that pause, not end the turn.

## 3. Arbitration during a ring

While the Kitchen Dot rang, the Living Room Dot heard the user too:

- **933** (20:57:42.5, Living Room, idle): superseded.
- **935** (20:57:48.4, Living Room, idle): it won the utterance and sent
  "stop" to Home Assistant as `HassMediaPause` for the Living Room. Home
  Assistant paused nothing and said "Okay." The Kitchen's own candidate for
  the same words (934) was refused as `verifier_timeout` 0.4 s later.

`_accept_candidate` calls `arbiter.claim()` only when the candidate has no
context or a media context, so a candidate heard during an alert never claims
the utterance. It also cannot be accepted until the verifier answers, while an
idle Dot accepts at once. So the idle Dot claims first and nothing contests
it.

**Fix:** let a ringing Dot claim the utterance as soon as its candidate
arrives, and have an idle Dot defer to a ringing Dot's candidate inside the
arbitration window. A bare "stop" that an idle Dot hears while another Dot
rings should dismiss that ring, not go to Home Assistant.

## 4. Controller CPU

### How the controller is built

The controller is one Python process. One thread runs the asyncio event loop,
and that thread does all the coordination for every Dot: it reads every audio
frame off every device link, writes it into the lease timelines, runs each
Dot's session actor and endpoint, talks to Home Assistant, writes the
database and serves the dashboard. Only one thread can run Python code at a
time (the GIL), so all of that shares one core.

The model work goes to a separate pool of 2 threads (`ThreadPoolExecutor(2)`
in `em_speech_worker`): Silero VAD, Kroko streaming ASR, the Kroko re-decodes
at pauses and for wake verification, BCResNet scoring of what the speaker
played, and the echo comparison. Each model runs single-threaded. Jobs from
one lane (one lease's mic, its reference, or one verification) run in order.
Remote pause transcription has its own 2 threads, which only wait on the
network.

### What the measurements show

- **The models are not the bottleneck.** Kroko releases the GIL while it
  decodes: in the deployed controller, one 2 s decode took 132 ms, and four
  in parallel threads took 153 ms in total. Over 20 hours of normal use, each
  pool thread used about 5 s of CPU.
- **The event loop thread does almost all the work.** Over those same 20
  hours, it used 781 s of the process's 857 s of CPU (91%).
- **An ordinary turn already uses most of a core.** The controller idles at
  1%. Turns 951, 952 and 953 on 2026-10-27 (one Dot, nothing playing) ran it
  at 30–85% for the 4–5 s they listened, about 3–4 CPU-seconds each. So two
  Dots listening at once need more than one core. On 2026-10-26 the Kitchen
  alone, listening through the ring at 20:57:33–38 (930), ran it at 54–106%.
  At 20:57:42–58, when the Kitchen and the Living Room both heard the user, it
  ran at 33–133%, mostly above 55%. It was also at 100–105% at 20:53:47–49
  during 927.
- **When the event loop thread is saturated, the process stops at one core.**
  On 2026-10-27 at 18:35:41, pushing new firmware to five Dots pinned the event
  loop thread at 100% for 27 s while every other thread sat idle, on a
  16-thread host. The loop could not send control messages, so every Dot
  dropped its session after 3 s without one (`session_lost`). There is no
  per-thread history for the evening of 2026-10-26, so it is an inference
  that the same thread was the one saturated then, but the ceiling looked the
  same: about one core, with idle cores beside it.

The `wyoming-faster-whisper` container spiked to 2–3.6 cores during pause
transcriptions, but it is a separate process on a host with idle cores. The
host's load average of about 24 comes from 22 amdgpu kernel workers stuck in D
state; it is not CPU load.

### How a saturated loop turns into failures

When the event loop thread is busy, everything it does happens late:

- It reads device frames late, so evidence reaches the utterance in batches.
  That batch is what made one `advance()` return `[Pending, Commit]` in 927.
- It hands the next speech job to the pool late. Job deadlines count from
  when the job was created, so that wait counts against the VAD job's 750 ms
  (issue 2).
- It handles the verifier's verdict late. The 700 ms verification deadline
  counts from when the candidate arrived, so that wait counts against it
  (issue 1).
- It sends control messages late. Past 3 s, every Dot drops its session.

### Measuring it safely

Do not profile the live controller with py-spy in its default blocking mode.
At 50 samples a second it stopped the controller's threads often enough that
a Living Room turn on 2026-10-27 (954, 18:29:50) failed its VAD deadline after
1 s, and the next one (18:31:44) stayed in the listening state with the mic
open until the controller was restarted. That build still had the dropped
commit, and the restart did not save that turn, so it is likely, not proven,
that the dropped commit is what hung it. py-spy's `--nonblocking` mode only
returned errors in this container.

Reading each thread's CPU time from `/proc/<pid>/task/*/stat` once a second
never pauses the process, and it separates the event loop thread (the
process's main thread) from the pool threads.

### The test on 2026-10-27

With that sampler running, a 30-minute timer and then a 10 s timer rang on
the Living Room Dot, and the user woke it during both rings. Only that Dot was
active.

| Time | What happened | Whole process | Event loop thread |
|---|---|---|---|
| 18:39:31–42 | The timer rang, nobody spoke | 1–2% | 1–2% |
| 18:39:43–50 | Wake 956 during the ring ("Ophelia", then a "say that again" prompt) | 13–63% | 13–28% |
| 18:39:50–53 | Reply turn 957 listening (no wake, no verification) | 24–110% | 16–98% |
| 18:40:00–02 | Wake 958 during the ring, "stop" | 31–83% | 16–49% |
| 18:40:42–46 | Idle turn 959, "set a ten second timer" | 29–98% | 20–62% |
| 18:40:55–06 | The 10 s timer rang, nobody spoke | 1–2% | 1–2% |
| 18:41:07–10 | Wake 960 during the ring, "stop" | 30–70% | 13–28% |

What this shows:

- **A ringing alarm by itself costs nothing.** Both rings ran at 1–2%.
- **A wake during a ring costs about what any turn costs.** The verifier's
  decode adds about 0.25 s on a pool thread. The two pool threads reached
  30–36% each.
- **The heaviest moment had no wake and no verification at all.** While reply
  turn 957 listened, the event loop thread ran at 78–98% for 3 s.

The cost is in how the controller processes any listening turn on its event
loop thread, not in the ring or the verifier.

**Fix:** find out which work on the event loop thread costs 3–4 CPU-seconds
per turn, and move it off that thread or make it cheaper. To find it without
py-spy, add a sampler inside the controller that is switched on by an
environment variable: a background thread that records the event loop
thread's stack with `sys._current_frames()` many times a second. Candidates
to measure first: decoding and filing the audio frames, the session actor's
per-cell work, the TLS and websocket handling of the device links, and live
updates pushed to an open dashboard. Adding pool threads alone would not
help, because the loop is the thread that saturates.

## 5. Wake turns have no wall-clock limit

In 927 the utterance stopped taking evidence once its commit was dropped, so
neither `no_input` nor `too_long` could fire. Nothing else on a wake turn has a
wall-clock limit, so the turn stayed open with the mic live until the
unrelated VAD deadline in issue 2 ended it, about 12 s later. Reply turns
already have such a limit.

**Fix:** add a wall-clock limit to wake turns as a backstop, so any future
stall ends the turn instead of leaving the mic open.

## 6. Hearing through a ringing alarm (open finding)

During the ring the mic peaked at −24 dBFS RMS (volume 70). The native AEC
took 7–13 dB off on average (ERLE mean in the AFE evidence).

- On the Dot, two clear attempts scored 0.49 (near miss 470) and 0.33 (near
  miss 472), below the 0.65 playback threshold, so no candidate opened.
- Kroko, decoding the AEC output, mishears through the ring. Live, it heard
  "Ophelia, stop" as "Philia style" in 926. Replayed, the audio of 932
  decodes as "Austil you." (which is why it was refused) and near miss 472 as
  "Ophilia stock". The Wyoming pause transcription heard both successful
  stops (926 and 939) correctly.
- During the four-minute ring (21:04:13 to about 21:04:46), no Dot recorded a
  candidate or a near miss (the floor is 0.17) before 938 at 21:04:42. If the
  user tried earlier, the wake model did not register those attempts at all.
- 938 (21:04:42, score 0.80) was refused as `unverified_wake`. It kept no
  audio, because the Dot's session had no AFE metadata at the time.

In the 2026-10-27 test on the Living Room Dot, all three wakes during its two
rings opened candidates (scores 0.97–1.00) and passed verification, and Kroko
heard "Ophelia, stop" and "Ophilia stop". So the problem may be specific to
the Kitchen Dot or to how far the user stood from it.

No fix is planned yet. With Save wake clips and Save utterances on, refused
wakes now keep their clips and traces. Gather more of them during rings before
deciding whether the problem is the AEC, the wake model's playback threshold,
or Kroko.
