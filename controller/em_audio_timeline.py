"""Uplink audio: EMA1 frames, stream epochs, uplink leases, per-lease timelines.

SPEC §4.2–4.4 and §16.1; the byte layout and JSON spellings are the WIRE
(`docs/protocol-v1.md` §3, §4.2, §4.5, §5). Pure Python + numpy: the transport
feeds raw frames and control bodies in, the session actor reads timelines out.

Sample indices are per-epoch uint64 frame counts. A range is half-open
`[start, end)`. Nothing here invents audio: a sample nobody sent stays unknown,
digital silence is known zeros, and muted audio is not valid evidence.
"""

from __future__ import annotations

import bisect
import collections
import enum
import struct
import time
from collections.abc import Iterator, Mapping
from dataclasses import dataclass, field
from typing import Callable, Iterable, TypedDict

import numpy as np

# -- EMA1 frame (§16.1, WIRE §3) --------------------------------------------

MAGIC = b"EMA1"
HEADER = struct.Struct("<4sBBBBQQQQIIIIII")
HEADER_BYTES = 64
assert HEADER.size == HEADER_BYTES

U32_MAX = 0xFFFFFFFF
U64_MAX = 0xFFFFFFFFFFFFFFFF


class Kind(enum.IntEnum):
    MIC = 1
    REFERENCE = 2
    RENDER = 3
    CELLS = 4
    AFE = 5               # native AFE period records; only to a session.ready that granted afe_metadata


FLAG_DISCONTINUITY = 1 << 0
FLAG_MUTED = 1 << 1
FLAG_UNDERRUN = 1 << 2
FLAG_ESTIMATED = 1 << 3
FLAG_DIGITAL_SILENCE = 1 << 4
_KNOWN_FLAGS = FLAG_DISCONTINUITY | FLAG_MUTED | FLAG_UNDERRUN | FLAG_ESTIMATED | FLAG_DIGITAL_SILENCE

FORMAT_PCM16 = 1
FORMAT_CELLS = 2
FORMAT_AFE = 3
UNCERTAINTY_UNKNOWN = U32_MAX

UPLINK_RATE = 16_000
RENDER_RATE = 48_000
_RATE = {Kind.MIC: UPLINK_RATE, Kind.REFERENCE: UPLINK_RATE, Kind.RENDER: RENDER_RATE, Kind.CELLS: UPLINK_RATE,
         Kind.AFE: UPLINK_RATE}
_FORMAT = {Kind.MIC: FORMAT_PCM16, Kind.REFERENCE: FORMAT_PCM16, Kind.RENDER: FORMAT_PCM16, Kind.CELLS: FORMAT_CELLS,
           Kind.AFE: FORMAT_AFE}
MAX_FRAMES = {Kind.MIC: 1280, Kind.REFERENCE: 1280, Kind.RENDER: 3840, Kind.CELLS: 320, Kind.AFE: 125}

# Active-source mask bits (§16.1): content=1 alert=2 dialog=4 earcon=8.
SOURCE_CONTENT = 1
SOURCE_ALERT = 2
SOURCE_DIALOG = 4
SOURCE_EARCON = 8
SOURCE_MASK_BITS = SOURCE_CONTENT | SOURCE_ALERT | SOURCE_DIALOG | SOURCE_EARCON

# -- Cell records (§16.1) ---------------------------------------------------

CELL_SAMPLES = 512
CELL_RECORD = np.dtype([("e", "<i2"), ("flags", "u1"), ("mask", "u1")])
CELL_RECORD_BYTES = CELL_RECORD.itemsize
assert CELL_RECORD_BYTES == 4
CELL_E_MIN = -12_000
CELL_E_MAX = 0
CELL_FLAG_GAP = 1 << 0
CELL_FLAG_MUTED = 1 << 1
_CELL_FLAG_BITS = CELL_FLAG_GAP | CELL_FLAG_MUTED

# -- AFE period records (afe_metadata_v1, WIRE §4.2) --------------------------
#
# Fire OS 6's AFE writes one metadata frame per 8 ms of capture; the device
# summarises each 80 ms capture period in one 14-byte record. Evidence only:
# no wake, attribution or endpoint decision reads it.

AFE_PERIOD_SAMPLES = 1_280          # one record per capture period
AFE_PERIOD_FRAMES = 10              # AFE frames per period
_AFE_FIELDS = ("frames", "flags", "playback", "erle_max", "erle_mean", "erle_frames", "dtd_max", "dtd_frames",
               "rms_max", "rms_mean", "vad_max", "vad_frames", "volume", "lost")
AFE_RECORD = np.dtype([(name, "u1") for name in _AFE_FIELDS])
AFE_RECORD_BYTES = AFE_RECORD.itemsize
assert AFE_RECORD_BYTES == 14
AFE_FLAG_GAP = 1 << 0
AFE_FLAG_SYNC = 1 << 1
AFE_FLAG_OUTPUT_CLIPPED = 1 << 2
AFE_FLAG_MIC_CLIPPED = 1 << 3
AFE_FLAG_AEC_DIVERGED = 1 << 4
AFE_FLAG_DEVICE_MUTE = 1 << 5
_AFE_FLAG_BITS = (AFE_FLAG_GAP | AFE_FLAG_SYNC | AFE_FLAG_OUTPUT_CLIPPED | AFE_FLAG_MIC_CLIPPED
                  | AFE_FLAG_AEC_DIVERGED | AFE_FLAG_DEVICE_MUTE)
AFE_DTD_MAX = 31
AFE_VAD_MAX = 3
AFE_VOLUME_MAX = 127
# Samples one EMA1 frame stands for, where that is not one sample.
_FRAME_SAMPLES = {Kind.CELLS: CELL_SAMPLES, Kind.AFE: AFE_PERIOD_SAMPLES}

# -- Leases and retention (§4.4, §8.1) --------------------------------------

REFERENCE_HOP = 2_560               # reference backfill grid, samples (§4.4)
LEASE_TTL_MS = 3_000                # §4.4
LEASE_RENEW_S = 1.0                 # the controller renews every second (§4.4)
CANDIDATE_ACK_S = 1.0               # unacknowledged candidate lease ends with `ttl` (§4.4)
DIAGNOSTIC_MAX_S = 30 * 60          # §4.4
CANDIDATE_MIC_PREROLL = 4_800       # support_start − 300 ms (WIRE §4.5)
CANDIDATE_REFERENCE_LEAD = 40_000   # mic start − 2,500 ms
CANDIDATE_CELLS_LEAD = 160_000      # mic start − 10 s

ROLLING_SAMPLES = 10 * UPLINK_RATE          # rolling history kept before the utterance (§8.1)
UTTERANCE_MAX_SAMPLES = 30 * UPLINK_RATE    # longest open utterance (§16.6)
PCM_CAPACITY = UTTERANCE_MAX_SAMPLES + ROLLING_SAMPLES   # per stream: 1.28 MB of int16
REFERENCE_EXTRA = 40_000            # reference backfill reaches 2.5 s before mic
CELL_CAPACITY = (PCM_CAPACITY + ROLLING_SAMPLES) // CELL_SAMPLES
# AFE records reach as far back as the reference: mic retention plus its 2.5 s
# lead, plus the period the floor cuts.
AFE_CAPACITY = -(-(PCM_CAPACITY + REFERENCE_EXTRA) // AFE_PERIOD_SAMPLES) + 1
LEASE_BYTES_CAP = 2_600_000         # §8.1 per-lease bound

# -- Clock fit (§4.3) -------------------------------------------------------

CLOCK_WINDOW_SAMPLES = 10 * UPLINK_RATE
CLOCK_RESIDUAL_NS = 80_000_000
CLOCK_MAX_PPM = 1_000
NOMINAL_NS_PER_SAMPLE = 1e9 / UPLINK_RATE


class ProtocolError(Exception):
    """A frame or stream message violating the WIRE. The receiver reports
    `protocol.error` with `code`/`detail` and closes the audio socket."""

    def __init__(self, code: str, detail: str):
        super().__init__(f"{code}: {detail}")
        self.code = code
        self.detail = detail


class LeaseError(Exception):
    """A lease operation that does not apply to the lease's current state."""


# ---------------------------------------------------------------------------
# uint64 JSON values (WIRE §2: decimal strings)
# ---------------------------------------------------------------------------

def parse_u64(value: object, name: str) -> int:
    if not isinstance(value, str) or not value.isascii() or not value.isdigit():
        raise ProtocolError("malformed_message", f"{name} must be a decimal string, got {value!r}")
    n = int(value)
    if n > U64_MAX:
        raise ProtocolError("malformed_message", f"{name} exceeds uint64")
    return n


def format_u64(value: int) -> str:
    if not 0 <= value <= U64_MAX:
        raise ValueError(f"{value} is not a uint64")
    return str(value)


# ---------------------------------------------------------------------------
# Packets
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class CellRecords:
    """Parsed kind-4 payload. `e_centi` is `E` in hundredths of a dB."""

    e_centi: np.ndarray   # int16
    flags: np.ndarray     # uint8
    mask: np.ndarray      # uint8

    @property
    def e_db(self) -> np.ndarray:
        return self.e_centi.astype(np.float32) / 100.0

    def __len__(self) -> int:
        return int(self.e_centi.size)


@dataclass(frozen=True, slots=True)
class AfeRecord:
    """One capture period's native AFE summary (afe record v1, WIRE §4.2):
    capture samples `[start, start + 1280)`. Raw device units; `frames` = 0
    means the period has no data. Evidence only, never a decision input."""

    start: int
    frames: int             # valid AFE frames that ended in the period, 0–10
    flags: int              # AFE_FLAG_*
    playback: int           # frames with PLAYBACK_ACTIVE
    erle_max: int           # ERLE_RAW max
    erle_mean: int          # mean of the non-zero ERLE_RAW values; 0 when erle_frames = 0
    erle_frames: int        # frames with ERLE_RAW > 0
    dtd_max: int            # DTD max, 0–31 (value = raw / 31)
    dtd_frames: int         # frames with DTD > 0
    rms_max: int            # RMS max raw (dB = raw − 256); 0 = no frame computed RMS
    rms_mean: int           # mean RMS raw over those frames; 0 likewise
    vad_max: int            # DNN_VAD_PROB max, 0–3 (value = raw × 0.25)
    vad_frames: int         # frames with DNN_VAD_PROB > 0
    volume: int             # VOLUME of the period's last valid frame, 0–127
    lost: int               # AFE frames the counters show missing before the period's frames

    @property
    def gap(self) -> bool:
        return bool(self.flags & AFE_FLAG_GAP)

    @property
    def sync(self) -> bool:
        return bool(self.flags & AFE_FLAG_SYNC)

    @property
    def output_clipped(self) -> bool:
        return bool(self.flags & AFE_FLAG_OUTPUT_CLIPPED)

    @property
    def mic_clipped(self) -> bool:
        return bool(self.flags & AFE_FLAG_MIC_CLIPPED)

    @property
    def aec_diverged(self) -> bool:
        return bool(self.flags & AFE_FLAG_AEC_DIVERGED)

    @property
    def device_mute(self) -> bool:
        return bool(self.flags & AFE_FLAG_DEVICE_MUTE)


@dataclass(frozen=True, slots=True)
class Packet:
    kind: Kind
    flags: int
    epoch: int
    sequence: int
    first_sample: int
    mono_ns: int
    uncertainty_us: int | None      # None = unknown (0xffffffff on the wire)
    sample_rate: int
    frame_count: int
    generation: int
    source_mask: int
    payload: bytes

    @property
    def discontinuity(self) -> bool:
        return bool(self.flags & FLAG_DISCONTINUITY)

    @property
    def muted(self) -> bool:
        return bool(self.flags & FLAG_MUTED)

    @property
    def underrun(self) -> bool:
        return bool(self.flags & FLAG_UNDERRUN)

    @property
    def estimated(self) -> bool:
        return bool(self.flags & FLAG_ESTIMATED)

    @property
    def digital_silence(self) -> bool:
        return bool(self.flags & FLAG_DIGITAL_SILENCE)

    @property
    def end_sample(self) -> int:
        """First sample index after this packet (for cells and AFE records, in mic samples)."""
        return self.first_sample + self.frame_count * _FRAME_SAMPLES.get(self.kind, 1)

    def pcm(self) -> np.ndarray:
        """int16 samples; zeros for a digital-silence packet."""
        if self.kind in _FRAME_SAMPLES:
            raise TypeError("record packets carry no PCM")
        if self.digital_silence:
            return np.zeros(self.frame_count, dtype=np.int16)
        return np.frombuffer(self.payload, dtype="<i2").astype(np.int16, copy=False)

    def cells(self) -> CellRecords:
        if self.kind != Kind.CELLS:
            raise TypeError("not a cell packet")
        return parse_cells(self.payload)

    def afe(self) -> list[AfeRecord]:
        if self.kind != Kind.AFE:
            raise TypeError("not an afe packet")
        return afe_records(parse_afe(self.payload), self.first_sample)


def parse_cells(payload: bytes) -> CellRecords:
    if len(payload) % CELL_RECORD_BYTES:
        raise ProtocolError("malformed_frame", f"cell payload of {len(payload)} bytes is not whole records")
    rec = np.frombuffer(payload, dtype=CELL_RECORD)
    e = rec["e"].astype(np.int16)
    flags = rec["flags"].copy()
    mask = rec["mask"].copy()
    if e.size and (e.min() < CELL_E_MIN or e.max() > CELL_E_MAX):
        raise ProtocolError("malformed_frame", "cell E outside [-12000, 0]")
    if np.any(flags & ~np.uint8(_CELL_FLAG_BITS)):
        raise ProtocolError("malformed_frame", "cell flags set undefined bits")
    if np.any(mask & ~np.uint8(SOURCE_MASK_BITS)):
        raise ProtocolError("malformed_frame", "cell source mask sets undefined bits")
    gap = (flags & CELL_FLAG_GAP) != 0
    if np.any(e[gap] != CELL_E_MIN):
        raise ProtocolError("malformed_frame", "gap cell must carry E = -12000")
    return CellRecords(e, flags, mask)


def build_cells(e_centi: Iterable[int], flags: Iterable[int], mask: Iterable[int]) -> bytes:
    e = np.asarray(list(e_centi), dtype=np.int64)
    f = np.asarray(list(flags), dtype=np.int64)
    m = np.asarray(list(mask), dtype=np.int64)
    if not (e.size == f.size == m.size):
        raise ValueError("cell columns differ in length")
    rec = np.zeros(e.size, dtype=CELL_RECORD)
    rec["e"] = e
    rec["flags"] = f
    rec["mask"] = m
    data = rec.tobytes()
    parse_cells(data)
    return data


def parse_afe(payload: bytes) -> np.ndarray:
    """Validate a kind-5 payload (afe record v1, WIRE §4.2); returns its
    AFE_RECORD rows. A record out of bounds is a protocol error."""
    if len(payload) % AFE_RECORD_BYTES:
        raise ProtocolError("malformed_frame", f"afe payload of {len(payload)} bytes is not whole records")
    rec = np.frombuffer(payload, dtype=AFE_RECORD)
    frames = rec["frames"]
    if np.any(frames > AFE_PERIOD_FRAMES):
        raise ProtocolError("malformed_frame", f"afe record frames above {AFE_PERIOD_FRAMES}")
    if np.any(rec["flags"] & ~np.uint8(_AFE_FLAG_BITS)):
        raise ProtocolError("malformed_frame", "afe record flags set reserved bits")
    for name in ("playback", "erle_frames", "dtd_frames", "vad_frames"):
        if np.any(rec[name] > frames):
            raise ProtocolError("malformed_frame", f"afe record {name} above its frames")
    for name, bound in (("dtd_max", AFE_DTD_MAX), ("vad_max", AFE_VAD_MAX), ("volume", AFE_VOLUME_MAX)):
        if np.any(rec[name] > bound):
            raise ProtocolError("malformed_frame", f"afe record {name} above {bound}")
    return rec


def afe_records(rec: np.ndarray, first_sample: int) -> list[AfeRecord]:
    """Typed records of consecutive AFE_RECORD rows, the first at capture sample `first_sample`."""
    return [AfeRecord(first_sample + i * AFE_PERIOD_SAMPLES, *row) for i, row in enumerate(rec.tolist())]


def build_afe(records: Iterable[AfeRecord]) -> bytes:
    """A kind-5 payload of consecutive records; their `start` is the packet's
    first sample plus 1280 per record, so it is not encoded."""
    rows = [tuple(getattr(r, name) for name in _AFE_FIELDS) for r in records]
    data = np.array(rows, dtype=AFE_RECORD).tobytes()
    parse_afe(data)
    return data


def parse_packet(data: bytes | bytearray | memoryview) -> Packet:
    """Decode and statically validate one EMA1 frame (WIRE §3)."""
    data = bytes(data)
    if len(data) < HEADER_BYTES:
        raise ProtocolError("malformed_frame", f"frame of {len(data)} bytes is shorter than the header")
    (magic, kind, flags, channels, fmt, epoch, sequence, first, mono_ns, unc,
     rate, frames, generation, mask, payload_bytes) = HEADER.unpack_from(data)
    if magic != MAGIC:
        raise ProtocolError("malformed_frame", f"bad magic {magic!r}")
    try:
        kind = Kind(kind)
    except ValueError:
        raise ProtocolError("malformed_frame", f"unknown kind {kind}") from None
    if flags & ~_KNOWN_FLAGS:
        raise ProtocolError("malformed_frame", f"flags 0x{flags:02x} set undefined bits")
    silence = bool(flags & FLAG_DIGITAL_SILENCE)
    if silence and kind != Kind.REFERENCE:
        raise ProtocolError("malformed_frame", "digital silence is only defined for reference packets")
    if channels != 1:
        raise ProtocolError("malformed_frame", f"channels must be 1, got {channels}")
    if fmt != _FORMAT[kind]:
        raise ProtocolError("malformed_frame", f"format {fmt} is wrong for kind {kind.value}")
    if epoch == 0:
        raise ProtocolError("malformed_frame", "epoch 0 is reserved")
    if rate != _RATE[kind]:
        raise ProtocolError("malformed_frame", f"sample rate {rate} is wrong for kind {kind.value}")
    if not 1 <= frames <= MAX_FRAMES[kind]:
        raise ProtocolError("malformed_frame", f"frame count {frames} outside 1..{MAX_FRAMES[kind]}")
    if kind != Kind.RENDER and generation != 0:
        raise ProtocolError("malformed_frame", "render generation must be 0 outside kind 3")
    if kind != Kind.REFERENCE and mask != 0:
        raise ProtocolError("malformed_frame", "active-source mask must be 0 outside kind 2")
    if mask & ~SOURCE_MASK_BITS:
        raise ProtocolError("malformed_frame", f"source mask 0x{mask:x} sets undefined bits")
    if kind == Kind.CELLS:
        expected = frames * CELL_RECORD_BYTES
        if first % CELL_SAMPLES:
            raise ProtocolError("malformed_frame", "cell packet first sample is not a cell boundary")
    elif kind == Kind.AFE:
        expected = frames * AFE_RECORD_BYTES
        if first % AFE_PERIOD_SAMPLES:
            raise ProtocolError("malformed_frame", "afe packet first sample is not a period boundary")
    elif silence:
        expected = 0
    else:
        expected = frames * 2
    if payload_bytes != expected:
        raise ProtocolError("malformed_frame", f"payload bytes {payload_bytes}, expected {expected}")
    if len(data) != HEADER_BYTES + payload_bytes:
        raise ProtocolError("malformed_frame", f"frame is {len(data)} bytes, header says {HEADER_BYTES + payload_bytes}")
    end = first + frames * _FRAME_SAMPLES.get(kind, 1)
    if end > U64_MAX:
        raise ProtocolError("malformed_frame", "sample index overflows uint64")
    payload = data[HEADER_BYTES:]
    if kind == Kind.CELLS:
        parse_cells(payload)
    elif kind == Kind.AFE:
        parse_afe(payload)
    return Packet(kind, flags, epoch, sequence, first, mono_ns,
                  None if unc == UNCERTAINTY_UNKNOWN else unc,
                  rate, frames, generation, mask, payload)


def build_packet(
    kind: Kind,
    *,
    epoch: int,
    sequence: int,
    first_sample: int,
    pcm: np.ndarray | None = None,
    cells: bytes | None = None,
    afe: bytes | None = None,
    frame_count: int | None = None,
    flags: int = 0,
    mono_ns: int = 0,
    uncertainty_us: int | None = None,
    generation: int = 0,
    source_mask: int = 0,
) -> bytes:
    """Encode one EMA1 frame; the result is re-validated by `parse_packet`.

    PCM kinds take `pcm` (int16). A digital-silence reference packet takes
    `frame_count` and no PCM. Cell packets take `cells` from `build_cells`,
    AFE packets `afe` from `build_afe`.
    """
    kind = Kind(kind)
    if kind == Kind.CELLS:
        if cells is None:
            raise ValueError("cell packet needs cells")
        payload = bytes(cells)
        frames = len(payload) // CELL_RECORD_BYTES
    elif kind == Kind.AFE:
        if afe is None:
            raise ValueError("afe packet needs afe records")
        payload = bytes(afe)
        frames = len(payload) // AFE_RECORD_BYTES
    elif flags & FLAG_DIGITAL_SILENCE:
        if pcm is not None or frame_count is None:
            raise ValueError("digital-silence packet takes frame_count and no PCM")
        payload = b""
        frames = frame_count
    else:
        if pcm is None:
            raise ValueError("PCM packet needs pcm")
        arr = np.asarray(pcm)
        if arr.dtype != np.int16 or arr.ndim != 1:
            raise ValueError("pcm must be a 1-D int16 array")
        payload = arr.astype("<i2", copy=False).tobytes()
        frames = arr.size
    header = HEADER.pack(
        MAGIC, int(kind), flags, 1, _FORMAT[kind], epoch, sequence, first_sample, mono_ns,
        UNCERTAINTY_UNKNOWN if uncertainty_us is None else uncertainty_us,
        _RATE[kind], frames, generation, source_mask, len(payload),
    )
    frame = header + payload
    parse_packet(frame)
    return frame


# ---------------------------------------------------------------------------
# Interval sets
# ---------------------------------------------------------------------------

class IntervalSet:
    """Disjoint, merged half-open integer intervals in ascending order."""

    __slots__ = ("_starts", "_ends")

    def __init__(self) -> None:
        self._starts: list[int] = []
        self._ends: list[int] = []

    def __iter__(self) -> Iterator[tuple[int, int]]:
        return iter(zip(self._starts, self._ends))

    def __bool__(self) -> bool:
        return bool(self._starts)

    @property
    def lowest(self) -> int | None:
        return self._starts[0] if self._starts else None

    @property
    def highest(self) -> int | None:
        return self._ends[-1] if self._ends else None

    def overlapping(self, a: int, b: int) -> list[tuple[int, int]]:
        """The parts of this set inside `[a, b)`."""
        out: list[tuple[int, int]] = []
        i = max(bisect.bisect_right(self._ends, a), 0)
        while i < len(self._starts) and self._starts[i] < b:
            lo, hi = max(self._starts[i], a), min(self._ends[i], b)
            if lo < hi:
                out.append((lo, hi))
            i += 1
        return out

    def missing(self, a: int, b: int) -> list[tuple[int, int]]:
        """The parts of `[a, b)` not in this set."""
        out: list[tuple[int, int]] = []
        cur = a
        for lo, hi in self.overlapping(a, b):
            if lo > cur:
                out.append((cur, lo))
            cur = hi
        if cur < b:
            out.append((cur, b))
        return out

    def covers(self, a: int, b: int) -> bool:
        return a >= b or self.overlapping(a, b) == [(a, b)]

    def add(self, a: int, b: int) -> list[tuple[int, int]]:
        """Insert `[a, b)`; returns the sub-ranges that were not present."""
        if a >= b:
            return []
        new = self.missing(a, b)
        i = bisect.bisect_left(self._ends, a)
        j = bisect.bisect_right(self._starts, b)
        if i < j:
            a = min(a, self._starts[i])
            b = max(b, self._ends[j - 1])
        self._starts[i:j] = [a]
        self._ends[i:j] = [b]
        return new

    def trim_before(self, x: int) -> None:
        i = bisect.bisect_right(self._ends, x)
        del self._starts[:i]
        del self._ends[:i]
        if self._starts and self._starts[0] < x:
            self._starts[0] = x

    def clear(self) -> None:
        self._starts.clear()
        self._ends.clear()


# ---------------------------------------------------------------------------
# Sample timelines
# ---------------------------------------------------------------------------

class SampleTimeline:
    """One epoch of one int16 stream, first copy of each sample wins.

    Storage is a ring of `capacity` samples addressed by `index % capacity`;
    every retained sample lies in `[floor, frontier)` with
    `frontier − floor ≤ capacity`. Samples below `floor` are forgotten.
    """

    def __init__(self, capacity: int = PCM_CAPACITY):
        self.capacity = capacity
        self._buf: np.ndarray | None = None
        self.known = IntervalSet()
        self.silence = IntervalSet()      # digital-silence runs (known zeros)
        self.muted = IntervalSet()        # received with the muted flag: not valid audio
        self.discontinuities: list[int] = []
        self.floor = 0
        self.frontier: int | None = None

    @property
    def first_sample(self) -> int | None:
        return self.known.lowest

    @property
    def nbytes(self) -> int:
        return 0 if self._buf is None else self._buf.nbytes

    def _advance(self, end: int) -> None:
        if self.frontier is None or end > self.frontier:
            self.frontier = end
        if self.frontier - self.floor > self.capacity:
            self.trim_before(self.frontier - self.capacity)

    def _store(self, first: int, pcm: np.ndarray) -> None:
        if self._buf is None:
            self._buf = np.zeros(self.capacity, dtype=np.int16)
        n = pcm.size
        i = first % self.capacity
        head = min(n, self.capacity - i)
        self._buf[i:i + head] = pcm[:head]
        if head < n:
            self._buf[: n - head] = pcm[head:]

    def write(self, first: int, pcm: np.ndarray, *, silence: bool = False) -> list[tuple[int, int]]:
        """Store samples not yet known; returns the newly known ranges."""
        end = first + pcm.size
        if end <= self.floor:
            return []
        if self.frontier is not None and end - self.capacity > self.frontier:
            # A jump beyond the whole ring: everything retained is too old.
            self.trim_before(end - self.capacity)
        self._advance(end)
        lo = max(first, self.floor)
        new = self.known.missing(lo, end)
        for a, b in new:
            self._store(a, pcm[a - first:b - first])
            self.known.add(a, b)
            if silence:
                self.silence.add(a, b)
        return new

    def write_silence(self, first: int, n: int) -> list[tuple[int, int]]:
        return self.write(first, np.zeros(n, dtype=np.int16), silence=True)

    def mark_muted(self, first: int, end: int) -> None:
        if end > self.floor:
            self._advance(end)
            self.muted.add(max(first, self.floor), end)

    def mark_discontinuity(self, sample: int) -> None:
        if sample >= self.floor and sample not in self.discontinuities:
            bisect.insort(self.discontinuities, sample)

    def covers(self, a: int, b: int) -> bool:
        return a >= self.floor and self.known.covers(a, b)

    def missing(self, a: int, b: int) -> list[tuple[int, int]]:
        """Unknown sub-ranges of `[a, b)`, including anything below `floor`."""
        return self.known.missing(a, b)

    def is_silent(self, a: int, b: int) -> bool:
        """Every sample of `[a, b)` is known digital silence."""
        return self.silence.covers(a, b)

    def read(self, a: int, b: int) -> np.ndarray:
        """Copy of samples `[a, b)`. Raises unless every one is known."""
        if not self.covers(a, b):
            raise KeyError(f"samples [{a}, {b}) are not all known")
        out = np.empty(b - a, dtype=np.int16)
        if b == a:
            return out
        assert self._buf is not None
        i = a % self.capacity
        head = min(b - a, self.capacity - i)
        out[:head] = self._buf[i:i + head]
        if head < b - a:
            out[head:] = self._buf[: b - a - head]
        return out

    def discontinuities_in(self, a: int, b: int) -> list[int]:
        lo = bisect.bisect_left(self.discontinuities, a)
        hi = bisect.bisect_left(self.discontinuities, b)
        return self.discontinuities[lo:hi]

    def trim_before(self, sample: int) -> None:
        if sample <= self.floor:
            return
        self.floor = sample
        self.known.trim_before(sample)
        self.silence.trim_before(sample)
        self.muted.trim_before(sample)
        del self.discontinuities[: bisect.bisect_left(self.discontinuities, sample)]


class CellTimeline:
    """Cell records of one capture epoch, indexed by cell number k (mic
    samples `[512k, 512k + 512)`), first copy wins, ring of `capacity` cells."""

    def __init__(self, capacity: int = CELL_CAPACITY):
        self.capacity = capacity
        self._e = np.full(capacity, CELL_E_MIN, dtype=np.int16)
        self._flags = np.zeros(capacity, dtype=np.uint8)
        self._mask = np.zeros(capacity, dtype=np.uint8)
        self.known = IntervalSet()        # in cell numbers
        self.floor = 0                    # cell number
        self.frontier: int | None = None  # cell number

    @property
    def nbytes(self) -> int:
        return self._e.nbytes + self._flags.nbytes + self._mask.nbytes

    def write(self, first_sample: int, records: CellRecords) -> list[tuple[int, int]]:
        """Store records not yet known; returns new ranges in mic samples."""
        k0 = first_sample // CELL_SAMPLES
        k1 = k0 + len(records)
        if k1 <= self.floor:
            return []
        if self.frontier is None or k1 > self.frontier:
            self.frontier = k1
        if self.frontier - self.floor > self.capacity:
            self.trim_before_cell(self.frontier - self.capacity)
        new = self.known.missing(max(k0, self.floor), k1)
        for a, b in new:
            for src, dst in ((records.e_centi, self._e), (records.flags, self._flags), (records.mask, self._mask)):
                seg = src[a - k0:b - k0]
                idx = np.arange(a, b) % self.capacity
                dst[idx] = seg
            self.known.add(a, b)
        return [(a * CELL_SAMPLES, b * CELL_SAMPLES) for a, b in new]

    def covers(self, a_sample: int, b_sample: int) -> bool:
        a, b = _cell_range(a_sample, b_sample)
        return a >= self.floor and self.known.covers(a, b)

    def read(self, a_sample: int, b_sample: int) -> CellRecords:
        """Copy of the cells covering mic samples `[a, b)` (cell-aligned).
        Raises unless every cell is known."""
        a, b = _cell_range(a_sample, b_sample)
        if not self.covers(a_sample, b_sample):
            raise KeyError(f"cells [{a}, {b}) are not all known")
        idx = np.arange(a, b) % self.capacity
        return CellRecords(self._e[idx].copy(), self._flags[idx].copy(), self._mask[idx].copy())

    def trim_before(self, sample: int) -> None:
        self.trim_before_cell(sample // CELL_SAMPLES)

    def trim_before_cell(self, cell: int) -> None:
        if cell > self.floor:
            self.floor = cell
            self.known.trim_before(cell)


def _cell_range(a_sample: int, b_sample: int) -> tuple[int, int]:
    if a_sample % CELL_SAMPLES or b_sample % CELL_SAMPLES:
        raise ValueError("cell ranges must be cell-aligned")
    return a_sample // CELL_SAMPLES, b_sample // CELL_SAMPLES


class AfeTimeline:
    """AFE records of one capture epoch, indexed by period number k (capture
    samples `[1280k, 1280k + 1280)`), first copy wins, ring of `capacity`
    periods. A period nobody sent has no record — never a zero record."""

    def __init__(self, capacity: int = AFE_CAPACITY):
        self.capacity = capacity
        self._rec: np.ndarray | None = None   # AFE_RECORD ring, allocated with the first record
        self.known = IntervalSet()             # in period numbers
        self.floor = 0                         # period number
        self.frontier: int | None = None       # period number

    @property
    def nbytes(self) -> int:
        return 0 if self._rec is None else int(self._rec.nbytes)

    def write(self, first_sample: int, records: np.ndarray) -> list[tuple[int, int]]:
        """Store validated AFE_RECORD rows not yet known, the first at
        `first_sample`; returns new ranges in capture samples."""
        k0 = first_sample // AFE_PERIOD_SAMPLES
        k1 = k0 + int(records.size)
        if k1 <= self.floor:
            return []
        if self.frontier is None or k1 > self.frontier:
            self.frontier = k1
        if self.frontier - self.floor > self.capacity:
            self._trim_before_period(self.frontier - self.capacity)
        if self._rec is None:
            self._rec = np.zeros(self.capacity, dtype=AFE_RECORD)
        new = self.known.missing(max(k0, self.floor), k1)
        for a, b in new:
            self._rec[np.arange(a, b) % self.capacity] = records[a - k0:b - k0]
            self.known.add(a, b)
        return [(a * AFE_PERIOD_SAMPLES, b * AFE_PERIOD_SAMPLES) for a, b in new]

    def read(self, a_sample: int, b_sample: int) -> list[AfeRecord]:
        """The retained records whose periods overlap capture samples `[a, b)`,
        in order. A missing period simply has no record."""
        if self._rec is None or a_sample >= b_sample:
            return []
        a = max(a_sample // AFE_PERIOD_SAMPLES, self.floor)
        b = -(-b_sample // AFE_PERIOD_SAMPLES)
        out: list[AfeRecord] = []
        for lo, hi in self.known.overlapping(a, b):
            out.extend(afe_records(self._rec[np.arange(lo, hi) % self.capacity], lo * AFE_PERIOD_SAMPLES))
        return out

    def trim_before(self, sample: int) -> None:
        """Forget the periods that end at or before capture sample `sample`."""
        self._trim_before_period(sample // AFE_PERIOD_SAMPLES)

    def _trim_before_period(self, period: int) -> None:
        if period > self.floor:
            self.floor = period
            self.known.trim_before(period)

    def clear(self) -> None:
        """Erase every record (mute or session loss, with the lease's audio)."""
        self._rec = None
        self.known.clear()


class ReferenceView:
    """Read access to one reference epoch of a `ReferenceTimeline`, in that
    epoch's own sample indices."""

    def __init__(self, owner: "ReferenceTimeline", epoch: int):
        self._owner = owner
        self.epoch = epoch

    def _bounds(self) -> _Segment | None:
        return self._owner._segments.get(self.epoch)

    def _span(self, a: int, b: int) -> tuple[int, int] | None:
        seg = self._bounds()
        if seg is None:
            return None
        ga, gb = a + seg.delta, b + seg.delta
        if ga < seg.lo or gb > seg.hi:
            return None
        return ga, gb

    @property
    def first_sample(self) -> int | None:
        seg = self._bounds()
        if seg is None:
            return None
        parts = self._owner.store.known.overlapping(max(seg.lo, self._owner.store.floor), seg.hi)
        return None if not parts else parts[0][0] - seg.delta

    @property
    def frontier(self) -> int | None:
        seg = self._bounds()
        if seg is None:
            return None
        parts = self._owner.store.known.overlapping(max(seg.lo, self._owner.store.floor), seg.hi)
        return None if not parts else parts[-1][1] - seg.delta

    def covers(self, a: int, b: int) -> bool:
        g = self._span(a, b)
        return g is not None and self._owner.store.covers(*g)

    def is_silent(self, a: int, b: int) -> bool:
        g = self._span(a, b)
        return g is not None and self._owner.store.is_silent(*g)

    def read(self, a: int, b: int) -> np.ndarray:
        g = self._span(a, b)
        if g is None:
            raise KeyError(f"reference samples [{a}, {b}) are outside epoch {self.epoch}")
        return self._owner.store.read(*g)

    def missing(self, a: int, b: int) -> list[tuple[int, int]]:
        seg = self._bounds()
        if seg is None:
            return [(a, b)] if a < b else []
        delta = seg.delta
        out = []
        for x, y in self._owner.store.missing(a + delta, b + delta):
            out.append((x - delta, y - delta))
        return out

    def discontinuities_in(self, a: int, b: int) -> list[int]:
        seg = self._bounds()
        if seg is None:
            return []
        delta = seg.delta
        return [x - delta for x in self._owner.store.discontinuities_in(a + delta, b + delta)]


@dataclass(slots=True)
class _Segment:
    """One reference epoch's place in the ring: global = epoch sample + `delta`, valid in `[lo, hi)`."""

    delta: int
    lo: int
    hi: int


class ReferenceTimeline:
    """The lease's final-mix reference, bounded to `capacity` samples in total.

    The reference epoch restarts with the render epoch while a lease lives on.
    Every epoch is mapped affinely into one internal ring: a new epoch starts
    at the ring's current frontier, and an older epoch is closed at that point,
    so epochs never alias and memory never exceeds one ring."""

    def __init__(self, capacity: int = PCM_CAPACITY):
        self.capacity = capacity
        self.store = SampleTimeline(capacity)
        self._segments: collections.OrderedDict[int, _Segment] = collections.OrderedDict()

    @property
    def nbytes(self) -> int:
        return self.store.nbytes

    @property
    def epochs(self) -> tuple[int, ...]:
        return tuple(self._segments)

    @property
    def current_epoch(self) -> int | None:
        return next(reversed(self._segments), None)

    def segment(self, epoch: int) -> ReferenceView | None:
        return ReferenceView(self, epoch) if epoch in self._segments else None

    def write(self, epoch: int, first: int, pcm: np.ndarray | None, n: int,
              *, discontinuity: bool = False) -> list[tuple[int, int]]:
        """Store `n` samples at epoch index `first` (`pcm=None`: digital
        silence); returns newly known ranges in epoch indices."""
        seg = self._segments.get(epoch)
        if seg is None:
            base = 0 if self.store.frontier is None else self.store.frontier
            for other in self._segments.values():
                other.hi = min(other.hi, base)
            seg = self._segments[epoch] = _Segment(base - first, base, U64_MAX)
        delta = seg.delta
        g0, g1 = first + delta, first + delta + n
        a, b = max(g0, seg.lo), min(g1, seg.hi)
        if a >= b:
            return []
        if discontinuity:
            self.store.mark_discontinuity(a)
        if pcm is None:
            new = self.store.write_silence(a, b - a)
        else:
            new = self.store.write(a, pcm[a - g0:b - g0])
        self._drop_forgotten()
        return [(x - delta, y - delta) for x, y in new]

    def _drop_forgotten(self) -> None:
        while len(self._segments) > 1:
            epoch, seg = next(iter(self._segments.items()))
            if seg.hi > self.store.floor:
                break
            del self._segments[epoch]

    def trim_to(self, samples: int) -> None:
        """Keep at most the newest `samples` of reference."""
        if self.store.frontier is not None:
            self.store.trim_before(self.store.frontier - samples)
            self._drop_forgotten()


# ---------------------------------------------------------------------------
# Stream registry (stream.open / stream.end, WIRE §4.2)
# ---------------------------------------------------------------------------

class StreamId(enum.StrEnum):
    """Uplink stream ids (WIRE §4.2, §4.5). `afe` exists only in sessions
    whose session.ready granted `afe_metadata`; it runs on the capture epoch."""

    MIC = "mic"
    REFERENCE = "reference"
    CELLS = "cells"
    AFE = "afe"


_STREAM_KIND = {StreamId.MIC: Kind.MIC, StreamId.REFERENCE: Kind.REFERENCE, StreamId.CELLS: Kind.CELLS,
                StreamId.AFE: Kind.AFE}
_KIND_STREAM = {v: k for k, v in _STREAM_KIND.items()}


def _stream_id(value: object) -> StreamId:
    if not isinstance(value, str) or value not in StreamId:
        raise ProtocolError("bad_stream", f"unknown stream_id {value!r}")
    return StreamId(value)


@dataclass
class _EpochState:
    stream_id: StreamId
    epoch: int
    ended: bool = False
    final_sample: int | None = None
    last_sequence: int | None = None
    high: int | None = None             # highest first sample seen (the live position)
    backfills: int = 0                  # granted, not yet started backfill runs
    runs: list[int] = field(default_factory=list)   # end sample of each running backfill


class StreamRegistry:
    """Epochs announced by the device for the current session.

    A packet must name an open epoch of its stream, with a strictly increasing
    sequence. First samples advance, except that each lease that starts
    wanting the stream grants one backfill run: packets below the live
    position that start a new run or continue an existing one in sample order
    (possibly interleaved with live packets). Duplicate samples are then
    dropped by the timelines."""

    def __init__(self) -> None:
        self._epochs: dict[tuple[StreamId, int], _EpochState] = {}
        self._current: dict[StreamId, int] = {}

    def open(self, body: Mapping[str, object]) -> tuple[StreamId, int]:
        stream_id = _stream_id(body.get("stream_id"))
        kind = _STREAM_KIND[stream_id]
        epoch = parse_u64(body.get("epoch"), "epoch")
        if epoch == 0:
            raise ProtocolError("bad_stream", "epoch 0 is reserved")
        if body.get("kind") != int(kind) or body.get("sample_rate") != _RATE[kind] \
                or body.get("format") != _FORMAT[kind]:
            raise ProtocolError("bad_stream", f"stream.open parameters do not match {stream_id}")
        key = (stream_id, epoch)
        state = self._epochs.get(key)
        if state is not None and state.ended:
            raise ProtocolError("bad_stream", f"{stream_id} epoch {epoch} already ended")
        if state is None:
            self._epochs[key] = _EpochState(stream_id, epoch)
        self._current[stream_id] = epoch
        return stream_id, epoch

    def end(self, body: Mapping[str, object]) -> tuple[StreamId, int, int]:
        raw_id = body.get("stream_id")
        epoch = parse_u64(body.get("epoch"), "epoch")
        final = parse_u64(body.get("final_sample"), "final_sample")
        state = (self._epochs.get((StreamId(raw_id), epoch))
                 if isinstance(raw_id, str) and raw_id in StreamId else None)
        if state is None:
            raise ProtocolError("unknown_epoch", f"stream.end for unknown {raw_id} epoch {epoch}")
        stream_id = state.stream_id
        state.ended = True
        state.final_sample = final
        if self._current.get(stream_id) == epoch:
            del self._current[stream_id]
        return stream_id, epoch, final

    def current(self, stream_id: StreamId) -> int | None:
        return self._current.get(stream_id)

    def is_open(self, stream_id: StreamId, epoch: int) -> bool:
        state = self._epochs.get((stream_id, epoch))
        return state is not None and not state.ended

    def grant_backfill(self, stream_id: StreamId, epoch: int | None = None) -> None:
        """A lease starts wanting `stream_id`: allow one backfill run on the
        named epoch (or every open epoch of it, for the reference)."""
        for (sid, ep), state in self._epochs.items():
            # Before the epoch's first packet a backfill is simply its start.
            if sid == stream_id and not state.ended and state.high is not None \
                    and (epoch is None or ep == epoch):
                state.backfills += 1

    def check(self, packet: Packet) -> StreamId:
        """Validate a packet against its epoch; returns its stream id."""
        stream_id = _KIND_STREAM.get(packet.kind)
        if stream_id is None:
            raise ProtocolError("unexpected_kind", f"kind {int(packet.kind)} is not an uplink stream")
        state = self._epochs.get((stream_id, packet.epoch))
        if state is None:
            raise ProtocolError("unknown_epoch", f"{stream_id} epoch {packet.epoch} was never opened")
        if state.ended:
            raise ProtocolError("stream_ended", f"{stream_id} epoch {packet.epoch} already ended")
        if state.last_sequence is not None and packet.sequence <= state.last_sequence:
            raise ProtocolError("sequence", f"{stream_id} sequence {packet.sequence} after {state.last_sequence}")
        first = packet.first_sample
        if state.high is None or first >= state.high:
            state.runs = [end for end in state.runs if end < first]
            state.high = first
        else:
            continuing = [i for i, end in enumerate(state.runs) if end <= first]
            if continuing:
                i = max(continuing, key=lambda j: state.runs[j])
                state.runs[i] = packet.end_sample
            elif state.backfills > 0:
                state.backfills -= 1
                state.runs.append(packet.end_sample)
            else:
                raise ProtocolError("sample_order", f"{stream_id} first sample {first} goes back "
                                                    f"from {state.high} outside a backfill run")
        state.last_sequence = packet.sequence
        return stream_id

    def clear(self) -> None:
        self._epochs.clear()
        self._current.clear()


# ---------------------------------------------------------------------------
# Clock fit (§4.2, §4.3)
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class ClockFit:
    """`mono_ns ≈ t0_ns + (sample − s0) * ns_per_sample`, ± `uncertainty_ns`."""

    s0: int
    t0_ns: float
    ns_per_sample: float
    uncertainty_ns: float

    def to_mono(self, sample: float) -> float:
        return self.t0_ns + (sample - self.s0) * self.ns_per_sample

    def to_sample(self, mono_ns: float) -> float:
        return self.s0 + (mono_ns - self.t0_ns) / self.ns_per_sample


class StreamClock:
    """Anchors of one stream epoch: each packet's first sample and its
    estimated device-monotonic time. Least squares over the latest 10 s of
    samples, anchors with residual above 80 ms rejected; a fitted rate more
    than 1,000 ppm from nominal gives no fit (the device resets the epoch)."""

    def __init__(self) -> None:
        self._anchors: collections.deque[tuple[int, int, int]] = collections.deque()

    def add(self, packet: Packet) -> None:
        if packet.mono_ns == 0 or packet.uncertainty_us is None:
            return
        if self._anchors and packet.first_sample <= self._anchors[-1][0]:
            return  # backfill already anchored, or a re-sent sample
        self._anchors.append((packet.first_sample, packet.mono_ns, packet.uncertainty_us))
        horizon = packet.first_sample - CLOCK_WINDOW_SAMPLES
        while self._anchors and self._anchors[0][0] < horizon:
            self._anchors.popleft()

    def fit(self) -> ClockFit | None:
        if not self._anchors:
            return None
        s = np.array([a[0] for a in self._anchors], dtype=np.float64)
        t = np.array([a[1] for a in self._anchors], dtype=np.float64)
        u = np.array([a[2] for a in self._anchors], dtype=np.float64) * 1e3
        s0 = int(s[-1])
        if s.size == 1:
            return ClockFit(s0, float(t[-1]), NOMINAL_NS_PER_SAMPLE, float(u[-1]))
        keep = np.ones(s.size, dtype=bool)
        for _ in range(2):
            if keep.sum() < 2:
                return None
            slope, icpt = np.polyfit(s[keep] - s0, t[keep], 1)
            resid = t - (icpt + slope * (s - s0))
            new_keep = np.abs(resid) <= CLOCK_RESIDUAL_NS
            if np.array_equal(new_keep, keep):
                break
            keep = new_keep
        if keep.sum() < 2:
            return None
        if abs(slope / NOMINAL_NS_PER_SAMPLE - 1.0) * 1e6 > CLOCK_MAX_PPM:
            return None
        rms = float(np.sqrt(np.mean(resid[keep] ** 2)))
        return ClockFit(s0, float(icpt), float(slope), float(u[keep].max()) + rms)


@dataclass(frozen=True, slots=True)
class MappedSample:
    sample: float
    uncertainty_samples: float


class ClockMap:
    """Maps capture-epoch sample indices to reference-epoch indices and back
    through device monotonic time: an affine estimate with uncertainty."""

    def __init__(self, capture: ClockFit, reference: ClockFit):
        self.capture = capture
        self.reference = reference

    def _unc(self, fit_rate: float) -> float:
        return (self.capture.uncertainty_ns + self.reference.uncertainty_ns) / fit_rate

    def capture_to_reference(self, sample: float) -> MappedSample:
        mono = self.capture.to_mono(sample)
        return MappedSample(self.reference.to_sample(mono), self._unc(self.reference.ns_per_sample))

    def reference_to_capture(self, sample: float) -> MappedSample:
        mono = self.reference.to_mono(sample)
        return MappedSample(self.capture.to_sample(mono), self._unc(self.capture.ns_per_sample))


# ---------------------------------------------------------------------------
# Leases (§4.4, WIRE §4.5)
# ---------------------------------------------------------------------------

class LeaseReason(enum.StrEnum):
    CANDIDATE = "candidate"
    TURN = "turn"
    REPLY = "reply"
    DIAGNOSTIC = "diagnostic"


class AckState(enum.StrEnum):
    PENDING = "pending"
    ACCEPTED = "accepted"
    REJECTED = "rejected"


class LeaseEnd(enum.StrEnum):
    """Why a lease is over: an `uplink.close` reason (controller) or an `uplink.ended` reason (device)."""

    REJECTED = "rejected"
    ARBITRATION_LOST = "arbitration_lost"
    COMMITTED = "committed"
    CLOSED = "closed"
    TTL = "ttl"
    MUTE = "mute"
    EPOCH = "epoch"
    OVERRUN = "overrun"
    SESSION = "session"


LIVE = "live"
CLOSE_REASONS = frozenset({LeaseEnd.REJECTED, LeaseEnd.ARBITRATION_LOST, LeaseEnd.COMMITTED, LeaseEnd.CLOSED})
ENDED_REASONS = frozenset({LeaseEnd.CLOSED, LeaseEnd.TTL, LeaseEnd.MUTE, LeaseEnd.EPOCH, LeaseEnd.OVERRUN,
                           LeaseEnd.SESSION})


@dataclass
class Lease:
    lease_id: str
    reason: LeaseReason
    owner: str
    generation: int
    capture_epoch: int
    streams: dict[StreamId, int | None]   # stream → start (capture samples), None = live
    ttl_ms: int
    opened_at: float
    renewed_at: float
    ack: AckState | None = None         # candidate leases only
    candidate_id: str | None = None
    ended: LeaseEnd | None = None       # close/ended reason once over
    last_sample: dict[StreamId, int | None] = field(default_factory=dict)   # None: nothing sent
    clipped_start: dict[StreamId, int | None] = field(default_factory=dict)

    @property
    def active(self) -> bool:
        """Receiving audio: open, and acknowledged if it is a candidate lease."""
        return self.ended is None and self.ack in (None, AckState.ACCEPTED)

    def wants(self, stream_id: StreamId) -> bool:
        return self.active and stream_id in self.streams

    @property
    def expires_at(self) -> float:
        return self.renewed_at + self.ttl_ms / 1000.0


class LeaseCommand(enum.StrEnum):
    """The control message types of controller lease commands (WIRE §4.5)."""

    OPEN = "uplink.open"
    RENEW = "uplink.renew"
    CLOSE = "uplink.close"


class UplinkOpenBody(TypedDict):
    lease_id: str
    owner: str
    reason: LeaseReason
    streams: dict[StreamId, str]          # start as a u64 decimal string, or LIVE
    ttl_ms: int


class UplinkRenewBody(TypedDict):
    lease_id: str
    ttl_ms: int


class UplinkConvertBody(UplinkRenewBody):
    """The renewal that converts a candidate lease into the turn's lease."""

    reason: LeaseReason
    owner: str


class UplinkCloseBody(TypedDict):
    lease_id: str
    reason: LeaseEnd


@dataclass(frozen=True, slots=True)
class LeaseMessage:
    """A control message to send: envelope `type`, `generation`, `body`."""

    type: LeaseCommand
    generation: int
    body: UplinkOpenBody | UplinkRenewBody | UplinkConvertBody | UplinkCloseBody


def _round_down(x: int, grid: int) -> int:
    return max(0, x - x % grid)


class LeaseTable:
    """Controller-side record of the session's uplink leases."""

    def __init__(self, now: Callable[[], float] = time.monotonic):
        self._now = now
        self._leases: dict[str, Lease] = {}

    def __iter__(self) -> Iterator[Lease]:
        return iter(list(self._leases.values()))

    def get(self, lease_id: str) -> Lease | None:
        return self._leases.get(lease_id)

    def _live(self, lease_id: str) -> Lease:
        lease = self._leases.get(lease_id)
        if lease is None:
            raise LeaseError(f"unknown lease {lease_id}")
        if lease.ended is not None:
            raise LeaseError(f"lease {lease_id} already ended ({lease.ended})")
        return lease

    def open_candidate(self, lease_id: str, candidate_id: str, capture_epoch: int,
                       support_start: int, *, afe: bool = False) -> Lease:
        """Record the lease the device opened with `wake.candidate` (gen 1).
        `afe`: the session opted in to `afe_metadata_v1`, so the device's
        candidate lease also wants `afe` from its reference start (the AEC
        state before the wake), on the period grid (WIRE §4.5)."""
        if lease_id in self._leases:
            raise LeaseError(f"lease {lease_id} already exists")
        mic = _round_down(support_start - CANDIDATE_MIC_PREROLL, CELL_SAMPLES)
        streams: dict[StreamId, int | None] = {
            StreamId.MIC: mic,
            StreamId.CELLS: _round_down(mic - CANDIDATE_CELLS_LEAD, CELL_SAMPLES),
            StreamId.REFERENCE: max(0, mic - CANDIDATE_REFERENCE_LEAD),
        }
        if afe:
            streams[StreamId.AFE] = _round_down(mic - CANDIDATE_REFERENCE_LEAD, AFE_PERIOD_SAMPLES)
        now = self._now()
        lease = Lease(
            lease_id, LeaseReason.CANDIDATE, candidate_id, 1, capture_epoch, streams,
            LEASE_TTL_MS, now, now, ack=AckState.PENDING, candidate_id=candidate_id,
        )
        self._leases[lease_id] = lease
        return lease

    def acknowledge(self, lease_id: str, accepted: bool) -> Lease:
        """The controller's `command.ack` for the `wake.candidate`."""
        lease = self._live(lease_id)
        if lease.ack is not AckState.PENDING:
            raise LeaseError(f"lease {lease_id} is not awaiting acknowledgement")
        lease.ack = AckState.ACCEPTED if accepted else AckState.REJECTED
        lease.renewed_at = self._now()
        if not accepted:
            lease.ended = LeaseEnd.REJECTED
        return lease

    def open(self, lease_id: str, reason: LeaseReason, owner: str, capture_epoch: int,
             streams: Mapping[StreamId, int | None], ttl_ms: int = LEASE_TTL_MS) -> tuple[Lease, LeaseMessage]:
        """A controller-opened lease (`turn`, `reply`, `diagnostic`)."""
        reason = LeaseReason(reason)
        if reason is LeaseReason.CANDIDATE:
            raise LeaseError("candidate leases are opened by the device")
        if lease_id in self._leases:
            raise LeaseError(f"lease {lease_id} already exists")
        if not streams or any(sid not in StreamId for sid in streams):
            raise LeaseError(f"invalid streams {sorted(streams)}")
        starts: dict[StreamId, int | None] = {}
        for sid, start in streams.items():
            sid = StreamId(sid)
            if start is None:
                starts[sid] = None
            elif sid == StreamId.REFERENCE:
                starts[sid] = max(0, start)
            elif sid == StreamId.AFE:
                starts[sid] = _round_down(start, AFE_PERIOD_SAMPLES)
            else:
                starts[sid] = _round_down(start, CELL_SAMPLES)
        now = self._now()
        lease = Lease(lease_id, reason, owner, 1, capture_epoch, starts, ttl_ms, now, now)
        self._leases[lease_id] = lease
        body = UplinkOpenBody(
            lease_id=lease_id, owner=owner, reason=reason,
            streams={sid: LIVE if s is None else format_u64(s) for sid, s in starts.items()},
            ttl_ms=ttl_ms,
        )
        return lease, LeaseMessage(LeaseCommand.OPEN, lease.generation, body)

    def convert_to_turn(self, lease_id: str, owner: str) -> LeaseMessage:
        """Convert an accepted candidate lease into the turn's lease in place."""
        lease = self._live(lease_id)
        if lease.reason is not LeaseReason.CANDIDATE or lease.ack is not AckState.ACCEPTED:
            raise LeaseError(f"lease {lease_id} is not an accepted candidate lease")
        lease.reason = LeaseReason.TURN
        lease.owner = owner
        lease.generation += 1
        lease.renewed_at = self._now()
        return LeaseMessage(LeaseCommand.RENEW, lease.generation, UplinkConvertBody(
            lease_id=lease_id, ttl_ms=lease.ttl_ms, reason=LeaseReason.TURN, owner=owner))

    def renew(self, lease_id: str) -> LeaseMessage:
        lease = self._live(lease_id)
        now = self._now()
        if lease.reason is LeaseReason.DIAGNOSTIC and now - lease.opened_at >= DIAGNOSTIC_MAX_S:
            raise LeaseError(f"diagnostic lease {lease_id} reached {DIAGNOSTIC_MAX_S} s")
        if lease.ack is AckState.PENDING:
            raise LeaseError(f"candidate lease {lease_id} is not acknowledged")
        lease.renewed_at = now
        return LeaseMessage(LeaseCommand.RENEW, lease.generation, UplinkRenewBody(lease_id=lease_id, ttl_ms=lease.ttl_ms))

    def due_renewals(self) -> list[Lease]:
        """Leases whose once-a-second renewal is due."""
        now = self._now()
        return [l for l in self._leases.values()
                if l.active and now - l.renewed_at >= LEASE_RENEW_S
                and not (l.reason is LeaseReason.DIAGNOSTIC and now - l.opened_at >= DIAGNOSTIC_MAX_S)]

    def close(self, lease_id: str, reason: LeaseEnd) -> LeaseMessage:
        if reason not in CLOSE_REASONS:
            raise LeaseError(f"invalid close reason {reason!r}")
        lease = self._live(lease_id)
        lease.ended = reason
        return LeaseMessage(LeaseCommand.CLOSE, lease.generation, UplinkCloseBody(lease_id=lease_id, reason=reason))

    def device_ended(self, body: Mapping[str, object]) -> Lease | None:
        """Apply `uplink.ended`; returns the lease, or None if unknown."""
        lease_id = body.get("lease_id")
        lease = self._leases.get(lease_id) if isinstance(lease_id, str) else None
        if lease is None:
            return None
        reason = body.get("reason")
        if not isinstance(reason, str) or reason not in ENDED_REASONS:
            raise ProtocolError("malformed_message", f"uplink.ended reason {reason!r}")
        if lease.ended is None:
            lease.ended = LeaseEnd(reason)
        for key, target in (("last_sample", lease.last_sample), ("clipped_start", lease.clipped_start)):
            values = body.get(key) or {}
            if not isinstance(values, Mapping):
                raise ProtocolError("malformed_message", f"uplink.ended {key} must be an object")
            for sid, value in values.items():
                if isinstance(sid, str) and sid in StreamId:
                    target[StreamId(sid)] = None if value is None else parse_u64(value, f"{key}.{sid}")
        return lease

    def expire(self) -> list[Lease]:
        """End leases the device has certainly ended by TTL: past their TTL,
        or candidate leases still unacknowledged after 1 s."""
        now = self._now()
        out = []
        for lease in self._leases.values():
            if lease.ended is not None:
                continue
            if lease.ack is AckState.PENDING and now - lease.opened_at >= CANDIDATE_ACK_S:
                lease.ended = LeaseEnd.TTL
            elif now >= lease.expires_at:
                lease.ended = LeaseEnd.TTL
            else:
                continue
            out.append(lease)
        return out

    def end_all(self, reason: LeaseEnd) -> list[Lease]:
        """Mute, epoch change, or session loss: every open lease ends."""
        if reason not in ENDED_REASONS:
            raise LeaseError(f"invalid end reason {reason!r}")
        out = []
        for lease in self._leases.values():
            if lease.ended is None:
                lease.ended = reason
                out.append(lease)
        return out

    def forget(self, lease_id: str) -> None:
        self._leases.pop(lease_id, None)

    def wanting(self, stream_id: StreamId, epoch: int) -> list[Lease]:
        """Active leases that take this packet's stream/epoch. Mic, cells and
        AFE belong to the lease's capture epoch; the reference has its own epochs."""
        return [l for l in self._leases.values()
                if l.wants(stream_id) and (stream_id == StreamId.REFERENCE or l.capture_epoch == epoch)]


# ---------------------------------------------------------------------------
# Per-lease timeline
# ---------------------------------------------------------------------------

class LeaseTimeline:
    """Mic, reference, cells and AFE records a lease received. The open
    utterance (≤30 s) plus a rolling 10 s before it are kept; the rest ages
    out (§8.1)."""

    def __init__(self, lease: Lease):
        self.lease = lease
        self.mic = SampleTimeline(PCM_CAPACITY)
        self.cells = CellTimeline(CELL_CAPACITY)
        self.reference = ReferenceTimeline(PCM_CAPACITY)
        self.afe = AfeTimeline(AFE_CAPACITY)
        self.utterance_start: int | None = None

    @property
    def nbytes(self) -> int:
        return self.mic.nbytes + self.cells.nbytes + self.reference.nbytes + self.afe.nbytes

    def set_utterance_start(self, sample: int | None) -> None:
        """Retention anchor: an open utterance's first sample, or None."""
        self.utterance_start = sample
        self._retain()

    def trim_before(self, sample: int) -> None:
        """Forget mic before capture sample `sample`, AFE records before it
        less the reference's 2.5 s lead, cells before it less the 10 s lead
        that `B` needs, and the reference beyond what remains."""
        self.mic.trim_before(sample)
        self.afe.trim_before(max(0, sample - REFERENCE_EXTRA))
        self.cells.trim_before(max(0, sample - ROLLING_SAMPLES))
        if self.mic.frontier is not None:
            span = self.mic.frontier - self.mic.floor
            self.reference.trim_to(min(PCM_CAPACITY, span + REFERENCE_EXTRA))

    def _retain(self) -> None:
        if self.mic.frontier is None:
            return
        anchor = self.mic.frontier if self.utterance_start is None else self.utterance_start
        self.trim_before(max(0, anchor - ROLLING_SAMPLES))

    def store(self, stream_id: StreamId, packet: Packet) -> list[tuple[int, int]]:
        """Place a validated packet; returns newly known ranges."""
        start = self.lease.streams.get(stream_id)
        if stream_id == StreamId.MIC:
            first = packet.first_sample
            if packet.discontinuity:
                self.mic.mark_discontinuity(first)
            if packet.muted:
                self.mic.mark_muted(first, packet.end_sample)
                self._retain()
                return []
            pcm = packet.pcm()
            if start is not None and first < start:
                if packet.end_sample <= start:
                    return []
                pcm = pcm[start - first:]
                first = start
            new = self.mic.write(first, pcm)
        elif stream_id == StreamId.CELLS:
            records = packet.cells()
            first = packet.first_sample
            if start is not None and first < start:
                skip = (start - first + CELL_SAMPLES - 1) // CELL_SAMPLES
                if skip >= len(records):
                    return []
                records = CellRecords(records.e_centi[skip:], records.flags[skip:], records.mask[skip:])
                first += skip * CELL_SAMPLES
            new = self.cells.write(first, records)
        elif stream_id == StreamId.AFE:
            rows = parse_afe(packet.payload)
            first = packet.first_sample
            if start is not None and first < start:
                skip = (start - first + AFE_PERIOD_SAMPLES - 1) // AFE_PERIOD_SAMPLES
                if skip >= rows.size:
                    return []
                rows = rows[skip:]
                first += skip * AFE_PERIOD_SAMPLES
            new = self.afe.write(first, rows)
        else:
            reference = None if packet.digital_silence else packet.pcm()
            new = self.reference.write(packet.epoch, packet.first_sample, reference, packet.frame_count,
                                       discontinuity=packet.discontinuity)
        self._retain()
        return new


# ---------------------------------------------------------------------------
# Session: registry + leases + timelines + clocks
# ---------------------------------------------------------------------------

@dataclass(frozen=True, slots=True)
class Delivery:
    """New samples a packet contributed to one lease's timeline."""

    lease_id: str
    stream_id: StreamId
    epoch: int
    ranges: tuple[tuple[int, int], ...]
    packet: Packet


class UplinkSession:
    """Everything the controller keeps about one device session's uplink.
    `afe_metadata`: session.ready granted it (the device announced
    `afe_metadata_v1`), so the session carries the `afe` stream."""

    def __init__(self, now: Callable[[], float] = time.monotonic, *, afe_metadata: bool = False):
        self.afe_metadata = afe_metadata
        self.streams = StreamRegistry()
        self.leases = LeaseTable(now)
        self.timelines: dict[str, LeaseTimeline] = {}
        self._clocks: dict[tuple[Kind, int], StreamClock] = {}

    # -- stream messages -------------------------------------------------

    def stream_open(self, body: Mapping[str, object]) -> tuple[StreamId, int]:
        return self.streams.open(body)

    def stream_end(self, body: Mapping[str, object]) -> tuple[StreamId, int, int]:
        stream_id, epoch, final = self.streams.end(body)
        self._clocks.pop((_STREAM_KIND[stream_id], epoch), None)
        return stream_id, epoch, final

    # -- leases ----------------------------------------------------------

    def _attach(self, lease: Lease) -> None:
        self.timelines[lease.lease_id] = LeaseTimeline(lease)
        for sid in lease.streams:
            self.streams.grant_backfill(sid, None if sid == StreamId.REFERENCE else lease.capture_epoch)

    def candidate(self, lease_id: str, candidate_id: str, capture_epoch: int, support_start: int) -> Lease:
        return self.leases.open_candidate(lease_id, candidate_id, capture_epoch, support_start,
                                          afe=self.afe_metadata)

    def acknowledge(self, lease_id: str, accepted: bool) -> Lease:
        lease = self.leases.acknowledge(lease_id, accepted)
        if accepted:
            self._attach(lease)
        return lease

    def open(self, lease_id: str, reason: LeaseReason, owner: str, capture_epoch: int,
             streams: Mapping[StreamId, int | None], ttl_ms: int = LEASE_TTL_MS) -> LeaseMessage:
        lease, msg = self.leases.open(lease_id, reason, owner, capture_epoch, streams, ttl_ms)
        self._attach(lease)
        return msg

    def release(self, lease_id: str) -> None:
        """Drop an ended lease's timeline and record once the actor is done."""
        self.timelines.pop(lease_id, None)
        self.leases.forget(lease_id)

    def clear(self, reason: LeaseEnd) -> list[Lease]:
        """Mute or session loss: end every lease and erase buffered audio and
        AFE records (also where the actor still holds a lease's records)."""
        ended = self.leases.end_all(reason)
        for timeline in self.timelines.values():
            timeline.afe.clear()
        self.timelines.clear()
        if reason == LeaseEnd.SESSION:
            self.streams.clear()
            self._clocks.clear()
        return ended

    # -- audio -----------------------------------------------------------

    def ingest(self, frame: bytes | bytearray | memoryview) -> list[Delivery]:
        """Validate one uplink frame and place it in every lease that wants it.

        Raises ProtocolError for a WIRE violation. A valid packet no active
        lease wants is dropped (empty result)."""
        packet = parse_packet(frame)
        stream_id = self.streams.check(packet)
        clock = self._clocks.setdefault((packet.kind, packet.epoch), StreamClock())
        clock.add(packet)
        out: list[Delivery] = []
        for lease in self.leases.wanting(stream_id, packet.epoch):
            timeline = self.timelines.get(lease.lease_id)
            if timeline is None:
                continue
            ranges = timeline.store(stream_id, packet)
            if ranges or packet.discontinuity or packet.muted:
                out.append(Delivery(lease.lease_id, stream_id, packet.epoch, tuple(ranges), packet))
        return out

    def clock_fit(self, stream_id: StreamId, epoch: int) -> ClockFit | None:
        clock = self._clocks.get((_STREAM_KIND[stream_id], epoch))
        return None if clock is None else clock.fit()

    def clock_map(self, capture_epoch: int, reference_epoch: int) -> ClockMap | None:
        """Affine capture↔reference mapping, or None while either fit is unknown."""
        cap = self.clock_fit(StreamId.MIC, capture_epoch)
        ref = self.clock_fit(StreamId.REFERENCE, reference_epoch)
        if cap is None or ref is None:
            return None
        return ClockMap(cap, ref)


def ceil_to(x: int, grid: int) -> int:
    return -(-x // grid) * grid
