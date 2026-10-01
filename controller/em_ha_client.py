"""Stock Home Assistant websocket/REST client (SPEC §16.7, §16.4, §10.4).

One authenticated websocket per controller, targeting stock HA 2026.8.1 only.
Commands are correlated by message id; subscriptions and pipeline runs are
routed by the id that opened them. HA forgets every subscription when the
socket drops, so owners re-establish theirs from an on-connect listener; a
subscription handle from an old connection is dead and unsubscribing it is a
no-op.

aiohttp is imported lazily so the exception and protocol types load in
environments without it (the alert engine's tests).
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
from collections.abc import AsyncIterator, Awaitable, Callable, Iterable, Mapping, Sequence
from dataclasses import dataclass
from datetime import datetime
from enum import StrEnum
from typing import TYPE_CHECKING, Any, Protocol, TypedDict, TypeGuard

from em_alert_scripts import RelayEvent, ScriptId

if TYPE_CHECKING:
    import aiohttp

log = logging.getLogger("em_ha_client")

# HA's websocket/REST JSON as received: the dynamic boundary. Everything read
# from one is narrowed where it is used (dataclasses, TypedDicts below).
HaMessage = dict[str, Any]

# ── Constants ────────────────────────────────────────────────────────────────

SUPERVISOR_WS_URL = "ws://supervisor/core/websocket"  # §16.7 add-on mode
SUPERVISOR_HTTP_URL = "http://supervisor/core"         # REST base is <this>/api

COMMAND_TIMEOUT_S = 10.0       # reply deadline for ordinary commands and REST calls
RECONNECT_INITIAL_S = 1.0      # first reconnect delay; doubles per failure
RECONNECT_MAX_S = 30.0         # reconnect delay cap
HEARTBEAT_S = 30.0             # websocket ping interval; detects half-open sockets
WS_MAX_MSG_BYTES = 64 * 1024 * 1024  # registry listings on large installs exceed aiohttp's 4 MiB default

STT_SAMPLE_RATE = 16000        # §16.7 STT input, PCM16 mono
STT_TIMEOUT_S = 15             # §16.7 STT-only run "timeout"
INTENT_TIMEOUT_S = 30          # §16.7 intent→TTS run "timeout"
TTS_TIMEOUT_S = 30             # TTS-only run "timeout"; the URL arrives before synthesis
BINARY_MESSAGE_MAX_BYTES = 64 * 1024  # §16.7: each binary message, handler byte included
# Payload per binary message: the handler byte leaves 65,535 bytes; round down
# to whole PCM16 samples so no sample is split across messages.
STT_CHUNK_BYTES = (BINARY_MESSAGE_MAX_BYTES - 1) & ~1
RUN_REPLY_MARGIN_S = 5.0       # beyond HA's own run timeout before HA counts as silent
STT_RECONNECT_WAIT_S = 5.0     # how long the single STT retry waits for a reconnect

ENTITY_LOOKUP_ATTEMPTS = 5     # calendar entity registration after a new config entry
ENTITY_LOOKUP_DELAY_S = 0.5

VOCAB_DEBOUNCE_S = 1.0
CONVERSATION_ASSISTANT = "conversation"  # Assist's key in exposed-entity settings

PREFERRED_PIPELINE = "preferred"   # assist_pipeline OPTION_PREFERRED
VAD_RELAXED = "relaxed"            # assist_pipeline VadSensitivity.RELAXED (1.25 s), §16.7
PIPELINE_SELECT_SUFFIX = "-pipeline"          # assist_pipeline select unique_id "<mac>-pipeline"
VAD_SELECT_SUFFIX = "-vad_sensitivity"        # "<mac>-vad_sensitivity"

LOCAL_CALENDAR_DOMAIN = "local_calendar"
# local_calendar surfaces ical's EventStoreError for an unknown uid/recurrence
# as code "failed" with this text (ical/store.py EventStore.delete).
_DELETE_NOT_FOUND_TEXT = "No existing item with uid"

_TIMER_COMPONENTS = ("intent", "esphome", "assist_satellite")


class HaFeature(StrEnum):
    """What the startup probe checks (§16.7); a failure disables only that feature."""
    VOICE = "voice"
    CALENDAR = "calendar"
    SCRIPTS = "scripts"
    VOCABULARY = "vocabulary"
    TIMERS = "timers"


class VocabEvent(StrEnum):
    """Registry events that rebuild the vocabulary snapshot (§16.7)."""
    ENTITY_REGISTRY_UPDATED = "entity_registry_updated"
    AREA_REGISTRY_UPDATED = "area_registry_updated"
    FLOOR_REGISTRY_UPDATED = "floor_registry_updated"


class PipelineEventType(StrEnum):
    """assist_pipeline run event `type`s this client reads."""
    RUN_START = "run-start"
    STT_END = "stt-end"
    INTENT_PROGRESS = "intent-progress"
    INTENT_END = "intent-end"
    TTS_END = "tts-end"
    ERROR = "error"
    RUN_END = "run-end"


# ── HA reply shapes ──────────────────────────────────────────────────────────

class CalendarEvent(TypedDict):
    """One calendar item, normalized (§16.4): absent fields None."""
    summary: str | None
    start: str | None
    end: str | None
    description: str | None
    uid: str | None
    recurrence_id: str | None
    rrule: str | None
    all_day: bool


CALENDAR_EVENT_KEYS = tuple(CalendarEvent.__annotations__)  # §16.4


class HaTimer(TypedDict, total=False):
    """One timer of HA's `HassTimerStatus` `speech_slots.timers`."""
    id: str
    name: str | None
    device_id: str | None
    start_hours: int
    start_minutes: int
    start_seconds: int
    is_active: bool


class SpeechSlots(TypedDict, total=False):
    timers: list[HaTimer]


class IntentResponse(TypedDict, total=False):
    """HA's `POST /api/intent/handle` reply (an IntentResponse as_dict)."""
    response_type: str
    speech: dict[str, Any]
    data: dict[str, Any]
    speech_slots: SpeechSlots


# ── Errors and the alert engine's interface ──────────────────────────────────

class HaUnavailable(Exception):
    """Not connected, transport failure, or HA did not reply in time."""


class HaError(Exception):
    """HA answered with an error."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


class Subscription(Protocol):
    async def unsubscribe(self) -> None: ...


class HaApi(Protocol):
    connected: bool

    async def time_zone(self) -> str: ...
    async def calendar_create(self, entity_id: str, event: Mapping[str, object]) -> None: ...
    async def calendar_update(self, entity_id: str, uid: str, event: Mapping[str, object],
                              recurrence_id: str | None = None) -> None: ...
    async def calendar_delete(self, entity_id: str, uid: str,
                              recurrence_id: str | None = None) -> bool: ...
    async def calendar_subscribe(self, entity_id: str, start: datetime, end: datetime,
                                 handler: Callable[[list[CalendarEvent]], None]) -> Subscription: ...
    async def calendar_events(self, entity_id: str, start: datetime, end: datetime) -> list[CalendarEvent]: ...
    async def subscribe_events(self, event_type: str,
                               handler: Callable[[HaMessage], None]) -> Subscription: ...
    async def fire_event(self, event_type: str, data: Mapping[str, object]) -> None: ...
    async def ensure_local_calendar(self, title: str) -> tuple[str, str]: ...
    async def get_script_config(self, object_id: str) -> HaMessage | None: ...
    async def put_script_config(self, object_id: str, config: Mapping[str, object]) -> None: ...
    async def call_service(self, domain: str, service: str,
                           data: Mapping[str, object] | None = None) -> object: ...
    async def expose_entities(self, entity_ids: Sequence[str], assistants: Sequence[str]) -> None: ...


# ── Endpoint ─────────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class HaEndpoint:
    """Where HA lives. `http_url` has no trailing slash and no /api suffix."""

    ws_url: str
    http_url: str
    token: str

    @classmethod
    def from_url(cls, url: str, token: str) -> HaEndpoint:
        base = url.rstrip("/")
        if base.startswith("https://"):
            ws = "wss://" + base[len("https://"):]
        elif base.startswith("http://"):
            ws = "ws://" + base[len("http://"):]
        else:
            raise ValueError(f"HA_URL must start with http:// or https://: {url!r}")
        return cls(ws_url=ws + "/api/websocket", http_url=base, token=token)

    @classmethod
    def supervisor(cls, token: str) -> HaEndpoint:
        return cls(ws_url=SUPERVISOR_WS_URL, http_url=SUPERVISOR_HTTP_URL, token=token)

    @classmethod
    def from_env(cls, env: Mapping[str, str] | None = None) -> HaEndpoint | None:
        """HA_URL + HA_TOKEN (Docker, bare metal), else SUPERVISOR_TOKEN (add-on)."""
        source: Mapping[str, str] = os.environ if env is None else env
        url, token = source.get("HA_URL", "").strip(), source.get("HA_TOKEN", "").strip()
        if url and token:
            return cls.from_url(url, token)
        supervisor_token = source.get("SUPERVISOR_TOKEN", "").strip()
        if supervisor_token:
            return cls.supervisor(supervisor_token)
        return None

    def absolute(self, url: str) -> str:
        """Resolve an HA-relative URL such as /api/tts_proxy/<token>."""
        if url.startswith(("http://", "https://")):
            return url
        return self.http_url + url


# ── Pipeline results ─────────────────────────────────────────────────────────

@dataclass(frozen=True)
class PipelineInfo:
    id: str
    name: str


@dataclass(frozen=True)
class Pipelines:
    items: tuple[PipelineInfo, ...]
    preferred_id: str | None


@dataclass(frozen=True)
class RunRejected:
    """HA refused to start the run (bad pipeline, validation); nothing executed. Terminal."""
    code: str
    message: str


@dataclass(frozen=True)
class SttEnded:
    """HA's transcript of a reply streamed over the ESPHome satellite path
    (its `stt-end`). The websocket intent→TTS run starts from text and never
    emits this."""
    text: str


@dataclass(frozen=True)
class IntentEnded:
    """`intent-end`. `processed_locally`: True when HA's built-in agent
    answered (its sentence matcher), False when the pipeline's conversation
    agent did; None when the event did not say (the ESPHome path)."""
    speech: str
    conversation_id: str | None
    continue_conversation: bool
    response_type: str | None
    processed_locally: bool | None


@dataclass(frozen=True)
class TtsReady:
    """Response audio URL (absolute), sent once HA has decided what the reply
    is. `streamed`: the run-start URL, released by `intent-progress`
    `tts_start_streaming` and playable while the agent is still generating;
    otherwise the `tts-end` URL. An earlier fetch parks forever when HA
    overrides the result (acknowledge sound, §16.7)."""
    url: str
    streamed: bool


@dataclass(frozen=True)
class RunFailed:
    """Pipeline `error` event. Terminal."""
    code: str
    message: str


@dataclass(frozen=True)
class RunEnded:
    """`run-end`. Terminal."""


@dataclass(frozen=True)
class RunLost:
    """The connection dropped before the run ended; its outcome is unknown. Terminal."""


RunEvent = RunRejected | SttEnded | IntentEnded | TtsReady | RunFailed | RunEnded | RunLost
_TERMINAL = (RunRejected, RunFailed, RunEnded, RunLost)


# ── Vocabulary (§16.7) ───────────────────────────────────────────────────────

@dataclass(frozen=True)
class VocabEntity:
    entity_id: str
    domain: str
    names: tuple[str, ...]   # full entity name first, then aliases
    area_id: str | None      # the entity's area, else its device's


@dataclass(frozen=True)
class VocabArea:
    area_id: str
    names: tuple[str, ...]   # name, then aliases
    floor_id: str | None


@dataclass(frozen=True)
class VocabFloor:
    floor_id: str
    names: tuple[str, ...]


@dataclass(frozen=True)
class Vocabulary:
    entities: tuple[VocabEntity, ...]
    areas: tuple[VocabArea, ...]
    floors: tuple[VocabFloor, ...]

    def targets(self) -> frozenset[str]:
        """Every entity, area, and floor name, casefolded with single spaces."""
        out: set[str] = set()
        for group in (self.entities, self.areas, self.floors):
            for item in group:
                out.update(_norm_phrase(n) for n in item.names)
        out.discard("")
        return frozenset(out)


def _norm_phrase(text: str) -> str:
    return " ".join(text.casefold().split())


def _unique(names: Iterable[str | None]) -> tuple[str, ...]:
    seen: dict[str, None] = {}
    for n in names:
        if n and (s := n.strip()) and s not in seen:
            seen[s] = None
    return tuple(seen)


def build_vocabulary(exposed: Mapping[str, HaMessage], entries: Mapping[str, HaMessage | None],
                     devices: Sequence[HaMessage], areas: Sequence[HaMessage],
                     floors: Sequence[HaMessage]) -> Vocabulary:
    """Assemble the snapshot from raw HA replies.

    `exposed` is `homeassistant/expose_entity/list`'s `exposed_entities`; only
    entities exposed to Assist count. Names follow HA's
    `async_get_full_entity_name`: a user-set registry name wins, otherwise the
    device name joined with the (unprefixed) original name. A `None` alias is
    HA's COMPUTED_NAME marker, i.e. the full name already listed first.
    Exposed entities absent from the registry carry no registry name and are
    skipped.
    """
    devices_by_id = {d["id"]: d for d in devices}
    entities: list[VocabEntity] = []
    for entity_id in sorted(exposed):
        if not exposed[entity_id].get(CONVERSATION_ASSISTANT):
            continue
        entry = entries.get(entity_id)
        if entry is None:
            continue
        device = devices_by_id.get(entry.get("device_id"))
        if entry.get("name"):
            full = entry["name"]
        else:
            device_name = (device.get("name_by_user") or device.get("name")) if device else None
            full = " ".join(p for p in (device_name, entry.get("original_name")) if p)
        names = _unique([full, *(a for a in entry.get("aliases") or [] if a is not None)])
        if not names:
            continue
        area_id = entry.get("area_id") or (device.get("area_id") if device else None)
        entities.append(VocabEntity(entity_id=entity_id, domain=entity_id.split(".", 1)[0],
                                    names=names, area_id=area_id))
    return Vocabulary(
        entities=tuple(entities),
        areas=tuple(VocabArea(area_id=a["area_id"], names=_unique([a["name"], *a.get("aliases", [])]),
                              floor_id=a.get("floor_id")) for a in areas),
        floors=tuple(VocabFloor(floor_id=f["floor_id"], names=_unique([f["name"], *f.get("aliases", [])]))
                     for f in floors),
    )


# ── Calendar normalization (§16.4) ───────────────────────────────────────────

def _opt_str(value: object) -> str | None:
    return value if isinstance(value, str) else None


def normalize_calendar_event(item: Mapping[str, object]) -> CalendarEvent:
    """One event with exactly CALENDAR_EVENT_KEYS.

    Subscription pushes use `CalendarEvent.as_dict`, which drops None fields;
    the REST view wraps times as {"dateTime": …} or {"date": …}. Both become
    ISO strings, absent fields None, and `all_day` true for date-only events.
    """
    all_day = item.get("all_day")
    times: dict[str, str | None] = {}
    for key in ("start", "end"):
        value = item.get(key)
        if isinstance(value, dict):
            if "date" in value:
                times[key] = _opt_str(value["date"])
                all_day = True if all_day is None else all_day
            else:
                times[key] = _opt_str(value.get("dateTime"))
        else:
            times[key] = _opt_str(value)
    start = times["start"]
    if all_day is None:
        all_day = start is not None and "T" not in start
    return CalendarEvent(
        summary=_opt_str(item.get("summary")), start=start, end=times["end"],
        description=_opt_str(item.get("description")), uid=_opt_str(item.get("uid")),
        recurrence_id=_opt_str(item.get("recurrence_id")), rrule=_opt_str(item.get("rrule")),
        all_day=bool(all_day),
    )


# ── Probe ────────────────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class FeatureStatus:
    ok: bool
    detail: str | None = None   # failure reason; None when ok


@dataclass(frozen=True, slots=True)
class SatelliteEntities:
    pipeline_select: str | None
    vad_sensitivity_select: str | None


# ── Internal routing ─────────────────────────────────────────────────────────

class _Channel:
    """Receives every message carrying the id that opened it."""

    done_on_result = False

    def on_result(self, msg: HaMessage) -> None: ...
    def on_event(self, event: object) -> None: ...
    def on_lost(self) -> None: ...


def _error_of(msg: HaMessage) -> HaError:
    err = msg.get("error") or {}
    return HaError(str(err.get("code", "unknown_error")), str(err.get("message", "")))


class _CommandChannel(_Channel):
    done_on_result = True

    def __init__(self, future: asyncio.Future[Any]) -> None:
        self.future = future

    def on_result(self, msg: HaMessage) -> None:
        if self.future.done():
            return
        if msg.get("success"):
            self.future.set_result(msg.get("result"))
        else:
            self.future.set_exception(_error_of(msg))

    def on_lost(self) -> None:
        if not self.future.done():
            self.future.set_exception(HaUnavailable("connection lost"))


class _EventChannel(_Channel):
    def __init__(self, future: asyncio.Future[None], handler: Callable[[Any], None]) -> None:
        self.future = future
        self.handler = handler

    def on_result(self, msg: HaMessage) -> None:
        if not self.future.done():
            if msg.get("success"):
                self.future.set_result(None)
            else:
                self.future.set_exception(_error_of(msg))

    def on_event(self, event: object) -> None:
        try:
            self.handler(event)
        except Exception:
            log.exception("HA subscription handler failed")

    def on_lost(self) -> None:
        if not self.future.done():
            self.future.set_exception(HaUnavailable("connection lost"))


class _RunItem(StrEnum):
    """What a run channel queued: HA's `result`, one run `event`, the connection
    `lost`, or the handle `abandoned`."""
    RESULT = "result"
    EVENT = "event"
    LOST = "lost"
    ABANDONED = "abandoned"


class _RunChannel(_Channel):
    """Queues a pipeline run's result, events, and loss in arrival order."""

    def __init__(self) -> None:
        self.queue: asyncio.Queue[tuple[_RunItem, Any]] = asyncio.Queue()
        self.msg_id = 0
        self.generation = 0

    def on_result(self, msg: HaMessage) -> None:
        self.queue.put_nowait((_RunItem.RESULT, msg))

    def on_event(self, event: object) -> None:
        self.queue.put_nowait((_RunItem.EVENT, event))

    def on_lost(self) -> None:
        self.queue.put_nowait((_RunItem.LOST, None))


class _WsSubscription:
    def __init__(self, client: HaClient, msg_id: int, generation: int) -> None:
        self._client = client
        self._msg_id = msg_id
        self._generation = generation
        self._closed = False

    async def unsubscribe(self) -> None:
        if self._closed:
            return
        self._closed = True
        self._client._channels.pop(self._msg_id, None)
        if not self._client.connected or self._client._generation != self._generation:
            return  # HA already dropped it with the old connection
        try:
            await self._client.command({"type": "unsubscribe_events", "subscription": self._msg_id})
        except (HaError, HaUnavailable) as err:
            log.debug("unsubscribe %d: %s", self._msg_id, err)


# ── Pipeline run handle ──────────────────────────────────────────────────────

class PipelineRun:
    """A dispatched intent→TTS run. Iterate for typed events until a terminal
    one. Never resubmitted (§16.7). `abandon()` fences it: later events are
    dropped and iteration stops."""

    def __init__(self, client: HaClient, channel: _RunChannel) -> None:
        self._client = client
        self._channel = channel
        self._finished = False
        self._tts_sent = False
        self._stream_url: str | None = None   # run-start URL of a streamable response

    def __aiter__(self) -> PipelineRun:
        return self

    async def __anext__(self) -> RunEvent:
        while not self._finished:
            kind, payload = await self._channel.queue.get()
            if self._fenced():
                break
            event = self._translate(kind, payload)
            if event is None:
                continue
            if isinstance(event, _TERMINAL):
                self._finish()
            return event
        raise StopAsyncIteration

    def abandon(self) -> None:
        if not self._finished:
            self._finish()
            self._channel.queue.put_nowait((_RunItem.ABANDONED, None))

    @property
    def finished(self) -> bool:
        return self._finished

    def _fenced(self) -> bool:
        """Abandoned while the iterator awaited (a method, so the loop re-reads it)."""
        return self._finished

    def _finish(self) -> None:
        self._finished = True
        self._client._channels.pop(self._channel.msg_id, None)

    def _translate(self, kind: _RunItem, payload: Any) -> RunEvent | None:
        if kind == _RunItem.LOST:
            return RunLost()
        if kind == _RunItem.RESULT:
            if payload.get("success"):
                return None
            err = _error_of(payload)
            return RunRejected(err.code, err.message)
        if kind != _RunItem.EVENT:
            return None
        etype = payload.get("type")
        data = payload.get("data") or {}
        if etype == PipelineEventType.RUN_START:
            tts = data.get("tts_output") or {}
            if tts.get("stream_response") and tts.get("url"):
                self._stream_url = self._client.endpoint.absolute(tts["url"])
        elif etype == PipelineEventType.INTENT_PROGRESS:
            if data.get("tts_start_streaming") and self._stream_url and not self._tts_sent:
                self._tts_sent = True
                return TtsReady(url=self._stream_url, streamed=True)
        elif etype == PipelineEventType.INTENT_END:
            return _intent_ended(data)
        elif etype == PipelineEventType.TTS_END:
            url = (data.get("tts_output") or {}).get("url")
            if url and not self._tts_sent:
                self._tts_sent = True
                return TtsReady(url=self._client.endpoint.absolute(url), streamed=False)
        elif etype == PipelineEventType.ERROR:
            return RunFailed(str(data.get("code", "unknown")), str(data.get("message", "")))
        elif etype == PipelineEventType.RUN_END:
            return RunEnded()
        return None


def _intent_ended(data: HaMessage) -> IntentEnded:
    output = data.get("intent_output") or {}
    response = output.get("response") or {}
    speech_by_kind = response.get("speech") or {}
    speech = ""
    for kind in ("plain", "ssml"):
        if (entry := speech_by_kind.get(kind)) and entry.get("speech"):
            speech = entry["speech"]
            break
    return IntentEnded(
        speech=speech,
        conversation_id=output.get("conversation_id"),
        continue_conversation=bool(output.get("continue_conversation")),
        response_type=response.get("response_type"),
        processed_locally=(None if data.get("processed_locally") is None
                           else bool(data["processed_locally"])),
    )


def _transport_errors() -> tuple[type[BaseException], ...]:
    import aiohttp
    return (aiohttp.ClientError, ConnectionError, OSError, RuntimeError)


# ── Client ───────────────────────────────────────────────────────────────────

class HaClient:
    """Implements HaApi plus the voice, provisioning, vocabulary, and probe surface."""

    def __init__(self, endpoint: HaEndpoint) -> None:
        self.endpoint = endpoint
        self.connected = False
        self.last_error: str | None = None       # latest connect/auth failure, for the dashboard
        self.ha_version: str | None = None
        self.vocabulary: Vocabulary | None = None
        self._session: aiohttp.ClientSession | None = None
        self._ws: aiohttp.ClientWebSocketResponse | None = None
        self._task: asyncio.Task[None] | None = None
        self._closing = False
        self._next_id = 0
        self._generation = 0
        self._channels: dict[int, _Channel] = {}
        self._send_lock = asyncio.Lock()
        self._connected_event = asyncio.Event()
        self._connect_listeners: list[Callable[[], Awaitable[None]]] = []
        self._disconnect_listeners: list[Callable[[], None]] = []
        self._background: set[asyncio.Task[None]] = set()
        self._vocab_listener: Callable[[Vocabulary], None] | None = None
        self._vocab_task: asyncio.Task[None] | None = None
        self._vocab_dirty = False

    # ── lifecycle ──

    async def start(self) -> None:
        """Open the HTTP session and keep the websocket connected until close()."""
        if self._task is not None:
            return
        import aiohttp
        self._session = aiohttp.ClientSession()
        self._task = asyncio.create_task(self._run_forever(), name="ha-client")

    async def close(self) -> None:
        self._closing = True
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
            self._task = None
        for task in list(self._background):
            task.cancel()
        if self._ws is not None:
            await self._ws.close()
        self._drop_connection()
        if self._session is not None:
            await self._session.close()
            self._session = None

    async def wait_connected(self, timeout: float) -> None:
        try:
            async with asyncio.timeout(timeout):
                while not (self.connected and self._ws is not None and not self._ws.closed):
                    self._connected_event.clear()
                    await self._connected_event.wait()
        except TimeoutError:
            raise HaUnavailable(f"not connected within {timeout}s") from None

    def add_connect_listener(self, listener: Callable[[], Awaitable[None]]) -> Callable[[], None]:
        """Run `listener` after every successful authentication (re-subscribe here)."""
        self._connect_listeners.append(listener)
        return lambda: self._connect_listeners.remove(listener)

    def add_disconnect_listener(self, listener: Callable[[], None]) -> Callable[[], None]:
        self._disconnect_listeners.append(listener)
        return lambda: self._disconnect_listeners.remove(listener)

    async def _run_forever(self) -> None:
        import aiohttp
        session = self._session
        if session is None:
            return
        delay = RECONNECT_INITIAL_S
        while not self._closing:
            try:
                ws = await session.ws_connect(
                    self.endpoint.ws_url, max_msg_size=WS_MAX_MSG_BYTES, heartbeat=HEARTBEAT_S)
            except (aiohttp.ClientError, OSError, TimeoutError) as err:
                self.last_error = f"connect: {err}"
                log.warning("HA connect to %s failed: %s; retry in %.0fs", self.endpoint.ws_url, err, delay)
                await asyncio.sleep(delay)
                delay = min(delay * 2, RECONNECT_MAX_S)
                continue
            try:
                await self._authenticate(ws)
            except (HaError, HaUnavailable) as err:
                self.last_error = str(err)
                log.warning("HA authentication failed: %s; retry in %.0fs", err, delay)
                await ws.close()
                await asyncio.sleep(delay)
                delay = min(delay * 2, RECONNECT_MAX_S)
                continue
            delay = RECONNECT_INITIAL_S
            self.last_error = None
            self._ws = ws
            self._generation += 1
            self.connected = True
            self._connected_event.set()
            log.info("HA connected (%s, version %s)", self.endpoint.ws_url, self.ha_version)
            for listener in list(self._connect_listeners):
                self._spawn(listener(), "HA connect listener")
            try:
                await self._read(ws)
            except (aiohttp.ClientError, OSError) as err:
                self.last_error = f"receive: {err}"
                log.warning("HA websocket read failed: %s", err)
            finally:
                await ws.close()
                self._drop_connection()
            if not self._closing:
                log.warning("HA connection lost; reconnecting in %.0fs", delay)
                await asyncio.sleep(delay)

    async def _authenticate(self, ws: aiohttp.ClientWebSocketResponse) -> None:
        import aiohttp

        async def receive() -> HaMessage:
            try:
                async with asyncio.timeout(COMMAND_TIMEOUT_S):
                    msg = await ws.receive_json()
            except (aiohttp.ClientError, OSError, TimeoutError, TypeError, ValueError) as err:
                raise HaUnavailable(f"auth handshake: {err!r}") from err
            if not isinstance(msg, dict):
                raise HaUnavailable(f"auth handshake: unexpected {msg!r}")
            return msg

        first = await receive()
        if first.get("type") != "auth_required":
            raise HaUnavailable(f"auth handshake: expected auth_required, got {first.get('type')!r}")
        try:
            await ws.send_json({"type": "auth", "access_token": self.endpoint.token})
        except (aiohttp.ClientError, OSError) as err:
            raise HaUnavailable(f"auth handshake: {err!r}") from err
        reply = await receive()
        if reply.get("type") == "auth_ok":
            self.ha_version = reply.get("ha_version")
            return
        if reply.get("type") == "auth_invalid":
            raise HaError("auth_invalid", str(reply.get("message", "")))
        raise HaUnavailable(f"auth handshake: unexpected {reply.get('type')!r}")

    async def _read(self, ws: aiohttp.ClientWebSocketResponse) -> None:
        import aiohttp
        async for frame in ws:
            if frame.type != aiohttp.WSMsgType.TEXT:
                if frame.type in (aiohttp.WSMsgType.ERROR, aiohttp.WSMsgType.CLOSE):
                    return
                continue
            try:
                payload = json.loads(frame.data)
            except ValueError:
                log.warning("HA sent non-JSON text frame")
                continue
            for msg in payload if isinstance(payload, list) else (payload,):
                if isinstance(msg, dict):
                    self._dispatch(msg)

    def _dispatch(self, msg: HaMessage) -> None:
        msg_id = msg.get("id")
        if not isinstance(msg_id, int):
            return
        channel = self._channels.get(msg_id)
        if channel is None:
            return
        mtype = msg.get("type")
        if mtype == "result":
            if channel.done_on_result or not msg.get("success") and isinstance(channel, _EventChannel):
                self._channels.pop(msg_id, None)
            channel.on_result(msg)
        elif mtype == "event":
            channel.on_event(msg.get("event"))

    def _drop_connection(self) -> None:
        was_connected = self.connected
        self.connected = False
        self._ws = None
        self._connected_event.clear()
        channels, self._channels = self._channels, {}
        for channel in channels.values():
            channel.on_lost()
        if was_connected:
            for listener in list(self._disconnect_listeners):
                try:
                    listener()
                except Exception:
                    log.exception("HA disconnect listener failed")

    def _spawn(self, coro: Awaitable[object], what: str) -> None:
        async def guarded() -> None:
            try:
                await coro
            except asyncio.CancelledError:
                raise
            except (HaError, HaUnavailable) as err:
                log.warning("%s: %s", what, err)
            except Exception:
                log.exception("%s failed", what)

        task = asyncio.create_task(guarded())
        self._background.add(task)
        task.add_done_callback(self._background.discard)

    # ── transport primitives ──

    async def _send(self, channel: _Channel, msg: Mapping[str, object]) -> int:
        async with self._send_lock:
            ws = self._ws
            if ws is None or ws.closed or not self.connected:
                raise HaUnavailable("not connected")
            self._next_id += 1
            msg_id = self._next_id
            self._channels[msg_id] = channel
            try:
                await ws.send_str(json.dumps({"id": msg_id, **msg}))
            except _transport_errors() as err:
                self._channels.pop(msg_id, None)
                await ws.close()
                raise HaUnavailable(f"send failed: {err}") from err
            return msg_id

    async def _send_binary(self, run: _RunChannel, data: bytes) -> None:
        async with self._send_lock:
            ws = self._ws
            if ws is None or ws.closed or self._generation != run.generation:
                raise HaUnavailable("connection lost during run")
            try:
                await ws.send_bytes(data)
            except _transport_errors() as err:
                await ws.close()
                raise HaUnavailable(f"send failed: {err}") from err

    async def command(self, msg: Mapping[str, object], timeout: float = COMMAND_TIMEOUT_S) -> Any:
        """Send one command; return its `result` (raw HA JSON, narrowed by the
        caller) or raise HaError/HaUnavailable."""
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        msg_id = await self._send(_CommandChannel(future), msg)
        try:
            async with asyncio.timeout(timeout):
                return await future
        except TimeoutError:
            self._channels.pop(msg_id, None)
            raise HaUnavailable(f"{msg.get('type')}: no reply within {timeout}s") from None

    async def subscribe(self, msg: Mapping[str, object], handler: Callable[[Any], None]) -> Subscription:
        """Open a subscription; `handler` receives each message's `event` field."""
        future: asyncio.Future[None] = asyncio.get_running_loop().create_future()
        msg_id = await self._send(_EventChannel(future, handler), msg)
        generation = self._generation
        try:
            async with asyncio.timeout(COMMAND_TIMEOUT_S):
                await future
        except TimeoutError:
            self._channels.pop(msg_id, None)
            raise HaUnavailable(f"{msg.get('type')}: no reply within {COMMAND_TIMEOUT_S}s") from None
        return _WsSubscription(self, msg_id, generation)

    async def rest(self, method: str, path: str, *, json_body: object = None,
                   params: Mapping[str, str] | None = None) -> tuple[int, Any]:
        """REST call to <http_url>/api<path> with the bearer token; returns (status, body)."""
        if self._session is None:
            raise HaUnavailable("client not started")
        import aiohttp
        url = f"{self.endpoint.http_url}/api{path}"
        try:
            async with self._session.request(
                method, url, json=json_body, params=params,
                headers={"Authorization": f"Bearer {self.endpoint.token}"},
                timeout=aiohttp.ClientTimeout(total=COMMAND_TIMEOUT_S),
            ) as resp:
                text = await resp.text()
                try:
                    body = json.loads(text) if text else None
                except ValueError:
                    body = text
                return resp.status, body
        except (aiohttp.ClientError, OSError, TimeoutError) as err:
            raise HaUnavailable(f"{method} {path}: {err!r}") from err

    async def _rest_ok(self, method: str, path: str, *, json_body: object = None,
                       params: Mapping[str, str] | None = None) -> Any:
        status, body = await self.rest(method, path, json_body=json_body, params=params)
        if status != 200:
            raise HaError(f"http_{status}", _message_of(body))
        return body

    async def stream_media(self, url: str, chunk_bytes: int = 16384) -> AsyncIterator[bytes]:
        """Stream an HA media URL (TTS proxy) with the bearer token, which the
        add-on's Supervisor proxy requires. Fetch immediately: TTS URLs expire."""
        if self._session is None:
            raise HaUnavailable("client not started")
        import aiohttp
        try:
            async with self._session.get(
                self.endpoint.absolute(url),
                headers={"Authorization": f"Bearer {self.endpoint.token}"},
                timeout=aiohttp.ClientTimeout(total=None, sock_connect=COMMAND_TIMEOUT_S,
                                              sock_read=COMMAND_TIMEOUT_S),
            ) as resp:
                if resp.status != 200:
                    raise HaError(f"http_{resp.status}", await resp.text())
                async for chunk in resp.content.iter_chunked(chunk_bytes):
                    yield chunk
        except (aiohttp.ClientError, OSError, TimeoutError) as err:
            raise HaUnavailable(f"GET {url}: {err!r}") from err

    # ── core ──

    async def get_config(self) -> HaMessage:
        config = await self.command({"type": "get_config"})
        if not isinstance(config, dict):
            raise HaError("bad_response", f"get_config: {config!r}")
        return config

    async def time_zone(self) -> str:
        zone = (await self.get_config()).get("time_zone")
        if not isinstance(zone, str):
            raise HaError("bad_response", f"time_zone: {zone!r}")
        return zone

    async def subscribe_events(self, event_type: str,
                               handler: Callable[[HaMessage], None]) -> Subscription:
        return await self.subscribe({"type": "subscribe_events", "event_type": event_type},
                                    lambda event: handler(event.get("data") or {}))

    async def fire_event(self, event_type: str, data: Mapping[str, object]) -> None:
        await self.command({"type": "fire_event", "event_type": event_type, "event_data": data})

    async def call_service(self, domain: str, service: str, data: Mapping[str, object] | None = None, *,
                           target: Mapping[str, object] | None = None,
                           return_response: bool = False) -> object:
        """Call a service; with return_response, return its response data, else None."""
        msg: dict[str, object] = {"type": "call_service", "domain": domain, "service": service}
        if target is not None:
            msg["target"] = target
        if data is not None:
            msg["service_data"] = data
        msg["return_response"] = return_response
        result = await self.command(msg)
        return (result or {}).get("response") if return_response else None

    # ── assist pipeline (§16.7) ──

    async def list_pipelines(self) -> Pipelines:
        result = await self.command({"type": "assist_pipeline/pipeline/list"})
        return Pipelines(
            items=tuple(PipelineInfo(id=p["id"], name=p["name"]) for p in result["pipelines"]),
            preferred_id=result.get("preferred_pipeline"),
        )

    async def resolve_pipeline(self, name: str | None) -> str:
        """Map a pipeline-select option to a pipeline id with HA's own rule
        (assist_pipeline.select.get_chosen_pipeline): "preferred", no state, or
        a name matching no pipeline all mean the preferred pipeline."""
        pipelines = await self.list_pipelines()
        if name is not None and name != PREFERRED_PIPELINE:
            for p in pipelines.items:
                if p.name == name:
                    return p.id
        if pipelines.preferred_id is None:
            raise HaError("pipeline_not_found", "HA has no preferred pipeline")
        return pipelines.preferred_id

    async def satellite_pipeline_id(self, device_id: str) -> str:
        """The pipeline the satellite's stock pipeline select chooses."""
        selects = await self.satellite_entities(device_id)
        option = await self.entity_state(selects.pipeline_select) if selects.pipeline_select else None
        return await self.resolve_pipeline(option)

    async def _start_run(self, msg: Mapping[str, object]) -> _RunChannel:
        channel = _RunChannel()
        channel.msg_id = await self._send(channel, msg)
        channel.generation = self._generation
        return channel

    async def run_stt(self, pipeline_id: str, device_id: str | None, pcm: bytes,
                      timeout: float = STT_TIMEOUT_S) -> str:
        """Transcribe exactly `pcm` (16 kHz PCM16 mono) with HA's segmenter off.
        STT has no side effects, so a transport failure is retried once (§16.7)."""
        try:
            return await self._stt_attempt(pipeline_id, device_id, pcm, timeout)
        except HaUnavailable as err:
            log.warning("HA STT transport failure (%s); retrying once", err)
        await self.wait_connected(STT_RECONNECT_WAIT_S)
        return await self._stt_attempt(pipeline_id, device_id, pcm, timeout)

    async def _stt_attempt(self, pipeline_id: str, device_id: str | None, pcm: bytes,
                           timeout: float) -> str:
        run = await self._start_run({
            "type": "assist_pipeline/run", "start_stage": "stt", "end_stage": "stt",
            "input": {"sample_rate": STT_SAMPLE_RATE, "no_vad": True},
            "pipeline": pipeline_id, "device_id": device_id, "timeout": timeout,
        })
        try:
            async with asyncio.timeout(timeout + RUN_REPLY_MARGIN_S):
                while True:
                    kind, payload = await run.queue.get()
                    if kind == _RunItem.LOST:
                        raise HaUnavailable("connection lost during STT run")
                    if kind == _RunItem.RESULT:
                        if not payload.get("success"):
                            raise _error_of(payload)
                        continue
                    etype = payload.get("type")
                    data = payload.get("data") or {}
                    if etype == PipelineEventType.RUN_START:
                        handler = (data.get("runner_data") or {}).get("stt_binary_handler_id")
                        if not isinstance(handler, int) or not 0 < handler < 256:
                            raise HaError("bad_handler", f"stt_binary_handler_id={handler!r}")
                        await self._send_stt_audio(run, handler, pcm)
                    elif etype == PipelineEventType.STT_END:
                        return str((data.get("stt_output") or {}).get("text") or "").strip()
                    elif etype == PipelineEventType.ERROR:
                        raise HaError(str(data.get("code", "unknown")), str(data.get("message", "")))
                    elif etype == PipelineEventType.RUN_END:
                        raise HaError("no_transcript", "STT run ended without stt-end")
        except TimeoutError:
            raise HaError("timeout", f"no STT result within {timeout + RUN_REPLY_MARGIN_S}s") from None
        finally:
            self._channels.pop(run.msg_id, None)

    async def _send_stt_audio(self, run: _RunChannel, handler: int, pcm: bytes) -> None:
        prefix = bytes((handler,))
        view = memoryview(pcm)
        for offset in range(0, len(pcm), STT_CHUNK_BYTES):
            await self._send_binary(run, prefix + view[offset:offset + STT_CHUNK_BYTES])
        await self._send_binary(run, prefix)  # handler byte alone: end of audio

    async def run_intent_tts(self, pipeline_id: str, device_id: str | None, text: str,
                             conversation_id: str | None,
                             timeout: float = INTENT_TIMEOUT_S) -> PipelineRun:
        """Dispatch intent→TTS. Raises HaUnavailable only if nothing was sent;
        once dispatched, every outcome (including loss) arrives as a RunEvent."""
        channel = await self._start_run({
            "type": "assist_pipeline/run", "start_stage": "intent", "end_stage": "tts",
            "input": {"text": text}, "pipeline": pipeline_id, "device_id": device_id,
            "conversation_id": conversation_id, "timeout": timeout,
        })
        return PipelineRun(self, channel)

    async def run_tts(self, pipeline_id: str, text: str, device_id: str | None = None) -> str:
        """TTS-only run with the pipeline's engine and voice; returns the absolute URL."""
        msg: dict[str, object] = {"type": "assist_pipeline/run", "start_stage": "tts", "end_stage": "tts",
                                  "input": {"text": text}, "pipeline": pipeline_id}
        if device_id is not None:
            msg["device_id"] = device_id
        msg["timeout"] = TTS_TIMEOUT_S
        run = await self._start_run(msg)
        try:
            async with asyncio.timeout(TTS_TIMEOUT_S + RUN_REPLY_MARGIN_S):
                while True:
                    kind, payload = await run.queue.get()
                    if kind == _RunItem.LOST:
                        raise HaUnavailable("connection lost during TTS run")
                    if kind == _RunItem.RESULT:
                        if not payload.get("success"):
                            raise _error_of(payload)
                        continue
                    etype = payload.get("type")
                    data = payload.get("data") or {}
                    if etype == PipelineEventType.TTS_END:
                        url = (data.get("tts_output") or {}).get("url")
                        if not url:
                            raise HaError("no_tts_output", "tts-end without a URL")
                        return self.endpoint.absolute(str(url))
                    if etype == PipelineEventType.ERROR:
                        raise HaError(str(data.get("code", "unknown")), str(data.get("message", "")))
                    if etype == PipelineEventType.RUN_END:
                        raise HaError("no_tts_output", "TTS run ended without tts-end")
        except TimeoutError:
            raise HaError("timeout", "no TTS result") from None
        finally:
            self._channels.pop(run.msg_id, None)

    async def handle_intent(self, name: str, slots: Mapping[str, object],
                            device_id: str | None) -> IntentResponse:
        """Run one of HA's intents directly, through the stock `POST /api/intent/handle`,
        for the speaker's HA device (EchoMuse's own timer grammar, §10.8). No conversation
        agent reads it. Returns HA's intent response; raises `HaError` when HA refused
        the request or the intent failed. `HaUnavailable` once the request may have been
        sent means its outcome is unknown: never resend it."""
        response = await self._rest_ok("POST", "/intent/handle",
                                       json_body={"name": name, "data": slots, "device_id": device_id})
        if not _is_intent_response(response):
            raise HaError("bad_response", f"{name}: {response!r}")
        if response.get("response_type") == "error":
            speech = ((response.get("speech") or {}).get("plain") or {}).get("speech")
            raise HaError(str((response.get("data") or {}).get("code", "failed_to_handle")), str(speech or name))
        return response

    # ── calendar (§16.4, §10.4) ──

    async def calendar_create(self, entity_id: str, event: Mapping[str, object]) -> None:
        await self.command({"type": "calendar/event/create", "entity_id": entity_id, "event": event})

    async def calendar_update(self, entity_id: str, uid: str, event: Mapping[str, object],
                              recurrence_id: str | None = None) -> None:
        msg: dict[str, object] = {"type": "calendar/event/update", "entity_id": entity_id, "uid": uid}
        if recurrence_id is not None:
            msg["recurrence_id"] = recurrence_id
        msg["event"] = event
        await self.command(msg)

    async def calendar_delete(self, entity_id: str, uid: str,
                              recurrence_id: str | None = None) -> bool:
        """Delete an event (no recurrence_id: the whole series). False = not found."""
        msg: dict[str, object] = {"type": "calendar/event/delete", "entity_id": entity_id, "uid": uid}
        if recurrence_id is not None:
            msg["recurrence_id"] = recurrence_id
        try:
            await self.command(msg)
        except HaError as err:
            if err.code == "failed" and _DELETE_NOT_FOUND_TEXT in err.message:
                return False
            raise
        return True

    async def calendar_subscribe(self, entity_id: str, start: datetime, end: datetime,
                                 handler: Callable[[list[CalendarEvent]], None]) -> Subscription:
        """Each push is the full event list in [start, end), normalized. HA
        pushes null when its own fetch fails; such pushes carry no list and are
        dropped rather than read as "everything deleted"."""
        def on_event(event: HaMessage) -> None:
            events = event.get("events")
            if events is None:
                log.warning("HA calendar %s push failed on the HA side; ignored", entity_id)
                return
            handler([normalize_calendar_event(e) for e in events])

        return await self.subscribe({"type": "calendar/event/subscribe", "entity_id": entity_id,
                                     "start": start.isoformat(), "end": end.isoformat()}, on_event)

    async def calendar_events(self, entity_id: str, start: datetime, end: datetime) -> list[CalendarEvent]:
        body = await self._rest_ok("GET", f"/calendars/{entity_id}",
                                   params={"start": start.isoformat(), "end": end.isoformat()})
        return [normalize_calendar_event(e) for e in body]

    # ── provisioning (§16.7) ──

    async def ensure_local_calendar(self, title: str) -> tuple[str, str]:
        """(config entry id, calendar entity id) of the Local Calendar titled
        `title`, created through the stock config flow when absent."""
        entries = await self.command({"type": "config_entries/get", "domain": LOCAL_CALENDAR_DOMAIN})
        entry_id = next((e["entry_id"] for e in entries if e.get("title") == title), None)
        if entry_id is None:
            entry_id = await self._create_local_calendar(title)
        for attempt in range(ENTITY_LOOKUP_ATTEMPTS):
            registry = await self.command({"type": "config/entity_registry/list"})
            entity_id = next((e["entity_id"] for e in registry
                              if e.get("config_entry_id") == entry_id
                              and e["entity_id"].startswith("calendar.")), None)
            if entity_id is not None:
                return entry_id, entity_id
            if attempt + 1 < ENTITY_LOOKUP_ATTEMPTS:
                await asyncio.sleep(ENTITY_LOOKUP_DELAY_S)
        raise HaError("calendar_entity_missing", f"config entry {entry_id} has no calendar entity")

    async def _create_local_calendar(self, title: str) -> str:
        form = await self._rest_ok("POST", "/config/config_entries/flow",
                                   json_body={"handler": LOCAL_CALENDAR_DOMAIN})
        if not isinstance(form, dict) or form.get("type") != "form" or not form.get("flow_id"):
            raise HaError("flow_failed", f"unexpected flow start: {form!r}")
        result = await self._rest_ok("POST", f"/config/config_entries/flow/{form['flow_id']}",
                                     json_body={"calendar_name": title, "import": "create_empty"})
        rtype = result.get("type") if isinstance(result, dict) else None
        if rtype == "create_entry":
            log.info("Created HA Local Calendar %r", title)
            return result["result"]["entry_id"]
        if rtype == "abort":
            raise HaError("flow_aborted", str(result.get("reason")))
        raise HaError("flow_failed", f"unexpected flow result: {result!r}")

    async def get_script_config(self, object_id: str) -> HaMessage | None:
        status, body = await self.rest("GET", f"/config/script/config/{object_id}")
        if status == 404:
            return None
        if status != 200:
            raise HaError(f"http_{status}", _message_of(body))
        if not isinstance(body, dict):
            raise HaError("bad_response", f"script {object_id}: {body!r}")
        return body

    async def put_script_config(self, object_id: str, config: Mapping[str, object]) -> None:
        await self._rest_ok("POST", f"/config/script/config/{object_id}", json_body=config)

    async def expose_entities(self, entity_ids: Sequence[str], assistants: Sequence[str]) -> None:
        await self.command({"type": "homeassistant/expose_entity", "assistants": list(assistants),
                            "entity_ids": list(entity_ids), "should_expose": True})

    async def satellite_entities(self, device_id: str) -> SatelliteEntities:
        """The device's stock assist_pipeline selects (first pipeline select, VAD sensitivity)."""
        pipeline: str | None = None
        vad: str | None = None
        for entry in await self.command({"type": "config/entity_registry/list"}):
            if entry.get("device_id") != device_id or not entry["entity_id"].startswith("select."):
                continue
            unique_id = entry.get("unique_id") or ""
            if unique_id.endswith(PIPELINE_SELECT_SUFFIX):
                pipeline = entry["entity_id"]
            elif unique_id.endswith(VAD_SELECT_SUFFIX):
                vad = entry["entity_id"]
        return SatelliteEntities(pipeline_select=pipeline, vad_sensitivity_select=vad)

    async def entity_state(self, entity_id: str) -> str | None:
        status, body = await self.rest("GET", f"/states/{entity_id}")
        if status == 404:
            return None
        if status != 200:
            raise HaError(f"http_{status}", _message_of(body))
        state = body.get("state") if isinstance(body, dict) else None
        return state if isinstance(state, str) else None

    async def set_select_option(self, entity_id: str, option: str) -> None:
        await self.call_service("select", "select_option", {"option": option},
                                target={"entity_id": entity_id})

    # ── vocabulary (§16.7) ──

    async def refresh_vocabulary(self) -> Vocabulary:
        exposed = (await self.command({"type": "homeassistant/expose_entity/list"}))["exposed_entities"]
        wanted = sorted(e for e, a in exposed.items() if a.get(CONVERSATION_ASSISTANT))
        entries = (await self.command({"type": "config/entity_registry/get_entries", "entity_ids": wanted})
                   if wanted else {})
        devices = await self.command({"type": "config/device_registry/list"})
        areas = await self.command({"type": "config/area_registry/list"})
        floors = await self.command({"type": "config/floor_registry/list"})
        vocab = build_vocabulary(exposed, entries, devices, areas, floors)
        self.vocabulary = vocab
        if self._vocab_listener is not None:
            self._vocab_listener(vocab)
        return vocab

    def track_vocabulary(self, on_change: Callable[[Vocabulary], None]) -> None:
        """Keep `self.vocabulary` current: rebuilt at every connect and, debounced,
        after each registry-updated event."""
        self._vocab_listener = on_change
        self.add_connect_listener(self._vocab_on_connect)
        if self.connected:
            self._spawn(self._vocab_on_connect(), "HA vocabulary")

    async def _vocab_on_connect(self) -> None:
        for event_type in VocabEvent:
            await self.subscribe_events(event_type, self._vocab_changed)
        await self.refresh_vocabulary()

    def _vocab_changed(self, _data: HaMessage) -> None:
        self._vocab_dirty = True
        if self._vocab_task is None or self._vocab_task.done():
            self._vocab_task = asyncio.create_task(self._vocab_debounced())
            self._background.add(self._vocab_task)
            self._vocab_task.add_done_callback(self._background.discard)

    async def _vocab_debounced(self) -> None:
        while self._vocab_dirty:
            await asyncio.sleep(VOCAB_DEBOUNCE_S)
            self._vocab_dirty = False
            try:
                await self.refresh_vocabulary()
            except (HaError, HaUnavailable) as err:
                log.warning("HA vocabulary refresh failed: %s", err)
                return

    # ── startup probe (§16.7) ──

    async def probe(self) -> dict[HaFeature, FeatureStatus]:
        """Exercise each dependency with a harmless call. A failure marks only
        its feature."""
        if not self.connected:
            reason = self.last_error or "not connected"
            return {f: FeatureStatus(False, reason) for f in HaFeature}
        config: HaMessage | None = None
        config_error: str | None = None
        try:
            config = await self.get_config()
        except (HaError, HaUnavailable) as err:
            config_error = f"get_config: {err}"

        def components(*names: str) -> None:
            if config is None:
                raise _ProbeFailure(config_error or "get_config failed")
            missing = [n for n in names if n not in config.get("components", [])]
            if missing:
                raise _ProbeFailure("integration not loaded: " + ", ".join(missing))

        async def voice() -> None:
            pipelines = await self.list_pipelines()
            if pipelines.preferred_id not in {p.id for p in pipelines.items}:
                raise _ProbeFailure("HA has no preferred pipeline")

        async def calendar() -> None:
            components("calendar")
            await self.command({"type": "config_entries/get", "domain": LOCAL_CALENDAR_DOMAIN})
            handlers = await self._rest_ok("GET", "/config/config_entries/flow_handlers")
            if LOCAL_CALENDAR_DOMAIN not in handlers:
                raise _ProbeFailure("local_calendar config flow unavailable")

        async def scripts() -> None:
            components("script")
            # §16.7 script and relay event: 200 or 404 both prove access;
            # subscribing to the relay event needs admin.
            await self.get_script_config(ScriptId.SET_ALARM)
            subscription = await self.subscribe_events(RelayEvent.REQUEST, lambda _data: None)
            await subscription.unsubscribe()

        async def vocabulary() -> None:
            await self.command({"type": "homeassistant/expose_entity/list"})
            await self.command({"type": "config/area_registry/list"})
            await self.command({"type": "config/floor_registry/list"})

        async def timers() -> None:
            components(*_TIMER_COMPONENTS)
            await self.command({"type": "config/device_registry/list"})
            # Voice timer starts and status questions go through the stock intent API
            # with a device (§10.8); a status question changes nothing.
            await self.handle_intent("HassTimerStatus", {}, None)

        checks: dict[HaFeature, Callable[[], Awaitable[None]]] = {
            HaFeature.VOICE: voice, HaFeature.CALENDAR: calendar, HaFeature.SCRIPTS: scripts,
            HaFeature.VOCABULARY: vocabulary, HaFeature.TIMERS: timers}
        status: dict[HaFeature, FeatureStatus] = {}
        for feature in HaFeature:
            try:
                await checks[feature]()
                status[feature] = FeatureStatus(True)
            except (HaError, HaUnavailable, _ProbeFailure) as err:
                status[feature] = FeatureStatus(False, str(err))
            except (KeyError, TypeError, AttributeError) as err:
                status[feature] = FeatureStatus(False, f"unexpected reply: {err!r}")
            if not status[feature].ok:
                log.warning("HA feature %s unavailable: %s", feature, status[feature].detail)
        return status


class _ProbeFailure(Exception):
    pass


def _is_intent_response(value: object) -> TypeGuard[IntentResponse]:
    """A JSON object whose IntentResponse fields, where present, are objects."""
    return isinstance(value, dict) and all(
        isinstance(value.get(key, {}), dict) for key in ("speech", "data", "speech_slots"))


def _message_of(body: object) -> str:
    if isinstance(body, dict) and "message" in body:
        return str(body["message"])
    return str(body)


def format_mac(mac: str) -> str:
    """A MAC in HA's device-registry form (lowercase, colon-separated); a
    string that is not 12 hex digits is only lowercased."""
    hexdigits = "".join(c for c in mac.lower() if c in "0123456789abcdef")
    if len(hexdigits) != 12:
        return mac.lower()
    return ":".join(hexdigits[i:i + 2] for i in range(0, 12, 2))
