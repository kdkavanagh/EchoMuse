# EchoMuse Quickstart

EchoMuse turns an Amazon Echo Dot (2nd generation) into a **fully local voice
assistant** — no Amazon account, no cloud, no audio leaving your house. The
Dot becomes a "satellite": it listens for the wake word itself, and a small
server on your network (the **controller**) handles each request and talks to
Home Assistant for the actual "turn on the lights" part.

This guide gets you from zero to talking to your Dot. No programming
knowledge needed — where something genuinely technical is unavoidable (the
one-time rooting of the Dot), we point you at the detailed guide instead of
pretending it's easy.

---

## What you need

| Thing | Why |
|---|---|
| Amazon Echo Dot 2nd gen ("biscuit") | The hardware being repurposed. Second-hand ones are cheap. |
| A computer that's always on (a home server, NAS, Raspberry-Pi-class box or better) | Runs the controller. Docker recommended. |
| Home Assistant | Does the assistant work: speech-to-text, understanding, text-to-speech, timers, and the calendar that holds your alarms. You need a working [Assist pipeline](https://www.home-assistant.io/voice_control/) with a speech-to-text and a text-to-speech engine. |
| A Home Assistant **administrator** account | The controller connects to Home Assistant with a long-lived access token from an admin user (not needed for the add-on install). |
| A USB cable + a laptop, once | For the one-time unlock/flash of the Dot. |

## Step 1 — Root the Dot (one time, per device)

The Dot ships locked to Amazon's software. Unlocking it involves flashing
modified firmware over USB — it's the only genuinely fiddly part of the
project, it takes an hour or so the first time, and it's documented in
[rooting](rooting.md), which points at R0rt1z2's XDA Forums thread for the
exploit itself.

The dashboard has a **provisioning wizard** (plug the Dot into your laptop's
USB port, open the dashboard in Chrome, follow the steps) that automates the
rest after the initial unlock.

If a step fails, the wizard offers a **Download diagnostics** file to attach
to an issue. It captures the device's state at the moment it failed, which
saves a round trip of being asked to run things by hand. If you unplug the
device at any point you can carry on, because **Reconnect** is on every step.
The cable is the Dot's only power, so unplugging reboots it and it comes back
in Android. The wizard will say so if the step you are on needed recovery
mode.

You only do this once per device. Everything afterwards — updates,
configuration, even a remote terminal — happens over WiFi from the dashboard.

## Step 2 — Make a Home Assistant token

Skip this step if you are installing the Home Assistant add-on (see below).

In Home Assistant, logged in as an **administrator**: click your user name
(bottom left) → **Security** → **Long-lived access tokens** → **Create
token**. Copy it; Home Assistant shows it once.

The controller uses it for stock Home Assistant features only: running your
Assist pipeline, creating one Local Calendar per Dot for alarms, installing
five alarm scripts that Assist can use, and setting each Dot's voice-detection
sensitivity. Those need an admin; with a non-admin token the dashboard reports
the features that failed.

## Step 3 — Start the controller

On your always-on computer, using the prebuilt image (nothing to compile):

```bash
mkdir echomuse && cd echomuse
curl -O https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/docker-compose.deploy.yml
curl -o .env https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/.env.example
# Edit .env:
#   SERVER_IP = this computer's LAN IP address
#   HA_URL    = your Home Assistant address, e.g. http://homeassistant.local:8123
#   HA_TOKEN  = the token from step 2
docker compose -f docker-compose.deploy.yml up -d
```

The controller refuses to start without `HA_URL` and `HA_TOKEN`; the reason
is in `docker logs echomuse-controller`.

To upgrade later: `docker compose -f docker-compose.deploy.yml pull && docker compose -f docker-compose.deploy.yml up -d`. Your devices, users, settings and wake models live in `./data` and survive upgrades.

<details>
<summary>Alternative: the Home Assistant add-on</summary>

If Home Assistant runs the Supervisor (HA OS, or Supervised), add this
repository in the Add-on Store, install **EchoMuse**, set **Controller LAN IP
address** to the Home Assistant host's LAN IP, and start it. No token is
needed: the add-on talks to Home Assistant through the Supervisor. Open the
dashboard with **Open Web UI**; the setup token in step 4 is in the add-on's
**Log** tab.

</details>

<details>
<summary>Alternative: build from source</summary>

```bash
git clone https://github.com/wilbowes/EchoMuse.git
cd EchoMuse/controller
cp .env.example .env
# Edit .env as above
docker compose up -d --build
```

`docker-compose.yml` builds with the repository root as its context and
requests an NVIDIA GPU (the `onnxruntime-gpu` build). On a machine without
one, remove the `deploy:` block and the `GPU: "1"` build arg — or use the
prebuilt image above, which is CPU-only. The build downloads the pinned,
hash-checked speech models.

</details>

The controller now runs:

- a **dashboard** at `http://<SERVER_IP>:8768` — your control panel
- a listener that the Dots find automatically on your network (no IP
  configuration needed on the device side)

## Step 4 — Create your admin account

Open `http://<SERVER_IP>:8768` in a browser. On a fresh install you'll see
the Echo graphic with a **pulsing amber ring** and a setup form.

It asks for a **setup token** — a one-time code printed in the controller's
logs, so that only you (the person who can read the server's logs) can claim
the controller:

```bash
docker logs echomuse-controller
```

Look for the boxed token near the top, paste it in, pick a username and
password, and you're in. From then on the page shows a **green ring** and a
normal login.

## Step 5 — Approve your device

When a rooted Dot powers up, it finds the controller by itself and asks to
join. New devices appear in the dashboard as **pending**, and the Dot's ring
pulses grey-white — nothing works until you give it a name and click
**Approve & Add to Fleet**. (This is deliberate: nothing joins your voice
network without you saying so.)

Once approved, the Dot connects fully: you'll see it as **online**, with its
volume, settings, and a live status.

A Dot running firmware from before protocol v1 shows **Upgrade required**
instead. Open it and click **Update firmware**; until then it takes no voice
turns, rings no timers or alarms and plays nothing.

## Step 6 — Connect it to Home Assistant

The controller makes each Dot look like an **ESPHome voice satellite** —
something Home Assistant already knows how to talk to, with no custom
integration:

1. If Home Assistant runs on the **same subnet** as the controller, the Dot
   appears under **Settings → Devices & Services** as a discovered
   "echomuse-…" ESPHome device; click **Add**. Otherwise add it by hand:
   **Add Integration → ESPHome**, the **controller's IP** and the device's
   **port** — 16001 for the first device, 16002 for the second, and so on
   (each device's port is shown on its dashboard page). One integration entry
   per device. Discovery uses local-only multicast, so it cannot cross a
   subnet or VLAN boundary.
2. Choose the device's Assist pipeline (Settings → Voice assistants, or the
   pipeline selector on the device page).

The device appears in HA as **`<name> Voice Assistant`** (e.g. "Lounge
Voice Assistant"), with Model "Echo Dot Gen 2 (biscuit)". Alongside the
voice satellite it has a media player, an action-button event, an "Alert
ringing" sensor, a "Stop alert" button and, where the hardware has one, an
ambient light sensor. The Bluetooth proxy, if enabled, shows up separately as
`<name> BT Proxy`.

Once Home Assistant is connected, the controller creates a Local Calendar
called **"EchoMuse &lt;name&gt;"** for each Dot and installs the alarm
scripts. The device's **Alerts** tab shows each of these under **Home
Assistant provisioning**, with the reason if one failed.

## Step 7 — Talk to it

Say the wake word — **"Ophelia"** with the bundled model — then speak
normally, with or without a pause:

> "Ophelia, turn off the kitchen lights."

Timers and alarms work by voice too:

> "Ophelia, set a timer for ten minutes."
> "Ophelia, set an alarm for seven a.m. on weekdays."

When one rings, say "Ophelia, stop" (or "Ophelia, snooze" for an alarm), or
tap the action button. Alarms also show up in Home Assistant's calendar,
where you can add, edit and delete them.

The LED ring tells you what's happening (full list in
[led-ring-states.md](led-ring-states.md)):

| Ring | Meaning |
|---|---|
| Dark | Idle, listening for the wake word |
| Solid green | Listening to your request, or waiting for your reply |
| Spinning | Thinking (Home Assistant is processing) |
| Throbbing with the sound | Speaking the answer |
| Partly lit | A timer is running; the lit part is the time remaining |
| Cyan pulse | An alarm or timer is ringing |
| Cyan arc | Volume level, shown for 2 seconds after a volume press |
| Solid red | Microphones muted (the physical mute button). Mute also cuts off whatever the assistant was doing |
| Orange pulse | Not connected to the controller |

## Everyday things

- **Updates**: when a new EchoMuse release is out, the dashboard shows an
  update badge — one click updates the device over WiFi. The release notes
  appear alongside it, so you can read what changed before deciding. If an
  update ever goes wrong, the device automatically rolls back to its
  previous version. **Deploy all** updates the whole fleet at once; it runs
  in the background, so you can close the dialog and reopen it from the
  header pill to check progress.
- **Settings**: everything tunable lives in the dashboard, either fleet-wide
  (the gear icon) or per device. See [configuration.md](configuration.md).
- **Terminal**: each device page has a full remote terminal (for the
  curious; you never *need* it).
- **Volume**: buttons on the Dot, the dashboard slider, or Home Assistant's
  media player card — they all stay in sync.
- **Interrupting**: say the wake word while it's talking and it stops and
  listens. The mute button also cuts it off instantly (and mutes).
- **Bluetooth proxy** (optional): each Dot can double as a Home Assistant
  Bluetooth proxy — passively picking up BLE advertisements (presence
  beacons, BLE sensors) and feeding them to HA as a *separate* ESPHome
  device, independent of the voice assistant. Enable it per device in the
  Config tab (Bluetooth section); it appears in HA as "<name> BT Proxy". See
  [configuration.md](configuration.md).

## When something doesn't work

1. Is the device **online** in the dashboard, and not marked **Upgrade
   required**?
2. Is Home Assistant connected? The device's **Alerts** tab lists each Home
   Assistant feature the controller depends on as ready or failed, with the
   reason.
3. Does the wake word register? The device's Status tab shows wake health
   (the active model, whether the Dot loaded it, near misses), and the
   Activity tab lists each turn with its date; click one to see its wake
   score, what the controller heard, what Home Assistant transcribed and
   answered, and how it ended. Frequent
   near misses or false wakes are a question of the model's thresholds; see
   [configuration.md](configuration.md#wake-model).
4. Bad transcriptions? See stages 1 and 8 of
   [voice-pipeline.md](voice-pipeline.md) — room noise and speaker distance
   are the usual suspects. To stop guessing, turn on **Save utterances**
   (Config → Speech) and *listen* to what Home Assistant was sent — the
   Activity tab gains a play button on each turn. It's off by default
   because it stores speech on your server; see
   [configuration.md](configuration.md#save-utterances) for exactly what's
   kept.
5. Still stuck, and want to ask? The dashboard's **Support** tab downloads a
   single diagnostic file to attach to a GitHub issue — versions, device
   state, recent logs and the statistics that make audio problems
   diagnosable at a distance. It is built as an allowlist: no transcripts, no
   recordings, no network names, no account names, and device labels are
   replaced with pseudonyms. [support-bundle.md](support-bundle.md) lists
   exactly what's in one so you can check before you share it.
