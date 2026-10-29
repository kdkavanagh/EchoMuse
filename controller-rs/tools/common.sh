# shellcheck shell=bash
# Shared helpers for the fetch and build scripts in this directory. Source it; do not run it.
#
# Every download is checked against a SHA-256 written in the calling script, and every
# hash that crates/speech/src/pins.rs also records is checked against that file, so the
# scripts and the Rust pins cannot drift apart silently.

set -euo pipefail

TOOLS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RS_DIR="$(cd "$TOOLS_DIR/.." && pwd)"
# shellcheck disable=SC2034 # used by the scripts that source this file
REPO_DIR="$(cd "$RS_DIR/.." && pwd)"
PINS_RS="$RS_DIR/crates/speech/src/pins.rs"

die() {
    echo "$(basename "$0"): $*" >&2
    exit 1
}

need() {
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null 2>&1 || die "needs '$tool' on PATH"
    done
}

sha256_of() {
    sha256sum "$1" | cut -c1-64
}

# check_file FILE SHA256: fails unless FILE exists with that digest.
check_file() {
    local file=$1 want=$2 got
    [ -f "$file" ] || die "$file is missing"
    got=$(sha256_of "$file")
    [ "$got" = "$want" ] || die "$file has SHA-256 $got, expected $want"
}

# file_ok FILE SHA256: true when FILE exists with that digest.
file_ok() {
    [ -f "$1" ] && [ "$(sha256_of "$1")" = "$2" ]
}

# pinned_in_rust SHA256 WHAT: fails unless pins.rs records that digest.
pinned_in_rust() {
    [ -f "$PINS_RS" ] || die "$PINS_RS is missing"
    grep -q "\"$1\"" "$PINS_RS" ||
        die "$2 ($1) is not in crates/speech/src/pins.rs; update this script to match the pins"
}

# fetch URL SHA256 DEST: downloads URL to DEST unless DEST already has the digest, and
# fails unless the download has it. A partial or wrong download never takes DEST's name.
fetch() {
    local url=$1 want=$2 dest=$3
    if file_ok "$dest" "$want"; then
        return 0
    fi
    mkdir -p "$(dirname "$dest")"
    echo "downloading $url" >&2
    curl -fsSL --retry 3 --retry-delay 2 -o "$dest.part" "$url" ||
        die "download failed: $url"
    local got
    got=$(sha256_of "$dest.part")
    if [ "$got" != "$want" ]; then
        rm -f "$dest.part"
        die "$url has SHA-256 $got, expected $want"
    fi
    mv "$dest.part" "$dest"
}

# install_file SRC DEST: copies SRC to DEST through a temporary name.
install_file() {
    mkdir -p "$(dirname "$2")"
    cp "$1" "$2.part"
    mv "$2.part" "$2"
}
