#!/bin/sh
# Regenerate the embedded cue PCM.
#
# The .pcm files beside this script are what go:embed ships: build artefacts
# that happen to be committed, in the same spirit as
# internal/wakeword/testdata's fixture. The wake chime, the announcement
# chime and the offline notice are decoded from their .flac/.mp3, kept only as
# provenance and as the input here; the no-input chime is synthesized below,
# so this script is its source.
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

# The announcement chime, played before a Home Assistant announcement:
# alerts_notification_01.mp3, the Amazon Echo's own announcement earcon.
# Decoded as-is, 1.44 s at 48 kHz, its stereo downmixed (peak -6.5 dBFS).
out=alerts_notification_01.pcm
pcm -i alerts_notification_01.mp3

# The offline notice, spoken when the wake word is heard with no controller.
# offline_notice.mp3 is Home Assistant's TTS for "I can't connect to the
# smart home right now.", rendered 2026-10-11 by a TTS-only run of the Dots'
# Assist pipeline ("STT": tts.elevenlabs_text_to_speech, voice
# Xb7hH8MSUJpSbSDYk0k2) -- the voice the Dots answer in -- with
#   {"type": "assist_pipeline/run", "start_stage": "tts", "end_stage": "tts",
#    "input": {"text": "..."}, "pipeline": "<id>"}
# and fetching tts-end's tts_output.url. Decoded exactly as the controller
# decodes a live reply (em_media), so it plays at a reply's level; its own
# ~0.24 s tail is the pause before the no-input chime.
out=offline_notice.pcm
pcm -i offline_notice.mp3

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
