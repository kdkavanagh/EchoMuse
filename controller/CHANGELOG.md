# Changelog

## Unreleased

- ASR gain: the stream handed to Home Assistant for recognition is now
  amplified by `ASR_GAIN_NOMINAL_DB` (20 dB), clamped at the int16 rail. This
  hardware has no AGC — ten consecutive utterances measured -37.6 to
  -46.2 dBFS peak — and faster-whisper fails there by silently dropping the
  quiet leading words rather than by erroring, so "How many ounces are in a
  cup?" arrives as "ounces are in a cup." and the intent never matches.
  Applied below the denoiser and above the capture tap, so the saved
  utterance still matches what STT heard, and after `_is_speech`, the
  relative endpointer and the noise floor, so every threshold calibrated
  against raw levels is untouched. Set to 0 to disable.
- The gain is bounded by the wake word's own measured loudness
  (`device.last_wake_db`, the same anchor the relative endpointer is seeded
  from) as a guard band, not as a normaliser. Normalising 1:1 was measured
  and rejected: over eight turns the wake peak sits a mean -0.3 dB from the
  command peak, but with +4.7/-5.6 dB of scatter and r = -0.17, so
  subtracting it widens the post-gain spread from 7.3 dB to 11.7 dB. Instead
  the nominal gain is held while the predicted level lands in the band that
  decodes well (-22 .. -34 dBFS), and only followed outside it — backing off
  a close talker who would clip, and pushing a distant one up to a 30 dB
  ceiling. Returns exactly nominal on every turn measured so far.

- Wake clips: the 1.4 seconds of audio that actually crossed the wake
  threshold are now kept per turn, opt-in per device (`saveWakeClips`,
  Config → Wake word). A turn's utterance recording cannot contain the wake
  word — the preroll discard exists to remove it — so until now a false
  trigger left no evidence, and there was nothing to feed back to
  `oww_forge` as a training negative.
- Each turn's clip plays and downloads from its row in Activity; every clip
  for a device comes down as one archive
  (`GET /api/devices/{id}/wakeclips.zip`) for dropping straight into a
  training corpus.
- Schema v20: `turns.wake_file`. Clips live in `wakes/<device>/` beside the
  database, 500 per device, and are removed with the device.

## 1.0.1

- Initial Home Assistant Supervisor add-on packaging: install and run the
  controller from Settings → Add-ons instead of hand-run docker-compose.
- Ingress support for the dashboard (no separate port to expose).
- Add-on config UI labels, icon, and logo.
