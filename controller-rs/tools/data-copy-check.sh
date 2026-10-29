#!/usr/bin/env bash
# Runs this controller on a private copy of a real EchoMuse data directory and checks that
# it keeps the installation as it is (docs/rust-controller-evaluation.md §3.5, §5.6
# "Database and file preservation"):
#
#   1. copies the data directory (the database through SQLite's backup API on a read-only
#      mount, so a live installation is never written to) into a scratch directory;
#   2. runs the release binary on the copy with the real speech bundle and a simulated Home
#      Assistant; the test `data_copy_check` in tests/voice.rs reads the dashboard API over
#      every stored row and file, drives a simulated speaker with a copied device's id and
#      token through a wake turn on one of the copy's wake recordings, and compares every
#      row the controller wrote with the stored rows column by column;
#   3. restarts it, checking that the cutover backup is not rewritten;
#   4. stops it and runs the previously deployed image on the same copy as a black box
#      (rollback, §3.5.1 rule 9), reading what the Rust controller wrote through its API;
#   5. runs this tree's image (controller-rs/Dockerfile) the same way on a second copy.
#
# Isolation is by construction. The copy holds the real devices' tokens, and a controller
# advertises itself over mDNS and dials Home Assistant, so every process that touches the
# copy runs inside one Docker network namespace created with `--network none`: it has only
# a loopback interface and no route anywhere. The controllers, the simulated Home Assistant
# and the checks join that namespace (`--network container:`). Programs built on this
# machine (the release binary, the test, curl, sqlite3, openssl) run in it through a chroot
# into a read-only view of this machine's root, as the invoking user, with all capabilities
# dropped except those chroot needs and with /run (the Docker socket) hidden. The isolation
# evidence (`ip addr`, the routing tables, a refused connection) is printed first.
#
# Usage: tools/data-copy-check.sh [--keep] [--skip-build] [--skip-image-build]
#                                 [--skip-rollback] DATA_DIR
#
#   --keep               keep the scratch directory (it holds private data: delete it)
#   --skip-build         use the release binary and test already built in the target
#                        directory instead of running cargo
#   --skip-image-build   use the existing $RS_IMAGE instead of building it
#   --skip-rollback      skip the previous image's black-box run
#
# Environment:
#   ROLLBACK_IMAGE   the previously deployed image (default hass-echomuse-controller:latest)
#   RS_IMAGE         the tag this tree's image is built as (default
#                    echomuse-controller-rs:data-copy-check)
#   SCRATCH_ROOT     where the scratch directory is made (default /var/tmp)
#   ECHOMUSE_DCC_TURN  <device id>/<turn id>: the stored wake turn whose recording is played
#   CARGO_TARGET_DIR   as for cargo
#
# Needs: docker, cargo (with tools/fetch-native.sh, tools/fetch-speech-assets.sh and
# tools/build-dashboard.sh already run), sqlite3, jq, curl, openssl and ffmpeg on this
# machine. Exit status 0 when every check passed.

# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

KEEP=0
BUILD=1
BUILD_IMAGE=1
ROLLBACK=1
while [ $# -gt 0 ]; do
    case "$1" in
        --keep) KEEP=1 ;;
        --skip-build) BUILD=0 ;;
        --skip-image-build) BUILD_IMAGE=0 ;;
        --skip-rollback) ROLLBACK=0 ;;
        -h | --help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        -*) die "unknown option $1" ;;
        *) break ;;
    esac
    shift
done
[ $# -eq 1 ] || die "usage: $0 [--keep] [--skip-build] [--skip-image-build] [--skip-rollback] DATA_DIR"
SRC=$(cd "$1" && pwd) || die "$1 is not a directory"
[ -f "$SRC/echomuse.db" ] || die "$SRC has no echomuse.db"

need docker sqlite3 jq curl openssl sha256sum od
[ "$BUILD" = 0 ] || need cargo
ROLLBACK_IMAGE=${ROLLBACK_IMAGE:-hass-echomuse-controller:latest}
RS_IMAGE=${RS_IMAGE:-echomuse-controller-rs:data-copy-check}
TARGET=${CARGO_TARGET_DIR:-$RS_DIR/target}
TZ_NAME=${TZ:-$(cat /etc/timezone 2>/dev/null || echo UTC)}
USER_ID="$(id -u):$(id -g)"
TEST_NAME=data_copy_check::a_copied_installation_under_the_controller

for path in local/static/dashboard.html local/speech local/models/dtln \
    local/models/ort_android/libonnxruntime.so native/onnxruntime/libonnxruntime.so.1.20.1; do
    [ -e "$RS_DIR/$path" ] || die "$RS_DIR/$path is missing; see README.md §1 and §3"
done
if [ "$ROLLBACK" = 1 ]; then
    docker image inspect "$ROLLBACK_IMAGE" >/dev/null 2>&1 ||
        die "$ROLLBACK_IMAGE is not here; set ROLLBACK_IMAGE or pass --skip-rollback"
fi

FAILURES=()
fail() {
    echo "FAIL: $*"
    FAILURES+=("$*")
}
pass() {
    echo "ok: $*"
}
phase() {
    printf '\n══ %s ══\n' "$*"
}

# ── Build ────────────────────────────────────────────────────────────────────────────
phase "building"
BIN=$TARGET/release/echomuse-controller
if [ "$BUILD" = 1 ]; then
    (cd "$RS_DIR" && cargo build --release)
    HARNESS=$(cd "$RS_DIR" && cargo test --release --test voice --no-run --message-format=json |
        jq -r 'select(.reason == "compiler-artifact" and .target.name == "voice" and .profile.test)
               | .executable' | tail -n 1)
else
    # The newest build of the voice test.
    HARNESS=$(find "$TARGET/release/deps" -maxdepth 1 -type f -executable -name 'voice-*' \
        -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -n 1 | cut -d' ' -f2-)
fi
[ -x "$BIN" ] && [ -x "$HARNESS" ] || die "the release binary or the voice test is not built"
echo "controller: $BIN"
echo "test: $HARNESS"
if [ "$BUILD_IMAGE" = 1 ]; then
    docker build --platform linux/amd64 -f "$RS_DIR/Dockerfile" -t "$RS_IMAGE" "$REPO_DIR"
fi
docker image inspect "$RS_IMAGE" >/dev/null 2>&1 || die "$RS_IMAGE is not here"

# ── Scratch, containers and cleanup ──────────────────────────────────────────────────
SCRATCH=$(mktemp -d "${SCRATCH_ROOT:-/var/tmp}/echomuse-dcc.XXXXXX")
chmod 700 "$SCRATCH"
NS="echomuse-dcc-${SCRATCH##*.}"
CONTAINERS=("$NS")
FOLLOWERS=()

cleanup() {
    local status=$?
    for pid in "${FOLLOWERS[@]}"; do kill "$pid" 2>/dev/null || true; done
    docker rm -f "${CONTAINERS[@]}" >/dev/null 2>&1 || true
    if [ "$KEEP" = 1 ]; then
        echo "kept $SCRATCH: it holds a copy of private data; delete it when done"
    else
        # Containers running as root (the images) leave root-owned files behind.
        docker run --rm --network none -v "$SCRATCH:/scratch" --entrypoint /bin/sh "$RS_IMAGE" \
            -c 'rm -rf /scratch/* /scratch/.[!.]*' >/dev/null 2>&1 || true
        rmdir "$SCRATCH" 2>/dev/null && echo "deleted $SCRATCH" ||
            echo "could not delete $SCRATCH: delete it by hand (private data)"
    fi
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

# The namespace every other container joins: loopback only.
docker run -d --name "$NS" --network none --entrypoint sleep "$RS_IMAGE" infinity >/dev/null

# in_ns NAME [DOCKER OPTIONS...] -- COMMAND...: runs a program of this machine inside the
# namespace, as the invoking user, through a chroot into a read-only view of this machine
# where only the scratch directory and a private /tmp are writable.
in_ns() {
    local name=$1
    shift
    local options=()
    while [ "$1" != -- ]; do
        options+=("$1")
        shift
    done
    shift
    CONTAINERS+=("$name")
    docker run --name "$name" --network "container:$NS" \
        --cap-drop ALL --cap-add SYS_CHROOT --cap-add SETUID --cap-add SETGID \
        --security-opt no-new-privileges \
        -v /:/host:ro -v "$SCRATCH:/host$SCRATCH" --tmpfs /host/tmp:exec --tmpfs /host/run \
        "${options[@]}" --entrypoint /usr/sbin/chroot "$RS_IMAGE" \
        --userspec="$USER_ID" --groups="$(id -g)" /host \
        /usr/bin/env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/tmp LANG=C.UTF-8 TZ="$TZ_NAME" "$@"
}

# ns_sh SCRIPT: a shell script inside the namespace; prints its output.
NS_RUNS=0
ns_sh() {
    NS_RUNS=$((NS_RUNS + 1))
    in_ns "$NS-sh-$NS_RUNS" --rm -- /bin/bash -c "$1"
}

# follow NAME FILE: appends container NAME's log to FILE until it ends.
follow() {
    docker logs -f "$1" >>"$2" 2>&1 &
    FOLLOWERS+=($!)
}

running() {
    [ "$(docker inspect -f '{{.State.Running}}' "$1" 2>/dev/null)" = true ]
}

# wait_for FILE SECONDS CONTAINER: until FILE exists, while CONTAINER runs.
wait_for() {
    local deadline=$((SECONDS + $2))
    while [ ! -e "$1" ]; do
        running "$3" || return 1
        [ $SECONDS -lt $deadline ] || return 1
        sleep 0.5
    done
}

# wait_log FILE PATTERN SECONDS CONTAINER: until FILE has a line matching PATTERN.
wait_log() {
    local deadline=$((SECONDS + $3))
    until grep -q -- "$2" "$1" 2>/dev/null; do
        running "$4" || return 1
        [ $SECONDS -lt $deadline ] || return 1
        sleep 0.5
    done
}

# api PATH: GET on the dashboard API inside the namespace with the check's session.
api() {
    ns_sh "curl -s -m 60 -H 'Authorization: Bearer $SESSION' -w '\n%{http_code}' 'http://127.0.0.1:8768$1'"
}

# counts DB: every contract table's row count, one "table count" line each.
TABLES="devices users sessions system_config turns near_misses device_logs device_metrics
wake_counters wake_shadow wake_shadow_events alert_ops alert_delivery alert_scripts"
counts() {
    local table
    for table in $TABLES; do
        echo "$table $(sqlite3 "file:$1?immutable=1" "SELECT count(*) FROM $table")"
    done
}

# ── Isolation evidence ───────────────────────────────────────────────────────────────
phase "isolation"
echo "namespace container $NS: network mode $(docker inspect -f '{{.HostConfig.NetworkMode}}' "$NS")"
ns_sh 'echo "interfaces:"; ip -br addr; echo "IPv4 routes:"; ip route; echo "IPv6 routes:"; ip -6 route
       echo "connecting to 192.0.2.1:80 (TEST-NET-1):"
       timeout 5 bash -c "exec 3<>/dev/tcp/192.0.2.1/80" 2>&1 | sed "s/^/  /"
       [ -S /run/docker.sock ] && echo "the Docker socket is visible" || echo "no Docker socket"' |
    tee "$SCRATCH/isolation.txt"
if grep -qE '^(eth|en|wl)' "$SCRATCH/isolation.txt" || ! grep -q 'Network is unreachable' "$SCRATCH/isolation.txt" ||
    grep -q 'Docker socket is visible' "$SCRATCH/isolation.txt"; then
    die "the namespace is not isolated; stopping before anything touches the copy"
fi
pass "only loopback, no route, no Docker socket"

# ── Copies ───────────────────────────────────────────────────────────────────────────
phase "copying $SRC"
# copy_data DEST: everything but the database files with tar, the database through
# SQLite's backup API on a read-only mount. Runs as root (the installation's files usually
# are root's) in a container with no network, then gives the copy to the invoking user.
copy_data() {
    mkdir -p "$1"
    docker run --rm --network none -v /:/host:ro -v "$1:/host$1" --tmpfs /host/tmp --tmpfs /host/run \
        --entrypoint /usr/sbin/chroot "$RS_IMAGE" /host /bin/bash -c "
        set -euo pipefail
        tar -C '$SRC' --exclude='./echomuse.db*' -cf - . | tar -C '$1' -xf -
        sqlite3 -readonly '$SRC/echomuse.db' \".backup '$1/echomuse.db'\"
        echo \"integrity: \$(sqlite3 '$1/echomuse.db' 'PRAGMA integrity_check')\"
        echo \"schema_version: \$(sqlite3 '$1/echomuse.db' \"SELECT value FROM system_config WHERE key = 'schema_version'\")\"
        chown -R '$USER_ID' '$1'"
}
RS_DATA=$SCRATCH/rs/data
IMG_DATA=$SCRATCH/image/data
copy_data "$RS_DATA"
mkdir -p "$SCRATCH/image"
cp -a "$RS_DATA" "$IMG_DATA"
du -sh "$RS_DATA" | sed 's/^/copy size: /'

ADMIN=$(sqlite3 "file:$RS_DATA/echomuse.db?immutable=1" "SELECT id FROM users WHERE role = 'admin' ORDER BY id LIMIT 1")
[ -n "$ADMIN" ] || die "the installation has no administrator to sign in as"
SESSION=$(od -An -tx1 -N32 /dev/urandom | tr -d ' \n')
for db in "$RS_DATA/echomuse.db" "$IMG_DATA/echomuse.db"; do
    sqlite3 "$db" "INSERT INTO sessions (token, user_id, created_at, expires_at)
                   VALUES ('$SESSION', $ADMIN, strftime('%s', 'now'), strftime('%s', 'now') + 86400)"
done
echo "a dashboard session for user $ADMIN was added to each copy (the only change before the runs)"
counts "$RS_DATA/echomuse.db" >"$SCRATCH/counts-copy.txt"

# The image's bundled firmware (FIRMWARE_DIR) and the dashboard and wake seed beside the
# binary's working directory, as the image lays them out.
container=$(docker create "$RS_IMAGE")
docker cp -q "$container:/app/firmware" "$SCRATCH/firmware"
docker rm "$container" >/dev/null
mkdir -p "$SCRATCH/app"
ln -s "$RS_DIR/local/static" "$SCRATCH/app/static"
ln -s "$REPO_DIR/bcresnet_audio.onnx" "$REPO_DIR/bcresnet_audio.json" "$SCRATCH/app/"

# harness MODE DATA OUT NAME: the test in MODE on DATA, detached; sets SIM_HA_TOKEN to the
# simulated Home Assistant's token once it listens (its URL is in OUT/ha_url).
harness() {
    mkdir -p "$3"
    rm -f "$3/ha_url" "$3/ha_token"
    in_ns "$4" -d -- \
        LD_LIBRARY_PATH="$TARGET/release:$TARGET/release/deps" RUST_LOG=warn \
        ECHOMUSE_DCC_MODE="$1" ECHOMUSE_DCC_DATA="$2" ECHOMUSE_DCC_OUT="$3" \
        ECHOMUSE_DCC_LOG="$3/controller.log" ECHOMUSE_DCC_SESSION="$SESSION" \
        ${ECHOMUSE_DCC_TURN:+ECHOMUSE_DCC_TURN="$ECHOMUSE_DCC_TURN"} \
        "$HARNESS" --ignored --exact "$TEST_NAME" --nocapture --test-threads=1 >/dev/null
    wait_for "$3/ha_url" 120 "$4" || {
        docker logs "$4" 2>&1 | tail -n 40
        die "the simulated Home Assistant did not start"
    }
    SIM_HA_TOKEN=$(cat "$3/ha_token")
}

# rust_binary NAME DATA HA_URL LOG: the release binary on DATA, detached.
rust_binary() {
    in_ns "$1" -d -- RUST_LOG=info \
        SERVER_HOST=127.0.0.1 SERVER_IP=127.0.0.1 MDNS_NAME=echomuse-dcc \
        HA_URL="$3" HA_TOKEN="$SIM_HA_TOKEN" DEVICE_APPROVAL=strict \
        DB_PATH="$2/echomuse.db" TLS_DIR="$2/tls" FIRMWARE_DIR="$SCRATCH/firmware" \
        DEVICE_PAYLOADS_DIR="$REPO_DIR/controller/device_payloads" \
        SPEECH_BUNDLE_DIR="$RS_DIR/local/speech" NS_MODEL_DIR="$RS_DIR/local/models/dtln" \
        ORT_ANDROID_LIB="$RS_DIR/local/models/ort_android/libonnxruntime.so" \
        ONNXRUNTIME_LIB="$RS_DIR/native/onnxruntime/libonnxruntime.so.1.20.1" \
        /bin/sh -c "cd '$SCRATCH/app' && exec '$BIN'" >/dev/null
    follow "$1" "$4"
}

# rust_image NAME DATA HA_URL LOG: this tree's image on DATA, detached, as it ships (root).
rust_image() {
    CONTAINERS+=("$1")
    docker run -d --name "$1" --network "container:$NS" -v "$2:/app/data" \
        -e RUST_LOG=info -e TZ="$TZ_NAME" -e SERVER_IP=127.0.0.1 -e MDNS_NAME=echomuse-dcc \
        -e HA_URL="$3" -e HA_TOKEN="$SIM_HA_TOKEN" -e DEVICE_APPROVAL=strict \
        -e DB_PATH=/app/data/echomuse.db "$RS_IMAGE" >/dev/null
    follow "$1" "$4"
}

# stop NAME: SIGTERM, then the exit status.
stop() {
    docker stop -t 60 "$1" >/dev/null
    docker inspect -f '{{.State.ExitCode}}' "$1"
}

# tls_check DATA: the certificate the device TLS port serves is the copy's.
tls_check() {
    local served stored
    served=$(ns_sh "openssl s_client -connect 127.0.0.1:8770 </dev/null 2>/dev/null |
                    openssl x509 -noout -fingerprint -sha256")
    stored=$(openssl x509 -in "$1/tls/server.pem" -noout -fingerprint -sha256)
    [ -n "$served" ] && [ "$served" = "$stored" ] &&
        pass "the device TLS port serves the copy's tls/server.pem ($stored)" ||
        fail "the device TLS port serves '$served', the copy holds '$stored'"
}

# drive_run LABEL DATA STARTER: the drive test against a controller STARTER starts.
drive_run() {
    local label=$1 data=$2 starter=$3 out="$SCRATCH/out-$1"
    local harness_name="$NS-$label-drive" controller="$NS-$label"
    harness drive "$data" "$out" "$harness_name"
    "$starter" "$controller" "$data" "$(cat "$out/ha_url")" "$out/controller.log"
    if wait_for "$out/driven" 1200 "$harness_name"; then
        tls_check "$data"
        echo "controller exit status after SIGTERM: $(stop "$controller")"
    else
        fail "$label: the drive did not finish"
        stop "$controller" >/dev/null || true
    fi
    touch "$out/stopped"
    local status
    status=$(docker wait "$harness_name")
    docker logs "$harness_name" >"$out/harness.log" 2>&1
    echo "── the drive test's report ($label) ──"
    if [ -f "$out/report-drive.txt" ]; then
        cat "$out/report-drive.txt"
    else
        tail -n 60 "$out/harness.log"
    fi
    [ "$status" = 0 ] && pass "$label: the drive test passed" ||
        fail "$label: the drive test failed (status $status; its log is above)"
    echo "── the controller's warnings and errors ($label) ──"
    # The log may carry colour codes around the level.
    sed 's/\x1b\[[0-9;]*m//g' "$out/controller.log" | grep -E ' (WARN|ERROR) ' |
        grep -v 'Home Assistant is not connected' | cut -c1-400 | head -n 40 || true
    grep -E '^(database|device TLS|device runtimes|bundled firmware|EchoMuse controller)' \
        "$out/controller.log" || true
}

# backup_check DATA: the cutover backup is there and holds the copy as it was.
backup_check() {
    local backup="$1/echomuse.db.pre-rust.bak"
    if [ -f "$backup" ]; then
        counts "$backup" >"$SCRATCH/counts-backup.txt"
        if cmp -s "$SCRATCH/counts-copy.txt" "$SCRATCH/counts-backup.txt"; then
            pass "echomuse.db.pre-rust.bak holds the copy's rows as they were"
        else
            fail "the backup's row counts differ from the copy's:"
            diff "$SCRATCH/counts-copy.txt" "$SCRATCH/counts-backup.txt" || true
        fi
    else
        fail "no echomuse.db.pre-rust.bak beside the database"
    fi
}

# ── The release binary ───────────────────────────────────────────────────────────────
phase "the release binary on the copy"
drive_run binary "$RS_DATA" rust_binary
backup_check "$RS_DATA"
BACKUP_SUM=$(sha256_of "$RS_DATA/echomuse.db.pre-rust.bak" 2>/dev/null || echo none)
DEVICE=$(jq -r '.device // empty' "$SCRATCH/out-binary/drive.json" 2>/dev/null || true)
NEW_TURN=$(jq -r '.turn // empty' "$SCRATCH/out-binary/drive.json" 2>/dev/null || true)
if [ -n "$NEW_TURN" ]; then
    echo "the drive wrote turn $NEW_TURN of $DEVICE"
else
    # The later checks look for a turn that does not exist; they fail and say so.
    fail "the drive wrote no turn"
    DEVICE=none NEW_TURN=0
fi

phase "restarting the release binary"
SERVE_OUT=$SCRATCH/out-serve
harness serve "$RS_DATA" "$SERVE_OUT" "$NS-serve"
HA_URL=$(cat "$SERVE_OUT/ha_url")
rust_binary "$NS-binary-2" "$RS_DATA" "$HA_URL" "$SCRATCH/out-binary/controller-2.log"
if wait_log "$SCRATCH/out-binary/controller-2.log" 'speech models are ready' 400 "$NS-binary-2"; then
    grep '^database:' "$SCRATCH/out-binary/controller-2.log" || true
    [ "$(sha256_of "$RS_DATA/echomuse.db.pre-rust.bak")" = "$BACKUP_SUM" ] &&
        pass "the restart left echomuse.db.pre-rust.bak as it was (sha256 $BACKUP_SUM)" ||
        fail "the restart changed echomuse.db.pre-rust.bak"
    listed=$(api "/api/devices/$DEVICE/turns?limit=5")
    if [ "$(tail -n 1 <<<"$listed")" = 200 ] &&
        sed '$d' <<<"$listed" | jq -e --argjson id "$NEW_TURN" 'any(.[]; .turn_id == $id)' >/dev/null; then
        pass "after the restart the dashboard still lists turn $NEW_TURN"
    else
        fail "after the restart turn $NEW_TURN is not listed: $(cut -c1-300 <<<"$listed")"
    fi
    echo "controller exit status after SIGTERM: $(stop "$NS-binary-2")"
else
    fail "the restarted controller did not get ready"
    tail -n 40 "$SCRATCH/out-binary/controller-2.log"
fi

# ── Rollback to the previous image ───────────────────────────────────────────────────
if [ "$ROLLBACK" = 1 ]; then
    phase "rollback: $ROLLBACK_IMAGE on the same copy"
    ls -1 "$RS_DATA" | grep '^echomuse\.db' | sort >"$SCRATCH/db-files-before.txt"
    ROLLBACK_LOG=$SCRATCH/out-binary/rollback.log
    CONTAINERS+=("$NS-rollback")
    docker run -d --name "$NS-rollback" --network "container:$NS" -v "$RS_DATA:/app/data" \
        -e TZ="$TZ_NAME" -e SERVER_HOST=0.0.0.0 -e SERVER_PORT=8767 -e SERVER_TLS_PORT=8770 \
        -e API_PORT=8768 -e SERVER_IP=127.0.0.1 -e MDNS_NAME=echomuse-dcc \
        -e REQUIRE_DEVICE_TLS=0 -e ESPHOME_PROJECT_VERSION=2.0.0 \
        -e HA_URL="$HA_URL" -e HA_TOKEN="$SIM_HA_TOKEN" -e DEVICE_APPROVAL=strict \
        -e DB_PATH=/app/data/echomuse.db "$ROLLBACK_IMAGE" >/dev/null
    follow "$NS-rollback" "$ROLLBACK_LOG"
    up=0
    for _ in $(seq 1 300); do
        running "$NS-rollback" || break
        if [ "$(api /api/auth/me | tail -n 1)" = 200 ]; then
            up=1
            break
        fi
        sleep 1
    done
    if [ "$up" = 1 ]; then
        pass "the previous image started and signs the copy's session in"
        devices=$(api /api/devices)
        echo "/api/devices: $(tail -n 1 <<<"$devices"), $(sed '$d' <<<"$devices" | jq length) devices"
        turns=$(api "/api/devices/$DEVICE/turns?limit=10")
        if sed '$d' <<<"$turns" | jq -e --argjson id "$NEW_TURN" 'any(.[]; .turn_id == $id)' >/dev/null; then
            pass "the previous image lists turn $NEW_TURN the Rust controller wrote:"
            sed '$d' <<<"$turns" | jq -c --argjson id "$NEW_TURN" '.[] | select(.turn_id == $id)
                | {turn_id, trigger, outcome, terminal_reason, commit_route, endpoint_class,
                   stt_text, response_text, audio_file, wake_file, total_ms}'
        else
            fail "the previous image does not list turn $NEW_TURN: $(cut -c1-400 <<<"$turns")"
        fi
        for clip in wake audio recording; do
            echo "  turn $NEW_TURN $clip: $(api "/api/devices/$DEVICE/turns/$NEW_TURN/$clip" | tail -n 1)"
        done
        echo "  its trace: $(api "/api/devices/$DEVICE/turns/$NEW_TURN/trace" | tail -n 1)"
        echo "  /api/turns: $(api '/api/turns?limit=1000' | sed '$d' | jq length) listed"
        echo "  /api/near_misses: $(api '/api/near_misses?limit=1000' | sed '$d' | jq length) listed"
        sleep 5
    else
        fail "the previous image did not answer its API"
    fi
    echo "the previous image's exit status after SIGTERM: $(stop "$NS-rollback")"
    sleep 1
    ls -1 "$RS_DATA" | grep '^echomuse\.db' | sort >"$SCRATCH/db-files-after.txt"
    if cmp -s "$SCRATCH/db-files-before.txt" "$SCRATCH/db-files-after.txt"; then
        pass "the previous image made no migration backup (database files: $(tr '\n' ' ' <"$SCRATCH/db-files-after.txt"))"
    else
        fail "the previous image changed the database files:"
        diff "$SCRATCH/db-files-before.txt" "$SCRATCH/db-files-after.txt" || true
    fi
    version=$(docker run --rm --network none -v "$RS_DATA:/data:ro" --entrypoint python3 "$ROLLBACK_IMAGE" -c \
        "import sqlite3; print(sqlite3.connect('file:/data/echomuse.db?mode=ro', uri=True).execute(\"SELECT value FROM system_config WHERE key = 'schema_version'\").fetchone()[0])")
    [ "$version" = 40 ] && pass "schema_version is still 40" || fail "schema_version is now $version"
    echo "── the previous image's log: errors, tracebacks and migrations, verbatim ──"
    grep -n -i -E 'error|traceback|exception|migrat|refus|schema' "$ROLLBACK_LOG" | cut -c1-400 | head -n 80 || true
    echo "($(wc -l <"$ROLLBACK_LOG") log lines in all)"
fi
docker stop -t 5 "$NS-serve" >/dev/null 2>&1 || true

# ── This tree's image ────────────────────────────────────────────────────────────────
phase "the image $RS_IMAGE on a second copy"
drive_run image "$IMG_DATA" rust_image
[ -f "$IMG_DATA/echomuse.db.pre-rust.bak" ] && pass "the image wrote its cutover backup" ||
    fail "the image wrote no cutover backup"

# ── Summary ──────────────────────────────────────────────────────────────────────────
phase "summary"
if [ ${#FAILURES[@]} -eq 0 ]; then
    echo "every check passed"
else
    printf 'FAIL: %s\n' "${FAILURES[@]}"
    exit 1
fi
