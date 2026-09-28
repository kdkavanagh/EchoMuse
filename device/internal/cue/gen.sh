#!/bin/sh
# Regenerate the embedded cue PCM from its source file.
#
# The .pcm beside this script is what go:embed ships; the .flac is kept only
# as provenance and as the input here. Run this if the source is ever
# replaced — the embedded file is a build artefact that happens to be
# committed, in the same spirit as internal/wakeword/testdata's fixture.
#
# 48kHz mono S16_LE is the render mixer's native format, so anything else
# would require device-side resampling.
set -eu
cd "$(dirname "$0")"

ffmpeg -v error -y -i wake_word_triggered.flac \
	-f s16le -acodec pcm_s16le -ac 1 -ar 48000 \
	wake_word_triggered.pcm

ls -l wake_word_triggered.pcm
