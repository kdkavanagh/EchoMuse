# Alert assets and the LED animation corpus

What a stock Dot Gen 2 (`biscuit`, build Nov 18 2022) ships on disk for
alerts and for its LED ring. Recovered from a rooted device with Alexa
disabled.

The alert **wire model** (`SetAlertDirective`, the event vocabulary and the
burst/gap fields) is in
[alexa-turn-control.md § Timers and alarms](alexa-turn-control.md). This file
covers what that model refers to: the sounds, the headless-device runtime that
schedules and rings them, and the LED animations.

Archive and re-derivation recipe:
`~/.local/share/echomuse-native/biscuit-2022-11-18/MANIFEST.md`.

## The alert sounds

`/system/local/share/earcon/` — `base/` plus 13 locale directories
(`de en en-AU en-CA en-GB en-IN es es-MX es-US fr fr-CA hi ja`).

**49 `system_alerts_*` files in `base/`, each an `.mp3` with a `_short.wav`
sibling**: `system_alerts_atonal_02`, `_alarming_03`, `_24_legacy`,
`_genuine_crush`, and celebrity alarms (`_alec_baldwin`, `_dan_marino`).
Alongside them `alerts_notification_01..03.mp3`, comms ringtones
(`comms_call_connected`, `comms_drop_in_incoming`, `comms_outbound_ringtone`)
and the state cues — `state_privacy_mode_on/off.wav`,
`state_boot_up_regular`, `state_boot_up_oobe`, `state_device_reset`,
`state_volume_adjust_tone`, `state_setup_*`, `state_sent_to_cloud.mp3`.

The `.mp3` / `_short.wav` pairing is directly consumed by
`AssetAudioPlayerManager`: `shortAlertAsset` is matched against an
`Asset.getAssetId()` separately from the full asset list. *[INFERENCE: the
short asset is the foreground/background transition cue — e.g. under a voice
turn — because the wire model has both the pairing and
`AlertEnteredForeground` / `AlertEnteredBackground` events. The runtime's
selection criterion is recovered; that particular policy association is not.]*

Note the encoding split is itself informative: the **short** cues are `.wav`
and the long ones `.mp3`, i.e. the latency-sensitive ones are kept decode-free
— the same reasoning behind EchoMuse compiling the wake chime into the
firmware as 48 kHz PCM rather than streaming it.

These are Amazon's assets. Readable on hardware you own; not redistributable,
and the celebrity alarms are separately licensed. Do not vendor any of them
into this repo.

## The HeadlessBeacon alert runtime

The actual runtime is the installed
`com.amazon.headlessbeacon` APK, not `SpeechInteractionManager`. The APK
contains 80 dex files; the core is archived and decompiled in
`~/.local/share/echomuse-native/biscuit-2022-11-18/dex/hb_{classes4,classes74}.dex`
(`hb_out4/`, `hb_out74/`), all md5-verified. It is why a physical dot-button
press stops an alarm rather than starting speech:

```java
// SIM's RegularUberButtonHandler
if (audioManager.getPackageInFocus().equals("com.amazon.headlessbeacon"))
    context.sendBroadcast(new Intent("amazon.alexa.alerts.ACTION_ALARM_BUTTON_STOP"));
```

### Schedule and persistence

`amazon.alexa.alerts.engine.AlarmsEngine` persists every `TimedEvent` in
`/data/data/com.amazon.headlessbeacon/databases/alarms.db`, then arms Android
with:

```java
AlarmManager.setAlarmClock(
    new AlarmManager.AlarmClockInfo(triggerTime, showIntent),
    operationPendingIntent);
```

It does **not** use `setExact` or `setExactAndAllowWhileIdle`.
`setAlarmClock` is Android's alarm-clock exemption from Doze/App-Standby, so a
ring is a stronger delivery contract than an ordinary idle-aware alarm. A
second *pre-intent* wakes the path **30 s early** (`WAKEUP_PRE_MILLIS = 30_000`).

`alarms.db` has eight tables:

```
timed_events             assets                  recurring_alarms
offline_support_timed_event  next_instance       retained_timed_events
notification_event_cache android_metadata
```

`timed_events` stores the directive fields plus `isRinging`, instance/local
state, recurrence, `shortAlertAsset`, loop parameters, music-alarm metadata
and `originalTime`; `assets` maps downloaded asset IDs to URLs. The database on
this never-signed-in Dot is a valid, zero-row 61,440 B schema — the runtime is
present even though it has never scheduled an Alexa alert.

`BootCompleteReceiver` starts `AlarmService.onBootComplete()`, which invokes
`restoreAndroidAlarmsFromDB()`: Android's `AlarmManager` entries do not survive
a reboot, so every future event is re-armed. A persisted ring near reboot is
handled inside a **30 min** `RING_AFTER_REBOOT_WINDOW`; stale duplicates are
identified from `createdDate` against the reconstructed boot wall clock, and
expired non-recurring events are removed.

Recurring alarms use `originalTime` plus `RecurrencePattern` in *local
wall-clock time*, then compute and arm the next occurrence. A 07:00 daily
alarm stays at 07:00 over a DST transition rather than moving by an hour.

### Ring, loop and ramp

`HeadlessRingingService` is the actual screenless-Dot ringer. It runs as a
foreground service, takes an `AlertsWakeLock`, serialises simultaneous alerts
through a queue, drives `ready-alarm`, `ready-timer`, `ready-alarm-short` and
`ready-timer-short` LED animations, and force-stops an undismissed ring after
**60 min** (`ALERT_RINGING_TIMEOUT` /
`CANCEL_RINGING_EVENT_DELAY_MINUTE`).

Two playback paths:

| Path | Loop policy |
|---|---|
| Cloud asset — `AssetAudioPlayerManager` / `CompositeMediaPlayer` | directive `loopPauseInMilliSeconds` and, in long mode, directive `loopCount` |
| Bundled fallback — `ShortAlertPlayerManager` | `LOOP_PAUSE_MILLIS = 10_000`; `Integer.MAX_VALUE` loop count |

Reminders and geo-location reminders targeted at a person are forced to one
loop regardless of the directive. A playback error falls back to a bundled
default earcon; `stopAudio()` ducks then waits **25 ms** before stopping to
avoid a click.

Freshly fired (not requeued/delayed) alerts may use `BezierVolumeRamp`: 0.0 to
1.0 over **20 s**, sampled every **200 ms**, behind
`com.amazon.alexa.ascendingAlarmVolumeEnabled` or debug
`persist.amazon.alerts.ramp`.

### Stop, snooze and focus

`LocalCommandReceiver` accepts SIM's
`com.amazon.speech.LocalCommand.{Stop,Cancel,Snooze}`, permission-gated to
`amazon.speech.permission.SEND_DATA_TO_ALEXA`. `AlertsStateMachine` then
enforces policy:

- **Stop** applies only to a ringing alarm or timer — never a reminder.
- **Snooze** applies only to an alarm — never a timer or reminder.
- Offline snooze changes local state and stops immediately; online snooze sends
  `AlarmSnoozed` to the cloud and awaits a replacement `SetAlert`.

At ring start/stop `SimManager` calls
`addAudioAlertFocus("Notifications")` / `releaseAudioAlertFocus("Notifications")`
into SIM's shared focus arbiter. Android focus has a second layer:
`DefaultFocusChangeListener` ducks on transient-can-duck, pauses/stops on a
loss unless the alert elects to retain focus, and re-acquires/resumes on gain.
An alarm can therefore fight to remain audible across another focus claim.

`ASPHelper` also reports alarm state to the AFE: code **53** for alarm
ringing/stopped, **54** for music alarm, **55/56** for screen/button press,
and generic playback state. The receiver is native `libasp` / Pryon logic —
the fact it is sent is proven; what it changes in wake sensitivity is not.

The asset cache's *exact* app-private directory is still unverified
(`FileStorageManager` uses Android `Context`, likely `files/` or `cache/`);
do not treat that likely path as evidence.

## The LED animation corpus

`/system/etc/led-resources/` — **284 files, 2.1 MB, all plain text.** This is
the find worth acting on, because EchoMuse already renders animations
device-side and this is a complete, readable animation language for the same
12-LED ring.

### Format

```
# ACTIVE - TIMER

loop
200:09F,09F,09F,09F,09F,09F,09F,09F,09F,09F,09F,09F
200:09F,000,000,09F,09F,09F,09F,000,000,09F,09F,09F
200:000,000,000,000,09F,09F,000,000,000,000,09F,09F
```

- `#` — comment, conventionally the animation's name.
- `<ms>:<c1>,<c2>,…,<c12>` — one frame: a duration in milliseconds followed by
  **twelve** colours, one per LED.
- Colour is **three hex digits, 4 bits per channel** (`09F` = R 0, G 9, B 15;
  `000` off; `AAA` mid-grey).
- `loop` — everything *after* it repeats; anything *before* it is a one-shot
  intro. `timer_led_countdown_sec-03` is exactly that: three 1000 ms frames
  fading a single pixel `AAA → 666 → 333`, then `loop` over an all-off frame.
- Blank lines group phases and are otherwise ignored.

### Inventory

| Group | Count | Notes |
|---|---|---|
| `timer_led_countdown_sec-01..60` | 60 | a full minute of countdown states |
| `volume_step*` | 30 | one per volume position |
| `OTA_step*` | 12 | update progress |
| alert states | 10 | `active_alarm`, `active_timer`, `ready-alarm`, `ready-timer`, `*-short`, `micsoff-ready-*` |
| `zzz_*` | 16 | demos — `fire`, `disco`, `jellyfish`, `rainbow`, `comets`, `fireflies`, `lava-flow`, `shooting-stars`, `paparazzi`, `turbo-boost`, `tortoise-hare`, `wave`, `overlappers`, `magenta-pulse`, `blue-fireflies` |
| states | rest | `wifi-config`, `wakeword-change`, `wait`, `volume-muted`, `volume_mute-ready/active`, `volume_off-to-full`, `volume_full-to-off`, `solid_{white,red,orange,green}`, `start-up_*`, `voicemail-feedback` |

Three of those are design decisions rather than data:

- **`ready-*` versus `active-*`.** An alert has a *pending* look and a
  *ringing* look, with a `-short` variant of each ready state.
- **`micsoff-ready-alarm` / `micsoff-ready-timer`** — a separate animation for
  the same state while the mic is muted, rather than letting a mute indicator
  override it. EchoMuse deliberately takes the opposite line (mute is
  device-sovereign and suppresses paints, recording them in `baseLEDs` for
  restore), which remains the right call for us — but it is worth knowing
  Amazon chose to author the *combination* instead.
- **A 60-step countdown is a state machine, not an animation.** Something has
  to select `timer_led_countdown_sec-NN` every second; the file format carries
  none of that policy.

### Against EchoMuse's `led_anim`

| | Amazon | EchoMuse |
|---|---|---|
| Unit | a frame list; 12 explicit colours per frame | a parametric spec `{pattern, colors, periodMs, ttlSec}` |
| Expressiveness | arbitrary per-LED sequences | `solid` / `spin` / `rotate` / `pulse` / `meter` / `off` |
| Wire cost | the whole file | one small message per state change |
| Liveness | fixed frames, no input | `meter` tracks live speaker RMS |
| Failure mode | none — it is a file on disk | `ttlSec` dead-man clears the ring if the controller dies |

Neither is better. Amazon's is a richer *authoring* format for fixed
sequences; EchoMuse's is a smaller *protocol* with a liveness story, which is
what a ring driven over a lossy network needs. The corpus is still worth
reading as a design reference — 284 worked examples of what this exact ring
looks good doing — and the format is trivial to parse if a frame-list pattern
is ever wanted beside the parametric ones.

Archived at
`~/.local/share/echomuse-native/biscuit-2022-11-18/led-resources.tgz`
(md5 `d7bbfe1841e99125bc985f66333aee35`). Amazon's assets — reference only.
