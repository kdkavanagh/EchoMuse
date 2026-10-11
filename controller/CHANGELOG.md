# Changelog

## Unreleased

### Post-AFE cutover (device protocol v1)

This release replaces the voice, wake and alert stack. The architecture is
[docs/post-afe-audio-architecture.md](../docs/post-afe-audio-architecture.md);
the device wire protocol is [docs/protocol-v1.md](../docs/protocol-v1.md).

**Before you upgrade**

- **Fire OS 5 is no longer supported, and every Dot needs new firmware.**
  Upgrade the controller first. Every Dot must be on Fire OS 6
  ([docs/rooting.md](../docs/rooting.md)) and provisioned with the wizard. A
  reflashed Dot is re-adopted with **Continue**, which keeps its name,
  settings, Home Assistant device and alarms. A Fire OS 5 Dot, or one on
  firmware predating this release, is refused at connect.
- **A Home Assistant administrator token is required.** Docker and bare-metal
  installs must set `HA_URL` and `HA_TOKEN` (a long-lived access token from an
  admin user) in `.env`; the controller refuses to start without them. The
  add-on needs nothing: it now declares `homeassistant_api: true` and uses the
  Supervisor token.
- **Settings are migrated automatically** (schema v22–v25, each one
  transaction). Removed settings are deleted from fleet and device configs;
  see below.

**What changes**

- **Wake detection runs on the Dot**, with a BCResNet model, continuously —
  over music, replies and ringing alarms. The controller confirms each
  detection against what the Dot itself was playing, so the Dot's own speech
  does not wake it. Barge-in is always on.
- **Wake model registry.** The dashboard's Wake word panel lists BCResNet
  models (ONNX graph + JSON sidecar) by SHA-256, each with its own
  thresholds. Uploading validates and probes a model but never activates it;
  selecting it (fleet-wide) makes each Dot fetch it by hash when its session
  next starts. The deployed "Ophelia" model
  is installed and selected on first start. API: `/api/wake_models`,
  `/api/wake_models/upload`, `/api/wake_models/{sha256}`, and
  `/api/devices/{id}/speech_assets` for what a Dot has installed.
  openWakeWord models, custom openWakeWord training (`oww_forge/`) and the
  controller-side and shadow wake paths are gone.
- **Endpointing and speech recognition on the controller.** A speech worker
  (sherpa-onnx 1.13.8 with Silero VAD and Kroko streaming ASR, hash-pinned
  and downloaded when the image is built) supplies evidence; a fixed policy
  (`post_afe_2`) decides when you stopped talking. Home Assistant then runs a
  speech-to-text-only pass on the committed audio and a separate intent/TTS
  pass. The wake word is removed from the transcript, never cut from the
  audio; when the streaming recognizer misses a quiet wake word, it is still
  removed from the start of HA's transcript, so local sentences keep matching.
  Utterances are capped at 15 s, or 30 s with the new **Extended
  utterances** toggle. A free-form question asked with a TV talking quietly
  across the room ends after the normal pause.
- **"Ophelia, stop" / "Ophelia, snooze"** stop or snooze a ringing alert. A
  bare wake word no longer stops a ring; a tap on the action button still
  does.
- **Timers are Home Assistant's native satellite timers.** When one
  finishes, the Dot rings it locally, looping the timer sound with the
  configured gap until stopped or until the ring limit (`timerRingSeconds`,
  new default 900 s; a stored value of the old default 60 is raised to 900).
  The LED ring shows the first timer's remaining time.
- **Timers answer without the LLM, and say what they did.** Timer commands
  are Home Assistant's own timer intents. The Home Assistant configuration
  adds local custom sentences (`custom_sentences/en/timers.yaml`) for the
  phrasings its built-in ones miss: "set a timer for 5 hour, 15 min", "set a
  5 and a half hour timer", "5 min, 10 second" ("Timer set for 5 hours and
  15 minutes."); "how much time is left", "what timers do I have" ("You have
  2 timers: a 5 minute timer with 3 minutes left and a 10 minute timer with
  8 minutes left."); "cancel the 5 min timer" ("5 minute timer cancelled."),
  add/remove time, pause, resume. EchoMuse answers the two cancels HA's own
  answer cannot name, through HA's timer intents: "cancel the timer" cancels
  the speaker's only timer and says which, or asks "Which one? Your 5 minute
  timer or your 10 minute timer?" and takes the answer without the wake
  word; "cancel all timers" cancels only this speaker's timers and names
  them. Alarm questions ("when's my next alarm?") are answered from the
  alert engine, and a voice alarm answers with its first due time ("Alarm
  set for 7 AM tomorrow."). The Activity tab shows these turns as handled by
  the timer cancel or the alarm question, with the HA intent time.
- **Alarms live in Home Assistant.** The controller creates one Local
  Calendar per speaker ("EchoMuse &lt;name&gt;"); every alarm is an event on
  it and can be edited in HA's calendar. It installs five scripts exposed to
  Assist for LLM agents — `echomuse_set_alarm`, `echomuse_list_alarms`,
  `echomuse_cancel_alarm`, `echomuse_dismiss_alert`, `echomuse_snooze_alarm`
  — and sets each satellite's VAD sensitivity to relaxed. Alarms can also be
  set by voice (handled by the controller's alarm grammar) and from the new
  **Alerts** tab. Each Dot caches the next 7 days of alarms and rings them
  even while the controller or Home Assistant is down.
- **New HA entities per Dot:** "Alert ringing" (binary sensor), "Stop
  alert" (button) and "Voice state" (sensor: `idle`, `listening`, `thinking`
  or `speaking`, the phases the LED ring shows). Wake-word and button turns
  do not run through the stock Assist satellite entity, which stays idle
  during them; automate on "Voice state" instead.
- **Alert sounds.** New `alarmSound` setting (initialised from
  `timerSound`). Each catalog sound is also exported to the Dot as its first
  10 s with a 50 ms fade-out; longer uploads are flagged. The sound test
  became a preview (`/api/devices/{id}/sounds/preview`).
- **Recording modes** (sample collection, ambient recording, capture) now
  hold a diagnostic uplink lease; the ring pulses magenta while one runs.
- **Activity tab, stage by stage.** Each turn shows its date and time;
  clicking one shows what the controller's streaming recognizer heard and
  why it stopped listening (route, how complete the text looked, silence
  waited), Home Assistant's transcript and the text sent to intent after the
  wake word was removed, who handled it (HA's built-in agent, the
  conversation agent, the alarm engine or a local command), its answer, and
  per-stage times. Schema v24 adds these turn columns. `tts_url_ms` is now
  dispatch → first response-audio URL; the new `intent_ms` is dispatch →
  intent result. Rows from before the cutover show their old outcome.
- **Activity tab, follow-up questions.** A turn whose answer asked a
  question (Home Assistant's `continue_conversation`, or EchoMuse's own
  "AM or PM?") shows what became of it — answered, a wake took over, no reply
  within 7 s, cut off before it finished, no microphone, chain limit — with a
  link to the reply turn; the reply links back to the question. The row list
  marks askers `?` and replies `↩`, a **Follow-ups answered** tile counts
  them, and each turn shows how its spoken answer ended (played to the end,
  cut off, …) and the trace id its controller log line carries. Schema v25
  adds `turn_uuid`, `conversation_id`, `reply_to`, `continuation` and
  `playback_reason`; `playback_ms` now measures audible time.
- **Firmware install for an offline Dot, on reconnect.** A Dot that is
  offline and not on the bundled firmware (for instance one that missed
  **Deploy all**) gets **Install when it reconnects** on its Updates tab and
  in the Deploy all dialog. The request survives a controller restart, shows
  as queued on the device card and page, can be cancelled, and runs when the
  Dot next connects — installing whatever firmware the controller bundles
  then, not a pinned version; a Dot that comes back already current just has
  it cleared. Deploy all now lists every approved device, including ones
  offline since the controller started. Device cards mark firmware that is
  behind. API: `POST`/`DELETE /api/devices/{id}/update/queue`; devices carry
  `update_queued_at`. Schema v33 adds that column.
- **Controller image:** removed `openwakeword`, `speexdsp-ns`, `tqdm`,
  `scikit-learn`, `requests`; added `sherpa-onnx` and the speech bundle (with
  the Kroko model's CC-BY-SA attribution). The image is built from the
  repository root.

**Removed settings**

- Config keys: `owwModel`, `owwThreshold`, `bargeInThreshold`,
  `nearMissThreshold` (thresholds belong to the registry model; `owwModel`
  is replaced by `wakeModel`), `owwOnDevice`, `owwSpeexNs`, `bargeInEnabled`,
  `endpointRelative`, `endpointLowPerMil`, `endpointSilenceMs`,
  `endpointBackporchMs`, `maxSpeechMs` (replaced by `extendedUtterances`:
  on when the old cap was off or above 15 s), `vadThreshold`, `vadSpeechMs`,
  `vadSilenceMs`, `timerRingBurstSeconds` (the sound loops whole). Schema
  v23 also deletes the microphone-processing keys the native AFE replaced —
  `micGainDb`, `adcDigitalGain`, `adcMicpga`, `agcEnabled`, `aecEnabled`,
  `aecDelayMs`, `aecTailMs`, `beamformingEnabled`, `beamAngle` — which were
  still stored and made every dashboard config save fail. The API rejects
  all of them by name.
- Dashboard config sections renamed: **Microphones** → **Speech** (it holds
  only the speech-to-text copy settings) and **Advanced** → **Button**.
- Environment: `OWW_MODEL`, `OWW_THRESHOLD`. Add-on options: `oww_model`,
  `oww_threshold`.
- Dashboard: stock wake word tiles, custom openWakeWord upload, Threshold,
  Speex NS, Barge-in, Barge threshold, Near-miss floor, On-device; Stop on
  quiet, Stop sensitivity, Pause before stopping, Trailing audio, Max turn
  length; VAD Threshold, Speech gate, Silence gate; Burst length; "ring this
  device now".

**Fixed during the cutover**

- **Follow-up questions never listened.** The reply window took dialog-input
  focus as soon as the question started playing, and the Dot answers that by
  cutting off dialog output — so the question was silenced, no reply window
  opened, and the half-open expectation left the ring showing "listening"
  (with a live reply lease) until the next wake. Input focus now passes to
  the reply window only when the question has finished (or an early answer
  cuts it off); a question that is cut off drops its expectation. A streamed
  answer that turns out to be a question starts watching for an early reply
  as soon as Home Assistant says so.
- **"Ophelia, stop" did not stop a ringing timer** (or alarm), and neither
  did the LLM's `echomuse_dismiss_alert`: the firmware accepted only UUIDv4
  operation IDs, but voice and LLM operations use UUIDv5. Firmware now
  accepts any RFC 4122 UUID; a non-benign `alert.act` rejection is logged as
  a warning.
- **Timers rang the built-in fallback tone instead of the Timer sound.** The
  Dot fetched only the sounds of armed alarms, and `alert.ring` names the
  timer sound only as the timer finishes. The controller now sends
  `alert.prefetch` with the timer sound when a Dot connects and when a timer
  starts (firmware capability `alert_prefetch`), and a ring whose sound is
  missing fetches it for the next ring.
- **Short spoken answers were cut off after about a second.** `render.end`
  travels on the control socket and overtook the tail of the audio on the
  audio socket; the Dot then dropped the late audio. A 1.8 s "Timer set for 5
  minutes." played 0.96 s. `render.end` now carries `end_frame` and the Dot
  plays every frame up to it (the same line now plays 1.80 s of 1.80 s).
  Each dialog playback logs how much of the audio sent was played.
- **A successful firmware update was reported as failed** ("OTA exception:
  … is offline"): the slot flip restarts the server, closing the session, and
  closing the update's shell on that dead session raised — skipping the
  reconnect check and leaving the device's shell lock held, so the next
  update could not open a shell until the controller restarted.
- **A Fire OS 6 Dot could lose its microphone for good right after boot**,
  so it never woke and button turns carried no audio. When the Dot booted
  with its clock wrong, the firmware's first NTP sync stepped it forward
  under the open `micAsr` stream; on Fire OS 6574.1 the stream then
  returned status 14 on every read, which the firmware logged in a hot loop
  (15 MB in 4 minutes) instead of re-opening. The firmware now opens the
  microphone only after the first sync (waiting at most 30 s), and re-opens
  the stream on any unexpected status (`docs/fireos6-port.md` §6).

### Code review fixes

- **`SERVER_IP` unset no longer advertises a hard-coded address.** The
  controller and its ESPHome/BT-proxy records fell back to `10.10.1.236`
  (the add-on's default `server_ip` is empty, so add-on installs hit this).
  Unset, it now advertises the address of the interface holding the default
  route and logs a warning; with no default route it refuses to start.
- **Firmware updates:** two update requests for one device could both start;
  a refused update request (device offline, update running) discarded the
  uploaded binary; a job that timed out waiting for the device shell could
  close the shell an update in progress was using; and closing an open
  Console could drop an update's pending shell request.
- **WiFi change:** when the command could not be sent, the device stayed
  marked "change pending" for four minutes and the request failed with a 500;
  it now fails with `device_offline` and nothing is left pending. A failed
  change is logged at `warn` (it was `warning`, which the dashboard did not
  colour).
- **Settings:** `PATCH /api/system/config` with one unknown key no longer
  saves the others before refusing; a non-numeric or zero
  `update_check_interval` no longer stops release checks for good (or polls
  GitHub in a loop). Concurrent config-section writes no longer lose one of
  the updates. `GET /api/releases/controller` now requires a session like
  every other read.
- **Home Assistant scripts** were reported as installed after a failed
  install when an earlier connect had left warnings.
- **Dashboard:** the live event stream now reconnects after the controller
  restarts (alerts, timers and HA status stopped updating until a reload); a
  pending device opens on its approval form; new releases and automatic
  rollbacks show without waiting for a poll; ring animation controls are
  disabled, with the reason, on a Dot without animated LEDs; played and
  downloaded recordings no longer leak memory; controls are reachable and
  named for keyboard and screen-reader use.
- **Memory:** the speech worker queued every observation for a consumer that
  never read them, growing for the life of the process.
- Malformed BLE adverts are dropped instead of being forwarded to Home
  Assistant as blank entries; stopping a media stream stops ffmpeg at once;
  a malformed `timerRingSeconds` no longer ends the ring.
- **Firmware:** a data race in stats collection on reconnect; a timer ring
  or sound preview read its WAV while holding the lock that times due
  alerts; an input-device handle leaked when the second button device failed
  to open.

### Turn latency and reliability

Based on a review of Office turns 433–476 (`docs/turn-latency-review.md`,
`docs/streaming-asr-eou-evaluation.md`).

- **Replies no longer hang when Home Assistant answers with its
  acknowledge chime.** A local command aimed at the speaker's own area ("next",
  "stop") makes HA play `acknowledge.mp3`. A reply fetch that reached HA before
  it had made that choice was never answered, and the turn stayed open for
  about 20 s. The controller now fetches the reply at `intent-progress`
  `tts_start_streaming` (streaming LLM answers) or at `tts-end`, never at
  `run-start`. The ESPHome reply path follows the same rule.
- **A reply that does not start within 10 s of intent-end ends as a response
  timeout.** The limit was 5 s, and streamed replies were allowed 120 s.
- **"Stop" said while a reply is still silent reaches Home Assistant.** It was
  treated as "stop the reply", so HA never received it and the music kept
  playing. A wake now cancels a reply only once its audio can be heard.
- **Faster endpoints (policy `post_afe_3`).**
  - After 320 ms of pause, the controller re-decodes the whole utterance to
    get the streaming model's final words at once. Previously it waited up to
    1.28 s for the model's next update.
  - The 240 ms text-stability wait is removed; it checked nothing.
  - For text taken from that re-decode, route A's trailing-silence check uses
    VAD silence after the last speech.
- **Requests Home Assistant already knows end sooner (policy `post_afe_4`).**
  The controller's own grammar only covers timers, alarms, on/off and stop, so
  "what time is it", the weather or "next" waited the full 1,792 ms pause.
  Now, whenever the streaming text changes into something that grammar does
  not know, the controller asks Home Assistant's own sentence matcher (the
  stock `conversation/agent/homeassistant/debug` command, which recognizes
  without running anything) whether your sentences and automation triggers
  match it. A full match waits 608 ms; a match that a wildcard could still
  extend ("what's the weather … tomorrow", "play …") waits 1,216 ms. The
  controller's grammar still decides everything it covers, and without an
  answer the old pause applies. On the last 38 such requests, 8 would have
  waited 608 ms and 12 1,216 ms. The decision trace records each answer
  (`utterance.recognizer`), and a new **recognizer** line in the Home
  Assistant status shows whether the command is available.
- **HA's sentence matcher is woken with the wake word.** When a wake candidate
  opens, the controller sends HA one throwaway sentence, so a matcher paged out
  after idle is back in memory before the command needs it.
- **Pause transcription can use Home Assistant's speech-to-text model
  (policy `post_afe_5`).** Config → Speech → **Pause transcription**
  (`pauseAsr`) chooses who transcribes a request at its pauses: Kroko
  (default, unchanged) or a Wyoming speech-to-text server, typically the one
  your Home Assistant pipeline uses. With a server, each pause also sends it
  the request so far, and whether the request is complete is judged on its
  words: a misheard wake word ("Fulfiliate, what's the weather in Detroit") no
  longer hides a request Home Assistant knows. The controller connects to the
  server directly and never waits for it: Kroko's words are judged at once,
  and the server's replace them when they arrive (up to 1.5 s later), so a
  request Kroko heard right ends as soon as without a server. **Check** lists
  the server's models. Turns judged on a server's words record the policy
  with `+wyoming:<model>` appended, and the decision trace records each
  pause (`utterance.pauses`).
- **See which transcriber won each pause.** Activity's turn detail has a
  **Pause ASR** row: for each pause, Kroko's words and the Wyoming server's,
  each with its time from the pause (Kroko's decode; the server's round trip,
  counted from when the pause sent it), when its words were judged, why a
  request gave none (dropped as speech went on, timed out, failed, or still
  out when the request ended), and a ★ on the words the request ended on.
  The decision trace carries the same (`utterance.pauses`, and `source` on
  the commit event: `server`, `kroko` or `streaming`), and
  `GET /api/devices/{id}/turns/{turn}/trace` returns a turn's whole trace
  (newest 1,000 turns per speaker).
- **Home Assistant no longer drops the last word of a request.** The audio
  sent for speech-to-text ends 192 ms after the last loud speech, often while
  a quieter last word is still sounding, and the model dropped it: "what time
  is it" arrived as "what time is" in 10 of the last 60 saved requests. The
  audio now ends with 0.5 s of silence, which fixed all 10 and changed no
  other request's words. Saved recordings still hold the span alone.
- **A pause after the wake word no longer cuts you off (policy
  `post_afe_6`).** If no words were recognised 3 s after the wake word, the
  request ended with "Sorry, I didn't catch that", even while you were still
  talking: the streaming model's text lags speech by up to 1.4 s. The 3 s now
  count from when you start the request, so "Ophelia… what's the weather in
  Detroit" with a second's pause after the wake word is heard out.
- **Wake verification and span re-decode decode the end of their window.**
  Their zero flush is 1.5 s (was 0.5 s), which reaches the model's next chunk
  edge. Replayed wake clips that returned empty text now transcribe.
- **Turn rows keep their decision trace** (schema v26): `decision_trace`,
  kept for the newest 1,000 turns per device, and `first_audio_ms`, the time
  from the endpoint to the reply becoming audible. Activity shows it as "first
  audio … after endpoint".
- **Response latency, per turn and per speaker** (schema v30). Each turn
  records `response_latency_ms`: from the end of your last word to the first
  audio of the reply, both timed on the Dot's own clock, so network delay to
  and from the controller does not enter it. Activity shows it on every turn
  (`↦3.4s`) and in the turn detail ("first audio … after speech ended");
  Status shows its p50/p90/p95/p99 over the last 24 hours, 7 days and 30
  days (`GET /api/devices/{id}/response_latency`). Turns that played no reply
  are left out of the percentiles, and earlier turns have no value.
- **Save utterances keeps the last 300 turns per device** (was 10).
- **The Dot starts the wake chime itself, without a controller round trip.** Firmware
  with the new `local_wake_chime` capability chimes on its own when it opens
  an idle wake, instead of waiting for the controller to accept it; the
  controller sends it `wakeSound` and skips its own chime when
  `wake.candidate` reports `chimed`. Wakes heard while the Dot is playing
  audio, and older firmware, still chime after acceptance.
- **Follow-up questions chime when they start listening.** With Wake chime
  on, the wake chime now also plays when a follow-up question has finished
  and the reply window opens, since the answer needs no wake word. It is
  skipped when you have already started answering over the question's end.
- **Answers to follow-up questions are heard like a button press (policy
  `post_afe_7`).** When a follow-up question finishes, the Dot now starts
  listening at once, exactly as after a button press: the ring shows
  listening from that moment, and the ordinary end-of-request logic decides
  what the answer is. A separate start detector used to have to find the
  answer first and missed answers it should have taken — the Kitchen's
  "I demand that you ask me a question" (turn 617) reached the controller
  intact but its first word was too short for it. An answer begun in the
  last half second of the question still counts; speech already going on
  before the question ended (a TV, someone talking) is not taken until it
  pauses. If nobody answers within 7 s the window closes quietly, with no
  "heard nothing" animation, and its row is kept on the Activity page; a
  brief microphone dropout before you answer no longer ends it, and if the
  microphone stops altogether it still closes after 10 s. A
  wake word or button press during a silent window takes the question over.
  Home Assistant hears the answer from just before it starts, not the chime
  and the wait. The Config → Speech → **Follow-up answers** setting
  (`replyOnset`) is gone; schema v31 removes it from saved configurations,
  and the API rejects it.
- **Configurable wake open rules, with shadow rules to try them first.**
  Firmware with the new `open_rules_v1` capability opens a wake on any of a
  list of rules (last 1–3 scores, average or every one, ≥ a threshold). The
  model's own 3-score average rules are always in the list, so live behaviour
  is unchanged until you add a rule (Config → Wake word → Open rules, up to 4).
  Shadow rules (up to 8; five idle rules by default) are evaluated on the Dot
  and never acted on: the Status tab's Wake health panel shows, per rule over 7
  days, how often it would have opened a real wake earlier, its would-be false
  wakes per hour with a 95% upper bound, likely rescued misses and missed
  wakes. Saving either list reconnects the Echo. No audio leaves the Dot for
  shadow rules; older firmware reports no shadow data rather than zeros.
- **Fire OS 6 Dots (amonet-biscuit v2) are supported.** A Dot unlocked with
  amonet-biscuit v2.0.0 can only boot Fire OS 6. Once R0rt1z2's boot-root.zip
  is installed, the provisioning wizard runs the Fire OS 6 flow. It needs no
  recovery mode, Magisk or boot-image write:
  - It writes one init file (`/system/etc/init/echomuse.rc`) plus an empty
    `/tmp` to the running system slot, installs the firmware and Wi-Fi,
    reboots, and waits until the Dot reaches this controller.
  - Only then does it write the same file to the other slot, so a bad install
    leaves a stock fallback slot.

  On Fire OS 6 the firmware records and
  plays through Amazon's `mixer` daemon, so capture still passes through the
  native AFE and playback is still its echo reference. The firmware also:
  - powers the Wi-Fi radio itself, runs DHCP, and sets the clock by NTP
    (Amazon's own Wi-Fi and time services cannot run once Alexa is stopped);
  - reads the light sensor through the kernel's IIO interface;
  - keeps Amazon's privacy driver in step with mute.

  Debloat on Fire OS 6 is the start script's service denylist, applied at
  every boot, by firmware updates, and by **Re-apply debloat**. Dashboard
  firmware updates use toybox `base64`/`md5sum`/`df`. The Dot reports its
  platform in `session.hello` (`platform: fireos6`), and the controller
  admits no other. Notes and evidence: `docs/fireos6-port.md`; how to
  get a Dot there: `docs/rooting.md`.
- **Fire OS 5 support is removed.** Firmware built from this tree runs audio
  only on Fire OS 6, so everything that served Fire OS 5 Dots is gone: the
  wizard's Fire OS 5 provisioning (TWRP, boot patch, Magisk, its database
  preseed and the debloat steps; an Android 5.x Dot is now refused at the
  first step), the Magisk debloat boot script and its `pm hide` package list,
  the firmware's OpenSL ES audio backend, the upgrade-only handler for
  pre-v1 firmware on `/control`, the `platform` and `upgrade_required` device
  API fields, and the `/api/provision/debloat_script`,
  `/api/provision/debloat_packages` and `/api/provision/magisk_db`
  endpoints. A `session.hello` without `platform: fireos6` is refused with
  `protocol`.
- **Reflashed Dots can be re-adopted from the provisioning wizard.** A Dot
  whose serial is already registered (one reflashed from Fire OS 5 to 6, say)
  used to be refused at the first step until you deleted it, which took its
  name, settings, Home Assistant device and alarms with it. The wizard now
  shows which device the serial belongs to and asks: **Continue** reinstalls
  EchoMuse and keeps that device, so the Dot rejoins with everything it had;
  **Abort** stops before anything is written to the Dot. Deleting the device
  and retrying still provisions it as new.
- **The controller image carries the device firmware, and installs nothing
  else.** `controller/Dockerfile` compiles the firmware from the same tree
  (the digest-pinned toolchain moved there from `device/compiler/`), and the
  provisioning wizard, a device update and **Deploy all** push exactly that
  binary. The wizard's Install EchoMuse step no longer asks anything; the
  GitHub firmware releases, **Check now**, release notes, the binary upload
  (**Local Build**, the wizard's custom build, `tools/ota.py`) and the device
  release workflow are gone. The firmware version is `fw-` plus a hash of its
  sources, so it changes only when the firmware does: a Dot is offered an
  update when its version differs from the bundled one, never because the
  controller alone changed. The hourly GitHub check now looks only for a
  newer controller image. A bare-metal controller needs `FIRMWARE_DIR` (fill
  it with `device/compile.sh`) and refuses to start without it.
- **Each Dot's OS is shown, apart from its EchoMuse firmware.** The firmware
  now reports the Fire OS build it runs (`ro.build.version.name`, e.g. "Fire
  OS 6.5.6.9 (NS6569/6009)") in `session.hello` as `os_version`, and the
  controller keeps it with the device (schema v32), so an offline Dot still
  shows what it last ran. Device cards, the device page and its Status tab
  show it as **OS**; the firmware version is labelled **EchoMuse firmware**
  everywhere, and the Updates tab says that updating it never changes the
  OS. A Dot shows its OS once it runs this firmware. The provisioning wizard
  calls the Fire OS build the OS too, where it used to say "Firmware".
- **Native AFE metadata from Fire OS 6 Dots, recorded as evidence only.**
  Firmware with the new `afe_metadata_v1` capability decodes Amazon's
  per-8 ms AFE metadata and, under the turn and candidate uplink leases,
  sends one 14-byte record per 80 ms (EMA1 kind 5 `afe`, about 1 kB/s). Each
  turn row (and refused wake) stores summaries of the second before the wake,
  the wake's support window and the utterance, plus when the Dot's playback
  last started (schema v28, `afe_evidence`): frames received, playback, ERLE,
  double-talk, RMS, DNN VAD, volume, clipping/divergence/mute and decoder
  gaps. It is in the decision trace and the turns API; Activity charts it
  per turn (below), and the Status tab's Wake health panel shows the decoder's
  health from `wake.stats`. Nothing decides on it yet. Older firmware shows
  it as unavailable with the reason; a span without
  records is "no data", never zeros. What each field can be trusted for, and
  the experiments that would decide any use beyond evidence:
  `docs/afe-metadata.md`.
- **Native AFE chart per turn.** Under "Native AFE", Activity charts the
  turn's 80 ms AFE records from the second before the wake through the
  utterance and the spoken answer: DNN VAD, double-talk, ERLE, RMS and
  playback, each on a fixed scale. Bars above the lanes show the wake
  window, the utterance and the turn's processing, each with its duration:
  the endpoint's wait for the end of speech, Home Assistant speech-to-text,
  intent handling, the spoken answer's audio until it was audible, and the
  answer playing. The processing bars are timed on the controller's clock, so
  they can run about 0.3 s ahead of the audio; the playback lane is the Echo's
  own record of when the answer played. Missing or short periods and flags are marked,
  and hovering reads out one period and the bars it falls in. The
  per-span text summaries are gone from the page (they stay in the decision
  trace and the turns API). The records are stored per turn (schema v29,
  `afe_series`) for the newest 1,000 turns per device, like the decision
  trace, with where each recording sits (`wake_clip`, `audio_clip`,
  `recording`: capture-sample spans, WAV sample k = span start + k) and the
  processing (`stages`). They are never in the turns list;
  `GET /api/devices/{id}/turns/{turn}/afe` serves one turn's. Older turns show
  no chart, or no processing bars.
- **The AFE chart runs on through the response.** When a turn commits, the
  controller opens one more `turn` lease for the `afe` stream before it closes
  the committed one, so the native AFE records continue without a gap. The
  lease stays open through the thinking time and the spoken answer, until 1 s
  after the turn ends or 30 s after the commit, whichever is first. Needs
  `afe_metadata_v1`, and no firmware update is needed. Such a turn's row
  is written about a second after the turn ends.
- **Play the whole turn against its chart.** With **Save utterances** on, that
  lease also carries the microphone, and the controller keeps a turn
  recording: the Echo's own processed audio from just before the wake word
  through the spoken answer, joined sample for sample across the commit.
  "Play recording" under the chart plays it with a playhead moving across the
  chart and the readout following it; a row's wake clip and utterance buttons
  drive the same playhead. The last 100 per device are kept in
  `turn_recordings/` beside the database
  (`GET /api/devices/{id}/turns/{turn}/recording`). Mute or a disconnect
  during the answer erases the answer's part. With **Save utterances** off,
  no microphone audio leaves the Dot after the request, as before.

### Earlier in this release

- ASR gain: the STT copy sent to Home Assistant is amplified by
  `ASR_GAIN_NOMINAL_DB` (20 dB), clamped at the int16 rail. Native-AFE speech
  is quiet, and faster-whisper fails there by silently dropping the quiet
  leading words rather than by erroring, so "How many ounces are in a cup?"
  arrived as "ounces are in a cup." and the intent never matched. The gain
  touches only the STT copy (and the saved utterance, which is that copy);
  wake, endpoint and attribution evidence are unaffected.
- The gain is bounded by the wake word's measured loudness as a guard band,
  not as a normaliser: the nominal gain is held while the predicted level
  lands in the band that decodes well (-22 .. -34 dBFS), and only followed
  outside it — backing off a close talker who would clip, and pushing a
  distant one up to a 30 dB ceiling. Button turns use the nominal gain.
- Wake clips: the audio that triggered each wake-word turn is kept, opt-in
  per device (`saveWakeClips`, Config → Wake word) — the accepted
  detection's support span from 300 ms before it starts. A turn's utterance
  recording does not show why a device woke; this does. Each turn's clip
  plays and downloads from its row in Activity; every clip for a device
  comes down as one archive (`GET /api/devices/{id}/wakeclips.zip`).
- Schema v20: `turns.wake_file`. Clips live in `wakes/<device>/` beside the
  database, 500 per device, and are removed with the device.

## 1.0.1

- Initial Home Assistant Supervisor add-on packaging: install and run the
  controller from Settings → Add-ons instead of hand-run docker-compose.
- Ingress support for the dashboard (no separate port to expose).
- Add-on config UI labels, icon, and logo.
