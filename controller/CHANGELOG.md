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
- **"Say that again" replays the last answer.** "Say that again", "repeat
  that", "what did you say" and similar replay the Dot's last spoken
  response, the same audio from Home Assistant, without asking the
  conversation agent again, as long as it ended within the last two minutes;
  otherwise the Dot says "I haven't said anything in the last two minutes."
  Announcements are not replayed. A replayed question can be answered
  without the wake word, in the same conversation. The Activity tab shows
  these turns as a replay.
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
- **Activity tab, stage by stage.** Each turn shows its trace number
  (`#1234`, the key of `/api/devices/{id}/turns/{n}/trace`; hover for the
  controller-log id) and its date and time;
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

- Config keys: `owwModel`, `owwThreshold`, `bargeInThreshold` (thresholds
  belong to the registry model; `owwModel` is replaced by `wakeModel`),
  `owwOnDevice`, `owwSpeexNs`, `bargeInEnabled`,
  `endpointRelative`, `endpointLowPerMil`, `endpointSilenceMs`,
  `endpointBackporchMs`, `maxSpeechMs` (replaced by `extendedUtterances`:
  on when the old cap was off or above 15 s), `vadThreshold`, `vadSpeechMs`,
  `vadSilenceMs`, `timerRingBurstSeconds` (the sound loops whole). Schema
  v23 also deletes the microphone-processing keys the native AFE replaced —
  `micGainDb`, `adcDigitalGain`, `adcMicpga`, `agcEnabled`, `aecEnabled`,
  `aecDelayMs`, `aecTailMs`, `beamformingEnabled`, `beamAngle` — which were
  still stored and made every dashboard config save fail. The API rejects
  all of them by name. `nearMissThreshold` is deleted too, because its value
  was on openWakeWord's scale; the key itself is kept and starts again at
  0.17 on the BCResNet scale (see "Near misses in Activity" below).
- Dashboard config sections renamed: **Microphones** → **Speech** (it holds
  only the speech-to-text copy settings) and **Advanced** → **Button**.
- Environment: `OWW_MODEL`, `OWW_THRESHOLD`. Add-on options: `oww_model`,
  `oww_threshold`.
- Dashboard: stock wake word tiles, custom openWakeWord upload, Threshold,
  Speex NS, Barge-in, Barge threshold, On-device; Stop on
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
- **A wake that hears nothing after the wake word ends with an error tone.**
  With Wake chime on, a wake turn that closes `no_input` now plays a new
  built-in earcon, `builtin:no_input_chime` (the Amazon Echo's own error
  tone, `ui_error_generic_2`), as the ring throbs, so the Dot says it has
  stopped listening and sent nothing to Home Assistant. Turn
  647 heard a whole command its speech detector never counted and ended in
  silence. Firmware with the new `no_input_chime` capability carries the
  sound; older firmware and button presses stay silent, and an unanswered
  follow-up question still ends without a cue.
- **A Dot without its controller says so.** While a Dot has no session —
  controller down, network gone, or the link lost — its ring pulses red
  slowly (4 s cycle; it was orange), and the wake word gets a spoken "I can't
  connect to the smart home right now" followed by the no-input chime,
  played by the Dot itself in the voice of your Assist pipeline (rendered
  once through Home Assistant TTS and built into the firmware). A request
  the controller was still handling when the link dropped gets the same
  answer instead of silence. The Dot keeps the last wake model it ran and
  loads it again at boot, so a Dot that restarts while the controller is
  down still hears the wake word; only a Dot that has never connected has
  none. Every Dot in earshot answers, since nothing chooses one without the
  controller; wakes heard over the Dot's own sound or a ringing alarm are
  ignored (the action button still stops an alarm).
- **Announcements open with the Echo's announcement chime.** A Home Assistant
  announcement — `tts.speak` or `media_player.play_media` with `announce` to
  a Dot, or `assist_satellite.announce` with its preannounce on — now starts
  with a new built-in earcon, `builtin:announcement_chime`, the Amazon Echo's
  own announcement sound, with any music already ducked. On the
  `assist_satellite` path it takes the place of Home Assistant's preannounce
  sound, including a custom `preannounce_media_id`; with preannounce off
  there is still no sound. A wake, stop or new turn during the chime drops the
  announcement, as during the announcement itself. Firmware with the new
  `announcement_chime` capability carries the sound; older firmware plays
  Home Assistant's preannounce sound as before.
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
- **"Ophelia, stop" pauses what the Dot is playing (policy `post_afe_8`).**
  A wake while the Dot's own media player plays — the news, a radio stream,
  anything Home Assistant or Music Assistant sent to its `media_player` —
  takes "stop" or "pause" as a local command: the controller pauses it
  without Home Assistant, as HA's pause button would, and HA shows it paused
  at once. "Resume" stays Home Assistant's. Before, the request went to Home
  Assistant, whose "stop" pauses only exposed players, and the Dots' own
  players are usually unexposed; the Kitchen's newscast played on through two
  "Okay."s. Another Dot that hears the same wake first still answers anything
  else, but no longer swallows the stop: the playing Dot keeps listening for
  it, silently (turn 654, where the idle Living Room Dot won and heard
  nothing). "Cancel" and "stop the music" still go to Home Assistant. A bare
  "pause" now ends after the short pause, like "stop".
- **Answers to follow-up questions are heard (policy `post_afe_9`).** A
  reply window could hear nothing at all: the speech detector carried what it
  had learned from the question's own echo into the window and then scored a
  quieter answer as silence for seconds. The Master Bathroom's "the bedroom
  fan" (turn 722, answering "Which fan should I turn off?") was transcribed
  but never counted as speech, and the window closed after 7 s; turns 686 and
  689 failed the same way. The window now starts the detector from the state
  it was in just after you finished asking (about 0.3 s after you stopped
  talking), or from a fresh state when Home Assistant started the
  conversation, and ignores anything in its first 320 ms except to date an
  answer that began there. With Wake chime on, the window now opens when the
  chime has finished instead of under it, so its 7 s start after the chime;
  you can still answer over the chime. Replayed on saved recordings, it heard
  all 3 missed answers, the 4 that already worked, and 68 of 70 wake
  commands used as stand-in answers, with no false start in 36 quiet
  stretches; on steady room noise false starts stay at 2% of windows (12%
  without the 320 ms guard). Each reply's trace records where its detector
  state came from (`reply_window`) and the guard (`guard_until`,
  `answer_start`).
- **Quiet commands are heard (policy `post_afe_10`).** The speech detector
  scores quiet speech lower than loud speech, and at the level it shared with
  the speech recognizer (+20 dB) the quietest commands never counted as
  speech: the Master Bathroom's "turn off the lights" and "open the curtains"
  (turns 729 and 732) were transcribed, but both turns ended as no input. The
  detector now reads its own copy of the microphone at +30 dB, and a command
  needs a score of 0.70 instead of 0.65 to start, which takes back part of the
  room sound the louder copy lets through. Replayed on 87 saved wake turns, it
  heard all 84 commands instead of 82, and where a command ends did not move
  (median 0 ms); on quiet room audio, false command starts went from 1.3 to
  2.6 an hour. The speech recognizer, wake verification and the audio sent to
  Home Assistant are unchanged.
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
- **Every turn gets its recording, not only accepted requests.** A turn that
  ended without one (no command heard, cut short) and a refused wake are
  recorded too while **Save utterances** is on, from just before the wake
  word to where the Dot stopped listening, and play against their chart.
  Refused wakes share the 100-recording window, so a Dot that false-triggers
  often keeps fewer accepted turns.
- **Sample clips and ambient recordings get the AFE chart too.** While a
  recording mode runs, its diagnostic lease also takes the `afe` stream, and
  each saved clip and ambient file keeps its 80 ms records beside the WAV
  (`<name>.afe.json`, on the recording's own timeline; deleted and pruned
  with it). **Chart** on a row of the Samples tab opens DNN VAD, double-talk,
  ERLE and RMS over the whole recording, drawn by the same chart as a turn's;
  ▶ Play draws the playhead across it. Lists report `afe` per entry;
  `GET /api/devices/{id}/samples/{name}/afe` and `…/ambient/{name}/afe` serve
  one. Needs `afe_metadata_v1`, and no firmware update; recordings made
  before this, or without the capability, have no chart.
- **Speech before the wake word no longer reaches Home Assistant.** When
  Home Assistant's speech-to-text keeps words said before "Ophelia" that the
  streaming recognizer dropped, the wake word sat too far into HA's
  transcript to be found, and the whole sentence went on unchanged: turn 552,
  transcribed without the STT server's VAD clip, would have sent "I gotta go
  faster. Ophelia What time is it?" to intent. A wake word found elsewhere in
  HA's transcript is now removed, with everything before it, when the number
  of words after it is within two of the number the streaming recognizer
  heard after its own wake word, so that turn sends "What time is it?". A
  later mention ("what does Ophelia mean" with the wake word dropped) still
  has too few words after it and is kept.
- **The wake word over a ringing alarm or timer gets the usual wake
  response.** The ring went quiet, but the LEDs stayed dim cyan and, with
  Wake chime on, no chime played: the alarm's silence was meant as the
  acknowledgement, yet it sounds the same as a gap between its bursts. The
  controller now chimes an alert-context wake on acceptance, and the Dot puts
  a backgrounded alert's dim cyan beneath the conversation ring, so the green
  listening ring, the thinking spinner and the speaking meter show as for any
  wake; a sounding alert still outranks them. The LEDs need the firmware
  update the dashboard offers; the chime needs only the controller.
- **Transcript correction (schema v34).** Home Assistant's speech-to-text
  sometimes gets a request almost right ("plate cherub rock", "watch flex",
  "what time is" with the last word lost, "ophelia, what's the weather" with
  the wake word left in), and Home Assistant then hands it to the conversation
  agent. The controller can now repair such a transcript just before the
  intent run, after its own routes (reply choices, "say that again", alarms,
  timer cancels) have seen it as heard. It proposes a repaired name, a removed
  wake-word remnant, a known request completed or matched (from your
  automations' sentence triggers, requests Home Assistant answered before, and
  your own phrases), or digits for number words, and accepts one only if Home
  Assistant's own sentence matcher, which runs nothing, fully understands it
  in the pipeline's language for this speaker; nothing said explicitly (not,
  on/off, numbers, units, the action, a room or device already recognised)
  changed; and no other reading comes within 0.05. Text the matcher already
  understands, and answers to a question, are sent as heard. Config →
  **Transcript correction** (`transcriptFix`): **Shadow** (default) records
  what would have been sent and still sends the transcript as heard; **On**
  sends a repair that is ready within 0.1 s of the final transcript; **Off**
  does nothing. Two ranked
  lists, **Preferred terms** (`transcriptFixTerms`) and **Preferred phrases**
  (`transcriptFixPhrases`), add your own titles and requests; they rank
  candidates and never train speech-to-text. Each turn records the outcome in
  `stt_fix` (heard, proposed, sent, the rule, Home Assistant's verdicts, why it
  was or was not applied, cache and timing), shown as **Correction** in
  Activity's turn detail; `stt_text` keeps the transcript as heard. Replayed
  over the last three weeks' 211 Home Assistant turns, 13 would have been
  repaired (every one with a known intent correctly) and 1 left alone as
  ambiguous. New dependency: `hassil==3.11.0`, the version Home Assistant
  2026.8.1 ships, to expand trigger sentences.
- **Try a transcript.** Config → **Transcript correction** has a box where
  you type what speech-to-text heard ("RMV alarm") and see, as you type,
  what On would send on that speaker: the proposed correction, whether it
  would apply and why, and every candidate with what Home Assistant's
  sentence matcher accepted it as. It uses the saved settings, the
  speaker's Home Assistant satellite and its pipeline's language, asks only
  the matcher (nothing runs), and says when EchoMuse itself handles the
  text. Activity's correction evidence now also shows what Home Assistant
  matched each candidate as (`POST /api/text_fix/trial`).
- **Transcript correction compares how words sound.** A guess is judged by
  how alike the words it changes sound, as phonemes from the CMU Pronouncing
  Dictionary compared by articulatory features, instead of by spelling and a
  rough sound key, and words the guess shares with the heard text no longer
  count. "rmv alarm" and "arm v alarm" now correct to "arm the alarm", and
  look-alikes that only shared a word ("the other tv" → "office tv") are no
  longer proposed. Config → **Transcript correction** gains
  **Pronunciations** (`transcriptFixPronunciations`): your own ARPAbet
  spelling for words the dictionary lacks or says differently ("plex" →
  `P L EH K S`), with Home Assistant's words that have none listed beneath
  (`GET /api/text_fix/unknown_words`). **Try a transcript** shows each
  word's pronunciation and where it came from. New dependency:
  `cmudict==1.1.3`.
- **One stall no longer takes speech offline for every speaker.** When the
  controller paused for a moment (a transcript-correction rebuild held it for
  about half a second), every speech job queued behind the late one missed
  its deadline too, each counted as a separate error, and three errors within
  60 s made speech unavailable on every speaker for at least 20 s. A missed
  deadline is now counted only when its job was submitted after the last
  counted one, in any lane and for any speaker; the queued jobs still end
  their turn as before. Exceptions always count, and a worker that stays too
  slow still goes unavailable.
- **Speech worker errors are logged.** Each counted error is logged as a
  warning with its component, speaker, lane and reason, even when its turn
  has already ended; speech becoming unavailable is a warning naming the
  errors that caused it, and recovery is logged too.
- **Transcript correction no longer pauses speech.** Building its tables
  and compiling pronunciations held the controller for up to 0.7 s at a
  time, long enough for a speech job to miss its deadline and end the turn
  (`interrupted`). All of transcript correction's work now runs in one child
  process of its own: the tables, pronunciations, candidates and the
  acceptance check live there, and the controller only sends it requests
  and asks Home Assistant's matcher. Measured while the child builds and
  compiles, a speech-sized job in the controller is delayed by at most 2 ms,
  against over 100 ms before. If the child exits it is logged and restarted;
  until its tables are rebuilt, corrections report `not_ready` and the
  transcript is sent as heard.
- **Transcript correction rebuilds only when something it uses changed.**
  Home Assistant republishes some entities' registry entries every few
  minutes (a media player's `capabilities`), and each one withdrew the
  correction tables and rebuilt them: over a thousand rebuilds in under two
  days, with the same 224 names every time. A registry event now refreshes
  the vocabulary only when it can change it (a name, alias, area, device,
  Assist exposure, rename, enabling, or any area or floor change), and a
  refresh that changed nothing keeps the tables in use without a rebuild.

### Echo Show 8 (`crown`)

- **The controller admits the Echo Show 8 (1st gen, `crown`).** Its firmware
  is the same bundled binary the Dots run, choosing its audio backend at
  runtime, and reports `platform: fireos6_crown` in `session.hello`; the
  controller admits `fireos6` and `fireos6_crown` and refuses any other
  platform with `protocol`. Updates, **Deploy all** and the install queue
  push it the same firmware, the same way.
- **Four new optional capabilities** (docs/protocol-v1.md §4.1):
  `wifi_control`, `ble_proxy` and `privacy_mute`, announced by Dot firmware
  only, and `screen_wake`, announced by the Echo Show only. On an Echo Show,
  Android owns Wi-Fi and Bluetooth and mute is not integrated yet. What each
  absent capability turns off, with the reason shown:
  - **Wi-Fi** change and scan (device page, Config tab): disabled, and the
    API refuses them (`409 not_supported`, "Wi-Fi changes unavailable: …").
  - **Bluetooth proxy** toggle: disabled. The controller drops `ble_adverts`
    from such a device and runs no Home Assistant Bluetooth proxy for it.
  - **Mute** (Status tab, and the Muted state on cards): unavailable rather
    than off. The `muted` device field is null for such a device.

  The reason is "managed by the device" for Wi-Fi and the Bluetooth proxy on
  an Echo Show and "mute not supported on this device yet" for its mute. A Dot
  on firmware predating these capabilities shows "firmware update available"
  until it takes the bundled firmware, and until then its Wi-Fi and
  Bluetooth-proxy controls are disabled and its mute is shown unavailable.
- **The Echo Show has no ring and no action button.** The Ring and Button
  settings are disabled for it ("no LED ring on this device", "no action
  button on this device"), and the controller sends LED messages only to a
  device announcing `leds`.
- **Screen settings (new section, Echo Show only).** **Light the screen on
  wake** (`screenWake`, off by default) makes the Echo Show turn its kiosk
  screen on when it hears the wake word, through Fully Kiosk Browser's local
  REST API at `screenWakeUrl` (default `http://127.0.0.1:2323`, the kiosk on
  the device itself) with `screenWakePassword`. The keys are sent only to a
  device announcing `screen_wake`. A Dot shows them disabled ("no screen on
  this device"). The support bundle redacts the URL and the password.
- **Device page:** the Status tab shows the **Platform** (Echo Dot 2nd gen or
  Echo Show 8 1st gen) and **Mute**. The device API adds `platform` and
  `capability_gaps` (each absent capability's reason). The support bundle
  adds `platform`.
- **Echo Show volume that cannot be verified is shown unavailable.** The
  Echo Show's volume is Android's; when its firmware's startup check of the
  Android volume calls fails, its `session.hello` carries `volume.level` null
  for that run. The controller then sends it no `volume_set` and no
  `startupVolume`, ignores any `volume_state`, gives Home Assistant's media
  player no volume slider and an unknown level, and the Status tab shows
  Volume "unavailable — volume control unavailable: Android volume check
  failed (see device log)". The device API adds `volume_gap` with that reason
  and reports `volume` null.
- A volume change on the device (buttons, or the Echo Show's volume keys) no
  longer flashes a raw 0–127 level as a percentage on the Status tab until
  the next refresh.
- **Firmware: one binary, platform chosen at start.** The firmware detects
  the image (`libmixerAPI.so` is the Dot; `ro.product.device=crown` with
  `libOpenSLES.so` is the Echo Show) and refuses to start on anything else,
  with a log line naming what it found. On the Echo Show it never runs the
  Dot's Wi-Fi management, clock setting, BLE scanner, LED ring, buttons,
  headphone routing or `tinymix`, leaving the network, Bluetooth and clock
  to Android, and writes heap profiles to `/data/local/tmp/echomuse` (no
  `/tmp` there). Stats from an Echo Show carry no SSID, BSSID, link speed or
  frequency: those came from `wpa_supplicant`, which Android owns there.
- **Echo Show volume is Android's music volume.** Dashboard volume maps
  proportionally onto Android's `STREAM_MUSIC` index (0–30) through
  `IAudioService` (`service call audio`, transaction codes pinned for build
  NS65741/8146 and checked at every start); the volume keys and the kiosk
  move the same index, and their changes are reported back like a Dot's
  button presses. Android's index is kept across restarts: the stored
  startup volume is not re-applied over it. If the start-up check fails the
  firmware logs why, leaves volume alone (no changes applied or reported)
  and sends its hello volume level as `null`, so the dashboard shows volume
  as unavailable rather than a level it cannot set.
- **Echo Show ambient light** is read from its `stk3x1x` (`als_ps` driver)
  and announced as `ambient_light` when it reads.
- **Echo Show audio goes through Android's own audio stack** (OpenSL ES,
  `internal/opensl`; libOpenSLES is loaded at run time, so the Dot's binary
  is unaffected). Capture is a `VOICE_RECOGNITION` recorder, which Amazon's
  AFE echo-cancels and tags with its v3.3 metadata, decoded into the same
  per-period records as the Dot's. Each 80 ms period is stamped from the
  metadata's `AFE_TIMESTAMP` (CLOCK_MONOTONIC ms, unwrapped against the read
  completion; the completion while the decoder is unlocked), never later
  than its delivery and never stepping back. A recorder that errors or goes
  1.5 s without a buffer is replaced, and its loss counted as drops. So is a
  new one silent for 2 s: when another app already records, Android 7.1
  gives our recorder no audio while OpenSL reports success, and only a fresh
  start recovers it, so capture returns within about 2 s of the other app
  stopping (log lines rate-limited). Output is a 48 kHz mono player on
  `STREAM_MUSIC` holding two 2,048-frame buffers, and each completion is
  stamped at its estimated playout from the queue depth and a 40 ms
  pipeline lower bound, which `device/tools/sl_latency` (new, hand-run)
  measures through the firmware's own audio path. The Dot's render play
  clock moved to the shared `internal/audio/playclock` unchanged.
- **The provisioning wizard provisions an Echo Show 8.** `connect_android`
  tells the two apart (`biscuit_puffin` or `crown`, Android 7.1.x, a root adb
  shell), warns off the tested build per device (`NS6569/6009` on the Dot,
  `NS65741/8146` on the Show), and refuses a Show that is not already on
  Wi-Fi, which is joined on the device itself. The Show's steps are
  `connect_android → check_home → install_boot_hook → install_em →
  neutralize_alexa → reboot → reconnect → verify_service → confirm_link`;
  the Dot's are unchanged, and each device's other steps show as not
  applicable. **Check Kiosk** refuses until a non-Amazon home app is
  offered (`cmd package query-activities`), switching on Fully Kiosk's
  `.LauncherReplacement`, which ships disabled, when Fully is installed.
  The boot hook is one `/system/etc/init/echomuse.rc` (uid 0, group `audio`;
  single partition, nothing mirrored). **Neutralise Alexa** marks setup
  complete; repairs Amazon's Settings app, whose pages all crash on a Show
  never registered with Amazon (no config bundle), with one broadcast
  (`com.amazon.dcp.sso.broadcast.CORPFMHasChanged` to the explicit
  `AccountPropertyChangeReceiver`; that receiver's account-removed action
  factory-resets the device and is never sent); makes the chosen kiosk the
  default home app (`cmd package set-home-activity`); `pm hide`s the Tier 1
  list, plus Tier 2 when its switch is on (off by default; switched off on a
  later run, Tier 2 is unhidden), never the package holding every enabled
  keyboard and never `amazon.speech.sim` or `com.amazon.kindle.otter.oobe`
  (hidden, they crash-loop SystemUI and every Settings page; the wizard
  unhides them if an earlier list hid them); denies the microphone to the
  Amazon apps that stay (`appops set <package> RECORD_AUDIO ignore`: Alexa's
  wake engine and Amazon's dictation keyboard) and verifies it; then checks
  that home resolves to the kiosk, which can only hold once Amazon's setup
  screen is hidden. It logs the Show's Wi-Fi and Bluetooth Settings
  components to add to the kiosk. Failure diagnostics on a Show collect
  `audioserver`, the home activities, hidden-package counts, the applied
  tiers, setup state, the mute's `amazon-gating` state, `wlan0`, the
  microphone app ops and the Settings bundle instead of the Dot's `wpa_cli`
  and mixer probes. Undo: docs/rooting.md.
- **Hide and mic-deny lists are controller payloads** (`device_payloads/
  crown_hide_packages.txt`, Tier 1, 26 packages; `crown_hide_packages_tier2.txt`;
  `crown_mic_deny_packages.txt`), served to the wizard by
  `GET /api/provision/crown_lists` and installed on the Show in
  `/data/local/etc/echomuse/` beside `crown_tiers` (the tiers applied). Every
  firmware update and **Re-apply debloat** sync them to an Echo Show by md5,
  then the script re-applies them; the device log names any package
  re-hidden or refused, and any microphone denied again or still allowed.
- **`start_server.sh` has an Echo Show branch** (chosen by
  `ro.product.device=crown`): it waits for `audioserver`, stops the Show's
  native Amazon services (`CROWN_STOPLIST`), holds the wake lock, re-hides
  any listed package that has become visible again and, while Tier 1 is
  applied, re-denies any listed microphone whose app op is no longer
  `ignore`, and runs the same supervisor with its log in
  `/data/local/tmp/echomuse/server.log`. It never touches `tinymix`,
  `wpa_supplicant`, `audioserver` or the Dot's denylist.
  `start_server.sh debloat` on a Show does the same re-hiding, re-denying
  and stopping and reports `HIDE_REASSERTED:… STILL_VISIBLE:…` and
  `MIC_REASSERTED:… STILL_ALLOWED:…` before its result line. The Dot's path
  is unchanged.

### Echo Show 8 on LineageOS 18.1 (`lineage18_crown`)

- **The controller admits an Echo Show 8 running LineageOS 18.1.** Its
  firmware (the same bundled binary) reports `platform: lineage18_crown`;
  the controller now admits `fireos6`, `fireos6_crown` and `lineage18_crown`
  and still refuses any other platform with `protocol`. Its capability gaps
  are the Fire OS Show's ("managed by the device" for Wi-Fi and the Bluetooth
  proxy, "mute not supported on this device yet", no ring, no action
  button), Home Assistant shows it as model "Echo Show 8 Gen 1 (crown,
  LineageOS)", and the Status tab's Platform reads "Echo Show 8 1st gen ·
  LineageOS 18.1". Volume on the wire stays 0–127 (Android's index is 0–15
  there; docs/protocol-v1.md §4.8).
- **The firmware runs there on AAudio** (docs/echo-show-lineage-plan.md
  Phases L1–L3). `internal/platform` picks LineageOS by
  `ro.lineage.version`, and now admits the Fire OS Show only at API 25, so
  a LineageOS Show can never take the Fire OS path, whose volume check would
  call `setStreamVolume`. Capture is a `VOICE_RECOGNITION` AAudio stream
  stamped from a least-squares fit through AAudio's own capture timestamps,
  5 ms earlier (their measured bias), never after the read completes;
  render is a `USAGE_MEDIA` AAudio stream stamped at AAudio's presentation
  time plus 60 ms (`aaudioRenderLatencyNs`). Measured with
  `sl_latency -backend aaudio` over five opens: no late stamp, the echo
  40–54 ms after its reference stamp, the per-open medians within 2 ms.
  The AFE metadata decodes on every open and `afe_metadata_v1` is
  announced. Volume uses Android 11's binder codes (get 16, set 10, max 18)
  over `STREAM_MUSIC` 0–15; the light sensor's polling is switched on
  before it is read (it is off on LineageOS, and the reading then stays at
  `0 lux`).
- **The wizard no longer leaves `/data/local/bin` world-writable.** adb's
  shell runs with umask 000 (measured on the LineageOS Show), so `mkdir -p`
  created it mode 777; it is now chmod 755 on every image. With SELinux
  permissive on LineageOS, the file mode is the only thing keeping an app
  from replacing the binary init runs as root.
- **No debloat on LineageOS.** There are no Amazon apps or services to stop,
  hide or deny the microphone, so firmware updates sync `start_server.sh`
  but never run its `debloat` mode there, **Re-apply debloat** is disabled
  with the reason "nothing to debloat: LineageOS has no Amazon apps or
  services", and `POST /api/devices/{id}/debloat` refuses it (`409
  not_supported`). The device API adds `debloat_gap` with that reason.
- **`start_server.sh` has a LineageOS branch** (chosen by a non-empty
  `ro.lineage.version`, tested before `ro.product.device=crown`): logs and
  scratch space in `/data/local/tmp/echomuse`, a wait for `audioserver` and
  the wake lock, as on the Fire OS Show, but no move into `su` (init starts
  it there), no stop list, no hide lists, no microphone denials and no
  `tinymix`. `start_server.sh debloat` there changes nothing and reports
  `DEBLOAT_STOPPED:0 STILL_RUNNING:`.
- **The provisioning wizard provisions a LineageOS Show.** `connect_android`
  accepts `crown` with `ro.lineage.version` set, Android 11 and our build's
  `/system/lib/libasp.so` (SHA-1 `e3816eb3…`, the NS6571 audio front end);
  it refuses another build (upstream's) naming our fork's
  `crown-ns6571-asp` branches, and a Show not on Wi-Fi. Its adb starts
  unprivileged after every boot, so the wizard sends adb's `root:` service
  and reconnects to the restarted `adbd` by USB serial without the picker,
  at connect and again at reconnect; when `adbd` refuses, the wizard names
  Developer options' **USB debugging** and **Rooted debugging**. Steps:
  `connect_android → install_boot_hook → install_em → reboot → reconnect →
  verify_service → confirm_link`. The boot hook is
  `/system/etc/init/echomuse.rc` (uid 0, groups `root audio inet`,
  `seclabel u:r:su:s0`) plus `/system/addon.d/50-echomuse.sh`, an addon.d v2
  script that keeps the `.rc` across a LineageOS update, both written with
  `/` remounted read-write and skipped when already in place. No build-pin
  warning (its pin is the `libasp.so` hash). Failure diagnostics on it
  collect `audioserver`, `wlan0`, the mute's state, the `libasp.so` SHA-1,
  the addon.d script and `ro.lineage.version`. Flashing, Developer options,
  the accepted risks (an unofficial ROM, SELinux permissive, adbd on TCP 5555
  with key authorisation) and the undo: docs/rooting.md.

### Echo Show 8 on LineageOS: the EchoMuse app

A LineageOS Show now shows what a Dot's ring shows, and takes touch in place
of the action button (docs/echo-show-companion-plan.md).

- **The firmware carries and installs the EchoMuse app.** The APK
  (`org.echomuse.companion`) is built in the controller image and embedded in
  the firmware binary, so adoption, updates, queued updates, rollback and
  auto-rollback each bring their firmware's app with no separate delivery.
  On a LineageOS Show the firmware reinstalls the app whenever the installed
  version differs from its own, allows its overlay, and restarts it after
  30 s away; an app failure never stops the firmware. The Dot and the Fire
  OS Show carry it and never install it.
- **A light bar on screen.** The ring compositor's frame is drawn as a
  12-segment bar along the bottom edge, over the kiosk: wake, listening,
  thinking, speaking, timers, alerts and link state, as on a Dot's ring.
  The app has no launcher icon, and shows a silent, persistent notification
  with the link state and the firmware and app versions.
- **Touch is the action button.** During a turn or a ringing alert the
  bottom band takes touches: a tap cancels the turn or stops the alert on the
  device, a hold of 750 ms or more is Home Assistant's `long` event. Nothing
  is touchable while idle.
- **Screen wake through the app.** With **Light the screen on wake** on, the
  app turns the screen on at candidate open itself; the kiosk API is not
  called, and the dashboard disables its URL and password there with the
  reason "the EchoMuse app lights the screen". The controller still pushes
  them, for older LineageOS firmware.
- **Capabilities.** A LineageOS Show now announces `leds`, `led_anim`,
  `buttons`, `button_hold` and the new `screen_overlay`, so the controller
  sends it LEDs, the Ring section and the button settings are enabled, and
  its touches follow the action-button policy. On LineageOS firmware from
  before the app they read "firmware update available"; the Fire OS Show
  keeps "no LED ring on this device" and "no action button on this device".
  `em_device` keeps per-platform optional sets for this.
- **App health on the Status tab.** Device `stats` carry `companion`
  (`version`, `linked`, `overlay`, `error`; docs/protocol-v1.md §4.8), and
  the Status tab shows it in a Companion app panel: an unknown value reads
  "unavailable", and a device that reports none shows no panel.
- **The wizard verifies the app.** A LineageOS Show's adoption ends with
  `verify_companion`, which waits up to 60 s for a fresh stats report with
  the app linked and its overlay allowed (read over adb while the Show is
  pending approval and sends no stats). On failure it shows the firmware's
  `[companion]` log lines and `dumpsys package org.echomuse.companion`. Undo
  adds `pm uninstall org.echomuse.companion` (docs/rooting.md).

### Echo Show 8 on LineageOS: the turn view in Kiosk Satellite

With Kiosk Satellite as the kiosk, every turn shows full screen: the request
in grey, the answer in large text as it streams
(docs/echo-show-companion-plan.md §11).

- **`turn.view` (device protocol §4.9).** The session sends a device
  announcing the new `turn_view_v1` the whole view of each turn: `listening`
  when its input starts, `thinking` with the text sent to Home Assistant (or,
  for EchoMuse's own commands, the text as heard), `answering` as Home
  Assistant streams the answer (`intent-progress` `chat_log_delta`, now the
  run event `AnswerDelta`; at most 10 sends a second, the latest text
  winning) and with the final speech at `intent-end`, and `done` once the
  reply has played. `follow_up` keeps the view up into a reply window;
  `no_input`, cancelled and failed turns end with no answer. Answers are cut
  at 16 KiB, at a word. A wake that lost arbitration shows nothing.
- **Kiosk Satellite login.** `POST /api/devices/{id}/kiosk_login {password}`
  logs in at the Echo Show's Kiosk Satellite (`http://<device ip>:<kioskPort>`,
  a ten-year token) and keeps only the token, in the device row (schema v35),
  never the password; the device JSON says only `kioskTokenSet`. A wrong
  password, a throttled login and an unreachable Kiosk Satellite each get
  their own error. The token goes in `config` (`kioskToken`, `kioskPort`) to
  a device announcing the new `kiosk_plugin`, whose firmware installs the
  plugin with it.
- **Screen settings.** `kioskPort` (default 2324) and `turnViewLinger`
  (seconds the view stays after a turn, default 6, 0–60), each disabled with
  the reason where the device lacks its capability; and the Kiosk Satellite
  token, shown as set or not set, with a login.
- **The password prompt.** The wizard's new last LineageOS step,
  `kiosk_plugin`, asks for the password when Kiosk Satellite is installed
  (read from stats, or with `pm path` while the Show is pending approval).
  The dashboard asks again before a firmware update or a queued one when it
  holds no token or Kiosk Satellite rejected it. Every prompt can be skipped,
  and each states that Remote Administration with a password must be on and
  that changing the password does not revoke the token.
- **Status.** The Companion app panel shows `stats.companion.kiosk`:
  Kiosk Satellite's version, remote admin, the plugin's version, link and
  error, and whether the firmware switched Kiosk Satellite's plugins on.
  Unknown values read "unavailable".
- **Capabilities.** A LineageOS Show on firmware from before the plugin reads
  "firmware update available" for both; a Fire OS Show "needs LineageOS on
  this Echo Show"; a Dot "no screen on this device".

### Echo Show 8 on LineageOS: timers on screen

With Kiosk Satellite as the kiosk, each running timer of the speaker is drawn
over the page as a ring that empties as time runs out, with the time left in
the middle (docs/echo-show-companion-plan.md §11.10).

- **`timer.view` (device protocol §4.10).** The alert engine sends a device
  announcing the new `timer_view_v1` the whole list of its Home Assistant
  timers (name, total, time left, paused or not) at connect, on every timer
  event and once a second while one runs. An unnamed timer is sent without a
  name, so its ring shows only the time; the dashboard still calls it
  "timer".
- **The rings.** Top right, up to four, the soonest nearest the corner; with
  more than four the last ring counts the rest as `+N`. A paused timer is
  drawn grey. They count down on the Show between messages, hide while the
  turn view is up, and clear when the controller goes quiet for 5 s or its
  session is lost.
- **Capabilities.** A LineageOS Show on older firmware reads "firmware update
  available"; a Fire OS Show "needs LineageOS on this Echo Show"; a Dot "no
  screen on this device".

### Near misses in Activity

A near miss is a wake score that reached the near-miss floor without waking
the Echo. Near misses had been only a count on the Status tab. The Activity
tab now lists each one among the turns, and records it while **Save wake
clips** is on.

- **Activity.** Each near miss shows its time, its peak score against the
  floor and the wake threshold it missed, whether the Echo was idle or
  playing, how long it lasted, and ▷/⤓ for its recording. Without a recording
  the row says why: Save wake clips off, capture not permitted, recording
  failed, or no longer kept. The **Near misses** pill hides or shows them.
  An Echo on older firmware shows *firmware update available* instead. API:
  `GET /api/devices/{id}/near_misses` and `GET
  /api/devices/{id}/near_misses/{nm}/recording`.
- **Near-miss floor is a setting again** (`nearMissThreshold`, Config →
  Wake word, fleet or per device): 0.05–0.60, default 0.17, which was the
  built-in model's value. It decides only what is listed and recorded, never
  a wake. Saving a new value reconnects the affected Echos so the new floor
  reaches them.
- **Wake models have three thresholds.** A registry entry keeps `idle`,
  `playback` and `reference`. The upload form and `POST` drop `near_miss`, and
  the validation probe must now stay below `reference`. The controller
  rewrites `registry.json` as version 2 at its first start.
- **Recordings.** A near-miss clip runs from 300 ms before the scored stretch
  to 300 ms after the last score at or above the floor, taken from the Echo's
  6 s microphone history. It is uploaded only while Save wake clips is on and
  never reaches speech recognition. The controller keeps the newest 200 clips
  per device in `nearmiss_recordings/` and the newest 2,000 near misses per
  device in the database (schema v36).
- **Device protocol.** The new capability `near_miss_v1` brings the
  `wake.near_miss` message, sent as each near miss ends, and an `uplink.open`
  reason `near_miss` (docs/protocol-v1.md §4.4, §4.5). Firmware from this
  tree announces it on all three images.

### Activity across every device

The dashboard's page has an **Activity** view beside **Devices**: every
Echo's voice turns and near misses in one list by time.

- **The list.** The newest 200 turns and 200 near misses across the Echos,
  each row naming its Echo; the name opens it. Pills above the list narrow it
  to one Echo. The tiles, recordings and turn detail are those of the device
  Activity tab. It reloads as each turn finishes and every 10 s, and the view
  is kept in the address (`#activity`).
- **Wake-score filter.** A two-thumb slider keeps the turns whose wake score,
  and the near misses whose peak, lie in its range; narrowed, it hides turns
  without a score (button, reply) and counts them. The tiles follow it.
- **Filtered downloads.** **⤓ Confirmed wakes (.zip)** holds the wake clip of
  every wake turn that was not refused (`self_output`, `echo_only`,
  `unverified_wake`, `verifier_timeout`, `arbitration_lost`), and **⤓ Near
  misses (.zip)** every saved near-miss recording, each limited to the Echo
  and score range shown, over every clip still kept rather than the rows
  listed. Each file is named as its own ⤓ in the list names it
  (`<device>-<UTC time>-wake.wav`, `<device>-nearmiss<id>.wav`), side by side,
  a repeated name numbered `(1)`; the dashboard's file-name slug now trims
  leading and trailing dashes as the controller's does, so the two always
  agree. Both are streamed, never held in memory. API: `GET
  /api/wakeclips.zip` and `GET /api/near_misses.zip` (`device`, `min`, `max`;
  404 when nothing matches).
- **Near-miss detail.** A near miss is now clickable, here and on the device
  Activity tab: its peak against the floor and the wake threshold, its
  recording, and a native AFE chart from a second before the scored stretch
  through the recording's end, with the playhead. The near-miss lease takes
  the `afe` stream for it in an `afe_metadata` session (docs/protocol-v1.md
  §4.5; firmware unchanged, it already served `afe` on any lease), so only
  near misses recorded from now on, with Save wake clips on, have one. API:
  `GET /api/devices/{id}/near_misses/{nm}/afe`.
- **API.** `GET /api/turns` and `GET /api/near_misses`: the newest rows of
  every registered device by time (`limit`, default 50, at most 1,000;
  `since`). A deleted device's rows, which the database keeps, are left out.
  The rows of `GET /api/devices/{id}/turns` and `…/near_misses` now name their
  `device_id` too.
- **Schema v37** indexes `turns` and `near_misses` by time, so a list across
  devices reads only the rows it returns. The turn lists no longer read each
  row's decision trace and AFE series (up to ~46 KB a row) only to drop them.
  **Schema v38** adds `near_misses.afe_series`, kept for the newest 1,000
  near misses per device.

### Silero VAD on the turn chart

A turn's chart now sets the controller's own VAD beside the Echo's: the
**Native VAD** lane (renamed from VAD) is the AFE metadata's DNN VAD per
80 ms period as before, and the new **Silero VAD** lane is the speech
probability the controller's Silero v5 computed for each 32 ms cell of the
lease's mic, the values its attribution and endpointing read. Hovering reads
both ("native VAD max 0.50 over 10 fr · Silero max 0.81 over 3 cells").

- **Recorded per lease.** Each lease timeline keeps the probabilities the
  speech worker reports (`em_audio_timeline.VadTrack`, bounded as the lease's
  AFE records and erased with them on mute or session loss), and the turn's
  series stores them as `silero_start` and `silero` (one entry per 512-sample
  cell) in `turns.afe_series`. No schema change; no decision reads them.
- **Where it shows.** Turns from now on, on devices with native AFE charts.
  It covers the committed lease's mic (from support −300 ms), not the
  response. Older turns, near misses (whose lease runs no Silero) and sample
  clips show no Silero lane.

### Marking near misses

A near miss can now be marked with what you heard in it, and the marked ones
download as two training sets.

- **Verdicts.** A near miss's detail asks *Was the wake word said?*:
  **Missed wake** (Y) means it was, and the Echo should have woken;
  **Correctly ignored** (N) means it was not the wake word. **Clear** (U)
  takes the mark off, and P plays or stops the recording. Only an
  administrator can mark, and only a near miss with a recording. The row
  shows the verdict where a turn shows how it ended.
- **Review.** **▶ Start review** opens the newest near miss nobody has marked
  and plays it. Marking it moves on to the next one and plays that. The
  **Near misses** pill became a row of filters: **Hide**, **All**, **To
  review**, **Missed wake** and **Correctly ignored**, each with its count.
- **Downloads.** The Activity view's archives are now **Confirmed wakes**,
  **Missed wakes**, **Correctly ignored** and **All near misses**, each still
  limited to the Echo and score range shown.
- **Retention.** A marked near miss and its recording are kept however many
  come after it, until the mark is cleared. The newest 2,000 unmarked near
  misses and 200 unmarked recordings per device are kept as before.
- **API and schema.** `PATCH /api/devices/{id}/near_misses/{nm}`
  `{"verdict": "missed_wake" | "correctly_ignored" | null}`; the near-miss
  lists carry each one's `verdict`, and `GET /api/near_misses.zip` takes
  `verdict` (archive `echomuse-missedwakes-…` or `echomuse-correctlyignored-…`).
  **Schema v39** adds `near_misses.verdict`.

### The wake model's windows over every saved clip

A saved clip runs as long as the Echo's score stayed up, plus 300 ms either
side (about 2–3.6 s), while the wake model scores one 1.4 s window, and
training keeps only a clip's first 1.4 s: when the 479 saved near misses were
re-scored, the best window started 0.48–0.96 s into eight in ten of them, and
at the start of only one. The controller now scores every saved clip once and
keeps where the model heard it.

- **Scoring.** In the background, every wake clip and near-miss recording is
  scored again with the graph that scored it on the Echo (the active one if
  that was deleted), one 1.4 s window every 160 ms, on one thread with one ORT
  thread, so it takes at most one core from the realtime path. About 70 ms a
  clip: a new clip is scored within about 10 s of being saved, and the clips
  saved before this release within a few minutes of starting it. The windows
  are kept on the clip's row (**schema v40**: `near_misses.windows`,
  `turns.wake_windows`).
- **Activity.** A near miss's detail, and a wake turn's, has a **Windows**
  stage: a bar per window as tall as its score, the best window shaded, the
  wake threshold dashed and the playhead while the clip plays, and **▶ Best
  window**, which plays that window alone. Until a clip is scored it says so.
- **Archives: Each clip, Whole or Crop**, beside them. A crop is the length
  you enter (1.4 s to start with) around the best window, or around every
  window that scored at or above the score you enter (the near-miss floor to
  start with), cut from the kept windows, so an archive takes no scoring
  unless it holds a clip the background has not reached yet. While the
  background has clips left, the row says how many. Every archive takes it,
  Confirmed wakes included.
- **Names.** `kitchen-nearmiss446-0.64s-0.41.wav`: the clip's name, where the
  crop starts in it, and the window's score; the archive
  `echomuse-missedwakes-0.00-1.00-crop1.40s[-min0.17].zip`.
- **API.** The near-miss lists carry `windows`, and the turn lists
  `wake_windows` (`graph_sha256`, `samples`, `window`, `scores` as
  `[start sample, score]`; null until scored). `GET /api/wakeclips.zip` and
  `GET /api/near_misses.zip` take `crop` (seconds, 0.1–10) and `window_min`
  (0–1); a `window_min` that no window reaches is a 404 that says so.
  `GET /api/clip_windows` gives `pending`, the clips the background has still
  to score.

### Wake verification kept with every row

A wake heard while an Echo is playing (a ringing timer, music, a reply) must
pass the verifier, a fresh Kroko decode that must hear the wake word, within
700 ms. Until now a refused one left only its reason: on 2026-10-26 three of
eight attempts to stop a ringing timer were refused as `verifier_timeout`, and
only an offline replay showed the verifier would have heard "Ophelia" in all
three.

- **The verification record.** Every row of a wake heard while playing,
  accepted or refused, now has in its decision trace what the verifier
  decoded, its distance from the wake word and its verdict; when, after the
  wake, the mic covered the verifier's input, the verdict arrived, and the
  wake was decided, against the deadline; how long the decode waited for a
  thread and how long it ran; and how much audio past the wake had arrived
  at the decision.
- **Late verdicts.** A refused candidate's row waits up to 1.5 s for a
  verifier that has not answered yet, and keeps its words and timing marked
  `late`. A verification that ran past the worker's 750 ms job deadline is
  still a timeout, but now carries the text it decoded.
- **Refused wakes have a decision trace and a wake clip.** With **Save wake
  clips** on, a refused wake heard while playing keeps a clip from 300 ms
  before the wake through the verifier's 480 ms lookahead, with or without an
  AFE chart. The **Confirmed wakes** archive still leaves refused rows out.
- **Activity.** A turn's detail has a **Verification** stage: the verifier's
  words and verdict, whether a timed-out verdict would have passed, and where
  the time went.

### A turn no longer hangs when late results end and commit it at once

When the speech worker fell behind, its late results could end a command and
commit it in the same step. The controller acted on the "end pending" part
but never on the commit that followed, so the command was never sent, and the
turn sat with the mic open until something else ended it. That is what
happened to "set a three minute timer" on 2026-10-26. The controller now acts
on every step in such a batch, so the command commits and runs as usual.

### A Dot keeps its AFE chart after a power cut

An Echo Dot that was unplugged and powered back on could reach the controller
before its microphone started, because the Dot opens its microphone only
after its first NTP sync. Its session then lacked native AFE metadata, so its
turns had no AFE chart and kept no turn recordings until the next reconnect.
Now, once the AFE decoder starts working, the Dot waits until it is idle and
reconnects once, and the new session carries the metadata. Its light ring may
show the dim red "link down" pulse for a moment while it reconnects.

### More wake clips and near-miss recordings kept

Each Echo now keeps its newest 1,000 wake clips (was 500) and its newest
1,000 unmarked near-miss recordings (was 200). The two are counted
separately, as before, and a marked near miss still keeps its recording
until the mark is cleared. A busy room filled the 200 near-miss recordings
in under a week; 1,000 holds about a month of them. At the full count the
two take about 46 MB and 60 MB per Echo.

### "Stop" heard by another Echo stops the ring

On 2026-10-26 the Kitchen Echo rang a timer and the user said "Ophelia,
stop". The idle Living Room Echo heard it too and took the utterance, though
the Kitchen's wake reached the controller about 300 ms sooner: a ringing Echo
skipped arbitration, and its wake waits for the verifier while an idle one is
accepted at once. The Living Room sent "stop" to Home Assistant as a media
pause, Home Assistant answered "Okay.", and the Kitchen rang on (turns 934 and
935).

- **A ringing Echo takes part in arbitration.** Its wake claims like any other
  and, if another Echo already claimed, closes as `arbitration_lost` with no
  turn and no chime. Its ring comes back up until the winner stops it.
- **An Echo with nothing of its own to stop acts on the one other ring.** When
  a wake has no alert, reply or content of its own and exactly one other Echo
  is ringing, "stop", "cancel" or "snooze" stops or snoozes that ring through
  the alert engine, without Home Assistant. With two or more other Echos
  ringing, the words go to Home Assistant as before. The turn's decision trace
  names the other Echo in `context_device`.

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
