# Controller dev tools

Programmatic device access via the controller API, for development sessions
without a dashboard. All three run **inside the controller container** (they
mint a temporary session token directly in the SQLite `sessions` table, use
it as `Authorization: Bearer`, and delete it afterwards):

```bash
docker cp controller/tools/devshell.py echomuse-controller:/tmp/
docker exec echomuse-controller python /tmp/devshell.py "<shell command>" ["<another>" ...]
```

- **devshell.py** — run commands on a device over the `/shell` proxy (PTY
  mode; output includes echoed input — device is mksh + toybox only, no
  busybox or python). Defaults to the device hardcoded at
  the top; `-d <serial>` picks another (repeatable) and `--all` runs against
  every currently-connected device, which is what most diagnostic questions
  want. **Read-only in practice**: the shell is a child of the server, so
  stopping the service kills the shell mid-command and takes the device down
  until it is power cycled.
- **pull_so.py** — pull a file off the device (toybox base64 over the
  shell, echo disabled, split end-markers). Writes the decoded file to
  stdout: `python /tmp/pull_so.py /system/lib64/libled_hal.so > out.so`.
- **push_file.py** — the other direction, for arbitrary paths (firmware
  goes only through the controller's bundled-firmware update):
  `python /tmp/push_file.py <device_id> /tmp/oww_probe
  /data/local/tmp/oww_probe --chmod 755`. Reconnects every
  30s because the shell plane drops after ~50s under load, so it handles
  multi-megabyte files; **deletes the destination first** unless
  `--resume`, since resuming on size alone will append to a different file
  and then report success. Success means md5 agreement, nothing less.

**text_fix_replay.py** replays transcript correction (SPEC §16.8) over the
household's own turns. `record SNAPSHOT` runs in the container (it reads the
database and asks HA's non-executing debug matcher about every original and
candidate); `replay SNAPSHOT [--held-out 0.3] [--labels LABELS.json]` needs
neither HA nor models and reports the earlier (tuning) and later (held-out)
turns separately. The image does not carry it: `docker cp
controller/tools/text_fix_replay.py echomuse-controller:/app/tools/`, then
`docker exec echomuse-controller python /app/tools/text_fix_replay.py …`.
The snapshot holds household data: keep it out of the repository.
