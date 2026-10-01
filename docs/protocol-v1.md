# EchoMuse device protocol v1 — wire reference

This is the exact wire contract between the Dot firmware (`device/`) and the
controller (`controller/`). It implements
[post-afe-audio-architecture.md](post-afe-audio-architecture.md) §16.1; where
that document names a field in prose, this file fixes its JSON spelling. Both
sides MUST implement exactly these names. Unknown message types and unknown
fields are ignored in both directions.

## 1. Sockets

All three sockets are WebSockets on the controller's device listener (plain
`SERVER_PORT` or TLS `SERVER_TLS_PORT`, chosen exactly as the legacy link did).
Every upgrade carries `X-EM-Token: <per-device token>` when the device holds one
(`/data/local/etc/echomuse/token`); the controller applies the existing
`em_linkauth` policy (`REQUIRE_DEVICE_TLS`). Compression is disabled.

| Path | Direction | Frames |
|---|---|---|
| `/device/v1/control` | both | UTF-8 JSON text frames, ≤256 KiB, one envelope each |
| `/device/v1/audio` | both | binary EMA1 frames (§3); upgrade header `X-EM-Session` |
| `/device/v1/assets` | both | JSON text requests/replies + binary chunks (§6); upgrade header `X-EM-Session` |

The device opens control first, sends `session.hello` with `session_id: null`,
and waits ≤10 s for `session.ready` or `session.rejected`. Only after
`session.ready` does it open audio and assets. The controller rejects (HTTP 403)
an audio/assets upgrade whose `X-EM-Session` is not the device's current session.
Closing the control socket ends the session; the controller closes the other
two.

The `/shell/{device_id}` plane is unchanged. The legacy `/control` path is
served only by the controller's upgrade-only legacy handler (architecture §12).

## 2. Control envelope

```json
{"protocol":1,"type":"focus.acquire","session_id":"uuid|null","message_id":"uuid",
 "device_id":"<ro.serialno>","generation":17,"body":{}}
```

- `message_id`: a fresh UUIDv4 per message; it is the command ID.
- `generation`: the owner generation of the object the message acts on (lease,
  playback, candidate lease). `0` when the message has no generation.
- uint64 values (sample indices, epochs, monotonic ns, UTC ms) are JSON
  **decimal strings**. Small counters are JSON numbers.
- Epoch values are random nonzero uint64s, written as decimal strings.

### Liveness

Each side sends `heartbeat` every 1 s; any received control message counts as
liveness. 2 s of silence = degraded (dashboard only). 3 s = session lost: both
sides close all three sockets and end every dialog/focus/uplink lease. The
device keeps executing alerts.

## 3. EMA1 audio frame

64-byte little-endian header, then payload. Exactly the table in architecture
§16.1:

| Off | Type | Field |
|---:|---|---|
| 0 | 4 bytes | `EMA1` |
| 4 | u8 | kind: 1 mic, 2 reference, 3 render source, 4 cells |
| 5 | u8 | flags: bit0 discontinuity, bit1 muted, bit2 underrun, bit3 estimated timing, bit4 digital silence (kind 2 only; payload omitted) |
| 6 | u8 | channels = 1 |
| 7 | u8 | format: 1 PCM16LE (kinds 1–3), 2 cell record v1 (kind 4) |
| 8 | u64 | stream epoch |
| 16 | u64 | sequence (0-based per epoch, +1 per packet) |
| 24 | u64 | first sample-frame index in epoch (kind 4: first cell's start **sample**, a multiple of 512) |
| 32 | u64 | first-sample device CLOCK_MONOTONIC ns (0 for downlink) |
| 40 | u32 | timing uncertainty µs (`0xffffffff` unknown) |
| 44 | u32 | sample rate: 16000 kinds 1,2,4; 48000 kind 3 |
| 48 | u32 | frame count: kinds 1–2 1…1280 samples; kind 3 1…3840; kind 4 1…320 cells |
| 52 | u32 | render generation (kind 3); 0 otherwise |
| 56 | u32 | active-source mask (kind 2): content=1 alert=2 dialog=4 earcon=8; 0 otherwise |
| 60 | u32 | payload bytes: frames×2 (PCM), 0 (digital silence), cells×4 (kind 4) |

Uplink (device → controller): kinds 1, 2, 4, only under a lease (§5). Downlink
(controller → device): kind 3 only.

Cell record (kind 4, 4 bytes): int16 LE `E` in hundredths of dB clamped to
[−12000, 0]; u8 flags (bit0 gap, bit1 muted); u8 active-source mask. Cell `k`
covers mic samples `[512k, 512k+512)` of the capture epoch; the header epoch is
the mic capture epoch. A gap cell has `E = −12000` and the gap flag.

Reference stream: its own epoch (restarted with each render epoch); reference
sample `k` = FIR output aligned with render sample `3k` (architecture §16.1).

Validation (receiver): reject unknown epoch, wrong rate/format/channels,
payload-length mismatch, decreasing sequence or first sample within an epoch;
report `protocol.error` and close the audio socket. A gap advances indices; it
is never filled.

## 4. Message catalogue

`D→C` device to controller, `C→D` controller to device. `gen` = envelope
`generation`.

### 4.1 Session

**`session.hello`** D→C, `session_id: null`
```json
{"capabilities":["audio_timeline_v1","uplink_leases_v1","device_wake_v1",
  "render_reference_v1","render_progress_v1","focus_leases_v1","alert_cache_v1",
  "turn_protocol_v1","leds","led_anim","buttons","button_hold","alert_prefetch","local_wake_chime",
  "open_rules_v1","ambient_light"],
 "firmware_version":"v3.0.0","boot_id":"<proc boot_id>","protocols":[1],
 "ip":"10.0.0.5","ambient_light_status":{},
 "privacy":{"muted":false,"capture_epoch":"123"},
 "clock":{"trusted":true,"mono_ns":"…","utc_ms":"…"},
 "alerts":{"delivery_epoch":"str|null","acked_sequence":0,"wakeup":"ok|alarm_wakeup_unavailable"},
 "assets":["<sha256>",…],
 "volume":{"level":120,"seeded":false}}
```
`alert_cache_v1` is withheld when the wakelock cannot be acquired
(`alerts.wakeup = "alarm_wakeup_unavailable"`). `ambient_light` only when the
sensor is readable. `ambient_light_status` is the legacy `als.Report()` object.
`local_wake_chime`: the device plays the wake chime itself at candidate open
when `config` `wakeSound` is true (§4.4).
`open_rules_v1`: the device opens candidates on `session.ready`
`detector.open_rules`, evaluates `detector.shadow_rules` without acting on
them, names the opening rule in `wake.candidate.rule` and reports shadow
counters in `wake.stats.shadow` (§4.4; architecture §5.2).

**`session.ready`** C→D
```json
{"protocol":1,"session_id":"uuid","server_boot_id":"uuid","capture_permitted":true,
 "assets":{"runtime_sha256":"174233cf…","graph_sha256":"4eb74512…","sidecar_sha256":"25da0c65…"},
 "detector":{"thresholds":{"idle":0.90,"playback":0.65,"near_miss":0.17},
   "hop_blocks":2,"smoothing":3,"clear_after_unscored":6,
   "open_rules":[{"profile":"idle","windows":3,"combine":"mean","threshold":0.90},
                 {"profile":"playback","windows":3,"combine":"mean","threshold":0.65}],
   "shadow_rules":[{"profile":"idle","windows":2,"combine":"mean","threshold":0.95}],
   "provisional_duck":{"duck_db":-18.0,"max_per_window":2,"window_ms":5000}},
 "utc_ms":"…"}
```
`open_rules` and `shadow_rules` are sent only to a device announcing
`open_rules_v1`. A rule is `{profile: "idle"|"playback", windows: 1–3,
combine: "mean"|"all", threshold: (0, 1]}`: at a scored hop of its profile it
fires when, over the last `min(windows, n)` raw scores of the scored history
(`n` < `windows` only right after a reset), their mean (`mean`) or every one
(`all`) is ≥ `threshold`. `open_rules` is evaluated in list order; the
controller sends the two baseline rules `{idle,3,mean,thresholds.idle}`,
`{playback,3,mean,thresholds.playback}` first, then the configured extras
(`wakeOpenRules`). Absent `open_rules` (older controller) = those two baseline
rules derived from `thresholds`; absent `shadow_rules` = none. The device
validates both lists — at most 6 live and 8 shadow rules, every field valid, at
least one live rule per profile, an explicit empty `open_rules` invalid — and an
invalid policy leaves the detector unavailable (`wake.stats`
`wake_unavailable: "load_failed"` with the error as `detail`), like an
unimplemented `hop_blocks`/`smoothing`. `thresholds.near_miss`, `smoothing`,
`hop_blocks` and `clear_after_unscored` keep their meaning: the 3-window
smoothed value still drives candidate extension and close, near misses, peaks
and `hops` records. A changed policy on an unchanged graph is applied between
candidates (deferred while one is open, latest wins); the same policy again is
a no-op.

`capture_permitted: false` = the device keeps capturing and scoring locally but
the controller refuses every candidate (diagnostics/approval states).

**`session.rejected`** C→D then close: `{"reason":"pending_approval|unauthorized|protocol"}`.
The device retries after 10 s (white pulse while `pending_approval`).

**`heartbeat`** both: `{"mono_ns":"…"}` (sender's monotonic clock).

**`clock.request`** D→C `{"nonce":"uuid"}` → **`clock.reply`** C→D
`{"nonce":"uuid","utc_ms":"…"}`. The device measures round trip on its
monotonic clock; it trusts UTC after two replies with RTT ≤500 ms whose offset
estimates agree within 1 s (architecture §16.5). It requests every 1 s until
trusted, then every 10 min.

**`command.ack`** both:
`{"message_id":"<acked message's id>","status":"accepted|applied|durable|rejected","error":"code|null"}`.
The device acks every C→D `focus.*`, `render.*`, `uplink.*`, `alert.act`,
`alert.ring`, `alert.prefetch`: `accepted` on receipt, `rejected` with a code on refusal; `alert.act`
on an alarm acks `durable` once journaled. The controller acks `wake.candidate`
(`accepted`, which releases the candidate lease's audio, or `rejected`).

**`protocol.error`** both: `{"code":"…","detail":"…"}`.

### 4.2 Streams and render

**`stream.open`** D→C (uplink epochs): `{"stream_id":"mic|reference|cells","epoch":"…","kind":1,"sample_rate":16000,"format":1,"reason":"start|discontinuity|privacy|clock_reset|render_epoch"}`.
Sent when an epoch starts and, for current epochs, right after `session.ready`.
**`stream.end`** D→C: `{"stream_id":"…","epoch":"…","final_sample":"…","reason":"…"}`.

**`render.start`** C→D, gen = playback generation:
```json
{"playback_id":"uuid","source_class":"content|dialog_output|earcon|alert_preview",
 "epoch":"<kind-3 epoch, absent for local sources>","gain_db":0.0,"format":1,
 "local_asset":"builtin:wake_chime|<sha256>|builtin:fallback|null",
 "announcement":false}
```
Network sources (`content`, `dialog_output`) receive kind-3 packets on the
audio socket with this epoch and generation; the device FIFO holds 128 writes
and primes 24 before starting. Local sources (`earcon`, `alert_preview`) play
`local_asset` immediately, no prime.

**`render.end`** C→D `{"playback_id":"…","end_frame":"<u64>"}`: no audio
follows `end_frame`, the source frame after the last one sent. The audio
before it may still be in flight on the audio socket, so the device keeps
accepting contiguous packets up to `end_frame`, drains once all of it has
arrived, and reports `render.finished` `drained`; a packet past `end_frame` is
dropped. `end_frame` absent or null: the audio already received is all there is.

**`render.cancel`** C→D, gen = playback generation:
`{"playback_id":"…","reason":"…"}`. Idempotent. Discards FIFO, reports
`render.finished` `cancelled`.

**`render.progress`** D→C every 80 ms while playing and on every event, gen =
playback generation:
```json
{"playback_id":"…","event":"progress|start|flush|seek|pause|resume|underrun|gain",
 "submitted_frames":"…","completed_frames":"…","mono_ns":"…","uncertainty_us":42700,
 "timing_quality":"estimated","reference_coverage":"full|partial",
 "seek_frame":"…","missing_from":"…","missing_to":"…","gain_db":-18.0}
```
Per-event fields present only for that event.

**`render.finished`** D→C, gen = playback generation:
`{"playback_id":"…","last_completed_frame":"…","reason":"drained|cancelled|failed|underrun","timing_quality":"estimated"}`.
`drained` is declared 150 ms after the last buffer completion.

`render.progress` and `render.finished` report only playbacks the controller
started. A device-originated local playback (the `local_wake_chime` chime) is
mixed like any other, reference included, but never reported.

### 4.3 Focus

**`focus.acquire`** C→D, gen = owner generation:
`{"lease_id":"uuid","owner":"turn-uuid","focus":"dialog_input|dialog_output","ttl_ms":3000}`.
**`focus.renew`** `{"lease_id":"…","ttl_ms":3000}`. **`focus.release`** `{"lease_id":"…"}`.

Device policy (architecture §6.2, §16.2): any live dialog lease ducks every
content playback by `duckDb` (30 ms ramp) and backgrounds the foreground alert
(stops its burst, keeps it pending) under the 15 s occurrence background cap. On
expiry the device cancels dialog output owned by that lease's owner, restores
content, and foregrounds the alert. Content focus is implicit: a content
playback plays unless ducked or paused by an alert foreground.

### 4.4 Wake

**`wake.candidate`** D→C, gen = 1 (the candidate lease's generation):
```json
{"candidate_id":"uuid","lease_id":"uuid","capture_epoch":"…","graph_sha256":"…",
 "scorer_revision":3,"profile":"idle|playback","threshold":0.9,
 "producing_sound":false,"chimed":false,"first_crossing_end":"…","support_start":"…",
 "mono_ns":"…",
 "active_alert":{"id":"…","kind":"alarm|timer","name":"…","foreground":true},
 "hops":[{"end_sample":"…","raw":0.93,"smoothed":0.91,"profile":"idle"}],
 "rule":{"profile":"idle","windows":3,"combine":"mean","threshold":0.9}}
```
`rule` (`open_rules_v1` only) is the live rule that opened the candidate — the
first rule of `detector.open_rules`, in list order, to fire at the opening hop
— and `threshold` is its threshold, latched for the candidate. `support_start`
is the end of the oldest window the rule used minus 22,400 (one window), so a
2-window rule opening one hop before the baseline reports the same
`support_start` the baseline would have. Absent (older firmware) = the
candidate opened on the baseline rule of its `profile`.
`hops` holds one record per hop slot from the slot whose window starts at
`support_start` through the opening hop; `raw`/`smoothed` null for unscored slots.
`active_alert` is null when nothing is ringing or backgrounded.
`chimed` is true iff the device started the built-in wake chime for this
candidate. It does so before sending `wake.candidate`, only with `config`
`wakeSound` true, a session, `producing_sound` false, `active_alert` null and
no live `diagnostic` lease, as an `earcon` at the slot's current generation
(the fence is not raised; the controller's next earcon `render.start` replaces
it). The chime is not cancelled if the candidate is rejected. The controller
plays the chime on acceptance only when `chimed` is false; absent (older
firmware) means false.

**`wake.candidate_end`** D→C: `{"candidate_id":"…","support_end":"…","peak_smoothed":0.95,"reason":"below|gap|reset|mute|overrun"}`.

**`wake.stats`** D→C every 30 s and immediately when `wake_unavailable` changes:
```json
{"window_ms":30000,"hops_scored":187,"hops_dropped":0,"inference_errors":0,
 "infer_mean_ms":82.1,"infer_max_ms":131.0,
 "near_misses":[{"mono_ns":"…","peak":0.31}],"candidates_opened":1,
 "peak_smoothed":0.93,"graph_sha256":"…",
 "wake_unavailable":null,"detail":null,
 "shadow":[{"rule":{"profile":"idle","windows":2,"combine":"mean","threshold":0.95},
   "hops":187,"opens":1,"matched":1,"lead_hist":[0,0,0,0,1,0,0],
   "unmatched":0,"retried":0,"live_only":0,
   "events":[{"kind":"unmatched|retried","open_sample":"…","mono_ns":"…",
              "peak_raw":0.97,"raws":[0.41,0.93,1.0]}],
   "events_dropped":0}]}
```
`wake_unavailable`: `null|"missing_asset"|"load_failed"|"inference_errors"`.

`shadow` (`open_rules_v1` only; `[]` with no shadow rules) has one entry per
`detector.shadow_rules` rule, in that order, with counters for this stats
window only. A shadow **episode** opens when its rule fires on a hop of its
profile and none is open, extends while the 3-window smoothed value is ≥ 50 %
of the rule's threshold, and closes on the second consecutive scored hop below
it; an invalid window, gap, overrun, reset, mute or model/policy change
abandons it uncounted. `hops`: scored hops of the rule's profile. `opens`:
episodes opened. `matched`: episodes during which a live candidate was open at
some scored hop; each adds one to `lead_hist[lead+3]`, `lead` = (live
`first_crossing_end` − shadow open sample) / 2560 clamped to −3..+3 (positive:
the shadow rule would have opened earlier). An episode no live candidate
overlapped is held until 160,000 capture samples after its open: a live
candidate opening in that time resolves it `retried` (likely a real wake the
live rules missed and the user repeated), otherwise — or on a capture-epoch
change — `unmatched` (a would-be false wake); held episodes carry across
windows. `live_only`: live candidates of the rule's profile that closed with
no episode of the rule overlapping them (real wakes the rule would have
missed). Each resolution emits one `events` entry, at most 16 per rule per
window, the rest counted in `events_dropped`; `open_sample` (capture-epoch
sample at the opening hop's end) and `mono_ns` are uint64 decimal strings,
`raws` the last ≤3 raw scores at the open, `peak_raw` the highest raw score
over the episode. `shadow` absent (older firmware) means no shadow data, never
zero counts.

### 4.5 Uplink leases

Stream keys: `mic`, `reference`, `cells`. Starts are **capture-epoch sample
indices** (decimal strings) or `"live"`; the device maps a reference start to
the reference epoch through its clock fits, then rounds down (mic/cells to
512, reference to 2560) and clips to the ring.

**`uplink.open`** C→D, gen = lease generation (starts at 1):
`{"lease_id":"uuid","owner":"…","reason":"turn|reply|diagnostic","streams":{"mic":"…|live","reference":"…|live","cells":"…|live"},"ttl_ms":3000}`.
An omitted stream key is not wanted.

**`uplink.renew`** C→D, gen = the lease's generation, or +1 to convert:
`{"lease_id":"…","ttl_ms":3000,"reason":"turn","owner":"turn-uuid"}` (`reason`/`owner`
only when converting a candidate lease into a turn lease).

**`uplink.close`** C→D, gen = lease generation:
`{"lease_id":"…","reason":"rejected|arbitration_lost|committed|closed"}`.
Closing a candidate lease releases its provisional duck.

**`uplink.ended`** D→C:
`{"lease_id":"…","reason":"closed|ttl|mute|epoch|overrun|session","last_sample":{"mic":"…|null"},"clipped_start":{"mic":"…|null"}}`.
One key per wanted stream. `last_sample` is the last index sent (inclusive;
null when nothing was sent); `clipped_start` is the backfill start after
clipping to the ring, in that stream's header index domain, or null when unclipped.

A candidate lease (opened by the device with `wake.candidate`, generation 1)
wants mic from `support_start − 4800`, cells from mic start − 160000, and
reference from mic start − 40000 (capture samples; a silent mix costs only
payload-less digital-silence packets).
The device uploads nothing for it until the controller's `command.ack`
`accepted` for the `wake.candidate`; no ack within 1 s ends it with `ttl`.

### 4.6 Physical events

**`privacy.changed`** D→C: `{"muted":true,"capture_epoch":"…|null","physical_seq":12}`.

**`button.action`** D→C:
```json
{"click_type":138,"button":"Dot","down":false,"held_ms":120,"muted":false,
 "mono_ns":"…","capture_epoch":"…|null","capture_sample":"…|null",
 "physical_seq":13,"occurrence_id":"…|null","handled":"alert_stopped|null"}
```
A dot-button tap release while an alert is ringing or backgrounded stops it on
the device (alarm: journaled `dismiss`; timer: stopped) and reports
`handled:"alert_stopped"` with its `occurrence_id`. Otherwise `handled` is null
and the controller applies its gesture policy. Volume and mute buttons report
the same message with their `click_type` (115, 114, 113) and are handled locally
as before.

### 4.7 Alerts

**`alert.snapshot`** C→D (paged ≤128 objects/page):
`{"delivery_epoch":"…","high_water_mark":42,"page_index":0,"page_count":2,"sha256":"…","objects":[…]}`.
**`alert.delta`** C→D: `{"delivery_epoch":"…","sequence":43,"objects":[…]}`.
Objects are the canonical occurrence / tombstone objects of architecture §16.4.
`sha256` is over the canonical JSON array (sorted keys, no whitespace, UTF-8) of
all pages' objects concatenated in order.

**`alert.ack`** D→C: `{"delivery_epoch":"…","applied_through":43,"durable":true,"need_snapshot":false}`.

**`alert.local_operation`** D→C:
```json
{"op_id":"uuid","action":"dismiss|snooze|expire","occurrence_id":"…",
 "schedule_id":"…","revision":17,"reason":"timed_out|missed|null",
 "source":"button|voice|entity|llm|dashboard|executor",
 "child":{"schedule_id":"…","occurrence_id":"…","due_utc_ms":"…","due_local":"…"}}
```
`child` only for `snooze`. Re-sent (same `op_id`) at every session start until
the controller answers **`alert.op_result`** C→D
`{"op_id":"…","state":"applied|rejected","error":"…|null"}`, after which the
device may drop the operation once the occurrence is no longer delivered.

**`alert.act`** C→D: `{"op_id":"uuid","target_id":"<occurrence or ring id>","action":"dismiss|snooze","source":"voice|entity|llm|dashboard"}`.
`op_id` is any canonical lowercase RFC 4122 UUID: the controller sends UUIDv5
for voice commands and LLM tool calls (architecture §16.7) and UUIDv4 otherwise.
Alarms go through the journaled local-operation path (`command.ack durable`,
then an `alert.local_operation` with the same `op_id`); timer rings stop
(`command.ack applied`).

**`alert.ring`** C→D (timers only, not persisted):
`{"ring_id":"<HA timer id>","name":"pasta","sound":"<sha256>|builtin:fallback","loop_gap_ms":2000,"max_ring_ms":900000}`.
A sound the device has not installed rings as the fallback; the device fetches it
in the background for later rings.

**`alert.prefetch`** C→D, only to a device announcing `alert_prefetch`:
`{"sounds":["<sha256>",…]}`. The device installs each missing sound from
`/device/v1/assets` in the background; `rejected` `invalid` when any entry is not
a SHA-256. The controller sends the effective timer sound after `session.hello`
and whenever an HA timer starts, so a timer rings its configured sound rather
than the fallback.

**`alert.ring_ended`** D→C: `{"id":"…","kind":"timer|alarm","reason":"stopped|button|entity|limit|preempted|restart"}`.

**`alert.state`** D→C on every change and after `session.ready`:
```json
{"active":{"id":"…","kind":"alarm|timer","name":"Wake up","foreground":true,
  "started_mono_ns":"…","deadline_mono_ns":"…"}|null,
 "queue":["…"],"clock_trusted":true,"wakelock":"held|released|unavailable",
 "store":"ok|corrupt"}
```

### 4.8 Retained messages

Carried in the envelope with the legacy JSON object (minus `type`) as `body`,
fields unchanged:

- C→D: `leds`, `led_anim`, `volume_set`, `config`, `shell_open`, `shell_close`,
  `wifi_change`, `wifi_commit`, `wifi_scan`, `ping`.
- D→C: `volume_state`, `ambient_light`, `log`, `wifi_result`,
  `wifi_scan_result`, `ble_adverts`, `stats`, `pong`.

`config` keys after cutover: `startupVolume`, `duckDb`, `bleProxyEnabled`, and
`wakeSound` (JSON boolean) only to a device announcing `local_wake_chime`.

## 5. Transport rules

- Audio for a lease is sent only while that lease is open; backfill precedes
  live audio per stream, in the order mic, cells, reference.
- Per stream, the device keeps ≤400 ms of queued live audio beyond backfill;
  live audio older than 1,000 ms in the send queue ends every lease wanting that
  stream with `overrun`.
- A reference packet whose samples are all zero is sent with flag bit4 and no
  payload.
- The controller keeps the first copy of each sample index per epoch and drops
  packets for a stream no active lease wants.
- Session loss discards queued audio; nothing is sent on the next session.

## 6. Assets socket

One request in flight. Request `{"sha256":"<hex>","offset":0}` → binary chunks
≤64 KiB from `offset`, then `{"sha256":"<hex>","size":N,"done":true}`, or
`{"error":"not_found"}`. A second request before the answer ends closes the
socket. The device writes `<sha256>.part`, resumes by offset, verifies SHA-256,
fsyncs, renames atomically. Speech assets go to
`/data/local/share/echomuse/speech/<sha256>.{so,onnx,json}`; alert sounds to
`/data/local/etc/echomuse/alerts/assets/<sha256>.wav`.
