# The Voice Pipeline, Explained

What actually happens between you saying "Ophelia, turn off the lights" and
the lights going off, stage by stage, in plain language, with the benefits
and trade-offs of each design choice.

The one-sentence version: **the Dot listens for the wake word itself and
sends no microphone audio anywhere until it hears it (or you press the
button); the controller decides
whether it was really you, when you have finished speaking, and where your
words go; Home Assistant transcribes, understands, and answers.**

Work that has to run all the time, for every speaker, runs on the Dot. Work
that only runs while someone is talking runs on the controller, where the
models are much faster. An idle speaker costs the controller nothing and its
audio stays in the room.

```
 YOUR VOICE
    │
    ▼
┌─ On the Echo Dot (always running) ───────────────────────────────┐
│  7 mics → Echo's own audio front end (echo cancel, beamform)     │
│        → short memory of recent audio and of what it played      │
│        → wake-word model ── "I think I heard it" ──┐             │
└────────────────────────────────────────────────────│─────────────┘
                                                     │ only now: audio
                                                     ▼
┌─ On the controller (only while someone is talking) ──────────────┐
│  was it really you? → speech detector + live transcriber         │
│  → "you've finished" → wake word removed from the text           │
└────────────────────────────────────────────────────│─────────────┘
                                                     ▼
┌─ In Home Assistant (stock, no custom integration) ───────────────┐
│  speech-to-text → understanding → action → text-to-speech        │
└────────────────────────────────────────────────────│─────────────┘
                                                     │ spoken reply
                                                     ▼
                                    back to the Dot's speaker
```

The design reference behind all of this is
[post-afe-audio-architecture.md](post-afe-audio-architecture.md); the
messages between Dot and controller are in [protocol-v1.md](protocol-v1.md).

---

## Stage 1 — Seven microphones and the Echo's own audio front end

The Dot has 6 microphones in a ring plus 1 in the centre. Everything between
those microphones and one clean mono channel is done by **the software the
Echo shipped with**, the audio front end Amazon built for Alexa. EchoMuse
captures through Android's normal audio path with the "voice recognition"
setting, which is what switches that front end on, and gets back one
processed 16 kHz channel in 80 ms blocks. Inside it:

- **Echo cancellation.** A copy of what the speaker is playing is subtracted
  from what the microphones hear, so the Dot can hear you over its own voice
  or music.
- **Beamforming and beam selection**, combining the microphones into beams
  and picking the one pointed at whoever is talking.
- **Gain**, in the audio chips and after processing.

**Benefit:** it is tuned for this exact microphone array by the people who
designed it, and it costs nothing to use.

**Caveats:** none of it is adjustable. There are no gain sliders, pickup
presets, or echo-cancellation switches in the dashboard, because there is
nothing to connect them to. Playback must also go through the same Android
audio path, because that is where the echo canceller takes its reference
from; capture and playback are inseparable. And it does nothing about the TV:
telling your voice apart from other people's speech is handled later
(Stage 6), and only partly.

## Stage 2 — A short memory, and nothing sent

The Dot keeps three rolling buffers in RAM: the last 6 seconds of microphone
audio, the last 8 seconds of **what it actually played** (its final speaker
mix, after volume and ducking), and 16 seconds of a loudness reading for
every 32 ms of microphone audio. Nothing in them leaves the Dot unless
something opens a window for it (Stage 5).

**Benefit:** an idle Dot uploads no audio at all. When you do speak, the
controller still gets the moments *before* the wake word was recognised, so
your first word is never clipped.

**Caveat:** the memory is short on purpose. It only covers what a single
turn needs, and it is erased on privacy mute.

## Stage 3 — Wake word, on the Dot

The Dot runs a small neural network (a BCResNet model) over the microphone
continuously: every 160 ms it scores the last 1.4 seconds of audio for "how
much did that sound like the wake word?", and compares the average of its
last three scores with a threshold. When the average crosses it, the Dot
sends the controller a **wake candidate**: "I think I heard it, here is
when." It does not start a turn on its own.

The threshold depends on what the Dot is doing, and the Dot decides that
because it owns the speaker:

| Dot is… | Profile | Threshold (shipped model) |
|---|---|---:|
| silent, or speaking a reply | idle | 0.90 |
| playing music, or ringing an alarm or timer (including the gaps) | playback | 0.65 |

Scores that come close without crossing are counted as **near misses** and
shown on the Status tab. No audio goes with them.

Which model runs is chosen in **Config → Wake word**. Models live in the
controller's registry, each identified by the SHA-256 of its file, with its
own thresholds and spoken form: thresholds belong to the model, because
scores mean different things for different models. The controller tells each
Dot which model to run by hash, and the Dot fetches and checks exactly those
bytes. Uploading a model validates it and runs a quick silence/noise/tone
test, but never switches to it; selecting it is a separate step.

**Benefit:** always-on detection costs the controller nothing, and adding
speakers does not add controller load. The same hash-checked model runs on
every Dot.

**Caveats:** it is a probability, not a certainty. There is no sensitivity
slider; thresholds come with the model. Only BCResNet audio-in ONNX models
with their JSON sidecar are accepted. If a Dot cannot load the model, the
Status tab says so, and the action button still starts turns.

## Stage 4 — Was that really you?

The controller runs one **session actor** per Dot (`em_session`). It is the
only thing allowed to start, end, or cancel a turn, so there is never a
second listener competing with the first.

- **Dot silent:** the candidate is accepted straight away.
- **Dot making sound** (a reply, music, an alarm): the Dot immediately turns
  its own sound down by the duck depth, so you hear it react and the checks
  below hear less playback. The controller then checks, in order:
  1. Did the Dot *play* something wake-word-like at that moment? It scores
     the Dot's own speaker mix with the same model. If so, it is rejected as
     the Dot hearing itself, unless there is clear extra speech on top.
  2. Does the Dot's own playback explain what the microphones heard? If so,
     rejected as echo (`em_attribution`).
  3. Does the live transcriber hear the wake word in that audio? If not,
     rejected as unverified.

  All of this must finish within 700 ms of the candidate arriving, or the
  candidate is rejected. Rejection restores the sound level; nothing else
  happens.
- **More than one Dot:** the first Dot whose candidate is accepted answers;
  any other Dot accepting within the **arbitration window** (default 700 ms,
  0 disables) stands down silently. A wake word said to stop a ringing alarm
  or a reply skips arbitration, so a Dot in the next room cannot steal your
  "stop".

**Benefit:** you can say the wake word over music, a reply, or an alarm, and
the Dot's own voice saying "Ophelia" does not start a turn. Barge-in is
always on; there is nothing to enable.

**Caveat:** if you say the wake word at exactly the moment the Dot's own
audio says it, the two can be indistinguishable, and the wake may be missed.

## Stage 5 — Opening the window

Accepting a wake opens an **uplink lease**: permission for the Dot to send
audio for this turn. It starts with a backfill from the Dot's memory,
beginning 300 ms before the wake word, followed by live microphone audio. It
also carries the loudness readings and, when the Dot is making sound, what it
played. The controller renews the lease every second; if the controller
disappears, the lease expires on its own within 3 seconds.

If **Wake chime** is on (off by default), the Dot plays a short earcon. On
current firmware it does not wait for this stage: it chimes the moment it
detects the wake word. Only a wake heard while the Dot is playing audio, or on
older firmware, chimes here, once the controller has accepted it.

**Benefit:** microphone audio flows only while a turn needs it, and only to
your controller.

## Stage 6 — Listening: speech evidence on the controller

The controller's speech worker (`em_speech_worker`, using sherpa-onnx) runs
two models on the uploaded audio, only while a lease is open:

- **Silero VAD**, a speech detector: "is this 32 ms slice speech?"
- **Kroko**, a streaming speech recogniser that transcribes as you speak. It
  supplies timing evidence, local commands, and where the wake word sits in
  your sentence; the final words come from Home Assistant (Stage 8).

Both read a copy of the audio boosted by a fixed +20 dB, because the Echo's
front end delivers speech too quietly for them. The models ship as a
hash-pinned bundle fetched by `controller/tools/fetch_speech_bundle.py` (the
Docker image does this at build time); the controller refuses to start if a
file is missing or does not match.

Every 32 ms slice is then labelled with what most likely produced it: the
Dot's own playback, non-speech (fan, hum, silence), background speech
(clearly quieter than you: a TV across the room), your command, or unknown.
The yardstick for "you" is how loud the wake word was (for a button turn,
your first words). The audio itself is
never edited; the labels only decide timing.

**Benefit:** a fan or dishwasher never keeps a turn open, and quieter
background talk does not count as you still speaking.

**Caveat:** one microphone channel cannot pull apart two voices at the same
level. A TV at conversational volume, or a second person talking straight
after you, counts as your command.

## Stage 7 — Deciding you've finished

The **endpoint reducer** (`em_endpoint_policy`, fixed policy `post_afe_3`)
decides when your utterance ends, measured in audio time, never wall-clock
time, so a slow network cannot shorten or lengthen a pause. It ends a turn in
one of these ways:

- **Normal pause.** The speech detector, the transcriber's trailing silence,
  and the transcript all agree you stopped. The streaming transcriber only
  updates every 1.28 s, so 320 ms into each pause the controller re-transcribes
  the whole utterance at once to get your last word without waiting for it.
  How long it waits depends on whether your words already form a complete
  command: 608 ms for a complete one
  ("turn off the kitchen lights", "stop"), 1,216 ms when a longer name could
  follow ("turn off the kitchen…"), 1,792 ms otherwise, including every
  free-form question. For those 1,792 ms waits, speech clearly quieter than
  you (a TV across the room) counts as silence, so a question asked with the
  TV on ends as it would in a quiet room.
- **Complete command under background speech.** A recognised, complete
  command followed only by quieter speech that does not add to it ends
  without waiting for silence. This is the TV-room path, and it only covers
  commands the controller's grammar knows (timers, alarms, on/off of exposed
  entities, local commands).
- **Bounded failure.** No speech within 5 seconds of the wake word ends
  quietly with the no-speech animation. Speaking for 15 seconds without a
  real pause ends with "That was too long. Try a shorter request." (30 seconds
  with **Extended utterances**). Three seconds without the transcript making
  progress ends with "Sorry, I didn't catch that." and a chance to repeat,
  unless what was heard is already a complete command. A dropout in the audio
  ends the turn with the error animation rather than guessing.

A tentative end is held for 192 ms. If you carry on talking, it is revoked
and listening continues; otherwise the audio from the start of the pre-roll
to 192 ms after your last word is frozen, and nothing after that can change
the request.

**Benefit:** short commands finish quickly, questions get time for a
thinking pause, and failure modes are named rather than guessed. See
[led-ring-states.md](led-ring-states.md) for what the ring shows at each
point. Home Assistant's **Voice state** sensor follows the same phases as the
ring: `listening` (from the accepted wake word or button press until the
request is frozen, and again while a follow-up answer is awaited), `thinking`
(speech-to-text, intent and TTS), `speaking` (the response or question
plays), and `idle`. Wake-word and button turns run over Home Assistant's
websocket API, not through the satellite, so the stock Assist satellite
entity stays idle for them.

**Caveat:** a free-form question asked over a TV about as loud as you only
ends at a real pause, or at the length limit. And in a free-form question,
dropping well below your own volume for about two seconds reads as the end:
anything said after that is not part of the request.

## Stage 8 — Speech-to-text, in Home Assistant

The frozen span goes to Home Assistant's own pipeline in a **speech-to-text
only** run, using the STT engine of the pipeline selected on the speaker's
satellite device in Home Assistant. Home Assistant's own end-of-speech
detection is switched off for this run, so it transcribes exactly the span
EchoMuse chose. A transport failure is retried once, since transcription has
no side effects.

What HA receives is a separate **STT copy** (`em_stt_copy`): a gain of
nominally +20 dB, adjusted by how loud your wake word was, and, with
**Noise suppression** on, the DTLN denoiser. These touch only the copy sent
to speech-to-text, never the audio used for wake or endpoint decisions.

**Benefit:** you keep whatever STT engine you configured in Home Assistant,
and its accuracy, while EchoMuse keeps control of when you finished.

**Caveat:** this is still where background noise errors land. The span
includes everything the microphones heard, TV included. And it is where much
of the time goes on modest hardware.

## Stage 9 — Removing the wake word, and routing

No audio is ever trimmed; the span always starts before the wake word, so
your first command word is never cut off. Instead the controller
(`em_wake_phrase`) removes the wake word from the **text**: the live
transcript shows where it was said, and everything up to and including the
closest match to "Ophelia" near that position in HA's transcript is removed.
"Ophelia, what does Ophelia mean?" becomes "what does Ophelia mean?". The live
recognizer can miss a quiet wake word entirely; then the match is sought at
the start of HA's transcript instead. A leftover "Ophelia," would stop Home
Assistant's built-in sentences from matching and hand the request to your
conversation agent.

Then the request goes to one of four places:

1. **Local commands** ("stop", "cancel", "snooze", "stop the timer") while
   something was ringing or speaking when you woke it: handled on the spot,
   without Home Assistant at all. These are decided from the live transcript
   right after Stage 7, before any STT run. See
   [Interrupting](#interrupting-barge-in-and-ophelia-stop).
2. **Alarm commands** ("set an alarm for 7 am on weekdays"): handled by the
   controller's alert engine. See [Timers and alarms](#timers-and-alarms).
3. **Alarm questions and the timer cancels Home Assistant cannot answer**
   ("when's my next alarm", "cancel the timer", "cancel all timers"):
   answered by the controller, the cancels through Home Assistant's own timer
   intents, without the conversation agent. See
   [Timers and alarms](#timers-and-alarms).
4. **Everything else**, including every other timer command: on to Home
   Assistant.

## Stage 10 — Understanding and action

The cleaned text goes to Home Assistant in an **intent-to-speech** run, with
the speaker's HA device and any ongoing conversation. Home Assistant's
conversation agent, local intents, timers, and chosen TTS voice all apply as
normal, and every turn shows up in HA's pipeline debug view.

The controller allows 30 seconds for an answer (an LLM agent may call
tools). A request is **never resent**: if the connection drops after it was
sent, the Dot says "I'm not sure that worked." rather than risk doing it
twice. Interrupting a turn stops the reply, but cannot undo an action Home
Assistant has already taken.

The controller connects to Home Assistant with `HA_URL` and `HA_TOKEN` (an
administrator's long-lived token) in Docker or bare metal, or with the
Supervisor token as an add-on. Each Dot also still appears in Home Assistant
as an ESPHome voice satellite with a media player; see
[quickstart.md](quickstart.md) and [configuration.md](configuration.md).

## Stage 11 — The response

The reply audio is fetched from Home Assistant as soon as it is available,
decoded with ffmpeg to the speaker's native 48 kHz mono, run through the EQ
from the Playback section, and streamed to the Dot while HA is still
producing it. The Dot buffers about 1 second before it starts and can hold
about 5.5 seconds, so a Wi-Fi hiccup does not become a gap.

While the reply plays, any music on the Dot stays playing underneath, turned
down by **Duck depth** (default −18 dB). The turn ends when the Dot reports
that its speaker has actually finished, not when the controller estimates it
should have.

**Benefit:** long answers start speaking about as fast as short ones, and
every Dot gets the same tuned sound.

## Stage 12 — Follow-up questions

When Home Assistant's agent asks you something back (`continue_conversation`),
the controller sets up a **reply expectation** instead of an open mic. You
can answer without the wake word:

- The window opens when the question has finished playing and lasts
  7 seconds. An answer that starts in the last moments of the question is
  still caught, because the audio was kept.
- You can talk over the question only if you are clearly louder than the
  room and the Dot's own playback does not explain it; otherwise it plays out
  and your answer is taken from the retained audio.
- At most five answers without the wake word, or 60 seconds of back and
  forth, whichever comes first. Then say the wake word again.
- A silent window closes without sending anything to Home Assistant.

EchoMuse asks its own questions the same way. "Set an alarm for seven" gets
"AM or PM?"; an unclear answer gets "Please say AM or PM." once, then
"Okay, I left it."

Conversations started by a Home Assistant automation
(`assist_satellite.start_conversation`) work too. Their replies go back
through the ESPHome satellite path so HA applies the automation's prompt,
and they end at the first pause of 1,024 ms.

**Caveat:** replies without a wake word are only accepted after an explicit
question. EchoMuse does not keep listening after ordinary answers.

## The action button

- **Tap:** starts a turn, with the audio from 300 ms before the press. There
  is no wake word to remove.
- **Tap during a turn:** cancels it.
- **Tap while something rings:** stops it, on the Dot itself. That press
  starts nothing else.
- **Hold:** sent to Home Assistant as an event for your own automations.

With **Tap sends an event** (the Advanced section), a tap goes to
Home Assistant instead of starting a turn. Details are in
[configuration.md](configuration.md).

## Interrupting: barge-in and "Ophelia, stop"

Every voice command, including stop, starts with the wake word. Nothing
listens for a bare "stop".

- **Over a reply:** the wake word cuts the reply off at once and starts a new
  turn. "Ophelia, stop" (or "cancel") then ends it quietly, without asking
  Home Assistant.
- **Over music:** the music ducks under your turn and comes back afterwards.
- **Over an alarm or timer:** the wake word moves the ring into the
  background (the sound stops, the ring stays pending) so you can speak.
  - "Ophelia, stop" (or "cancel", or "stop the timer", "turn off the alarm")
    dismisses the ring that was sounding when you said the wake word, even if
    another one became due while you were talking.
  - "Ophelia, snooze" snoozes an alarm. Timers cannot be snoozed.
  - Anything else ("Ophelia, what's the weather?") is a normal turn, and the
    ring comes back when it ends.
  - A ring stays in the background for at most 15 seconds in total; then it
    takes over again.

No chime plays for these; the sound stopping is the acknowledgement. Voice
stop needs the controller, but not Home Assistant or its STT. The action
button needs neither.

**Caveat:** extra words make it a normal request. "Ophelia, stop please" is
a local stop; "Ophelia, don't stop" and "Ophelia, stop the music" go to Home
Assistant.

## Timers and alarms

**Timers** work exactly as on Home Assistant's own voice satellites. Home
Assistant holds every timer; timer commands are ordinary requests, answered by
Home Assistant's own timer intents without the LLM. Its built-in sentences miss
common phrasings, so the Home Assistant configuration adds custom sentences
(`custom_sentences/en/timers.yaml`) for them:

- "Set a timer for 5 hour, 15 min", "set a 5 and a half hour timer", "set a
  timer for 5 min, 10 second": "Timer set for 5 hours and 15 minutes."
- "How much time is left [on the 5 minute timer]?", "What timers do I have?":
  "You have 2 timers: a 5 minute timer with 3 minutes left and a 10 minute
  timer with 8 minutes left."
- "Cancel the 5 second timer": "5 second timer cancelled." Likewise "add 2
  min to the timer", "pause the 5 minute timer", "resume my timer".

The controller answers the two cancels Home Assistant's own answer can't name:
"cancel the timer" cancels this speaker's only timer and says which ("5
second timer cancelled."), or with several asks "Which one? Your 5 minute
timer or your 10 minute timer?" (answer without the wake word: "the 10
minute one", "both", or "never mind"); "cancel all timers" cancels only this
speaker's timers and names them.

When a
timer finishes, the controller tells the Dot to ring: it loops the
**Timer sound** with a pause (**Gap between repeats**) between loops, for at most the
**Timer ring limit** (default 15 minutes). Music pauses while it rings. The
ring shows the first active timer's remaining time.

Like other HA satellites: timers are lost if Home Assistant restarts; a
timer that finishes while HA cannot reach the controller, or the controller
cannot reach the Dot, does not ring (the dashboard shows it); a timer with an
action ("in 10 minutes turn off the lights") runs the action and does not
ring.

**Alarms** live in a stock **Local Calendar**, one per speaker ("EchoMuse
Office"), which the controller creates. Set them by voice, from the
dashboard, from Home Assistant's calendar, or through an LLM agent: the
controller installs five scripts exposed to Assist (set, list, cancel,
dismiss, snooze). The Dot keeps a copy of the next 7 days of alarms on its
own storage and rings them itself, so an alarm rings, and the button stops
it, even when the controller or Home Assistant is down. By default an alarm
rings for up to 10 minutes and snoozes for 9. An alarm missed while things
were down still rings if it is less than 30 minutes late.

Home Assistant also shows two entities per speaker: **Alert ringing** and
a **Stop alert** button that ends whatever is ringing.

## Music

Each Echo is a Home Assistant **media player** you can play things on:
`media_player.play_media`, the media browser, Music Assistant, radio. The
controller decodes it with ffmpeg and streams it to the Dot. Music:

- **ducks** under any voice turn and reply by **Duck depth**, and keeps
  playing;
- **pauses** while an alarm or timer rings, and resumes when the ring
  queue is empty;
- takes a pause, resume, or stop sent during a turn **after** the turn
  ends, so the turn does not bring back music you stopped.

**Caveat:** sound played outside EchoMuse's mixer (another Android app,
Bluetooth, the aux input) is not in the Dot's record of what it played, so
checks against its own playback (Stage 4) are weaker while it plays.

## Privacy mute

The mute button is enforced on the Dot. It mutes the microphone hardware,
lights the mute button, turns the ring red, and survives a reboot. On mute the Dot stops
wake detection, ends every uplink lease, and erases its microphone memory; a
turn in progress ends. Alarms and timers still ring and the button still
stops them. Unmuting starts fresh.

## What is kept on disk

On the controller, in its data directory:

| What | When | Kept |
|---|---|---|
| Turn history: time, trigger, what the controller's recognizer heard, Home Assistant's transcript and the text sent on, the spoken answer, per-stage timings, why it ended | always | newest 20,000 per device |
| Rejected wake candidates: score and reason, **no audio** | always | in the same history |
| **Save utterances**: the STT copy sent to Home Assistant, as WAV | off by default | newest 10 per device |
| **Save wake clips**: each accepted wake word, from 300 ms before it | off by default | newest 500 per device |

Clicking a turn on the Activity tab shows it stage by stage: the wake score,
what the controller's streaming recognizer heard and why it stopped
listening (the route, how complete the text looked, the silence it waited),
Home Assistant's transcript and the text sent to intent after the wake word
was removed, who handled it (Home Assistant's built-in agent, the
conversation agent, the alarm engine or a local command) and what was said
back. Both recordings play from the Activity tab. Local commands such as "stop"
never reach speech-to-text, so they have no utterance recording. The Dot's
own buffers are RAM only. Dashboard recording modes (the Samples tab,
Ambient recording) hold their own lease and show while they run; no wake is
accepted while one runs.

Your committed speech goes to the STT engine configured in Home Assistant,
and the text to its conversation agent. If those are cloud services, that is
where it goes. For everything EchoMuse itself connects to, see
[what leaves your network](configuration.md#what-leaves-your-network).

## When parts are unavailable

| Down | What still works |
|---|---|
| Home Assistant | Alarms ring from the Dot; "Ophelia, stop" and the button stop them. No other voice requests. Creating an alarm fails with the error cue rather than pretending to succeed. |
| Controller | Alarms already on the Dot ring; the button stops them. No voice at all. |
| Speech worker (three failures within a minute) | Wake word and button give the error animation instead of a turn until it recovers; it retries every 10 seconds. Alarms, mute, and button stop are unaffected. |
| Old firmware | The Dot connects in upgrade-only mode: the dashboard can upgrade it, and voice and alarms are unavailable until it does. |

---

## Design principles, if you're wondering "why is it like this?"

1. **Continuous work on the Dot, per-utterance work on the controller.**
   The wake word, the short memory, the mixer, and alarm ringing have to run
   all the time, so they run where they cost nothing extra per speaker.
   Speech detection and transcription only run while someone speaks.
2. **One owner per decision.** The Dot proposes wakes; the session actor
   accepts them and ends turns; the Dot owns the speaker and what is
   audible; Home Assistant owns the meaning, timers, and alarm schedules.
   No two components race to act on the same audio.
3. **Measure, don't modify.** Evidence decides timing, but the audio sent to
   speech-to-text is the real audio, with no pieces cut out. The wake word is
   removed from text, never from sound.
4. **Unknown is not silence.** A network dropout, a missing reading, or an
   unclear transcript ends a turn with a named reason rather than a guess.
5. **Stock Home Assistant.** Everything uses stock APIs, a stock calendar,
   stock scripts, and the ESPHome satellite HA already understands.
