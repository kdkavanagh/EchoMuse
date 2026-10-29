//! Bounded storage for one epoch of one uplink stream.
//!
//! Each stream epoch's buffer keeps its newest records in a ring
//! addressed by position, with the metadata of the packet each run of records
//! came from and the ranges the device reported missing. The first copy of
//! every position wins: when two subscriptions overlap the device may send a
//! sample twice, and the second copy is not stored again
//! (`docs/protocol-v1.md` §5). Only the records a buffer keeps are copied out
//! of a packet.
//!
//! Positions outside this module are stream samples, the EMA1 header's
//! domain: capture samples for mic, cells and AFE records (cells on a 512-sample
//! grid, AFE records on 1,280), reference samples for the reference. Inside,
//! the ring works in record indices.

use std::collections::VecDeque;

use bytes::{BufMut, Bytes, BytesMut};
use echomuse_protocol::StreamId;
use echomuse_protocol::ema::{AfeRecord, Cell, CellFlags, Content, Flags, Packet, SourceMask};

use crate::chunk::{ChunkData, Pcm};
use crate::starts::record_samples;

/// Device monotonic nanoseconds per 16 kHz sample.
const SAMPLE_NS: u64 = 62_500;

/// A half-open range of stream samples, `[start, end)`, on one epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Span {
    /// The first sample.
    pub start: u64,
    /// The first sample after the range.
    pub end: u64,
}

impl Span {
    /// The range `[start, end)`, unless `end` is before `start`.
    pub fn new(start: u64, end: u64) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }

    /// How many stream samples it covers.
    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Whether it covers nothing.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

/// The timing and state the device stamped on a run of records: the header of
/// the packet that first brought them, carried forward to the run's first
/// record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockMeta {
    /// The header flags: muted, underrun, estimated timing and, on the
    /// reference, digital silence. Never the discontinuity flag, which is
    /// reported on its own.
    pub flags: Flags,
    /// The device `CLOCK_MONOTONIC` time of the run's first record, in ns.
    pub mono_ns: u64,
    /// The timing uncertainty in µs; `None` when the device did not know it.
    pub uncertainty_us: Option<u32>,
    /// The final mix's active sources (reference only; empty elsewhere).
    pub source_mask: SourceMask,
}

impl BlockMeta {
    pub(crate) fn of(packet: &Packet) -> Self {
        let header = packet.header();
        let flags = Flags::from_bits(header.flags.bits() & !Flags::DISCONTINUITY.bits())
            .unwrap_or(Flags::NONE);
        Self {
            flags,
            mono_ns: header.mono_ns,
            uncertainty_us: header.uncertainty_us,
            source_mask: header.source_mask,
        }
    }

    /// The same run, `records` records further on.
    fn advanced(self, records: u64, unit: u64) -> Self {
        Self {
            mono_ns: self
                .mono_ns
                .saturating_add(records.saturating_mul(unit * SAMPLE_NS)),
            ..self
        }
    }
}

/// What a stretch of a stream epoch holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentKind {
    /// Records the buffer holds.
    Data {
        /// Their timing and state.
        meta: BlockMeta,
        /// The device skipped missing audio just before this segment's first
        /// record.
        discontinuity: bool,
    },
    /// The device reported these positions missing: it skipped them and
    /// will never send them.
    Missing,
    /// Positions older than the buffer keeps.
    Evicted,
    /// Positions the buffer has neither received nor been told are missing.
    Unknown,
}

/// One ordered piece of a read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Segment {
    /// The positions it covers.
    pub span: Span,
    /// What they hold.
    pub kind: SegmentKind,
}

/// What one position holds, in stream samples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lookup {
    /// Held data with the same metadata up to `end`.
    Data {
        end: u64,
        meta: BlockMeta,
        discontinuity: bool,
    },
    /// Reported missing up to `end`.
    Missing { end: u64 },
    /// Older than the buffer keeps; the oldest kept position is `until`.
    Evicted { until: u64 },
    /// Not known; `next_known` is the next position that is held or known
    /// missing.
    Unknown { next_known: Option<u64> },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum BlockKind {
    Data {
        meta: BlockMeta,
        discontinuity: bool,
    },
    Missing,
}

#[derive(Clone, Copy, Debug)]
struct Block {
    first: u64,
    end: u64,
    kind: BlockKind,
}

/// The block bookkeeping, in record indices.
#[derive(Debug)]
struct BlockIndex {
    unit: u64,
    capacity: u64,
    max_blocks: usize,
    /// Sorted, non-overlapping.
    blocks: VecDeque<Block>,
    /// One past the highest record known.
    end: u64,
    /// Nothing below this record is held.
    floor: u64,
}

impl BlockIndex {
    fn advance(&mut self, end: u64) {
        self.end = self.end.max(end);
        self.raise_floor(self.end.saturating_sub(self.capacity));
    }

    fn raise_floor(&mut self, floor: u64) {
        if floor <= self.floor {
            return;
        }
        self.floor = floor;
        while self.blocks.front().is_some_and(|b| b.end <= floor) {
            self.blocks.pop_front();
        }
        if let Some(front) = self.blocks.front_mut()
            && front.first < floor
        {
            if let BlockKind::Data {
                meta,
                discontinuity,
            } = &mut front.kind
            {
                *meta = meta.advanced(floor - front.first, self.unit);
                *discontinuity = false;
            }
            front.first = floor;
        }
    }

    fn enforce_block_limit(&mut self) {
        while self.blocks.len() > self.max_blocks {
            if let Some(oldest) = self.blocks.pop_front() {
                self.floor = self.floor.max(oldest.end);
            }
        }
    }

    /// The first block that ends after `pos`.
    fn after(&self, pos: u64) -> usize {
        self.blocks.partition_point(|b| b.end <= pos)
    }

    /// The parts of `[first, end)` no block covers; with `data_only`, missing
    /// blocks do not count as covering.
    fn uncovered(&self, first: u64, end: u64, data_only: bool, out: &mut Vec<(u64, u64)>) {
        out.clear();
        let mut cur = first;
        for b in self.blocks.range(self.after(first)..) {
            if b.first >= end {
                break;
            }
            if data_only && b.kind == BlockKind::Missing {
                continue;
            }
            if b.first > cur {
                out.push((cur, b.first));
            }
            cur = cur.max(b.end);
        }
        if cur < end {
            out.push((cur, end));
        }
    }

    fn insert(&mut self, block: Block) {
        let at = self.blocks.partition_point(|b| b.first < block.first);
        self.blocks.insert(at, block);
    }

    /// Removes the missing marks inside `[first, end)`, which data now covers.
    fn cut_missing(&mut self, first: u64, end: u64) {
        let mut at = self.after(first);
        while let Some(b) = self.blocks.get(at).copied() {
            if b.first >= end {
                break;
            }
            self.blocks.remove(at);
            if b.end > end {
                self.blocks.insert(at, Block { first: end, ..b });
            }
            if b.first < first {
                self.blocks.insert(at, Block { end: first, ..b });
                at += 1;
            }
        }
    }

    /// Records `[first, end)` as held with `meta`, except what is already
    /// held or too old; returns the newly held ranges in `novel`.
    fn insert_data(
        &mut self,
        first: u64,
        end: u64,
        meta: BlockMeta,
        discontinuity: bool,
        novel: &mut Vec<(u64, u64)>,
    ) {
        self.advance(end);
        let from = first.max(self.floor);
        if from >= end {
            novel.clear();
            return;
        }
        self.uncovered(from, end, true, novel);
        for &(a, b) in novel.iter() {
            self.cut_missing(a, b);
            self.insert(Block {
                first: a,
                end: b,
                kind: BlockKind::Data {
                    meta: meta.advanced(a - first, self.unit),
                    discontinuity: discontinuity && a == first,
                },
            });
        }
        self.enforce_block_limit();
    }

    /// Records `[first, end)` as missing wherever nothing is known yet.
    fn insert_missing(&mut self, first: u64, end: u64, scratch: &mut Vec<(u64, u64)>) {
        self.advance(end);
        let from = first.max(self.floor);
        if from >= end {
            return;
        }
        self.uncovered(from, end, false, scratch);
        for &(a, b) in scratch.iter() {
            self.insert(Block {
                first: a,
                end: b,
                kind: BlockKind::Missing,
            });
        }
        self.enforce_block_limit();
    }

    fn lookup(&self, pos: u64) -> Lookup {
        if pos < self.floor {
            return Lookup::Evicted { until: self.floor };
        }
        let at = self.after(pos);
        match self.blocks.get(at) {
            Some(b) if b.first <= pos => match b.kind {
                BlockKind::Data {
                    meta,
                    discontinuity,
                } => Lookup::Data {
                    end: b.end,
                    meta: meta.advanced(pos - b.first, self.unit),
                    discontinuity: discontinuity && pos == b.first,
                },
                BlockKind::Missing => Lookup::Missing { end: b.end },
            },
            next => Lookup::Unknown {
                next_known: next.map(|b| b.first),
            },
        }
    }
}

/// The records themselves, sized to the ring's capacity on first use.
#[derive(Debug)]
enum Records {
    Pcm(Vec<i16>),
    Cells(Vec<Cell>),
    Afe(Vec<AfeRecord>),
}

/// One stream epoch's retained records.
#[derive(Debug)]
pub(crate) struct EpochBuffer {
    index: BlockIndex,
    records: Records,
    scratch: Vec<(u64, u64)>,
}

/// A gap cell, the ring's filler for cells it has not been sent.
const GAP_CELL: Cell = Cell {
    e_centi: Cell::E_MIN,
    flags: CellFlags::GAP,
    mask: SourceMask::NONE,
};

impl EpochBuffer {
    /// A buffer keeping the newest `retention` stream samples of `stream`
    /// (rounded down to whole records, at least one) in at most `max_blocks`
    /// runs of distinct metadata.
    pub(crate) fn new(stream: StreamId, retention: u64, max_blocks: usize) -> Self {
        let unit = record_samples(stream);
        let records = match stream {
            StreamId::Mic | StreamId::Reference => Records::Pcm(Vec::new()),
            StreamId::Cells => Records::Cells(Vec::new()),
            StreamId::Afe => Records::Afe(Vec::new()),
        };
        Self {
            index: BlockIndex {
                unit,
                capacity: (retention / unit).max(1),
                max_blocks: max_blocks.max(1),
                blocks: VecDeque::new(),
                end: 0,
                floor: 0,
            },
            records,
            scratch: Vec::new(),
        }
    }

    fn unit(&self) -> u64 {
        self.index.unit
    }

    /// One past the highest position known, in stream samples.
    pub(crate) fn end(&self) -> u64 {
        self.index.end * self.unit()
    }

    /// Stores the records of `packet` this buffer does not hold yet and
    /// returns how many stream samples that was.
    pub(crate) fn store(&mut self, packet: &Packet) -> u64 {
        let header = packet.header();
        let unit = self.unit();
        let first = header.first_sample / unit;
        let end = first + u64::from(header.frame_count);
        let mut novel = std::mem::take(&mut self.scratch);
        self.index.insert_data(
            first,
            end,
            BlockMeta::of(packet),
            header.flags.contains(Flags::DISCONTINUITY),
            &mut novel,
        );
        let capacity = self.index.capacity;
        let stored: u64 = novel.iter().map(|(a, b)| b - a).sum();
        if stored > 0 {
            match (&mut self.records, packet.content()) {
                (Records::Pcm(ring), Content::Pcm(samples)) => {
                    write(ring, capacity, 0, first, samples, &novel);
                }
                (Records::Pcm(ring), Content::DigitalSilence { frames }) => {
                    let zeros = std::iter::repeat_n(0, frames as usize);
                    write(ring, capacity, 0, first, zeros, &novel);
                }
                (Records::Cells(ring), Content::Cells(cells)) => {
                    write(ring, capacity, GAP_CELL, first, cells, &novel);
                }
                (Records::Afe(ring), Content::Afe(records)) => {
                    write(ring, capacity, AfeRecord::default(), first, records, &novel);
                }
                // Buffers are keyed by the stream a packet's kind names, so
                // the content always matches.
                _ => {}
            }
        }
        self.scratch = novel;
        stored * unit
    }

    /// Records that the device skipped `span` and will never send it.
    pub(crate) fn mark_missing(&mut self, span: Span) {
        let unit = self.unit();
        let mut scratch = std::mem::take(&mut self.scratch);
        self.index
            .insert_missing(span.start / unit, span.end.div_ceil(unit), &mut scratch);
        self.scratch = scratch;
    }

    /// What `pos` holds, in stream samples.
    pub(crate) fn lookup(&self, pos: u64) -> Lookup {
        let unit = self.unit();
        match self.index.lookup(pos / unit) {
            Lookup::Data {
                end,
                meta,
                discontinuity,
            } => Lookup::Data {
                end: end * unit,
                meta,
                discontinuity,
            },
            Lookup::Missing { end } => Lookup::Missing { end: end * unit },
            Lookup::Evicted { until } => Lookup::Evicted {
                until: until * unit,
            },
            Lookup::Unknown { next_known } => Lookup::Unknown {
                next_known: next_known.map(|n| n * unit),
            },
        }
    }

    /// The ordered pieces of `span`, whose ends must be on the record grid.
    pub(crate) fn segments(&self, span: Span) -> Vec<Segment> {
        let mut out = Vec::new();
        let mut pos = span.start;
        while pos < span.end {
            let (end, kind) = match self.lookup(pos) {
                Lookup::Data {
                    end,
                    meta,
                    discontinuity,
                } => (
                    end,
                    SegmentKind::Data {
                        meta,
                        discontinuity,
                    },
                ),
                Lookup::Missing { end } => (end, SegmentKind::Missing),
                Lookup::Evicted { until } => (until, SegmentKind::Evicted),
                Lookup::Unknown { next_known } => {
                    (next_known.unwrap_or(span.end), SegmentKind::Unknown)
                }
            };
            let end = end.min(span.end);
            out.push(Segment {
                span: Span { start: pos, end },
                kind,
            });
            pos = end;
        }
        out
    }

    fn slots(&self, span: Span) -> impl Iterator<Item = usize> {
        let unit = self.unit();
        let capacity = self.index.capacity;
        (span.start / unit..span.end / unit).map(move |i| (i % capacity) as usize)
    }

    /// Appends the PCM of a held `span` to `out`.
    pub(crate) fn copy_pcm(&self, span: Span, out: &mut Vec<i16>) {
        if let Records::Pcm(ring) = &self.records {
            out.extend(self.slots(span).map(|i| ring[i]));
        }
    }

    /// Appends the cells of a held `span` to `out`.
    pub(crate) fn copy_cells(&self, span: Span, out: &mut Vec<Cell>) {
        if let Records::Cells(ring) = &self.records {
            out.extend(self.slots(span).map(|i| ring[i]));
        }
    }

    /// Appends the AFE records of a held `span` to `out`.
    pub(crate) fn copy_afe(&self, span: Span, out: &mut Vec<AfeRecord>) {
        if let Records::Afe(ring) = &self.records {
            out.extend(self.slots(span).map(|i| ring[i]));
        }
    }

    /// The records of a held `span` for a chunk. PCM inside `packet` is a
    /// slice of the packet's payload rather than a copy; digital silence
    /// carries no samples.
    pub(crate) fn chunk_data(
        &self,
        span: Span,
        meta: &BlockMeta,
        packet: Option<&Packet>,
    ) -> ChunkData {
        let unit = self.unit();
        match &self.records {
            Records::Pcm(ring) => {
                if meta.flags.contains(Flags::DIGITAL_SILENCE) {
                    return ChunkData::DigitalSilence { frames: span.len() };
                }
                if let Some(p) = packet {
                    let h = p.header();
                    let silent = h.flags.contains(Flags::DIGITAL_SILENCE);
                    if !silent && h.first_sample <= span.start && span.end <= p.end_sample() {
                        let from = ((span.start - h.first_sample) * 2) as usize;
                        let to = ((span.end - h.first_sample) * 2) as usize;
                        return ChunkData::Pcm(Pcm::from_le_bytes(p.payload().slice(from..to)));
                    }
                }
                let mut bytes = BytesMut::with_capacity((span.len() * 2) as usize);
                for i in self.slots(span) {
                    bytes.put_i16_le(ring[i]);
                }
                ChunkData::Pcm(Pcm::from_le_bytes(Bytes::from(bytes)))
            }
            Records::Cells(_) => {
                let mut cells = Vec::with_capacity((span.len() / unit) as usize);
                self.copy_cells(span, &mut cells);
                ChunkData::Cells(cells)
            }
            Records::Afe(_) => {
                let mut records = Vec::with_capacity((span.len() / unit) as usize);
                self.copy_afe(span, &mut records);
                ChunkData::Afe(records)
            }
        }
    }
}

/// Writes the values of the novel record ranges, skipping the rest.
fn write<T: Copy>(
    ring: &mut Vec<T>,
    capacity: u64,
    fill: T,
    first: u64,
    values: impl Iterator<Item = T>,
    novel: &[(u64, u64)],
) {
    if ring.is_empty() {
        ring.resize(capacity as usize, fill);
    }
    let mut ranges = novel.iter().peekable();
    for (i, value) in values.enumerate() {
        let pos = first + i as u64;
        while ranges.peek().is_some_and(|(_, end)| pos >= *end) {
            ranges.next();
        }
        match ranges.peek() {
            Some((start, _)) if pos >= *start => ring[(pos % capacity) as usize] = value,
            Some(_) => {}
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{mic, ramp};

    fn kinds(buffer: &EpochBuffer, start: u64, end: u64) -> Vec<(u64, u64, &'static str)> {
        buffer
            .segments(Span { start, end })
            .into_iter()
            .map(|s| {
                let name = match s.kind {
                    SegmentKind::Data { .. } => "data",
                    SegmentKind::Missing => "missing",
                    SegmentKind::Evicted => "evicted",
                    SegmentKind::Unknown => "unknown",
                };
                (s.span.start, s.span.end, name)
            })
            .collect()
    }

    #[test]
    fn retention_keeps_only_the_newest_records() {
        let mut b = EpochBuffer::new(StreamId::Mic, 4_000, 64);
        for k in 0..5u64 {
            let first = k * 1_280;
            assert_eq!(
                b.store(&mic(7, k, first, &ramp(first, 1_280), Flags::NONE)),
                1_280
            );
        }
        // 6,400 samples seen, 4,000 kept: everything before 2,400 is gone.
        assert_eq!(b.end(), 6_400);
        assert_eq!(
            kinds(&b, 0, 6_400),
            vec![
                (0, 2_400, "evicted"),
                (2_400, 2_560, "data"),
                (2_560, 3_840, "data"),
                (3_840, 5_120, "data"),
                (5_120, 6_400, "data")
            ]
        );
        let mut out = Vec::new();
        b.copy_pcm(
            Span {
                start: 2_400,
                end: 6_400,
            },
            &mut out,
        );
        assert_eq!(out, ramp(2_400, 4_000));
        // The trimmed block's timing moved with its first record.
        let Lookup::Data { meta, .. } = b.lookup(2_400) else {
            panic!("held")
        };
        assert_eq!(meta.mono_ns, crate::testutil::mono(2_400));
        // A packet entirely older than the ring stores nothing.
        assert_eq!(b.store(&mic(7, 9, 0, &ramp(0, 1_280), Flags::NONE)), 0);
    }

    #[test]
    fn the_first_copy_wins_and_overlaps_store_only_new_samples() {
        let mut b = EpochBuffer::new(StreamId::Mic, 100_000, 64);
        b.store(&mic(7, 0, 1_280, &[5; 1_280], Flags::NONE));
        // A backfill packet overlapping the held range carries other values;
        // only its first 640 samples are new.
        let stored = b.store(&mic(7, 1, 640, &[9; 1_280], Flags::NONE));
        assert_eq!(stored, 640);
        let mut out = Vec::new();
        b.copy_pcm(
            Span {
                start: 640,
                end: 2_560,
            },
            &mut out,
        );
        assert!(out[..640].iter().all(|&s| s == 9));
        assert!(out[640..].iter().all(|&s| s == 5));
    }

    #[test]
    fn gaps_missing_ranges_and_block_limits_read_in_order() {
        let mut b = EpochBuffer::new(StreamId::Mic, 100_000, 4);
        b.store(&mic(7, 0, 0, &ramp(0, 512), Flags::NONE));
        b.mark_missing(Span {
            start: 512,
            end: 1_024,
        });
        b.store(&mic(7, 1, 1_536, &ramp(1_536, 512), Flags::DISCONTINUITY));
        assert_eq!(
            kinds(&b, 0, 2_560),
            vec![
                (0, 512, "data"),
                (512, 1_024, "missing"),
                (1_024, 1_536, "unknown"),
                (1_536, 2_048, "data"),
                (2_048, 2_560, "unknown")
            ]
        );
        let segs = b.segments(Span {
            start: 1_536,
            end: 1_600,
        });
        assert!(matches!(
            segs[0].kind,
            SegmentKind::Data {
                discontinuity: true,
                ..
            }
        ));
        // Data for a range marked missing replaces the mark.
        b.store(&mic(7, 2, 768, &ramp(768, 256), Flags::NONE));
        assert_eq!(
            kinds(&b, 0, 1_536),
            vec![
                (0, 512, "data"),
                (512, 768, "missing"),
                (768, 1_024, "data"),
                (1_024, 1_536, "unknown")
            ]
        );
        // A fifth run exceeds four blocks: the oldest goes, and the floor rises.
        b.store(&mic(7, 3, 4_096, &ramp(4_096, 512), Flags::NONE));
        assert_eq!(b.lookup(100), Lookup::Evicted { until: 512 });
    }
}
