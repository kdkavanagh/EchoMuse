"""A Wyoming ASR server on a free localhost port, in a thread, answering with the
events recorded from the live server (fixtures/wyoming/faster_whisper.json)."""

from __future__ import annotations

import json
import socket
import socketserver
import threading
import time
from pathlib import Path

FIXTURE = json.loads((Path(__file__).parent / "fixtures" / "wyoming" / "faster_whisper.json").read_text())


def _send(sock: socket.socket, event_type: str, data: dict) -> None:
    body = json.dumps(data).encode()
    sock.sendall(json.dumps({"type": event_type, "version": "1.10.2", "data_length": len(body)}).encode()
                 + b"\n" + body)


class FakeWyoming:
    """`transcript` answers every transcription (the recorded text by default), after
    `delay` seconds; `error` answers with an error event instead; `hang` answers nothing.
    `requests` holds each connection's events as (type, data, payload)."""

    def __init__(self, transcript: str | None = None, *, error: str | None = None, hang: bool = False,
                 delay: float = 0.0):
        self.transcript = FIXTURE["transcript"]["text"] if transcript is None else transcript
        self.error = error
        self.hang = hang
        self.delay = delay
        self.requests: list[list[tuple[str, dict, bytes]]] = []
        fake = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self) -> None:
                events: list[tuple[str, dict, bytes]] = []
                fake.requests.append(events)
                while line := self.rfile.readline():
                    header = json.loads(line)
                    data = dict(header.get("data") or {})
                    if header.get("data_length"):
                        data.update(json.loads(self.rfile.read(header["data_length"])))
                    payload = self.rfile.read(header["payload_length"]) if header.get("payload_length") else b""
                    events.append((header["type"], data, payload))
                    if header["type"] == "describe":
                        _send(self.connection, "info", FIXTURE["info"])
                    elif header["type"] == "audio-stop" and not fake.hang:
                        time.sleep(fake.delay)
                        if fake.error is not None:
                            _send(self.connection, "error", {"text": fake.error})
                        else:
                            _send(self.connection, "transcript", {"text": fake.transcript})

        self._server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self._server.daemon_threads = True
        self.host, self.port = self._server.server_address[0], self._server.server_address[1]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self) -> FakeWyoming:
        self._thread.start()
        return self

    def __exit__(self, *_: object) -> None:
        self._server.shutdown()
        self._server.server_close()
