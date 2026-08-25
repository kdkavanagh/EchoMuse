# The Voice Pipeline, Explained

What actually happens between you saying "Hey Rhasspy, turn off the lights"
and the lights going off — stage by stage, in plain language, with the
benefits and trade-offs of each design choice.

The one-sentence version: **the Dot is deliberately dumb** — it captures
sound as cleanly as possible and streams it out; all the intelligence
(recognising the wake word, deciding when you've finished speaking,
understanding you) lives on the controller and in Home Assistant, where it
can be updated, tuned, and observed without touching the hardware.

```
 YOUR VOICE
    │
    ▼
┌─ On the Echo Dot ───────────────────────────────────────────┐
│  7 microphones → the Echo's own audio front end             │
│                  (echo cancel, beamform, select, gain)      │
└──────────────────────────────│──────────────────────────────┘
                               │  continuous audio stream (WiFi)
                               ▼
┌─ On the controller ─────────────────────────────────────────┐
│  wake-word spotting → conversation management → sound shaping│
└──────────────────────────────│──────────────────────────────┘
                               │
                               ▼
┌─ In Home Assistant ─────────────────────────────────────────┐
│  speech-to-text → understanding → action → text-to-speech   │
└──────────────────────────────│──────────────────────────────┘
                               │  spoken response
                               ▼
                     back to the Dot's speaker
```

---

## Stage 1 — Seven microphones

The Dot has 6 microphones in a ring plus 1 in the centre, all captured
together and continuously.

**Benefit:** hearing from every direction at once, plus the raw material for
working out *which direction* you spoke from.

**Caveat:** they're tiny microphones in a small puck sitting in your room —
they hear the TV, the dishwasher, and the Dot's own speaker just as keenly
as they hear you. Most of the rest of the pipeline exists to deal with that.

## Stage 2 — The Echo's own audio front end

Everything between the raw microphones and one clean mono channel is done by
**the software the Echo shipped with**, the audio front end Amazon built for
Alexa. EchoMuse captures through Android's normal audio path at the
"voice recognition" setting, which is the switch that turns it on, and gets
back a single processed channel. Four jobs happen in there:

- **Echo cancellation**, per microphone. When the Dot is speaking, its
  microphones hear its own voice — loudly. A copy of exactly what the speaker
  is playing is subtracted from what each mic hears, leaving only *other*
  sounds — like you interrupting. This is why follow-up questions work over
  the tail of a response, why the device's own speech can't trigger it, and
  what makes **barge-in** possible.
- **Beamforming**, fixed and adaptive, combining the seven microphones into
  beams pointed in different directions.
- **Beam selection**, choosing the beam with the best signal-to-noise ratio —
  in practice, the one pointed at whoever is talking.
- **Gain**, both the analogue amplifier in the audio chips and a boost after
  the processing.

**Benefit:** it is tuned for this exact microphone array, by the people who
designed the array, and it is a genuinely sophisticated piece of signal
processing — per-microphone echo cancellation and an adaptive beamformer are
well beyond what EchoMuse could sensibly reimplement. Getting it costs nothing
and runs on hardware that was built for it.

**Caveats:** none of it is adjustable. Its tuning lives in a configuration file
on a read-only part of the device, so there are no gain sliders, no pickup
presets, no echo-cancellation switches — that whole section of the dashboard
went away when this became the audio path. It also means **the Dot must hand
its speaker back to Android**: playback goes through the same framework,
because that is where the echo canceller takes its reference from. Capture and
playback are inseparable here.

It also does nothing about the TV. Removing *other people's speech* is a
different problem, and a hard one — see Stage 6.

**Honesty note:** how well this performs on this hardware has not actually been
measured. It replaced EchoMuse's own beamformer and echo canceller, which were
measured, and the expectation is that Amazon's is better; that expectation is
reasonable and unverified.

## Stage 3 — The continuous stream

Every 80 milliseconds, the processed audio is sent over WiFi to the
controller. Always. There is deliberately **no** "only send when it sounds
like speech" gate on this stream.

**Benefit:** the wake-word recogniser sees smooth, uninterrupted audio,
which measurably improves its accuracy — and there's no on-device logic
that can drift, misjudge your room, or degrade over days (both of which
actually happened with earlier, cleverer designs; boring won).

That uninterrupted stream is also what makes it possible to run the *same*
recogniser on the Dot itself and compare the two on byte-identical audio —
which is exactly what the experimental on-device scoring mode does, without
being allowed to act on the result. It is also why gating this stream on
"sounds like speech" would be harder than it looks: the recogniser's internal
buffers assume continuity, and splicing gated bursts together measurably
depresses its scores.

**Caveat:** a constant ~32KB/s per device on your WiFi — about 1/6th of
what streaming the *response* audio uses, so in practice a non-issue on any
home network. And to be clear about privacy: the stream goes to *your*
controller on *your* LAN and nowhere else.

## Stage 4 — Wake-word spotting

The controller runs openwakeword, a small neural network, over each
device's stream, scoring every moment: "how much did that sound like the
wake word?" Cross the sensitivity bar and the conversation starts.

With more than one device online, the **first** Echo to hear you answers
straight away, and any other device detecting the same word within the
**arbitration window** (default 700ms, configurable) stands down silently.
One utterance, one response, even in earshot of two devices — and no added
latency, because the winner claims the turn on the spot rather than waiting
out the window.

An earlier design instead waited out the window and gave the turn to
whichever device heard you *best*. It was dropped for two measured reasons:
it taxed every wake by ~364ms even with nothing competing, and the
signal-to-noise winner produced a *worse* transcript than the device that
simply heard you first.

**Benefit:** because this runs on the controller rather than the Dot, you
can change the wake word or sensitivity live from the dashboard, see every
detection *and* every near-miss in the Status tab, and future improvements
don't need firmware updates.

**Caveat:** it's a probability, not a certainty — the sensitivity slider is
a false-accepts vs. false-rejects trade-off you tune to your room (the
near-miss counter exists precisely to make that tuning informed rather than
vibes-based).

## Stage 5 — The conversation ("turn")

On wake: the LED goes green, the device's mic selection locks toward you,
and the controller pipes your audio to Home Assistant, which decides when
you've stopped talking (its own speech detector does this — with a
controller-side backstop that quietly ends things after 5 seconds if a
false wake meant nobody was speaking, judged against that room's measured
background noise level).

**Benefit:** endpointing ("has the user finished?") is done by Home
Assistant's well-maintained detector rather than home-grown logic, and the
false-wake backstop adapts to each room by itself — a quiet study and a
loud lounge get equally sensible behaviour with zero tuning.

**Caveat:** in a noisy room, the detector sometimes hangs on a beat too
long and the tail of TV dialogue rides along into speech-to-text (you'll
occasionally see a stray phrase appended to your transcript). Cleaning the
audio sent to speech-to-text is the next planned fix for this.

## Stage 6 — Speech-to-text, understanding, action

Home Assistant's Assist pipeline takes over: your speech becomes text
(Whisper or whichever STT you've configured), the text becomes intent
("turn off + kitchen lights"), the action happens, and a reply is composed.

**Benefit:** this is all standard, well-documented Home Assistant machinery
— every STT/LLM/TTS option HA supports works, and EchoMuse doesn't need to
know anything about it.

**Caveat:** it's also where most of the *time* goes (transcription and
response generation are the slow steps, especially on modest hardware), and
where background-noise transcription errors ultimately land. Better mics and
cleaner audio help; they can't fully substitute for a good STT model.

## Stage 7 — The response

The reply audio comes back through the controller, which shapes the sound
(the EQ from the configuration guide — the raw speaker is boomy) and
streams it to the Dot, which plays it while a copy is fed to the echo
canceller (Stage 2) so the mics can subtract it. The audio arrives at the
hardware's native rate: the satellite tells Home Assistant what format the
speaker wants (48kHz mono), so recent HA versions transcode at source, and
ffmpeg covers anything else during decode.

While it plays, the ring throbs in time with the audio, and it clears when
the Dot reports that it has *actually* finished rather than when the
controller estimates it should have. The old estimate could clear the ring
several seconds before the speaker stopped on a slow WiFi link — the device
is the only party that knows when its own buffer runs dry.

**Benefit:** centrally-applied EQ means every device gets consistent,
tuned sound, adjustable live from the dashboard.

The reply is **streamed while Home Assistant is still generating it**: the
response is piped through ffmpeg and out to the Dot as it arrives, rather
than being fetched and decoded in full first. A long answer starts speaking
at roughly the same moment a short one would, instead of making you wait for
the last word to be synthesised before hearing the first. The EQ carries its
filter state across chunks, so there's no click at the joins.

**Caveat:** interrupting a response by voice (**barge-in**) works — say the
wake word over the top and the response cuts off. The mics stay live during
playback, and the Dot's echo cancellation (Stage 2) is what stops it waking
itself. Interrupting by *just talking* (without the wake word) is deliberately
not attempted.

## Beyond voice — music

Each Echo appears in Home Assistant as a **media player** you can
actually play things on: `media_player.play_media`, the HA media
browser, Music Assistant, radio streams. The controller decodes
whatever you throw at it with ffmpeg and streams it to the speaker,
running a few seconds ahead so a WiFi hiccup doesn't become an audible
gap. Pause and stop are still instant — they don't wait for that buffer
to drain, they throw it away.

Saying the wake word over music **ducks** it: the music drops to a quiet
bed under the answer and comes back up afterwards. It doesn't pause.
That matters because those few seconds of lead are already inside the
Dot when you start speaking, so ducking has to happen on the device —
and because a Music Assistant flow stream can't be seeked, so pausing
one used to cost you however long the conversation took, sometimes
landing you in the next track. The voice itself is never turned down,
only the bed under it. How far it drops is yours to set (**Ducking**,
in the Playback section) — it's a taste call best made by ear in the
actual room.

Wake-over-music leans on the same echo cancellation (Stage 2) that lets
you interrupt the assistant's own voice — it is what lets the Dot hear
you over a song it is playing itself.

Older firmware that can't mix the two streams falls back to the previous
behaviour — pause for the turn, resume after.

---

## Design principles, if you're wondering "why is it like this?"

1. **Dumb device, smart controller.** Anything that can drift, misjudge, or
   need tuning lives where it can be observed and updated without touching
   hardware. The Dot captures, hands the array to the audio front end it
   already had, and streams the result — that's it. The one exception proves
   the rule: ducking music under a voice turn happens on the device, because
   the next few seconds of music have already left the controller by the time
   you start speaking.
2. **Measure, don't modify.** The controller tracks each room's noise floor
   and uses it to make *decisions* (is anyone speaking?), but never rewrites
   the audio on its way to speech-to-text. Adaptive audio-mangling is how
   the system's worst historical bugs happened.
3. **Boring and continuous beats clever and gated.** The always-on,
   unprocessed wake stream replaced a cleverer design that degraded over
   days. When in doubt, the pipeline chooses the predictable option.
