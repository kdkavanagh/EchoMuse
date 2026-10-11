"""The device firmware bundled with this controller.

The controller installs exactly one firmware build on Dots: the `server`
binary in FIRMWARE_DIR, with the `version` file beside it naming the string
compiled into it as client.Version (what the Dot reports in session.hello
`firmware_version`). The image carries both at /app/firmware; bare metal
points FIRMWARE_DIR at device/build, which device/compile.sh fills.
"""

from __future__ import annotations

import hashlib
import os
from dataclasses import dataclass
from pathlib import Path

FIRMWARE_ENV = "FIRMWARE_DIR"
FIRMWARE_DIR = Path(os.environ.get(FIRMWARE_ENV, "/app/firmware"))
BINARY_NAME = "server"
VERSION_NAME = "version"
_HASH_CHUNK = 1024 * 1024


class FirmwareError(Exception):
    pass


def _sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@dataclass(frozen=True, slots=True)
class BundledFirmware:
    """The bundled binary as verified at startup."""

    version: str
    size: int
    sha256: str
    path: Path

    def read(self) -> bytes:
        """The binary's bytes. Refuses a file that changed since `load`: those
        bytes would be installed under a version they may not report."""
        data = self.path.read_bytes()
        if len(data) != self.size or _sha256(data) != self.sha256:
            raise FirmwareError(f"{self.path} changed since the controller started "
                                f"(expected {self.version}, sha256 {self.sha256}); "
                                f"restart the controller to bundle the new build")
        return data


def load(directory: Path = FIRMWARE_DIR) -> BundledFirmware:
    """Verify and describe the bundled firmware in `directory`."""
    binary = directory / BINARY_NAME
    version_file = directory / VERSION_NAME
    if not binary.is_file():
        raise FirmwareError(f"firmware binary is missing: {binary}")
    if not version_file.is_file():
        raise FirmwareError(f"firmware version file is missing: {version_file}")

    try:
        version = version_file.read_text(encoding="utf-8").strip()
    except UnicodeDecodeError as err:
        raise FirmwareError(f"firmware version file is not UTF-8: {version_file}") from err
    if not version:
        raise FirmwareError(f"firmware version file is blank: {version_file}")
    if "\n" in version or "\r" in version:
        raise FirmwareError(f"firmware version file must be a single line: {version_file}")

    digest = hashlib.sha256()
    size = 0
    with binary.open("rb") as source:
        while chunk := source.read(_HASH_CHUNK):
            digest.update(chunk)
            size += len(chunk)
    if size == 0:
        raise FirmwareError(f"firmware binary is empty: {binary}")
    return BundledFirmware(version=version, size=size, sha256=digest.hexdigest(), path=binary)
