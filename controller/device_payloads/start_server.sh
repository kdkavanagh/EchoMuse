#!/system/bin/sh
# EchoMuse start script — it handles A/B slots and rolls back automatically.
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

# ── Platform ─────────────────────────────────────────────────────────────────
# One script for every device, as the firmware is one binary
# (docs/echo-show-port-plan.md decision 1). An Echo Show 8 on LineageOS 18.1
# names itself in ro.lineage.version, tested first because ro.product.device
# reads crown on both Show images; the Fire OS Echo Show 8 names itself in
# ro.product.device; everything else takes the Echo Dot path, exactly as it
# ran before the Show existed. Only the boot preparation and the debloat
# differ: the supervisor at the bottom is shared.
#
# The server's output goes to $LOG: the Dot's /tmp is a tmpfs mounted below;
# the Show has no /tmp at all on either image (Fire OS's root is a read-only
# ramdisk rebuilt at every boot), so its logs and the server's scratch space
# live under /data/local/tmp/echomuse.
if [ -n "$(getprop ro.lineage.version)" ]; then
    PLATFORM="lineage"
    LOG_DIR=/data/local/tmp/echomuse
elif [ "$(getprop ro.product.device)" = crown ]; then
    PLATFORM="crown"
    LOG_DIR=/data/local/tmp/echomuse
else
    PLATFORM="dot"
    LOG_DIR=/tmp
fi
LOG=$LOG_DIR/server.log

# ── Service denylist (docs/fireos6-port.md §4.1) ─────────────────────────────
# Echo Dot only (the Show's is CROWN_STOPLIST below).
# No APKs on Fire OS 6: every entry is a native init service, stopped fresh
# each boot. Adding to this list means repeating §4.1's test on hardware
# first: stop it, then confirm micAsr still delivers audio and playback is
# still accepted. NEVER add anything from §4.1's keep list (mixer and the
# AIPC/shm/power transport it needs).
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

# ── Echo Show 8 (`crown`) on Fire OS: Alexa neutralisation ───────────────────
# docs/echo-show-port-plan.md Phases 3–4, docs/echo-show-crown.md §9. The
# Show runs the full Android framework, and its audio, Wi-Fi, Bluetooth and
# clock are Android's. Nothing in the crown path touches tinymix,
# wpa_supplicant, wifisvc, the mixer or the Dot's denylist above, and it
# NEVER stops audioserver or media: they own both PCMs and Amazon's AEC, and
# stopping them takes the ASR pipeline with them. None of this runs on
# LineageOS ($PLATFORM lineage), which has no Amazon apps or services.
#
# Alexa is APKs here, kept away from the hardware by `pm hide`. The wizard's
# Neutralise Alexa step pushes the hide lists into $EM_ETC, hides them, and
# records the tiers it applied in $CROWN_TIERS: "1", or "1 2" when the
# operator opted into Tier 2. Every boot (and `start_server.sh debloat`)
# re-hides any listed package that has become visible again: hiding
# persists, but a package holding CHANGE_COMPONENT_ENABLED_STATE could undo
# it before being hidden itself. Undo: `pm unhide` each listed package, then
# delete $CROWN_TIERS so nothing re-hides them.
#
# Some Amazon packages cannot be hidden without breaking SystemUI or
# Settings (crown_hide_packages.txt's header). While Tier 1 is applied,
# the ones that could record keep their microphone denied by app op
# ($CROWN_MIC_DENY), re-asserted the same way. Undo:
# `appops set <package> RECORD_AUDIO default`.
EM_ETC=/data/local/etc/echomuse
CROWN_TIERS=$EM_ETC/crown_tiers
CROWN_MIC_DENY=$EM_ETC/crown_mic_deny_packages.txt

# Native init services stopped at every boot (findings §9.2's table). `stop`
# does not persist, so /system keeps no edits for them, and a boot without
# this script brings them back. Adding one means repeating findings §9.4's
# verification on a device first. NEVER anything from §9.3's keep list:
# audioserver, media, cameraserver, aal, wpa_supplicant, p2p_supplicant,
# netd, dhcpcd, wmt_launcher, perfmonitord, perfrecoveryd, securetime, kisd,
# healthd.
CROWN_STOPLIST="ahe shchipd shblemeshd shlocalskillsd smarthomewifid halo-bssid-scan halo-hub-broker meshmgrservice neo-coordinator neo-init inlocservice trackerd touchlogger dha_service wifi_log_levels whad_cc"

crown_stop_services() {
    for svc in $CROWN_STOPLIST; do
        stop "$svc" 2>/dev/null
    done
}

# The stop list's state, read back in fos6_debloat_check's shape.
crown_stop_check() {
    STOPPED=0
    UP=""
    for svc in $CROWN_STOPLIST; do
        case "$(getprop "init.svc.$svc")" in
            running|restarting) UP="$UP,$svc" ;;
            *) STOPPED=$((STOPPED + 1)) ;;
        esac
    done
    UP=${UP#,}
}

# The tiers the wizard applied, into TIERS ("1", "1 2", or empty).
crown_tiers() {
    TIERS=""
    if [ -f "$CROWN_TIERS" ]; then
        read -r TIERS < "$CROWN_TIERS"
    fi
}

# The hide lists of the tiers the wizard applied, one path per line.
crown_hide_lists() {
    crown_tiers
    for tier in $TIERS; do
        case "$tier" in
            1) echo "$EM_ETC/crown_hide_packages.txt" ;;
            2) echo "$EM_ETC/crown_hide_packages_tier2.txt" ;;
        esac
    done
}

# Hides every listed package the package manager shows again. REHID names
# the packages hidden by this pass, VISIBLE those pm refused to hide; both
# comma-separated, normally empty. A package this build does not carry is
# neither: `pm list packages` never shows it. Lists are one package per line,
# `#` comments (device_payloads/crown_hide_packages*.txt).
crown_rehide() {
    REHID=""
    VISIBLE=""
    LISTS=$(crown_hide_lists)
    [ -n "$LISTS" ] || return 0
    # This is framed by newlines, so a case pattern matches one whole line and
    # com.amazon.device.software.ota never matches its .override.
    SHOWN="
$(pm list packages 2>/dev/null)
"
    for list in $LISTS; do
        [ -f "$list" ] || continue
        while read -r pkg _rest; do
            case "$pkg" in ''|'#'*) continue ;; esac
            case "$SHOWN" in
                *"
package:$pkg
"*) ;;
                *) continue ;;
            esac
            # </dev/null: pm must not read the rest of the list as its stdin.
            case "$(pm hide "$pkg" </dev/null 2>&1)" in
                *"hidden state: true"*) REHID="$REHID,$pkg" ;;
                *) VISIBLE="$VISIBLE,$pkg" ;;
            esac
        done < "$list"
    done
    REHID=${REHID#,}
    VISIBLE=${VISIBLE#,}
}

# Denies the microphone to every installed package on $CROWN_MIC_DENY whose
# RECORD_AUDIO app op is not already `ignore`, while Tier 1 is applied.
# MUTED names the packages denied by this pass, ALLOWED those still allowed
# after it; both comma-separated, normally empty: the mode persists, so a
# boot after the wizard's finds nothing to do. `appops`, like `pm`, needs
# the framework up.
crown_mic_deny() {
    MUTED=""
    ALLOWED=""
    crown_tiers
    case " $TIERS " in *" 1 "*) ;; *) return 0 ;; esac
    [ -f "$CROWN_MIC_DENY" ] || return 0
    INSTALLED="
$(pm list packages -u 2>/dev/null)
"
    while read -r pkg _rest; do
        case "$pkg" in ''|'#'*) continue ;; esac
        case "$INSTALLED" in
            *"
package:$pkg
"*) ;;
            *) continue ;;
        esac
        # </dev/null: appops must not read the rest of the list as its stdin.
        case "$(appops get "$pkg" RECORD_AUDIO </dev/null 2>&1)" in
            *"RECORD_AUDIO: ignore"*) continue ;;
        esac
        appops set "$pkg" RECORD_AUDIO ignore </dev/null >/dev/null 2>&1
        case "$(appops get "$pkg" RECORD_AUDIO </dev/null 2>&1)" in
            *"RECORD_AUDIO: ignore"*) MUTED="$MUTED,$pkg" ;;
            *) ALLOWED="$ALLOWED,$pkg" ;;
        esac
    done < "$CROWN_MIC_DENY"
    MUTED=${MUTED#,}
    ALLOWED=${ALLOWED#,}
}

# ── `start_server.sh debloat` ─────────────────────────────────────────────────
# Re-apply debloat: stop the denylist now, report, and exit. Nothing else in
# this script runs — no SELinux move, no tmpfs, no mixer wait, no
# supervisor — so the live server is untouched. The last line is the
# controller's result:
#   DEBLOAT_STOPPED:<listed services not running> STILL_RUNNING:<a,b,…>
# On the Show it re-hides the applied lists, re-denies the microphones and
# stops CROWN_STOPLIST instead, and reports the first two on the lines
# before:
#   HIDE_REASSERTED:<re-hidden a,b,…> STILL_VISIBLE:<a,b,…>
#   MIC_REASSERTED:<re-denied a,b,…> STILL_ALLOWED:<a,b,…>
# LineageOS has no Amazon apps or services (docs/echo-show-lineage-plan.md
# decision 10): nothing is listed, so nothing is stopped, and the result
# says so. The controller never asks there (em_device.debloat_gap).
if [ "$1" = "debloat" ] && [ "$PLATFORM" = lineage ]; then
    echo "DEBLOAT_STOPPED:0 STILL_RUNNING:"
    exit 0
fi
if [ "$1" = "debloat" ] && [ "$PLATFORM" = crown ]; then
    crown_rehide
    crown_mic_deny
    crown_stop_services
    sleep 1
    crown_stop_check
    if [ -n "$UP" ]; then
        echo "DEBLOAT_WAITING:$UP"
        sleep 2
        crown_stop_check
    fi
    echo "HIDE_REASSERTED:$REHID STILL_VISIBLE:$VISIBLE"
    echo "MIC_REASSERTED:$MUTED STILL_ALLOWED:$ALLOWED"
    echo "DEBLOAT_STOPPED:$STOPPED STILL_RUNNING:$UP"
    exit 0
fi
if [ "$1" = "debloat" ]; then
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

# init refuses to start a service in boot-root's `su` domain, so echomuse.rc
# starts this script in `adbd`, which boot-root allows to move itself into
# `su` (`allow adbd su process dyntransition`). The move is made here, in
# this single-threaded shell, and the server inherits it: in `adbd`
# wpa_supplicant's replies are denied (`avc: denied { sendto } … scontext=
# u:r:wpa:s0 tcontext=u:r:adbd:s0`), so the firmware could not drive Wi-Fi.
# `echo` is a shell builtin, so /proc/self is this shell itself.
# The Show's echomuse.rc takes the same way in, and needs the same move
# (measured 2026-10-17, docs/echo-show-crown.md §10.4): in `adbd`,
# audioserver's binder replies are refused (`transaction failed 29201`), so
# the OpenSL recorder cannot realize and `pm` lists nothing; after the move
# capture, `pm` and `service call audio` all work.
# LineageOS needs no move: its echomuse.rc has init start this script in
# `su` directly (seclabel u:r:su:s0, measured, docs/echo-show-crown.md §8.5).
if [ "$PLATFORM" != lineage ]; then
    echo -n u:r:su:s0 > /proc/self/attr/current
fi

# ── Echo Dot boot preparation ─────────────────────────────────────────────────
dot_boot() {
    # Fire OS 6 has no /tmp, and everything below logs there. The wizard's
    # install_boot_hook creates the empty mountpoint on the read-only root; a
    # tmpfs goes on it here, before the first write. If the redirect target were
    # missing, `server >> /tmp/server.log` would not run the server at all.
    if ! grep -q " /tmp " /proc/mounts; then
        mount -t tmpfs -o mode=1777,size=32m tmpfs /tmp
    fi

    # ── Wait for the audio service (up to 4 minutes) ─────────────────────────
    # Amazon's `mixer` daemon (init service `mixer`) owns the AFE — there is no
    # AudioFlinger at all (docs/fireos6-port.md §4 Phase 2).
    i=0
    while [ $i -lt 120 ]; do
        state=$(getprop init.svc.mixer)
        if [ "$state" = "running" ]; then
            sleep 5
            break
        fi
        sleep 2
        i=$((i + 2))
    done

    # ── Hardware init ─────────────────────────────────────────────────────────
    # No `ip` binary on Fire OS 6 (docs/fireos6-port.md §2 evidence).
    ifconfig p2p0 down

    # ── Service denylist (FOS6_DENYLIST, top of this script) ─────────────────
    fos6_debloat

    # Prevent WiFi suspension
    echo "EchoMuse" > /sys/power/wake_lock

    # Speaker mixer init
    tinymix -D 0 56 On
    tinymix -D 0 64 1 1
    tinymix -D 0 88 On
    tinymix -D 0 61 100 100

    # Mic gain — equalised across all four ADCs (A/B/C/D).
    #
    # Kept even though the audio HAL, driven by Amazon's mixer daemon, owns the
    # mic chain now (its own PGA plus the AFE's output gain —
    # docs/native-afe-migration.md's bypass table).
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
}

# ── Echo Show 8 boot preparation ──────────────────────────────────────────────
# Both Show images: scratch space, the audio service, the wake lock. Fire OS
# also stops its native stop list here, and re-asserts the hiding further
# down, once sup_log exists, and in the background: it waits on the package
# manager, and the server need not. LineageOS has no Amazon apps or services
# (docs/echo-show-lineage-plan.md decision 10): no stop list, no hide lists,
# no microphone denials.
show_boot() {
    mkdir -p "$LOG_DIR"
    # The firmware's scratch space; Go's os.TempDir() reads it.
    export TMPDIR=$LOG_DIR

    # ── Wait for the audio service (up to 4 minutes) ─────────────────────────
    # audioserver owns AudioFlinger, the vendor HAL and libasp.so (findings
    # §4.1); the firmware records and plays through it (OpenSL on Fire OS,
    # AAudio on LineageOS).
    i=0
    while [ $i -lt 120 ]; do
        if [ "$(getprop init.svc.audioserver)" = "running" ]; then
            break
        fi
        sleep 2
        i=$((i + 2))
    done

    # Belt and braces: the open recorder already holds Android's `AudioIn`
    # wake lock, which kept a screen-off Show from suspending (measured,
    # findings §10). This one also covers the gaps while capture re-opens.
    echo "EchoMuse" > /sys/power/wake_lock
}

crown_boot() {
    show_boot
    crown_stop_services
}

case "$PLATFORM" in
    lineage) show_boot ;;
    crown)   crown_boot ;;
    *)       dot_boot ;;
esac

# ── Log size cap ──────────────────────────────────────────────────────────────
# $LOG only ever grows: everything below only ever appends — without a cap
# server.log grows until reboot on the Dot's RAM-backed /tmp (45MB observed,
# 2026-07-07), and on the Show's /data without end. Past MAX_LOG,
# keep the newest KEEP_LOG bytes in server.log.1 and truncate in place; the
# server's O_APPEND fd (from >>) just continues writing at the new EOF, so
# no restart is needed and total log footprint stays bounded at ~5.5MB.
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
# On the Dot everything above logs to /tmp, which is RAM-backed and therefore
# wiped by the reboot you perform to recover from a device that will not come
# back.
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

# ── Fire OS Show: re-assert the hide lists and mic denials ───────────────────
# crown_rehide and crown_mic_deny. The service starts on sys.boot_completed,
# and the package manager answers shortly after; `pm path android` printing
# a path is the readiness test, and `appops` (app_process, like `pm`) is
# ready with it. Logged only when it had to act, which means something
# unhid a package or allowed a microphone again. Not on LineageOS: nothing
# there is Amazon's.
if [ "$PLATFORM" = crown ]; then
    (
        i=0
        while [ $i -lt 120 ]; do
            case "$(pm path android 2>/dev/null)" in
                package:*) break ;;
            esac
            sleep 5
            i=$((i + 5))
        done
        crown_rehide
        if [ -n "$REHID$VISIBLE" ]; then
            sup_log "crown hide lists: re-hid ${REHID:-none}; pm refused ${VISIBLE:-none}"
        fi
        crown_mic_deny
        if [ -n "$MUTED$ALLOWED" ]; then
            sup_log "crown mic deny: re-denied ${MUTED:-none}; still allowed ${ALLOWED:-none}"
        fi
    ) &
fi

# ── Amp safety ────────────────────────────────────────────────────────────────
# Mute + amp off whenever the server is not running. An enabled amp on an
# idle DAC produces audible hiss for as long as the server is down (between
# OTA slots was the worst case). This is the ONLY thing that does it: the
# speaker backend deliberately leaves the amp/mute controls alone on close,
# because the mixer daemon owns the PCM and the HAL reacts to the same
# controls (see slspeaker.MixerSink.Close). Idempotent; the server re-enables
# the amp in its own startup sequence.
# Echo Dot only: on the Show (either image) audioserver owns the codec and
# its controls have other meanings (findings §3), so there is nothing here
# to switch off.
amp_off() {
    if [ "$PLATFORM" != dot ]; then
        return 0
    fi
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

    /data/local/bin/server >> "$LOG" 2>&1 &
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
        echo "[start_server] Server ran ${RUNTIME}s before exit (code $EXIT_CODE) — restarting" >> "$LOG"
        sup_log "exit pid=$SERVER_PID code=$EXIT_CODE ran=${RUNTIME}s — restarting"
        sleep 2
        continue
    fi

    attempt=$(( attempt + 1 ))
    echo "[start_server] Fast exit ${attempt}/${MAX_ATTEMPTS}: runtime=${RUNTIME}s exit=$EXIT_CODE" >> "$LOG"
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
            echo "[start_server] Unknown slot '$CURRENT' — cannot auto-rollback, giving up" >> "$LOG"
            sup_log "giving up — unknown slot '$CURRENT', supervisor exiting"
            exit 1
            ;;
    esac

    # Verify fallback slot exists and is executable before committing
    if [ ! -x "/data/local/bin/$FALLBACK" ]; then
        echo "[start_server] Fallback slot $FALLBACK missing or not executable — cannot auto-rollback" >> "$LOG"
        sup_log "giving up — fallback $FALLBACK missing, supervisor exiting"
        exit 1
    fi

    echo "[start_server] Auto-rollback: $CURRENT → $FALLBACK after $MAX_ATTEMPTS failed starts" >> "$LOG"
    ln -sf "$FALLBACK" /data/local/bin/server
    sup_log "rollback $CURRENT -> $FALLBACK after $MAX_ATTEMPTS fast exits, supervisor exiting for init to restart it"

    # Exit cleanly — Android init will restart the service, now using $FALLBACK
    exit 0
done
