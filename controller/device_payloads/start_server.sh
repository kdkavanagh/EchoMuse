#!/system/bin/sh
# EchoMuse start script — A/B slot aware with auto-rollback.
#
# Retry policy: if the server exits in under MIN_RUNTIME seconds,
# it counts as a failed start. After MAX_ATTEMPTS consecutive fast
# exits the inactive slot is restored via symlink and the script
# exits cleanly so Android init restarts it with the old binary.
#
# If the server runs for >= MIN_RUNTIME seconds, the attempt counter
# resets — this was a successful start that crashed later (operational
# failure, not deployment failure), so we just restart without rollback.

MAX_ATTEMPTS=3
MIN_RUNTIME=15   # seconds below which an exit is treated as a failed start

# ── Platform ──────────────────────────────────────────────────────────────────
# Same test as device/internal/platform.FireOS6() and the echomuse.rc
# seclabel decision (docs/fireos6-port.md §3 decision 4). Fire OS 5 below
# must stay byte-for-byte what it is today.
FIREOS6=0
if [ "$(getprop ro.build.version.sdk)" -ge 25 ]; then
    FIREOS6=1
fi

# ── Fire OS 6 service denylist (docs/fireos6-port.md §4.1) ───────────────────
# No APKs on Fire OS 6, so there is no `pm hide` counterpart — every entry is
# a native init service, stopped fresh each boot. Replaces both
# debloat_packages.txt and echomuse-debloat.sh, which apply only to Fire OS
# 5's package list. Adding to this list means repeating §4.1's test on
# hardware first: stop it, then confirm micAsr still delivers audio and
# playback is still accepted. NEVER add anything from §4.1's keep list
# (mixer and the AIPC/shm/power transport it needs).
#
# sntpd follows wifisvc: it waits on ACE NetMgr (/dev/aipc/1, served by
# wifisvc), exits, and init restarts it every ~20 s forever; the firmware
# syncs the clock itself. Never `time_update` — it restores the saved time
# at boot.
#
# This is the only copy of the list: boot applies it below, and the
# dashboard's Re-apply debloat runs `start_server.sh debloat`.
FOS6_DENYLIST="puffin puffinmrmd ahe shs dacd smarthomed commsd uxeventd amakit_server tokend credmgrsvc trackerd UdssCampSvc ace_dioded oobed_on_boot provisionerd otad ace_otad factory-reset perfrecoveryd ace_metricd logmgr acedropboxd aceusagestatd ace_coex_metric dha_service ledcontroller acebuttond aceinputmanager ace_sensorsd btmanagerd BTSinkPlayer blemesh_service wifisvc sntpd avahi-daemon"

fos6_debloat() {
    for svc in $FOS6_DENYLIST; do
        stop "$svc" 2>/dev/null
    done
    # oobed_on_boot spawns its own dnsmasq outside init — `stop` can't reach it.
    kill $(pidof dnsmasq) 2>/dev/null
}

# Reads the denylist's state back: STOPPED counts listed services init does
# not report running or restarting; UP names the rest, plus a surviving
# dnsmasq, comma-separated (empty when nothing is up).
fos6_debloat_check() {
    STOPPED=0
    UP=""
    for svc in $FOS6_DENYLIST; do
        case "$(getprop init.svc.$svc)" in
            running|restarting) UP="$UP,$svc" ;;
            *) STOPPED=$((STOPPED + 1)) ;;
        esac
    done
    if pidof dnsmasq >/dev/null 2>&1; then
        UP="$UP,dnsmasq"
    fi
    UP=${UP#,}
}

# ── `start_server.sh debloat` ─────────────────────────────────────────────────
# Fire OS 6's Re-apply debloat: stop the denylist now, report, and exit.
# Nothing else in this script runs — no SELinux move, no tmpfs, no mixer
# init, no supervisor — so the live server is untouched. The last line is
# the controller's result:
#   DEBLOAT_STOPPED:<listed services not running> STILL_RUNNING:<a,b,…>
# Fire OS 5's debloat is the Magisk service.d script plus `pm hide`, which
# the controller applies itself, so the mode refuses there.
if [ "$1" = "debloat" ]; then
    if [ "$FIREOS6" != "1" ]; then
        echo "start_server.sh debloat: Fire OS 6 only — Fire OS 5's debloat is echomuse-debloat.sh plus pm hide" >&2
        exit 2
    fi
    fos6_debloat
    # `stop` only asks init; give it a moment before reading state back.
    # The WAITING line also keeps a reader that times out on silence alive.
    sleep 1
    fos6_debloat_check
    if [ -n "$UP" ]; then
        echo "DEBLOAT_WAITING:$UP"
        sleep 2
        fos6_debloat_check
    fi
    echo "DEBLOAT_STOPPED:$STOPPED STILL_RUNNING:$UP"
    exit 0
fi

# Fire OS 6: init refuses to start a service in boot-root's `su` domain, so
# echomuse.rc starts this script in `adbd`, which boot-root allows to move
# itself into `su` (`allow adbd su process dyntransition`). The move is made
# here, in this single-threaded shell, and the server inherits it: in `adbd`
# wpa_supplicant's replies are denied (`avc: denied { sendto } … scontext=
# u:r:wpa:s0 tcontext=u:r:adbd:s0`), so the firmware could not drive Wi-Fi.
# `echo` is a shell builtin, so /proc/self is this shell itself.
if [ "$FIREOS6" = "1" ]; then
    echo -n u:r:su:s0 > /proc/self/attr/current
fi

# Fire OS 6 has no /tmp, and everything below logs there (RAM-backed on
# Fire OS 5 too). The wizard's install_boot_hook creates the empty mountpoint
# on the read-only root; a tmpfs goes on it here, before the first write. If
# the redirect target were missing, `server >> /tmp/server.log` would not run
# the server at all.
if [ "$FIREOS6" = "1" ] && ! grep -q " /tmp " /proc/mounts; then
    mount -t tmpfs -o mode=1777,size=32m tmpfs /tmp
fi

# ── Wait for the audio service (up to 4 minutes) ─────────────────────────────
# Fire OS 5: echoaudioservice owns AudioFlinger's HAL. Fire OS 6: Amazon's
# `mixer` daemon (init service `mixer`) owns the AFE instead — there is no
# AudioFlinger at all (docs/fireos6-port.md §4 Phase 2).
i=0
while [ $i -lt 120 ]; do
    if [ "$FIREOS6" = "1" ]; then
        state=$(getprop init.svc.mixer)
        if [ "$state" = "running" ]; then
            sleep 5
            break
        fi
    else
        pid=$(ps | grep echoaudio | grep -v grep)
        if [ -n "$pid" ]; then
            sleep 5
            break
        fi
    fi
    sleep 2
    i=$((i + 2))
done

# ── Hardware init ─────────────────────────────────────────────────────────────
if [ "$FIREOS6" = "1" ]; then
    # No `ip` binary on Fire OS 6 (docs/fireos6-port.md §2 evidence).
    ifconfig p2p0 down
else
    ip link set p2p0 down
fi

# ── Fire OS 6 service denylist (FOS6_DENYLIST, top of this script) ───────────
if [ "$FIREOS6" = "1" ]; then
    fos6_debloat
fi

# Prevent WiFi suspension
echo "EchoMuse" > /sys/power/wake_lock

# Speaker mixer init
tinymix -D 0 56 On
tinymix -D 0 64 1 1
tinymix -D 0 88 On
tinymix -D 0 61 100 100

# Mic gain — equalised across all four ADCs (A/B/C/D).
#
# Kept even though Android's audio HAL owns the mic chain now (its own PGA
# plus the AFE's output gain — docs/native-afe-migration.md's bypass table).
# Whether the HAL rewrites these four pairs when it opens the input, or
# inherits whatever it finds, is unverified on hardware; if it inherits, this
# is the only thing equalising the four ADCs, and if it rewrites, this costs
# eight tinymix calls at boot. Removing it is a measurement away, not a
# guess.
tinymix -D 0 89 88 88
tinymix -D 0 92 40 40
tinymix -D 0 107 88 88
tinymix -D 0 110 40 40
tinymix -D 0 125 88 88
tinymix -D 0 128 40 40
tinymix -D 0 143 88 88
tinymix -D 0 146 40 40

kill $(ps | grep ledcontroller | grep -v grep) 2>/dev/null

# ── Log size cap ──────────────────────────────────────────────────────────────
# /tmp is RAM-backed and everything below only ever appends — without a cap
# server.log grows until reboot (45MB observed, 2026-07-07). Past MAX_LOG,
# keep the newest KEEP_LOG bytes in server.log.1 and truncate in place; the
# server's O_APPEND fd (from >>) just continues writing at the new EOF, so
# no restart is needed and total log footprint stays bounded at ~5.5MB.
LOG=/tmp/server.log
MAX_LOG=5242880    # 5MB
KEEP_LOG=524288    # 512KB carried into server.log.1
(
    while true; do
        sleep 300
        SIZE=$(wc -c < "$LOG" 2>/dev/null)
        if [ -n "$SIZE" ] && [ "$SIZE" -gt $MAX_LOG ]; then
            tail -c $KEEP_LOG "$LOG" > "${LOG}.1" 2>/dev/null
            : > "$LOG"
            echo "[start_server] Log trimmed: ${SIZE} bytes (tail kept in ${LOG}.1)" >> "$LOG"
        fi
    done
) &
TRIM_PID=$!

# ── Persistent supervisor log ────────────────────────────────────────────────
# Everything above logs to /tmp, which is RAM-backed and therefore wiped by
# the reboot you perform to recover from a device that will not come back.
# The evidence needed to diagnose a failed restart is destroyed by the act of
# recovering from it (learned the hard way 2026-08-01).
#
# So the SUPERVISOR's own decisions — and only those — also go to /data, which
# survives reboots and OTA slot flips. Not the server's output: that is 45MB a
# day and none of it is what you need. This is a handful of lines per boot.
#
# Timestamps are SECONDS SINCE BOOT, not wall clock. Echos boot with bogus
# clocks before NTP — the same reason TLS verification clamps to the firmware
# build time — and a boot-time log is precisely where the wall clock is least
# trustworthy. The wall clock is recorded alongside as a hint only.
SUP_LOG=/data/local/etc/echomuse/supervisor.log
SUP_MAX=65536      # 64KB cap — this must never be able to fill /data
SUP_KEEP=32768     # bytes retained when trimming

mkdir -p /data/local/etc/echomuse 2>/dev/null

sup_log() {
    # Trim BEFORE appending, so a crash-loop writing every few seconds can
    # never grow past the cap between checks.
    # -f guard, not just 2>/dev/null: the redirect failure is reported by the
    # shell before wc runs, so a missing file would print to stderr on the
    # first write of every boot.
    if [ -f "$SUP_LOG" ]; then
        SIZE=$(wc -c < "$SUP_LOG" 2>/dev/null)
    else
        SIZE=0
    fi
    if [ "$SIZE" -gt $SUP_MAX ]; then
        tail -c $SUP_KEEP "$SUP_LOG" > "${SUP_LOG}.tmp" 2>/dev/null
        mv "${SUP_LOG}.tmp" "$SUP_LOG" 2>/dev/null
    fi
    # Shell built-ins only. `cut` is not on this device's PATH — the first
    # version of this used it and logged an empty uptime, quietly losing the
    # one timestamp that is trustworthy at boot.
    UP=""
    if [ -r /proc/uptime ]; then
        read UP _REST < /proc/uptime 2>/dev/null
        UP=${UP%.*}
    fi
    echo "up=${UP}s wall=$(date '+%Y-%m-%d %H:%M:%S') $*" >> "$SUP_LOG"
}

sup_log "boot slot=$(readlink /data/local/bin/server 2>/dev/null)"

# ── Amp safety ────────────────────────────────────────────────────────────────
# Mute + amp off whenever the server is not running. An enabled amp on an
# idle DAC produces audible hiss for as long as the server is down (between
# OTA slots was the worst case). This is the ONLY thing that does it: the
# speaker backend deliberately leaves the amp/mute controls alone on close,
# because mediaserver owns the PCM and Android's HAL reacts to the same
# controls (see slspeaker.Close). Idempotent; the server re-enables the amp
# in its own startup sequence.
amp_off() {
    tinymix -D 0 61 0 0 2>/dev/null
    tinymix -D 0 5 Off 2>/dev/null
}

# ── Signal handling ───────────────────────────────────────────────────────────
# Forward SIGTERM/SIGINT to the server subprocess so Android init can
# cleanly stop the service (exec is no longer used, so init signals us).
# Wait for the server to finish its own graceful shutdown, then amp_off
# as belt-and-braces. The log-trim loop dies with us too.
SERVER_PID=0
trap 'sup_log "term signalled — supervisor exiting"; kill $SERVER_PID $TRIM_PID 2>/dev/null; wait $SERVER_PID 2>/dev/null; amp_off; exit 0' TERM INT

# ── Retry loop with auto-rollback ─────────────────────────────────────────────
attempt=0

while true; do
    START_TIME=$(date +%s)

    /data/local/bin/server >> /tmp/server.log 2>&1 &
    SERVER_PID=$!
    sup_log "start pid=$SERVER_PID slot=$(readlink /data/local/bin/server 2>/dev/null)"
    wait $SERVER_PID
    EXIT_CODE=$?

    # Server is down — silence the amp until the next start (or forever,
    # if this turns out to be the rollback/give-up path).
    amp_off

    END_TIME=$(date +%s)
    RUNTIME=$(( END_TIME - START_TIME ))

    if [ $RUNTIME -ge $MIN_RUNTIME ]; then
        # Ran long enough — not a deployment failure.
        # Reset counter and restart (handles operational crashes).
        attempt=0
        echo "[start_server] Server ran ${RUNTIME}s before exit (code $EXIT_CODE) — restarting" >> /tmp/server.log
        sup_log "exit pid=$SERVER_PID code=$EXIT_CODE ran=${RUNTIME}s — restarting"
        sleep 2
        continue
    fi

    attempt=$(( attempt + 1 ))
    echo "[start_server] Fast exit ${attempt}/${MAX_ATTEMPTS}: runtime=${RUNTIME}s exit=$EXIT_CODE" >> /tmp/server.log
    sup_log "fast-exit ${attempt}/${MAX_ATTEMPTS} code=$EXIT_CODE ran=${RUNTIME}s"

    if [ $attempt -lt $MAX_ATTEMPTS ]; then
        sleep 3
        continue
    fi

    # ── Auto-rollback ─────────────────────────────────────────────────────────
    CURRENT=$(readlink /data/local/bin/server 2>/dev/null)
    case "$CURRENT" in
        server_a) FALLBACK=server_b ;;
        server_b) FALLBACK=server_a ;;
        *)
            echo "[start_server] Unknown slot '$CURRENT' — cannot auto-rollback, giving up" >> /tmp/server.log
            sup_log "giving up — unknown slot '$CURRENT', supervisor exiting"
            exit 1
            ;;
    esac

    # Verify fallback slot exists and is executable before committing
    if [ ! -x "/data/local/bin/$FALLBACK" ]; then
        echo "[start_server] Fallback slot $FALLBACK missing or not executable — cannot auto-rollback" >> /tmp/server.log
        sup_log "giving up — fallback $FALLBACK missing, supervisor exiting"
        exit 1
    fi

    echo "[start_server] Auto-rollback: $CURRENT → $FALLBACK after $MAX_ATTEMPTS failed starts" >> /tmp/server.log
    ln -sf "$FALLBACK" /data/local/bin/server
    sup_log "rollback $CURRENT -> $FALLBACK after $MAX_ATTEMPTS fast exits, supervisor exiting for init to restart it"

    # Exit cleanly — Android init will restart the service, now using $FALLBACK
    exit 0
done
