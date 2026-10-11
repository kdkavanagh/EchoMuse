#!/bin/bash
# Builds the firmware exactly as the controller image bundles it: the
# `firmware` stage of controller/Dockerfile (same pinned toolchain, same
# source-hash version), exported to device/build/ as `server` + `version`.
#
# The controller installs only the firmware inside its own image, so a
# Docker deployment never needs this. It is for a bare-metal controller
# (FIRMWARE_DIR=<repo>/device/build) and for checking the tree compiles for
# the Dot.
set -e
REPO_ROOT=$(git rev-parse --show-toplevel)
OUT="$REPO_ROOT/device/build"

docker build \
  -f "$REPO_ROOT/controller/Dockerfile" \
  --target firmware-out \
  --output "type=local,dest=$OUT" \
  "$REPO_ROOT"

echo ""
echo "✓ Build succeeded → device/build/server  ($(cat "$OUT/version"))"
