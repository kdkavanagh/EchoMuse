//! The three-way echo comparison over a run of aligned cells (post-AFE architecture §16.6
//! "Reference comparison").
//!
//! For each 32 ms cell, with the reference already moved to the lag, the device's output is
//! scaled to the microphone by one gain `a = dot(mic, ref) / dot(ref, ref)`, and what that
//! scaled output does not explain is the residual `mic − a·ref`. A cell is described by:
//!
//! - its **correlation**, the mean-subtracted normalized correlation of microphone and
//!   reference;
//! - its **unexplained ratio**, `Σ(mic − a·ref)² / max(Σ mic², 1e-12)`;
//! - its **unexplained energy**, `10·log10(mean((mic − a·ref)²) + 1e-12)` dB over samples
//!   scaled to ±1, the units of the device's loudness `E` and of `B`.
//!
//! The run's verdict is then:
//!
//! - **`near_end_present`** when two consecutive speech-positive cells each have unexplained
//!   energy at least 6 dB above `B`: something loud that the device did not play;
//! - **`echo_only`** when that is not so and at least 90% of the speech-positive cells have
//!   correlation ≥ 0.92 and unexplained ratio ≤ 0.20: the device's output explains the speech;
//! - **unknown** otherwise, with the reason.
//!
//! Both definite verdicts also need at least 80% of the compared cells valid, `B` known for
//! every speech-positive cell, and at least one speech-positive cell. The architecture does not
//! say what a run without speech-positive cells is; here it is unknown, because it holds no
//! evidence either way, and unknown is the verdict that never rejects a wake.

use serde::{Deserialize, Serialize};

use crate::policy::EchoPolicy;

/// The residual energy floor of §16.6, which keeps the logarithm finite.
const ENERGY_FLOOR: f64 = 1e-12;

/// One cell's comparison, at the lag.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CellMeasure {
    /// The microphone audio of the cell is present.
    pub valid: bool,
    /// The cell is valid and its VAD is at or above the speech-positive line.
    pub speech_positive: bool,
    /// Correlation of microphone and reference (0 when either is flat).
    pub correlation: f64,
    /// Share of the microphone energy the scaled reference does not explain.
    pub unexplained_ratio: f64,
    /// Energy of the residual, in dB.
    pub unexplained_db: f64,
    /// `B` for the cell, in dB, when known.
    pub background_db: Option<f64>,
}

impl CellMeasure {
    /// An invalid cell: no microphone audio to compare.
    pub const INVALID: CellMeasure = CellMeasure {
        valid: false,
        speech_positive: false,
        correlation: 0.0,
        unexplained_ratio: 1.0,
        unexplained_db: -120.0,
        background_db: None,
    };

    /// Measures one cell of microphone audio against its aligned reference (both scaled to
    /// ±1, the same length).
    pub fn measure(
        mic: &[f32],
        reference: &[f32],
        speech_positive: bool,
        background_db: Option<f64>,
    ) -> Self {
        debug_assert_eq!(mic.len(), reference.len());
        let n = mic.len() as f64;
        let (mut sm, mut sr, mut smm, mut srr, mut smr) = (0.0, 0.0, 0.0, 0.0, 0.0);
        for (&m, &r) in mic.iter().zip(reference) {
            let (m, r) = (f64::from(m), f64::from(r));
            sm += m;
            sr += r;
            smm += m * m;
            srr += r * r;
            smr += m * r;
        }
        let var_m = smm - sm * sm / n;
        let var_r = srr - sr * sr / n;
        let correlation = if var_m > ENERGY_FLOOR && var_r > ENERGY_FLOOR {
            (smr - sm * sr / n) / (var_m * var_r).sqrt()
        } else {
            0.0
        };
        let a = if srr > ENERGY_FLOOR { smr / srr } else { 0.0 };
        // Σ(m − a·r)² expanded: no second pass over the samples.
        let residual = (smm - 2.0 * a * smr + a * a * srr).max(0.0);
        Self {
            valid: true,
            speech_positive,
            correlation,
            unexplained_ratio: residual / smm.max(ENERGY_FLOOR),
            unexplained_db: 10.0 * (residual / n + ENERGY_FLOOR).log10(),
            background_db,
        }
    }

    fn explained(&self, policy: &EchoPolicy) -> bool {
        self.correlation >= policy.echo_correlation_min
            && self.unexplained_ratio <= policy.echo_unexplained_max
    }

    fn loud_residual(&self, policy: &EchoPolicy) -> bool {
        self.background_db
            .is_some_and(|b| self.unexplained_db >= b + policy.near_end_margin_db)
    }
}

/// Why a comparison could not decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Undecided {
    /// No lag is established.
    NoLag,
    /// Part of the reference needed is missing.
    ReferenceMissing,
    /// The device reported that some of its output bypassed the reference.
    PartialCoverage,
    /// Fewer than 80% of the compared cells hold microphone audio.
    TooFewValid,
    /// No compared cell is speech-positive.
    NoSpeech,
    /// `B` is unknown for a speech-positive cell.
    BackgroundUnavailable,
    /// Neither rule holds.
    Inconclusive,
}

/// The verdict over a run of cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict", content = "reason")]
pub enum Verdict {
    /// The device's output explains the speech-positive cells.
    EchoOnly,
    /// Sound the reference does not explain is present.
    NearEndPresent,
    /// Undecided, and why.
    Unknown(Undecided),
}

/// Applies the three-way rule to `cells` (already known to have full reference coverage).
pub fn judge(cells: &[CellMeasure], policy: &EchoPolicy) -> Verdict {
    let compared = cells.len();
    let valid = cells.iter().filter(|c| c.valid).count();
    if compared == 0 || (valid as f64) < policy.min_valid_share * compared as f64 - 1e-9 {
        return Verdict::Unknown(Undecided::TooFewValid);
    }
    let speech_positive = cells.iter().filter(|c| c.speech_positive).count();
    if speech_positive == 0 {
        return Verdict::Unknown(Undecided::NoSpeech);
    }
    if cells
        .iter()
        .any(|c| c.speech_positive && c.background_db.is_none())
    {
        return Verdict::Unknown(Undecided::BackgroundUnavailable);
    }
    let near_end = cells.windows(2).any(|w| {
        w.iter()
            .all(|c| c.speech_positive && c.loud_residual(policy))
    });
    if near_end {
        return Verdict::NearEndPresent;
    }
    let explained = cells
        .iter()
        .filter(|c| c.speech_positive && c.explained(policy))
        .count();
    if explained as f64 >= policy.echo_cell_share * speech_positive as f64 - 1e-9 {
        Verdict::EchoOnly
    } else {
        Verdict::Unknown(Undecided::Inconclusive)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(sp: bool, corr: f64, ratio: f64, u: f64) -> CellMeasure {
        CellMeasure {
            valid: true,
            speech_positive: sp,
            correlation: corr,
            unexplained_ratio: ratio,
            unexplained_db: u,
            background_db: Some(-60.0),
        }
    }

    #[test]
    fn measures_follow_the_formulas() {
        // The microphone is the reference at half amplitude: fully explained.
        let r: Vec<f32> = (0..512)
            .map(|i| ((i * 37 % 101) as f32 - 50.0) / 200.0)
            .collect();
        let m: Vec<f32> = r.iter().map(|x| x * 0.5).collect();
        let c = CellMeasure::measure(&m, &r, true, Some(-50.0));
        assert!((c.correlation - 1.0).abs() < 1e-9);
        assert!(c.unexplained_ratio < 1e-9);
        assert!(c.unexplained_db < -100.0);
        // A flat reference explains nothing: the residual is the microphone.
        let c = CellMeasure::measure(&m, &[0.0; 512], true, None);
        assert_eq!(c.correlation, 0.0);
        assert!((c.unexplained_ratio - 1.0).abs() < 1e-12);
        let e = 10.0 * (m.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / 512.0).log10();
        assert!((c.unexplained_db - e).abs() < 1e-6);
    }

    #[test]
    fn the_boundaries_are_categorical() {
        let p = EchoPolicy::default();
        let ok = cell(true, 0.92, 0.20, -70.0);
        assert_eq!(judge(&[ok; 6], &p), Verdict::EchoOnly);
        let low = cell(true, 0.919_999, 0.20, -70.0);
        let high = cell(true, 0.92, 0.200_001, -70.0);
        // 5 of 6 explained is 83% < 90%.
        assert_eq!(
            judge(&[ok, ok, ok, ok, ok, low], &p),
            Verdict::Unknown(Undecided::Inconclusive)
        );
        assert_eq!(
            judge(&[high, ok, ok, ok, ok, ok], &p),
            Verdict::Unknown(Undecided::Inconclusive)
        );
        // Ten speech-positive cells: 9 explained is exactly 90%.
        let mut ten = [ok; 10];
        ten[3] = low;
        assert_eq!(judge(&ten, &p), Verdict::EchoOnly);
        // Unexplained energy B + 6 dB in two consecutive cells is near-end; one cell is not.
        let loud = cell(true, 0.5, 0.7, -54.0);
        let quieter = cell(true, 0.5, 0.7, -54.000_1);
        assert_eq!(
            judge(&[ok, loud, loud, ok, ok, ok], &p),
            Verdict::NearEndPresent
        );
        assert_eq!(
            judge(&[ok, loud, quieter, ok, ok, ok], &p),
            Verdict::Unknown(Undecided::Inconclusive)
        );
        // Separated by a non-speech cell, two loud cells are not consecutive.
        let quiet = cell(false, 0.0, 1.0, -54.0);
        assert_eq!(
            judge(&[loud, quiet, loud, ok, ok, ok], &p),
            Verdict::Unknown(Undecided::Inconclusive)
        );
    }

    #[test]
    fn missing_inputs_are_unknown() {
        let p = EchoPolicy::default();
        let ok = cell(true, 0.99, 0.01, -70.0);
        // 4 of 6 valid is 67% < 80%; 5 of 6 is 83%.
        let i = CellMeasure::INVALID;
        assert_eq!(
            judge(&[ok, ok, ok, ok, i, i], &p),
            Verdict::Unknown(Undecided::TooFewValid)
        );
        assert_eq!(judge(&[ok, ok, ok, ok, ok, i], &p), Verdict::EchoOnly);
        let no_b = CellMeasure {
            background_db: None,
            ..ok
        };
        assert_eq!(
            judge(&[ok, ok, no_b, ok, ok, ok], &p),
            Verdict::Unknown(Undecided::BackgroundUnavailable)
        );
        let quiet = cell(false, 0.99, 0.01, -70.0);
        assert_eq!(
            judge(&[quiet; 6], &p),
            Verdict::Unknown(Undecided::NoSpeech)
        );
    }
}
