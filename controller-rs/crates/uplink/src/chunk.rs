//! What a subscription delivers to its consumers: ordered evidence chunks of
//! each stream epoch, explicit missing ranges, and one final [`Ended`].

use std::num::NonZeroU64;

use bytes::Bytes;
use echomuse_protocol::StreamId;
use echomuse_protocol::ema::{AFE_RECORD_BYTES, AfeRecord, CELL_RECORD_BYTES, Cell};
use echomuse_protocol::streams::UplinkEnded;

use crate::buffer::{BlockMeta, Span};
use crate::wire::{CloseReason, Generation, Owner, SubscriptionId, SubscriptionReason};

/// Little-endian PCM16, usually a slice of the packet it arrived in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pcm(Bytes);

impl Pcm {
    pub(crate) fn from_le_bytes(bytes: Bytes) -> Self {
        Self(bytes)
    }

    /// The samples.
    pub fn samples(&self) -> impl ExactSizeIterator<Item = i16> + '_ {
        self.0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
    }

    /// How many samples it holds.
    pub fn len(&self) -> usize {
        self.0.len() / 2
    }

    /// Whether it holds no samples.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The raw little-endian bytes.
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }
}

/// The records of one data chunk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChunkData {
    /// Mic or reference PCM.
    Pcm(Pcm),
    /// Reference samples that were all zero; the device sent no payload.
    DigitalSilence {
        /// How many samples.
        frames: u64,
    },
    /// Loudness cells, one per 512 capture samples.
    Cells(Vec<Cell>),
    /// AFE period records, one per 1,280 capture samples.
    Afe(Vec<AfeRecord>),
}

impl ChunkData {
    /// The bytes these records took on the wire: 2 per PCM sample, none for
    /// digital silence, 4 per cell, 14 per AFE record.
    pub fn payload_bytes(&self) -> u64 {
        match self {
            ChunkData::Pcm(pcm) => pcm.as_bytes().len() as u64,
            ChunkData::DigitalSilence { .. } => 0,
            ChunkData::Cells(cells) => (cells.len() * CELL_RECORD_BYTES) as u64,
            ChunkData::Afe(records) => (records.len() * AFE_RECORD_BYTES) as u64,
        }
    }
}

/// Why a range of a subscription's stream will never be delivered. Each is
/// final: audio for the range that arrives later is not delivered to this
/// subscription, and nothing stands in for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingCause {
    /// The device skipped it: its own history had a missing range there, or
    /// the audio was overwritten before it could be sent. The next packet
    /// carried the discontinuity flag.
    Device,
    /// The subscription's backfill began later than requested, because the
    /// start was older than the device's history (the device reports the
    /// same in `uplink.ended.clipped_start`).
    Clipped,
    /// The device moved past it on this subscription's stream without
    /// sending it.
    NotSent,
    /// The stream's live position is further past it than the device's ring
    /// holds, so it can no longer be sent.
    Overwritten,
    /// Later audio waited behind it for longer than the subscription's hole
    /// wait, or the subscription's drain after `uplink.ended` ran out.
    TimedOut,
    /// The controller's buffer had dropped it before the subscription
    /// reached it.
    Evicted,
}

/// A chunk's content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChunkBody {
    /// Records, all with the same metadata.
    Data {
        /// Their timing and state.
        meta: BlockMeta,
        /// The device skipped missing audio just before the first record.
        discontinuity: bool,
        /// The records.
        data: ChunkData,
    },
    /// A range that will never arrive.
    Missing(MissingCause),
}

/// One ordered piece of one stream epoch for one subscription. Within a
/// subscription, stream and epoch, chunks follow each other without overlap
/// or holes. Positions of different epochs are not comparable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    /// The subscription.
    pub subscription: SubscriptionId,
    /// Its generation when the chunk was released.
    pub generation: Generation,
    /// The stream.
    pub stream: StreamId,
    /// The stream epoch: the capture epoch for mic, cells and AFE, a
    /// reference epoch for the reference.
    pub epoch: NonZeroU64,
    /// The positions it covers, in stream samples (capture samples for cells
    /// and AFE records).
    pub span: Span,
    /// The content.
    pub body: ChunkBody,
}

impl Chunk {
    /// The wire bytes of its records; 0 for a missing range.
    pub fn payload_bytes(&self) -> u64 {
        match &self.body {
            ChunkBody::Data { data, .. } => data.payload_bytes(),
            ChunkBody::Missing(_) => 0,
        }
    }
}

/// Why a subscription ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EndCause {
    /// The controller closed it with `uplink.close`.
    Closed(CloseReason),
    /// The device ended it with `uplink.ended`. Everything the report says was
    /// sent has been delivered or declared missing.
    Device(UplinkEnded),
    /// The capture epoch it belonged to ended.
    CaptureEpoch,
    /// The privacy mute went on. Its undelivered audio was discarded.
    Muted,
    /// The session ended. Its undelivered audio was discarded.
    SessionLost,
    /// The device refused a command about it.
    Rejected {
        /// The `command.ack` error code; `None` when the device gave none.
        code: Option<String>,
    },
    /// Its consumer did not drain its deliveries within the bounds, so the
    /// controller closed it. Treat it as an overload of the session.
    Overflow,
}

/// A subscription's last delivery.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ended {
    /// The subscription.
    pub subscription: SubscriptionId,
    /// What it served last.
    pub owner: Owner,
    /// Its reason last.
    pub reason: SubscriptionReason,
    /// Its generation last.
    pub generation: Generation,
    /// Why it ended.
    pub cause: EndCause,
}

/// One delivery for a subscription's consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Evidence.
    Chunk(Chunk),
    /// Nothing more follows for this subscription.
    Ended(Ended),
}

impl Delivery {
    /// The subscription it belongs to.
    pub fn subscription(&self) -> &SubscriptionId {
        match self {
            Delivery::Chunk(chunk) => &chunk.subscription,
            Delivery::Ended(ended) => &ended.subscription,
        }
    }

    /// The wire bytes of its records.
    pub fn payload_bytes(&self) -> u64 {
        match self {
            Delivery::Chunk(chunk) => chunk.payload_bytes(),
            Delivery::Ended(_) => 0,
        }
    }
}

/// Deliveries waiting for a consumer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Backlog {
    /// Deliveries queued.
    pub deliveries: usize,
    /// The wire bytes of their records.
    pub payload_bytes: u64,
}

/// Deliveries a consumer has taken.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Accounting {
    /// Chunks taken, data and missing.
    pub chunks: u64,
    /// The wire bytes of their records.
    pub payload_bytes: u64,
}
