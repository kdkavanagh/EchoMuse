//! Bounds and timings of one session's uplink, and what the session allows.

use std::time::Duration;

use echomuse_protocol::{Capabilities, Capability, StreamId};

/// How much of each stream the controller keeps per epoch, in stream samples
/// (capture samples for cells and AFE records).
///
/// The defaults cover the longest utterance (30 s with `extendedUtterances`,
/// §4.6) with its pre-roll, and for cells and AFE records also the 10 s of
/// loudness history before it, with room to spare. Consumers receive audio as
/// it arrives; the buffers serve later reads by range, such as the committed
/// span for Home Assistant's STT copy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retention {
    /// Mic samples: 40 s.
    pub mic: u64,
    /// Reference samples: 40 s.
    pub reference: u64,
    /// Capture samples of cells: 60 s.
    pub cells: u64,
    /// Capture samples of AFE records: 60 s.
    pub afe: u64,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            mic: 40 * 16_000,
            reference: 40 * 16_000,
            cells: 60 * 16_000,
            afe: 60 * 16_000,
        }
    }
}

impl Retention {
    /// The retention of `stream`.
    pub fn of(&self, stream: StreamId) -> u64 {
        match stream {
            StreamId::Mic => self.mic,
            StreamId::Reference => self.reference,
            StreamId::Cells => self.cells,
            StreamId::Afe => self.afe,
        }
    }
}

/// The uplink's bounds and timings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UplinkConfig {
    /// Per-epoch retention.
    pub retention: Retention,
    /// The most runs of distinct packet metadata one epoch buffer records;
    /// past it the oldest are dropped. Live packets carry at most 80 ms, so
    /// the default of 4,096 is several times what the default retention
    /// needs.
    pub max_blocks: usize,
    /// Capture epochs kept for reads: the current one and those before it
    /// that have not been erased by a mute.
    pub capture_epochs_kept: usize,
    /// Reference epochs kept for reads, the current one included.
    pub reference_epochs_kept: usize,
    /// How long later audio may wait behind a hole in a subscription's stream
    /// before the hole is declared missing. Backfill for a subscription can
    /// queue behind other subscriptions' runs, which the device sends at link
    /// speed: a candidate's whole backfill is about 430 kB. The default of
    /// 2 s is twice the device's own limit for queued live audio.
    pub hole_wait: Duration,
    /// The most wire bytes of deliveries one subscription may have waiting
    /// for its consumer before it is closed as overloaded: 2 MiB, about a
    /// minute of mic.
    pub max_backlog_bytes: u64,
    /// The most deliveries one subscription may have waiting.
    pub max_backlog_deliveries: usize,
}

impl Default for UplinkConfig {
    fn default() -> Self {
        Self {
            retention: Retention::default(),
            max_blocks: 4_096,
            capture_epochs_kept: 2,
            reference_epochs_kept: 2,
            hole_wait: Duration::from_secs(2),
            max_backlog_bytes: 2 << 20,
            max_backlog_deliveries: 8_192,
        }
    }
}

/// What a session's handshake allows.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SessionTerms {
    /// `session.ready` carried `afe_metadata`, so the `afe` stream exists:
    /// candidate subscriptions want it and `uplink.open` may name it.
    pub afe_metadata: bool,
    /// The device announced `near_miss_v1`, so it accepts `near_miss`
    /// subscriptions.
    pub near_miss: bool,
}

impl SessionTerms {
    /// The terms of a session whose hello announced `capabilities` and whose
    /// `session.ready` did or did not carry `afe_metadata`. The device honours
    /// `afe_metadata` only with `afe_metadata_v1`.
    pub fn new(capabilities: &Capabilities, afe_metadata_sent: bool) -> Self {
        Self {
            afe_metadata: afe_metadata_sent && capabilities.contains(Capability::AfeMetadata),
            near_miss: capabilities.contains(Capability::NearMiss),
        }
    }
}
