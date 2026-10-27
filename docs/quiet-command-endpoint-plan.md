# Ending a turn on Kroko's words when Silero misses a quiet command: plan

**Status:** proposed on 2026-10-26; nothing is implemented yet.
**Evidence:** turns 911 and 912 in the Master Bedroom on 2026-10-25, and a replay of every stored turn recording that we ran on 2026-10-26 while evaluating FireRedVAD (§1). The design this changes is in [post-afe-audio-architecture.md](post-afe-audio-architecture.md), referred to below as "spec §N".
**Goal:** when someone says the wake word and then quietly says a command, and Kroko transcribes the command but Silero doesn't mark it as speech, the turn should still end and the command should still go to Home Assistant. Today the turn times out with "no input".

**Not in scope:**
- Follow-up replies (`reply`, `ha_reply`). Without a wake word, nothing tells us who is speaking. Our replay also can't reproduce the state Silero starts a reply in, so we have no evidence to tune on.
- Replacing Silero. We tested FireRedVAD separately, and it can't simply be swapped in.
- Moving the recording's cut point when the new checks below fail, for example when the Dot is playing music, when the words are much quieter than the wake word, or in a reply. In those cases Kroko may be transcribing lyrics, a TV or someone across the room, so ending with "Sorry, I didn't catch that" is the safer result. When the checks pass, the "nothing new for 3 seconds" rule uses the same cut point as the new ending (§3).
- The echo of the wake chime being marked as command speech, which happened in 912. That needs a fix in echo detection.
- Local commands (stop, snooze, pause, dismiss). They still require Silero to have marked the words as speech (`local_command_preconditions`).

## 1. Where things stand

The controller splits the audio into 32 ms slices, and Silero decides whether each slice contains someone talking (spec §8.2). Every way a turn can end depends on Silero having marked at least one slice after the wake word as command speech:

| Rule in `em_endpoint_policy.EndpointReducer` | What it needs |
|---|---|
| "No input" timeout, 5 s after the wake word | no slice marked as command speech yet |
| Route A, the normal ending after a pause | at least 6 slices (`MIN_COMMAND_CELLS`) marked as command speech |
| Route B, used when a TV is talking | a command-speech slice before the point where Kroko's text settled (`_command_boundary_at`) |
| Route R, for replies to conversations HA started | a command-speech slice |
| The "nothing new for 3 seconds" rule | at least one command-speech slice |

On its own, Kroko's text can't end a turn or decide where to cut the recording. Turn 911 shows what that costs:

- Silero scored "Ophelia" at 0.5–0.77, and BCResNet accepted the wake word (0.97, with nothing playing).
- Kroko heard "turn on the fan" from 1.99 to 2.79 s after the wake word. Silero never scored those slices above 0.03, so it marked none of them as command speech.
- By 3.36 s Kroko's text read "turn on the fan", and HA's sentence matcher said it was a complete command.
- At 5.0 s the controller ended the turn as "no input".

The command was quiet: 7.6 dB above the room's background noise, 4 dB below the wake word, and only weakly voiced. We took 913's clearly spoken repeat and turned it down to 911's level over 911's background noise. Silero scored it 0.67, just under the 0.70 it needs, and Kroko still transcribed it. Kroko only failed when we turned it down another 6 dB.

We replayed the 209 stored wake and button turns through today's code. For the 88 turns recorded under the current policy (`post_afe_10`), at least 99% of the replay's slice labels matched the live ones on 87 turns. These turns had command text from Kroko but fewer than 6 command-speech slices from Silero:

| Turn | Device | What Kroko heard | Slices Silero marked | Complete command? |
|---|---|---|---|---|
| 911 | Master Bedroom | turn on the fan | 0 | yes |
| 912 | Master Bedroom | turn on the fan | 2 (the chime's echo) | yes |
| 732 | Master Bathroom | open the curtains | 5 | yes |
| 818 | Master Bedroom | when is the michigan game | 5 | no; it's a question for the conversation agent |
| 881 | Master Bathroom | closed the lines | 0 | no |
| 865 | Nursery | but also like that there | 0 | no; background talk after a false wake |

## 2. What changes

1. **A new way for a turn to end.** In the code it's route T, alongside routes A, B and R. The controller only tries it while Silero has marked fewer than 6 slices as command speech, which is too few for a normal ending. Once Silero has marked 6 or more, the existing endings apply exactly as they do today, so no turn that now ends by route A or B will end differently.

2. **When Kroko's words count.** The controller trusts Kroko's words only if all of these are true:

   | Check | Rule | Why |
   |---|---|---|
   | 1. Kind of turn | It's a wake or button turn. | Replies are out of scope. |
   | 2. After the wake word | After the wake word is removed (spec §8.4), there are words left, and Kroko reported the first of them after the wake trigger. | The wake word itself is never the command. |
   | 3. A complete command | Our grammar says the words form a complete command, or HA's sentence matcher does when our grammar doesn't know them. | We only act on commands HA can carry out. Partial sentences and background chatter don't qualify. |
   | 4. Nothing playing | The Dot wasn't playing anything when it heard the wake word, and nothing other than a short chime started during the turn. For a button press: no music, speech or alarm was playing when the button was pressed. | Music and speech can contain words, but a chime can't. In 912 the words overlapped the end of the chime. |
   | 5. Loud enough | If we know how loud the wake word was, the loudest moment of the words is no more than 10 dB quieter than it. If we don't know, this check passes. | This is the same 10 dB rule that already marks quieter speech as background. |

   The word times are the moments Kroko reported each word (`StreamingTranscript.first_samples` and `last_samples`). We use Kroko's times even when the text itself came from the faster-whisper server, because that server doesn't report times. Kroko already places the wake word this way.

3. **When the new ending happens.** All of these must be true:
   - All five checks pass, and Silero has marked fewer than 6 slices as command speech.
   - Kroko hasn't reported a new word for at least 400 ms (`MIN_TRAILING_BLANKS`).
   - At least 608 ms have passed since Kroko reported the last word. Route A waits the same 608 ms after a complete command.
   - Silero calls at least 15 of the last 19 slices silent or the Dot's own sound, so it doesn't hear anyone still talking.

   Then the controller waits another 192 ms, as it does for every ending, and ends the turn unless something new happens first. It cancels the ending if Kroko's text changes, or if Silero marks 4 more slices as command speech (the existing rule).

4. **Where the recording is cut, and the double-check.** The controller cuts the recording where Kroko reported the last word, rounded up to the next slice, plus the usual 192 ms. Kroko reports a word slightly after it's spoken: in 911 it reported the last word at 2.79 s, while the sound faded at about 2.5 s. So the cut includes the whole word.

   Before going on, the controller transcribes the cut recording again from scratch, with Kroko, or with the faster-whisper server if that's where the text came from. If the result doesn't match, the turn ends with "Sorry, I didn't catch that". This is the same check that caught 912's bad cut. After that, the turn continues as usual: HA's speech-to-text, removing the wake word, our local commands, transcript correction, then HA's intent handling and spoken reply.

5. **The "no input" timeout waits for Kroko's words.** At 5 s, the controller ends the turn as "no input" only if Silero has marked no command speech *and* Kroko's words don't pass the checks. If they do pass, the turn keeps listening, and one of these ends it:
   - the new ending;
   - the "nothing new for 3 seconds" rule, which now also applies when only Kroko heard the words. Three seconds after Kroko's text last changed, counted from no earlier than the first word, the controller ends the turn using the new ending's cut if the words are a complete command. Otherwise it says "Sorry, I didn't catch that";
   - the existing 15-second limit.

   Whether words that aren't a complete command should also hold off the timeout is the open decision in §8.

6. **No local commands through the new ending.** Local commands still require Silero to have marked the words as speech, and that doesn't change. A quiet "Ophelia, stop" during an alarm wouldn't qualify anyway, because the alarm fails check 4.

7. **A new policy revision, `post_afe_11`** (`em_speech_worker.POLICY_REVISION`), so each turn records which rules it ran under.

## 3. Rules for spec §16.6

```text
words_ok      = kind in (wake, button)                                    # check 1
                and command words exist and first_word > trigger_sample   # check 2
                and nothing_playing                                       # check 4
                and (F unknown or loudest_E(first_word − 7,680, last_word) ≥ F − 10 dB)   # check 5
command_ready = words_ok and completeness(stable_prefix) == complete      # check 3

if no command speech and not command_ready and frontier ≥ no_input_at: close(no_input)
    # §8 option B: "not words_ok" in place of "not command_ready"

... routes A and B unchanged ...

route T, when Silero missed the words:
    fewer than 6 command slices, command_ready
    trailing blanks ≥ 10 frames (400 ms)
    frontier − last_word ≥ 9,728 samples (608 ms)
    ≥ 15 of the last 19 slices non_speech or self_output
    → pending = {cut at ceil_to_cell(last_word), since frontier, route T}

cancel a pending ending: ≥ 4 command slices after pending.since (as now),
    or, for route T, the stable prefix changed after pending.since

if (command speech exists or command_ready) and frontier − max(progress_sample, command_start or first_word) ≥ 3.0 s:
    # §8 option B: "or words_ok", so words that aren't a complete command end here with "retry"
    if the stable prefix is a complete command:
        cut = route T's cut point when command_ready and fewer than 6 command slices,
              else the last command slice at or before prefix_sample (as now)
        finalize_once(cut, route = T or fallback)
    otherwise: close(retry)

an ending by route T requires the fresh re-transcription check, as routes B and fallback do.
```

## 4. Changes by file

| File | Change |
|---|---|
| `em_wake_phrase.py` | Add the moment Kroko reported the last command word (`CommandText.last_token_sample`, taken from `transcript.last_samples`). |
| `em_utterance.py` | `streaming_command` returns that moment too. `Utterance` keeps each slice's loudness from `push_cell`, runs checks 1, 2, 4 and 5, and gives the reducer a `KrokoWords(first_word, last_word, checks_pass)` every 80 ms. A new `note_output_started()` records that the Dot started playing something other than a chime during the turn. The turn's trace gets a `kroko_words` field. |
| `em_endpoint_policy.py` | Add `Route.T` and `KrokoWords`, and pass `KrokoWords` into `step()`. Change the "no input" rule, add route T, cancel a pending route T ending when the text changes, extend the 3-second rule, and require the re-transcription check for route T. |
| `em_session.py` | Add `UtteranceSpec.nothing_playing`. For a wake it's true when the candidate wasn't `producing_sound`; for a button press it's true when no music, speech or alarm was playing. Call `Utterance.note_output_started()` when anything other than a chime starts playing during an open turn. |
| `em_speech_worker.py` | Set `POLICY_REVISION = "post_afe_11"`. |
| `static/dashboard.jsx` | Add a label for route T to `COMMIT_ROUTES`, for example "words Kroko heard after the wake word (Silero missed them)". The "waited … after speech" figure (`endpoint_ms`) will then count from the last word. |
| `docs/post-afe-audio-architecture.md` | In §8.3, describe the fifth ending. In §16.6, add the rules above, their values, and a `post_afe_11` paragraph that cites 911, 912 and 732. |
| `docs/voice-pipeline.md` | Add one plain paragraph: a quiet command that Kroko understood can now end the turn. |
| `controller/CHANGELOG.md` | Describe the behaviour change. |

We don't need a database migration: `turns.commit_route` is a text column, and the new fields go into `decision_trace`.

## 5. What the turn record shows

Each turn's trace gets a `kroko_words` object, updated every time the controller runs the checks:

```text
first_word, last_word        when Kroko reported the first and last command word (capture samples), or null
complete                     whether the words form a complete command
nothing_playing              check 4
loudest_db, foreground_db    the loudness of the words' loudest slice and of the wake word, or null
checks_pass                  whether all five checks passed
failed_check                 "kind" | "after_wake" | "complete" | "playing" | "loudness" | null
```

When route T ends a turn, the endpoint log shows `{"event": "commit", "route": "T", ...}`. When a turn still times out as "no input" even though Kroko heard words, `failed_check` says which check stopped it.

## 6. Tests

The tests use numbers taken from the 911 and 912 traces and recordings: slice labels, loudness, and the moments Kroko reported each word. They don't use audio. They go in `tests/test_endpoint_policy.py` and `tests/test_utterance.py`.

- **Like 911:** no command-speech slices, a complete command after the wake word, then silence. Route T should end the turn 608 ms after the last word plus 192 ms, cut at the last word, and require the re-transcription check. The turn must not time out at 5 s.
- **Like 912:** two command-speech slices from the chime, then the words. Route T ends the turn, and the cut lands after the words, not at the chime.
- **Incomplete words** (881's "closed the lines"): the turn times out as "no input", as it does today (§8 option A).
- **Something playing:** the same words, but the Dot was playing when it heard the wake word, or started playing during the turn. Route T must not end the turn.
- **Too quiet:** the loudest moment of the words is more than 10 dB below the wake word. Route T must not end the turn.
- **More words while waiting:** Kroko reports a new word during the 192 ms wait. The controller cancels the ending and starts again after the new last word.
- **Six or more command-speech slices:** route A ends the turn exactly as before, and the controller never tries route T.
- **A reply:** route T never applies.

## 7. Checking it before deploying

1. **Add `controller/tools/turn_replay.py`**, a tool we keep, like `text_fix_replay.py`. It runs Silero and Kroko over each stored turn recording. It takes each slice's loudness from the recording and the echo and gap labels from the stored trace, runs the real `Utterance` code, and reports how each turn would end under `post_afe_10` and under `post_afe_11`.
2. **Check the replay first.** For `post_afe_10` wake and button turns, the replay's slice labels must match the live trace (at least 99%), and each turn must end the same way, before we trust any comparison.
3. **What must be true:**
   - Only turns where Silero marked fewer than 6 command-speech slices change.
   - 911, 912 and 732 end with their command, and the re-transcription check passes for each.
   - 818, 865 and 881 end as they do today.
   - No turn that completed today ends differently.
4. **Live test:** with your consent, say "turn on the fan" quietly in the Master Bedroom. Check that the trace shows route T, look at `endpoint_ms`, and check what HA's speech-to-text heard in the cut recording.

## 8. Open decision

What should happen at 5 s when Kroko's words pass checks 1, 2, 4 and 5 but aren't a complete command (881's "closed the lines")?

- **A (recommended): time out as "no input", as today.** We won't say "Sorry, I didn't catch that" after a false wake that only picked up background talk (865).
- **B: keep listening, then say "Sorry, I didn't catch that".** That's friendlier when it really was you (881), but 865's background talk would also have gotten the prompt.

## 9. Risks

- **We haven't checked HA's speech-to-text on audio this quiet.** Kroko transcribed 911's command. HA's faster-whisper gets the same recording after `asr_gain_for(wake_db)` boosts it, but we haven't run it on audio as quiet as 911's. The live test in §7 covers this.
- **A TV could trigger a command.** Suppose the Dot falsely wakes while nothing is playing, and a TV then says a complete command no more than 10 dB quieter than the wake word. The command would run. That already happens today whenever Silero picks up the TV. Checks 3 and 5 make it less likely. The re-transcription check doesn't help here, because it would hear the same TV.
- **Quiet commands end a little later than normal ones.** Route T has to wait for Kroko's text, which arrives in chunks every 1.28 s. Kroko also skips the extra pass it makes at pauses, because that pass only runs when Silero has heard speech. 911 would have ended at about 3.6 s (last word at 2.79 s, then 608 ms, then 192 ms), instead of timing out at 5.0 s.
- **A word reported early would be clipped.** If Kroko reported a word before the speaker finished it, the cut would clip the end of that word. The 192 ms margin covers normal delays, and the re-transcription check would turn a clipped word into "Sorry, I didn't catch that" rather than a wrong command.
