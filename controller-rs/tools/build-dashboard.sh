#!/usr/bin/env bash
# Builds the dashboard's static directory into OUT (default: controller-rs/local/static):
# controller/static as it is in the repository, the browser libraries and fonts its pages
# load from static/vendor/, and dashboard.js compiled from dashboard.jsx by esbuild 0.20.2
# (JSX to React.createElement, no bundling). Every download is checked by SHA-256; the
# esbuild archives' digests were checked against the npm registry's published integrity,
# and React's against cdnjs's.
#
# Usage: tools/build-dashboard.sh [OUT]

# shellcheck source=common.sh
source "$(dirname "$0")/common.sh"

out="$RS_DIR/local/static"
case ${1:-} in
-h | --help)
    sed -n '2,/^$/s/^# \{0,1\}//p' "$0"
    exit 0
    ;;
-*) die "unknown option $1" ;;
"") ;;
*) out=$1 ;;
esac

need curl tar sha256sum

case $(uname -m) in
x86_64)
    esbuild_pkg=linux-x64
    esbuild_sha=552f7b19b3422ad79a90d2d0e21948fb5c5ad0f85f832161d99468a73bae95f2
    ;;
aarch64)
    esbuild_pkg=linux-arm64
    esbuild_sha=482c03216e8cccd641b55e25931ca4f5d585504b546a7b4cd23401adaeeb5ee3
    ;;
*) die "no pinned esbuild for $(uname -m)" ;;
esac

src="$REPO_DIR/controller/static"
[ -f "$src/dashboard.jsx" ] || die "$src/dashboard.jsx is missing"
mkdir -p "$out"
out=$(cd "$out" && pwd)
downloads="$out/../.downloads"

esbuild_archive="$downloads/esbuild-$esbuild_pkg-0.20.2.tgz"
fetch "https://registry.npmjs.org/@esbuild/$esbuild_pkg/-/$esbuild_pkg-0.20.2.tgz" \
    "$esbuild_sha" "$esbuild_archive"
esbuild_dir="$downloads/esbuild-$esbuild_pkg"
rm -rf "$esbuild_dir"
mkdir -p "$esbuild_dir"
tar -xzf "$esbuild_archive" -C "$esbuild_dir" package/bin/esbuild

# Google Fonts refuses woff2 to clients it does not recognise.
font_agent="Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
vendor() {
    local url=$1 sha=$2 name=$3
    local cached="$downloads/vendor/$name"
    if ! file_ok "$cached" "$sha"; then
        mkdir -p "$(dirname "$cached")"
        echo "downloading $url" >&2
        curl -fsSL --retry 3 -A "$font_agent" -o "$cached.part" "$url" || die "download failed: $url"
        [ "$(sha256_of "$cached.part")" = "$sha" ] || {
            rm -f "$cached.part"
            die "$url does not have SHA-256 $sha"
        }
        mv "$cached.part" "$cached"
    fi
}

vendor https://cdnjs.cloudflare.com/ajax/libs/react/18.2.0/umd/react.production.min.js \
    4b4969fa4ef3594324da2c6d78ce8766fbbc2fd121fff395aedf997db0a99a06 react.min.js
vendor https://cdnjs.cloudflare.com/ajax/libs/react-dom/18.2.0/umd/react-dom.production.min.js \
    21758ed084cd0e37e735722ee4f3957ea960628a29dfa6c3ce1a1d47a2d6e4f7 react-dom.min.js
vendor https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.min.js \
    4196e242ef1cf4c2adead8d97f4a772a69576076f70b095e004b4abbb049e7bf xterm.js
vendor https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.min.css \
    f7f724aea2bb620a6482bfb8e4bdecfae1152b0c7facef55fbda61f3b6cfedb2 xterm.css
vendor https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.min.js \
    a6a7bbb33569f16aa3e18d71425e34d035fc89a0b7e8cba084f8855f91aa38f1 addon-fit.js
vendor https://fonts.gstatic.com/s/dmsans/v17/rP2Yp2ywxg089UriI5-g4vlH9VoD8Cmcqbu0-K4.woff2 \
    9fea608a947e67020c33cad9a6fe3d60c54119dfb8cff87768a8117a15ed7543 fonts/dm-sans.woff2
vendor https://fonts.gstatic.com/s/dmmono/v16/aFTR7PB1QTsUX8KYvumzEYOtbQ.woff2 \
    9964608a849396bd00c4bfd7034afe03486469dcf20b4f6b8cbdfdd310369951 fonts/dm-mono-400.woff2
vendor https://fonts.gstatic.com/s/dmmono/v16/aFTU7PB1QTsUX8KYthqQBA.woff2 \
    e1896b13b2b1bb112fac2f9571bd6c40e118746e77a4511edbf43fbb41bf3e1e fonts/dm-mono-500.woff2

# Assemble beside OUT, then swap it in, so OUT is never half built.
staging="$out.staging"
rm -rf "$staging"
cp -R "$src" "$staging"
rm -f "$staging/dashboard.js"
mkdir -p "$staging/vendor"
cp -R "$downloads/vendor/." "$staging/vendor/"
"$esbuild_dir/package/bin/esbuild" "$staging/dashboard.jsx" \
    --bundle=false \
    --jsx=transform \
    --jsx-factory=React.createElement \
    --jsx-fragment=React.Fragment \
    --log-level=warning \
    --outfile="$staging/dashboard.js"
rm -rf "$esbuild_dir" "$out"
mv "$staging" "$out"
echo "dashboard: $out"
