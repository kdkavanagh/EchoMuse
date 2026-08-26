# Changelog

## Unreleased

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
