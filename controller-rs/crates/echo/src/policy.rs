//! The comparator's and the reference scorer's values, each with where it comes from.
//!
//! Every value below is either written down in the post-AFE architecture (§5.2, §5.3, §16.6) or
//! copied from the firmware's wake detector (`device/internal/wakeword/detector`), except the
//! per-cell lag refinement, which is this crate's own engineering choice and says why.

use echomuse_turn::AttributionPolicy;
use serde::{Deserialize, Serialize};

/// Microphone and reference samples per second.
pub const SAMPLE_RATE: u32 = 16_000;
/// Samples in one analysis cell (32 ms), the grid of the device's loudness cells and the VAD.
pub const CELL_SAMPLES: usize = 512;
/// The wake model's window: 1.4 s (firmware `WindowSamples`).
pub const WAKE_WINDOW_SAMPLES: usize = 22_400;
/// The wake scorer's hop: two 80 ms capture blocks, 160 ms (firmware `HopSamples`).
pub const WAKE_HOP_SAMPLES: u64 = 2_560;

/// The echo comparison's values (post-AFE architecture §16.6 "Reference comparison" and
/// "Per-cell echo comparison").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EchoPolicy {
    /// The longest acoustic delay searched, in samples: 500 ms. §16.6 searches 0–500 ms past
    /// the clock estimate, and §5.2 sizes the reference backfill for "up to 500 ms of lag".
    /// The real render-to-capture delay distribution is still one of the architecture's
    /// "Measurement gates, not open design questions", so this is the documented bound, not a
    /// measured one.
    pub lag_max: u32,
    /// The microphone audio one lag estimate correlates: 1 s (§16.6 "from the preceding 1 s").
    pub lag_window: u32,
    /// How often the lag is re-estimated: every second (§16.6).
    pub lag_every: u32,
    /// A lag is accepted only with at least this many 32 ms cells (192 ms) of non-silent
    /// reference under the microphone window (§16.6).
    pub lag_min_active_cells: u32,
    /// ...and only when the correlation peak beats the best value outside
    /// [`EchoPolicy::lag_peak_exclusion`] by at least this much (§16.6). For one second of
    /// speech-band audio a chance correlation peak is about 0.05, so 0.10 sits clear of chance.
    pub lag_peak_margin: f64,
    /// The neighbourhood of the peak, in samples, that the runner-up must lie outside: ±20 ms
    /// (§16.6).
    pub lag_peak_exclusion: u32,
    /// How far, in samples, the per-cell comparison may move the established lag to the best
    /// correlation of its own window. Not in the architecture: the lag is re-estimated once a
    /// second, and two device clocks that disagree by a crystal's 100 ppm drift 1.6 samples
    /// apart in that time. A one-sample slip costs a speech-band correlation up to a few
    /// hundredths, which would move cells across the 0.92 boundary for no acoustic reason.
    /// ±2 samples (125 µs) absorbs the drift and is far too small to explain near-end speech.
    pub lag_refine: u32,
    /// Cells in the trailing window each per-cell label is computed over: 6 (192 ms, §16.6).
    pub window_cells: u32,
    /// The reference counts as silent below this RMS, in dBFS: −60 (§16.6).
    pub silence_dbfs: f64,
    /// At least this share of the compared cells must be valid: 80% (§16.6).
    pub min_valid_share: f64,
    /// `echo_only` needs at least this correlation in a speech-positive cell: 0.92 (§16.6).
    pub echo_correlation_min: f64,
    /// ...and at most this unexplained share of the cell's energy: 0.20 (§16.6). With the
    /// one-gain fit the unexplained share is `1 − ρ²`, so 0.20 is ρ ≈ 0.894, consistent with
    /// the correlation clause.
    pub echo_unexplained_max: f64,
    /// ...in at least this share of the speech-positive compared cells: 90% (§16.6).
    pub echo_cell_share: f64,
    /// Unexplained energy this far above `B`, in dB, in two consecutive speech-positive cells is
    /// near-end sound: 6 dB (§16.6).
    pub near_end_margin_db: f64,
    /// A valid cell with VAD at or above this is speech-positive: 0.50 (§16.6, and
    /// [`AttributionPolicy::speech_positive_min`] in the speech policy).
    pub speech_positive_vad: f32,
    /// `B` leaves out cells with VAD at or above this: 0.35, the speech policy's
    /// [`AttributionPolicy::non_speech_max`], so `B` here is the reducer's `B`.
    pub background_exclude_vad: f32,
}

impl Default for EchoPolicy {
    fn default() -> Self {
        let attribution = AttributionPolicy::default();
        Self {
            lag_max: 8_000,
            lag_window: 16_000,
            lag_every: 16_000,
            lag_min_active_cells: 6,
            lag_peak_margin: 0.10,
            lag_peak_exclusion: 320,
            lag_refine: 2,
            window_cells: 6,
            silence_dbfs: -60.0,
            min_valid_share: 0.80,
            echo_correlation_min: 0.92,
            echo_unexplained_max: 0.20,
            echo_cell_share: 0.90,
            near_end_margin_db: 6.0,
            speech_positive_vad: attribution.speech_positive_min,
            background_exclude_vad: attribution.non_speech_max,
        }
    }
}

impl EchoPolicy {
    /// The silence threshold as a mean square of samples scaled to ±1.
    pub(crate) fn silence_mean_square(&self) -> f64 {
        10f64.powf(self.silence_dbfs / 10.0)
    }
}

/// The reference wake scorer's values (post-AFE architecture §5.2 and §5.3; the firmware's
/// detector constants for everything the architecture says is "the same" as the device's).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReferenceWakePolicy {
    /// A reference candidate opens when the mean of the last three raw scores reaches this:
    /// 0.30 (§5.3: deliberately more sensitive than the microphone's 0.65/0.90, because a
    /// missed self-detection admits a false wake while an extra one only feeds attribution).
    pub threshold: f64,
    /// Raw scores in the smoothing mean: 3 (firmware `SmoothingWindows`).
    pub smoothing_windows: usize,
    /// An open candidate extends while the smoothed score is at least this share of its
    /// threshold: 0.5 (firmware `extendFraction`).
    pub extend_fraction: f64,
    /// ...and closes after this many consecutive scored hops below it: 2 (firmware
    /// `closeAfterBelow`).
    pub close_after_below: u32,
    /// This many consecutive unscored (below the RMS floor) hops clear the smoothing history:
    /// 6, 960 ms (firmware `ClearAfterUnscored`).
    pub clear_after_unscored: u32,
}

impl Default for ReferenceWakePolicy {
    fn default() -> Self {
        Self {
            threshold: 0.30,
            smoothing_windows: 3,
            extend_fraction: 0.5,
            close_after_below: 2,
            clear_after_unscored: 6,
        }
    }
}
