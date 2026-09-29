"""
Controller alert engine: alarm journal, HA calendar merge, device delivery,
LLM alarm tools, and timer rings.

Authority (SPEC §3.1, §10.1): HA's Local Calendar owns which alarm schedules
and unhandled occurrences exist; HA's timer manager owns every timer. This
engine owns the occurrence lifecycle, the calendar write order (§10.3), the
write-ahead journal and restore guard (§16.3), delivery of the device alert
cache (§16.4), and turning HA timer `finished` events into `alert.ring`
(§10.8). It never writes ringing state or timers to HA.

Callers inject everything that touches the outside world: the stock HA client
(`em_ha_client.HaApi`), the device control sender, the sound/config/speaker
resolvers, a notification sink, and the wall clock.

Notification codes passed to `notify(endpoint_id, code, data)`:
  alert_ringing             {"ringing": bool | None}   (None = session lost, unknown)
  timers                    {"timers": [display copy, …]}   every tick and change
  timer_finish_missed       {"timer_id", "name"}
  timer_ring_undeliverable  {"timer_id", "name", "error"}
  operation_rejected        {"op_id", "action", "error"}
  restore_guard             {"uid", "recurrence_id"}   a restored occurrence was re-deleted
  flags                     {"flags": [...]}   dashboard flags changed
"""


from __future__ import annotations

import asyncio
import json
import logging
import secrets
import sqlite3
import time
import uuid
from contextlib import AbstractContextManager, nullcontext
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone, tzinfo
from enum import StrEnum
from typing import Any, Callable, Coroutine, Iterable, Mapping, NamedTuple, NotRequired, Protocol, Required, TypedDict
from zoneinfo import ZoneInfo

from em_alert_scripts import SCRIPT_REVISION, RelayAction, RelayEvent, config_sha256, render_scripts
from em_alert_wire import (
    DEFAULT_LABEL, FALLBACK_SOUND, LABEL_MAX_CHARS,
    AlarmEventBody, CacheObject, EchomuseLine, Occurrence, OccurrenceBody, OccurrenceKind,
    OccurrenceObject, ParentRef, RingKind, RingSettings, TombstoneReason, Weekday, alarm_event,
    alarm_start_on, build_description, canonical_json, days_from_rrule, delta_body, from_utc_ms,
    iso_seconds, key_start, next_alarm_start, normalize_days, occurrence_from_event,
    occurrence_id_for, parse_clock, rrule_for_days, sha256_hex, snapshot_pages, snooze_label,
    snooze_schedule_id, tombstone_object, with_revision,
)
from em_device_link import AckStatus, Capability, CommandAck, MessageType
from em_ha_client import CalendarEvent, HaApi, HaError, HaMessage, HaUnavailable, Subscription

log = logging.getLogger(__name__)

CATCHUP = timedelta(minutes=30)             # §10.7 catch-up window; live window start
LOOKAHEAD = timedelta(days=7)               # §10.4 materialization horizon
BACKLOG = timedelta(days=90)                # §16.4 backlog fetch
JOURNAL_RETENTION_MS = 90 * 86_400_000      # §16.3 restore-guard retention
VISIBILITY_SECONDS = 10.0                   # §16.4 step 3
RESULT_SECONDS = 5.0                        # §16.7 result timing
ROLL_SECONDS = 3600.0                       # §10.4 hourly re-subscribe
TICK_SECONDS = 1.0                          # §10.8 display countdown
MAX_SCHEDULES = 256                         # §16.3 capacity
MAX_OCCURRENCES = 4096                      # §16.3 capacity
MAX_LISTED = 10                             # §16.7 list_alarms
STATUS_OPERATIONS = 50                      # dashboard: most recent journal rows shown
STATUS_WINDOW_MS = 86_400_000               # dashboard: finished rows younger than a day
TIMER_FINISH_GRACE_MS = 5_000               # §10.8 timer_finish_missed
TIMER_RING_SECONDS = 900                    # §10.8 default timerRingSeconds
TIMER_RING_GAP_SECONDS = 2.0                # default timerRingGapSeconds


# ── vocabulary ───────────────────────────────────────────────────────────────

class OpAction(StrEnum):
    """`alert_ops.action` (§16.3)."""
    CREATE = "create"
    UPDATE = "update"
    DISMISS = "dismiss"
    SNOOZE = "snooze"
    EXPIRE = "expire"
    CANCEL = "cancel"


class OpState(StrEnum):
    """`alert_ops.state` (§16.3); APPLIED/REJECTED are also `alert.op_result.state`."""
    PENDING = "pending"
    APPLIED = "applied"
    REJECTED = "rejected"


class JournalSource(StrEnum):
    """`alert_ops.source` (§16.3)."""
    VOICE = "voice"
    LLM = "llm"
    DEVICE = "device"
    DASHBOARD = "dashboard"
    ENGINE = "engine"


class ActSource(StrEnum):
    """`alert.act.source` (WIRE §4.7)."""
    VOICE = "voice"
    ENTITY = "entity"
    LLM = "llm"
    DASHBOARD = "dashboard"


class ExpireReason(StrEnum):
    """`reason` of an expire operation (WIRE §4.7 `alert.local_operation`)."""
    TIMED_OUT = "timed_out"
    MISSED = "missed"


class TimerEvent(StrEnum):
    """HA's native satellite timer events (§10.8)."""
    STARTED = "started"
    UPDATED = "updated"
    CANCELLED = "cancelled"
    FINISHED = "finished"


class AlertNotice(StrEnum):
    """Codes passed to `notify(endpoint_id, code, data)`; see the module docstring."""
    ALERT_RINGING = "alert_ringing"
    TIMERS = "timers"
    TIMER_FINISH_MISSED = "timer_finish_missed"
    TIMER_RING_UNDELIVERABLE = "timer_ring_undeliverable"
    OPERATION_REJECTED = "operation_rejected"
    RESTORE_GUARD = "restore_guard"
    FLAGS = "flags"


class AlertFlag(StrEnum):
    """Dashboard flags of one endpoint's alert panel."""
    MALFORMED_EVENT = "malformed_event"
    OCCURRENCE_CAPACITY_EXCEEDED = "occurrence_capacity_exceeded"
    ALARM_SOUND_UNRESOLVED = "alarm_sound_unresolved"
    TIMER_SOUND_UNRESOLVED = "timer_sound_unresolved"


_SOUND_UNRESOLVED = {RingKind.ALARM: AlertFlag.ALARM_SOUND_UNRESOLVED,
                     RingKind.TIMER: AlertFlag.TIMER_SOUND_UNRESOLVED}
_TOMBSTONE_FOR = {OpAction.DISMISS: TombstoneReason.DISMISSED, OpAction.SNOOZE: TombstoneReason.SNOOZED,
                  OpAction.EXPIRE: TombstoneReason.EXPIRED, OpAction.CANCEL: TombstoneReason.DELETED}
# Device operation sources journaled as themselves; every other one journals as 'device'.
_JOURNAL_SOURCES = {JournalSource.VOICE, JournalSource.LLM, JournalSource.DASHBOARD}
_LOCAL_ACTIONS = (OpAction.DISMISS, OpAction.SNOOZE, OpAction.EXPIRE)


# ── results (JSON: HA relay answers, API bodies, `alert_ops.result_json`) ────

class ChildResult(TypedDict):
    schedule_id: str
    occurrence_id: str
    name: str
    due: str


class CancelledAlarm(TypedDict):
    name: str
    kind: OccurrenceKind


class ListedAlarm(TypedDict):
    name: str
    kind: OccurrenceKind
    due: str
    repeats: list[Weekday]
    schedule_id: str
    occurrence_id: str
    armed_on_endpoint: bool


class OccurrenceFacts(TypedDict, total=False):
    """What an operation acted on, as its answer reports it."""
    kind: OccurrenceKind | RingKind
    name: str
    schedule_id: str
    occurrence_id: str
    due: str
    reason: ExpireReason
    first_due: str
    days: list[Weekday]
    child: ChildResult


class AlertResult(OccurrenceFacts, total=False):
    """Every operation's answer; which keys appear depends on the operation."""
    ok: Required[bool]
    op_id: str
    error: str | None
    stored_in_ha: bool
    pending: bool
    armed_on_endpoint: bool
    delivery_pending: bool
    cancelled: list[CancelledAlarm]
    errors: list[str | None]
    alarms: list[ListedAlarm]


class OpPayload(TypedDict):
    """`alert_ops.payload_json`: the caller's `request` plus what applying needs."""
    result: OccurrenceFacts
    request: NotRequired[dict[str, object]]
    event: NotRequired[AlarmEventBody]
    uid: NotRequired[str]
    group: NotRequired[list[str]]
    child_occurrence_id: NotRequired[str]
    device_child_occurrence_id: NotRequired[object]


class TimerDisplay(TypedDict):
    id: str
    name: str
    total_seconds: int
    remaining_seconds: int
    active: bool


class OccurrenceStatus(OccurrenceBody):
    armed_on_endpoint: bool


class OperationStatus(TypedDict):
    op_id: str
    action: OpAction
    source: JournalSource
    state: OpState
    step: int
    created_ms: int
    result: AlertResult | None


class AlertStatus(TypedDict):
    """The dashboard alert panel of one endpoint."""
    calendar_entity: str
    calendar_entry_id: str
    delivery_epoch: str
    sequence: int
    acked_sequence: int
    online: bool
    calendar_live: bool
    flags: list[AlertFlag]
    ringing: bool | None
    alert_state: dict[str, object] | None
    occurrences: list[OccurrenceStatus]
    timers: list[TimerDisplay]
    operations: list[OperationStatus]


# ── collaborators ────────────────────────────────────────────────────────────

class Send(Protocol):
    """Send one control message; returns the envelope message_id (or None)."""
    async def __call__(self, device_id: str, msg_type: MessageType, body: Mapping[str, object], *,
                      generation: int = 0) -> str | None: ...


# (endpoint_id, kind) -> asset sha256 or "builtin:fallback"; None when the
# configured catalog ID no longer resolves.
SoundResolver = Callable[[str, RingKind], str | None]
Notify = Callable[[str, AlertNotice, dict[str, object]], None]


class OpConflict(Exception):
    """An op_id was reused with a different payload (§16.3 `op_id_conflict`)."""


@dataclass(frozen=True, slots=True)
class Speaker:
    """An endpoint as the LLM relay names it (§16.7 speaker resolution)."""
    endpoint_id: str
    name: str
    area: str | None = None


@dataclass(slots=True)
class TimerCopy:
    """Display copy of one HA timer (§10.8); never decides that it finished."""
    timer_id: str
    name: str
    total_seconds: int
    seconds_left: int
    active: bool
    at_ms: int              # when `seconds_left` was reported

    def remaining(self, now_ms: int) -> float:
        if not self.active:
            return float(self.seconds_left)
        return max(0.0, self.seconds_left - (now_ms - self.at_ms) / 1000)

    def zero_ms(self) -> int:
        return self.at_ms + self.seconds_left * 1000

    def as_dict(self, now_ms: int) -> TimerDisplay:
        return {"id": self.timer_id, "name": self.name, "total_seconds": self.total_seconds,
                "remaining_seconds": round(self.remaining(now_ms)), "active": self.active}


@dataclass(frozen=True, slots=True)
class ActiveRing:
    """`alert.state.active` (WIRE §4.7): what rings on the device now."""
    id: str
    kind: RingKind | None           # None: a kind this controller does not know
    name: str

    @classmethod
    def parse(cls, active: object) -> ActiveRing | None:
        if not isinstance(active, Mapping) or not active:
            return None
        kind = active.get("kind")
        name = active.get("name", "")
        return cls(id=str(active.get("id")),
                   kind=RingKind(kind) if isinstance(kind, str) and kind in RingKind.__members__.values() else None,
                   name=name if isinstance(name, str) else "")


@dataclass(frozen=True, slots=True)
class _Act:
    """An `alert.act` this engine sent, by op_id."""
    target_id: str
    kind: RingKind | None
    name: str


@dataclass(frozen=True, slots=True)
class OpRow:
    """One `alert_ops` row."""
    op_id: str
    action: OpAction
    schedule_id: str
    occurrence_key: str | None
    source: JournalSource
    payload: OpPayload
    payload_sha256: str
    state: OpState
    step: int
    result: AlertResult | None
    created_ms: int

    @classmethod
    def from_row(cls, row: sqlite3.Row) -> OpRow:
        payload: OpPayload = json.loads(row["payload_json"])
        result: AlertResult | None = json.loads(row["result_json"]) if row["result_json"] else None
        return cls(op_id=row["op_id"], action=OpAction(row["action"]), schedule_id=row["schedule_id"],
                   occurrence_key=row["occurrence_key"], source=JournalSource(row["source"]),
                   payload=payload, payload_sha256=row["payload_sha256"], state=OpState(row["state"]),
                   step=row["step"], result=result, created_ms=row["created_ms"])


class _Terminal(NamedTuple):
    action: OpAction
    schedule_id: str
    occurrence_key: str | None


@dataclass
class _Endpoint:
    endpoint_id: str
    entry_id: str
    calendar: str
    epoch: str
    sequence: int
    acked: int
    # Materialization.
    subscription: Subscription | None = None
    token: str = ""
    window_start: datetime | None = None
    window_end: datetime | None = None
    fresh: bool = False             # a push arrived since the latest (re)subscribe
    materialized: bool = False      # any push since start
    pushed: dict[str, Occurrence] = field(default_factory=dict)   # last push, raw
    live: dict[str, Occurrence] = field(default_factory=dict)     # guarded, capped
    seen: dict[str, Occurrence] = field(default_factory=dict)     # every occurrence observed
    push_event: asyncio.Event = field(default_factory=asyncio.Event)
    guard_inflight: set[tuple[str, str | None]] = field(default_factory=set)
    worker: asyncio.Task[None] | None = None
    # Delivery.
    online: bool = False
    prefetch: bool = False          # the device installs sounds alert.prefetch names
    delivered: dict[str, tuple[OccurrenceBody, int]] = field(default_factory=dict)
    delivered_valid: bool = False   # `delivered` is known to equal the device cache
    schedule_rev: dict[str, int] = field(default_factory=dict)
    ack_event: asyncio.Event = field(default_factory=asyncio.Event)
    delivery_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    # Device state and acts.
    alert_state: dict[str, object] | None = None    # the latest `alert.state` body
    active: ActiveRing | None = None                # its `active`
    act_by_message: dict[str, str] = field(default_factory=dict)
    act_by_target: dict[str, str] = field(default_factory=dict)
    act_info: dict[str, _Act] = field(default_factory=dict)
    timer_results: dict[str, AlertResult] = field(default_factory=dict)
    timers: dict[str, TimerCopy] = field(default_factory=dict)
    flags: set[AlertFlag] = field(default_factory=set)
    unresolved: dict[str, Mapping[str, object]] = field(default_factory=dict)   # device ops awaiting HA


class _Journal:
    """`alert_ops` / `alert_delivery` / `alert_scripts` access. Every call is one
    short transaction under the shared connection lock."""

    def __init__(self, db: sqlite3.Connection, lock: AbstractContextManager[object]):
        self.db, self.lock = db, lock

    def _cur(self) -> sqlite3.Cursor:
        cur = self.db.cursor()
        cur.row_factory = sqlite3.Row
        return cur

    def query(self, sql: str, params: tuple[object, ...] = ()) -> list[sqlite3.Row]:
        with self.lock:
            return self._cur().execute(sql, params).fetchall()

    def write(self, sql: str, params: tuple[object, ...] = ()) -> None:
        with self.lock, self.db:
            self.db.execute(sql, params)

    def get(self, op_id: str) -> OpRow | None:
        rows = self.query("SELECT * FROM alert_ops WHERE op_id=?", (op_id,))
        return OpRow.from_row(rows[0]) if rows else None

    def insert(self, *, op_id: str, endpoint_id: str, calendar: str, action: OpAction,
               schedule_id: str, key: str | None, source: JournalSource, payload: OpPayload,
               digest: str, now_ms: int) -> OpRow:
        """Insert a pending row, or return the existing row for a same-hash duplicate."""
        with self.lock, self.db:
            cur = self._cur()
            old = cur.execute("SELECT * FROM alert_ops WHERE op_id=?", (op_id,)).fetchone()
            if old is not None:
                if old["payload_sha256"] != digest:
                    raise OpConflict(op_id)
                return OpRow.from_row(old)
            cur.execute(
                "INSERT INTO alert_ops(op_id,endpoint_id,calendar_entity,action,schedule_id,"
                "occurrence_key,source,payload_json,payload_sha256,state,created_ms) "
                "VALUES (?,?,?,?,?,?,?,?,?,'pending',?)",
                (op_id, endpoint_id, calendar, action, schedule_id, key, source,
                 canonical_json(payload), digest, now_ms))
            return OpRow.from_row(cur.execute("SELECT * FROM alert_ops WHERE op_id=?", (op_id,)).fetchone())

    def advance(self, op_id: str, step: int) -> None:
        self.write("UPDATE alert_ops SET step=? WHERE op_id=?", (step, op_id))

    def finish(self, op_id: str, applied: bool, result: AlertResult, now_ms: int) -> None:
        self.write("UPDATE alert_ops SET state=?, result_json=?, applied_ms=? WHERE op_id=?",
                   (OpState.APPLIED if applied else OpState.REJECTED, canonical_json(result),
                    now_ms if applied else None, op_id))

    def next_pending(self, endpoint_id: str) -> OpRow | None:
        rows = self.query("SELECT * FROM alert_ops WHERE endpoint_id=? AND state='pending' "
                          "ORDER BY seq LIMIT 1", (endpoint_id,))
        return OpRow.from_row(rows[0]) if rows else None

    def terminal(self, endpoint_id: str) -> list[_Terminal]:
        return [_Terminal(OpAction(r["action"]), r["schedule_id"], r["occurrence_key"]) for r in self.query(
            "SELECT action, schedule_id, occurrence_key FROM alert_ops WHERE endpoint_id=? "
            "AND state='applied' AND action IN ('dismiss','snooze','expire','cancel')",
            (endpoint_id,))]

    def last_terminal(self, schedule_id: str, key: str) -> OpAction | None:
        rows = self.query(
            "SELECT action FROM alert_ops WHERE schedule_id=? AND occurrence_key=? "
            "AND state='applied' ORDER BY seq DESC LIMIT 1", (schedule_id, key))
        return OpAction(rows[0]["action"]) if rows else None

    def schedule_keys(self, schedule_id: str) -> list[str]:
        return [r["occurrence_key"] for r in self.query(
            "SELECT DISTINCT occurrence_key FROM alert_ops WHERE schedule_id=? "
            "AND occurrence_key IS NOT NULL", (schedule_id,))]

    def pending_creates(self, endpoint_id: str) -> set[str]:
        return {r["schedule_id"] for r in self.query(
            "SELECT schedule_id FROM alert_ops WHERE endpoint_id=? AND state='pending' "
            "AND action='create'", (endpoint_id,))}

    def recent(self, endpoint_id: str, since_ms: int) -> list[OpRow]:
        """Pending rows and rows created since `since_ms`, newest first."""
        return [OpRow.from_row(r) for r in self.query(
            "SELECT * FROM alert_ops WHERE endpoint_id=? AND (state='pending' OR created_ms>=?) "
            "ORDER BY seq DESC LIMIT ?", (endpoint_id, since_ms, STATUS_OPERATIONS))]

    def prune(self, before_ms: int) -> None:
        self.write("DELETE FROM alert_ops WHERE state!='pending' AND created_ms<?", (before_ms,))

    def endpoints(self) -> list[_Endpoint]:
        return [_Endpoint(r["endpoint_id"], r["calendar_entry_id"], r["calendar_entity"],
                          r["delivery_epoch"], r["sequence"], r["acked_sequence"])
                for r in self.query("SELECT * FROM alert_delivery")]

    def add_endpoint(self, endpoint_id: str, entry_id: str, calendar: str, epoch: str) -> None:
        self.write("INSERT INTO alert_delivery VALUES (?,?,?,?,0,0)",
                   (endpoint_id, entry_id, calendar, epoch))

    def save_delivery(self, endpoint_id: str, sequence: int, acked: int) -> None:
        self.write("UPDATE alert_delivery SET sequence=?, acked_sequence=? "
                   "WHERE endpoint_id=?", (sequence, acked, endpoint_id))

    def script_sha256(self, object_id: str) -> str | None:
        rows = self.query("SELECT sha256 FROM alert_scripts WHERE object_id=?", (object_id,))
        return rows[0]["sha256"] if rows else None

    def record_script(self, object_id: str, digest: str) -> None:
        self.write("INSERT OR REPLACE INTO alert_scripts VALUES (?,?,?)",
                   (object_id, digest, SCRIPT_REVISION))


def _occurrence_result(occ: Occurrence, reason: ExpireReason | None = None) -> OccurrenceFacts:
    result: OccurrenceFacts = {"kind": occ.kind, "name": occ.label, "schedule_id": occ.schedule_id,
                               "occurrence_id": occ.occurrence_id, "due": occ.due_local}
    if reason:
        result["reason"] = reason
    return result


def _pending(op_id: str) -> AlertResult:
    return {"ok": True, "stored_in_ha": False, "pending": True, "op_id": op_id}


def _failure(error: str, *, stored_in_ha: bool | None = None) -> AlertResult:
    result: AlertResult = {"ok": False, "error": error}
    if stored_in_ha is not None:
        result["stored_in_ha"] = stored_in_ha
    return result


_HA_UNREACHABLE = "Home Assistant is unreachable"


class AlertEngine:
    """One engine per controller. All methods run on the asyncio thread."""

    def __init__(
        self,
        db: sqlite3.Connection,
        ha: HaApi,
        send: Send,
        *,
        sound_resolver: SoundResolver,
        config_resolver: Callable[[str], Mapping[str, object]],
        speakers: Callable[[], Iterable[Speaker]],
        awaiting_intent: Callable[[], Iterable[str]],
        notify: Notify | None = None,
        now_ms: Callable[[], int] | None = None,
        db_lock: AbstractContextManager[object] | None = None,
    ):
        self.ha, self.send = ha, send
        self.sound_resolver = sound_resolver
        self.config_resolver = config_resolver
        self.speakers = speakers
        self.awaiting_intent = awaiting_intent
        self.notify: Notify = notify or (lambda _endpoint, _code, _data: None)
        self.now_ms = now_ms or (lambda: time.time_ns() // 1_000_000)
        self.journal = _Journal(db, db_lock if db_lock is not None else nullcontext())
        self._eps: dict[str, _Endpoint] = {}
        self._futures: dict[str, asyncio.Future[AlertResult]] = {}
        self._tasks: set[asyncio.Task[None]] = set()
        self._relay: Subscription | None = None
        self._tz: tzinfo = timezone.utc
        self._connect_lock = asyncio.Lock()
        self._closed = False
        for ep in self.journal.endpoints():
            self._eps[ep.endpoint_id] = ep

    # ── lifecycle ─────────────────────────────────────────────────────────

    async def start(self) -> None:
        """Prune the journal and, if HA is up, reconcile and subscribe."""
        self.journal.prune(self.now_ms() - JOURNAL_RETENTION_MS)
        if self.ha.connected:
            await self.on_ha_connected()

    async def run(self) -> None:
        """Tick timer display copies every second; roll windows hourly."""
        last_roll = time.monotonic()
        while not self._closed:
            await asyncio.sleep(TICK_SECONDS)
            self.tick_timers()
            if time.monotonic() - last_roll >= ROLL_SECONDS:
                last_roll = time.monotonic()
                await self.roll_windows()

    async def close(self) -> None:
        self._closed = True
        subs = [self._relay] + [ep.subscription for ep in self._eps.values()]
        self._relay = None
        for ep in self._eps.values():
            ep.subscription = None
            ep.token = ""
            if ep.worker:
                ep.worker.cancel()
        for sub in subs:
            if sub is not None:
                try:
                    await sub.unsubscribe()
                except HaUnavailable:
                    pass
        for task in list(self._tasks):
            task.cancel()

    async def ensure_endpoint(self, endpoint_id: str, label: str) -> str:
        """Provision the endpoint's Local Calendar once (§16.7); the stored entity
        survives renames. Returns the calendar entity."""
        ep = self._eps.get(endpoint_id)
        if ep is not None:
            return ep.calendar
        entry_id, calendar = await self.ha.ensure_local_calendar(f"EchoMuse {label}")
        epoch = str(secrets.randbits(64) or 1)
        self.journal.add_endpoint(endpoint_id, entry_id, calendar, epoch)
        ep = self._eps[endpoint_id] = _Endpoint(endpoint_id, entry_id, calendar, epoch, 0, 0)
        if self.ha.connected and self._relay is not None:
            await self._reconcile_backlog(ep)
            await self._subscribe(ep)
        return calendar

    async def on_ha_connected(self) -> None:
        """Backlog reconciliation, then subscriptions, then retries (§16.4)."""
        async with self._connect_lock:
            try:
                self._tz = ZoneInfo(await self.ha.time_zone())
                if self._relay is None:
                    self._relay = await self.ha.subscribe_events(RelayEvent.REQUEST, self._on_relay_event)
                for ep in self._eps.values():
                    await self._reconcile_backlog(ep)
                    await self._subscribe(ep)
                    for body in list(ep.unresolved.values()):
                        await self.on_local_operation(ep.endpoint_id, body)
            except HaUnavailable as err:
                log.warning("alerts: HA dropped during reconnect: %s", err)

    def on_ha_disconnected(self) -> None:
        self._relay = None
        for ep in self._eps.values():
            ep.fresh = False
            ep.subscription = None
            ep.token = ""

    async def roll_windows(self) -> None:
        """Re-subscribe every endpoint for the current window (§10.4)."""
        horizon = self.now_ms() - JOURNAL_RETENTION_MS
        for ep in self._eps.values():
            ep.seen = {k: o for k, o in ep.seen.items() if o.due_utc_ms >= horizon}
            if self.ha.connected:
                try:
                    await self._subscribe(ep)
                except HaUnavailable:
                    return
        self.journal.prune(horizon)

    # ── materialization ───────────────────────────────────────────────────

    def _now(self) -> datetime:
        return from_utc_ms(self.now_ms(), timezone.utc)

    async def _subscribe(self, ep: _Endpoint) -> None:
        now = self._now()
        token = str(uuid.uuid4())
        old, ep.subscription = ep.subscription, None
        ep.token, ep.fresh = token, False
        ep.window_start, ep.window_end = now - CATCHUP, now + LOOKAHEAD
        if old is not None:
            try:
                await old.unsubscribe()
            except HaUnavailable:
                pass
        ep.subscription = await self.ha.calendar_subscribe(
            ep.calendar, now - CATCHUP, now + LOOKAHEAD,
            lambda events: self._on_push(ep, token, events))

    def _occurrences(self, ep: _Endpoint, events: Iterable[CalendarEvent]) -> dict[str, Occurrence]:
        sound = self._sound(ep, RingKind.ALARM)
        out = {}
        for event in events:
            occ = occurrence_from_event(ep.calendar, event, sound)
            if occ is not None:
                out[occ.occurrence_id] = occ
        return out

    def _on_push(self, ep: _Endpoint, token: str, events: list[CalendarEvent]) -> None:
        """Handle one full-window push (§10.4): diff, guard, cap, deliver."""
        if token != ep.token or self._closed:
            return
        pushed = self._occurrences(ep, events)
        ep.pushed = pushed
        ep.seen.update(pushed)
        self._set_flag(ep, AlertFlag.MALFORMED_EVENT, any(o.malformed for o in pushed.values()))
        live, deletes = self._guard(ep, pushed)
        ordered = sorted(live.values(), key=lambda o: (o.due_utc_ms, o.occurrence_id))
        self._set_flag(ep, AlertFlag.OCCURRENCE_CAPACITY_EXCEEDED, len(ordered) > MAX_OCCURRENCES)
        ep.live = {o.occurrence_id: o for o in ordered[:MAX_OCCURRENCES]}
        was_fresh = ep.fresh
        ep.fresh = ep.materialized = True
        event, ep.push_event = ep.push_event, asyncio.Event()
        event.set()
        self._spawn(self._after_push(ep, deletes))
        if not was_fresh:
            self._kick(ep)

    def _guard(self, ep: _Endpoint, occurrences: dict[str, Occurrence]
               ) -> tuple[dict[str, Occurrence], list[tuple[str, str | None]]]:
        """Restore guard (§16.4): split out occurrences an applied terminal row or
        cancel already handled; return what remains and the deletes to redo."""
        rows = self.journal.terminal(ep.endpoint_id)
        cancelled = {r.schedule_id for r in rows if r.action == OpAction.CANCEL}
        handled = {(r.schedule_id, r.occurrence_key) for r in rows if r.action != OpAction.CANCEL}
        keep: dict[str, Occurrence] = {}
        deletes: list[tuple[str, str | None]] = []
        for oid, occ in occurrences.items():
            if occ.schedule_id in cancelled:
                deletes.append((occ.uid, None))
            elif (occ.schedule_id, occ.key) in handled:
                deletes.append((occ.uid, occ.recurrence_id))
            else:
                keep[oid] = occ
        return keep, list(dict.fromkeys(deletes))

    async def _redelete(self, ep: _Endpoint, deletes: list[tuple[str, str | None]]) -> None:
        for uid, rid in deletes:
            if (uid, rid) in ep.guard_inflight:
                continue
            ep.guard_inflight.add((uid, rid))
            try:
                if await self.ha.calendar_delete(ep.calendar, uid, rid):
                    self.notify(ep.endpoint_id, AlertNotice.RESTORE_GUARD, {"uid": uid, "recurrence_id": rid})
            except (HaUnavailable, HaError) as err:
                log.warning("alerts: restore guard delete %s/%s failed: %s", uid, rid, err)
            finally:
                ep.guard_inflight.discard((uid, rid))

    async def _after_push(self, ep: _Endpoint, deletes: list[tuple[str, str | None]]) -> None:
        await self._redelete(ep, deletes)
        await self._deliver(ep)

    async def _reconcile_backlog(self, ep: _Endpoint) -> None:
        """REST backlog (§16.4): guard, then expire the rest as `missed`."""
        now = self._now()
        events = await self.ha.calendar_events(ep.calendar, now - BACKLOG, now - CATCHUP)
        past = {k: o for k, o in self._occurrences(ep, events).items() if o.start < now - CATCHUP}
        ep.seen.update(past)
        keep, deletes = self._guard(ep, past)
        await self._redelete(ep, deletes)
        for occ in keep.values():
            op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"echomuse-missed:{occ.occurrence_id}"))
            request: dict[str, object] = {"action": OpAction.EXPIRE, "occurrence_id": occ.occurrence_id,
                                          "schedule_id": occ.schedule_id, "reason": ExpireReason.MISSED}
            self._enqueue(ep, op_id, OpAction.EXPIRE, occ.schedule_id, occ.key, JournalSource.ENGINE, request,
                          {"result": _occurrence_result(occ, reason=ExpireReason.MISSED)})

    # ── the journal worker (§16.4 "Applying one operation") ──────────────

    def _spawn(self, coro: Coroutine[Any, Any, None]) -> asyncio.Task[None]:
        task = asyncio.get_running_loop().create_task(coro)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    def _kick(self, ep: _Endpoint) -> None:
        if ep.worker is None or ep.worker.done():
            ep.worker = self._spawn(self._drain(ep))

    def _enqueue(self, ep: _Endpoint, op_id: str, action: OpAction, schedule_id: str,
                 key: str | None, source: JournalSource, request: Mapping[str, object],
                 payload: OpPayload) -> OpRow:
        """Journal an operation as `pending`. `payload_sha256` hashes the request
        (the caller-visible arguments), so a retry that recomputes derived fields
        is still a duplicate. Raises OpConflict."""
        row = self.journal.insert(
            op_id=op_id, endpoint_id=ep.endpoint_id, calendar=ep.calendar, action=action,
            schedule_id=schedule_id, key=key, source=source,
            payload={**payload, "request": dict(request)}, digest=sha256_hex(request),
            now_ms=self.now_ms())
        if row.state == OpState.PENDING:
            self._kick(ep)
        return row

    async def _drain(self, ep: _Endpoint) -> None:
        """Apply pending operations for one calendar strictly in seq order."""
        while not self._closed and self.ha.connected and ep.fresh:
            row = self.journal.next_pending(ep.endpoint_id)
            if row is None:
                return
            try:
                await self._apply(ep, row)
            except HaUnavailable:
                return
            except HaError as err:
                self._reject(ep, row, f"{err.code}: {err.message}")

    async def _apply(self, ep: _Endpoint, row: OpRow) -> None:
        op_id, action, payload = row.op_id, row.action, row.payload
        sid, key = row.schedule_id, row.occurrence_key
        if action == OpAction.CREATE and "event" in payload:
            event = payload["event"]
            start = datetime.fromisoformat(event["dtstart"])
            if ep.window_end is not None and start >= ep.window_end:
                # A series whose first day falls past the live window (§16.3 DST edge).
                if await self._rest_by_op(ep, op_id, start) is None:
                    await self.ha.calendar_create(ep.calendar, event)
                    self.journal.advance(op_id, 1)
                if await self._rest_by_op(ep, op_id, start) is None:
                    return self._reject(ep, row, "not_visible")
            else:
                if self._by_op(ep, op_id) is None:
                    await self.ha.calendar_create(ep.calendar, event)
                    self.journal.advance(op_id, 1)
                if not await self._visible(ep, lambda: self._by_op(ep, op_id) is not None):
                    return self._reject(ep, row, "not_visible")
        elif action in (OpAction.DISMISS, OpAction.EXPIRE) and key is not None:
            if not await self._delete_occurrence(ep, sid, key, op_id, step=1):
                return self._reject(ep, row, "not_visible")
        elif action == OpAction.CANCEL and "uid" in payload:
            await self.ha.calendar_delete(ep.calendar, payload["uid"], None)
            self.journal.advance(op_id, 1)
            if not await self._visible(ep, lambda: not any(
                    o.schedule_id == sid for o in ep.pushed.values())):
                return self._reject(ep, row, "not_visible")
        elif (action == OpAction.SNOOZE and key is not None and "event" in payload
              and "child_occurrence_id" in payload):
            if self._by_op(ep, op_id) is None:
                if row.step == 0 and not await self._schedule_exists(ep, sid, key):
                    return self._reject(ep, row, "parent_schedule_deleted")
                await self.ha.calendar_create(ep.calendar, payload["event"])
                self.journal.advance(op_id, 1)
            if not await self._visible(ep, lambda: self._by_op(ep, op_id) is not None):
                return self._reject(ep, row, "not_visible")
            if not await self._delete_occurrence(ep, sid, key, op_id, step=2):
                return self._reject(ep, row, "not_visible")
            device_child = payload.get("device_child_occurrence_id")
            if isinstance(device_child, str) and device_child and device_child != payload["child_occurrence_id"]:
                await self._send_objects(ep, [(device_child, None, TombstoneReason.DELETED)])
        else:
            return self._reject(ep, row, f"unsupported action {action}")
        self._finish(op_id, True, {**payload["result"], "ok": True, "op_id": op_id,
                                   "stored_in_ha": True})

    def _reject(self, ep: _Endpoint, row: OpRow, error: str) -> None:
        op_id, action = row.op_id, row.action
        self._finish(op_id, False, {"ok": False, "op_id": op_id, "error": error,
                                    "stored_in_ha": False})
        self.notify(ep.endpoint_id, AlertNotice.OPERATION_REJECTED,
                    {"op_id": op_id, "action": action, "error": error})
        log.warning("alerts: %s %s rejected: %s", action, op_id, error)
        if action == OpAction.SNOOZE:
            child = row.payload.get("device_child_occurrence_id")
            if isinstance(child, str) and child:
                self._spawn(self._send_objects(ep, [(child, None, TombstoneReason.DELETED)]))

    def _finish(self, op_id: str, applied: bool, result: AlertResult) -> None:
        self.journal.finish(op_id, applied, result, self.now_ms())
        self._resolve(op_id, result)

    def _resolve(self, op_id: str, result: AlertResult) -> None:
        """Answer whoever awaits `op_id`'s result."""
        future = self._futures.pop(op_id, None)
        if future is not None and not future.done():
            future.set_result(result)

    async def _delete_occurrence(self, ep: _Endpoint, sid: str, key: str,
                                 op_id: str, step: int) -> bool:
        """Delete one occurrence if present ("not found" = done) and confirm."""
        occ, from_rest = await self._current(ep, sid, key)
        if occ is not None:
            await self.ha.calendar_delete(ep.calendar, occ.uid, occ.recurrence_id)
        self.journal.advance(op_id, step)
        if from_rest:
            return (await self._current(ep, sid, key))[0] is None
        return await self._visible(ep, lambda: self._find(ep.pushed, sid, key) is None)

    async def _visible(self, ep: _Endpoint, done: Callable[[], bool]) -> bool:
        """Wait for a push showing the final state; after VISIBILITY_SECONDS
        re-subscribe once to force a fresh push (§16.4 step 3)."""
        for attempt in range(2):
            if attempt:
                await self._subscribe(ep)
            deadline = asyncio.get_running_loop().time() + VISIBILITY_SECONDS
            while not done():
                if not self.ha.connected:
                    raise HaUnavailable("disconnected while confirming")
                remaining = deadline - asyncio.get_running_loop().time()
                if remaining <= 0:
                    break
                try:
                    await asyncio.wait_for(ep.push_event.wait(), remaining)
                except asyncio.TimeoutError:
                    break
            if done():
                return True
        return False

    @staticmethod
    def _find(view: dict[str, Occurrence], sid: str, key: str) -> Occurrence | None:
        return next((o for o in view.values() if o.schedule_id == sid and o.key == key), None)

    def _by_op(self, ep: _Endpoint, op_id: str) -> Occurrence | None:
        return next((o for o in ep.pushed.values() if o.line and o.line.op_id == op_id), None)

    async def _rest_by_op(self, ep: _Endpoint, op_id: str, start: datetime) -> Occurrence | None:
        events = await self.ha.calendar_events(ep.calendar, start - timedelta(minutes=1),
                                               start + timedelta(minutes=2))
        return next((o for o in self._occurrences(ep, events).values()
                     if o.line and o.line.op_id == op_id), None)

    async def _current(self, ep: _Endpoint, sid: str, key: str) -> tuple[Occurrence | None, bool]:
        """The occurrence (sid, key) as HA has it now: from the live push when it
        falls inside the window, else by a narrow REST fetch. Returns
        (occurrence, looked_up_by_rest)."""
        occ = self._find(ep.pushed, sid, key)
        if occ is not None:
            return occ, False
        start = key_start(key, self._tz)
        if start is None or ep.window_start is None or start >= ep.window_start:
            return None, False
        events = await self.ha.calendar_events(ep.calendar, start - timedelta(days=1),
                                               start + timedelta(days=1))
        return self._find(self._occurrences(ep, events), sid, key), True

    async def _schedule_exists(self, ep: _Endpoint, sid: str, key: str) -> bool:
        if any(o.schedule_id == sid for o in ep.pushed.values()):
            return True
        return (await self._current(ep, sid, key))[0] is not None

    # ── delivery (§16.4) ─────────────────────────────────────────────────

    async def _deliver(self, ep: _Endpoint) -> None:
        """Bring the device cache to `ep.live`: a delta when the device's view is
        known, otherwise a snapshot."""
        async with ep.delivery_lock:
            if not ep.online:
                if self._diff(ep) != ([], []):
                    ep.delivered_valid = False
                return
            if not ep.delivered_valid:
                await self._snapshot(ep)
                return
            changed, removed = self._diff(ep)
            if not changed and not removed:
                return
            ep.sequence += 1
            objects: list[CacheObject] = []
            for occ in changed:
                body = occ.cache_body()
                ep.delivered[occ.occurrence_id] = (body, ep.sequence)
                ep.schedule_rev[occ.schedule_id] = ep.sequence
                objects.append(with_revision(body, ep.sequence))
            for oid in removed:
                body, _ = ep.delivered.pop(oid)
                ep.schedule_rev[body["schedule_id"]] = ep.sequence
                objects.append(tombstone_object(oid, ep.sequence, self._tombstone_reason(ep, oid)))
            self._persist_delivery(ep)
            await self._send_device(ep, MessageType.ALERT_DELTA, delta_body(ep.epoch, ep.sequence, objects))

    def _diff(self, ep: _Endpoint) -> tuple[list[Occurrence], list[str]]:
        changed = [o for oid, o in ep.live.items()
                   if oid not in ep.delivered or ep.delivered[oid][0] != o.cache_body()]
        removed = sorted(set(ep.delivered) - set(ep.live))
        return changed, removed

    def _tombstone_reason(self, ep: _Endpoint, oid: str) -> TombstoneReason:
        occ = ep.seen.get(oid)
        action = self.journal.last_terminal(occ.schedule_id, occ.key) if occ else None
        return _TOMBSTONE_FOR.get(action, TombstoneReason.DELETED) if action else TombstoneReason.DELETED

    async def _snapshot(self, ep: _Endpoint) -> None:
        ep.sequence += 1
        delivered: dict[str, tuple[OccurrenceBody, int]] = {}
        for oid, occ in ep.live.items():
            body = occ.cache_body()
            old = ep.delivered.get(oid)
            rev = old[1] if ep.delivered_valid and old and old[0] == body else ep.sequence
            delivered[oid] = (body, rev)
            ep.schedule_rev[occ.schedule_id] = ep.sequence
        for oid in set(ep.delivered) - set(ep.live):
            ep.schedule_rev[ep.delivered[oid][0]["schedule_id"]] = ep.sequence
        ep.delivered, ep.delivered_valid = delivered, True
        self._persist_delivery(ep)
        objects: list[OccurrenceObject] = [with_revision(body, rev) for body, rev in delivered.values()]
        for page in snapshot_pages(ep.epoch, ep.sequence, objects):
            if not await self._send_device(ep, MessageType.ALERT_SNAPSHOT, page):
                return

    async def _send_objects(self, ep: _Endpoint,
                            tombstones: list[tuple[str, str | None, TombstoneReason]]) -> None:
        """Deliver tombstones for occurrences the controller never delivered
        (a rejected or re-identified snooze child)."""
        async with ep.delivery_lock:
            if not ep.online or not ep.delivered_valid:
                return
            ep.sequence += 1
            self._persist_delivery(ep)
            objects: list[CacheObject] = [tombstone_object(oid, ep.sequence, reason)
                                          for oid, _sid, reason in tombstones]
            await self._send_device(ep, MessageType.ALERT_DELTA, delta_body(ep.epoch, ep.sequence, objects))

    async def _send_device(self, ep: _Endpoint, msg_type: MessageType, body: Mapping[str, object]) -> bool:
        try:
            await self.send(ep.endpoint_id, msg_type, body, generation=0)
            return True
        except Exception as err:  # the session layer owns transport errors
            log.warning("alerts: %s to %s failed: %s", msg_type, ep.endpoint_id, err)
            ep.delivered_valid = False
            return False

    def _persist_delivery(self, ep: _Endpoint) -> None:
        self.journal.save_delivery(ep.endpoint_id, ep.sequence, ep.acked)

    def _armed(self, ep: _Endpoint, schedule_ids: Iterable[str]) -> bool:
        sids = list(schedule_ids)
        return (ep.online and ep.delivered_valid and bool(sids)
                and all(sid in ep.schedule_rev and ep.acked >= ep.schedule_rev[sid] for sid in sids))

    async def _facts(self, ep: _Endpoint, result: AlertResult, schedule_ids: Iterable[str],
                     deadline: float) -> AlertResult:
        """Add `armed_on_endpoint` / `delivery_pending` (§10.5), waiting for the
        device's durable ack until `deadline` (loop time)."""
        sids = list(schedule_ids)
        loop = asyncio.get_running_loop()
        while ep.online and not self._armed(ep, sids) and loop.time() < deadline:
            try:
                await asyncio.wait_for(ep.ack_event.wait(), deadline - loop.time())
            except asyncio.TimeoutError:
                break
        armed = self._armed(ep, sids)
        return {**result, "armed_on_endpoint": armed,
                "delivery_pending": bool(result.get("stored_in_ha")) and not armed}

    # ── device messages (WIRE §4.1, §4.7) ────────────────────────────────

    async def on_session_hello(self, endpoint_id: str, alerts: Mapping[str, object], *,
                               capabilities: Iterable[str] = ()) -> None:
        """`session.hello.alerts`: snapshot on an epoch/sequence mismatch; then the
        timer sound, so a first ring does not fall back for want of it."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        ep.online = True
        ep.prefetch = Capability.ALERT_PREFETCH in capabilities
        dev_epoch, dev_acked = alerts.get("delivery_epoch"), alerts.get("acked_sequence")
        if dev_epoch == ep.epoch and isinstance(dev_acked, int) and 0 <= dev_acked <= ep.sequence:
            ep.acked = dev_acked
            self._persist_delivery(ep)
        if dev_epoch != ep.epoch or dev_acked != ep.sequence:
            ep.delivered_valid = False
        if ep.materialized:
            await self._deliver(ep)
        await self._prefetch_timer_sound(ep)

    def on_session_lost(self, endpoint_id: str) -> None:
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        ep.online = False
        if ep.alert_state is not None:
            ep.alert_state, ep.active = None, None
            self.notify(endpoint_id, AlertNotice.ALERT_RINGING, {"ringing": None})

    async def on_alert_ack(self, endpoint_id: str, body: Mapping[str, object]) -> None:
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        if body.get("need_snapshot"):
            ep.delivered_valid = False
            if ep.materialized:
                await self._deliver(ep)
            return
        applied = body.get("applied_through")
        if (body.get("durable") and body.get("delivery_epoch") == ep.epoch
                and isinstance(applied, int) and ep.acked <= applied <= ep.sequence):
            ep.acked = applied
            self._persist_delivery(ep)
            event, ep.ack_event = ep.ack_event, asyncio.Event()
            event.set()

    def on_alert_state(self, endpoint_id: str, body: Mapping[str, object]) -> None:
        """`alert.state`: drives the “Alert ringing” entity."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        was = ep.active is not None if ep.alert_state is not None else None
        ep.alert_state, ep.active = dict(body), ActiveRing.parse(body.get("active"))
        now = ep.active is not None
        if was is not now:
            self.notify(endpoint_id, AlertNotice.ALERT_RINGING, {"ringing": now})

    def on_alert_ring_ended(self, endpoint_id: str, body: Mapping[str, object]) -> None:
        """Telemetry; completes a pending timer `alert.act` for that ring."""
        ep = self._eps.get(endpoint_id)
        if ep is None or body.get("kind") != RingKind.TIMER:
            return
        op_id = ep.act_by_target.pop(str(body.get("id")), None)
        if op_id:
            self._complete_timer_act(ep, op_id, {"ok": True})

    def on_command_ack(self, endpoint_id: str, ack: CommandAck) -> None:
        """`command.ack` for an `alert.act` this engine sent."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        op_id = ep.act_by_message.get(ack.message_id)
        if op_id is None:
            return
        act = ep.act_info.get(op_id)
        timer = act is not None and act.kind == RingKind.TIMER
        if ack.status == AckStatus.REJECTED:
            ep.act_by_message.pop(ack.message_id, None)
            result: AlertResult = {"ok": False, "op_id": op_id, "error": ack.error or "rejected"}
            if timer:
                self._complete_timer_act(ep, op_id, result)
            else:
                self._resolve(op_id, result)
        elif ack.status == AckStatus.APPLIED and timer:
            ep.act_by_message.pop(ack.message_id, None)
            self._complete_timer_act(ep, op_id, {"ok": True})

    def _complete_timer_act(self, ep: _Endpoint, op_id: str, outcome: AlertResult) -> None:
        act = ep.act_info.pop(op_id, None)
        ep.act_by_target.pop(act.target_id if act else "", None)
        result: AlertResult = {**outcome, "op_id": op_id, "kind": RingKind.TIMER,
                               "name": act.name if act else ""}
        ep.timer_results[op_id] = result
        self._resolve(op_id, result)

    async def on_local_operation(self, endpoint_id: str, body: Mapping[str, object]) -> None:
        """`alert.local_operation` (§10.6 merge table); answers `alert.op_result`
        once the operation is applied or rejected."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        raw_op_id = body.get("op_id")
        try:
            op_id = str(uuid.UUID(str(raw_op_id)))
            raw_action, oid, sid = body["action"], body["occurrence_id"], body["schedule_id"]
            if raw_action not in _LOCAL_ACTIONS:
                raise ValueError("invalid_action")
            if not isinstance(oid, str) or not isinstance(sid, str):
                raise ValueError("invalid_occurrence")
        except (KeyError, ValueError) as err:
            await self._early_result(ep, str(raw_op_id), False, str(err))
            return
        action = OpAction(str(raw_action))
        request = {k: body.get(k) for k in ("action", "occurrence_id", "schedule_id", "reason", "child")}
        source = body.get("source")
        journal_source = (JournalSource(source) if isinstance(source, str) and source in _JOURNAL_SOURCES
                          else JournalSource.DEVICE)
        existing = self.journal.get(op_id)
        if existing is not None:
            if existing.payload_sha256 != sha256_hex(request):
                await self._early_result(ep, op_id, False, "op_id_conflict")
            else:
                self._spawn(self._answer_when_done(ep, op_id))
            return
        try:
            occ = await self._locate(ep, sid, oid)
        except HaUnavailable:
            # Retried after the next HA connect; the device also re-sends it.
            ep.unresolved[op_id] = body
            return
        ep.unresolved.pop(op_id, None)
        if occ is None:
            if action == OpAction.SNOOZE:
                await self._early_result(ep, op_id, False, "parent_occurrence_unknown")
                child = body.get("child")
                child_oid = child.get("occurrence_id") if isinstance(child, Mapping) else None
                if isinstance(child_oid, str):
                    await self._send_objects(ep, [(child_oid, None, TombstoneReason.DELETED)])
            else:
                # Absent from HA and never observed: nothing to write, and nothing
                # a restore could bring back.
                await self._early_result(ep, op_id, True, None)
            return
        try:
            payload: OpPayload
            if action == OpAction.SNOOZE:
                payload = self._snooze_payload(occ, op_id, body.get("child"))
            else:
                reason = body.get("reason") if action == OpAction.EXPIRE else None
                if action == OpAction.EXPIRE and reason not in (ExpireReason.TIMED_OUT, ExpireReason.MISSED):
                    raise ValueError("invalid_reason")
                payload = {"result": _occurrence_result(
                    occ, reason=ExpireReason(str(reason)) if reason is not None else None)}
            self._enqueue(ep, op_id, action, occ.schedule_id, occ.key, journal_source, request, payload)
        except ValueError as err:
            await self._early_result(ep, op_id, False, str(err))
            return
        except OpConflict:
            await self._early_result(ep, op_id, False, "op_id_conflict")
            return
        self._spawn(self._answer_when_done(ep, op_id))

    async def _early_result(self, ep: _Endpoint, op_id: str, applied: bool, error: str | None) -> None:
        """Answer a device operation that needs no journal row."""
        self._resolve(op_id, {"ok": applied, "op_id": op_id, "error": error, "stored_in_ha": applied})
        await self._op_result(ep, op_id, applied, error)

    async def _answer_when_done(self, ep: _Endpoint, op_id: str) -> None:
        result = await self._result(op_id)
        await self._op_result(ep, op_id, bool(result.get("ok")), result.get("error"))

    async def _op_result(self, ep: _Endpoint, op_id: str, applied: bool, error: str | None) -> None:
        if ep.online:
            await self._send_device(ep, MessageType.ALERT_OP_RESULT, {
                "op_id": op_id, "state": OpState.APPLIED if applied else OpState.REJECTED, "error": error})

    async def _locate(self, ep: _Endpoint, sid: str, oid: str) -> Occurrence | None:
        """Find the occurrence a device operation names: everything observed, then
        the journal's keys, then HA's REST view over the backlog and window.
        Raises HaUnavailable when only HA could answer and it is unreachable."""
        occ = ep.seen.get(oid)
        if occ is not None and occ.schedule_id == sid:
            return occ
        for key in self.journal.schedule_keys(sid):
            if occurrence_id_for(sid, key) == oid:
                return self._ghost(ep, sid, key, oid)
        if not self.ha.connected:
            raise HaUnavailable("occurrence lookup needs Home Assistant")
        now = self._now()
        try:
            events = await self.ha.calendar_events(ep.calendar, now - BACKLOG, now + LOOKAHEAD)
        except HaError as err:
            raise HaUnavailable(f"occurrence lookup failed: {err.message}") from err
        found = self._occurrences(ep, events).get(oid)
        if found is not None:
            ep.seen[oid] = found
        return found if found is not None and found.schedule_id == sid else None

    def _ghost(self, ep: _Endpoint, sid: str, key: str, oid: str) -> Occurrence:
        """An occurrence known only by its journaled key: label and ring settings
        come from any other observed occurrence of the same schedule."""
        sibling = next((o for o in ep.seen.values() if o.schedule_id == sid), None)
        start = key_start(key, self._tz) or _EPOCH_UTC
        return Occurrence(
            occurrence_id=oid, schedule_id=sid, key=key, uid=sibling.uid if sibling else "",
            recurrence_id=None, rrule=None, summary=sibling.summary if sibling else "",
            start=start, line=sibling.line if sibling else None, malformed=False,
            settings=sibling.settings if sibling else RingSettings(sound=self._sound(ep, RingKind.ALARM)))

    def _snooze_payload(self, parent: Occurrence, op_id: str, child: object) -> OpPayload:
        """The child event the device derived (§10.7, §16.3): dtstart from the
        device's frozen `due_utc_ms`, written in HA's timezone, carrying the
        device's op_id."""
        if not isinstance(child, Mapping):
            raise ValueError("missing_child")
        child_sid = snooze_schedule_id(parent.occurrence_id)
        if child.get("schedule_id") != child_sid:
            raise ValueError("invalid_child_schedule")
        try:
            due_ms = int(child["due_utc_ms"])
        except (KeyError, TypeError, ValueError):
            raise ValueError("invalid_child_due") from None
        start = from_utc_ms(due_ms, self._tz)
        child_oid = occurrence_id_for(child_sid, iso_seconds(start))
        line = EchomuseLine(child_sid, OccurrenceKind.SNOOZE, op_id,
                            ParentRef(parent.schedule_id, parent.key), parent.settings)
        label = snooze_label(parent.summary)
        return {
            "event": alarm_event(label, start, None, build_description(line)),
            "child_occurrence_id": child_oid,
            "device_child_occurrence_id": child.get("occurrence_id"),
            "result": {**_occurrence_result(parent), "child": {
                "schedule_id": child_sid, "occurrence_id": child_oid, "name": label,
                "due": iso_seconds(start)}},
        }

    # ── operation results ─────────────────────────────────────────────────

    async def _result(self, op_id: str) -> AlertResult:
        """The finished result of `op_id`, once it has one."""
        row = self.journal.get(op_id)
        if row is not None and row.result is not None:
            return row.result
        return await asyncio.shield(self._future(op_id))

    async def _result_by(self, op_id: str, deadline: float) -> AlertResult | None:
        """The finished result of `op_id`, or None if still pending at `deadline` (loop time)."""
        row = self.journal.get(op_id)
        if row is not None and row.result is not None:
            return row.result
        timeout = max(0.0, deadline - asyncio.get_running_loop().time())
        try:
            return await asyncio.wait_for(asyncio.shield(self._future(op_id)), timeout)
        except asyncio.TimeoutError:
            return None

    def _future(self, op_id: str) -> asyncio.Future[AlertResult]:
        future = self._futures.get(op_id)
        if future is None or future.done():
            future = self._futures[op_id] = asyncio.get_running_loop().create_future()
        return future

    # ── voice / LLM / dashboard operations (§10.5, §16.7) ────────────────

    async def set_alarm(self, endpoint_id: str, time: str, days: Iterable[object] = (),
                        name: str = "", *, on_date: date | None = None,
                        source: JournalSource = JournalSource.VOICE,
                        op_id: str | None = None) -> AlertResult:
        """Create an alarm event (§10.3 Create). `time` is `HH:MM[:SS]` wall time
        in HA's timezone; `days` weekday names (empty = once); `on_date` pins a
        one-shot to a date. Answers when applied or after RESULT_SECONDS."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return _failure("speaker has no alarm calendar")
        try:
            at, norm_days = parse_clock(time), normalize_days(days)
        except ValueError as err:
            return _failure(str(err))
        if on_date is not None and norm_days:
            return _failure("a dated alarm cannot repeat")
        label = (name or "").strip() or DEFAULT_LABEL
        if len(label) > LABEL_MAX_CHARS:
            return _failure(f"name is longer than {LABEL_MAX_CHARS} characters")
        op_id = op_id or str(uuid.uuid4())
        request: dict[str, object] = {
            "action": RelayAction.SET_ALARM, "endpoint_id": endpoint_id, "time": at.isoformat(),
            "days": list(norm_days), "name": label, "date": on_date.isoformat() if on_date else None}
        existing = self.journal.get(op_id)
        if existing is None:
            if not self.ha.connected:
                return _failure(_HA_UNREACHABLE, stored_in_ha=False)
            echomuse = {o.schedule_id for o in ep.pushed.values() if o.is_echomuse}
            if len(echomuse | self.journal.pending_creates(endpoint_id)) >= MAX_SCHEDULES:
                return _failure("capacity_exceeded")
            try:
                tz = ZoneInfo(await self.ha.time_zone())
            except HaUnavailable:
                return _failure(_HA_UNREACHABLE, stored_in_ha=False)
            if on_date is not None:
                start = alarm_start_on(on_date, at, tz)
                if start <= self._now():
                    return _failure("that time has already passed")
            else:
                start = next_alarm_start(self._now(), tz, at, norm_days)
            schedule_id = str(uuid.uuid5(uuid.UUID(op_id), "schedule"))
            sound = self._sound(ep, RingKind.ALARM)
            line = EchomuseLine(schedule_id, OccurrenceKind.ALARM, op_id, None, RingSettings(sound=sound))
            event = alarm_event(label, start, rrule_for_days(norm_days), build_description(line))
            result: OccurrenceFacts = {"kind": OccurrenceKind.ALARM, "name": label,
                                       "schedule_id": schedule_id, "first_due": iso_seconds(start),
                                       "days": list(norm_days)}
            try:
                self._enqueue(ep, op_id, OpAction.CREATE, schedule_id, None, source, request,
                              {"event": event, "result": result})
            except OpConflict:
                return _failure("op_id_conflict")
        elif existing.payload_sha256 != sha256_hex(request):
            return _failure("op_id_conflict")
        else:
            schedule_id = existing.schedule_id
        answer = await self._result_by(op_id, deadline)
        if answer is None:
            return _pending(op_id)
        if not answer.get("ok"):
            return answer
        return await self._facts(ep, answer, [schedule_id], deadline)

    def list_alarms(self, endpoint_id: str) -> AlertResult:
        """The next ≤10 alarm occurrences on the speaker."""
        ep = self._eps.get(endpoint_id)
        if ep is None or not ep.materialized:
            return _failure("alarm calendar is unavailable")
        now = self.now_ms()
        upcoming = [o for o in ep.live.values() if o.due_utc_ms >= now][:MAX_LISTED]
        return {"ok": True, "alarms": [{
            "name": o.label, "kind": o.kind, "due": o.due_local,
            "repeats": list(days_from_rrule(o.rrule) or ()) if o.rrule else [],
            "schedule_id": o.schedule_id, "occurrence_id": o.occurrence_id,
            "armed_on_endpoint": self._armed(ep, [o.schedule_id]),
        } for o in upcoming]}

    async def cancel_alarm(self, endpoint_id: str, *, name: str = "", time: str = "",
                           all_alarms: bool = False, source: JournalSource = JournalSource.VOICE,
                           op_id: str | None = None) -> AlertResult:
        """Delete matching schedules, whole series (§10.3 Cancel)."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return _failure("speaker has no alarm calendar")
        name = (name or "").strip()
        if not (name or time or all_alarms):
            return _failure("name, time, or all: true is required")
        try:
            at = parse_clock(time) if time else None
        except ValueError as err:
            return _failure(str(err))
        root = op_id or str(uuid.uuid4())
        request: dict[str, object] = {
            "action": RelayAction.CANCEL_ALARM, "endpoint_id": endpoint_id, "name": name,
            "time": at.isoformat() if at else None, "all": bool(all_alarms)}
        existing = self.journal.get(root)
        if existing is not None:
            if existing.payload_sha256 != sha256_hex(request):
                return _failure("op_id_conflict")
            group = existing.payload.get("group", [root])
        else:
            if not self.ha.connected:
                return _failure(_HA_UNREACHABLE, stored_in_ha=False)
            targets: dict[str, Occurrence] = {}
            for occ in ep.live.values():
                if name and occ.label.casefold() != name.casefold():
                    continue
                if at and occ.start.time() != at:
                    continue
                targets.setdefault(occ.schedule_id, occ)
            if not targets:
                return _failure("no matching alarm")
            sids = list(targets)
            group = [root] + [str(uuid.uuid5(uuid.UUID(root), sid)) for sid in sids[1:]]
            for op, sid in zip(group, sids):
                occ = targets[sid]
                facts: OccurrenceFacts = {"kind": occ.kind, "name": occ.label, "schedule_id": sid}
                req = request if op == root else {**request, "root": root, "schedule_id": sid}
                self._enqueue(ep, op, OpAction.CANCEL, sid, None, source, req,
                              {"uid": occ.uid, "group": group, "result": facts})
        results = []
        for op in group:
            result = await self._result_by(op, deadline)
            if result is None:
                return _pending(root)
            results.append(result)
        done = [r for r in results if r.get("ok")]
        combined: AlertResult = {
            "ok": bool(done), "op_id": root,
            "cancelled": [{"name": r.get("name", ""), "kind": OccurrenceKind(r.get("kind", OccurrenceKind.ALARM))}
                          for r in done],
            "stored_in_ha": len(done) == len(results)}
        if len(done) < len(results):
            combined["errors"] = [r.get("error") for r in results if not r.get("ok")]
        return await self._facts(ep, combined, [r.get("schedule_id", "") for r in done], deadline)

    async def dismiss_alert(self, endpoint_id: str, *, source: ActSource = ActSource.VOICE,
                            op_id: str | None = None) -> AlertResult:
        """Stop whatever rings on the speaker (timer or alarm); a repeating alarm
        loses only this occurrence."""
        return await self._act(endpoint_id, OpAction.DISMISS, source, op_id)

    async def snooze_alarm(self, endpoint_id: str, *, source: ActSource = ActSource.VOICE,
                           op_id: str | None = None) -> AlertResult:
        """Snooze the ringing alarm; timers are rejected."""
        return await self._act(endpoint_id, OpAction.SNOOZE, source, op_id)

    async def _act(self, endpoint_id: str, action: OpAction, source: ActSource,
                   op_id: str | None) -> AlertResult:
        """`alert.act` to the device; an alarm completes through the device's
        journaled local operation with the same op_id (§6.3, WIRE §4.7)."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return _failure("unknown speaker")
        op_id = op_id or str(uuid.uuid4())
        if op_id in ep.timer_results:
            return ep.timer_results[op_id]
        if self.journal.get(op_id) is not None:
            return await self._finish_act(ep, op_id, deadline)
        if not ep.online:
            return _failure("speaker is offline")
        active = ep.active
        if active is None:
            return _failure("nothing is ringing")
        if action == OpAction.SNOOZE and active.kind != RingKind.ALARM:
            return _failure("timers cannot be snoozed")
        ep.act_info[op_id] = _Act(active.id, active.kind, active.name)
        if active.kind == RingKind.TIMER:
            ep.act_by_target[active.id] = op_id
        self._future(op_id)
        message_id = await self.send(endpoint_id, MessageType.ALERT_ACT, {
            "op_id": op_id, "target_id": active.id, "action": action,
            "source": source}, generation=0)
        if message_id:
            ep.act_by_message[message_id] = op_id
        return await self._finish_act(ep, op_id, deadline)

    async def _finish_act(self, ep: _Endpoint, op_id: str, deadline: float) -> AlertResult:
        result = await self._result_by(op_id, deadline)
        if result is None:
            return _pending(op_id)
        if result.get("kind") == RingKind.TIMER or not result.get("ok"):
            return result
        schedule_id = result.get("schedule_id")
        if schedule_id is None:
            # Early result (an occurrence the controller never observed):
            # nothing was written to HA, so there are no delivery facts.
            return result
        sids = [schedule_id]
        child = result.get("child")
        if child is not None:
            sids.append(child["schedule_id"])
        return await self._facts(ep, result, sids, deadline)

    # ── LLM relay (§16.7) ─────────────────────────────────────────────────

    def resolve_speaker(self, speaker: str | None) -> str | None:
        """Speaker field → endpoint name, then area; empty → the single turn
        awaiting an intent run; otherwise None ("which speaker?")."""
        query = (speaker or "").strip().casefold()
        if query:
            speakers = list(self.speakers())
            fields: tuple[Callable[[Speaker], str | None], ...] = (lambda s: s.name, lambda s: s.area)
            for field_of in fields:
                hits = {s.endpoint_id for s in speakers if (field_of(s) or "").casefold() == query}
                if hits:
                    return hits.pop() if len(hits) == 1 else None
            return None
        waiting = set(self.awaiting_intent())
        return waiting.pop() if len(waiting) == 1 else None

    def _on_relay_event(self, data: HaMessage) -> None:
        self._spawn(self._relay_request(data))

    async def _relay_request(self, data: HaMessage) -> None:
        request_id = data.get("request_id")
        if not isinstance(request_id, str) or not request_id:
            return
        result = await self.handle_request(data.get("action"), data.get("args"), request_id)
        try:
            await self.ha.fire_event(RelayEvent.RESULT, {"request_id": request_id, "result": result})
        except (HaUnavailable, HaError) as err:
            log.warning("alerts: could not answer %s: %s", request_id, err)

    async def handle_request(self, action: object, args: object, request_id: str) -> AlertResult:
        """One `echomuse_alert_request`: validate, resolve the speaker, apply with
        op_id = UUIDv5(NAMESPACE_URL, request_id)."""
        if not isinstance(args, Mapping):
            return _failure("args must be an object")
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, request_id))

        def text(key: str) -> str:
            value = args.get(key)
            return value if isinstance(value, str) else ""

        days = args.get("days") or []
        if action == RelayAction.SET_ALARM:
            if not text("time"):
                return _failure("time is required")
            if not isinstance(days, list):
                return _failure("days must be a list of weekdays")
            try:
                parse_clock(text("time"))
                normalize_days(days)
            except ValueError as err:
                return _failure(str(err))
        elif action == RelayAction.CANCEL_ALARM:
            if not (text("name").strip() or text("time") or _truthy(args.get("all"))):
                return _failure("name, time, or all: true is required")
            if text("time"):
                try:
                    parse_clock(text("time"))
                except ValueError as err:
                    return _failure(str(err))
        elif action not in (RelayAction.LIST_ALARMS, RelayAction.DISMISS_ALERT, RelayAction.SNOOZE_ALARM):
            return _failure("unknown action")
        endpoint = self.resolve_speaker(text("speaker"))
        if endpoint is None:
            return _failure("which speaker?")
        if action == RelayAction.SET_ALARM and isinstance(days, list):
            return await self.set_alarm(endpoint, text("time"), days, text("name"),
                                        source=JournalSource.LLM, op_id=op_id)
        if action == RelayAction.LIST_ALARMS:
            return self.list_alarms(endpoint)
        if action == RelayAction.CANCEL_ALARM:
            return await self.cancel_alarm(endpoint, name=text("name"), time=text("time"),
                                           all_alarms=_truthy(args.get("all")),
                                           source=JournalSource.LLM, op_id=op_id)
        if action == RelayAction.DISMISS_ALERT:
            return await self.dismiss_alert(endpoint, source=ActSource.LLM, op_id=op_id)
        return await self.snooze_alarm(endpoint, source=ActSource.LLM, op_id=op_id)

    async def install_scripts(self) -> list[str]:
        """Install/upgrade the five Assist scripts (§16.7 Installation). Returns
        warnings for user-edited scripts, which are left untouched."""
        warnings: list[str] = []
        managed: list[str] = []
        wrote = False
        for object_id, config in render_scripts().items():
            want = config_sha256(config)
            current = await self.ha.get_script_config(object_id)
            recorded = self.journal.script_sha256(object_id)
            if current is not None:
                have = config_sha256(current)
                if have != recorded and have != want:
                    warnings.append(f"script.{object_id} was edited in Home Assistant; "
                                    "EchoMuse left it unchanged")
                    log.warning("alerts: %s", warnings[-1])
                    continue
                revision = (current.get("variables") or {}).get("echomuse_revision")
                if revision == SCRIPT_REVISION or have == want:
                    if recorded != have:
                        self.journal.record_script(object_id, have)
                    managed.append(object_id)
                    continue
            await self.ha.put_script_config(object_id, config)
            self.journal.record_script(object_id, want)
            managed.append(object_id)
            wrote = True
        if wrote:
            await self.ha.call_service("script", "reload")
        if managed:
            await self.ha.expose_entities([f"script.{o}" for o in managed], ["conversation"])
        return warnings

    # ── timers (§10.8) ────────────────────────────────────────────────────

    async def on_timer_event(self, endpoint_id: str, event: TimerEvent, timer_id: str, name: str,
                             total_seconds: int, seconds_left: int, is_active: bool) -> None:
        """HA's native timer event for this speaker's satellite."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        if event in (TimerEvent.STARTED, TimerEvent.UPDATED):
            ep.timers[timer_id] = TimerCopy(timer_id, name, int(total_seconds), int(seconds_left),
                                            bool(is_active), self.now_ms())
        else:
            ep.timers.pop(timer_id, None)
        self.notify(endpoint_id, AlertNotice.TIMERS, {"timers": self.timers(endpoint_id)})
        if event == TimerEvent.STARTED:
            await self._prefetch_timer_sound(ep)      # timerSound may have changed since hello
        elif event == TimerEvent.FINISHED:
            await self._ring_timer(ep, timer_id, name)

    def timers(self, endpoint_id: str) -> list[TimerDisplay]:
        ep = self._eps.get(endpoint_id)
        now = self.now_ms()
        return [t.as_dict(now) for t in ep.timers.values()] if ep else []

    def tick_timers(self) -> None:
        """Once a second: publish display copies; flag a zero with no `finished`."""
        now = self.now_ms()
        for ep in self._eps.values():
            if not ep.timers:
                continue
            for timer in list(ep.timers.values()):
                if timer.active and now - timer.zero_ms() >= TIMER_FINISH_GRACE_MS:
                    del ep.timers[timer.timer_id]
                    log.warning("alerts: timer %s reached zero without finished", timer.timer_id)
                    self.notify(ep.endpoint_id, AlertNotice.TIMER_FINISH_MISSED,
                                {"timer_id": timer.timer_id, "name": timer.name})
            self.notify(ep.endpoint_id, AlertNotice.TIMERS, {"timers": self.timers(ep.endpoint_id)})

    async def _ring_timer(self, ep: _Endpoint, timer_id: str, name: str) -> None:
        config = self.config_resolver(ep.endpoint_id) or {}
        body = {
            "ring_id": timer_id, "name": name, "sound": self._sound(ep, RingKind.TIMER),
            "loop_gap_ms": round(_seconds(config, "timerRingGapSeconds", TIMER_RING_GAP_SECONDS) * 1000),
            "max_ring_ms": round(_seconds(config, "timerRingSeconds", TIMER_RING_SECONDS) * 1000),
        }
        error = None
        if not ep.online:
            error = "speaker offline"
        else:
            try:
                await self.send(ep.endpoint_id, MessageType.ALERT_RING, body, generation=0)
            except Exception as err:  # the session layer owns transport errors
                error = str(err)
        if error is not None:
            log.warning("alerts: timer_ring_undeliverable %s on %s: %s", timer_id, ep.endpoint_id, error)
            self.notify(ep.endpoint_id, AlertNotice.TIMER_RING_UNDELIVERABLE,
                        {"timer_id": timer_id, "name": name, "error": error})

    async def _prefetch_timer_sound(self, ep: _Endpoint) -> None:
        """Have the device install the timer sound before a ring needs it: `alert.ring`
        names it only as the timer finishes, and a missing asset rings the fallback
        tone (§16.5). The device skips a sound it already holds."""
        if not (ep.online and ep.prefetch):
            return
        sound = self._sound(ep, RingKind.TIMER)
        if sound == FALLBACK_SOUND:
            return
        try:
            await self.send(ep.endpoint_id, MessageType.ALERT_PREFETCH, {"sounds": [sound]}, generation=0)
        except Exception as err:  # the session layer owns transport errors
            log.info("alerts: timer sound prefetch on %s not sent: %s", ep.endpoint_id, err)

    # ── sounds, flags, status ─────────────────────────────────────────────

    def _sound(self, ep: _Endpoint, kind: RingKind) -> str:
        """Effective alarmSound/timerSound as a sha256; unresolvable → fallback, flagged."""
        sound = self.sound_resolver(ep.endpoint_id, kind)
        self._set_flag(ep, _SOUND_UNRESOLVED[kind], sound is None)
        return sound or FALLBACK_SOUND

    def _set_flag(self, ep: _Endpoint, flag: AlertFlag, on: bool) -> None:
        if on == (flag in ep.flags):
            return
        (ep.flags.add if on else ep.flags.discard)(flag)
        self.notify(ep.endpoint_id, AlertNotice.FLAGS, {"flags": sorted(ep.flags)})

    def status(self, endpoint_id: str) -> AlertStatus | None:
        """Dashboard alert panel state for one endpoint."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return None
        ops: list[OperationStatus] = [
            {"op_id": r.op_id, "action": r.action, "source": r.source, "state": r.state,
             "step": r.step, "created_ms": r.created_ms, "result": r.result}
            for r in self.journal.recent(endpoint_id, self.now_ms() - STATUS_WINDOW_MS)]
        occurrences: list[OccurrenceStatus] = [
            {**o.cache_body(), "armed_on_endpoint": self._armed(ep, [o.schedule_id])}
            for o in ep.live.values()]
        return {
            "calendar_entity": ep.calendar, "calendar_entry_id": ep.entry_id,
            "delivery_epoch": ep.epoch, "sequence": ep.sequence, "acked_sequence": ep.acked,
            "online": ep.online, "calendar_live": ep.fresh, "flags": sorted(ep.flags),
            "ringing": ep.active is not None if ep.alert_state else None,
            "alert_state": ep.alert_state,
            "occurrences": occurrences,
            "timers": self.timers(endpoint_id),
            "operations": ops,
        }


def _truthy(value: object) -> bool:
    return value is True or (isinstance(value, str) and value.strip().casefold() == "true")


def _seconds(config: Mapping[str, object], key: str, default: float) -> float:
    """A config duration in seconds; the default when absent or not a number."""
    value = config.get(key, default)
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        return default
    try:
        return float(value)
    except ValueError:
        return default


_EPOCH_UTC = datetime(1970, 1, 1, tzinfo=timezone.utc)
