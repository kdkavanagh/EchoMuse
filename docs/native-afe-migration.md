# Native AFE migration — spec and plan

**Status:** Phases 0–2 implemented (branch `native-afe-migration`), Phase 3
not started. Everything below still reflects static analysis only — nothing
has been recorded through this path on real hardware yet, and the code
exists specifically to make that cheap to do:

- **Phase 0** — `device/tools/afe_probe`, `internal/opensl`. Builds and links
  (verified via `echomuse-compiler`, real ARM binary produced); not yet run
  on a device. This is the next step, and it gates everything after it — see
  the kill criteria below.
- **Phase 1** — `internal/bindings/slmic`, `internal/bindings/slspeaker`,
  selected by `EM_NATIVE_AFE` in `cmd/server.go` (default off, falls back to
  tinyalsa on any failure to open). Compiles clean under `-tags server` and
  `go vet`; the existing tinyalsa path is unmodified and remains the default.
  The firmware build carrying this has been OTA'd to the one fielded device
  (`G090LF10728426PR`, `20260812-1957-dev`), and its durable opt-in marker
  ("Opting a device in" below) is deployed and confirmed by md5 — but the
  marker itself is **not set**, so that device is still running the tinyalsa
  path exactly as before. Nothing has run through the OpenSL ES path yet.
- **Phase 2** — `native_afe` capability (advertised only while the backend is
  actually active, not merely compiled in — see `control.go`'s
  `nativeAFEActive`), `Device.native_afe_capable`, `/api/devices`'s
  `nativeAfeCapable`, and the bypassed config controls (beamforming, AEC,
  AGC, mic/ADC gain) rendered disabled-with-reason in the dashboard when it
  is set. Covered by `tests/test_capabilities.py`.
- **Phase 3/4** — not started; both need Phase 0's numbers first.

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
Alexa packages un-hidden.

The goal of this work is to put EchoMuse's device audio on that path.

## The one rule that matters

**Capture and playback must BOTH go through the framework, or the AEC has
nothing to cancel.**

ASP takes its far-end reference on the HAL's playback side
(`AudioALSAPlaybackHandlerNormal::asp_open` / `doProcessAsp`). EchoMuse today
runs `stop media` and writes PCM straight to `pcm23p` with tinyalsa — audio
written that way never passes through the HAL, so the AFE would never see it.

A build that converts capture only will still *work*. It will produce audio
that is beamformed but not echo-cancelled, with no error anywhere, and it will
look like the AFE underperforming rather than like a wiring mistake. Any
capture-side change must land together with the playback-side change or behind
the same off-by-default flag.

## Architecture

New backends behind the **existing** interfaces. Nothing above them changes.

```
pkg/mic.Microphone       ← internal/bindings/mic       (tinyalsa, today)
                         ← internal/bindings/slmic     (OpenSL ES, new)

pkg/speaker.Speaker      ← internal/bindings/speaker   (tinyalsa, today)
                         ← internal/bindings/slspeaker (OpenSL ES, new)

internal/opensl/         ← shared dlopen shim
```

Both interfaces are already the right shape:

- `mic.Microphone` is `Init()` + `Listen(cb, ctx)`, plus the optional
  `mic.Subscribable` fan-out that `vadStreamHandler` uses. The new backend
  implements both; it fans out one processed mono stream instead of one raw
  9-channel stream.
- `speaker.Speaker` already carries the two-plane music/voice API
  (`PumpPeriod`, `PumpMusic`, `SetDuck`, `Flush`, `FlushMusic`, `EndStream`,
  `EndMusicStream`). Keep all of it — see "Ducking" below.

Selection happens in `cmd/server.go` where `mic.NewMicrophone()` and
`speaker.NewPcmSpeaker()` are called today (lines ~71 and ~85). One config
switch picks the pair. **They are chosen together, never independently.**

### dlopen, don't link

`/system/lib/libOpenSLES.so` is present on the device, but follow the
`internal/wakeword/ort` precedent and dlopen it: a device where the load fails
falls back to the tinyalsa path and keeps working, rather than producing a
binary that will not start. `internal/opensl` holds the shim, the same shape as
`ort/shim.h`.

## Wire protocol: unchanged

This is worth stating plainly, because it is the reason the change is
containable.

- **Mic.** The device sends mono S16 16 kHz in 80 ms frames today, after its
  own beamformer reduces 9 channels to 1. The AFE hands us mono S16 16 kHz.
  Same format, same framing. The controller needs no change.
- **Speaker.** The controller sends mono 48 kHz on the voice plane (0x02/0x03)
  and the music plane (0x04/0x05). Open the OpenSL player at 48 kHz so nothing
  resamples. Unchanged.

No new message types. No controller changes are required for the audio path
itself — only for the dashboard's disabled-control handling (below).

## Ducking stays exactly as it is

The device-side music/voice mix is **not** removed. It moves from "mix at the
ALSA write" to "mix before the AudioTrack write" — same code, same
`SetDuck(db)`, same per-sample gain ramp, same saturation rather than wrap.

The reason ducking has to be device-side is that the controller runs its music
feed `LEAD_S` = 4.0 s ahead of realtime, so four seconds of un-ducked music
have already left the controller when a wake word fires. Under this design the
lead buffer moves into `slspeaker` rather than into the ALSA ring, so the
argument is unchanged and the code that acts on it is unchanged. `audio_mix`
stays advertised; `duckDb`, `music_flush` and `speaker_flush` keep their
meaning.

Do not "simplify" this into two AudioTracks with framework volume control
until the mix path is proven — the per-sample ramp exists because a gain step
at a period boundary is an audible click landing exactly when the user started
speaking.

## What is bypassed, and how

Bypassed means **gated off and reported as unavailable**, never deleted. Every
one of these has a dashboard control, and CLAUDE.md's rule is that a control
whose feature the device lacks is shown **disabled with the reason**, never as
a control that silently does nothing.

| Component | Under the AFE path | Config key affected |
|---|---|---|
| `internal/beamformer` | replaced by FBF + ABF + SNR beam selection | `beamformingEnabled`, `beamAngle` |
| `internal/aec` (speexdsp) | replaced by 7 per-mic subband AECs | `aecEnabled`, `aecDelayMs`, `aecTailMs` |
| `internal/processor` (AGC) | replaced by the AFE's AGC | `agcEnabled` |
| mic gain pre-truncation | 16-bit from the framework; gain is HAL PGA (20 dB) + AFE out (+7.2 dB) | `micGainDb`, `adcDigitalGain`, `adcMicpga` |
| controller `em_ns` / DTLN | redundant; would be a second NS on an already-denoised stream | `nsAsr` — default OFF on this path |

Kept and still ours:

- **VAD gate.** EchoMuse's VAD runs on the processed mono stream and still
  provides end-of-speech for bounded `lock_mic` turns. `vadThreshold` stays
  meaningful, but its calibration changes — it is currently expressed in
  pre-gain units and scaled internally, and the AFE's output level is not the
  same. Re-tune, do not reuse the number.
- **Mute.** Unchanged and still device-sovereign. The ADC mute is a `tinymix`
  control on the TLV320ADC3101 and is independent of who owns the PCM; the
  device still refuses `mic_start` while muted. **This must not regress** — it
  is what makes the controller-side button logic safe (see CLAUDE.md).
- **Jack handling.** `internal/bindings/jack` still re-enables
  `Ext_Speaker_Amp_Switch` on plug removal. accdet mutes it on insert and
  nothing else turns it back on.
- **On-device wake word** (`internal/wakeword`, `oww_shadow`). It taps the
  frames written to the wire, which are unchanged in format. Expect scores to
  move — it is now scoring AFE output, not our own beamformed mic — so
  shadow-mode comparisons across the change are not comparable and
  `turns.dev_threshold` will need reading with that in mind.

## `stop media` must not run

`speaker.PcmSpeaker.Init()` runs `stop media` and then `waitForFreePcm` to take
`pcm23p` from mediaserver; `mic.PcmMicrophone.Init()` runs `stop mixer`.
Neither may happen on the AFE path — mediaserver has to own both PCMs for the
HAL to be in the loop at all.

Read the jack section of CLAUDE.md before touching this. `stop media` is there
because a headset present at boot let mediaserver park a blocking `snd_pcm_open`
ahead of us with no timeout, which stranded the whole device. Handing the PCM
back to mediaserver makes that specific failure mode **moot rather than
reintroduced** — we are no longer racing for the device — but the reasoning
should be re-verified on hardware with a plug inserted at boot, because it is
the exact scenario that produced the original report.

## Instrumentation must survive

`slspeaker` has to report the same `StreamStats` the tinyalsa backend does:
`min_depth`, `prime_wait_ms`, `recv_span_ms`, `max_gap_ms`, `bytes_recv`,
periods and underruns, measured against its own ring. The schema v7 delivery
instrumentation and the dashboard's Activity tab are built on these.

**Report them properly or report nothing.** Never emit zeros for values that
were not measured — absence is stored as NULL precisely so that "never
reported" and "zero underruns" stay distinguishable, and a backend that fakes
zeros makes every device on this path look perfect.

The level tap (`levelTap func(rms float64)`, which drives the `meter` LED
pattern) moves to the new write point. It gets *more* accurate: the ALSA-write
tap exists because audio sits ~5.5 s ahead of audible, and a shallower
AudioTrack removes most of that skew.

The echo tap (`canceller.WriteFar`) is no longer needed — the HAL takes the
reference itself. Leave the plumbing, pass a no-op.

## Phases

### Phase 0 — spike, before touching anything

A standalone probe in `device/tools/`, following `capture_mics`, `bf_capture`
and `oww_probe`. Does not link into the server, does not change any interface.

It should record N seconds via OpenSL ES and write WAVs, at each of
`VOICE_RECOGNITION`, `VOICE_COMMUNICATION` and `MIC`, optionally while playing
a known signal through an OpenSL player.

This answers, cheaply and on hardware, everything the rest of the plan assumes:

1. Does OpenSL ES work at all on this FireOS build?
2. Does `VOICE_RECOGNITION` actually instantiate ASP pipeline 0? (`MIC` is
   configured with an empty algorithm list, so it is a built-in control: any
   difference between the two IS the AFE.)
3. **ERLE.** Play a known signal, measure residual at the mic, both sources.
   This is the number the whole exercise is for.
4. What channel count comes back. `MicsPostAECOutputGain` sits in the ASR path
   and pryon consumes `BEAMFORMED` / `pre-aec` / `post-aec` channel types, so
   the AFE can plainly emit more than the beam — but `audio_policy.conf`
   advertises only `MONO|STEREO` on the primary input, so mono is the
   expectation. Worth confirming, because stereo carrying beam + something
   would change the direction-arc story.
5. End-to-end latency, against the mic pipeline's hard 160 ms deadline.

**Kill criteria.** If ERLE at `VOICE_RECOGNITION` is not clearly better than
the ~7–9 dB the current speexdsp path achieves, stop here. The whole
justification is that Amazon's front end is better; if it measures otherwise on
this hardware, none of the rest is worth the disruption.

Needs the `echomuse-compiler` image (`cd device && docker build -t
echomuse-compiler compiler/`), which is not built on every dev machine.

### Phase 1 — capture + playback backends

`internal/opensl` shim, `internal/bindings/slmic`, `internal/bindings/slspeaker`.
Both selected together by one env/config switch, defaulting **off**. Old path
untouched and still the default.

`slspeaker` carries the lead ring, the prime-before-start hold, the two planes,
the duck ramp, and `StreamStats`.

#### Opting a device in

`EM_NATIVE_AFE` is a boot-time env var, read once by `cmd/server.go` before any
controller connection exists — deliberately not a config push (see the "Risks"
section: recovery is a flag flip and a restart, not a dashboard toggle).

Making that durable on a real device needs one more piece, because two of the
obvious ways to set it don't survive: hand-exporting it in
`/data/local/bin/start_server.sh` gets silently overwritten the next time an
OTA re-syncs that script against the canonical payload
(`em_api._sync_start_script`), and there is no config-message path that can
reach a choice made before the device has even connected.

The durable switch is a **marker file**, checked once near the top of
`start_server.sh` (`controller/device_payloads/start_server.sh`, symlinked as
`device/scripts/start_server.sh`) — data survives the same re-sync that would
erase a script edit:

```
/data/local/etc/echomuse/native_afe.enabled   # present = EM_NATIVE_AFE=1
```

To flip it, write or remove that file over the shell proxy
(`controller/tools/devshell.py`) — **never** `stop`/`start` the echomuse
service from that same shell to make it take effect. That shell is a child of
the very server process being stopped; per `devshell.py`'s own warning, doing
so can strand the device until it is power-cycled, because stopping the
service also kills the one channel that could send the follow-up `start`.
**Reboot instead** — safe to issue from the same shell proctor (recovery is
automatic; Android's own init re-execs every service, `echomuse` included, on
its own once boot completes), and it is what actually gets `start_server.sh`
to re-read its own file (per `_sync_start_script`'s comment, a running
script's shell keeps the old inode open regardless):

```bash
# from inside the echomuse-controller container
python devshell.py -d <device_id> \
  'mkdir -p /data/local/etc/echomuse && touch /data/local/etc/echomuse/native_afe.enabled && echo MARKER_SET'
python devshell.py -d <device_id> reboot

# to revert:
python devshell.py -d <device_id> \
  'rm -f /data/local/etc/echomuse/native_afe.enabled && echo MARKER_CLEARED'
python devshell.py -d <device_id> reboot
```

Run `device/tools/afe_probe` on the target device **before** flipping this —
it answers the kill criteria without ever touching the live server, where
enabling the marker puts brand-new, hardware-unverified code (OpenSL ES, HAL
routing) directly in the path of the next real voice turn.

### Phase 2 — capability and dashboard

Advertise a capability (suggest `native_afe`) from `capabilities()` in
`internal/client/control.go`. It is append-only and negotiated by capability,
never by version.

Controller side: `Device.native_afe_capable`, and the config controls listed
in the bypass table above rendered **disabled with the reason** when it is set.
`tests/test_capabilities.py` covers both directions of this pattern already —
follow it.

### Phase 3 — measurement on real devices

Wake-word detection rate, barge-in behaviour, transcript quality (use
`saveUtterances` — the tap is below NS, so the file is what STT actually
heard), CPU, and the delivery stats. Compare against the same devices on the
old path.

### Phase 4 — decide the default

Not before Phase 3 has numbers and the deferred item below is resolved.

## Deferred / filed

- **Speaker buffering depth.** Today: `audioChanDepth` 128 periods ≈ 5.46 s on
  the device, `primePeriods` 24 ≈ 1 s hold before start, controller `LEAD_S`
  4.0 s — sized against measured 1.8–2.6 s link stalls. `slspeaker` must
  reproduce this in its own ring; an AudioTrack buffer is tens of milliseconds
  and is not a substitute. **Filed, but this is a real regression if shipped
  without it** — it is the thing that keeps music from gapping on a marginal
  link, and it blocks Phase 4.
- **Per-device tuning of the mic chain.** `AFE.cfg` is on read-only `/system`,
  so the mic-path config keys become fleet-fixed rather than per-device. Filed;
  no work planned. If it is ever wanted, it means remounting `/system` and
  editing a system file, which is a device-image operation, not a config one.
- **LED direction arc.** Dropped on this path. The AFE selects a beam
  internally and `AFEDiagSelectedBeamNumber` exists only in ASP debug output,
  not per-frame. Out of scope.

## Risks

- **Everything above the phase-0 line is inferred from a 44-byte function.**
  `getAspPipelineType` is unambiguous about the mapping, but nothing has been
  recorded through it. Phase 0 exists to make that inference cheap to falsify.
- AudioFlinger adds buffering on both paths against a mic pipeline that already
  has a hard 160 ms deadline and shares a core with wake-word inference.
- The device stops owning its audio hardware outright, which is a real
  reduction in how much of the failure surface we control. Mute is unaffected
  (verified: separate mixer control), but boot ordering, jack behaviour and
  mediaserver restarts all become things the HAL decides.
- No fallback at runtime: if the AFE path misbehaves in the field, recovery is
  a firmware flag flip and a restart, not a config push. Keep the old backends
  compiled in for exactly this reason.
