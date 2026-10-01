# Streaming ASR and end-of-utterance model evaluation (2026-09-29)

Question: is Kroko the right streaming model for the controller, and can a dedicated end-of-utterance (EOU) or turn-detection model replace or supplement text-based endpointing? This follows the latency review in `docs/turn-latency-review.md`.

All numbers come from replaying Office Dot audio on the controller host (AMD Ryzen 9 6900HX, x86-64, no GPU). Scripts and raw results are in `/tmp/sttlab`, outside the repository.

## Summary

- **No dedicated EOU model works as the endpoint signal on Dot audio.**
  - The audio-only detectors (Smart Turn v3.2, LiveKit v1-mini) cannot tell the pause after "Ophelia" apart from the real end of a command. That pause is the most frequent hold in this product.
  - Parakeet Realtime EOU never cuts off early. But with real room noise after speech it fires on only 5 of 10 command ends within 4 s, and its transcripts miss the wake word, which rules it out as the streaming ASR.
  - The text EOU models finish more free-form requests than the grammar (50–80% vs 15%), but they miss the commonest ones ("what time is it", "what's the weather").
- **The endpoint bottleneck is when text arrives, not how clever the judgement is.** Kroko's 1.28 s chunk delays text by a median 1.1 s. Nemotron's 160 ms steps cut that to 0.5 s; flushing Kroko at the pause is projected to reach about 0.45 s (300 ms of pause plus a 110–185 ms decode). Completeness is then best judged by the grammar plus HA's own recognizer (`docs/turn-latency-review.md`, P3), which knows the user's actual sentences.
- **Streaming accuracy matters where no second pass exists:** wake verification during playback and local stop, cancel and snooze.
  - Replaying the saved wake clips, Kroko passes 47 of 54 genuine wakes with the deployed 0.5 s flush (49 with 1.5 s). Nemotron passes 50 of 54, Parakeet EOU 18 of 54. None passes a self-wake.
  - Kroko's 0.5 s verification flush does not always reach its next chunk edge. That is a latent defect independent of any model change.

## Test data and method

- **Command recordings:** `data/recordings/G090LF10728426PR_{466..476}.wav` (10 clips, no 472). Each is the committed span of a wake turn as the controller's evidence copy: canonical mic × +20 dB, 16 kHz. A clip runs from the pre-roll start, 2.02 s before the wake trigger, to speech end + 192 ms. So each clip contains:
  - the wake word;
  - the **real pause after it**, while the user waits for the wake chime (a genuine mid-turn hold);
  - the command;
  - the **real end of the utterance**.
- **Wake clips:** `data/wakes/G090LF10728426PR/*.wav` (195 clips, turns 266–477, 1.4–2.7 s): the wake word and the audio just after it. They are stored at canonical level (about −57 dBFS RMS, against about −32 dBFS for the recordings), so every model and the VAD received them ×10, the evidence level.
- **Replay:** streaming models get 80 ms blocks, the speech worker's cadence. After a recording ends, they get 3 s of Gaussian noise at the recording's pre-roll room level, because the recordings stop 192 ms after speech end.
- **Transcripts:** there are no human reference transcripts. HA's STT output (`turns.stt_text`) and the obvious intent stand in for them. For eight of the ten the words are known ("Ophelia next/stop/what time is it", "play Nothing Else Matters by Chris Stapleton"); 468 and 469 are uncertain.
- **Limits:** one room, one speaker, ten end-of-utterance examples. Latency differences that follow from a model's design are reliable; accuracy differences are indicative only.

## Why transcription accuracy matters for the streaming model

The streaming model does **not** produce the text HA acts on. HA's own STT (section 16.7) transcribes the committed span again, and HA runs intents from that. The streaming text drives four decisions inside the controller (section 16.6):

| Use | Where | What a recognition error does |
|---|---|---|
| **Local commands** | `echomuse_grammar.match_local_command`, only in alert context (a ringing timer or alarm) or dialog context (a reply playing) | "stop", "cancel" and "snooze" run from this text alone, with no HA involved. If the model misses the word, nothing matches; the turn goes to HA, which cannot dismiss an EchoMuse alert. |
| **Wake verification** | Every wake candidate while the Dot is producing sound (music, TTS, a ringing alert): a fresh decode of the wake window, which must contain an approximate match for `ophel` (Levenshtein distance ≤ 0.40) | If the wake word comes out garbled ("failure"), a real wake is rejected. That is exactly when the user is trying to stop music, a reply, or an alarm. |
| **Pause length** | The grammar class of the stable text picks the silence the endpoint waits for: 608 ms, 1,216 ms, or 1,792 ms | Garbled text falls to `unknown` and waits the full 1.8 s. A mishearing that happens to parse as complete ends the turn early and cuts off the command. |
| **Wake-word cut** | Step 2 of the wake-phrase removal locates "Ophelia" in the streaming transcript; the cut is applied to HA's transcript | If the model misses the wake word, the cut falls back to position 0. A leftover "Ophelia," in HA's transcript makes HA's local agent fail to match, so the turn goes to the LLM (turn 433). |

So what matters is accuracy on a **small vocabulary**: the wake word, the local command words, and enough of each command to classify it. Word error rate on free-form content, such as song titles, affects only the grammar class. The general WER is what the earlier bake-off measured when it picked Kroko (`docs/post-afe-audio-architecture.md`, evidence [P2]). That bake-off never measured text latency.

If the controller's streaming text ever **replaces HA's STT** as the final transcript, full accuracy matters as well. That would reverse the section 16.7 decision.

A dedicated EOU model covers only the third use. The other three still need text, so an EOU model can sit beside a streaming ASR but cannot replace it.

## Baseline: Kroko's 1.28 s cadence

Kroko (`sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06`) is a chunked zipformer2 (`decode_chunk_len=128`, `T=141`).

- **Update cadence.** Its text updates only at 1.44 s into the stream and every 1.28 s after that; the ten recordings changed text only at 1.44, 2.72, 4.00, 5.28 and 6.56 s.
- **Median delay.** The final text appeared a median **1.10 s** after speech end (max 1.69 s).
- **Stability rule.** The 240 ms stability rule adds pure delay: the text cannot change between chunk edges, so waiting 240 ms checks nothing.
- **Trailing-blank rule.** The trailing-blank count also moves only once per chunk, so a word ending in the last 400 ms of a chunk costs a whole extra chunk (turns 466 and 471).
- **Complete commands.** Even if every command were classed `complete`, the median endpoint would still be 1.70 s after speech end, against 2.05 s actual. Kroko is the binding constraint in 8 of 10 turns.

## Streaming ASR candidates (sherpa-onnx, same replay)

"Final text after speech end" is when the model's last transcript first appeared. "Projected endpoint" is the time from speech end to the endpoint for a `complete` command: max(608 ms pause, text delay + 240 ms stability) + 192 ms lookahead, ignoring the trailing-blank rule. RTF is the real-time factor on one thread, measured over the ten clips plus padding.

| Model | Update step | Final text after speech end, median / max | Projected endpoint, median | RTF, 1 thread | Peak RSS |
|---|---|---|---|---|---|
| Kroko (deployed) | 1.28 s | 1.10 / 1.69 s | 1.53 s | 0.027 | 185 MB |
| Nemotron Speech Streaming EN 0.6B, 160 ms (2026-04-25) | 0.16 s | 0.51 / 1.37 s | 0.94 s (0.80 s without the 240 ms rule) | 0.85; 0.54 on 2 threads; 0.39 on 4 | ~970 MB |
| Nemotron Speech Streaming EN 0.6B, 80 ms | 0.08 s | 0.47 / 1.21 s | 0.90 s | 1.72 (slower than real time) | — |
| Nemotron 3.5 ASR Streaming 0.6B (multilingual), 160 ms | 0.16 s | 0.47 / 1.01 s | 0.91 s | 0.78 | — |
| NeMo FastConformer transducer 80 ms (2025) | 0.08–0.16 s | — | — | 0.19 | 257 MB [P2] |
| Parakeet unified 0.6B, 240 ms | — | empty output for 8 of 10 clips in this harness | — | — | — |

Transcripts:

| Turn | HA STT (after wake cut) | Kroko | Nemotron EN 160 ms | Nemotron 3.5 160 ms | NeMo FC 80 ms |
|---|---|---|---|---|---|
| 466 | Like Nothing Else Matters by Chris Stapleton | Ophilia, nothing else mattered by Chris Stapleton | Ophelia like nothing else matters by Chris Stapleton | Ophelia leve nothing else matters by Chris Stapleton | nothing else matters by chris able |
| 467 | played Nothing Else Matters by Chris Stapleton | Ophilia. Play nothing else matters by Chris Stapleton | Ophelia nothing else matters by Chris Stapleton | Ophelia play nothing else matters by Chris Stapleton | appeal ya point nothing else matters by chris people |
| 468 | plain night | Ophilia, clean note | Ophelia plain night | Plain night | okay ya point nine |
| 469 (reply turn, no wake word) | Play night | play night | Play night | Play night | play night |
| 470 | next | Ophilia next | Ophelia next | Ophelia next | ophelia |
| 471 | next | failure next | Ophelia next | The next | he |
| 473 | start | A failure | Ophelia stop | Ophelia stop | oh yeah |
| 474 | stop | Ophilia stop | Ophelia stop | Phylstop | (empty) |
| 475 | what time is it | Okay, what time is it | What time is it | What time is it | (empty) |
| 476 | what time is it | Ophilia. What time is it | Ophelia, what time is it | O failure what time is it | a failure both what time is it |

- **Nemotron EN** recognized the wake word in 8 of 9 wake turns and got 473 right. Kroko and HA's STT both failed on 473, and HA's "start." is what sent that turn to the LLM. It dropped "play" in 467.
- **Kroko** garbled the wake word in 3 of 9 ("failure", "A failure", "Okay").
- **Parakeet TDT 110M** was tried as a fast non-streaming re-decoder at a pause. It took 100–180 ms per clip but garbled short commands: "Oh feel your stuff." for 474, "We'll feel ya." for 473.

### Wake verification replay: the accuracy that matters most

Section 16.6's verifier was replayed on the saved wake clips. Each clip was raised to the evidence level (×10), decoded by a fresh greedy stream plus a zero flush, and passed if its letters-only transcript contains a substring within Levenshtein 0.40 of `ophel`.

"Genuine" means the clip's turn produced a recognisable request in HA's transcript (54 clips). Self-wakes are the four barge-in clips (321, 356, 421, 426). The saved clip window is not exactly the live verifier's window (support start − 300 ms to candidate open + 480 ms), so these are replays, not re-runs of live decisions.

| Model | Flush | Genuine wakes passing | Self-wakes passing | Genuine failures |
|---|---|---|---|---|
| Kroko | 0.5 s (as deployed) | 47 / 54 | 0 / 4 | "A failure", "", "I'm feeling it", "Okay,", "", "", "" |
| Kroko | 1.5 s | 49 / 54 | 0 / 4 | "A failure", "I'm feeling it.", "Okay,", "failure that", "Alien" |
| Nemotron EN 0.6B 160 ms | 0.5 s or 1.5 s | 50 / 54 | 0 / 4 | four empty transcripts |
| Parakeet Realtime EOU 120M | 0.5 s | 18 / 54 | 0 / 4 | 34 empty transcripts, plus "papalia" and "a few years" |

- **Kroko's chunking also affects verification.** A 0.5 s flush does not always reach the next 1.28 s chunk edge, so the end of the window is never decoded. Clips 433 and 474 returned empty text with 0.5 s and passed with 1.5 s.
- **Kroko's remaining failures are the wake word heard as other words.** Nemotron's are silence (empty output), with no substitutions.
- **Neither model passes a self-wake.**

## Dedicated EOU and turn-detection models

### Decision points for audio EOU models

Every audio detector is scored the way LiveKit's eot-bench scores them: **200 ms into a pause**, using audio up to that point. Silero v5 (the controller's VAD) finds the pauses in the evidence-level audio.

- **10 true ends:** each recording at speech end + 192 ms.
- **81 holds:** the pause after the wake word, while the user waits for the chime. 7 come from the recordings and 74 from wake clips that contain at least 220 ms of pause after the wake word. Four self-wake clips are excluded.

The user always goes on to speak the command, so a detector that fires at a hold is a false cutoff.

### Parakeet Realtime EOU 120M (NVIDIA): streaming ASR plus an `<EOU>` token

A 120M cache-aware FastConformer RNN-T (`att_context_size` [70, 1]; first chunk 90 ms, then 160 ms) that emits `<EOU>` (and `<EOB>`) tokens. It was run with NeMo 3.0 on CPU in PyTorch fp32, one thread, streaming chunk by chunk with `conformer_stream_step`.

- **Packaging.** sherpa-onnx does not ship it. Community ONNX exports exist (for example `ysdede/parakeet-realtime-eou-120m-v1-onnx`), but they need a custom cache-aware runner.
- **Licence.** NVIDIA Open Model License.
- **Harness.** NeMo 3.0 crashes merging hypotheses when timestamps are on, so decoding ran with `compute_timestamps=False`. Stream termination emits a spurious `<EOU>` from the final partial chunk, so each run appended a 1 s guard and ignored events inside it.

| Recording | After the wake word (hold) | `<EOU>` after speech end, room-noise tail | `<EOU>`, digital-zero tail | Transcript |
|---|---|---|---|---|
| 466 | none | 0.77 s | 0.45 s | played nothing else mattered by chris stapleton |
| 467 | none | 3.65 s | 0.45 s | ophelia played nothing else matters by chris stapleton |
| 468 | none | 1.64 s | 1.32 s | ophelia plain night |
| 469 | — | none within 4 s | 0.64 s | play night |
| 470 | none | none within 4 s | 1.54 s | ophelia next |
| 471 | none | none within 4 s | 1.48 s | papalia next |
| 473 | — | none within 4 s | none | (empty) |
| 474 | — | none within 4 s | 2.12 s | ophelia stop |
| 475 | none | 0.58 s | 0.58 s | what time is it |
| 476 | none | 1.73 s | 0.61 s | what time is it |

- **It never fired during a hold.** That covers the recordings' post-wake pauses and all 195 wake clips.
- **It rarely fires on real Dot audio.** With the room noise that really follows speech on the evidence copy, it fired within 4 s on only 5 of 10 ends, at a median of 1.64 s. With digital silence it fired on 9 of 10 at a median of 0.64 s. Its published EOU latency (160 ms median) was measured on TTS audio with 3 s of appended silence (model card), and the evidence copy's +20 dB room noise is not silence.
- **Its end-of-turn latency is no better than today's pause rules.** The best case (zero tail, median 0.64 s) is roughly the 608 ms `complete` pause. The realistic case is worse than the 1,792 ms `unknown` pause on half the turns.
- **Its transcripts are weaker than Kroko's on the wake word.** "Ophelia" appeared in 4 of 9 wake turns. 473 produced nothing, and 466 lost the wake word.
- **It is level-sensitive.** At canonical level (without the +20 dB evidence gain), 6 of 10 recordings produced no text at all.
- **Speed.** Real-time factor 0.6–0.8 on one thread in PyTorch fp32.

### LiveKit Turn Detector v1-mini: audio only, local CPU build

This is the local-CPU version from `livekit-local-inference`, the build that LiveKit Agents runs outside LiveKit Cloud. It is given the last 1.2 s of audio as PCM16 and returns P(end of turn).

| Threshold | True ends above | Holds above |
|---|---|---|
| 0.5 | 9 / 10 | 76 / 81 |
| 0.6 | 8 / 10 | 67 / 81 |
| 0.7 | 3 / 10 | 38 / 81 |
| 0.8 | 0 / 10 | 2 / 81 |

- **No threshold separates the two.** True ends scored 0.41–0.74; wake-word holds scored a median of 0.70.
- **Speed.** 47 ms per call.

### Smart Turn v3.2 (Pipecat): audio only

An 8 MB int8 ONNX model (Whisper-tiny encoder plus a linear head, BSD-2), given up to 8 s of audio ending at the pause.

| Threshold | True ends above | Holds above |
|---|---|---|
| 0.3 | 2 / 10 | 78 / 81 |
| 0.5 | 2 / 10 | 72 / 81 |
| 0.7 | 2 / 10 | 66 / 81 |
| 0.9 | 1 / 10 | 50 / 81 |

- **Its judgement is inverted on this audio.** "Ophelia" followed by a pause looks finished (median 0.94), and short commands look unfinished ("Ophelia stop" 0.02, "Ophelia next" 0.01).
- **The earlier synthetic-hesitation test agreed.** 11 of 23 mid-command pauses scored above 0.5.
- **Speed.** 110–130 ms per call on one thread.

### Text EOU models vs `echomuse_grammar`

Text EOU models judge from the transcript alone, which is what the grammar does today. The test set comes from the `turns` history:

- **75 complete requests:** distinct HA transcripts that are recognisably requests to the assistant. Weather, time, timers, scores, lights, conversions and stop phrases are kept; TV speech is dropped.
- **40 incomplete prefixes:** for each request, its longest prefix ending in a function word ("what's the", "set a timer for", "how many cups are in a").

Results, one thread:

| Model | Complete judged done | Incomplete judged done | Median time |
|---|---|---|---|
| `echomuse_grammar` (`complete`/`extendable` = done) | 11 / 75 | 0 / 40 | 0.1 ms |
| Namo Turn Detector v1 English (DistilBERT, fp32 ONNX, Apache-2.0) | 59 / 75 | 4 / 40 ("play", "set a", "start a", "what's the weather tomorrow at 8 a") | 11 ms |
| LiveKit text turn detector `v1.2.2-en` (SmolLM2-135M, q8 ONNX), shipped `en` threshold 0.0289 | 42 / 75 | 1 / 40 ("what's the weather at") | 4 ms |
| Same, with an assistant greeting ("How can I help?") as prior context | 38 / 75 | 0 / 40 | 7 ms |

- **The grammar rejects every incomplete prefix, but finishes only 15% of real requests.** Timers and stop phrases pass. Weather, time, scores and conversions do not, and those get the 1,792 ms pause.
- **The learned text models finish 50–80% of real requests.** That makes them a candidate for the `unknown` class specifically.
- **Both miss the commonest requests.** Namo scores "what time is it" 0.000, "never mind" 0.000 and "how many cups are in a quart" 0.000. LiveKit scores "what's the weather" 0.002, "what time is it" 0.004 and "next" 0.001. Both are trained on conversational turns with an agent, not on terse commands.
- **The prefix set is easy.** Every prefix ends in a function word. Real hesitations after a complete-looking fragment ("turn on the living room lights … to medium") are not represented.
- **Namo's published quantized model does not work.** `model_quant.onnx` returns a constant P ≈ 0.13 for every input, including the model card's own examples, under ONNX Runtime 1.30. The fp32 `model.onnx` was used instead.

The HA recognizer oracle proposed in `docs/turn-latency-review.md` (P3) classified 31 of 39 recent turns `complete` or `extendable`, from HA's own sentence templates. No learned text model is needed to reach that.

### Surveyed, not run

| Model | Why not |
|---|---|
| Vogent-Turn-80M (Whisper-tiny audio + SmolLM-135M text, Apache-2.0 code) | Weights are gated on Hugging Face (`vogent/Vogent-Turn-80M` needs a logged-in account that has accepted the terms); there is no token on this host. |
| ultraVAD (Ultravox, audio + dialog context) | 687M parameters on an LLM backbone; it needs dialog context. On LiveKit's eot-bench (English) it matches LiveKit v1-mini: 27.7% false cutoffs at 300 ms, 899 ms latency at a 5% cutoff budget. |
| Kyutai STT 1B with semantic VAD | ~1B parameters and a built-in 0.5 s text delay (model card). Real-time performance on this host's CPU is not established. |
| LiveKit Turn Detector v1 (full) | Served only on LiveKit Cloud; the local build is v1-mini (measured above). |
| Deepgram Flux, AssemblyAI, Soniox, OpenAI Realtime | Cloud services. EchoMuse keeps speech local. |

For scale, LiveKit's public eot-bench (English, real human-to-agent turns) reports these false-cutoff rates at a 300 ms latency budget:

| Model | False cutoffs @ 300 ms |
|---|---|
| LiveKit v1 | 9.9% |
| LiveKit v1-mini | 27.8% |
| Smart Turn v3.2 | 35.2% |
| Silence-only VAD baseline | 55.6% |

None of these are command-style voice-assistant data ([eot-bench](https://github.com/livekit/eot-bench)).

## Conclusions

1. **Do not add an audio EOU model.** On Dot audio, Smart Turn and LiveKit v1-mini fire on the post-wake pause as often as on real ends. Parakeet EOU is conservative but slow and unreliable under real room noise. All three are trained and evaluated on conversational turns (model cards), not on a wake word, a chime, and then a terse command.
2. **Fix when text arrives before tuning how completeness is judged.**
   - Either finalize Kroko at the pause (a shadow decode, 110–185 ms per pause, measured in `docs/turn-latency-review.md` P2) or move to Nemotron EN 0.6B at 160 ms (median text delay 0.51 s, about 1 core and 1 GB per active utterance).
   - Either way, drop the 240 ms stability span for chunked text; it adds delay and checks nothing.
   - Then judge completeness with the grammar plus HA's recognizer (review P3).
3. **Make Kroko's verification flush reach the next chunk edge.** With a 0.5 s flush, up to ~0.9 s at the end of a window between 1.28 and ~2.2 s is never decoded; a 1.5 s flush recovered 2 of 7 failures in the replay. This holds for as long as Kroko is the streaming model, and costs about one extra chunk decode (~37 ms of CPU) per verification.
4. **Text EOU models are, at most, a fallback for the `unknown` class.**
   - LiveKit's text model had the best precision here (1 false "done" in 40), but it misses "what time is it" and "what's the weather".
   - If used, it should only shorten `unknown` to the `extendable` pause (1,216 ms), not to 608 ms, and only after testing on command data.
5. **Collect a labelled Dot corpus with real mid-command hesitations before changing endpoint thresholds.** For example: "set a timer for … five minutes" and "turn on the living room lights … to medium". The ten retained recordings contain no such pauses, and the section 13 clipped-command gate (< 1%) cannot be evaluated from them.
