# Changelog

## Unreleased

### Post-AFE cutover (device protocol v1)

This release replaces the voice, wake and alert stack. The architecture is
[docs/post-afe-audio-architecture.md](../docs/post-afe-audio-architecture.md);
the device wire protocol is [docs/protocol-v1.md](../docs/protocol-v1.md).

**Before you upgrade**

- **Every Dot needs new firmware.** Upgrade the controller first. A Dot
  still on older firmware connects only to an upgrade-only handler: the
  dashboard marks it **Upgrade required**, and until you click **Update
  firmware** on its device page it takes no voice turns, rings no timers or
  alarms and plays nothing. Nothing it reports changes controller state.
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
  (`post_afe_1`) decides when you stopped talking. Home Assistant then runs a
  speech-to-text-only pass on the committed audio and a separate intent/TTS
  pass. The wake word is removed from the transcript, never cut from the
  audio; when the streaming recognizer misses a quiet wake word, it is still
  removed from the start of HA's transcript, so local sentences keep matching.
  Utterances are capped at 15 s, or 30 s with the new **Extended
  utterances** toggle.
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
- **New HA entities per Dot:** "Alert ringing" (binary sensor) and "Stop
  alert" (button).
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
