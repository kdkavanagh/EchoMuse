//! EMA1 audio frames (docs/protocol-v1.md §3, spec §3.1.6): a 64-byte
//! little-endian header followed by PCM16 samples, cell records or AFE
//! records. The rules match the firmware's `device/internal/audio/ema`.
//!
//! [`Packet::parse`] validates one whole frame and keeps its payload as a
//! slice of the received buffer, without copying it. [`encode_render`]
//! builds the only kind the controller sends, kind 3.

use std::fmt;
use std::iter::FusedIterator;
use std::ops::BitOr;
use std::slice;

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{ProtocolError, ProtocolErrorCode};

/// The fixed header length in bytes.
pub const HEADER_BYTES: usize = 64;
/// The ASCII tag at offset 0.
pub const MAGIC: [u8; 4] = *b"EMA1";
/// The timing uncertainty value that means "unknown".
pub const UNCERTAINTY_UNKNOWN: u32 = 0xFFFF_FFFF;
/// The sample rate of mic, reference, cells and AFE packets.
pub const CAPTURE_RATE: u32 = 16_000;
/// The sample rate of render packets.
pub const RENDER_RATE: u32 = 48_000;
/// Capture samples one cell record covers.
pub const CELL_SAMPLES: u64 = 512;
/// Capture samples one AFE record covers: one capture period.
pub const AFE_PERIOD_SAMPLES: u64 = 1_280;
/// Bytes per cell record.
pub const CELL_RECORD_BYTES: usize = 4;
/// Bytes per AFE record.
pub const AFE_RECORD_BYTES: usize = 14;

/// What a frame carries (offset 4).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Kind {
    /// Capture PCM, uplink.
    Mic = 1,
    /// The 16 kHz final-mix reference, uplink.
    Reference = 2,
    /// Render source PCM at 48 kHz, downlink.
    Render = 3,
    /// Loudness cell records, uplink.
    Cells = 4,
    /// AFE period records, uplink, only in `afe_metadata` sessions.
    Afe = 5,
}

impl Kind {
    /// The kind numbered `value`, if any.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Kind::Mic),
            2 => Some(Kind::Reference),
            3 => Some(Kind::Render),
            4 => Some(Kind::Cells),
            5 => Some(Kind::Afe),
            _ => None,
        }
    }

    /// The only payload format this kind may carry.
    pub const fn format(self) -> Format {
        match self {
            Kind::Cells => Format::CellV1,
            Kind::Afe => Format::AfeV1,
            Kind::Mic | Kind::Reference | Kind::Render => Format::Pcm16,
        }
    }

    /// The only sample rate this kind may carry.
    pub const fn sample_rate(self) -> u32 {
        match self {
            Kind::Render => RENDER_RATE,
            _ => CAPTURE_RATE,
        }
    }

    /// The most frames one packet of this kind may carry.
    pub const fn max_frames(self) -> u32 {
        match self {
            Kind::Mic | Kind::Reference => 1_280,
            Kind::Render => 3_840,
            Kind::Cells => 320,
            Kind::Afe => 125,
        }
    }

    /// The stream samples one frame of this kind stands for: one for PCM,
    /// 512 for a cell, 1,280 for an AFE record.
    pub const fn samples_per_frame(self) -> u64 {
        match self {
            Kind::Cells => CELL_SAMPLES,
            Kind::Afe => AFE_PERIOD_SAMPLES,
            Kind::Mic | Kind::Reference | Kind::Render => 1,
        }
    }

    /// The exact payload length of `frames` frames with `flags`.
    pub const fn payload_bytes(self, flags: Flags, frames: u32) -> u64 {
        let frames = frames as u64;
        match self {
            Kind::Cells => frames * CELL_RECORD_BYTES as u64,
            Kind::Afe => frames * AFE_RECORD_BYTES as u64,
            _ if flags.contains(Flags::DIGITAL_SILENCE) => 0,
            _ => frames * 2,
        }
    }
}

/// The payload encoding (offset 7).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum Format {
    /// Signed PCM16 little-endian, kinds 1–3.
    Pcm16 = 1,
    /// Cell record v1, kind 4.
    CellV1 = 2,
    /// AFE record v1, kind 5.
    AfeV1 = 3,
}

/// Header flag bits (offset 5).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Flags(u8);

impl Flags {
    /// No flag set.
    pub const NONE: Flags = Flags(0);
    /// Audio before this packet is missing.
    pub const DISCONTINUITY: Flags = Flags(1 << 0);
    /// The capture was privacy-muted.
    pub const MUTED: Flags = Flags(1 << 1);
    /// The source ran dry.
    pub const UNDERRUN: Flags = Flags(1 << 2);
    /// The timing is estimated.
    pub const ESTIMATED: Flags = Flags(1 << 3);
    /// Every sample is zero and the payload is omitted (kind 2 only).
    pub const DIGITAL_SILENCE: Flags = Flags(1 << 4);
    const KNOWN: u8 = 0b1_1111;

    /// The flags of `bits`, or `None` when an undefined bit is set.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(Flags(bits))
        } else {
            None
        }
    }

    /// The wire byte.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

/// The final mix's active sources (header offset 56 on kind 2, cell byte 3).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct SourceMask(u8);

impl SourceMask {
    /// Nothing playing.
    pub const NONE: SourceMask = SourceMask(0);
    /// Content playback.
    pub const CONTENT: SourceMask = SourceMask(1);
    /// An alert.
    pub const ALERT: SourceMask = SourceMask(2);
    /// Dialog output.
    pub const DIALOG: SourceMask = SourceMask(4);
    /// An earcon.
    pub const EARCON: SourceMask = SourceMask(8);
    const KNOWN: u32 = 0b1111;

    /// The mask of `bits`, or `None` when an undefined bit is set.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(SourceMask(bits as u8))
        } else {
            None
        }
    }

    /// The mask's bits.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every source of `other` is active.
    pub const fn contains(self, other: SourceMask) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for SourceMask {
    type Output = SourceMask;
    fn bitor(self, rhs: SourceMask) -> SourceMask {
        SourceMask(self.0 | rhs.0)
    }
}

/// One EMA1 header. Channels (always 1), format and sample rate follow from
/// `kind`, so they are checked on decode and written on encode rather than
/// stored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header {
    /// What the frame carries.
    pub kind: Kind,
    /// Header flags.
    pub flags: Flags,
    /// The stream epoch, never 0.
    pub epoch: u64,
    /// The packet's sequence within its epoch.
    pub sequence: u64,
    /// The first sample's index in the epoch; for cells and AFE records the
    /// first record's start sample.
    pub first_sample: u64,
    /// The first sample's device `CLOCK_MONOTONIC` time in ns; 0 downlink.
    pub mono_ns: u64,
    /// The timing uncertainty in µs; `None` when unknown.
    pub uncertainty_us: Option<u32>,
    /// Samples, cells or AFE records in the payload.
    pub frame_count: u32,
    /// The playback generation on kind 3; 0 on every other kind.
    pub generation: u32,
    /// The active sources on kind 2; empty on every other kind.
    pub source_mask: SourceMask,
}

impl Header {
    /// The exact payload length this header calls for.
    pub const fn payload_bytes(&self) -> u64 {
        self.kind.payload_bytes(self.flags, self.frame_count)
    }

    /// The first stream sample after this packet, or `None` if it would pass
    /// `u64::MAX`.
    pub const fn end_sample(&self) -> Option<u64> {
        self.first_sample
            .checked_add(self.frame_count as u64 * self.kind.samples_per_frame())
    }

    /// Checks every rule one header can break, given the payload length it
    /// announces, in the firmware's order.
    fn check(&self, payload_bytes: u32) -> Result<(), FrameError> {
        if self.uncertainty_us == Some(UNCERTAINTY_UNKNOWN) {
            return Err(FrameError::UncertaintySentinel);
        }
        RawHeader {
            kind: self.kind,
            flags: self.flags.bits(),
            channels: 1,
            format: self.kind.format() as u8,
            epoch: self.epoch,
            first_sample: self.first_sample,
            sample_rate: self.kind.sample_rate(),
            frame_count: self.frame_count,
            generation: self.generation,
            source_mask: u32::from(self.source_mask.bits()),
            payload_bytes,
        }
        .check()
    }

    fn write(&self, payload_bytes: u32, out: &mut BytesMut) {
        out.put_slice(&MAGIC);
        out.put_u8(self.kind as u8);
        out.put_u8(self.flags.bits());
        out.put_u8(1);
        out.put_u8(self.kind.format() as u8);
        out.put_u64_le(self.epoch);
        out.put_u64_le(self.sequence);
        out.put_u64_le(self.first_sample);
        out.put_u64_le(self.mono_ns);
        out.put_u32_le(self.uncertainty_us.unwrap_or(UNCERTAINTY_UNKNOWN));
        out.put_u32_le(self.kind.sample_rate());
        out.put_u32_le(self.frame_count);
        out.put_u32_le(self.generation);
        out.put_u32_le(u32::from(self.source_mask.bits()));
        out.put_u32_le(payload_bytes);
    }
}

/// The header fields as they sit on the wire, before validation.
struct RawHeader {
    kind: Kind,
    flags: u8,
    channels: u8,
    format: u8,
    epoch: u64,
    first_sample: u64,
    sample_rate: u32,
    frame_count: u32,
    generation: u32,
    source_mask: u32,
    payload_bytes: u32,
}

impl RawHeader {
    fn check(&self) -> Result<(), FrameError> {
        let kind = self.kind;
        let Some(flags) = Flags::from_bits(self.flags) else {
            return Err(FrameError::UndefinedFlags(self.flags));
        };
        if flags.contains(Flags::DIGITAL_SILENCE) && kind != Kind::Reference {
            return Err(FrameError::DigitalSilenceOutsideReference(kind));
        }
        if self.channels != 1 {
            return Err(FrameError::Channels(self.channels));
        }
        if self.format != kind.format() as u8 {
            return Err(FrameError::Format {
                kind,
                format: self.format,
            });
        }
        if self.epoch == 0 {
            return Err(FrameError::ZeroEpoch);
        }
        if self.sample_rate != kind.sample_rate() {
            return Err(FrameError::SampleRate {
                kind,
                rate: self.sample_rate,
            });
        }
        if self.frame_count == 0 || self.frame_count > kind.max_frames() {
            return Err(FrameError::FrameCount {
                kind,
                count: self.frame_count,
            });
        }
        if kind != Kind::Render && self.generation != 0 {
            return Err(FrameError::Generation {
                kind,
                generation: self.generation,
            });
        }
        if (kind != Kind::Reference && self.source_mask != 0)
            || SourceMask::from_bits(self.source_mask).is_none()
        {
            return Err(FrameError::SourceMask {
                kind,
                mask: self.source_mask,
            });
        }
        if kind == Kind::Cells && !self.first_sample.is_multiple_of(CELL_SAMPLES) {
            return Err(FrameError::CellAlignment(self.first_sample));
        }
        if kind == Kind::Afe && !self.first_sample.is_multiple_of(AFE_PERIOD_SAMPLES) {
            return Err(FrameError::AfeAlignment(self.first_sample));
        }
        let expected = kind.payload_bytes(flags, self.frame_count);
        if u64::from(self.payload_bytes) != expected {
            return Err(FrameError::PayloadBytes {
                expected,
                header: self.payload_bytes,
            });
        }
        Ok(())
    }
}

/// Decodes and validates the header at the start of `bytes`, without looking
/// at the payload.
pub fn decode_header(bytes: &[u8]) -> Result<Header, FrameError> {
    let Some(head) = bytes.first_chunk::<HEADER_BYTES>() else {
        return Err(FrameError::Short(bytes.len()));
    };
    decode_raw(head).map(|(header, _)| header)
}

fn decode_raw(head: &[u8; HEADER_BYTES]) -> Result<(Header, u32), FrameError> {
    let u64_at = |at: usize| u64::from_le_bytes(head[at..at + 8].try_into().expect("8 bytes"));
    let u32_at = |at: usize| u32::from_le_bytes(head[at..at + 4].try_into().expect("4 bytes"));
    if head[0..4] != MAGIC {
        return Err(FrameError::Magic([head[0], head[1], head[2], head[3]]));
    }
    let Some(kind) = Kind::from_u8(head[4]) else {
        return Err(FrameError::Kind(head[4]));
    };
    let raw = RawHeader {
        kind,
        flags: head[5],
        channels: head[6],
        format: head[7],
        epoch: u64_at(8),
        first_sample: u64_at(24),
        sample_rate: u32_at(44),
        frame_count: u32_at(48),
        generation: u32_at(52),
        source_mask: u32_at(56),
        payload_bytes: u32_at(60),
    };
    raw.check()?;
    let uncertainty = u32_at(40);
    let header = Header {
        kind,
        flags: Flags(raw.flags),
        epoch: raw.epoch,
        sequence: u64_at(16),
        first_sample: raw.first_sample,
        mono_ns: u64_at(32),
        uncertainty_us: (uncertainty != UNCERTAINTY_UNKNOWN).then_some(uncertainty),
        frame_count: raw.frame_count,
        generation: raw.generation,
        source_mask: SourceMask(raw.source_mask as u8),
    };
    Ok((header, raw.payload_bytes))
}

/// One validated EMA1 frame. The payload is a slice of the received buffer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Packet {
    header: Header,
    end_sample: u64,
    payload: Bytes,
}

impl Packet {
    /// Decodes and validates one whole frame: the header rules, the frame
    /// length against `payload_bytes`, sample indices that fit in a `u64`,
    /// and every cell or AFE record.
    pub fn parse(frame: Bytes) -> Result<Self, FrameError> {
        let Some(head) = frame.first_chunk::<HEADER_BYTES>() else {
            return Err(FrameError::Short(frame.len()));
        };
        let (header, payload_bytes) = decode_raw(head)?;
        let actual = frame.len() - HEADER_BYTES;
        if actual as u64 != u64::from(payload_bytes) {
            return Err(FrameError::FrameLength {
                header: payload_bytes,
                actual,
            });
        }
        let end_sample = header.end_sample().ok_or(FrameError::SampleOverflow)?;
        let payload = frame.slice(HEADER_BYTES..);
        check_records(header.kind, &payload)?;
        Ok(Self {
            header,
            end_sample,
            payload,
        })
    }

    /// The validated header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The raw payload bytes.
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }

    /// The first stream sample after this packet.
    pub fn end_sample(&self) -> u64 {
        self.end_sample
    }

    /// The payload, decoded by kind.
    pub fn content(&self) -> Content<'_> {
        match self.header.kind {
            Kind::Cells => Content::Cells(CellRecords(self.payload.as_chunks().0.iter())),
            Kind::Afe => Content::Afe(AfeRecords(self.payload.as_chunks().0.iter())),
            _ if self.header.flags.contains(Flags::DIGITAL_SILENCE) => Content::DigitalSilence {
                frames: self.header.frame_count,
            },
            _ => Content::Pcm(PcmSamples(self.payload.as_chunks().0.iter())),
        }
    }
}

/// Checks every record of a payload whose length the header rules already
/// made a whole number of records.
fn check_records(kind: Kind, payload: &[u8]) -> Result<(), FrameError> {
    match kind {
        Kind::Cells => {
            for (index, raw) in payload
                .as_chunks::<CELL_RECORD_BYTES>()
                .0
                .iter()
                .enumerate()
            {
                Cell::parse(raw).map_err(|error| FrameError::Cell { index, error })?;
            }
        }
        Kind::Afe => {
            for (index, raw) in payload.as_chunks::<AFE_RECORD_BYTES>().0.iter().enumerate() {
                AfeRecord::parse(raw).map_err(|error| FrameError::Afe { index, error })?;
            }
        }
        Kind::Mic | Kind::Reference | Kind::Render => {}
    }
    Ok(())
}

/// A packet's payload, decoded by kind.
#[derive(Clone, Debug)]
pub enum Content<'a> {
    /// PCM16 samples (kinds 1–3).
    Pcm(PcmSamples<'a>),
    /// `frames` samples that are all zero (kind 2 with digital silence).
    DigitalSilence {
        /// The number of zero samples.
        frames: u32,
    },
    /// Cell records (kind 4).
    Cells(CellRecords<'a>),
    /// AFE records (kind 5).
    Afe(AfeRecords<'a>),
}

/// The PCM16 samples of a packet.
#[derive(Clone, Debug)]
pub struct PcmSamples<'a>(slice::Iter<'a, [u8; 2]>);

impl Iterator for PcmSamples<'_> {
    type Item = i16;
    fn next(&mut self) -> Option<i16> {
        self.0.next().map(|b| i16::from_le_bytes(*b))
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for PcmSamples<'_> {}
impl FusedIterator for PcmSamples<'_> {}

/// The cell records of a validated packet.
#[derive(Clone, Debug)]
pub struct CellRecords<'a>(slice::Iter<'a, [u8; CELL_RECORD_BYTES]>);

impl Iterator for CellRecords<'_> {
    type Item = Cell;
    fn next(&mut self) -> Option<Cell> {
        self.0.next().map(Cell::from_bytes)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for CellRecords<'_> {}
impl FusedIterator for CellRecords<'_> {}

/// The AFE records of a validated packet.
#[derive(Clone, Debug)]
pub struct AfeRecords<'a>(slice::Iter<'a, [u8; AFE_RECORD_BYTES]>);

impl Iterator for AfeRecords<'_> {
    type Item = AfeRecord;
    fn next(&mut self) -> Option<AfeRecord> {
        self.0.next().map(AfeRecord::from_bytes)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl ExactSizeIterator for AfeRecords<'_> {}
impl FusedIterator for AfeRecords<'_> {}

/// Cell record flags (cell byte 2).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct CellFlags(u8);

impl CellFlags {
    /// No flag set.
    pub const NONE: CellFlags = CellFlags(0);
    /// The cell overlaps a missing range; its loudness is then −120 dB.
    pub const GAP: CellFlags = CellFlags(1 << 0);
    /// The capture was muted during the cell.
    pub const MUTED: CellFlags = CellFlags(1 << 1);
    const KNOWN: u8 = 0b11;

    /// The flags of `bits`, or `None` when an undefined bit is set.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(CellFlags(bits))
        } else {
            None
        }
    }

    /// The wire byte.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: CellFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for CellFlags {
    type Output = CellFlags;
    fn bitor(self, rhs: CellFlags) -> CellFlags {
        CellFlags(self.0 | rhs.0)
    }
}

/// One 512-sample loudness record (cell record v1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cell {
    /// Loudness in hundredths of a dB, −12000 to 0.
    pub e_centi: i16,
    /// Gap and muted flags.
    pub flags: CellFlags,
    /// The final mix's active sources during the cell.
    pub mask: SourceMask,
}

impl Cell {
    /// The lowest loudness, and the loudness of every gap cell.
    pub const E_MIN: i16 = -12_000;
    /// The highest loudness.
    pub const E_MAX: i16 = 0;

    /// Decodes and validates one record.
    pub fn parse(raw: &[u8; CELL_RECORD_BYTES]) -> Result<Cell, CellError> {
        let cell = Cell::from_bytes(raw);
        if !(Cell::E_MIN..=Cell::E_MAX).contains(&cell.e_centi) {
            return Err(CellError::Loudness(cell.e_centi));
        }
        if CellFlags::from_bits(raw[2]).is_none() {
            return Err(CellError::UndefinedFlags(raw[2]));
        }
        if SourceMask::from_bits(u32::from(raw[3])).is_none() {
            return Err(CellError::UndefinedSources(raw[3]));
        }
        if cell.flags.contains(CellFlags::GAP) && cell.e_centi != Cell::E_MIN {
            return Err(CellError::GapLoudness(cell.e_centi));
        }
        Ok(cell)
    }

    fn from_bytes(raw: &[u8; CELL_RECORD_BYTES]) -> Cell {
        Cell {
            e_centi: i16::from_le_bytes([raw[0], raw[1]]),
            flags: CellFlags(raw[2]),
            mask: SourceMask(raw[3]),
        }
    }

    /// The record's four wire bytes.
    pub fn to_bytes(self) -> [u8; CELL_RECORD_BYTES] {
        let [lo, hi] = self.e_centi.to_le_bytes();
        [lo, hi, self.flags.bits(), self.mask.bits()]
    }
}

/// Why a cell record is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellError {
    /// Loudness outside −12000..=0.
    Loudness(i16),
    /// Flag bits beyond gap and muted.
    UndefinedFlags(u8),
    /// Source mask bits beyond the four sources.
    UndefinedSources(u8),
    /// A gap cell whose loudness is not −12000.
    GapLoudness(i16),
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::Loudness(e) => write!(f, "loudness {e} outside [-12000, 0]"),
            CellError::UndefinedFlags(b) => write!(f, "flags 0x{b:02x} set undefined bits"),
            CellError::UndefinedSources(b) => {
                write!(f, "source mask 0x{b:02x} sets undefined bits")
            }
            CellError::GapLoudness(e) => write!(f, "gap cell has loudness {e}, not -12000"),
        }
    }
}

/// AFE record flags (record byte 1).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct AfeFlags(u8);

impl AfeFlags {
    /// No flag set.
    pub const NONE: AfeFlags = AfeFlags(0);
    /// AFE frames were missing before a frame of this period, or the frame
    /// phase moved.
    pub const GAP: AfeFlags = AfeFlags(1 << 0);
    /// The decoder acquired lock in this period.
    pub const SYNC: AfeFlags = AfeFlags(1 << 1);
    /// A frame reported clipped output.
    pub const OUTPUT_CLIPPED: AfeFlags = AfeFlags(1 << 2);
    /// A frame reported a clipped microphone.
    pub const MIC_CLIPPED: AfeFlags = AfeFlags(1 << 3);
    /// A frame reported a diverged echo canceller.
    pub const AEC_DIVERGED: AfeFlags = AfeFlags(1 << 4);
    /// A frame reported the device muted.
    pub const DEVICE_MUTE: AfeFlags = AfeFlags(1 << 5);
    const KNOWN: u8 = 0b11_1111;

    /// The flags of `bits`, or `None` when a reserved bit is set.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::KNOWN == 0 {
            Some(AfeFlags(bits))
        } else {
            None
        }
    }

    /// The wire byte.
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every flag of `other` is set.
    pub const fn contains(self, other: AfeFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

impl BitOr for AfeFlags {
    type Output = AfeFlags;
    fn bitor(self, rhs: AfeFlags) -> AfeFlags {
        AfeFlags(self.0 | rhs.0)
    }
}

/// One capture period's summary of the native AFE metadata (AFE record v1,
/// docs/protocol-v1.md §3). Values keep the AFE's own scales; `frames` 0
/// means the period has no data.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AfeRecord {
    /// Valid AFE frames that ended in the period, 0–10.
    pub frames: u8,
    /// Period flags.
    pub flags: AfeFlags,
    /// Frames with playback active.
    pub playback: u8,
    /// The highest raw ERLE.
    pub erle_max: u8,
    /// The rounded mean of the non-zero raw ERLE values.
    pub erle_mean: u8,
    /// Frames with raw ERLE above 0.
    pub erle_frames: u8,
    /// The highest double-talk value, 0–31 (value = raw / 31).
    pub dtd_max: u8,
    /// Frames with double talk above 0.
    pub dtd_frames: u8,
    /// The highest raw RMS (dB = raw − 256); 0 when no frame computed one.
    pub rms_max: u8,
    /// The rounded mean raw RMS; 0 likewise.
    pub rms_mean: u8,
    /// The highest DNN VAD value, 0–3 (value = raw × 0.25).
    pub vad_max: u8,
    /// Frames with DNN VAD above 0.
    pub vad_frames: u8,
    /// The volume of the period's last valid frame, 0–127.
    pub volume: u8,
    /// AFE frames missing before the period's frames, clamped to 255.
    pub lost: u8,
}

impl AfeRecord {
    /// The most AFE frames one period holds.
    pub const MAX_FRAMES: u8 = 10;
    /// The highest double-talk value.
    pub const DTD_MAX: u8 = 31;
    /// The highest DNN VAD value.
    pub const VAD_MAX: u8 = 3;
    /// The highest volume.
    pub const VOLUME_MAX: u8 = 127;

    /// Decodes and validates one record.
    pub fn parse(raw: &[u8; AFE_RECORD_BYTES]) -> Result<AfeRecord, AfeError> {
        if AfeFlags::from_bits(raw[1]).is_none() {
            return Err(AfeError::ReservedFlags(raw[1]));
        }
        let r = AfeRecord::from_bytes(raw);
        let bounds = [
            ("frames", r.frames, AfeRecord::MAX_FRAMES),
            ("playback", r.playback, r.frames),
            ("erle_frames", r.erle_frames, r.frames),
            ("dtd_frames", r.dtd_frames, r.frames),
            ("vad_frames", r.vad_frames, r.frames),
            ("dtd_max", r.dtd_max, AfeRecord::DTD_MAX),
            ("vad_max", r.vad_max, AfeRecord::VAD_MAX),
            ("volume", r.volume, AfeRecord::VOLUME_MAX),
        ];
        for (field, value, max) in bounds {
            if value > max {
                return Err(AfeError::OutOfRange { field, value, max });
            }
        }
        Ok(r)
    }

    fn from_bytes(b: &[u8; AFE_RECORD_BYTES]) -> AfeRecord {
        AfeRecord {
            frames: b[0],
            flags: AfeFlags(b[1]),
            playback: b[2],
            erle_max: b[3],
            erle_mean: b[4],
            erle_frames: b[5],
            dtd_max: b[6],
            dtd_frames: b[7],
            rms_max: b[8],
            rms_mean: b[9],
            vad_max: b[10],
            vad_frames: b[11],
            volume: b[12],
            lost: b[13],
        }
    }

    /// The record's fourteen wire bytes.
    pub fn to_bytes(self) -> [u8; AFE_RECORD_BYTES] {
        [
            self.frames,
            self.flags.bits(),
            self.playback,
            self.erle_max,
            self.erle_mean,
            self.erle_frames,
            self.dtd_max,
            self.dtd_frames,
            self.rms_max,
            self.rms_mean,
            self.vad_max,
            self.vad_frames,
            self.volume,
            self.lost,
        ]
    }
}

/// Why an AFE record is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AfeError {
    /// Reserved flag bits 6–7 set.
    ReservedFlags(u8),
    /// A field above its bound (a frame count above the period's frames).
    OutOfRange {
        /// The field's wire name.
        field: &'static str,
        /// Its value.
        value: u8,
        /// Its bound.
        max: u8,
    },
}

impl fmt::Display for AfeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AfeError::ReservedFlags(b) => write!(f, "flags 0x{b:02x} set reserved bits"),
            AfeError::OutOfRange { field, value, max } => write!(f, "{field} {value} above {max}"),
        }
    }
}

/// Why a frame is malformed. Every one is reported as `malformed_frame`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    /// The frame is shorter than the header.
    Short(usize),
    /// The first four bytes are not `EMA1`.
    Magic([u8; 4]),
    /// A kind outside 1–5.
    Kind(u8),
    /// Flag bits beyond the five defined ones.
    UndefinedFlags(u8),
    /// Digital silence on a kind other than reference.
    DigitalSilenceOutsideReference(Kind),
    /// Channels other than 1.
    Channels(u8),
    /// A format that does not match the kind.
    Format {
        /// The frame's kind.
        kind: Kind,
        /// The format byte.
        format: u8,
    },
    /// Epoch 0, which is reserved.
    ZeroEpoch,
    /// A sample rate that does not match the kind.
    SampleRate {
        /// The frame's kind.
        kind: Kind,
        /// The rate field.
        rate: u32,
    },
    /// A frame count of 0 or above the kind's maximum.
    FrameCount {
        /// The frame's kind.
        kind: Kind,
        /// The count field.
        count: u32,
    },
    /// A nonzero generation outside kind 3.
    Generation {
        /// The frame's kind.
        kind: Kind,
        /// The generation field.
        generation: u32,
    },
    /// A nonzero source mask outside kind 2, or undefined source bits.
    SourceMask {
        /// The frame's kind.
        kind: Kind,
        /// The mask field.
        mask: u32,
    },
    /// A cell packet whose first sample is not a multiple of 512.
    CellAlignment(u64),
    /// An AFE packet whose first sample is not a multiple of 1,280.
    AfeAlignment(u64),
    /// `payload_bytes` disagrees with the kind, flags and frame count.
    PayloadBytes {
        /// The length the kind, flags and frame count call for.
        expected: u64,
        /// The `payload_bytes` field.
        header: u32,
    },
    /// The frame's length disagrees with `payload_bytes`.
    FrameLength {
        /// The `payload_bytes` field.
        header: u32,
        /// The bytes after the header.
        actual: usize,
    },
    /// The packet's samples run past `u64::MAX`.
    SampleOverflow,
    /// An invalid cell record.
    Cell {
        /// The record's position in the packet.
        index: usize,
        /// What is wrong with it.
        error: CellError,
    },
    /// An invalid AFE record.
    Afe {
        /// The record's position in the packet.
        index: usize,
        /// What is wrong with it.
        error: AfeError,
    },
    /// An encoder was given `Some(0xFFFFFFFF)` as the uncertainty, the value
    /// that means unknown on the wire; unknown is `None`.
    UncertaintySentinel,
    /// An encoder was given a payload whose length does not fit the header.
    PayloadTooLong(usize),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::Short(n) => write!(f, "frame of {n} bytes is shorter than the header"),
            FrameError::Magic(m) => write!(f, "bad magic {m:02x?}"),
            FrameError::Kind(k) => write!(f, "unknown kind {k}"),
            FrameError::UndefinedFlags(b) => write!(f, "flags 0x{b:02x} set undefined bits"),
            FrameError::DigitalSilenceOutsideReference(k) => {
                write!(
                    f,
                    "digital silence on kind {}, defined for reference packets only",
                    *k as u8
                )
            }
            FrameError::Channels(c) => write!(f, "channels must be 1, got {c}"),
            FrameError::Format { kind, format } => {
                write!(f, "format {format} is wrong for kind {}", *kind as u8)
            }
            FrameError::ZeroEpoch => f.write_str("epoch 0 is reserved"),
            FrameError::SampleRate { kind, rate } => {
                write!(f, "sample rate {rate} is wrong for kind {}", *kind as u8)
            }
            FrameError::FrameCount { kind, count } => {
                write!(
                    f,
                    "frame count {count} outside 1..={} for kind {}",
                    kind.max_frames(),
                    *kind as u8
                )
            }
            FrameError::Generation { kind, generation } => {
                write!(
                    f,
                    "generation {generation} on kind {}, allowed on kind 3 only",
                    *kind as u8
                )
            }
            FrameError::SourceMask { kind, mask } => {
                write!(f, "source mask 0x{mask:x} invalid for kind {}", *kind as u8)
            }
            FrameError::CellAlignment(s) => {
                write!(f, "cell packet first sample {s} is not a multiple of 512")
            }
            FrameError::AfeAlignment(s) => {
                write!(f, "afe packet first sample {s} is not a multiple of 1280")
            }
            FrameError::PayloadBytes { expected, header } => {
                write!(f, "payload bytes {header}, expected {expected}")
            }
            FrameError::FrameLength { header, actual } => {
                write!(
                    f,
                    "frame carries {actual} payload bytes, header says {header}"
                )
            }
            FrameError::SampleOverflow => f.write_str("sample index overflows uint64"),
            FrameError::Cell { index, error } => write!(f, "cell record {index}: {error}"),
            FrameError::Afe { index, error } => write!(f, "afe record {index}: {error}"),
            FrameError::UncertaintySentinel => {
                f.write_str("uncertainty 0xffffffff means unknown; encode None")
            }
            FrameError::PayloadTooLong(n) => {
                write!(f, "payload of {n} bytes does not fit the header")
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for ProtocolError {
    fn from(error: FrameError) -> Self {
        ProtocolError::new(ProtocolErrorCode::MalformedFrame, error.to_string())
    }
}

/// Encodes one frame of any kind after checking every rule [`Packet::parse`]
/// applies. The controller sends only kind 3 ([`encode_render`]); the other
/// kinds serve simulated devices.
pub fn encode(header: &Header, payload: &[u8]) -> Result<Bytes, FrameError> {
    let payload_bytes =
        u32::try_from(payload.len()).map_err(|_| FrameError::PayloadTooLong(payload.len()))?;
    header.check(payload_bytes)?;
    header.end_sample().ok_or(FrameError::SampleOverflow)?;
    check_records(header.kind, payload)?;
    let mut out = BytesMut::with_capacity(HEADER_BYTES + payload.len());
    header.write(payload_bytes, &mut out);
    out.put_slice(payload);
    Ok(out.freeze())
}

/// Encodes one kind-3 render frame of `pcm` at 48 kHz for the playback of
/// `generation`. The monotonic time is 0 and the uncertainty unknown, as for
/// every downlink frame.
pub fn encode_render(
    epoch: u64,
    sequence: u64,
    first_sample: u64,
    generation: u32,
    flags: Flags,
    pcm: &[i16],
) -> Result<Bytes, FrameError> {
    let frame_count = u32::try_from(pcm.len()).unwrap_or(u32::MAX);
    let header = Header {
        kind: Kind::Render,
        flags,
        epoch,
        sequence,
        first_sample,
        mono_ns: 0,
        uncertainty_us: None,
        frame_count,
        generation,
        source_mask: SourceMask::NONE,
    };
    let payload_bytes = header.payload_bytes();
    header.check(u32::try_from(payload_bytes).unwrap_or(u32::MAX))?;
    header.end_sample().ok_or(FrameError::SampleOverflow)?;
    let mut out = BytesMut::with_capacity(HEADER_BYTES + pcm.len() * 2);
    header.write(payload_bytes as u32, &mut out);
    for sample in pcm {
        out.put_i16_le(*sample);
    }
    Ok(out.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(
        kind: Kind,
        flags: Flags,
        epoch: u64,
        sequence: u64,
        first_sample: u64,
        frame_count: u32,
    ) -> Header {
        Header {
            kind,
            flags,
            epoch,
            sequence,
            first_sample,
            mono_ns: 0,
            uncertainty_us: Some(0),
            frame_count,
            generation: 0,
            source_mask: SourceMask::NONE,
        }
    }

    /// The firmware's `validHeaders` (device/internal/audio/ema/ema_test.go).
    fn valid_headers() -> Vec<Header> {
        let mut mic = header(
            Kind::Mic,
            Flags::ESTIMATED | Flags::DISCONTINUITY,
            0xdead_beef,
            7,
            1280 * 9,
            1280,
        );
        mic.mono_ns = 123_456_789;
        mic.uncertainty_us = Some(80_000);
        let mut silence = header(Kind::Reference, Flags::DIGITAL_SILENCE, 42, 0, 2560, 1280);
        silence.source_mask = SourceMask::CONTENT | SourceMask::EARCON;
        let mut reference = header(Kind::Reference, Flags::NONE, 42, 1, 3840, 160);
        reference.source_mask = SourceMask::ALERT;
        let mut render = header(Kind::Render, Flags::UNDERRUN, 99, 3, 0, 3840);
        render.generation = 17;
        render.uncertainty_us = None;
        let cells = header(Kind::Cells, Flags::NONE, 0xdead_beef, 2, 512 * 40, 320);
        let mut afe = header(
            Kind::Afe,
            Flags::ESTIMATED | Flags::DISCONTINUITY,
            0xdead_beef,
            4,
            1280 * 40,
            125,
        );
        afe.mono_ns = 5_000_000_000;
        afe.uncertainty_us = Some(81_000);
        vec![mic, silence, reference, render, cells, afe]
    }

    fn base(kind: Kind) -> Header {
        valid_headers()
            .into_iter()
            .find(|h| h.kind == kind)
            .expect("kind")
    }

    /// A zero payload of the right length; zero cell and AFE records are valid.
    fn frame(h: &Header) -> Bytes {
        encode(h, &vec![0; h.payload_bytes() as usize]).expect("valid header")
    }

    /// Writes every header field verbatim, bypassing the encoder's checks
    /// (the firmware test's `writeRaw`).
    struct Raw {
        kind: u8,
        flags: u8,
        channels: u8,
        format: u8,
        epoch: u64,
        first: u64,
        rate: u32,
        frames: u32,
        generation: u32,
        mask: u32,
        payload_bytes: u32,
    }

    impl Raw {
        fn of(h: &Header) -> Raw {
            Raw {
                kind: h.kind as u8,
                flags: h.flags.bits(),
                channels: 1,
                format: h.kind.format() as u8,
                epoch: h.epoch,
                first: h.first_sample,
                rate: h.kind.sample_rate(),
                frames: h.frame_count,
                generation: h.generation,
                mask: u32::from(h.source_mask.bits()),
                payload_bytes: h.payload_bytes() as u32,
            }
        }

        fn bytes(&self) -> Vec<u8> {
            let mut b = Vec::with_capacity(HEADER_BYTES);
            b.extend_from_slice(&MAGIC);
            b.extend_from_slice(&[self.kind, self.flags, self.channels, self.format]);
            b.extend_from_slice(&self.epoch.to_le_bytes());
            b.extend_from_slice(&5u64.to_le_bytes());
            b.extend_from_slice(&self.first.to_le_bytes());
            b.extend_from_slice(&0u64.to_le_bytes());
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&self.rate.to_le_bytes());
            b.extend_from_slice(&self.frames.to_le_bytes());
            b.extend_from_slice(&self.generation.to_le_bytes());
            b.extend_from_slice(&self.mask.to_le_bytes());
            b.extend_from_slice(&self.payload_bytes.to_le_bytes());
            b
        }
    }

    #[test]
    fn every_kind_round_trips_every_header_field() {
        for h in valid_headers() {
            let bytes = frame(&h);
            assert_eq!(bytes.len(), HEADER_BYTES + h.payload_bytes() as usize);
            let packet = Packet::parse(bytes).expect("valid frame");
            assert_eq!(*packet.header(), h);
            assert_eq!(packet.payload().len() as u64, h.payload_bytes());
        }
    }

    #[test]
    fn header_fields_sit_at_the_documented_offsets() {
        let bytes = frame(&valid_headers()[0]);
        assert_eq!(&bytes[0..4], b"EMA1");
        assert_eq!(bytes[4], 1);
        assert_eq!(bytes[5], (Flags::ESTIMATED | Flags::DISCONTINUITY).bits());
        assert_eq!((bytes[6], bytes[7]), (1, 1));
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(
            (u64_at(8), u64_at(16), u64_at(24), u64_at(32)),
            (0xdead_beef, 7, 11_520, 123_456_789)
        );
        assert_eq!(
            (u32_at(40), u32_at(44), u32_at(48), u32_at(60)),
            (80_000, 16_000, 1280, 2560)
        );
    }

    #[test]
    fn unknown_uncertainty_reads_as_none_not_zero() {
        let render = base(Kind::Render);
        let bytes = frame(&render);
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            UNCERTAINTY_UNKNOWN
        );
        assert_eq!(Packet::parse(bytes).unwrap().header().uncertainty_us, None);
        let mut sentinel = render;
        sentinel.uncertainty_us = Some(UNCERTAINTY_UNKNOWN);
        assert_eq!(
            encode(&sentinel, &[0; 7680]),
            Err(FrameError::UncertaintySentinel)
        );
    }

    /// The firmware's `TestHeaderValidation`, applied to raw header bytes.
    #[test]
    fn every_header_rule_rejects_raw_bytes() {
        type Case = (&'static str, Kind, fn(&mut Raw), fn(&FrameError) -> bool);
        let cases: &[Case] = &[
            (
                "kind zero",
                Kind::Mic,
                |r| r.kind = 0,
                |e| *e == FrameError::Kind(0),
            ),
            (
                "kind six",
                Kind::Mic,
                |r| r.kind = 6,
                |e| *e == FrameError::Kind(6),
            ),
            (
                "reserved flag",
                Kind::Mic,
                |r| r.flags |= 1 << 5,
                |e| matches!(e, FrameError::UndefinedFlags(_)),
            ),
            (
                "silence on mic",
                Kind::Mic,
                |r| {
                    r.flags |= Flags::DIGITAL_SILENCE.bits();
                    r.payload_bytes = 0;
                },
                |e| *e == FrameError::DigitalSilenceOutsideReference(Kind::Mic),
            ),
            (
                "stereo",
                Kind::Mic,
                |r| r.channels = 2,
                |e| *e == FrameError::Channels(2),
            ),
            (
                "cells as pcm",
                Kind::Cells,
                |r| r.format = 1,
                |e| matches!(e, FrameError::Format { .. }),
            ),
            (
                "mic as cells",
                Kind::Mic,
                |r| r.format = 2,
                |e| matches!(e, FrameError::Format { .. }),
            ),
            (
                "afe as cells",
                Kind::Afe,
                |r| r.format = 2,
                |e| matches!(e, FrameError::Format { .. }),
            ),
            (
                "zero epoch",
                Kind::Mic,
                |r| r.epoch = 0,
                |e| *e == FrameError::ZeroEpoch,
            ),
            (
                "mic at 48k",
                Kind::Mic,
                |r| r.rate = RENDER_RATE,
                |e| matches!(e, FrameError::SampleRate { .. }),
            ),
            (
                "render at 16k",
                Kind::Render,
                |r| r.rate = CAPTURE_RATE,
                |e| matches!(e, FrameError::SampleRate { .. }),
            ),
            (
                "zero frames",
                Kind::Mic,
                |r| {
                    r.frames = 0;
                    r.payload_bytes = 0;
                },
                |e| matches!(e, FrameError::FrameCount { count: 0, .. }),
            ),
            (
                "mic 1281",
                Kind::Mic,
                |r| {
                    r.frames = 1281;
                    r.payload_bytes = 2562;
                },
                |e| matches!(e, FrameError::FrameCount { count: 1281, .. }),
            ),
            (
                "render 3841",
                Kind::Render,
                |r| {
                    r.frames = 3841;
                    r.payload_bytes = 7682;
                },
                |e| matches!(e, FrameError::FrameCount { count: 3841, .. }),
            ),
            (
                "cells 321",
                Kind::Cells,
                |r| {
                    r.frames = 321;
                    r.payload_bytes = 1284;
                },
                |e| matches!(e, FrameError::FrameCount { count: 321, .. }),
            ),
            (
                "afe 126",
                Kind::Afe,
                |r| {
                    r.frames = 126;
                    r.payload_bytes = 126 * 14;
                },
                |e| matches!(e, FrameError::FrameCount { count: 126, .. }),
            ),
            (
                "afe at 48k",
                Kind::Afe,
                |r| r.rate = RENDER_RATE,
                |e| matches!(e, FrameError::SampleRate { .. }),
            ),
            (
                "generation on mic",
                Kind::Mic,
                |r| r.generation = 1,
                |e| matches!(e, FrameError::Generation { .. }),
            ),
            (
                "mask on mic",
                Kind::Mic,
                |r| r.mask = 1,
                |e| matches!(e, FrameError::SourceMask { .. }),
            ),
            (
                "unknown source",
                Kind::Reference,
                |r| r.mask = 16,
                |e| matches!(e, FrameError::SourceMask { .. }),
            ),
            (
                "unaligned cells",
                Kind::Cells,
                |r| r.first = 513,
                |e| *e == FrameError::CellAlignment(513),
            ),
            (
                "unaligned afe",
                Kind::Afe,
                |r| r.first = 512 * 3,
                |e| *e == FrameError::AfeAlignment(1536),
            ),
            (
                "afe silence",
                Kind::Afe,
                |r| r.flags |= Flags::DIGITAL_SILENCE.bits(),
                |e| *e == FrameError::DigitalSilenceOutsideReference(Kind::Afe),
            ),
            (
                "pcm payload",
                Kind::Mic,
                |r| r.payload_bytes = 1280,
                |e| matches!(e, FrameError::PayloadBytes { .. }),
            ),
            (
                "silence with payload",
                Kind::Reference,
                |r| r.payload_bytes = 2560,
                |e| matches!(e, FrameError::PayloadBytes { expected: 0, .. }),
            ),
            (
                "cell payload",
                Kind::Cells,
                |r| r.payload_bytes = 640,
                |e| matches!(e, FrameError::PayloadBytes { .. }),
            ),
            (
                "afe payload",
                Kind::Afe,
                |r| r.payload_bytes = 125 * 12,
                |e| matches!(e, FrameError::PayloadBytes { .. }),
            ),
        ];
        for (name, kind, mutate, expect) in cases {
            // `base(Kind::Reference)` is the digital-silence header.
            let mut raw = Raw::of(&base(*kind));
            mutate(&mut raw);
            let bytes = raw.bytes();
            let err = decode_header(&bytes).expect_err(name);
            assert!(expect(&err), "{name}: {err:?}");
            let mut whole = bytes;
            whole.resize(HEADER_BYTES + raw.payload_bytes as usize, 0);
            let err = Packet::parse(Bytes::from(whole)).expect_err(name);
            assert!(expect(&err), "{name}: parse gave {err:?}");
            assert_eq!(
                ProtocolError::from(err).code,
                ProtocolErrorCode::MalformedFrame
            );
        }
    }

    #[test]
    fn bad_magic_short_frames_and_length_mismatches_are_rejected() {
        let bytes = frame(&base(Kind::Mic));
        assert_eq!(
            Packet::parse(bytes.slice(..HEADER_BYTES - 1)),
            Err(FrameError::Short(63))
        );
        assert_eq!(decode_header(&bytes[..40]), Err(FrameError::Short(40)));
        assert_eq!(
            Packet::parse(bytes.slice(..bytes.len() - 2)),
            Err(FrameError::FrameLength {
                header: 2560,
                actual: 2558
            })
        );
        let mut longer = bytes.to_vec();
        longer.extend_from_slice(&[0, 0]);
        assert_eq!(
            Packet::parse(Bytes::from(longer)),
            Err(FrameError::FrameLength {
                header: 2560,
                actual: 2562
            })
        );
        let mut bad = bytes.to_vec();
        bad[3] = b'2';
        assert_eq!(
            Packet::parse(Bytes::from(bad)),
            Err(FrameError::Magic(*b"EMA2"))
        );
    }

    #[test]
    fn samples_running_past_u64_max_are_rejected() {
        let mut h = base(Kind::Mic);
        h.first_sample = u64::MAX - 100;
        assert_eq!(encode(&h, &[0; 2560]), Err(FrameError::SampleOverflow));
        let mut raw = Raw::of(&base(Kind::Mic));
        raw.first = u64::MAX - 100;
        let mut whole = raw.bytes();
        whole.resize(HEADER_BYTES + 2560, 0);
        assert_eq!(
            Packet::parse(Bytes::from(whole)),
            Err(FrameError::SampleOverflow)
        );
    }

    #[test]
    fn payload_is_a_slice_of_the_received_buffer() {
        let pcm = [0i16, 1, -1, 32767, -32768, 1234];
        let bytes = encode_render(99, 0, 0, 4, Flags::NONE, &pcm).unwrap();
        let start = bytes.as_ptr() as usize;
        let packet = Packet::parse(bytes).unwrap();
        assert_eq!(packet.payload().as_ptr() as usize, start + HEADER_BYTES);
        assert_eq!(&packet.payload()[6..8], &[0xff, 0x7f], "little-endian");
        let Content::Pcm(samples) = packet.content() else {
            panic!("pcm")
        };
        assert_eq!(samples.collect::<Vec<_>>(), pcm);
    }

    #[test]
    fn render_frames_carry_the_playback_generation_and_no_device_timing() {
        let pcm = vec![7i16; 3840];
        let bytes = encode_render(0x1234, 9, 3840 * 9, 17, Flags::DISCONTINUITY, &pcm).unwrap();
        let h = *Packet::parse(bytes).unwrap().header();
        assert_eq!(
            (h.kind, h.epoch, h.sequence, h.first_sample),
            (Kind::Render, 0x1234, 9, 34_560)
        );
        assert_eq!(
            (h.generation, h.mono_ns, h.uncertainty_us, h.frame_count),
            (17, 0, None, 3840)
        );
        assert_eq!(
            encode_render(0, 0, 0, 1, Flags::NONE, &pcm),
            Err(FrameError::ZeroEpoch)
        );
        assert!(matches!(
            encode_render(1, 0, 0, 1, Flags::NONE, &vec![0; 3841]),
            Err(FrameError::FrameCount { count: 3841, .. })
        ));
        assert!(matches!(
            encode_render(1, 0, 0, 1, Flags::NONE, &[]),
            Err(FrameError::FrameCount { count: 0, .. })
        ));
        assert_eq!(
            encode_render(1, 0, 0, 1, Flags::DIGITAL_SILENCE, &pcm),
            Err(FrameError::DigitalSilenceOutsideReference(Kind::Render))
        );
    }

    #[test]
    fn digital_silence_has_no_payload_and_stands_for_zeros() {
        let packet = Packet::parse(frame(&valid_headers()[1])).unwrap();
        assert!(packet.payload().is_empty());
        assert!(matches!(
            packet.content(),
            Content::DigitalSilence { frames: 1280 }
        ));
        assert_eq!(packet.end_sample(), 2560 + 1280);
    }

    /// The firmware's `TestCellValidation` raw vectors.
    #[test]
    fn cell_records_follow_the_firmware_vectors() {
        let cases: [([u8; 4], Result<(), CellError>); 6] = [
            ([0x01, 0x00, 0, 0], Err(CellError::Loudness(1))),
            ([0x1f, 0xd1, 0, 0], Err(CellError::Loudness(-12_001))),
            ([0x00, 0x00, 4, 0], Err(CellError::UndefinedFlags(4))),
            ([0x00, 0x00, 0, 16], Err(CellError::UndefinedSources(16))),
            ([0x00, 0x00, 1, 0], Err(CellError::GapLoudness(0))),
            ([0x20, 0xd1, 3, 1], Ok(())),
        ];
        for (raw, want) in cases {
            assert_eq!(Cell::parse(&raw).map(|_| ()), want, "{raw:02x?}");
        }
        let cell = Cell {
            e_centi: -4321,
            flags: CellFlags::MUTED,
            mask: SourceMask::CONTENT | SourceMask::DIALOG,
        };
        assert_eq!(Cell::parse(&cell.to_bytes()), Ok(cell));
    }

    #[test]
    fn cell_packets_decode_their_records_and_cover_512_samples_each() {
        let cells = [
            Cell {
                e_centi: -5000,
                flags: CellFlags::NONE,
                mask: SourceMask::CONTENT,
            },
            Cell {
                e_centi: Cell::E_MIN,
                flags: CellFlags::GAP,
                mask: SourceMask::NONE,
            },
            Cell {
                e_centi: 0,
                flags: CellFlags::MUTED,
                mask: SourceMask::DIALOG | SourceMask::EARCON,
            },
        ];
        let payload: Vec<u8> = cells.iter().flat_map(|c| c.to_bytes()).collect();
        let packet = Packet::parse(
            encode(&header(Kind::Cells, Flags::NONE, 3, 0, 1024, 3), &payload).unwrap(),
        )
        .unwrap();
        let Content::Cells(records) = packet.content() else {
            panic!("cells")
        };
        assert_eq!(records.collect::<Vec<_>>(), cells);
        assert_eq!(packet.end_sample(), 1024 + 3 * 512);

        let mut bad = payload.clone();
        bad[4..8].copy_from_slice(&[0, 0, 1, 0]);
        let mut raw = Raw::of(&header(Kind::Cells, Flags::NONE, 3, 0, 1024, 3)).bytes();
        raw.extend_from_slice(&bad);
        assert_eq!(
            Packet::parse(Bytes::from(raw)),
            Err(FrameError::Cell {
                index: 1,
                error: CellError::GapLoudness(0)
            })
        );
    }

    /// The firmware's `TestAFERecordRoundTripAndLayout` and
    /// `TestAFERecordValidation`.
    #[test]
    fn afe_records_follow_the_firmware_layout_and_bounds() {
        let r = AfeRecord {
            frames: 10,
            flags: AfeFlags::GAP | AfeFlags::OUTPUT_CLIPPED,
            playback: 7,
            erle_max: 26,
            erle_mean: 14,
            erle_frames: 6,
            dtd_max: 31,
            dtd_frames: 2,
            rms_max: 218,
            rms_mean: 200,
            vad_max: 3,
            vad_frames: 4,
            volume: 127,
            lost: 255,
        };
        assert_eq!(
            r.to_bytes(),
            [10, 0b101, 7, 26, 14, 6, 31, 2, 218, 200, 3, 4, 127, 255]
        );
        assert_eq!(AfeRecord::parse(&r.to_bytes()), Ok(r));

        let ok = AfeRecord {
            frames: 10,
            playback: 10,
            erle_frames: 10,
            dtd_frames: 10,
            vad_frames: 10,
            dtd_max: 31,
            vad_max: 3,
            volume: 127,
            ..AfeRecord::default()
        };
        assert!(AfeRecord::parse(&ok.to_bytes()).is_ok());
        type Mutation = fn(&mut AfeRecord);
        let out_of_range: [(&str, Mutation); 9] = [
            ("frames", |r| r.frames = 11),
            ("playback", |r| r.frames = 9),
            ("dtd_max", |r| r.dtd_max = 32),
            ("vad_max", |r| r.vad_max = 4),
            ("volume", |r| r.volume = 128),
            ("erle_frames", |r| {
                r.frames = 3;
                r.playback = 3;
                r.erle_frames = 4;
            }),
            ("dtd_frames", |r| {
                r.frames = 3;
                r.playback = 0;
                r.erle_frames = 0;
                r.dtd_frames = 4;
            }),
            ("vad_frames", |r| {
                r.frames = 3;
                r.playback = 0;
                r.erle_frames = 0;
                r.dtd_frames = 0;
                r.vad_frames = 4;
            }),
            ("playback", |r| {
                r.frames = 0;
            }),
        ];
        for (field, mutate) in out_of_range {
            let mut bad = ok;
            mutate(&mut bad);
            assert!(
                matches!(AfeRecord::parse(&bad.to_bytes()), Err(AfeError::OutOfRange { field: f, .. }) if f == field),
                "{field}: {:?}",
                AfeRecord::parse(&bad.to_bytes())
            );
        }
        for bit in [6, 7] {
            let mut raw = ok.to_bytes();
            raw[1] = 1 << bit;
            assert_eq!(
                AfeRecord::parse(&raw),
                Err(AfeError::ReservedFlags(1 << bit))
            );
        }
    }

    #[test]
    fn afe_packets_decode_their_records_and_cover_1280_samples_each() {
        let rec = AfeRecord {
            frames: 10,
            flags: AfeFlags::SYNC,
            volume: 70,
            rms_max: 200,
            ..AfeRecord::default()
        };
        let empty = AfeRecord::default();
        let payload: Vec<u8> = [rec, empty].iter().flat_map(|r| r.to_bytes()).collect();
        let h = header(Kind::Afe, Flags::DISCONTINUITY, 8, 0, 2560, 2);
        let packet = Packet::parse(encode(&h, &payload).unwrap()).unwrap();
        let Content::Afe(records) = packet.content() else {
            panic!("afe")
        };
        assert_eq!(records.collect::<Vec<_>>(), [rec, empty]);
        assert_eq!(packet.end_sample(), 2560 + 2 * 1280);
        let mut bad = payload;
        bad[14 + 12] = 128;
        assert_eq!(
            encode(&h, &bad),
            Err(FrameError::Afe {
                index: 1,
                error: AfeError::OutOfRange {
                    field: "volume",
                    value: 128,
                    max: 127
                }
            })
        );
    }
}
