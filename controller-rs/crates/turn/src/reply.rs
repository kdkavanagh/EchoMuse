//! The early-answer watch (§4.5; post-AFE architecture §16.6 "Early answer").
//!
//! While a question that expects a reply plays, and through the wake chime after it, the reply
//! subscription's cells are watched for an answer that starts before the question has finished.
//! The question is interrupted after 15 consecutive cells (480 ms) of clear near-end speech: VAD
//! ≥ 0.85, the room's background known and the cell at least 12 dB above it, and a per-cell echo
//! result saying the sound is not the device's own (`near_end_present`, or no reference at all).
//! The onset is the first of those cells. Unknown evidence never qualifies, so missing reference
//! or loudness keeps the question playing and retains the audio instead.

use crate::evidence::{CellAudio, CellEvidence, EchoLabel};
use crate::level::BackgroundLevel;
use crate::time::Sample;

/// The early-answer rule's values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EarlyAnswerPolicy {
    /// Consecutive qualifying cells needed (15, 480 ms).
    pub cells: u32,
    /// VAD at or above this qualifies (0.85).
    pub vad_min: f32,
    /// The cell must be at least this far above `B`, in dB (12).
    pub above_background_db: f64,
    /// Cells with VAD at or above this are left out of `B` (0.35).
    pub background_speech_vad: f32,
}

impl Default for EarlyAnswerPolicy {
    fn default() -> Self {
        Self {
            cells: 15,
            vad_min: 0.85,
            above_background_db: 12.0,
            background_speech_vad: 0.35,
        }
    }
}

/// Watches a reply subscription's cells, in sample order, for an early answer.
#[derive(Debug)]
pub struct EarlyAnswerWatch {
    policy: EarlyAnswerPolicy,
    background: BackgroundLevel,
    streak: u32,
    streak_start: Option<Sample>,
}

impl EarlyAnswerWatch {
    /// A watch with no history.
    pub fn new(policy: EarlyAnswerPolicy) -> Self {
        Self {
            background: BackgroundLevel::new(policy.background_speech_vad),
            policy,
            streak: 0,
            streak_start: None,
        }
    }

    /// Takes the next cell; returns the answer's onset when this cell completes the run.
    pub fn observe(&mut self, cell: &CellEvidence) -> Option<Sample> {
        let background = self.background.current_db();
        self.background.push(&cell.audio);
        let qualifies = match cell.audio {
            CellAudio::Present { echo, .. } => {
                let vad_ok = cell.audio.vad().is_some_and(|p| p >= self.policy.vad_min);
                let level_ok = match (cell.audio.energy_db(), background) {
                    (Some(e), Some(b)) => e >= b + self.policy.above_background_db,
                    _ => false,
                };
                let echo_ok = matches!(
                    echo.label,
                    EchoLabel::NearEndPresent | EchoLabel::NoReference
                );
                vad_ok && level_ok && echo_ok
            }
            CellAudio::Gap { .. } => false,
        };
        if !qualifies {
            self.streak = 0;
            self.streak_start = None;
            return None;
        }
        let onset = *self.streak_start.get_or_insert(cell.cell.start());
        self.streak += 1;
        (self.streak == self.policy.cells).then_some(onset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{EchoCell, ReferenceCoverage, VadInput};
    use crate::time::CellIndex;

    fn cell(i: u64, e: i16, vad: f32, echo: EchoLabel) -> CellEvidence {
        CellEvidence {
            cell: CellIndex(i),
            audio: CellAudio::Present {
                energy_centi: Some(e),
                source_mask: Some(4),
                vad: VadInput::probability(vad).unwrap(),
                echo: EchoCell {
                    label: echo,
                    coverage: ReferenceCoverage::Full,
                },
            },
        }
    }

    /// 40 cells of a quiet room at -70 dB, so `B` is known.
    fn warmed() -> (EarlyAnswerWatch, u64) {
        let mut w = EarlyAnswerWatch::new(EarlyAnswerPolicy::default());
        for i in 0..40 {
            assert_eq!(
                w.observe(&cell(i, -7000, 0.05, EchoLabel::NoReference)),
                None
            );
        }
        (w, 40)
    }

    #[test]
    fn fifteen_qualifying_cells_interrupt_with_the_first_as_onset() {
        let (mut w, n) = warmed();
        for i in n..n + 14 {
            assert_eq!(
                w.observe(&cell(i, -5800, 0.9, EchoLabel::NearEndPresent)),
                None
            );
        }
        assert_eq!(
            w.observe(&cell(n + 14, -5800, 0.85, EchoLabel::NoReference)),
            Some(CellIndex(n).start())
        );
    }

    #[test]
    fn a_broken_run_starts_over() {
        let (mut w, n) = warmed();
        for i in n..n + 14 {
            w.observe(&cell(i, -5800, 0.9, EchoLabel::NearEndPresent));
        }
        // 11.99 dB above B does not qualify.
        assert_eq!(
            w.observe(&cell(n + 14, -5801, 0.9, EchoLabel::NearEndPresent)),
            None
        );
        for i in n + 15..n + 29 {
            assert_eq!(
                w.observe(&cell(i, -5800, 0.9, EchoLabel::NearEndPresent)),
                None
            );
        }
        assert_eq!(
            w.observe(&cell(n + 29, -5800, 0.9, EchoLabel::NearEndPresent)),
            Some(CellIndex(n + 15).start())
        );
    }

    #[test]
    fn unknown_echo_or_background_never_qualifies() {
        let (mut w, n) = warmed();
        for i in n..n + 20 {
            assert_eq!(w.observe(&cell(i, -5800, 0.9, EchoLabel::Unknown)), None);
            assert_eq!(w.observe(&cell(i, -5800, 0.9, EchoLabel::EchoOnly)), None);
        }
        // Without B (a fresh watch) nothing qualifies.
        let mut w = EarlyAnswerWatch::new(EarlyAnswerPolicy::default());
        for i in 0..20 {
            assert_eq!(
                w.observe(&cell(i, -3000, 0.99, EchoLabel::NoReference)),
                None
            );
        }
    }
}
