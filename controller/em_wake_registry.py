"""Content-addressed BCResNet model registry (§5.1).

Entries live under `<data>/oww_models/`; the fleet config `wakeModel` selects an
entry by graph SHA-256. Upload validates graph+sidecar, executes the deterministic
silence/noise/tone probe, stores atomically, and never changes selection.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import tempfile
from collections.abc import Mapping
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Callable, Protocol

from em_wake_scorer import BcresnetSpec, Infer, load_spec, onnx_infer, probe_model, validate_probe

DEPLOYED_GRAPH_SHA256 = "4eb745120ea56f5681eddbf788a0c69e1fd406d4694a04a4dba0c1e41d862d3f"
DEPLOYED_SIDECAR_SHA256 = "25da0c652c562bf0a45a8f34bb46e38788bfc689e31401f33950831a0f2af51f"
INDEX_VERSION = 1
INDEX_NAME = "registry.json"
MAX_GRAPH_BYTES = 20 * 1024 * 1024
MAX_SIDECAR_BYTES = 64 * 1024
_SHA_RE = re.compile(r"^[0-9a-f]{64}$")
_WORD_RE = re.compile(r"^[a-z]+$")


class RegistryError(Exception):
    pass


@dataclass(frozen=True, slots=True)
class Thresholds:
    idle: float
    playback: float
    reference: float
    near_miss: float

    def __post_init__(self) -> None:
        values = (self.idle, self.playback, self.reference, self.near_miss)
        if not all(isinstance(x, (int, float)) and not isinstance(x, bool) and 0.0 < float(x) < 1.0
                   for x in values):
            raise RegistryError("all thresholds must be numbers in (0, 1)")
        if not self.near_miss < self.reference <= self.playback <= self.idle:
            raise RegistryError("threshold order must be near_miss < reference <= playback <= idle")


DEPLOYED_THRESHOLDS = Thresholds(idle=0.90, playback=0.65, reference=0.30, near_miss=0.17)


@dataclass(frozen=True, slots=True)
class WakeModel:
    graph_sha256: str
    graph_file: str
    sidecar_sha256: str
    sidecar_file: str
    thresholds: Thresholds
    wake_phrase: str
    verify_core: str
    probe: dict[str, float]

    @classmethod
    def from_dict(cls, raw: object) -> WakeModel:
        if not isinstance(raw, Mapping):
            raise RegistryError("malformed registry entry: not an object")
        entry: Mapping[str, object] = raw

        def text(key: str) -> str:
            value = entry[key]
            if not isinstance(value, str):
                raise TypeError(f"{key} must be a string")
            return value

        try:
            thresholds, probe = entry["thresholds"], entry["probe"]
            if not isinstance(thresholds, Mapping) or not isinstance(probe, Mapping):
                raise TypeError("thresholds and probe must be objects")
            model = cls(
                text("graph_sha256"), text("graph_file"), text("sidecar_sha256"),
                text("sidecar_file"), Thresholds(**thresholds),
                text("wake_phrase"), text("verify_core"),
                {str(k): float(v) for k, v in probe.items()},
            )
        except (KeyError, TypeError, ValueError) as exc:
            raise RegistryError(f"malformed registry entry: {exc}") from None
        _validate_words(model.wake_phrase, model.verify_core)
        if not _SHA_RE.fullmatch(model.graph_sha256) or not _SHA_RE.fullmatch(model.sidecar_sha256):
            raise RegistryError("registry entry has malformed SHA-256")
        validate_probe(model.probe, model.thresholds.near_miss)
        return model

    def to_dict(self) -> dict[str, object]:
        return asdict(self)


@dataclass(frozen=True, slots=True)
class ValidatedUpload:
    graph_sha256: str
    sidecar_sha256: str
    probe: dict[str, float]


def sha256_file(path: str | os.PathLike[str]) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as src:
        for chunk in iter(lambda: src.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def _validate_words(wake_phrase: str, verify_core: str) -> None:
    if not isinstance(wake_phrase, str) or not _WORD_RE.fullmatch(wake_phrase):
        raise RegistryError("wake_phrase must contain lowercase ASCII letters only")
    if not isinstance(verify_core, str) or not _WORD_RE.fullmatch(verify_core):
        raise RegistryError("verify_core must contain lowercase ASCII letters only")


def default_registry_dir(db_path: str | os.PathLike[str] | None = None) -> Path:
    db = Path(db_path or os.environ.get("DB_PATH", "echomuse.db")).resolve()
    return db.parent / "oww_models"


class InferLoader(Protocol):
    """Opens a graph for the upload probe (`em_wake_scorer.onnx_infer`)."""

    def __call__(self, model_path: str | os.PathLike[str], *,
                 spec: BcresnetSpec | None = None) -> tuple[Infer, str]: ...


class WakeRegistry:
    """Durable model index. `active_getter()` returns fleet `wakeModel` or a
    config dict containing it; selection is read, never cached or changed here.
    `infer_loader(graph, spec=spec)` opens a graph for the upload probe
    (default: `em_wake_scorer.onnx_infer`)."""

    def __init__(self, directory: str | os.PathLike[str] | None = None,
                 active_getter: Callable[[], str | Mapping[str, object] | None] = lambda: None,
                 infer_loader: InferLoader = onnx_infer):
        self.directory = Path(directory) if directory is not None else default_registry_dir()
        self.active_getter = active_getter
        self.infer_loader = infer_loader
        self.index_path = self.directory / INDEX_NAME
        self._models: dict[str, WakeModel] = {}
        self.directory.mkdir(parents=True, exist_ok=True)
        self._load()

    def _load(self) -> None:
        if not self.index_path.exists():
            return
        try:
            raw = json.loads(self.index_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise RegistryError(f"cannot read {self.index_path}: {exc}") from None
        models = raw.get("models") if isinstance(raw, dict) else None
        if not isinstance(models, list) or raw.get("version") != INDEX_VERSION:
            raise RegistryError("unsupported or malformed registry index")
        loaded: dict[str, WakeModel] = {}
        for item in models:
            model = WakeModel.from_dict(item)
            if model.graph_sha256 in loaded:
                raise RegistryError(f"duplicate registry SHA {model.graph_sha256}")
            self._verify_files(model)
            loaded[model.graph_sha256] = model
        self._models = loaded

    def _verify_files(self, model: WakeModel) -> None:
        graph = self.directory / model.graph_file
        sidecar = self.directory / model.sidecar_file
        if sha256_file(graph) != model.graph_sha256:
            raise RegistryError(f"graph {graph.name} missing or has wrong SHA-256")
        if sha256_file(sidecar) != model.sidecar_sha256:
            raise RegistryError(f"sidecar {sidecar.name} missing or has wrong SHA-256")
        spec = load_spec(graph, sidecar)
        if len(spec.labels) < 2:
            raise RegistryError(f"sidecar {sidecar.name} has fewer than two labels")

    def _save(self) -> None:
        payload = {"version": INDEX_VERSION,
                   "models": [self._models[k].to_dict() for k in sorted(self._models)]}
        data = (json.dumps(payload, sort_keys=True, separators=(",", ":")) + "\n").encode()
        fd, tmp_name = tempfile.mkstemp(prefix=".registry.", suffix=".tmp", dir=self.directory)
        try:
            with os.fdopen(fd, "wb") as out:
                out.write(data)
                out.flush()
                os.fsync(out.fileno())
            os.replace(tmp_name, self.index_path)
            dir_fd = os.open(self.directory, os.O_RDONLY)
            try:
                os.fsync(dir_fd)
            finally:
                os.close(dir_fd)
        except BaseException:
            try:
                os.unlink(tmp_name)
            except FileNotFoundError:
                pass
            raise

    def list(self) -> tuple[WakeModel, ...]:
        return tuple(self._models[k] for k in sorted(self._models))

    def get(self, graph_sha256: str) -> WakeModel:
        try:
            return self._models[graph_sha256]
        except KeyError:
            raise RegistryError(f"unknown wake model {graph_sha256}") from None

    @property
    def active_sha256(self) -> str | None:
        selected = self.active_getter()
        value = selected.get("wakeModel") if isinstance(selected, Mapping) else selected
        if value is None:
            return None
        if not isinstance(value, str) or not _SHA_RE.fullmatch(value):
            raise RegistryError(f"fleet wakeModel is not a SHA-256: {value!r}")
        return value

    def active(self) -> WakeModel:
        sha = self.active_sha256
        if sha is None:
            raise RegistryError("fleet wakeModel is not configured")
        return self.get(sha)

    def for_config(self, config: Mapping[str, object] | None) -> WakeModel:
        """The model an effective device config selects (its `wakeModel`,
        which inherits the fleet value unless the device overrides it)."""
        sha = (config or {}).get("wakeModel")
        if sha is None:
            return self.active()
        if not isinstance(sha, str):
            raise RegistryError(f"wakeModel is not a SHA-256: {sha!r}")
        return self.get(sha)

    def graph_path(self, graph_sha256: str) -> Path:
        return self.directory / self.get(graph_sha256).graph_file

    def sidecar_path(self, graph_sha256: str) -> Path:
        return self.directory / self.get(graph_sha256).sidecar_file

    def resolve_asset(self, sha256: str) -> Path | None:
        """Resolve any registered graph or sidecar by its content hash."""
        for model in self._models.values():
            if sha256 == model.graph_sha256:
                return self.directory / model.graph_file
            if sha256 == model.sidecar_sha256:
                return self.directory / model.sidecar_file
        return None

    def validate_upload(self, graph: str | os.PathLike[str], sidecar: str | os.PathLike[str],
                        thresholds: Thresholds, wake_phrase: str,
                        verify_core: str) -> ValidatedUpload:
        graph, sidecar = Path(graph), Path(sidecar)
        _validate_words(wake_phrase, verify_core)
        if not graph.is_file() or graph.stat().st_size == 0 or graph.stat().st_size > MAX_GRAPH_BYTES:
            raise RegistryError(f"graph size must be 1..{MAX_GRAPH_BYTES} bytes")
        if not sidecar.is_file() or sidecar.stat().st_size == 0 or sidecar.stat().st_size > MAX_SIDECAR_BYTES:
            raise RegistryError(f"sidecar size must be 1..{MAX_SIDECAR_BYTES} bytes")
        try:
            spec = load_spec(graph, sidecar)
            infer, _ = self.infer_loader(graph, spec=spec)
            scores = probe_model(infer, spec)
            validate_probe(scores, thresholds.near_miss)
        except (OSError, ValueError) as exc:
            raise RegistryError(f"invalid BCResNet pair: {exc}") from None
        return ValidatedUpload(sha256_file(graph), sha256_file(sidecar), scores)

    def register(self, graph: str | os.PathLike[str], sidecar: str | os.PathLike[str],
                 thresholds: Thresholds, wake_phrase: str,
                 verify_core: str) -> WakeModel:
        """Validate and atomically register, without activating it."""
        graph, sidecar = Path(graph), Path(sidecar)
        checked = self.validate_upload(graph, sidecar, thresholds, wake_phrase, verify_core)
        existing = self._models.get(checked.graph_sha256)
        if existing is not None:
            if existing.sidecar_sha256 != checked.sidecar_sha256:
                raise RegistryError("graph SHA already exists with a different sidecar")
            if (existing.thresholds, existing.wake_phrase, existing.verify_core) != (
                    thresholds, wake_phrase, verify_core):
                raise RegistryError("graph is already registered with different thresholds "
                                    "or spoken forms; delete it first to change them")
            return existing
        graph_name = f"{checked.graph_sha256}.onnx"
        sidecar_name = f"{checked.sidecar_sha256}.json"
        model = WakeModel(checked.graph_sha256, graph_name, checked.sidecar_sha256,
                          sidecar_name, thresholds, wake_phrase, verify_core, checked.probe)
        written = []
        try:
            for source, name, expect in ((graph, graph_name, checked.graph_sha256),
                                         (sidecar, sidecar_name, checked.sidecar_sha256)):
                dest = self.directory / name
                if dest.exists():
                    if sha256_file(dest) != expect:
                        raise RegistryError(f"existing asset {name} has wrong SHA-256")
                    continue
                tmp = self.directory / f".{name}.tmp"
                shutil.copyfile(source, tmp)
                if sha256_file(tmp) != expect:
                    tmp.unlink(missing_ok=True)
                    raise RegistryError(f"copy of {source.name} changed SHA-256")
                with open(tmp, "rb") as f:
                    os.fsync(f.fileno())
                os.replace(tmp, dest)
                written.append(dest)
            self._models[model.graph_sha256] = model
            self._save()
            return model
        except BaseException:
            self._models.pop(model.graph_sha256, None)
            for path in written:
                path.unlink(missing_ok=True)
            raise

    def install_deployed(self, graph: str | os.PathLike[str], sidecar: str | os.PathLike[str]) -> WakeModel:
        """Install the pinned deployed entry from verified repository bytes."""
        if sha256_file(graph) != DEPLOYED_GRAPH_SHA256:
            raise RegistryError("deployed graph fixture has the wrong SHA-256")
        if sha256_file(sidecar) != DEPLOYED_SIDECAR_SHA256:
            raise RegistryError("deployed sidecar fixture has the wrong SHA-256")
        model = self.register(graph, sidecar, DEPLOYED_THRESHOLDS, "ophelia", "ophel")
        if model.graph_sha256 != DEPLOYED_GRAPH_SHA256:
            raise RegistryError("registered deployed graph under an unexpected SHA-256")
        return model

    def delete(self, graph_sha256: str) -> WakeModel:
        """Delete only an inactive entry, then garbage-collect unreferenced files."""
        model = self.get(graph_sha256)
        if graph_sha256 == self.active_sha256:
            raise RegistryError(f"wake model {graph_sha256} is active")
        del self._models[graph_sha256]
        self._save()
        referenced_sidecars = {m.sidecar_sha256 for m in self._models.values()}
        (self.directory / model.graph_file).unlink(missing_ok=True)
        if model.sidecar_sha256 not in referenced_sidecars:
            (self.directory / model.sidecar_file).unlink(missing_ok=True)
        return model
