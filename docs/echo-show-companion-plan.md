# Echo Show 8 on LineageOS: companion app plan

**Status:** proposed 2026-10-17; Phases C1–C5 implemented 2026-10-17, ahead of C0. C0 and C6 need the unit and are open (§10). The Kiosk Satellite plugin and turn view (§11) were proposed 2026-10-18; its probe, Phase K0, was measured the same day (findings §8.7), and K1–K5 were implemented the same day, with the plugin checked by hand on the unit against a stub link (§11.9). K6 needs a controller deploy and is open.
**Evidence:** [echo-show-crown.md](echo-show-crown.md) (written "findings §N") and the code cited inline. The platform this extends is [echo-show-lineage-plan.md](echo-show-lineage-plan.md), written "LineageOS plan".
**Goal:** a LineageOS Show shows what the Dot's ring shows (wake, listening, thinking, speaking, timers, link state, alerts) as a light bar on screen. Touch takes the place of the action button. The app is installed by adoption and kept current by every firmware update, rollback and auto-rollback, with no separate delivery path.

**Not in scope:**
- Fire OS crown. It keeps Fully's REST screen wake (`internal/kiosk`) and gets no app.
- Volume through `AudioManager`, a Bluetooth proxy through `BluetoothLeScanner` and named timer countdowns. These are later candidates that build on this link (§8). The request and answer text are §11.
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
| Named timer countdowns | the bar's fill fraction | the timer names and remaining time in a controller → device message |

## 9. Rejected alternatives

- **A separate controller-side APK delivery** (assets plane or shell plane). It gives a second version to track, and rollback would not carry the app.
- **A committed signing key.** It keeps a private key in a public repository, for no benefit once decision 4 reinstalls on every version change.
- **Unix socket.** It works on LineageOS's permissive policy, but would need policy for any enforcing build. TCP needs nothing.
- **`am broadcast` per update.** It starts a Java process per call: fine for starting the app, far too slow for frames.
- **Hidden `SurfaceControl` from an `app_process` helper** (no install). It relies on private APIs.
- **An HA dashboard card.** HA's `assist_satellite` entities stay `idle`, because the controller runs `assist_pipeline/run` directly (`em_ha_client.py:1146,1191`). `sensor.<name>_voice_state` does follow the turn, but no entity carries the request or answer text, and a card shows nothing offline or over other pages.

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

## 11. Kiosk Satellite plugin and turn view (proposed 2026-10-18)

**Goal:** on a Show whose kiosk is Kiosk Satellite (`me.jxl.kiosk_satellite`), every EchoMuse turn shows a full-screen view modelled on Kiosk Satellite's own voice overlay ([reference](https://kiosksatellite.com/img/voice.jpg)): the request as transcribed, in grey, and the answer in large text below it. The view draws over the dashboard and the screensaver, lifting a screensaver that has darkened the panel (decision 9). The firmware installs the plugin and keeps it current through Kiosk Satellite's remote API. The dashboard turns the Kiosk Satellite admin password into a token for that.
**Evidence:** Kiosk Satellite at `28113b6` and its plugin SDK repository `kiosk-satellite-plugin-hello-world` at `233e739` (written "SDK `docs/…`").

**Not in scope:**
- Tool-call lines ("Get weather forecast") and result panels (the weather card). Both come from HA's chat log; see §11.8.
- Live partial transcripts while listening. The session has no partial text, so the request appears when STT ends.
- Announcement, timer and alarm text in the view.
- Kiosk Satellite's skins. They are Flutter code inside the app, not reusable from a plugin's Android view; the view is our own drawing.
- Kiosks other than Kiosk Satellite, and Fire OS crown.

### 11.1 Starting point

| Area | Today | Source |
|---|---|---|
| Turn text on the device | none: the device link carries no transcript or answer | `docs/protocol-v1.md` |
| Request text | `sent`: the text dispatched to HA, after transcript correction | `controller/em_session.py:2449` |
| Answer text | `IntentEnded.speech` at `intent-end`; from `intent-progress` only `tts_start_streaming` is read, not `chat_log_delta` | `controller/em_ha_client.py:703-733` |
| Local link | one connection, accepted only from the companion's uid; lines at most 4 KiB | `device/internal/companion/server.go:18-56` |
| Plugin overlays | native Android views composited into Kiosk Satellite's screen; `onTop(true)` takes the voice overlay's slot, over the screensaver. The SDK says its notifications, intercom cards, announcements, the screensaver's black cover and Lockdown Mode cover it; on the unit a notification drew under it | SDK `docs/overlays.md:3,164`; findings §8.7 |
| Plugin threads | Kiosk Satellite runs StrictMode: socket I/O on the main thread throws, and an exception in a plugin callback disables the plugin | findings §8.7 |
| Screensaver on the unit | mode `black`: its window sets `screenBrightness` 0.0 and the backlight sits at 1 of 255, so a view drawn over it is not visible. `stopScreensaver` restores the brightness within 0.3–0.8 s | findings §8.7 |
| Plugin code | runs in Kiosk Satellite's process with its identity, unsandboxed; may open sockets | SDK `docs/installing-plugins.md:53`, `docs/creating-plugins.md:187` |
| Plugin install | `POST /api/commands/installPlugin {data, trusted: true}` (ZIP up to 4 MB, base64). `listPlugins` returns each plugin's `sha256` (of the ZIP), `enabled` and `running`. A new plugin starts disabled; a replacement keeps the enabled state | SDK `docs/installing-plugins.md:41,61-86`; `PluginBridge.kt:593-599,698-747` |
| Remote API auth | `POST /api/login {password, ttl_days}` returns a bearer token valid up to 3650 days. Tokens are stateless, so a password change does not revoke them. The server runs only with Remote Administration on and a password set, on port 2324 unless changed; `GET /api/health` needs no token | `docs/remote-api.md:3,16-17,83-98` |

### 11.2 Decisions

1. **The plugin ships inside the firmware**, as the APK does (decision 1). `internal/companion` embeds `ksplugin.zip`, built from `device/companion/ksplugin/` in the `companion` stage, and the firmware installs it. Every update, rollback and auto-rollback carries the matching plugin. Its manifest version is `1.0.0-<hash>`, with the `cmp-` hash: Kiosk Satellite requires `major.minor.patch[-suffix]` (`PluginManifest.kt:13-14`).
2. **Install by hash.** On `127.0.0.1:<kioskPort>`, the firmware runs `listPlugins` and compares the `echomuse` record's `sha256` with the embedded ZIP's.
   - A missing record or a different hash: `installPlugin`.
   - Then `getPluginState`. If the master switch is off: `setPluginsEnabled {enabled: true}`, recorded in health, since it also resumes other plugins already enabled. If the record is disabled: `enablePlugin {id: "echomuse"}`.
   - It runs at firmware start, when the token changes and when the plugin has been unlinked for 30 s, paced as decision 5. Failures go to health only; nothing here ends the server.
3. **The token, never the password.**
   - The dashboard asks for the Kiosk Satellite admin password. The controller logs in at `http://<device ip>:<kioskPort>/api/login` with `ttl_days: 3650`, so a wrong password is reported at once.
   - Only the token is kept, in the device config (`kioskToken`; `kioskPort`, default 2324), and pushed to devices announcing `kiosk_plugin` (decision 7).
   - The API reports the token to the dashboard only as set or unset.
   - A 401 from any call is reported in health as `token_rejected`, and the dashboard asks again.
4. **When the dashboard asks.** Every prompt can be skipped: the bar works without the plugin.
   - In the LineageOS wizard's new step `kiosk_plugin`, after `verify_companion`, when the device reports Kiosk Satellite installed.
   - Before a firmware update or queued update to such a device that has no token, or whose health reads `token_rejected`.
   - From the Screen section at any time.
5. **Detection.** `stats.companion.kiosk`, each field null when unknown:
   - `installed`: the package is present;
   - `version`: its `versionName`;
   - `remote_admin`: `GET /api/health` answers;
   - `plugin`: `version`, `linked`, `error`.
6. **One link, two roles.** Link version 2.
   - `hello` carries `role: "app" | "plugin"`. Role `app` is accepted only from the companion's uid, role `plugin` only from Kiosk Satellite's (the owner of `/data/data/me.jxl.kiosk_satellite`).
   - One connection per role; a new one replaces the old.
   - Lines may be up to 128 KiB.
7. **Capabilities.** `turn_view_v1`: the device accepts `turn.view`. `kiosk_plugin`: the device installs the plugin and takes `kioskToken` and `kioskPort`. The controller sends each only to devices announcing it, and the LineageOS firmware announces both.
8. **`turn.view`**, a new controller → device message that the firmware relays unchanged to the plugin as `view`. The controller sends the whole view every time, never a delta, so a lost message costs nothing.

   | Field | Value |
   |---|---|
   | `turn_id` | the turn |
   | `phase` | `listening` \| `thinking` \| `answering` \| `done` |
   | `heard` | `sent`, or the text as heard for a local route; null until STT ends |
   | `answer` | the answer so far; null until the first text. At most 16 KiB UTF-8, cut at a word with "…" |
   | `final` | true once `intent-end` has replaced the streamed text with the final speech |
   | `follow_up` | `continue_conversation` |

   When it is sent:
   - at the turn's `dialog_input` start (`listening`);
   - at dispatch (`thinking`, with `heard`);
   - on each `chat_log_delta`, at most 10 per second with the latest winning (`answering`);
   - at `intent-end` (`final`);
   - at the end of the reply's playback, or of a turn with no reply (`done`). A turn ending `no_input`, cancelled or in error sends `done` with no answer.
9. **The view.**
   - **One long-lived overlay**, `OverlaySpec.fullScreen().onTop(true).touchable(false)`, created when the plugin starts. A turn changes its contents' visibility; a turn never calls `showOverlay`, since creating an overlay can take a second or more on a slow panel (SDK `docs/overlays.md:40`).
   - **Layout, after the reference:**
     - a near-opaque backdrop in `KsTheme`'s `surface` colour; there is no blur, because `RenderEffect` needs API 31;
     - `heard` in `onSurfaceVariant`, 28 sp;
     - three pulsing dots while `thinking`;
     - `answer` in `onSurface`, 34 sp, Rubik (`KsTheme.typeface`), scrolling up as it grows past its area. On the unit a scroll ran at about 35 fps on its 54 Hz panel (findings §8.7);
     - a soft glow along the bottom in the colours of the firmware's latest `frame`.
   - **When it shows:** from `listening` until `done`, plus `turnViewLinger` seconds (config, default 6; 0 hides at once). A new turn replaces it; `follow_up` keeps it up into the next `listening`.
   - **A darkened screensaver is lifted.** The unit's `black` screensaver holds the panel at backlight 1 of 255, so drawing over it shows nothing (findings §8.7). At `listening`, if Kiosk Satellite's screensaver is active, the plugin calls `stopScreensaver` (`host.control`). It tracks the screensaver with the `screensaver.state` event and `isScreensaverActive` (`host.read`). A screensaver that leaves the panel lit is dismissed the same way: the plugin cannot read whether it is lit, only the brightness Kiosk Satellite reports, and one rule is simpler than two.
   - **Threads.** All link I/O runs on the plugin's own threads; the main thread only posts to a writer queue (StrictMode, findings §8.7).
10. **One bar on screen.** The app keeps the bar and its touch zones. Its `TYPE_APPLICATION_OVERLAY` window sits above Kiosk Satellite's window, so the bar draws over the bottom edge of the view, and the view leaves that edge to it. The view takes no touches.

### 11.3 Local link, version 2

These are added to §3. The app's `hello` gains `role: "app"` and `link: 2`. The app comes from the same build as the firmware, so the version bump costs nothing.

| Direction | `t` | Body | When |
|---|---|---|---|
| plugin → fw | `hello` | `link: 2`, `role: "plugin"`, `plugin: "1.0.0-…"` | on connect |
| fw → plugin | `hello` | `link: 2`, `firmware: "fw-…"`, `plugin: "1.0.0-…"` (the embedded one) | reply; on a mismatch both sides close and the firmware reinstalls the plugin (decision 2) |
| fw → plugin | `config` | `turn_view_linger`: seconds | after `hello`, and when `turnViewLinger` changes |
| fw → plugin | `frame` | as for the app | as for the app |
| fw → plugin | `view` | the `turn.view` body | on every `turn.view`, and the last one after `hello`. When the controller session is lost while the last view is not `done`, the firmware sends it again as `done` |

After the plugin's `hello` the firmware sends `hello`, `config`, `frame`, then the last `view`. A `hello` without `role` is an app's (only link-1 apps omit it).

### 11.4 Build

- `device/companion/ksplugin/`:
  - `kiosk-satellite-plugin.json`: `schemaVersion: 1`, `apiVersion: 1`, `id: "echomuse"`, `name`, `version`, `minAndroidSdk: 30`, `entryClass: org.echomuse.ksplugin.EchoMusePlugin`, `description`, `author`, `capabilities: ["overlay", "host.read", "host.control"]`;
  - the Java sources, reusing the link codec in `device/companion/core`;
  - a `LICENSE` file: Kiosk Satellite's installer rejects a ZIP without one (SDK `docs/installing-plugins.md:41`). Internal use only, so it is a one-line placeholder.
- The SDK interfaces: the five files under `me/jxl/kiosk/plugins/` (`KioskPlugin`, `KsTheme`, `OverlayFactory`, `OverlaySpec`, `PluginHost`), vendored in `device/companion/kssdk/` with the source commit. They are compile-time only and never packaged (SDK `docs/creating-plugins.md:76`).
- In the `companion` stage:
  1. `javac` against `android.jar` and the SDK;
  2. the plugin's tests on the JDK;
  3. `d8` into `plugin.jar` (`classes.dex` only);
  4. a ZIP with fixed timestamps, checked to be at most 4 MB.

  Output: `/out/ksplugin.zip`, which the firmware stage embeds as `internal/companion/ksplugin.zip`, excluded from the `fw-` hash like `app.*`.

### 11.5 Phases

#### Phase K0: Probe on the unit (done 2026-10-18; findings §8.7)

1. **Install over the remote API from `u:r:su:s0`:** `login`, `listPlugins`, `installPlugin` (1.07 s), `setPluginsEnabled`, `enablePlugin` all worked. `listPlugins` `sha256` is the ZIP's own hash, which decision 2 compares.
2. **Loopback link:** the plugin's socket is an IPv4-mapped `tcp6` row with uid 10148, the owner of `/data/data/me.jxl.kiosk_satellite`.
3. **Placement:** the overlay drew over the dashboard and the screensaver, a Kiosk Satellite notification drew under it, and a tap went through it to Kiosk Satellite. The screensaver's zero brightness changed decision 9.
4. **Speed:** from receipt on the link to the presented frame, median 44 ms, p90 55 ms, maximum 62 ms (n = 20). A long answer scrolled at about 35 fps.
5. **Bar:** the app's bar layer composites above Kiosk Satellite's (decision 10 holds).

Not measured: Lockdown Mode and the `screen_off_black` cover over the view. Neither is on for this unit.

#### Phase K1: Plugin

- `EchoMusePlugin`: `start` opens its own link thread with a 0.5–5 s backoff; `stop` closes the socket and joins the thread within the 3 s callback deadline (SDK `docs/creating-plugins.md:145`).
- `ViewOverlay` (decision 9). Every listener and `onDraw` catches its own exceptions, since one escaping there ends the Kiosk Satellite app (SDK `docs/overlays.md:178`).
- A pure-Java view-state reducer (phases, linger, replacement, follow-up), with tests.

**Acceptance:** reducer tests green in the `companion` stage. By hand on the unit, with a stub listener, every `view` phase draws as decision 9 says.

#### Phase K2: Build

§11.4.

**Acceptance:**
- The stage builds and runs the plugin tests.
- The ZIP installs through Kiosk Satellite's Install from ZIP.
- An edit under `device/companion/ksplugin` moves both the `cmp-` and `fw-` versions.

#### Phase K3: Firmware

- Link version 2: roles, per-role uid, 128 KiB lines, the `view` relay.
- `KioskInstaller` over an injected HTTP client: decisions 2, 3 (`token_rejected`) and 5.
- The `turn_view_v1` and `kiosk_plugin` capabilities; `turn.view` handling; the config keys `kioskToken` and `kioskPort`.
- Host tests:
  - the role and uid matrix, and replacement per role;
  - the relay, and replay after `hello`;
  - installer decisions: no package, remote admin down, no token, 401, same hash, different hash, master switch off, record disabled, pacing;
  - an installer failure never ends `Run`.

**Acceptance:** host `go test` and `go vet` green.

#### Phase K4: Controller and dashboard

- `em_ha_client`: `intent-progress` `chat_log_delta.content` becomes a new run event, `AnswerDelta(text)`; tested with the fake HA.
- `em_session`: `turn.view` as in decision 8, to `turn_view_v1` devices only. Tested for a normal turn, a streamed answer, a follow-up, `no_input`, a local route, and a device without the capability.
- `Capability.TURN_VIEW` and `Capability.KIOSK_PLUGIN`. `kioskToken`, `kioskPort` and `turnViewLinger` in the Screen section, with the token write-only in the API.
- `POST /api/devices/{id}/kiosk_login {password}` (decision 3), tested against a fake Kiosk Satellite: success stores the token and pushes the config; a 401; an unreachable device.
- Dashboard:
  - the password prompt (decision 4);
  - `kiosk` in the Status panel's companion block;
  - capability gaps for the two new capabilities.
- Docs:
  - `docs/protocol-v1.md`: `turn.view`, the capabilities and `stats.companion.kiosk`;
  - the `CLAUDE.md` capability list;
  - `docs/quickstart.md`: Remote Administration with a password is the prerequisite.

**Acceptance:** pytest, mypy, pyflakes and the dashboard tests green.

#### Phase K5: Adoption and upgrade

- The wizard step `kiosk_plugin` (decision 4), pinned in `provision_wizard.test.mjs`.
- The update prompt (decision 4).
- Behaviour to verify, not to build:
  - rollback to a firmware with an older plugin reinstalls that plugin;
  - rollback to a firmware from before the plugin leaves it installed but unlinked, since that firmware refuses Kiosk Satellite's uid, so it draws nothing.

#### Phase K6: Acceptance on the unit

- Adoption with Kiosk Satellite installed: the prompt, then the plugin installed, enabled and linked.
- A spoken turn shows the listening view, the request, the thinking dots and the answer as it streams. The view then stays for 6 s and hides.
- A wake during the `black` screensaver lifts it: the view is visible within a second of the wake.
- A long answer scrolls.
- A follow-up keeps the view up.
- `no_input` hides it after listening.
- Only one bar is visible.
- An OTA with a changed plugin reinstalls it without a prompt.
- A token rejected after a Kiosk Satellite reset brings back the prompt.

### 11.6 Risks

| Risk | Mitigation |
|---|---|
| A plugin bug crashes Kiosk Satellite | drawing and listener code catch everything; Kiosk Satellite switches the plugin off before the crash, and health shows `linked: false` |
| A long-lived credential on the device and in the controller's database | only the token is kept, never the password, and the dashboard never reads it back. A password change does not revoke it (`docs/remote-api.md:91-93`), which the dashboard states next to the prompt |
| The request and answer text reach Kiosk Satellite's process | inherent: the plugin runs there, and Kiosk Satellite is the kiosk the user chose to trust |
| `installPlugin` is labelled "developer testing only" | it is part of the authenticated command API; a repository install would mean a public plugin repository and GitHub Actions releases for every firmware version |
| Turning the master switch on also resumes other enabled plugins | done only when it is off, and reported in health |
| Kiosk Satellite changes its plugin SDK | `apiVersion 1` is its published contract; an `installPlugin` refusal reaches health with Kiosk Satellite's message |

### 11.7 Open questions (resolved 2026-10-18)

1. The `turnViewLinger` default: **6 s**, configurable per device.

### 11.8 Later candidates

| Candidate | Needs |
|---|---|
| Tool lines and result panels | the controller subscribes to `conversation/chat_log/subscribe` for the turn's conversation (its HA token is an administrator's) and adds `tools` and `results` to `turn.view` |
| Live partial transcripts | partial text from the speech worker into the session |
| Announcement text | `turn.view` for announcements |

### 11.9 Status (2026-10-18)

| Phase | State | Evidence |
|---|---|---|
| K0 Probe | done | findings §8.7 |
| K1 Plugin | done; checked by hand on the unit | `device/companion/ksplugin/`: `EchoMusePlugin`, `ViewOverlay`, the pure-Java reducer `TurnView` (`core/`), manifest, `LICENSE`. `TurnViewTest` (11 cases, 54 checks) and the v2 codec in `LinkTest` (26 cases, 164 checks) run in the `companion` stage. On the unit (firmware `fw-b063e8ba4d8d` stopped for 28 s, a `toybox nc -L` stub on 127.0.0.1:8771 answering role `plugin` only): `installPlugin`, `setPluginsEnabled`, `enablePlugin`; the plugin linked from uid 10148 over `tcp6`; `listening`, `thinking`, a growing `answering`, `done` and a long answer each drew as decision 9 says, and the view hid 6 s after `done`. With the `black` screensaver started over the API (backlight 1), `listening` lifted it: backlight 30 after 0.3 s, 102 after 1 s, `isScreensaverActive` false. Afterwards the plugin was removed, the master switch put back off and the service relinked the app |
| K2 Build | done | Vendored SDK in `device/companion/kssdk/` (`SOURCE`: commit `233e739`). The `companion` stage generates `BuildInfo.java` with the plugin version, compiles against `android.jar` and the SDK, runs the tests, d8s into `plugin.jar` and zips with `jar --date` (fixed timestamps), failing over 4 MB: 18 138 bytes, the same SHA-256 on a rebuild. The firmware stage embeds it as `internal/companion/ksplugin.zip`, excluded from the `fw-` hash; `.gitignore`, `Dockerfile.dockerignore`. The ZIP carved out of the `firmware-out` binary is byte-identical to the one installed on the unit. Version acceptance: an edit under `device/companion/ksplugin` moved `cmp-` and `fw-`; an edit to a Go `_test.go` moved neither. It was installed with `installPlugin`, the firmware's path, not Kiosk Satellite's Install from ZIP |
| K3 Firmware | done, not run on the unit | Link v2 in `internal/companion` (`server.go`: roles by uid, one connection per role, 128 KiB lines, `config`/`frame`/`view` to the plugin), `KioskInstaller` (`kiosk.go`) over an injected `HTTPClient`, the shared pacing in `pacer.go`, `stats.companion.kiosk`; `proto.CapTurnView`, `proto.CapKioskPlugin`, `proto.TypeTurnView`; config `kioskToken`, `kioskPort`, `turnViewLinger`; the LineageOS row in `cmd/server.go`. Host tests for every K3 item; `go vet` and `go test` green |
| K4 Controller | done | `AnswerDelta`; `em_turn_view.py` and the session's `turn.view`; `Capability.TURN_VIEW`, `Capability.KIOSK_PLUGIN`, both platform-optional on `lineage18_crown`; Screen keys; `POST /api/devices/{id}/kiosk_login`; dashboard prompt, Status rows, gaps; `docs/protocol-v1.md` §4.1, §4.8, §4.9, `CLAUDE.md`, `docs/quickstart.md`. pytest, mypy, pyflakes and the three dashboard tests green |
| K5 Adoption | done, wizard not run over WebUSB | wizard step `kiosk_plugin` pinned in `provision_wizard.test.mjs`; the update prompt; changelog |
| K6 Acceptance | in progress | 2026-10-18:<br>• the controller was deployed with `fw-d017242243b9` (schema v35), and the unit took it by queued OTA ("✓ Update confirmed", previous `fw-b063e8ba4d8d`);<br>• the token was obtained by the same login `kiosk_login` makes, with `ttl_days` 3650, and written to `devices.kiosk_token` directly: there was no dashboard session to call the endpoint, so the prompt and the endpoint are not yet exercised on the unit;<br>• the firmware then installed and enabled the plugin by itself: Kiosk Satellite `getPluginState` reads master on, `echomuse` `1.0.0-f9dccdbfc8cd`, `sha256` `4d19874f…`, enabled and running, no error;<br>• two clients are linked on 8771 (app and plugin; the reinstalled app is now uid 10150).<br>A spoken turn, the screensaver lift, follow-up, `no_input`, OTA reinstall and token rejection are still to do |

Where the build differs from the text above:
- **The token is a device column, not a config key.** `devices.kiosk_token` (migration v35). Screen-section keys follow the fleet unless a device overrides the whole section, and config is echoed by the config API, websocket pushes and the device JSON; a column makes "never returned" structural. On the wire it is still `config` `kioskToken`, and the device JSON carries only `kioskTokenSet`.
- **`config` on the local link.** `turnViewLinger` reaches the plugin as a `config` line (§11.3), which the plan did not list.
- **`done` on session loss.** The firmware ends a view the controller can no longer end, so the plugin does not stay up after a lost session (§11.3).
- **`turn.view` is never acknowledged**; a lost one is covered by the next whole view. Bodies that are not an object with a string `phase`, or that would exceed a 128 KiB line, are dropped and logged.
- **`follow_up`** is true only when a reply window actually follows (HA's `continue_conversation`, or EchoMuse's own question), not when the chain limit or a cancelled prompt means nothing will listen.
- **A wake that lost arbitration shows no view**: the winning device shows the turn. `done` on a completed turn carries the turn's response text, which covers EchoMuse's own clarification prompts.
- **Reducer rules the plan left open:** a follow-up `done` keeps the view up for the longer of the linger and 15 s; a view not `done` hides 5 min after its last update; a repeated `done` (a replay after reconnecting) does not restart the linger; link loss hides after the linger or the time left on a running countdown, whichever is shorter; a `done` with neither request nor answer hides at once, `follow_up` or not.
- **No writer queue in the plugin.** Its `hello` is its only outgoing line, written by the link thread right after connecting; the main thread never touches the socket, which is what the StrictMode rule needs.
- **Layout:** hidden is `View.INVISIBLE` on the long-lived view; text stays out of the bottom 100 dp (the app's 72 dp touch band plus a gap); the glow fades up over the bottom 160 dp.
- **The manifest carries `license`**, which `PluginManifest.kt:43` requires.
- **The plugin knows its version** from a `BuildInfo` class the image build generates.
- **Installer pacing.** Kiosk Satellite not installed, remote admin down and `no_token` back off like failures, so a Show without Kiosk Satellite is not probed every 30 s. A change of `kioskToken` or `kioskPort` resets the backoff, so a token just stored is used within 10 s. Config is not persisted across firmware starts, so the start attempt usually reads `no_token` until `session.ready`'s config arrives.
- **`remote_admin`** is a 200 from `GET /api/health` over plain HTTP. With Kiosk Satellite's **Use HTTPS** on it reads false and the plugin is not installed; `docs/quickstart.md` says to leave it off.
- **Replacement per role happens at accept,** by the role the peer's uid allows. A role refusal is reported in the companion `error`; a plugin link or version mismatch in `kiosk.plugin.error`.
- **The update prompt** asks when `kioskTokenSet` is false or `plugin.error` is `token_rejected`; it gates the per-device install and the queued install, not the fleet-wide "install now". `kiosk_login` is accepted while the only gap is "firmware update available", so the token can be stored before the update that brings the plugin.
