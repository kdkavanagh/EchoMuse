"""Device speech assets and hash-addressed assets-socket reads (§16.1, §16.5)."""

from __future__ import annotations

import hashlib
import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterator

from em_wake_registry import WakeModel, WakeRegistry

RUNTIME_SHA256 = "174233cf1a3f841f1eac82a4328f4f23f8819d5c62969c09e994a6b1abf498c1"
RUNTIME_ENV = "ORT_ANDROID_LIB"
RUNTIME_DEFAULT = "/app/models/ort_android/libonnxruntime.so"
MAX_CHUNK_BYTES = 64 * 1024
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")


class AssetError(Exception):
    pass


class AssetNotFound(AssetError):
    """Answer the request with `{"error":"not_found"}`."""


class AssetCorrupt(AssetNotFound):
    """The file behind a hash no longer matches it; served as not_found."""


@dataclass(frozen=True, slots=True)
class SpeechAssets:
    runtime_sha256: str
    graph_sha256: str
    sidecar_sha256: str

    def wire(self) -> dict[str, str]:
        return {
            "runtime_sha256": self.runtime_sha256,
            "graph_sha256": self.graph_sha256,
            "sidecar_sha256": self.sidecar_sha256,
        }


@dataclass(frozen=True, slots=True)
class AssetInfo:
    sha256: str
    path: Path
    size: int


def installed_speech_assets(hello_assets: list[str], named: dict[str, str] | None,
                            wake_stats: dict | None) -> list[str]:
    """Speech assets the device holds: its `session.hello` list, plus the
    named set once `wake.stats` shows that graph loaded (the device fetches
    missing assets after hello, and loads runtime, graph and sidecar together).
    """
    installed = list(hello_assets)
    loaded = (named is not None and wake_stats is not None
              and wake_stats.get("graph_sha256") == named["graph_sha256"]
              and wake_stats.get("wake_unavailable") is None)
    if loaded:
        installed += [digest for digest in named.values() if digest not in installed]
    return installed


def sha256_file(path: str | os.PathLike) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


class DeviceAssets:
    """Resolve speech and alert assets by SHA-256.

    `alert_lookup(sha)` returns a path for an exported 48 kHz mono WAV or None.
    `session.ready` names the graph the device's effective config selects
    (`WakeRegistry.for_config`); every registered graph can be served.
    """

    def __init__(self, registry: WakeRegistry,
                 alert_lookup: Callable[[str], str | os.PathLike | None],
                 runtime_path: str | os.PathLike | None = None):
        self.registry = registry
        self.alert_lookup = alert_lookup
        self.runtime_path = Path(runtime_path or os.environ.get(RUNTIME_ENV, RUNTIME_DEFAULT))
        # (path, size, mtime_ns) → SHA-256 of files already hashed.
        self._verified: dict[tuple[str, int, int], str] = {}
        if not self.runtime_path.is_file():
            raise AssetError(f"Android ONNX Runtime is missing: {self.runtime_path}")
        got = self._hash(self.runtime_path)
        if got != RUNTIME_SHA256:
            raise AssetError(f"Android ONNX Runtime SHA-256 {got}, expected {RUNTIME_SHA256}")

    def _hash(self, path: Path) -> str:
        st = path.stat()
        key = (str(path), st.st_size, st.st_mtime_ns)
        digest = self._verified.get(key)
        if digest is None:
            digest = self._verified[key] = sha256_file(path)
        return digest

    def speech_assets(self, model: WakeModel) -> SpeechAssets:
        """The `session.ready.assets` object for a selected wake model."""
        return SpeechAssets(RUNTIME_SHA256, model.graph_sha256, model.sidecar_sha256)

    def resolve(self, sha256: str) -> AssetInfo:
        if not isinstance(sha256, str) or not _SHA_RE.fullmatch(sha256):
            raise AssetNotFound("not_found")
        if sha256 == RUNTIME_SHA256:
            path = self.runtime_path
        else:
            path = self.registry.resolve_asset(sha256)
            if path is None:
                candidate = self.alert_lookup(sha256)
                path = None if candidate is None else Path(candidate)
        if path is None or not path.is_file():
            raise AssetNotFound("not_found")
        # Every served file must still hash to the requested name; alert lookup
        # is injected, and any file can change on disk after registration.
        if self._hash(path) != sha256:
            raise AssetCorrupt(f"asset {path} no longer matches {sha256}")
        return AssetInfo(sha256, path, path.stat().st_size)

    def chunks(self, sha256: str, offset: int = 0,
               chunk_bytes: int = MAX_CHUNK_BYTES) -> Iterator[bytes]:
        """Binary asset-socket chunks, each ≤64 KiB, beginning at `offset`."""
        if isinstance(offset, bool) or not isinstance(offset, int) or offset < 0:
            raise AssetError("offset must be a non-negative integer")
        if not 1 <= chunk_bytes <= MAX_CHUNK_BYTES:
            raise AssetError(f"chunk size must be 1..{MAX_CHUNK_BYTES}")
        asset = self.resolve(sha256)
        if offset > asset.size:
            raise AssetError(f"offset {offset} exceeds asset size {asset.size}")
        with open(asset.path, "rb") as source:
            source.seek(offset)
            while True:
                block = source.read(chunk_bytes)
                if not block:
                    return
                yield block

    def completion(self, sha256: str) -> dict[str, object]:
        asset = self.resolve(sha256)
        return {"sha256": asset.sha256, "size": asset.size, "done": True}

    def parse_request(self, request: object) -> tuple[AssetInfo, int]:
        if not isinstance(request, dict) or set(request) != {"sha256", "offset"}:
            raise AssetError("asset request must contain only sha256 and offset")
        offset = request["offset"]
        if isinstance(offset, bool) or not isinstance(offset, int) or offset < 0:
            raise AssetError("offset must be a non-negative integer")
        asset = self.resolve(request["sha256"])
        if offset > asset.size:
            raise AssetError(f"offset {offset} exceeds asset size {asset.size}")
        return asset, offset
