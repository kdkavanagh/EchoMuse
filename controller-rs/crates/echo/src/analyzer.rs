//! The microphone analysis worker's echo state for one subscription (Rust controller
//! evaluation §5.1, §6.1; post-AFE architecture §16.6).
//!
//! An [`EchoAnalyzer`] takes the microphone, the reference, the device's loudness cells and the
//! worker's own VAD as they arrive, in any interleaving, and produces two things:
//!
//! - **a per-cell echo label** ([`CellLabel`], carrying the speech policy's [`EchoCell`]) for every
//!   32 ms cell from the start the runtime names, in increasing cell order, each computed over
//!   the trailing six cells at the current lag;
//! - **a candidate comparison** ([`CandidateComparison`], carrying [`CandidateEcho`]) over a wake
//!   candidate's support, with its own lag search.
//!
//! A label is produced only once every input it reads has arrived or is known never to arrive,
//! so results depend on the inputs alone, never on the order or timing of their arrival.
//! Missing reference evidence is always [`EchoCell::MISSING`]: unknown, never silence.
//!
//! # Feeding it
//!
//! The device runtime opens one analyzer per subscription capture epoch with the same values it
//! gives the speech policy's evidence plan ([`EchoPlan`]), and forwards each delivery:
//!
//! | Delivery | Call |
//! |---|---|
//! | mic PCM | [`EchoAnalyzer::push_mic`] |
//! | mic missing range | [`EchoAnalyzer::push_mic_missing`] |
//! | reference PCM / digital silence | [`EchoAnalyzer::push_reference`] / [`EchoAnalyzer::push_reference_silence`] |
//! | reference missing range | [`EchoAnalyzer::push_reference_missing`] |
//! | no reference for this subscription, or its reference stream ended | [`EchoAnalyzer::reference_ended`] |
//! | device loudness cells | [`EchoAnalyzer::push_device_cells`] |
//! | any other gap the evidence plan marks | [`EchoAnalyzer::mark_gap`] |
//! | the worker's VAD results | [`EchoAnalyzer::push_vad`] |
//!
//! When an utterance or reply window opens, [`EchoAnalyzer::start_labels`] starts the per-cell
//! labels at its first cell; [`EchoAnalyzer::poll`] then appends every label that has become
//! ready. A capture-epoch change, discontinuity handling that restarts the subscription, or a
//! mute calls [`EchoAnalyzer::open`] or [`EchoAnalyzer::reset`], which forget everything.

use std::collections::VecDeque;

use echomuse_turn::level::BackgroundLevel;
use echomuse_turn::{
    CandidateEcho, CellAudio, CellIndex, DeviceInput, EchoCell, EchoLabel, Epoch,
    ReferenceCoverage, Sample, SampleSpan, VadInput,
};
use serde::{Deserialize, Serialize};

use crate::clock::{Alignment, StreamClock};
use crate::compare::{CellMeasure, Undecided, Verdict, judge};
use crate::lag::{Lag, LagEstimator, LagOutcome, LagUnknown, mean_square};
use crate::policy::{CELL_SAMPLES, EchoPolicy};
use crate::store::{Read, SampleStore};

/// Microphone kept: 8 s, more than a candidate's longest support (about 3.7 s) plus the time
/// its comparison may wait for the reference.
const MIC_CAPACITY: usize = 128_000;
/// Reference kept: 10 s, the microphone's span plus the 500 ms search and backfill slack.
const REFERENCE_CAPACITY: usize = 160_000;
/// Per-cell inputs kept behind the newest work: 320 cells (10.24 s).
const HISTORY_CELLS: u64 = 320;
/// How far ahead of the oldest undecided cell an input may land: 64 s, as in the speech
/// policy's evidence assembler.
const MAX_CELLS_AHEAD: u64 = 2_000;
/// Lag estimates kept for the trace.
const MAX_ESTIMATES: usize = 16;

/// Reference sample tags: whether the device claimed full coverage when it sent them.
const TAG_FULL: u8 = 1;
const TAG_PARTIAL: u8 = 2;

/// Why an input was refused. A refused input changes nothing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EchoError {
    /// No plan is open.
    #[error("no subscription epoch is open")]
    NotOpen,
    /// The input belongs to another capture epoch.
    #[error("input for capture epoch {got}, but epoch {expected} is open")]
    ForeignEpoch {
        /// The open epoch.
        expected: u64,
        /// The input's epoch.
        got: u64,
    },
    /// The plan's cells are out of order.
    #[error("the plan's first loudness cell is after its first VAD cell")]
    InvalidPlan,
    /// The input lands unreasonably far ahead of the cells still being worked on.
    #[error("input for cell {} is too far ahead", cell.0)]
    TooFarAhead {
        /// The cell.
        cell: CellIndex,
    },
}

/// What the analyzer is told about one subscription's capture epoch: the same values as the
/// speech policy's `EvidencePlan`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EchoPlan {
    /// The capture epoch.
    pub epoch: Epoch,
    /// The first device loudness cell the subscription carries (10 s before the microphone, for
    /// `B`).
    pub cells_from: CellIndex,
    /// The subscription's first microphone cell, from which VAD is computed.
    pub vad_from: CellIndex,
}

/// What the device said about the reference's coverage of its output, from the most recent
/// `render.progress` `reference_coverage`. The firmware's mixer reports `full` for every
/// playback, because everything it plays passes through the tap the reference is cut from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCoverage {
    /// Everything the device played is in the reference.
    Full,
    /// Some output bypassed the reference.
    Partial,
}

/// One cell's echo label, with the lag it was computed at. (The speech policy's `EchoCell` has
/// no serialized form; its trace records the labels it releases.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellLabel {
    /// The cell.
    pub cell: CellIndex,
    /// The speech policy's input.
    pub echo: EchoCell,
    /// The lag the window was compared at (after refinement), when one was established.
    pub lag: Option<Lag>,
    /// Why the label is unknown, when it is.
    pub undecided: Option<Undecided>,
}

/// One lag estimate, for the trace.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LagEstimate {
    /// The end of the microphone second it correlated.
    pub window_end: Sample,
    /// What it found.
    pub outcome: LagOutcome,
}

/// The candidate-level echo comparison (post-AFE §16.6 "Reference comparison").
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateComparison {
    /// The wake decision's input.
    pub echo: CandidateEcho,
    /// The reference coverage over the candidate's search range.
    pub coverage: ReferenceCoverage,
    /// The lag search over the support, when it ran.
    pub lag: Option<LagOutcome>,
    /// Why the result is unknown, when it is.
    pub undecided: Option<Undecided>,
    /// Cells compared, valid ones and speech-positive ones.
    pub cells: u32,
    /// Compared cells holding microphone audio.
    pub valid_cells: u32,
    /// Valid cells with VAD at or above the speech-positive line.
    pub speech_positive_cells: u32,
}

impl CandidateComparison {
    /// An unknown comparison that compared no cells.
    fn unknown(coverage: ReferenceCoverage, undecided: Undecided, lag: Option<LagOutcome>) -> Self {
        Self {
            echo: CandidateEcho::Unknown,
            coverage,
            lag,
            undecided: Some(undecided),
            cells: 0,
            valid_cells: 0,
            speech_positive_cells: 0,
        }
    }
}

/// The per-cell inputs held for one cell.
#[derive(Clone, Copy, Debug, Default)]
struct CellSlot {
    device: Option<DeviceInput>,
    vad: Option<VadInput>,
    gap: bool,
    /// `B` for this cell once computed: `Some(None)` when it is known to be unavailable.
    background: Option<Option<f64>>,
}

impl CellSlot {
    fn device_gap(&self) -> bool {
        matches!(self.device, Some(DeviceInput::Cell(d)) if d.is_gap() || d.is_muted())
    }

    /// Whether a released copy of this cell would be a gap, as the evidence assembler builds it.
    fn is_gap(&self) -> bool {
        self.gap || self.device_gap()
    }
}

/// The reference stream as the analyzer holds it.
#[derive(Debug)]
struct ReferenceState {
    epoch: Option<Epoch>,
    store: SampleStore<f32>,
    clock: StreamClock,
}

/// Per-cell labelling state.
#[derive(Clone, Debug)]
struct Labels {
    /// The next cell to label.
    next: CellIndex,
    /// The end of the first lag estimate's window; later ones follow every `lag_every`.
    first_estimate: u64,
    /// The end of the next estimate's window.
    next_estimate: u64,
    /// The established lag, held across estimates that fail.
    lag: Option<Lag>,
}

/// The echo comparator for one subscription's capture epoch.
pub struct EchoAnalyzer {
    policy: EchoPolicy,
    plan: Option<EchoPlan>,
    mic: SampleStore<f32>,
    mic_clock: StreamClock,
    reference: ReferenceState,
    cells: VecDeque<CellSlot>,
    cells_base: CellIndex,
    background: BackgroundLevel,
    background_next: CellIndex,
    labels: Option<Labels>,
    estimates: VecDeque<LagEstimate>,
    estimator: LagEstimator,
    mic_buf: Vec<f32>,
    valid_buf: Vec<bool>,
    ref_buf: Vec<f32>,
    measures: Vec<CellMeasure>,
    sample_valid: Vec<bool>,
    contexts: Vec<(bool, Option<f64>)>,
}

impl std::fmt::Debug for EchoAnalyzer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoAnalyzer")
            .field("plan", &self.plan)
            .field("labels", &self.labels)
            .field("background_next", &self.background_next)
            .finish_non_exhaustive()
    }
}

/// The mean-subtracted normalized correlation of `mic` against `reference` over the valid
/// cells.
fn window_correlation(mic: &[f32], valid: &[bool], reference: &[f32]) -> f64 {
    let (mut n, mut sm, mut sr, mut smm, mut srr, mut smr) = (0f64, 0.0, 0.0, 0.0, 0.0, 0.0);
    for ((m, r), _) in mic
        .as_chunks::<CELL_SAMPLES>()
        .0
        .iter()
        .zip(reference.as_chunks::<CELL_SAMPLES>().0)
        .zip(valid)
        .filter(|(_, v)| **v)
    {
        for (&m, &r) in m.iter().zip(r) {
            let (m, r) = (f64::from(m), f64::from(r));
            n += 1.0;
            sm += m;
            sr += r;
            smm += m * m;
            srr += r * r;
            smr += m * r;
        }
    }
    let var_m = smm - sm * sm / n.max(1.0);
    let var_r = srr - sr * sr / n.max(1.0);
    if var_m <= 1e-12 || var_r <= 1e-12 {
        return 0.0;
    }
    (smr - sm * sr / n) / (var_m * var_r).sqrt()
}

/// Whether every 32 ms chunk of `reference` is below the silence line.
fn all_silent(reference: &[f32], silence: f64) -> bool {
    reference
        .chunks(CELL_SAMPLES)
        .all(|c| mean_square(c) < silence)
}

impl EchoAnalyzer {
    /// An analyzer with no plan open.
    pub fn new(policy: EchoPolicy) -> Self {
        let background = BackgroundLevel::new(policy.background_exclude_vad);
        Self {
            policy,
            plan: None,
            mic: SampleStore::new(MIC_CAPACITY),
            mic_clock: StreamClock::default(),
            reference: ReferenceState {
                epoch: None,
                store: SampleStore::new(REFERENCE_CAPACITY),
                clock: StreamClock::default(),
            },
            cells: VecDeque::new(),
            cells_base: CellIndex(0),
            background,
            background_next: CellIndex(0),
            labels: None,
            estimates: VecDeque::with_capacity(MAX_ESTIMATES),
            estimator: LagEstimator::new(),
            mic_buf: Vec::new(),
            valid_buf: Vec::new(),
            ref_buf: Vec::new(),
            measures: Vec::new(),
            sample_valid: Vec::new(),
            contexts: Vec::new(),
        }
    }

    /// The open plan.
    pub fn plan(&self) -> Option<&EchoPlan> {
        self.plan.as_ref()
    }

    /// Forgets everything: the plan, all audio, `B`, the lag and the labels. Used on mute,
    /// which erases buffered audio on both sides, and on session loss. The allocations are
    /// kept, so a later [`EchoAnalyzer::open`] allocates nothing new.
    pub fn reset(&mut self) {
        self.plan = None;
        self.mic.clear();
        self.mic_clock.clear();
        self.reference.epoch = None;
        self.reference.store.clear();
        self.reference.clock.clear();
        self.cells.clear();
        self.cells_base = CellIndex(0);
        self.background = BackgroundLevel::new(self.policy.background_exclude_vad);
        self.background_next = CellIndex(0);
        self.labels = None;
        self.estimates.clear();
    }

    /// Resets and opens a subscription's capture epoch. Every later input must name this
    /// epoch.
    pub fn open(&mut self, plan: EchoPlan) -> Result<(), EchoError> {
        if plan.cells_from > plan.vad_from {
            return Err(EchoError::InvalidPlan);
        }
        self.reset();
        self.cells_base = plan.cells_from;
        self.background_next = plan.cells_from;
        self.plan = Some(plan);
        Ok(())
    }

    fn check(&self, epoch: Epoch) -> Result<EchoPlan, EchoError> {
        let plan = self.plan.ok_or(EchoError::NotOpen)?;
        if plan.epoch == epoch {
            Ok(plan)
        } else {
            Err(EchoError::ForeignEpoch {
                expected: plan.epoch.get(),
                got: epoch.get(),
            })
        }
    }

    /// The oldest cell any pending work still needs.
    fn working_cell(&self) -> CellIndex {
        let mut c = self.background_next;
        if let Some(l) = &self.labels {
            c = c.min(l.next);
        }
        c
    }

    /// The slot of `cell`, growing the queue; `None` for a cell older than the queue keeps.
    fn slot_mut(&mut self, cell: CellIndex) -> Result<Option<&mut CellSlot>, EchoError> {
        if cell < self.cells_base {
            return Ok(None);
        }
        if cell.0 >= self.working_cell().0 + MAX_CELLS_AHEAD {
            return Err(EchoError::TooFarAhead { cell });
        }
        let i = (cell.0 - self.cells_base.0) as usize;
        if self.cells.len() <= i {
            self.cells.resize(i + 1, CellSlot::default());
        }
        Ok(Some(&mut self.cells[i]))
    }

    fn slot(&self, cell: CellIndex) -> Option<&CellSlot> {
        if cell < self.cells_base {
            return None;
        }
        self.cells.get((cell.0 - self.cells_base.0) as usize)
    }

    fn mark_cells_gap(&mut self, span: SampleSpan) -> Result<(), EchoError> {
        if span.is_empty() {
            return Ok(());
        }
        let first = span.start.cell().max(self.cells_base);
        let last = Sample(span.end.0 - 1).cell();
        let mut c = first;
        while c <= last {
            if let Some(slot) = self.slot_mut(c)? {
                slot.gap = true;
            }
            c = c.next();
        }
        Ok(())
    }

    /// Adds microphone PCM from `first`, stamped with the device time of its first sample.
    pub fn push_mic(
        &mut self,
        epoch: Epoch,
        first: Sample,
        mono_ns: u64,
        samples: impl IntoIterator<Item = i16>,
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.mic_clock.add(first.0, mono_ns);
        self.mic.append(
            first.0,
            1,
            samples.into_iter().map(|s| f32::from(s) / 32_768.0),
        );
        Ok(())
    }

    /// Records a range of microphone audio that will never arrive. Its cells become gaps, as
    /// the evidence assembler marks them.
    pub fn push_mic_missing(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.mark_cells_gap(span)?;
        self.mic.mark_missing(span.start.0, span.end.0);
        Ok(())
    }

    /// Marks a range of cells as gaps for any other reason the runtime marks them in the
    /// speech policy's evidence (for example a missing range of loudness cells).
    pub fn mark_gap(&mut self, epoch: Epoch, span: SampleSpan) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.mark_cells_gap(span)
    }

    /// Switches to `epoch` when the reference epoch changed: the device restarted its render
    /// sink, so the old reference, its clock fit and the lag measured through it are dropped.
    fn reference_epoch(&mut self, epoch: Epoch) {
        if self.reference.epoch != Some(epoch) {
            self.reference.epoch = Some(epoch);
            self.reference.store.clear();
            self.reference.clock.clear();
            if let Some(l) = &mut self.labels {
                l.lag = None;
            }
        }
    }

    /// Adds reference PCM from reference sample `first` of reference epoch `reference_epoch`,
    /// stamped with the device time of its first sample, sent while the device reported
    /// `coverage`.
    pub fn push_reference(
        &mut self,
        epoch: Epoch,
        reference_epoch: Epoch,
        first: u64,
        mono_ns: u64,
        coverage: OutputCoverage,
        samples: impl IntoIterator<Item = i16>,
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.reference_epoch(reference_epoch);
        self.reference.clock.add(first, mono_ns);
        self.reference.store.append(
            first,
            coverage_tag(coverage),
            samples.into_iter().map(|s| f32::from(s) / 32_768.0),
        );
        Ok(())
    }

    /// Adds `frames` reference samples the device flagged as digital silence (all zero).
    pub fn push_reference_silence(
        &mut self,
        epoch: Epoch,
        reference_epoch: Epoch,
        first: u64,
        frames: u64,
        mono_ns: u64,
        coverage: OutputCoverage,
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.reference_epoch(reference_epoch);
        self.reference.clock.add(first, mono_ns);
        self.reference
            .store
            .append_fill(first, frames, coverage_tag(coverage), 0.0);
        Ok(())
    }

    /// Records reference samples `[first, end)` that will never arrive.
    pub fn push_reference_missing(
        &mut self,
        epoch: Epoch,
        reference_epoch: Epoch,
        first: u64,
        end: u64,
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.reference_epoch(reference_epoch);
        self.reference.store.mark_missing(first, end);
        Ok(())
    }

    /// Declares that no further reference will arrive for the current reference epoch: the
    /// subscription carries no reference, the device has no reference stream, or the stream
    /// or the subscription ended. Every reference position not yet received is then missing,
    /// so the cells waiting for it are labelled unknown instead of waiting forever. A later
    /// reference in a new epoch reopens it.
    pub fn reference_ended(&mut self, epoch: Epoch) -> Result<(), EchoError> {
        self.check(epoch)?;
        self.reference.store.close();
        Ok(())
    }

    /// Adds the device's loudness cells from `first`, for `B`.
    pub fn push_device_cells(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        cells: &[DeviceInput],
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        for (i, &d) in cells.iter().enumerate() {
            if let Some(slot) = self.slot_mut(CellIndex(first.0 + i as u64))? {
                slot.device.get_or_insert(d);
            }
        }
        Ok(())
    }

    /// Adds the worker's VAD results from `first`.
    pub fn push_vad(
        &mut self,
        epoch: Epoch,
        first: CellIndex,
        vad: &[VadInput],
    ) -> Result<(), EchoError> {
        self.check(epoch)?;
        for (i, &v) in vad.iter().enumerate() {
            if let Some(slot) = self.slot_mut(CellIndex(first.0 + i as u64))? {
                slot.vad.get_or_insert(v);
            }
        }
        Ok(())
    }

    /// Starts per-cell labels at `from` (the utterance's or reply window's first cell). The lag
    /// is estimated afresh: from the second of microphone audio before `from` when the
    /// subscription holds it, otherwise from the subscription's first second (whose cells then
    /// wait for it), and again every second after (§16.6).
    pub fn start_labels(&mut self, from: CellIndex) -> Result<(), EchoError> {
        let plan = self.plan.ok_or(EchoError::NotOpen)?;
        let window = u64::from(self.policy.lag_window);
        let mic_first = plan.vad_from.start().0;
        let start = from.start().0;
        let first_estimate = if start >= mic_first + window {
            start
        } else {
            mic_first + window
        };
        self.labels = Some(Labels {
            next: from.max(plan.vad_from),
            first_estimate,
            next_estimate: first_estimate,
            lag: None,
        });
        Ok(())
    }

    /// Stops per-cell labels.
    pub fn stop_labels(&mut self) {
        self.labels = None;
    }

    /// The established lag of the per-cell labels.
    pub fn lag(&self) -> Option<Lag> {
        self.labels.as_ref().and_then(|l| l.lag)
    }

    /// The most recent lag estimates, oldest first.
    pub fn estimates(&self) -> impl Iterator<Item = &LagEstimate> {
        self.estimates.iter()
    }

    /// Where the reference sits on the microphone timeline, once both streams have stamps.
    pub fn alignment(&self) -> Option<Alignment> {
        let plan = self.plan?;
        Some(Alignment {
            mic_epoch: plan.epoch,
            reference_epoch: self.reference.epoch?,
            mic: self.mic_clock.fit()?,
            reference: self.reference.clock.fit()?,
            lag: self.lag(),
        })
    }

    /// Computes `B` for every cell whose inputs are complete, in cell order, with the speech
    /// policy's own estimator and the evidence assembler's rules for what a cell holds.
    fn advance_background(&mut self) {
        let Some(plan) = self.plan else {
            return;
        };
        loop {
            let c = self.background_next;
            let Some(slot) = self.slot(c).copied() else {
                return;
            };
            let ready = slot.is_gap()
                || (slot.device.is_some() && (c < plan.vad_from || slot.vad.is_some()));
            if !ready {
                return;
            }
            // The cell as the evidence assembler releases it, so `B` here is the reducer's `B`.
            let vad = if c < plan.vad_from { None } else { slot.vad };
            let audio = CellAudio::from_inputs(slot.device, slot.gap, vad, None);
            let b = self.background.current_db();
            self.background.push(&audio);
            if let Ok(Some(s)) = self.slot_mut(c) {
                s.background = Some(b);
            }
            self.background_next = c.next();
        }
    }

    /// `B` for `cell` and whether it is speech-positive, once both are known.
    fn cell_context(&self, plan: &EchoPlan, cell: CellIndex) -> Option<(bool, Option<f64>)> {
        let slot = self.slot(cell)?;
        let b = slot.background?;
        if slot.is_gap() || cell < plan.vad_from {
            return Some((false, b));
        }
        let vad = slot.vad?;
        let sp =
            matches!(vad, VadInput::Probability(p) if p.get() >= self.policy.speech_positive_vad);
        Some((sp, b))
    }

    /// Fills `contexts` with the context of each of `cells` cells from `first`; `false` while
    /// one of them is not known yet.
    fn fill_contexts(
        &self,
        plan: &EchoPlan,
        first: CellIndex,
        cells: usize,
        contexts: &mut Vec<(bool, Option<f64>)>,
    ) -> bool {
        contexts.clear();
        for i in 0..cells {
            match self.cell_context(plan, CellIndex(first.0 + i as u64)) {
                Some(ctx) => contexts.push(ctx),
                None => return false,
            }
        }
        true
    }

    /// The reference position heard at microphone sample `m` with no lag, rounded.
    fn mic_to_reference(&self, m: u64) -> Option<i64> {
        let mic = self.mic_clock.fit()?;
        let reference = self.reference.clock.fit()?;
        Some(reference.position_at(mic.ns_at(m as f64)).round() as i64)
    }

    /// Whether reference evidence is still to come (so work must wait) or never will be.
    fn reference_read(&self, from: i64, to: i64) -> Read {
        self.reference.store.state(from, to)
    }

    /// What a read of the reference is worth before its position on the microphone timeline is
    /// known (no stamped reference yet): wait while it may still come, missing once it cannot.
    fn reference_unplaced(&self) -> Read {
        if self.reference.store.is_closed() {
            Read::Missing
        } else {
            Read::Pending
        }
    }

    /// Runs the lag estimate whose microphone window ends at `end`. `None` means it must wait.
    fn run_estimate(&mut self, end: u64) -> Option<LagOutcome> {
        let n = self.policy.lag_window as usize;
        let lag_max = i64::from(self.policy.lag_max);
        let start = end - n as u64;
        match self.mic.state(start as i64, end as i64) {
            Read::Pending => return None,
            Read::Missing => return Some(LagOutcome::Unknown(LagUnknown::MicMissing)),
            Read::Held { .. } => {}
        }
        let Some(r0) = self.mic_to_reference(start) else {
            return match self.reference_unplaced() {
                Read::Pending => None,
                _ => Some(LagOutcome::Unknown(LagUnknown::ReferenceMissing)),
            };
        };
        match self.reference_read(r0 - lag_max, r0 + n as i64) {
            Read::Pending => return None,
            Read::Missing => return Some(LagOutcome::Unknown(LagUnknown::ReferenceMissing)),
            Read::Held { .. } => {}
        }
        self.mic_buf.resize(n, 0.0);
        self.mic.copy_to(start as i64, &mut self.mic_buf);
        self.ref_buf.resize(n + lag_max as usize, 0.0);
        self.reference
            .store
            .copy_to(r0 - lag_max, &mut self.ref_buf);
        Some(
            self.estimator
                .estimate(&self.mic_buf, None, &self.ref_buf, &self.policy),
        )
    }

    fn record_estimate(&mut self, end: u64, outcome: LagOutcome) {
        if self.estimates.len() == MAX_ESTIMATES {
            self.estimates.pop_front();
        }
        self.estimates.push_back(LagEstimate {
            window_end: Sample(end),
            outcome,
        });
    }

    /// Appends every per-cell label that has become ready, in cell order.
    pub fn poll(&mut self, out: &mut Vec<CellLabel>) {
        self.advance_background();
        while let Some(label) = self.next_label() {
            out.push(label);
        }
        self.trim();
    }

    /// Drops per-cell inputs no pending work or candidate comparison can still need.
    fn trim(&mut self) {
        let keep_from = self.working_cell().0.saturating_sub(HISTORY_CELLS);
        while self.cells_base.0 < keep_from && !self.cells.is_empty() {
            self.cells.pop_front();
            self.cells_base = self.cells_base.next();
        }
    }

    fn next_label(&mut self) -> Option<CellLabel> {
        let plan = self.plan?;
        let labels = self.labels.clone()?;
        let k = labels.next;
        if self.mic.end() < k.end().0 {
            return None;
        }
        // The trailing window: up to six cells, none before the subscription's first.
        let w = u64::from(self.policy.window_cells.max(1));
        let k0 = CellIndex(k.0.saturating_sub(w - 1)).max(plan.vad_from);
        let cells = (k.0 - k0.0 + 1) as usize;

        // When everything the device played that could reach the window (any lag in the
        // search) is silent, the cell is `no_reference` whatever the lag: nothing is computed and
        // nothing waits for an estimate. Any lag's window is part of that range, so the answer
        // is the one the lag would have given.
        if self.reachable_reference_silent(k0, cells) {
            if let Some(state) = &mut self.labels {
                state.next = k.next();
            }
            return Some(CellLabel {
                cell: k,
                echo: EchoCell::SILENT_REFERENCE,
                lag: None,
                undecided: None,
            });
        }

        // Every lag estimate that applies to this cell must have run: the first one (which the
        // cells before it wait for), and every later one whose window ends by this cell's end.
        let target = k.end().0.max(labels.first_estimate);
        let mut next_estimate = labels.next_estimate;
        let mut lag = labels.lag;
        while next_estimate <= target {
            let outcome = self.run_estimate(next_estimate)?;
            self.record_estimate(next_estimate, outcome);
            if let Some(l) = outcome.lag() {
                lag = Some(l);
            }
            next_estimate += u64::from(self.policy.lag_every);
            if let Some(state) = &mut self.labels {
                state.next_estimate = next_estimate;
                state.lag = lag;
            }
        }

        let mut contexts = std::mem::take(&mut self.contexts);
        let labelled = if self.fill_contexts(&plan, k0, cells, &mut contexts) {
            self.label_window(k0, cells, lag, &contexts)
        } else {
            None
        };
        self.contexts = contexts;
        let (echo, used_lag, undecided) = labelled?;
        if let Some(state) = &mut self.labels {
            state.next = k.next();
        }
        Some(CellLabel {
            cell: k,
            echo,
            lag: used_lag,
            undecided,
        })
    }

    /// Whether every reference sample that could reach the window `[k0, k0 + cells)` at any lag
    /// of the search has arrived and is silent. Anything missing, pending or audible is `false`.
    fn reachable_reference_silent(&mut self, k0: CellIndex, cells: usize) -> bool {
        let Some(r_s) = self.mic_to_reference(k0.start().0) else {
            return false;
        };
        let from = r_s - i64::from(self.policy.lag_max);
        let to = r_s + (cells * CELL_SAMPLES) as i64;
        if !matches!(self.reference_read(from, to), Read::Held { .. }) {
            return false;
        }
        self.ref_buf.resize((to - from) as usize, 0.0);
        self.reference.store.copy_to(from, &mut self.ref_buf);
        all_silent(&self.ref_buf, self.policy.silence_mean_square())
    }

    /// Reads the microphone cells of a window into the scratch buffers, with their validity.
    fn read_mic_cells(&mut self, first: CellIndex, cells: usize) {
        self.mic_buf.resize(cells * CELL_SAMPLES, 0.0);
        self.valid_buf.clear();
        for i in 0..cells {
            let start = (first.0 + i as u64) * CELL_SAMPLES as u64;
            let held = matches!(
                self.mic
                    .state(start as i64, (start + CELL_SAMPLES as u64) as i64),
                Read::Held { .. }
            );
            let gap = self
                .slot(CellIndex(first.0 + i as u64))
                .is_some_and(CellSlot::is_gap);
            let dst = &mut self.mic_buf[i * CELL_SAMPLES..(i + 1) * CELL_SAMPLES];
            if held && !gap {
                self.mic.copy_to(start as i64, dst);
            } else {
                dst.fill(0.0);
            }
            self.valid_buf.push(held && !gap);
        }
    }

    /// Labels one window. `None` means it must wait for reference.
    fn label_window(
        &mut self,
        k0: CellIndex,
        cells: usize,
        lag: Option<Lag>,
        contexts: &[(bool, Option<f64>)],
    ) -> Option<(EchoCell, Option<Lag>, Option<Undecided>)> {
        let len = cells * CELL_SAMPLES;
        let silence = self.policy.silence_mean_square();
        let missing = Some((EchoCell::MISSING, None, Some(Undecided::ReferenceMissing)));
        let Some(r_s) = self.mic_to_reference(k0.start().0) else {
            return match self.reference_unplaced() {
                Read::Pending => None,
                _ => missing,
            };
        };
        let Some(lag) = lag else {
            // No lag: the window is silent only if everything that could reach it is.
            let from = r_s - i64::from(self.policy.lag_max);
            let to = r_s + len as i64;
            let tags = match self.reference_read(from, to) {
                Read::Pending => return None,
                Read::Missing => return missing,
                Read::Held { tags } => tags,
            };
            self.ref_buf.resize((to - from) as usize, 0.0);
            self.reference.store.copy_to(from, &mut self.ref_buf);
            if all_silent(&self.ref_buf, silence) {
                return Some((EchoCell::SILENT_REFERENCE, None, None));
            }
            return Some((
                EchoCell {
                    label: EchoLabel::Unknown,
                    coverage: tag_coverage(tags),
                },
                None,
                Some(Undecided::NoLag),
            ));
        };

        let refine = i64::from(self.policy.lag_refine);
        let aligned = r_s - i64::from(lag.0);
        let from = aligned - refine;
        let to = aligned + len as i64 + refine;
        let tags = match self.reference_read(from, to) {
            Read::Pending => return None,
            Read::Missing => return missing,
            Read::Held { tags } => tags,
        };
        self.ref_buf.resize((to - from) as usize, 0.0);
        self.reference.store.copy_to(from, &mut self.ref_buf);
        let at = |d: i64| (refine - d) as usize;
        if mean_square(&self.ref_buf[at(0)..at(0) + len]) < silence {
            // Nothing is compared for a silent window, so no lag is reported for it.
            return Some((EchoCell::SILENT_REFERENCE, None, None));
        }
        let coverage = tag_coverage(tags);
        if coverage == ReferenceCoverage::Partial {
            return Some((
                EchoCell {
                    label: EchoLabel::Unknown,
                    coverage,
                },
                Some(lag),
                Some(Undecided::PartialCoverage),
            ));
        }
        self.read_mic_cells(k0, cells);
        // Settle the lag to this window's best correlation within ±refine samples, never
        // outside the 0–500 ms search.
        let base = i64::from(lag.0);
        let lowest = (-refine).max(-base);
        let highest = refine.min(i64::from(self.policy.lag_max) - base);
        let mut best = (0i64, f64::MIN);
        for d in lowest..=highest {
            let r = &self.ref_buf[at(d)..at(d) + len];
            let c = window_correlation(&self.mic_buf, &self.valid_buf, r);
            if c > best.1 {
                best = (d, c);
            }
        }
        let used = Lag((base + best.0) as u32);
        let offset = at(best.0);
        self.measure_cells(contexts, offset);
        let (label, undecided) = match judge(&self.measures, &self.policy) {
            Verdict::EchoOnly => (EchoLabel::EchoOnly, None),
            Verdict::NearEndPresent => (EchoLabel::NearEndPresent, None),
            Verdict::Unknown(u) => (EchoLabel::Unknown, Some(u)),
        };
        Some((EchoCell { label, coverage }, Some(used), undecided))
    }

    /// Compares a wake candidate's support with the reference (post-AFE §16.6): its own 0–500 ms
    /// lag search over the support, then the three-way rule over every cell of the support.
    /// `None` while an input it needs has not arrived; call again after more input.
    pub fn compare_candidate(&mut self, support: SampleSpan) -> Option<CandidateComparison> {
        let plan = self.plan?;
        self.advance_background();
        if support.is_empty() {
            return Some(CandidateComparison::unknown(
                ReferenceCoverage::Missing,
                Undecided::TooFewValid,
                None,
            ));
        }
        let first = support.start.cell().max(plan.vad_from);
        let last = Sample(support.end.0 - 1).cell();
        if last < first {
            return Some(CandidateComparison::unknown(
                ReferenceCoverage::Missing,
                Undecided::TooFewValid,
                None,
            ));
        }
        if self.mic.end() < last.end().0 {
            return None;
        }
        let cells = (last.0 - first.0 + 1) as usize;
        let mut contexts = std::mem::take(&mut self.contexts);
        let result = if self.fill_contexts(&plan, first, cells, &mut contexts) {
            self.compare_cells(first, &contexts)
        } else {
            None
        };
        self.contexts = contexts;
        result
    }

    /// The candidate comparison over the cells from `first`, whose speech-positive flags and
    /// `B` are `contexts`.
    fn compare_cells(
        &mut self,
        first: CellIndex,
        contexts: &[(bool, Option<f64>)],
    ) -> Option<CandidateComparison> {
        let cells = contexts.len();
        let n = cells * CELL_SAMPLES;
        let lag_max = i64::from(self.policy.lag_max);
        let Some(r_s) = self.mic_to_reference(first.start().0) else {
            return match self.reference_unplaced() {
                Read::Pending => None,
                _ => Some(CandidateComparison::unknown(
                    ReferenceCoverage::Missing,
                    Undecided::ReferenceMissing,
                    None,
                )),
            };
        };
        let tags = match self.reference_read(r_s - lag_max, r_s + n as i64) {
            Read::Pending => return None,
            Read::Missing => {
                return Some(CandidateComparison::unknown(
                    ReferenceCoverage::Missing,
                    Undecided::ReferenceMissing,
                    None,
                ));
            }
            Read::Held { tags } => tags,
        };
        self.ref_buf.resize(n + lag_max as usize, 0.0);
        self.reference
            .store
            .copy_to(r_s - lag_max, &mut self.ref_buf);
        if all_silent(&self.ref_buf, self.policy.silence_mean_square()) {
            return Some(CandidateComparison::unknown(
                ReferenceCoverage::Silent,
                Undecided::NoLag,
                Some(LagOutcome::Unknown(LagUnknown::ReferenceSilent)),
            ));
        }
        let coverage = tag_coverage(tags);
        if coverage == ReferenceCoverage::Partial {
            return Some(CandidateComparison::unknown(
                coverage,
                Undecided::PartialCoverage,
                None,
            ));
        }
        self.read_mic_cells(first, cells);
        self.sample_valid.clear();
        for &v in &self.valid_buf {
            self.sample_valid
                .extend(std::iter::repeat_n(v, CELL_SAMPLES));
        }
        let outcome = self.estimator.estimate(
            &self.mic_buf,
            Some(&self.sample_valid),
            &self.ref_buf,
            &self.policy,
        );
        let base = CandidateComparison {
            echo: CandidateEcho::Unknown,
            coverage,
            lag: Some(outcome),
            undecided: None,
            cells: cells as u32,
            valid_cells: self.valid_buf.iter().filter(|v| **v).count() as u32,
            speech_positive_cells: contexts.iter().filter(|c| c.0).count() as u32,
        };
        let Some(lag) = outcome.lag() else {
            return Some(CandidateComparison {
                undecided: Some(Undecided::NoLag),
                ..base
            });
        };
        let offset = (lag_max - i64::from(lag.0)) as usize;
        self.measure_cells(contexts, offset);
        Some(match judge(&self.measures, &self.policy) {
            Verdict::EchoOnly => CandidateComparison {
                echo: CandidateEcho::EchoOnly,
                ..base
            },
            Verdict::NearEndPresent => CandidateComparison {
                echo: CandidateEcho::NearEndPresent,
                ..base
            },
            Verdict::Unknown(u) => CandidateComparison {
                undecided: Some(u),
                ..base
            },
        })
    }

    /// Measures every cell in the scratch microphone buffer against the scratch reference from
    /// `offset`, into `measures`.
    fn measure_cells(&mut self, contexts: &[(bool, Option<f64>)], offset: usize) {
        self.measures.clear();
        for (i, &(sp, b)) in contexts.iter().enumerate() {
            if !self.valid_buf[i] {
                self.measures.push(CellMeasure::INVALID);
                continue;
            }
            let m = &self.mic_buf[i * CELL_SAMPLES..(i + 1) * CELL_SAMPLES];
            let r = &self.ref_buf[offset + i * CELL_SAMPLES..offset + (i + 1) * CELL_SAMPLES];
            self.measures.push(CellMeasure::measure(m, r, sp, b));
        }
    }
}

fn coverage_tag(c: OutputCoverage) -> u8 {
    match c {
        OutputCoverage::Full => TAG_FULL,
        OutputCoverage::Partial => TAG_PARTIAL,
    }
}

fn tag_coverage(tags: u8) -> ReferenceCoverage {
    if tags & TAG_PARTIAL != 0 {
        ReferenceCoverage::Partial
    } else {
        ReferenceCoverage::Full
    }
}
