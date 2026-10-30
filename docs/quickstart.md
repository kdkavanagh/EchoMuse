# EchoMuse Quickstart

EchoMuse turns an Amazon Echo Dot (2nd generation), or an Echo Show 8 (1st
generation), into a **fully local voice
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
| Amazon Echo Dot 2nd gen ("biscuit") | The hardware being repurposed. Second-hand ones are cheap. An Echo Show 8 1st gen ("crown") works too: see [Using an Echo Show 8](#using-an-echo-show-8). |
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
rest after the initial unlock. EchoMuse runs on Fire OS 6 only (amonet-biscuit
v2 plus boot-root.zip), and that route needs no recovery mode at all. A Dot
still on Fire OS 5 is refused at the wizard's first step; [rooting](rooting.md)
explains how to move it to Fire OS 6.

If a step fails, the wizard offers a **Download diagnostics** file to attach
to an issue. It captures the device's state at the moment it failed, which
saves a round trip of being asked to run things by hand. If you unplug the
device at any point you can carry on, because **Reconnect** is on every step.
The cable is the Dot's only power, so unplugging reboots it and it comes back
in Android.

You only do this once per device. Everything afterwards — updates,
configuration, even a remote terminal — happens over WiFi from the dashboard.

### Using an Echo Show 8

The Echo Show 8 1st gen (2019, "crown") runs the same EchoMuse firmware as a
voice satellite, but keeps its screen: Android stays, showing a kiosk app of
your choice (Fully Kiosk Browser with a Home Assistant dashboard, say), and
EchoMuse hides Amazon's Alexa apps rather than replacing the system. Its
unlock is R0rt1z2's crown thread, and [rooting](rooting.md#echo-show-8-1st-gen-crown)
has everything EchoMuse adds to it. Before you run the wizard on a Show:

- **Unlock and root it** per the thread (the adb shell must be root).
- **Join Wi-Fi on the Show itself**, from its setup screen or Settings.
  EchoMuse does not configure the Show's Wi-Fi or Bluetooth; they stay the
  device's own, and the wizard refuses a Show that is not on a network.
- **Install your kiosk** with adb. The wizard refuses to hide Amazon's
  launchers until a home-screen app other than Amazon's exists, since the
  Show would otherwise boot to a blank screen. Fully Kiosk Browser's home
  screen ships switched off; the wizard switches it on for you.
- **In the kiosk, leave the microphone alone**: Fully Kiosk's acoustic
  (sound) detection stays off, and do not use an Assist microphone in the
  dashboard page. The Show allows one recording at a time, and EchoMuse
  holds it. Fully's camera motion detection, screen timeout and screensaver
  are fine.
- **Put the Show's own Settings pages in the kiosk.** Its Wi-Fi and
  Bluetooth are managed from its Settings from now on. Add these two to
  Fully Kiosk's app launcher (or your kiosk's equivalent); Android's generic
  Wi-Fi settings link does not open on this build:
  `com.android.settings/.Settings$WifiSettingsActivity` and
  `com.android.settings/.Settings$BluetoothSettingsActivity`.

The wizard then checks the kiosk, installs the boot hook and the firmware,
and **neutralises Alexa**: it marks Amazon's setup complete, repairs
Amazon's Settings app (on a Show never registered with Amazon its pages
crash until it is given its configuration), makes your kiosk the home
screen, hides Amazon's Alexa apps with `pm hide`, and denies the microphone
to the two Amazon apps that must stay installed (Alexa's wake engine and
Amazon's dictation keyboard). All of it is reversible;
[rooting](rooting.md#to-undo-it-on-the-show) has the undo. A second, optional tier
(Amazon's cloud, smart-home radio and telemetry apps) is a switch on that
step, off by default. There is no Wi-Fi step and no second slot to mirror.

Optionally, a Show can light its screen when it hears the wake word, through
the kiosk's local API (Fully Kiosk's Remote Admin): off by default, set per
device in the dashboard.

#### On LineageOS 18.1

The Show can also run LineageOS 18.1 instead of Fire OS: Android without any
Amazon apps, so there is nothing to neutralise and the screen is yours, but
for EchoMuse's light bar. EchoMuse needs **our build** (it carries the Amazon audio front end
whose metadata EchoMuse reads; upstream releases do not, and the wizard
refuses them), flashed from TWRP after the same amonet-crown unlock;
[rooting](rooting.md#echo-show-8-on-lineageos-181) has the build, the flash
and the accepted risks (an unofficial ROM with SELinux permissive). Before
you run the wizard:

- **Join Wi-Fi on the Show itself** during LineageOS's setup.
- **Switch on adb**: tap Build number in Settings → About seven times, then
  switch on **USB debugging** and **Rooted debugging** in Developer options,
  and allow your computer's key when the Show asks. The wizard restarts adb
  as root itself, each time it connects.
- **Install your kiosk** and make it the home app. While EchoMuse records,
  the kiosk gets no microphone.

The wizard then installs the boot hook (plus a small addon.d script that
keeps it across LineageOS updates) and the firmware, reboots, waits for
the Show to reach the controller, and checks that the EchoMuse app is up.
There is no kiosk check, no Alexa step and no Wi-Fi step.

The firmware installs and starts the **EchoMuse app** itself, and keeps it
current with every firmware update. It has no launcher icon; you see it as:

- **The light bar**: a 12-segment bar along the bottom edge of the screen,
  showing what a Dot's ring would (the table in Step 7), drawn over your
  kiosk and its screensaver. It is dark while there is nothing to show.
- **Touch**: during a turn and while an alert rings, the bottom band of the
  screen is the action button. A tap cancels the turn, or stops the ringing
  alert (the band then reads "Stop"); a hold is the button's long press in
  Home Assistant. While idle nothing there is touchable, and every touch
  reaches the kiosk.
- **A notification**: a silent, persistent one with the link state and the
  firmware and app versions, which Android requires while the app runs.

With **Light the screen on wake** on, the app turns the screen on at the
wake word itself, so the kiosk API URL and password are not used (the
dashboard disables them). It does not dismiss the kiosk's screensaver: the
bar shows over it. The app's health (linked, version, overlay allowed, last
error) is on the device's Status tab.

**With Kiosk Satellite as your kiosk**, the firmware also installs an
EchoMuse plugin into it that shows each turn full screen: what you asked, in
grey, and the answer in large text as it arrives, staying up 6 s after the
reply (Config → Screen, **Turn view stays up**; 0 hides it at once). The
prerequisite is in Kiosk Satellite: turn on **Remote Administration** with a
password, leaving **Use HTTPS** off (Settings → Device → Remote
Administration). The wizard's last step then asks for that password, and so
does the dashboard before a firmware update while it holds no token, or after
Kiosk Satellite rejected it; Config → Screen has a login at any time. EchoMuse
logs in once and keeps only the token Kiosk Satellite issues, never the
password; changing the password later does not revoke that token. Every
prompt can be skipped: the light bar works without the plugin. The plugin's
state is in the Status tab's Companion app panel.

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

On your always-on computer, build the supported Rust controller from source:

```bash
git clone https://github.com/wilbowes/EchoMuse.git
cd EchoMuse/controller
cp .env.example .env
# Edit .env:
#   SERVER_IP = this computer's LAN IP address
#   HA_URL    = your Home Assistant address, e.g. http://homeassistant.local:8123
#   HA_TOKEN  = the token from step 2
docker compose up -d --build
```

The controller refuses to start without `HA_URL` and `HA_TOKEN`; the reason
is in `docker logs echomuse-controller`.

To rebuild after updating the source, run `docker compose up -d --build` again. Your devices, users, settings and wake models live in `./data` and survive rebuilds.

<details>
<summary>Alternative: the Home Assistant add-on</summary>

The add-on pulls a published image. Existing Python releases are not replaced by
the source cutover; use the source build above until a Rust release is published.

If Home Assistant runs the Supervisor (HA OS, or Supervised), add this
repository in the Add-on Store, install **EchoMuse**, set **Controller LAN IP
address** to the Home Assistant host's LAN IP, and start it. No token is
needed: the add-on talks to Home Assistant through the Supervisor. Open the
dashboard with **Open Web UI**; the setup token in step 4 is in the add-on's
**Log** tab.

</details>

<details>
<summary>Alternative: the published Rust image</summary>

```bash
mkdir echomuse && cd echomuse
curl -O https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/docker-compose.deploy.yml
curl -o .env https://raw.githubusercontent.com/wilbowes/EchoMuse/main/controller/.env.example
# Edit .env as above, then use a release that contains the Rust controller.
docker compose -f docker-compose.deploy.yml up -d
```

Future image releases build `controller-rs/Dockerfile` and include the speech models,
dashboard and device firmware. Existing published tags still contain the implementation
they were released with; this source change does not update them. Until the first Rust
image is published, use the source build above.

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

A Dot that never appears as pending was refused: the controller admits only
Dots running Fire OS 6, and Echo Show 8s running Fire OS 6 or LineageOS 18.1,
with protocol v1 firmware. Run the
wizard on it (after
moving it to Fire OS 6, if needed); a device already registered with the
controller is re-adopted with **Continue**, keeping its name and settings.

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
| Slow red pulse | Not connected to the controller. Saying the wake word gets "I can't connect to the smart home right now" |

## Everyday things

- **Updates**: the device firmware ships inside the controller image. After
  you update the controller, any Dot running different firmware shows an
  update badge — one click installs the bundled firmware over WiFi. If an
  update ever goes wrong, the device automatically rolls back to its
  previous version. **Deploy all** updates the whole fleet at once; it runs
  in the background, so you can close the dialog and reopen it from the
  header pill to check progress. A Dot that is offline can't update then:
  click **Install when it reconnects** (in that dialog or on the Dot's
  Updates tab) and it installs the bundled firmware the next time it
  connects. You can cancel that until it does.
- **Settings**: everything tunable lives in the dashboard, either fleet-wide
  (the gear icon) or per device. See [configuration.md](configuration.md).
- **Terminal**: each device page has a full remote terminal (for the
  curious; you never *need* it).
- **Volume**: buttons on the Dot, the dashboard slider, or Home Assistant's
  media player card — they all stay in sync. On an Echo Show the volume keys
  are Android's, and stay in sync the same way.
- **Interrupting**: say the wake word while it's talking and it stops and
  listens. The mute button also cuts it off instantly (and mutes). On an
  Echo Show, mute is not supported by EchoMuse yet, and the dashboard says so.
- **Bluetooth proxy** (optional): each Dot can double as a Home Assistant
  Bluetooth proxy — passively picking up BLE advertisements (presence
  beacons, BLE sensors) and feeding them to HA as a *separate* ESPHome
  device, independent of the voice assistant. Enable it per device in the
  Config tab (Bluetooth section); it appears in HA as "<name> BT Proxy". See
  [configuration.md](configuration.md). An Echo Show's Bluetooth and Wi-Fi
  are the device's own (its Settings), so those controls show as managed by
  the device.

## When something doesn't work

1. Is the device **online** in the dashboard?
2. Is Home Assistant connected? The device's **Alerts** tab lists each Home
   Assistant feature the controller depends on as ready or failed, with the
   reason.
3. Does the wake word register? The device's Status tab shows wake health
   (the active model, whether the Dot loaded it, near misses), and the
   Activity tab lists each turn with its date, and each near miss (the model
   heard something close to the wake word but not close enough); click a turn
   to see its wake score, what the controller heard, what Home Assistant
   transcribed and answered, and how it ended. With **Save wake clips** on,
   each near miss has a recording, so you can hear whether it was you. Missed
   or false wakes are a question of the model's thresholds; see
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
