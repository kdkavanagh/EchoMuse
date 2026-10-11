#!/bin/sh
# Regenerate the embedded cue PCM.
#
# The .pcm files beside this script are what go:embed ships: build artefacts
# that happen to be committed, in the same spirit as
# internal/wakeword/testdata's fixture. The wake chime is decoded from its
# .flac, kept only as provenance and as the input here; the no-input chime
# is synthesized below, so this script is its source.
#
# 48kHz mono S16_LE is the render mixer's native format, so anything else
# would require device-side resampling.
set -eu
cd "$(dirname "$0")"

pcm() {
	ffmpeg -v error -y "$@" -f s16le -acodec pcm_s16le -ac 1 -ar 48000 "$out"
	ls -l "$out"
}

out=wake_word_triggered.pcm
pcm -i wake_word_triggered.flac

# note FREQ ONSET DECAY AMPLITUDE: a struck tone with 4 ms attack whose 2nd
# and 3rd harmonics die 3x and 5x faster than the fundamental.
note() {
	u="(t-$2)"
	echo "$4*gte(t,$2)*min(1,$u/0.004)*(sin(2*PI*$1*$u)*exp(-$u/$3)+0.3*sin(4*PI*$1*$u)*exp(-3*$u/$3)+0.1*sin(6*PI*$1*$u)*exp(-5*$u/$3))"
}

# The no-input chime: the wake chime's two pitches (~792 Hz rising to
# ~939 Hz) falling instead, so "stopped listening" mirrors "listening";
# a little softer (peak -8.4 dBFS against the wake chime's -3.1).
out=no_input_chime.pcm
pcm -f lavfi -i "aevalsrc='$(note 939 0 0.08 0.24)+$(note 792 0.1 0.18 0.30)':s=48000:c=mono:d=0.8" \
	-af 'afade=t=out:st=0.65:d=0.15'
