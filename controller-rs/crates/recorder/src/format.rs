//! The on-disk framing of a session file. The crate documentation describes the format in
//! full; this module is its single implementation.

use std::io::{self, Write};

use bytes::{Buf, Bytes};
use serde::Serialize;

/// The first eight bytes of every session file. The `\r\n` and `^Z` catch a file mangled by
/// text-mode transfer, as PNG's signature does.
pub const MAGIC: [u8; 8] = *b"EMREC\r\n\x1a";
/// The format version this crate writes and reads.
pub const FORMAT_VERSION: u16 = 1;
/// The preamble: magic, version, six zero bytes.
pub(crate) const PREAMBLE_BYTES: usize = 16;
/// The largest record body a reader accepts: more than any frame (64 + 7,680 bytes) or event.
pub(crate) const MAX_RECORD_BYTES: u32 = 16 * 1024 * 1024;

/// What a record holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum RecordType {
    /// JSON [`Header`](crate::Header); always the first record.
    Header = 1,
    /// JSON [`Entry`](crate::Entry).
    Event = 2,
    /// Binary frame: seq, time and ingress as little-endian `u64`s, then the EMA1 frame.
    Frame = 3,
    /// JSON [`Marker`](crate::Marker).
    Marker = 4,
    /// JSON [`End`](crate::End); always the last record of a file that ended in order.
    End = 5,
}

impl RecordType {
    pub(crate) fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            1 => Self::Header,
            2 => Self::Event,
            3 => Self::Frame,
            4 => Self::Marker,
            5 => Self::End,
            _ => return None,
        })
    }
}

pub(crate) fn write_preamble(w: &mut impl Write) -> io::Result<()> {
    w.write_all(&MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&[0; 6])
}

/// Writes one record: `u32` body length, then the body (type byte and payload pieces), then the
/// CRC-32 of the body.
pub(crate) fn write_record(w: &mut impl Write, ty: RecordType, pieces: &[&[u8]]) -> io::Result<()> {
    let len: usize = 1 + pieces.iter().map(|p| p.len()).sum::<usize>();
    let len = u32::try_from(len)
        .ok()
        .filter(|l| *l <= MAX_RECORD_BYTES)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "record too large"))?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(&[ty as u8]);
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&[ty as u8])?;
    for p in pieces {
        crc.update(p);
        w.write_all(p)?;
    }
    w.write_all(&crc.finalize().to_le_bytes())
}

pub(crate) fn write_json(
    w: &mut impl Write,
    ty: RecordType,
    value: &impl Serialize,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(io::Error::other)?;
    write_record(w, ty, &[&body])
}

pub(crate) fn write_frame(
    w: &mut impl Write,
    seq: u64,
    t: u64,
    ingress: u64,
    frame: &[u8],
) -> io::Result<()> {
    let mut head = [0u8; 24];
    head[..8].copy_from_slice(&seq.to_le_bytes());
    head[8..16].copy_from_slice(&t.to_le_bytes());
    head[16..].copy_from_slice(&ingress.to_le_bytes());
    write_record(w, RecordType::Frame, &[&head, frame])
}

/// Why the preamble was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreambleError {
    NotARecording,
    UnsupportedVersion(u16),
}

pub(crate) fn check_preamble(bytes: &[u8]) -> Result<(), PreambleError> {
    let Some(pre) = bytes.first_chunk::<PREAMBLE_BYTES>() else {
        return Err(PreambleError::NotARecording);
    };
    if pre[..8] != MAGIC || pre[10..] != [0; 6] {
        return Err(PreambleError::NotARecording);
    }
    let version = u16::from_le_bytes([pre[8], pre[9]]);
    if version != FORMAT_VERSION {
        return Err(PreambleError::UnsupportedVersion(version));
    }
    Ok(())
}

/// One record as read back.
#[derive(Debug)]
pub(crate) struct RawRecord {
    pub(crate) ty: RecordType,
    pub(crate) payload: Bytes,
}

/// Why reading records stopped before the end of the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TailError {
    /// The last record is cut short (a crash or a file still being written).
    Truncated,
    /// A record's checksum, length or type is wrong.
    Corrupt,
}

/// Reads records from `rest` (the bytes after the preamble) until it is empty or a record is
/// cut short or corrupt.
pub(crate) fn next_record(rest: &mut Bytes) -> Option<Result<RawRecord, TailError>> {
    if rest.is_empty() {
        return None;
    }
    if rest.len() < 4 {
        return Some(Err(TailError::Truncated));
    }
    let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
    if len == 0 || len > MAX_RECORD_BYTES {
        return Some(Err(TailError::Corrupt));
    }
    let total = 4 + len as usize + 4;
    if rest.len() < total {
        return Some(Err(TailError::Truncated));
    }
    let body = rest.slice(4..4 + len as usize);
    let crc = u32::from_le_bytes([
        rest[total - 4],
        rest[total - 3],
        rest[total - 2],
        rest[total - 1],
    ]);
    if crc32fast::hash(&body) != crc {
        return Some(Err(TailError::Corrupt));
    }
    let Some(ty) = RecordType::from_u8(body[0]) else {
        return Some(Err(TailError::Corrupt));
    };
    rest.advance(total);
    Some(Ok(RawRecord {
        ty,
        payload: body.slice(1..),
    }))
}
