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
from enum import StrEnum
from typing import TYPE_CHECKING, Iterable, NotRequired, TypeGuard, TypedDict

if TYPE_CHECKING:
    from em_ha_client import CalendarEvent


class OccurrenceKind(StrEnum):
    """`kind` of an `echomuse:` line and of a §16.4 occurrence object."""
    ALARM = "alarm"
    SNOOZE = "snooze"


class RingKind(StrEnum):
    """What rings on the device (WIRE §4.7 `alert.state` / `alert.ring_ended`)."""
    ALARM = "alarm"
    TIMER = "timer"


class TombstoneReason(StrEnum):
    """§16.4 tombstone `tombstone` values."""
    DISMISSED = "dismissed"
    SNOOZED = "snoozed"
    EXPIRED = "expired"
    DELETED = "deleted"


class Weekday(StrEnum):
    """Schedule weekday names, Monday first (§16.7 `days`)."""
    MON = "mon"
    TUE = "tue"
    WED = "wed"
    THU = "thu"
    FRI = "fri"
    SAT = "sat"
    SUN = "sun"

    @classmethod
    def of(cls, day: date) -> Weekday:
        return _WEEKDAYS[day.weekday()]


_WEEKDAYS: tuple[Weekday, ...] = tuple(Weekday)


# §16.3 description line.
ECHOMUSE_PREFIX = "echomuse: "
ECHOMUSE_MARKER = "echomuse:"
LINE_VERSION = 1
LINE_KEYS = frozenset(
    {"id", "kind", "loop_gap_ms", "max_ring_ms", "op", "parent",
     "ramp_ms", "snooze_ms", "sound", "v", "volume"}
)

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

_BYDAY = ("MO", "TU", "WE", "TH", "FR", "SA", "SU")
_SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
_TIME_RE = re.compile(r"^([01]\d|2[0-3]):([0-5]\d)(?::([0-5]\d))?$")


# ── wire shapes ──────────────────────────────────────────────────────────────

class ParentObject(TypedDict):
    id: str
    occ: str


# The `echomuse:` line object; "id", "op" and "v" are its §16.3 key names.
EchomuseLineObject = TypedDict("EchomuseLineObject", {
    "id": str, "kind": OccurrenceKind, "loop_gap_ms": int, "max_ring_ms": int, "op": str,
    "parent": ParentObject | None, "ramp_ms": int, "snooze_ms": int, "sound": str, "v": int,
    "volume": float | None,
})


class OccurrenceBody(TypedDict):
    """The §16.4 occurrence object without `revision`."""
    due_local: str
    due_utc_ms: str
    kind: OccurrenceKind
    label: str
    loop_gap_ms: int
    max_ring_ms: int
    occurrence_id: str
    ramp_ms: int
    schedule_id: str
    snooze_ms: int
    sound: str
    volume: float | None


class OccurrenceObject(OccurrenceBody):
    revision: int


class TombstoneObject(TypedDict):
    occurrence_id: str
    revision: int
    tombstone: TombstoneReason


CacheObject = OccurrenceObject | TombstoneObject


class SnapshotPage(TypedDict):
    """One `alert.snapshot` body (WIRE §4.7)."""
    delivery_epoch: str
    high_water_mark: int
    page_index: int
    page_count: int
    sha256: str
    objects: list[OccurrenceObject]


class DeltaBody(TypedDict):
    """An `alert.delta` body (WIRE §4.7)."""
    delivery_epoch: str
    sequence: int
    objects: list[CacheObject]


class AlarmEventBody(TypedDict):
    """The `event` of `calendar/event/create|update` (§16.4)."""
    summary: str
    dtstart: str
    dtend: str
    description: str
    rrule: NotRequired[str]


def canonical_json(obj: object) -> str:
    """Sorted keys, no whitespace, non-ASCII kept as UTF-8 text."""
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False,
                      allow_nan=False)


def sha256_hex(obj: object) -> str:
    """SHA-256 of the canonical JSON encoding of `obj`."""
    return hashlib.sha256(canonical_json(obj).encode("utf-8")).hexdigest()


def _is_uuid(value: object) -> TypeGuard[str]:
    if not isinstance(value, str):
        return False
    try:
        return str(uuid.UUID(value)) == value
    except ValueError:
        return False


def _is_int(value: object) -> TypeGuard[int]:
    return isinstance(value, int) and not isinstance(value, bool)


def is_sound_ref(value: object) -> TypeGuard[str]:
    """A sound in a line or cache object: an asset SHA-256 or the fallback."""
    return isinstance(value, str) and (value == FALLBACK_SOUND or bool(_SHA256_RE.match(value)))


def clamp_label(text: str | None) -> str:
    """The human label: the summary, `DEFAULT_LABEL` when empty, ≤120 chars."""
    text = (text or "").strip()
    return (text or DEFAULT_LABEL)[:LABEL_MAX_CHARS]


# ── description line ─────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
class RingSettings:
    """How one occurrence rings (§16.3). `volume` None = current media volume."""
    sound: str
    volume: float | None = None
    snooze_ms: int = DEFAULT_SNOOZE_MS
    max_ring_ms: int = DEFAULT_MAX_RING_MS
    loop_gap_ms: int = DEFAULT_LOOP_GAP_MS
    ramp_ms: int = DEFAULT_RAMP_MS


@dataclass(frozen=True, slots=True)
class ParentRef:
    """The occurrence a snooze child was created from."""
    schedule_id: str
    occurrence_key: str


@dataclass(frozen=True, slots=True)
class EchomuseLine:
    """The parsed `echomuse:` line of an EchoMuse-written event."""
    schedule_id: str
    kind: OccurrenceKind
    op_id: str
    parent: ParentRef | None
    settings: RingSettings

    def to_obj(self) -> EchomuseLineObject:
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


@dataclass(frozen=True, slots=True)
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


def _parent_ref(parent: object) -> ParentRef | None:
    if not (isinstance(parent, dict) and set(parent) == {"id", "occ"}):
        return None
    schedule_id, key = parent["id"], parent["occ"]
    if not (_is_uuid(schedule_id) and isinstance(key, str) and key):
        return None
    return ParentRef(schedule_id, key)


def _line_from_obj(obj: object) -> EchomuseLine | None:
    if not isinstance(obj, dict) or set(obj) != LINE_KEYS:
        return None
    version, schedule_id, op_id = obj["v"], obj["id"], obj["op"]
    if version != LINE_VERSION or isinstance(version, bool):
        return None
    if not (_is_uuid(schedule_id) and _is_uuid(op_id)):
        return None
    kind, parent = obj["kind"], obj["parent"]
    parent_ref: ParentRef | None
    if kind == OccurrenceKind.ALARM:
        if parent is not None:
            return None
        parent_ref = None
    elif kind == OccurrenceKind.SNOOZE:
        parent_ref = _parent_ref(parent)
        if parent_ref is None:
            return None
    else:
        return None
    loop_gap, ramp, max_ring, snooze = obj["loop_gap_ms"], obj["ramp_ms"], obj["max_ring_ms"], obj["snooze_ms"]
    if not (_is_int(loop_gap) and loop_gap >= 0 and _is_int(ramp) and ramp >= 0):
        return None
    if not (_is_int(max_ring) and max_ring > 0 and _is_int(snooze) and snooze > 0):
        return None
    sound, volume = obj["sound"], obj["volume"]
    if not is_sound_ref(sound):
        return None
    if volume is not None:
        if isinstance(volume, bool) or not isinstance(volume, (int, float)) or not 0.0 <= volume <= 1.0:
            return None
    return EchomuseLine(
        schedule_id=schedule_id, kind=OccurrenceKind(kind), op_id=op_id, parent=parent_ref,
        settings=RingSettings(sound=sound, volume=volume, snooze_ms=snooze,
                              max_ring_ms=max_ring, loop_gap_ms=loop_gap, ramp_ms=ramp),
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


def normalize_days(days: Iterable[object]) -> tuple[Weekday, ...]:
    """Distinct weekday names in Monday-first order."""
    wanted: set[str] = set()
    for d in days:
        if not isinstance(d, str) or d.strip().lower() not in _WEEKDAYS:
            raise ValueError(f"unknown weekday {d!r}")
        wanted.add(d.strip().lower())
    return tuple(d for d in _WEEKDAYS if d in wanted)


def rrule_for_days(days: Iterable[object]) -> str | None:
    """None for a one-shot, `FREQ=DAILY` for every day, else `FREQ=WEEKLY;BYDAY=…`."""
    wanted = normalize_days(days)
    if not wanted:
        return None
    if len(wanted) == len(_WEEKDAYS):
        return "FREQ=DAILY"
    return "FREQ=WEEKLY;BYDAY=" + ",".join(_BYDAY[_WEEKDAYS.index(d)] for d in wanted)


def days_from_rrule(rrule: str | None) -> tuple[Weekday, ...] | None:
    """Weekdays of a DAILY/WEEKLY;BYDAY rule; () for none; None for other rules."""
    if not rrule:
        return ()
    parts = dict(p.split("=", 1) for p in rrule.split(";") if "=" in p)
    extra = set(parts) - {"FREQ", "BYDAY", "INTERVAL", "WKST"}
    if extra or parts.get("INTERVAL", "1") != "1":
        return None
    if parts.get("FREQ") == "DAILY" and "BYDAY" not in parts:
        return _WEEKDAYS
    if parts.get("FREQ") == "WEEKLY" and "BYDAY" in parts:
        codes = parts["BYDAY"].split(",")
        if not all(c in _BYDAY for c in codes):
            return None
        return tuple(_WEEKDAYS[i] for i, c in enumerate(_BYDAY) if c in codes)
    return None


def _wall_exists(day: date, at: time, tz: tzinfo) -> bool:
    local = datetime.combine(day, at, tzinfo=tz)
    return local.astimezone(timezone.utc).astimezone(tz).replace(tzinfo=None) == local.replace(tzinfo=None)


def next_alarm_start(now: datetime, tz: tzinfo, at: time, days: Iterable[object]) -> datetime:
    """First start strictly after `now` at wall time `at` in `tz` on one of `days`
    (any day when empty). A repeating series starts on a day where the wall time
    exists, so HA's expansion keeps the requested time; a one-shot at a skipped
    wall time rings at the equivalent instant."""
    wanted = normalize_days(days)
    today = now.astimezone(tz).date()
    for offset in range(15):
        day = today + timedelta(days=offset)
        if wanted and Weekday.of(day) not in wanted:
            continue
        if wanted and not _wall_exists(day, at, tz):
            continue
        start = datetime.combine(day, at, tzinfo=tz).astimezone(timezone.utc).astimezone(tz)
        if start > now:
            return start
    raise ValueError("no start within two weeks")  # unreachable for valid input


def alarm_start_on(day: date, at: time, tz: tzinfo) -> datetime:
    """A one-shot start at wall time `at` on `day` in `tz` (a skipped wall time
    maps to the equivalent instant)."""
    return datetime.combine(day, at, tzinfo=tz).astimezone(timezone.utc).astimezone(tz)


def alarm_event(summary: str, start: datetime, rrule: str | None, description: str) -> AlarmEventBody:
    """The `event` of `calendar/event/create|update` (§16.4)."""
    event: AlarmEventBody = {
        "summary": summary,
        "dtstart": iso_seconds(start),
        "dtend": iso_seconds(start + EVENT_DURATION),
        "description": description,
    }
    if rrule:
        event["rrule"] = rrule
    return event


# ── occurrences ──────────────────────────────────────────────────────────────

@dataclass(frozen=True, slots=True)
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
    def kind(self) -> OccurrenceKind:
        return self.line.kind if self.line else OccurrenceKind.ALARM

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

    def cache_body(self) -> OccurrenceBody:
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


def with_revision(body: OccurrenceBody, revision: int) -> OccurrenceObject:
    """A cache body as the delivered §16.4 occurrence object."""
    return {**body, "revision": revision}


def _parse_start(value: str | None) -> datetime | None:
    if not isinstance(value, str) or "T" not in value:
        return None  # all-day (date only) or missing
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    return dt if dt.tzinfo is not None else None


def occurrence_from_event(calendar_entity: str, event: CalendarEvent, default_sound: str) -> Occurrence | None:
    """Occurrence for one pushed item; None for all-day or unidentifiable items,
    which carry no clock time and are not alarms."""
    uid = event["uid"]
    start = _parse_start(event["start"])
    if event["all_day"] or not uid or start is None:
        return None
    parsed = parse_description(event["description"])
    line = parsed.line
    schedule_id = line.schedule_id if line else ui_schedule_id(calendar_entity, uid)
    key = occurrence_key(event["recurrence_id"], start)
    settings = line.settings if line else RingSettings(sound=default_sound)
    return Occurrence(
        occurrence_id=occurrence_id_for(schedule_id, key),
        schedule_id=schedule_id,
        key=key,
        uid=uid,
        recurrence_id=event["recurrence_id"] or None,
        rrule=event["rrule"] or None,
        summary=event["summary"] or "",
        start=start,
        line=line,
        malformed=parsed.malformed,
        settings=settings,
    )


def tombstone_object(occurrence_id: str, revision: int, reason: TombstoneReason) -> TombstoneObject:
    return {"occurrence_id": occurrence_id, "revision": revision, "tombstone": reason}


def sort_occurrence_objects(objects: Iterable[OccurrenceObject]) -> list[OccurrenceObject]:
    """Snapshot order: `due_utc_ms` (numerically), then `occurrence_id`."""
    return sorted(objects, key=lambda o: (int(o["due_utc_ms"]), o["occurrence_id"]))


def snapshot_pages(delivery_epoch: str, high_water_mark: int,
                   objects: Iterable[OccurrenceObject]) -> list[SnapshotPage]:
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


def delta_body(delivery_epoch: str, sequence: int, objects: list[CacheObject]) -> DeltaBody:
    return {"delivery_epoch": delivery_epoch, "sequence": sequence, "objects": objects}
