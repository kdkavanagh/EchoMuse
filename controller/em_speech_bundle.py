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
from dataclasses import dataclass
from pathlib import Path

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
    manifest: dict


@dataclass(frozen=True, slots=True)
class Qualification:
    text: str
    blank_slope_per_s: float
    final_trailing_blank_frames: int


def load_manifest(path: str | os.PathLike = MANIFEST_PATH) -> dict:
    try:
        raw = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        raise BundleError(f"cannot read speech bundle manifest: {exc}") from None
    if raw.get("version") != 1:
        raise BundleError(f"unsupported speech bundle manifest version {raw.get('version')!r}")
    return raw


def _sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def _verify_file(directory: Path, entry: dict, label: str) -> Path:
    try:
        rel, size, expected = entry["path"], entry["size"], entry["sha256"]
    except (KeyError, TypeError):
        raise BundleError(f"manifest {label} entry is malformed") from None
    path = directory / rel
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


def verify_bundle(directory: str | os.PathLike | None = None, *,
                  check_installed_package: bool = True,
                  manifest_path: str | os.PathLike = MANIFEST_PATH) -> SpeechBundle:
    """Verify every §16.6 hash. Startup must complete this before loading models."""
    root = Path(directory or os.environ.get(BUNDLE_ENV, BUNDLE_DEFAULT))
    manifest = load_manifest(manifest_path)
    wheel = _verify_file(root, manifest["runtime"]["wheel"], "sherpa wheel")
    archive = _verify_file(root, manifest["asr"]["archive"], "Kroko archive")
    members = {name: _verify_file(root, entry, f"Kroko {name}")
               for name, entry in manifest["asr"]["files"].items()}
    vad = _verify_file(root, manifest["vad"], "Silero v5")
    attribution = root / manifest["attribution"]["path"]
    try:
        text = attribution.read_text(encoding="utf-8")
    except FileNotFoundError:
        raise BundleError(f"Kroko attribution is missing: {attribution}") from None
    if text != ATTRIBUTION_TEXT:
        raise BundleError(f"Kroko attribution is incomplete or modified: {attribution}")
    package_version = manifest["runtime"]["version"]
    if check_installed_package:
        try:
            installed = importlib.metadata.version(manifest["runtime"]["package"])
        except importlib.metadata.PackageNotFoundError:
            raise BundleError("sherpa-onnx is not installed") from None
        if installed != package_version:
            raise BundleError(f"sherpa-onnx {installed} installed, bundle requires {package_version}")
    return SpeechBundle(root, wheel, archive, members["encoder"], members["decoder"],
                        members["joiner"], members["tokens"], vad, attribution,
                        package_version, manifest)


def create_recognizer(bundle: SpeechBundle):
    """Immutable shared Kroko recognizer with the exact §16.6 constructor."""
    import sherpa_onnx

    return sherpa_onnx.OnlineRecognizer.from_transducer(
        tokens=str(bundle.tokens), encoder=str(bundle.encoder), decoder=str(bundle.decoder),
        joiner=str(bundle.joiner), num_threads=1, sample_rate=16_000, feature_dim=80,
        decoding_method="greedy_search", enable_endpoint_detection=False, provider="cpu",
    )


def read_qualification_wav(bundle: SpeechBundle) -> tuple[int, np.ndarray]:
    """Read the archive's distributed `test_wavs/0.wav` without extracting it."""
    member_name = bundle.manifest["asr"]["test_wav_member"]
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


def qualify_bundle(bundle: SpeechBundle, recognizer=None) -> Qualification:
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
