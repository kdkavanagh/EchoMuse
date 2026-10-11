# Native AFE — the device's audio path

**Superseded: Fire OS 5 history.** EchoMuse now runs only on Fire OS 6, which
has no OpenSL ES. Capture and playback go through Amazon's `mixer` daemon via
`libmixerAPI` (`internal/mixerapi`, [fireos6-port.md](fireos6-port.md)), which
keeps this document's one rule: both directions through the native path, so
the AFE has its far-end reference
([post-afe-audio-architecture.md](post-afe-audio-architecture.md) §4.1). The
OpenSL ES backend, `internal/opensl` and `device/tools/afe_probe` were removed
with Fire OS 5 support. Everything below is the Fire OS 5 record as it stood.

**Status then:** done and exclusive. EchoMuse captured and played through
Android's audio HAL via OpenSL ES, which put the microphone stream through
Amazon's ASP front end. There was no second backend: the tinyalsa mic/speaker
bindings, the Go-side beamformer, the speexdsp echo canceller and the AGC were
deleted when this became the only path, along with the opt-in marker, the
capability pair and the dashboard switch that chose between them.

The measurement that was supposed to justify it (Phase 0, below) was **never
run on hardware** before Fire OS 5 support was removed.

**Background:** [alexa-afe.md](alexa-afe.md) — read that first, especially
"5. Open the AFE through the standard Android capture API".

## What this is

Amazon's audio front end lives inside the audio HAL
(`audio.primary.mt8163.so` links `libasp.so` and calls `asp_init`,
`asp_create_pipeline`, `asp_process`, `asp_set_device`). The HAL picks an ASP
pipeline from the capture stream's `input_source`:

| `input_source` | value | pipeline | `AFE.cfg` path |
|---|---|---|---|
| `AUDIO_SOURCE_VOICE_RECOGNITION` | 6 | **0** | **ASR** — per-mic AEC, fixed + adaptive beamformer, SNR beam selection, false-WW prevention, +7.2 dB |
| `AUDIO_SOURCE_HOTWORD` | 1999 | **0** | same |
| `AUDIO_SOURCE_VOICE_COMMUNICATION` | 7 | 1 | Voice/VoIP — AEC, RES, NR, CNG, AGC |
| `AUDIO_SOURCE_MIC` | 1 | 2 | `"Mic": { "Algorithms": {} }` — empty, passthrough |
| anything else | — | −1 | no ASP pipeline |

So capturing at `VOICE_RECOGNITION` through the normal Android audio path
gets Amazon's entire ASR front end, with no Amazon service running and no
Alexa packages un-hidden. That is what `internal/bindings/slmic` does.

## The one rule that matters

**Capture and playback must BOTH go through the framework, or the AEC has
nothing to cancel.**

ASP takes its far-end reference on the HAL's playback side
(`AudioALSAPlaybackHandlerNormal::asp_open` / `doProcessAsp`). Writing PCM
straight to `pcm23p` never passes through AudioFlinger, so the AFE would never
see it.

A build that converted capture only would still *work*. It would produce audio
that is beamformed but not echo-cancelled, with no error anywhere, and it would
look like the AFE underperforming rather than like a wiring mistake. There is
no longer a second backend pair to get that wrong with — `cmd/server.go`'s
`newAudioBackends` opens `slmic` and `slspeaker` together or fails — but the
rule is what any future change to either side has to respect.

## Architecture

```
pkg/mic.Microphone       ← internal/bindings/slmic     (OpenSL ES)
pkg/speaker.FullSpeaker  ← internal/bindings/slspeaker (OpenSL ES)
internal/opensl/         ← shared dlopen shim
```

- `mic.Microphone` is `Init()` + `Listen(cb, ctx)`, plus the `mic.Subscribable`
  fan-out that `vadStreamHandler` uses. Every period handed out is one fully
  processed mono channel.
- `speaker.FullSpeaker` carries the two-plane music/voice API (`PumpPeriod`,
  `PumpMusic`, `SetDuck`, `Flush`, `FlushMusic`, `EndStream`,
  `EndMusicStream`) plus `OnStreamStats` and `EnableSpeakerAmp`.

### dlopen, don't link

`/system/lib/libOpenSLES.so` is present on the device, but it is dlopen'd
rather than linked, following the `internal/wakeword/ort` precedent, so a
missing or incompatible library is a runtime error naming itself rather than a
binary that will not start. `internal/opensl` holds the shim, the same shape as
`ort/shim.h`.

### There is no fallback, and that is deliberate

The old backends were what a failed OpenSL open fell back to. With them gone,
`main()` fatals instead. That is the better recovery path: `start_server.sh`
counts three fast exits and flips the A/B symlink to the other slot, which is a
firmware the device is known to boot. A silent fallback to a different pipeline
would have been a device that works differently for reasons nothing on screen
explains.

## Wire protocol: unchanged

- **Mic.** Mono S16 16 kHz in 80 ms frames — the same format and framing the
  controller has always received, now produced by the HAL rather than by our
  own 9-channel reduction.
- **Speaker.** Mono 48 kHz on the voice plane (0x02/0x03) and the music plane
  (0x04/0x05). The OpenSL player is opened at 48 kHz so nothing resamples.

## Ducking is unchanged

The device-side music/voice mix survives intact: same `SetDuck(db)`, same
per-sample gain ramp, same saturation rather than wrap. It moved from "mix at
the ALSA write" to "mix before the OpenSL write".

The reason ducking has to be device-side is `LEAD_S` = 4.0 s: four seconds of
un-ducked music have already left the controller when a wake word fires. The
lead buffer lives in `slspeaker`'s own ring rather than an ALSA one, so the
argument is unchanged. `audio_mix` stays advertised; `duckDb`, `music_flush`
and `speaker_flush` keep their meaning.

Do not "simplify" this into two AudioTracks with framework volume control —
the per-sample ramp exists because a gain step at a period boundary is an
audible click landing exactly when the user started speaking.

## What the HAL took over

These were config keys with dashboard controls. They are **gone from both
ends**, not disabled: the firmware does not read them, the controller does not
store them, and `tests/test_capabilities.py::test_the_mic_chain_is_not_offered_
as_config` fails if one comes back. A slider that moves, saves, pushes and
changes nothing about the sound is indistinguishable from a setting that simply
does not help, which is why this is a test and not a comment.

| Component | Under the AFE | Config keys removed |
|---|---|---|
| directional mic selection | FBF + ABF + SNR beam selection | `beamformingEnabled`, `beamAngle` |
| speexdsp echo canceller | 7 per-mic subband AECs | `aecEnabled`, `aecDelayMs`, `aecTailMs` |
| AGC | the AFE's own | `agcEnabled` |
| pre-truncation mic gain | 16-bit from the framework; gain is HAL PGA (20 dB) + AFE out (+7.2 dB) | `micGainDb`, `adcDigitalGain`, `adcMicpga` |

`AFE.cfg` is on read-only `/system`, so the mic chain is fleet-fixed rather
than per-device. Changing it means remounting `/system` and editing a system
file — a device-image operation, not a config one. No work planned.

Kept and still ours:

- **VAD gate.** EchoMuse's VAD runs on the processed mono stream and still
  provides end-of-speech for bounded `lock_mic` turns. `vadThreshold` stays
  meaningful, but its calibration is **not** comparable with the pre-AFE
  fleet's: the HAL's PGA and the AFE's output gain sit where `micGainDb` used
  to. Re-tune by measurement, do not carry a value over.
- **Mute.** Unchanged and still device-sovereign. The ADC mute is a `tinymix`
  control on the TLV320ADC3101 and is independent of who owns the PCM; the
  device still refuses `mic_start` while muted. **This must not regress** — it
  is what makes the controller-side button logic safe (see CLAUDE.md).
- **Jack handling.** `internal/bindings/jack` still re-enables
  `Ext_Speaker_Amp_Switch` on plug removal. accdet mutes it on insert and
  nothing else turns it back on.
- **On-device wake word** (`internal/wakeword`, `oww_shadow`). It taps the
  frames written to the wire, whose format is unchanged. Scores are not
  comparable across the change — it now scores AFE output — so shadow-mode
  history and `turns.dev_threshold` want reading with that in mind.
- **Controller-side noise suppression** (`nsAsr`, DTLN on the ASR-bound
  stream). Still functional, but it is now a *second* denoise on an already
  denoised stream. Default off, and that is the right default here.

## `stop media` must not run

`slmic` and `slspeaker` deliberately run no `stop mixer` / `stop media`:
mediaserver has to own both PCMs for the HAL to be in the loop at all.

That is the opposite of what the tinyalsa speaker did, and the reason it did it
is worth knowing. A headset present at boot let mediaserver park a blocking
`snd_pcm_open` ahead of us with no timeout, which stranded the whole device
(#80). Handing the PCM back to mediaserver makes that specific failure mode
**moot rather than reintroduced** — we are no longer racing for the device —
but it has not been re-verified on hardware with a plug inserted at boot, which
is the exact scenario that produced the original report.

Note also that a plug in the jack degrades the audio subsystem in ways that are
still unexplained (#117, #141), and the live hypothesis there is *this*: on
stock, Amazon's HAL and mediaserver coordinate with accdet on jack transitions.
See CLAUDE.md's jack section — that analysis predates this migration and the
two experiments it lists are still the right next step.

## Instrumentation

`slspeaker` reports the same `StreamStats` shape the ALSA backend did:
`min_depth`, `prime_wait_ms`, `recv_span_ms`, `max_gap_ms`, `bytes_recv`,
periods and underruns, measured against its own ring. The schema v7 delivery
instrumentation and the dashboard's Activity tab are built on these.

**Report them properly or report nothing.** Never emit zeros for values that
were not measured — absence is stored as NULL precisely so that "never
reported" and "zero underruns" stay distinguishable.

The level tap (`levelTap`, which drives the `meter` LED pattern) sits at the
OpenSL write and is *more* accurate than the ALSA one it replaced: a shallower
buffer removes most of the ~5.5 s skew between written and audible.

There is no echo tap. The HAL takes the reference itself.

## What was never measured

### Phase 0 — the spike, never run

`device/tools/afe_probe` (removed with Fire OS 5 support) built and linked
(verified via `echomuse-compiler`, real ARM binary produced) and **was never
run on a device**. It recorded N seconds via OpenSL ES and wrote WAVs, at each
of `VOICE_RECOGNITION`, `VOICE_COMMUNICATION` and `MIC`, optionally while
playing a known signal through an OpenSL player.

It would have answered, cheaply and on hardware, everything this migration assumed:

1. Does `VOICE_RECOGNITION` actually instantiate ASP pipeline 0? (`MIC` is
   configured with an empty algorithm list, so it is a built-in control: any
   difference between the two IS the AFE.)
2. **ERLE.** Play a known signal, measure residual at the mic, both sources.
   This is the number the whole exercise was for — the speexdsp path it
   replaced achieved ~7–9 dB, which was measured as the physical ceiling for
   *that* approach, not for this one.
3. What channel count comes back. `MicsPostAECOutputGain` sits in the ASR path
   and pryon consumes `BEAMFORMED` / `pre-aec` / `post-aec` channel types, so
   the AFE can plainly emit more than the beam — but `audio_policy.conf`
   advertises only `MONO|STEREO` on the primary input, so mono is the
   expectation. *Answered for Fire OS 6 (2026-10-07):* the mixer's
   `micMultiChAsr` emits the beam, up to 4 post-AEC mics and the far-end
   reference, but never the individual beams; `micAsr` is the beam alone
   (`docs/alexa-afe.md`, "Fire OS 6: the mixer's ASR streams"). Fire OS 5
   was never measured.
4. End-to-end latency, against the mic pipeline's deadline.

Needs the `echomuse-compiler` image, the toolchain stage of
`controller/Dockerfile` (`device/tools/build_tools.sh` builds it).

### Phase 3 — field comparison, not done

Wake-word detection rate, barge-in behaviour, transcript quality (use
`saveUtterances` — the tap is below NS, so the file is what STT actually
heard), CPU, and the delivery stats. There is no longer an old path to compare
against on the same device, so this is now a comparison against recorded
history rather than an A/B.

Two parameters are unverified on this backend specifically and are the first
things to look at if something sounds wrong:

- **Speaker buffering depth.** `audioChanDepth` 128 periods ≈ 5.46 s,
  `primePeriods` 24 ≈ 1 s hold before start, controller `LEAD_S` 4.0 s. Those
  figures were sized against measured 1.8–2.6 s link stalls on the ALSA ring
  and are carried over as a starting point, not re-derived for an OpenSL
  buffer queue. This is what keeps music from gapping on a marginal link.
- **`bargeInThreshold`** (0.05). Every figure behind that default — 0.002–0.003
  converged self-echo, 0.055 worst-case unconverged — was measured against the
  speexdsp canceller. The AFE's cancellation depth is unmeasured, so treat the
  headroom as unverified. The symptom of getting it wrong is a device cutting
  its own response short, which is self-evident; the fix is to raise it.

## Risks carried forward

- **Everything here was inferred from a 44-byte function.**
  `getAspPipelineType` is unambiguous about the mapping, but nothing has been
  recorded through it. Phase 0 exists to make that inference cheap to falsify.
- AudioFlinger adds buffering on both paths, against a mic pipeline that shares
  a core with wake-word inference.
- The device no longer owns its audio hardware outright, which is a real
  reduction in how much of the failure surface we control. Mute is unaffected
  (verified: separate mixer control), but boot ordering, jack behaviour and
  mediaserver restarts are all things the HAL decides.
- **Recovery from a bad audio run in the field is a firmware rollback**, not a
  config push. The A/B slot flip is that mechanism, and it is why the failure
  path is a fatal rather than a fallback.
