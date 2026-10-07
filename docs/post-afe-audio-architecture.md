# EchoMuse post-AFE audio and turn architecture

**Status:** implemented (wire contract: [protocol-v1.md](protocol-v1.md)); the section 13 qualification on real Dots has not been run.  
**Date:** 2026-09-22; model placement revised 2026-09-26 from on-device measurements (section 14, [D6]).  
**Scope:** Echo Dot Gen 2 using its native Android/Amazon audio front end; the currently deployed BCResNet audio-in ONNX wake model, run on the Dot; controller-side per-utterance speech processing; HA 2026.8.1 integration; timers handled exactly as on HA's own voice satellites; Home Assistant-owned durable alarms. Section 16 is normative where it is more specific than earlier sections.

## 1. Decision

Keep the native AFE. Replace everything above its capture/render boundary with an explicitly owned, sample-timed system:

1. **A device audio supervisor** continuously captures native-AFE audio, owns the final render mixer, arbitrates audible focus, and executes cached alert schedules independently of the controller.
2. **A device wake detector** runs the deployed BCResNet on the microphone continuously, computes per-cell loudness, and keeps short microphone and rendered-reference histories. No microphone audio leaves the Dot until a wake candidate, a button turn, a reply expectation, or an explicit diagnostic recording opens an uplink lease (section 4.4).
3. **A controller session actor per device** owns turn transitions. It consumes observations; it does not run inference or manipulate playback from competing background tasks.
4. **A controller speech worker** does the heavy, per-utterance work only while a lease is open: it attributes candidates against the rendered reference, verifies wakes heard over the device's own sound, and runs streaming ASR. An endpointer makes a provisional decision, observes a bounded lookahead, and only then commits an immutable audio span.
5. **Stock Home Assistant**, with no custom integration. HA's configured STT engine transcribes the bounded span in an STT-only pipeline run. The controller removes the wake phrase, handles alarm commands and local stop/snooze itself, and sends everything else to HA's intent→TTS run. **Timers work exactly as on HA's own voice satellites:** HA holds them in memory and tells the speaker when one ends, and the Dot rings (section 10.8). **Alarms** have no HA equivalent, so they are events on a stock Local Calendar per speaker: HA holds them durably, and the controller's alert engine drives their lifecycle (sections 10.1–10.7).

This copies the useful native separations—continuous on-device detection with heavy recognition elsewhere, self-output awareness, explicit dialog states, focus ownership, contextual local commands, bounded continuation, and persisted alerts—not Amazon's models, numerical thresholds, or every recovered behavior.

**Placement is by duty cycle.** Work that must run all the time for every speaker runs on the Dot, so controller load grows with simultaneous conversations rather than with the number of speakers. Work that runs only inside an utterance runs on the controller, where the models are 30× faster [D6]. On the Dot the deployed BCResNet measured 43% of one core. Silero VAD and the streaming ASR crash while loading under the Dot's ONNX Runtime unless graph optimization is disabled; unoptimized they cost 6.5 ms per 32 ms cell and an estimated 0.9× real time (section 8.1). So the Dot computes only loudness beside the wake model, and speech probability comes from the controller's VAD on uploaded audio.

**Important tradeoff:** robust endpoint control requires a speech decoder whose incremental state EchoMuse owns, so a streaming ASR in the controller decides when the utterance ends. HA's configured STT engine (currently `stt.faster_whisper`) produces the final text from the committed span, preserving its recognition quality, with HA's segmenter disabled so it does not cut the stream again. HA remains the home-control, conversation, and TTS authority. Every HA interaction uses stock APIs (section 16.7).

This is not a proposal to tune the existing collection of wake, ring, barge-in, and continuation watchers. Those paths are replaced at cutover.

### Intended observable behavior

- A user can say the wake word over music, a response, an alarm burst, or an alarm gap without waiting for a new detector window to fill.
- The device's own speech containing the wake word is evidence of self-playback, not an automatic invitation to start a turn.
- A real interruption does not require turning the wake threshold down by an arbitrary large factor.
- Waking a ringing device **backgrounds the alert; it does not dismiss it**. The wake word followed by “stop,” a physical action-button press, or an explicit dismiss command ends the occurrence.
- “<wake word>, snooze” affects an alarm occurrence, not a timer, and survives service restarts.
- A requested answer is heard after the actual prompt finishes, with a bounded early-answer path while it is still speaking.
- Fans and other non-speech noise do not keep a turn open. Competing speech is handled as a target-attribution problem, not mislabeled as silence.
- Previously armed alarms can ring and be stopped by button while HA or the controller is unavailable. Voice stop needs the controller. Timers behave like those on HA's own voice satellites: a timer rings only if HA and the controller are connected when it ends, and an HA restart loses running timers.
- An idle speaker sends no microphone audio anywhere.
- Ambiguous or contaminated speech can fail safely with a short clarification; it must not become a confident home-control action merely to meet a latency target.

## 2. Evidence boundary

The existing implementation and its comments are **not** normative inputs. Source was inspected only for hardware/model/API constraints. Native research was checked against already-decompiled archived code by separate investigators. No new device capture, playback, configuration change, restart, or Alexa activation was performed. Model cost was measured on the live Office Dot with a temporary benchmark in `/data/local/tmp`, removed afterwards [D6].

Evidence labels used here:

- **Recovered:** a decompiled control-flow path, shipped configuration, or binary API/parameter exists. This does not establish that every feature runs on this SKU.
- **Observed:** an actual command/model execution in this investigation, or an explicitly identified persisted record.
- **Proposed:** a design decision in this document. All new timeouts and performance targets are proposed unless explicitly attributed otherwise.
- **Unverified:** a dependency on measurements or access not established here.

### 2.1 Native behaviors to borrow, with qualifications

| Recovered evidence | What this design takes from it | What it does not assume |
|---|---|---|
| `EnumeratedPolicy` compares mic detections with speech-mark and playback-detection history [N1]. | Carry timestamped self-output evidence into wake acceptance. | A time match proves echo, or native suppression always rescues a simultaneous human wake. The recovered Java policy can suppress by time alone. |
| The native service has a second, playback-side wake decoder and suppresses mic detections within 1100 ms of one [N1][N2]. The shipped model sets include that decoder only for the `AMAZON` wake word (`en-US/AMAZON/pryon.manifest` → `playback.config`); `ALEXA`'s manifest has no playback model. | Score the actual rendered reference as well as the microphone. | A downloaded audio buffer has already been heard, or the default Alexa wake word ran this decoder on this build. |
| The spotter is two-stage: an HMM search proposes with thresholds wide open, and a DNN verifier accepts at 0.7. `kw.cfg.json` lowers only the verifier to 0.5 while `AudioPlayerState`, `audio_playback`, or `AlarmState` is set; TTS output has no override [N2]. | Relax the wake threshold modestly, and only for content and alerts; verify every wake heard during output with a second stage. | Amazon's numbers transfer to BCResNet, or a low bar alone can make barge-in work. |
| On this SKU the device runs only Pryon's spotter: `libwakewordserver_jni.so` imports the spotter surface, not a recognizer, and no ASR, language-model, or endpointer artifact exists to download. Endpointing is server-side (`Endpointer.Server`, `StopCaptureDirective`). The spotter is built to run continuously on this CPU: features once per 10 ms frame with continuous normalization, a quantized MLP scorer that scores every 2nd frame and drops to every 5th when 300 ms behind, and its second stage only on proposals. Stop, snooze, and cancel are extra keywords of the same spotter, used only while an alert rings [N2][N4]. | Continuous detection on the device; heavy per-utterance recognition and endpointing off the device (the controller here). | Pryon's cost, quantization, or frame skipping transfer to BCResNet: its weights are not in the archive, and BCResNet's windowed score semantics forbid skipping (section 5.2). |
| `ReadyState`, `ListenState`, `ThinkState`, and `MultiTurnRequestState` have explicit transitions and timeout/cancellation paths [N3]. | One turn owner, bounded states, separate endpoint and playback completion. | Current EchoMuse state names or task boundaries are correct. |
| Native continuation accepts an `ExpectSpeech` timeout; TTS has normal, cancelled, and interrupted completion paths [N3]. | Explicit reply expectation, beginning from audible completion, with cancellation invalidating old expectations. | A TTS URL, generated speech mark, or HA `tts-end` means the speaker has finished. |
| Pryon contains search-, VAD-, contextual-, DNN-, and speculative-endpoint machinery, plus backporch estimators [N4]. | Fuse evidence, distinguish provisional/final endpoint, preserve trailing speech, and impose bounds. | A recovered class name proves the active cloud configuration, or ASR endpointing is immune to TV speech. |
| NTT interfaces consume acoustic and contextual evidence; the research reports NTT unsupported on this build [N2]. | Device-directedness is a separate question from speech presence. | Speaker identity equals directedness, or an Amazon NTT model is available. |
| HeadlessBeacon persists alerts, rearms eligible records at boot, serializes ringing, and selects full versus short assets by foreground/background job state [N5]. | Durable schedule plus local executor, explicit alert focus, and bounded serial ringing. | A wake word itself dismisses an alert; that causal path was not established. |
| Native alerts notify ASP about playback classes [N5]. | Nothing: the notifications were tested and reach only a disabled playback leveller, so none are sent (section 4.5, [D5]). | Notification codes expose ERLE/DTD, or sending them improves wake accuracy. |

### 2.2 Corrections to avoid importing into the new design

Some research prose is stronger than the underlying evidence:

- **A decoder does not automatically exclude a television.** It can transcribe the television. Search state is useful evidence, not a source-separation guarantee.
- **An acoustic directedness embedding is not proven to be speaker identity.** The initial implementation uses no speaker model; a speaker-takeover route is backlog (section 19.1).
- **The native Java self-wake veto has no demonstrated double-talk rescue.** We deliberately avoid its unconditional temporal veto.
- **Full/short alert selection is stronger evidence than an asset-name inference:** `AssetAudioPlayerManager` selects `shortAlertAsset` outside full mode, and `HeadlessRingingService` maps job states to those modes. The reason the upstream job scheduler chooses a mode is not fully traced.
- **Native recurrence evidence is narrower than universal RRULE support.** The recovered `originalTime`/ISO path uses local time; an inspected RRULE branch returns null. Our DST policy is specified independently below.
- **Native command policy is not perfectly uniform.** The alert FSM restricts stop/snooze by type, while the headless service can stop its current item before a secondary policy handler runs. Copying that inconsistency is not a goal.
- **AFE metadata is evidence, not a prerequisite.** The notes this design started from contained conflicting interpretations of the PCM-LSB fields, and no validated per-frame ERLE/DTD contract existed, so the design works without it. The Fire OS 6 v3.3 layout has since been validated frame by frame and is carried as evidence only (section 4.5).

The archived files are proprietary reference material. Do not redistribute their binaries, model weights, sounds, or LED assets.

## 3. Deployment and authority

```text
Dot Gen 2 (continuous work)                          Controller host (per-utterance work)
───────────────────────────                          ────────────────────────────────────
microphones
   │
Android native AFE ── capture clock + PCM ─┬─ 6 s mic ring
   ▲                                       ├─ BCResNet mic scorer ── wake.candidate ───► session actor
   │ native far-end reference              └─ cell loudness, 16 s ring                     │ accept/reject,
   │                                                                                       │ uplink leases
OpenSL render ◄── final mixer                                                              │
                     ├─ post-mix reference ── 8 s reference ring                           │
                     ├─ presentation progress / uncertainty                                │
                     └─ local alert executor                                               │
                                                                                           ▼
uplink lease only: ring backfill + live mic, cells, reference ──────► per-lease audio timeline
                                                                        ├─ reference scorer (on demand)
                                                                        ├─ echo comparison, attribution
                                                                        └─ wake verification, streaming ASR
                                                                             ▼
                                                                        session actor
                                                                        ├─ render/focus/uplink commands
                                                                        ├─ committed audio span
                                                                        └─ alert engine + journal
Dot alert cache ◄──────────── versioned snapshots / acknowledgements ─────────┤
                                                                              │
Home Assistant (stock) ◄──── stock websocket/REST ─────────────────────────────┘
   ├─ assist_pipeline/run: STT only · intent→TTS · TTS only
   ├─ intent timers (in memory) ── started/updated/cancelled/finished ──► ESPHome satellite
   ├─ Local Calendar per speaker: durable alarms
   ├─ scripts exposed to Assist: LLM alarm tools
   ├─ selected conversation agent / home actions
   └─ ESPHome satellite: announce, start_conversation, entities
```

### 3.1 Ownership table

| Component | Sole authority | Must not own |
|---|---|---|
| Device audio supervisor | Capture epoch, sample counters, actual mixing/gain, audible focus application, provisional duck, physical mute/button handling, cached alert execution | Conversation meaning, cloud success claims |
| Device wake detector | Mic BCResNet scores, candidate open/close under the active profile, near-miss counts, per-cell loudness, mic/reference/cell rings, executing uplink leases | Accepting a wake, attribution, turn transitions |
| Controller session actor | Current turn/utterance IDs, wake acceptance and interruption, endpoint commitment, reply expectation, desired focus and uplink leases | Actual audible completion, durable alert schedule truth |
| Controller speech worker | VAD, on-demand reference scores, echo comparison, verification, streaming ASR observations; per-lease PCM | HA actions, ring dismissal, turn transitions |
| HA timer manager | Every timer: its existence, countdown, and finish, in memory | Ringing, stop, anything durable |
| HA Local Calendar | Which alarm schedules exist, their settings, and which occurrences are still unhandled; recurrence expansion | Ringing state, device delivery, microphone endpoint |
| Controller alert engine | Alarm occurrence lifecycle, calendar write order, device delivery, voice/LLM/dashboard alarm operations; turning HA's timer `finished` events into rings | Existence of alarm schedules (HA holds it); timer state (HA holds it) |
| HA pipeline (stock) | Final transcript of the committed span, intent outcome, conversation ID, response content, explicit need for a reply | When the utterance ended; wake-phrase removal; when generated audio becomes audible |

One actor serializes events per device. Inference runs outside its event loop, with bounded queues and explicit deadlines. There is no independent “ring listener” competing with a “barge listener” for the same microphone.

Implementation placement is fixed.

- **Device:** one inference thread, `wake`: a goroutine locked to its OS thread that runs BCResNet with one ONNX intra-op thread, sequential execution, and spinning off. Capture, mixing, and transport never wait for it. Cell loudness is plain arithmetic in the capture path.
- **Controller:** the asyncio thread runs actors and transport only; one two-thread speech executor runs every on-demand job (VAD, reference scoring, echo comparison, ASR). A stream's jobs are serialized and never execute concurrently with another job for that stream. Load one immutable model session per artifact, retain per-lease stream state separately, and use one ONNX intra-op thread with sequential execution. There is no process-per-device requirement or distributed inference service. An executor failure aborts affected utterances without dispatching text.

The device enforces focus because it owns the speaker. The controller requests named leases; it does not send a sequence of blind volume writes. Alert execution remains local even when the lease requester disappears.

### 3.2 Failure and authority invariants

1. One active utterance generation per device; every asynchronous result names its generation.
2. One final endpoint decision per utterance; only a committed transcript can reach home-control intent execution.
3. A late old TTS chunk, end-of-stream, timeout, wake result, or focus release cannot affect a newer generation.
4. Wake inference continues on the device across dialog and alert phase changes; privacy mute and capture discontinuity are explicit exceptions.
5. A capture gap is **unknown audio**, never evidence that the user stopped speaking.
6. Silence between ring bursts is still an active alert occurrence.
7. Missing playback/AFE evidence is unknown, not “no playback” or “no human.”
8. Acknowledging creation of an alert requires a successful HA durable commit. “Armed on the Dot” additionally requires the device's durable acknowledgement.
9. A persisted local dismissal prevents replay of the same occurrence after reconnect/reboot.
10. Privacy mute is device-sovereign. It suppresses microphone capture/transmission/inference, ends every uplink lease, and clears speech buffers and rings, without disabling scheduled alerts or physical stop.
11. No microphone audio or cell evidence leaves the device outside an uplink lease (section 4.4).

## 4. Audio boundary and clocks

### 4.1 Retain native signal processing

Use the native OpenSL ES/Android capture path with `VOICE_RECOGNITION`, and render through the native output path so the AFE retains its far-end reference. Do not reopen the codec with raw ALSA, replace beamforming/AEC, or cascade a second adaptive echo canceller after the native AFE.

The implementation retains **16 kHz mono S16 capture in 80 ms/1,280-sample callbacks** and **48 kHz mono S16 render** in 2,048-frame (42.7 ms) OpenSL writes [R1]. Transport forwards an acquired block immediately; acoustic analysis may subdivide it without pretending that subdivision reduces acquisition latency. Changing callback or write size is not part of this cutover. Mixer gain ramps are computed per sample, so ramp length is independent of the write period.

On FireOS 6 (no Android framework, no OpenSL ES) the same rule is realised through Amazon's `mixer` daemon via `libmixerAPI`. Capture opens the mixer's `micAsr` stream, which makes the HAL select the same `VOICE_RECOGNITION` ASP pipeline (`input_source=6`). Render opens a 48 kHz mono `MUSIC` stream, which the mixer loops back into ASP as the far-end reference. The firmware re-frames capture to the same 1,280-sample periods and keeps the same render write size, so nothing above the binding changes. One binary picks OpenSL or the mixer at start (`docs/fireos6-port.md`).

Fork captured audio before any model-specific transform:

- **Canonical PCM:** native-AFE signal, original level, shared by STT and acoustic analysis.
- **Wake input:** a scratch window with exactly the BCResNet training normalization.
- **Level/reference evidence:** computed from unnormalized samples.

Do not peak-normalize speech/VAD/energy history just because the wake classifier expects it. Do not add arbitrary EQ, AGC, denoising, or clipping to make a score look better. A new transform changes the model contract and requires its own evaluation.

### 4.2 Audio block contract

Each block has:

```text
stream_id                  mic | render_reference | cells
capture_epoch / render_epoch
sequence
first_sample               uint64, per-channel frames, not bytes
sample_rate, channels, format
frame_count
device_monotonic_time       anchor for first sample
clock_uncertainty_us
flags                       discontinuity, muted, underrun, estimated_time
PCM, or cell records for stream cells
```

`first_sample` advances through elapsed stream time, including explicitly represented missing ranges. Never concatenate across a gap as if adjacent. A restart, privacy transition, or unrecoverable clock reset starts a new epoch.

Use device monotonic time to relate capture and render. Controller receive time measures transport latency, not the acoustic event. Wall-clock UTC is for schedules and human logs, not audio alignment. The clocks need an estimated affine mapping and uncertainty; assuming equal sample rates or common boot time is insufficient.

The device keeps fixed-capacity rings: 6 s of mic PCM (192 kB), 8 s of the 16 kHz final-mix reference (256 kB), and 16 s of cell records (2 kB; section 16.1). These cover the oldest backfill a candidate lease can request (section 4.4). Use pooled blocks; do not copy the entire history on every frame. The controller keeps a timeline per uplink lease, bounded by the utterance policy in section 8.

### 4.3 Reference must describe what was rendered

Tap the **device's final digital mix**, after software source gain, ducking, resampling, and limiting, immediately before OpenSL output. This is an observation feed, not another AEC reference injected into the native pipeline. Include TTS, earcons, alert bursts, and every supported music source. Hardware volume and downstream native processing remain separate attributes/uncertainties.

Flush, seek, pause, rate change, underrun, and volume change are reported in `render.progress` so the controller maps reference samples to what was actually played.

Every output has `playback_id`, `generation`, `source_class`, sample positions, and a completion reason: `drained`, `cancelled`, `failed`, or `underrun`. `drained` means device-confirmed completion under its timing contract, not that the sender ran out of bytes.

**Clock decision:** use callback/sample accounting, not an unimplemented hardware presentation API. Timestamp every capture callback with device `CLOCK_MONOTONIC`, backdate by its sample duration, and mark the anchor `estimated`. Timestamp submission and completion of each render buffer; retain the completed sample frontier and outstanding queue duration. Fit sample-index versus callback-time by least squares over the latest 10 seconds, reject anchors with residual above 80 ms, and reset the stream epoch if estimated rate differs from nominal by more than 1,000 ppm or time moves backwards. The nominal uncertainty floor is one capture period plus outstanding render-queue time; unknown downstream acoustic delay is additional, not zero. Absolute self-match searches allow 0–500 ms extra delay. Never label these estimates measured DAC timestamps.

`render.progress` reports submitted and completed frame counts separately every 80 ms and on transitions. A normal output completion is declared after the last buffer completion plus a **150 ms drain guard**; the final completion includes `timing_quality=estimated`. This guard is a product default to measure at qualification, not a claim about this hardware's exact latency. Capture continues and early-answer detection remains active during the guard. Cancellation reports the last completed frontier, discards unsubmitted audio, invalidates affected reference intervals, and emits `cancelled` rather than `drained`. No endpoint or self-wake hard veto depends solely on estimated presentation timing.

`render.end` carries `end_frame`, the source frame after the last one sent. It travels on the control socket, so it can overtake the tail of the audio on the audio socket; the device keeps accepting contiguous audio up to `end_frame` and drains only once all of it has arrived. Without it, a short answer sent faster than the link carried it played only what had arrived when `render.end` did (about the 1 s prime) and dropped the rest.

The supported music path is controller-decoded media delivered to the device's `content` mixer input; TTS, earcons, and locally generated/cached alerts use the same mixer. Direct Android/Bluetooth/auxiliary players are outside the supported full-reference contract. Their use marks `reference_coverage=partial`; it does not secretly route around the mixer or turn wake detection off. No Bluetooth/AudioFlinger loopback implementation is required for this cutover.

### 4.4 Bounded transport

Control/physical-stop/focus messages must not queue behind seconds of PCM. Use separate authenticated control and bounded audio channels, carrying a shared generation and monotonic sequence.

**Uplink leases.** Microphone PCM (kind 1), final-mix reference (kind 2), cell records (kind 4), and in `afe_metadata_v1` sessions AFE records (kind 5, section 4.5) are uploaded only while a lease covers them. Each lease has an ID, a reason, an owner (the candidate, turn, expectation, or diagnostic mode it serves), a generation, a start sample per stream, and a 3 s TTL that the controller renews every second, like dialog focus leases. `uplink.close`, TTL expiry, privacy mute, a capture-epoch change, or session loss ends it. The device ignores an `uplink.renew` or `uplink.close` whose generation is older than the lease's. Per stream, the device uploads while at least one lease wants that stream. A sample may be sent twice when leases overlap; the controller keeps the first copy of each sample index per epoch. AFE records, where a session carries them, start with the lease's reference.

| Reason | Opened by | Mic from | Reference from | Cells from | Ends |
|---|---|---|---|---|---|
| `candidate` | device, with `wake.candidate` at candidate open | `support_start − 300 ms` | mic start − 2,500 ms | mic start − 10 s | `uplink.close` (`rejected` or `arbitration_lost`), conversion to `turn`, or TTL |
| `turn` | controller: converts an accepted candidate's lease in place (`uplink.renew` with reason `turn`, the turn as new owner, and generation + 1), or opens one for a button turn | button: press sample − 300 ms | mic start − 2,500 ms | mic start − 10 s | the utterance commits or closes |
| `reply` | controller, when a prompt carrying a reply expectation starts playing, or when a response carrying one arrives with no audio | live | live | open − 10 s | the expectation ends or its reply commits |
| `diagnostic` | controller, while a dashboard recording mode runs (section 18.1) | live | none | none | the mode ends; at most 30 minutes |

Transport rules:

- **Candidate ordering:** the device sends a candidate lease's audio only after the controller's `command.ack(accepted)` for its `wake.candidate`, so audio never arrives for a lease the controller has not seen. Without that acknowledgement within 1 s, the device ends the lease with reason `ttl`. Controller-opened leases need no acknowledgement: `uplink.open` precedes their audio.
- **Backfill** comes from the rings, in sample order, before live packets of the same stream: mic first, then cells, then AFE records (`afe_metadata_v1` sessions, section 4.5), then reference. Every mic and cell start is rounded down to a cell boundary (a multiple of 512 samples), every AFE-record start to a capture period (1,280 samples), and every reference start to a multiple of 2,560 reference samples, so VAD cells and reference hops line up with continuous processing. A start older than the ring is clipped to the oldest valid sample on that grid and reported in `uplink.ended`. Backfill is at most 6 s of mic, 8 s of reference and of AFE records, and 16 s of cells. Worst case for a candidate lease: `support_start` up to 3.32 s before the first crossing (section 5.2), plus up to 0.77 s of hop queue and inference before candidate open and up to 1 s waiting for the acknowledgement, puts the mic start at most 5.4 s back, the reference and AFE-record start 7.9 s, and the cell start 15.4 s.
- **Digital silence:** a reference packet whose samples are all zero carries flag `digital_silence` and no payload (section 16.1), so a quiet room costs no reference bandwidth.
- **Bounds:** at most 400 ms of queued live audio per stream beyond backfill still being sent. Live audio that waits more than 1,000 ms in the device's send queue ends the lease with reason `overrun`; the controller closes the utterance as `audio_overrun`. On the controller, an active speech job older than 750 ms does the same. Candidate evaluation must finish by the verification deadline (section 16.6) or reject.
- **Gaps:** a gap inside an open utterance is a missing range; the reducer closes the utterance as `interrupted` (section 16.6). Nothing invents silence or drops words.
- **Session loss:** leases do not survive it. Audio queued for a lost session is discarded, never sent on the next one. Heartbeats and session loss are defined in section 16.1.
- **Priority:** backpressure never delays the physical button path, a due alert, or control messages.
- **Downlink** keeps the existing device jitter buffer: each network render source (`content`, `dialog_output`) has a FIFO of 128 writes (~5.5 s) and primes 24 writes (~1.0 s) before starting. Measured link retransmission gaps of 0.4–1.4 s make a shallow buffer an underrun generator. Buffer depth never delays focus or cancellation: ducking applies at the mixer, and `render.cancel` discards the FIFO. Local sources (`earcon`, cached `alert`) do not prime.

These are implementation defaults, with qualification gates in section 13.

### 4.5 Native metadata is evidence, not an implementation dependency

The cutover sends **no ASP Binder commands** and changes no AFE configuration. Device-owned playback class, volume, mute, and audio-clock observations are sufficient inputs to the specified baseline.

**Fire OS 6: the AFE's per-frame metadata is carried as evidence, under `afe_metadata_v1`.** This is the measured compatibility change the paragraph at the end of this section asks for. The AFE writes one 128-bit v3.3 frame into bit 0 of every 128 samples (8 ms) of the `micAsr` capture ([alexa-afe.md](alexa-afe.md), "AFE metadata in bit 0 (v3.3)"). Every frame of the captures there validated. On the device, the firmware's decoder validated every frame it received: 3,750 of 3,750 per 30 s window, idle and during playback. Across a forced stream re-open it kept its lock and counted the 72 AFE frames (576 ms) the re-open lost.

- **Device.** The mixer capture binding decodes bit 0 of each 80 ms period without modifying a sample: wake scoring and ASR receive exactly what they did before. A frame counts only with sync `0xA5`, version 3.3, payload size 104, a matching checksum, and a lock confirmed by a second frame with the next `FRAME_COUNTER`. `FRAME_COUNTER` and `AFE_TIMESTAMP` measure lost frames, independently of the capture timeline. Each period becomes one 14-byte record (protocol-v1.md §3) of `PLAYBACK_ACTIVE`, `ERLE_RAW`, `DTD`, `RMS`, `DNN_VAD_PROB`, the clip/diverged/mute flags, `VOLUME` and the decoder's gap and sync evidence. Records are kept in an 8 s ring on the capture timeline.
- **Transport.** Records ride the audio plane as kind 5 under uplink leases, aligned to capture samples like cells. A candidate lease takes them from its reference start. `wake.stats.afe` carries only decoder health. A controller opts in through `session.ready` `afe_metadata`, because older controllers end the session on an unknown EMA1 kind.
- **Controller.** It parses records at the boundary and summarises them over a turn's wake support span and utterance span as evidence on the turn record and decision trace. It shows them on the dashboard next to the wake and turn details, with decoder health beside wake health.
- **Not carried.** Direction (`SSL_*`, `FD_*`) was constant in every capture, and wake-time energy has no field. Both stay `unavailable`.

**Fire OS 5:** its capture carries v2.1 frames (all 1,878,292 frames of 439 saved office-Dot clips passed the checksum), but the layout's field names are unresolved and it has no frame counter or live timestamp. It is not decoded, the capability is never announced, and native ERLE, DTD, direction and wake-time energy stay `unavailable` there. A device without the capability shows them as `unavailable` with the reason.

**No decision uses these values.** Wake acceptance, the self-playback verdicts of section 6.1, attribution, endpointing and every threshold are unchanged and read no AFE field. No measurement yet relates any field to an outcome. `DTD` and `DNN_VAD_PROB` fired on the device's own unconverged echo with nobody talking. `ERLE_RAW` is also low for quiet echo, not only for an unconverged AEC. Behaviour under real near-end speech is unmeasured. Making any field an input to a decision is a further measured change, with its own qualification. [afe-metadata.md](afe-metadata.md) validates each field against the captures, ranks what EchoMuse could build on them, and specifies the experiments that would decide each use.

Telling the ASP that a reply or alarm is playing was tested and **does not help** [D5]. Codes 1 (TTS status) and 9 (alarm status) are what native Alexa sends. Disassembly of `libasp.so`'s command dispatcher shows both reach only the playback-side automatic volume leveller. That leveller is disabled on this device, and its TTS and music tables are identical. None of the handlers touches echo cancellation, beamforming, beam selection, or the false-wake stage, which is keyed only by volume. A reversible live toggle confirmed the flags land in the leveller's state. The command is reachable (`service call audiosignalprocessor 3 i32 <code> i32 4 i32 <0|1> i32 0` as root), but it changes nothing EchoMuse's microphone hears, so it is not sent. The two self-wakes in the live history are not explained by the missing notification.

Any further addition of native notifications or metadata, or any decision that reads them, requires its own measured compatibility change. Do not add an empty adapter or a configuration switch that claims it is operational.

## 5. BCResNet wake detection

### 5.1 Deployed artifact contract

The wake engine supports BCResNet audio-in ONNX plus sidecar, not openWakeWord embeddings, TFLite, Pryon, feature-in graphs, or a generic plugin abstraction. The baseline is the model **currently gating wakes on the live controller**, verified on 2026-09-22 by hash, live scorer logs, graph inspection, and inference [D1]:

| Item | Deployed value |
|---|---|
| Graph | `oww_models/bcresnet_audio.onnx`, 640,360 B, SHA-256 `4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f` |
| Sidecar | `oww_models/bcresnet_audio.json`, SHA-256 `25da0c652c562bf0a45a8f34bb46e38788bfc689e31401f33950831a0f2af51f` |
| Input | `audio`, float32 `[batch,22400]` = 1.4 s at 16 kHz |
| Output | `logits`, float32 `[batch,3]`; no softmax node |
| Opset / frontend | ai.onnx 20; in-graph STFT, 512-sample frame, 160-sample hop, one-sided |
| Labels | `[noise, ohphelia, unknown]`, `wakeIndex=1` |
| Normalization | `normPeak=0.8`, peak guard `1e-4` |

The repository-root `bcresnet_audio.onnx` (SHA-256 `f1adf221…`) is **not** the deployed model. It has the same IO and sidecar, but the deployed graph is 1.5× wider at every BC-ResNet stage (68,777 versus 40,629 initializer elements). `~/git/bcresnet/configs/ohphelia.yaml` currently says `tau: 2`, which reproduces the narrower width; the deployed widths are consistent with `tau: 3` under `base_c=int(8*tau)` [INFERENCE from initializer shapes]. The two exports also lower the full-extent pooling differently (ReduceMean versus AveragePool); both evaluate the same kind of reduction, but only the deployed graph is authoritative. At cutover, replace the repository fixture with the deployed graph and pin both hashes in the controller's model registry so repository tests exercise the shipped weights. The authoritative bytes are the live controller's `/app/data/oww_models/bcresnet_audio.onnx` and `.json`, verified against the hashes above before they are copied. Record the deployed training run's config beside the model when it is located; until then, the hash is the provenance.

Exact per-window preparation, matching the live controller scorer that produced the calibrated threshold. The device implements exactly this preparation:

```text
x = int16_PCM_window / 32768.0                  # 22,400 samples, float32
if sqrt(mean(x^2)) < 1e-4: no score; smoothing history unchanged
p = max(abs(x))
if p > 1e-4: x = x * (0.8 / p)
logits = ONNX(audio=x[None, :])
prob = softmax(logits - max(logits))[wakeIndex]
smoothed = mean(last 3 scored prob values)       # the value compared to thresholds
```

The RMS floor and peak guard are part of the calibrated deployed behavior, not a speech VAD. Stream validity excludes gaps/muted audio before this step.

**Model registry.** The controller keeps a BCResNet registry under `oww_models/`, one entry per graph SHA-256: graph file, sidecar file and hash, that model's thresholds `{idle, playback, reference, near_miss}`, its spoken form (`wake_phrase`, used by wake-phrase removal, and `verify_core`, used by wake verification). Thresholds live with the model because scores are model-specific. The deployed entry is `4eb74512…` with `{0.90, 0.65, 0.30, 0.17}` (section 5.3), `wake_phrase` `ophelia`, and `verify_core` `ophel`. Upload (dashboard, section 18) validates the pair against this section's contract, runs the silence/noise/tone probe, and stores the entry; it never activates it. The fleet config key `wakeModel` names the active registry hash. Load, on the controller and on the device, rejects a graph/sidecar pair whose output count differs from the label count, whose wake index is invalid, whose sample rate is not 16 kHz, or whose hash is not the one the controller named. The device switches graph plus sidecar atomically between hops and starts a new scorer revision. Never pair a graph with a stale sidecar.

### 5.2 Scoring cadence, histories, and candidates

Score every **second 80 ms mic chunk (160 ms hop)** with **three-window smoothing**. That is the live controller cadence that produced the current operating threshold; changing either changes score semantics and requires recalibration. Windows end at capture-epoch sample indices that are multiples of 2,560. The first window after an epoch start ends at sample 23,040, the first multiple of 2,560 at or after 22,400, and the grid is kept across gaps and resets. The smoothed value is the mean of the most recent up-to-three scored probabilities since the last reset: after a reset, one scored window already gives a smoothed value. A window below the RMS floor is not scored and leaves that history unchanged, as the live scorer does [D1]. To keep candidate audio addressable, **six consecutive unscored hop slots (960 ms) clear the history**. This is the only deliberate difference from the live scorer's unbounded silence carry; it changes policy, not the graph, and is covered by the section 13 calibration. Each history entry keeps its window's end sample.

**The mic scorer runs on the device**, on its `wake` thread (section 3.1): the deployed graph through the device's ONNX Runtime 1.19.2 (armeabi-v7a), XNNPACK execution provider, one intra-op thread, spinning off. Measured on the Office Dot at the real 160 ms cadence for 60 s: mean 82 ms per window, p95 100 ms, maximum 131 ms, 43% of one Cortex-A53 core. The older model the firmware scores in shadow mode today costs 34% [D6]. The same graph takes 2.2–2.7 ms per window on the controller host [D1][P1], but a controller scorer needs every microphone streamed continuously: 127 MB an hour per Dot today (282 kbps measured) [D6], plus continuous controller work that grows with the number of speakers rather than with conversations. On the device, an idle speaker costs the controller nothing and its audio stays in the room, as with native Alexa's spotter and ESPHome's on-device `micro_wake_word` (section 2.1). An earlier draft read the shadow scorer's “187 of 374 frames skipped” as overload. That count is the hop: half the 80 ms chunks are skipped by design.

The device scorer is never reset at turn, response, alert-burst, or focus boundaries. Reset only on capture-epoch change, discontinuity, model revision change, or privacy mute; after reset it produces no value until it has a full valid 1.4 s window. A window spanning a gap is invalid. Pending hops queue at most four deep (640 ms). If a hop arrives with the queue full, the device drops the oldest pending hop, treats it as an invalid window (closing any open candidate and clearing the smoothing history), and counts a `wake_overrun`. Hops are never skipped or interpolated to catch up: that would change what the three-window mean means. At the measured cost the queue does not fill; section 13 gates it.

**The reference scorer runs on the controller, on demand.** It scores the uploaded 16 kHz final-mix reference of a candidate or turn lease (section 4.4) with the same graph, preparation, and three-window mean. Its hop grid is fixed to the reference stream: windows end at reference-stream sample indices that are multiples of 2,560. Its smoothing history starts empty at the first uploaded reference sample of the lease. Reference backfill starts 2.5 s before the mic backfill (`support_start − 2,800 ms`), which covers every reference window overlapping the candidate support after up to 500 ms of lag, its two prior windows, and up to one hop of grid alignment. The reference scorer uses 0.30 (section 5.3), costs about 2.5 ms per window on the controller host, and scores nothing while the reference is digital silence.

Candidate rules, evaluated on the device:

- **Open rules.** A candidate opens at a scored hop, with none open, when a live open rule of that hop's profile fires. A rule is `{profile, windows, combine, threshold}` with `windows` 1–3 and `combine` `mean` or `all`; it fires when, over the last min(`windows`, n) raw scores of the scored history — n, the scores since the last reset, is below `windows` only right after one, exactly as for the smoothed value — their mean (`mean`) or every one of them (`all`) is ≥ its threshold. The device tries the rules of the hop's profile in `session.ready` order; the first to fire opens the candidate and is credited in `wake.candidate.rule`. The list always begins with the active model's two **baseline rules**, `{idle, 3, mean, idle threshold}` and `{playback, 3, mean, playback threshold}` (section 5.3): the smoothed value ≥ the profile's threshold, the whole opening test before open rules existed, byte for byte. Extra rules (`wakeOpenRules`, at most four) are OR-ed after them, so they can open a candidate earlier, or where the baseline would not, but never suppress one the baseline opens. The device then sends `wake.candidate` and opens a `candidate` uplink lease (sections 4.4, 16.1). The opening profile and the opening rule's threshold are latched until the candidate closes, even if the mix changes, for example because the provisional duck backgrounds an alert. A changed rule list or threshold for the same graph is applied between candidates: deferred while one is open, the latest winning.
- Extend it while smoothed ≥ 50% of the latched threshold.
- Close after two consecutive scored values below 50% of the latched threshold, an invalid/gap window, or model reset. The device sends `wake.candidate_end`.
- A closed candidate re-arms the scorer; there is no global cooldown.
- `support_start` = start sample of the oldest window whose score the opening rule used: for a baseline rule normally first crossing end − 320 ms − 1.4 s, for a k-window rule first crossing end − (k − 1) × 160 ms − 1.4 s, later when fewer than k scores exist since a reset. A 2-window rule that opens one hop before the baseline therefore reports the `support_start` the baseline would have. Unscored hops between scores push it earlier, but each gap holds at most five (six clear the history), so it is never earlier than first crossing end − 1,920 ms − 1.4 s. `support_end` = end sample of the last window whose raw probability was ≥ the latched threshold. Both carry the ±160 ms hop uncertainty.
- Turn audio starts at `support_start − 300 ms`, clipped to valid history; the candidate lease backfills it from the mic ring. Do not discard the classifier window; wake-phrase removal is textual (section 16.6).
- **Near miss:** an episode opens at a smoothed value ≥ the `near_miss` floor and closes after two consecutive values below the floor. An episode during which no candidate opened counts once, with its peak, in `wake.stats`. No lease and no audio.
- During `ARMED`/`LISTENING`, a new mic candidate whose support overlaps the accepting candidate is a duplicate. A non-overlapping accepted candidate starts a new turn and fences the old one. These are controller decisions; the device reports every candidate.

**Shadow rules.** The device also evaluates up to eight shadow rules (`session.ready` `shadow_rules`, the `wakeShadowRules` setting) at every scored hop, after the live rules, and never acts on them: no candidate, lease, chime, duck, or audio upload. Each shadow rule runs its own **episode**, shaped like a candidate: it opens when the rule fires on a hop of its profile and none is open, extends while the smoothed value is ≥ 50% of the rule's threshold, and closes after two consecutive scored hops below; an invalid window, gap, overrun, reset, mute, or model or policy change abandons it uncounted. Per 30 s `wake.stats` window and rule, the device reports scored `hops` of the rule's profile, `opens`, and how episodes compared with the live candidates:

- `matched`: a live candidate was open at some scored hop of the episode. Its lead, (live first crossing end − shadow open sample) in hops clamped to ±3, goes into `lead_hist`; positive means the rule would have opened earlier.
- `unmatched`: no live candidate overlapped it and none opened within 10 s (160,000 capture samples) of its open — a would-be false wake.
- `retried`: no live candidate overlapped it, but one opened within those 10 s — likely a real wake the live rules missed and the user repeated.
- `live_only`: a live candidate of the rule's profile closed without any episode of the rule overlapping it — a real wake the rule on its own would have missed.

Each `unmatched`/`retried` resolution is also an event with the raw scores at the open and the episode's peak (at most 16 per rule per window, the rest counted). The controller adds the counters into per-hour rows per device and rule and keeps the newest 2,000 events per device; the Status tab shows the last 7 days per rule. Section 13.3 says how to read them. Shadow rules are how a candidate open rule earns its evidence before it is enabled.

Do not gate wake inference behind a separate VAD. Level, VAD, and reference evidence are used by attribution after a candidate exists.

### 5.3 Thresholds

Initial policy `post_afe_1`:

| Profile | Active when | Mic threshold |
|---|---|---:|
| `idle` | no content playing and no alert occurrence foreground; includes dialog output (TTS) and earcons | 0.90 |
| `playback` | content playing, or an alert occurrence foreground, including the gaps between bursts | 0.65 |

The device selects the profile at every hop from its own mixer state, since it owns the mix, and reports it with each candidate. The controller sends the thresholds in `session.ready`, together with the open rules built from them (the two baseline rules, then any `wakeOpenRules`) and the shadow rules (section 5.2).

This follows the native structure [N2]. Alexa's en-US verifier drops from 0.7 to 0.5 (×0.71), and only for the music, audio-playback, and alarm states. The ECHO wake word drops from 0.8 to 0.5–0.6. TTS output keeps the normal threshold. `0.65` is the deployed 0.90 scaled by the same ×0.71. Amazon's numbers themselves do not transfer; the ratio and the keying do. Threshold relaxation is never the barge-in mechanism on its own: every candidate heard while the device is producing sound still passes section 6.1 attribution and the section 16.6 wake verification.

The live controller's 0.10 playback threshold is the counter-example [D2][D3]:

- **Both logged barge-ins over TTS were the device triggering on its own answer.** The user asked “How many tablespoons is eight ounces?” twice (turns 420 and 425). Each answer was cut off 1.55 s and 1.2 s in by a barge-in scored at 0.31 and 0.21. The clips that triggered them transcribe as “Eight” and “I need to”, the answer's own words, not the wake word. The forced turns captured “Uh” and nothing. Two older barge-in clips (turns 322 and 356) also transcribe without the wake word (“fear” and nothing).
- **Self-echo scores high on this model.** Rescoring those two clips on the deployed model gives a newest-window probability of 0.71 and 0.33 at the firing hop. The logged 0.31 and 0.21 were three-window means still rising from about 0.02. Both barges fell 1.2–1.55 s into an answer that followed silence, where the native AEC is plausibly still converging [INFERENCE]. Pryon has an ERLE-span suppressor aimed at that case, but it defaults to disabled and no shipped `pryon.config` enables it. On this build, native relies instead on not relaxing the threshold during TTS and on speech-mark suppression (section 2.1).
- **The logged numbers were never evidence that genuine playback wakes score low.** A three-window mean crossing from below 0.10 to 0.29–0.31 in one hop means the newest window scored at least 0.57. It was about 0.8–0.9 if the preceding windows matched the logged 0.01–0.02. That is what happened for the two wakes that stopped a timer ring (ring-time scores 0.011–0.020, then 0.289 and 0.312). Those two are likely genuine but unverified: ring stops save no clip.
- **Room audio stays well below 0.65.** Over 50.7 h of logs, 30 idle near-misses reached 0.172–0.407 with no wake within 3 s, so none would have crossed 0.65 [D2].

An earlier draft of this document adopted 0.10 from these logs; that conclusion was wrong for the reasons above. No music session exists in the logs, so content playback is measured only at the section 13 gate.

The reference scorer uses **0.30**. It is deliberately more sensitive because a missed self-detection admits a false wake, whereas an extra reference candidate only feeds attribution. The near-miss logging floor remains 0.17.

**Multi-device arbitration.** An accepted mic candidate (after section 6.1 attribution) claims the fleet `WakeArbiter` before the session actor opens a turn. The first claim wins; any other device accepting within `wakeArbitrationMs` (default 700) closes as `arbitration_lost`: the controller sends `uplink.close(arbitration_lost)`, which releases any provisional duck, and there is no turn focus, controller chime, or dispatch. A losing device on `local_wake_chime` firmware may already have chimed an idle wake at candidate open (section 11.2); that chime is not retracted. Arbitration never applies to the button or reference candidates. A wake that captures a ringing alert or cancelled dialog output on its own device (section 6.3) skips arbitration, so a nearby speaker cannot steal a local stop.

These are initial values for the deployed artifact, not claims of field accuracy. Section 13 defines the release gates. If those gates fail, adjust thresholds only with the calibration procedure of section 13.3; do not add special cases for rings, song titles, or particular responses.

**Extra open rules are calibrated policy values too.** A rule over fewer windows pools less evidence, and `all` instead of `mean` is a different test; either changes what a threshold means as much as moving it does, so on the same audio such a rule can open where the 3-window baseline never would. The baseline stays in every list, so an extra rule cannot cost recall; what it can cost is false activations. Add one only with evidence that it meets section 5.4's idle ceiling, read from that same rule's shadow results as section 13.3 describes, and keep it a shadow rule until that evidence exists. Shadow rules themselves change nothing: they are measurement, not policy.

### 5.4 No wake-model retraining in this cutover

Retraining is **not required and not part of this implementation**. The native mechanisms worth copying are policy around a spotter—self-output evidence, a second verification stage, wake-prefixed local commands, persistent listening, and explicit dialog state—not a different wake classifier. The deployed model satisfies the contract: correct IO/sidecar agreement, finite calibrated-shape output, 0.0033–0.0106 wake probability on silence/noise/tone probes, maximum smoothed 0.0043 on a non-wake LibriSpeech-derived utterance, and live idle gating at 0.90 with no load errors [D1][D2][P1]. Its observed weakness is false-accepting its own unconverged echo (section 5.3). The verifier and attribution stage address that without new weights.

The most likely future fit is a **playback-condition fine-tune** of this same model: same audio-in/sidecar contract, trained from the deployed checkpoint with device-channel data the new infrastructure collects anyway. That data is accepted and rejected playback-profile candidates with their mic window, the aligned 16 kHz reference, and an attribution record; opt-in wake clips; and user-labeled misses (a wake followed within 10 s by a button press or repeated wake). It is triggered only by the gate below, not scheduled.

The capabilities a new model might be expected to add are handled elsewhere:

| Alexa-like need | Handled by | Why retraining is not the stronger fix |
|---|---|---|
| `STOP` in context | wake word, then a local match of the streaming-ASR text (section 6.3) | Same shape as native “Alexa, stop”; no single-word class to false-accept, and “stop please” and negation are handled explicitly. |
| Self-wake from TTS/music/alerts | reference scorer, final-mix comparison, and ASR wake verification (section 16.6) | Native evidence uses self-output/timing policy plus a second-stage verifier; a mic-only model cannot know what the device rendered. |
| Continuation | explicit reply expectations | NTT weights and directedness training data are unavailable. |
| Word boundary for command start | pre-roll plus textual alias removal | A window classifier cannot provide phoneme alignment even if retrained. |

A retraining project is justified only by failed acceptance evidence on native-AFE device-channel recordings: at the best measured operating point either (a) playback-profile recall is below 95% while false activations exceed 0.5/hour, or (b) idle false activations exceed 0.2/hour with recall below the current baseline. It must retain the same audio-in/sidecar contract, add device-channel playback-leak and alarm-gap negatives/positives, and pass the same gates before replacing the deployed hash. Model weight changes are never used to hide an attribution, transport, or endpoint defect.

## 6. Self-playback and barge-in

### 6.1 Separate proposal, attribution, and action

A BCResNet score proposes a wake. A self-playback evaluator attributes it. The session actor decides whether to interrupt. These are separate records and failure reasons.

Available evidence:

1. Render-reference BCResNet candidates, scored by the controller on the candidate lease's uploaded reference (section 5.2) and mapped into mic time with uncertainty.
2. The rendered TTS itself through that reference scorer. HA TTS provides no speech marks, so protocol v1 does not use them; response text is not a timestamp.
3. Short-time spectral/time similarity between reference and microphone over a delay search around the calibrated acoustic path.
4. Near-end residual/activity in cells that the reference does not explain.
5. On Fire OS 6 with `afe_metadata_v1`, the native AFE's per-frame metadata (playback activity, ERLE, double-talk, VAD) as recorded evidence only: no verdict below reads it (section 4.5). Elsewhere native AFE metrics are `unavailable`.

Decision policy for a mic candidate while the device is producing sound, in this order. The device decides that fact, because it owns the mix: any source audible in the final mix during the candidate support or the preceding 2 s. It reports it as `producing_sound` in `wake.candidate`.

1. **Device rendered something wake-like:** an overlapping reference-scorer candidate rejects as `self_output`, unless near-end speech is present (double-talk). This mirrors the native playback decoder, which ships for the `AMAZON` wake word, and the speech-mark window, adding a double-talk rescue.
2. **Echo explains the mic:** `echo_only` from the reference comparison rejects.
3. **Second-stage verification:** the streaming ASR must hear the wake word in the candidate's audio (section 16.6). Otherwise reject as `unverified_wake`. Replayed offline, this stage rejects both self-wakes in the live history, where the native AEC residue was speech-like and did not resemble the reference waveform [D3].
4. Otherwise accept.

From candidate open until the decision, the device applies a provisional duck on its own (section 16.6), so the user hears an immediate response and the verifier hears less playback; the controller's decision keeps or releases it. With no sound produced by the device, the idle path accepts on the BCResNet open rule alone (section 5.2), like today's live idle gating: the controller needs only the `wake.candidate` message to accept. For those idle wakes a `local_wake_chime` device plays the wake chime itself at candidate open, without waiting for the controller (section 11.2).

The comparator is not a second adaptive AEC and does not alter the canonical microphone signal. Waveform correlation is weak evidence after the native AEC: its residue is not linearly related to the reference, and in the observed self-wakes it was intelligible speech. Near-end evidence therefore never accepts a candidate on its own. It only rescues double-talk in step 1.

A person who says only the wake word at the same moment the device says it may remain indistinguishable. Log it as an ambiguous miss rather than claiming it is solved. There is no blanket deaf interval around a wake chime, every alarm burst, or every occurrence of the wake word in TTS.

### 6.2 Focus policy

The independent focus sources are `content`, `alert`, `dialog_input`, `dialog_output`, and `earcon`. Privacy/mute indication is separate from speaker focus.

| Event/state | Content | Alert occurrence | Dialog output |
|---|---|---|---|
| Idle | Normal | Foreground when due | None |
| Accepted wake / dialog input | Duck. Every mixer source ducks; a player outside the mixer is `reference_coverage=partial` and is not controlled | Background: stop full burst promptly; remain pending | Cancel current generation |
| Thinking | Remain ducked for bounded turn lifetime | Remain background under a bounded lease | None |
| Speaking | Duck | Background | Foreground |
| Expected reply | Remain ducked during reply deadline | Remain background under the same bounded interaction | None, unless an early reply interrupts the prompt |
| Turn ends normally | Restore eligible content | Return undismissed alert to foreground | Release |
| Wake + “stop” / action button while ringing | Remain ducked until alert queue is empty, then restore eligible content | Dismiss the captured occurrence; advance queue | The button opens no turn; wake + “stop” closes its turn without HA |
| Due alert during long TTS | Pause content | Become audible when dialog output ends | Continue until it drains or 2 s after the alert became due, whichever is first; then cancel with a 30 ms fade |

Use a **30 ms linear gain ramp** for normal duck/restore and a **10 ms ramp** for physical stop. Dialog ducks content by **`duckDb`** (default −18 dB) relative to its user-selected gain; alert foreground pauses content. Do not wait for a long queued buffer to drain before applying focus. Music's resume token includes source generation and prior user-paused state: releasing dialog focus must not resurrect media the user stopped, a track replaced meanwhile, or an expired stream.

An alert remains visually pending while acoustically backgrounded. Stop its full burst; play **no additional background cue while listening**. At a **15-second maximum continuous background lease**, terminate the current uncommitted utterance with `alert_preempted`, cancel any remaining dialog output, and foreground the undismissed alert. Already committed HA actions are not rolled back. Repeated wakes do not reset this occurrence-level deadline. After an actual foreground burst has completed, a new wake can start a fresh bounded interaction. This is the specified policy, not a recovered Amazon duration.

### 6.3 Stop and snooze: wake word, then a local command

The deployed model's labels are `noise`, `ohphelia`, and `unknown`; it has no `stop` or `snooze` class. Do not invent those outputs or retain an openWakeWord stop model as a hidden dependency.

**Every voice command, including stop and snooze, starts with the wake word**: “Ophelia, stop”, the same shape as native “Alexa, stop”. Native Alexa handles that stop locally only while its alert channel is active (`ListenState`) [N3]. There is no bare-word listening at any time. No speech is interpreted without an accepted wake, the action button, or an explicit reply expectation (section 9).

1. **Wake.** An accepted wake backgrounds any ringing alert (its full bursts stop) and cancels or ducks dialog output as usual (section 6.2). A wake while the device is producing sound uses the playback profile and verification (sections 5.3, 6.1). At acceptance the actor captures the **command context**: the foreground or backgrounded alert occurrence ID, if any; otherwise the playback ID of dialog output the wake cancelled, if its audio has started. A reply still waiting for its first audio is not audible and is no context, so a “stop” then goes to HA.
2. **Utterance.** The turn runs through the normal endpoint reducer. A bare “stop” is grammar-complete, so it commits after the 608 ms pause.
3. **Local execution.** At commit, if the command context is non-empty and the stable streaming text, after wake-phrase removal (section 16.6), is exactly one of the local commands (section 16.2), the actor executes it without dispatching to HA:
   - alert context + “stop”/“cancel”: send `alert.act` dismiss for the captured occurrence. For an alarm, the device applies it as a local operation, the same journaled path as the button, and it reaches the calendar through the normal local-operation upload (section 10.6). A timer ring is simply stopped: HA already dropped the timer when it finished (section 10.8).
   - alert context + “snooze” on an alarm: `alert.act` snooze with the occurrence's frozen `snooze_ms`.
   - dialog context + “stop”/“cancel”: the wake already cancelled the output; close the turn.
4. **Everything else** is a normal HA turn: “don't stop”, “stop the music”, “cancel the kitchen timer” while no timer is ringing (HA cancels the running timer), a snooze of a timer (HA explains that timers cannot be snoozed; the ring continues when the turn closes), and any stop with an empty command context.

The target is always the occurrence captured at wake acceptance. A queue transition while the user is speaking cannot redirect a “stop” to the next alarm. A wake followed by silence or an unrelated request never dismisses: the turn closes and the undismissed alert returns to the foreground.

**Behavior change from today:** the current controller stops a ringing timer on the wake word alone (`wake_word_listener` → `stop_timer_ring`). After cutover the wake only backgrounds it, and “stop” or the button dismisses it.

Local execution uses the streaming-ASR text, so voice stop needs the controller but not HA or its STT. The action button needs neither: it is always local, stops the active occurrence while one exists, and otherwise starts a turn. There is no on-Dot ASR: the streaming ASR runs at an estimated 0.9× real time on the Dot and misses the verification deadline by an order of magnitude [D6]. Native Alexa stops offline because “stop” is an extra keyword of its on-device spotter (section 2.1); the deployed BCResNet has no such class (section 5.4), so a Dot that cannot reach its controller stops by button only.

## 7. Turn lifecycle

Alert state, content state, and dialog state are orthogonal. Do not create a separate dialog implementation for “ringing.”

| Dialog state | Entry | Exit / required behavior |
|---|---|---|
| `IDLE` | No accepted dialog | Wake or button opens `ARMED`; explicit authorized prompt can create a reply expectation. Continuous wake histories remain alive. |
| `ARMED` | Allocate turn/utterance; acquire input focus; attach pre-roll | Target command speech → `LISTENING`. No command speech by deadline → finish without HA intent. Wake word alone is not the command. |
| `LISTENING` | Stream PCM to turn decoder and evidence extractors | Provisional end → `END_PENDING`; cancel/mute/gap/maximum duration → explicit terminal reason. |
| `END_PENDING` | Freeze candidate boundary, continue observing | Resumed target speech revokes candidate → `LISTENING`. Valid lookahead and stable text → exactly one `COMMITTED`. |
| `COMMITTED` | Immutable audio/text span and commit ID | Execute a wake-prefixed local command (section 6.3) and close, or dispatch once to HA and transition to `THINKING`. No speculative home actions before this point. |
| `THINKING` | Await intent/response | Response audio → `SPEAKING`; no-audio result → finish or `EXPECT_REPLY`; accepted new wake cancels old response eligibility. |
| `SPEAKING` | Device reports current playback generation started | Device reports drained → finish or `EXPECT_REPLY`; wake/stop/early answer cancels remaining output. |
| `EXPECT_REPLY` | Explicit, unexpired reply expectation | Qualified speech → new utterance under retained conversation context; timeout/cancel/mute/failure → finish. |
| `CLOSING` | Terminal reason chosen | Release only owned leases, clear temporary target/context as appropriate, preserve independently active alerts/content. Then `IDLE`. |

Every waiting state has a deadline: 30 seconds from sending the intent run to `intent-end` (section 16.7; an LLM agent may call tools), named `ha_timeout`; 10 seconds from `intent-end` to the start of response audio, 2 seconds without output progress, and 120 seconds for one spoken response, all named `response_timeout`. The start limit applies to a response streamed before `intent-end` too: until `intent-end` its start is bounded by the intent deadline plus 10 seconds, and from `intent-end` it must start within 10 seconds of it. The longest start observed in the turn history is about 2 seconds after the URL, so the limit catches a stalled fetch while leaving room for a slow cloud TTS engine. Exceeding a limit closes with that reason and fences later callbacks; it does not leave content ducked indefinitely. Ephemeral dialog focus leases expire after 3 seconds unless renewed every second. On expiry the device cancels orphaned dialog output, restores still-eligible content, and foregrounds any active alert; durable alert execution does not depend on that heartbeat. The occurrence-level alert-background cap still applies even while the controller renews a dialog lease.

**Terminal feedback.** Every closed turn gives exactly this feedback. The LED animations are the existing scene entries. A spoken line is a TTS-only run (section 16.7); if that run fails, the turn shows `error_anim` alone.

| Terminal reason | Feedback | Reply expectation |
|---|---|---|
| `no_input` | `nospeech_anim` | none |
| `empty_transcript` (HA STT returned no text) | “Sorry, I didn't catch that.” | free-form clarification; counts toward the chain limit |
| `retry` | “Sorry, I didn't catch that.” | free-form clarification; counts toward the chain limit |
| `too_long` | “That was too long. Try a shorter request.” | none |
| `interrupted`, `audio_overrun` | `error_anim` | none |
| `speech_unavailable`, `ha_timeout`, `response_timeout`, `stt_failed` (after its one retry), `ha_error` (a pipeline `error` event) | `error_anim`, then “Something went wrong.” | none |
| `muted` | the mute indication only | none |
| `session_lost` | none; the device clears dialog indicators when their leases expire (section 11.2) | none |
| `outcome_unknown` | “I'm not sure that worked.” | none |
| `arbitration_lost`, `unverified_wake`, `verifier_timeout`, `self_output`, `echo_only` | nothing; any provisional duck is released | none |
| `alert_preempted` | nothing; the alert takes the foreground | none |

The error cue named elsewhere is `error_anim`.

Identifiers: `device_session_id`, `turn_id`, `utterance_id`, `conversation_id`, `playback_id`, and `generation` are distinct. A conversation may contain multiple utterances; an alert can outlive many conversations.

An accepted interruption while HA is processing prevents the old response from playing and fences its callbacks. It **cannot undo a home action already executed**. Do not claim cancellation rolled back an intent. On an ambiguous HA transport failure, do not automatically resubmit an ordinary command; report uncertainty. Durable alert operations have a separate idempotency contract.

New explicit wake normally starts a new turn. It retains a pending question's context only when it is an interruption of that still-valid question; otherwise clear stale continuation state. A rejected self-wake does not arm local dialog commands or create reply expectation.

## 8. Endpointing: end this user's utterance, not all sound

### 8.1 Speech worker

The speech worker turns audio into timestamped evidence. It never decides anything: it does not open, commit, reset, or dispatch a turn, and sherpa-onnx's own endpoint detection is disabled. The session actor (section 7) owns every decision. BCResNet on the device remains the only wake detector. The other models here supply evidence for endpointing, attribution, wake verification, and local commands. Numeric rules are in section 16.6.

**What runs where, on which audio, and when**

| Component | Model | Runs on | Input | When | Output | State lifetime |
|---|---|---|---|---|---|---|
| Mic wake scorer | deployed BCResNet | Dot | canonical mic; its own peak normalization | continuously, every 160 ms | candidates, near misses, stats | capture epoch |
| Cell loudness | none (arithmetic) | Dot | canonical mic | continuously, every 32 ms cell | `E`, flags, active-source mask | 16 s cell ring |
| VAD | Silero v5 | controller | evidence copy of uploaded mic | every cell of a lease | speech probability per cell | lease; recurrent state carried |
| Background and foreground | none | controller | device `E`, controller VAD | every cell of a lease | background `B`, command foreground `F` | `B` rolling 10 s; `F` per utterance |
| Reference wake scorer | deployed BCResNet | controller | uploaded 16 kHz final-mix reference | on demand: candidate and turn leases, while the reference is non-silent | reference candidates | lease |
| Reference comparator | none (array math) | controller | uploaded canonical mic + reference | per wake candidate; per cell inside an utterance while the reference is non-silent | `echo_only` / `near_end_present` / `unknown` | lag re-estimated every second (section 16.6) |
| Streaming ASR | Kroko | controller | evidence copy | one stream per utterance; one fresh stream per wake candidate that needs verification | tokens, emission times, trailing blanks, stable prefix, grammar result | one utterance |

**Why this split.** Measured on the Office Dot with its own ONNX Runtime (1.19.2, armeabi-v7a, one Cortex-A53 thread at 1.3 GHz) and on the controller host with one thread [D6]:

| Model, one call | Dot | Controller host | Placement |
|---|---|---|---|
| BCResNet, one 1.4 s window | 82 ms mean, 131 ms max at the 160 ms cadence with XNNPACK: 43% of one core | 2.3 ms | Dot: the only model that must run all the time |
| Silero v5, one 32 ms cell | crashes in `CreateSession` (SIGBUS) at every enabled graph-optimization level; 6.5 ms with optimization disabled, about 20% of one core | 0.08 ms | controller, only inside leases |
| Kroko encoder, one 1.28 s chunk | same crash; 1,133 ms with optimization disabled, an estimated 0.9× real time | 37 ms | controller |

The newest official ONNX Runtime Android build that loads on API 22 is 1.20.0, since later builds lack the SysV symbol hash this linker needs, and it crashes the same way. The Dot therefore keeps its current 1.19.2 runtime and runs only BCResNet. A device VAD would serve only the pre-wake half of the background estimate, which per-cell loudness serves (section 16.6); a continuous fifth of a core is not worth that.

The two speech models read the **evidence copy**, a fixed +20 dB copy of the mic (section 16.6). At the raw native-AFE level Silero v4 and the smaller zipformers missed speech [P2]; Kroko is level-insensitive (9.4% WER at raw, +20 dB, and +30 dB), so one shared copy serves both. The controller builds it from uploaded canonical PCM. Level and reference comparisons read canonical PCM: their ratios do not depend on gain, and the echo comparison must see exactly what was captured. BCResNet applies its own peak normalization. HA's final transcription gets a separate STT copy (section 16.7).

**Per-block flow on the device**, for every 80 ms capture block:

1. Append canonical PCM to the mic ring. Compute `E` for each completed 512-sample cell, with its flags and the final mix's active-source mask, into the cell ring. A gap becomes an explicit missing range, never silence.
2. Every second block, queue a BCResNet hop on the `wake` thread (section 5.2).
3. For each stream some lease wants, send the block after that stream's backfill.

**Per-packet flow on the controller**, for every uploaded packet of a lease:

1. Transport validates the EMA1 frame and places it in the lease timeline by sample index. A duplicate sample is dropped; a gap becomes an explicit missing range.
2. The actor schedules the new samples' jobs on the speech executor: VAD for new mic cells; an ASR decode if an utterance stream is open; the reference comparison for new cells if an utterance is open and the reference is non-silent; reference scoring for new reference hops of a candidate or turn lease.
3. The worker builds the evidence copy once per block and shares it between VAD and ASR.
4. Each result returns as an observation stamped with the sample it covers through. The actor applies observations per stream in sample order. It evaluates the endpoint reducer for a block only after device loudness, VAD, and ASR have all reached that block's end: the evidence frontier.

**Observation contract**

```text
Observation
  device_id, capture_epoch, stream: mic | reference
  source: device | controller
  kind: candidate | level | vad | reference_score | echo | asr | verification | error
  through_sample          evidence covers samples before this index
  utterance_id?           set for per-utterance streams
  model_revision, policy_hash
  payload                 kind-specific; any field may be `unavailable`
  computed_at_ms          host time, diagnostics only

payloads
  candidate        candidate_id, lease_id, profile, threshold, producing_sound,
                   first_crossing_sample, support_start, support_end | open, hop scores
  level            first_cell_sample, E[], flags[], source_mask[]   from the device
                   B | unavailable, F | unavailable                 derived by the controller
  vad              first_cell_sample, probabilities[]               one per 32 ms cell
  reference_score  first_hop_end_sample, raw[], smoothed[], candidates[]
  echo             per-cell result[], lag_samples | unavailable
  asr              tokens[], token_emission_sample[], text, stable_prefix,
                   trailing_blank_frames, grammar_result, finalized
  verification     candidate_id, text, alias_distance, pass | fail | timeout
  error            component, reason
```

`unavailable` is never encoded as zero or false. A missing `B` means “background not yet measured”, not “silent room”.

**Streams inside an utterance**

- **ASR** opens with pre-roll, which the lease backfilled. For a wake it starts at the candidate's `support_start − 300 ms`, up to about 2.2 s of history; for the button, at the press minus 300 ms; for an explicit reply, at the detected onset minus 300 ms. Catching up on 2 s of pre-roll takes about 65 ms at the measured real-time factor 0.032. The stream closes at commit or close and is never reused, so one utterance's decoder context cannot leak into the next. Wake-verification streams are separate, and their text never becomes command text.
- **VAD** starts with fresh state and zero context at the lease's first mic sample.
- **Per-cell echo comparison** runs only while the final mix is non-silent: ducked music, or an earcon at turn open. With a silent reference, cells are marked `no_reference` and the comparison costs nothing.

**Resets.** A capture-epoch change, discontinuity, or mute resets VAD state and its 64-sample context, discards `B` (it needs 1 s of support again), and closes any open ASR stream, ending that utterance as `interrupted`. Mute also ends every lease and erases all buffered audio on both sides. A model or policy revision change is applied only between utterances.

**Failure and backpressure.** Queue-age limits are in section 4.4. A controller worker exception, non-finite output, or missed job deadline produces an `error` observation. The actor then closes the affected utterance as `interrupted`, plays the error cue, and dispatches nothing. Three errors within 60 s mark the worker `speech_unavailable`, and new turns are refused with the error cue. While it is unavailable, the worker runs a probe every 10 s: 1 s of zeros through VAD and ASR. It clears after two consecutive probes each finish without error within 1 s. Wake detection on the device keeps running, so a wake yields the cue rather than silence, and alerts, button stop, and mute are unaffected. On the device, a BCResNet error or non-finite output invalidates that window and counts in `wake.stats`; three within 60 s report `wake_unavailable`, which the dashboard shows. The button still starts turns.

**Startup.** The controller verifies every bundle hash (section 16.6) and warms each model on 1 s of zeros before accepting audio; the trailing-blank rate check runs when a bundle is installed, not on every start. The device verifies its speech assets' hashes and loads the graph before it scores.

**Cost**

- **Dot, continuous:** BCResNet at 43% of one A53 core; loudness is arithmetic. Today's firmware uses about 38% of one core, 34% of it for the shadow scorer this replaces, beside about 28% for the Amazon AFE in `mediaserver`. The Dot kept 2 of its 4 cores online at that load, at 37 °C with no throttling [D6]. The BCResNet session peaked at 15.5 MB RSS in isolation.
- **Controller, per idle speaker:** control traffic only.
- **Controller, per candidate while the device produces sound:** about 15 reference windows at 2.5 ms, the echo comparison, and one verification decode of about 70 ms. An idle-profile candidate costs nothing until it becomes a turn.
- **Controller, per open utterance:** VAD at 0.10 ms per cell and ASR at real-time factor 0.032: about 4% of one host core per concurrent utterance. The two-thread executor therefore serves about 50 simultaneous utterances, however many speakers are idle.
- **Controller memory:** model sessions are shared; the measured process RSS with Kroko loaded was 185 MB. Per lease: the open utterance (at most 30 s) plus a rolling 10 s before it, of mic and reference, at most 2.6 MB, plus one ASR stream. Older lease audio is dropped as it ages; a diagnostic lease's audio goes to its recording store, not this timeline.
- **Network:** an idle Dot uploads no audio, where today it streams 282 kbps continuously [D6]. A candidate backfills about 2.2 s of mic (70 kB), about 4.7 s of reference (150 kB) when the device is producing sound, and about 12 s of cell records (1.5 kB). An open turn then streams 256 kbps of mic, plus 256 kbps of reference while the mix is non-silent.

These figures are per-model feasibility measurements [D1][P2][D6], not a multi-device load test (section 13).

**After commit.** The committed canonical span becomes the STT copy (section 16.7) and goes to the endpoint's HA STT engine for the final transcript. Local commands (section 6.3) skip that step. HA receives a finite, already-bounded stream and does not segment it again, so endpoint ownership stays with EchoMuse.

### 8.2 Attribution without pretending to separate sources

Attribution answers one question for every 32 ms cell of an open utterance: **what produced this sound?** It uses only evidence available for that cell: echo comparison against what the device played, VAD probability, and level relative to the room's background `B` and the command speaker's foreground `F`. It never edits audio and never guesses identity.

**Cell classes**, first match wins:

| Order | Class | Assigned when | Meaning |
|---:|---|---|---|
| 1 | `gap` | the cell is inside a missing range or mute | unknown audio; the reducer closes the utterance as `interrupted` |
| 2 | `self_output` | per-cell echo comparison returns `echo_only` | the device's own playback explains the sound |
| 3 | `non_speech` | VAD ≤ 0.35 | fan, hum, music bed without voice, silence |
| 4 | `background_speech` | VAD ≥ 0.50, `F` known, and `E ≤ F − 10 dB` | speech clearly quieter than the command: a TV across the room, a distant conversation, ducked vocals |
| 5 | `command_speech` | inside a command-speech run | the person issuing the command, or someone indistinguishable from them |
| 6 | `unknown` | anything else | ambiguous; bounded rather than guessed |

A **command-speech run** opens on a cell with VAD ≥ 0.65 whose level is within the command range (`r ≥ 0.55`, or `r` unavailable). It continues while VAD > 0.35 and the cell is not `self_output` or `background_speech`. This hysteresis keeps a word's softer syllables from flickering between classes. Cells between 0.35 and 0.65 outside a run are `unknown`.

**Where `F` comes from.** Section 16.6 defines it and the trigger sample. The wake word may seed `F`, but it is not command speech for `no_input` or an endpoint. Button turns and explicit replies build `F` from their first command speech. Until `F` exists, no cell is `background_speech`, so early speech is command speech. `B` is the room's floor: the 20th percentile of loudness over the preceding 10 s, excluding cells the controller's VAD calls speech (section 16.6). It is what `r` is measured against and what the echo tests use.

**How each class affects the turn**

| Class | Updates `last_cmd_end` | Qualifies for route A's 15-of-19 vote | Route B (complete command, then background) |
|---|---|---|---|
| `command_speech` | yes | no | blocks it |
| `background_speech` | no | only after `needs_more` or `unknown` text | counts toward the 80% requirement |
| `self_output` | no | yes | ignored |
| `non_speech` | no | yes | ignored |
| `unknown` | no | no | ignored |

`unknown` cells neither extend a command nor end it. They can only delay a pause, and the reducer's 3.0 s no-progress limit bounds that delay (section 16.6).

**Worked cases**

| Situation | What the cells look like | Outcome |
|---|---|---|
| Fan or dishwasher running | `non_speech` after the command | Route A commits after the normal pause |
| TV across the room during a grammar command (“set a timer for five minutes”) | TV speech arrives ≥ 10 dB below the commander: `background_speech` | Route B commits once the stable parse is complete and the TV's words don't extend it |
| TV across the room during a free-form question (“what's the score of the Bears game”) | TV speech arrives ≥ 10 dB below the commander: `background_speech` | Route A counts it as pause and commits after the 1,792 ms `unknown` pause, as in a quiet room, once the recognizer has emitted no word for 400 ms |
| TV at conversational level during a free-form question | TV cells within 10 dB of `F`: `command_speech` or `unknown` | No real pause: `too_long` at the 15 s bound, unless the user pauses while the TV is quiet |
| Second person speaks right after the commander, at similar level | `command_speech` (indistinguishable by level) | Both people's words land in one utterance, ending at the next real pause or at `too_long`, and HA hears both: the physical limit below. Speaker takeover is backlog (section 19.1) |
| Music ducked 18 dB with vocals | Native AEC plus the duck usually leave vocals ≥ 10 dB below the commander: `background_speech`; clean echo matches become `self_output` | Music does not keep the turn open |
| Earcon at turn open | Reference non-silent; echo comparison `echo_only` | `self_output`, never mistaken for the start of a command |
| Soft syllables (“…lights”) and quieter stretches of the command | Can fall 10 dB below `F`: `background_speech` | They never move the cut, which route A places 192 ms after the last `command_speech` cell. After `complete` or `extendable` text they hold the turn open, and route B still needs text that no longer extends. After `needs_more` or `unknown` text they count as pause: a soft stretch of about 1.9 s, with no louder cell and no word recognized in its last 400 ms, ends the turn before anything said after it |

**No speaker model.** Attribution uses echo, VAD, and level only. Voiceprints were probed and rejected for the initial implementation: 1.2 s windows gave no usable separation, and the 2.0 s evidence is a two-clip sanity test [P1]. The deferred speaker-takeover route is in section 19.1.

**What attribution does not do**

- **It does not separate sources.** The committed span is contiguous canonical audio, background included. HA's STT hears the TV too, which is why route B and the fallback re-decode the span and reject a parse that changed.
- **It does not establish identity or authorization.** Level says “probably the same talker as the wake word”, not who the talker is.
- **It does not infer directedness.** Whether speech was addressed to the device comes only from the wake word, the button, or an explicit reply expectation.

**Physical limit:** one beam-selected mono stream cannot reliably unmix equal-level overlapping voices, distinguish every replay of the same speaker, or reconstruct speech the native selector suppressed. The design responds with bounded, named failures (`retry`, `too_long`) rather than confident guesses.

The decision trace (section 11.3) records the class sequence as run-length segments with the deciding rule (`vad`, `level`, `echo`, `hysteresis`), so a wrong endpoint can be explained from the record.

### 8.3 Endpoint routes

Four decision paths exist, with numeric rules in section 16.6:

1. **Normal pause:** VAD votes, decoder trailing blanks, and stable text agree; required pause length depends on grammar completeness. After `needs_more` or `unknown` text, speech at least 10 dB below the command counts as pause, so a free-form request ends under a quieter TV.
2. **Complete command under background speech:** a stable grammar-complete command is followed only by lower-level speech that does not extend the parse. This is the TV-room path for covered commands.
3. **Reply on the ESPHome path:** for replies to HA-started conversations only, any 1,024 ms pause after command speech ends the reply (section 16.7).
4. **Bounded failure:** no command, gap, too-long audio, or no stable progress closes as `no_input`, `interrupted`, `too_long`, or `retry` without dispatching guessed text.

Credible command resumption revokes a pending end before commit. After commit, continued speech is a new turn and cannot mutate the committed request. The submitted span includes a fixed 192 ms tail, is cut from canonical PCM, and never includes synthetic flush silence. Background speech is evidence against extending the command; it is not permission to splice words from separate islands.

### 8.4 Wake-phrase removal

No audio is ever trimmed. The STT span starts with the pre-roll, so it always contains the wake word and never clips the first command word, even in “Ophelia turn off the lights” said without a pause. The wake phrase is removed from the **text**, in the controller, between HA's stock STT-only run and its intent run (section 16.7), so no HA customization is needed.

The rule is deterministic and never strips audio by a time offset (section 16.6):

1. The streaming recognizer's transcript locates the wake phrase: the closest fuzzy match to the registered spelling (`ophelia`) among the words decoded by 480 ms after the wake detection. If it has no match, the wake phrase is assumed to open the utterance (position 0).
2. In the final transcript, the same fuzzy match is sought only near that position. Everything up to and including it is removed, and the rest is kept verbatim.
3. If the final transcript lacks a match there, nothing is removed.

This removes speech said before the wake word in the pre-roll, never removes a later mention, and applies only to wake-initiated turns. The position-0 fallback exists because Kroko can drop a quiet wake word outright, and a leftover “Ophelia,” is not harmless: HA's built-in agent needs the whole sentence to match, so with `prefer_local_intents` it sends a locally handled command to the conversation agent instead [D7].

## 9. Continuation

### 9.1 Explicit expected reply

Map HA `continue_conversation=true` to a first-class expectation:

```text
expectation_id, conversation_id, originating_turn_id
prompt_playback_id + generation
reason                      explicit_question | clarification
reply_deadline
reply_choices               EchoMuse clarifications only: [{value, aliases[]}]
pending_operation           EchoMuse clarifications only: the partial alarm parse
                            {action, time, missing, days, name, endpoint_id, turn_id}
```

An EchoMuse clarification (“set an alarm for seven” → “AM or PM?”) stores the partial parse in `pending_operation`, with `missing` naming the absent slot, and offers `reply_choices`, here `[{value: "am", aliases: ["am", "a m", "morning", "in the morning"]}, {value: "pm", aliases: ["pm", "p m", "evening", "in the evening", "afternoon", "at night"]}]`. A reply that selects a choice fills the missing slot, and the alert engine validates and applies the completed operation as if it had been spoken whole. Its operation ID derives from the original `turn_id`. Section 16.6 defines matching and the one re-prompt.

HA's documented flag means the agent expects a follow-up [H1]. It does not say that the mic should open immediately when TTS generation ends. Replies to an HA-started conversation (`assist_satellite.start_conversation`) follow the same rules, with two differences from section 16.7: they are sent through the ESPHome satellite path so HA applies the automation's stored prompt, and the utterance ends at its first pause of 1,024 ms (section 16.6).

- Open and renew the `reply` uplink lease when prompt audio starts; when no prompt audio exists, open it as soon as the response creates the expectation. A streamed response that is already audible when `continue_conversation` arrives opens it then. Keep capture and wake histories continuous throughout the prompt. Retain pre-roll across prompt completion so an immediate answer is not lost.
- The expectation's `dialog_input` focus is acquired only at the guarded drain (before the prompt's `dialog_output` lease is released, so content and alerts never see a gap) or when an early answer cuts the prompt off. Acquiring it while the prompt plays would make the device cancel the prompt (section 6.2: dialog input cancels current dialog output).
- During an explicit question, permit an early answer only under section 16.6's level/VAD/reference rule; otherwise let the prompt finish while retaining the audio. A user can always interrupt with the wake word or button. Do not accept the prompt's own words as an answer.
- On prompt failure/cancellation, invalidate its delayed completion and expectation unless an already-accepted early answer owns the next utterance.
- A silent reply window exits without a second HA intent and releases only dialog-owned focus.
- Continuation reuses the normal endpointer and generation fencing. It is not “VAD fired, therefore start another pipeline.”

A new speaker can explicitly wake or press the button, which creates a new command speaker while retaining a still-valid question context. Nothing restricts by voice who may answer. Without a wake, section 16.6's onset rule and optional EchoMuse reply choices limit capture of background speech; a free-form HA agent question accepts the first qualifying reply. HA currently expires idle chat sessions after 5 minutes and does not persist them across restart [H5], well beyond the 60-second EchoMuse chain cap.

Bound the overall exchange: initially at most five no-wake replies or 60 seconds of chained reply expectation, after which require another wake. Alert background audibility has its shorter independent limit.

### 9.2 Unsolicited follow-up

Do **not** turn every response into an automatically accepted open mic. The archive does not provide Amazon's NTT model, and same-speaker matching is not directedness.

Unsolicited no-wake follow-up is **not enabled or implemented by this specification**. After a response without `continue_conversation=true`, return to wake-word listening. Explicit questions, clarifications, early answers, and a wake that interrupts a still-valid question retain the continuation behavior above. There is no undefined “address/command criterion,” optional NTT classifier, or ambient LLM classification path left for the implementer to invent. General unsolicited natural-turn-taking would require a separately justified directedness model and acceptance corpus; changing the wake model alone would not supply it.

## 10. Timers and alarms (stock Home Assistant)

Timers and alarms use different mechanisms:

- **Timers** follow HA's own voice satellites exactly: HA holds them in memory, and the speaker rings when HA says one has finished (section 10.8).
- **Alarms** have no HA equivalent, so they are events on a stock Local Calendar, durable in HA and cached on the Dot (sections 10.1–10.7).

### 10.1 Alarm store: one Local Calendar per speaker

**Constraint:** everything works on stock Home Assistant. There is no custom integration and no core patch.

HA has no clock-time alarm facility. Its native Assist timers, which EchoMuse uses for timers (section 10.8), are countdowns held only in memory [H3]. The `timer` helper restores active timers and fires `timer.finished` on the bus for a timer that expired during downtime, but ordinary YAML automations can miss that startup event because they may not be listening yet (HA core issue #79145) [H2]. Neither provides clock-time alarms, recurrence, snooze identity, or offline device execution.

Every EchoMuse alarm is an event on a stock **Local Calendar** created per endpoint: “EchoMuse Office”, `calendar.echomuse_office`. It was chosen because it is the one stock store that has all of these:

- **Durable:** persisted by HA and included in HA backups.
- **Recurrence:** repeating events, with timezone-aware expansion by HA's calendar library, and deletion of a single occurrence. Both were verified in memory against the installed library [H6].
- **API:** create, update, delete, and a change subscription over the stock websocket API (`calendar/event/create|update|delete|subscribe`) [H6].
- **Visible:** users see and edit alarms in HA's calendar UI, and automations can use stock calendar triggers.

Rejected stock stores for alarms: timer helpers (countdowns only, the downtime race above, one entity per timer), `input_datetime` (no recurrence), to-do lists (no time semantics), and native Assist timers (countdowns in memory only).

| Part | Owns |
|---|---|
| HA Local Calendar | Which schedules exist, their settings, and which occurrences are still unhandled |
| Controller alert engine | Occurrence lifecycle; voice, LLM, and dashboard operations; device delivery; merging device operations; calendar write order |
| Controller journal (`em_db`) | Write-ahead record of operations and the restore guard. Never the truth of what exists |
| Device | Cached execution while the controller or HA is unreachable |

A calendar write is atomic per event, not across events. Multi-step operations use a fixed write order plus idempotent recovery (section 10.3) instead of a transaction.

### 10.2 Calendar encoding and identity

Each schedule is one event (normative encoding in section 16.3):

- `summary`: human label: “Alarm”, “Wake up”, “Snoozed: Wake up”.
- start and end: local time in HA's timezone; end is start plus 1 minute.
- `rrule`: `FREQ=DAILY`, or `FREQ=WEEKLY;BYDAY=…`, for repeating alarms; absent otherwise.
- `description`: optional human text, then one final line `echomuse: <JSON>` carrying the schedule UUID, kind, creating operation ID, and sound and ring settings.

An event without an `echomuse:` line was created in the HA calendar UI or by an automation. It is an alarm with the default settings of section 16.3. EchoMuse never rewrites such events, except to delete occurrences it has handled.

**Identity.** `schedule_id` is the JSON `id`, or UUIDv5 of the calendar entity and event UID for a UI-created event. The **occurrence key** is the instance start: `recurrence_id` for a repeating event, the start time for a one-shot. `occurrence_id` = UUIDv5(`schedule_id`, occurrence key). Tomorrow's alarm and today's snooze are different occurrences. Dismissing an occurrence never deletes the repeating series.

### 10.3 Handled means deleted

One rule: **an occurrence still on the calendar at or after its time is unhandled; handling it deletes it.** Deleting past occurrences is how handling is recorded in HA. The controller journal records why.

| Operation | Calendar writes, in order | Recovery if interrupted |
|---|---|---|
| Create | create the event carrying the operation ID | the operation ID is already on the calendar: done |
| Dismiss a one-shot | delete the event | not found: done |
| Dismiss a repeating occurrence | delete that instance (UID + `recurrence_id`) | not found: done |
| Snooze (alarms only) | 1. create a child event at the snooze press + `snooze_ms`, carrying the parent occurrence key (section 16.3); 2. delete the parent occurrence | the child exists but the parent occurrence is still present: delete the parent |
| Expire (missed past catch-up, or ring timeout) | delete the occurrence; journal `missed` or `timed_out` | as dismiss |
| Cancel a schedule | delete the event (whole series) | not found: done |
| Edit an alarm | update the event | re-apply |

Ringing state is never written to HA. The device persists first-ring time and the ring deadline (section 16.5).

### 10.4 Time semantics

Timers are HA's and count down in HA's memory (section 10.8); this section covers alarms only.

Alarms are wall-clock schedules. HA's calendar library expands recurrence in HA's configured timezone, and **EchoMuse uses that expansion unchanged**, so the HA calendar and the speaker never disagree. Measured on the installed library for America/Chicago [H6]:

- A repeating 01:30 alarm rings once on the fall-back day, at the first 01:30.
- A repeating 02:30 alarm on the spring-forward day rings at 08:30 UTC, which is 03:30 CDT.

Changing HA's timezone changes future expansion; that is HA's behavior, not a second EchoMuse rule.

**Materialization.** The controller holds one `calendar/event/subscribe` per endpoint for the window [now − 30 min, now + 7 days]. Older unhandled occurrences are found by the backlog reconciliation of section 16.4. It re-subscribes hourly to roll the window, and after every reconnect. Each push is the full list of events in the window; the controller diffs it against the previous list to find created, changed, and deleted occurrences, then updates the device cache. Edits in the HA calendar UI reach the speaker with the next push.

A device with no trustworthy post-reboot wall clock must reconcile before executing absolute deadlines. It marks `clock_untrusted`, surfaces the loss of guarantee, and does not ring arbitrary old cached alarms. A still-running device keeps its previously armed monotonic deadlines during network loss.

### 10.5 Entry points

Every alarm path goes through the same controller alert engine:

- **Voice.** After wake-phrase removal, the final transcript is matched against the alarm grammar (section 16.6). Matches are handled by the controller and never sent to HA's intent stage. Confirmations are spoken with a stock TTS-only pipeline run. An incomplete request (“set an alarm for seven”) creates an EchoMuse reply expectation with choices (“AM or PM?”). Timer commands are not matched here; they go to HA like any other request (section 10.8).
- **LLM.** The satellite advertises `TIMERS`, so HA offers LLM agents its native timer tools for this speaker [H3]. For alarms, the controller installs stock scripts exposed to Assist (section 16.7). Each relays the request to the alert engine and returns the engine's result as the tool response.
- **HA calendar UI and automations.** Add, edit, or delete events on the speaker's calendar. The stock `calendar.create_event` action works for one-shots; it has no recurrence field.
- **EchoMuse dashboard.** The same engine.

Facts reported back to voice, tool responses, and the dashboard:

- `stored_in_ha`: the calendar write succeeded and the event appeared in the subscription.
- `armed_on_endpoint`: the device durably acknowledged the occurrence.
- `delivery_pending`: stored in HA, device not yet armed.

“Saved, but the Office speaker is offline” is an honest answer. Creating an alert while HA is unreachable fails explicitly, with an error cue; there is no controller-only “success”. An HA intent run whose outcome is uncertain is never automatically repeated; report the outcome as unknown.

### 10.6 Delivery, offline execution, and reconciliation

1. The controller applies a calendar write and sees it in the subscription.
2. The controller sends the device ordered deltas, or a snapshot, under its own per-endpoint delivery epoch and sequence.
3. The device writes its cache atomically and durably before acknowledging. A Go implementation can use an atomic snapshot plus a small fsynced operation journal; it need not introduce SQLite on FireOS. One journal transaction holds both parent and child changes for a snooze. Crash-safe snapshot replacement means file sync, atomic rename, directory sync where supported, and replay of only complete checksummed journal records; qualify these on the actual device filesystem.
4. The device schedules local ring work from that cache. Alarms never need a live message to ring; `alert.ring` exists only for timers (section 10.8).
5. On reconnect, the device uploads pending local operations first. The controller applies them to the calendar under the section 10.3 rules, then sends the snapshot or deltas. A local dismissal is never replaced by a stale snapshot.

Merge precedence for operations that originated on the device:

- **Dismiss** of an occurrence no longer on the calendar (someone deleted or edited it) succeeds with nothing to write.
- **Snooze** is adopted only if the parent schedule still exists. Otherwise no child is created, and the device receives a tombstone for its child.
- **Edits made in HA** while the device was offline do not retarget device operations. Those apply to the old occurrence key only; new occurrences come from the new expansion.

While HA is unreachable, the controller keeps writes `pending` in its journal and retries them in order. The device keeps ringing from its cache without HA. It plays a built-in, redistributable fallback tone when the selected asset is missing or corrupt, and never downloads an asset at the deadline. It keeps its speaker executor and deadline scheduler alive under the OS's real power management (section 16.5); a running process alone is not proof of reliable wakeup.

**Partition tradeoff:** a device disconnected after caching an occurrence may ring even if someone deletes that occurrence in HA while it is offline; it cannot know. The dashboard reports the deletion as delivery-pending until the device acknowledges. Do not claim both autonomous offline ringing and instant remote cancellation under partition. Do not reassign an occurrence to another endpoint without fencing the old assignment or explicitly accepting duplicate sound.

Device cache entries carry the 7-day horizon. A device offline beyond it has no guarantee for later repeating alarms. HA is the durable source, not an infinite disconnected replica.

### 10.7 Local stop, snooze, queues, and crash cases

**Dismiss:** fade immediately, append a local terminal operation, persist its tombstone, then acknowledge. If persistence fails, keep the sound stopped in memory but report the failure; do not claim reboot-safe dismissal. Retry the same operation ID to the controller. Dismissing an old occurrence cannot dismiss a later occurrence of the same schedule.

**Snooze:** only for alarms. Persist the parent's terminal snooze and a deterministic child occurrence (identity in section 16.3) atomically on the device before acknowledging an offline snooze. Compute the child deadline once, from the press, and include it in the operation; reconnect must not start a fresh snooze duration. The controller creates the child event with that same deadline and identity. A schedule deleted concurrently wins over future child execution once delivered; the partition caveat still applies. Rejected offline operations are surfaced, not silently rewritten.

**Multiple due alerts:** queue by due time, then occurrence ID; a timer ring joins the queue when its `alert.ring` arrives. One audible full alert at a time. Stop acts on the foreground occurrence captured by the input event; “stop all alarms” is a separate explicit operation. A backgrounded alert remains the current alert for a turn's captured stop/snooze context. Gaps between bursts do not release its identity.

**Recovery policy:** after an HA, controller, or device restart, reconcile occurrences still on the calendar whose time has passed. Allow catch-up ringing within 30 minutes of the due time; older occurrences are expired and deleted as `missed`. An occurrence that was already ringing resumes only within its original ring deadline; a restart never grants another full ring. The ring maximum is 10 minutes per occurrence, configurable per schedule. These are EchoMuse policies, not the native stack's 60-minute timeout. Queued alerts are checked against the catch-up limit again when they reach the head.

**Expire:** at the ring maximum, or when a restart finds an occurrence past its catch-up window or ring deadline, the device stops it, persists an `expire` local operation with reason `timed_out` or `missed`, and uploads it like a dismiss. `alert.ring_ended` is telemetry only.

**Crash boundaries:**

- **Calendar write succeeded, controller crashed before journaling it applied:** on restart the pending operation is re-applied. A create is deduplicated by operation ID; a delete sees not-found.
- **Operation journaled, HA unreachable:** retried in order.
- **Device stopped and persisted before the controller heard:** the local tombstone prevents a re-ring, and the upload repairs the calendar.
- **Snooze interrupted between the child create and the parent delete:** the recovery rule in section 10.3.
- **HA restored from an older backup:** handled occurrences and cancelled schedules reappear on the calendar. The controller journal keeps applied terminal operations for at least 90 days and re-deletes them before they can ring. Anything older than the catch-up window is ignored anyway. If the journal is lost too, the worst case is that a restored occurrence inside the 30-minute catch-up window rings once.
- **Controller database lost:** schedules are intact in HA. Only the restore guard and unsent writes are lost; the device re-uploads its own journal.

### 10.8 Timers: HA's native satellite timers

Timers work exactly as on HA's own voice satellites: Voice PE and other ESPHome satellites, and Wyoming satellites [H3][H7]. HA owns every timer. EchoMuse stores nothing about timers and only rings.

1. **Registration.** The controller's emulated ESPHome satellite advertises `TIMERS`, as it does today. HA then registers its timer handler for the speaker's HA device and offers LLM agents its native timer tools when they talk through that device [H3].
2. **Commands.** HA owns every timer and runs every timer command as one of its own timer intents with the satellite's `device_id`, so the timer attaches to this speaker. Timer requests are ordinary HA turns: the intent→TTS run carries the satellite's `device_id` (section 16.7), and HA's own timer intents, matched locally under `prefer_local_intents`, act on this speaker and speak the result. HA's built-in sentences miss common spoken durations and questions (measured on HA 2026.8.1 [D8]: “5 hour, 15 min”, “an hour and a half”, “how much time is left”, and “what timers do I have” reach the conversation agent, an LLM; “add 5 min to the timer” is taken as a shopping-list item), so the HA configuration adds **custom sentences** (`custom_sentences/en/timers.yaml`, the stock mechanism) for `HassStartTimer` (“set a timer for <duration>”, “set a <duration> timer”), `HassCancelTimer`, `HassIncreaseTimer`, `HassDecreaseTimer`, `HassPauseTimer`, `HassUnpauseTimer`, and `HassTimerStatus`. A duration is numbers as digits or words, units full or short (hour/hr, minute/min, second/sec) in any descending combination, “N and a half <unit>”, “a <unit> and a half”, “half an hour”, or “a quarter of an hour”; a timer is named by the length it started with (“the 5 minute timer”). Their responses say what happened (“Timer set for 5 hours and 15 minutes.”, “5 second timer cancelled.”) and the status answer lists every timer (“You have 2 timers: a 5 minute timer with 3 minutes left and a 10 minute timer with 8 minutes left.”). HA's cancel response cannot say which timer a bare “cancel the timer” cancelled, and its `HassCancelAllTimers` cancels every device's timers, so EchoMuse answers those two itself through HA's stock intent API (`POST /api/intent/handle`): it lists this speaker's timers with `HassTimerStatus`, cancels the only one and names it (“5 second timer cancelled.”), asks **which one** when there are several and cancels the choice, and for “cancel all timers” cancels only this speaker's and names them. A `HassCancelTimer` is sent only with slots under which HA's own matching (name, then start duration, then device) picks exactly the intended timer; a timer HA cannot tell apart is reported, not guessed. A cancel whose request may have reached HA without an answer is never resent: the turn ends `outcome_unknown`.
3. **Countdown.** HA counts down in memory and sends `started`, `updated`, `cancelled`, and `finished` over the ESPHome API. The controller keeps a display copy of each timer (ID, name, total and remaining seconds, active flag) and ticks it down once a second for the LED ring and dashboard, as Voice PE does. The copy never decides that a timer has finished.
4. **Ring.** On `finished`, the controller sends the Dot `alert.ring` (section 16.1) with the HA timer ID and name, the endpoint's effective `timerSound` (section 16.7), no ramp, the loop gap `timerRingGapSeconds`, and the ring limit `timerRingSeconds` (default 900 s, as on Voice PE). The Dot rings through the same alert executor and focus rules as alarms (section 6.2): it loops the sound, pauses content, is backgrounded while a turn is open, and returns to the foreground afterwards.
5. **Stop.** “Ophelia, stop” or “Ophelia, stop the timer” (section 16.2), the physical button, or the “Stop alert” entity ends the ring, and so does the ring limit. The Dot reports `alert.ring_ended`. Nothing is written back to HA, which already dropped the timer when it finished. Timers cannot be snoozed.

Like HA's own satellites, and deliberately so:

- **HA restart:** running timers are lost silently and do not ring.
- **Link down at the finish:** if HA cannot reach the controller when a timer finishes, the ring is lost; HA does not resend it. If the controller has the event but cannot reach the Dot, the ring is also lost and logged as `timer_ring_undeliverable`. When a display copy reaches zero and no `finished` arrives within 5 s, the controller logs `timer_finish_missed` for the dashboard. It never rings from its own countdown.
- **Controller restart:** timers survive, because HA holds them and delivers `finished` to the handler registered for the device, as long as the satellite has reconnected by then. HA does not resend running timers on reconnect, so the display copy stays empty until the next timer event.
- **Dot restart:** a ringing timer stops; timer rings are not persisted.
- **Timers with an action** (“in 10 minutes, turn off the lights”) run a conversation command inside HA and never ring [H3].

## 11. Protocol, feedback, and privacy

### 11.1 Capability cutover

Negotiate these exact capability names:

```text
audio_timeline_v1
uplink_leases_v1
device_wake_v1
render_reference_v1
render_progress_v1
focus_leases_v1
alert_cache_v1
turn_protocol_v1
```

These names are the protocol contract. Features depend on capability sets, not firmware-version comparisons. `uplink_leases_v1` covers the rings, cell records, and leases (section 4.4). `device_wake_v1` means the firmware implements the wake detector. A device that cannot load the graph the controller named reports `wake_unavailable` with its reason in `wake.stats` (section 16.1), which the dashboard shows; it can still start turns by button. A deployment cannot enable the new reliable-alert mode on an old device and silently approximate it with legacy fire-and-forget playback. Show missing capabilities and require upgrade. Use one protocol implementation after the fleet-wide cutover (section 12), not permanent old/new watchers both capable of taking action.

All commands include an owner/generation, command ID, deadline where applicable, and expected object revision. Acknowledgements distinguish accepted, applied, durably stored, and audibly completed. Authenticate endpoints; scope stop/snooze to authorized occurrence targets. Do not treat LAN membership or a spoken wake word as authorization for sensitive HA actions.

### 11.2 LEDs and earcons

Render LEDs as a projection of explicit local states, not as a separate turn owner:

- mic muted: unmistakable device-sovereign privacy indication;
- listening, thinking, speaking, expected reply;
- alert foreground versus background/pending;
- the first active timer's remaining fraction, from the controller's display copy (section 10.8);
- offline/degraded delivery state when user interaction makes it relevant.

Use owned assets and animations. A fade/spin animation cannot itself expire a turn or dismiss an occurrence. Remote dialog indicators have leases and clear on controller loss; local alert/mute indication survives it. Earcons participate in the reference stream and must not create a blind capture interval. Accessibility options can disable confirmation sounds without changing turn logic.

**Wake chime.** With `wakeSound` on, the built-in `earcon` `builtin:wake_chime` confirms a wake with no command context (a wake that stops an alert or cancels a reply has its stopping as the acknowledgement, section 16.2). A device announcing the optional `local_wake_chime` capability receives `wakeSound` in `config` and plays the chime itself the moment it opens a candidate, when it has a controller session, is not producing sound, has no ringing or backgrounded alert, and holds no `diagnostic` lease; `wake.candidate.chimed` reports that it did. The chime is not cancelled when the controller rejects the candidate: a truncated chime is worse than a whole one, and a cancel cannot retract the audio already queued in the sink. Every other accepted wake — older firmware, or a wake heard while the device produces sound, which the controller verifies first (section 6.1) — gets the chime from the controller (`render.start`) after acceptance, unless `chimed` is true. The device-local chime is mixed like any earcon (reference stream and active-source mask included) at the earcon slot's current generation without raising its fence, and is never reported in `render.progress` or `render.finished`.

### 11.3 Evidence records, not permanent room recording

Keep a structured decision trace sufficient to explain a failure:

```text
turn/utterance/occurrence IDs; epochs and sample ranges
model/policy revisions, device asset hashes
wake candidate (profile, threshold, hop scores, producing_sound, chimed) + accepted/rejected/ambiguous + reason
uplink leases: reason, streams, backfill clipping, end reason
reference coverage and timing uncertainty
endpoint candidate/revocation/commit + evidence availability
focus acquire/apply/release; playback completion reason
alert stored/armed/due/ringing/terminal + revision
transport gap, queue age, worker latency, clock health
```

The actor writes this trace as compact JSON with the turn's row (`turns.decision_trace`), alongside `first_audio_ms`: monotonic time from handling the endpoint commit to the response becoming audible, NULL when nothing became audible. Only each device's newest 1,000 rows keep their trace; older rows keep every other column. The turn list the dashboard reads never carries the trace.

Device rings and controller lease timelines remain RAM-only by default and expire promptly. No microphone audio leaves the device without an uplink lease: a wake candidate, a button turn, a reply expectation, or a dashboard diagnostic mode, which the dashboard shows while it runs. Audio of a rejected candidate is used only to decide the candidate and is then discarded. Raw clips/transcripts for tuning require explicit opt-in, bounded retention, and access control. No speech is sent to HA or to a cloud agent without an accepted wake, a button press, or an explicit reply expectation. Hardware privacy mute ends every lease and clears all speech history and anchors, including device rings and buffered unsent audio.

## 12. Implementation cutover

These are implementation work packages for the complete design, not claims that a scaffold meets the requirements:

1. **Device boundary:** continuous native capture, epoch/sample headers, rings, cell loudness, uplink leases and backfill, bounded transport, unified mixer/reference tap, output progress, physical mute/button handling, focus leases and the provisional duck, kernel wakelock, and local alert cache/executor.
2. **Device wake detector:** speech-asset download and install, the BCResNet scorer on the `wake` thread with the deployed contract and candidate rules, near-miss and overrun statistics.
3. **Speech worker:** per-lease timelines, VAD, on-demand reference scoring, echo comparison and attribution, wake verification, pinned streaming ASR, endpoint reducer, and immutable committed spans.
4. **Session actor:** one event reducer, generation fencing, candidate acceptance and lease control, interruption policy, expected-reply handling, structured decision trace, and the stock HA pipeline runs (section 16.7).
5. **Alert engine and HA provisioning:** calendar encoding, write ordering and merge, journal and restore guard, device delivery, timer display copies and `alert.ring`, LLM alarm tool scripts, calendar and script provisioning through stock APIs, and explicit stored/armed acknowledgements.
6. **Qualification and cutover:** run section 13; compare old/new on the same opt-in recordings. A shadow path may record decisions but must never issue playback or HA actions.

**Cutover is fleet-wide.** One controller release replaces every REMOVED and REPLACED component of section 18 at once. It keeps exactly one piece of the old protocol: an **upgrade-only legacy handler** on the old `/control` path. That handler accepts an old device's registration, lists it on the dashboard as `upgrade_required`, answers `shell_open`, and runs the existing firmware update over the retained `/shell` plane (`POST /api/devices/{id}/update`). It sends an old device no turns, audio, alerts, or configuration, and it never acts on anything the device reports. The handler is deleted in the next controller release, once no registered device has connected through it for 30 days.

Order:

1. Build and qualify the v1 firmware and controller together (section 13), with the old controller still in service.
2. Deploy the controller. It provisions each endpoint's Local Calendar and the five alarm scripts, sets each satellite's VAD sensitivity to `relaxed` (section 16.7), pins the deployed BCResNet in the registry (section 5.1), and runs the section 18.4 migration. Voice and alerts are unavailable on each Dot from this moment until that Dot is upgraded.
3. Upgrade every Dot through the legacy handler. On first v1 start a Dot downloads its speech assets, loads the wake graph, and arms its alarm cache from the calendar. Timers need no migration: the satellite keeps advertising `TIMERS`, and timers already running in HA ring through the new path once their Dot is upgraded.

The old and new paths never both act on one device. Rolling back means reinstalling the previous controller image and the previous firmware slot; the alarm calendars and journal stay, but the previous controller ignores them.

Do not combine this with a compiler/NDK base-image change. The FireOS build pin remains unchanged. Do not change the AFE signal-processing configuration as a shortcut around endpoint mistakes.

## 13. Qualification and acceptance

### 13.1 Release gates

These are acceptance thresholds for implementation, **not measured results**. Numerical policy values may be recalibrated to meet them; the architecture and message contracts stay fixed.

- Candidate-to-provisional-duck p95 ≤50 ms on the device, and wake-to-audible-duck p95 ≤250 ms **from accepted candidate**, with wake-acoustic-end-to-candidate latency reported separately.
- Device wake scorer: zero `wake_overrun` and no capture discontinuity attributable to CPU over 24 h mixing idle, music, TTS, and alarms; maximum inference below 160 ms on every Dot in the fleet.
- Candidate lease: backfill received by the controller p95 ≤250 ms after candidate open; verification decided within its 700 ms deadline at p99.
- Outside uplink leases, a Dot uploads zero microphone, reference, or cell bytes over 24 h (device transport counters).
- Successful wake barge-in ≥95% on the section 13.3 held-out music and alarm wakes; report quiet/far-field and double-talk strata separately.
- Playback-profile false activations ≤0.2/h from non-self room audio (TV, conversation) during content playback and alert rings at 0.65, counted after verification.
- Wake verification: genuine wakes spoken over TTS, content, and alerts pass ≥95%, with far-field and loud-playback strata reported. Self-output candidates, including answer openings that score on the wake model like the observed “Eight” and “I need to”, pass 0. Calibrate the 0.40 alias distance on these recordings, not on the 23 clips that chose it. Failing recall at every workable distance is the section 5.4 retraining trigger; failing rejection is an attribution defect.
- No accepted self-wakes in at least 24 hours of owned-output replay; report exposure hours and a confidence bound, not “zero false-wake probability.”
- Complete-command end-to-commit p95 ≤1.2 s in quiet/non-speech noise. For grammar-covered commands followed by background TV/radio speech at least 10 dB below the command, p95 ≤1.8 s through route B. For free-form requests followed by such background speech, report route A end-to-commit latency and the clipped-command rate of that stratum; it counts toward the clipped-command gate below. For free-form requests under equal-level continuous TV, report commit-after-real-pause, `retry`, and `too_long` rates separately; merged action text is a failure. Retry rate is always reported beside latency so rejecting everything cannot pass.
- Clipped-command rate <1% on the section 13.3 endpoint set, with explicit long-pause/accessibility subsets.
- Initial contaminated/false-action acceptance target <0.1% on negative-turn cases, with enough examples to bound it meaningfully. Safety-sensitive HA actions retain their own confirmation policy.
- Device mute and physical stop work without HA/controller; local sound-stop target ≤100 ms from the physical event.
- Warm-cache due-to-audible-alarm p95 ≤250 ms while the device is running and its clock is trusted. Reboot/wakeup behavior is measured separately.
- Timer ring: HA `finished` event received by the controller to audible ring p95 ≤300 ms.
- No duplicate live occurrence player under replay/reconnect/crash injection. Delivery is at-least-once; the occurrence state machine provides idempotent execution, not a fictional exactly-once network.

If worker throughput or LAN jitter cannot meet these targets, expose the limitation and change deployment capacity based on measurements. Do not mask delay by discarding pre-roll or weakening wake precision. Placement is fixed by the measured per-model costs (section 8.1): the Dot runs only BCResNet and loudness.

### 13.2 Required scenario matrix

| Scenario | Required observation |
|---|---|
| Wake plus command with no pause | First command phoneme retained; wake normalization/removal does not cut the command. |
| Wake in first alarm burst, middle of burst, and gap | Continuous detection; same occurrence stays active/backgrounded; no detector warm-up per burst. |
| Wake over music / song contains wake word | Human interruption works; self-output candidate is attributed; both latency and false rejects recorded. |
| Quiet background music, TV talking, nobody addresses the device | Playback-profile candidates from TV speech fail verification; activations stay within the false-activation gate. |
| Answer opens with words the wake model mistakes for the wake word (observed: “Eight…”, “I need to…”) | Candidate rejected as `unverified_wake`; the answer continues after at most the provisional duck. |
| TTS says wake word; human says it simultaneously | No blanket mute window; ambiguous double-talk measured separately; cannot count all suppressions as successes. |
| Music source outside supervisor | Partial reference coverage shown; no unsupported full-coverage claim. |
| User stops with fan/noise continuing | Endpoint commits without waiting for acoustic zero. |
| User stops with TV continuing; TV starts during pause | Target clock does not follow TV indefinitely; clean target command or explicit retry, not merged action text. |
| Long request, then a second person talks immediately at similar level | One utterance, ending at the next real pause or `too_long`; no speaker model runs. Measured to size the backlog route (section 19.1). |
| “Turn off the kitchen” where “kitchen lights” also exists | `extendable` waits 1,216 ms before commit; completing “lights” within that pause is one command. |
| “Set a timer for … five minutes” | Incomplete-slot pause survives; no action at provisional endpoint. |
| User resumes just before/after endpoint commit | Before: candidate revoked. After: no duplicated/retroactively changed action; next-turn policy explicit. |
| Prompt finishes, immediate one-word answer | Deadline uses actual completion; pre-roll retains answer; no echo of prompt accepted. |
| Early answer during question / different household member | Qualified early interruption works; new speaker can explicitly wake; no identity-based authorization. |
| Wake + “stop”; bare “stop” without the wake word; TTS saying “stop”; wake + “don't stop”; wake + “snooze” on a timer | Only wake-prefixed complete local commands act locally; bare “stop” is ignored; “don't stop” goes to HA; the timer is not snoozed. |
| Wake during a ring, then silence | Alert backgrounds during the turn, returns to foreground at `no_input`; not dismissed. |
| Two alarms due; delayed stop result | Exactly the captured occurrence stops; next alert not accidentally dismissed. |
| HA/controller restart before an alarm is due, during its ring, during its snooze | Calendar events survive; the armed device executes; the same occurrence and child IDs are retained. |
| Delete or retime an alarm in HA's calendar UI while the device is online, and while it is offline | Online: the next push tombstones the old occurrence and the speaker follows. Offline: delivery pending, with the stale-ring risk explicit. |
| Alarm created in HA's calendar UI (no `echomuse:` line), repeating weekdays | Rings with the section 16.3 defaults on the right local days; dismissing it deletes only that day's occurrence. |
| HA restored from a backup taken before an alarm was dismissed | The restore guard re-deletes the handled occurrence; it does not ring. |
| Automation calls `assist_satellite.start_conversation` | Prompt plays; the reply goes through the ESPHome path with the automation's prompt applied; a reply with a 0.9 s internal pause is one utterance, and a 1.1 s pause ends it before HA's segmenter would. |
| Claude asked “wake me at 6:30 on weekdays” | Uses `script.echomuse_set_alarm`; the event appears on the speaker's calendar; the tool response reports stored and armed. |
| Timer set by voice (“Ophelia, set a pasta timer for 8 minutes”) and by Claude | HA's native timer on this speaker; HA's reply (“Timer set for 8 minutes”) plays in full; the dashboard and LEDs show the countdown; `finished` rings the Dot; “Ophelia, stop the timer” or the button ends it; nothing is written to HA or the calendar. |
| “Set a timer for 5 hour, 15 min”; “how much time is left?”; “cancel the timer” with one and with two running | HA's local timer intents set the timer (“Timer set for 5 hours and 15 minutes.”) and answer the question with every timer's length and time left, without a conversation agent. With one timer, EchoMuse cancels it and says “5 second timer cancelled.”; with two it asks “Which one? Your 5 minute timer or your 10 minute timer?” and a reply without the wake word (“the 10 minute one”) cancels exactly that timer. |
| HA restarts while a timer is running | The timer is lost and does not ring, as on HA's own satellites. |
| Controller restarts and reconnects before a timer ends | The timer still rings; the display copy is empty until the next timer event. |
| Timer ends while the Dot is disconnected from the controller | No ring; `timer_ring_undeliverable` logged. |
| Wake phrase before, inside, and after the command; STT drops or misspells it | Only the leading phrase and speech before it are removed; no audio trimmed; unmatched transcripts pass unchanged. |
| Ack loss and duplicate delivery | Same schedule/operation is acknowledged, not duplicated. |
| Device reboot after local dismissal | Persisted terminal state prevents re-ring. |
| Disk full, corrupt asset, clock rollback, untrusted boot clock | No false durable-success acknowledgement; fallback asset or explicit degraded outcome; no duplicate occurrence. |
| DST fold/gap and timezone change for alarms | Specified local-time semantics, not 24-hour arithmetic. |
| Mic gap, stale ASR result, old EOS after barge-in | No false silence; generation fencing rejects stale work. |
| Privacy mute during a turn; controller dies | Capture/buffers clear; voice actions cease; scheduled alerts and physical stop remain available. |
| Wake candidate over music while the controller is unreachable | The device ducks provisionally, then restores the music when the candidate lease's TTL expires; no turn, no chime. |
| A Dot's wake graph is missing or fails to load | `wake.stats` reports `wake_unavailable` with the reason, shown on the dashboard; no scoring; the button still starts turns. |
| Two leases overlap: a wake during a reply expectation | Each sample is uploaded at least once and used once; the new candidate is decided normally. |
| Claude calls each of list, cancel, dismiss, and snooze alarm scripts | Correct response object; no duplicate on retry; missing speaker with two active turns returns “which speaker?” |
| “Stop alert” entity, physical button, and wake-prefixed stop | All stop only the captured foreground occurrence; alarm stop persists operation; timer stop does not write HA |
| Alarm reaches ring limit while HA/controller are offline | Device persists `expire(timed_out)` before reboot; it does not re-ring; reconnect deletes the calendar occurrence |
| Timer and alarm become due together | One foreground ring at a time, sorted by due then occurrence ID; stop targets the captured head; the next proceeds |
| Offline snooze, then parent schedule is deleted in HA | Reconnect rejects the child and delivers its tombstone; child never rings |
| Snapshot spans 129 occurrences, lost delta, and a new controller epoch | Canonical snapshot hash validates, device requests snapshot on sequence gap, protected local tombstones survive installation |
| Diagnostic lease and button during it | Dashboard visibly marks capture; no wake is accepted; the button and due alerts still work; lease closes on mode end |
| Idle day with music, TV, and conversation | Near misses counted on the device; no microphone audio uploaded outside candidate leases. |

Use acoustic playback/capture measurements on the actual Dot for reference coverage, AEC behavior, output latency, and mute correctness. Host inference and state-machine replay cannot prove those properties. Use deterministic event/crash traces for alert and turn invariants; those traces must include crash points before and after durable writes, not only clean restarts.

### 13.3 Qualification corpus and calibration

`qualification/manifest-v1.json` is committed before implementation release. It lists every WAV by SHA-256, the device/sample rate/channel, its transcript, wake interval, playback class, room/noise stratum, and whether it is a positive, self-output, room-negative, or endpoint case. The first release corpus contains:

| Set | Minimum | Required strata |
|---|---:|---|
| Genuine wakes | 800 | 200 each: idle, TTS, controller-decoded music, alarm; at least 3 speakers and quiet/far-field labels |
| Playback room negatives | 24 h per class | TTS, music, and alarm, with TV/conversation/no-address spans |
| Owned-output replay | 40 h (so the 60% held-out split exceeds the 24 h gate) | TTS openings, wake-word-containing responses, alert assets, music vocals |
| Endpoint commands | 300 | complete, extendable, needs-more, free-form, TV at −10 dB, TV at equal level, fan/noise, two speakers |
| Continuation replies | 100 | immediate one-word, early barge-in, no-audio expectation, choices, rejected choices |
| Alert traces | 1 trace per alarm or timer row in section 13.2 | normal and every named crash/partition boundary |

No clip appears in both calibration and held-out acceptance. `qualification/split-v1.json` fixes the split by SHA-256: 40% calibration, 60% held-out, stratified by the table. A recording with no explicit consent cannot enter either list.

**Calibration procedure.**

1. Run the immutable architecture, deployed graph, and exact candidate/endpoint protocol on calibration clips only.
2. Sweep `idle`, `playback`, and `reference` thresholds independently in 0.01 increments and wake-verification distance in 0.01 increments. Reject a tuple unless, on the calibration split, it satisfies every false-activation and self-output gate **and** every recall gate: ≥95% playback barge-in, ≥95% verification pass for genuine wakes over playback, and idle recall no lower than the deployed 0.90 baseline.
3. Among remaining tuples, choose the highest idle threshold, then the highest playback threshold, then the lowest verification distance, then lexicographically lowest tuple. This deterministic tie-break prefers rejection without silently weakening barge-in.
4. Fix the selected policy hash, run it once on the held-out split, and report every section 13.1 measure with a two-sided 95% Wilson interval for rates. A gate with a minimum count is unmet until that count exists.
5. Any held-out release gate failure blocks release. Only numerical policy values may return to step 2; a failure never changes ownership, transport, or message semantics. A failed playback gate may trigger the section 5.4 fine-tune project, but does not authorize a special case.

**Shadow evidence for an extra open rule.** Calibration above sweeps thresholds on the corpus; an extra open rule (section 5.2) is evaluated in the field first, as a shadow rule on the speakers it would run on, and read from the Status tab's shadow table (7 days by default; the API takes up to 180):

- **Exposure** is the hours evaluated: scored hops of the rule's profile × 160 ms. It is the denominator of every rate.
- **False activations:** `unmatched` episodes are would-be false wakes. In the idle profile they are directly comparable with section 5.4's idle ceiling of 0.2/h, because an idle candidate is accepted on the open rule alone; in the playback profile attribution and verification would still reject some, so `unmatched`/h overstates the playback rate. Compare the **one-sided 95% Poisson upper bound** of `unmatched`/h with 0.2/h, never the point estimate: a rule qualifies only when that bound is ≤ 0.2/h. With no `unmatched` at all the bound is 2.996/T, so qualifying needs at least 15 idle hours; one `unmatched` needs about 24.
- **Rescued misses:** `retried` episodes are likely real wakes the live rules missed and the user repeated within 10 s. They are the recall the rule would add, not false activations, but they are inferred from timing: confirm them from their events (raw scores and peak) before counting them as gain.
- **Recall loss:** `live_only` counts live candidates of the rule's profile that the rule on its own would have missed. With the baseline always in the open list this costs nothing live, but it bounds the benefit: `matched_earlier` out of `matched` + `live_only` is the share of the profile's live wakes the rule would actually speed up.
- **Latency gain:** `matched_earlier` and the lead histogram show how often and by how much (in 160 ms hops) the rule would have opened before the live rules on the same wake. `matched` counts overlap with live candidates, which are not all genuine wakes.

A rule enabled on this evidence is a new policy value: record it with the evidence window, keep it as a shadow rule elsewhere, and remove it if the section 13.1 gates fail.

## 14. Evidence collected for this specification

### What was actually checked

- Revisited the native research in `alexa-turn-control.md`, `alexa-alerts-and-leds.md`, and `alexa-endpointing.md`; subagents verified archived turn and alert control flow against decompiled code [N1–N5].
- **Deployed wake model [D1]:** read the controller's active graph/sidecar from the running `echomuse-controller` container and verified SHA-256, MD5, and size against the controller's asset API. Live logs show controller-side wake decisions and near misses at threshold 0.90; the Office Dot is in shadow mode, so its stale device copy does not gate wakes. Graph inspection found the IO, opset, in-graph STFT, label count, and 1.5× wider stage widths recorded in section 5.1. Independent local execution reproduced IO and non-wake scores (maximum smoothed 0.0043 on a LibriSpeech-derived utterance, 2.66 ms/window).
- **Device runtime prerequisites [D1]:** the EchoMuse server runs as UID 0 with supplementary group 1000; `/sys/power/wake_lock` and `/wake_unlock` exist; `/proc/sys/kernel/random/boot_id` is readable; `/data/local/etc/echomuse` is on the `/data` ext4 mount (`data=ordered`, `commit=1`). No lock was acquired and no persistence write was made.
- **Speech bundle [P1]:** installed the pinned wheel and models in an isolated `/tmp` Python 3.12 environment; exercised streaming decode, JSON trailing blanks, finalization flags, endpoint API, raw stateful Silero probabilities, and speaker extraction on distributed sample audio, not household recordings. A first probe script failed to carry VAD state and was corrected; results above use the corrected stateful run.
- **Playback wake scores [D2]:** parsed the running controller's `docker logs` (50.7 h, 2026-09-20 21:51 to 2026-09-23 00:35 UTC) and the `turns` table. The barge watcher records the score of the hop that fired, not the peak (`_barge_watcher` returns on firing).
- **Barge-in clips and wake verification [D3]:** rescored the saved clips for barge-in turns 421 and 426 on the deployed model: newest-window probability 0.71 and 0.33 at the firing hop. Transcribed them and all saved wake clips with the pinned Kroko model at the +20 dB evidence level. Both barge clips transcribe as the answer's own words, and the turn history shows the user repeating the interrupted question. The fuzzy alias test (section 16.6) passed 23 of the 24 idle wakes from turns 396–427. The one failure (turn 424, “After”) captured no command and produced no response, so it was probably a false wake [INFERENCE]. The test failed all 4 barge-in clips (turns 322, 356, 421, 426). Each test used the 1.44 s clip ending at detection, without the 480 ms lookahead the contract adds. Ring-stop wakes save no clip and remain unverified.
- **Speech models on real Dot audio [P2]:** ran candidates on the 10 retained `recordings/` WAVs (post-gain STT copies; 7 with HA faster-whisper transcripts used as references) and the 148 retained wake clips. The data is one room and one speaker, whisper transcripts stand in for ground truth, and the 240 ms preroll discard clips some first words. Greedy WER at raw / +20 dB (STT copy) / +30 dB: pinned 20M zipformer 100% / 72% / 69%; zipformer 2023-06-26 100% / 63% / 44%; NeMo fast-conformer 80 ms 72% / 31% / 22%; NeMo 480 ms 94% / 25% / 25%; Kroko 2025-08-06 9.4% at all three. Silero v5 beat v4 at every level; TEN VAD barely fired through sherpa's segment wrapper and was not pursued. No model was trained or fine-tuned.
- **Silero v5 cost:** 0.10 ms per 32 ms cell (real-time factor 0.0033, one thread) over the concatenated Dot recordings at the evidence-copy level.
- **Home Assistant [H2–H6]:** inspected installed HA 2026.8.1/Python 3.14.6 source and registry/configuration metadata read-only: conversation, pipeline, STT, TTS, websocket, LLM API, intent timers, ESPHome satellite feature gating and pipeline path, calendar websocket API and Local Calendar setup flow, script tools, the Office device registry entry, its selected pipeline, and LLM API usage. Validated the `assist_pipeline/run` schema in the container for STT-only with `no_vad`, intent→TTS, and TTS-only messages. Exercised the installed calendar library in memory for recurrence across both 2027 DST transitions and for single-occurrence deletion. No HA action, configuration mutation, or credential read was performed.
- Executed the repository `bcresnet_audio.onnx` and compared its audio-in graph against `bcresnet.onnx` on silence, seeded noise, and a 440 Hz tone (maximum logit differences 0, 4.77e-7, and 8.17e-4). That fixture is not the deployed model.
- **Wake-phrase removal [D4]:** replayed the section 16.6 rule on the streaming transcripts of 23 genuine wake clips, 233 HA transcripts and 18 streaming transcripts without the wake word, and constructed cases. A first draft that matched `ophel` as a substring anywhere stripped “the lights” from “Turn off the lights.”; the whole-window rule fixes that.
- **Wake-phrase miss [D7]:** turn 433 (“Ophelia, what's the weather?”, idle wake) streamed as “What's the weather?”, so step 2 found nothing and the unstripped HA transcript went to intent. HA's `conversation/agent/homeassistant/debug` matches the stripped text to a sentence trigger and not the unstripped one; with `prefer_local_intents` the turn went to the conversation agent (3.5 s intent). The saved STT copy equals the evidence copy (+20 dB, `nsAsr` off); feeding it to the pinned Kroko constructor in 80 ms blocks reproduced the transcript. The wake word is −34 dBFS there, 14 dB above the pre-roll floor. Moving the stream start later in 20 ms steps over 0–380 ms located the phrase in 9 of 20 starts (11 at +6 dB, 10 at +10 dB); a fresh decode from support start − 300 ms reads “Ophilia”. Step 2 is alignment-sensitive for quiet wake words, which the position-0 fallback covers.
- **ASP playback notifications [D5]:** a subagent disassembled `libasp.so`'s command dispatcher and toggled codes 1 and 9 on the live device reversibly, without audible playback or capture. Final `dumpsys` matched baseline.
- Computed the specified reference FIR: flat through 6 kHz (−0.011 dB at 1 kHz), −6.0 dB at 7.2 kHz, −53.7 dB at 8 kHz, and −70.6 dB at 10 kHz before decimation to 16 kHz.
- Executed the section 16.3 journal DDL in memory. It rejects a duplicate operation ID, an unknown action, a dismiss without an occurrence key, `applied` without `applied_ms` and the reverse, and an acknowledged sequence ahead of the delivery sequence.
- **Other satellites' timers [H7]:** read the installed HA intent timer manager, HA's ESPHome and Wyoming satellite timer handlers, ESPHome's device-side `voice_assistant` timer code, the Voice PE firmware configuration, and wyoming-satellite's timer handling. All of them keep timers in HA memory; the device counts down for display only and rings on HA's `finished` event.
- **On-device model cost [D6]:** a subagent cross-compiled a generic ONNX Runtime benchmark with the firmware's NDK and ran it on the Office Dot in `/data/local/tmp`, with the firmware's own `libonnxruntime.so` (1.19.2, bit-identical to the official armeabi-v7a release) and session options (one thread, spinning off, sequential), each run at `oom_score_adj` 1000. The deployed BCResNet at the 160 ms cadence for 60 s used 43.1% of one core with XNNPACK (82 ms mean, 100 ms p95, 131 ms max) and 49.7% on the CPU provider; the stale shadow model used 33.9%. Silero v5, CAM++, and the Kroko encoder crash with SIGBUS in `CreateSession` at every enabled graph-optimization level, on 1.19.2 and on 1.20.0, the newest official Android build that loads on API 22. With optimization disabled they took 6.5 ms per cell (20 calls), 1,062 ms per window, and 1,133 ms per 1.28 s encoder chunk. The same harness on the controller host is about 30× faster for BCResNet. The benchmark was removed afterwards; the device stayed connected with zero dropped frames. Read-only device probes found 4 Cortex-A53 cores with 2 online, 483 MB RAM with 277 MB available, 35% average CPU over the two online cores, and 127 MB/h of microphone upload per Dot (the controller's `device_metrics` table).

### Measurement gates, not open design questions

All architectural choices are closed in section 17. The following require the implemented system on the actual Dot and are release gates:

- native-AFE echo residual, render/capture delay distribution, and 150 ms drain-guard adequacy;
- BCResNet playback/idle operating points (0.65 for content and alerts), wake-verification recall over playback and its 0.40 alias distance, and self-wake rejection on device-channel continuous recordings, including content playback that has never been measured;
- route B level separation on real room audio;
- deployed controller concurrency, LAN jitter, candidate backfill latency, device scorer overruns over 24 h, and per-stage latency against section 13.1;
- kernel wakelock suspend prevention, device journal durability, and restart/partition traces;
- HA 2026.8.1 stock-API compatibility tests for every command, entity, and flow named in section 16.7, including that a timer set through an intent→TTS websocket run carrying the satellite's `device_id` reaches that satellite's timer handler.

The architecture does not depend on Amazon's unavailable NTT/STT/endpointer models, native per-frame AFE metadata, or any newly trained model. It uses the deployed BCResNet plus off-the-shelf pretrained speech models.

## 15. Source map

Native anchors below are relative to the local archive:
`~/.local/share/echomuse-native/biscuit-2022-11-18/`.
Its `MANIFEST.md` records source APK/dex hashes. Paths are provenance references, not repo dependencies.

- **[N1] Native suppression:** `dex/out59/sources/amazon/speech/wakewordservice/EnumeratedPolicy.java`, especially `processResultFromInput` lines 141–165, `onInputSuppressed`, and speech-mark ingestion. Related research: [native turn control](alexa-turn-control.md), “Self-wake suppression using TTS word timings.”
- **[N2] Native detector/NTT interfaces:** [native turn control](alexa-turn-control.md), “The second decoder,” “Continuation: Natural Turn Taking,” and “The genuinely reusable thing is not code.” `lib/libwakewordserver_jni.so`, `lib/libpryon.so`, `skel/{api,params,classes,src}.txt`; shipped configs under `cfg/`. Interface/class presence is not proof of active runtime use.
- **[N3] Native dialog and TTS:** `dex/out171/sources/amazon/speech/sim/state/{ReadyState,ListenState,MultiTurnRequestState}.java`; `.../receivers/ListenReceiver.java`; `dex/out62/sources/amazon/speech/simclient/TtsMediaPlayer.java` lines 527–630 and 775–838; `TtsSpeechMarksEmitter.java` lines 54–116. Focus: `dex/out118/sources/amazon/speech/simclient/focus/{Channel,ChannelSet,AudioChannelSet}.java`.
- **[N4] Endpoint capabilities:** [native endpointing research](alexa-endpointing.md), “The five endpointing modes,” “The decision objects,” “The vote-queue VAD,” and “Backporch”; archive `skel/params.txt`, `skel/classes.txt`, `skel/src.txt`. Interpret performance/active-mode claims using section 2's qualifications.
- **[N5] Alert runtime:** [alerts and LEDs research](alexa-alerts-and-leds.md); `dex/hb_out4/sources/amazon/alexa/alerts/engine/AlarmsEngine.java` lines 108–169, 250–301, 461–495; `.../fsm/AlertsStateMachine.java` lines 217–286; `.../audioplayer/AssetAudioPlayerManager.java` lines 57–108; `.../recurrence/NextInstanceScheduler.java` lines 32–74; `dex/hb_out74/sources/com/amazon/headlessbeacon/service/HeadlessRingingService.java` lines 280–343, 516–580, 653–713.
- **[R1] Existing hardware boundary, descriptive only:** [`slmic.go`](../device/internal/bindings/slmic/slmic.go) capture constants/setup; [`slspeaker.go`](../device/internal/bindings/slspeaker/slspeaker.go) render constants/mixer; [`data.go`](../device/internal/client/data.go) audio frame kinds. [Native AFE migration](native-afe-migration.md) gives historical context, not proof of measured AEC performance.
- **[M1] Model trainer contract:** `~/git/bcresnet/scripts/train_qc.py`: `LogMel`, `AudioIn`, `export_onnx_audio`, `write_audio_sidecar`, `verify_onnx_audio_clips`; `scripts/eval.py` normalization; `configs/ohphelia.yaml` (`tau: 2`).
- **[D1] Live deployed model/device probe, 2026-09-22 23:55 UTC:** controller `echomuse-controller`, `/app/data/oww_models/bcresnet_audio.{onnx,json}` (SHA-256 `4eb74512…`, `25da0c65…`), `GET /api/devices` and `/api/devices/G090LF10728426PR/oww_assets`, controller wake/near-miss logs, device `/proc/1240/status`, `/sys/power`, and mounts. Local evidence copy: `/tmp/dev-model-evidence/`. Scorer semantics: `controller/em_wake_scorer.py` `BcresnetScorer.push/_score`, `SMOOTHING_WINDOWS`, `SILENCE_RMS`, `build(hop_chunks=2)`.
- **[P1] Speech and model probes:** `/tmp/speech-bundle-probe/speech_bundle_probe_report.json`; throwaway scripts `/tmp/post_afe_probe/{probe_speech_contract,probe_refined,probe_vad_polarity}.py`.
- **[D2] Live playback-listening evidence:** `docker logs --timestamps echomuse-controller` lines `Barge watcher done … peak=`, `Barge-in: wake word during playback`, `Ring listening … score=`, `Wake word stopped the timer ring`, `OWW near-miss: score=`, and `Wake word detected (… threshold=0.100 …)`; `turns` rows 419–427. Behavior of the recorded value: `controller/em_controller.py` `_barge_watcher` (returns on the firing hop).
- **[D3] Barge-in clip analysis:** `data/wakes/G090LF10728426PR/{421,426,322,356}.wav` and all saved wake clips; `turns.stt_text` rows 419–426; deployed graph `/tmp/dev-model-evidence/bcresnet_audio.onnx`; Kroko model from [P2]. Analysis run in the session evaluation kernel; the alias test is `lev(substring, "ophel")/5 ≤ 0.40` over substrings of length 4–6.
- **[P2] Device-audio speech-model evaluation:** `/tmp/post_afe_probe/probe_device_audio.py`, `/tmp/asr-bakeoff/scripts/{bakeoff_asr,bakeoff_vad}.py`, results `/tmp/asr-bakeoff/report.json`; candidate archives and hashes in `/tmp/asr-bakeoff/downloads/sha256sums.txt`.
- **[D4] Wake-phrase removal replay:** Kroko streaming transcripts of saved wake clips (turns 396–427) and of the `recordings/` utterances; `turns.stt_text` for all rows; the rule of section 16.6 with `wake_phrase` `ophelia` and cut-off 0.45. Run in the session evaluation kernel.
- **[D7] Wake-phrase miss replay, 2026-09-28:** turn 433 trace in `docker logs echomuse-controller` (`utterance.heard`, `stt_raw`, `intent_local`); HA `assist_pipeline/pipeline_debug/get` runs `01M3JWP1GS4DKDZTH3RRK19SXS` (STT) and `01M3JWP6PS5F69WCCAYMW60YER` (intent→TTS); `conversation/agent/homeassistant/debug` on both sentences; `data/recordings/G090LF10728426PR_433.wav` and `data/wakes/G090LF10728426PR/433.wav` decoded in the container with `em_speech_bundle.create_recognizer`. Throwaway scripts, not retained.
- **[D8] HA's own timer handling, 2026-09-28:** `conversation/agent/homeassistant/debug` on HA 2026.8.1 before the custom sentences: “set a timer for 5 hour, 15 min”, “5 min, 10 second”, “an hour and a half”, “10 min”, “2 hrs” matched nothing; “add 5 min to the timer” matched `HassListAddItem`, “unpause my timer” `HassMediaUnpause`, “cancel the 5 min timer” `HassCancelTimer` with name “5 min”. `assist_pipeline/run` (intent stage, the Office `device_id`) with a 5 and a 10 minute timer: “how much time is left” and “what timers do I have” answered by `conversation.claude_conversation`; “cancel the timer” `processed_locally` with `response_type` `error`. `intent/timers.py` `CancelTimerIntentHandler` returns no speech slots; `_find_timers` returns every device's timers and `_find_timer` filters by name, start duration, then device. After adding `custom_sentences/en/timers.yaml` every one of these ran locally (`processed_locally`). Throwaway scripts, not retained; the sentence lint is `custom_sentences/lint_timers.py` in the HA configuration.
- **[D5] ASP notification analysis:** `/tmp/asp-notify/REPORT.md` and `report.json`. `libasp.so`, `libaspclient.so`, and `aspclient.jar` MD5-match between archive and device. Codes 1 and 9 dispatch via `fcn.0001dd6c` → `fcn.0001cc18` → `fcn.0001c9a0` (“updateAudioConfigToAFE”), which sets only the volume-leveller normalization type, the output speech-enhancement selector, and the upmix selector. The strings “Alarm : set AEC adaptation” and “Caller has permission” were not reached from this dispatcher. Binder transaction 3 on `audiosignalprocessor`.
- **[D6] On-device cost and placement evidence, 2026-09-26:** `/tmp/ondevice-bench/REPORT.md` and `report.json`; harness `/tmp/ondevice-bench/harness/bench.c`; raw runs `/tmp/ondevice-bench/results/{device,host}/*.json`. Device runtime `/data/local/share/echomuse/oww/libonnxruntime.so` (SHA-256 `174233cf1a3f841f1eac82a4328f4f23f8819d5c62969c09e994a6b1abf498c1`). Device probes through `controller/tools/devshell.py`: `/proc/cpuinfo`, `/sys/devices/system/cpu/{online,present}`, `/proc/meminfo`, `top`; controller `wake_counters` and `device_metrics` rows for `G090LF10728426PR`. Shadow statistics semantics: `device/internal/wakeword/shadow/shadow.go` (`Skipped` counts hop duty cycle). Native split: [native endpointing research](alexa-endpointing.md), “Where it lives” and “What the device DOES use libpryon for”; `cfg/system/local/models/keyword/en-US/{ALEXA/pryon.config,AMAZON/pryon.inc}` (`scorer.score_upsampling.*`). ESPHome on-device wake word: [Micro Wake Word](https://esphome.io/components/micro_wake_word/).
- **[H6] Stock HA surfaces:** installed `components/calendar/__init__.py` (`calendar/event/create|update|delete|subscribe`, `CalendarEvent.as_dict`, `dtstart`/`dtend` keys); `components/local_calendar/config_flow.py` (`calendar_name`, `import`); the installed `ical` library's timeline expansion and `EventStore.delete`; `assist_satellite/entity.py` `async_internal_start_conversation` and `async_accept_pipeline_from_satellite` (stored prompt and conversation, segmenter at the VAD-sensitivity setting: 0.7, 1.25, or 0.25 s); `esphome/manager.py` (device events `esphome.*` fired without the allow-actions option); `helpers/llm.py` `ActionTool` (blocking call, `return_response=True`); `websocket_api/commands.py` (`subscribe_events` allowlist, admin-only `fire_event`).
- **[H1] HA continuation:** [Conversation API](https://developers.home-assistant.io/docs/intent_conversation_api/), `continue_conversation` and `conversation_id`.
- **[H2] HA timer helper:** installed `homeassistant/components/timer/__init__.py` (SHA-256 `c3b477ca…`) `Timer.async_added_to_hass` and `async_finish`; [Timer integration docs](https://www.home-assistant.io/integrations/timer/); home-assistant/core issue #79145.
- **[H3] Assist timer implementation:** installed HA 2026.8.1 `homeassistant/components/intent/timers.py` (SHA-256 `953e6aa9…`): `TimerInfo`, `TimerManager`, `async_register_timer_handler`, `async_device_supports_timers`; `components/intent/llm.py` (SHA-256 `59027aa0…`) exposes timer tools when the device supports timers.
- **[H4] HA pipeline/conversation boundary:** installed `assist_pipeline/websocket_api.py` (`f94c08ee…`) and `pipeline.py` (`1b3d97e6…`), `conversation/models.py` (`ab01f362…`), `stt/__init__.py`, `stt/models.py`, `tts/__init__.py` (`f8a3a586…`), `helpers/llm.py`, `components/llm/__init__.py`, `websocket_api/connection.py`; [Assist pipelines](https://developers.home-assistant.io/docs/voice/pipelines/), [Assist satellite](https://developers.home-assistant.io/docs/core/entity/assist-satellite/), [LLM API](https://developers.home-assistant.io/docs/core/llm/).
- **[H5] Live HA configuration:** Office device `b285ec5e94b98f4c1cbeabf926be4b77` (ESPHome, MAC `09:0f:10:72:84:26`); selected pipeline `STT` (`01hnpec9vt52ec4dkkxqa89z46`): `stt.faster_whisper`, `conversation.claude_conversation`, `tts.elevenlabs_text_to_speech`, `prefer_local_intents=true`; Claude and OpenAI conversation subentries select `llm_hass_api=["assist"]`; `helpers/chat_session.py` 5-minute idle session expiry; `esphome/assist_satellite.py:292-301` registers timers only for `VoiceAssistantFeature.TIMERS`.
- **[H7] Other satellites' timer handling:** HA 2026.8.1 `intent/timers.py` (`TimerManager`: in-memory `timers`/`timer_tasks`, `asyncio.sleep` countdown, `_timer_finished` calls the device's handler once), `esphome/assist_satellite.py` (`handle_timer_event` forwards all four events; registered only with `TIMERS`), `wyoming/assist_satellite.py` (`_handle_timer` drops events while the satellite is disconnected); ESPHome `components/voice_assistant/voice_assistant.cpp` (`on_timer_event`, `timer_tick_` decrements for display, `timer_finished_trigger_` fires only on HA's event); Voice PE `home-assistant-voice.yaml` (`on_timer_finished` → `timer_ringing`: loop the sound with a 500 ms gap, duck media 20 dB, enable the on-device “stop” model, stop on wake word, stop word, or button, give up after 15 min); wyoming-satellite `satellite.py` (`trigger_timer_finished` plays a WAV a configured number of times).
- **[S1] Speech runtime interfaces:** [online recognizer result/API](https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/online-recognizer.h), [endpoint rules](https://github.com/k2-fsa/sherpa-onnx/blob/master/sherpa-onnx/csrc/endpoint.h), [separate speaker extractor example](https://github.com/k2-fsa/sherpa-onnx/blob/master/python-api-examples/speaker-identification.py), [3D-Speaker exporter/model choices](https://github.com/k2-fsa/sherpa-onnx/blob/master/scripts/3dspeaker/export-onnx.py). These mutable upstream references establish available interfaces, not a tested release/model bundle.

**Bottom line:** native AFE supplies the enhanced audio; BCResNet on the Dot proposes wakes continuously and keeps idle audio in the room; reference-aware policy on the controller admits interruptions; a target-aware decoder/endpointer owns utterance completion; a single actor owns the dialog; HA owns timers and durable alarm truth, while the Dot owns audible execution. None of those responsibilities should be hidden inside a playback watcher or inferred from whether one audio queue happens to be empty.

## 16. Normative implementation contracts

This section closes choices that must not be left to an implementer. A numerical default can be calibrated without changing ownership or message semantics. Such a measurement is a release gate, not an unresolved architectural choice.

### 16.1 Device transport and audio framing

**Sockets, identity, and sessions.** The device uses three WebSockets over the retained TLS link. It verifies the controller's certificate against its stored `ca.pem` and sends its per-device token in the `X-EM-Token` header on every upgrade (section 18.2).

1. It opens `/device/v1/control` first and sends `session.hello`, whose envelope carries `session_id: null`.
2. The controller answers with `session.ready` carrying a new `session_id`, which every later control envelope carries.
3. Only then does the device open `/device/v1/audio` (binary audio and cell records) and `/device/v1/assets` (files by hash), each with the extra header `X-EM-Session: <session_id>`. The controller rejects an audio or assets socket whose session is not the device's current one.

Closing the control socket ends the session: the controller closes the other two, and the device reconnects from `session.hello`. Control messages use UTF-8 JSON, maximum 256 KiB; audio messages use a fixed 64-byte little-endian header followed by their payload. WebSocket compression is disabled for all three. Large alert snapshots are paged, never stuffed into a control frame.

**Heartbeats.** Each side sends `heartbeat` on the control socket every second and counts any received control message as liveness. After 2 s with nothing received, the link is degraded. After 3 s, the session is lost: both sides close all three sockets and end every dialog and uplink lease. The device keeps executing alerts.

| Byte offset | Type | Meaning |
|---:|---|---|
| 0 | 4 bytes | ASCII `EMA1` |
| 4 | uint8 | kind: 1 mic, 2 final-mix reference, 3 render-source audio, 4 cell records, 5 AFE records (`afe_metadata_v1` sessions only, section 4.5) |
| 5 | uint8 | flags: bit 0 discontinuity, 1 muted, 2 underrun, 3 estimated timing, 4 digital silence (kind 2 only: every sample is zero and the payload is omitted); other bits zero |
| 6 | uint8 | channels, exactly 1 |
| 7 | uint8 | format: 1 = signed PCM16 little-endian for kinds 1–3; 2 = cell record v1 for kind 4; 3 = AFE record v1 for kind 5 |
| 8 | uint64 | stream epoch, random nonzero value allocated by its producer |
| 16 | uint64 | sequence, starts at zero for this epoch |
| 24 | uint64 | first sample-frame index in this epoch |
| 32 | uint64 | estimated first-sample device monotonic time in ns; zero for not-yet-rendered downlink |
| 40 | uint32 | timing uncertainty in microseconds; `0xffffffff` means unknown |
| 44 | uint32 | sample rate: 16,000 for mic, reference, cell and AFE records; 48,000 for render sources |
| 48 | uint32 | frame count: samples, 1–1,280 uplink PCM or 1–3,840 downlink; for kind 4, cells, 1–320; for kind 5, records, 1–125 |
| 52 | uint32 | render generation for kind 3. Zero for kinds 1, 2, 4 and 5: a reference packet mixes several sources, which its mask names. Wrapping requires a new stream epoch |
| 56 | uint32 | active-source mask: content=1, alert=2, dialog=4, earcon=8; zero for mic, cells and AFE records |
| 60 | uint32 | payload bytes: frame count × 2 for PCM, 0 with digital silence, frame count × 4 for cell records, frame count × 14 for AFE records |

Mic frames normally contain 1,280 samples. Render-source packets normally contain 3,840 samples, consumed internally in 480-sample mixer blocks.

**Reference stream.** Reference packets carry the final mix decimated by a **127-tap Hamming-windowed sinc FIR, 7,200 Hz cutoff at 48 kHz, factor 3**.

- **Coefficients.** Generate them once as `h[n] = (2*7200/48000) * sinc((2*7200/48000)*(n-63)) * (0.54-0.46*cos(2*pi*n/126))` for `n=0..126`, using normalized `sinc(x)=sin(pi*x)/(pi*x)`, and normalize their sum to one.
- **Output sample.** With `x` the final mix indexed from its render epoch, reference sample `k` is `y[k] = sum over n=0..126 of h[n]*x[3k+63-n]`. It is aligned with render sample `3k`, so no group-delay correction applies. It is emitted once render sample `3k+63` exists, 1.3 ms later.
- **Epoch.** The reference stream has its own epoch, which a render-epoch change restarts. Samples `k < 21`, whose sums would reach before the render epoch, are sent as a missing range. Carry FIR history across packets.
- **No normalization.** Do not peak-normalize the reference before storing it; the wake scorer prepares its own scratch window.

`stream.open` precedes audio and binds epoch to `stream_id`, `playback_id` where applicable, source class, sample rate, generation, and format. A kind-3 packet's epoch selects exactly one render source. Reject unknown epochs, malformed lengths, wrong rates, decreasing sequence/sample counters, and audio after `stream.end`; report a protocol error and close the offending audio session. Explicit gap ranges advance indices; a packet may not disguise a missing range as contiguous audio. uint64 values in JSON are decimal strings to avoid JavaScript precision loss.

**Cell records** (kind 4, format 2) belong to the mic's capture epoch and use its epoch value. Cell `k` covers mic samples [512k, 512k + 512); the header's first index is the first cell's start sample. Each record is 4 bytes:

| Bytes | Content |
|---|---|
| 0–1 | `E` in hundredths of a dB, a little-endian two's-complement int16, clamped to −12,000…0 |
| 2 | flags: bit 0 gap (the cell overlaps a missing range); bit 1 muted; other bits zero |
| 3 | the final mix's active-source mask during the cell (content=1, alert=2, dialog=4, earcon=8), mapped from render to capture time with the clock fit (section 4.3) |

A gap cell carries `E` = −12,000 and the gap flag.

**AFE records** (kind 5, format 3) exist only in a session whose `session.ready` opted into `afe_metadata_v1` (section 4.5). Like cells they belong to the mic's capture epoch; record `k` summarises the native AFE metadata frames that ended in capture samples [1280k, 1280k + 1280), and the header's first index is the first record's start sample, a multiple of 1,280. The 14-byte layout is in [protocol-v1.md](protocol-v1.md) §3. A capture missing range has no records.

**Uplink packets** exist only under a lease (section 4.4). Backfill packets keep their original sample indices and timestamps and precede live packets of the same stream. The controller rejects packets for a stream that no active lease wants.

Control envelope:

```json
{
  "protocol": 1,
  "type": "focus.acquire",
  "session_id": "uuid",
  "message_id": "uuid",
  "device_id": "registered-endpoint-id",
  "generation": 17,
  "body": {
    "lease_id": "uuid",
    "owner": "turn-uuid",
    "focus": "dialog_input",
    "ttl_ms": 3000
  }
}
```

Required command/event families:

| Type | Required body fields / result |
|---|---|
| `session.hello` | capabilities the firmware implements, device boot ID, protocol versions, privacy state, clock health, alert delivery epoch and acknowledged sequence, SHA-256 of every installed speech asset |
| `session.ready` | chosen protocol, session ID, server boot ID, capture permission; `assets: {runtime_sha256, graph_sha256, sidecar_sha256}`; detector policy: thresholds `{idle, playback, near_miss}`, hop of 2 blocks, smoothing of 3, history cleared after 6 unscored hops (section 5.2), and the provisional-duck rule with its depth `duckDb` (section 16.6); to an `open_rules_v1` device also `open_rules` (the two baseline rules from the thresholds, then `wakeOpenRules`; at most 6, at least one per profile) and `shadow_rules` (`wakeShadowRules`, at most 8), each rule `{profile, windows 1–3, combine mean/all, threshold}`. Absent `open_rules` means the baseline rules from the thresholds, absent `shadow_rules` none; an invalid list leaves the detector `load_failed` (section 5.2) |
| `stream.open` / `stream.end` | stream ID, epoch, format and owner; end includes final sample count and reason |
| `focus.acquire` / `focus.renew` / `focus.release` | lease ID, owner, generation; acquire includes class and TTL |
| `render.start` | playback ID, epoch, generation, source class, gain, format |
| `render.cancel` | exact playback ID/generation, reason; cancellation is idempotent |
| `render.progress` | Sent every 80 ms and on every event. Fields: playback ID/generation; event `progress/flush/seek/pause/resume/underrun/gain`; submitted and completed frames; monotonic anchor; uncertainty; timing quality; `reference_coverage` `full/partial`. Per event: the new source frame for `seek`, the missing frame range for `underrun`, the applied gain in dB for `gain` |
| `render.finished` | playback ID/generation, last completed frame, reason `drained/cancelled/failed/underrun` |
| `privacy.changed` | muted bool, new capture epoch or null, physical-event sequence |
| `button.action` | device monotonic time, capture sample index at the press, physical-event sequence, current occurrence ID or null |
| `wake.candidate` | device → controller. Fields: candidate ID and its new `candidate` lease ID, whose generation is always 1 (conversion to `turn` makes it 2); capture epoch; graph SHA-256 and scorer revision; latched profile and threshold; `producing_sound`; `chimed` (the device started the wake chime for this candidate, section 11.2); first-crossing end sample; `support_start`; device monotonic time of open; on `open_rules_v1` firmware `rule`, the open rule that opened it, whose threshold is the latched one (absent: the baseline rule). `hops` holds one record per hop slot from `support_start` to open, `{end_sample, raw, smoothed, profile}`, with `raw` and `smoothed` null for a window not scored |
| `wake.candidate_end` | device → controller: candidate ID, `support_end`, peak smoothed probability, close reason `below/gap/reset/mute/overrun` |
| `wake.stats` | device → controller every 30 s: hops scored, hops dropped (`wake_overrun`), inference errors, mean and maximum inference time, near-miss episodes with time and peak, candidates opened, peak smoothed value, graph SHA-256, and `wake_unavailable` as null or a reason (`missing_asset/load_failed/inference_errors`); on `open_rules_v1` firmware `shadow`, one entry per shadow rule with this window's `hops`, `opens`, `matched`, `lead_hist`, `unmatched`, `retried`, `live_only`, events and `events_dropped` (section 5.2; absent from older firmware means no shadow data, not zeros); in an `afe_metadata_v1` session `afe`, the AFE metadata decoder's periods, valid and invalid frames, syncs, gaps and lost frames for the window (section 4.5; absent means no data). Also sent immediately whenever `wake_unavailable` changes |
| `uplink.open` | controller → device: lease ID, owner, generation, reason `turn/reply/diagnostic`, streams, per-stream start sample or `live`, TTL |
| `uplink.renew` | controller → device: lease ID, owner, generation, TTL. On a `candidate` lease, `reason: turn` with the turn as owner and generation + 1 converts it into the accepted turn's lease |
| `uplink.close` | controller → device: lease ID, generation, reason `rejected/arbitration_lost/committed/closed`. Closing a candidate lease also releases its provisional duck |
| `uplink.ended` | device → controller: lease ID, reason `closed/ttl/mute/epoch/overrun/session`, last sample sent per stream, clipped backfill start per stream or null |
| `alert.snapshot` / `alert.delta` | the alert cache objects of section 16.4 |
| `alert.local_operation` | device → controller. Fields: operation ID; captured occurrence ID and its revision; action `dismiss/snooze/expire`. For `expire`, the reason `timed_out/missed`. For `snooze`, the child's schedule ID, occurrence ID, and due time (section 16.3) |
| `alert.act` | controller → device: operation ID, captured occurrence or ring ID, action `dismiss` or `snooze`. For an alarm, the device applies it through the same journaled local-operation path as the button and replies `command.ack` `durable`; a timer ring is stopped without journaling |
| `alert.ring` | controller → device, timers only: ring ID (the HA timer ID), name, sound hash or `builtin:fallback`, loop gap, ring limit. Not persisted; the device queues it under the alert focus rules and rings immediately when it reaches the head |
| `alert.prefetch` | controller → device, when the device announces `alert_prefetch`: alert sound hashes to install ahead of need. The controller names the effective `timerSound` after `session.hello` and whenever an HA timer starts, because `alert.ring` names it only as the timer finishes and a missing asset rings the fallback |
| `alert.ring_ended` | device → controller, telemetry only: ring or occurrence ID, reason `stopped/button/entity/limit/preempted/restart`. The durable result of an alarm ring is its `alert.local_operation` |
| `alert.ack` | delivery epoch, applied-through sequence, durable bool, `need_snapshot` bool; not merely socket receipt |
| `command.ack` | message ID, status `accepted/applied/durable/rejected`, error code if rejected |
| `heartbeat` | monotonic time and session ID; not an assertion that audio progressed |

**Assets.** The device keeps at most one `/device/v1/assets` socket and at most one request in flight on it.

1. The device sends `{"sha256":"<hex>","offset":<n>}`.
2. The controller answers with binary chunks of at most 64 KiB from that offset, then `{"sha256":"<hex>","size":<n>,"done":true}`, or `{"error":"not_found"}`. A second request sent before that answer ends closes the socket.
3. The device writes `<sha256>.part`, resumes by offset after a disconnect, verifies SHA-256, syncs, and renames atomically.

This one path serves alert sounds and speech assets (section 16.5).

No general `set_volume`/`stop_everything` message implements turn cleanup. Release/cancel exactly the named owner. Device button/mute events bypass queued inference, use a monotonically increasing physical-event sequence, and are deduplicated independently of audio.

### 16.2 Actor ordering, continuation, and focus details

Process already-received events in this priority order within a single actor iteration: physical mute; physical stop; session loss/lease expiry; accepted explicit interruption; audio discontinuity; speech observations, including `wake.candidate`; playback/HA results; ordinary timer deadlines. This priority does not reorder audio samples inside a valid stream. A deadline commits only after the actor has processed observations through its required sample frontier. Model completion time is not an acoustic timestamp.

**Candidate handling.** On `wake.candidate`, the actor records the candidate, returns `command.ack(accepted)` so its candidate lease may upload, and renews that lease every second until it decides. Without `producing_sound`, it accepts immediately, claims the arbiter (section 5.3), and converts the lease with `uplink.renew(reason: turn)`; the backfilled audio becomes the utterance's pre-roll. With `producing_sound`, it runs the section 6.1 steps as backfill arrives and decides by the verification deadline (section 16.6). An accepted wake without a command context gets the wake chime from the controller only when `wakeSound` is on and `chimed` is false (section 11.2). A rejection sends `uplink.close(rejected)`, which also releases the device's provisional duck; a chime the device already played stays. `wake.candidate_end` only updates `support_end`; the decision never waits for it.

`generation` increases before cancelling prior output. Every resulting command carries the new owner plus the old exact playback ID where cancellation targets it. The actor drops callbacks whose utterance/playback/generation is no longer current. `COMMITTED` records a commit ID before dispatch; a second commit of that utterance is an internal error, not another HA call.

Explicit reply policy is deterministic:

- The expectation expires after 7 seconds without accepted speech, starting after guarded `drained`; if no prompt audio exists, start on receipt of the response.
- Early answers use section 16.6's rule: 480 ms qualifying near-end/no-reference speech, or an explicit wake/button. Otherwise retain pre-roll but do not interrupt the prompt.
- A new speaker can wake/press the button. Preserve question context only when the expectation remains valid.
- Chain count is at most five no-wake replies and the expectation chain lasts at most 60 seconds, whichever limit occurs first.
- Prompt failure, an unrelated new wake, privacy mute, or session loss clears the expectation. A late `drained` from that prompt cannot recreate it.

Foreground order is a policy table, not numeric class ordering: local physical stop/mute first; an alert whose background budget expired next; accepted dialog input/output next; ordinary due alert next; content last. Earcons belong to their requesting owner and cannot claim an independent long-lived lease. At most one dialog output and one alert occurrence exist as foreground candidates. Gains combine by taking the most attenuating active lease, not multiplying repeated duck commands. Restore recomputes current policy from surviving leases and source state.

Local command matching (section 6.3) uses the streaming text after wake-phrase removal (section 16.6, step 4), normalized as follows. It must be `stop`, `cancel`, or `snooze`, each with an optional `please` before or after. When the command context is an alert, `(stop|cancel|turn off|dismiss) [the] [<name>] (timer|alarm) [please]` also matches: the noun must match the ringing occurrence's kind, and `<name>`, if present, must equal its name. Snooze applies only to alarms. Lowercase, trim edge punctuation, and collapse whitespace; do not delete negation or match substrings. Matching applies only to the first utterance of a wake-initiated turn with a non-empty command context, only after the normal reducer commits, and only if the text was unchanged for the 240 ms before the commit and fewer than half of the speech-positive cells from 480 ms before the first command token's emission sample to the commit boundary are `self_output`. The ringing alert or cancelled output in the pre-roll does not count, and no wake chime plays for a turn whose command context is non-empty: the ring or reply stopping is the acknowledgement. Any other text, including extra words or negation, is a normal HA turn. No speaker anchor is required: any nearby person who says the wake word may stop an alert. Speaker evidence never authorizes an action.

### 16.3 Persistent data

**Calendar event encoding.** The stock websocket `calendar/event/create` and `calendar/event/update` take an `event` with `summary`, `dtstart`, `dtend`, `description`, and `rrule` [H6]. EchoMuse writes:

| Field | Alarm | Snooze child |
|---|---|---|
| `summary` | label, default “Alarm” | “Snoozed: <parent summary>” |
| `dtstart` | first local time, with HA's timezone offset | the snooze press, rounded up to the next whole second, + `snooze_ms` |
| `dtend` | `dtstart` + 1 min | same |
| `rrule` | `FREQ=DAILY` or `FREQ=WEEKLY;BYDAY=…`; absent for a one-shot | absent |
| `description` | optional human text, then the `echomuse:` line | same |

The last description line is `echomuse: ` followed by canonical JSON: sorted keys, no whitespace, UTF-8, one line. Wrapped here for reading:

```text
echomuse: {"id":"<uuid>","kind":"alarm|snooze","loop_gap_ms":2000,
  "max_ring_ms":600000,"op":"<uuid>","parent":null|{"id":"<uuid>","occ":"<key>"},
  "ramp_ms":20000,"snooze_ms":540000,"sound":"<sha256>|builtin:fallback",
  "v":1,"volume":null|0.0-1.0}
```

`volume: null` means “ring at the current media volume”. Labels are at most 120 characters.

An event without a valid line (created in the HA calendar UI or by an automation) is an alarm with these defaults: the endpoint's effective `alarmSound` (section 16.7), volume `null`, snooze 540,000 ms, ring maximum 600,000 ms, loop gap 2,000 ms, ramp 20,000 ms. A line that is present but malformed is treated the same way and flagged on the dashboard; it is never guessed at.

**Identity.** `schedule_id` is the JSON `id`, or UUIDv5(`NAMESPACE_URL`, `"<calendar entity>/<uid>"`) for an event without one. The occurrence key is `recurrence_id` for a repeating instance, otherwise `dtstart` in ISO-8601 with offset, second precision. `occurrence_id` = UUIDv5(`schedule_id`, key). A snooze child's `schedule_id`, which is also its JSON `id`, is UUIDv5(`NAMESPACE_URL`, `"echomuse-snooze:<parent occurrence_id>"`); its key is its `dtstart`. The device and the controller derive the same child from the parent occurrence and the frozen due time, and a snoozed child snoozes the same way.

**Capacity.** At most 256 EchoMuse schedules per calendar; a further create returns `capacity_exceeded` before any write. If the 7-day window holds more than 4,096 occurrences, deliver the earliest 4,096 and flag the endpoint.

**Controller journal.** Added to the controller database by the schema 21 → 22 migration (section 18.4):

```sql
CREATE TABLE alert_ops (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id TEXT NOT NULL UNIQUE,
    endpoint_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN
      ('create','update','dismiss','snooze','expire','cancel')),
    schedule_id TEXT NOT NULL,
    occurrence_key TEXT,
    source TEXT NOT NULL CHECK (source IN ('voice','llm','device','dashboard','engine')),
    payload_json TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','applied','rejected')),
    step INTEGER NOT NULL DEFAULT 0,
    result_json TEXT,
    created_ms INTEGER NOT NULL,
    applied_ms INTEGER,
    CHECK ((state = 'applied') = (applied_ms IS NOT NULL)),
    CHECK (action NOT IN ('dismiss','snooze','expire') OR occurrence_key IS NOT NULL)
);
CREATE INDEX alert_ops_pending ON alert_ops(endpoint_id, state, seq);
CREATE INDEX alert_ops_terminal ON alert_ops(schedule_id, occurrence_key);
CREATE TABLE alert_delivery (
    endpoint_id TEXT PRIMARY KEY,
    calendar_entry_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    delivery_epoch TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    acked_sequence INTEGER NOT NULL,
    CHECK (acked_sequence <= sequence)
);
```

A duplicate `op_id` with the same payload hash returns the stored `result_json`; a different hash returns `op_id_conflict`. `step` records progress through a multi-write operation (a snooze is at step 1 after its child exists). Keep applied `dismiss`, `snooze`, `expire`, and `cancel` rows for at least 90 days: they are the restore guard. No transcript or audio is stored in the journal.

### 16.4 Calendar writes, merge, and delivery

Stock websocket messages [H6]:

```json
{"type":"calendar/event/create","entity_id":"calendar.echomuse_office",
 "event":{"summary":"Wake up","dtstart":"2026-09-23T06:30:00-05:00",
          "dtend":"2026-09-23T06:31:00-05:00","rrule":"FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR",
          "description":"echomuse: {…}"}}
{"type":"calendar/event/update","entity_id":"…","uid":"…","event":{…the full event…}}
{"type":"calendar/event/delete","entity_id":"…","uid":"…","recurrence_id":"20260924T063000"}
{"type":"calendar/event/subscribe","entity_id":"…","start":"…","end":"…"}
```

Each subscription message carries `{"events":[…]}`. Each item has `summary`, `start`, `end`, `description`, `uid`, `recurrence_id`, `rrule`, and `all_day`. `create` returns no UID; the controller learns it from the next push by matching `op`. Omit `recurrence_id` to act on a whole event or series.

**Applying one operation:**

1. Insert the `alert_ops` row as `pending`.
2. Perform the section 10.3 writes in order, advancing `step` after each acknowledged write.
3. Mark it `applied` once the subscription shows the final state: the `op` present after a create, the occurrence absent after a delete, the new fields after an update. If that has not appeared 10 s after the last acknowledged write, re-subscribe to force a fresh push. If it is still missing, mark the operation `rejected` with a reason and surface it.
4. Emit the device delta.

Pending operations for one calendar are applied strictly in `seq` order, one at a time. “Not found” on a delete counts as success. A failed create is `rejected` and reported as not stored. While HA is disconnected, operations stay `pending`; after reconnecting, the controller re-subscribes before retrying, so deduplication sees current data.

**Merging device-originated operations:**

| Device operation | Calendar state when applied | Result |
|---|---|---|
| Dismiss occurrence | occurrence present | delete it |
| Dismiss occurrence | occurrence absent | success, no write |
| Snooze occurrence, child deadline D | parent schedule present | create the child at D with the device's operation ID, then delete the parent occurrence |
| Snooze occurrence | parent schedule gone | `rejected`; tombstone for the child to the device |
| Expire, reason `timed_out` or `missed` | occurrence present | delete it; journal the reason |
| Expire | occurrence absent | success, no write |
| Ring start | — | never written to HA |

An occurrence that disappears from the calendar while the device is ringing it (deleted or retimed in HA) becomes a device tombstone. The device stops ringing it once delivered.

**Restore guard.** On every push, re-delete (and do not deliver) any occurrence whose (`schedule_id`, occurrence key) matches an applied terminal row, and any EchoMuse event whose `id` matches an applied `cancel`. This undoes an HA restore from an older backup before anything can ring.

**Backlog reconciliation.** At controller start and after every HA reconnect, before subscribing, the controller fetches the stock REST `GET /api/calendars/<calendar entity>?start=<now − 90 days>&end=<now − 30 min>`. Every returned occurrence is past the catch-up window. It applies the restore guard to them, then expires the rest as `missed` through the normal operation path. The live subscription covers only [now − 30 min, now + 7 days].

**Device delivery.** `alert_delivery` holds a per-endpoint epoch and sequence; a new epoch starts whenever the controller database is created. The device reports its epoch and acknowledged sequence in `session.hello`. On a mismatch, or when an `alert.ack` carries `need_snapshot: true`, the controller sends a full snapshot. The device uploads its local operations before any snapshot is installed. A stale epoch is never an instruction to discard local operations.

**Alert cache objects.** All are canonical JSON: sorted keys, no whitespace, UTF-8. An occurrence:

```text
{"due_local":"2026-09-23T06:30:00-05:00","due_utc_ms":"1790422200000","kind":"alarm|snooze",
 "label":"Wake up","loop_gap_ms":2000,"max_ring_ms":600000,"occurrence_id":"<uuid>",
 "ramp_ms":20000,"revision":17,"schedule_id":"<uuid>","snooze_ms":540000,
 "sound":"<sha256>|builtin:fallback","volume":null}
```

A tombstone is `{"occurrence_id":"<uuid>","revision":18,"tombstone":"dismissed|snoozed|expired|deleted"}`. `revision` is the delivery sequence at which the object last changed.

- **`alert.delta`** body: `{"delivery_epoch","sequence","objects"}`, where each object is an occurrence to upsert or a tombstone. The sequence is the previous one plus one. The device applies a delta only when its sequence is its acknowledged sequence plus one; otherwise it answers `alert.ack` with `need_snapshot: true`.
- **`alert.snapshot`** page: `{"delivery_epoch","high_water_mark","page_index","page_count","sha256","objects"}`, with at most 128 objects per page. A snapshot carries live occurrences only, sorted by `due_utc_ms` then `occurrence_id`. `sha256` covers the canonical JSON array of all pages' objects concatenated in order. The device stages all pages, validates the digest, and keeps its own protected local tombstones: an occurrence it dismissed, snoozed, or expired stays tombstoned until the controller has applied that operation and stopped delivering the occurrence. It then installs atomically and acknowledges the high-water mark.

### 16.5 Device persistence, clock, and assets

Device paths:

```text
/data/local/etc/echomuse/alerts/snapshot.json
/data/local/etc/echomuse/alerts/operations.log
/data/local/etc/echomuse/alerts/assets/<sha256>.wav
/data/local/share/echomuse/speech/<sha256>.{so,onnx,json}
```

Journal records are length-prefixed JSON plus CRC32; a record contains one whole transaction, including a snooze parent and child. Sync the record before durable acknowledgement. On replay, ignore only an incomplete final record; a checksum failure before the tail marks the store corrupt and requires reconciliation, not silent truncation. Snapshot compaction writes a temporary file, syncs it, renames it, syncs the directory, then replaces the covered journal prefix. Keep the previous snapshot until the new one is durable.

Store `device_boot_id` and monotonic deadlines with each armed cache entry. Reuse a monotonic deadline across process restart only within that same OS boot. Across boot, require two authenticated controller time samples within 1 second of each other and a round-trip bound ≤500 ms before accepting UTC time; until then the clock is untrusted and newly reconstructed wall-clock deadlines do not fire. No unauthenticated network time packet changes an alert deadline.

The privileged device supervisor owns a named Android kernel wakelock while any cached occurrence is armed or ringing: write `echomuse_alerts` to `/sys/power/wake_lock`, verify it is listed, and write that same name to `/sys/power/wake_unlock` when the cache becomes empty or shuts down cleanly. These nodes were observed on the active device; they are mode 0660 `radio:system`, so an ordinary `shell` process cannot acquire them. Provision/run the supervisor with the required existing root privilege; do not rely on an unspecified Java/platform bridge. Acquisition failure produces `alarm_wakeup_unavailable` and withholds `alert_cache_v1`. On a process restart within the same boot, reconcile ownership of the same named lock instead of accumulating new names. The selected always-awake executor is appropriate to a mains-powered Dot; no new Amazon AlarmManager dependency is introduced. The read-only probe did not acquire a lock, so actual suspend prevention remains a hardware acceptance check.

Alert assets are mono PCM16 WAV at 48 kHz, at most 10 seconds/960,000 PCM bytes each, validated by SHA-256 before atomic installation. The built-in fallback is generated by EchoMuse: four 200 ms 880 Hz sine bursts separated by 100 ms silence, with 10 ms edge fades, then an 1,800 ms inter-loop gap. It is not an Amazon asset. Alarm default ramp is 20 seconds to the configured alert gain; timer ramp is zero. The ring deadline counts gaps and background time, not only audible samples. A missing asset immediately chooses the fallback; no fetch or codec startup blocks the deadline.

Audio admission requires nonzero alert gain; the UI can explicitly silence/delete a schedule instead of silently creating an inaudible alarm. Mic privacy mute does not mute the alert source.

**Speech assets.** `session.ready` names, by SHA-256, the ONNX Runtime library, the active wake graph, and its sidecar (section 5.1). The runtime is `libonnxruntime.so` 1.19.2 armeabi-v7a, SHA-256 `174233cf1a3f841f1eac82a4328f4f23f8819d5c62969c09e994a6b1abf498c1`: the firmware's current runtime, identical to the official release [D6]. The device fetches missing files over `/device/v1/assets` (section 16.1), verifies each hash, loads the runtime with `dlopen`, and creates the BCResNet session with one intra-op thread, spinning off, sequential execution, `ORT_ENABLE_ALL`, and the XNNPACK provider: the configuration measured in section 5.2. It keeps the previous graph until the new one loads, switches between hops, and deletes unreferenced files after a successful switch. The runtime is pinned. Official Android builds from 1.21.0 lack the SysV symbol hash this API 22 linker requires, and 1.20.0 was measured no better. A runtime change re-measures section 5.2's cost on the Dot.

**Alert loudness and routing.** The codec DAC volume (`tinymix` control 61) stays the single hardware master and normally holds the persisted media volume (`startupVolume`). Alert foreground already pauses content, so the supervisor sets the DAC to the occurrence's `volume` for the alert and ramps back to the media volume on release. Volume buttons during a foreground alert adjust only that occurrence's playback and are not persisted; otherwise they adjust media volume as today. If a headphone is inserted (the kernel accessory detector mutes the internal amp), alert foreground re-enables the internal amp (`tinymix` control 5) for the occurrence and restores the prior amp state afterward, so an alarm is never routed only to headphones. Alert assets loop with the occurrence's `loop_gap_ms`; the fallback tone uses its own cadence below.

### 16.6 Speech bundle, evidence, and endpoint rules

The values below define policy revision `post_afe_3`. They are implementable defaults, **not calibrated field probabilities**. Changing a value produces a new policy hash and reruns the section 13 acoustic gates; it does not require BCResNet retraining.

`post_afe_2` differs from `post_afe_1` in one rule: after `needs_more` or `unknown` text, route A's vote counts `background_speech` as quiet. Under `post_afe_1` a free-form question followed by a TV at least 10 dB down had no route: route A refused the TV as pause, route B and the fallback need `complete` text, and the no-progress limit closed it as `retry` (Office turn 460, 2026-09-28). The 1,792 ms pause bounds the added risk: only a soft stretch at least that long, containing no louder cell and ending in 400 ms without a recognized word, can end a request before speech that follows it.

`post_afe_3` differs from `post_afe_2` in how streaming text reaches the reducer; no reducer threshold changes. The pinned Kroko encoder (`decode_chunk_len=128`, `T=141`) decodes 1.28 s chunks, each needing 13 more frames of right context, so its text changes only at 1.44 s and then every 1.28 s of stream audio, and greedy decoding only appends tokens. Three changes follow. The three-result, 240 ms stability rule is dropped: between chunk edges consecutive results are identical, so it added 240 ms and checked nothing. With it goes route B's test of streaming text after the stable prefix, which is now always empty. The text is finalized 320 ms into each pause: the final word of a command otherwise appeared a median 1.10 s (max 1.69 s) after speech end, so a `complete` command committed about 1.5–1.9 s after speech instead of 608 + 192 ms. Every fresh-decode flush is 1.5 s: a 0.5 s flush often stopped short of the next chunk edge (the worst case needs 141 frames, 1.41 s) and left the end of a verification window or re-decoded span undecoded; in replay, 1.5 s recovered clips that returned empty. A fresh stream over [utterance start, frontier) plus 1.28 s of zeros took 110–185 ms on one thread for 3–5.6 s of audio and produced the full final text; 0.64 s of padding dropped the final word (`docs/streaming-asr-eou-evaluation.md`).

**Pinned speech bundle.** Selected by running candidates on real Dot recordings (section 14, [P2]); runtime behavior was probed in an isolated Python 3.12 environment on the controller host [P1]:

| Component | Artifact | SHA-256 | Contract |
|---|---|---|---|
| Runtime | `sherpa-onnx==1.13.8`, `cp312 manylinux2014_x86_64` wheel | `6949773017647febc0c3696dffb2c67dd3febd4737b87e3dd135a42773704a06` | Apache-2.0 |
| Streaming ASR | `sherpa-onnx-streaming-zipformer-en-kroko-2025-08-06` archive (streaming zipformer2), sherpa-onnx `asr-models` release | `c8676e5ff9ac2a85296e53ee0fd4d5fb1db6770e7a7647166eeafe349ade6834` | encoder `d4881c57449d581e0770fd53fa66c2fdc6cd167d92ece7c715e603defc96d9d4`, decoder `455ba38466fce8d5a57e7db68a323b684079ca4d9e1dd93a740d9b2429aae3b1`, joiner `d406f616736350e2a7df3e39398b78eb2fc1a2ca6973a19d3853fa3227e25b52`, tokens `396dbeb5f4858875690716084f54e90d339679d0ba3e6b5b584f3d7589254d2d`; Kroko community model, CC-BY-SA (ship the attribution; model redistribution stays under CC-BY-SA) |
| VAD | `silero_vad_v5.onnx`, 2,313,101 B, sherpa-onnx `asr-models` release | `6b99cbfd39246b6706f98ec13c7c50c6b299181f2474fa05cbc8046acc274396` | MIT |

Package them as a controller-side speech bundle with a manifest containing every file hash above. Startup refuses a missing or mismatched file. None of them runs on the Dot (section 8.1). The Dot's speech assets are the runtime, the wake graph, and its sidecar (section 16.5). Controller-host one-thread measurements: Kroko real-time factor 0.032 with 185 MB peak process RSS on the Dot recordings [P2]; VAD was under 0.02 in the earlier probe [P1]. These are feasibility measurements.

**Evidence copy.** VAD and streaming ASR consume one **evidence copy**: canonical PCM × +20 dB (×10), clamped to int16. This is a fixed linear gain with no AGC dynamics. Canonical native-AFE speech sits at roughly −55 to −68 dBFS RMS; on that raw audio, Silero v4 missed most speech and the smaller zipformers returned nothing [P2]. Energy, level, and reference features still use canonical PCM; their ratios are gain-invariant, and the reference comparison must see what was captured. BCResNet keeps its own peak normalization. The HA STT copy (section 16.7) is separate: it uses the per-turn wake-level gain and optional DTLN.

**Streaming ASR API.** Construct with `OnlineRecognizer.from_transducer(tokens, encoder, decoder, joiner, num_threads=1, sample_rate=16000, feature_dim=80, decoding_method="greedy_search", enable_endpoint_detection=False, provider="cpu")`. On the Dot recordings, `modified_beam_search` (4 paths) scored worse than greedy (18.8% vs 9.4% WER) [P2]. Feed the evidence copy as float32 in [-1,1] after each 80 ms block; call `decode_stream` while `is_ready`. Read `json.loads(recognizer.get_result_as_json_string(stream))`: the released Python result object lacks `num_trailing_blanks`, while the JSON contains it [P1][P2]. One blank frame is **40 ms**. Bundle qualification feeds 4 s of digital silence after the archive's `test_wavs/0.wav`; the fitted slope must be 22–27 blanks/s (measured 23.5/s for Kroko) [P2]. Token `timestamps` are emission times from stream start, not word alignments. `is_final`/`is_eof` are unused: the probe left both false after flushing. `reset(stream)` returns `None`. The actor, not `is_endpoint`, owns commits.

The streaming recognizer supplies endpoint evidence, grammar completeness, local command matching (section 6.3), and wake-phrase location. Final command text still comes from section 16.7's second-pass HA transcription of the committed span, so the user's configured STT engine stays authoritative for free-form requests. Local commands are the one exception: they execute from the streaming text so that voice stop works without HA. Kroko is good enough for the evidence role on this channel: 9.4% WER against faster-whisper at raw, +20 dB, and +30 dB alike. Its only errors were a dropped two-word opening that the old 240 ms preroll discard probably clipped, and an empty result for a lone “Uh” [P2]. The previously pinned 20M model scored 72% WER at +20 dB and returned nothing at raw level, so it is dropped [P2].

**VAD API.** Use raw ONNX Runtime for probabilities, not `sherpa_onnx.VoiceActivityDetector`; that wrapper exposes only segments [P1]. Silero v5 inputs: `input:[1,576]`, which is the previous chunk's last 64 samples followed by the current 512, zeros at stream start; `state:[2,1,128]`; and `sr` int64 = 16000. Outputs: `output:[1,1]` probability and `stateN`. It runs on the controller over uploaded lease audio. Carry state and the 64-sample context within a lease; reset both at the lease's first mic sample and on epoch change, discontinuity, or mute. On the Dot recordings at the evidence-copy level, v5 detected every transcribed utterance (peak 0.82–0.999) and caught quiet speech that v4 missed. The evidence is thin and has no verified negatives: two of the three recordings with an empty HA transcript (so no verified label) also peaked at 0.61 and 0.92. Only 75% of the 1.44 s wake clips peaked at ≥0.5 [P2]. VAD therefore never ends a turn alone: route A also needs decoder blanks and stable text.

**Analysis timeline.** VAD, energy, and reference features use the VAD's non-overlapping **512-sample/32 ms cells**. An 80 ms mic block contributes 2.5 cells; carry the remainder. Every result carries its cell-end sample index. The actor waits for the evidence frontier a rule requires; it never compares unrelated callback times or fills unobserved cells.

**Activity and level.** The device computes `E = 10*log10(mean(x*x)+1e-12)` for each 512-sample cell of canonical float PCM (`x = int16/32768`) and sends it rounded to 0.01 dB (section 16.1).

- **`B`** is the 20th percentile of `E` over the valid cells of the 10 s before the cell, excluding cells whose controller VAD is known and ≥0.35. Cells from before the lease have no VAD, since the device runs none, so they all count; a 20th percentile over 10 s still lands in the gaps between sounds. Cell backfill starts 10 s before the mic backfill (section 4.4), so every uploaded mic cell has its full window. `B` needs 1 s of support or is unavailable.
- **Speech-positive** means a valid cell with VAD ≥0.50. The echo tests and route B use this term and no other threshold.
- **Trigger sample.** Each utterance has one: for a wake, the end sample of the last hop in `wake.candidate.hops` whose raw probability is ≥ the latched threshold (known at candidate open; `wake.candidate_end` never moves it); for the button, the press sample; for a reply, the onset. **Command speech** means `command_speech` cells that end after the trigger sample. The wake word's own cells can seed `F` but are never the command.
- **`F`** is the 80th percentile of `E` over the utterance's `command_speech` cells.
  - For a wake turn it is seeded, once the backfill reaches the trigger sample, from the cells between `support_start` and the trigger sample with VAD ≥0.35, when there are at least 10 of them (320 ms). Otherwise `F` is unavailable until 10 command-speech cells exist.
  - The reducer classifies each cell with the `F` in force before that cell, then adds the cell to `F`'s sample if it is `command_speech`. It never relabels earlier cells.
- **`r`**: with `B` and `F` both available, `r=(E-B)/max(F-B,12 dB)`.

Cells are classified in the order of section 8.2:

1. `gap`;
2. `self_output` (per-cell `echo_only`);
3. `non_speech` (VAD ≤0.35);
4. `background_speech` (VAD ≥0.50 with `F` available and `E ≤ F-10 dB`);
5. `command_speech`;
6. `unknown`.

A command-speech run opens on a cell with VAD ≥0.65 and either `r≥0.55` or `r` unavailable. It continues while VAD >0.35 and the cell is neither `self_output` nor `background_speech`. Cells between 0.35 and 0.65 outside a run are `unknown`. Level never declares identity and never suppresses a wake alone.

**Reference comparison.** Over valid cells in the mic candidate support, compare against the final-mix reference under the clock estimate plus a 0–500 ms lag search. Use mean-subtracted normalized cross-correlation. Accept a lag only with ≥6 cells (192 ms) of non-silent reference support and a peak exceeding the second peak outside ±20 ms by 0.10. At that lag fit `a=dot(mic,ref)/dot(ref,ref)` per cell. The unexplained ratio is `sum((mic-a*ref)^2)/max(sum(mic^2),1e-12)`, and a cell's unexplained energy is `10*log10(mean((mic-a*ref)^2)+1e-12)`, in the units of `E`. This residual is analysis-only and never replaces mic/STT audio.

The comparison returns one of three results:

- **`echo_only`** requires all of:
  - `reference_coverage=full`, with ≥80% of compared cells valid;
  - correlation ≥0.92 and unexplained ratio ≤0.20 in ≥90% of the speech-positive compared cells;
  - no two consecutive speech-positive cells with unexplained energy ≥`B+6 dB`.

  A reference candidate is not required: the observed self-wakes had no wake-like content in the reference.
- **`near_end_present`**: full coverage with ≥80% of compared cells valid, `B` available, and two consecutive speech-positive cells with unexplained energy ≥`B+6 dB`.
- **`unknown`**: anything else, including missing `B`, lag quality, compared support, or coverage.

Only `echo_only` rejects by itself. `near_end_present` is used solely as the double-talk rescue for an overlapping reference candidate (section 6.1 step 1). `unknown` passes the candidate to verification.

**Per-cell echo comparison.** Label each cell with the three-way result above, computed over the trailing 6-cell (192 ms) window at the current lag. This runs while an utterance is open or a `reply` lease is active, and only while the final-mix reference is non-silent (RMS ≥ −60 dBFS over the lag window).

- **Lag.** Estimate it when the utterance or reply lease opens, from the preceding 1 s when that audio exists and otherwise from the first 1 s of the lease. Re-estimate it every second. Without an established lag the result is `unknown`.
- **Silent reference.** The cell is `no_reference` and nothing is computed.
- **Effect.** `echo_only` cells are `self_output` and count as non-speech for route A.

**Wake verification.** This runs for every candidate while the device is producing sound (section 6.1), after the self-output and echo steps.

- **Input:** the evidence copy of the candidate lease's mic, from `support_start − 300 ms` to candidate open `+ 480 ms`, decoded by a fresh greedy stream of the pinned streaming ASR. Flush with 1.5 s of internal zeros, enough to decode audio just past a chunk edge (a 1.28 s chunk plus 130 ms of right context); that padding is never counted as audio.
- **Match:** letters-only lowercase transcript. Compute the minimum over substrings of length `len(core)−1` to `len(core)+1` of `levenshtein(substring, core) / len(core)`. Pass when it is ≤ **0.40**. `core` is the registry entry's `verify_core`; for the deployed model it is `ophel`.
- **Result:** pass accepts; fail or empty text rejects as `unverified_wake`.
- **Deadline:** 700 ms after the controller receives `wake.candidate`. The input's lookahead arrives in real time over the candidate lease, so this leaves about 220 ms for backfill and decoding. A missed deadline rejects and records `verifier_timeout`; it never accepts unverified.
- **Measured:** 23 of the 24 idle wakes from turns 396–427 pass, rendered as “Ophili”, “Pophili”, “Lophili”, “Aphili”, “Apheli”, “Nopheli”, “Ophi”, and “Ophilia”. The failure, turn 424 (“After”), captured no command and was probably a false wake. All four barge-in clips in the history fail: “Eight”, “I need to”, “fear”, and an empty transcript [D3]. The 0.40 cut-off was chosen on these same clips; section 13 recalibrates it.
- **Cost:** about 70 ms of one core per verification at the measured real-time factor 0.032 [P2]. It runs only on candidates, not continuously.

The verifier is the native two-stage structure (search proposes, verifier decides), implemented with the speech model this design already runs. It deliberately does not run in the idle profile. There the live 0.90 gate shows at most one probable false accept among the 24 recent idle wakes, and verification would add 480 ms to every normal wake.

**Provisional duck.** The device applies it on its own at candidate open when `producing_sound` is true, from the rule in `session.ready`: attenuate content and dialog by `duckDb` (default −18 dB), background any alert without dismissing it, and keep all mic audio. It restores the exact previous focus policy on `uplink.close` with reason `rejected` or `arbitration_lost`, or when the candidate lease expires. Accepting converts the duck into the turn's normal focus (section 6.2). Allow one provisional duck per candidate and at most two per 5 s. Further candidates in that window are still reported and verified, but without a duck. The duck never dismisses an alert or starts HA processing.

**Text stability.** The stable prefix is the latest streaming result's normalized tokens: the streaming recognizer only appends tokens, at its chunk edges, so a repeated result adds no evidence. For a wake turn it is taken over the streaming text after the wake-phrase cut of step 4 below, so the wake word and anything said before it are never part of it; every completeness judgment, route B, and the fallback use this stripped prefix. Finality flags and punctuation are not stability evidence. Two samples are recorded for it:

- `prefix_sample`: the covered-through sample of the result in which the stable prefix last changed; none while it is empty.
- `progress_sample`: that same sample whenever the stable prefix changes; the trigger sample until a first non-empty stable prefix exists.

**Finalize at the pause.** The utterance's evidence copy is kept from its start. Once VAD has analyzed 10 cells (320 ms) past the end of the last speech-positive cell seen since the utterance opened, that block's ASR result is a fresh greedy decode of the evidence copy over [utterance start, block end), flushed with 1.5 s of internal zeros, and is marked `finalized`. It runs once per pause, in the lease's mic lane and so in sample order; a later speech-positive cell re-arms it. The finalized result stands in for the live stream's while the live tokens are a prefix of it, since the live trailing-blank count stops at its last chunk edge; the first live result that adds or changes a token replaces it. While it stands, its trailing blanks are the real audio since the end of the latest speech-positive cell, (covered-through sample − that end) / 640: at least 320 ms when it is made, and falling back toward zero as soon as new speech is heard, before the live stream has new tokens. They are not counted from the last token: Kroko emits the final word and punctuation late, often inside the flush, which held route A up to 1.07 s after speech end on the Office recordings. The flush never counts. Token emission samples stay as decoded. The utterance trace records the covered-through sample of every finalized result.

**Completeness and alarm grammar.** Implement one pure-Python package, `echomuse_grammar`, used only by the controller: for endpoint completeness of every family below, for local commands, and for the authoritative parse of alarm commands (section 10.5). Timer commands are only judged for completeness and then go to HA (section 10.8). Normalize lowercase, spoken digits, and English number words through 99; duration units are seconds, minutes, and hours. Results are `complete`, `extendable`, `needs_more`, or `unknown`:

| Family | `complete` examples | `needs_more` examples |
|---|---|---|
| Timer (sent to HA) | “set a [name] timer for five minutes”, “timer for 1 hour and 5 minutes”, “add 2 minutes to the pasta timer”, “pause the timer”, “cancel the pasta timer” | “set a timer for”, “timer for five”, “cancel the” |
| Alarm question | “what alarms do I have”, “when's my next alarm”, “what time does my alarm go off” | — |
| Alarm | “set an alarm for 7 am [tomorrow/weekdays/every day]”, “cancel my 7 am alarm” | “set an alarm for seven”, “wake me up at” |
| Local command | “stop”, “cancel”, “snooze”, “stop the timer”, “turn off the alarm” | “snooze for” |
| Home on/off | “turn on/off <exposed target>”, “switch on/off <exposed target>”, “<exposed target> on/off” | “turn off the” |
| Reply | `yes/no/ordinal/<offered option>` when the expectation supplies choices | — |

Targets come from section 16.7's HA vocabulary snapshot. A parse whose target is a strict token prefix of another valid target is `extendable` (for example `kitchen` beside `kitchen lights`). A terminal token in `{a, an, the, for, to, in, on, at, and, or, with}` is never complete. An alarm hour without am/pm is `needs_more`. Everything else is `unknown`. Completeness is endpoint evidence only; HA still resolves entities and permissions.

**Grammar contract.** The families are defined as follows, and a checked-in conformance corpus pins them:

- **Timer.** The English sentence templates of `home-assistant-intents` 2026.7.30, the version HA 2026.8.1 installs, for `HassStartTimer`, `HassCancelTimer`, `HassCancelAllTimers`, `HassIncreaseTimer`, `HassDecreaseTimer`, `HassPauseTimer`, `HassUnpauseTimer`, and `HassTimerStatus`. A generator script expands them with durations of 1, 2, 5, 10, 15, 30, 45, and 90 in each unit and the names `pasta`, `kitchen`, and `tea`. Each expansion is `complete`. Each proper token prefix that ends before a required slot or inside a duration is `needs_more`.
- **Alarm.**

  ```text
  alarm_set    = ("set" | "create" | "make") ["an" | "a"] "alarm" ("for" | "at") time [days]
               | "wake me" ["up"] ("at" | "for") time [days]
  alarm_cancel = ("cancel" | "delete" | "remove" | "turn off") ["my" | "the"] [time] "alarm"
               | ("cancel" | "delete" | "remove") "all" ["my" | "the"] "alarms"
  time         = hour [minute] ampm | "noon" | "midnight"
  hour         = 1..12 (digits or words) ; minute = 00..59, "oh" 1..9, or number words
  ampm         = "am" | "a m" | "pm" | "p m" | "in the morning" | "in the afternoon"
               | "in the evening" | "at night"
  days         = "today" | "tomorrow" | "every day" | "daily" | ["on"] "weekdays"
               | ["on"] "weekends" | ["every" | "on"] weekday {("and" | ",") weekday}
  ```

  An hour without `ampm` is `needs_more`.
- **Alarm question.** Questions about this speaker's alarms: listings (“what/which alarms [are set|do I have]”, “do I have any alarms”, “list my alarms”, “alarm status”) and the next alarm (“when's my [next] alarm”, “what time does my alarm go off”). Only a complete sentence matches, so there is no `needs_more` prefix. `echomuse_grammar.parse_alarm_query` is the authoritative parse the actor answers. Timer questions are HA's (section 10.8).
- **Timer cancel.** Within the timer family, `echomuse_grammar.parse_timer_cancel` is the authoritative parse of a cancel: a verb (`cancel`, `stop`, `turn off`, `delete`, `remove`, `end`, `clear`), then `[the|my|this|that] [<length>] [<name>] timer`, `the timer (called <name> | for <length> | for [the] <name>)`, or all timers (`all [of] [my|the] timers`, `both timers`, `every timer`, `[my|the] timers`). The actor acts on a cancel naming no timer, and on all timers (section 10.8); a cancel naming a length or a name goes to HA. The same parse resolves a reply to “which one?”.
- **Local command.** The patterns of section 16.2.
- **Home on/off.** The three forms in the table, with targets from the vocabulary snapshot.
- **Reply.** The aliases of the expectation's `reply_choices` (section 9.1).

`echomuse_grammar/conformance.tsv` holds one line per case: utterance, family, and expected class. It is generated for the timer family and hand-written for the others, with at least 40 cases per family including the table's examples and the prefix rules. An implementation conforms when it reproduces every line. The generator and its template version are checked in beside the corpus.

**Endpoint reducer.** Evaluate after every 80 ms mic block, once the evidence frontier (section 8.1) reaches that block's end. `frontier` is that sample. `last_cmd_end` is the end sample of the last `command_speech` cell after the trigger. `pending` is null or `{boundary, since, route}`.

```text
if muted: close(muted); if session lost: close(session_lost); if gap: close(interrupted); if overrun: close(audio_overrun). None dispatches
if no command speech and frontier − trigger_sample ≥ 5.0 s: close(no_input)
if frontier − trigger_sample ≥ 15.0 s (30.0 s with extendedUtterances): close(too_long), no dispatch

if pending:
    if ≥ 4 command_speech cells lie after pending.since: pending = null, back to LISTENING
    if pending still set and frontier − pending.since ≥ 192 ms: finalize_once(pending.boundary)
    stop here

routes are tried in the order written; the first whose conditions hold sets pending

route A, normal pause:
    command speech ≥ 192 ms
    stable prefix exists
    ≥ 15 of the last 19 cells are non_speech or self_output (or background_speech when the stable prefix is needs_more or unknown)
    trailing blanks ≥ 10 frames (400 ms); for a finalized result, real audio since the last speech-positive cell
    frontier − last_cmd_end ≥ 608 ms complete | 1,216 ms extendable | 1,792 ms needs_more/unknown
    → pending = {last_cmd_end, frontier, A}

route B, complete command under background speech:
    stable prefix parses complete, not extendable
    frontier − prefix_sample ≥ 608 ms
    ≥ 80% of speech-positive cells after prefix_sample are background_speech
    → pending = {end of the last command_speech cell at or before prefix_sample, frontier, B}

route R, reply on the ESPHome path (HA-started conversations only; replaces routes A and B):
    command speech exists
    frontier − last_cmd_end ≥ 1,024 ms
    → pending = {last_cmd_end, frontier, R}

if command speech exists and frontier − progress_sample ≥ 3.0 s:
    if the stable prefix parses complete and a command_speech cell ends at or before prefix_sample: finalize_once(end of that last cell, without route B's level test)
    otherwise: close(retry), no dispatch
```

`pending.since` is the frontier at which the decision was made, so the 192 ms lookahead is always new audio. `finalize_once` cuts canonical audio at `boundary + 192 ms`, clipped to valid audio, and submits that immutable span for final transcription. Route B and the complete-prefix fallback re-decode the span with a new streaming recognizer, flushed with 1.5 s of internal zeros; if its grammar parse differs from the committed parse except by completion of the final word, close as `retry`. Synthetic flush silence never advances pause clocks and is never submitted.

Every age and duration in this reducer is measured in acquired-audio sample time, never wall time. A delayed link therefore cannot shorten a no-input window or mistake late audio for silence; a missing range is a gap (`interrupted`), not quiet.

This makes the TV case explicit. A grammar-covered command can finish while background speech continues. A free-form request followed by TV at least 10 dB below the command finishes after route A's long pause. An uncovered free-form request finishes under equal-level TV only after a real pause; otherwise it reaches the 15 s bound and closes as `too_long`, asking for a shorter request. The design does not claim that one beam-selected microphone stream can separate arbitrary equal-level voices.

**Early answer.** While a prompt carrying a reply expectation plays, its `reply` lease gives each cell VAD, level, and a per-cell echo result. The prompt is interrupted for an early answer after 15 consecutive cells (480 ms) with VAD ≥0.85, `B` available and `E ≥ B+12 dB`, and a per-cell result of `near_end_present` or `no_reference`. The onset is the first of those cells. Otherwise the audio is retained and the prompt continues.

**Reply onset after drain.** At the guarded drain, or when the expectation starts if there is no prompt audio (section 16.2), scan the retained cells from that point − 1,000 ms onward; “drain” below means that point. The onset is the first run of 8 consecutive cells with VAD ≥0.85 that are not `self_output`. The run must start no earlier than drain − 480 ms, and ≥10 cells that are `non_speech` or `self_output` must precede it; the prompt's own echoed tail counts as quiet. A short answer given as the prompt ends is therefore found in retained audio. The reply utterance starts at onset − 300 ms.

**Choices.** When the expectation carries `reply_choices` (section 9.1), a finalized reply whose normalized text equals one alias of one choice selects that choice. Any other finalized reply is re-prompted once (“Please say AM or PM.”) with the same pending operation, and the 7 s deadline restarts after the re-prompt drains. A second non-matching reply closes the expectation, and EchoMuse says “Okay, I left it.” without applying anything. Free-form expectations accept any finalized reply. An explicit wake or button always opens a new target and may answer a still-valid question.

**Wake-phrase removal.** This runs in the controller on the final HA STT transcript, before routing. **No audio is ever trimmed**: the STT span starts with the pre-roll and always contains the wake word; only the text is changed.

Definitions:

- `words(text)`: lowercase; every character other than letters, digits, and apostrophes becomes a space; split on spaces. Each word keeps its character span in the original transcript. Word indices are 0-based.
- `distance(window)`: the window's letters joined, Levenshtein distance to the registry entry's `wake_phrase` spelling (`ophelia` for the deployed model), divided by the longer of the two lengths.
- A window of 1–3 consecutive words **matches** when its distance is ≤ 0.45. It is **minimal** when it matches and neither the window without its first word nor the window without its last word matches.

Procedure:

1. Only wake-initiated turns are eligible. Button turns, reply turns, and replies to HA-started conversations never strip anything.
2. **Locate the phrase in the streaming transcript.** Among minimal windows whose last token was emitted by the end of the verification lookahead (candidate open + 480 ms), choose the lowest distance, then the earliest. Let `s` be its start index. If none matches, let `s` = 0: the wake phrase is assumed to open the utterance, since Kroko can drop a quiet wake word outright [D7].
3. **Strip it from the final transcript.** Among minimal windows starting at index ≤ `s` + 1, choose the lowest distance, then the start closest to `s`, then the earliest. Remove the original transcript from its beginning through the end of that window, plus any punctuation and whitespace that follow, and keep the rest verbatim. If none matches, send the transcript unchanged.
4. **Streaming text for local commands.** Cut the streaming transcript the same way through the end of step 2's window, then apply section 16.2's normalization to the remainder. Without a window nothing is cut.

This removes the wake phrase and anything said before it in the pre-roll (“Damn it. Oh hell yeah, Ophelia. …” keeps only what follows “Ophelia.”). It never removes a later mention: “Ophelia, what does Ophelia mean?” becomes “what does Ophelia mean?”, and when STT drops the leading wake word, “what does Ophelia mean” is unchanged, because its match starts at index 2 > `s` + 1 = 1. With no streaming window, “Ophelia, what's the weather?” becomes “what's the weather?”, and a preamble longer than one word keeps the transcript unchanged (“Damn it, Ophelia, …” matches at index 2).

Measured on real data [D4]: step 2 found the phrase in all 23 genuine wake clips' streaming transcripts. Over 233 HA transcripts and 18 streaming transcripts without the wake word, the only matches were two HA transcripts consisting solely of “Sophia.”, most likely the wake word itself misheard. Replayed cases include “Turn off the lights.” with the wake word dropped by STT (left unchanged), “Ofelia …” and “Sophia, …” (stripped), and “Oh, feel ya. Stop.” (not matched, sent unchanged). The replay used the looser bound “index ≤ number of words through the streaming window + 1”; step 3's bound is never looser, so it strips no more. The 0.45 cut-off is recalibrated in section 13. A later live turn [D7] missed step 2, so the 23 of 23 does not hold in general; the same 233 transcripts bound the position-0 fallback's false strips at those two “Sophia.” transcripts, which become empty and close as `empty_transcript`.

### 16.7 Home Assistant contract (stock HA only)

Target the installed **HA 2026.8.1** [H4][H5][H6]. There is no custom integration, no core patch, and no hand-edited HA configuration. Every interface below is a stock websocket or REST command, a stock integration (ESPHome, Local Calendar, Script), or a stock entity. At startup the controller exercises each one it depends on with a harmless call. A failure appears on the dashboard and disables only the dependent feature.

**Credentials.** Docker and bare metal use `HA_URL` and `HA_TOKEN`, a long-lived token of an HA **administrator**. The HA add-on uses `homeassistant_api: true` and `SUPERVISOR_TOKEN` via `ws://supervisor/core/websocket`. Administrator rights are needed because subscribing to custom events, `fire_event`, script configuration, entity exposure, config flows, and registry listing are admin-only [H6].

**Endpoint identity and satellite.** The controller keeps emulating one ESPHome device per Dot. Its HA device ID comes from `config/device_registry/list`, matched by MAC connection (Office: `b285ec5e94b98f4c1cbeabf926be4b77`).

- The emulated satellite advertises `VOICE_ASSISTANT | API_AUDIO | ANNOUNCE | TIMERS | START_CONVERSATION`, the same features as today. With `TIMERS`, HA registers its timer handler for the speaker and offers LLM agents its native timer tools (section 10.8) [H3][H5].
- The satellite's stock pipeline select chooses the pipeline; the stock `assist_pipeline/pipeline/list` command maps its name (or “preferred”) to a pipeline ID.
- Besides the existing media player, button event, and ambient-light entities, the emulated device exposes three stock ESPHome entities: binary sensor “Alert ringing”; button “Stop alert”, which ends whatever is ringing (timer or alarm) exactly like the physical button; and text sensor “Voice state”, the actor state's phase (`ActorState.phase`: `idle` for `IDLE`/`CLOSING`, `listening` for `ARMED`/`LISTENING`/`END_PENDING`/`EXPECT_REPLY`, `thinking` for `COMMITTED`/`THINKING`, `speaking` for `SPEAKING`), the same projection the LED ring and dashboard use. It exists because wake, button and EchoMuse reply-expectation turns are websocket runs, which never move HA's own assist-satellite state. The calendar entity shows the next alarm.

**Voice turns** (wake, button, EchoMuse reply expectations, and `continue_conversation`) run in this order:

0. **Local commands first.** At commit, if section 6.3's conditions hold on the streaming text (section 16.2), the actor executes the command on the device and closes the turn. No HA run happens, so voice stop works while HA or its STT is down.
1. **STT only**, on the committed span, through stock `assist_pipeline/run` [H4]:

   ```json
   {"type":"assist_pipeline/run","start_stage":"stt","end_stage":"stt",
    "input":{"sample_rate":16000,"no_vad":true},
    "pipeline":"<id>","device_id":"<HA device>","timeout":15}
   ```

   Read `runner_data.stt_binary_handler_id` from `run-start`. Send the STT copy as binary messages of at most 64 KiB, each prefixed with that handler byte, then a message containing only the handler byte to end it. `no_vad` disables HA's segmenter, so HA transcribes exactly the committed span; the installed schema accepts it (verified in the container). Read `stt-end` → `stt_output.text`. STT has no side effects, so a transport failure is retried once.

   The **STT copy** is the committed canonical span with the existing wake-level ASR gain (`asr_gain_for`/`apply_asr_gain`, nominal +20 dB, clamped) and, when `nsAsr` is on, DTLN denoising. Raw native-AFE level is about −40 dBFS, where the deployed faster-whisper drops quiet leading words. These transforms touch only the STT copy, never wake, endpoint, or reference evidence.
2. **Route** the final transcript after wake-phrase removal (section 16.6). An alarm-grammar match goes to the alert engine (section 10.5), and so does an alarm question. “Cancel the timer” and “cancel all timers” run HA's own timer intents for this speaker through the stock intent API (section 10.8):

   ```json
   POST /api/intent/handle
   {"name":"HassCancelTimer","data":{"start_minutes":10},"device_id":"<HA device>"}
   ```

   `HassTimerStatus` returns every device's timers in `speech_slots.timers`, each with its `device_id`. HA answers a failed intent with HTTP 200 and `response_type` `error`. The startup probe runs `HassTimerStatus`, which changes nothing. Everything else goes to step 3, including every other timer command, which HA's local timer intents answer.
3. **Intent through TTS:**

   ```json
   {"type":"assist_pipeline/run","start_stage":"intent","end_stage":"tts",
    "input":{"text":"<transcript after wake-phrase removal>"},"pipeline":"<id>",
    "device_id":"<HA device>","conversation_id":"<id or null>","timeout":30}
   ```

   Read `intent-end` → `intent_output` (`response.speech`, `conversation_id`, `continue_conversation`), the response URL, and any `error`. This keeps the pipeline's conversation agent, `prefer_local_intents`, TTS engine and voice, and HA's debug trace.

   **Fetch the response URL only once HA has decided what the response is.** `run-start` → `tts_output.url` with `stream_response: true` is remembered, not fetched; it becomes fetchable on the first `intent-progress` with `tts_start_streaming: true` (HA sends it only for a streaming agent's reply longer than 60 characters), where it plays while the agent is still generating. Otherwise the URL is `tts-end` → `tts_output.url`. This is the rule HA's own ESPHome satellite follows. An earlier fetch is unsafe: a local intent targeting the satellite's own area sets `acknowledge_override`, HA then overrides the stream's result with the acknowledge sound, and a `/api/tts_proxy` GET that arrived before the override waits forever (measured on 2026.8.1 with HA's own `ResultStream`). The ESPHome path applies the same rule to `VOICE_ASSISTANT_RUN_START` `url`, `VOICE_ASSISTANT_INTENT_PROGRESS` `tts_start_streaming` `"1"`, and `VOICE_ASSISTANT_TTS_END` `url`.

   The actor allows 30 s from sending the run to `intent-end`, matching HA's `"timeout":30`. Past 30 s the actor closes the turn as `ha_timeout`, and later events of that run are fenced. After `intent-end`, response audio must start within 10 s, streamed or not (section 7). **Never resubmit after dispatch:** a lost connection means `outcome_unknown`.
4. **EchoMuse's own speech** (alarm confirmations, alarm questions, the timer cancels of step 2, clarifications, errors): the same command with `start_stage` and `end_stage` both `"tts"` and `input.text`, so it uses the configured TTS engine and voice. A confirmation or answer plays as the turn's own dialog output, so the turn's row records what was said and how it played.

TTS URLs are unauthenticated capability URLs that expire after about 300 s by default [H4]. Fetch immediately, never cache, decode to 48 kHz mono, and stream as dialog output. A failed fetch ends as `render.finished(reason=failed)`, not a drain. HA chat sessions expire after 5 minutes idle and are not restored [H5]; a stale conversation ID silently starts a new conversation, and EchoMuse's 60 s chain cap remains the reply limit.

**HA-started conversations** (`assist_satellite.start_conversation`). HA stores the automation's extra system prompt and a new conversation ID on the satellite entity, and applies them only to the next pipeline the satellite starts over the ESPHome API [H6]. These replies therefore take the ESPHome path, not the websocket runs:

- HA sends an announce request with `start_conversation=true`. The controller plays it as dialog output and opens a reply expectation (section 9).
- After the reply commits, the controller sends the ESPHome voice-assistant start request with no wake-word stage and streams the committed span's STT copy, then ends the stream. HA runs STT → intent → TTS with its stored prompt and conversation, and returns results through the ESPHome voice events the controller already handles.
- HA always runs its segmenter on this path, with the silence limit set by the satellite's VAD-sensitivity select [H6]. At setup the controller sets that select to `relaxed` (1.25 s). For these replies only, the reducer uses its ESPHome-reply route (section 16.6): any 1,024 ms pause after command speech commits, so no pause inside the committed span exceeds 1,216 ms and HA's 1.25 s segmenter cannot cut inside it. If HA's own VAD still ends the stream early, HA's transcript is used as returned.
- A reply contains no wake phrase, so nothing is stripped. A following `continue_conversation` continues on the websocket path with the returned conversation ID.

**Announcements** arrive through the ESPHome `ANNOUNCE` path and play as `dialog_output` owned by an announcement generation. Send `AnnounceFinished` after the guarded drain or on cancellation. Announcements queue behind active dialog output and are preempted by foreground alerts.

**Timers:** HA's native timer events over the ESPHome API (section 10.8). **Alarms:** the stock calendar commands in section 16.4, on one Local Calendar per endpoint.

**Calendar provisioning.** Per endpoint, use the Local Calendar config entry ID and `calendar.*` entity stored in `alert_delivery`. Only when none is stored, find the entry titled `EchoMuse <label>` and its entity (`config/entity_registry/list`), and store both. Renaming an endpoint changes neither. If the entry is missing, create it with the stock config flow: `POST /api/config/config_entries/flow` with `{"handler":"local_calendar"}`, then `POST /api/config/config_entries/flow/<flow_id>` with `{"calendar_name":"EchoMuse <label>","import":"create_empty"}` [H6].

**LLM alarm tools (stock scripts).** Timers use HA's native timer tools. For alarms and for stopping a ring, the controller installs five scripts and exposes them to Assist:

| Script | Fields | Result returned to the LLM |
|---|---|---|
| `script.echomuse_set_alarm` | `time` (required), `days` (weekdays; empty means once), `name`, `speaker` | stored/armed facts and the first due time |
| `script.echomuse_list_alarms` | `speaker` | the next alarms on the speaker, at most 10 |
| `script.echomuse_cancel_alarm` | `name`, `time`, `all`, `speaker`; one of `name`, `time`, or `all: true` is required | what was cancelled |
| `script.echomuse_dismiss_alert` | `speaker` | the timer or alarm that was ringing, now stopped; never deletes a repeating series |
| `script.echomuse_snooze_alarm` | `speaker` | the ringing alarm's child occurrence and its new due time; rejects timers and a speaker with nothing ringing |

All five follow one pattern. `request_id` joins the conversation's context ID, which HA passes to every tool call of the turn, with the action and its arguments. A retried call therefore gets the same ID, and two different calls in one turn get different IDs.

```yaml
echomuse_set_alarm:
  alias: Set an alarm on an EchoMuse speaker
  description: >-
    The only way to set a clock-time alarm. Alarms are saved in Home Assistant's
    calendar and ring on the speaker. Set speaker to the area or speaker the user
    is talking through unless they name another. Use the timer tools for
    countdown timers.
  mode: parallel
  fields:
    time: {required: true, selector: {time: {}}}
    days:
      selector:
        select: {multiple: true, options: [mon, tue, wed, thu, fri, sat, sun]}
    name: {selector: {text: {}}}
    speaker: {selector: {text: {}}}
  variables:
    echomuse_revision: 1
  sequence:
    - variables:
        args:
          time: "{{ time }}"
          days: "{{ days | default([]) }}"
          name: "{{ name | default('') }}"
          speaker: "{{ speaker | default('') }}"
        request_id: "{{ context.id }}|set_alarm|{{ args | tojson }}"
    - event: echomuse_alert_request
      event_data: {request_id: "{{ request_id }}", action: set_alarm, args: "{{ args }}"}
    - wait_for_trigger:
        - trigger: event
          event_type: echomuse_alert_result
          event_data: {request_id: "{{ request_id }}"}
      timeout: "00:00:10"
    - variables:
        result: >-
          {{ wait.trigger.event.data.result if wait.trigger
             else {'ok': false, 'error': 'EchoMuse did not answer'} }}
    - stop: done
      response_variable: result

echomuse_list_alarms:
  alias: List alarms on an EchoMuse speaker
  description: >-
    Lists the next alarms on a speaker. Set speaker to the area or speaker the
    user is talking through unless they name another.
  mode: parallel
  fields:
    speaker: {selector: {text: {}}}
  variables:
    echomuse_revision: 1
  sequence:
    # Same sequence as echomuse_set_alarm with action list_alarms and
    # args: {speaker: "{{ speaker | default('') }}"}.

echomuse_cancel_alarm:
  alias: Cancel alarms on an EchoMuse speaker
  description: >-
    Cancels an alarm by name or time, or every alarm with all. Cancelling a
    repeating alarm removes the whole series. Set speaker as for the other
    EchoMuse tools.
  mode: parallel
  fields:
    name: {selector: {text: {}}}
    time: {selector: {time: {}}}
    all: {default: false, selector: {boolean: {}}}
    speaker: {selector: {text: {}}}
  variables:
    echomuse_revision: 1
  sequence:
    # Same sequence with action cancel_alarm and args:
    # {name: "{{ name | default('') }}", time: "{{ time | default('') }}",
    #  all: "{{ all | default(false) }}", speaker: "{{ speaker | default('') }}"}.

echomuse_dismiss_alert:
  alias: Stop the ringing timer or alarm on an EchoMuse speaker
  description: >-
    Stops whatever timer or alarm is ringing on the speaker. A repeating alarm
    still rings on its next day. Set speaker as for the other EchoMuse tools.
  mode: parallel
  fields:
    speaker: {selector: {text: {}}}
  variables:
    echomuse_revision: 1
  sequence:
    # Same sequence with action dismiss_alert and args: {speaker: ...}.

echomuse_snooze_alarm:
  alias: Snooze the ringing alarm on an EchoMuse speaker
  description: >-
    Snoozes the alarm ringing on the speaker. Timers cannot be snoozed. Set
    speaker as for the other EchoMuse tools.
  mode: parallel
  fields:
    speaker: {selector: {text: {}}}
  variables:
    echomuse_revision: 1
  sequence:
    # Same sequence with action snooze_alarm and args: {speaker: ...}.
```

**Generation schema (normative).** The shown `echomuse_set_alarm.sequence` is the template `relay(action,args)`. Each other script's emitted YAML replaces its comment with that exact sequence, after these substitutions; no other keys vary:

| Object ID | `action` | `args` object |
|---|---|---|
| `echomuse_list_alarms` | `list_alarms` | `{speaker: "{{ speaker | default('') }}"}` |
| `echomuse_cancel_alarm` | `cancel_alarm` | `{name: "{{ name | default('') }}", time: "{{ time | default('') }}", all: "{{ all | default(false) }}", speaker: "{{ speaker | default('') }}"}` |
| `echomuse_dismiss_alert` | `dismiss_alert` | `{speaker: "{{ speaker | default('') }}"}` |
| `echomuse_snooze_alarm` | `snooze_alarm` | `{speaker: "{{ speaker | default('') }}"}` |

For each, `request_id` is exactly `"{{ context.id }}|<action>|{{ args | tojson }}"`; the event data is exactly `{request_id: "{{ request_id }}", action: <action>, args: "{{ args }}"}`; and the 10-second wait/result/stop steps are byte-for-byte those in `echomuse_set_alarm`. The controller renders the five full scripts from this schema before hashing and installing them.

- **Relay.** The controller subscribes with `subscribe_events` (`event_type: echomuse_alert_request`), applies the operation through the alert engine, and answers with `fire_event` `echomuse_alert_result`, whose data is `{"request_id", "result"}`. Assist returns a script's response to the model (a blocking call with `return_response=True`) [H6].
- **Result timing.** The engine answers when the operation is applied, or 5 s after the request with `{"ok": true, "stored_in_ha": false, "pending": true, "op_id": "…"}`, so a slow calendar confirmation is never reported as a failure and a tool call fits inside the actor's intent deadline. The script waits 10 s. A pending operation still completes, and the dashboard shows its final state.
- **Validation.** A missing required field, a `cancel_alarm` without `name`, `time`, or `all: true`, or a `time` that is not `HH:MM[:SS]` returns `{"ok": false, "error": "<reason>"}` without a write.
- **Idempotency.** The operation ID is UUIDv5(`NAMESPACE_URL`, `request_id`), so a retried tool call is idempotent.
- **Speaker resolution**, in order:
  1. the `speaker` field, matched case-insensitively against endpoint names and then area names;
  2. otherwise the single EchoMuse turn currently waiting on an intent run;
  3. otherwise `{"ok": false, "error": "which speaker?"}`.

  Two speakers with intent runs in flight at once therefore make an empty `speaker` ambiguous, and the model asks. HA gives scripts no device context to do better.
- **Installation.** `POST /api/config/script/config/<object_id>`, then `script.reload`, then `homeassistant/expose_entity` with `assistants: ["conversation"]`. The controller records the SHA-256 of what it wrote. If a script's current config differs from that hash (a user edit), it does not overwrite it and reports a warning. It upgrades only when `echomuse_revision` changes and the hash still matches.

**Vocabulary for the grammar.** Exposed entity IDs from `homeassistant/expose_entity/list`, their names and aliases from `config/entity_registry/get_entries`, plus `config/area_registry/list` and `config/floor_registry/list`. Refresh at connect and on `entity_registry_updated`, `area_registry_updated`, and `floor_registry_updated` events.

**Sounds.** The sound catalog lives in the controller only. Config values name catalog sounds by ID; the alert engine resolves an ID to its asset's SHA-256 when it writes an event or sends `alert.ring`, and an ID that no longer resolves becomes `builtin:fallback` and is flagged. EchoMuse alarm events reference a sound by SHA-256 in their `echomuse:` line. Other alarm events use the endpoint's effective `alarmSound`, and timer rings its effective `timerSound`: the device override if set, else the fleet value. The device fetches assets by hash with its cache snapshot or `alert.ring`, and a missing asset rings the fallback tone.

## 17. Resolved questions

| Question previously open | Decision | Evidence / reason |
|---|---|---|
| Which BCResNet contract? | Deployed audio-in graph `4eb74512…` plus sidecar `25da0c65…`; 160 ms hop, 3-window mean, RMS floor, peak normalization, softmax index 1 | Live controller active asset, logs, graph inspection, inference [D1] |
| Is the repository model current? | No. Replace its fixture with the deployed graph at cutover | Same IO/sidecar, 1.5× wider deployed weights [D1] |
| Train a new wake model? | No. Keep the deployed model: 0.90 idle and during TTS, 0.65 during content or alerts, plus ASR verification of every candidate while the device produces sound. A playback-condition fine-tune is triggered only by the section 5.4 criteria | Follows native keying and the ×0.71 ratio [N2]; the live 0.10 bar false-triggered on the device's own answers, and the verifier rejects those clips [D2][D3] |
| Wake verification? | Second stage over the candidate audio with the pinned streaming ASR and a fuzzy alias test; only while the device produces sound | Native two-stage spotter [N2]; 23 of 24 recent idle wakes pass (the failure captured no command), 4 of 4 barge-in clips fail [D3] |
| Run BCResNet on the Dot? | Yes. The Dot scores the mic continuously with the controller-named, hash-verified graph, proposes candidates, and uploads audio only under leases. The controller accepts, attributes, and scores the reference on demand | 43% of one A53 core at the production cadence; idle speakers then cost the controller nothing and stream no audio; the old “skipped frames” were the hop, and the stale copy is fixed by hash-named assets [D6] |
| Which other models run on the Dot? | None. Silero VAD and Kroko run on the controller, only inside leases; the Dot computes per-cell loudness | Both crash under the Dot's runtime unless graph optimization is disabled; then 6.5 ms per VAD cell and an estimated 0.9× real time for ASR [D6] |
| Stop/snooze detector? | Wake word, then a local match of the streaming-ASR text (section 6.3); no bare-word listening | Deployed labels have no command classes; mirrors native “Alexa, stop” [N3]; user decision |
| Final STT? | Endpoint's HA STT engine on the committed span | Preserves `stt.faster_whisper`; the streaming ASR is evidence-only [H5] |
| Streaming ASR model? | Pretrained Kroko 2025-08-06, greedy, on the +20 dB evidence copy | 9.4% WER on Dot recordings at every level; the 20M model 72–100% [P2] |
| VAD model and input? | Raw Silero v5 probabilities with carried state and context, on the evidence copy, on the controller within leases | v4 and raw-level input missed Dot speech; the sherpa wrapper is segment-only [P1][P2] |
| New model fits required? | None to build or ship. Conditional: a BCResNet playback fine-tune only if the section 13 playback gates fail | Every model is either deployed (BCResNet) or pretrained, and measured on Dot audio where data exists [D2][D3][P2] |
| Speaker gating? | None in the initial implementation; speaker takeover is backlog (section 19.1) | Only a two-clip sanity test exists [P1]; published approaches use trained per-frame models |
| TV-room endpoint? | Grammar-complete background-speech route; background speech ≥10 dB down counts as pause after `needs_more`/`unknown` text; bounded retry | Equal-level voices are not separable from one stream; speech ≥10 dB down is, and a 1,792 ms pause outlasts soft stretches inside a command |
| Unsolicited follow-up? | Not implemented | No available directedness/NTT model |
| Native AFE metadata? | Fire OS 6: decoded on the device and carried as evidence under `afe_metadata_v1`; no decision reads it. Fire OS 5 and direction/wake-time energy: `unavailable` | v3.3 validated per frame on the device (section 4.5); no measurement yet relates a field to an outcome |
| Presentation timing? | Estimated callback/sample accounting plus 150 ms drain guard | No measured DAC timestamp API; guard is a release gate |
| HA voice entry? | Stock `assist_pipeline/run`: STT only with `no_vad`, then intent→TTS; the controller removes the wake phrase between them | No custom integration allowed; schema validated in the installed HA [H4][H6] |
| Custom HA integration? | None. Stock websocket/REST APIs, Local Calendar, scripts, and ESPHome entities only | User decision; every surface verified in the installed HA [H6] |
| Alarm store? | One stock Local Calendar per speaker; controller alert engine and journal; handled occurrences are deleted | Persisted, repeating, single-occurrence delete, change subscription, UI-editable [H6] |
| Timers? | HA's native satellite timers, exactly as on HA's own voice satellites: the satellite advertises `TIMERS`, HA holds timers in memory, and the Dot rings on HA's `finished` event. Not durable across HA restarts, like those devices | User decision; matches Voice PE, ESPHome, and Wyoming satellites [H3][H7] |
| START_CONVERSATION compatibility? | Kept. Replies take the ESPHome path so HA applies the stored prompt; the reply commits after any 1,024 ms pause and is streamed only after commit, so HA's 1.25 s segmenter cannot cut inside | HA applies the stored prompt only on the satellite path [H6] |
| Wake-phrase removal? | Text-only, in the controller, gated by the streaming transcript; no audio trimmed | Deterministic; 23/23 located, no false strips on real transcripts [D4] |
| ASP TTS/alarm notifications? | Not sent | They reach only the disabled playback leveller [D5] |
| Alarm executor wakeup? | Root supervisor holds `/sys/power/wake_lock` while alarms are armed | Server runs as UID 0; nodes exist [D1] |
| What existing code is replaced? | Section 18 inventory, mapped at commit `bd73e25` | Source reads of every listed symbol |

No architectural or interface question remains open. Deferred features are listed in section 19 and are outside the initial implementation. Section 14's measurement gates determine whether numeric defaults ship unchanged; a failed gate changes a policy value or blocks release, not ownership or protocol.

## 18. Replacement inventory: what existing EchoMuse code this replaces

Mapped from the source at commit `bd73e25`. Existing code was read only to learn what it currently does; none of it defines how the new design should behave. Dispositions:

- **REMOVED** — deleted; nothing takes over its job.
- **REPLACED** — deleted; the named new component takes over the job.
- **MODIFIED** — the file stays; only the listed parts change.
- **RETAINED** — unchanged by this effort (callers may move).

The fleet-wide cutover (section 12) deletes everything REMOVED or REPLACED. The only old-protocol code left is the upgrade-only legacy handler, which takes no action and is deleted one release later. No dashboard control survives that no longer affects anything.

### 18.1 Controller runtime (`controller/`)

| Current code | What it does today | Disposition | Successor |
|---|---|---|---|
| `em_controller.wake_word_listener` | Per-device loop over the always-on stream: openWakeWord or BCResNet scoring; skips scoring while `device.speaking` unless a timer rings; swaps to `bargeInThreshold` during a ring and routes a ring-time wake to `stop_timer_ring`; noise-floor tracking; device-shadow correlation; near-miss counting; starts turns | REPLACED | Device wake detector and `wake.candidate` handling (§5.2, §16.2), on-demand reference scoring and attribution (§6.1), arbiter wiring (§5.3), wake-prefixed local commands (§6.3; a wake alone no longer stops a ring), session actor (§7) |
| `em_controller._barge_watcher` | Second listener during TTS/music; sends `speaker_flush` on detection | REPLACED | The single wake path plus interruption policy (§6) |
| `_run_timer_ring`, `start_timer_ring`, `stop_timer_ring`, `leds_timer_ring`; `Device.timer_ringing`, `timer_ring_stop`, `timer_ring_task`, `timer_ring_audible`, `timer_sound`, `timer_ring_seconds`, `timer_ring_gap`, `timer_ring_burst`; `RING_GAP_SECONDS`, `RING_MIN_BURST_SECONDS` | Controller-rendered ring started by HA's timer-finished event, capped at `timerRingSeconds` because HA has already discarded the timer | REPLACED | `alert.ring` to the device alert executor on HA's timer `finished` event (§10.8, §16.1) |
| `_run_voice_locked` including its continuation loop; `_run_post_turn_playback`, `_run_streaming_post_turn_playback`, `_meter_at_playback_start` | Voice lock, `em_player.interrupt`/`resume_interrupted`, barge watcher start, TTS streaming and prime-wait drain, re-trigger on `continue_conversation` | REPLACED | Session actor (§7, §9, §16.2); `render.*` protocol (§16.1) |
| `Device.stream_speaker`, `stream_speaker_chunks`, `send_data`/`begin_data_stream` grace, `mic_start`/`mic_start_turn`/`mic_stop`, `play_wake_sound`; `SPEAKER_*`, `MIC_FRAME_TYPE`, `VAD_END_TYPE`, `VAD_NO_SPEECH_TIMEOUT_TYPE`, `SPEAKER_*_TYPE`, `MIC_HEADER_LEN` | 0x01–0x05 data plane, `lock_mic` turn streams, chime trigger | REPLACED | `/device/v1/audio` EMA1 frames, `uplink.*` leases, and `render.*` (§4.4, §16.1); chime becomes an `earcon` source |
| `Device.shadow`, `ctrl_shadow`, `oww_on_device`, `pending_wake`, `oww_shadow_capable`, `oww_trigger_capable`, `oww_bcresnet_capable`, `oww_model`, `oww_threshold`, `barge_*`, `endpoint_*`, `timer_*` fields; `OWW_MODEL`/`OWW_THRESHOLD` env reads | Per-device state for the removed paths | REMOVED | — |
| `handle_button_event` | Tap/hold via `em_button.decide(ring_active=device.timer_ringing)`; tap during ring calls `stop_timer_ring` | MODIFIED | Consumes `button.action`; stop targets the device-reported `occurrence_id` (§6.3, §16.1) |
| `handle_control`, `handle_data`, `_route` | Legacy `/control` and `/data` planes, registration, capability parsing | MODIFIED | `/device/v1/control`, `/device/v1/audio`, and `/device/v1/assets` (§16.1); capability set (§11.1). The old `/control` path keeps only the upgrade-only legacy handler for one release (§12); `/data` is REMOVED. `/shell` plane RETAINED |
| `leds_listening`, `leds_spin_green`, `_leds_turn_end`, `_OUTCOME_ANIM` | Turn-state ring driven from inside the voice turn | MODIFIED | Rendered from actor state (§11.2); outcome cues keyed to the new terminal reasons |
| `set_collect_mode`/`_collect_frame`, `_ambient_*`, `_capture_*`/`open_capture_window`/`take_recording` | Recording modes tapping the wake stream; device does not answer while one runs | MODIFIED | Hold a `diagnostic` uplink lease for the mode's duration and tap its timeline (§4.4). While a mode runs the actor accepts no wake; alerts still ring and physical stop still works |
| Ping/RTT, mDNS, TLS/link auth, shell, loop-lag monitor, `main` | Infrastructure | RETAINED | — |
| `em_esphome.VOICE_ASSISTANT_FLAGS` = VOICE_ASSISTANT \| API_AUDIO \| ANNOUNCE \| TIMERS \| START_CONVERSATION | Makes HA run Assist turns and native timers on this satellite | MODIFIED | Flags unchanged, including `TIMERS`; add binary sensor “Alert ringing”, button “Stop alert” and text sensor “Voice state” (§16.7) |
| `EchoMuseSatellite.run_esphome_voice_turn`, `_stream_mic_audio`, `_handle_voice_event`, `trigger_voice_turn`, `cancel_voice_turn`; `VAD_SENTINEL_*`, `VOICE_PREROLL_DISCARD`, `_conversation_id`, `_continue_conversation` | Streams mic to HA Assist; HA VAD or the relative endpointer ends the stream; reacts to HA STT/intent/TTS events; fixed 240 ms preroll discard | REPLACED / MODIFIED | Wake, button, and reply turns: endpoint reducer + committed span + stock websocket runs (§8, §16.6, §16.7). The ESPHome pipeline path is kept only for replies to HA-started conversations, streaming a committed span. The preroll discard is replaced by text-only wake-phrase removal |
| `asr_gain_for`, `apply_asr_gain`, `ASR_*` constants; the `em_ns` hook | Gain (and optional DTLN) on the STT-bound copy only | RETAINED; moved | STT-copy builder for both STT paths (§16.7) |
| `_handle_timer_event` | HA timer events → `start_timer_ring` | MODIFIED | Keeps a display copy of each timer from all four events; on `finished` sends `alert.ring` (§10.8) |
| `_start_conversation_turn` and the `start_conversation` announce branch | Opens a turn after an HA-started prompt | MODIFIED | Reply expectation whose reply streams over the ESPHome path with the 1,024 ms pause route (§9.1, §16.6, §16.7) |
| `_fetch_and_play_announce`, `VoiceAssistantAnnounceRequest`/`play_media announce` handling | Announcement playback via turn or `_standalone_play` | MODIFIED | `dialog_output` announcement generation; `AnnounceFinished` after guarded drain (§16.7) |
| `available_wake_words` in the configuration response, `oww_model_id`, `update_oww_model` (and its callers in `em_api`/`em_controller`) | Reports the wake word to HA; reconnects HA on model change | REMOVED | — |
| `TurnTrace` and its `[TURN]` log line | Per-turn stage timings | REPLACED | Decision trace (§11.3) persisted to `turns` (§18.4) |
| `_save_utterance`, `_save_wake_clip`, `_persist_turn`, `_record_dropped_turn` | Turn row, utterance WAV, wake clip | MODIFIED | Utterance = the uploaded STT copy; wake clip = accepted candidate support −300 ms…`support_end`; new trace columns |
| Media-player entity, button event entity, ambient-lux sensor, `_stream_tts_audio`/`_fetch_tts_audio`, server lifecycle, mDNS | HA entities and TTS decode | RETAINED | TTS fetch/decode is reused for pipeline-run TTS URLs |
| `em_shadow.py` (whole module) | Device/controller wake correlation and source choice | REMOVED | — |
| `em_wake_scorer.OwwScorer`, openWakeWord branches of `build`, `classify_model_shape`, `describe_accepted_shapes`, `speex_ns` | openWakeWord scoring | REMOVED | — |
| `em_wake_scorer.BcresnetScorer`, `parse_spec`, `load_spec`, `softmax`, `onnx_infer` | Deployed BCResNet scoring (160 ms hop, 3-window mean) | MODIFIED | On-demand reference scoring on the reference hop grid, and the registry's upload probe (§5.1, §5.2) |
| `em_oww_models.py` (`scan`, `prediction_key`, `models_dir`, `safe_model_filename`, `in_use_by`) and `em_wake_scorer.is_bcresnet_model` | Custom openWakeWord/BCResNet model files in `oww_models/` | REPLACED | BCResNet model registry (§5.1) |
| `em_oww_assets.py` | Plans ORT runtime/model pushes to the Dot | REPLACED | `em_device_assets`: the speech-asset set per device (runtime, wake graph, sidecar) named in `session.ready` and served by hash over `/device/v1/assets` (§16.1, §16.5) |
| `em_endpoint.py` | Relative level endpointer, seeded by wake loudness | REPLACED | Endpoint reducer (§16.6) |
| `em_turnclock.py` | No-speech verdict measured from the first real frame | REPLACED | Sample-time ages in the reducer (§16.6) keep the same lesson |
| `em_player.interrupt`, `resume_interrupted`, `MediaSession.owned_by_turn/pending/resume_after/ducked`, `duck`/`music_flush`/`speaker_flush` sends | Pauses or ducks music for a turn; defers user pause/resume/stop until the turn ends | MODIFIED | Content focus-lease client (§6.2). The deferred-command rule is kept: a user command during dialog applies at release |
| `em_player.MediaSession` decode, bookmark, seek, HA state reporting | Music session | RETAINED | Streams to the `content` source |
| `em_sounds.py` | Sound upload/decode; `ring_pcm` resolves id → fleet default → `builtin_pcm` tone | MODIFIED | Catalog IDs stay the user-facing names and config values. Each sound is also exported as an alert asset: 48 kHz mono PCM16 WAV keyed by SHA-256, cut to its first 10 s with a 50 ms fade-out and flagged on the dashboard when longer (§16.5). `ring_pcm` REMOVED; `builtin_pcm` REPLACED by the device-embedded fallback tone (§16.5) |
| `em_button.decide` | Gesture policy with `ring_active` bool | MODIFIED | `active_occurrence_id` instead of a bool |
| `em_arbiter.WakeArbiter` | Multi-device first-claim suppression | RETAINED | Wired after attribution (§5.3) |
| `em_ns`, `em_recordings`, `em_wakeclips`, `em_samples`, `em_ambient`, `em_capture` | DTLN; diagnostic stores; recording modes | RETAINED | Inputs change as noted above |
| `em_tap_burst`, `em_volume`, `em_eq`, `em_scenes`, `em_ble_proxy`, `em_auth`, `em_pki`, `em_linkauth`, `em_support`, `version` | Unrelated to turn control | RETAINED | — |
| `em_api` routes `/api/oww_models`, `/api/oww_models/upload`, `/api/oww_models/{file}` | Model list/upload/delete | REPLACED | `/api/wake_models` (list), `/api/wake_models/upload` (validate + probe + register), `/api/wake_models/{sha256}` (delete if inactive) |
| `em_api` routes `GET/POST /api/devices/{id}/oww_assets` | Device model sync | REPLACED | `GET /api/devices/{id}/speech_assets`: installed hashes and the latest `wake.stats` |
| `em_api` `/api/sounds*`, `/api/devices/{id}/sounds/test`, `/api/devices/{id}/sounds/stop` | Sound library; ring test | MODIFIED | Catalog management; preview plays through `render.start` as an `alert`-class preview with no calendar occurrence |
| `em_api` config application (`owwModel`, `owwSpeexNs`, endpoint/VAD/timer keys) | Pushes removed keys to live devices | MODIFIED | Per §18.4 key table |
| `em_start.py` add-on option map `oww_model`, `oww_threshold` | Add-on → env bridge | MODIFIED | Remove both; the add-on uses the Supervisor token (§16.7) |
| `em_db`, `em_config_sections` | Defaults, sections, migrations | MODIFIED | §18.4 |
| — | — | NEW | `em_audio_timeline` (EMA1 parse, cell records, epochs, uplink leases, per-lease timelines, clock fit), `em_speech_worker` (executor, VAD, on-demand reference scoring, ASR), `em_attribution` (reference comparison, pure), `em_endpoint_policy` (reducer, pure), `em_wake_phrase` (removal rule, pure), `em_session` (actor), `em_alerts` (alert engine, journal, calendar encoding, merge, delivery), `em_ha_client` (stock websocket/REST: pipeline runs, calendar, events, provisioning, vocabulary), `em_wake_registry`, `em_device_assets`; package `echomuse_grammar/` (controller only) |

### 18.2 Device firmware (`device/`)

| Current code | What it does today | Disposition | Successor |
|---|---|---|---|
| `internal/client/data.go` frame types 0x01–0x05 with the 3-byte `[type][seq]` header; `StartMic(lockMic)`/`streamMic` VAD gate, preroll ring, `noSpeechTimeout`, end-of-speech sentinels; speaker/music frame intake | Legacy data plane and device-side turn gating | REPLACED | Rings, cell loudness, and lease uploads on `/device/v1/audio` EMA1 (§4.4, §16.1); no device VAD or no-speech timer |
| `internal/client/control.go` inbound `mic_start`, `mic_stop`, `speaker_flush`, `music_flush`, `wake_sound`, `duck`; outbound `oww_shadow_cross`, `oww_wake`, `playback_stats`, `button`, `mute_state` | Turn/audio control and wake reporting | REPLACED | `stream.*`, `wake.*`, `uplink.*`, `render.*`, `focus.*`, `button.action`, `privacy.changed` (§16.1) |
| `control.go` `register` capabilities `mic`, `speaker`, `audio_mix`, `wake_sound`, `oww_shadow`, `oww_trigger`, `oww_bcresnet` | Legacy capability names | REMOVED | The eight v1 capabilities (§11.1) |
| `control.go` `leds`, `led_anim`, `volume_set`, `config`, `shell_*`, `wifi_*`; outbound `volume_state`, `ambient_light`, `log`, `wifi_result`, `wifi_scan_result`, `ble_adverts`, `stats`; capabilities `leds`, `led_anim`, `buttons`, `button_hold`, `ambient_light` | LEDs, volume, config, shell, Wi-Fi, BLE, telemetry | RETAINED; moved | Same bodies carried in the `/device/v1/control` envelope; `/shell` plane unchanged |
| `internal/config/config.go` `OwwThreshold`, `OwwModel`, `OwwOnDevice`, `BargeInEnabled`, `BargeInThreshold`, `VadThreshold`, `VadSpeechMs`, `VadSilenceMs` and the on-device mode constants | Wake/VAD configuration | REMOVED | Model, runtime, thresholds, and hop arrive in `session.ready` (§16.1) |
| `config.go` `StartupVolume`, `DuckDb`, `BleProxyEnabled` | Volume, duck depth, BLE | RETAINED | `DuckDb` also sets provisional-duck depth |
| `internal/wakeword/stream.go` (openWakeWord pipeline) and its tests; `wakeword/testdata` openWakeWord fixtures | On-device openWakeWord scoring | REMOVED | — |
| `wakeword/shadow/*` | Shadow scoring, crossings, drop and inference statistics | REPLACED | `wakeword/detector`: profile thresholds, open rules and shadow-rule episodes, candidate rules, near-miss episodes, the four-hop queue and `wake_overrun`, `wake.stats` (§5.2) |
| `wakeword/bcresnet/*` | Window buffering, peak normalization, hop | MODIFIED | Checked against §5.1's preparation, including the RMS floor that leaves smoothing unchanged; feeds `wakeword/detector` |
| `wakeword/ort/*` | dlopen'd ONNX Runtime, one session per model, XNNPACK | MODIFIED | Runtime and graph loaded from hash-named speech assets (§16.5); session options unchanged |
| `wakeword/fixture/*` | openWakeWord engine-agreement fixture | REMOVED | — |
| `internal/bindings/slmic` | OpenSL `VOICE_RECOGNITION` capture, 80 ms callbacks, subscriber fan-out | MODIFIED | Per-callback `CLOCK_MONOTONIC` stamps; single consumer (the supervisor) |
| `internal/bindings/slspeaker/slspeaker.go`, `stream.go` voice/music planes, `EndStream`/`EndMusicStream`, `IsStreaming`, prime gate (`primePeriods=24`), `audioChanDepth=128` | Two network planes with EOS semantics | REPLACED | Source-class inputs with playback IDs/generations, `render.progress/finished` accounting (§4.3, §16.1). Prime and FIFO depth kept for network sources (§4.4) |
| `slspeaker/mix.go` Q15 mixer, `DuckGain`; `duckRampPeriods=4` (~170 ms full-range ramp) | Mix and duck arithmetic | MODIFIED | Per-sample 30 ms (normal) / 10 ms (stop) ramps; lease-derived gains; post-mix 48→16 kHz reference FIR tap (§16.1) |
| `slspeaker.EnableSpeakerAmp` | Re-enables the internal amp after headphone removal | RETAINED | Also used by alert foreground with a headphone inserted (§16.5) |
| `internal/cue` | Embedded wake chime mixed as a third plane | MODIFIED | `earcon` source mixed before the reference tap; played on `render.start`, or by the device itself at candidate open on `local_wake_chime` (§11.2) |
| `internal/server/server.go` | Owns LEDs, buttons, mic, speaker, volume, mute | MODIFIED | Hosts the audio supervisor, focus leases, and alert executor |
| `internal/server/volume.go` | DAC control 61 steps, volume arc | MODIFIED | Per-occurrence alert DAC level and restore (§16.5) |
| `internal/server/mute.go` | ADC mute, red ring, persisted state | MODIFIED | Also emits `privacy.changed` and ends the capture epoch |
| `pkg/speaker` `Speaker`/`FullSpeaker` (`PumpPeriod`, `EndStream`, `Flush`, `PumpMusic`, `EndMusicStream`, `FlushMusic`, `SetDuck`, `IsStreaming`, `PlayCue`, `OnStreamStats`) | Plane-based playback interface | REPLACED | Source-class render interface: start/pump/cancel per playback ID, lease gains, progress callbacks, reference tap |
| `pkg/mic` `AudioCallback func([]byte)`, `Subscribable` | Untimed capture callback and fan-out | MODIFIED | Callback carries the capture monotonic timestamp and sample index |
| `pkg/buttons`, `pkg/led` | Button and LED interfaces | RETAINED | — |
| `internal/server/animator.go`, `state.go`, `shell.go`; `bindings/buttons`, `led`, `als`, `jack`, `opensl`; `bluetooth`, `discovery`, `wifi`; `client/pty.go`, `tlscreds.go`; the `stats` body (`client/stats.go`, now `proto.Stats`) | LED rendering, persistence, hardware, networking | RETAINED | — |
| — | — | NEW | Audio supervisor, mic/reference/cell rings, cell loudness, uplink-lease executor, `/device/v1/assets` fetcher, alert cache/journal/executor, kernel wakelock (§4.4, §16.5) |
| `device/tools/oww_probe` | On-device openWakeWord probe | REMOVED | — |
| `device/tools/afe_probe`, `capture_mics`, `bf_capture`, `analyse_capture.py` | AFE/array diagnostics | RETAINED | — |
| Device files `/data/local/share/echomuse/oww/*` (`libonnxruntime.so`, `melspectrogram.onnx`, `embedding_model.onnx`, `bcresnet_audio.onnx/.json`, `ophelia-bcresnet.onnx/.json`) | On-device model copies | REMOVED | Deleted on first v1 start; there is no fallback that loads them. The same runtime bytes are fetched by hash into `speech/` (§16.5) |
| Device files `/data/local/etc/echomuse/{ca.pem,token,state.json}` | TLS, identity, mute | RETAINED | Beside the new `alerts/` directory (§16.5) |

### 18.3 Dashboard (`controller/static/dashboard.jsx`)

| Current control | Disposition | Successor |
|---|---|---|
| Wake word: stock `WW_MODELS` tiles, custom model upload/delete, Threshold, Speex NS, Barge-in toggle, Barge threshold, Near-miss floor, On-device select and its capability hints | REPLACED | BCResNet registry picker: upload, validation result, graph hash, per-model thresholds, active selection. Per device: installed graph hash, wake availability, maximum inference time, overruns, near misses |
| Wake word: Wake chime, Save wake clips, Arbitration window | RETAINED | — |
| Microphones: Stop on quiet, Stop sensitivity, Pause before stopping, Trailing audio, Max turn length | REMOVED | Fixed policy `post_afe_1`; new toggle “Extended utterances” (`extendedUtterances`) |
| Microphones: Noise suppression, Save utterances | RETAINED | Both apply to the STT copy |
| Advanced: VAD Threshold, Speech gate, Silence gate | REMOVED | — |
| Advanced: button single-tap event, multi-tap window | RETAINED | — |
| Timers: sound tiles, upload/delete | MODIFIED | Sound catalog; `timerSound` for timer rings and `alarmSound` for alarms without their own sound |
| Timers: Gap between bursts, Stop ringing after | RETAINED | Loop gap and ring limit for timer rings (`timerRingGapSeconds`, `timerRingSeconds`) |
| Timers: Burst length | REMOVED | The sound loops whole |
| Timers: “ring this device now” | REPLACED | Sound preview |
| Activity: “Voice activity — model @ threshold” header, near-miss count | MODIFIED | Registry model, route/attribution/terminal-reason trace |
| Activity: turn list, utterance playback, wake clips | RETAINED | — |
| `CONFIG_SECTIONS` mirror, `onDeviceMode`, `bcresnetCapable`/`triggerCapable`/shadow props | MODIFIED / REMOVED | Mirror follows §18.4; shadow/trigger props removed |
| — | NEW | Alert panel: active timers from the controller's display copy; this endpoint's alarm schedules and occurrences from its HA calendar; alarm delivery state (`stored_in_ha`/`armed_on_endpoint`/`delivery_pending`), pending journal operations, clock/wakelock health, and HA provisioning status. Alarm create and cancel go through the alert engine; HA's calendar UI remains the full alarm editor |

### 18.4 Configuration and persisted data

Per-key decisions (`em_db.DEFAULT_DEVICE_CONFIG` and `em_config_sections.SECTIONS`):

| Key(s) | Disposition | Successor |
|---|---|---|
| `owwModel` | REPLACED | `wakeModel` = active registry graph SHA-256 |
| `owwThreshold`, `bargeInThreshold`, `nearMissThreshold` | REPLACED | Registry entry thresholds (§5.1) |
| `owwOnDevice`, `owwSpeexNs`, `bargeInEnabled` | REMOVED | Wake detection always runs on the device; barge-in is always on and attribution governs it |
| `endpointRelative`, `endpointLowPerMil`, `endpointSilenceMs`, `endpointBackporchMs` | REMOVED | Policy `post_afe_1` |
| `maxSpeechMs` | REPLACED | `extendedUtterances` (false → 15 s, true → 30 s) |
| `vadThreshold`, `vadSpeechMs`, `vadSilenceMs` | REMOVED | — |
| `timerSound`, `timerRingGapSeconds` | RETAINED | Timer ring sound and gap between repeats (§10.8) |
| `timerRingSeconds` | RETAINED | Timer ring limit; default 60 → 900 s (§10.8) |
| `timerRingBurstSeconds` | REMOVED | The sound loops whole |
| — | NEW | `alarmSound`: the sound for alarm events without an `echomuse:` line and for new alarms; fleet value with an optional device override, like `timerSound` |
| — | NEW | `wakeOpenRules` (extra live open rules, default none, at most 4) and `wakeShadowRules` (shadow rules, at most 8, default five idle rules): lists of `{profile, windows, combine, threshold}` sent in `session.ready` to `open_rules_v1` devices (§5.2) |
| `wakeArbitrationMs`, `wakeSound`, `saveWakeClips`, `nsAsr`, `saveUtterances`, `duckDb`, `eqBands`, `eqLoudness`, `buttonSingleTapEvent`, `buttonMultiTapMs`, `bleProxyEnabled`, `ledScene`, `ledListenColor`, `ledThinkColor`, `meter*`; state key `startupVolume` | RETAINED | EQ applies to controller-rendered content/dialog, not to device-executed alerts |

Sections after cutover: `wakeword` = `wakeModel`, `saveWakeClips`, `wakeArbitrationMs`, `wakeSound`, `wakeOpenRules`, `wakeShadowRules`; `microphones` = `nsAsr`, `saveUtterances`, `extendedUtterances`; `advanced` = `buttonSingleTapEvent`, `buttonMultiTapMs`; `timers` = `timerSound`, `timerRingSeconds`, `timerRingGapSeconds`, `alarmSound`; `playback`, `ring`, `bluetooth` unchanged.

Append **one** migration after the current last entry (schema 21 → 22; `MIGRATIONS` stays append-only). In one transaction including its Python fixup:

1. Rewrite fleet and every device config JSON: delete the REMOVED/REPLACED keys; set fleet `wakeModel` to `4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f`; set `extendedUtterances=true` when the old effective `maxSpeechMs` is 0 (no cap) or exceeds 15,000, otherwise `false`.
2. Rewrite a stored `timerRingSeconds` of 60 (the old default) to 900; keep any other value.
3. Add nullable `turns` columns `wake_model_sha256`, `policy_hash`, `wake_attribution`, `reference_coverage`, `commit_route`, `terminal_reason`, `commit_id`; add `wake_counters.dev_hops INTEGER NOT NULL DEFAULT 0`. Existing columns stay; shadow/device-wake columns keep their history and are no longer written.
4. Keep `wake_counters`, filled from the device's `wake.stats`: `near_misses` and `near_miss_max` from near-miss episodes, `dev_hops` from hops scored, `dev_drops` from `wake_overrun`, `dev_crossings` from candidates opened, `dev_max_score` from the peak smoothed value, and `dev_max_infer_ms` from maximum inference. `dev_frames`, `dev_max_gap_ms`, and `underruns` are no longer written, so no column changes its unit.

The migration also creates the `alert_ops` and `alert_delivery` tables (§16.3) and adds `alarmSound` to fleet config, initialized from `timerSound`. Before it commits config rewrites, `em_sounds` resolves every effective fleet/device `timerSound` and `alarmSound` ID plus `default`: it decodes each resolvable file to 48 kHz mono PCM16 WAV, exports its first 10 s with a 50 ms fade-out under its content SHA-256, and records a dashboard warning when it shortened a file. Config values remain catalog IDs. A missing or undecodable ID remains stored but resolves to `builtin:fallback` and is flagged; the migration does not claim a sound was preserved when it was not.

Environment/add-on: `OWW_MODEL`, `OWW_THRESHOLD`, and add-on options `oww_model`/`oww_threshold` are REMOVED. `HA_URL` and `HA_TOKEN` are added for Docker/bare metal; the add-on's `config.yaml` gains `homeassistant_api: true`. `.env.example`, `config.yaml`, and `em_start.py` change together.

### 18.5 Dependencies, build, tests, and documentation

- **Controller image:** remove `openwakeword==0.6.0` (Dockerfile `--no-deps` install and `download_models()`), `speexdsp-ns`, `tqdm`, `scikit-learn`, and `requests` (no controller module imports the last three). Keep `onnxruntime`, `numpy`, `scipy` (EQ), the DTLN models, and the hash-checked `onnxruntime-android` 1.19.2 extraction of `jni/armeabi-v7a/libonnxruntime.so`, now served to devices as a speech asset (§16.5). Add `sherpa-onnx==1.13.8` and the hash-pinned speech bundle (§16.6). The Dockerfile `COPY` list drops `em_shadow`, `em_oww_assets`, `em_oww_models`, `em_endpoint`, `em_turnclock`, and adds the new modules.
- **Home Assistant:** no custom integration and no HA core patch. The controller provisions stock objects only, through stock APIs: one Local Calendar per endpoint, five alarm scripts exposed to Assist, and the satellite's VAD-sensitivity setting (§16.7).
- **Speech bundle licensing:** the image or first-start downloader must ship the Kroko model's CC-BY-SA attribution (link to `huggingface.co/Banafo/Kroko-ASR`). Silero v5 (MIT) and sherpa-onnx (Apache-2.0) keep their notices. The bundle is fetched and hash-checked, never retrained in this repository.
- **Controller tests:** remove `test_shadow.py`, `test_endpoint.py`, `test_turnclock.py`; replace `test_oww_assets.py` with `em_device_assets` tests. Add: `test_alerts.py` (calendar encoding, journal ordering, merge table, restore guard, backlog expiry, crash points before and after each write), `test_alert_wire.py` (snapshot/delta objects and the canonical digest), `test_ha_client.py` (every section 16.7 message shape against fixtures recorded from HA 2026.8.1), `test_alert_scripts.py` (the five generated scripts and `request_id` construction), `test_uplink.py` (lease table, backfill grid alignment, duplicate samples), `test_endpoint_policy.py` (every reducer route, `pending` revocation, no-input, retry), `test_wake_phrase.py` (the section 16.6 cases), and `test_grammar_conformance.py` (the conformance corpus). Replace `test_oww_models.py` with registry tests. Modify `test_wake_scorer.py` (drop openWakeWord), `test_button.py`, `test_player.py`, `test_capabilities.py`, `test_config_sections.py`, `test_config_guard.py`, `test_db_migrations.py` (the new migration), `test_db_instrumentation.py`, `test_deploy.py` (COPY list, add-on threshold pin), `test_sounds.py`, and the `conftest.py` scope note. Keep the rest. New pure modules (`em_attribution`, `em_endpoint_policy`, `em_session`, `echomuse_grammar`) are tested with evidence fixtures only, so the suite still needs just pytest, numpy, and scipy.
- **Device tests:** remove the openWakeWord `wakeword` stream and shadow tests and their fixtures; keep and adapt the `wakeword/bcresnet` and `wakeword/ort` tests; add tests for detector candidate rules, the hop grid, latched thresholds, overruns, lease backfill from rings, cell records, the reference FIR's alignment, the alert journal and snapshot install (including crash points), and the alert executor's queue, expiry, and snooze child identity; modify `slspeaker` mix/stream tests, `cue_test`, and `client/data_race_test`.
- **`oww_forge/`:** REMOVED. It trains openWakeWord models, which nothing can load after cutover; BCResNet training stays in `~/git/bcresnet`.
- **Repository model fixtures:** replace root `bcresnet_audio.onnx` with the deployed graph (§5.1); remove `bcresnet.onnx` (feature-in graphs are unsupported).
- **Docs:** `voice-pipeline.md`, `configuration.md`, `led-ring-states.md`, and `quickstart.md` are rewritten to match; `ring-stop-word.md` is superseded by §6.3 and deleted; `playback-capture.md` drops its `oww_forge` framing; `native-afe-migration.md` and the `alexa-*.md` research files stay as history. `CLAUDE.md` loses the `oww_forge`/openWakeWord paragraph, the old capability list, and the `em_oww_models` test-scope mention.

## 19. Backlog: not in the initial implementation

Features considered and deliberately deferred. Nothing here is built, packaged, or tested in the initial implementation, and no initial behavior depends on it.

### 19.1 Speaker takeover endpoint

**Problem.** A long request followed, with no pause, by a second person talking at a similar level becomes one utterance: level cannot tell the two apart (section 8.2). In the initial implementation that utterance ends at the next real pause (route A) or closes as `too_long`, so the second person's words may reach HA as part of the request.

**Idea.** End the utterance where the voice changes. This has prior art, though not in the form first drafted here:

- Amazon's **anchored speech detection** uses the wake word as an anchor to tell the device-directed talker from interfering talkers, frame by frame, with a model trained for that task ([Maas et al., Interspeech 2016](https://www.isca-archive.org/interspeech_2016/maas16_interspeech.html); [End-to-end Anchored Speech Recognition](https://arxiv.org/abs/1902.02383)).
- Google's **Personal VAD** labels each frame as non-speech, target speaker, or other speaker from an enrolled voiceprint, and reports beating a baseline that combines a standard VAD with a separate speaker-recognition model ([Ding et al., Odyssey 2020](https://arxiv.org/abs/1908.04284)).

The first draft of this design (“route C”) was that weaker baseline: CAM++ voiceprints (`3dspeaker_speech_campplus_sv_en_voxceleb_16k.onnx`, SHA-256 `357a834f702b80161e5b981182c038e18553c1f2ca752ed6cec2052365d4129b`) compared over 2.0 s windows every 480 ms, anchored on the first 2.0 s of command speech, with cosine cut-offs 0.50/0.35. It was removed because:

- its only evidence is a two-clip sanity test on distributed sample audio [P1]; it was never run on Dot audio, on two people, or on similar voices;
- it reaches only long requests (a 2.0 s anchor is required), and detects a switch about 2 s after the second person starts;
- its cut point is only accurate to about ±0.5 s, so it could commit the second person's first words.

**Before adopting it:**

1. Record two-person cases on Dots (added to the section 13.3 endpoint set): long request plus immediate second talker, similar voices, a person against a TV voice, with fan and music underneath.
2. Measure separation on those recordings. Prefer an anchored per-frame approach seeded by the wake word; a pretrained verification model is acceptable only if it meets the gate. A trained model falls under the section 5.4 bar for new models.
3. If a window-based model is used, require two consecutive different-voice windows before ending, cancel on any same-voice window in between, cut at the last streaming-ASR token before the change, and re-decode every committed span (free-form included), closing as `retry` when the text changed beyond its final word.
4. Gate: on the held-out two-person set, the route ends at the voice change in ≥90% of cases with zero merged action text, and never fires on single-talker commands.

Placement if adopted: controller only. CAM++ crashed under the Dot's runtime and took 1.06 s per window unoptimized [D6].
