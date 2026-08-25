"""
em_oww_assets.py — on-device wake word asset distribution
==========================================================

The device can run the wake model itself (see "On-device wake word" in
CLAUDE.md), but the ONNX Runtime and the models are deliberately NOT in the
firmware: 12.3MB would double the OTA payload and both A/B slots, and the
runtime changes far less often than the binary. They are installed out of
band, which until now meant `controller/tools/push_file.py` by hand — fine
for us, a wall for anyone else, and the blocker for `owwOnDevice=on`.

What has to land on a device, in `shadow.DefaultDir`:

    libonnxruntime.so     12.3MB   the dlopen'd runtime
    melspectrogram.onnx    1.1MB   shared feature models, same for every
    embedding_model.onnx   1.3MB   wake word
    <stem>.onnx           ~0.9MB   one classifier per wake model

The runtime is vendored into the controller image at build time (pinned by
version + sha256, the same treatment the Dockerfile already gives xterm and
DTLN), so devices never need internet. The models come from the installed
openwakeword package, or from `oww_models/` for custom ones — no second copy
to keep in step.

This module is pure path/planning logic — no aiohttp, no db, no websockets —
so the part that decides what to push and what to delete is unit-tested
rather than only exercised against a live device. The transport lives in
em_api.py.

**md5 is the only definition of success.** Not exit status, not byte count:
the shell transport has produced files that were the right length and wrong
content, and a corrupt libonnxruntime.so fails at dlopen with an error that
says nothing about why.
"""

from __future__ import annotations

import hashlib
import os
from dataclasses import dataclass, field
from pathlib import Path

import em_oww_models
import em_wake_scorer

# Where the device expects everything. Must match shadow.DefaultDir in
# device/internal/wakeword/shadow/open.go — there is a test.
DEVICE_DIR = "/data/local/share/echomuse/oww"

RUNTIME_NAME = "libonnxruntime.so"

# Shared feature models: identical for every wake word, so they are pushed
# once and never re-pushed when the model changes.
SHARED_NAMES = ("melspectrogram.onnx", "embedding_model.onnx")

# Where the vendored ARM runtime lands in the image (see Dockerfile).
RUNTIME_DIR = "/app/models/oww_runtime"

# Classifier slots kept on a device, any mix of stock and custom.
#
# Four rather than "just the selected one" so switching models back and forth
# — which is exactly what A/B-ing two wake words involves — does not re-push
# ~0.9MB each time. Four is ~18MB worst case including the runtime, against
# 346-374MB free on these devices.
CLASSIFIER_SLOTS = 4

# Refuse to push if the device would be left below this. Devices have been
# seen at 346MB free, so this is not a limit anyone should hit — it exists so
# that a device which HAS filled up fails with a clear reason instead of a
# half-written file and a dlopen error.
FREE_FLOOR_MB = 50


def device_path(name: str) -> str:
    """Absolute path of an asset on the device."""
    return f"{DEVICE_DIR}/{name}"


def md5_file(path: str | Path, _chunk: int = 1 << 20) -> str:
    h = hashlib.md5()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(_chunk), b""):
            h.update(block)
    return h.hexdigest()


def runtime_source(runtime_dir: str | Path = RUNTIME_DIR) -> Path | None:
    """
    The vendored ARM runtime, or None if the image was built without it.

    None is an ordinary condition worth reporting rather than raising on: a
    controller built from an older Dockerfile simply cannot install assets,
    and saying so is more useful than a stack trace.
    """
    p = Path(runtime_dir) / RUNTIME_NAME
    return p if p.is_file() else None


def openwakeword_resources() -> Path | None:
    """
    The installed openwakeword package's bundled models directory.

    Imported lazily and tolerantly: this module is unit-tested in an
    environment that deliberately does not have openwakeword (see CLAUDE.md
    on keeping the test deps to pytest/numpy/scipy).
    """
    try:
        import openwakeword  # noqa: F401
    except Exception:
        return None
    d = Path(openwakeword.__file__).parent / "resources" / "models"
    return d if d.is_dir() else None


def classifier_source(oww_model: str,
                      resources: Path | None,
                      models_dir: Path | None) -> Path | None:
    """
    Resolve an `owwModel` config value to the .onnx the DEVICE needs.

    Mirrors em_oww_models.prediction_key: a value ending in .onnx is a custom
    model referenced by path, anything else is a stock name. The device
    derives the same stem (shadow.ModelStem), so the filename it looks for
    and the file we send are the same by construction rather than by
    convention.
    """
    name = (oww_model or "").strip()
    if not name:
        return None
    if name.endswith(".onnx"):
        p = Path(name)
        if not p.is_absolute() and models_dir is not None:
            p = models_dir / p.name
        return p if p.is_file() else None
    if resources is None:
        return None
    p = resources / f"{name}.onnx"
    return p if p.is_file() else None


@dataclass
class Asset:
    """One file that should exist on the device."""
    name: str
    source: Path
    md5: str
    size: int
    kind: str  # "runtime" | "shared" | "classifier"


@dataclass
class Plan:
    push: list[Asset] = field(default_factory=list)
    prune: list[str] = field(default_factory=list)
    keep: list[str] = field(default_factory=list)
    # Set when the plan cannot be carried out; push/prune are then empty.
    blocked: str | None = None

    @property
    def push_bytes(self) -> int:
        return sum(a.size for a in self.push)

    @property
    def is_noop(self) -> bool:
        return not self.push and not self.prune and not self.blocked


def desired_assets(models: list[str],
                   runtime_dir: str | Path = RUNTIME_DIR,
                   resources: Path | None = None,
                   models_dir: Path | None = None,
                   bcresnet_capable: bool = False) -> tuple[list[Asset], list[str]]:
    """
    Build the asset list for a device that should be able to score `models`.

    Returns (assets, problems). A model whose file cannot be found is a
    problem reported by name, NOT an exception and NOT a silent omission:
    "the toggle does nothing" with no visible cause is the failure mode this
    whole feature exists to remove.

    `models` is ordered most-important-first; the first entry is the one the
    device is currently configured to use.

    `bcresnet_capable` is the device's `oww_bcresnet` capability. A BC-ResNet
    model is planned as a PAIR — the .onnx plus its .json sidecar — because the
    device decides which engine to load from the sidecar's presence, so sending
    the model without it produces a device that loads the openWakeWord pipeline
    against a graph that is not an openWakeWord classifier. Defaults to False so
    a caller that has not been taught about the capability refuses rather than
    ships a half-installed pair.
    """
    if resources is None:
        resources = openwakeword_resources()
    if models_dir is None:
        try:
            models_dir = em_oww_models.models_dir()
        except Exception:
            models_dir = None

    assets: list[Asset] = []
    problems: list[str] = []

    rt = runtime_source(runtime_dir)
    if rt is None:
        problems.append(
            "ONNX Runtime is not vendored in this controller image — "
            "rebuild it, or update to a release that bundles it"
        )
    else:
        assets.append(Asset(RUNTIME_NAME, rt, md5_file(rt),
                            rt.stat().st_size, "runtime"))

    for name in SHARED_NAMES:
        p = (resources / name) if resources else None
        if p is None or not p.is_file():
            problems.append(f"{name} not found in the installed openwakeword package")
            continue
        assets.append(Asset(name, p, md5_file(p), p.stat().st_size, "shared"))

    seen: set[str] = set()
    for model in models:
        stem = em_oww_models.prediction_key(model)
        if not stem or stem in seen:
            continue
        seen.add(stem)
        is_bcresnet = em_wake_scorer.is_bcresnet_model(model)
        if is_bcresnet and not bcresnet_capable:
            # A BC-ResNet .onnx is a whole detector — its own log-mel frontend
            # and a 22400-sample audio input — where openWakeWord's scorer
            # expects a classifier HEAD taking [1,16,96] off the shared
            # embedding model. Firmware without the engine would install a file
            # named exactly what it looks for and fail at session creation,
            # which reads as a broken install rather than an unsupported model.
            problems.append(
                f"wake model '{stem}' is BC-ResNet and this device's firmware "
                f"has no BC-ResNet engine — the controller scores it instead; "
                f"update the device, or pick an openWakeWord model"
            )
            continue
        src = classifier_source(model, resources, models_dir)
        if src is None:
            problems.append(f"wake model '{model}' has no .onnx the device could use")
            continue
        assets.append(Asset(f"{stem}.onnx", src, md5_file(src),
                            src.stat().st_size, "classifier"))
        if is_bcresnet:
            # The sidecar is what makes the device choose the BC-ResNet engine,
            # so the pair is planned together and never separately. A model
            # without its sidecar is silently the wrong engine; a sidecar
            # without its model is an error that at least names itself.
            side = em_wake_scorer.sidecar_path(src)
            if not side.is_file():
                problems.append(
                    f"wake model '{stem}' has no {side.name} beside it — "
                    f"a BC-ResNet model cannot be installed without its sidecar"
                )
                assets.pop()
                continue
            assets.append(Asset(side.name, side, md5_file(side),
                                side.stat().st_size, "sidecar"))

    return assets, problems


def plan_sync(desired: list[Asset],
              actual: dict[str, tuple[str, int]],
              free_mb: int | None = None,
              slots: int = CLASSIFIER_SLOTS) -> Plan:
    """
    Decide what to push and what to delete. Pure.

    `actual` maps a filename on the device to (md5, mtime_epoch). mtime is
    the recency signal for eviction — it is set by the push itself, and the
    sync touches the selected classifier, so "least recently used" needs no
    controller-side bookkeeping and survives a controller restart or a
    database wipe.

    Eviction only ever applies to CLASSIFIERS. The runtime and the shared
    feature models are required by definition; pruning either would be
    deleting something we are about to push back.

    The first desired classifier is pinned: it is the model the device is
    configured to use, so evicting it is the one outcome that would visibly
    break the device rather than merely cost a re-push.
    """
    p = Plan()

    want = {a.name: a for a in desired}
    for a in desired:
        have = actual.get(a.name)
        if have is not None and have[0] == a.md5:
            p.keep.append(a.name)
        else:
            p.push.append(a)

    # Free-space check AFTER working out what actually needs sending: a
    # device that already has everything must never be blocked for space.
    if free_mb is not None and p.push:
        need_mb = p.push_bytes / (1024 * 1024)
        if free_mb - need_mb < FREE_FLOOR_MB:
            p.blocked = (
                f"device has {free_mb}MB free; installing needs "
                f"{need_mb:.0f}MB and must leave {FREE_FLOOR_MB}MB"
            )
            p.push, p.prune, p.keep = [], [], []
            return p

    # Extra classifiers already on the device, newest first. Anything that is
    # not a .onnx, or is one of the shared/desired files, is left alone —
    # this deletes only files it positively recognises as evictable.
    required = set(want) | set(SHARED_NAMES) | {RUNTIME_NAME}
    extras = sorted(
        (n for n in actual if n.endswith(".onnx") and n not in required),
        key=lambda n: actual[n][1],
        reverse=True,
    )

    desired_classifiers = [a.name for a in desired if a.kind == "classifier"]
    room = max(0, slots - len(desired_classifiers))
    p.keep.extend(extras[:room])
    p.prune.extend(extras[room:])

    # A sidecar is evicted WITH its classifier. Leaving one behind would make
    # the device take the BC-ResNet path for a stem whose model is gone — and,
    # worse, for whatever openWakeWord model is later installed under that
    # name, since the sidecar's presence is the whole of the engine decision.
    # Orphans with no matching .onnx on the device are still left alone: this
    # only cleans up after its own eviction, keeping "delete only what it
    # positively recognises" true.
    for name in list(p.prune):
        side = name[: -len(".onnx")] + ".json"
        if side in actual and side not in required:
            p.prune.append(side)

    return p


def parse_free_mb(df_line: str) -> int | None:
    """
    Free megabytes from a `busybox df -m` data line.

    Parsed here rather than with awk because the column INDEX is not stable:
    busybox wraps a long filesystem name onto its own line, so the data row is
    sometimes "1010 648 346 65% /data" and sometimes
    "/dev/block/x 1010 648 346 65% /data". An awk $4 gets one of those right
    and silently returns "65%" for the other — which parsed as no reading at
    all, and would have disabled the free-space check without saying so.

    The use-percentage column is the anchor: available is always the field
    immediately before it.
    """
    fields = (df_line or "").split()
    for i, f in enumerate(fields):
        if f.endswith("%") and i > 0 and fields[i - 1].isdigit():
            return int(fields[i - 1])
    return None


def parse_device_listing(text: str) -> dict[str, tuple[str, int]]:
    """
    Parse the device-side inventory into {name: (md5, mtime_epoch)}.

    The device is asked for `md5sum` and an mtime per file, one file per
    line as "<md5> <mtime> <name>". Anything unparseable is skipped rather
    than guessed at — a partial inventory makes the sync push more than it
    needed to, which is wasteful but correct; a mis-parsed one makes it push
    nothing and report success.
    """
    out: dict[str, tuple[str, int]] = {}
    for line in (text or "").splitlines():
        parts = line.strip().split()
        if len(parts) != 3:
            continue
        digest, mtime, name = parts
        if len(digest) != 32 or not all(c in "0123456789abcdef" for c in digest.lower()):
            continue
        if not mtime.isdigit():
            continue
        out[os.path.basename(name)] = (digest.lower(), int(mtime))
    return out
