# EchoMuse

Give your Amazon Echo Dot 2nd Generation a second life as a fully local,
open-source voice assistant and media player for Home Assistant.

EchoMuse replaces the Alexa firmware with a lightweight Go server and pairs
it with a Python controller that presents each Dot to Home Assistant as a
native **ESPHome voice satellite** — no cloud, no custom HA integration to
install. Say the wake word, talk to [Assist](https://www.home-assistant.io/voice_control/),
hear the answer through the Dot's speaker.

## What you get

- **Wake word → Assist → spoken response**, fully local. The wake word
  ("Ophelia" with the bundled model) is detected **on the Dot** by a small
  BCResNet network, continuously, including over music, a reply, or a ringing
  alarm. The controller verifies each detection against what the Dot itself
  was playing, so the Dot saying the wake word does not wake it.
- **Speech handled on the controller** — voice activity detection and
  streaming speech recognition (sherpa-onnx: Silero VAD, Kroko ASR) decide
  when you have finished speaking; Home Assistant's own pipeline then
  transcribes, runs the intent and speaks the answer. The wake word is removed
  from the transcript, not cut out of the audio, so "Ophelia turn off the
  lights" said in one breath keeps its first word.
- **Wake models** — the dashboard keeps a registry of BCResNet models
  (ONNX graph + JSON sidecar). Upload one, it is validated and probed, and you
  choose when it becomes the fleet's model; each Dot fetches it by hash.
  Training models happens outside this repository.
- **Barge-in and "stop"** — say the wake word while the Dot is talking to cut
  it off. "Ophelia, stop" and "Ophelia, snooze" work while a timer or alarm
  rings; so does a tap on the action button. "Ophelia, stop" (or "pause")
  pauses what the Dot itself is playing, without Home Assistant.
- **Timers the Home Assistant way** — timers are HA's native satellite timers
  (set by voice or by an LLM agent); when one finishes, the Dot rings. The
  LED ring shows the time remaining.
- **Alarms that survive outages** — each Dot gets its own HA **Local
  Calendar**; every alarm is an event on it, visible and editable in HA's
  calendar. Set alarms by voice, from the dashboard, from the calendar, or
  through five scripts the controller exposes to Assist for LLM agents. The
  Dot caches upcoming alarms and rings them even if the controller or HA is
  down.
- **Multi-room done right** — one utterance in earshot of two Echos gets
  **one** response: the first Dot to claim the wake answers and the others
  stand down.
- **Music** — each Dot is an HA `media_player` you can actually play things
  on (media browser, Music Assistant, radio streams), with instant
  pause/stop. Speaking over music **ducks** it rather than pausing it; an
  alarm pauses it.
- **Bluetooth proxy** — each Echo can double as an HA Bluetooth advertisement
  proxy (great with [Bermuda](https://github.com/agittins/bermuda) for room
  presence).
- **Sensors and buttons in HA** — most Dots have an ambient light sensor that
  Amazon's software never exposed; it turns up as a lux sensor. Not every
  hardware revision has it fitted, and a device without one simply doesn't
  get the sensor. Holding the action button fires an event you can trigger
  automations from, while a normal press starts a voice turn — or fires its
  own event instead, if you'd rather bind the tap. Each Dot also gets an
  "Alert ringing" sensor, a "Stop alert" button, and a "Voice state" sensor
  (`idle`, `listening`, `thinking` or `speaking`) for automations.
- **Headphones** — plug into the 3.5mm jack and audio moves there, unplug and
  it comes back, no reboot needed.
- **Fleet dashboard** — provisioning wizard, per-device or fleet config
  pushed live (EQ, LED ring scenes, alert sounds), A/B-slot firmware updates
  with automatic fallback, root shell, logs, and per-turn activity (wake
  model and score, how the utterance ended, latencies). Optionally keep the
  last few turns' audio, or the wake word that triggered each turn, to play
  back.
- **Encrypted device link** — TLS with a controller-generated CA plus
  per-device tokens; the wizard installs credentials automatically.
