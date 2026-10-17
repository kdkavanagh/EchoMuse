# Echo Show 8 on LineageOS: companion app plan

**Status:** proposed 2026-10-17; Phases C1–C5 implemented 2026-10-17, ahead of C0. C0 and C6 need the unit and are open (§10).
**Evidence:** [echo-show-crown.md](echo-show-crown.md) (written "findings §N") and the code cited inline. The platform this extends is [echo-show-lineage-plan.md](echo-show-lineage-plan.md), written "LineageOS plan".
**Goal:** a LineageOS Show shows what the Dot's ring shows (wake, listening, thinking, speaking, timers, link state, alerts) as a light bar on screen. Touch takes the place of the action button. The app is installed by adoption and kept current by every firmware update, rollback and auto-rollback, with no separate delivery path.

**Not in scope:**
- Fire OS crown. It keeps Fully's REST screen wake (`internal/kiosk`) and gets no app.
- Volume through `AudioManager`, a Bluetooth proxy through `BluetoothLeScanner`, captions and named timer countdowns. These are later candidates that build on this link (§8).
- Mute. It stays LineageOS plan scope: `privacy_mute` is not announced.
- Tap-to-talk while idle (open question 3).
- The camera.

## 1. Starting point

| Area | Today on `lineage18_crown` | Source |
|---|---|---|
| Lights | none; `/sys/class/leds` has only `lcd-backlight` | findings §5 |
| Ring compositor | built but unattached: `server.New` always makes a ring, its `led.Controller` stays nil, and `paintLocked` returns early. Controller, link and alert layers are computed and dropped | `internal/server/server.go:65-96`, `ring.go:202-218` |
| Controller LEDs | not sent: `Device._led_loop` skips devices without `leds` | `controller/em_device.py:917-950` |
| Action button | none; capability gap `NO_BUTTON` | `controller/em_device.py:113-126` |
| Screen wake | Fully's REST `screenOn` with the user's Remote Administration password, at every candidate open | `internal/kiosk`, `supervisor/wake.go` |
| Link state on device | `LinkDown` (connecting, offline, rejected), `LinkPending` (`pending_approval`), `LinkUp` | `supervisor/session.go:100-177` |
| Firmware delivery | wizard `install_em` over WebUSB; OTA over the shell plane into A/B slots, then `kill $PPID`; `start_server.sh` flips slots after three exits under 15 s | `controller/em_api.py:2836-3066`, `start_server.sh:521-590` |
| Firmware version | `fw-` + hash of every file under `device/{go.mod,go.sum,cmd,internal,pkg}` | `controller/Dockerfile:65-74` |
| SELinux, firmware domain | permissive; root in `u:r:su:s0`, groups `root audio inet` | findings §8.5, LineageOS plan decision 9 |
| Screen | ILI9881C LCD, 800×1280 rotated 90° | findings §5 |

**Kiosk.** The unit's kiosk is Kiosk Satellite (`me.jxl.kiosk_satellite`, https://kiosksatellite.com); its wake word and Voice Satellite are not used. EchoMuse does not deny the kiosk's microphone. The condition: as long as nothing in the kiosk records, the mic-sharing hazard of findings §8.4 (an app's `MIC` capture opens the input first and pins EchoMuse's `VOICE_RECOGNITION` to PipelineVoice) does not arise. If the kiosk records, EchoMuse joins its input and loses the ASR output and the bit-0 metadata until that input idles. In Kiosk Satellite that means Voice Satellite, clap gestures or intercom.

## 2. Decisions

1. **The APK ships inside the firmware binary.** `internal/companion` embeds `app.apk` and `app.version`, and the firmware installs the app itself. Adoption, OTA, queued updates, manual rollback and `start_server.sh`'s auto-rollback all carry the app with no change, because each delivers a firmware binary and each binary installs its own app. The firmware and the app can never run versions that disagree about the link protocol for longer than one install. The Dot carries the APK too (one binary for all platforms) and never installs it.
2. **One version.** `app.version` is `cmp-` + the hash of `device/companion/**`, and it is the APK's `versionName`. The firmware hash gains `device/companion` and excludes the built `internal/companion/app.*`, so the firmware version moves exactly when the app's sources move.
3. **Signing key generated per image build.** The `companion` stage creates a key with `keytool` and signs with it. No secret is kept in the repository. Binary bytes are already build-specific (`BuildUnix`), so nothing relies on reproducible bytes.
4. **Install by version, always fresh.** When the installed `versionName` differs from `app.version`, the firmware uninstalls and then installs. This one path covers a key change, a downgrade on rollback and a missing app. The app keeps no state of its own. Every grant is applied again on every firmware start.
5. **The firmware owns the app's life.** At start, and whenever the link has been down for 30 s, the firmware runs the install check, `appops set … SYSTEM_ALERT_WINDOW allow`, and `am start-foreground-service`. It does this asynchronously and rate-limited. The voice path never waits on it, and no failure exits the server: three fast exits would trigger `start_server.sh`'s slot flip. The app has no boot receiver.
6. **Loopback TCP, firmware listens.** The firmware binds `127.0.0.1:8771` and the app connects to it, holding `INTERNET`. The firmware accepts only the app's uid: the connection's row must carry the owner uid of `/data/data/<pkg>`. The row is looked up in `/proc/net/tcp` and `/proc/net/tcp6`, because Android's Java sockets may be dual-stack, which puts the app's end at `::ffff:127.0.0.1` (C0 item 5 confirms which). One connection at a time; a new one replaces the old. §9 covers why not Unix sockets or `am broadcast`.
7. **The app is a display and a touch surface, nothing else.** The ring compositor's painted frame, after every layer and overlay, is the only display input. A `led.Controller` implemented by `internal/companion` is attached to the ring, as `InitLEDs` attaches the I²C controller on the Dot. Wake, turn, alert and link logic stay in the firmware and the controller.
8. **Capabilities.** `lineage18_crown` adds `leds`, `led_anim`, `buttons`, `button_hold` and a new `screen_overlay`, all announced statically. Whether the app is up right now is health, not a capability (decision 11), because hello cannot change during a session.
9. **Touch is the action button.** A press in an active zone reaches the existing action-button path (`Supervisor.DotButton`): `button.action` on both edges, with `held_ms` measured by the app from its `MotionEvent` times. A tap during a foreground alert stops it on the device (`handled: alert_stopped`). A tap during a turn cancels it. A hold of 750 ms or more is HA's `long` event. Zones are set by the firmware:

   | Zone | When | Touchable area |
   |---|---|---|
   | `none` | idle, including a timer's progress frame | none: the bar ignores touches |
   | `cancel` | a dialog focus lease is held | the bottom 72 dp band |
   | `stop` | an alert holds foreground focus | the bottom 72 dp band, labelled "Stop" |
10. **Screen wake moves to the app.** At candidate open, with `screenWake` on, the firmware sends `screen wake`. The app turns the screen on with an `ACQUIRE_CAUSES_WAKEUP` wake lock and holds it while the zone is not `none`. Afterwards the kiosk's own timeout applies again. The bar draws over the kiosk's screensaver: EchoMuse does not dismiss the screensaver on wake, makes no kiosk API call and keeps no kiosk credentials on the device. `screenWakeUrl` and `screenWakePassword` are unused on `screen_overlay` devices. The dashboard disables them with the reason "the EchoMuse app lights the screen". The controller keeps pushing them to every `screen_wake` device, which older LineageOS firmware still needs.
11. **Health in `stats`.** `proto.Stats` gains `companion: {version, linked, overlay, error}`, each null when unknown. The controller adds `companion` to `STATS_KEYS` (`em_device.py:58-65`) and the dashboard's Status panel shows it. A device without the field shows nothing, not "off".
12. **No volume arc on the bar.** Android's own volume panel is the Show's volume UI, so the ring's external-change arc (`server/volume.go:154,161`) is suppressed when the Android hardware drives volume.
13. **Notification:** the foreground service's notification, on a silent channel at the lowest importance this build accepts (Phase C0). Contents: the link state line, the firmware and app versions. No actions: mute and alert controls stay off it.
14. **No Activity, ever.** The app declares and starts only its foreground service, through `am start-foreground-service`; there is no launcher, settings or permission activity. The unit's kiosk, Kiosk Satellite (`me.jxl.kiosk_satellite`), pulls itself back to the front about 1 s after losing it in Kiosk Mode (its `docs/kiosk.md`, "Staying in Front"), so any activity of ours would fight it. The overlay permission comes from the firmware's `appops` grant (decision 5), never from a Settings intent.

## 3. Local link (version 1)

Newline-delimited JSON, one object per line, at most 4 KiB, field `t` for the type. The app reconnects with a 0.5–5 s backoff.

| Direction | `t` | Body | When |
|---|---|---|---|
| app → fw | `hello` | `link: 1`, `companion: "cmp-…"`, `overlay: bool`, `screen_on: bool` | on connect |
| fw → app | `hello` | `link: 1`, `firmware: "fw-…"`, `companion: "cmp-…"` (embedded) | reply; on a `link` mismatch both sides close and the firmware reinstalls |
| fw → app | `frame` | `rgb`: 72 hex chars, 12 × RRGGBB, index 0 at the left | every `paintLocked`, at most 25/s (pulse and meter tick at 40 ms), and the last frame after `hello` |
| fw → app | `status` | `link: "up" \| "down" \| "pending"`, `reason`, `device_id`, `controller` (host:port or null) | on every link transition, and after `hello` |
| fw → app | `touch` | `zone: "none" \| "cancel" \| "stop"` | on change, and after `hello` |
| fw → app | `screen` | `wake: true` or `hold: bool` | candidate open with `screenWake` on; zone changes |
| app → fw | `press` | `down: true`, or `down: false` with `held_ms` | touch in an active zone |
| app → fw | `state` | `overlay: bool`, `screen_on: bool` | on change; `overlay: false` makes the firmware re-apply the appop |

The app formats the status text and the firmware sends only state, so the overlay text and the notification can never disagree. "Waiting for approval · G6G16D12143203LD" is `link: pending` plus `device_id`.

## 4. Build

- `device/companion/`: `AndroidManifest.xml` (package `org.echomuse.companion`, min and target SDK 30, permissions `SYSTEM_ALERT_WINDOW`, `INTERNET`, `WAKE_LOCK`, `FOREGROUND_SERVICE`), Java sources with no AndroidX, one notification icon, and plain-Java tests of the line codec.
- `controller/Dockerfile`, new stage `companion`: `FROM --platform=linux/amd64` a JDK 17 image pinned by digest. It downloads build-tools r30.0.3 and platform android-30, each checked by SHA-256 as the ORT AAR is (`Dockerfile:153-158`). Then: `javac --release 8` against `android.jar`; the codec tests on the JDK; `d8`; `aapt2 compile`/`link` with `--version-name cmp-<hash>`; `zipalign`; `keytool` + `apksigner` (decision 3). Output: `/out/companion.apk`, `/out/companion_version`. The pinned `compiler` stage is not touched.
- The `firmware` stage copies `device/companion` in for hashing, copies the built files to `internal/companion/app.apk` and `app.version`, and adds `companion` to the hash's `find`, excluding `internal/companion/app.*` (decision 2). `Dockerfile.dockerignore` allows `device/companion/`. `.gitignore` ignores the two copied files.
- `internal/companion/embed_server.go` (`//go:build server`) holds the `//go:embed`. `embed_host.go` (`!server`) returns nothing, so host `go test`/`go vet` in CI build without an APK.
- `device/compile.sh` needs no change: `firmware-out` pulls the `companion` stage in through `COPY --from`.
- CI gets a job that runs `docker build --target companion`, which compiles and tests the app on every push. CI builds no image today.

## 5. Phases

### Phase C0: Probe on the unit (throwaway code; findings only)

A minimal APK plus a loopback listener started by an init service in `u:r:su:s0` (not from adb), measuring:
1. `pm install`, `appops set`, `am start-foreground-service` and `dumpsys package` from the init-started service context.
2. Overlay over the kiosk (Kiosk Satellite on the unit): visible; touches pass through outside the window; a touchable bottom band receives them; window type and flags as decision 9.
3. Foreground-service survival: 24 h with the kiosk in front, screen-off cycles, `am kill`. The lowest notification importance accepted for a foreground service, what the shade shows, and whether Android marks the app as drawing over others.
4. Screen wake from screen-off with an `ACQUIRE_CAUSES_WAKEUP` wake lock, and its latency.
5. Loopback connect from the app uid to the `su` listener; the `/proc/net/tcp` uid match.
6. Time from frame write to visible pixel (slow-motion video), against the Dot's ring for the same wake.
7. Z-order of the bar against Kiosk Satellite's Lockdown shield, a full-screen, touch-consuming `TYPE_APPLICATION_OVERLAY` (`LockShieldOverlay.kt`): which is on top when each is added first, and whether the `cancel`/`stop` zones receive touches under lockdown.
8. With Kiosk Satellite's "Use a black screen instead", its window sits at zero brightness: whether the bar is visible, and whether a `screenBrightness` override on the bar's window while the zone is not `none` makes it visible.
9. A wake-lock wake from panel-off with Kiosk Satellite in front. It classifies wakes it did not start as "Power button": the dashboard by default, or the screensaver if "Power button" is picked under "Wake to screensaver". Record which happens and that the bar shows. Kiosk Satellite relights the panel itself with `SCREEN_BRIGHT_WAKE_LOCK | ACQUIRE_CAUSES_WAKEUP | ON_AFTER_RELEASE` (`BackgroundBridge.kt:1176-1181`), which is evidence that the mechanism works.
10. Bar placement with Kiosk Satellite's system bars hidden, against a visible navigation bar.

**Acceptance:** each item answered in findings §8.x with numbers. A failure in 1, 2, 4 or 5 sends the matching decision back for review before C1.

### Phase C1: App

- `LinkService`: foreground service; the §3 client with backoff; the notification (decision 13).
- `BarView` in a `TYPE_APPLICATION_OVERLAY` window at the bottom edge, 8 dp tall, `FLAG_NOT_FOCUSABLE | FLAG_NOT_TOUCHABLE | FLAG_LAYOUT_IN_SCREEN`. It draws the 12 colours as a horizontal gradient and nothing for an all-black frame. With the link down for 2 s it shows nothing, and the notification reads "EchoMuse is not running".
- Touch zones (decision 9): the window grows to 72 dp and drops `FLAG_NOT_TOUCHABLE` while the zone is `cancel` or `stop`; "Stop" is drawn in `stop`.
- A small status text above the bar while `link` is `pending`, or `down` for more than 10 s.
- Screen wake (decision 10).
- `AppOpsManager.startWatchingMode` on `SYSTEM_ALERT_WINDOW`: it reports `state`, removes the window when the op is denied and adds it back when allowed.

**Acceptance:** codec tests green in the `companion` stage; by hand on the unit with a stub listener, every §3 message has its effect.

### Phase C2: Build and ship

§4 in full.

**Acceptance:** `docker build --target companion` and `device/compile.sh` succeed. Changing a file under `device/companion` changes both `cmp-` and `fw-`. Changing a Go test changes neither. Two clean builds of the same sources give the same `fw-` version.

### Phase C3: Firmware

- `internal/companion`:
  - `Installer` (decisions 4 and 5), over an injected command runner;
  - `Server`: the listener, the uid check, the §3 codec, replay after `hello`;
  - `Sink`: a `led.Controller` that keeps the last frame;
  - the `press` → `Supervisor.DotButton` mapping;
  - health for `stats` (decision 11).
- `cmd/server.go`, LineageOS row only:
  - attach the `Sink` to the ring;
  - start the installer and the server;
  - `supCfg.ScreenOn` is replaced by the app path (decision 10);
  - the volume arc is suppressed (decision 12).
- `supervisor`: emit `touch` zones from alert focus and dialog focus leases; emit `screen` at candidate open; feed link transitions to `status`. Capabilities per decision 8 (`platformCapabilities`, `proto.CapScreenOverlay`).
- Host tests:
  - `/proc/net/tcp` and `tcp6` parsing (including IPv4-mapped loopback) and uid matching, including a foreign uid refused;
  - install decisions: missing app, same version, different version, failing `pm`, failing `am`, retry pacing;
  - codec round trips and replay after `hello`;
  - zone transitions across an alert and a turn;
  - `supervisor_test` hello capability sets per platform;
  - an installer failure never ends `Run`.

**Acceptance:** host `go test`/`go vet` green; on the unit, the firmware installs the app, which connects and paints the link layer before the controller session is up.

### Phase C4: Controller and dashboard

- `em_device_link.Capability.SCREEN_OVERLAY`.
- `em_device`: the ring and button gaps (`NO_RING`, `NO_BUTTON`) apply to `fireos6_crown` only. For `lineage18_crown`, `leds`, `led_anim`, `buttons`, `button_hold` and `screen_overlay` become platform-optional, so a LineageOS Show on older firmware reads "firmware update available". This needs per-platform optional sets in place of the single `_PLATFORM_OPTIONAL`. Add `companion` to `STATS_KEYS`.
- Dashboard:
  - `CAPABILITY.SCREEN_OVERLAY`;
  - the Screen section disables URL and password on `screen_overlay` devices (decision 10);
  - the Ring section is enabled for the Show through the existing `leds`/`led_anim` gates;
  - the Status panel shows companion health: linked, app version, overlay allowed, last error.
- Tests:
  - `test_capabilities.py`: split `SHOW`/`SHOWS` per platform; LEDs are sent to a LineageOS Show; button policy for a LineageOS Show; older-firmware gaps.
  - `capability_gap.test.mjs`: per-platform Show fixtures.
  - `test_config_sections.py` is unchanged.
- Docs: `docs/protocol-v1.md` (capability list, `stats.companion`), `CLAUDE.md` (capability list), [echo-show-lineage-plan.md](echo-show-lineage-plan.md) §3 (point to this plan).

**Acceptance:** pytest, mypy, pyflakes and the three dashboard tests green.

### Phase C5: Adoption, upgrade, rollback

- Wizard, LineageOS steps: `… verify_service → confirm_link → verify_companion`. `verify_companion` polls the device's `stats.companion` through the controller API until `linked` and `overlay` are true (60 s). On failure it shows the `[companion]` lines of `server.log` and the output of `dumpsys package org.echomuse.companion` over adb. `provision_wizard.test.mjs` pins the step list.
- OTA, queued update, rollback, auto-rollback: no code change (decision 1). Behaviour to verify, not to build:
  - update: the new firmware reinstalls the app on its first start;
  - rollback to a firmware with the app: that firmware's app is installed;
  - rollback to a firmware from before the app: the app stays installed, shows nothing, and reads "EchoMuse is not running".
- `docs/rooting.md` undo gains `pm uninstall org.echomuse.companion`. `docs/quickstart.md` describes the bar, the touch zones and the notification.

**Acceptance:** wizard tests green; the update and rollback paths exercised on the unit (C6).

### Phase C6: Acceptance on the unit

The unit (`G6G16D12143203LD`, pending approval on the LAN controller since 2026-10-17) is approved first.
- Adoption from a wiped `/data/local`: the wizard ends with the app linked.
- OTA to a build with a changed app: the app is reinstalled, and the controller sees the new `companion.version` within one stats period.
- Manual rollback and back again.
- `am force-stop org.echomuse.companion`: the bar returns within 35 s (decision 5's 30 s plus start).
- Overlay op revoked by hand: the firmware re-grants and the bar returns.
- Reboot: the bar shows the link layer before the controller session.
- A wake from screen-off lights the screen; a spoken turn shows listening, thinking and speaking; a timer shows progress.
- A ringing alarm is stopped by touch; a turn is cancelled by touch; a hold reaches HA as `long`.
- 24 h with the kiosk in front: no app restarts beyond those induced, and touches outside active zones always reach the kiosk.

## 6. Risks

| Risk | Mitigation |
|---|---|
| Android 11 kills or restricts the foreground service | C0 item 3 measures it; the firmware restarts the app (decision 5) |
| The overlay op is revoked from the shade | the app reports `overlay: false`; the firmware re-applies it, and every start applies it anyway |
| Touch zones steal kiosk touches | zones exist only during turns and alerts (decision 9), and only along the bottom 72 dp |
| An installer failure takes the firmware into auto-rollback | installer errors go to health only; a host test pins that `Run` never ends on them |
| Another local app connects to the port | uid check (decision 6) |
| The APK drifts from the firmware | not possible across a firmware start (decisions 1, 2, 4) |
| `aapt2`/`zipalign` are x86-64 only | the `companion` stage runs as `linux/amd64`, like `compiler` |
| A bar lit for a long timer | an LCD, not OLED: no burn-in; timer progress is dim in the default scenes |
| Kiosk Satellite's status-bar shield and System UI guard close the notification shade, so the foreground notification is not seen on a locked kiosk | the notification is secondary by design (decision 13); health reaches the dashboard through `stats` (decision 11) |

## 7. Open questions (resolved 2026-10-17)

1. Package name: **`org.echomuse.companion`**.
2. Bar mapping: **12 segments left to right**, drawn as one horizontal gradient, index 0 at the left. Mirroring from the centre was not taken: the `spin` pattern would run outward from both ends.
3. Tap-to-talk while idle: **none**. Idle stays `zone: none`, and every touch reaches the kiosk.
4. `screenWake` default: **unchanged (off)** for `screen_overlay` devices too.
5. Dashboard section label: **unchanged ("Ring")** on a Show.

## 8. Later candidates on this link

| Candidate | What it replaces or adds | Needs |
|---|---|---|
| Volume through `AudioManager` | `androidvol`'s pinned `service call audio` codes and the `settings_system.xml` mtime watch | a cutover decision: volume would depend on the app being up |
| BLE proxy through `BluetoothLeScanner` | the Show's `ble_proxy` gap ("managed by the device") | a probe: raw advert bytes and address type from the public API on API 30, scan throttling, location permission |
| Captions and named timer countdowns | the bar's fill fraction | a new controller → device message and capability |

## 9. Rejected alternatives

- **A separate controller-side APK delivery** (assets plane or shell plane). It gives a second version to track, and rollback would not carry the app.
- **A committed signing key.** It keeps a private key in a public repository, for no benefit once decision 4 reinstalls on every version change.
- **Unix socket.** It works on LineageOS's permissive policy, but would need policy for any enforcing build. TCP needs nothing.
- **`am broadcast` per update.** It starts a Java process per call: fine for starting the app, far too slow for frames.
- **Hidden `SurfaceControl` from an `app_process` helper** (no install). It relies on private APIs.
- **An HA dashboard card.** The HA satellite entities never leave `idle`, because the controller runs `assist_pipeline/run` directly (`em_ha_client.py:1146,1191`). It would also show nothing offline or over other pages.

## 10. Status (2026-10-17)

C1–C5 were built before the C0 probe, by decision of the project, because the unit is pending approval and its adb is not authorised. Every C0 answer can still send a decision back; the items most likely to are listed under C0.

| Phase | State | Evidence |
|---|---|---|
| C0 Probe | open | Needs the unit: its adb is unauthorised and it is pending approval on the LAN controller. All ten items of §5 C0 are unanswered. The build assumes, and C0 must confirm: `pm install` from `/data/local/tmp/echomuse` in the init-started `su` context; root `am start-foreground-service` reaching the `exported="false"` `LinkService`; `IMPORTANCE_MIN` accepted for the foreground notification; the app's socket appearing in `tcp` or `tcp6` (both handled); `setFitInsetsTypes(0)` putting the bar at the true bottom edge; the bar's Z-order against Kiosk Satellite's Lockdown shield and its black-screen mode |
| C1 App | done, not run on the unit | `device/companion/`: manifest (no activity, no receiver), pure-Java codec `core/` (`Json`, `Link`, `StatusText`), `app/` (`LinkService`, `BarView`), notification icon. Codec and status-text tests (`test/…/LinkTest.java`, 21 cases, 122 checks) run in the `companion` stage. By-hand check of every §3 message on the unit is C6 |
| C2 Build | done | `companion` stage (`eclipse-temurin:17-jdk@sha256:fe947d99…`, build-tools r30.0.3 and platform-30 r03 by SHA-256), `companion-out` export stage; the firmware stage embeds the APK and hashes `device/companion`, excluding `internal/companion/app.*`; `.dockerignore`, `.gitignore`; CI job `companion-build`. `aapt2 dump badging`: `org.echomuse.companion`, `versionName` `cmp-…`, SDK 30/30; `apksigner verify` v2+v3. Version acceptance: an edit under `device/companion` moved both `cmp-` and `fw-`; an edit to a Go `_test.go` moved neither; two builds with the `companion` and `firmware` stages uncached gave the same `fw-`. `firmware-out` was exported to a temporary directory rather than through `device/compile.sh` (same target) |
| C3 Firmware | done, not run on the unit | `internal/companion` (`Installer`, `Server`, `Sink`, `Health`, `procnet`), `supervisor/companion.go` (zones, link status), `proto.CapScreenOverlay`, `proto.CompanionStats`, the LineageOS row in `cmd/server.go`. Host tests for every C3 item; `go vet` and `go test` green |
| C4 Controller | done | `Capability.SCREEN_OVERLAY`, per-platform optional sets, `companion` in `STATS_KEYS`; dashboard Screen and Status changes; `test_capabilities.py` and `capability_gap.test.mjs` per platform. pytest, mypy, pyflakes and the three dashboard tests green |
| C5 Adoption | done, wizard not run over WebUSB | `verify_companion` step and its test; `docs/rooting.md`, `docs/quickstart.md`, changelog. The update and rollback paths are C6 |
| C6 Acceptance | open | Needs the unit approved; every item of §5 C6 is to do |

Where the build differs from the text above:
- **`verify_companion` on a pending device.** A device pending approval has no session, so no `stats`; under strict approval the step would always fail on a first adoption. For a pending device the step reads the same two facts over adb: an established `/proc/net/tcp{,6}` row to port 8771 owned by the app's uid, and `appops get … SYSTEM_ALERT_WINDOW` reading `allow`. Approved devices are read through `stats.companion`, and the step waits for a report newer than the first it sees, because `Device.stats` outlives a session.
- **No volume-arc flag (decision 12).** The arc is painted only from the Dot's volume buttons (`volumeController.step`); external and dashboard changes never paint it, so a LineageOS flag would never be exercised. `TestShowVolumeChangeNeverPaintsTheArc` pins that a Show's attached display stays dark on key and dashboard changes.
- **Hold without `ACQUIRE_CAUSES_WAKEUP`.** Only `screen wake` turns the screen on, which respects `screenWake: false`; `hold` keeps a lit screen lit. An alert from screen-off therefore does not light the screen.
- **Colour as alpha.** Each segment's brightest channel becomes its alpha, at full-strength hue, so an off segment is transparent over the kiosk rather than a black block.
- **Installer pacing.** Attempts at least 10 s apart, the gap doubling per consecutive failure up to 5 min, plus the 30 s link-down recheck of decision 5.
- **`status` reasons.** `up`: none; `pending`: `pending_approval`; `down`: `starting` (controller null), `closed`, `session_lost`, `protocol`, `unauthorized`. The controller is the host:port actually dialled, and the last one reached is kept while down. `client.Handler.Rejected` gained the controller address for this.
- **SDK levels** come from the manifest's `<uses-sdk>` alone, not also from `aapt2 link` flags.
