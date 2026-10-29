//! The room's background level `B` and the command speaker's foreground level `F`
//! (post-AFE architecture §8.2 and §16.6 "Activity and level").
//!
//! Both are percentiles of the device's per-cell loudness `E`, kept in hundredths of a dB so the
//! arithmetic is exact and deterministic. Percentiles interpolate linearly between the two
//! nearest ranks (rank `p·(n−1)` over the sorted values), the common default for a percentile of
//! a small sample.

use std::collections::VecDeque;

use crate::evidence::CellAudio;

/// `B` looks back over the 312 whole cells (9.98 s) before each cell.
const BACKGROUND_WINDOW_CELLS: usize = 312;
/// `B` needs 1 s of support: 32 counted cells (31.25 rounded up).
const BACKGROUND_MIN_CELLS: usize = 32;
/// `F` needs 10 cells (320 ms).
const FOREGROUND_MIN_CELLS: usize = 10;

/// The `p` percentile (0..=1) of sorted values in hundredths of a dB, as dB.
fn percentile_db(sorted: &[i16], p: f64) -> Option<f64> {
    let (&first, rest) = sorted.split_first()?;
    if rest.is_empty() {
        return Some(f64::from(first) / 100.0);
    }
    let rank = p * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = (lo + 1).min(sorted.len() - 1);
    let frac = rank - lo as f64;
    let v = f64::from(sorted[lo]) + (f64::from(sorted[hi]) - f64::from(sorted[lo])) * frac;
    Some(v / 100.0)
}

/// The room's floor `B`: the 20th percentile of `E` over the valid cells of the 10 s before a
/// cell, leaving out cells the controller's VAD knows to be speech (≥ 0.35). Cells without a VAD
/// result (from before the subscription) all count; gap cells and cells of unknown loudness never
/// do. A 20th percentile over 10 s lands in the pauses between sounds, so it measures the room,
/// not the talker.
#[derive(Debug)]
pub struct BackgroundLevel {
    window: VecDeque<Option<i16>>,
    scratch: Vec<i16>,
    speech_vad: f32,
}

impl BackgroundLevel {
    /// An empty estimate; cells with VAD at or above `speech_vad` are left out.
    pub fn new(speech_vad: f32) -> Self {
        Self {
            window: VecDeque::with_capacity(BACKGROUND_WINDOW_CELLS),
            scratch: Vec::with_capacity(BACKGROUND_WINDOW_CELLS),
            speech_vad,
        }
    }

    /// `B` for the next cell, in dB, or `None` until 1 s of cells counts.
    pub fn current_db(&mut self) -> Option<f64> {
        self.scratch.clear();
        self.scratch.extend(self.window.iter().flatten());
        if self.scratch.len() < BACKGROUND_MIN_CELLS {
            return None;
        }
        self.scratch.sort_unstable();
        percentile_db(&self.scratch, 0.20)
    }

    /// Adds a cell after it has been judged.
    pub fn push(&mut self, audio: &CellAudio) {
        let counted = match *audio {
            CellAudio::Present {
                energy_centi: Some(e),
                ..
            } if audio.vad().is_none_or(|p| p < self.speech_vad) => Some(e),
            _ => None,
        };
        if self.window.len() == BACKGROUND_WINDOW_CELLS {
            self.window.pop_front();
        }
        self.window.push_back(counted);
    }
}

/// The command speaker's level `F`: the 80th percentile of `E` over the utterance's command
/// speech, available once 10 cells count.
#[derive(Debug, Default)]
pub struct ForegroundLevel {
    sorted: Vec<i16>,
    seeded: bool,
}

impl ForegroundLevel {
    /// An empty estimate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds `F` from the wake word's cells, but only when there are at least 10 of them;
    /// otherwise `F` stays unavailable until 10 command-speech cells exist.
    pub fn seed(&mut self, energies: &[i16]) {
        if energies.len() >= FOREGROUND_MIN_CELLS {
            self.sorted.extend_from_slice(energies);
            self.sorted.sort_unstable();
            self.seeded = true;
        }
    }

    /// Adds one command-speech cell's loudness.
    pub fn add(&mut self, e_centi: i16) {
        let at = self.sorted.partition_point(|&v| v <= e_centi);
        self.sorted.insert(at, e_centi);
    }

    /// `F` in dB, or `None` while fewer than 10 cells count.
    pub fn current_db(&self) -> Option<f64> {
        if self.sorted.len() < FOREGROUND_MIN_CELLS {
            return None;
        }
        percentile_db(&self.sorted, 0.80)
    }

    /// Whether `F` was seeded from the wake word.
    pub fn seeded(&self) -> bool {
        self.seeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{EchoCell, VadInput};

    fn cell(e: i16, vad: Option<f32>) -> CellAudio {
        CellAudio::Present {
            energy_centi: Some(e),
            source_mask: Some(0),
            vad: vad.map_or(VadInput::Unknown, |p| VadInput::probability(p).unwrap()),
            echo: EchoCell::MISSING,
        }
    }

    #[test]
    fn percentiles_interpolate_between_ranks() {
        assert_eq!(percentile_db(&[], 0.2), None);
        assert_eq!(percentile_db(&[-5000], 0.2), Some(-50.0));
        // rank 0.2 * 4 = 0.8 between -60 and -50.
        let v = [-6000, -5000, -4000, -3000, -2000];
        assert!((percentile_db(&v, 0.2).unwrap() - (-52.0)).abs() < 1e-9);
        assert!((percentile_db(&v, 0.8).unwrap() - (-28.0)).abs() < 1e-9);
    }

    #[test]
    fn background_needs_a_second_of_cells_and_skips_speech_and_gaps() {
        let mut b = BackgroundLevel::new(0.35);
        for _ in 0..31 {
            b.push(&cell(-7000, None));
        }
        assert_eq!(b.current_db(), None, "31 cells is under 1 s");
        b.push(&CellAudio::Gap { muted: false });
        b.push(&cell(-3000, Some(0.35)));
        b.push(&CellAudio::Present {
            energy_centi: None,
            source_mask: None,
            vad: VadInput::Unknown,
            echo: EchoCell::MISSING,
        });
        assert_eq!(
            b.current_db(),
            None,
            "gaps, speech and unknown loudness never count"
        );
        b.push(&cell(-7000, Some(0.34)));
        assert_eq!(b.current_db(), Some(-70.0));
    }

    #[test]
    fn background_forgets_cells_older_than_ten_seconds() {
        let mut b = BackgroundLevel::new(0.35);
        for _ in 0..BACKGROUND_WINDOW_CELLS {
            b.push(&cell(-8000, None));
        }
        assert_eq!(b.current_db(), Some(-80.0));
        for _ in 0..BACKGROUND_WINDOW_CELLS {
            b.push(&cell(-6000, None));
        }
        assert_eq!(b.current_db(), Some(-60.0));
    }

    #[test]
    fn foreground_seeds_only_with_ten_cells() {
        let mut f = ForegroundLevel::new();
        f.seed(&[-5000; 9]);
        assert!(!f.seeded());
        assert_eq!(f.current_db(), None);
        for _ in 0..9 {
            f.add(-5000);
        }
        assert_eq!(f.current_db(), None);
        f.add(-4000);
        assert!(f.current_db().is_some());
        let mut g = ForegroundLevel::new();
        g.seed(&[-5000; 10]);
        assert!(g.seeded());
        assert_eq!(g.current_db(), Some(-50.0));
    }
}
