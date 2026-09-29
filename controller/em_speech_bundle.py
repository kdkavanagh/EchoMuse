"""Pinned controller speech bundle (§16.6): manifest, startup verification,
and install-time trailing-blank qualification."""

from __future__ import annotations

import hashlib
import importlib.metadata
import io
import json
import os
import tarfile
import wave
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol

import numpy as np

BUNDLE_ENV = "SPEECH_BUNDLE_DIR"
BUNDLE_DEFAULT = "/app/speech"
MANIFEST_PATH = Path(__file__).with_name("speech_bundle.json")
ATTRIBUTION_TEXT = (
    "Kroko-ASR model attribution\n"
    "\n"
    "This bundle redistributes the Kroko-ASR community model under CC-BY-SA.\n"
    "Model and license information: https://huggingface.co/Banafo/Kroko-ASR\n"
)
BLANK_SLOPE_MIN = 22.0
BLANK_SLOPE_MAX = 27.0


class BundleError(Exception):
    pass


class RecognizerStream(Protocol):
    """What the controller uses of a `sherpa_onnx.OnlineStream`."""

    def accept_waveform(self, sample_rate: int, waveform: np.ndarray) -> None: ...
    def input_finished(self) -> None: ...


class Recognizer(Protocol):
    """What the controller uses of a `sherpa_onnx.OnlineRecognizer`."""

    def create_stream(self) -> RecognizerStream: ...
    def is_ready(self, stream: RecognizerStream) -> bool: ...
    def decode_stream(self, stream: RecognizerStream) -> None: ...
    def get_result_as_json_string(self, stream: RecognizerStream) -> str: ...


@dataclass(frozen=True, slots=True)
class FileEntry:
    """One hash-pinned bundle file, relative to the bundle directory."""

    path: str
    size: int
    sha256: str

    @classmethod
    def parse(cls, raw: object, label: str) -> FileEntry:
        if not isinstance(raw, Mapping):
            raise BundleError(f"manifest {label} entry is malformed")
        path, size, sha256 = raw.get("path"), raw.get("size"), raw.get("sha256")
        if not isinstance(path, str) or isinstance(size, bool) or not isinstance(size, int) \
                or not isinstance(sha256, str):
            raise BundleError(f"manifest {label} entry is malformed")
        return cls(path, size, sha256)


@dataclass(frozen=True, slots=True)
class Manifest:
    """`speech_bundle.json` (version 1): what the bundle holds and each file's pin."""

    package: str                      # the sherpa-onnx distribution name
    package_version: str
    wheel: FileEntry
    archive: FileEntry                # the Kroko model archive
    asr_files: Mapping[str, FileEntry]   # encoder, decoder, joiner, tokens
    test_wav_member: str              # qualification WAV inside `archive`
    vad: FileEntry
    attribution_path: str

    @classmethod
    def parse(cls, raw: object) -> Manifest:
        if not isinstance(raw, Mapping):
            raise BundleError("speech bundle manifest is not an object")
        if raw.get("version") != 1:
            raise BundleError(f"unsupported speech bundle manifest version {raw.get('version')!r}")
        runtime, asr, attribution = raw.get("runtime"), raw.get("asr"), raw.get("attribution")
        if not isinstance(runtime, Mapping) or not isinstance(asr, Mapping) or not isinstance(attribution, Mapping):
            raise BundleError("speech bundle manifest lacks runtime, asr or attribution")
        package, version = runtime.get("package"), runtime.get("version")
        files, member, attribution_path = asr.get("files"), asr.get("test_wav_member"), attribution.get("path")
        if not isinstance(package, str) or not isinstance(version, str) or not isinstance(files, Mapping) \
                or not isinstance(member, str) or not isinstance(attribution_path, str):
            raise BundleError("speech bundle manifest is malformed")
        asr_files = {str(name): FileEntry.parse(entry, f"Kroko {name}") for name, entry in files.items()}
        missing = {"encoder", "decoder", "joiner", "tokens"} - set(asr_files)
        if missing:
            raise BundleError(f"speech bundle manifest lacks Kroko {sorted(missing)}")
        return cls(package, version, FileEntry.parse(runtime.get("wheel"), "sherpa wheel"),
                   FileEntry.parse(asr.get("archive"), "Kroko archive"), asr_files, member,
                   FileEntry.parse(raw.get("vad"), "Silero v5"), attribution_path)


@dataclass(frozen=True, slots=True)
class SpeechBundle:
    directory: Path
    wheel: Path
    archive: Path
    encoder: Path
    decoder: Path
    joiner: Path
    tokens: Path
    vad: Path
    attribution: Path
    package_version: str
    manifest: Manifest


@dataclass(frozen=True, slots=True)
class Qualification:
    text: str
    blank_slope_per_s: float
    final_trailing_blank_frames: int


def load_manifest(path: str | os.PathLike[str] = MANIFEST_PATH) -> Manifest:
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BundleError(f"cannot read speech bundle manifest: {exc}") from None
    return Manifest.parse(raw)


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def _verify_file(directory: Path, entry: FileEntry, label: str) -> Path:
    path, size, expected = directory / entry.path, entry.size, entry.sha256
    try:
        actual_size = path.stat().st_size
    except FileNotFoundError:
        raise BundleError(f"speech bundle file is missing: {path}") from None
    if actual_size != size:
        raise BundleError(f"speech bundle {label} has size {actual_size}, expected {size}: {path}")
    actual_sha = _sha256(path)
    if actual_sha != expected:
        raise BundleError(f"speech bundle {label} SHA-256 {actual_sha}, expected {expected}: {path}")
    return path


def verify_bundle(directory: str | os.PathLike[str] | None = None, *,
                  check_installed_package: bool = True,
                  manifest_path: str | os.PathLike[str] = MANIFEST_PATH) -> SpeechBundle:
    """Verify every §16.6 hash. Startup must complete this before loading models."""
    root = Path(directory or os.environ.get(BUNDLE_ENV, BUNDLE_DEFAULT))
    manifest = load_manifest(manifest_path)
    wheel = _verify_file(root, manifest.wheel, "sherpa wheel")
    archive = _verify_file(root, manifest.archive, "Kroko archive")
    members = {name: _verify_file(root, entry, f"Kroko {name}")
               for name, entry in manifest.asr_files.items()}
    vad = _verify_file(root, manifest.vad, "Silero v5")
    attribution = root / manifest.attribution_path
    try:
        text = attribution.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise BundleError(f"Kroko attribution is missing: {attribution}") from None
    if text != ATTRIBUTION_TEXT:
        raise BundleError(f"Kroko attribution is incomplete or modified: {attribution}")
    package_version = manifest.package_version
    if check_installed_package:
        try:
            installed = importlib.metadata.version(manifest.package)
        except importlib.metadata.PackageNotFoundError:
            raise BundleError("sherpa-onnx is not installed") from None
        if installed != package_version:
            raise BundleError(f"sherpa-onnx {installed} installed, bundle requires {package_version}")
    return SpeechBundle(root, wheel, archive, members["encoder"], members["decoder"],
                        members["joiner"], members["tokens"], vad, attribution,
                        package_version, manifest)


def create_recognizer(bundle: SpeechBundle) -> Recognizer:
    """Immutable shared Kroko recognizer with the exact §16.6 constructor."""
    import sherpa_onnx

    recognizer: Recognizer = sherpa_onnx.OnlineRecognizer.from_transducer(
        tokens=str(bundle.tokens), encoder=str(bundle.encoder), decoder=str(bundle.decoder),
        joiner=str(bundle.joiner), num_threads=1, sample_rate=16_000, feature_dim=80,
        decoding_method="greedy_search", enable_endpoint_detection=False, provider="cpu",
    )
    return recognizer


def read_qualification_wav(bundle: SpeechBundle) -> tuple[int, np.ndarray]:
    """Read the archive's distributed `test_wavs/0.wav` without extracting it."""
    member_name = bundle.manifest.test_wav_member
    try:
        with tarfile.open(bundle.archive, "r:bz2") as archive:
            member = archive.getmember(member_name)
            source = archive.extractfile(member)
            if source is None:
                raise BundleError(f"archive member is not a file: {member_name}")
            data = source.read()
    except (tarfile.TarError, KeyError) as exc:
        raise BundleError(f"Kroko qualification WAV is unavailable: {exc}") from None
    with wave.open(io.BytesIO(data), "rb") as wav:
        if wav.getnchannels() != 1 or wav.getsampwidth() != 2:
            raise BundleError("Kroko qualification WAV must be mono PCM16")
        rate = wav.getframerate()
        samples = np.frombuffer(wav.readframes(wav.getnframes()), dtype="<i2").astype(np.float32) / 32768.0
    return rate, samples


def qualify_bundle(bundle: SpeechBundle, recognizer: Recognizer | None = None) -> Qualification:
    """Install-time qualification: test WAV + 4 s silence must produce a
    22–27/s trailing-blank slope (§16.6). This is not run at every startup."""
    recognizer = recognizer or create_recognizer(bundle)
    rate, samples = read_qualification_wav(bundle)
    stream = recognizer.create_stream()
    for start in range(0, samples.size, max(1, rate * 80 // 1000)):
        stream.accept_waveform(rate, samples[start:start + rate * 80 // 1000])
        while recognizer.is_ready(stream):
            recognizer.decode_stream(stream)
    points = []
    silence_chunk = np.zeros(rate * 80 // 1000, dtype=np.float32)
    for i in range(50):
        stream.accept_waveform(rate, silence_chunk)
        while recognizer.is_ready(stream):
            recognizer.decode_stream(stream)
        result = json.loads(recognizer.get_result_as_json_string(stream))
        blanks = result.get("num_trailing_blanks")
        if isinstance(blanks, bool) or not isinstance(blanks, int) or blanks < 0:
            raise BundleError("Kroko result JSON lacks a valid num_trailing_blanks")
        points.append(((i + 1) * 0.08, blanks))
    stream.input_finished()
    while recognizer.is_ready(stream):
        recognizer.decode_stream(stream)
    # The initial decoder latency can keep the count flat. Fit the last 3 s,
    # after enough zero waveform has crossed the encoder context.
    t = np.array([p[0] for p in points if p[0] >= 1.0], dtype=np.float64)
    b = np.array([p[1] for p in points if p[0] >= 1.0], dtype=np.float64)
    slope = float(np.polyfit(t, b, 1)[0])
    if not BLANK_SLOPE_MIN <= slope <= BLANK_SLOPE_MAX:
        raise BundleError(f"Kroko trailing-blank slope {slope:.3f}/s outside "
                          f"{BLANK_SLOPE_MIN:g}..{BLANK_SLOPE_MAX:g}/s")
    final = json.loads(recognizer.get_result_as_json_string(stream))
    return Qualification(final.get("text", ""), slope, int(final["num_trailing_blanks"]))
