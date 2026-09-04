# Configuration Guide

Every setting, what it actually does, and when you'd touch it — in plain
language.

## Where settings live

- **Fleet config** (gear icon → Fleet Config): the defaults every device
  uses.
- **Per-device config** (device page → Config tab): each section carries its
  own **Fleet / Device** switch in its header.

Scoping is **per section**, not all-or-nothing. Leave a section on *Fleet*
and it keeps following the fleet-wide value, including future changes. Flip
it to *Device* and only that section becomes this device's own — everything
else carries on tracking the fleet.

So a Dot in a small room can have its own **Ring** scene and its own
**Microphones** gain while still picking up every fleet change to the wake
word, EQ and Bluetooth settings. Before this, one override forked *all* the
settings and froze them against fleet changes permanently.

A section showing *Fleet* is displayed read-only rather than hidden, so you
can always see what it is inheriting. The banner at the top of the tab
summarises — `Fleet`, or `Local override (2 of 6)` with the sections named —
and **Revert all to fleet** puts everything back.

Flipping a section back to *Fleet* **discards** the values it was holding.
There is no hidden shadow copy waiting to reappear if you flip it to *Device*
again months later; it starts from the fleet value.

Changes apply **immediately** — no restarts, no rebuilds. The Config tab
opens with the device's **network (WiFi)** settings at the top — always
per-device, never inherited from the fleet — followed by the
fleet-inheritable sections, in order of how often you'll realistically touch
them: **Playback**, **Wake word**, **Microphones**, **Ring**, **Advanced**,
**Bluetooth**.

The **CPU** meter shows the core count beside the percentage — "27% · 2/4
cores". The Dot has four CPU cores and parks the ones it isn't using, and the
percentage is a share of the cores that are *awake*, so the same amount of
work reads as a bigger number when fewer are. Without the core count beside
it the figure can appear to halve when nothing actually changed.

Two other device tabs worth knowing: **Status** (IP, firmware, WiFi network,
ESPHome port, current volume, whether the config is fleet or overridden,
resource meters including **Latency** (the round trip to the device — amber
past 200ms, red past 1s; the only link-health signal the Echo's WiFi driver
actually provides, since it reports no retry or noise figures) and **Temp**
(the Dot's CPU sensor, and the hottest of its eleven sensors when that's
meaningfully warmer; it idles around 33°C, so anything amber is genuinely
unusual — and if the chip's thermal governor ever starts capping CPU
capacity, this is where it says so), and the
Bluetooth-proxy diagnostics panel when enabled —
the Status row reads `Online`, or `Offline` with how long ago the device was
last heard from) and **Activity** (voice-turn history — what was heard, how it was
transcribed, wake-word scores, playback underruns, near-misses, and — if
**Save utterances** is on — the recorded audio of each turn, playable and
downloadable). Activity
history is stored in the controller's database, so it survives controller
and device restarts; hourly hardware trends (CPU, memory, WiFi signal) are
kept for 180 days and available via the API
(`/api/devices/{id}/activity?days=N`).

---

## 01 — Playback

How responses sound.

### Equalizer (8 faders + presets)
Shapes the tone of the voice responses, like the EQ on a stereo. The Dot's
little speaker is boomy and dull by default.

- **Flat** — no shaping.
- **Clarity** — boosts the upper-mid frequencies where speech intelligibility
  lives. Good default for voice.
- **Warmth** — gentle low-mid lift, softer top. Nicer for music-ish content.
- Drag any fader for a custom curve.

### Speech boost
An extra presence bump for spoken responses. Try it if responses sound
muffled from across the room.

### Duck depth
How far music drops while the assistant is talking over it. Music **keeps
playing** through a voice turn — it isn't paused — so the answer arrives
mixed over a quiet bed, and the bed comes back up when the answer ends.

Default **−18dB**. Less negative (−6, −10) leaves the music more present;
more negative (−25, −30) all but silences it for the length of the answer.
It's worth setting by ear in the room it plays in: the right depth depends
on what you listen to and how loud, and there is no value that is correct
everywhere. The response itself is never turned down, only the music under
it — so if an answer is hard to hear over music, this is the setting, not
the volume.

Requires firmware **v2.10.0 or newer**, which mixes the two audio streams on
the device itself. That is not an arbitrary requirement: the controller runs
about four seconds ahead of what you actually hear, so when you say the wake
word those four seconds of music are already sitting on the Echo, past
anything the controller could still change. Older firmware shows the slider
disabled and falls back to pausing the music for the turn and resuming
after.

### Volume
Volume **tracks what you actually use** and survives reboots: every change —
buttons, Home Assistant slider, wherever — is remembered by the controller
and restored when the device reconnects. Set it low in the evening and a
midnight power blip brings it back low.

There is no volume slider in this section. There used to be, and it was
misleading: the device only re-applies the stored level on the first config
push after it boots, so moving the slider did nothing until the device
restarted — and any real volume change overwrote it in the meantime. Volume
is remembered device state rather than a setting you dial in, so the current
level is now shown read-only on the **Status** tab. Change it from Home
Assistant or the device buttons.

It is also never inherited from the fleet, whatever the section's Fleet /
Device switch says — otherwise a device would come back at another room's
volume.

Mute is remembered too, but by the device itself: a muted Dot stays muted
through reboots, power cuts, and firmware updates — red ring and all —
whether or not the controller is reachable.

---

## 02 — Wake word

How the device decides you said the magic word. By default this work happens
on the controller, not the Dot — the Dot just streams audio to it. The Dot
can also do this work itself, either alongside the controller as a
comparison or instead of it; see **Wake word detection** below.

### Wake word model
Which word wakes it: Hey Jarvis, Alexa, Hey Mycroft, or Hey Rhasspy. These
are pre-trained recognisers — you're picking a word, not training anything.
Pick one that doesn't collide with words you say a lot (and if your
household still talks to real Alexas, don't pick Alexa).

Want your own word? Train a model with `oww_forge/` (see its README), then
use the **+ Custom model** tile to upload the `.onnx` — it's stored in the
controller's data volume, appears as a tile next to the stock words, and
takes effect immediately on selection. The `×` on an unselected custom tile
deletes it.

### Arbitration window
With more than one Echo, saying the wake word in earshot of two of them
used to start two competing conversations. Now the **first device to hear
you answers immediately**, and any other device detecting the same word
within this window (default 700ms) quietly stands down.

There is **no latency cost**: the winner claims the turn on the spot rather
than waiting out the window, so a solo wake is exactly as fast as it was
before. The window only decides how long afterwards a second device counts
as "the same utterance". `0` disables it, and it never applies when only
one device is online.

An earlier version instead waited out the window and gave the turn to
whichever device heard you *best*. That was dropped: it taxed every wake by
~364ms even when nothing was competing, and field data showed the
signal-to-noise winner produced a *worse* transcript than the device that
simply heard you first.

### Sensitivity (Precise ↔ Eager)
The confidence bar the recogniser must clear.

- Toward **Precise**: fewer false wakes (it triggering off the TV), but it
  may ignore you sometimes.
- Toward **Eager**: catches you more reliably, but expect the occasional
  ghost activation.

**How to tune it**: the Status tab counts **near-misses** — moments where the
score came close but didn't trigger. If you're being ignored and see
near-misses climbing, move one step toward Eager. If it wakes up when nobody
spoke, move toward Precise — but a ghost activation is also worth *hearing*
rather than guessing at, so turn on **Save wake clips** below and the next one
leaves you the audio that caused it, which is both the answer and the material
for retraining the word so it stops firing on that sound at all.

### Near-miss floor
What counts as a near-miss at all. A frame scores near-zero constantly —
ordinary room tone, not almost-a-wake — so the near-miss counter above
ignores everything at or below a floor (default **0.05**) and only counts
scores between the floor and the wake threshold.

Raise it in a noisy room (TV, open kitchen) where background sound sits well
above silence but nowhere near triggering: left at 0.05 there, the counter
climbs on ambient noise and stops being a useful signal for tuning
Sensitivity. Lower it only to see scores further from the bar — it does not
change what triggers a wake, only what gets counted and logged as "close".

### Barge-in
Lets the wake word **interrupt the assistant mid-turn** — say "Hey
Rhasspy, stop" while it's reading you a paragraph (or still thinking
about your last question) and it cuts off and listens. On by default. It works
by leaving the microphones live while the device speaks; what stops it hearing
itself is the Dot's own echo cancellation, which is always running.

The **barge threshold** is the wake confidence required during playback — and
counter-intuitively it should be much *lower* than the normal wake threshold:
the speaker is far louder at the microphones than you are, so your voice
scores lower over playback than in a quiet room, while the device's own
(cancelled) voice barely scores at all. **0.05 is the default** — you
shouldn't need to raise your voice much. Raise it if responses ever cut
themselves off, which is what a chime or a phrase in the response scoring as
the wake word looks like. (During the silent *thinking* pause the normal wake
sensitivity applies instead — nothing is playing, so the low barge threshold
isn't needed there.)

**Wake word over a ringing timer** — a separate switch, off by default. A
ringing timer plays in bursts with gaps between them, and by default the Dot
only listens for you in the gaps. Turn this on to have it listen *through* the
chime as well, at the barge threshold above, so "stop" lands sooner. The reason
it is off: if the chime itself ever scores as your wake word, the alarm
silences itself. The device log says which window a score came from
(`Ring listening (chime audible)` vs `(silent window)`), so a self-silencing
ring states its own cause.

### Speex denoise
Runs a noise cleaner on the audio *only for wake-word scoring* (your actual
commands are untouched). Worth trying in rooms with constant background
noise (TV, air-con) if wake detection is unreliable there. Off by default —
it's a "try it and compare" option.

### Save wake clips
Keeps the **1.4 seconds of audio that crossed the threshold** — the sound
that woke the device, not the request that followed it. Off by default. Every
wake that starts a turn gets one — including a **Barge-in** wake, which is the
one most worth having, since barge-in listens at a deliberately lower bar than
the wake word does and so is the likeliest thing here to fire on nothing. The
turn's row in **Activity** grows an amber ▷ (play it here) and ⤓ (download the
WAV) beside the pair a saved utterance already puts there. The legend above
the list also offers **Download wake clips (.zip)** — the device's whole set in
one archive, which is the form training wants.

This exists for one question: *what woke it?* When a Dot answers a room
nobody was talking to, the Activity row tells you a wake fired and how
confidently, and that is all it can tell you. **Save utterances cannot help
here, and never could** — a turn's recording deliberately starts *after* the
wake word, because the tail of "hey jarvis" would otherwise be transcribed as
part of your request. So the one sound worth hearing was the one sound
nothing kept, and a false trigger you could describe but not produce is a
false trigger you cannot fix.

With the clip you can. Play it and you usually hear the culprit outright — a
line of TV dialogue, a name that rhymes, a jingle. Then it becomes training
data: drop the clips into `oww_forge` as negatives and retrain, and the model
stops firing on that sound specifically, which is a better fix than moving
**Sensitivity** toward Precise and losing real wakes along with the ghost.
`oww_forge/README.md` has the steps.

**What it does and doesn't store.** The window sits entirely *before* the
trigger, so a clip contains the wake word and the seconds of room before it,
and never a command — what came next is a saved utterance's job, and a
training clip carrying somebody's actual request would be a cost with no
benefit. While the setting is off **nothing is buffered at all** — there is no
rolling 1.4 seconds of your room sitting anywhere waiting for the switch to be
flipped. Kept per device: the newest **500 clips**, about 46kB each, so a
device cannot use more than ~23MB however badly its threshold is tuned.

Turning it back off stops new clips but **leaves the ones already saved**, so
switching off doesn't destroy the corpus you were part-way through collecting.
They stay until newer clips push them past the 500, or you delete the device,
which removes its clips too. To clear them out sooner, delete the files from
the controller's `data/wakes/` folder.

### Wake word detection
Who decides you said the wake word. Three settings:

- **Controller** (default) — the Dot streams audio and the controller
  listens. What EchoMuse has always done.
- **Both (compare)** — the Echo *also* runs the same model over the same
  audio and reports what it would have detected, without acting on it. It
  never triggers a turn. This is the one to use first: it tells you whether
  on-device detection is trustworthy on your hardware, in your room, before
  anything depends on it.
- **On device** — the Echo decides, and the controller starts the turn on
  its word.

**Why you might want "On device".** The wake decision stops crossing your
network. On a marginal link that is the difference between a Dot that
responds instantly and one that lags unpredictably, and it keeps working
through a controller restart. It does *not* reduce network traffic — the
audio still streams, because the controller runs the rest of the turn.

The controller keeps listening alongside it, which is deliberate: it costs
nothing extra (it was already scoring), it keeps the comparison in
**Activity** running so you can see whether the two agree, and it leaves
barge-in — interrupting a response by speaking over it — working exactly as
before.

Each voice turn's row in **Activity** shows both scores side by side, and
the per-device activity API returns an agreement summary (how often they
agreed, how far apart in milliseconds, and crossings the device saw that
never became a turn).

**Multi-device caveat.** If you have several Echos in earshot of each other,
put only one on **On device** for now. The rule that stops two Dots
answering at once still judges claims by when they arrive rather than when
each Echo actually heard you, so a device whose message was delayed can lose
to one that heard you less well. With a single device set this way, or with
Echos that cannot hear each other, this does not apply.

Three things to know before leaving Controller:

- **It needs files installed on the Dot** that aren't part of the firmware —
  ONNX Runtime plus the wake-word models, about 15MB, placed in
  `/data/local/share/echomuse/oww`. They're deliberately not shipped in the
  firmware image, because that would double both the download and the space
  each of the two firmware slots takes. Until they're there, the setting does
  nothing and the device log says which file is missing.
- **It costs about half a CPU core, permanently**, because the wake stream is
  always on. Measured on an Echo Dot Gen 2 that has capacity for it — the mic
  pipeline was unaffected across hours of use, including during music
  playback — but enable it on **one device at a time** and watch the
  **Resources** panel on the Status tab.
- **It needs recent firmware**, and the two settings need different
  vintages: scoring shipped before triggering did. Each option is disabled
  and says so on an Echo whose firmware cannot do it, rather than appearing
  to work.

---

## 03 — Microphones

What the controller does with the sound the Dot sends it.

### The microphone chain is not adjustable, and that is on purpose

The Dot has 7 microphones, and the work of turning them into one usable
channel — picking the direction your voice is coming from, subtracting the
Dot's own speech so it doesn't hear itself, levelling the volume, setting the
gain — is done by **the Echo's own audio software**, the one Amazon shipped it
with for Alexa. EchoMuse hands it the job rather than doing it again worse: it
is tuned for this exact microphone array by the people who designed it.

That software's settings live on a read-only part of the device, so there is
nothing here to turn up or down, and there are no pickup presets any more. If
a Dot hears you badly, the levers that exist are physical: move it away from
the wall, away from the TV, and closer to where people talk.

### Advanced (inside the Microphones section)

**Noise suppression** — cleans the audio sent to speech-to-text (and only
that — wake-word listening is untouched). It uses a small neural denoiser
(DTLN) running on the controller, so there's no load on the Dot. Note this is
a *second* pass: the Dot has already denoised what it sent. Off by default,
and worth leaving off unless steady noise — fans, air-con, appliance hum — is
visibly garbling transcripts in one room. Turn it on per device and compare.
It does not remove other people talking or the TV.

**Save utterances** — keeps the audio of recent voice turns so you can
*listen* to what was sent for transcription. The **Activity** tab then shows
a ▶ (play here) and a ⤓ (download the WAV) on every turn that has a
recording.

What's saved is the audio **exactly as speech-to-text received it** — so if
**Noise suppression** is on, you're hearing the cleaned-up version, not the
raw microphone. That's deliberate: when a transcript comes back wrong, the
only recording that can explain it is the one the recogniser actually heard.

This is the honest way to answer "is my microphone any good?". Without it
you're guessing from a garbled transcript, which can't tell you whether the
room was noisy, the Dot is too far away, or the denoiser chewed a word. Thirty
seconds of listening usually settles it — and it's the only sensible way to
A/B **Noise suppression** or a change of position, since you can compare the
same phrase before and after.

**Off by default, and worth thinking about before switching on.** This is the
setting that stores recognisable speech on the controller. What's kept: the
**last 10 turns per device**, as plain WAV files in the controller's data
folder, each overwritten as newer ones arrive. Only the audio sent for
recognition is saved by this setting — the always-on wake-word listening is
discarded continuously, and the sole exception is **Save wake clips** in the
Wake word section, which keeps 1.4 seconds of it per wake and is off by
default too.

Turning the setting back off stops new recordings immediately, but **leaves
the ones already saved where they are** — deliberately, so that switching off
doesn't destroy samples you were part-way through comparing. They stay until
newer recordings push them out (which needs the setting back on) or you
delete the device, which removes its recordings too. To clear them out sooner,
delete the files from the controller's `data/recordings/` folder.

A turn recorded a while ago may show no buttons — that just means its
recording has aged past the last 10 and the turn history has outlived it.

---

## 04 — Ring

The colours the LED ring uses during conversations. Scenes apply
instantly and can differ per device. On current firmware (v2.9+) the
device animates the ring itself — the controller sends one "play this
animation" instruction per state change, so the spinner stays perfectly
smooth regardless of WiFi or controller load, and while a response is
speaking the ring **throbs in time with the audio** (brightness follows
the actual level coming out of the speaker). If the controller ever
vanishes mid-conversation the ring times itself out rather than spinning
forever. Older firmware falls back to controller-rendered frames.

- **Standard** — the classic green.
- **Airy** — a pale, calm sky blue.
- **Malevolent** — deep crimson listening ring with an ember spinner.
- **Pride** — a rotating rainbow.
- **Custom** — pick your own **Listening** (solid ring while recording) and
  **Thinking** (spinner while processing) colours.

Two things never change, in every scene: the **red mute ring** (red always
means the microphones are off — it's a privacy indicator, not decoration)
and the cyan volume arc. The directional "which mic is listening" highlight
also adapts automatically: it brightens the scene's ring colour rather than
painting green.

The volume arc holds the ring for about two seconds so a turn animation
can't wipe it the instant it appears — but **pressing the action button
cancels it immediately**, so adjusting the volume and then talking to the
device still shows you the listening ring straight away.

### How a turn ends
The ring tells you *why* a conversation stopped, using rhythm rather than
colour (red, orange and cyan already mean mute, no-controller and volume):

- **One slow throb** — the device was listening and heard nothing.
- **A few quick blinks** — something went wrong (Home Assistant errored, or
  no speech came back).
- **Ring simply goes out** — normal end, or you cancelled it yourself.

The ring also now clears when the audio *actually* finishes, rather than
when the controller estimates it should have. On a slow WiFi link the old
estimate could clear the ring several seconds before the Dot had stopped
talking.

### Meter response (Advanced)
While a response plays the ring throbs with the live speaker level. The
**Advanced** panel here shapes how hard it throbs — the device renders it
locally, so changes apply on the next response with no restart:

- **Decay** — how fast it falls. Higher tracks individual syllables; lower
  reads as a slow swell.
- **Attack** — how fast it rises on a peak.
- **Gamma** — contrast. Higher makes the swing more visible.
- **Floor** — brightness during silence. `0` goes fully dark between words.
- **Reference** — the speaker level mapped to full brightness. Lower is more
  sensitive.
- **Curve** — below `1` lifts quiet consonants into view.

These are taste settings, which is exactly why they're adjustable here
rather than baked into firmware. The defaults are tuned for speech; if the
ring looks too static, raise **Decay** and **Gamma** first.

## 05 — Advanced

Everything in this section affects **only button-press conversations**
(tapping the action button to talk without a wake word — a *hold* is a
separate gesture that fires an event in Home Assistant instead). Wake-word
conversations ignore all of it — they're managed by Home Assistant's own
speech detection.

While the mic is muted a tap does nothing, but a hold still fires its event:
the mute silences speech, not the button.

### Make the tap an event instead

**Tap fires an event** (`buttonSingleTapEvent`) — turns a tap into a Home
Assistant event rather than the start of a conversation, so you can bind it to
anything you like. With it on, the device no longer starts a voice turn from
the button at all; the wake word is untouched, and a hold still fires `long`.
A tap fires while muted too, for the same reason a hold does — it's an event,
not speech.

Needs firmware v2.10.0 or newer. On older firmware the toggle is disabled and
says so.

**Bind destructive automations to the hold, not the tap.** The button has no
authentication, and a speaker sitting on a counter is a great deal easier to
tap by accident than to hold for three quarters of a second.

**Multi-tap window** (`buttonMultiTapMs`) — set it above zero and taps are
grouped into `single`, `double` or `triple`. The cost is that *every* tap is
delayed by this window, because a tap can only be called single once no
second one follows.

**Use 350ms.** Below about 300ms it fights both human timing — double taps
land roughly 150–400ms apart — and network jitter, because the gap is
currently measured when the taps reach the controller rather than on the
device. On a busy or distant device you may need more. Zero disables
grouping, and a tap fires `single` immediately.

### Speech gate

Decides when a button-press utterance starts and stops:

- **Threshold** — how loud counts as "speech", measured against the audio the
  Dot sends. Raise it only if a noisy room keeps a button turn open; lower it
  if a quiet talker gets cut off. Worth knowing: the level the Dot delivers
  changed when the microphone chain moved to the Echo's own audio software, so
  a value carried over from an older install may want re-checking.
- **Speech gate (ms)** — how much continuous speech opens the gate. Higher =
  ignores brief noises, but clips fast talkers.
- **Silence gate (ms)** — how much silence ends your turn. Higher = you can
  pause mid-sentence without being cut off; lower = snappier responses. 900ms
  default; raise to ~1200 if you get cut off mid-thought.

Note (v2.9.4): these two timings now behave exactly as configured. Older
firmware quietly applied them ~5× longer than the number said (a counting
bug against the mic's real batch size), so button-press turns used to hang
on for a few seconds of silence before ending — if turns feel snappier
after updating, that's why, and if a slow talker now gets clipped, raise
the silence gate.

---

## 06 — Bluetooth

**Bluetooth proxy** — turns the Dot into a Home Assistant Bluetooth proxy.
The device passively listens for Bluetooth Low Energy advertisements
(presence beacons, BLE temperature/humidity sensors, phones and watches for
room-presence systems like Bermuda) and forwards them to Home Assistant.

In Home Assistant the proxy appears as a **separate ESPHome device** (named
`<label> BT Proxy`), independent of the voice assistant — you can add,
remove, or ignore it without touching the voice satellite. Once added, its
scanner feeds HA's Bluetooth integration exactly like an ESP32 Bluetooth
proxy would, and a diagnostic sensor counts received advertisements.

Two things to know before enabling:

- Enabling **permanently switches the Dot's Bluetooth chip away from
  Android's stack** (it survives reboots). Nothing EchoMuse uses needs
  Android Bluetooth — but stock-style Bluetooth speaker pairing stops being
  possible on that device.
- The proxy is **receive-only** (passive scanning). Devices that need an
  active connection to read data (some smart locks, older BLE devices)
  aren't supported — advert-based sensors and presence tracking are.

Diagnostics live on the device's **Status tab** (Bluetooth proxy panel):
scanner state, advertisements seen, nearby device count, and whether Home
Assistant is connected and receiving.

---

## WiFi (device page → Config tab, top section)

Move a device to a different WiFi network without touching ADB. The section
at the top of the Config tab shows the current network, signal, and IP, lets
you scan for visible networks, and switches with a confirmation step.

The switch is designed to be **unbrickable**: the device applies the change
itself and must pass three checks — join the network, get an IP, and
**reconnect to this controller** — before the change is kept. Fail any of
them (wrong passphrase, DHCP trouble, or a network that works but can't
reach the controller, like an isolated guest VLAN) and it automatically
restores the previous network and tells you why. Even a power cut
mid-switch recovers: an unconfirmed change is rolled back on boot. Allow
about two minutes for the device to drop off and come back.

---

## Controller settings (the `.env` file)

These are set once, on the server, and need a controller restart to change:

| Setting | What it is |
|---|---|
| `SERVER_IP` | The controller computer's LAN IP — what devices connect to. |
| `OWW_MODEL` / `OWW_THRESHOLD` | Startup defaults for wake word/sensitivity — the dashboard values override these. |
| `DEVICE_APPROVAL` | `strict` (you approve every new device — recommended) or `auto`. |
| `SERVER_TLS_PORT` | Encrypted device link (wss) port — default 8770, `0` disables. Devices switch to it automatically once they hold pushed credentials (wizard install, or the **Secure link** button on the device Status tab). |
| `REQUIRE_DEVICE_TLS` | Set to `1` **only after every device shows "wss (TLS)"** on its Status tab — from then on the controller rejects unencrypted or tokenless device connections. |

See `.env.example` for the complete list with comments.

### Encrypted device link

The controller generates its own certificate authority on first start
(stored in `tls/` next to the database) and listens for encrypted device
connections alongside the plain ones. Each device gets two credentials —
the CA certificate and a private token — installed automatically by the
provisioning wizard, or pushed to an existing device with the **Secure
link** button on its Status tab. A device with credentials connects
encrypted from its next reconnect; the Status tab's **Link** row shows
which mode each device is using. Once the whole fleet shows `wss (TLS)`,
set `REQUIRE_DEVICE_TLS=1` to lock out unencrypted connections entirely.

## What leaves your network

EchoMuse has **no telemetry**. There is no usage reporting, no analytics, no
crash reporting and no install counter. Nothing reports which features you
use, how many devices you have, or that you installed it at all. This is a
deliberate decision rather than an omission: the project exists to take a
cloud voice assistant off your network, and quietly adding a ping home would
undo the reason to run it.

A consequence worth stating plainly: **nobody, including the maintainers, can
tell how many people use EchoMuse.** Adoption is guessed at from GitHub stars
and release download counts, which is the trade being made.

### The one outbound connection

The controller contacts `api.github.com` once an hour to ask what the newest
release is, so the dashboard can tell you an update is available and show its
notes. When you choose to update a device, the firmware binary is downloaded
from `github.com` at that moment.

That is the whole of it. The request carries no identifiers — it is an
ordinary unauthenticated API call — but like any request it does reveal your
public IP to GitHub, the same exposure as a `git clone` or opening the repo
in a browser.

Set `update_check_interval` (seconds, default `3600`) in the system config to
change how often it runs. A long interval, say `86400`, reduces it to once a
day. Note that `0` does **not** disable checking — it currently makes the
poll loop spin without pausing, which is worse than leaving it alone.

### What never leaves

- **Voice audio and transcripts.** Mic audio goes from the device to your
  controller and on to your Home Assistant, over your LAN. What happens next
  is whatever your Assist pipeline does — if you have configured HA to use a
  cloud speech-to-text service, HA sends it there. EchoMuse itself sends it
  nowhere but HA.
- **Saved utterance recordings** (`saveUtterances`, off by default) — written
  to disk beside the database and never uploaded.
- **Saved wake clips** (`saveWakeClips`, off by default) — the same: written
  to disk beside the database, and uploaded nowhere unless you download the
  archive yourself to retrain a model with it.
- **Device serials, WiFi credentials, network names and your fleet's
  configuration.** These live only in the controller's database.
- **Support bundles** are built only when you ask for one, and sharing the
  file is your decision. They deliberately exclude speech, transcripts,
  network names and account names — see
  [support-bundle.md](support-bundle.md).
