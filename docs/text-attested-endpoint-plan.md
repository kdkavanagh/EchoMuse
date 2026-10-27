# Text-attested endpoint (route T): plan

**Status:** proposed 2026-10-26; nothing implemented.
**Evidence:** turns 911 and 912 (Master Bedroom, 2026-10-25) and the replay of every stored turn recording made for the FireRedVAD evaluation (2026-10-26), summarised in §1. The architecture it changes is [post-afe-audio-architecture.md](post-afe-audio-architecture.md), written "spec §N".
**Goal:** a wake or button command that Kroko transcribes but Silero does not mark as speech ends and goes to Home Assistant, instead of closing `no_input`.

**Not in scope:**
- Reply windows (`reply`, `ha_reply`). Without a wake word nothing anchors the speaker, and the replay cannot reproduce a reply's primed VAD state, so there is no evidence to tune on.
- Replacing Silero. FireRedVAD was evaluated separately and is not a drop-in.
- Moving the no-progress fallback's cut when route T's checks fail (the Dot playing music or speech, words much quieter than the wake word, replies). Those are the cases where Kroko's words are least trustworthy, so a cut that misses them and ends in `retry` is the safer outcome. When the checks pass, the fallback already uses route T's cut (§3).
- The wake chime's onset residue counted as command speech (912's two cells at +576–640 ms): an echo-tracker fix.
- Local commands (stop, snooze, pause, dismiss). They keep requiring speech-positive cells (`local_command_preconditions`).

## 1. Starting point

Every way an utterance can end needs `command_speech` cells after the trigger, and only Silero produces them (spec §8.2):

| Rule (`em_endpoint_policy.EndpointReducer`) | Needs |
|---|---|
| `no_input` at trigger + 5 s | no command cell has ended |
| Route A (`_route_a`) | ≥ `MIN_COMMAND_CELLS` (6) command cells |
| Route B (`_route_b`) | a command cell at or before the stable prefix (`_command_boundary_at`) |
| Route R (`_route_r`) | a command cell |
| No-progress fallback | at least one command cell |

Kroko's text cannot open, end or cut a command. Turn 911 shows the cost:

- "Ophelia" scored 0.5–0.77 and was accepted (idle wake, BCResNet 0.97).
- "Turn on the fan", Kroko tokens +1.99 to +2.79 s after the trigger, scored at most 0.03 at the +30 dB VAD input: no command cell.
- Kroko's stable text was "turn on the fan", and HA's sentence matcher judged it `complete`, by +3.36 s.
- The reducer closed `no_input` at +5.0 s.

The command was quiet (speech 7.6 dB above the room floor, 4 dB below the wake word) and weakly voiced. 913's clearly voiced repeat, moved to 911's level over 911's noise, scored 0.67 against the 0.70 that opens a run, while Kroko still transcribed it. Kroko failed only 6 dB lower.

Replaying the 209 stored wake and button turns through today's attributor (it reproduces the live traces of the 88 `post_afe_10` turns: 87 at ≥ 99% cell agreement) finds these with Kroko command text but fewer than 6 Silero command cells:

| Turn | Device | Text | Silero command cells | Complete command |
|---|---|---|---|---|
| 911 | Master Bedroom | turn on the fan | 0 | yes |
| 912 | Master Bedroom | turn on the fan | 2 (the chime's residue) | yes |
| 732 | Master Bathroom | open the curtains | 5 | yes |
| 818 | Master Bedroom | when is the michigan game | 5 | no (a question for the conversation agent) |
| 881 | Master Bathroom | closed the lines | 0 | no |
| 865 | Nursery | but also like that there | 0 | no (background talk after a false wake) |

## 2. Decisions

1. **A fifth endpoint route, `T`** (`Route.T = "T"`, "words heard after the wake word, the voice detector missed them"). It is evaluated after routes A and B, and only while the utterance has fewer than `MIN_COMMAND_CELLS` command cells. With 6 or more, routes A and B already apply and nothing changes: no turn that ends today by route A or B can end differently.

2. **Attested words.** The utterance's command words are *attested* when all of these hold:

   | Gate | Rule | Why |
   |---|---|---|
   | G1 kind | `wake` or `button` | replies are out of scope |
   | G2 after the wake word | the judged command text (after the wake cut, spec §8.4) is non-empty and its first word's Kroko emission sample follows the trigger | the wake word is never the command |
   | G3 complete | `completeness(prefix) == COMPLETE` (EchoMuse grammar, else HA's sentence matcher with the probe word) | only executable commands; `extendable`, `needs_more` and `unknown` do not qualify |
   | G4 nothing playing | the wake candidate was not `producing_sound` (a button turn: no content, dialog or alert output at the press), and no output other than earcons started during the utterance | music and speech can carry words; an earcon cannot, and 912's words overlap the chime's tail |
   | G5 loudness | with `F` known, the loudest cell `E` over [first word − 480 ms, last word] is at least `F − BACKGROUND_MARGIN_DB` (10 dB); unknown `F` passes | the same margin that makes a cell `background_speech` |

   Word samples are Kroko's emission samples (`StreamingTranscript.first_samples`/`last_samples`), also when the judged text came from the pause server: the server has no timings, and Kroko already places the wake phrase that way.

3. **Route T's end.** With attested words, fewer than 6 command cells, and no pending end:
   - `trailing_blank_frames ≥ MIN_TRAILING_BLANKS` (400 ms without a new token);
   - `frontier − last_word ≥ ROUTE_A_PAUSE[COMPLETE]` (608 ms);
   - at least `PAUSE_QUIET_CELLS` (15) of the last `PAUSE_WINDOW_CELLS` (19) cells are `non_speech` or `self_output`, so Silero hears nothing ongoing;

   then the end is pending at `boundary = last_word`, rounded up to the cell grid, `since = frontier`.

4. **Commit.** As for every route: `LOOKAHEAD` (192 ms) after `since`, `end = boundary + LOOKAHEAD` clipped to valid audio. Revoked if the stable prefix changes after `since`, or by `REVOCATION_CELLS` command cells (the existing rule). `redecode_required = True`: a fresh decode of the committed span (Kroko, or the pause server when the text was the server's) must pass `redecode_differs`, or the turn ends `retry` (spec §8.2, the guard that caught 912's short cut). After that the turn follows the normal path: HA STT on the STT copy, wake-phrase removal, local routes, transcript correction, intent and TTS.

   Kroko emits a word slightly after its audio: 911's last token came at +2.79 s against audio fading at about +2.5 s. The cut therefore holds the whole word. An emission that lands early clips the word, and the re-decode rejects it.

5. **`no_input` waits for attested words.** The deadline closes the utterance only when no command cell has ended *and* there are no attested words. With attested words, three things bound the turn:
   - route T;
   - the no-progress rule, extended to attested words: 3.0 s without text change, measured from the later of `progress_sample` and the first word, commits at route T's boundary while the text is complete, else closes `retry`;
   - `too_long`.

   What counts here is the open decision in §8.

6. **No local actions from route T.** `local_command_preconditions` needs speech-positive cells and stays as it is. A quiet "Ophelia, stop" during an alarm does not qualify anyway: the alarm fails G4.

7. **Policy revision `post_afe_11`** (`em_speech_worker.POLICY_REVISION`), so each turn records which rules judged it.

## 3. Reducer rules (spec §16.6 addition)

```text
heard    = kind in (wake, button)                                         # G1
           and command_words and first_word > trigger_sample             # G2
           and output_idle                                               # G4
           and (F unknown or loudest_E(first_word − 7,680, last_word) ≥ F − 10 dB)   # G5
attested = heard and completeness(stable_prefix) == complete              # G3

if no command speech and not attested and frontier ≥ no_input_at: close(no_input)
    # §8 option B: "not heard" in place of "not attested"

... routes A and B unchanged ...

route T, words the VAD missed:
    command cells < 6, attested
    trailing blanks ≥ 10 frames (400 ms)
    frontier − last_word ≥ 9,728 (608 ms)
    ≥ 15 of the last 19 cells non_speech or self_output
    → pending = {ceil_to_cell(last_word), frontier, T}

pending revoked: ≥ 4 command cells after pending.since (as now), or the stable prefix changed after pending.since (route T)

if (command speech exists or attested) and frontier − max(progress_sample, command_start or first_word) ≥ 3.0 s:
    # §8 option B: "or heard", so heard-but-incomplete words end here as retry
    if the stable prefix parses complete:
        boundary = route T's boundary when attested and command cells < 6,
                   else the last command cell at or before prefix_sample (fallback as now)
        finalize_once(boundary, route = T or fallback)
    otherwise: close(retry)

route T commits require a fresh re-decode of the span (as routes B and fallback).
```

## 4. Changes by file

| File | Change |
|---|---|
| `em_wake_phrase.py` | `CommandText.last_token_sample`: the last command word's emission sample, from `transcript.last_samples` |
| `em_utterance.py` | `streaming_command` returns the last word sample too. `Utterance` keeps the cells' `E` from `push_cell`, computes G1, G2, G4 and G5, and hands the reducer a `TextEvidence(first_word, last_word, gates_ok)` each block. `mark_output()` records non-earcon output starting during the utterance. New trace field `text_evidence` |
| `em_endpoint_policy.py` | `Route.T`; `TextEvidence`; `step(..., text=TextEvidence)`; the `no_input` change, `_route_t`, text-change revocation, the no-progress extension; `redecode_required` for `T` |
| `em_session.py` | `UtteranceSpec.output_idle`: a wake's `not candidate.producing_sound`, a button's "no content, dialog or alert output at the press". Calls `Utterance.mark_output()` when non-earcon output starts during an open utterance |
| `em_speech_worker.py` | `POLICY_REVISION = "post_afe_11"` |
| `static/dashboard.jsx` | `COMMIT_ROUTES.T`. "Waited … after speech" (`endpoint_ms = decided_at − boundary`) then reads from the last word |
| `docs/post-afe-audio-architecture.md` | §8.3 gains the fifth route; §16.6 gains the rules above, the values, and a `post_afe_11` paragraph with 911, 912 and 732 as evidence |
| `docs/voice-pipeline.md` | one plain-language paragraph: a quiet command Kroko understood can still end the turn |
| `controller/CHANGELOG.md` | the behaviour change |

No schema migration: `turns.commit_route` is text, and `text_evidence` rides in `decision_trace`.

## 5. Trace

`utterance.text_evidence`, the latest evaluation:

```text
first_word, last_word        capture-epoch samples | null
complete                     bool
output_idle                  bool
loudest_db, foreground_db    float | null
attested                     bool
failed_gate                  "kind" | "after_wake" | "complete" | "output" | "loudness" | null
```

A route T ending is the endpoint event `{"event": "commit", "route": "T", ...}`. A `no_input` with `failed_gate` set explains why heard words did not count.

## 6. Tests

Evidence fixtures (cell classes, levels and token emission samples taken from the 911 and 912 traces and recordings, no audio), in `tests/test_endpoint_policy.py` and `tests/test_utterance.py`:

- **911's shape:** no command cells, complete words after the trigger, quiet cells after them → route T pending at the last word plus 608 ms, commit 192 ms later, boundary at the last word, re-decode required; no `no_input` at +5 s.
- **912's shape:** two command cells at the chime, words after them → route T, with the boundary after the words, not at the cells.
- **Incomplete text** (881's) → `no_input` at the deadline (§8 option A).
- **Not idle:** the same words with the wake candidate producing sound, or with output started during the utterance → no route T.
- **Quiet words:** loudest `E` under the words more than 10 dB below `F` → no route T.
- **New words while pending** → revoked, then a fresh pending at the new last word.
- **Six or more command cells** → route A exactly as before. Route T is never evaluated.
- **Reply** → route T never applies.

## 7. Validation before deploying

1. **`controller/tools/turn_replay.py`**, kept like `text_fix_replay.py`. It drives the speech worker's Silero and Kroko over each stored turn recording, takes cell levels from the canonical PCM and echo and gap labels from the stored trace, and runs the real `Utterance`. It reports each turn's ending under `post_afe_10` and `post_afe_11`.
2. **Fidelity first:** on `post_afe_10` wake and button turns, the replay must reproduce the live trace's cells (≥ 99%) and ending before any comparison is trusted.
3. **Acceptance:**
   - only turns with fewer than 6 Silero command cells change;
   - 911, 912 and 732 commit their command, and their re-decode matches;
   - 818, 865 and 881 end as they do now;
   - no turn that completed changes.
4. **Live:** a quiet "turn on the fan" in the Master Bedroom, with consent. Check route `T` in the trace, `endpoint_ms`, and HA's transcript of the committed span.

## 8. Open decision

At the deadline, attested-but-incomplete words (881's "closed the lines"):

- **A (recommended): `no_input`, as now.** No prompt after a false wake that heard background talk (865).
- **B: hold the turn, then `retry`** ("Sorry, I didn't catch that." and a reply window). Friendlier when it really was the user (881), but 865's background talk would also have been prompted.

## 9. Risks

- **HA STT on quiet audio is unverified.** Kroko transcribed 911's command. The STT copy gets `asr_gain_for(wake_db)` before faster-whisper, which has not been run on 911-level audio. The live check in §7 covers it.
- **A TV saying a complete command right after a false idle wake,** within 10 dB of the wake word, would run. Today that happens whenever Silero fires on the TV; G3 and G5 bound it, and the re-decode does not.
- **Later endpoints for quiet commands.** Route T waits for Kroko's text, which the live stream emits at its 1.28 s chunk edges, and no pause finalize runs because Silero saw no speech. 911 would have committed at about +3.6 s (last word +2.79 s, then 608 ms, then 192 ms of lookahead), against `no_input` at +5.0 s today.
- **An emission sample earlier than the word's audio end** cuts the word short. The 192 ms lookahead covers ordinary lag, and the re-decode turns a clipped word into `retry`, not a wrong command.
