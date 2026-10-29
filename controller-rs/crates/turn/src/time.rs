//! The two clocks the speech policy reads, kept as distinct types (§5.3).
//!
//! - **Sample time** is a position on one capture epoch's timeline: the index of a 16 kHz mic
//!   sample. Endpointing, reply windows and every pause length are measured in it, so a delayed
//!   link can neither shorten a window nor make late audio look like silence. Positions from
//!   different epochs are not comparable, and nothing here ever mixes them.
//! - **The controller's monotonic clock** carries deadlines and budgets: the wake decision budget,
//!   the backstops that close a turn when audio stops arriving, and the recognizer's answer limit.
//!   The device runtime passes the current instant in; nothing in this crate reads a clock.
//!
//! Wall time is not used anywhere in this crate.

use std::num::NonZeroU64;
use std::ops::{Add, Sub};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Mic samples per second.
pub const SAMPLE_RATE: u64 = 16_000;
/// Samples in one analysis cell: 32 ms, the VAD's input size and the device's loudness cell.
pub const CELL_SAMPLES: u64 = 512;
/// Samples in one capture block: 80 ms, the cadence at which the reducer is evaluated.
pub const BLOCK_SAMPLES: u64 = 1_280;

/// A capture epoch. Sample positions are only meaningful within one epoch, and the epoch
/// identifier is never zero on the wire (§3.1.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Epoch(NonZeroU64);

impl Epoch {
    /// The epoch with this wire value, or `None` for zero.
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// The wire value.
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A position on one epoch's 16 kHz mic timeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sample(pub u64);

/// A length of audio, counted in 16 kHz samples.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Samples(pub u64);

impl Samples {
    /// The length of `ms` milliseconds of audio.
    pub const fn from_ms(ms: u64) -> Self {
        Self(ms * SAMPLE_RATE / 1_000)
    }

    /// This length in whole milliseconds, rounded down.
    pub const fn as_ms(self) -> u64 {
        self.0 * 1_000 / SAMPLE_RATE
    }

    /// The number of whole cells in this length.
    pub const fn whole_cells(self) -> u64 {
        self.0 / CELL_SAMPLES
    }
}

impl Sample {
    /// The cell holding this sample.
    pub const fn cell(self) -> CellIndex {
        CellIndex(self.0 / CELL_SAMPLES)
    }

    /// The audio between `earlier` and this position; zero when `earlier` is not earlier.
    pub const fn since(self, earlier: Sample) -> Samples {
        Samples(self.0.saturating_sub(earlier.0))
    }

    /// This position rounded down to a multiple of `grid` samples.
    pub const fn round_down(self, grid: u64) -> Sample {
        Sample(self.0 - self.0 % grid)
    }
}

impl Add<Samples> for Sample {
    type Output = Sample;
    fn add(self, rhs: Samples) -> Sample {
        Sample(self.0.saturating_add(rhs.0))
    }
}

/// Subtraction stops at sample zero, the start of the epoch.
impl Sub<Samples> for Sample {
    type Output = Sample;
    fn sub(self, rhs: Samples) -> Sample {
        Sample(self.0.saturating_sub(rhs.0))
    }
}

/// A half-open range of samples, `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleSpan {
    /// The first sample in the range.
    pub start: Sample,
    /// The first sample after the range.
    pub end: Sample,
}

impl SampleSpan {
    /// The range `[start, end)`; an `end` before `start` gives an empty range at `start`.
    pub fn new(start: Sample, end: Sample) -> Self {
        Self {
            start,
            end: end.max(start),
        }
    }

    /// How much audio the range holds.
    pub fn len(&self) -> Samples {
        self.end.since(self.start)
    }

    /// Whether the range holds no audio.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// Whether the two ranges share at least one sample.
    pub fn overlaps(&self, other: &SampleSpan) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// The index of a 512-sample cell on an epoch's timeline: cell `k` covers samples
/// `[512k, 512k + 512)`. The device's loudness cells and the controller's VAD cells share this
/// grid, because every stream start is rounded down to it (§3.1.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CellIndex(pub u64);

impl CellIndex {
    /// The cell's first sample.
    pub const fn start(self) -> Sample {
        Sample(self.0 * CELL_SAMPLES)
    }

    /// The first sample after the cell.
    pub const fn end(self) -> Sample {
        Sample((self.0 + 1) * CELL_SAMPLES)
    }

    /// The cell after this one.
    pub const fn next(self) -> CellIndex {
        CellIndex(self.0 + 1)
    }
}

/// An instant on the controller's monotonic clock, in nanoseconds from an arbitrary origin the
/// device runtime chooses. Deadlines and budgets are measured on it, never on sample time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonoInstant(pub u64);

impl MonoInstant {
    /// The instant `d` after this one.
    pub fn after(self, d: Duration) -> MonoInstant {
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        MonoInstant(self.0.saturating_add(nanos))
    }

    /// The time from `earlier` to this instant; zero when `earlier` is later.
    pub fn since(self, earlier: MonoInstant) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_tile_the_timeline() {
        let c = Sample(1_364_068_800).cell();
        assert_eq!(c.start(), Sample(1_364_068_352));
        assert_eq!(c.end(), Sample(1_364_068_864));
        assert_eq!(c.next().start(), c.end());
    }

    #[test]
    fn durations_round_trip_through_milliseconds() {
        assert_eq!(Samples::from_ms(608), Samples(9_728));
        assert_eq!(Samples::from_ms(608).whole_cells(), 19);
        assert_eq!(Samples(9_728).as_ms(), 608);
        assert_eq!(Sample(10) - Samples(20), Sample(0));
        assert_eq!(Sample(10).since(Sample(20)), Samples(0));
    }

    #[test]
    fn epoch_zero_is_not_an_epoch() {
        assert!(Epoch::new(0).is_none());
        assert_eq!(Epoch::new(7).map(Epoch::get), Some(7));
    }

    #[test]
    fn monotonic_instants_saturate() {
        let t = MonoInstant(5);
        assert_eq!(t.after(Duration::from_nanos(10)), MonoInstant(15));
        assert_eq!(MonoInstant(5).since(MonoInstant(10)), Duration::ZERO);
    }
}
