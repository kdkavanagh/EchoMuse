# Stopping a ringing timer by saying "stop"

**Status:** specification. Nothing built yet.

## The short version

When a timer or alarm goes off on an Echo running EchoMuse, the only way to
silence it by voice is to say the full wake word — "hey jarvis" — and then
"stop". That works badly, because the device is playing a loud alarm into its
own microphones at the exact moment you need it to hear you.

This spec adds a second, small speech model that listens for the single word
**"stop"**. It runs all the time but is only allowed to *do* anything while an
alarm is actually ringing. Say "stop" during a ring and the ring stops. No wake
word, no round trip to Home Assistant.

This is the same design Amazon uses on the stock device, discovered by taking
apart the original firmware. See [alexa-endpointing.md](alexa-endpointing.md)
for that investigation.

---

## Background: what happens today

### The pieces involved

A few terms, since they come up throughout:

- **Wake word.** The phrase that gets the device's attention — "hey jarvis" by
  default. A small neural network scores every 80 milliseconds of microphone
  audio and reports how confident it is that the phrase was just spoken. Cross
  a confidence threshold and a conversation begins.
- **openWakeWord.** The open-source engine EchoMuse uses to do that scoring.
  It is what makes a custom wake word possible.
- **Echo cancellation (AEC).** Subtracting the device's own speaker output from
  what its microphones pick up, so it can hear you over itself. Off by default
  on this fleet.
- **Timer ring.** When Home Assistant reports a timer has finished, the
  controller plays an alarm sound on the device in repeated bursts, with silent
  gaps in between, for up to a configured number of seconds.

### Why stopping a ring is hard

Three things stack up:

**1. Nothing upstream can stop it.** Home Assistant discards a timer at the
moment it fires. Once the alarm is sounding, Home Assistant no longer has a
timer to cancel — so "stop" spoken as a normal voice command has nothing to act
on. The controller is the only thing that knows the alarm exists.

**2. The wake word is at its weakest.** The device is playing an alarm at full
volume roughly ten centimetres from its own microphones. Speech competing with
that scores far lower than normal. This is the one moment the wake word most
needs to work and is least able to.

**3. With echo cancellation off, the device is deaf during the noise.** The
wake word listener currently *discards* microphone audio while the alarm is
audible, unless echo cancellation is switched on. So on a default device,
"hey jarvis" is only heard during the silent gaps between alarm bursts.

The result is a ring that is hard to stop by voice. There is a physical button
that works — pressing it is deliberately given priority over almost everything
else — but reaching for a device across the room is not the point of a voice
assistant.

---

## What Amazon does

Taking apart the stock firmware showed the original device solves this with a
dedicated small model rather than the wake word.

Its speech library ships a set of short phrases it can spot directly:

```
com.amazon.speech.LocalCommand.Stop
com.amazon.speech.LocalCommand.Snooze
com.amazon.speech.LocalCommand.Cancel
```

Three words, handled entirely on the device with no network involved. They are
received by the alarms subsystem and **thrown away when nothing is ringing** —
the firmware's own log line is literally `Received local command stop but no
alert is ringing.`

Two details worth copying:

- The "stop" spotter lives **in the same bundle as the wake word** and runs on
  the same audio continuously. It is not started and stopped.
- There is **no wake word prefix**. Just "stop".

Two details worth *not* copying: their model files are Amazon's, are not
present on a device that was never signed in to an Amazon account, and cannot
be recreated. We train our own instead — EchoMuse already has a tool for that.

---

## Design decisions

### 1. Always listening, but only allowed to act while ringing

The model runs continuously alongside the wake word model. What is gated is not
the listening, it is the *permission to act*: a "stop" detected while no alarm
is ringing is logged and discarded.

**Why not just switch it on when the alarm starts?**

Two reasons, and the second is the important one.

*It is nearly free to leave running.* Both models share the expensive part of
the work. Scoring audio happens in two stages: first the audio is converted
into a compact numerical summary, then each model's small final layer reads
that summary and produces a score. The first stage is most of the cost and is
done once regardless of how many models are loaded. Measured on the live
controller:

```
1 model : 2.044 ms per 80 ms of audio
2 models: 2.135 ms  (+0.091)
3 models: 2.341 ms  (+0.149 per extra model)
```

An extra model costs about a tenth of a millisecond against an 80 millisecond
budget. There is no CPU argument for switching it off.

*It needs a running start.* The scorer is not stateless — it works from a
rolling window of roughly the last second of audio, and its scores are
meaningless until that window has filled. Starting it at the moment the alarm
begins would leave it blind for the first second, which is exactly the second
you cannot afford to lose. Leaving it running means it is already warm.

**Both models must be loaded into a single scorer instance**, not two. Two
separate instances would each redo the expensive first stage and double the
cost — 2.0 ms becomes 4.1 ms.

### 2. Timers only — not music, not spoken replies

The gate is "a timer is currently ringing", and nothing else.

**Music is deliberately excluded.** Music can already be stopped four ways: the
Home Assistant media player control, the physical button, the phone app, and
"hey jarvis, stop" — which works today, because unlike a fired timer the music
is still there for Home Assistant to act on. The only thing a bare "stop" would
add is skipping the wake word.

Against that, it would mean a detector for a very common short word being live
and armed for the entire length of every song, where a false trigger stops the
user's music for no reason. Bad trade for a small convenience.

**Spoken replies are already handled** by the existing barge-in feature (saying
the wake word during a reply interrupts it).

The timer ring is the only case where the capability is genuinely missing.

### 3. The word is "stop", not "hey jarvis stop"

- **"hey jarvis stop" already works.** It goes through the normal wake word and
  a voice turn. A dedicated model for it would add nothing.
- **Requiring the wake word defeats the purpose.** The entire problem is that
  the wake word struggles over a blaring alarm. Making it mandatory keeps the
  hard part.
- **Amazon uses the bare word**, matching how a real Echo behaves.
- **Short phrases are what this kind of model is good at.** The scorer works
  over a fixed, short window of audio. A two-word phrase means a longer window
  and a harder model to train well.

The risk of a bare common word is false triggers — but that risk is contained
precisely *because* the model is only allowed to act during a ring. That is a
window of tens of seconds, during which the user is expected to say "stop", and
the worst possible outcome is an alarm that stops slightly early. Compare that
to an alarm nobody can stop.

---

## How it works, step by step

1. A timer finishes. Home Assistant tells the controller. The controller starts
   playing the alarm sound on the device in bursts and marks the device as
   ringing.
2. The wake word listener, which was already running, keeps scoring every 80
   milliseconds of microphone audio. It now gets back **two** scores — one for
   "hey jarvis", one for "stop".
3. The "stop" score is compared against its own threshold, and only acted on if
   the device is currently ringing.
4. On a match, the controller silences the ring through the existing
   `stop_timer_ring` path and logs which model fired and what it scored.
5. No conversation is started. The word was consumed silencing the alarm.
   (This mirrors what already happens when the wake word stops a ring — saying
   "hey jarvis" to silence an alarm does not then open the microphone.)

Existing behaviour that does not change: the physical button still stops a
ring and still outranks everything, muting still stops a ring, and the wake
word still works as a second route.

---

## What needs building

**1. Train the model.** `oww_forge` is the existing tool for this: a
containerised batch job that generates synthetic speech, augments it with noise
and room echo, trains a small classifier, and exports a model file. Target the
single word "stop". Worth training against alarm-like background noise
specifically, since that is the only condition it will ever run in.

**2. Install it.** Existing path, no new plumbing: the dashboard's Config →
Wake word → "+ Custom model" upload puts the file next to the database and the
controller can then load it by path.

**3. Load both models in one scorer.** Where the controller currently builds
its wake word scorer with a single model, it needs to build it with two when a
ring-stop model is configured. The scores come back keyed by model, so both are
available from the same call.

**4. Act on the score.** A check in the wake word listener: if the device is
ringing and the ring-stop score clears its threshold, silence the ring.

**5. Settings and a dashboard control**, listed below.

**6. Tests.** The decision — *should this score, in this state, stop the ring?*
— should be a small pure function with its own tests, following the pattern
already used for button presses, link authentication and turn timing. The
reason those exist is that each of them broke once in a way no integration test
caught.

---

## Settings

All device-scoped, under the **Wake word** section:

| Setting | Default | What it does |
|---|---|---|
| `ringStopModel` | empty | Which model listens for "stop". Empty disables the whole feature. |
| `ringStopThreshold` | to be measured | How confident the model must be before it silences a ring. |

`ringStopThreshold` needs its own value and must **not** reuse the existing
wake word or barge-in thresholds. It is a different model with a different
score distribution, judged under a very specific and unusual acoustic
condition. Borrowing a number from another model would be guesswork wearing a
sensible-looking value.

The feature is off by default, because it does not exist until someone trains
and uploads a model.

---

## Five things that will go wrong if you are not careful

**1. Silencing the ring is not the same as cancelling a conversation.** There
is an existing code path for cancelling an in-progress voice turn, and it looks
like it would work here. It does not. A ring holds an internal lock; the cancel
path stops the *sound* but leaves the ring alive and holding that lock for its
full configured duration. The result is a device that appears to have obeyed
you and is then unable to take a voice command for the next minute. Use
`stop_timer_ring` and nothing else. This exact mistake has already been made
once and is written up in the project's main documentation.

**2. The alarm sound may trigger the model itself.** A device that silences its
own alarm the moment it starts is worse than one that ignores you. This risk
already exists for the wake word and is documented; the log records whether a
score came from a moment when the alarm was audible or from a silent gap, which
is how the two are told apart. The proper fix already identified — scoring an
uploaded alarm sound against the model at upload time and warning if it matches
— now needs to check **both** models, not just the wake word.

**3. With echo cancellation off, the device only hears the gaps.** The
listener discards microphone audio while the alarm is audible unless echo
cancellation is enabled. So on a default device, "stop" is only heard in the
silent gaps between alarm bursts, and how quickly it responds depends on the
configured burst and gap lengths. This is a real limitation, not a bug, and it
should be stated in the user-facing description. Turning echo cancellation on
makes the device able to hear over its own alarm — at the cost of the risk in
point 2.

**4. On-device wake word detection will silently break it.** EchoMuse can
optionally run wake word scoring on the Echo itself rather than the controller.
That path has room for four models on the device and evicts the least recently
used one to make space — and only the *selected* wake word model is protected
from eviction. A ring-stop model would eventually be deleted, and the symptom
would be the feature quietly not working with no error anywhere. It needs the
same protection.

**5. Multi-device fleets need a decision, but not yet.** If one Echo is ringing
and you say "stop" to a different one in the same room, nothing happens. For a
first version, the ringing device listens for its own stop word. Making a
ring stoppable from any device in earshot is a separate piece of work, and it
overlaps with the existing multi-device wake arbitration logic.

---

## How to tell whether it is working

- The ring log line already records the score and whether the alarm was audible
  at the time. Extend it to name which model produced the score. A ring that
  silences itself and a ring silenced by a person look identical otherwise.
- Count how often a "stop" is detected while nothing is ringing. That number is
  the false-trigger rate, and it is worth knowing even though nothing acts on
  it — it says how safe it would be to widen the feature to music later.
- The existing utterance recording feature can capture what the microphone
  heard, which is the fastest way to answer "why did it not respond".

---

## Out of scope

- **"Snooze" and "cancel."** Amazon has them; Home Assistant's timers do not
  really have a snooze concept. Add later if wanted — the mechanism is
  identical, only the model and the action differ.
- **Extending to music.** Deliberately excluded, reasoning above. Revisit only
  with real false-trigger numbers in hand.
- **Using Amazon's own speech engine.** Evaluated and rejected — the audio
  plumbing turns out to be workable, but the model files are absent, cannot be
  recreated, and would take substantial work to reach even if they existed. See
  [alexa-endpointing.md](alexa-endpointing.md).

---

## Open questions

- What threshold does a "stop" model actually need over a ringing alarm? Only
  measurable once a model exists. Recording a few real rings with the alarm
  playing is the way to answer it.
- Should the ring-stop word be user-choosable, or fixed at "stop"? Choosable is
  more work for little gain, but "stop" is a common enough word in ordinary
  speech that some rooms may want something more distinctive.
- Should a detected "stop" while nothing is ringing also stop music, once
  there is evidence about the false-trigger rate? Explicitly deferred rather
  than refused.
