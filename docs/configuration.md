# Configuration Guide

Every dashboard setting, what it does, and when you'd touch it — in plain
language. The last section lists [what leaves your network](#what-leaves-your-network).

How the pieces fit together (wake word on the Echo, end-of-request detection
on the controller, Home Assistant for speech-to-text and everything after it)
is in [voice-pipeline.md](voice-pipeline.md). What each light on the ring
means is in [led-ring-states.md](led-ring-states.md).

## Where settings live

- **Fleet config** (⚙ Settings → **Config**): the values every device uses
  unless it overrides them. Only administrators can change them. **Save & push
  to fleet** stores them and sends them to every connected device that follows
  the fleet.
- **Per-device config** (device page → **Config** tab): the same sections,
  each with its own **Scope: Fleet / Device** switch in its header. **Push
  config** stores and sends the device's changes.

Scoping is **per section**. A section on *Fleet* keeps following the fleet
value, including future fleet changes. A section on *Device* uses this
device's own values for every setting in that section; everything else still
tracks the fleet. So a Dot in a small room can have its own **Ring** scene and
its own **Timers & alarms** sounds while still following the fleet's wake word,
EQ and Bluetooth settings.

A section on *Fleet* is shown dimmed and read-only, so you can see what it
inherits. The banner above the sections reads `Following fleet config`, or
`Local override (2 of 7)` with the overridden sections named, and **Revert all
to fleet** puts every section back. Switching a section to *Device* starts it
from the current fleet values.

The Config tab opens with the device's **network (WiFi)** settings, which are
always per device, then the seven sections in this order: **Playback**, **Wake
word**, **Speech**, **Ring**, **Button**, **Bluetooth**, **Timers &
alarms**. A control that needs something the device's firmware does not
announce is shown disabled with the reason.

An Echo still running firmware from before protocol v1 shows an **Upgrade
required** panel instead of its Config, Activity and Alerts tabs. Until you
update it from the **Updates** tab it takes no voice turns and rings no timers
or alarms.

### Other device tabs

- **Status** — IP, firmware, WiFi network, ESPHome port, `Online` or `Offline
  · last seen …`, current volume, link mode (`wss (TLS)` or `plain ws`), and
  whether the config is `Fleet` or `Local override (n of 7)`. **Resources**
  shows CPU (with the number of cores awake, because the Dot parks idle cores
  and the percentage is a share of the awake ones), RAM, storage, WiFi signal,
  **Latency** (round trip to the device; amber from 200 ms, red from 1 s) and
  **Temp** (amber from 70 °C, red from 85 °C, with a note when the thermal
  governor is capping cores). **Wake health** is described under
  [Wake word](#02--wake-word). The Bluetooth proxy panel appears when the proxy
  is on.
- **Activity** — the voice-turn history: what was heard, the transcript, how
  the turn ended, and the audio of each turn when **Save utterances** or
  **Save wake clips** is on. The header names the wake model with its idle and
  playback thresholds, and a counter shows **Refused wakes**. A turn whose
  answer asked a follow-up question is marked `?` (colored by what became of
  it: answered, no reply within 7 s, cut off, no microphone, …) and its reply
  `↩`; each links to the other, and **Follow-ups answered** counts them. Each
  turn also shows how its spoken answer ended and the trace id of its
  controller log line (`turn <id> trace`). History is kept in the
  controller's database. Daily turn summaries, hourly wake counters and hourly
  hardware metrics (CPU, memory, WiFi signal) for up to 180 days are available
  from `/api/devices/{id}/activity?days=N`.
- **Alerts** — timers and alarms for this Echo; see
  [The Alerts tab](#the-alerts-tab).
- **Samples** (administrators) — recording tools; see
  [Recording tools](#recording-tools-samples-tab).
- **Console**, **Updates**, **Logs** — a remote shell, firmware updates and
  rollback, and the device log.

---

## 01 — Playback

How responses and music sound.

### Equalizer (8 faders + presets)

Shapes the tone of everything the controller plays through the Dot: voice
responses and music. It does not apply to timer and alarm sounds, which the
Dot plays itself.

Bands are 125, 250, 500, 1k, 2k, 3.5k, 5.5k and 8k Hz; each fader runs from
−12 to +12 dB. Default: all flat.

- **Flat** — no shaping.
- **Clarity** — +7 dB at 3.5k, +4 dB at 5.5k, +2 dB at 8k: lifts the range
  where speech is intelligible.
- **Warmth** — +3 dB at 250, +2 dB at 500, −2 dB at 2k.
- Drag any fader for a custom curve (shown as `· Custom`).

### Speech boost

Adds a +5 dB presence boost centred on 2.5 kHz on top of the EQ. Try it if
responses sound muffled from across the room. Off by default.

### Duck depth

How far music drops while the assistant is talking over it. Music keeps
playing through a voice turn, quieter, and comes back up when the answer
ends. The same depth is used for the brief dip while the controller checks a
wake word heard over music.

Default **−18 dB**; the slider runs from −40 to 0 dB. Less negative (−6, −10)
keeps the music more present; more negative all but silences it for the
answer. Set it by ear in the room it plays in. The response itself is never
turned down, only the music under it — if an answer is hard to hear over
music, this is the setting to change, not the volume.

### Volume

Volume is device **state**, not a setting, so the Playback section has no
volume control. Every change — buttons, Home Assistant, wherever — is
remembered by the controller and restored when the device reconnects, so a
power blip does not reset it. A device that has never reported a volume
starts at 85%. The current level is on the **Status** tab. Volume is never
inherited from the fleet, whatever the Playback section's scope says.

Mute is held by the device itself: a muted Dot stays muted through reboots
and power cuts, red ring and all, whether or not the controller is reachable.

---

## 02 — Wake word

The Echo listens for the wake word itself. It scores the microphone audio
every 160 ms with a BCResNet model and, when the score crosses the model's
threshold, tells the controller it may have heard the wake word. The
controller decides whether to start a turn. When the Dot was silent, the
threshold is enough. When the Dot was making sound — a response, music, an
alarm — the controller first checks the wake against what the Dot was playing
at that moment, so the Dot's own sound cannot wake it, and confirms the wake
word in the recognised speech. Meanwhile the Dot briefly dips its own sound.

So you can always interrupt the Dot by saying the wake word, whatever it is
doing. There is no switch for interrupting; it is always on.

The default wake word is **Ophelia**.

### Wake model

The section shows one tile per model in the controller's wake-model registry:
its wake phrase, the short hashes of its graph and sidecar files, its four
thresholds, and its validation probe result. The model in use is highlighted
and marked `active`. Click another tile to select it, then save.

The model is chosen **fleet-wide**. Every Echo runs the fleet's selected
model, even when its own Wake word section is set to *Device*. Each Echo picks
up the model and its thresholds when its connection to the controller starts,
so an Echo that is already connected keeps the previous model until it next
reconnects (a reboot does this).

A model that is selected anywhere cannot be deleted; the `×` appears only on
unselected, unused tiles. A red **Missing registry model** tile means a stored
setting names a model the registry does not have.

#### Adding a model

**+ BCResNet model** opens the upload form. You need:

| Field | What it is |
|---|---|
| **ONNX graph** | The BCResNet audio-in `.onnx` file (up to 20 MiB). |
| **JSON sidecar** | Its `.json` sidecar (up to 64 KiB) describing input, labels and normalisation. |
| **Wake phrase** | The word as it appears in transcripts, lowercase letters only (`ophelia`). The controller removes it from the transcript text before Home Assistant's conversation agent sees the request; the audio is never cut. |
| **Verification core** | The part of the word the controller must find in the recognised speech to confirm a wake heard while the Dot was making sound, lowercase letters only (`ophel`). |
| **idle, playback, reference, near miss** | The model's thresholds, each between 0 and 1. The form starts with the values of the built-in model. |

**Validate & add** checks that the graph and sidecar agree, runs the model on
silence, noise and a tone, and requires every probe score to stay below the
near-miss threshold. A model that passes is added but **not** selected; select
it yourself when you are ready.

Thresholds belong to the model because scores mean different things for
different models. They must satisfy near miss < reference ≤ playback ≤ idle:

| Threshold | Used for | Built-in model |
|---|---|---:|
| **idle** | Wake threshold while nothing is playing, including while the Dot speaks a response | 0.90 |
| **playback** | Wake threshold while music plays or an alarm or timer rings | 0.65 |
| **reference** | Controller-side threshold for spotting the wake word in what the Dot itself played | 0.30 |
| **near miss** | Scores at or above this that do not become a wake are counted as near misses | 0.17 |

Thresholds are fixed when a model is added. Uploading the same graph again
returns the existing entry unchanged. To change a model's thresholds, select a
different model, delete this one, and upload it again with the new values.

### Wake health (Status tab)

Each Echo reports its wake detector every 30 seconds. The **Wake health**
panel shows:

- **Availability** — `ready`, or why wake detection is off: `missing asset`
  (the Dot has not yet fetched the model or runtime from the controller),
  `load failed`, or `inference errors`.
- **Installed graph** — the short hash of the model the Dot is running; amber
  when it differs from the one its config selects.
- **Last report**, **Max inference** (ms per scoring window), **Overruns**
  (scoring windows dropped because the previous one had not finished),
  **Near misses** and **Near-miss peak**, **Candidates** (possible wakes sent
  to the controller) and **Inference errors**.

A missing value shows `—`, never a reassuring zero.

`GET /api/devices/{id}/speech_assets` returns the model and runtime files the
controller has named for the device, which of them the device reported
installed, and which are missing.

### Wake chime

Plays a short confirmation sound on the Dot as soon as a wake is accepted.
Off by default. It does not play when the wake word interrupts a ringing
timer or alarm or a spoken response — the sound stopping is the
acknowledgement.

### Save wake clips

Keeps the audio of each accepted wake: from 300 ms before the stretch the
model scored to the end of the last window that crossed the threshold. Off by
default.

This answers *what woke it?* When a Dot answers a room nobody was talking to,
the Activity row says a wake happened; the clip lets you hear the sound that
caused it — a line of TV dialogue, a name that rhymes, a jingle. Those clips
are the material for training a better model.

Each turn with a clip gets an amber ▷ (play) and ⤓ (download the WAV) in
**Activity**. **Download wake clips (.zip)** above the list gives the device's
whole set as one archive.

Nothing extra is recorded to make a clip: it is cut from audio the controller
already received for that wake. The newest **500 clips per device** are kept.
Turning the setting off stops new clips but keeps the saved ones until newer
clips push them out or you delete the device. To clear them sooner, delete the
files in the `wakes/` folder beside the controller's database.

### Arbitration window

With several Echos in earshot, one spoken wake word can reach more than one.
The first Echo whose wake the controller accepts answers immediately; any
other Echo accepted within this window stands down without a chime or a turn.

Default **700 ms**; the slider runs from 0 to 2000 ms. The winner does not wait
out the window, so a single Echo answers just as fast. `0` turns arbitration
off. It never applies to the action button, and it never blocks an Echo from
stopping its own ringing alarm.

---

## 03 — Speech

What the controller does with the speech it receives.

### The microphone chain is not adjustable

The Dot's own audio software — the one Amazon shipped for Alexa — turns its
microphones into one channel: it picks the direction of the voice, removes the
Dot's own sound, and sets the level. EchoMuse keeps it because it is tuned for
this exact microphone array. Its settings are on a read-only part of the
device, so there is no gain or pickup setting here. If a Dot hears you badly,
the levers are physical: move it away from walls and the TV and closer to
where people talk.

When your request has ended is decided by a fixed policy on the controller
(`post_afe_1`, described in [voice-pipeline.md](voice-pipeline.md)). It has no
dashboard tuning.

### Noise suppression

Runs a neural denoiser (DTLN) on the controller over the copy of your request
sent to Home Assistant's speech-to-text. Wake detection is untouched. It is a
second pass: the Dot has already cleaned the audio. Off by default. Try it in
a room where steady noise — fans, air conditioning, appliance hum — visibly
garbles transcripts, and compare. It does not remove other people talking or
the TV.

### Save utterances

Keeps the audio of recent requests so you can hear what speech-to-text heard.
Each turn with a recording gets a ▶ (play) and ⤓ (download the WAV) in
**Activity**. Off by default.

The recording is exactly what speech-to-text received, including the wake
word at the start and, when **Noise suppression** is on, after denoising. When
a transcript comes back wrong, this is the recording that can explain it —
room noise, distance, or a word the denoiser damaged. It is also the way to
compare **Noise suppression** or a new position on the same phrase.

**Think before switching it on.** This is the setting that stores recognisable
speech on the controller. It keeps the **last 10 turns per device** as WAV
files in the `recordings/` folder beside the database, oldest replaced first.
Turning it off stops new recordings but keeps the saved ones until newer ones
replace them or you delete the device. To clear them sooner, delete the files.
An older turn may show no buttons because its recording has already been
replaced.

### Extended utterances

Sets the longest request the controller accepts: **15 seconds** when off (the
default), **30 seconds** when on. Turn it on for dictation or long requests.
A request still going when it reaches the limit is dropped rather than sent
half-finished; the Activity row shows it ended as too long.

---

## 04 — Ring

The colours the LED ring uses during conversations. The Dot animates the ring
itself, so it stays smooth regardless of WiFi or controller load. The full
list of ring states is in [led-ring-states.md](led-ring-states.md).

- **Standard** — green (default).
- **Airy** — pale sky blue.
- **Malevolent** — deep crimson listening ring with an ember spinner.
- **Pride** — rainbow.
- **Custom** — pick your own **Listening** colour (the solid ring while it
  records; default `#00b400`) and **Thinking** colour (the spinner while it
  works; default `#00c800`). The colours apply only to this scene.

Two things never change in any scene: the **red mute ring** (red always means
the microphones are off) and the cyan volume arc.

### Meter response (Advanced)

While a response plays, the ring pulses with the live speaker level.
The **Advanced** panel shapes that pulse. Changes apply on the next response.

| Control | What it does | Default | Range |
|---|---|---:|---|
| **Decay** | How fast it falls; higher follows individual syllables | 0.30 | 0.02–1 |
| **Attack** | How fast it rises on a peak | 0.6 | 0.05–1 |
| **Gamma** | Contrast; higher makes the swing more visible | 2.2 | 1–3.5 |
| **Floor** | Brightness during silence; `0` goes dark between words | 0.06 | 0–0.6 |
| **Reference** | Speaker level mapped to full brightness; lower is more sensitive | 0.22 | 0.02–1 |
| **Curve** | Below 1 lifts quiet sounds into view | 0.7 | 0.3–2 |

These are taste settings. For a livelier ring raise **Decay** and **Gamma**;
lower them for a calmer one.

---

## 05 — Button

Action-button gestures. A tap normally starts a conversation without the wake
word, or stops a ringing timer or alarm; a hold fires the `long` event on the
device's **Action Button** entity in Home Assistant. While the microphones are
muted a tap starts nothing, but a hold still fires its event.

### Tap sends an event

With this on, a tap fires the `single` event on the Action Button entity
instead of starting a conversation, so you can bind it to anything. The wake
word is unaffected, a tap still stops a ringing alert, and a hold still fires
`long`. A tap fires while muted too, because it is an event, not speech. Off
by default. Disabled, with the reason, on a device that does not report hold
support.

**Bind destructive automations to the hold, not the tap.** The button has no
authentication, and a tap is far easier to make by accident than a hold.

### Multi-tap window

Available when **Tap sends an event** is on. Above zero, taps within the window
are grouped into `single`, `double` or `triple` (four or more count as
`triple`). The cost is that **every** tap waits for the window to close before
anything fires, because a tap can only be called single once no second tap
follows.

Default **0** (off: every tap fires `single` at once); the slider runs from 0
to 600 ms. The gap is measured when the taps reach the controller, so network
delay counts against it; a busy or distant device may need a longer window.

---

## 06 — Bluetooth

**Bluetooth proxy** — turns the Dot into a Home Assistant Bluetooth proxy. The
Dot passively listens for Bluetooth Low Energy advertisements (presence
beacons, BLE temperature and humidity sensors, phones and watches for
room-presence systems like Bermuda) and forwards them to Home Assistant. Off
by default.

In Home Assistant the proxy appears as a **separate ESPHome device** named
`<label> BT Proxy`, independent of the voice assistant — add, remove or
ignore it without touching the voice satellite.

Before enabling:

- Enabling **permanently switches the Dot's Bluetooth chip away from Android's
  Bluetooth stack**, surviving reboots. EchoMuse does not use Android
  Bluetooth, but Bluetooth speaker pairing stops being possible on that
  device.
- The proxy is **receive-only**. Devices that need an active connection to read
  data (some smart locks, older BLE devices) are not supported; advertisement
  sensors and presence tracking are.

The **Bluetooth proxy** panel on the Status tab shows scanner state,
advertisements seen, nearby devices in the last 5 minutes, the BT address,
whether Home Assistant is connected and receiving, advertisements forwarded,
the proxy's ESPHome port, and HCI errors and restarts.

---

## 07 — Timers & alarms

Home Assistant owns both:

- **Timers** are Home Assistant's own voice-satellite timers. You set, change
  and cancel them with ordinary requests; Home Assistant counts down, and
  when a timer finishes EchoMuse rings the Dot.
- **Alarms** are events on a **Local Calendar** EchoMuse creates for each
  speaker ("EchoMuse Kitchen"), so they survive restarts and appear in Home
  Assistant's calendar. The Dot keeps a copy of the next 7 days of alarms and
  rings them itself, even while the controller or Home Assistant is
  unreachable.

The dashboard's banner calls this section **Timers**.

### Sounds

Each sound picker shows the controller's sound catalog as tiles.

- **Default** — the catalog sound named `default` if you have uploaded one,
  otherwise the Dot's built-in tone.
- **+ Upload sound** — mp3, wav, flac, ogg or m4a, up to 10 MiB. The sound is
  named after the file (so uploading `default.mp3` sets the default sound),
  converted to 48 kHz mono, and cut to its **first 10 seconds** with a 50 ms
  fade-out. A tile reads `shortened to 10s` when that happened.
- **▶ preview** (device Config tab only) — plays the sound once on this Echo;
  press again to stop. Unavailable while the device is offline.
- **×** deletes a sound. A sound selected by the fleet or any device cannot be
  deleted.

A ring loops the whole sound. The Dot fetches sounds from the controller ahead
of time and never downloads one when a ring is due; if a selected sound cannot
be resolved, the picker says so and rings use the built-in tone.

| Setting | What it rings | Default |
|---|---|---|
| **Timer sound** (`timerSound`) | Finished Home Assistant timers | empty = **Default** |
| **Default alarm sound** (`alarmSound`) | New alarms set by voice, by an AI agent, or from the dashboard, and alarms created in Home Assistant's calendar | empty = **Default** |

An alarm keeps the sound it was created with; changing **Default alarm sound**
affects new alarms and alarms added in Home Assistant's calendar.

### Gap between repeats

Silence between loops of the timer sound. Default **2 s**; the slider runs from
0 to 10 s. Alarms use their own gap (2 s by default), stored with each alarm.

### Timer ring limit

How long a finished timer rings if nobody stops it. Default **15 min**; the
slider runs from 30 seconds to 30 minutes. Home Assistant forgets a timer as
soon as it finishes, so this limit is the only thing that ends an unattended
timer ring. Alarms have their own limit, 10 minutes by default, stored with
each alarm.

### Stopping, snoozing and interrupting

- **Tap the action button** — stops whatever is ringing, on the Dot itself,
  without the controller or Home Assistant.
- **Say the wake word, then "stop"** (or "cancel") — stops the ring. **"Snooze"**
  snoozes an alarm (9 minutes by default); timers cannot be snoozed. Saying the
  wake word alone quiets the ring while you speak; the ring comes back if the
  request is not a stop or snooze. Voice stop needs the controller; the button
  does not.
- The **`<label> Stop alert`** button entity in Home Assistant stops it too,
  and the `<label> Alert ringing` sensor shows when something rings.

An alarm that was due while everything was off rings on recovery only within
30 minutes of its time; older ones are recorded as missed. A Home Assistant
restart loses running timers, as it does for Home Assistant's own satellites.

---

## The Alerts tab

Per device, available once its firmware speaks protocol v1:

- **Now** — what is ringing (in the foreground, or in the background while a
  conversation is open) and the running timers with their time left. The
  timer list is a display copy of what Home Assistant reported; it never rings
  on its own.
- **Delivery health** — whether the calendar subscription is live, whether the
  Echo is online, how far the Echo has acknowledged alarm updates, whether its
  clock is trusted (after a reboot without a trustworthy clock it does not
  ring old cached alarms until it has caught up), its wakelock, and its alarm
  store. Warnings appear below.
- **Alarm schedules and occurrences** — upcoming alarms with their state:
  `armed on endpoint` (the Echo has it), `delivery pending` (saved in Home
  Assistant, not yet on the Echo), `stored in HA`, or `unconfirmed`.
  Administrators can **Cancel** an alarm (a repeating alarm is removed with all
  its future occurrences) or create one with **Time**, **Name**, weekday
  buttons for a repeating alarm, or **one date**. Home Assistant's calendar is
  the full alarm editor; **Open Home Assistant calendar editor →** links to it.
- **Home Assistant provisioning** — the result of the controller's checks
  against Home Assistant, one row per feature: `voice` (a preferred Assist
  pipeline exists), `calendar` (Local Calendar is available), `scripts` (the
  alarm scripts can be installed and their requests received), `vocabulary`
  (areas, floors and exposed entities can be read), `timers` (the timer
  integrations are loaded). A failure disables only that feature and shows
  why.
- **Journal operations** — alarm changes waiting to reach Home Assistant, and
  those applied in the last 24 hours.

The controller installs five scripts exposed to Assist so AI agents can manage
alarms: `echomuse_set_alarm`, `echomuse_list_alarms`, `echomuse_cancel_alarm`,
`echomuse_dismiss_alert` and `echomuse_snooze_alarm`.

---

## WiFi (device Config tab, top section)

Moves a device to a different WiFi network without ADB. The section shows the
current network and IP, scans for visible networks, and switches after a
confirmation step.

The device applies the change itself and keeps it only after it has joined
the network, got an address and **reconnected to this controller**. If any
step fails — wrong passphrase, DHCP trouble, or a network that cannot reach
the controller, such as an isolated guest VLAN — it restores the previous
network. A power cut mid-switch recovers too: an unconfirmed change is rolled
back on boot.

---

## Recording tools (Samples tab)

Administrators get two recording modes per device for collecting wake-model
training material. While either runs, the device answers nothing — no wake
word, no button turn, nothing reaches Home Assistant — and its ring throbs
magenta.

- **Sample collection** records continuously and cuts clips at the silences:
  say the wake word around the room and each one becomes a WAV. Up to 2000
  clips per device are kept in the `samples/` folder.
- **Ambient recording** keeps everything the microphone hears as one WAV per
  session, starting a new file every 30 minutes. The newest 6 files per device
  are kept in the `ambient/` folder.

Both are stored beside the database and can be played, downloaded and deleted
from the tab.

---

## Controller settings

### Environment (`.env`, Docker Compose, bare metal)

Set once on the server; a change needs a controller restart.

| Variable | Default | What it is |
|---|---|---|
| `SERVER_IP` | — | The controller's LAN IP, advertised over mDNS; devices connect here. |
| `HA_URL` | — | Home Assistant's base URL, for example `http://homeassistant.local:8123`. |
| `HA_TOKEN` | — | A long-lived access token for a Home Assistant **administrator** (Profile → Security). Administrator rights are needed to create the calendars and scripts and to receive the scripts' requests. |
| `DEVICE_APPROVAL` | `strict` | `strict`: an administrator approves each new device. `auto`: new devices are approved as `Unknown` plus the first 8 characters of their serial. See the note below. |
| `SERVER_HOST` | `0.0.0.0` | Address the listeners bind to. |
| `SERVER_PORT` | `8767` | Device connections (unencrypted). |
| `SERVER_TLS_PORT` | `8770` | Encrypted device connections; `0` disables. |
| `REQUIRE_DEVICE_TLS` | `0` | Set to `1` only after every device shows `wss (TLS)` on its Status tab; from then on unencrypted or tokenless device connections are refused. |
| `API_PORT` | `8768` | Dashboard and API. |
| `MDNS_NAME` | `echomuse` | Name advertised over mDNS. |
| `ESPHOME_PROJECT_VERSION` | controller version | Project version each emulated ESPHome satellite reports to Home Assistant. |
| `DB_PATH` | `echomuse.db` | The SQLite database. Everything the controller stores (wake models, sounds, recordings, TLS files) lives beside it. |

`.env.example` lists them with comments. A bare-metal install also needs the
files the Docker image downloads at build time: the speech bundle
(`SPEECH_BUNDLE_DIR`, default `/app/speech`, fetched with
`python tools/fetch_speech_bundle.py <dir>`), the Android ONNX Runtime the
controller serves to devices (`ORT_ANDROID_LIB`), and the DTLN models for
**Noise suppression** (`NS_MODEL_DIR`). See `controller/Dockerfile` for the
exact sources.

**Device approval note.** The approval policy in effect is the
`device_approval` system setting in the database, which every new database
starts as `strict`. `DEVICE_APPROVAL` (and the add-on's `device_approval`
option) is consulted only when that setting is missing, so setting it to
`auto` does not change the policy. To switch to `auto`, change the system
setting through the API below.

### Home Assistant add-on options

The add-on connects to Home Assistant through the Supervisor, so it needs no
URL or token. Its options are `server_ip`, `server_host`, `mdns_name`,
`esphome_project_version`, `require_device_tls` and `device_approval`, with the
same meanings as above. The database lives in the add-on's `/data`. Restart
the add-on after changing an option.

### System settings (API only)

A few controller-wide settings have no dashboard control. Read them with
`GET /api/system/config` and change them with `PATCH /api/system/config` (both
administrator-only):

| Key | Default | What it is |
|---|---|---|
| `device_approval` | `strict` | Approval policy for new devices, as above. |
| `session_expiry_days` | `30` | How long a dashboard sign-in lasts. |
| `update_check_interval` | `3600` | Seconds between release checks; see below. |
| `github_repo` | `wilbowes/EchoMuse` | Repository whose releases the update check reads. |

### Encrypted device link

The controller creates its own certificate authority on first start (in
`tls/` beside the database) and accepts encrypted device connections alongside
unencrypted ones. Each device gets the CA certificate and a private token,
installed by the provisioning wizard or pushed to an existing device with the
**Secure link** button on its Status tab. The device connects encrypted from
its next reconnect; the Status tab's **Link** row shows which mode it uses.
Once the whole fleet shows `wss (TLS)`, set `REQUIRE_DEVICE_TLS=1`.

---

## Defaults at a glance

| Section | Dashboard label | Key | Default |
|---|---|---|---|
| Playback | Equalizer faders | `eqBands` | all 0 dB |
| Playback | Speech boost | `eqLoudness` | off |
| Playback | Duck depth | `duckDb` | −18 dB |
| Wake word | model tiles | `wakeModel` | the built-in Ophelia model (`4eb74512…`) |
| Wake word | Wake chime | `wakeSound` | off |
| Wake word | Save wake clips | `saveWakeClips` | off |
| Wake word | Arbitration window | `wakeArbitrationMs` | 700 ms |
| Speech | Noise suppression | `nsAsr` | off |
| Speech | Save utterances | `saveUtterances` | off |
| Speech | Extended utterances | `extendedUtterances` | off (15 s) |
| Ring | scene tiles | `ledScene` | `standard` |
| Ring | Listening | `ledListenColor` | `#00b400` |
| Ring | Thinking | `ledThinkColor` | `#00c800` |
| Ring | Attack / Decay / Floor | `meterAttack`, `meterDecay`, `meterFloor` | 0.6 / 0.30 / 0.06 |
| Ring | Gamma / Reference / Curve | `meterGamma`, `meterRef`, `meterCurve` | 2.2 / 0.22 / 0.7 |
| Button | Tap sends an event | `buttonSingleTapEvent` | off |
| Button | Multi-tap window | `buttonMultiTapMs` | 0 ms |
| Bluetooth | Bluetooth proxy | `bleProxyEnabled` | off |
| Timers & alarms | Timer sound | `timerSound` | empty (Default) |
| Timers & alarms | Default alarm sound | `alarmSound` | empty (Default) |
| Timers & alarms | Gap between repeats | `timerRingGapSeconds` | 2 s |
| Timers & alarms | Timer ring limit | `timerRingSeconds` | 900 s (15 min) |
| — (device state) | Volume on the Status tab | `startupVolume` | 85 |

---

## What leaves your network

EchoMuse has **no telemetry**: no usage reporting, no analytics, no crash
reporting, no install counter. Nothing reports which features you use, how
many devices you have, or that you installed it. The project exists to take a
cloud voice assistant off your network, and a ping home would undo that.

A consequence: **nobody, including the maintainers, can tell how many people
use EchoMuse.** Adoption is guessed from GitHub stars and release downloads.

### Connections the controller makes

- **Release check — `api.github.com`.** About 30 seconds after start, then
  every `update_check_interval` seconds (default 3600, once an hour), the
  controller asks GitHub for the newest firmware release and the newest
  controller version, so the dashboard can offer updates with their notes.
  **Check now** on the Updates tab asks immediately. These are ordinary
  unauthenticated API requests with no EchoMuse identifier; like any request
  they reveal your public IP to GitHub.
- **Firmware download — `github.com`.** Only when you update a device, deploy
  to the fleet, or use the provisioning wizard's install-latest step. The
  controller downloads the release binary and passes it to the device; the
  Echo itself connects only to the controller.
- **Audio Home Assistant asks it to play.** Spoken responses and music reach
  the controller as URLs from Home Assistant; the controller fetches each URL
  to play it. Responses come from your Home Assistant; music comes from
  wherever its stream lives.
- **Home Assistant** at `HA_URL` (or through the Supervisor in the add-on).

To make release checks rarer, set `update_check_interval` to a larger number
of seconds (`86400` is once a day). **Do not set it to `0`**: that does not
disable checking, it makes the controller check GitHub continuously.

### Downloads at install time, not at run time

The speech-recognition models, the voice-activity model, the denoiser models,
the Android ONNX Runtime and the dashboard's JavaScript and fonts are
downloaded when the Docker image is **built** — from GitHub, PyPI, Maven
Central, npm, cdnjs, jsDelivr and Google Fonts — and every model is checked
against a pinned hash. The published image is built by the project's release
workflow, so installing it contacts only the image registry (`ghcr.io`). A
bare-metal install downloads the same files when you run the fetch tool. The
running controller downloads none of them.

### From your browser

The dashboard is served entirely by the controller, except for one step: the
**provisioning wizard** loads its USB (ADB) library from `esm.sh` in your
browser the first time you connect a Dot over USB.

### What never leaves

- **Voice audio and transcripts.** Wake detection runs on the Echo, which
  sends the controller audio only around a possible wake word and for the
  request that follows, with a copy of what it was playing at those moments
  (plus the recording tools, when you start them). The controller sends the
  request audio to your Home Assistant for its speech-to-text step, then the
  transcript to its conversation agent — all over your LAN. What happens next
  is up to your Assist pipeline: if it uses a cloud speech-to-text engine or
  conversation agent, Home Assistant sends the audio or text there. EchoMuse
  itself sends it nowhere else.
- **Saved recordings** — utterances (`saveUtterances`), wake clips
  (`saveWakeClips`), samples and ambient recordings stay on disk beside the
  database. Nothing uploads them; downloading an archive is your decision.
- **Device serials, WiFi credentials, network names and your fleet's
  configuration** stay in the controller's database.
- **Support bundles** are built only when you ask for one (⚙ Settings →
  **Support**), and sharing the file is your decision. They exclude
  transcripts, recordings, device names, WiFi networks, addresses, tokens and
  passwords — see [support-bundle.md](support-bundle.md).
