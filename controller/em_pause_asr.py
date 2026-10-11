"""
Pause transcription by a remote Wyoming ASR server (§16.6).

At each pause the speech worker re-decodes the utterance so far. Kroko, the
streaming model, always does; config `pauseAsr` can also send the same audio
to a Wyoming speech-to-text server, for example the one Home Assistant's
pipeline uses, so that completeness is judged on that model's words. Its
transcript has no word timings, so it never replaces Kroko's tokens: it only
replaces the judged text while Kroko's finalized result stands.

The client below is the part of the Wyoming protocol this needs, synchronous
over one TCP connection per request: `describe` → `info`, and `transcribe`,
`audio-start`, `audio-chunk`…, `audio-stop` → `transcript`. An event is a JSON
header line, then `data_length` bytes of JSON data, then `payload_length`
bytes of payload.
"""

from __future__ import annotations

import json
import re
import socket
import time
from collections.abc import Mapping
from dataclasses import dataclass
from enum import StrEnum

import numpy as np

PAUSE_ASR_KEY = "pauseAsr"
DEFAULT_WYOMING_PORT = 10300
WYOMING_VERSION = "1.5.4"
SAMPLE_RATE = 16_000
CHUNK_BYTES = 32_000                 # 1 s of 16-bit mono per audio-chunk
MAX_HEADER_BYTES = 65_536
MAX_EVENT_BYTES = 4 * 1024 * 1024    # data or payload of one event
DESCRIBE_TIMEOUT_S = 2.0

_FIELDS = ("engine", "host", "port", "model", "language")
_HOST = re.compile(r"[A-Za-z0-9._-]{1,253}|[0-9A-Fa-f:]{2,39}")
_LANGUAGE = re.compile(r"[A-Za-z]{2,3}([-_][A-Za-z0-9]{1,8})*")
_MAX_MODEL = 200


class PauseAsrEngine(StrEnum):
    """Who transcribes the utterance at a pause: Kroko alone, or Kroko and a Wyoming server."""

    KROKO = "kroko"
    WYOMING = "wyoming"


@dataclass(frozen=True, slots=True)
class WyomingServer:
    """A Wyoming ASR server. Empty `model`/`language`: the server's own default."""

    host: str
    port: int
    model: str
    language: str

    @property
    def policy_suffix(self) -> str:
        """What a decision made with this server's text adds to the policy hash."""
        return f"+{PauseAsrEngine.WYOMING}:{self.model or 'default'}"


@dataclass(frozen=True, slots=True)
class PauseDecode:
    """The server's transcript of the utterance through sample `through`, at one pause.
    `text` None: no usable answer (`error` says why); Kroko's result stands alone.
    `ms`: from the pause that sent the request to its answer, or to its being dropped."""

    through: int
    text: str | None
    ms: int
    error: str | None


def parse_pause_asr(raw: object) -> WyomingServer | None:
    """The `pauseAsr` config object: None for Kroko alone, else the server.
    ValueError names what is wrong."""
    if not isinstance(raw, Mapping):
        raise ValueError(f"must be an object with {', '.join(_FIELDS)}")
    if set(raw) != set(_FIELDS):
        raise ValueError(f"must have exactly {', '.join(_FIELDS)}")
    engine = raw["engine"]
    if engine not in tuple(PauseAsrEngine):
        raise ValueError(f"engine must be one of {', '.join(PauseAsrEngine)}")
    host, port, model, language = raw["host"], raw["port"], raw["model"], raw["language"]
    if not isinstance(host, str) or (host and not _HOST.fullmatch(host)):
        raise ValueError("host must be a host name or IP address")
    if isinstance(port, bool) or not isinstance(port, int) or not 1 <= port <= 65_535:
        raise ValueError("port must be an integer from 1 through 65535")
    if not isinstance(model, str) or len(model) > _MAX_MODEL or not model.isprintable():
        raise ValueError(f"model must be a model name of at most {_MAX_MODEL} characters, or empty")
    if not isinstance(language, str) or (language and not _LANGUAGE.fullmatch(language)):
        raise ValueError("language must be a language code such as en, or empty")
    if engine == PauseAsrEngine.KROKO:
        return None
    if not host:
        raise ValueError("host is required for a Wyoming server")
    return WyomingServer(host, port, model.strip(), language)


# --- Wyoming client -----------------------------------------------------------------


class WyomingError(Exception):
    """The server reported an error, closed early, or broke the protocol."""


@dataclass(frozen=True, slots=True)
class WyomingModel:
    name: str
    languages: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class WyomingProgram:
    """One ASR program a server describes; its first model is the server's default."""

    name: str
    version: str | None
    models: tuple[WyomingModel, ...]


class _Connection:
    """One request's TCP connection, every read and write bounded by one deadline."""

    def __init__(self, host: str, port: int, timeout: float) -> None:
        self._deadline = time.monotonic() + timeout
        self._sock = socket.create_connection((host, port), timeout=timeout)
        self._sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self._buffer = bytearray()

    def __enter__(self) -> _Connection:
        return self

    def __exit__(self, *_: object) -> None:
        self._sock.close()

    def _arm(self) -> None:
        remaining = self._deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("Wyoming request deadline passed")
        self._sock.settimeout(remaining)

    def send(self, event_type: str, data: Mapping[str, object] | None = None, payload: bytes = b"") -> None:
        header: dict[str, object] = {"type": event_type, "version": WYOMING_VERSION}
        body = json.dumps(data).encode() if data else b""
        if body:
            header["data_length"] = len(body)
        if payload:
            header["payload_length"] = len(payload)
        self._arm()
        self._sock.sendall(json.dumps(header).encode() + b"\n" + body + payload)

    def _fill(self) -> None:
        self._arm()
        chunk = self._sock.recv(65_536)
        if not chunk:
            raise WyomingError("server closed the connection")
        self._buffer += chunk

    def _line(self) -> bytes:
        while (end := self._buffer.find(b"\n")) < 0:
            if len(self._buffer) > MAX_HEADER_BYTES:
                raise WyomingError("event header too long")
            self._fill()
        line = bytes(self._buffer[:end])
        del self._buffer[:end + 1]
        return line

    def _exactly(self, n: int) -> bytes:
        while len(self._buffer) < n:
            self._fill()
        out = bytes(self._buffer[:n])
        del self._buffer[:n]
        return out

    def receive(self) -> tuple[str, dict[str, object]]:
        """The next event's type and data (header data merged with its data block)."""
        try:
            header = json.loads(self._line())
        except json.JSONDecodeError as exc:
            raise WyomingError(f"bad event header: {exc}") from None
        if not isinstance(header, dict) or not isinstance(header.get("type"), str):
            raise WyomingError(f"bad event header: {header!r}")
        data: dict[str, object] = {}
        inline = header.get("data")
        if isinstance(inline, dict):
            data.update(inline)
        sizes = []
        for key in ("data_length", "payload_length"):
            n = header.get(key) or 0
            if isinstance(n, bool) or not isinstance(n, int) or not 0 <= n <= MAX_EVENT_BYTES:
                raise WyomingError(f"bad {key}: {n!r}")
            sizes.append(n)
        if sizes[0]:
            try:
                block = json.loads(self._exactly(sizes[0]))
            except json.JSONDecodeError as exc:
                raise WyomingError(f"bad event data: {exc}") from None
            if not isinstance(block, dict):
                raise WyomingError(f"bad event data: {block!r}")
            data.update(block)
        if sizes[1]:
            self._exactly(sizes[1])      # no event this client reads carries one it needs
        return header["type"], data


def transcribe(server: WyomingServer, pcm: np.ndarray, *, timeout: float) -> str:
    """The server's transcript of 16 kHz mono int16 `pcm`. Raises TimeoutError past
    `timeout` (connect included), OSError on the network, WyomingError otherwise."""
    audio = np.asarray(pcm, dtype="<i2").tobytes()
    fmt = {"rate": SAMPLE_RATE, "width": 2, "channels": 1}
    request: dict[str, object] = {}
    if server.model:
        request["name"] = server.model
    if server.language:
        request["language"] = server.language
    with _Connection(server.host, server.port, timeout) as conn:
        conn.send("transcribe", request)
        conn.send("audio-start", fmt)
        for i in range(0, len(audio), CHUNK_BYTES):
            conn.send("audio-chunk", fmt, audio[i:i + CHUNK_BYTES])
        conn.send("audio-stop")
        while True:
            kind, data = conn.receive()
            if kind == "transcript":
                text = data.get("text")
                return text if isinstance(text, str) else ""
            if kind == "error":
                raise WyomingError(str(data.get("text") or "server error"))


def describe(host: str, port: int, *, timeout: float = DESCRIBE_TIMEOUT_S) -> tuple[WyomingProgram, ...]:
    """The ASR programs and models the server offers."""
    with _Connection(host, port, timeout) as conn:
        conn.send("describe")
        while True:
            kind, data = conn.receive()
            if kind == "info":
                break
            if kind == "error":
                raise WyomingError(str(data.get("text") or "server error"))
    programs = []
    asr = data.get("asr")
    for program in asr if isinstance(asr, list) else ():
        if not isinstance(program, dict) or not isinstance(program.get("name"), str):
            continue
        models = []
        listed = program.get("models")
        for model in listed if isinstance(listed, list) else ():
            if isinstance(model, dict) and isinstance(model.get("name"), str):
                languages = model.get("languages")
                models.append(WyomingModel(model["name"], tuple(
                    lang for lang in languages if isinstance(lang, str)) if isinstance(languages, list) else ()))
        version = program.get("version")
        programs.append(WyomingProgram(program["name"], version if isinstance(version, str) else None, tuple(models)))
    return tuple(programs)
