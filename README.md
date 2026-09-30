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
  rings; so does a tap on the action button.
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
  pushed live (EQ, LED ring scenes, alert sounds), A/B-slot OTA updates with
  automatic fallback, root shell, logs, and per-turn activity (wake model and
  score, how the utterance ended, latencies). Optionally keep the last few
  turns' audio, or the wake word that triggered each turn, to play back.
- **Encrypted device link** — TLS with a controller-generated CA plus
  per-device tokens; the wizard installs credentials automatically.
- **No phone-home** — there is no telemetry, no analytics and no install
  counter. Outside your network the running controller contacts only GitHub
  (an hourly check for a newer release, and firmware downloads when you
  update) and whatever media URLs Home Assistant asks it to play; every
  connection is listed in
  [docs/configuration.md](docs/configuration.md#what-leaves-your-network).

The LED ring, buttons and speaker are driven natively: device-local LED
animations, per-sample ducking, mute that's genuinely hardware (ADC off, red
ring, button LED). The 7-mic array goes through the Echo's own audio front
end, the one Amazon built for Alexa, which does per-microphone echo
cancellation, adaptive beamforming and beam selection tuned for this exact
array. Reaching it takes no Amazon service and no Alexa packages
([docs/native-afe-migration.md](docs/native-afe-migration.md)).

## How it works

```
Echo Dot (Go firmware)                Controller (Python)                    Home Assistant (stock)
native AFE capture, BCResNet   ⇄    session actor, speech worker,    ⇄    Assist pipeline, timers,
wake, playback, alarm cache   TLS   alert engine, dashboard     ESPHome API   Local Calendar, scripts
                           protocol v1                          + websocket/REST
```

The Dot does the continuous work: it captures through the native front end,
scores every hop for the wake word, keeps a few seconds of audio in memory,
mixes and plays everything you hear, and rings cached alarms. Nothing leaves
it until the controller grants an **uplink lease** — after a wake, a button
press, or when a reply is expected. The controller does the per-utterance
work: it decides whether a wake is real, when the utterance ends, and what
reaches Home Assistant. Privacy mute is enforced on the Dot and ends every
lease.

The full tour is in [docs/voice-pipeline.md](docs/voice-pipeline.md); the
architecture and its exact contracts are in
[docs/post-afe-audio-architecture.md](docs/post-afe-audio-architecture.md)
and [docs/protocol-v1.md](docs/protocol-v1.md).

This project builds on [EchoGo](https://github.com/Binozo/EchoGo) by Binozo —
the original SDK that made this hardware accessible.

---

## Before you start

**New here? Start with the [quickstart](docs/quickstart.md)** — it's the
guided path from zero to talking to your Dot, and it sends you to the
[rooting guide](docs/rooting.md) at the right moment rather than opening
with it.

The short version:
- Persistent unlock via [amonet-biscuit](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/) (R0rt1z2)
- FireOS 5 (Android 5.1, API 22)
- Magisk 17.3
- Alexa voice stack disabled (the dashboard's debloat step handles this)
- Home Assistant with a working Assist pipeline, and an **administrator**
  long-lived access token for the controller (not needed for the add-on)

---

## Running the controller

The controller ships as a prebuilt Docker image:

```bash
mkdir echomuse && cd echomuse
curl -O https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/docker-compose.deploy.yml
curl -o .env https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/.env.example
# Edit .env: SERVER_IP (this machine's LAN IP), HA_URL and HA_TOKEN
docker compose -f docker-compose.deploy.yml up -d
```

The controller will not start without `HA_URL` and `HA_TOKEN`. It uses the
token to run Assist pipelines, create each Dot's Local Calendar, install the
alarm scripts, and relax each satellite's VAD sensitivity — all through
stock Home Assistant APIs, which is why it must belong to an administrator.

### Or, as a Home Assistant add-on

If Home Assistant runs the Supervisor (HA OS, or Supervised), install this
repository as an add-on repository and add "EchoMuse" from the Add-on Store
— no separate Docker host and no token needed (the add-on uses the
Supervisor's).

[![Open your Home Assistant instance and show the add add-on repository dialog with this repository pre-filled.](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fwilbowes%2FEchoMuse)

Open the dashboard — `http://<SERVER_IP>:8768` for the Docker install, or the
add-on's **Open Web UI** button / sidebar panel for the add-on install. From
there the **provisioning wizard** takes a stock Dot the rest of the way over
USB: root, debloat, WiFi, firmware and TLS credentials. It ends by rebooting
the Dot, which then finds the controller itself and appears as pending for
you to approve. Home Assistant discovers each approved device via its
built-in ESPHome integration.

See the [quickstart](docs/quickstart.md) for the full walkthrough and
[configuration](docs/configuration.md) for every setting explained in plain
language.

Images are published to `ghcr.io/wilbowes/echomuse-controller` from
`controller-v*` tags; device firmware binaries are released from plain
`v*` tags (see Releases).

### Upgrading from firmware older than protocol v1

A Dot running older firmware still connects, but only to be updated: the
dashboard marks it **Upgrade required**, and until you update it from the
device page it takes no voice turns, rings no timers or alarms and plays
nothing. What else changed is in
[controller/CHANGELOG.md](controller/CHANGELOG.md).

---

## Building from source

The Echo Dot runs FireOS 5 (API 22). A custom Docker build environment is
required — standard Go cross-compilation won't produce a compatible binary.

```bash
git submodule update --init          # GoTinyAlsa (wilbowes fork; used by the capture tools)
cd device
docker build -t echomuse-compiler compiler/
./compile.sh                         # output: build/server
```

Controller from source: `cd controller && docker compose up --build` (the
build context is the repository root). Bare metal (Python 3.12) also needs
the speech bundle — see [CLAUDE.md](CLAUDE.md#running-the-controller).

Tests run on the host and in CI on every push: `go test ./...` under
`device/` (pure-Go logic) and `python -m pytest tests/` under `controller/`.

---

## Compatibility

Device firmware (`v*` tags) and the controller (`controller-v*` tags) are
released independently, so at any moment you may be running new firmware
against an older controller or the reverse. Two rules keep that safe:

**Features are negotiated by capability, not version.** On connect, a device
announces what it implements: the eight protocol capabilities
(`audio_timeline_v1`, `uplink_leases_v1`, `device_wake_v1`,
`render_reference_v1`, `render_progress_v1`, `focus_leases_v1`,
`alert_cache_v1`, `turn_protocol_v1`) and its hardware (`leds`, `led_anim`,
`buttons`, `button_hold`, and `ambient_light` only when the sensor is
readable). The controller asks "does this device say it can?" rather than
"is its version at least X" — the latter means encoding release history into
the controller, and it gets a dev build wrong immediately. A device missing
any of the eight is offered only the firmware update. A control that depends
on a hardware capability the device lacks is shown disabled with the reason,
never as a control that silently does nothing. A test asserts the capability
strings match across the Go and Python sources and the architecture document.

**Unknown data is ignored, and missing data is not zero.** Unknown JSON
fields and message types are ignored in both directions. Where a field
records a measurement, its absence is stored as *no data* rather than as
zero, which is why several activity columns are nullable.

## Acknowledgements

- [EchoGo](https://github.com/Binozo/EchoGo) — Binozo
- [GoTinyAlsa](https://github.com/Binozo/GoTinyAlsa) — Binozo
- [amonet-biscuit](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/) — R0rt1z2
- [EchoCLI](https://github.com/Dragon863/EchoCLI) — Dragon863
- [sherpa-onnx](https://github.com/k2-fsa/sherpa-onnx) — k2-fsa (Apache-2.0) — controller speech runtime
- [Kroko ASR](https://huggingface.co/Banafo/Kroko-ASR) — Banafo (CC-BY-SA) — streaming speech recognition model
- [Silero VAD](https://github.com/snakers4/silero-vad) — Silero (MIT) — voice activity detection
- [ONNX Runtime](https://github.com/microsoft/onnxruntime) — Microsoft (MIT) — model inference on the controller and the Dot
- [DTLN](https://github.com/breizhn/DTLN) — Nils L. Westhausen (MIT) — controller-side noise suppression models

---

## License

MIT
