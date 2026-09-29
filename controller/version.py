"""
version.py — controller version resolution
============================================

The controller is versioned independently of the device firmware:
device binaries are released from plain `v*` tags (embedded via
-ldflags at compile time), the controller from `controller-v*` tags
(baked into the Docker image as the EM_CONTROLLER_VERSION env var by
.github/workflows/controller-release.yml).

Resolution order:
  1. EM_CONTROLLER_VERSION env var — set in the published image; also
     the override hook for anyone building their own image.
  2. `git describe --tags --match 'controller-v*'` — bare-metal runs
     from a git checkout. The `controller-` prefix is stripped so the
     displayed form matches the image's ("v2.8.0", or
     "v2.8.0-3-gabc1234-dirty" between tags).
  3. "dev" — no env var, no git (e.g. a bare source copy).
"""

from __future__ import annotations

import os
import subprocess
from enum import StrEnum
from typing import TypedDict

_PREFIX = "controller-"


class UpdateStatus(StrEnum):
    UPDATE = "update"
    CURRENT = "current"
    UNKNOWN = "unknown"


class UpdateCheck(TypedDict):
    """The `status`/`available` pair merged into the controller-update API body."""
    status: UpdateStatus
    available: bool


def _resolve() -> str:
    env = os.environ.get("EM_CONTROLLER_VERSION")
    if env:
        return env

    try:
        out = subprocess.run(
            ["git", "describe", "--tags", "--match", f"{_PREFIX}v*", "--dirty"],
            capture_output=True,
            text=True,
            timeout=3,
            cwd=os.path.dirname(os.path.abspath(__file__)),
        )
        described = out.stdout.strip()
        if out.returncode == 0 and described:
            return described.removeprefix(_PREFIX)
    except (OSError, subprocess.SubprocessError, UnicodeDecodeError):
        pass    # no git binary, not a checkout, or a hung/garbled describe: "dev"

    return "dev"


VERSION = _resolve()


def parse(text: str) -> tuple[int, ...] | None:
    """
    ("v2.10.0", "controller-v2.10.0", "v2.10.0-3-gabc1234-dirty") -> (2, 10, 0).

    None for anything that is not a version at all ("dev", a bare source
    copy). Callers must treat that as "cannot compare" rather than as zero —
    a controller that does not know its own version has no business claiming
    to be out of date.
    """
    text = (text or "").strip().removeprefix(_PREFIX).lstrip("v")
    if not text:
        return None
    parts = text.split("-")[0].split(".")
    if not parts or not all(p.isdigit() for p in parts):
        return None
    return tuple(int(p) for p in parts[:3])


def compare(current: str, latest: str) -> UpdateCheck:
    """
    Is `latest` newer than the running `current`?

    Three outcomes, and the distinction matters more than a boolean would:
      - "update"  — a strictly newer version exists.
      - "current" — running the newest, or a build AHEAD of it. A local build
                    between tags describes as v2.10.0-3-gabc1234, whose base
                    parses equal to v2.10.0, so a `>` test alone is right here
                    only because it is strict; anything looser would claim an
                    update forever on a dev checkout.
      - "unknown" — the running version is not comparable ("dev"). Say so
                    rather than guessing: a false "up to date" is worse than
                    an honest shrug, because it is the answer that stops
                    someone looking.
    """
    c, l = parse(current), parse(latest)
    if c is None or l is None:
        return {"status": UpdateStatus.UNKNOWN, "available": False}
    if l > c:
        return {"status": UpdateStatus.UPDATE, "available": True}
    return {"status": UpdateStatus.CURRENT, "available": False}
