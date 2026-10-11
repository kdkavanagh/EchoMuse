#!/bin/sh
# Regenerate the embedded cue PCM.
#
# The .pcm files beside this script are what go:embed ships: build artefacts
# that happen to be committed, in the same spirit as
# internal/wakeword/testdata's fixture. Each is decoded from the .flac/.mp3
# of the same name, kept only as provenance and as the input here.
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

# The no-input chime, ending a wake that heard no command, and closing the
# offline notice: ui_error_generic_2.mp3, the Amazon Echo's own error earcon.
# Decoded as-is, 0.68 s at 48 kHz, its stereo downmixed (peak -12.9 dBFS).
out=ui_error_generic_2.pcm
pcm -i ui_error_generic_2.mp3

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
