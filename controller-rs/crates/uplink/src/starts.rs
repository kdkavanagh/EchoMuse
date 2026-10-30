//! Where a subscription's streams start, and the device's ring sizes
//! (spec §3.1.6, `docs/protocol-v1.md` §4.5, post-AFE architecture §4.4,
//! `device/internal/uplink` and `device/internal/audio/ring`).
//!
//! The device computes a candidate subscription's starts itself, so these
//! formulas are contract: mic `round_down(support_start − 4800, 512)`, cells
//! 160,000 samples before the mic start, reference 40,000 before it, and AFE
//! 40,000 before it rounded down to 1,280. Every subtraction stops at sample 0,
//! the start of the capture epoch, as the firmware's does. The mic and cell
//! formulas are the turn crate's ([`TurnSpec::candidate_mic_start`],
//! [`TurnSpec::cells_start`]); this module adds the reference and AFE ones and
//! the grids the device rounds every start to.

use echomuse_protocol::StreamId;
use echomuse_protocol::ema::{AFE_PERIOD_SAMPLES, CELL_SAMPLES};
use echomuse_turn::{Sample, Samples, TurnSpec};

/// The reference and AFE lead before the mic start: 2.5 s.
const REFERENCE_LEAD: Samples = Samples(40_000);
/// A near miss's AFE lead before its support start: 1 s.
pub(crate) const NEAR_MISS_AFE_LEAD: Samples = Samples(16_000);
/// How far past a near miss's support end its mic must reach: 300 ms.
pub(crate) const NEAR_MISS_TAIL: Samples = Samples(4_800);
/// How much further the mic may run while a near miss's AFE records are
/// still missing before the controller closes it anyway: 1 s.
pub(crate) const NEAR_MISS_GRACE: Samples = Samples(16_000);

/// The starts the device gives a candidate subscription, in capture samples.
/// The reference start is a capture sample too: the device maps it onto the
/// reference epoch through its clock fits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CandidateStarts {
    /// `round_down(support_start − 4800, 512)`.
    pub mic: Sample,
    /// The first sample of the cell holding `mic − 160,000`.
    pub cells: Sample,
    /// `mic − 40,000`.
    pub reference: Sample,
    /// `round_down(mic − 40,000, 1280)`, only in an `afe_metadata` session.
    pub afe: Option<Sample>,
}

impl CandidateStarts {
    /// The starts for a candidate whose wake word's support starts at
    /// `support_start`. A button turn uses the same formulas with the press
    /// sample in place of the support start (300 ms of pre-roll).
    pub fn new(support_start: Sample, afe_session: bool) -> Self {
        let mic = TurnSpec::candidate_mic_start(support_start);
        Self {
            mic,
            cells: TurnSpec::cells_start(mic).start(),
            reference: reference_start(mic),
            afe: afe_session.then(|| afe_start(mic)),
        }
    }
}

/// The reference start for a mic start: 40,000 capture samples before it.
fn reference_start(mic_start: Sample) -> Sample {
    mic_start - REFERENCE_LEAD
}

/// The AFE start for a mic start: 40,000 samples before it, rounded down to a
/// 1,280-sample capture period.
fn afe_start(mic_start: Sample) -> Sample {
    (mic_start - REFERENCE_LEAD).round_down(AFE_PERIOD_SAMPLES)
}

/// The grid the device rounds a stream's start to, in stream samples: 512
/// for mic and cells, 1,280 for AFE records, and 2,560 for the reference
/// (after mapping the start into the reference epoch).
pub(crate) const fn device_grid(stream: StreamId) -> u64 {
    match stream {
        StreamId::Mic | StreamId::Cells => CELL_SAMPLES,
        StreamId::Afe => AFE_PERIOD_SAMPLES,
        StreamId::Reference => 2_560,
    }
}

/// Where a capture stream requested at capture sample `requested` really
/// starts, on its [`device_grid`]. `None` for the reference, whose start the
/// device maps into another epoch.
pub(crate) fn device_grid_start(stream: StreamId, requested: Sample) -> Option<Sample> {
    (stream != StreamId::Reference).then(|| requested.round_down(device_grid(stream)))
}

/// How much of a stream the device's ring holds, in stream samples (capture
/// samples for cells and AFE records): 6 s of mic, 8 s of reference, 16 s of
/// cells and 8 s of AFE records (`device/internal/audio/ring`). Once the
/// stream's live position is this far past a sample, the device can never
/// send it again.
pub(crate) const fn device_history(stream: StreamId) -> u64 {
    match stream {
        StreamId::Mic => 96_000,
        StreamId::Reference => 128_000,
        StreamId::Cells => 500 * CELL_SAMPLES,
        StreamId::Afe => 100 * AFE_PERIOD_SAMPLES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_formulas_at_their_boundaries() {
        // Support start too early for 300 ms of pre-roll: everything starts at
        // the epoch's first sample.
        let early = CandidateStarts::new(Sample(4_799), true);
        assert_eq!(
            early,
            CandidateStarts {
                mic: Sample(0),
                cells: Sample(0),
                reference: Sample(0),
                afe: Some(Sample(0)),
            }
        );
        // The mic grid: 511 samples past the lead still round to 0, 512 do not.
        assert_eq!(
            CandidateStarts::new(Sample(4_800 + 511), false).mic,
            Sample(0)
        );
        assert_eq!(
            CandidateStarts::new(Sample(4_800 + 512), false).mic,
            Sample(512)
        );
        // Cells: the cell holding the sample 160,000 before the mic start.
        let mic = Sample(161_792);
        let at = CandidateStarts::new(mic + Samples(4_800), false);
        assert_eq!(at.mic, mic);
        assert_eq!(at.cells, Sample(1_536));
        let short = CandidateStarts::new(Sample(159_744 + 4_800), false);
        assert_eq!(short.mic, Sample(159_744));
        assert_eq!(short.cells, Sample(0));
        // 160,000 is not a whole number of cells.
        let odd = CandidateStarts::new(Sample(160_256 + 4_800), false);
        assert_eq!(odd.mic, Sample(160_256));
        assert_eq!(odd.cells, Sample(0));
        let next = CandidateStarts::new(Sample(160_768 + 4_800), false);
        assert_eq!(next.mic, Sample(160_768));
        assert_eq!(next.cells, Sample(512));
        // Reference and AFE: 40,000 before, the AFE on 1,280-sample periods.
        assert_eq!(reference_start(Sample(41_279)), Sample(1_279));
        assert_eq!(afe_start(Sample(41_279)), Sample(0));
        assert_eq!(afe_start(Sample(41_280)), Sample(1_280));
        assert_eq!(afe_start(Sample(39_000)), Sample(0));
        assert_eq!(CandidateStarts::new(Sample(100_000), false).afe, None);
    }

    #[test]
    fn device_grids() {
        assert_eq!(
            device_grid_start(StreamId::Mic, Sample(1_023)),
            Some(Sample(512))
        );
        assert_eq!(
            device_grid_start(StreamId::Cells, Sample(1_024)),
            Some(Sample(1_024))
        );
        assert_eq!(
            device_grid_start(StreamId::Afe, Sample(2_559)),
            Some(Sample(1_280))
        );
        assert_eq!(device_grid_start(StreamId::Reference, Sample(9)), None);
    }
}
