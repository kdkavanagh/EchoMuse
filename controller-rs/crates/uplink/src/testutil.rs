//! Synthetic EMA1 packets for the tests.

use echomuse_protocol::ema::{
    self, AfeRecord, Cell, CellFlags, Flags, Header, Kind, Packet, SourceMask,
};

/// The device monotonic time the synthetic packets stamp on `sample`.
pub(crate) fn mono(sample: u64) -> u64 {
    1_000_000 + sample * 62_500
}

/// `n` distinguishable samples starting at `first`.
pub(crate) fn ramp(first: u64, n: u64) -> Vec<i16> {
    (first..first + n).map(|i| (i % 30_000) as i16).collect()
}

fn packet(header: Header, payload: &[u8]) -> Packet {
    Packet::parse(ema::encode(&header, payload).unwrap()).unwrap()
}

fn header(kind: Kind, epoch: u64, sequence: u64, first: u64, frames: u32, flags: Flags) -> Header {
    Header {
        kind,
        flags,
        epoch,
        sequence,
        first_sample: first,
        mono_ns: mono(first),
        uncertainty_us: Some(100),
        frame_count: frames,
        generation: 0,
        source_mask: SourceMask::NONE,
    }
}

/// A mic packet carrying `samples` from `first`.
pub(crate) fn mic(epoch: u64, sequence: u64, first: u64, samples: &[i16], flags: Flags) -> Packet {
    let payload: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    packet(
        header(
            Kind::Mic,
            epoch,
            sequence,
            first,
            samples.len() as u32,
            flags,
        ),
        &payload,
    )
}

/// A mic packet of the ramp over `[first, end)`.
pub(crate) fn mic_ramp(epoch: u64, sequence: u64, first: u64, end: u64, flags: Flags) -> Packet {
    mic(epoch, sequence, first, &ramp(first, end - first), flags)
}

/// A reference packet of `frames` samples, digital silence when `silent`.
pub(crate) fn reference(
    epoch: u64,
    sequence: u64,
    first: u64,
    frames: u32,
    silent: bool,
    mask: SourceMask,
) -> Packet {
    let mut h = header(Kind::Reference, epoch, sequence, first, frames, Flags::NONE);
    h.source_mask = mask;
    if silent {
        h.flags = Flags::DIGITAL_SILENCE;
        return packet(h, &[]);
    }
    let payload: Vec<u8> = ramp(first, u64::from(frames))
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    packet(h, &payload)
}

/// A cells packet of `n` cells from the cell starting at sample `first`.
pub(crate) fn cells(epoch: u64, sequence: u64, first: u64, n: u32) -> Packet {
    let payload: Vec<u8> = (0..u64::from(n))
        .flat_map(|i| {
            Cell {
                e_centi: -(((first / 512 + i) % 9_000) as i16),
                flags: CellFlags::NONE,
                mask: SourceMask::NONE,
            }
            .to_bytes()
        })
        .collect();
    packet(
        header(Kind::Cells, epoch, sequence, first, n, Flags::NONE),
        &payload,
    )
}

/// An AFE packet of `n` records from the period starting at sample `first`.
pub(crate) fn afe(epoch: u64, sequence: u64, first: u64, n: u32) -> Packet {
    let payload: Vec<u8> = (0..u64::from(n))
        .flat_map(|i| {
            AfeRecord {
                frames: 10,
                volume: ((first / 1_280 + i) % 100) as u8,
                ..AfeRecord::default()
            }
            .to_bytes()
        })
        .collect();
    packet(
        header(Kind::Afe, epoch, sequence, first, n, Flags::NONE),
        &payload,
    )
}
