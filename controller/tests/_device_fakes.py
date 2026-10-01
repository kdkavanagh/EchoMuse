"""Fakes for em_device / em_legacy tests: link, actor, render, host, store."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from types import SimpleNamespace

import em_db
import em_device
import em_session
from em_device_link import Envelope, MessageType, SessionHello

ALL_V1 = sorted(em_device.REQUIRED_CAPABILITIES)


def hello_body(capabilities=None, **extra) -> dict:
    """A `session.hello` body as the device sends it."""
    body = {
        "capabilities": list(ALL_V1 if capabilities is None else capabilities),
        "firmware_version": "v3.0.0",
        "boot_id": "boot-1",
        "protocols": [1],
        "privacy": {"muted": False, "capture_epoch": "7"},
        "alerts": {"delivery_epoch": None, "acked_sequence": 0, "wakeup": "ok"},
        "assets": [],
        "volume": {"level": 120, "seeded": False},
    }
    body.update(extra)
    return body


def hello(capabilities=None, **extra) -> SessionHello:
    return SessionHello.parse(hello_body(capabilities, **extra))


def envelope(msg_type: str, body: dict) -> Envelope:
    return Envelope(MessageType(msg_type), "s-1", "m", "DEV1", 0, body)


class FakeActor:
    def __init__(self) -> None:
        self.state = em_session.ActorState.IDLE
        self.awaiting_intent = False
        self.turn_active = False
        self.messages: list[tuple[str, dict]] = []
        self.audio: list[bytes] = []
        self.turns: list[dict] = []
        self.cancels: list[str] = []
        self.attached = None
        self.detached: list[str] = []
        self.listeners = []

    async def start(self) -> None: ...
    async def close(self) -> None: ...

    def attach(self, link, render) -> None:
        self.attached = (link, render)

    def detach(self, reason: str) -> None:
        self.detached.append(reason)

    def on_message(self, envelope: Envelope) -> None:
        self.messages.append((envelope.type, envelope))

    def on_audio(self, frame: bytes) -> None:
        self.audio.append(frame)

    def button_turn(self, action: dict) -> None:
        self.turns.append(action)

    def cancel_turn(self, reason: str = "interrupted") -> None:
        self.cancels.append(reason)

    def add_listener(self, cb):
        self.listeners.append(cb)
        return lambda: self.listeners.remove(cb)

    async def open_diagnostic(self, on_mic) -> None: ...
    async def close_diagnostic(self) -> None: ...


class FakeRender:
    def __init__(self, consumes=("render.progress", "render.finished")) -> None:
        self.consumes = set(consumes)
        self.seen: list[str] = []
        self.failed: list[str] = []

    def on_message(self, envelope: Envelope) -> bool:
        self.seen.append(envelope.type)
        return envelope.type in self.consumes

    def fail_all(self, reason: str) -> None:
        self.failed.append(reason)


class FakeLink:
    def __init__(self, session_hello: SessionHello, device_id: str = "DEV1") -> None:
        self.device_id = device_id
        self.session_id = "s-1"
        self.hello = session_hello
        self.capabilities = session_hello.capabilities
        self.peer_ip = "10.0.0.5"
        self.secure = True
        self.closed = False
        self.degraded = False
        self.sent: list[tuple[str, dict]] = []
        self.clock: tuple[int, float] | None = None   # (device mono_ns, wall s) of a heartbeat

    def device_wall_s(self, mono_ns: int) -> float | None:
        if self.clock is None:
            return None
        return self.clock[1] + (mono_ns - self.clock[0]) / 1e9

    async def send(self, msg_type: str, body: dict, *, generation: int = 0) -> str:
        if self.closed:
            raise em_device.em_device_link.LinkClosed("closed")
        self.sent.append((msg_type, body))
        return f"m{len(self.sent)}"

    async def close(self, reason: str) -> None:
        self.closed = True


class FakeAlerts:
    def __init__(self) -> None:
        self.calls: list[tuple] = []

    async def on_session_hello(self, endpoint_id, alerts, *, capabilities=()):
        self.calls.append(("hello", endpoint_id, alerts))

    def on_session_lost(self, endpoint_id):
        self.calls.append(("lost", endpoint_id))

    async def on_alert_ack(self, endpoint_id, body):
        self.calls.append(("ack", body))

    def on_alert_state(self, endpoint_id, body):
        self.calls.append(("state", body))

    def on_alert_ring_ended(self, endpoint_id, body):
        self.calls.append(("ring_ended", body))

    def on_command_ack(self, endpoint_id, body):
        self.calls.append(("command_ack", body))

    async def on_local_operation(self, endpoint_id, body):
        self.calls.append(("local_operation", body))

    def timers(self, endpoint_id):
        return []


@dataclass
class FakeHost:
    alerts: FakeAlerts = field(default_factory=FakeAlerts)
    actors: dict = field(default_factory=dict)
    render: FakeRender = field(default_factory=FakeRender)
    events: list = field(default_factory=list)
    pushed: list = field(default_factory=list)

    def make_actor(self, device_id):
        actor = self.actors[device_id] = FakeActor()
        return actor

    def make_render(self, device, link):
        return self.render

    async def connected(self, device):
        self.events.append(("connected", device.device_id))

    async def disconnected(self, device):
        self.events.append(("disconnected", device.device_id))

    async def pending(self, device_id, ip):
        self.events.append(("pending", device_id))

    async def push_state(self, device, state):
        self.pushed.append(state)

    async def push_log(self, device_id, level, message): ...

    def button_event(self, device_id, event_type):
        self.events.append(("button_event", event_type))

    def ambient_lux(self, device_id, lux): ...
    def voice_phase(self, device_id, phase): ...
    def volume(self, device_id, value): ...
    def ble_adverts(self, device_id, adverts): ...
    def ble_stats(self, device_id, stats): ...

    def player_busy(self, device_id):
        return False

    def player_gone(self, device_id):
        self.events.append(("player_gone", device_id))

    async def dialog_released(self, device_id):
        self.events.append(("dialog_released", device_id))

    def wifi_result(self, device_id, ok, ssid, error):
        return {"ok": ok}


def device_row(device_id: str, *, approved: bool, label: str | None) -> em_db.DeviceRow:
    """A `devices` row as a freshly registered device has it."""
    return em_db.DeviceRow(
        device_id=device_id, label=label, approved=int(approved), ip=None,
        firmware_ver=None, firmware_previous=None, first_seen=None, last_seen=None,
        config="{}", esphome_api_port=None, esphome_noise_psk=None, use_global_config=1,
        ble_proxy_port=None, token=None, config_sections="[]", collect_mode=0, ambient_mode=0)


class FakeStore:
    """In-memory device table with the em_db registration semantics."""

    def __init__(self, *, approval: str = "strict", token: str | None = None) -> None:
        self.rows: dict[str, em_db.DeviceRow] = {}
        self.approval = approval
        self.stored_token = token
        self.configs: dict[str, dict] = {}
        self.wake: list[dict] = []
        self.shadow: list[tuple] = []           # (reports, {mono_ns: wall s} of their events)
        self.registered: list[str] = []

    def add(self, device_id: str, *, approved: bool, label: str = "Office") -> None:
        self.rows[device_id] = device_row(device_id, approved=approved, label=label)

    def device(self, device_id):
        return self.rows.get(device_id)

    def token(self, device_id):
        return self.stored_token

    def approval_mode(self, default):
        return em_device.ApprovalMode(self.approval)

    def register(self, device_id, ip, version):
        self.registered.append(device_id)
        self.rows.setdefault(device_id, device_row(device_id, approved=False, label=None))

    def approve(self, device_id, label):
        self.rows[device_id] = device_row(device_id, approved=True, label=label)

    def seen(self, device_id, ip, version): ...

    def config(self, device_id):
        return dict(self.configs.get(device_id, {"duckDb": -12.0}))

    def set_config(self, device_id, config):
        self.configs[device_id] = config

    def log(self, *args): ...
    def stats(self, device_id, stats): ...

    def wake_stats(self, device_id, body):
        self.wake.append(body)

    def wake_shadow(self, device_id, reports, wall):
        self.shadow.append((reports, {e.mono_ns: wall(e.mono_ns) for r in reports for e in r.events}))


class FakeRegistry:
    def for_config(self, config):
        return SimpleNamespace(graph_sha256="g", sidecar_sha256="s",
                               thresholds=SimpleNamespace(idle=0.9, playback=0.65,
                                                          reference=0.3, near_miss=0.17))


class FakeAssets:
    def speech_assets(self, model):
        return em_device.em_device_assets.SpeechAssets("r", "g", "s")


def make_hub(store: FakeStore | None = None, host: FakeHost | None = None,
             devices: dict | None = None, *, require_tls: bool = False):
    host = host or FakeHost()
    store = store or FakeStore()
    hub = em_device.LinkHub({} if devices is None else devices, host, FakeRegistry(), FakeAssets(),
                            store=store, require_tls=require_tls)
    return hub, host, store


def run(coro):
    return asyncio.run(coro)


async def settle() -> None:
    """Let spawned tasks and `asyncio.to_thread` hops finish."""
    for _ in range(5):
        await asyncio.sleep(0.01)
