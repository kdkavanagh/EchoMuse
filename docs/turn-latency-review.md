# Turn latency review and improvement proposals (2026-09-29)

Scope: the Office Dot (`G090LF10728426PR`). This covers the "ophelia next" / "ophelia stop" episode from 18:40–18:42 (turns 470–474) and the ordinary spoken-command history, both before and after the post-AFE cutover.

**Status (2026-10-08, policy `post_afe_4`):**
- **Implemented:**
  - P1: STT container recreated with its swap limit.
  - P4 items 1–3; the start limit was then raised to 10 s (2026-09-30).
  - P7: `turns.decision_trace` and `first_audio_ms`; "speech end → first audio" as `response_latency_ms` (on the Dot's clock), per turn on Activity and as percentiles on Status (2026-10-09).
  - The P2 mechanism (finalize at the pause).
  - The 240 ms text-stability rule removed.
  - The 1.5 s ASR flush for wake verification and span re-decode.
  - `saveUtterances` now keeps 300 recordings per device.
  - P3 (`post_afe_4`), with two changes from the proposal below. A literal-template match is not enough for `complete`: the text plus a probe word (`zqxj`) is asked in the same request, and if that also matches, a wildcard can take more words and the class is `extendable`. This keeps "what's the weather … tomorrow" and "play …" from being cut after 608 ms. A throwaway query when a wake candidate opens pages HA's matcher back in after idle. On the 38 `unknown` turns of `post_afe_3`, HA answers 8 `complete` and 12 `extendable` (the proposal's 22/9 was without the probe and on 39 earlier turns).
- **Not implemented:** P4 item 4 (the upstream HA report), P5, P6.

## Summary

A typical locally-handled command ("what time is it", "set a timer", "stop", "next") takes a median **3.2 s** from the end of speech to the first reply audio. Where that time goes:

| Stage | Median | Share | Cause |
|---|---|---|---|
| Endpoint wait (`endpoint_ms`) | 2,048 ms | 65% | The grammar classes most requests `unknown`, so the controller waits the full 1,792 ms pause. Requests it classes `complete` are held back by Kroko's 1.28 s output cadence instead. |
| HA STT (`stt_ms`) | 294 ms | 9% | The cold first turn after idle takes 0.7–5.3 s because the STT process has been swapped out. |
| HA intent (`intent_ms`, local agent) | 13 ms | — | — |
| Reply URL → first audio (derived) | ~520 ms | 16% | ElevenLabs synthesis, the fetch, and the device output path. |

A like-for-like request, "what time is it?", took a median **3.8 s** end to end before the cutover (n = 23) and **6.0–6.7 s** after it (n = 2). Almost all of the difference is the endpoint wait.

The music episode was a different problem: a correctness failure. Getting "next" and then "stop" through took five turns and about 95 s (18:40:11 → 18:41:46).

Recommended order:

| # | Proposal | Expected effect |
|---|---|---|
| P1 | Keep HA's STT process out of swap (host config) | Removes the 0.7–5.3 s first-turn-after-idle STT tail (9 of 31 HA turns) |
| P2 | Finalize Kroko on pause, using a shadow decode | Endpoint for `complete` commands: 1.9 s → ~0.86 s |
| P3 | Use HA's own intent recognizer as the completeness oracle | Classes 31 of 39 real turns `complete`/`extendable` (13 today); with P2, endpoint p50 2,048 → 864 ms |
| P4 | Fix the streamed-reply race, the 120 s start budget, and "stop" swallowed as a dialog stop | Removes the 15.6 s hang in turn 471 and the swallowed "stop" in turn 472 |
| P5 | Play a local earcon instead of fetching HA's acknowledge sound | About 1.6–2.0 s off every local device or media command; the music unducks sooner |
| P6 | Make the wake chime earlier and shorter (A/B test first) | About 0.5 s off every wake turn; may reduce first-word recognition errors |
| P7 | Persist per-turn traces and the time to first audio | This review had to reconstruct both |

## Data

- `turns` table: 44 post-AFE turns (433–476, `post_afe_1`/`post_afe_2`, 2026-09-27 → 09-29) and 432 pre-cutover turns (2026-08-07 → 09-27). The pre-cutover timings are cumulative from the wake and are compared only where both eras recorded the same quantity.
- Decision traces: only turn 476 survives. The container was recreated at 19:14 and traces live only in `docker logs` (see P7). My figures for 470–474 come from the traces as read before the redeploy.
- Recordings: 466–476 (committed spans). I replayed them through the pinned Kroko model, Silero, and HA's STT engine.
- HA 2026.8.1: pipeline debug and the `conversation/agent/homeassistant/debug` websocket.
- Pipeline "STT" (preferred): STT `stt.faster_whisper`, which is actually the sherpa-onnx Parakeet 0.6B int8 model in the `wyoming-faster-whisper` container; conversation agent `conversation.claude_conversation` with `prefer_local_intents: true`; TTS ElevenLabs.

Timings are sample-time where the controller measures sample time (`endpoint_ms`) and wall time otherwise. "First audio" is derived: `total_ms` minus the measured stages. It is meaningful only for turns whose reply was fetched after `intent-end`.

### What people ask

These are the 276 non-empty transcripts from both eras. I excluded 64 background or TV transcripts ("Thank you for watching", the Dot's own weather reply) and categorized the remaining 212 by keyword:

| Category | n | Share | Handled by (post-AFE) |
|---|---|---|---|
| Weather | 54 | 25% | HA sentence trigger (automation) |
| Kitchen conversions / general questions | 43 | 20% | `UnitConvert` custom sentences; LLM otherwise |
| Time / date | 30 | 14% | HA sentence trigger |
| Bare stop / cancel / never mind | 24 | 11% | EchoMuse local command, or HA `HassMediaPause` |
| Timers | 22 | 10% | Custom `timers.yaml` / built-in intents |
| Sports scores | 21 | 10% | HA sentence trigger |
| Lights / devices | 11 | 5% | Built-in intents |
| Media | 6 | 3% | Custom `media_controls.yaml`, triggers |

In the post-AFE era, HA's local agent answered **21 of 31** HA turns (`intent_local = 1`). The LLM answered the other 10: a median 4.2 s of intent time, often ending "Can you repeat that?".

## Findings

### F1. Every turn waits about 2 s after speech, whatever the grammar class

| `endpoint_class` | n | `endpoint_ms` p50 | Range | Design value (pause + 192 ms lookahead) |
|---|---|---|---|---|
| `unknown` | 25 | 2,064 | 2,032–2,208 | 1,984 |
| `complete` | 13 | 1,904 | 1,424–2,128 | 800 |
| `needs_more` | 1 | 2,032 | — | 1,984 |

- `docs/post-afe-audio-architecture.md` §16 sets a release gate: *complete-command end-to-commit p95 ≤ 1.2 s in quiet*. The fielded p50 is 1.9 s.
- **`unknown` dominates** (25 of 39 turns), because `echomuse_grammar` only has families for timers, alarms, local commands, home targets, and reply choices. Weather, time, sports, media, and conversions are all `unknown` and wait 1,792 ms. Separately, the classifier returns:
  - `next`, `skip`, `previous`, `play`, `stop the music` → `unknown`
  - `pause`, `resume` → `needs_more`, because they prefix "pause the timer"
- **`complete` is held back by Kroko.** The chunked streaming model (`decode_chunk_len=128`) emits text only at 1.44 s and then every 1.28 s of stream time. Route A needs the text to be stable for 240 ms (`STABILITY_SPAN`) and needs 10 trailing blank frames (`MIN_TRAILING_BLANKS`), and both are updated only on that same cadence. A short command's last word usually lands in the update after speech ends.
  - Turn 474 "stop": speech ended at 2.95 s. Kroko showed "Ophilia st" at 2.72 s and "Ophilia stop" at 4.00 s. The text was stable at 4.24 s and the endpoint went pending at 4.26 s, where the 608 ms rule alone would have allowed 3.56 s.
  - Turn 472 lost 1.3 s the same way.
- **Before the cutover**, HA's VAD ended turns after about 0.7–1.0 s of silence while STT streamed alongside. The STT result arrived a median 132 ms after the endpoint when warm.

### F2. HA STT is cold after idle because its model is in swap

| Idle before the turn | Pre-AFE STT after endpoint, p50 / p90 (n) | Post-AFE `stt_ms` p50 / max (n) |
|---|---|---|
| < 2 min | 132 / 440 ms (98) | 234 / 463 ms (17) |
| 2–10 min | 173 / 567 ms (33) | 290 / 2,133 ms (7) |
| 10–30 min | 200 / 2,362 ms (22) | 1,168 / 3,223 ms (3) |
| > 30 min | 1,327 / 4,689 ms (39) | 2,842 / 5,312 ms (5) |

- The STT process (`wyoming_faster_whisper` running Parakeet) has **407 MB in swap** out of a 684 MB RSS (`VmSwap`), and its cgroup reads `memory.swap.max = max`.
  - `docker-compose.yml` sets `memswap_limit: 3g` = `mem_limit`, which should forbid swap. The container predates that setting (created 2026-08-09).
  - `echomuse-controller`, recreated today with the same pattern, correctly reads `memory.swap.max = 0`.
- The host is under memory pressure: 14 GiB RAM, 10 GiB of swap in use, `vm.swappiness = 60`.
- The problem predates the cutover (pre-AFE p90 was 4.7 s after long idle).

### F3. The wake chime starts about 0.5 s after the trigger, and people wait for it

- The chime (`device/internal/cue/wake_word_triggered.pcm`) is **0.95 s** long.
- It is rendered from `_accept_candidate`, which runs only after the controller's wake verification. That verification needs audio through candidate open + 480 ms (`VERIFICATION_LOOKAHEAD`).
- Cross-correlating the chime against the three quiet recordings finds its AEC residual at trigger + 0.51, 0.52 and 0.54 s (turns 475, 476, 468; normalized peak 0.15–0.25 against a 0.07–0.09 p99 background).
- In all three turns the command starts 1.0–1.1 s after the trigger, right after the chime's loud part.
- In 476's trace, the cells at trigger + 0.576 → 0.80 s (the chime residual) are labeled `command_speech` rather than `self_output`.
- The Dot itself fires 0.2–0.55 s after the wake word ends (BCResNet smoothing), so the user hears the chime 0.75–1.05 s after finishing "Ophelia".

### F4. The first word after the wake word is often lost, and the LLM fallback is expensive

Five of 31 post-AFE HA turns had the first command word mangled or dropped:

| Turn | What HA heard | Consequence |
|---|---|---|
| 439 | "I'm feeling off the home seater" | LLM 3.2 s → "Can you repeat that?" |
| 445 | "st" | LLM 2.1 s → "Can you repeat that?" |
| 466 | "like Nothing Else Matters…" | LLM 6.1 s → **played the wrong song** |
| 468 | "plain night" | LLM 8.9 s → "Can you repeat that?" plus a reply turn |
| 473 | "Popphelia start." | LLM 3.0 s → "Can you repeat that?" plus a 7 s reply window |

- In 468 the damaged word ("play", about 3.0 s into the clip) falls inside the chime's window (2.56–3.50 s).
- Trimming the wake word off the STT audio does **not** help. Replaying through the same Parakeet container, trimmed at 1.9 s / 2.02 s:
  - 468 → "Clean" / "Clean"
  - 473 → "" / "Stop."
  - 471 → "Challenge." / ""
  - 475 → "What time is it?" / ""

  This backs the spec's "no audio is ever trimmed" rule. The damage is in the audio, not in how the wake word is stripped.
- The cause is unconfirmed. Chime overlap and music under the command are the candidates; P6 tests the chime.

### F5. The music episode (turns 470–474)

| Turn | Said | What happened | Turn length |
|---|---|---|---|
| 470 | next | Endpoint 2.10 s (`unknown`) → `HassMediaNext` → HA played `acknowledge.mp3` (1.59 s) with music ducked | 6.3 s |
| 471 | next | Action ran. The reply fetch, started at `run-start`, parked on HA's stream before the acknowledge override was set (an HA race, reproduced with HA's own `ResultStream`). The streamed path allows 120 s to start (`RESPONSE_TOTAL_S`), so the turn stayed open until the next wake superseded it | 19.0 s |
| 472 | stop | 471's unfinished playback made `_command_context` return `DIALOG`, so "stop" became a local dialog stop of nothing audible. **The music kept playing** | 2.5 s |
| 473 | stop | Kroko heard "A failure", HA heard "Popphelia start." → LLM 3.0 s → "Can you repeat that?" → 7 s reply window, `reply_timeout` | 8.5 s |
| 474 | stop | Endpoint 1.55 s (Kroko-bound `complete`) → `HassMediaPause` → acknowledge sound 1.59 s + drain | 4.9 s |

Stopping the music took three attempts and about 60 s (18:40:45 → 18:41:46).

### F6. Traces are not persisted

The traces for 470–475 (segments, endpoint events) disappeared with the 19:14 redeploy. The `turns` table also has no commit → first-audio time; I derived it here as a residual. The residual is unusable for streamed replies, where it goes negative.

## Proposals

### P1. Keep the STT model resident (host config in `~/git/hass`)

- **Change:**
  1. Run `docker compose up -d --force-recreate wyoming-faster-whisper` so the existing `memswap_limit: 3g` applies.
  2. Confirm `cat /sys/fs/cgroup/<scope>/memory.swap.max` reads `0`.
- **Gain:** removes the cold-STT tail. p90 `stt_ms` goes from 2.8 s to about 0.3 s, affecting 9 of 31 post-AFE HA turns.
- **Tradeoffs:**
  - The model now counts against the 3 GiB cap with no swap. Today's RSS + swap is about 1.1 GB, so there is headroom.
  - Host memory pressure stays; other services swap more instead.
  - HA itself also has 257 MB swapped. I'm not proposing a limit for HA, because it runs without `mem_limit`.
- **Rejected alternative:** a controller-side STT pre-warm at wake. It costs one pipeline run per wake and hides a host problem.
- **Verify:** the first turn after ≥ 30 min idle has `stt_ms` < 500.

### P2. Finalize Kroko on pause (shadow decode)

- **Change:** after about 300 ms of VAD pause following command speech (before route A's 608 ms minimum can expire), the speech worker decodes a fresh stream of `[utterance start … frontier] + 1.28 s zeros`. It pushes that text as the ASR observation at the frontier sample, and repeats on each new pause. The live stream is left untouched.
- **Measured** (controller host, 1 thread; pads shorter than one chunk drop the last word):

  | Clip | Pad | Decode | Text |
  |---|---|---|---|
  | 474, 3.1 s | 0.64 s | 75 ms | "Ophilia st" |
  | 474, 3.1 s | 1.28 s | 110 ms | "Ophilia stop" |
  | 476, 4.0 s | 1.28 s | 148 ms | "…What time is it?" |
  | 467, 5.6 s | 1.28 s | 185 ms | full text |

- **Gain:** `complete` endpoint 1.9 s → about 0.86 s (608 + 192 + one 64 ms cell), which meets the §16.6 gate. Local voice stop gets the same benefit.
- **Tradeoffs:**
  - Up to about 190 ms of worker CPU per pause.
  - The shadow's trailing blanks are synthetic. The §16.6 rule that "decoder trailing blanks agree" must be relaxed to VAD quiet cells for shadow-finalized text: a spec and `policy_hash` change.
  - Determinism is kept: the shadow result depends only on audio up to the frontier, so replay reproduces it.
- **Alternative:** a Kroko export with smaller chunks. I haven't found or verified one. It would cost accuracy and CPU, and needs a new pinned bundle.
- **Verify:** replay the endpoint corpus; `complete` end-to-commit p95 ≤ 1.2 s; the clipped-command rate stays under the < 1% gate.

### P3. Use HA's own recognizer as the completeness oracle

- **Evidence:** `conversation/agent/homeassistant/debug` (stock websocket; recognizes without executing) against the user's real configuration:

  | Text | Result |
  |---|---|
  | "what time is it" | match, trigger `what time is it` |
  | "what time is" | no match |
  | "how many cups are in a quart" | match, `UnitConvert` |
  | "how many cups are in a" | unmatched slot `a` |
  | "set a timer for five minutes" | match |
  | "set a timer for five" | unmatched slot |
  | "turn off the office lights" | match |
  | "play nothing" | match, trigger `(play\|played\|plays) {query}` (wildcard) |
  | "next" | `HassMediaNext`, `match: false` without `device_id` (needs area context) |

  Latency was 0.3–48 ms, with one cold 129 ms.
- **Change:** when the stable prefix changes, query the oracle with the satellite's HA `device_id` and classify from `sentence_template`:
  - match with a literal template → `complete`
  - match with a template ending in a wildcard → `extendable`
  - unmatched slots → `needs_more`
  - no match → the existing `echomuse_grammar` result

  Record each answer as an observation stamped at the frontier sample, so replay stays deterministic. Until an answer arrives, and whenever HA is unreachable, the local grammar applies.
- **Gain on the 39 post-AFE turns:** 22 `complete`, 9 `extendable`, 8 `unknown`. Combined with P2, endpoint p50 drops from 2,048 to 864 ms, saving 29 s in total over 39 turns. P3 needs P2: without it, `complete` stays Kroko-bound at about 1.9 s.
- **Tradeoffs:**
  - Completeness now depends on the live HA configuration, so the checked-in conformance corpus can no longer pin it.
  - A literal template with optional words (weather's `[like]`) classifies `complete`, so "what's the weather … tomorrow" with a pause over 608 ms gets clipped. The clipped-command gate measures this.
  - Adds one websocket call per stable-prefix change.
  - Requires a §16.6 change, because an async oracle feeds the reducer.

### P4. Fix the streamed-reply race and the dialog-stop context

These are the correctness fixes from the episode:

1. **Request the reply audio later.** Start playback on `intent-progress` `tts_start_streaming`, else on `tts-end`, instead of `run-start`. This applies to both paths: `em_ha_client.py:608-611` and `em_esphome.py:294-297`. It is HA's own ESPHome-satellite order, and no audio is lost because HA produces none before those events.
2. **Arm the 5 s start limit (`RESPONSE_START_S`) at `intent-end` on the streamed path** as well, instead of `RESPONSE_TOTAL_S` (`em_session.py:1880`). Slow cloud TTS then ends as `response_timeout`, as it already does on the non-streamed path.
3. **Enter the `DIALOG` command context only once the reply is audible** (`_command_context`, `em_session.py:1283-1291`), so "stop" during a pending, silent reply reaches HA and pauses the music. Tradeoff: a "stop" meant to cancel a not-yet-audible reply goes to HA instead. With item 1 in place, that window is about 0.5 s.
4. Report the HA bug: `ResultStream.async_override_result` does not wake a GET that is already waiting.

- **Verify:** regression tests for an early GET against an acknowledge override, and for "stop" during an inaudible reply.

### P5. Local acknowledgement instead of HA's `acknowledge.mp3`

- **Change:** read `acknowledge_override` from `tts-start` (HA 2026.8.1, `assist_pipeline/pipeline.py:1463`). When it is true, don't fetch HA's audio. Play a short device earcon, or nothing for media-control intents, where the music changing is the confirmation. Then release dialog focus straight away.
- **Gain:** removes the 1.59 s chime plus about 0.37 s of drain from every local device or media command (turns 470 and 474), and releases the music duck about 1.8 s sooner. Combined with P4 item 1, these turns never fetch HA's audio at all.
- **Tradeoffs:**
  - A user-configured HA acknowledge sound would be ignored.
  - Relies on an event field HA emits but doesn't document as stable. The fallback, if the field is missing, is today's behavior.

### P6. Earlier, shorter wake chime (measure first)

- **Change:** A/B test for a week with the chime off or shortened.
  - Compare speech onset after the trigger (today 1.0–1.1 s) and the first-word error rate (today 5 of 31).
  - If the chime is the cause, play a short (≤ 250 ms) chime on the Dot at local detection, and have the controller cancel it on rejection.
  - Separately, mark the earcon's AEC residual `self_output` (476 labeled it `command_speech`).
- **Gain:** about 0.5 s earlier start of speaking on every wake turn, and possibly fewer first-word errors and LLM fallbacks. Each of those costs 2–9 s, and one played the wrong song.
- **Tradeoffs:**
  - Rejected candidates would chime: 2 `unverified_wake` out of 44 post-AFE candidates.
  - Needs a protocol and firmware change for a device-initiated earcon.
  - The chime is the user's preference (`wakeSound: true`), so the change must stay optional.
- **Status (2026-09-30):** the earlier half is implemented as firmware capability `local_wake_chime`: with `wakeSound` on, the Dot plays the existing chime at idle candidate open, and the controller skips its own. A rejected candidate keeps its chime rather than being cancelled (a cancel cannot retract the ~200 ms already queued in the render sink). Wakes heard during playback still chime after acceptance. The chime is not shortened.

### P7. Persist traces and the time to first audio

- **Change:** store the §11.3 decision trace JSON with the `turns` row (or in a rotating file under `data/`). Add these columns:
  - `first_audio_ms`: commit → audible
  - `ack_override`
  - `wake_to_accept_ms`

  Show "speech end → first audio" on the dashboard turn view.
- **Gain:** this review can be repeated from data. Today, a redeploy erases the evidence.
- **Tradeoffs:** about 5 KB of trace per turn, and a migration (appended to `em_db.MIGRATIONS`).

## Projected effect

For locally-handled commands, from speech end to first reply audio:

| Case | Today | With P1 + P2 + P3 | With P5 as well (media/device commands) |
|---|---|---|---|
| "what time is it" (476) | 2.68 s | ~1.50 s | — |
| "set a two minute timer" (453) | 2.60 s | ~1.72 s | — |
| "what's the score of the Bears game", after 66 min idle (461) | 5.75 s | ~2.47 s (`extendable`) | — |
| "next" during music (470): turn length | 6.3 s | ~5.1 s | ~3.2 s |
| Cold first turn of the day, "what's the weather" (464) | 6.68 s | ~3.1 s | — |

LLM-answered requests keep their 3–9 s of agent time. Those savings apply only to the endpoint and STT stages.

## Not proposed

- LLM latency (Claude, 1.2–8.9 s), the length of the weather automation's reply (8–16 s of speech), and ElevenLabs first-audio latency. These are HA-side configuration.
- Trimming the wake word from the STT audio. Tested in F4 and rejected.
- Lowering the `unknown` pause below 1,792 ms for free-form requests. It protects TV-room turns (§16.6, `post_afe_2`), and P3 takes most regular requests out of `unknown` anyway.
