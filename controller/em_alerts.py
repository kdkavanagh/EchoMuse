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
import threading
import time
import uuid
from contextlib import nullcontext
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta, timezone, tzinfo
from typing import Any, Awaitable, Callable, ContextManager, Iterable
from zoneinfo import ZoneInfo

from em_alert_scripts import EVENT_REQUEST, EVENT_RESULT, SCRIPT_REVISION, config_sha256, render_scripts
from em_alert_wire import (
    DEFAULT_LABEL, FALLBACK_SOUND, KIND_ALARM, KIND_SNOOZE, LABEL_MAX_CHARS,
    EchomuseLine, Occurrence, ParentRef, RingSettings, alarm_event, alarm_start_on,
    build_description, canonical_json, days_from_rrule, delta_body, from_utc_ms,
    iso_seconds, key_start, next_alarm_start, normalize_days, occurrence_from_event,
    occurrence_id_for, parse_clock, rrule_for_days, sha256_hex, snapshot_pages,
    snooze_label, snooze_schedule_id, tombstone_object,
)
from em_ha_client import HaApi, HaError, HaUnavailable, Subscription

log = logging.getLogger(__name__)

# §16.3 journal DDL verbatim, plus the script-installation record (§16.7).
# The schema 21 → 22 migration executes this constant.
ALERT_SCHEMA_SQL = """\
CREATE TABLE alert_ops (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id TEXT NOT NULL UNIQUE,
    endpoint_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN
      ('create','update','dismiss','snooze','expire','cancel')),
    schedule_id TEXT NOT NULL,
    occurrence_key TEXT,
    source TEXT NOT NULL CHECK (source IN ('voice','llm','device','dashboard','engine')),
    payload_json TEXT NOT NULL,
    payload_sha256 TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','applied','rejected')),
    step INTEGER NOT NULL DEFAULT 0,
    result_json TEXT,
    created_ms INTEGER NOT NULL,
    applied_ms INTEGER,
    CHECK ((state = 'applied') = (applied_ms IS NOT NULL)),
    CHECK (action NOT IN ('dismiss','snooze','expire') OR occurrence_key IS NOT NULL)
);
CREATE INDEX alert_ops_pending ON alert_ops(endpoint_id, state, seq);
CREATE INDEX alert_ops_terminal ON alert_ops(schedule_id, occurrence_key);
CREATE TABLE alert_delivery (
    endpoint_id TEXT PRIMARY KEY,
    calendar_entry_id TEXT NOT NULL,
    calendar_entity TEXT NOT NULL,
    delivery_epoch TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    acked_sequence INTEGER NOT NULL,
    CHECK (acked_sequence <= sequence)
);
CREATE TABLE alert_scripts (
    object_id TEXT PRIMARY KEY,
    sha256 TEXT NOT NULL,
    revision INTEGER NOT NULL
);
"""

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
TIMER_FINISH_GRACE_MS = 5_000               # §10.8 timer_finish_missed
TIMER_RING_SECONDS = 900                    # §10.8 default timerRingSeconds
TIMER_RING_GAP_SECONDS = 2.0                # default timerRingGapSeconds

TERMINAL_ACTIONS = ("dismiss", "snooze", "expire", "cancel")
_TOMBSTONE_FOR = {"dismiss": "dismissed", "snooze": "snoozed", "expire": "expired",
                  "cancel": "deleted"}
_JOURNAL_SOURCES = {"voice", "llm", "dashboard"}   # other device sources journal as 'device'
_TIMER_EVENTS = ("started", "updated", "cancelled", "finished")

# send(device_id, msg_type, body, generation=0) -> the envelope message_id (or None).
Send = Callable[..., Awaitable[str | None]]
# (endpoint_id, "alarm" | "timer") -> asset sha256 or "builtin:fallback"; None when
# the configured catalog ID no longer resolves.
SoundResolver = Callable[[str, str], str | None]
Notify = Callable[[str, str, dict], None]


class OpConflict(Exception):
    """An op_id was reused with a different payload (§16.3 `op_id_conflict`)."""


@dataclass(frozen=True)
class Speaker:
    """An endpoint as the LLM relay names it (§16.7 speaker resolution)."""
    endpoint_id: str
    name: str
    area: str | None = None


@dataclass
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

    def as_dict(self, now_ms: int) -> dict:
        return {"id": self.timer_id, "name": self.name, "total_seconds": self.total_seconds,
                "remaining_seconds": round(self.remaining(now_ms)), "active": self.active}


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
    worker: asyncio.Task | None = None
    # Delivery.
    online: bool = False
    delivered: dict[str, tuple[dict, int]] = field(default_factory=dict)
    delivered_valid: bool = False   # `delivered` is known to equal the device cache
    schedule_rev: dict[str, int] = field(default_factory=dict)
    ack_event: asyncio.Event = field(default_factory=asyncio.Event)
    delivery_lock: asyncio.Lock = field(default_factory=asyncio.Lock)
    # Device state and acts.
    alert_state: dict | None = None
    act_by_message: dict[str, str] = field(default_factory=dict)
    act_by_target: dict[str, str] = field(default_factory=dict)
    act_info: dict[str, dict] = field(default_factory=dict)
    timer_results: dict[str, dict] = field(default_factory=dict)
    timers: dict[str, TimerCopy] = field(default_factory=dict)
    flags: set[str] = field(default_factory=set)
    unresolved: dict[str, dict] = field(default_factory=dict)   # device ops awaiting HA


class _Journal:
    """`alert_ops` / `alert_delivery` / `alert_scripts` access. Every call is one
    short transaction under the shared connection lock."""

    def __init__(self, db: sqlite3.Connection, lock: ContextManager):
        self.db, self.lock = db, lock

    def _cur(self) -> sqlite3.Cursor:
        cur = self.db.cursor()
        cur.row_factory = sqlite3.Row
        return cur

    def query(self, sql: str, params: tuple = ()) -> list[sqlite3.Row]:
        with self.lock:
            return self._cur().execute(sql, params).fetchall()

    def write(self, sql: str, params: tuple = ()) -> None:
        with self.lock, self.db:
            self.db.execute(sql, params)

    def get(self, op_id: str) -> sqlite3.Row | None:
        rows = self.query("SELECT * FROM alert_ops WHERE op_id=?", (op_id,))
        return rows[0] if rows else None

    def insert(self, *, op_id: str, endpoint_id: str, calendar: str, action: str,
               schedule_id: str, key: str | None, source: str, payload: dict,
               digest: str, now_ms: int) -> sqlite3.Row:
        """Insert a pending row, or return the existing row for a same-hash duplicate."""
        with self.lock, self.db:
            cur = self._cur()
            old = cur.execute("SELECT * FROM alert_ops WHERE op_id=?", (op_id,)).fetchone()
            if old is not None:
                if old["payload_sha256"] != digest:
                    raise OpConflict(op_id)
                return old
            cur.execute(
                "INSERT INTO alert_ops(op_id,endpoint_id,calendar_entity,action,schedule_id,"
                "occurrence_key,source,payload_json,payload_sha256,state,created_ms) "
                "VALUES (?,?,?,?,?,?,?,?,?,'pending',?)",
                (op_id, endpoint_id, calendar, action, schedule_id, key, source,
                 canonical_json(payload), digest, now_ms))
            return cur.execute("SELECT * FROM alert_ops WHERE op_id=?", (op_id,)).fetchone()

    def advance(self, op_id: str, step: int) -> None:
        self.write("UPDATE alert_ops SET step=? WHERE op_id=?", (step, op_id))

    def finish(self, op_id: str, applied: bool, result: dict, now_ms: int) -> None:
        self.write("UPDATE alert_ops SET state=?, result_json=?, applied_ms=? WHERE op_id=?",
                   ("applied" if applied else "rejected", canonical_json(result),
                    now_ms if applied else None, op_id))

    def next_pending(self, endpoint_id: str) -> sqlite3.Row | None:
        rows = self.query("SELECT * FROM alert_ops WHERE endpoint_id=? AND state='pending' "
                          "ORDER BY seq LIMIT 1", (endpoint_id,))
        return rows[0] if rows else None

    def terminal(self, endpoint_id: str) -> list[sqlite3.Row]:
        return self.query(
            "SELECT action, schedule_id, occurrence_key FROM alert_ops WHERE endpoint_id=? "
            "AND state='applied' AND action IN ('dismiss','snooze','expire','cancel')",
            (endpoint_id,))

    def last_terminal(self, schedule_id: str, key: str) -> str | None:
        rows = self.query(
            "SELECT action FROM alert_ops WHERE schedule_id=? AND occurrence_key=? "
            "AND state='applied' ORDER BY seq DESC LIMIT 1", (schedule_id, key))
        return rows[0]["action"] if rows else None

    def schedule_keys(self, schedule_id: str) -> list[str]:
        return [r["occurrence_key"] for r in self.query(
            "SELECT DISTINCT occurrence_key FROM alert_ops WHERE schedule_id=? "
            "AND occurrence_key IS NOT NULL", (schedule_id,))]

    def pending_creates(self, endpoint_id: str) -> set[str]:
        return {r["schedule_id"] for r in self.query(
            "SELECT schedule_id FROM alert_ops WHERE endpoint_id=? AND state='pending' "
            "AND action='create'", (endpoint_id,))}

    def prune(self, before_ms: int) -> None:
        self.write("DELETE FROM alert_ops WHERE state!='pending' AND created_ms<?", (before_ms,))


class AlertEngine:
    """One engine per controller. All methods run on the asyncio thread."""

    def __init__(
        self,
        db: sqlite3.Connection,
        ha: HaApi,
        send: Send,
        *,
        sound_resolver: SoundResolver,
        config_resolver: Callable[[str], dict],
        speakers: Callable[[], Iterable[Speaker]],
        awaiting_intent: Callable[[], Iterable[str]],
        notify: Notify | None = None,
        now_ms: Callable[[], int] | None = None,
        db_lock: ContextManager | threading.Lock | None = None,
    ):
        self.ha, self.send = ha, send
        self.sound_resolver = sound_resolver
        self.config_resolver = config_resolver
        self.speakers = speakers
        self.awaiting_intent = awaiting_intent
        self.notify = notify or (lambda _endpoint, _code, _data: None)
        self.now_ms = now_ms or (lambda: time.time_ns() // 1_000_000)
        self.journal = _Journal(db, db_lock if db_lock is not None else nullcontext())
        self._eps: dict[str, _Endpoint] = {}
        self._futures: dict[str, asyncio.Future] = {}
        self._tasks: set[asyncio.Task] = set()
        self._relay: Subscription | None = None
        self._tz: tzinfo = timezone.utc
        self._connect_lock = asyncio.Lock()
        self._closed = False
        for r in self.journal.query("SELECT * FROM alert_delivery"):
            self._eps[r["endpoint_id"]] = _Endpoint(
                r["endpoint_id"], r["calendar_entry_id"], r["calendar_entity"],
                r["delivery_epoch"], r["sequence"], r["acked_sequence"])

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
        self.journal.write("INSERT INTO alert_delivery VALUES (?,?,?,?,0,0)",
                           (endpoint_id, entry_id, calendar, epoch))
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
                    self._relay = await self.ha.subscribe_events(EVENT_REQUEST, self._on_relay_event)
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

    def _occurrences(self, ep: _Endpoint, events: Iterable[dict]) -> dict[str, Occurrence]:
        sound = self._sound(ep, "alarm")
        out = {}
        for event in events:
            occ = occurrence_from_event(ep.calendar, event, sound)
            if occ is not None:
                out[occ.occurrence_id] = occ
        return out

    def _on_push(self, ep: _Endpoint, token: str, events: list[dict]) -> None:
        """Handle one full-window push (§10.4): diff, guard, cap, deliver."""
        if token != ep.token or self._closed:
            return
        pushed = self._occurrences(ep, events)
        ep.pushed = pushed
        ep.seen.update(pushed)
        self._set_flag(ep, "malformed_event", any(o.malformed for o in pushed.values()))
        live, deletes = self._guard(ep, pushed)
        ordered = sorted(live.values(), key=lambda o: (o.due_utc_ms, o.occurrence_id))
        self._set_flag(ep, "occurrence_capacity_exceeded", len(ordered) > MAX_OCCURRENCES)
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
        cancelled = {r["schedule_id"] for r in rows if r["action"] == "cancel"}
        handled = {(r["schedule_id"], r["occurrence_key"]) for r in rows if r["action"] != "cancel"}
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
                    self.notify(ep.endpoint_id, "restore_guard", {"uid": uid, "recurrence_id": rid})
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
            request = {"action": "expire", "occurrence_id": occ.occurrence_id,
                       "schedule_id": occ.schedule_id, "reason": "missed"}
            self._enqueue(ep, op_id, "expire", occ.schedule_id, occ.key, "engine", request,
                          {"result": self._occurrence_result(occ, reason="missed")})

    # ── the journal worker (§16.4 "Applying one operation") ──────────────

    def _spawn(self, coro: Awaitable) -> asyncio.Task:
        task = asyncio.get_running_loop().create_task(coro)
        self._tasks.add(task)
        task.add_done_callback(self._tasks.discard)
        return task

    def _kick(self, ep: _Endpoint) -> None:
        if ep.worker is None or ep.worker.done():
            ep.worker = self._spawn(self._drain(ep))

    def _enqueue(self, ep: _Endpoint, op_id: str, action: str, schedule_id: str,
                 key: str | None, source: str, request: dict, payload: dict) -> sqlite3.Row:
        """Journal an operation as `pending`. `payload_sha256` hashes the request
        (the caller-visible arguments), so a retry that recomputes derived fields
        is still a duplicate. Raises OpConflict."""
        row = self.journal.insert(
            op_id=op_id, endpoint_id=ep.endpoint_id, calendar=ep.calendar, action=action,
            schedule_id=schedule_id, key=key, source=source,
            payload={**payload, "request": request}, digest=sha256_hex(request),
            now_ms=self.now_ms())
        if row["state"] == "pending":
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

    async def _apply(self, ep: _Endpoint, row: sqlite3.Row) -> None:
        op_id, action = row["op_id"], row["action"]
        sid, key = row["schedule_id"], row["occurrence_key"]
        payload = json.loads(row["payload_json"])
        if action == "create":
            start = datetime.fromisoformat(payload["event"]["dtstart"])
            if ep.window_end is not None and start >= ep.window_end:
                # A series whose first day falls past the live window (§16.3 DST edge).
                if await self._rest_by_op(ep, op_id, start) is None:
                    await self.ha.calendar_create(ep.calendar, payload["event"])
                    self.journal.advance(op_id, 1)
                if await self._rest_by_op(ep, op_id, start) is None:
                    return self._reject(ep, row, "not_visible")
            else:
                if self._by_op(ep, op_id) is None:
                    await self.ha.calendar_create(ep.calendar, payload["event"])
                    self.journal.advance(op_id, 1)
                if not await self._visible(ep, lambda: self._by_op(ep, op_id) is not None):
                    return self._reject(ep, row, "not_visible")
        elif action in ("dismiss", "expire"):
            if not await self._delete_occurrence(ep, sid, key, op_id, step=1):
                return self._reject(ep, row, "not_visible")
        elif action == "cancel":
            await self.ha.calendar_delete(ep.calendar, payload["uid"], None)
            self.journal.advance(op_id, 1)
            if not await self._visible(ep, lambda: not any(
                    o.schedule_id == sid for o in ep.pushed.values())):
                return self._reject(ep, row, "not_visible")
        elif action == "snooze":
            if self._by_op(ep, op_id) is None:
                if row["step"] == 0 and not await self._schedule_exists(ep, sid, key):
                    return self._reject(ep, row, "parent_schedule_deleted")
                await self.ha.calendar_create(ep.calendar, payload["event"])
                self.journal.advance(op_id, 1)
            if not await self._visible(ep, lambda: self._by_op(ep, op_id) is not None):
                return self._reject(ep, row, "not_visible")
            if not await self._delete_occurrence(ep, sid, key, op_id, step=2):
                return self._reject(ep, row, "not_visible")
            device_child = payload.get("device_child_occurrence_id")
            if device_child and device_child != payload["child_occurrence_id"]:
                await self._send_objects(ep, [(device_child, None, "deleted")])
        else:
            return self._reject(ep, row, f"unsupported action {action}")
        self._finish(op_id, True, {**payload["result"], "ok": True, "op_id": op_id,
                                   "stored_in_ha": True})

    def _reject(self, ep: _Endpoint, row: sqlite3.Row, error: str) -> None:
        op_id, action = row["op_id"], row["action"]
        self._finish(op_id, False, {"ok": False, "op_id": op_id, "error": error,
                                    "stored_in_ha": False})
        self.notify(ep.endpoint_id, "operation_rejected",
                    {"op_id": op_id, "action": action, "error": error})
        log.warning("alerts: %s %s rejected: %s", action, op_id, error)
        if action == "snooze":
            child = json.loads(row["payload_json"]).get("device_child_occurrence_id")
            if child:
                self._spawn(self._send_objects(ep, [(child, None, "deleted")]))

    def _finish(self, op_id: str, applied: bool, result: dict) -> None:
        self.journal.finish(op_id, applied, result, self.now_ms())
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
            objects = []
            for occ in changed:
                body = occ.cache_body()
                ep.delivered[occ.occurrence_id] = (body, ep.sequence)
                ep.schedule_rev[occ.schedule_id] = ep.sequence
                objects.append({**body, "revision": ep.sequence})
            for oid in removed:
                body, _ = ep.delivered.pop(oid)
                ep.schedule_rev[body["schedule_id"]] = ep.sequence
                objects.append(tombstone_object(oid, ep.sequence, self._tombstone_reason(ep, oid)))
            self._persist_delivery(ep)
            await self._send_device(ep, "alert.delta", delta_body(ep.epoch, ep.sequence, objects))

    def _diff(self, ep: _Endpoint) -> tuple[list[Occurrence], list[str]]:
        changed = [o for oid, o in ep.live.items()
                   if oid not in ep.delivered or ep.delivered[oid][0] != o.cache_body()]
        removed = sorted(set(ep.delivered) - set(ep.live))
        return changed, removed

    def _tombstone_reason(self, ep: _Endpoint, oid: str) -> str:
        occ = ep.seen.get(oid)
        action = self.journal.last_terminal(occ.schedule_id, occ.key) if occ else None
        return _TOMBSTONE_FOR.get(action, "deleted")

    async def _snapshot(self, ep: _Endpoint) -> None:
        ep.sequence += 1
        delivered: dict[str, tuple[dict, int]] = {}
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
        objects = [{**body, "revision": rev} for body, rev in delivered.values()]
        for page in snapshot_pages(ep.epoch, ep.sequence, objects):
            if not await self._send_device(ep, "alert.snapshot", page):
                return

    async def _send_objects(self, ep: _Endpoint, tombstones: list[tuple[str, str | None, str]]) -> None:
        """Deliver tombstones for occurrences the controller never delivered
        (a rejected or re-identified snooze child)."""
        async with ep.delivery_lock:
            if not ep.online or not ep.delivered_valid:
                return
            ep.sequence += 1
            self._persist_delivery(ep)
            objects = [tombstone_object(oid, ep.sequence, reason) for oid, _sid, reason in tombstones]
            await self._send_device(ep, "alert.delta", delta_body(ep.epoch, ep.sequence, objects))

    async def _send_device(self, ep: _Endpoint, msg_type: str, body: dict) -> bool:
        try:
            await self.send(ep.endpoint_id, msg_type, body, generation=0)
            return True
        except Exception as err:  # the session layer owns transport errors
            log.warning("alerts: %s to %s failed: %s", msg_type, ep.endpoint_id, err)
            ep.delivered_valid = False
            return False

    def _persist_delivery(self, ep: _Endpoint) -> None:
        self.journal.write("UPDATE alert_delivery SET sequence=?, acked_sequence=? "
                           "WHERE endpoint_id=?", (ep.sequence, ep.acked, ep.endpoint_id))

    def _armed(self, ep: _Endpoint, schedule_ids: Iterable[str]) -> bool:
        sids = list(schedule_ids)
        return (ep.online and ep.delivered_valid and bool(sids)
                and all(sid in ep.schedule_rev and ep.acked >= ep.schedule_rev[sid] for sid in sids))

    async def _facts(self, ep: _Endpoint, result: dict, schedule_ids: Iterable[str],
                     deadline: float) -> dict:
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

    async def on_session_hello(self, endpoint_id: str, alerts: dict) -> None:
        """`session.hello.alerts`: snapshot on an epoch/sequence mismatch."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        ep.online = True
        dev_epoch, dev_acked = alerts.get("delivery_epoch"), alerts.get("acked_sequence")
        if dev_epoch == ep.epoch and isinstance(dev_acked, int) and 0 <= dev_acked <= ep.sequence:
            ep.acked = dev_acked
            self._persist_delivery(ep)
        if dev_epoch != ep.epoch or dev_acked != ep.sequence:
            ep.delivered_valid = False
        if ep.materialized:
            await self._deliver(ep)

    def on_session_lost(self, endpoint_id: str) -> None:
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        ep.online = False
        if ep.alert_state is not None:
            ep.alert_state = None
            self.notify(endpoint_id, "alert_ringing", {"ringing": None})

    async def on_alert_ack(self, endpoint_id: str, body: dict) -> None:
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

    def on_alert_state(self, endpoint_id: str, body: dict) -> None:
        """`alert.state`: drives the “Alert ringing” entity."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        was = bool(ep.alert_state.get("active")) if ep.alert_state is not None else None
        ep.alert_state = body
        now = bool(body.get("active"))
        if was is not now:
            self.notify(endpoint_id, "alert_ringing", {"ringing": now})

    def on_alert_ring_ended(self, endpoint_id: str, body: dict) -> None:
        """Telemetry; completes a pending timer `alert.act` for that ring."""
        ep = self._eps.get(endpoint_id)
        if ep is None or body.get("kind") != "timer":
            return
        op_id = ep.act_by_target.pop(str(body.get("id")), None)
        if op_id:
            self._complete_timer_act(ep, op_id, {"ok": True})

    def on_command_ack(self, endpoint_id: str, body: dict) -> None:
        """`command.ack` for an `alert.act` this engine sent."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        op_id = ep.act_by_message.get(body.get("message_id"))
        if op_id is None:
            return
        status = body.get("status")
        if status == "rejected":
            ep.act_by_message.pop(body.get("message_id"), None)
            result = {"ok": False, "op_id": op_id, "error": body.get("error") or "rejected"}
            if ep.act_info.get(op_id, {}).get("kind") == "timer":
                self._complete_timer_act(ep, op_id, result)
            else:
                future = self._futures.pop(op_id, None)
                if future is not None and not future.done():
                    future.set_result(result)
        elif status == "applied" and ep.act_info.get(op_id, {}).get("kind") == "timer":
            ep.act_by_message.pop(body.get("message_id"), None)
            self._complete_timer_act(ep, op_id, {"ok": True})

    def _complete_timer_act(self, ep: _Endpoint, op_id: str, outcome: dict) -> None:
        info = ep.act_info.pop(op_id, {})
        ep.act_by_target.pop(info.get("id", ""), None)
        result = {**outcome, "op_id": op_id, "kind": "timer", "name": info.get("name", "")}
        ep.timer_results[op_id] = result
        future = self._futures.pop(op_id, None)
        if future is not None and not future.done():
            future.set_result(result)

    async def on_local_operation(self, endpoint_id: str, body: dict) -> None:
        """`alert.local_operation` (§10.6 merge table); answers `alert.op_result`
        once the operation is applied or rejected."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return
        op_id = body.get("op_id")
        try:
            op_id = str(uuid.UUID(str(op_id)))
            action, oid, sid = body["action"], body["occurrence_id"], body["schedule_id"]
            if action not in ("dismiss", "snooze", "expire"):
                raise ValueError("invalid_action")
            if not isinstance(oid, str) or not isinstance(sid, str):
                raise ValueError("invalid_occurrence")
        except (KeyError, ValueError) as err:
            await self._early_result(ep, str(op_id), False, str(err))
            return
        request = {k: body.get(k) for k in ("action", "occurrence_id", "schedule_id", "reason", "child")}
        source = body.get("source") if body.get("source") in _JOURNAL_SOURCES else "device"
        existing = self.journal.get(op_id)
        if existing is not None:
            if existing["payload_sha256"] != sha256_hex(request):
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
            if action == "snooze":
                await self._early_result(ep, op_id, False, "parent_occurrence_unknown")
                child = body.get("child") or {}
                if isinstance(child.get("occurrence_id"), str):
                    await self._send_objects(ep, [(child["occurrence_id"], None, "deleted")])
            else:
                # Absent from HA and never observed: nothing to write, and nothing
                # a restore could bring back.
                await self._early_result(ep, op_id, True, None)
            return
        try:
            if action == "snooze":
                payload = self._snooze_payload(occ, op_id, body.get("child"))
            else:
                reason = body.get("reason") if action == "expire" else None
                if action == "expire" and reason not in ("timed_out", "missed"):
                    raise ValueError("invalid_reason")
                payload = {"result": self._occurrence_result(occ, reason=reason)}
            self._enqueue(ep, op_id, action, occ.schedule_id, occ.key, source, request, payload)
        except ValueError as err:
            await self._early_result(ep, op_id, False, str(err))
            return
        except OpConflict:
            await self._early_result(ep, op_id, False, "op_id_conflict")
            return
        self._spawn(self._answer_when_done(ep, op_id))

    async def _early_result(self, ep: _Endpoint, op_id: str, applied: bool, error: str | None) -> None:
        """Answer a device operation that needs no journal row."""
        future = self._futures.pop(op_id, None)
        if future is not None and not future.done():
            future.set_result({"ok": applied, "op_id": op_id, "error": error, "stored_in_ha": applied})
        await self._op_result(ep, op_id, applied, error)

    async def _answer_when_done(self, ep: _Endpoint, op_id: str) -> None:
        result = await self._result(op_id, None)
        await self._op_result(ep, op_id, bool(result.get("ok")), result.get("error"))

    async def _op_result(self, ep: _Endpoint, op_id: str, applied: bool, error: str | None) -> None:
        if ep.online:
            await self._send_device(ep, "alert.op_result", {
                "op_id": op_id, "state": "applied" if applied else "rejected", "error": error})

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
            settings=sibling.settings if sibling else RingSettings(sound=self._sound(ep, "alarm")))

    def _snooze_payload(self, parent: Occurrence, op_id: str, child: Any) -> dict:
        """The child event the device derived (§10.7, §16.3): dtstart from the
        device's frozen `due_utc_ms`, written in HA's timezone, carrying the
        device's op_id."""
        if not isinstance(child, dict):
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
        line = EchomuseLine(child_sid, KIND_SNOOZE, op_id,
                            ParentRef(parent.schedule_id, parent.key), parent.settings)
        label = snooze_label(parent.summary)
        return {
            "event": alarm_event(label, start, None, build_description(line)),
            "child_occurrence_id": child_oid,
            "device_child_occurrence_id": child.get("occurrence_id"),
            "result": {**self._occurrence_result(parent), "child": {
                "schedule_id": child_sid, "occurrence_id": child_oid, "name": label,
                "due": iso_seconds(start)}},
        }

    @staticmethod
    def _occurrence_result(occ: Occurrence, reason: str | None = None) -> dict:
        result = {"kind": occ.kind, "name": occ.label, "schedule_id": occ.schedule_id,
                  "occurrence_id": occ.occurrence_id, "due": occ.due_local}
        if reason:
            result["reason"] = reason
        return result

    # ── operation results ─────────────────────────────────────────────────

    async def _result(self, op_id: str, timeout: float | None) -> dict | None:
        """The finished result of `op_id`, or None if still pending at timeout."""
        row = self.journal.get(op_id)
        if row is not None and row["state"] != "pending":
            return json.loads(row["result_json"])
        future = self._futures.get(op_id)
        if future is None or future.done():
            future = self._futures[op_id] = asyncio.get_running_loop().create_future()
        try:
            return await asyncio.wait_for(asyncio.shield(future), timeout)
        except asyncio.TimeoutError:
            return None

    @staticmethod
    def _pending(op_id: str) -> dict:
        return {"ok": True, "stored_in_ha": False, "pending": True, "op_id": op_id}

    # ── voice / LLM / dashboard operations (§10.5, §16.7) ────────────────

    async def set_alarm(self, endpoint_id: str, time: str, days: Iterable[str] = (),
                        name: str = "", *, on_date: date | None = None,
                        source: str = "voice", op_id: str | None = None) -> dict:
        """Create an alarm event (§10.3 Create). `time` is `HH:MM[:SS]` wall time
        in HA's timezone; `days` weekday names (empty = once); `on_date` pins a
        one-shot to a date. Answers when applied or after RESULT_SECONDS."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return {"ok": False, "error": "speaker has no alarm calendar"}
        try:
            at, norm_days = parse_clock(time), normalize_days(days)
        except ValueError as err:
            return {"ok": False, "error": str(err)}
        if on_date is not None and norm_days:
            return {"ok": False, "error": "a dated alarm cannot repeat"}
        label = (name or "").strip() or DEFAULT_LABEL
        if len(label) > LABEL_MAX_CHARS:
            return {"ok": False, "error": f"name is longer than {LABEL_MAX_CHARS} characters"}
        op_id = op_id or str(uuid.uuid4())
        request = {"action": "set_alarm", "endpoint_id": endpoint_id, "time": at.isoformat(),
                   "days": list(norm_days), "name": label,
                   "date": on_date.isoformat() if on_date else None}
        existing = self.journal.get(op_id)
        if existing is None:
            if not self.ha.connected:
                return {"ok": False, "error": "Home Assistant is unreachable", "stored_in_ha": False}
            echomuse = {o.schedule_id for o in ep.pushed.values() if o.is_echomuse}
            if len(echomuse | self.journal.pending_creates(endpoint_id)) >= MAX_SCHEDULES:
                return {"ok": False, "error": "capacity_exceeded"}
            try:
                tz = ZoneInfo(await self.ha.time_zone())
            except HaUnavailable:
                return {"ok": False, "error": "Home Assistant is unreachable", "stored_in_ha": False}
            if on_date is not None:
                start = alarm_start_on(on_date, at, tz)
                if start <= self._now():
                    return {"ok": False, "error": "that time has already passed"}
            else:
                start = next_alarm_start(self._now(), tz, at, norm_days)
            schedule_id = str(uuid.uuid5(uuid.UUID(op_id), "schedule"))
            sound = self._sound(ep, "alarm")
            line = EchomuseLine(schedule_id, KIND_ALARM, op_id, None, RingSettings(sound=sound))
            event = alarm_event(label, start, rrule_for_days(norm_days), build_description(line))
            result = {"kind": KIND_ALARM, "name": label, "schedule_id": schedule_id,
                      "first_due": iso_seconds(start), "days": list(norm_days)}
            payload = {"event": event, "result": result}
        else:
            schedule_id, payload = existing["schedule_id"], None
        try:
            if payload is not None:
                self._enqueue(ep, op_id, "create", schedule_id, None, source, request, payload)
            elif existing["payload_sha256"] != sha256_hex(request):
                raise OpConflict(op_id)
        except OpConflict:
            return {"ok": False, "error": "op_id_conflict"}
        result = await self._result(op_id, max(0.0, deadline - asyncio.get_running_loop().time()))
        if result is None:
            return self._pending(op_id)
        if not result.get("ok"):
            return result
        return await self._facts(ep, result, [schedule_id], deadline)

    def list_alarms(self, endpoint_id: str) -> dict:
        """The next ≤10 alarm occurrences on the speaker."""
        ep = self._eps.get(endpoint_id)
        if ep is None or not ep.materialized:
            return {"ok": False, "error": "alarm calendar is unavailable"}
        now = self.now_ms()
        upcoming = [o for o in ep.live.values() if o.due_utc_ms >= now][:MAX_LISTED]
        return {"ok": True, "alarms": [{
            "name": o.label, "kind": o.kind, "due": o.due_local,
            "repeats": list(days_from_rrule(o.rrule) or ()) if o.rrule else [],
            "schedule_id": o.schedule_id, "occurrence_id": o.occurrence_id,
            "armed_on_endpoint": self._armed(ep, [o.schedule_id]),
        } for o in upcoming]}

    async def cancel_alarm(self, endpoint_id: str, *, name: str = "", time: str = "",
                           all_alarms: bool = False, source: str = "voice",
                           op_id: str | None = None) -> dict:
        """Delete matching schedules, whole series (§10.3 Cancel)."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return {"ok": False, "error": "speaker has no alarm calendar"}
        name = (name or "").strip()
        if not (name or time or all_alarms):
            return {"ok": False, "error": "name, time, or all: true is required"}
        try:
            at = parse_clock(time) if time else None
        except ValueError as err:
            return {"ok": False, "error": str(err)}
        root = op_id or str(uuid.uuid4())
        request = {"action": "cancel_alarm", "endpoint_id": endpoint_id, "name": name,
                   "time": at.isoformat() if at else None, "all": bool(all_alarms)}
        existing = self.journal.get(root)
        if existing is not None:
            if existing["payload_sha256"] != sha256_hex(request):
                return {"ok": False, "error": "op_id_conflict"}
            group = json.loads(existing["payload_json"])["group"]
        else:
            if not self.ha.connected:
                return {"ok": False, "error": "Home Assistant is unreachable", "stored_in_ha": False}
            targets: dict[str, Occurrence] = {}
            for occ in ep.live.values():
                if name and occ.label.casefold() != name.casefold():
                    continue
                if at and occ.start.time() != at:
                    continue
                targets.setdefault(occ.schedule_id, occ)
            if not targets:
                return {"ok": False, "error": "no matching alarm"}
            sids = list(targets)
            group = [root] + [str(uuid.uuid5(uuid.UUID(root), sid)) for sid in sids[1:]]
            for op, sid in zip(group, sids):
                occ = targets[sid]
                payload = {"uid": occ.uid, "group": group,
                           "result": {"kind": occ.kind, "name": occ.label, "schedule_id": sid}}
                req = request if op == root else {**request, "root": root, "schedule_id": sid}
                self._enqueue(ep, op, "cancel", sid, None, source, req, payload)
        results = []
        for op in group:
            remaining = max(0.0, deadline - asyncio.get_running_loop().time())
            result = await self._result(op, remaining)
            if result is None:
                return self._pending(root)
            results.append(result)
        done = [r for r in results if r.get("ok")]
        combined = {"ok": bool(done), "op_id": root,
                    "cancelled": [{"name": r["name"], "kind": r["kind"]} for r in done],
                    "stored_in_ha": len(done) == len(results)}
        if len(done) < len(results):
            combined["errors"] = [r.get("error") for r in results if not r.get("ok")]
        return await self._facts(ep, combined, [r["schedule_id"] for r in done], deadline)

    async def dismiss_alert(self, endpoint_id: str, *, source: str = "voice",
                            op_id: str | None = None) -> dict:
        """Stop whatever rings on the speaker (timer or alarm); a repeating alarm
        loses only this occurrence."""
        return await self._act(endpoint_id, "dismiss", source, op_id)

    async def snooze_alarm(self, endpoint_id: str, *, source: str = "voice",
                           op_id: str | None = None) -> dict:
        """Snooze the ringing alarm; timers are rejected."""
        return await self._act(endpoint_id, "snooze", source, op_id)

    async def _act(self, endpoint_id: str, action: str, source: str, op_id: str | None) -> dict:
        """`alert.act` to the device; an alarm completes through the device's
        journaled local operation with the same op_id (§6.3, WIRE §4.7)."""
        deadline = asyncio.get_running_loop().time() + RESULT_SECONDS
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return {"ok": False, "error": "unknown speaker"}
        op_id = op_id or str(uuid.uuid4())
        if op_id in ep.timer_results:
            return ep.timer_results[op_id]
        if self.journal.get(op_id) is not None:
            return await self._finish_act(ep, op_id, deadline)
        active = (ep.alert_state or {}).get("active") if ep.online else None
        if not ep.online:
            return {"ok": False, "error": "speaker is offline"}
        if not active:
            return {"ok": False, "error": "nothing is ringing"}
        if action == "snooze" and active.get("kind") != "alarm":
            return {"ok": False, "error": "timers cannot be snoozed"}
        kind = active.get("kind")
        ep.act_info[op_id] = {"id": str(active.get("id")), "kind": kind, "name": active.get("name", "")}
        if kind == "timer":
            ep.act_by_target[str(active.get("id"))] = op_id
        future = self._futures.get(op_id)
        if future is None or future.done():
            self._futures[op_id] = asyncio.get_running_loop().create_future()
        message_id = await self.send(endpoint_id, "alert.act", {
            "op_id": op_id, "target_id": str(active.get("id")), "action": action,
            "source": source}, generation=0)
        if message_id:
            ep.act_by_message[message_id] = op_id
        return await self._finish_act(ep, op_id, deadline)

    async def _finish_act(self, ep: _Endpoint, op_id: str, deadline: float) -> dict:
        result = await self._result(op_id, max(0.0, deadline - asyncio.get_running_loop().time()))
        if result is None:
            return self._pending(op_id)
        if result.get("kind") == "timer" or not result.get("ok"):
            return result
        sids = [result["schedule_id"]]
        if "child" in result:
            sids.append(result["child"]["schedule_id"])
        return await self._facts(ep, result, sids, deadline)

    # ── LLM relay (§16.7) ─────────────────────────────────────────────────

    def resolve_speaker(self, speaker: str | None) -> str | None:
        """Speaker field → endpoint name, then area; empty → the single turn
        awaiting an intent run; otherwise None ("which speaker?")."""
        query = (speaker or "").strip().casefold()
        if query:
            speakers = list(self.speakers())
            for field_of in (lambda s: s.name, lambda s: s.area):
                hits = {s.endpoint_id for s in speakers if (field_of(s) or "").casefold() == query}
                if hits:
                    return hits.pop() if len(hits) == 1 else None
            return None
        waiting = set(self.awaiting_intent())
        return waiting.pop() if len(waiting) == 1 else None

    def _on_relay_event(self, data: dict) -> None:
        self._spawn(self._relay_request(data))

    async def _relay_request(self, data: dict) -> None:
        request_id = data.get("request_id")
        if not isinstance(request_id, str) or not request_id:
            return
        result = await self.handle_request(data.get("action"), data.get("args"), request_id)
        try:
            await self.ha.fire_event(EVENT_RESULT, {"request_id": request_id, "result": result})
        except (HaUnavailable, HaError) as err:
            log.warning("alerts: could not answer %s: %s", request_id, err)

    async def handle_request(self, action: Any, args: Any, request_id: str) -> dict:
        """One `echomuse_alert_request`: validate, resolve the speaker, apply with
        op_id = UUIDv5(NAMESPACE_URL, request_id)."""
        if not isinstance(args, dict):
            return {"ok": False, "error": "args must be an object"}
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, request_id))

        def text(key: str) -> str:
            value = args.get(key)
            return value if isinstance(value, str) else ""

        if action == "set_alarm":
            if not text("time"):
                return {"ok": False, "error": "time is required"}
            days = args.get("days") or []
            if not isinstance(days, list):
                return {"ok": False, "error": "days must be a list of weekdays"}
            try:
                parse_clock(text("time"))
                normalize_days(days)
            except ValueError as err:
                return {"ok": False, "error": str(err)}
        elif action == "cancel_alarm":
            if not (text("name").strip() or text("time") or _truthy(args.get("all"))):
                return {"ok": False, "error": "name, time, or all: true is required"}
            if text("time"):
                try:
                    parse_clock(text("time"))
                except ValueError as err:
                    return {"ok": False, "error": str(err)}
        elif action not in ("list_alarms", "dismiss_alert", "snooze_alarm"):
            return {"ok": False, "error": "unknown action"}
        endpoint = self.resolve_speaker(text("speaker"))
        if endpoint is None:
            return {"ok": False, "error": "which speaker?"}
        if action == "set_alarm":
            return await self.set_alarm(endpoint, text("time"), args.get("days") or [],
                                        text("name"), source="llm", op_id=op_id)
        if action == "list_alarms":
            return self.list_alarms(endpoint)
        if action == "cancel_alarm":
            return await self.cancel_alarm(endpoint, name=text("name"), time=text("time"),
                                           all_alarms=_truthy(args.get("all")),
                                           source="llm", op_id=op_id)
        if action == "dismiss_alert":
            return await self.dismiss_alert(endpoint, source="llm", op_id=op_id)
        return await self.snooze_alarm(endpoint, source="llm", op_id=op_id)

    async def install_scripts(self) -> list[str]:
        """Install/upgrade the five Assist scripts (§16.7 Installation). Returns
        warnings for user-edited scripts, which are left untouched."""
        warnings, managed, wrote = [], [], False
        for object_id, config in render_scripts().items():
            want = config_sha256(config)
            current = await self.ha.get_script_config(object_id)
            rows = self.journal.query("SELECT sha256 FROM alert_scripts WHERE object_id=?", (object_id,))
            recorded = rows[0]["sha256"] if rows else None
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
                        self._record_script(object_id, have)
                    managed.append(object_id)
                    continue
            await self.ha.put_script_config(object_id, config)
            self._record_script(object_id, want)
            managed.append(object_id)
            wrote = True
        if wrote:
            await self.ha.call_service("script", "reload")
        if managed:
            await self.ha.expose_entities([f"script.{o}" for o in managed], ["conversation"])
        return warnings

    def _record_script(self, object_id: str, digest: str) -> None:
        self.journal.write("INSERT OR REPLACE INTO alert_scripts VALUES (?,?,?)",
                           (object_id, digest, SCRIPT_REVISION))

    # ── timers (§10.8) ────────────────────────────────────────────────────

    async def on_timer_event(self, endpoint_id: str, event: str, timer_id: str, name: str,
                             total_seconds: int, seconds_left: int, is_active: bool) -> None:
        """HA's native timer event for this speaker's satellite."""
        ep = self._eps.get(endpoint_id)
        if ep is None or event not in _TIMER_EVENTS:
            return
        if event in ("started", "updated"):
            ep.timers[timer_id] = TimerCopy(timer_id, name, int(total_seconds), int(seconds_left),
                                            bool(is_active), self.now_ms())
        else:
            ep.timers.pop(timer_id, None)
        self.notify(endpoint_id, "timers", {"timers": self.timers(endpoint_id)})
        if event == "finished":
            await self._ring_timer(ep, timer_id, name)

    def timers(self, endpoint_id: str) -> list[dict]:
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
                    self.notify(ep.endpoint_id, "timer_finish_missed",
                                {"timer_id": timer.timer_id, "name": timer.name})
            self.notify(ep.endpoint_id, "timers", {"timers": self.timers(ep.endpoint_id)})

    async def _ring_timer(self, ep: _Endpoint, timer_id: str, name: str) -> None:
        config = self.config_resolver(ep.endpoint_id) or {}
        body = {
            "ring_id": timer_id, "name": name, "sound": self._sound(ep, "timer"),
            "loop_gap_ms": round(float(config.get("timerRingGapSeconds", TIMER_RING_GAP_SECONDS)) * 1000),
            "max_ring_ms": round(float(config.get("timerRingSeconds", TIMER_RING_SECONDS)) * 1000),
        }
        error = None
        if not ep.online:
            error = "speaker offline"
        else:
            try:
                await self.send(ep.endpoint_id, "alert.ring", body, generation=0)
            except Exception as err:  # the session layer owns transport errors
                error = str(err)
        if error is not None:
            log.warning("alerts: timer_ring_undeliverable %s on %s: %s", timer_id, ep.endpoint_id, error)
            self.notify(ep.endpoint_id, "timer_ring_undeliverable",
                        {"timer_id": timer_id, "name": name, "error": error})

    # ── sounds, flags, status ─────────────────────────────────────────────

    def _sound(self, ep: _Endpoint, kind: str) -> str:
        """Effective alarmSound/timerSound as a sha256; unresolvable → fallback, flagged."""
        sound = self.sound_resolver(ep.endpoint_id, kind)
        self._set_flag(ep, f"{kind}_sound_unresolved", sound is None)
        return sound or FALLBACK_SOUND

    def _set_flag(self, ep: _Endpoint, flag: str, on: bool) -> None:
        if on == (flag in ep.flags):
            return
        (ep.flags.add if on else ep.flags.discard)(flag)
        self.notify(ep.endpoint_id, "flags", {"flags": sorted(ep.flags)})

    def status(self, endpoint_id: str) -> dict | None:
        """Dashboard alert panel state for one endpoint."""
        ep = self._eps.get(endpoint_id)
        if ep is None:
            return None
        ops = [{"op_id": r["op_id"], "action": r["action"], "source": r["source"],
                "state": r["state"], "step": r["step"], "created_ms": r["created_ms"],
                "result": json.loads(r["result_json"]) if r["result_json"] else None}
               for r in self.journal.query(
                   "SELECT * FROM alert_ops WHERE endpoint_id=? AND (state='pending' OR created_ms>=?) "
                   "ORDER BY seq DESC LIMIT 50", (endpoint_id, self.now_ms() - 86_400_000))]
        return {
            "calendar_entity": ep.calendar, "calendar_entry_id": ep.entry_id,
            "delivery_epoch": ep.epoch, "sequence": ep.sequence, "acked_sequence": ep.acked,
            "online": ep.online, "calendar_live": ep.fresh, "flags": sorted(ep.flags),
            "ringing": bool((ep.alert_state or {}).get("active")) if ep.alert_state else None,
            "alert_state": ep.alert_state,
            "occurrences": [{**o.cache_body(), "armed_on_endpoint": self._armed(ep, [o.schedule_id])}
                            for o in ep.live.values()],
            "timers": self.timers(endpoint_id),
            "operations": ops,
        }


def _truthy(value: Any) -> bool:
    return value is True or (isinstance(value, str) and value.strip().casefold() == "true")





_EPOCH_UTC = datetime(1970, 1, 1, tzinfo=timezone.utc)
