//! Evidence assembly (§6.1, post-AFE architecture §8.1 "Per-packet flow").
//!
//! The speech worker and the transport deliver evidence for one utterance in whatever order
//! their jobs finish: the device's loudness cells, the controller's VAD probability per cell, the
//! per-cell echo label, and streaming ASR results. The [`EvidenceAssembler`] puts them back into
//! sample order and releases each 32 ms cell exactly once, in increasing order, only when every
//! input the plan requires for it is present, explicitly unknown, or covered by a gap. An ASR
//! result is released as a *block* boundary once every cell ending at or before the sample it
//! covers through has been released. Worker completion order therefore never changes what the
//! reducer sees, and evidence from another capture epoch is refused rather than mixed in.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::time::{CellIndex, Epoch, Sample, SampleSpan};

/// The device's loudness record of a silent or missing cell, in hundredths of a dB.
pub const CELL_E_MIN: i16 = -12_000;
/// The loudest possible cell, in hundredths of a dB.
pub const CELL_E_MAX: i16 = 0;
const FLAG_GAP: u8 = 1;
const FLAG_MUTED: u8 = 2;
/// The final-mix sources a cell's mask may name: content, alert, dialog, earcon.
const SOURCES_KNOWN: u8 = 0x0F;
/// How far past the release point evidence may arrive before it is refused: 64 s of cells,
/// more than the longest utterance plus its 10 s of loudness history.
const MAX_CELLS_AHEAD: u64 = 2_000;

/// Why evidence was refused. Refused evidence changes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceError {
    /// The evidence belongs to another capture epoch.
    ForeignEpoch {
        /// The plan's epoch.
        expected: u64,
        /// The evidence's epoch.
        got: u64,
    },
    /// The evidence is for a cell this plan does not ask for that input on.
    OutsidePlan {
        /// The first refused cell.
        cell: CellIndex,
    },
    /// The evidence is further ahead of the release point than any utterance can need.
    TooFarAhead {
        /// The first refused cell.
        cell: CellIndex,
    },
    /// A second, different value arrived for an input a pending cell already has.
    Conflict {
        /// The cell.
        cell: CellIndex,
    },
    /// An ASR result does not cover more audio than the previous one, or covers none of the
    /// utterance.
    AsrOutOfOrder {
        /// The result's covered-through sample.
        through: Sample,
    },
    /// A device cell record breaks the cell-record rules (§3.1.6).
    InvalidCell {
        /// What is wrong with it.
        detail: &'static str,
    },
    /// A VAD probability is not a finite number in [0, 1].
    InvalidProbability,
    /// The plan's starts are out of order.
    InvalidPlan,
}

/// One device loudness cell (EMA1 kind 4): its loudness `E` in hundredths of a dB, its gap and
/// muted flags, and the final mix's active-source mask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceCell {
    e_centi: i16,
    gap: bool,
    muted: bool,
    source_mask: u8,
}

impl DeviceCell {
    /// Parses a wire record, applying the same rules the firmware's encoder enforces: `E` within
    /// −12000..=0, only the gap and muted flags, only known source bits, and a gap cell at
    /// exactly −12000.
    pub fn from_wire(e_centi: i16, flags: u8, source_mask: u8) -> Result<Self, EvidenceError> {
        if !(CELL_E_MIN..=CELL_E_MAX).contains(&e_centi) {
            return Err(EvidenceError::InvalidCell {
                detail: "E out of range",
            });
        }
        if flags & !(FLAG_GAP | FLAG_MUTED) != 0 {
            return Err(EvidenceError::InvalidCell {
                detail: "reserved flag bits",
            });
        }
        if source_mask & !SOURCES_KNOWN != 0 {
            return Err(EvidenceError::InvalidCell {
                detail: "unknown source bits",
            });
        }
        let gap = flags & FLAG_GAP != 0;
        if gap && e_centi != CELL_E_MIN {
            return Err(EvidenceError::InvalidCell {
                detail: "gap cell E must be -12000",
            });
        }
        Ok(Self {
            e_centi,
            gap,
            muted: flags & FLAG_MUTED != 0,
            source_mask,
        })
    }

    /// The loudness `E` in hundredths of a dB (−12000 for a gap cell).
    pub const fn e_centi(self) -> i16 {
        self.e_centi
    }

    /// Whether the cell overlaps a missing range of capture.
    pub const fn is_gap(self) -> bool {
        self.gap
    }

    /// Whether the device was muted during the cell.
    pub const fn is_muted(self) -> bool {
        self.muted
    }

    /// The final mix's active sources: content 1, alert 2, dialog 4, earcon 8.
    pub const fn source_mask(self) -> u8 {
        self.source_mask
    }

    /// A cell of real audio with this loudness, for building evidence from PCM.
    pub fn audio(e_centi: i16, source_mask: u8) -> Self {
        Self {
            e_centi: e_centi.clamp(CELL_E_MIN, CELL_E_MAX),
            gap: false,
            muted: false,
            source_mask: source_mask & SOURCES_KNOWN,
        }
    }

    /// The device's loudness formula, `10·log10(mean(x²) + 1e-12)` over `x = pcm / 32768`,
    /// rounded to 0.01 dB and clamped to the wire range (post-AFE §16.6 "Activity and level").
    pub fn loudness_centi(pcm: &[i16]) -> i16 {
        if pcm.is_empty() {
            return CELL_E_MIN;
        }
        let sum: f64 = pcm
            .iter()
            .map(|&s| {
                let x = f64::from(s) / 32_768.0;
                x * x
            })
            .sum();
        let db = 10.0 * (sum / pcm.len() as f64 + 1e-12).log10();
        let centi = (db * 100.0).round();
        if centi <= f64::from(CELL_E_MIN) {
            CELL_E_MIN
        } else if centi >= f64::from(CELL_E_MAX) {
            CELL_E_MAX
        } else {
            centi as i16
        }
    }
}

/// The device's loudness input for one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceInput {
    /// The device's record.
    Cell(DeviceCell),
    /// The device sent no record for this cell and never will (for example, its cell backfill
    /// was clipped). The cell's loudness is unknown, which is not the same as quiet.
    Unknown,
}

/// A speech probability in [0, 1].
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Probability(f32);

impl Probability {
    /// The probability, or `None` when `p` is not a finite number in [0, 1].
    pub fn new(p: f32) -> Option<Self> {
        (p.is_finite() && (0.0..=1.0).contains(&p)).then_some(Self(p))
    }

    /// The value.
    pub fn get(self) -> f32 {
        self.0
    }
}

/// The controller's VAD input for one cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VadInput {
    /// Silero's speech probability for the cell.
    Probability(Probability),
    /// No probability exists for this cell: it is unknown, never "no speech".
    Unknown,
}

impl VadInput {
    /// A probability input, refusing values outside [0, 1].
    pub fn probability(p: f32) -> Result<Self, EvidenceError> {
        Probability::new(p)
            .map(Self::Probability)
            .ok_or(EvidenceError::InvalidProbability)
    }

    fn known(self) -> Option<f32> {
        match self {
            Self::Probability(p) => Some(p.get()),
            Self::Unknown => None,
        }
    }
}

/// The per-cell echo comparison's result (post-AFE §16.6 "Per-cell echo comparison").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EchoLabel {
    /// The final-mix reference was silent, so nothing of the device's own output can be in the
    /// mic and nothing was computed.
    NoReference,
    /// The device's own playback explains the cell.
    EchoOnly,
    /// Sound the reference does not explain is present.
    NearEndPresent,
    /// The comparison could not decide: no lag, too little coverage, or no reference received.
    Unknown,
}

/// How much of the final mix the uploaded reference covered for the cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceCoverage {
    /// The reference was digital silence.
    Silent,
    /// The reference described everything the device played.
    Full,
    /// Some output was not in the reference (a player outside the mixer).
    Partial,
    /// No reference arrived for this cell.
    Missing,
}

/// The echo input for one cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EchoCell {
    /// The comparison's result.
    pub label: EchoLabel,
    /// The reference coverage it was computed with.
    pub coverage: ReferenceCoverage,
}

impl EchoCell {
    /// A cell with no reference evidence at all: unknown, never treated as "no echo".
    pub const MISSING: EchoCell = EchoCell {
        label: EchoLabel::Unknown,
        coverage: ReferenceCoverage::Missing,
    };
    /// A cell while the device's final mix was silent.
    pub const SILENT_REFERENCE: EchoCell = EchoCell {
        label: EchoLabel::NoReference,
        coverage: ReferenceCoverage::Silent,
    };
}

/// One recognizer token and when it was emitted, as a sample on the utterance's timeline.
/// Emission times are not word alignments: the recognizer emits a word as or after it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The token as the recognizer spells it; a leading space starts a new word.
    pub text: String,
    /// When it was emitted.
    pub emitted: Sample,
}

/// Which decode produced an ASR result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrKind {
    /// The utterance's live streaming decode.
    Live,
    /// The fresh decode made 320 ms into a pause, over the utterance so far plus flush padding
    /// (§16.6 "Finalize at the pause").
    Finalized,
}

/// One streaming ASR result for the block that ends at `through`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsrObservation {
    /// The result covers the utterance's audio before this sample.
    pub through: Sample,
    /// The tokens so far, in order.
    pub tokens: Vec<Token>,
    /// Decoder frames (40 ms each) since the last emitted token, as the live decoder counts them.
    pub trailing_blanks: u32,
    /// Which decode produced it.
    pub kind: AsrKind,
}

impl AsrObservation {
    /// Builds a result from the recognizer's own output: token strings with emission times in
    /// seconds from `stream_start`, as Kroko reports them.
    pub fn from_recognizer(
        stream_start: Sample,
        through: Sample,
        tokens: &[String],
        seconds: &[f32],
        trailing_blanks: u32,
        kind: AsrKind,
    ) -> Self {
        let tokens = tokens
            .iter()
            .zip(seconds)
            .map(|(t, s)| Token {
                text: t.clone(),
                emitted: Sample(stream_start.0 + (f64::from(*s) * 16_000.0).round() as u64),
            })
            .collect();
        Self {
            through,
            tokens,
            trailing_blanks,
            kind,
        }
    }
}

/// Which inputs the plan requires, and from where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvidencePlan {
    /// The capture epoch every input must belong to.
    pub epoch: Epoch,
    /// The first cell whose device loudness is needed: the background estimate looks 10 s back.
    pub cells_from: CellIndex,
    /// The first cell whose VAD probability is required: the subscription's first mic cell.
    pub vad_from: CellIndex,
    /// The first cell of the utterance: from here the echo label is required too, and cells are
    /// classified.
    pub analysis_from: CellIndex,
}

/// What one released cell holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellAudio {
    /// The cell overlaps a missing range of audio, or the device was muted: its audio is
    /// unknown.
    Gap {
        /// The device reported the cell muted.
        muted: bool,
    },
    /// The cell holds captured audio.
    Present {
        /// The device's loudness in hundredths of a dB, or `None` when unknown.
        energy_centi: Option<i16>,
        /// The final mix's active sources, or `None` when unknown.
        source_mask: Option<u8>,
        /// The VAD input: [`VadInput::Unknown`] for cells before the plan's VAD start.
        vad: VadInput,
        /// The echo input: [`EchoCell::MISSING`] for cells before the utterance.
        echo: EchoCell,
    },
}

impl CellAudio {
    /// The cell's loudness in dB, when known.
    pub fn energy_db(&self) -> Option<f64> {
        match self {
            Self::Present {
                energy_centi: Some(e),
                ..
            } => Some(f64::from(*e) / 100.0),
            _ => None,
        }
    }

    /// The cell's VAD probability, when known.
    pub fn vad(&self) -> Option<f32> {
        match self {
            Self::Present { vad, .. } => vad.known(),
            Self::Gap { .. } => None,
        }
    }

    /// Whether the cell is *speech-positive*: valid audio with VAD ≥ 0.50, the one threshold the
    /// echo tests and route B use (post-AFE §16.6).
    pub fn speech_positive(&self, threshold: f32) -> bool {
        self.vad().is_some_and(|p| p >= threshold)
    }
}

/// A cell released in sample order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellEvidence {
    /// Which cell.
    pub cell: CellIndex,
    /// What it holds.
    pub audio: CellAudio,
}

/// One item of evidence, released in sample order.
#[derive(Clone, Debug, PartialEq)]
pub enum Released {
    /// A cell, with every input it needs.
    Cell(CellEvidence),
    /// A block boundary: every cell ending at or before `through` has been released before it,
    /// and this ASR result covers through it.
    Block(AsrObservation),
}

#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    device: Option<DeviceInput>,
    vad: Option<VadInput>,
    echo: Option<EchoCell>,
    gap: bool,
}

/// Puts one utterance's evidence back into sample order.
#[derive(Debug)]
pub struct EvidenceAssembler {
    plan: EvidencePlan,
    /// The next cell to release.
    next: CellIndex,
    /// Pending inputs; `slots[i]` belongs to cell `next + i`.
    slots: VecDeque<Slot>,
    /// ASR results not yet released, in increasing `through`.
    blocks: VecDeque<AsrObservation>,
    /// The covered-through sample of the newest accepted ASR result.
    last_through: Option<Sample>,
    /// A gap cell of the utterance has been released: its ASR stream has ended, so later cells
    /// no longer wait for ASR coverage.
    asr_closed: bool,
}

impl EvidenceAssembler {
    /// An assembler for one utterance's plan.
    pub fn new(plan: EvidencePlan) -> Result<Self, EvidenceError> {
        if plan.cells_from > plan.vad_from || plan.vad_from > plan.analysis_from {
            return Err(EvidenceError::InvalidPlan);
        }
        Ok(Self {
            plan,
            next: plan.cells_from,
            slots: VecDeque::new(),
            blocks: VecDeque::new(),
            last_through: None,
            asr_closed: false,
        })
    }

    /// The plan.
    pub fn plan(&self) -> &EvidencePlan {
        &self.plan
    }

    /// The next cell that will be released.
    pub fn next_cell(&self) -> CellIndex {
        self.next
    }

    /// The covered-through sample of the newest ASR result accepted so far.
    pub fn asr_through(&self) -> Option<Sample> {
        self.last_through
    }

    fn check_epoch(&self, epoch: Epoch) -> Result<(), EvidenceError> {
        if epoch == self.plan.epoch {
            Ok(())
        } else {
            Err(EvidenceError::ForeignEpoch {
                expected: self.plan.epoch.get(),
                got: epoch.get(),
            })
        }
    }

    /// Checks that cells `[first, first + n)` may take an input that the plan requires from
    /// `from` on, and are not unreasonably far ahead of the release point.
    fn check_range(
        &self,
        from: CellIndex,
        first: CellIndex,
        n: usize,
    ) -> Result<(), EvidenceError> {
        if first < from {
            return Err(EvidenceError::OutsidePlan { cell: first });
        }
        let last = CellIndex(first.0 + n.saturating_sub(1) as u64);
        if n > 0 && last.0 >= self.next.0 + MAX_CELLS_AHEAD {
            return Err(EvidenceError::TooFarAhead { cell: last });
        }
        Ok(())
    }

    /// The slot of a cell not yet released, growing the queue as needed.
    fn slot(&mut self, cell: CellIndex) -> &mut Slot {
        let i = (cell.0 - self.next.0) as usize;
        if self.slots.len() <= i {
            self.slots.resize(i + 1, Slot::default());
        }
        &mut self.slots[i]
    }

    /// Applies `values` to consecutive cells from `first`. Inputs for cells already released are
    /// dropped: a released cell never changes. A value equal to one already held is a harmless
    /// duplicate; a different one is a conflict, and nothing of the batch is applied.
    fn apply<T: Copy + PartialEq>(
        &mut self,
        first: CellIndex,
        values: &[T],
        field: fn(&mut Slot) -> &mut Option<T>,
    ) -> Result<(), EvidenceError> {
        let skip = self.next.0.saturating_sub(first.0) as usize;
        for (i, v) in values.iter().enumerate().skip(skip) {
            let cell = CellIndex(first.0 + i as u64);
            let i = (cell.0 - self.next.0) as usize;
            if let Some(mut slot) = self.slots.get(i).copied()
                && let Some(held) = *field(&mut slot)
                && held != *v
            {
                return Err(EvidenceError::Conflict { cell });
            }
        }
        for (i, v) in values.iter().enumerate().skip(skip) {
            let cell = CellIndex(first.0 + i as u64);
            *field(self.slot(cell)) = Some(*v);
        }
        Ok(())
    }

    /// Adds the device's loudness cells starting at `first`.
    pub fn push_device_cells(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
    ) -> Result<(), EvidenceError> {
        self.check_epoch(epoch)?;
        self.check_range(self.plan.cells_from, first, cells.len())?;
        self.apply(first, cells, |s| &mut s.device)
    }

    /// Adds VAD results for consecutive cells starting at `first`.
    pub fn push_vad(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
    ) -> Result<(), EvidenceError> {
        self.check_epoch(epoch)?;
        self.check_range(self.plan.vad_from, first, vad.len())?;
        self.apply(first, vad, |s| &mut s.vad)
    }

    /// Adds echo results for consecutive cells starting at `first`.
    pub fn push_echo(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        echo: &[EchoCell],
    ) -> Result<(), EvidenceError> {
        self.check_epoch(epoch)?;
        self.check_range(self.plan.analysis_from, first, echo.len())?;
        self.apply(first, echo, |s| &mut s.echo)
    }

    /// Marks a missing range of audio: every cell it overlaps becomes a gap and needs no other
    /// input. Cells already released are not changed.
    pub fn mark_gap(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EvidenceError> {
        self.check_epoch(epoch)?;
        if span.is_empty() {
            return Ok(());
        }
        let first = span.start.cell().max(self.next).max(self.plan.cells_from);
        let last = Sample(span.end.0 - 1).cell();
        if last < first {
            return Ok(());
        }
        self.check_range(first, first, (last.0 - first.0 + 1) as usize)?;
        for c in first.0..=last.0 {
            self.slot(CellIndex(c)).gap = true;
        }
        Ok(())
    }

    /// Adds an ASR result. Results must cover strictly more audio each time, and some of the
    /// utterance.
    pub fn push_asr(&mut self, epoch: Epoch, obs: AsrObservation) -> Result<(), EvidenceError> {
        self.check_epoch(epoch)?;
        let after_previous = self.last_through.is_none_or(|t| obs.through > t);
        if obs.through <= self.plan.analysis_from.start() || !after_previous {
            return Err(EvidenceError::AsrOutOfOrder {
                through: obs.through,
            });
        }
        self.last_through = Some(obs.through);
        self.blocks.push_back(obs);
        Ok(())
    }

    /// Whether the cell at the release point has every input the plan requires. A cell of the
    /// utterance also needs an ASR result covering through its end: only then is every block
    /// boundary before the cell known, so the cell cannot overtake one. Gap cells need nothing
    /// else, since the ASR stream ends at a gap and the gap ends the utterance (or, for a reply
    /// before any command speech, stops its evaluation).
    fn front_ready(&self) -> bool {
        let Some(slot) = self.slots.front() else {
            return false;
        };
        let c = self.next;
        let device_gap = matches!(slot.device, Some(DeviceInput::Cell(d)) if d.gap || d.muted);
        let asr_covers = c < self.plan.analysis_from
            || self.asr_closed
            || self.last_through.is_some_and(|t| t >= c.end());
        slot.gap
            || device_gap
            || (slot.device.is_some()
                && (c < self.plan.vad_from || slot.vad.is_some())
                && (c < self.plan.analysis_from || slot.echo.is_some())
                && asr_covers)
    }

    /// Releases everything that is ready, in sample order, onto `out`. A block boundary comes
    /// after every cell that ends at or before it and before any cell that ends after it.
    pub fn release(&mut self, out: &mut Vec<Released>) {
        loop {
            let next_end = self.next.end();
            if let Some(b) = self.blocks.pop_front_if(|b| next_end > b.through) {
                out.push(Released::Block(b));
                continue;
            }
            if !self.front_ready() {
                break;
            }
            let slot = self.slots.pop_front().expect("front exists");
            let cell = self.next;
            self.next = cell.next();
            let audio = cell_audio(slot, cell, &self.plan);
            if cell >= self.plan.analysis_from && matches!(audio, CellAudio::Gap { .. }) {
                self.asr_closed = true;
            }
            out.push(Released::Cell(CellEvidence { cell, audio }));
        }
    }
}

fn cell_audio(slot: Slot, cell: CellIndex, plan: &EvidencePlan) -> CellAudio {
    let device = match slot.device {
        Some(DeviceInput::Cell(d)) if d.gap || d.muted => {
            return CellAudio::Gap { muted: d.muted };
        }
        Some(DeviceInput::Cell(d)) => Some(d),
        _ => None,
    };
    if slot.gap {
        return CellAudio::Gap { muted: false };
    }
    CellAudio::Present {
        energy_centi: device.map(|d| d.e_centi),
        source_mask: device.map(|d| d.source_mask),
        vad: if cell < plan.vad_from {
            VadInput::Unknown
        } else {
            slot.vad.unwrap_or(VadInput::Unknown)
        },
        echo: if cell < plan.analysis_from {
            EchoCell::MISSING
        } else {
            slot.echo.unwrap_or(EchoCell::MISSING)
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch() -> Epoch {
        Epoch::new(9).unwrap()
    }

    fn plan() -> EvidencePlan {
        EvidencePlan {
            epoch: epoch(),
            cells_from: CellIndex(10),
            vad_from: CellIndex(12),
            analysis_from: CellIndex(12),
        }
    }

    fn dev(e: i16) -> DeviceInput {
        DeviceInput::Cell(DeviceCell::audio(e, 0))
    }

    fn vad(p: f32) -> VadInput {
        VadInput::probability(p).unwrap()
    }

    fn asr(through: u64) -> AsrObservation {
        AsrObservation {
            through: Sample(through),
            tokens: Vec::new(),
            trailing_blanks: 0,
            kind: AsrKind::Live,
        }
    }

    /// The release order as (cell index | block through) markers.
    fn order(out: &[Released]) -> Vec<String> {
        out.iter()
            .map(|r| match r {
                Released::Cell(c) => format!("c{}", c.cell.0),
                Released::Block(b) => format!("b{}", b.through.0),
            })
            .collect()
    }

    #[test]
    fn device_cells_parse_like_the_firmware() {
        assert!(DeviceCell::from_wire(-5_000, 0, 0x0F).is_ok());
        assert!(DeviceCell::from_wire(1, 0, 0).is_err());
        assert!(DeviceCell::from_wire(-12_001, 0, 0).is_err());
        assert!(DeviceCell::from_wire(-12_000, 4, 0).is_err());
        assert!(DeviceCell::from_wire(-12_000, 0, 0x10).is_err());
        assert!(DeviceCell::from_wire(-11_000, 1, 0).is_err());
        assert!(DeviceCell::from_wire(-12_000, 1, 0).is_ok());
        assert_eq!(DeviceCell::loudness_centi(&[0; 512]), -12_000);
        assert_eq!(DeviceCell::loudness_centi(&[i16::MIN; 512]), 0);
        // A full-scale square wave at half amplitude is -6.02 dB.
        assert_eq!(DeviceCell::loudness_centi(&[16_384; 512]), -602);
    }

    #[test]
    fn probabilities_outside_the_unit_range_are_refused() {
        assert!(VadInput::probability(f32::NAN).is_err());
        assert!(VadInput::probability(1.01).is_err());
        assert!(VadInput::probability(-0.01).is_err());
        assert!(VadInput::probability(1.0).is_ok());
    }

    /// Out-of-order worker completion: VAD for later cells, ASR and echo arrive before the
    /// device loudness of the first cells, and the release order is still sample order.
    #[test]
    fn worker_completion_order_does_not_change_release_order() {
        let build = |forward: bool| {
            let mut a = EvidenceAssembler::new(plan()).unwrap();
            let mut out = Vec::new();
            type Step = Box<dyn Fn(&mut EvidenceAssembler)>;
            let mut steps: Vec<Step> = vec![
                Box::new(|a| {
                    a.push_device_cells(epoch(), CellIndex(10), &[dev(-7000); 6])
                        .unwrap()
                }),
                Box::new(|a| {
                    a.push_vad(epoch(), CellIndex(12), &[vad(0.1), vad(0.9)])
                        .unwrap()
                }),
                Box::new(|a| {
                    a.push_vad(epoch(), CellIndex(14), &[vad(0.2), vad(0.3)])
                        .unwrap()
                }),
                Box::new(|a| {
                    a.push_echo(epoch(), CellIndex(12), &[EchoCell::MISSING; 4])
                        .unwrap()
                }),
                // ASR results come from one serial decode, so they stay in order among
                // themselves; only their timing against the other workers changes.
                Box::new(|a| {
                    a.push_asr(epoch(), asr(12 * 512 + 1280)).unwrap();
                    a.push_asr(epoch(), asr(12 * 512 + 2456)).unwrap();
                }),
            ];
            if !forward {
                steps.reverse();
            }
            for s in steps {
                s(&mut a);
                a.release(&mut out);
            }
            out
        };
        let forward = build(true);
        let backward = build(false);
        assert_eq!(forward, backward);
        assert_eq!(
            order(&forward),
            ["c10", "c11", "c12", "c13", "b7424", "c14", "c15", "b8600"]
        );
    }

    #[test]
    fn a_cell_waits_for_every_required_input() {
        let mut a = EvidenceAssembler::new(plan()).unwrap();
        let mut out = Vec::new();
        a.push_device_cells(epoch(), CellIndex(10), &[dev(-7000); 3])
            .unwrap();
        a.release(&mut out);
        // Cells 10 and 11 need only loudness; cell 12 also needs VAD, echo and ASR coverage.
        assert_eq!(order(&out), ["c10", "c11"]);
        a.push_vad(epoch(), CellIndex(12), &[vad(0.5)]).unwrap();
        a.release(&mut out);
        assert_eq!(out.len(), 2);
        a.push_echo(epoch(), CellIndex(12), &[EchoCell::MISSING])
            .unwrap();
        a.release(&mut out);
        assert_eq!(out.len(), 2, "no ASR result covers cell 12 yet");
        a.push_asr(epoch(), asr(13 * 512 - 1)).unwrap();
        a.release(&mut out);
        assert_eq!(
            order(&out),
            ["c10", "c11", "b6655"],
            "still one sample short"
        );
        a.push_asr(epoch(), asr(13 * 512)).unwrap();
        a.release(&mut out);
        assert_eq!(order(&out), ["c10", "c11", "b6655", "c12", "b6656"]);
    }

    #[test]
    fn explicitly_unknown_inputs_release_as_unknown_not_silence() {
        let mut a = EvidenceAssembler::new(plan()).unwrap();
        let mut out = Vec::new();
        a.push_device_cells(epoch(), CellIndex(10), &[DeviceInput::Unknown; 3])
            .unwrap();
        a.push_vad(epoch(), CellIndex(12), &[VadInput::Unknown])
            .unwrap();
        a.push_echo(epoch(), CellIndex(12), &[EchoCell::MISSING])
            .unwrap();
        a.push_asr(epoch(), asr(13 * 512)).unwrap();
        a.release(&mut out);
        let Released::Cell(c) = &out[2] else {
            panic!("cell expected")
        };
        assert_eq!(c.audio.energy_db(), None);
        assert_eq!(c.audio.vad(), None);
        assert!(!c.audio.speech_positive(0.5));
    }

    #[test]
    fn gaps_need_no_other_input_and_device_gap_flags_become_gaps() {
        let mut a = EvidenceAssembler::new(plan()).unwrap();
        let mut out = Vec::new();
        a.push_device_cells(epoch(), CellIndex(10), &[dev(-7000), dev(-7000)])
            .unwrap();
        a.mark_gap(
            epoch(),
            SampleSpan::new(Sample(12 * 512 + 10), Sample(13 * 512 + 1)),
        )
        .unwrap();
        let muted = DeviceCell::from_wire(-6000, 2, 0).unwrap();
        a.push_device_cells(epoch(), CellIndex(14), &[DeviceInput::Cell(muted)])
            .unwrap();
        a.release(&mut out);
        // A muted or gap record from the device needs no VAD or echo either: the worker has no
        // audio to analyse there, and late input for the released cell is dropped.
        assert_eq!(order(&out), ["c10", "c11", "c12", "c13", "c14"]);
        a.push_vad(epoch(), CellIndex(14), &[vad(0.9)]).unwrap();
        a.push_echo(epoch(), CellIndex(14), &[EchoCell::MISSING])
            .unwrap();
        a.release(&mut out);
        let audio: Vec<CellAudio> = out
            .iter()
            .filter_map(|r| match r {
                Released::Cell(c) => Some(c.audio),
                Released::Block(_) => None,
            })
            .collect();
        assert_eq!(audio[2], CellAudio::Gap { muted: false });
        assert_eq!(audio[3], CellAudio::Gap { muted: false });
        assert_eq!(audio[4], CellAudio::Gap { muted: true });
    }

    #[test]
    fn foreign_epochs_and_misplaced_inputs_are_refused() {
        let mut a = EvidenceAssembler::new(plan()).unwrap();
        let other = Epoch::new(10).unwrap();
        assert_eq!(
            a.push_vad(other, CellIndex(12), &[vad(0.1)]),
            Err(EvidenceError::ForeignEpoch {
                expected: 9,
                got: 10
            })
        );
        assert_eq!(
            a.push_vad(epoch(), CellIndex(11), &[vad(0.1)]),
            Err(EvidenceError::OutsidePlan {
                cell: CellIndex(11)
            })
        );
        assert_eq!(
            a.push_echo(epoch(), CellIndex(11), &[EchoCell::MISSING]),
            Err(EvidenceError::OutsidePlan {
                cell: CellIndex(11)
            })
        );
        assert!(matches!(
            a.push_device_cells(epoch(), CellIndex(10 + MAX_CELLS_AHEAD), &[dev(-1)]),
            Err(EvidenceError::TooFarAhead { .. })
        ));
        a.push_asr(epoch(), asr(7000)).unwrap();
        assert!(a.push_asr(epoch(), asr(7000)).is_err());
        assert!(a.push_asr(epoch(), asr(6900)).is_err());
        let mut b = EvidenceAssembler::new(plan()).unwrap();
        assert!(b.push_asr(epoch(), asr(12 * 512)).is_err());
    }

    #[test]
    fn duplicates_are_dropped_and_conflicts_refused() {
        let mut a = EvidenceAssembler::new(plan()).unwrap();
        let mut out = Vec::new();
        a.push_device_cells(epoch(), CellIndex(10), &[dev(-7000)])
            .unwrap();
        a.push_device_cells(epoch(), CellIndex(10), &[dev(-7000)])
            .unwrap();
        a.push_device_cells(epoch(), CellIndex(11), &[dev(-7000)])
            .unwrap();
        assert_eq!(
            a.push_device_cells(epoch(), CellIndex(11), &[dev(-6000)]),
            Err(EvidenceError::Conflict {
                cell: CellIndex(11)
            })
        );
        a.release(&mut out);
        assert_eq!(order(&out), ["c10", "c11"]);
        // Released cells never change; late input for them is dropped.
        a.push_device_cells(epoch(), CellIndex(10), &[dev(-1000), dev(-1000)])
            .unwrap();
        a.release(&mut out);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_block_ending_mid_cell_comes_before_that_cell() {
        let mut a = EvidenceAssembler::new(EvidencePlan {
            epoch: epoch(),
            cells_from: CellIndex(0),
            vad_from: CellIndex(0),
            analysis_from: CellIndex(0),
        })
        .unwrap();
        let mut out = Vec::new();
        a.push_asr(epoch(), asr(1280)).unwrap();
        a.release(&mut out);
        assert!(out.is_empty(), "cells 0 and 1 are not in yet");
        a.push_device_cells(epoch(), CellIndex(0), &[dev(-7000); 3])
            .unwrap();
        a.push_echo(epoch(), CellIndex(0), &[EchoCell::MISSING; 3])
            .unwrap();
        a.push_vad(epoch(), CellIndex(0), &[vad(0.1); 3]).unwrap();
        a.release(&mut out);
        // Cell 2 ends at 1536, past the only ASR result: it waits for the next block.
        assert_eq!(order(&out), ["c0", "c1", "b1280"]);
        a.push_asr(epoch(), asr(2560)).unwrap();
        a.release(&mut out);
        assert_eq!(order(&out), ["c0", "c1", "b1280", "c2"]);
    }
}
