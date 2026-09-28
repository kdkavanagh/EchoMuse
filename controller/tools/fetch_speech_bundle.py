#!/usr/bin/env python3
"""Fetch and verify the pinned §16.6 controller speech bundle.

Production use downloads the three release artifacts. Tests/offline installs can
supply local files with `--wheel`, `--archive`, and `--vad`, or `--local-dir`
containing their manifest filenames. Only the four runtime Kroko members are
extracted; the archive stays for install-time qualification.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from em_speech_bundle import ATTRIBUTION_TEXT, MANIFEST_PATH, qualify_bundle, verify_bundle  # noqa: E402


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _verify(path: Path, entry: dict, label: str) -> None:
    if path.stat().st_size != entry["size"] or _sha256(path) != entry["sha256"]:
        raise SystemExit(f"{label} failed size/SHA-256 verification: {path}")


def _install(source: Path | None, destination: Path, entry: dict, label: str) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{destination.name}.", suffix=".part", dir=destination.parent)
    os.close(fd)
    tmp = Path(tmp_name)
    try:
        if source is None:
            request = urllib.request.Request(entry["url"], headers={"User-Agent": "EchoMuse-speech-bundle/1"})
            with urllib.request.urlopen(request, timeout=120) as response, open(tmp, "wb") as out:
                shutil.copyfileobj(response, out, length=1024 * 1024)
        else:
            shutil.copyfile(source, tmp)
        _verify(tmp, entry, label)
        os.chmod(tmp, 0o644)
        os.replace(tmp, destination)
    finally:
        tmp.unlink(missing_ok=True)


def _source(explicit: str | None, local_dir: Path | None, entry: dict) -> Path | None:
    if explicit:
        return Path(explicit)
    if local_dir is not None:
        path = local_dir / Path(entry["path"]).name
        if not path.is_file():
            raise SystemExit(f"local artifact is missing: {path}")
        return path
    return None


def _extract(archive_path: Path, root: Path, manifest: dict) -> None:
    prefix = manifest["asr"]["name"] + "/"
    wanted = {prefix + Path(entry["path"]).name: (name, entry)
              for name, entry in manifest["asr"]["files"].items()}
    found = set()
    with tarfile.open(archive_path, "r:bz2") as archive:
        for member in archive:
            target = wanted.get(member.name)
            if target is None:
                continue
            name, entry = target
            if not member.isfile() or member.issym() or member.islnk():
                raise SystemExit(f"Kroko member is not a regular file: {member.name}")
            source = archive.extractfile(member)
            if source is None:
                raise SystemExit(f"cannot read Kroko member: {member.name}")
            destination = root / entry["path"]
            destination.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp_name = tempfile.mkstemp(prefix=f".{destination.name}.", suffix=".part", dir=destination.parent)
            try:
                with os.fdopen(fd, "wb") as out:
                    shutil.copyfileobj(source, out, length=1024 * 1024)
                    out.flush()
                    os.fsync(out.fileno())
                tmp = Path(tmp_name)
                _verify(tmp, entry, f"Kroko {name}")
                os.chmod(tmp, 0o644)
                os.replace(tmp, destination)
                found.add(member.name)
            finally:
                Path(tmp_name).unlink(missing_ok=True)
    missing = set(wanted) - found
    if missing:
        raise SystemExit(f"Kroko archive lacks members: {', '.join(sorted(missing))}")


def fetch(destination: Path, *, wheel: str | None = None, archive: str | None = None,
          vad: str | None = None, local_dir: Path | None = None,
          qualify: bool = False) -> None:
    manifest = json.loads(MANIFEST_PATH.read_text(encoding="utf-8"))
    destination.mkdir(parents=True, exist_ok=True)
    wheel_entry = manifest["runtime"]["wheel"]
    archive_entry = manifest["asr"]["archive"]
    vad_entry = manifest["vad"]
    _install(_source(wheel, local_dir, wheel_entry), destination / wheel_entry["path"], wheel_entry, "wheel")
    _install(_source(archive, local_dir, archive_entry), destination / archive_entry["path"], archive_entry, "archive")
    _install(_source(vad, local_dir, vad_entry), destination / vad_entry["path"], vad_entry, "Silero v5")
    _extract(destination / archive_entry["path"], destination, manifest)
    attribution = destination / manifest["attribution"]["path"]
    attribution.write_text(ATTRIBUTION_TEXT, encoding="utf-8")
    bundle = verify_bundle(destination, check_installed_package=False)
    if qualify:
        result = qualify_bundle(bundle)
        print(json.dumps({"blank_slope_per_s": result.blank_slope_per_s,
                          "final_trailing_blank_frames": result.final_trailing_blank_frames,
                          "text": result.text}, sort_keys=True))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--wheel", help="local pinned sherpa-onnx wheel")
    parser.add_argument("--archive", help="local pinned Kroko .tar.bz2")
    parser.add_argument("--vad", help="local pinned silero_vad_v5.onnx")
    parser.add_argument("--local-dir", type=Path, help="directory containing all three manifest filenames")
    parser.add_argument("--qualify", action="store_true", help="run the install-time trailing-blank gate")
    args = parser.parse_args()
    fetch(args.destination, wheel=args.wheel, archive=args.archive, vad=args.vad,
          local_dir=args.local_dir, qualify=args.qualify)


if __name__ == "__main__":
    main()
