"""Pause transcription by a Wyoming server (em_pause_asr): the `pauseAsr` config
object, and the client against a server answering as the live one did."""

from __future__ import annotations

import socket
import time

import numpy as np
import pytest

from _wyoming_fake import FIXTURE, FakeWyoming
from em_pause_asr import WyomingError, WyomingServer, describe, parse_pause_asr, transcribe

KROKO = {"engine": "kroko", "host": "", "port": 10300, "model": "", "language": "en"}
WYOMING = {"engine": "wyoming", "host": "wyoming-faster-whisper", "port": 10300, "model": "", "language": "en"}


def test_kroko_is_no_server_and_wyoming_names_one():
    assert parse_pause_asr(KROKO) is None
    assert parse_pause_asr(WYOMING) == WyomingServer("wyoming-faster-whisper", 10300, "", "en")


@pytest.mark.parametrize("change, message", [
    ({"engine": "whisper"}, "engine"),
    ({"host": ""}, "host is required"),
    ({"host": "wyoming faster whisper"}, "host"),
    ({"port": 0}, "port"),
    ({"port": 65_536}, "port"),
    ({"port": True}, "port"),
    ({"port": "10300"}, "port"),
    ({"language": "english!"}, "language"),
    ({"model": "x" * 201}, "model"),
    ({"model": "bad\nname"}, "model"),
])
def test_invalid_settings_name_the_field(change, message):
    with pytest.raises(ValueError, match=message):
        parse_pause_asr({**WYOMING, **change})


def test_settings_must_be_exactly_the_five_fields():
    with pytest.raises(ValueError, match="exactly"):
        parse_pause_asr({**WYOMING, "timeout": 1})
    with pytest.raises(ValueError, match="exactly"):
        parse_pause_asr({k: v for k, v in WYOMING.items() if k != "language"})


def test_transcribe_streams_the_audio_and_returns_the_transcript():
    pcm = (np.arange(40_000) % 2_000 - 1_000).astype(np.int16)     # 2.5 s: three 1 s chunks
    with FakeWyoming() as fake:
        text = transcribe(WyomingServer(fake.host, fake.port, "parakeet", "en"), pcm, timeout=2.0)
    assert text == FIXTURE["transcript"]["text"]
    (events,) = fake.requests
    kinds = [kind for kind, _, _ in events]
    assert kinds == ["transcribe", "audio-start", "audio-chunk", "audio-chunk", "audio-chunk", "audio-stop"]
    assert events[0][1] == {"name": "parakeet", "language": "en"}
    assert events[1][1] == {"rate": 16_000, "width": 2, "channels": 1}
    assert b"".join(payload for _, _, payload in events) == pcm.astype("<i2").tobytes()


def test_server_default_model_and_language_are_not_named():
    with FakeWyoming() as fake:
        transcribe(WyomingServer(fake.host, fake.port, "", ""), np.zeros(1_600, np.int16), timeout=2.0)
    assert fake.requests[0][0] == ("transcribe", {}, b"")


def test_server_error_is_raised():
    with FakeWyoming(error="model failed") as fake, pytest.raises(WyomingError, match="model failed"):
        transcribe(WyomingServer(fake.host, fake.port, "", "en"), np.zeros(1_600, np.int16), timeout=2.0)


def test_a_silent_server_times_out_at_the_deadline():
    with FakeWyoming(hang=True) as fake:
        started = time.monotonic()
        with pytest.raises(TimeoutError):
            transcribe(WyomingServer(fake.host, fake.port, "", "en"), np.zeros(1_600, np.int16), timeout=0.3)
        assert time.monotonic() - started < 0.6


def test_nobody_listening_is_an_os_error():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    with pytest.raises(OSError):
        transcribe(WyomingServer("127.0.0.1", port, "", "en"), np.zeros(1_600, np.int16), timeout=1.0)


def test_describe_lists_the_recorded_server_and_its_model():
    with FakeWyoming() as fake:
        (program,) = describe(fake.host, fake.port)
    assert (program.name, program.version) == ("faster-whisper", "3.8.1")
    assert [m.name for m in program.models] == ["sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming"]
    assert "en" in program.models[0].languages
