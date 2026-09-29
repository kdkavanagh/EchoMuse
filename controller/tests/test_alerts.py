"""Alert engine against an in-memory stock Local Calendar (SPEC §10, §16.3–16.4, §16.7)."""

import asyncio
import copy
import hashlib
import json
import sqlite3
import uuid
from datetime import datetime, timedelta, timezone

import pytest

import em_alerts
import em_db
from em_alert_scripts import render_scripts, config_sha256
from em_alert_wire import (
    DEFAULT_LOOP_GAP_MS, DEFAULT_MAX_RING_MS, DEFAULT_RAMP_MS, DEFAULT_SNOOZE_MS,
    canonical_json, iso_seconds, occurrence_id_for, parse_description,
    snooze_due_utc_ms, snooze_schedule_id, ui_schedule_id,
)
from em_alerts import AlertEngine, Speaker
from em_device_link import CommandAck
from em_ha_client import HaUnavailable

SOUND = "b" * 64
TIMER_SOUND = "c" * 64
CAL = "calendar.echomuse_office"
NOW = datetime(2026, 9, 27, 12, 0, tzinfo=timezone.utc)      # a Sunday
_CODES = ["MO", "TU", "WE", "TH", "FR", "SA", "SU"]


class Sub:
    def __init__(self, stop):
        self._stop = stop

    async def unsubscribe(self):
        self._stop()


class FakeHa:
    """Stock Local Calendar semantics that matter here: full-window pushes,
    whole-event and single-instance delete, DAILY / WEEKLY;BYDAY expansion with
    HA-style floating `recurrence_id`s."""

    def __init__(self):
        self.connected = True
        self.zone = "UTC"
        self.cals = {}
        self.subs = {}
        self.events = {}
        self.scripts = {}
        self.exposed, self.services, self.fired, self.writes = [], [], [], []
        self.next_uid = 1
        self.suppress_push = False
        self.drop_creates = False
        self.crash = None           # (op, "before"|"after"): drop the link there

    def _maybe_crash(self, op, stage):
        if self.crash == (op, stage):
            self.crash = None
            self.connected = False
            raise HaUnavailable(f"link lost {stage} {op}")

    def _need(self):
        if not self.connected:
            raise HaUnavailable("down")

    async def time_zone(self):
        self._need()
        return self.zone

    async def ensure_local_calendar(self, title):
        entity = "calendar." + title.lower().replace(" ", "_")
        self.cals.setdefault(entity, {})
        return f"entry-{title}", entity

    async def calendar_create(self, entity_id, event):
        self._need()
        self._maybe_crash("create", "before")
        self.writes.append(("create", event["summary"]))
        if not self.drop_creates:
            uid = f"uid-{self.next_uid}"
            self.next_uid += 1
            self.cals[entity_id][uid] = {**copy.deepcopy(event), "uid": uid, "excluded": set()}
            self.push(entity_id)
        self._maybe_crash("create", "after")

    async def calendar_update(self, entity_id, uid, event, recurrence_id=None):
        raise AssertionError("the engine never rewrites events")

    async def calendar_delete(self, entity_id, uid, recurrence_id=None):
        self._need()
        self._maybe_crash("delete", "before")
        item = self.cals[entity_id].get(uid)
        if item is None:
            return False
        self.writes.append(("delete", uid, recurrence_id))
        if recurrence_id:
            item["excluded"].add(recurrence_id)
        else:
            del self.cals[entity_id][uid]
        self.push(entity_id)
        self._maybe_crash("delete", "after")
        return True

    async def calendar_subscribe(self, entity_id, start, end, handler):
        self._need()
        key = object()
        self.subs.setdefault(entity_id, {})[key] = (start, end, handler)
        handler(self.expand(entity_id, start, end))
        return Sub(lambda: self.subs.get(entity_id, {}).pop(key, None))

    async def calendar_events(self, entity_id, start, end):
        self._need()
        return self.expand(entity_id, start, end)

    async def subscribe_events(self, event_type, handler):
        self._need()
        key = object()
        self.events.setdefault(event_type, {})[key] = handler
        return Sub(lambda: self.events[event_type].pop(key, None))

    async def fire_event(self, event_type, data):
        self.fired.append((event_type, copy.deepcopy(data)))

    async def get_script_config(self, object_id):
        return copy.deepcopy(self.scripts.get(object_id))

    async def put_script_config(self, object_id, config):
        self.scripts[object_id] = copy.deepcopy(config)

    async def call_service(self, domain, service, data=None):
        self.services.append((domain, service))

    async def expose_entities(self, entity_ids, assistants):
        self.exposed.append((list(entity_ids), list(assistants)))

    # ── test helpers ──
    def push(self, entity):
        if self.suppress_push:
            return
        for start, end, handler in list(self.subs.get(entity, {}).values()):
            handler(self.expand(entity, start, end))

    def expand(self, entity, start, end):
        out = []
        for uid, item in self.cals.get(entity, {}).items():
            first = datetime.fromisoformat(item["dtstart"])
            length = datetime.fromisoformat(item["dtend"]) - first
            rule = item.get("rrule")
            if rule:
                parts = dict(p.split("=", 1) for p in rule.split(";"))
                wanted = set(_CODES if parts["FREQ"] == "DAILY" else parts["BYDAY"].split(","))
                starts, day = [], first.date()
                while day <= end.date():
                    dt = datetime.combine(day, first.timetz())
                    if _CODES[dt.weekday()] in wanted:
                        starts.append(dt)
                    day += timedelta(days=1)
            else:
                starts = [first]
            for dt in starts:
                if not (dt + length > start and dt < end):
                    continue
                rid = dt.strftime("%Y%m%dT%H%M%S") if rule else None
                if rid in item["excluded"]:
                    continue
                out.append({"summary": item["summary"], "start": dt.isoformat(),
                            "end": (dt + length).isoformat(),
                            "description": item.get("description"), "uid": uid,
                            "recurrence_id": rid, "rrule": rule, "all_day": False})
        return out

    def add_ui(self, entity, uid, start, summary="Alarm", description=None, rrule=None):
        self.cals.setdefault(entity, {})[uid] = {
            "uid": uid, "summary": summary, "dtstart": start.isoformat(),
            "dtend": (start + timedelta(minutes=1)).isoformat(),
            "description": description, "rrule": rrule, "excluded": set()}
        self.push(entity)

    def only(self, entity=CAL):
        (item,) = self.cals[entity].values()
        return item


class Rig:
    def __init__(self, conn=None, ha=None, now=None, awaiting=("dev1",)):
        if conn is None:
            conn = sqlite3.connect(":memory:")
            conn.executescript(em_db.ALERT_SCHEMA_SQL)   # the schema-22 migration's job
        self.conn = conn
        self.ha = ha or FakeHa()
        self.now = now or [int(NOW.timestamp() * 1000)]
        self.sent, self.notices = [], []
        self.awaiting = list(awaiting)
        self.auto_ack = True
        self.config = {"timerRingGapSeconds": 3, "timerRingSeconds": 60}
        self.sounds = {"alarm": SOUND, "timer": TIMER_SOUND}
        self.on_act = None
        self.engine = AlertEngine(
            self.conn, self.ha, self.send,
            sound_resolver=lambda _e, kind: self.sounds[kind],
            config_resolver=lambda _e: self.config,
            speakers=lambda: [Speaker("dev1", "Office", "Study"),
                              Speaker("dev2", "Kitchen", "Kitchen")],
            awaiting_intent=lambda: self.awaiting,
            notify=lambda dev, code, data: self.notices.append((dev, code, data)),
            now_ms=lambda: self.now[0])

    async def send(self, device, msg_type, body, generation=0):
        message_id = str(uuid.uuid4())
        self.sent.append((device, msg_type, copy.deepcopy(body)))
        ep = self.engine._eps[device]
        if self.auto_ack and msg_type == "alert.delta":
            asyncio.get_running_loop().call_soon(lambda: asyncio.ensure_future(self.engine.on_alert_ack(
                device, {"delivery_epoch": body["delivery_epoch"], "applied_through": body["sequence"],
                         "durable": True, "need_snapshot": False})))
        if self.auto_ack and msg_type == "alert.snapshot" and body["page_index"] == body["page_count"] - 1:
            asyncio.get_running_loop().call_soon(lambda: asyncio.ensure_future(self.engine.on_alert_ack(
                device, {"delivery_epoch": ep.epoch, "applied_through": body["high_water_mark"],
                         "durable": True, "need_snapshot": False})))
        if msg_type == "alert.act" and self.on_act:
            asyncio.get_running_loop().call_soon(
                lambda: asyncio.ensure_future(self.on_act(device, message_id, body)))
        return message_id

    def of(self, msg_type):
        return [body for _dev, kind, body in self.sent if kind == msg_type]

    def notes(self, code):
        return [data for _dev, c, data in self.notices if c == code]

    def row(self, op_id):
        cur = self.conn.cursor()
        cur.row_factory = sqlite3.Row
        return cur.execute("select * from alert_ops where op_id=?", (op_id,)).fetchone()

    def rows(self):
        cur = self.conn.cursor()
        cur.row_factory = sqlite3.Row
        return cur.execute("select * from alert_ops order by seq").fetchall()

    async def up(self, online=False):
        await self.engine.ensure_endpoint("dev1", "Office")
        await self.engine.start()
        await settle()
        if online:
            await self.engine.on_session_hello("dev1", {"delivery_epoch": None, "acked_sequence": 0})
            await settle()

    def live(self):
        return list(self.engine._eps["dev1"].live.values())


async def settle(rounds=150):
    for _ in range(rounds):
        await asyncio.sleep(0)


def run(coro):
    return asyncio.run(coro)


def device_op(occ, action, reason=None, child=None, op_id=None):
    return {"op_id": op_id or str(uuid.uuid4()), "action": action,
            "occurrence_id": occ.occurrence_id, "schedule_id": occ.schedule_id,
            "revision": 1, "reason": reason, "source": "button", "child": child}


def snooze_child(occ, press_ms):
    due = snooze_due_utc_ms(press_ms, occ.settings.snooze_ms)
    due_local = iso_seconds(datetime.fromtimestamp(due / 1000, tz=occ.start.tzinfo))
    sid = snooze_schedule_id(occ.occurrence_id)
    return {"schedule_id": sid, "occurrence_id": occurrence_id_for(sid, due_local),
            "due_utc_ms": str(due), "due_local": due_local}


def op_result(rig, op_id):
    return [b for b in rig.of("alert.op_result") if b["op_id"] == op_id]


def objects(rig):
    return [o for body in rig.of("alert.delta") + rig.of("alert.snapshot") for o in body["objects"]]


# ── provisioning, encoding, identity ────────────────────────────────────────

def test_calendar_provisioned_once_and_survives_rename_and_restart():
    async def go():
        rig = Rig()
        first = await rig.engine.ensure_endpoint("dev1", "Office")
        assert await rig.engine.ensure_endpoint("dev1", "Bedroom") == first == CAL
        restarted = Rig(conn=rig.conn, ha=rig.ha)
        assert await restarted.engine.ensure_endpoint("dev1", "Bedroom") == CAL
        (row,) = rig.conn.execute("select calendar_entity, delivery_epoch, sequence from alert_delivery")
        assert row[0] == CAL and int(row[1]) > 0 and row[2] == 0
        assert restarted.engine._eps["dev1"].epoch == row[1]
    run(go())


def test_set_alarm_writes_encoded_weekday_event_in_ha_timezone():
    async def go():
        rig = Rig()
        rig.ha.zone = "America/Chicago"
        await rig.up()
        op = str(uuid.uuid4())
        result = await rig.engine.set_alarm("dev1", "06:30", ["fri", "mon", "tue", "wed", "thu"],
                                            "Wake up", op_id=op)
        event = rig.ha.only()
        assert event["summary"] == "Wake up"
        assert event["dtstart"] == "2026-09-28T06:30:00-05:00"
        assert event["dtend"] == "2026-09-28T06:31:00-05:00"
        assert event["rrule"] == "FREQ=WEEKLY;BYDAY=MO,TU,WE,TH,FR"
        line = parse_description(event["description"]).line
        assert line.kind == "alarm" and line.op_id == op and line.parent is None
        assert line.settings.sound == SOUND and line.settings.volume is None
        assert (line.settings.snooze_ms, line.settings.max_ring_ms, line.settings.loop_gap_ms,
                line.settings.ramp_ms) == (DEFAULT_SNOOZE_MS, DEFAULT_MAX_RING_MS,
                                           DEFAULT_LOOP_GAP_MS, DEFAULT_RAMP_MS)
        assert result["ok"] and result["stored_in_ha"] and result["first_due"] == event["dtstart"]
        assert result["armed_on_endpoint"] is False and result["delivery_pending"] is True
        # Every weekday instance is its own occurrence keyed by recurrence_id.
        keys = [o.key for o in rig.live()]
        assert keys == ["20260928T063000", "20260929T063000", "20260930T063000",
                        "20261001T063000", "20261002T063000"]
        assert rig.row(op)["state"] == "applied" and rig.row(op)["step"] == 1
    run(go())


def test_armed_only_after_device_durable_ack():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        result = await rig.engine.set_alarm("dev1", "23:00", name="Night")
        assert result["armed_on_endpoint"] is True and result["delivery_pending"] is False
        rig.auto_ack = False
        late = await rig.engine.set_alarm("dev1", "23:30", name="Later")
        assert late["stored_in_ha"] and not late["armed_on_endpoint"] and late["delivery_pending"]
    em_alerts.RESULT_SECONDS, saved = 0.2, em_alerts.RESULT_SECONDS
    try:
        run(go())
    finally:
        em_alerts.RESULT_SECONDS = saved


def test_ui_event_is_default_alarm_and_malformed_line_is_flagged():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        rig.ha.add_ui(CAL, "ui-1", NOW + timedelta(hours=2), summary="School run")
        rig.ha.add_ui(CAL, "ui-2", NOW + timedelta(hours=3), description="echomuse: {broken")
        await settle()
        by_uid = {o.uid: o for o in rig.live()}
        ui = by_uid["ui-1"]
        assert ui.schedule_id == ui_schedule_id(CAL, "ui-1")
        assert ui.key == iso_seconds(NOW + timedelta(hours=2))
        body = ui.cache_body()
        assert body["sound"] == SOUND and body["kind"] == "alarm" and body["label"] == "School run"
        assert by_uid["ui-2"].settings.snooze_ms == DEFAULT_SNOOZE_MS
        assert "malformed_event" in rig.engine.status("dev1")["flags"]
        assert not any(w[0] != "create" for w in rig.ha.writes)   # never rewritten
    run(go())


def test_unresolvable_sound_rings_fallback_and_is_flagged():
    async def go():
        rig = Rig()
        rig.sounds["alarm"] = None
        await rig.up()
        await rig.engine.set_alarm("dev1", "23:00")
        assert parse_description(rig.ha.only()["description"]).line.settings.sound == "builtin:fallback"
        assert "alarm_sound_unresolved" in rig.engine.status("dev1")["flags"]
    run(go())


# ── journal ordering and idempotency ────────────────────────────────────────

def test_duplicate_op_returns_stored_result_and_different_payload_conflicts():
    async def go():
        rig = Rig()
        await rig.up()
        op = str(uuid.uuid4())
        first = await rig.engine.set_alarm("dev1", "06:30", ["mon"], "Work", op_id=op)
        rig.now[0] += 3_600_000         # a retry an hour later recomputes nothing
        again = await rig.engine.set_alarm("dev1", "06:30:00", ["mon"], "Work", op_id=op)
        assert again["first_due"] == first["first_due"] and again["ok"]
        assert json.loads(rig.row(op)["result_json"])["first_due"] == first["first_due"]
        assert await rig.engine.set_alarm("dev1", "07:30", ["mon"], "Work", op_id=op) == {
            "ok": False, "error": "op_id_conflict"}
        assert [w[0] for w in rig.ha.writes] == ["create"] and len(rig.rows()) == 1
    run(go())


def test_create_while_ha_unreachable_fails_without_journal_row():
    async def go():
        rig = Rig()
        await rig.up()
        rig.ha.connected = False
        rig.engine.on_ha_disconnected()
        result = await rig.engine.set_alarm("dev1", "07:00")
        assert result["ok"] is False and result["stored_in_ha"] is False
        assert rig.rows() == []
    run(go())


def test_pending_ops_wait_for_ha_then_apply_strictly_in_seq_order():
    async def go():
        rig = Rig()
        await rig.up()
        await rig.engine.set_alarm("dev1", "13:00", name="Early")
        await rig.engine.set_alarm("dev1", "15:00", name="Late")
        early, late = rig.live()
        rig.ha.connected = False
        rig.engine.on_ha_disconnected()
        for occ in (late, early):     # uploaded late first
            await rig.engine.on_local_operation("dev1", device_op(occ, "dismiss"))
        await settle()
        assert [r["state"] for r in rig.rows()][-2:] == ["pending", "pending"]
        rig.ha.writes.clear()
        rig.ha.connected = True
        await rig.engine.on_ha_connected()
        await settle()
        assert [r["state"] for r in rig.rows()] == ["applied"] * 4
        uids = [w[1] for w in rig.ha.writes]
        assert uids == [late.uid, early.uid]
    run(go())


def test_create_never_visible_is_rejected_after_resubscribe(monkeypatch):
    monkeypatch.setattr(em_alerts, "VISIBILITY_SECONDS", 0.02)

    async def go():
        rig = Rig()
        await rig.up()
        rig.ha.drop_creates = True
        subscribes = []
        original = rig.ha.calendar_subscribe

        async def counting(*args):
            subscribes.append(1)
            return await original(*args)
        rig.ha.calendar_subscribe = counting
        result = await rig.engine.set_alarm("dev1", "20:00")
        assert result == {"ok": False, "op_id": result["op_id"], "error": "not_visible",
                          "stored_in_ha": False}
        assert len(subscribes) == 1
        assert rig.rows()[0]["state"] == "rejected"
        assert rig.notes("operation_rejected")[0]["error"] == "not_visible"
    run(go())


# ── device-originated merge table (§16.4) ───────────────────────────────────

def test_merge_dismiss_present_and_absent():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "06:30", ["mon", "tue"], "Work")
        monday = rig.live()[0]
        op = device_op(monday, "dismiss")
        await rig.engine.on_local_operation("dev1", op)
        await settle()
        assert rig.ha.writes[-1] == ("delete", monday.uid, monday.recurrence_id)
        assert rig.row(op["op_id"])["state"] == "applied"
        assert op_result(rig, op["op_id"]) == [{"op_id": op["op_id"], "state": "applied", "error": None}]
        assert {"occurrence_id": monday.occurrence_id, "revision": rig.engine._eps["dev1"].sequence,
                "tombstone": "dismissed"} in objects(rig)
        assert [o.key for o in rig.live()] == ["20260929T063000"]   # the series survives
        # Absent: someone deleted Tuesday in HA first.
        tuesday = rig.live()[0]
        rig.ha.cals[CAL][tuesday.uid]["excluded"].add(tuesday.recurrence_id)
        rig.ha.push(CAL)
        await settle()
        writes = len(rig.ha.writes)
        absent = device_op(tuesday, "dismiss")
        await rig.engine.on_local_operation("dev1", absent)
        await settle()
        assert len(rig.ha.writes) == writes
        assert rig.row(absent["op_id"])["state"] == "applied"
        assert op_result(rig, absent["op_id"])[0]["state"] == "applied"
    run(go())


def test_merge_expire_present_journals_reason_and_absent_writes_nothing():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "12:10", name="Lunch")
        occ = rig.live()[0]
        op = device_op(occ, "expire", reason="timed_out")
        await rig.engine.on_local_operation("dev1", op)
        await settle()
        assert rig.ha.cals[CAL] == {}
        assert json.loads(rig.row(op["op_id"])["result_json"])["reason"] == "timed_out"
        assert any(o.get("tombstone") == "expired" and o["occurrence_id"] == occ.occurrence_id
                   for o in objects(rig))
        writes = len(rig.ha.writes)
        again = device_op(occ, "expire", reason="missed")
        await rig.engine.on_local_operation("dev1", again)
        await settle()
        assert len(rig.ha.writes) == writes and op_result(rig, again["op_id"])[0]["state"] == "applied"
        bad = device_op(occ, "expire", reason="bored")
        await rig.engine.on_local_operation("dev1", bad)
        assert op_result(rig, bad["op_id"])[0] == {"op_id": bad["op_id"], "state": "rejected",
                                                    "error": "invalid_reason"}
    run(go())


def test_merge_snooze_parent_present_creates_child_then_deletes_parent():
    async def go():
        rig = Rig()
        rig.ha.zone = "America/Chicago"
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "06:30", ["mon"], "Wake up")
        parent = rig.live()[0]
        press = parent.due_utc_ms + 12_345
        child = snooze_child(parent, press)
        op = device_op(parent, "snooze", child=child)
        await rig.engine.on_local_operation("dev1", op)
        await settle()
        created = [e for e in rig.ha.cals[CAL].values() if not e.get("rrule")][0]
        line = parse_description(created["description"]).line
        assert created["summary"] == "Snoozed: Wake up"
        assert created["dtstart"] == child["due_local"] == "2026-09-28T06:39:13-05:00"
        assert line.kind == "snooze" and line.op_id == op["op_id"]
        assert line.schedule_id == child["schedule_id"]
        assert (line.parent.schedule_id, line.parent.occurrence_key) == (parent.schedule_id, parent.key)
        assert parent.recurrence_id in rig.ha.cals[CAL][parent.uid]["excluded"]
        assert [w[0] for w in rig.ha.writes][-2:] == ["create", "delete"]
        row = rig.row(op["op_id"])
        assert (row["state"], row["step"]) == ("applied", 2)
        live_ids = {o.occurrence_id for o in rig.live()}
        assert child["occurrence_id"] in live_ids and parent.occurrence_id not in live_ids
        sent = objects(rig)
        assert any(o.get("tombstone") == "snoozed" and o["occurrence_id"] == parent.occurrence_id for o in sent)
        assert any(o.get("occurrence_id") == child["occurrence_id"] and o.get("kind") == "snooze" for o in sent)
    run(go())


def test_merge_snooze_after_parent_schedule_deleted_is_rejected_with_child_tombstone():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "12:05", name="Soon")
        parent = rig.live()[0]
        child = snooze_child(parent, parent.due_utc_ms + 1000)
        await rig.ha.calendar_delete(CAL, parent.uid)       # deleted in HA while offline
        await settle()
        writes = len(rig.ha.writes)
        op = device_op(parent, "snooze", child=child)
        await rig.engine.on_local_operation("dev1", op)
        await settle()
        assert len(rig.ha.writes) == writes
        assert rig.row(op["op_id"])["state"] == "rejected"
        assert op_result(rig, op["op_id"])[0]["state"] == "rejected"
        assert {"occurrence_id": child["occurrence_id"], "revision": rig.engine._eps["dev1"].sequence,
                "tombstone": "deleted"} in rig.of("alert.delta")[-1]["objects"]
    run(go())


@pytest.mark.parametrize("crash", [("create", "before"), ("create", "after"),
                                   ("delete", "before"), ("delete", "after")])
def test_snooze_recovers_from_crash_at_each_write(crash):
    async def go():
        rig = Rig()
        await rig.up()
        await rig.engine.set_alarm("dev1", "06:30", ["mon"], "Wake up")
        parent = rig.live()[0]
        op = device_op(parent, "snooze", child=snooze_child(parent, parent.due_utc_ms))
        rig.ha.crash = crash
        await rig.engine.on_local_operation("dev1", op)
        await settle()
        assert rig.row(op["op_id"])["state"] == "pending"
        await rig.engine.close()
        rig.ha.connected = True
        restarted = Rig(conn=rig.conn, ha=rig.ha, now=rig.now)
        await restarted.engine.start()
        await settle()
        children = [e for e in rig.ha.cals[CAL].values() if not e.get("rrule")]
        assert len(children) == 1
        assert parse_description(children[0]["description"]).line.op_id == op["op_id"]
        assert parent.recurrence_id in rig.ha.cals[CAL][parent.uid]["excluded"]
        row = restarted.row(op["op_id"])
        assert (row["state"], row["step"]) == ("applied", 2)
    run(go())


@pytest.mark.parametrize("stage", ["before", "after"])
def test_create_recovers_from_crash_without_duplicate(stage):
    async def go():
        rig = Rig()
        await rig.up()
        rig.ha.crash = ("create", stage)
        op = str(uuid.uuid4())
        result = await rig.engine.set_alarm("dev1", "21:00", op_id=op)
        assert result == {"ok": True, "stored_in_ha": False, "pending": True, "op_id": op}
        await rig.engine.close()
        rig.ha.connected = True
        restarted = Rig(conn=rig.conn, ha=rig.ha, now=rig.now)
        await restarted.engine.start()
        await settle()
        assert len(rig.ha.cals[CAL]) == 1 and restarted.row(op)["state"] == "applied"
    em_alerts.RESULT_SECONDS, saved = 0.05, em_alerts.RESULT_SECONDS
    try:
        run(go())
    finally:
        em_alerts.RESULT_SECONDS = saved


# ── restore guard and backlog ───────────────────────────────────────────────

def test_restore_guard_redeletes_handled_occurrences_and_cancelled_series():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "06:30", ["mon", "tue"], "Work")
        await rig.engine.set_alarm("dev1", "22:00", name="Bed")
        backup = copy.deepcopy(rig.ha.cals)
        monday = rig.live()[1] if rig.live()[0].label == "Bed" else rig.live()[0]
        assert monday.key == "20260928T063000"
        await rig.engine.on_local_operation("dev1", device_op(monday, "dismiss"))
        await settle()
        cancelled = await rig.engine.cancel_alarm("dev1", name="bed")
        assert cancelled["ok"] and cancelled["cancelled"] == [{"name": "Bed", "kind": "alarm"}]
        delivered_before = len(objects(rig))
        rig.ha.cals = copy.deepcopy(backup)             # HA restored from an older backup
        rig.ha.push(CAL)
        await settle()
        names = {e["summary"] for e in rig.ha.cals[CAL].values()}
        assert names == {"Work"}
        assert monday.recurrence_id in rig.ha.only()["excluded"]
        assert [o.key for o in rig.live()] == ["20260929T063000"]
        upserts = [o for o in objects(rig)[delivered_before:] if "tombstone" not in o]
        assert upserts == []                                  # nothing restored was delivered
        assert len(rig.notes("restore_guard")) == 2
    run(go())


def test_backlog_expires_missed_occurrences_and_keeps_catch_up_window():
    async def go():
        rig = Rig()
        await rig.engine.ensure_endpoint("dev1", "Office")
        rig.ha.add_ui(CAL, "old", NOW - timedelta(days=2), summary="Missed")
        rig.ha.add_ui(CAL, "recent", NOW - timedelta(minutes=10), summary="Catch up")
        await rig.engine.start()
        await settle()
        assert set(rig.ha.cals[CAL]) == {"recent"}
        (row,) = [r for r in rig.rows() if r["action"] == "expire"]
        assert row["source"] == "engine" and row["state"] == "applied"
        assert json.loads(row["result_json"])["reason"] == "missed"
        assert [o.uid for o in rig.live()] == ["recent"]
        # The expired occurrence reappears (restore) and is guarded at the next reconnect.
        rig.ha.add_ui(CAL, "old", NOW - timedelta(days=2), summary="Missed")
        rig.engine.on_ha_disconnected()
        await rig.engine.on_ha_connected()
        await settle()
        assert set(rig.ha.cals[CAL]) == {"recent"}
        assert len([r for r in rig.rows() if r["action"] == "expire"]) == 1
    run(go())


# ── delivery ────────────────────────────────────────────────────────────────

def test_snapshot_of_129_occurrences_then_deltas_and_need_snapshot():
    async def go():
        rig = Rig()
        await rig.up()
        for i in range(129):
            rig.ha.add_ui(CAL, f"u{i:03}", NOW + timedelta(minutes=5 + i), summary=f"A{i}")
        await settle()
        await rig.engine.on_session_hello("dev1", {"delivery_epoch": None, "acked_sequence": 0})
        await settle()
        pages = rig.of("alert.snapshot")
        assert [len(p["objects"]) for p in pages] == [128, 1]
        assert {(p["page_index"], p["page_count"]) for p in pages} == {(0, 2), (1, 2)}
        flat = pages[0]["objects"] + pages[1]["objects"]
        assert pages[0]["sha256"] == pages[1]["sha256"] == hashlib.sha256(
            canonical_json(flat).encode()).hexdigest()
        assert [o["label"] for o in flat] == [f"A{i}" for i in range(129)]
        ep = rig.engine._eps["dev1"]
        hwm = pages[0]["high_water_mark"]
        assert ep.acked == hwm == ep.sequence
        rig.ha.add_ui(CAL, "new", NOW + timedelta(hours=5), summary="New")
        await settle()
        (delta,) = rig.of("alert.delta")
        assert delta["sequence"] == hwm + 1 and delta["delivery_epoch"] == ep.epoch
        assert [o["label"] for o in delta["objects"]] == ["New"]
        assert delta["objects"][0]["revision"] == hwm + 1
        # Same epoch and sequence on reconnect: nothing resent.
        rig.engine.on_session_lost("dev1")
        await rig.engine.on_session_hello("dev1", {"delivery_epoch": ep.epoch,
                                                   "acked_sequence": ep.sequence})
        assert len(rig.of("alert.snapshot")) == 2
        await rig.engine.on_alert_ack("dev1", {"delivery_epoch": ep.epoch, "applied_through": 0,
                                               "durable": False, "need_snapshot": True})
        await settle()
        again = rig.of("alert.snapshot")[2:]
        assert sum(len(p["objects"]) for p in again) == 130
    run(go())


def test_device_op_for_unknown_occurrence_waits_for_ha_then_applies():
    async def go():
        rig = Rig()
        await rig.up()
        await rig.engine.set_alarm("dev1", "20:00", name="Later")
        occ = rig.live()[0]
        await rig.engine.close()
        rig.ha.connected = False
        restarted = Rig(conn=rig.conn, ha=rig.ha, now=rig.now)   # controller restart, HA down
        await restarted.engine.on_session_hello("dev1", {"delivery_epoch": None, "acked_sequence": 0})
        op = device_op(occ, "dismiss")
        await restarted.engine.on_local_operation("dev1", op)
        assert restarted.row(op["op_id"]) is None and op_result(restarted, op["op_id"]) == []
        rig.ha.connected = True
        await restarted.engine.start()
        await settle()
        assert rig.ha.cals[CAL] == {}
        assert restarted.row(op["op_id"])["state"] == "applied"
        assert op_result(restarted, op["op_id"])[0]["state"] == "applied"
    run(go())


def test_changes_while_offline_are_snapshotted_on_reconnect():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        ep = rig.engine._eps["dev1"]
        epoch, acked = ep.epoch, ep.acked
        rig.engine.on_session_lost("dev1")
        await rig.engine.set_alarm("dev1", "20:00")
        assert rig.of("alert.delta") == []
        await rig.engine.on_session_hello("dev1", {"delivery_epoch": epoch, "acked_sequence": acked})
        await settle()
        assert [o["label"] for o in rig.of("alert.snapshot")[-1]["objects"]] == ["Alarm"]
    run(go())


def test_capacity_limits(monkeypatch):
    monkeypatch.setattr(em_alerts, "MAX_SCHEDULES", 2)
    monkeypatch.setattr(em_alerts, "MAX_OCCURRENCES", 3)

    async def go():
        rig = Rig()
        await rig.up()
        assert (await rig.engine.set_alarm("dev1", "20:00"))["ok"]
        assert (await rig.engine.set_alarm("dev1", "21:00"))["ok"]
        writes = len(rig.ha.writes)
        assert await rig.engine.set_alarm("dev1", "22:00") == {"ok": False, "error": "capacity_exceeded"}
        assert len(rig.ha.writes) == writes
        for i in range(3):
            rig.ha.add_ui(CAL, f"ui{i}", NOW + timedelta(minutes=1 + i))
        await settle()
        assert [o.uid for o in rig.live()] == ["ui0", "ui1", "ui2"]
        assert "occurrence_capacity_exceeded" in rig.engine.status("dev1")["flags"]
    run(go())


# ── voice / dashboard acts on a ringing alert ───────────────────────────────

def test_dismiss_and_snooze_complete_through_device_local_operation():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        await rig.engine.set_alarm("dev1", "12:01", name="Tea")
        occ = rig.live()[0]
        rig.engine.on_alert_state("dev1", {"active": {"id": occ.occurrence_id, "kind": "alarm",
                                                      "name": "Tea", "foreground": True}})
        assert rig.notes("alert_ringing")[-1] == {"ringing": True}

        async def device(dev, message_id, body):
            rig.engine.on_command_ack(dev, CommandAck.parse({"message_id": message_id, "status": "durable", "error": None}))
            child = snooze_child(occ, rig.now[0]) if body["action"] == "snooze" else None
            await rig.engine.on_local_operation(dev, device_op(occ, body["action"], child=child,
                                                               op_id=body["op_id"]))
        rig.on_act = device
        result = await rig.engine.snooze_alarm("dev1", source="voice")
        assert result["ok"] and result["stored_in_ha"] and result["name"] == "Tea"
        assert result["child"]["name"] == "Snoozed: Tea"
        assert rig.of("alert.act")[0]["action"] == "snooze"
        assert rig.row(result["op_id"])["source"] == "device"
        # Timers stop on the device; nothing is written to HA.
        writes = len(rig.ha.writes)
        rig.engine.on_alert_state("dev1", {"active": {"id": "t-9", "kind": "timer", "name": "Pasta"}})
        assert (await rig.engine.snooze_alarm("dev1")) == {"ok": False, "error": "timers cannot be snoozed"}

        async def timer_device(dev, message_id, body):
            rig.engine.on_command_ack(dev, CommandAck.parse({"message_id": message_id, "status": "applied", "error": None}))
        rig.on_act = timer_device
        stopped = await rig.engine.dismiss_alert("dev1", source="entity")
        assert stopped["ok"] and stopped["kind"] == "timer" and stopped["name"] == "Pasta"
        assert len(rig.ha.writes) == writes
        rig.engine.on_alert_state("dev1", {"active": None, "queue": []})
        assert rig.notes("alert_ringing")[-1] == {"ringing": False}
        assert await rig.engine.dismiss_alert("dev1") == {"ok": False, "error": "nothing is ringing"}
    run(go())


# ── timers (§10.8) ──────────────────────────────────────────────────────────

def test_timer_display_copy_ring_missed_and_undeliverable():
    async def go():
        rig = Rig()
        await rig.up(online=True)
        e = rig.engine
        await e.on_timer_event("dev1", "started", "t1", "Tea", 4, 4, True)
        rig.now[0] += 1000
        e.tick_timers()
        assert rig.notes("timers")[-1]["timers"] == [
            {"id": "t1", "name": "Tea", "total_seconds": 4, "remaining_seconds": 3, "active": True}]
        await e.on_timer_event("dev1", "updated", "t1", "Tea", 4, 3, False)     # paused
        rig.now[0] += 60_000
        e.tick_timers()
        assert e.timers("dev1")[0]["remaining_seconds"] == 3 and not rig.notes("timer_finish_missed")
        await e.on_timer_event("dev1", "updated", "t1", "Tea", 4, 3, True)
        rig.now[0] += 3000 + 4999
        e.tick_timers()
        assert not rig.notes("timer_finish_missed")
        rig.now[0] += 1
        e.tick_timers()
        assert rig.notes("timer_finish_missed") == [{"timer_id": "t1", "name": "Tea"}]
        assert e.timers("dev1") == [] and rig.of("alert.ring") == []   # never rings on its own
        await e.on_timer_event("dev1", "started", "t2", "Pasta", 60, 60, True)
        await e.on_timer_event("dev1", "finished", "t2", "Pasta", 60, 0, False)
        assert rig.of("alert.ring") == [{"ring_id": "t2", "name": "Pasta", "sound": TIMER_SOUND,
                                         "loop_gap_ms": 3000, "max_ring_ms": 60_000}]
        assert e.timers("dev1") == []
        e.on_session_lost("dev1")
        await e.on_timer_event("dev1", "finished", "t3", "Eggs", 60, 0, False)
        assert rig.notes("timer_ring_undeliverable")[0]["timer_id"] == "t3"
        assert len(rig.of("alert.ring")) == 1 and rig.ha.writes == []
    run(go())


def test_the_timer_sound_is_prefetched_on_connect_and_timer_start_only_by_capable_devices():
    async def go():
        rig = Rig()
        await rig.up(online=True)                           # hello without alert_prefetch
        e = rig.engine
        await e.on_timer_event("dev1", "started", "t1", "Tea", 60, 60, True)
        assert rig.of("alert.prefetch") == []
        e.on_session_lost("dev1")
        await e.on_session_hello("dev1", {"delivery_epoch": None, "acked_sequence": 0},
                                 capabilities=frozenset({"alert_prefetch"}))
        assert rig.of("alert.prefetch") == [{"sounds": [TIMER_SOUND]}]
        await e.on_timer_event("dev1", "started", "t2", "Pasta", 60, 60, True)
        await e.on_timer_event("dev1", "updated", "t2", "Pasta", 60, 50, False)
        assert rig.of("alert.prefetch") == [{"sounds": [TIMER_SOUND]}] * 2
    run(go())


# ── LLM relay (§16.7) ───────────────────────────────────────────────────────

async def relay(rig, request_id, action, args):
    (handler,) = rig.ha.events["echomuse_alert_request"].values()
    before = len(rig.ha.fired)
    handler({"request_id": request_id, "action": action, "args": args})
    for _ in range(400):
        await asyncio.sleep(0.001 if len(rig.ha.fired) == before else 0)
        if len(rig.ha.fired) > before:
            break
    event_type, data = rig.ha.fired[-1]
    assert event_type == "echomuse_alert_result" and data["request_id"] == request_id
    return data["result"]


def test_relay_validation_errors_never_write():
    async def go():
        rig = Rig()
        await rig.up()
        cases = [
            ("set_alarm", {"time": "", "days": [], "name": "", "speaker": "Office"}, "time is required"),
            ("set_alarm", {"time": "7am", "speaker": "Office"}, "time must be HH:MM or HH:MM:SS"),
            ("set_alarm", {"time": "07:00", "days": ["funday"], "speaker": ""}, "unknown weekday 'funday'"),
            ("cancel_alarm", {"name": "", "time": "", "all": "False", "speaker": ""},
             "name, time, or all: true is required"),
            ("bogus", {}, "unknown action"),
        ]
        for i, (action, args, error) in enumerate(cases):
            assert await relay(rig, f"ctx|{i}", action, args) == {"ok": False, "error": error}
        assert rig.rows() == [] and rig.ha.writes == []
    run(go())


def test_relay_speaker_resolution_and_idempotent_retry():
    async def go():
        rig = Rig(awaiting=("dev1", "dev2"))
        await rig.up()
        args = {"time": "07:15:00", "days": ["sat"], "name": "Gym", "speaker": ""}
        assert await relay(rig, "ctx|set_alarm|a", "set_alarm", args) == {
            "ok": False, "error": "which speaker?"}
        assert await relay(rig, "ctx|x", "list_alarms", {"speaker": "garage"}) == {
            "ok": False, "error": "which speaker?"}
        rig.awaiting[:] = ["dev1"]
        request_id = "ctx|set_alarm|" + json.dumps(args)
        first = await relay(rig, request_id, "set_alarm", args)
        second = await relay(rig, request_id, "set_alarm", args)
        assert first["ok"] and first == second
        assert first["op_id"] == str(uuid.uuid5(uuid.NAMESPACE_URL, request_id))
        assert len(rig.rows()) == 1 and rig.rows()[0]["source"] == "llm"
        by_area = await relay(rig, "ctx|list", "list_alarms", {"speaker": "study"})
        assert by_area["ok"] and [a["name"] for a in by_area["alarms"]] == ["Gym"]
        assert by_area["alarms"][0]["repeats"] == ["sat"]
        cancelled = await relay(rig, "ctx|cancel", "cancel_alarm",
                                {"name": "", "time": "", "all": True, "speaker": "Office"})
        assert cancelled["ok"] and cancelled["cancelled"] == [{"name": "Gym", "kind": "alarm"}]
        retry = await relay(rig, "ctx|cancel", "cancel_alarm",
                            {"name": "", "time": "", "all": True, "speaker": "Office"})
        assert retry["cancelled"] == cancelled["cancelled"] and rig.ha.cals[CAL] == {}
    run(go())


def test_relay_answers_pending_after_result_timeout_and_still_completes(monkeypatch):
    monkeypatch.setattr(em_alerts, "RESULT_SECONDS", 0.05)

    async def go():
        rig = Rig()
        await rig.up()
        rig.ha.suppress_push = True
        request_id = "ctx|set_alarm|slow"
        result = await relay(rig, request_id, "set_alarm", {"time": "08:00", "speaker": "office"})
        op_id = str(uuid.uuid5(uuid.NAMESPACE_URL, request_id))
        assert result == {"ok": True, "stored_in_ha": False, "pending": True, "op_id": op_id}
        rig.ha.suppress_push = False
        rig.ha.push(CAL)
        await settle()
        assert rig.row(op_id)["state"] == "applied"
    run(go())


# ── script installation (§16.7) ─────────────────────────────────────────────

def test_scripts_install_skip_user_edits_and_upgrade_on_revision():
    async def go():
        rig = Rig()
        assert await rig.engine.install_scripts() == []
        assert set(rig.ha.scripts) == set(render_scripts())
        assert rig.ha.services == [("script", "reload")]
        assert sorted(rig.ha.exposed[-1][0]) == sorted(f"script.{o}" for o in render_scripts())
        assert rig.ha.exposed[-1][1] == ["conversation"]
        recorded = dict(rig.conn.execute("select object_id, sha256 from alert_scripts"))
        assert recorded == {o: config_sha256(c) for o, c in render_scripts().items()}
        # Unchanged: no writes, no reload.
        await rig.engine.install_scripts()
        assert rig.ha.services == [("script", "reload")]
        # A user edit is kept and reported.
        rig.ha.scripts["echomuse_set_alarm"]["alias"] = "Mine"
        warnings = await rig.engine.install_scripts()
        assert len(warnings) == 1 and "echomuse_set_alarm" in warnings[0]
        assert rig.ha.scripts["echomuse_set_alarm"]["alias"] == "Mine"
        # An older revision we wrote (hash still matches the record) is upgraded.
        old = copy.deepcopy(render_scripts()["echomuse_list_alarms"])
        old["variables"]["echomuse_revision"] = 0
        rig.ha.scripts["echomuse_list_alarms"] = old
        rig.conn.execute("update alert_scripts set sha256=? where object_id='echomuse_list_alarms'",
                         (config_sha256(old),))
        await rig.engine.install_scripts()
        assert rig.ha.scripts["echomuse_list_alarms"] == render_scripts()["echomuse_list_alarms"]
        # An older revision edited by the user is not upgraded.
        edited = copy.deepcopy(old)
        edited["alias"] = "Edited"
        rig.ha.scripts["echomuse_dismiss_alert"] = edited
        warnings = await rig.engine.install_scripts()
        assert rig.ha.scripts["echomuse_dismiss_alert"] == edited
        assert any("echomuse_dismiss_alert" in w for w in warnings)
    run(go())
