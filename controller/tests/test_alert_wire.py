import hashlib
import uuid
from datetime import date, datetime, time, timezone
from zoneinfo import ZoneInfo

import pytest

from em_alert_wire import (
    DEFAULT_LOOP_GAP_MS, DEFAULT_MAX_RING_MS, DEFAULT_RAMP_MS, DEFAULT_SNOOZE_MS,
    EchomuseLine, OccurrenceKind, ParentRef, RingSettings, alarm_start_on,
    build_description, canonical_json, days_from_rrule, delta_body, key_start,
    next_alarm_start, occurrence_from_event, occurrence_id_for, parse_clock,
    parse_description, rrule_for_days, snapshot_pages, snooze_due_utc_ms, snooze_label,
    snooze_schedule_id, ui_schedule_id, utc_ms,
)

CHICAGO = ZoneInfo("America/Chicago")

SOUND = "a" * 64


def line(kind=OccurrenceKind.ALARM, parent=None):
    return EchomuseLine(str(uuid.uuid4()), kind, str(uuid.uuid4()), parent,
                        RingSettings(SOUND))


def test_description_is_canonical_round_trip_and_preserves_human_text():
    original = line()
    description = build_description(original, "Wake me gently")
    assert description.splitlines()[-1] == "echomuse: " + canonical_json(original.to_obj())
    parsed = parse_description(description)
    assert parsed == parsed.__class__("Wake me gently", original, False)


def test_absent_and_malformed_lines_use_defaults_and_flag_only_malformed():
    ui = parse_description("made in the calendar UI")
    assert ui.line is None and not ui.malformed and ui.human_text == "made in the calendar UI"
    bad = parse_description("note\nechomuse: {not-json")
    assert bad.line is None and bad.malformed and bad.human_text == "note"
    wrong_shape = parse_description('echomuse: {"v":1}')
    assert wrong_shape.line is None and wrong_shape.malformed


def test_identity_ui_recurring_and_snooze_child():
    schedule = ui_schedule_id("calendar.echomuse_office", "event-uid")
    assert schedule == str(uuid.uuid5(uuid.NAMESPACE_URL,
                                      "calendar.echomuse_office/event-uid"))
    key = "20260928T063000"
    assert occurrence_id_for(schedule, key) == str(uuid.uuid5(uuid.UUID(schedule), key))
    parent_occ = occurrence_id_for(schedule, key)
    child = snooze_schedule_id(parent_occ)
    assert child == str(uuid.uuid5(uuid.NAMESPACE_URL, f"echomuse-snooze:{parent_occ}"))


def test_ui_event_materializes_default_alarm_and_one_shot_identity():
    event = {"uid": "ui-1", "summary": "School", "start": "2026-09-28T07:00:00-05:00",
             "end": "2026-09-28T07:01:00-05:00", "description": "from UI",
             "recurrence_id": None, "rrule": None, "all_day": False}
    occ = occurrence_from_event("calendar.echomuse_office", event, SOUND)
    assert occ.key == event["start"]
    assert occ.kind == "alarm" and occ.settings.sound == SOUND
    assert occ.settings.snooze_ms == DEFAULT_SNOOZE_MS
    assert occ.settings.max_ring_ms == DEFAULT_MAX_RING_MS
    assert occ.settings.loop_gap_ms == DEFAULT_LOOP_GAP_MS
    assert occ.settings.ramp_ms == DEFAULT_RAMP_MS
    assert not occ.malformed


def test_repeating_event_uses_recurrence_id_even_if_start_has_offset():
    event = {"uid": "r-1", "summary": "Work", "start": "2026-09-28T06:30:00-05:00",
             "end": "2026-09-28T06:31:00-05:00", "description": None,
             "recurrence_id": "20260928T063000", "rrule": "FREQ=WEEKLY;BYDAY=MO,WE",
             "all_day": False}
    occ = occurrence_from_event("calendar.office", event, SOUND)
    assert occ.key == "20260928T063000"
    assert occ.rrule == "FREQ=WEEKLY;BYDAY=MO,WE"


def test_snapshot_129_objects_pages_and_digest():
    objects = []
    for i in reversed(range(129)):
        objects.append({"occurrence_id": f"{i:04}", "due_utc_ms": str(1000 + i),
                        "revision": 3})
    pages = snapshot_pages("epoch", 3, objects)
    assert [len(p["objects"]) for p in pages] == [128, 1]
    assert [p["page_index"] for p in pages] == [0, 1]
    assert all(p["page_count"] == 2 for p in pages)
    flattened = pages[0]["objects"] + pages[1]["objects"]
    expected = hashlib.sha256(canonical_json(flattened).encode()).hexdigest()
    assert all(p["sha256"] == expected for p in pages)
    assert flattened == sorted(flattened, key=lambda o: (int(o["due_utc_ms"]), o["occurrence_id"]))


def test_empty_snapshot_and_delta_exact_wire_keys():
    pages = snapshot_pages("e", 9, [])
    assert pages == [{"delivery_epoch": "e", "high_water_mark": 9, "page_index": 0,
                      "page_count": 1,
                      "sha256": hashlib.sha256(b"[]").hexdigest(), "objects": []}]
    assert delta_body("e", 10, [{"x": 1}]) == {
        "delivery_epoch": "e", "sequence": 10, "objects": [{"x": 1}]}



def test_line_rejects_out_of_range_values_and_kind_parent_mismatch():
    good = line().to_obj()
    for key, value in [("volume", 1.5), ("volume", True), ("snooze_ms", 0), ("sound", "wav"),
                       ("v", 2), ("kind", "timer"), ("parent", {"id": good["id"], "occ": "k"})]:
        bad = {**good, key: value}
        parsed = parse_description("echomuse: " + canonical_json(bad))
        assert parsed.line is None and parsed.malformed, key
    snooze = EchomuseLine(str(uuid.uuid4()), OccurrenceKind.SNOOZE, str(uuid.uuid4()),
                          ParentRef(str(uuid.uuid4()), "20260928T063000"), RingSettings(SOUND, 0.4))
    assert parse_description(snooze.encode()).line == snooze


def test_snooze_due_rounds_press_up_to_whole_second():
    assert snooze_due_utc_ms(1_000, 540_000) == 541_000
    assert snooze_due_utc_ms(1_001, 540_000) == 542_000
    assert snooze_label("Wake up") == "Snoozed: Wake up"
    assert snooze_label("Snoozed: Wake up") == "Snoozed: Wake up"


def test_clock_days_and_rrule():
    assert parse_clock("07:05") == time(7, 5) and parse_clock("23:59:59") == time(23, 59, 59)
    for bad in ("7:05", "24:00", "07:60", "07:05:5", "seven"):
        with pytest.raises(ValueError):
            parse_clock(bad)
    assert rrule_for_days([]) is None
    assert rrule_for_days(["sun", "mon"]) == "FREQ=WEEKLY;BYDAY=MO,SU"
    assert rrule_for_days(["mon", "tue", "wed", "thu", "fri", "sat", "sun"]) == "FREQ=DAILY"
    assert days_from_rrule("FREQ=WEEKLY;BYDAY=MO,SU") == ("mon", "sun")
    assert days_from_rrule("FREQ=MONTHLY") is None


def test_alarm_start_follows_ha_timezone_across_dst():
    # Spring forward 2027-03-14 in Chicago: 02:30 does not exist.
    now = datetime(2027, 3, 13, 18, tzinfo=timezone.utc)
    once = alarm_start_on(date(2027, 3, 14), time(2, 30), CHICAGO)
    assert once.utcoffset() is not None and utc_ms(once) == utc_ms(
        datetime(2027, 3, 14, 8, 30, tzinfo=timezone.utc))
    weekly = next_alarm_start(now, CHICAGO, time(2, 30), ["sun"])
    assert weekly.date() == date(2027, 3, 21)          # a series starts where the time exists
    assert next_alarm_start(now, CHICAGO, time(6, 30), []).isoformat() == "2027-03-14T06:30:00-05:00"


def test_key_start_reads_both_key_forms():
    assert key_start("2026-09-28T06:30:00-05:00", CHICAGO).utcoffset().total_seconds() == -5 * 3600
    assert key_start("20260928T063000", CHICAGO) == datetime(2026, 9, 28, 6, 30, tzinfo=CHICAGO)
    assert key_start("garbage", CHICAGO) is None