#!/usr/bin/env bash
#
# train_wakeword.sh — end-to-end custom wake word on a machine that is NOT the
# controller. Synthetic positives (piper, optionally Google TTS) plus real
# clips recorded through an Echo in the fleet, which it collects over the
# controller's HTTP API — nothing needs to run beside the controller itself.
#
#   ./tools/train_wakeword.sh --phrase "hey clara" \
#       --controller http://192.168.3.211:8768 --device lounge --upload
#
# Every stage is idempotent: assets skip what is already downloaded, the
# config is not rewritten, and a build can be resumed with --from-step. Re-run
# the whole thing after a failure rather than picking through it.
#
set -euo pipefail

# Where the oww_forge checkout is. Normally the parent of this script, but the
# script is also handed around on its own (copied to a share, dropped on a
# training box), so fall back to the working directory and then to --forge-dir.
FORGE_DIR="${FORGE_DIR:-}"
is_forge_dir() { [[ -f "$1/docker-compose.yml" && -f "$1/forge.py" ]]; }
if [[ -z "$FORGE_DIR" ]]; then
    _here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
    if is_forge_dir "$_here"; then
        FORGE_DIR="$_here"
    elif is_forge_dir "$PWD"; then
        FORGE_DIR="$PWD"
    fi
fi

# ------------------------------------------------------------------ options

PHRASE=""
NAME=""
DEVICE=""
CLIPS=()
CONTROLLER="${EM_CONTROLLER:-}"
DATA_DIR=""
SAMPLES=30000
STEPS=50000
HOLDOUT=5
FROM_STEP=""
GOOGLE_TTS=0
UPLOAD=0
DISPLACE=0
FORCE_CPU=0
SKIP_ASSETS=0
SKIP_BUILD=0
MATCH_RATIO=0.75
EM_USER=""

usage() {
    cat <<'EOF'
usage: train_wakeword.sh --phrase "hey clara" [options]

  --phrase TEXT       wake phrase; comma-separate pronunciation variants
                      ("hey clara, hey clarra") to train one model on both
  --name SLUG         model name (default: slug of the first phrase)
  --clips PATH        real recordings to train on alongside the synthetic
                      ones: a .zip (the dashboard's samples.zip) or a
                      directory. Repeatable. Any audio format — converted to
                      16kHz mono in the trainer image, so no host ffmpeg.
  --device LABEL      ALSO record fresh clips from this Echo, button-pressed
                      and transcript-verified, before training (dashboard
                      label, case-insensitive, partial ok). Not needed if you
                      already have clips: use --clips.
  --controller URL    controller base URL (env EM_CONTROLLER). Needed for
                      --device and --upload.
  --user NAME         dashboard username (admin); prompts otherwise.
                      Token can also come from EM_TOKEN.
  --data DIR          host dir for the ~25GB of assets, workdirs and models
                      (default: oww_forge/data)
  --samples N         synthetic positives (default 30000). n_samples is
                      raised by however many real clips were imported, so you
                      get N synthetic PLUS your own — upstream's generate step
                      tops up to n_samples and would otherwise let real clips
                      displace synthetic ones.
  --displace          don't raise it: real clips take the place of synthetic
                      ones, keeping the total at N (upstream's behaviour)
  --steps N           max training steps (default 50000)
  --holdout N         real clips held back as an eval set, scored after the
                      build (default 5; 0 to keep them all in training)
  --google-tts        add Google Cloud TTS voices before training
                      (needs data/google-credentials.json)
  --upload            POST the finished .onnx to the controller
  --from-step STEP    resume the build at generate|augment|train
  --forge-dir DIR     the oww_forge checkout (default: this script's parent,
                      or the working directory; env FORGE_DIR)
  --cpu               ignore any GPU and use the CPU service
  --skip-assets       assume the ~25GB is already downloaded
  --skip-build        collect clips only, do not train
  --match-ratio R     clip verification strictness (default 0.75)
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        -p|--phrase)     PHRASE="$2"; shift 2 ;;
        -n|--name)       NAME="$2"; shift 2 ;;
        -d|--device)     DEVICE="$2"; shift 2 ;;
        --clips)         CLIPS+=("$2"); shift 2 ;;
        -c|--controller) CONTROLLER="$2"; shift 2 ;;
        --user)          EM_USER="$2"; shift 2 ;;
        --data)          DATA_DIR="$2"; shift 2 ;;
        --samples)       SAMPLES="$2"; shift 2 ;;
        --steps)         STEPS="$2"; shift 2 ;;
        --holdout)       HOLDOUT="$2"; shift 2 ;;
        --from-step)     FROM_STEP="$2"; shift 2 ;;
        --match-ratio)   MATCH_RATIO="$2"; shift 2 ;;
        --google-tts)    GOOGLE_TTS=1; shift ;;
        --displace)      DISPLACE=1; shift ;;
        --forge-dir)     FORGE_DIR="$2"; shift 2 ;;
        --upload)        UPLOAD=1; shift ;;
        --cpu)           FORCE_CPU=1; shift ;;
        --skip-assets)   SKIP_ASSETS=1; shift ;;
        --skip-build)    SKIP_BUILD=1; shift ;;
        -h|--help)       usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
info() { printf '   %s\n' "$*"; }
die()  { printf '\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }

[[ -n "$PHRASE" ]] || { usage >&2; die "--phrase is required"; }

if [[ -z "$FORGE_DIR" ]] || ! is_forge_dir "$FORGE_DIR"; then
    die "cannot find the oww_forge checkout (needs docker-compose.yml + forge.py).
       Run this from inside it, or pass --forge-dir /path/to/oww_forge"
fi
FORGE_DIR="$(cd "$FORGE_DIR" && pwd)"
TOOLS_DIR="$FORGE_DIR/tools"

# slug of the first phrase, matching forge.py's own slugify()
if [[ -z "$NAME" ]]; then
    NAME="$(printf '%s' "${PHRASE%%,*}" \
            | tr '[:upper:]' '[:lower:]' \
            | sed -E 's/[^a-z0-9]+/_/g; s/^_+//; s/_+$//')"
fi
[[ -n "$NAME" ]] || die "could not derive a model name from --phrase"

[[ -n "$DATA_DIR" ]] || DATA_DIR="$FORGE_DIR/data"
mkdir -p "$DATA_DIR"
DATA_DIR="$(cd "$DATA_DIR" && pwd)"

if [[ -n "$DEVICE" || $UPLOAD -eq 1 ]]; then
    [[ -n "$CONTROLLER" ]] || die "--controller is required for --device / --upload"
    CONTROLLER="${CONTROLLER%/}"
fi

# ------------------------------------------------------------------ preflight

say "preflight"

command -v docker  >/dev/null || die "docker not found"
command -v python3 >/dev/null || die "python3 not found"

if docker compose version >/dev/null 2>&1; then
    COMPOSE=(docker compose)
elif command -v docker-compose >/dev/null; then
    COMPOSE=(docker-compose)
else
    die "neither 'docker compose' nor 'docker-compose' is available"
fi

SVC=forge
if [[ $FORCE_CPU -eq 1 ]]; then
    SVC=forge-cpu
    info "GPU disabled by --cpu; training on CPU (expect overnight)"
elif docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q nvidia; then
    info "nvidia container runtime present — using the GPU service"
else
    SVC=forge-cpu
    info "no nvidia container runtime — using $SVC (CPU, expect overnight)"
fi

if [[ -n "$DEVICE" ]]; then
    command -v ffmpeg >/dev/null || die "ffmpeg not found (apt install ffmpeg) — needed to trim collected clips"
    command -v curl   >/dev/null || die "curl not found"
elif [[ $UPLOAD -eq 1 ]]; then
    command -v curl >/dev/null || die "curl not found"
fi

# Free space where the assets and workdirs land. ~25GB of assets plus the
# per-word clip set; refuse early rather than half-way through a 17GB pull.
if [[ $SKIP_ASSETS -eq 0 ]]; then
    avail_gb="$(df -Pk "$DATA_DIR" | awk 'NR==2 {print int($4/1024/1024)}')"
    if [[ -n "$avail_gb" && "$avail_gb" -lt 40 ]]; then
        die "$DATA_DIR has ${avail_gb}GB free; the assets alone are ~25GB. Use --data on a bigger disk."
    fi
    info "data dir: $DATA_DIR (${avail_gb:-?}GB free)"
else
    info "data dir: $DATA_DIR"
fi

# The compose file hard-codes ./data:/data, so a --data elsewhere needs an
# override. Marked, and never written over a file we did not write.
OVERRIDE="$FORGE_DIR/docker-compose.override.yml"
MARKER="# generated by tools/train_wakeword.sh"
if [[ "$DATA_DIR" != "$FORGE_DIR/data" ]]; then
    if [[ -e "$OVERRIDE" ]] && ! head -1 "$OVERRIDE" | grep -qF "$MARKER"; then
        die "$OVERRIDE exists and was not written by this script — point --data at $FORGE_DIR/data or merge it by hand"
    fi
    {
        echo "$MARKER"
        echo "services:"
        for s in forge-ui forge forge-cpu; do
            echo "  $s:"
            echo "    volumes:"
            echo "      - \"$DATA_DIR:/data\""
        done
    } > "$OVERRIDE"
    info "wrote $OVERRIDE mapping $DATA_DIR -> /data"
fi

COMPOSE_FILES=(-f "$FORGE_DIR/docker-compose.yml")
if [[ -f "$OVERRIDE" ]] && head -1 "$OVERRIDE" | grep -qF "$MARKER"; then
    COMPOSE_FILES+=(-f "$OVERRIDE")
fi

forge() { COMPOSE_PROFILES=cli "${COMPOSE[@]}" "${COMPOSE_FILES[@]}" run --rm "$SVC" "$@"; }

cd "$FORGE_DIR"

# ------------------------------------------------------------------ token

TOKEN="${EM_TOKEN:-}"
mint_token() {
    [[ -n "$TOKEN" ]] && return 0
    local user pass body
    user="$EM_USER"
    [[ -n "$user" ]] || read -r -p "controller username: " user
    read -r -s -p "password for $user: " pass; echo
    body="$(python3 -c 'import json,sys; print(json.dumps({"username":sys.argv[1],"password":sys.argv[2]}))' "$user" "$pass")"
    TOKEN="$(curl -fsS -X POST "$CONTROLLER/api/auth/login" \
                  -H 'Content-Type: application/json' -d "$body" \
             | python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')" \
        || die "login failed against $CONTROLLER"
    info "logged in to $CONTROLLER"
}

# ------------------------------------------------------------------ 1. image + assets

say "building the trainer image"
"${COMPOSE[@]}" "${COMPOSE_FILES[@]}" build forge-ui

if [[ $SKIP_ASSETS -eq 1 ]]; then
    say "skipping assets (--skip-assets)"
else
    say "downloading shared assets (~25GB; skips what is already there)"
    forge assets
fi

# ------------------------------------------------------------------ 2. wake word config

WW_DIR="$DATA_DIR/wakewords/$NAME"
# forge.py writes clips to <output_dir>/<model_name>/, so the positive set is
# one level deeper than the wake word dir.
POS_TRAIN="$WW_DIR/$NAME/positive_train"
EVAL_DIR="$DATA_DIR/eval/$NAME"

if [[ -f "$WW_DIR/config.yml" ]]; then
    say "wake word '$NAME' already exists — keeping $WW_DIR/config.yml"
else
    say "creating wake word '$NAME'"
    forge new "$PHRASE" --name "$NAME" --samples "$SAMPLES" --steps "$STEPS"
fi

# ------------------------------------------------------------------ 3. real clips

# Everything real lands in one staging pool first — imported zips, imported
# directories and freshly collected clips alike — so the eval holdback is
# taken across all of them and only ever from clips added by THIS run. The
# generate step later tops positive_train up to n_samples counting what is
# already there, so these displace synthetic clips rather than adding to them.
STAMP="$(date +%Y%m%d-%H%M%S)"
STAGE="$DATA_DIR/incoming/$NAME/$STAMP"
RAW="$STAGE/raw"
WAV="$STAGE/wav"

if [[ ${#CLIPS[@]} -gt 0 ]]; then
    say "importing ${#CLIPS[@]} clip source(s)"
    mkdir -p "$RAW"
    idx=0
    for src in "${CLIPS[@]}"; do
        idx=$((idx + 1))
        [[ -e "$src" ]] || die "--clips $src: no such file or directory"
        if [[ -d "$src" ]]; then
            info "dir: $src"
            # Flattened with a per-source prefix: two zips both holding
            # clip_0001.wav must not overwrite each other.
            find "$src" -type f -print0 | while IFS= read -r -d '' f; do
                cp -- "$f" "$RAW/s${idx}_$(basename -- "$f")"
            done
        else
            info "zip: $src"
            python3 - "$src" "$RAW" "$idx" <<'PY'
import pathlib, sys, zipfile

src, dest, idx = sys.argv[1], pathlib.Path(sys.argv[2]), sys.argv[3]
n = 0
with zipfile.ZipFile(src) as z:
    for m in z.infolist():
        if m.is_dir() or "__MACOSX" in m.filename:
            continue
        name = pathlib.PurePosixPath(m.filename).name
        if not name or name.startswith("."):
            continue
        with z.open(m) as fh, open(dest / f"s{idx}_{name}", "wb") as out:
            out.write(fh.read())
        n += 1
print(f"     {n} file(s) extracted")
PY
        fi
    done

    say "converting to 16kHz mono (in the trainer image — no host ffmpeg needed)"
    COMPOSE_PROFILES=cli "${COMPOSE[@]}" "${COMPOSE_FILES[@]}" run --rm \
        --entrypoint bash "$SVC" -c '
            set -eu
            in="$1"; out="$2"; stamp="$3"
            mkdir -p "$out"
            i=0; ok=0; skip=0
            while IFS= read -r -d "" f; do
                i=$((i + 1))
                dest="$out/$(printf "real_%s_%04d.wav" "$stamp" "$i")"
                if ffmpeg -nostdin -v error -y -i "$f" \
                          -ac 1 -ar 16000 -c:a pcm_s16le "$dest" 2>/dev/null; then
                    ok=$((ok + 1))
                else
                    rm -f "$dest"; skip=$((skip + 1))
                    echo "     skipped (not decodable audio): ${f##*/}"
                fi
            done < <(find "$in" -type f -print0 | sort -z)
            echo "     converted $ok, skipped $skip"
        ' bash "/data/incoming/$NAME/$STAMP/raw" "/data/incoming/$NAME/$STAMP/wav" "$STAMP"

    rm -rf "$RAW"
fi

if [[ -n "$DEVICE" ]]; then
    say "collecting clips from '$DEVICE' via $CONTROLLER"

    if [[ ! -x "$TOOLS_DIR/.venv/bin/python" ]]; then
        info "creating $TOOLS_DIR/.venv"
        python3 -m venv "$TOOLS_DIR/.venv"
        "$TOOLS_DIR/.venv/bin/pip" install -q --upgrade pip
        "$TOOLS_DIR/.venv/bin/pip" install -q -r "$TOOLS_DIR/requirements.txt"
    fi

    mint_token
    mkdir -p "$WAV"

    cat <<EOF

  Press the action button on '$DEVICE', say "${PHRASE%%,*}", pause. Repeat.
  Ctrl+C when you have enough (20-50 clips is already worth it).
  Mute must be off, HA will answer every press, and the device's config is
  restored on exit.

EOF
    # Ctrl+C is how you finish collecting, so it must not abort the script —
    # the collector restores the device config on SIGINT itself.
    set +e
    "$TOOLS_DIR/.venv/bin/python" "$TOOLS_DIR/collect_device_clips.py" \
        --device "$DEVICE" \
        --controller "$CONTROLLER" \
        --token "$TOKEN" \
        --phrase "$PHRASE" \
        --match-ratio "$MATCH_RATIO" \
        --out "$WAV"
    rc=$?
    set -e
    [[ $rc -eq 0 || $rc -eq 130 ]] || die "clip collection failed (exit $rc) — device config restore is logged above"
fi

# --- eval holdback, then into the training set -----------------------------

n_real=0
[[ -d "$WAV" ]] && n_real="$(find "$WAV" -maxdepth 1 -name '*.wav' | wc -l | tr -d ' ')"

if [[ "$n_real" -eq 0 ]]; then
    if [[ ${#CLIPS[@]} -gt 0 || -n "$DEVICE" ]]; then
        die "no usable clips came out of the import/collection — nothing to add"
    fi
    say "no --clips and no --device: training on synthetic audio only"
else
    say "$n_real real clip(s) ready"

    # The eval set is drawn from the staging pool, which holds ONLY the real
    # clips imported or collected by this run — no synthetic clip can end up
    # in it, and a synthetic one would be worthless there anyway: the question
    # the eval set answers is how the model scores YOUR voice in YOUR room.
    # Held back BEFORE they reach positive_train, because a clip that was
    # trained on scores high whatever the model learned.
    if [[ "$HOLDOUT" -gt 0 && "$n_real" -gt "$HOLDOUT" ]]; then
        mkdir -p "$EVAL_DIR"
        # shellcheck disable=SC2012
        ls -1t "$WAV"/*.wav | head -n "$HOLDOUT" | while read -r f; do
            mv -- "$f" "$EVAL_DIR/"
        done
        info "held $HOLDOUT back as an eval set: $EVAL_DIR"
    elif [[ "$HOLDOUT" -gt 0 ]]; then
        info "only $n_real clip(s) — keeping them all for training, no eval set"
    fi

    mkdir -p "$POS_TRAIN"
    n_added="$(find "$WAV" -maxdepth 1 -name '*.wav' | wc -l | tr -d ' ')"
    find "$WAV" -maxdepth 1 -name '*.wav' -exec mv -- {} "$POS_TRAIN/" \;
    info "training positives now in $POS_TRAIN: $(find "$POS_TRAIN" -maxdepth 1 -name '*.wav' | wc -l | tr -d ' ')"
    rmdir "$WAV" "$STAGE" "$DATA_DIR/incoming/$NAME" "$DATA_DIR/incoming" 2>/dev/null || true

    # Upstream's generate step TOPS UP to n_samples — it counts what is
    # already in positive_train and synthesizes only the remainder:
    #
    #   n_current_samples = len(os.listdir(positive_train_output_dir))
    #   if n_current_samples <= 0.95*config["n_samples"]:
    #       generate_samples(..., max_samples=config["n_samples"]-n_current_samples,
    #
    # So real clips REPLACE synthetic ones unless the target moves. Raise it
    # by what we added and the synthetic count is unchanged, with the real
    # clips on top. --displace keeps upstream's behaviour.
    if [[ $DISPLACE -eq 1 ]]; then
        info "--displace: leaving n_samples alone, your $n_added clip(s) take the place of synthetic ones"
    elif [[ "$n_added" -gt 0 ]]; then
        python3 - "$WW_DIR/config.yml" "$n_added" <<'PY'
import pathlib, re, sys

cfg, added = pathlib.Path(sys.argv[1]), int(sys.argv[2])
text = cfg.read_text()
m = re.search(r"^n_samples:\s*(\d+)\s*$", text, re.M)
if not m:
    sys.exit("could not find an 'n_samples:' line in " + str(cfg))
old = int(m.group(1))
new = old + added
cfg.write_text(text[:m.start()] + f"n_samples: {new}" + text[m.end():])
print(f"   n_samples {old} -> {new} ({added} real clip(s) on TOP of the synthetic set)")
PY
    fi
fi

# ------------------------------------------------------------------ 4. google tts

if [[ $GOOGLE_TTS -eq 1 ]]; then
    say "adding Google TTS voices"
    if [[ ! -f "$DATA_DIR/google-credentials.json" ]]; then
        # A copy of this script may carry the key inline (see the
        # EMBEDDED_GOOGLE_CREDS block at the top). Empty here on purpose: this
        # copy is in a PUBLIC git repo, and a service-account key committed to
        # one is a rotation, not a revert.
        SCRIPT_CREDS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/google-credentials.json"
        if [[ -n "${EMBEDDED_GOOGLE_CREDS:-}" ]]; then
            printf '%s\n' "$EMBEDDED_GOOGLE_CREDS" > "$DATA_DIR/google-credentials.json"
            chmod 600 "$DATA_DIR/google-credentials.json"
            info "wrote the embedded service-account key to $DATA_DIR/google-credentials.json"
        elif [[ -f "$SCRIPT_CREDS" ]]; then
            # Key carried alongside the script (a share, a USB stick) rather
            # than inside it — same one-copy workflow, nothing secret in a file
            # that might get committed.
            install -m 600 "$SCRIPT_CREDS" "$DATA_DIR/google-credentials.json"
            info "took the service-account key from $SCRIPT_CREDS"
        else
            die "--google-tts needs a service-account key at $DATA_DIR/google-credentials.json"
        fi
    fi
    forge google-tts "$NAME"
fi

# ------------------------------------------------------------------ 5. train

MODEL="$DATA_DIR/models/$NAME.onnx"

if [[ $SKIP_BUILD -eq 1 ]]; then
    say "skipping the build (--skip-build)"
    if [[ $UPLOAD -eq 0 ]]; then
        info "train later with: $0 --phrase \"$PHRASE\" --name $NAME --skip-assets"
        exit 0
    fi
    [[ -f "$MODEL" ]] || die "--skip-build --upload, but $MODEL does not exist yet"
else
    say "training '$NAME' (GPU ~1-2h, CPU overnight)"
    if [[ -n "$FROM_STEP" ]]; then
        forge build "$NAME" --from-step "$FROM_STEP"
    else
        forge build "$NAME"
    fi
    [[ -f "$MODEL" ]] || die "build finished but $MODEL is missing"
    say "model ready: $MODEL"
fi

# ------------------------------------------------------------------ 6. score the held-back clips

if [[ -d "$EVAL_DIR" ]] && find "$EVAL_DIR" -maxdepth 1 -name '*.wav' | grep -q .; then
    say "scoring the held-back device clips (want ~1.0; the device threshold is ~0.5)"
    forge test "$NAME" --wav "/data/eval/$NAME"
fi

# ------------------------------------------------------------------ 7. install

if [[ $UPLOAD -eq 1 ]]; then
    say "uploading to $CONTROLLER"
    mint_token
    curl -fsS -X POST "$CONTROLLER/api/oww_models/upload" \
         -H "Authorization: Bearer $TOKEN" \
         -F "model=@$MODEL" \
        | python3 -m json.tool
    cat <<EOF

  Uploaded. Select it per device in the dashboard: Config -> Wake word ->
  the '$NAME' tile. The listener hot-reloads; no device restart.
EOF
else
    cat <<EOF

  Install it with:
    $0 --phrase "$PHRASE" --name $NAME --skip-assets --skip-build --upload \\
       --controller <url>
  or upload $MODEL by hand in the dashboard: Config -> Wake word ->
  "+ Custom model".
EOF
fi
