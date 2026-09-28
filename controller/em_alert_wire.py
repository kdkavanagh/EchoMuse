"""
Alarm calendar encoding, identities, and device alert-cache objects.

Pure functions only: the `echomuse:` description line (SPEC §16.3), schedule
and occurrence identities (§10.2, §16.3), the occurrence/tombstone cache
objects and the `alert.snapshot` / `alert.delta` bodies (§16.4, WIRE §4.7).
Every JSON object that crosses to the device or into a calendar event is
canonical: sorted keys, no whitespace, UTF-8.
"""

from __future__ import annotations

import hashlib
import json
import re
import uuid
from dataclasses import dataclass
from datetime import date, datetime, time, timedelta, timezone, tzinfo
from typing import Any, Iterable, Sequence

# §16.3 description line.
ECHOMUSE_PREFIX = "echomuse: "
ECHOMUSE_MARKER = "echomuse:"
LINE_VERSION = 1
LINE_KEYS = frozenset(
    {"id", "kind", "loop_gap_ms", "max_ring_ms", "op", "parent",
     "ramp_ms", "snooze_ms", "sound", "v", "volume"}
)
KIND_ALARM = "alarm"
KIND_SNOOZE = "snooze"

# §16.3 defaults for events without a valid line, and for new alarms.
FALLBACK_SOUND = "builtin:fallback"
DEFAULT_LABEL = "Alarm"
SNOOZE_LABEL_PREFIX = "Snoozed: "
LABEL_MAX_CHARS = 120
DEFAULT_SNOOZE_MS = 540_000
DEFAULT_MAX_RING_MS = 600_000
DEFAULT_LOOP_GAP_MS = 2_000
DEFAULT_RAMP_MS = 20_000
EVENT_DURATION = timedelta(minutes=1)          # §16.3: dtend = dtstart + 1 min

# §16.4 delivery.
SNAPSHOT_PAGE_OBJECTS = 128
TOMBSTONE_REASONS = ("dismissed", "snoozed", "expired", "deleted")

WEEKDAYS = ("mon", "tue", "wed", "thu", "fri", "sat", "sun")
_BYDAY = ("MO", "TU", "WE", "TH", "FR", "SA", "SU")
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_TIME_RE = re.compile(r"^([01]\d|2[0-3]):([0-5]\d)(?::([0-5]\d))?$")


def canonical_json(obj: Any) -> str:
    """Sorted keys, no whitespace, non-ASCII kept as UTF-8 text."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                      allow_nan=False)


def sha256_hex(obj: Any) -> str:
    """SHA-256 of the canonical JSON encoding of `obj`."""
    return hashlib.sha256(canonical_json(obj).encode("utf-8")).hexdigest()


def _is_uuid(value: Any) -> bool:
    if not isinstance(value, str):
        return False
    try:
        return str(uuid.UUID(value)) == value
    except ValueError:
        return False


def _is_int(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool)


def is_sound_ref(value: Any) -> bool:
    """A sound in a line or cache object: an asset SHA-256 or the fallback."""
    return isinstance(value, str) and (value == FALLBACK_SOUND or bool(_SHA256_RE.match(value)))


def clamp_label(text: str | None) -> str:
    """The human label: the summary, `DEFAULT_LABEL` when empty, ≤120 chars."""
    text = (text or "").strip()
    return (text or DEFAULT_LABEL)[:LABEL_MAX_CHARS]


# ── description line ─────────────────────────────────────────────────────────

@dataclass(frozen=True)
class RingSettings:
    """How one occurrence rings (§16.3). `volume` None = current media volume."""
    sound: str
    volume: float | None = None
    snooze_ms: int = DEFAULT_SNOOZE_MS
    max_ring_ms: int = DEFAULT_MAX_RING_MS
    loop_gap_ms: int = DEFAULT_LOOP_GAP_MS
    ramp_ms: int = DEFAULT_RAMP_MS


@dataclass(frozen=True)
class ParentRef:
    """The occurrence a snooze child was created from."""
    schedule_id: str
    occurrence_key: str


@dataclass(frozen=True)
class EchomuseLine:
    """The parsed `echomuse:` line of an EchoMuse-written event."""
    schedule_id: str
    kind: str
    op_id: str
    parent: ParentRef | None
    settings: RingSettings

    def to_obj(self) -> dict:
        s = self.settings
        return {
            "id": self.schedule_id,
            "kind": self.kind,
            "loop_gap_ms": s.loop_gap_ms,
            "max_ring_ms": s.max_ring_ms,
            "op": self.op_id,
            "parent": (None if self.parent is None else
                       {"id": self.parent.schedule_id, "occ": self.parent.occurrence_key}),
            "ramp_ms": s.ramp_ms,
            "snooze_ms": s.snooze_ms,
            "sound": s.sound,
            "v": LINE_VERSION,
            "volume": s.volume,
        }

    def encode(self) -> str:
        return ECHOMUSE_PREFIX + canonical_json(self.to_obj())


@dataclass(frozen=True)
class ParsedDescription:
    """`line` is None for a UI/automation event; `malformed` flags a present but
    invalid line (§16.3: defaults apply, never guessed at)."""
    human_text: str | None
    line: EchomuseLine | None
    malformed: bool


def build_description(line: EchomuseLine, human_text: str | None = None) -> str:
    """Optional human text, then the `echomuse:` line as the last line."""
    text = (human_text or "").rstrip("\n")
    return f"{text}\n{line.encode()}" if text else line.encode()


def _line_from_obj(obj: Any) -> EchomuseLine | None:
    if not isinstance(obj, dict) or set(obj) != LINE_KEYS:
        return None
    if obj["v"] != LINE_VERSION or isinstance(obj["v"], bool):
        return None
    if not (_is_uuid(obj["id"]) and _is_uuid(obj["op"])):
        return None
    kind, parent = obj["kind"], obj["parent"]
    if kind == KIND_ALARM:
        if parent is not None:
            return None
        parent_ref = None
    elif kind == KIND_SNOOZE:
        if not (isinstance(parent, dict) and set(parent) == {"id", "occ"}
                and _is_uuid(parent["id"]) and isinstance(parent["occ"], str) and parent["occ"]):
            return None
        parent_ref = ParentRef(parent["id"], parent["occ"])
    else:
        return None
    for key in ("loop_gap_ms", "ramp_ms"):
        if not _is_int(obj[key]) or obj[key] < 0:
            return None
    for key in ("max_ring_ms", "snooze_ms"):
        if not _is_int(obj[key]) or obj[key] <= 0:
            return None
    if not is_sound_ref(obj["sound"]):
        return None
    volume = obj["volume"]
    if volume is not None:
        if isinstance(volume, bool) or not isinstance(volume, (int, float)) or not 0.0 <= volume <= 1.0:
            return None
    return EchomuseLine(
        schedule_id=obj["id"], kind=kind, op_id=obj["op"], parent=parent_ref,
        settings=RingSettings(sound=obj["sound"], volume=volume, snooze_ms=obj["snooze_ms"],
                              max_ring_ms=obj["max_ring_ms"], loop_gap_ms=obj["loop_gap_ms"],
                              ramp_ms=obj["ramp_ms"]),
    )


def parse_description(description: str | None) -> ParsedDescription:
    """Split a description into human text and its `echomuse:` line (last line)."""
    text = (description or "").rstrip()
    if not text:
        return ParsedDescription(None, None, False)
    head, _, last = text.rpartition("\n")
    if not last.startswith(ECHOMUSE_MARKER):
        return ParsedDescription(text, None, False)
    human = head.rstrip() or None
    if not last.startswith(ECHOMUSE_PREFIX):
        return ParsedDescription(human, None, True)
    try:
        obj = json.loads(last[len(ECHOMUSE_PREFIX):])
    except ValueError:
        return ParsedDescription(human, None, True)
    line = _line_from_obj(obj)
    return ParsedDescription(human, line, line is None)


# ── identity (§16.3) ─────────────────────────────────────────────────────────

def iso_seconds(dt: datetime) -> str:
    """ISO-8601 with UTC offset, second precision."""
    if dt.tzinfo is None:
        raise ValueError("aware datetime required")
    return dt.replace(microsecond=0).isoformat()


def ui_schedule_id(calendar_entity: str, uid: str) -> str:
    """Schedule ID of an event without an `echomuse:` id."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"{calendar_entity}/{uid}"))


def occurrence_key(recurrence_id: str | None, start: datetime) -> str:
    """`recurrence_id` for a repeating instance, else the start (second precision)."""
    return recurrence_id if recurrence_id else iso_seconds(start)


def occurrence_id_for(schedule_id: str, key: str) -> str:
    return str(uuid.uuid5(uuid.UUID(schedule_id), key))


def snooze_schedule_id(parent_occurrence_id: str) -> str:
    """A snooze child's schedule ID (also its line `id`)."""
    return str(uuid.uuid5(uuid.NAMESPACE_URL, f"echomuse-snooze:{parent_occurrence_id}"))


_EPOCH = datetime(1970, 1, 1, tzinfo=timezone.utc)


def utc_ms(dt: datetime) -> int:
    """Milliseconds since the Unix epoch, exact."""
    return (dt - _EPOCH) // timedelta(milliseconds=1)


def from_utc_ms(ms: int, tz: tzinfo) -> datetime:
    return (_EPOCH + timedelta(milliseconds=ms)).astimezone(tz)


def snooze_label(parent_summary: str) -> str:
    """“Snoozed: <parent summary>”; a snoozed child keeps its own label."""
    label = clamp_label(parent_summary)
    if label.startswith(SNOOZE_LABEL_PREFIX):
        return label
    return clamp_label(SNOOZE_LABEL_PREFIX + label)


def key_start(key: str, tz: tzinfo) -> datetime | None:
    """The instance start an occurrence key names: an ISO start with offset, or
    HA's floating local `recurrence_id` (`YYYYMMDDTHHMMSS`) read in `tz`."""
    try:
        dt = datetime.fromisoformat(key)
    except ValueError:
        try:
            dt = datetime.strptime(key, "%Y%m%dT%H%M%S")
        except ValueError:
            return None
    return dt if dt.tzinfo is not None else dt.replace(tzinfo=tz)


def snooze_due_utc_ms(press_utc_ms: int, snooze_ms: int) -> int:
    """§16.3: the press rounded up to the next whole second, plus `snooze_ms`."""
    return -(-press_utc_ms // 1000) * 1000 + snooze_ms


# ── schedules: time of day and weekdays ─────────────────────────────────────

def parse_clock(text: str) -> time:
    """`HH:MM[:SS]` (§16.7 validation)."""
    m = _TIME_RE.match(text.strip()) if isinstance(text, str) else None
    if not m:
        raise ValueError("time must be HH:MM or HH:MM:SS")
    return time(int(m.group(1)), int(m.group(2)), int(m.group(3) or 0))


def normalize_days(days: Iterable[str]) -> tuple[str, ...]:
    """Distinct weekday names in Monday-first order."""
    wanted = set()
    for d in days:
        if not isinstance(d, str) or d.strip().lower() not in WEEKDAYS:
            raise ValueError(f"unknown weekday {d!r}")
        wanted.add(d.strip().lower())
    return tuple(d for d in WEEKDAYS if d in wanted)


def rrule_for_days(days: Sequence[str]) -> str | None:
    """None for a one-shot, `FREQ=DAILY` for every day, else `FREQ=WEEKLY;BYDAY=…`."""
    days = normalize_days(days)
    if not days:
        return None
    if len(days) == len(WEEKDAYS):
        return "FREQ=DAILY"
    return "FREQ=WEEKLY;BYDAY=" + ",".join(_BYDAY[WEEKDAYS.index(d)] for d in days)


def days_from_rrule(rrule: str | None) -> tuple[str, ...] | None:
    """Weekdays of a DAILY/WEEKLY;BYDAY rule; () for none; None for other rules."""
    if not rrule:
        return ()
    parts = dict(p.split("=", 1) for p in rrule.split(";") if "=" in p)
    extra = set(parts) - {"FREQ", "BYDAY", "INTERVAL", "WKST"}
    if extra or parts.get("INTERVAL", "1") != "1":
        return None
    if parts.get("FREQ") == "DAILY" and "BYDAY" not in parts:
        return WEEKDAYS
    if parts.get("FREQ") == "WEEKLY" and "BYDAY" in parts:
        codes = parts["BYDAY"].split(",")
        if not all(c in _BYDAY for c in codes):
            return None
        return tuple(WEEKDAYS[i] for i, c in enumerate(_BYDAY) if c in codes)
    return None


def _wall_exists(day: date, at: time, tz: tzinfo) -> bool:
    local = datetime.combine(day, at, tzinfo=tz)
    return local.astimezone(timezone.utc).astimezone(tz).replace(tzinfo=None) == local.replace(tzinfo=None)


def next_alarm_start(now: datetime, tz: tzinfo, at: time, days: Sequence[str]) -> datetime:
    """First start strictly after `now` at wall time `at` in `tz` on one of `days`
    (any day when empty). A repeating series starts on a day where the wall time
    exists, so HA's expansion keeps the requested time; a one-shot at a skipped
    wall time rings at the equivalent instant."""
    days = normalize_days(days)
    today = now.astimezone(tz).date()
    for offset in range(15):
        day = today + timedelta(days=offset)
        if days and WEEKDAYS[day.weekday()] not in days:
            continue
        if days and not _wall_exists(day, at, tz):
            continue
        start = datetime.combine(day, at, tzinfo=tz).astimezone(timezone.utc).astimezone(tz)
        if start > now:
            return start
    raise ValueError("no start within two weeks")  # unreachable for valid input


def alarm_start_on(day: date, at: time, tz: tzinfo) -> datetime:
    """A one-shot start at wall time `at` on `day` in `tz` (a skipped wall time
    maps to the equivalent instant)."""
    return datetime.combine(day, at, tzinfo=tz).astimezone(timezone.utc).astimezone(tz)


def alarm_event(summary: str, start: datetime, rrule: str | None, description: str) -> dict:
    """The `event` of `calendar/event/create|update` (§16.4)."""
    event = {
        "summary": summary,
        "dtstart": iso_seconds(start),
        "dtend": iso_seconds(start + EVENT_DURATION),
        "description": description,
    }
    if rrule:
        event["rrule"] = rrule
    return event


# ── occurrences ──────────────────────────────────────────────────────────────

@dataclass(frozen=True)
class Occurrence:
    """One timed instance from a calendar push or backlog fetch."""
    occurrence_id: str
    schedule_id: str
    key: str
    uid: str
    recurrence_id: str | None
    rrule: str | None
    summary: str
    start: datetime
    line: EchomuseLine | None
    malformed: bool
    settings: RingSettings

    @property
    def kind(self) -> str:
        return self.line.kind if self.line else KIND_ALARM

    @property
    def label(self) -> str:
        return clamp_label(self.summary)

    @property
    def due_utc_ms(self) -> int:
        return utc_ms(self.start)

    @property
    def due_local(self) -> str:
        return iso_seconds(self.start)

    @property
    def is_echomuse(self) -> bool:
        return self.line is not None

    def cache_body(self) -> dict:
        """The §16.4 occurrence object without `revision`."""
        s = self.settings
        return {
            "due_local": self.due_local,
            "due_utc_ms": str(self.due_utc_ms),
            "kind": self.kind,
            "label": self.label,
            "loop_gap_ms": s.loop_gap_ms,
            "max_ring_ms": s.max_ring_ms,
            "occurrence_id": self.occurrence_id,
            "ramp_ms": s.ramp_ms,
            "schedule_id": self.schedule_id,
            "snooze_ms": s.snooze_ms,
            "sound": s.sound,
            "volume": s.volume,
        }

    def cache_object(self, revision: int) -> dict:
        return {**self.cache_body(), "revision": revision}


def _parse_start(value: Any) -> datetime | None:
    if not isinstance(value, str) or "T" not in value:
        return None  # all-day (date only) or missing
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    return dt if dt.tzinfo is not None else None


def occurrence_from_event(calendar_entity: str, event: dict, default_sound: str) -> Occurrence | None:
    """Occurrence for one pushed item; None for all-day or unidentifiable items,
    which carry no clock time and are not alarms."""
    uid = event.get("uid")
    start = _parse_start(event.get("start"))
    if event.get("all_day") or not uid or start is None:
        return None
    parsed = parse_description(event.get("description"))
    line = parsed.line
    schedule_id = line.schedule_id if line else ui_schedule_id(calendar_entity, uid)
    key = occurrence_key(event.get("recurrence_id"), start)
    settings = line.settings if line else RingSettings(sound=default_sound)
    return Occurrence(
        occurrence_id=occurrence_id_for(schedule_id, key),
        schedule_id=schedule_id,
        key=key,
        uid=uid,
        recurrence_id=event.get("recurrence_id") or None,
        rrule=event.get("rrule") or None,
        summary=event.get("summary") or "",
        start=start,
        line=line,
        malformed=parsed.malformed,
        settings=settings,
    )


def tombstone_object(occurrence_id: str, revision: int, reason: str) -> dict:
    if reason not in TOMBSTONE_REASONS:
        raise ValueError(f"tombstone reason {reason!r}")
    return {"occurrence_id": occurrence_id, "revision": revision, "tombstone": reason}


def sort_occurrence_objects(objects: Iterable[dict]) -> list[dict]:
    """Snapshot order: `due_utc_ms` (numerically), then `occurrence_id`."""
    return sorted(objects, key=lambda o: (int(o["due_utc_ms"]), o["occurrence_id"]))


def snapshot_pages(delivery_epoch: str, high_water_mark: int, objects: Iterable[dict]) -> list[dict]:
    """`alert.snapshot` bodies: live occurrences only, ≤128 per page, one digest
    over the canonical array of all pages' objects in order. An empty cache is
    one empty page."""
    ordered = sort_occurrence_objects(objects)
    digest = hashlib.sha256(canonical_json(ordered).encode("utf-8")).hexdigest()
    chunks = [ordered[i:i + SNAPSHOT_PAGE_OBJECTS]
              for i in range(0, len(ordered), SNAPSHOT_PAGE_OBJECTS)] or [[]]
    return [
        {"delivery_epoch": delivery_epoch, "high_water_mark": high_water_mark,
         "page_index": i, "page_count": len(chunks), "sha256": digest, "objects": chunk}
        for i, chunk in enumerate(chunks)
    ]


def delta_body(delivery_epoch: str, sequence: int, objects: list[dict]) -> dict:
    return {"delivery_epoch": delivery_epoch, "sequence": sequence, "objects": objects}
