# Rooting the Echo Dot Gen 2 (biscuit)

> **You do this at your own risk. We accept no responsibility for negative
> outcomes experienced.**

EchoMuse needs an Echo Dot Gen 2 that is unlocked with amonet-biscuit v2 and
running Fire OS 6 rooted with boot-root.zip. Two separate jobs get you there,
and they carry very different risk.

**Fire OS 5 is not supported.** The EchoMuse firmware runs its audio only
through Fire OS 6's `mixer` daemon, the provisioning wizard refuses an
Android 5.x Dot at its first step, and the controller refuses a Dot that does
not report Fire OS 6. A Dot on Fire OS 5 (amonet-biscuit v1) has to move to
Fire OS 6 first; see [Moving a Dot from Fire OS 5](#moving-a-dot-from-fire-os-5).

## Hardware

- Amazon Echo Dot 2nd Gen (RS03QR, 2016)
- Codename: biscuit
- SoC: MediaTek MT8163, quad-core ARM Cortex-A53 @ 1.5GHz
- RAM: 512MB
- OS: Fire OS 6 (Android 7.1.2, API 25, product `biscuit_puffin`)
- MicroUSB cable required

## What you need

For the unlock itself (R0rt1z2's thread has the authoritative list):

- Windows or Linux machine with ADB and fastboot installed
- From R0rt1z2's XDA thread: `amonet-biscuit-v2.0.0.zip` and `boot-root.zip`
- A Fire OS 6 update file from [FTVDB](https://ftvdb.com/echo/firmware/com.amazon.biscuit.android.os/)
  (**see below for which build**)
- `server`, the EchoMuse firmware, only if provisioning by hand (see below)

> **Which Fire OS 6 build?** EchoMuse is developed and tested against
> **Fire OS 6.5.6.9**, `NS6569/6009`. The thread's example flashes a newer
> build (`NS65741`); it is not known to be broken, only untested here, and
> the provisioning wizard warns on any build other than `NS6569/6009`. If you
> are choosing, choose this one. If something behaves oddly on another build,
> that is the first thing to mention when reporting it.
>
> Check what you have with `adb shell getprop ro.build.version.name`. The
> wizard reads it at the first step and says so in the log.

> **Linux ADB stability:** Linux aggressively power-manages USB devices by default, causing ADB disconnects. Disable autosuspend before starting: `echo -1 | sudo tee /sys/bus/usb/devices/*/power/autosuspend`. macOS doesn't have this problem.

For the EchoMuse half, the provisioning wizard needs only a **Chromium-based
browser** (Chrome or Edge — it talks to the device over WebUSB) and a running
controller. It installs the firmware bundled in the controller image, the
only firmware the controller installs, so the `server` binary above is only
needed if you are provisioning by hand. Take it from the controller:
`docker cp echomuse-controller:/app/firmware/server .`

---

## Unlocking the device — R0rt1z2's amonet-biscuit

The persistent unlock, the bootrom exploit and TWRP for this device are
**R0rt1z2's** work, documented and maintained here:

- [amonet-biscuit — unlock, root, TWRP, unbrick](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/)
  on XDA Forums

Follow that thread, not this page. We link to it rather than copying it
because a copy goes out of date without anyone noticing. If the two ever
disagree, the thread is correct.

**This is the part that can ruin a device.** It runs a bootrom exploit,
modifies the partition table and wipes userdata. A failure here can leave a
Dot soft-bricked badly enough that recovery means opening the case and
shorting contacts on the board. Read the thread first, and do not start on a
device you cannot afford to lose.

amonet-biscuit **v2.0.0** moves the Dot to Fire OS 6 bootloaders, and from
then on it can only boot Fire OS 6. R0rt1z2's thread: *"Starting with version
2.0 of amonet, YOU CAN ONLY FLASH FIRE OS 6 FIRMWARE."* Do not try to go back
to Fire OS 5 or to amonet v1.x on such a device. That means writing
bootloaders by hand, which is how an Echo gets hard-bricked.

The thread's own steps end with the three things EchoMuse needs: Fire OS 6
flashed to **both** slots (the A/B layout needs it twice), then
**boot-root.zip** installed from TWRP. boot-root gives the adb shell root
(uid 0, no `su` binary) and disables OTA updates.

### Moving a Dot from Fire OS 5

A Dot already unlocked with amonet-biscuit v1 and running Fire OS 5 moves
over from TWRP, per the thread: flash `amonet-biscuit-v2.0.0.zip` in TWRP
(*"To update to the current release if you are already unlocked, just flash
the ZIP in TWRP"*), then flash a Fire OS 6 build to both slots and
boot-root.zip exactly as the thread describes for a fresh unlock. The move is
one-way, and flashing Fire OS 6 wipes `/data`, which takes the old EchoMuse
install with it.

The controller keeps the Dot: run the wizard on it and choose **Continue**
when it names the device (see [Reflashing a Dot that is already
registered](#reflashing-a-dot-that-is-already-registered)).

## Where EchoMuse picks up

Everything below assumes a Dot unlocked with amonet-biscuit v2, running
Fire OS 6, with boot-root.zip installed.

From there EchoMuse takes over. The provisioning wizard in the dashboard
handles the rest — see the [Quickstart](quickstart.md). It starts from a
device already in that state; it does not run the exploit.

Fire OS 6 on this Dot is Amazon's headless image: no Android framework, no
apps, only toybox, and audio is owned by Amazon's `mixer` daemon, which runs
the same native front end Alexa uses. EchoMuse's firmware talks to it through
`libmixerAPI`, so capture still passes through the front end and playback is
still its echo reference.

What the wizard does, all from the running system (no TWRP step, no partition
writes):

1. Writes one file, `/system/etc/init/echomuse.rc`, and creates one empty
   directory, `/tmp` (Fire OS 6 has none; the firmware mounts a RAM disk on
   it for its log), on the system slot the Dot is **running**, by
   remounting `/` read-write for the write and read-only after. The other
   slot is left stock for now.
2. Installs the firmware bundled with the controller under `/data/local/bin`,
   its startup script, and the device-link TLS credentials.
3. Writes the Wi-Fi network into `/data/misc/wifi/wpa_supplicant.conf`.
4. Reboots and checks that `echomuse` and Amazon's `mixer` are running.
5. Waits for the device to reach the controller serving the wizard. A new
   device arrives pending approval.
6. Only then writes the same file and directory to the **other** slot, by
   mounting its `system` partition under `/data/local/tmp`. A bootloader
   fallback therefore still runs EchoMuse, and a bad install never reaches
   both slots: until step 5 passes, the other slot stays a stock,
   known-good fallback. Verity is off for both (the shared amonet bootloader
   sets `androidboot.veritymode=disabled`). The wizard logs both slots'
   build fingerprints and warns if they differ, since EchoMuse is tested on
   the active one.

### Reflashing a Dot that is already registered

Reflashing a Dot the controller already knows (moving one from Fire OS 5 to
Fire OS 6, say) keeps it as the same device. The controller keys everything it
holds for a Dot by `ro.serialno`: its name, settings, approval, Home Assistant
device and alarm calendar. That serial comes from the Dot's idme area, not
from Fire OS, so the wizard sees the same one after the flash. At its first
step the wizard names the device the serial belongs to and asks. **Continue**
reinstalls EchoMuse and the Dot rejoins as that device, already approved.
**Abort** stops before anything is written. To start over as a new device
instead, delete it from the dashboard and retry.

### What runs at boot

At every start, `start_server.sh` stops Amazon's Alexa, cloud, setup, OTA and
telemetry services and the daemons for the hardware the firmware drives. It
keeps the services the `mixer` depends on. The list and the measurements
behind it are in [fireos6-port.md](fireos6-port.md#41-service-denylist-measured-2026-10-05).
Nothing is uninstalled.

**To undo it**, remove both slots' hook and reboot:

```sh
adb shell 'stop echomuse; umount /tmp; mount -o rw,remount / && rm /system/etc/init/echomuse.rc && rmdir /tmp && mount -o ro,remount /'
adb shell 'S=$( [ "$(getprop ro.boot.slot_suffix)" = _a ] && echo _b || echo _a ); M=/data/local/tmp/sys; mkdir -p $M && mount -t ext4 /dev/block/platform/bootdevice/by-name/system$S $M && rm $M/system/etc/init/echomuse.rc && rmdir $M/tmp; umount $M; rmdir $M'
```

The Dot then comes back as stock Alexa.

## What EchoMuse writes, and what it does not

This device has several layers below the operating system, and EchoMuse only
ever writes the Fire OS one. Lowest first:

| Layer | What it is | Written by EchoMuse |
|---|---|---|
| Preloader | First stage of boot. Tracks boot attempts per slot. | No |
| LK (bootloader) | What `lk_build_desc` and `unlock_status` come from. amonet patches this. | No |
| amonet's unlock payload | Chainloads the real kernel. | No |
| TWRP (recovery) | | No |
| Fire OS kernel and ramdisk | The boot images of both slots. | No |
| `/system`, `/data` | Fire OS userspace. | Yes, files only: one init file and the empty `/tmp` in each slot's `/system`, the rest in `/data` |

## If a device will not boot

Anything at or below the bootloader is the unlock's territory, and
[R0rt1z2's thread](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/)
is the authority on it. One thing from that thread is worth repeating here
because it is time-critical:

> **Stop trying to boot it.** The preloader tracks boot attempts per slot, and
> if both slots run out of attempts the device stops booting altogether.

That state is recoverable, but the easy routes are gone: getting back in means
opening the case and shorting a pin on the board to reach the bootrom, which
R0rt1z2's thread documents and describes as not especially difficult. A device
sitting in fastboot or TWRP on the end of a cable needs none of that.
Repeatedly power cycling one that will not boot is what turns the first
situation into the second.

## How well tested is this?

The Fire OS 6 flow was developed and verified on hardware
([fireos6-port.md](fireos6-port.md), §6), on a handful of Dots of the same
model, by the same person. Treat it as encouraging rather than conclusive.

## Recovery

The wizard writes no partition, and the other slot stays stock until a boot
from the hooked slot has reached the controller, so a failed install falls
back to stock Alexa on the other slot. TWRP stays reachable: boot-root forces
adb, so `adb reboot recovery` works, and holding Volume Up while the Dot boots
(or the thread's `boot-recovery.sh`) enters it too. From TWRP, re-flashing
Fire OS 6 and boot-root.zip returns the Dot to the state the wizard starts
from. If it will not reach TWRP at all, that is the unlock's territory and
R0rt1z2's thread covers recovery and unbricking.

## Credits

- **R0rt1z2** — [amonet-biscuit](https://xdaforums.com/t/unlock-root-twrp-unbrick-amazon-echo-dot-2nd-gen-2016-biscuit.4761416/):
  persistent unlock, TWRP, boot-root and unbrick for this device
- **Dragon863** — [EchoCLI](https://github.com/Dragon863/EchoCLI): tethered root research
- **Binozo** — [GoTinyAlsa](https://github.com/Binozo/GoTinyAlsa) and the original EchoGo SDK

---

# Manual reference

The wizard performs the steps below for you. They are kept here for anyone
provisioning by hand, debugging a wizard step, or wanting to know exactly what
is being done to their device before letting something do it automatically.

The adb shell is already uid 0 (boot-root): run commands directly, with no
`su`. Fire OS 6 has only toybox, and `/sdcard` is a dangling link (there is no
framework to mount it), so stage pushes in `/data/local/tmp`.

## Step 1 — Install the boot hook on the running slot

Write `/system/etc/init/echomuse.rc` with exactly this content, **including the
trailing newline** (Android 7's init parser drops an unterminated last line,
which would be the `start echomuse` trigger):

```
service echomuse /system/bin/sh /data/local/bin/start_server.sh
    class late_start
    user root
    group aipc audio system inet wifi net_admin net_raw bluetooth net_bt_stack wakelock input
    seclabel u:r:adbd:s0
    disabled

on property:sys.boot_completed=1
    start echomuse
```

- **`group aipc` first.** The first group becomes the process's primary gid,
  and the `mixer` rejects streams from anything without gid `aipc` (2901).
- **`seclabel u:r:adbd:s0`.** Init refuses the transition into `su`; it may
  enter `adbd`, which boot-root made permissive. `start_server.sh` then moves
  itself into `su`, because `wpa_supplicant` does not reply to `adbd`.

```bash
adb push echomuse.rc /data/local/tmp/echomuse.rc.new
adb shell 'mount -o rw,remount / && cp /data/local/tmp/echomuse.rc.new /system/etc/init/echomuse.rc && chmod 0644 /system/etc/init/echomuse.rc && { [ -d /tmp ] || mkdir -Z u:object_r:system_file:s0 /tmp; } && chmod 0755 /tmp && sync; mount -o ro,remount /'
adb shell 'rm -f /data/local/tmp/echomuse.rc.new'
```

The argument order of the remount matters: `mount -o remount,rw /` fails on
this image.

## Step 2 — Install EchoMuse

EchoMuse runs as a Go binary on the device. It abstracts the hardware (mic,
speaker, LEDs, buttons) and connects outbound to the EchoMuse controller over
the v1 device link (plus a demand-opened shell plane). There is no HTTP server
on the device — no inbound ports, no iptables rules required.

### The binary, in A/B slots

EchoMuse uses A/B slots: `server_a` and `server_b` with `/data/local/bin/server`
as a symlink. This allows instant rollback without a binary transfer.

```bash
adb push server /data/local/tmp/server_new
adb shell 'mkdir -p /data/local/bin && rm -f /data/local/bin/server /data/local/bin/server_a /data/local/bin/server_b'
adb shell 'cp /data/local/tmp/server_new /data/local/bin/server_a && chmod 755 /data/local/bin/server_a && ln -sf server_a /data/local/bin/server && rm /data/local/tmp/server_new'
```

`server_b` starts empty. The first update from the dashboard populates it.
Use the controller's own `/app/firmware/server` (see
[What you need](#what-you-need)): the dashboard offers an update to any Dot
whose reported version differs from the one bundled with the controller.

### The startup script

The canonical script is **`controller/device_payloads/start_server.sh`** in the repo (`device/scripts/start_server.sh` is a symlink to it) — the controller serves that exact file at `/api/provision/start_script` (this is what the provisioning wizard installs), read from disk per request. Don't hand-maintain a copy.

```bash
# From the repo root:
adb push device/scripts/start_server.sh /data/local/tmp/start_server.sh
adb shell 'cp /data/local/tmp/start_server.sh /data/local/bin/start_server.sh && chmod 755 /data/local/bin/start_server.sh && rm /data/local/tmp/start_server.sh'
```

> The script moves itself into `su`, puts a RAM disk on `/tmp`, waits for
> `init.svc.mixer=running`, stops the [service denylist](fireos6-port.md#41-service-denylist-measured-2026-10-05),
> and then supervises the server. All server output is logged to
> `/tmp/server.log` (`adb shell cat /tmp/server.log`); the supervisor's own
> starts, exits and rollbacks also go to
> `/data/local/etc/echomuse/supervisor.log`, which survives a reboot.

> **Log cap:** `/tmp` is RAM-backed and the script only ever appends — a background loop in the script checks every 5 minutes and, past 5MB, keeps the newest 512KB in `/tmp/server.log.1` and truncates `server.log` in place (the server's `O_APPEND` fd continues at the new EOF). Total log footprint stays bounded at ~5.5MB.

> The script runs the server as a subprocess (not via `exec`) so SIGTERM can be forwarded from Android init via the `trap`. If the binary exits in under 15 seconds three times in a row, the inactive A/B slot is restored via symlink and the script exits cleanly — init restarts it with the old binary. If the binary runs for ≥15s before crashing, the attempt counter resets (operational crash, not a deployment failure).

### TLS credentials (optional)

The wizard mints a per-device token and installs it with the controller's CA
before the first connection (`POST /api/provision/tls_credentials` with
`{"device_id": "<ro.serialno>"}`, written as `ca.pem` and `token` into the
directory the reply names). Without them the Dot connects over plain `ws`;
the dashboard's **Secure link** action can add them later.

## Step 3 — Configure Wi-Fi

Write the network into `/data/misc/wifi/wpa_supplicant.conf` (owner
`wifi:wifi`, mode 0660). Nothing joins live: the firmware stops Amazon's
`wifisvc` and brings Wi-Fi up itself from this file at boot. The wizard writes
exactly this, with a single network replacing any Alexa-era entries (the
identity lines take the Dot's `ro.product.name`, `ro.product.manufacturer`,
`ro.product.model` and `ro.serialno`):

```
ctrl_interface=/data/misc/wifi/sockets
driver_param=use_p2p_group_interface=1
update_config=1
device_name=<ro.product.name>
manufacturer=<ro.product.manufacturer>
model_name=<ro.product.model>
model_number=<ro.product.model>
serial_number=<ro.serialno>
device_type=1-0050F204-9
os_version=01020300
config_methods=physical_display virtual_push_button
p2p_no_group_iface=1
external_sim=1
wowlan_triggers=disconnect
network={
	ssid="YourNetwork"
	psk="YourPassword"
	key_mgmt=WPA-PSK
	priority=1
}
```

Use `key_mgmt=NONE` (no `psk`) for an open network, and add `scan_ssid=1` for
a hidden one. Neither value may contain `"` or `\`.

```bash
adb push wpa_supplicant.conf /data/local/tmp/wpa_supplicant.conf.new
adb shell 'chmod 770 /data/misc/wifi && cp /data/local/tmp/wpa_supplicant.conf.new /data/misc/wifi/wpa_supplicant.conf && chown wifi:wifi /data/misc/wifi/wpa_supplicant.conf && chmod 660 /data/misc/wifi/wpa_supplicant.conf && rm /data/local/tmp/wpa_supplicant.conf.new'
```

## Step 4 — Reboot and verify

```bash
adb reboot
# after the boot completes:
adb shell getprop init.svc.echomuse    # Expected: running
adb shell getprop init.svc.mixer       # Expected: running
adb shell cat /tmp/server.log          # Expected: the server starting and finding the controller
```

The Dot then appears in the dashboard as pending approval.

## Step 5 — Mirror the hook to the other slot

Only once a boot from the hooked slot has reached the controller, write the
same file and directory to the other slot, so a bootloader fallback still runs
EchoMuse:

```bash
adb push echomuse.rc /data/local/tmp/echomuse.rc.new
adb shell 'S=$( [ "$(getprop ro.boot.slot_suffix)" = _a ] && echo _b || echo _a ); M=/data/local/tmp/sys; mkdir -p $M && mount -t ext4 /dev/block/platform/bootdevice/by-name/system$S $M && cp /data/local/tmp/echomuse.rc.new $M/system/etc/init/echomuse.rc && chmod 0644 $M/system/etc/init/echomuse.rc && { [ -d $M/tmp ] || mkdir -Z u:object_r:system_file:s0 $M/tmp; } && chmod 0755 $M/tmp && sync; umount $M; rmdir $M; rm -f /data/local/tmp/echomuse.rc.new'
```

---

## End State

```
✅ Persistent unlock (amonet-biscuit v2), Fire OS 6 on both slots, boot-root.zip (adb shell is uid 0)
✅ One init file plus an empty /tmp in each slot's /system; no partition or boot image written
✅ EchoMuse running as an init service on boot (uid 0, gid aipc), A/B binary slots, auto-rollback in start_server.sh
✅ Alexa, cloud, setup, OTA and telemetry services stopped at every boot; nothing uninstalled
✅ Wi-Fi, DHCP and NTP run by the firmware itself
✅ Microphone capture through Amazon's mixer (micAsr → the native front end → mono 16 kHz)
✅ Speaker output through the mixer, so the native front end keeps its echo reference
✅ LED ring (12 RGB LEDs), buttons, hardware mic mute
✅ No inbound ports — the device dials out to the controller: /device/v1/control, /device/v1/audio, /device/v1/assets, plus /shell on demand
✅ Device identity via ro.serialno; approval flow (strict or auto)
✅ Orange ring pulse while not connected; grey-white pulse while waiting for approval
```

From here the dashboard takes over: approve the device, then follow the
[quickstart](quickstart.md). What the ring shows in normal use is in
[led-ring-states.md](led-ring-states.md).
