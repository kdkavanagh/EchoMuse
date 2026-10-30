//! Audio of the analysed subscriptions (a wake candidate's, a turn's, a reply window's,
//! the early-answer watch's): what each delivery becomes, and how the workers' evidence
//! reaches the turn policy.
//!
//! Every chunk of an analysed subscription goes to the microphone analysis worker (mic,
//! reference and loudness cells: VAD, streaming ASR and echo attribution) and its
//! reference also to the reference scoring worker. The device's loudness cells and the
//! known gaps go straight to the turn policy; VAD, echo labels and ASR blocks come back
//! from the worker. Until a turn policy exists for the stream (a candidate still being
//! verified, a reply window before its utterance opens) that evidence is kept in
//! [`Evidence`], and the policy takes it when it is built: the policy's assembler
//! releases cells in sample order whatever order its inputs came in.
//!
//! The helpers here act on the [`Analysis`] its owner (the wake handling or the
//! conversation) hands them; they never look for it themselves.

use std::num::NonZeroU64;

use echomuse_core::types::WorkScope;
use echomuse_echo::OutputCoverage;
use echomuse_protocol::StreamId;
use echomuse_turn::{
    AsrObservation, CellIndex, DeviceCell, DeviceInput, EchoCell, Epoch, EvidenceError, Sample,
    SampleSpan, VadInput,
};
use echomuse_uplink::{Chunk, ChunkBody, ChunkData, Ended};

use crate::{
    evidence::Pushes,
    io::Effect,
    link::EffectLink,
    speech::{MicJob, Pcm16Le, ReferenceJob, StreamKey},
};

/// Evidence for a stream that no turn policy holds yet, in arrival order.
#[derive(Debug, Default)]
pub(crate) struct Evidence {
    cells: Vec<(CellIndex, Vec<DeviceInput>)>,
    vad: Vec<(CellIndex, Vec<VadInput>)>,
    echo: Vec<(CellIndex, Vec<EchoCell>)>,
    asr: Vec<AsrObservation>,
    gaps: Vec<SampleSpan>,
}

impl Evidence {
    /// The tokens of the latest ASR observation kept.
    pub(crate) fn last_tokens(&self) -> Vec<echomuse_turn::Token> {
        self.asr
            .last()
            .map(|observation| observation.tokens.clone())
            .unwrap_or_default()
    }

    /// The VAD of every cell heard, from the first: unknown cells (and cells the worker
    /// never reported) are `None`. `None` when no VAD came.
    pub(crate) fn vad_track(&self) -> Option<crate::afe::Silero> {
        let first = self.vad.iter().map(|(first, _)| first.0).min()?;
        let mut cells: Vec<Option<f32>> = Vec::new();
        for (at, values) in &self.vad {
            for (i, value) in values.iter().enumerate() {
                let index = (at.0 + i as u64 - first) as usize;
                if cells.len() <= index {
                    cells.resize(index + 1, None);
                }
                cells[index] = match value {
                    echomuse_turn::VadInput::Probability(p) => Some(p.get()),
                    echomuse_turn::VadInput::Unknown => None,
                };
            }
        }
        Some(crate::afe::Silero {
            start: CellIndex(first).start().0,
            cells,
        })
    }

    /// Hands everything to a policy's assembler. Inputs before the plan's start are
    /// history the plan does not want and are skipped.
    pub(crate) fn replay(
        self,
        epoch: Epoch,
        assembler: &mut impl Pushes,
    ) -> Result<(), EvidenceError> {
        for (first, cells) in self.cells {
            push_cells(assembler, epoch, first, &cells)?;
        }
        for (first, vad) in self.vad {
            skip_outside(assembler.push_vad(epoch, first, &vad))?;
        }
        for (first, echo) in self.echo {
            skip_outside(assembler.push_echo(epoch, first, &echo))?;
        }
        for span in self.gaps {
            assembler.mark_gap(epoch, span)?;
        }
        for observation in self.asr {
            assembler.push_asr(epoch, observation)?;
        }
        Ok(())
    }
}

impl Evidence {
    /// Hands the loudness, VAD and gaps already heard to a new utterance's assembler
    /// (a reopened reply). Its echo labels and ASR come fresh from the worker's new
    /// utterance, so they are not replayed.
    pub(crate) fn replay_history(
        &self,
        epoch: Epoch,
        assembler: &mut impl Pushes,
    ) -> Result<(), EvidenceError> {
        for (first, cells) in &self.cells {
            push_cells(assembler, epoch, *first, cells)?;
        }
        for (first, vad) in &self.vad {
            let from = assembler.plan().vad_from;
            let skip = from.0.saturating_sub(first.0) as usize;
            if skip < vad.len() {
                skip_outside(assembler.push_vad(
                    epoch,
                    CellIndex(first.0 + skip as u64),
                    &vad[skip..],
                ))?;
            }
        }
        for span in &self.gaps {
            assembler.mark_gap(epoch, *span)?;
        }
        Ok(())
    }
}

fn skip_outside(result: Result<(), EvidenceError>) -> Result<(), EvidenceError> {
    match result {
        Err(EvidenceError::OutsidePlan { .. }) => Ok(()),
        other => other,
    }
}

/// Pushes loudness cells, dropping those before the plan's first cell.
pub(crate) fn push_cells(
    assembler: &mut impl Pushes,
    epoch: Epoch,
    first: CellIndex,
    cells: &[DeviceInput],
) -> Result<(), EvidenceError> {
    let from = assembler.plan().cells_from;
    let skip = from.0.saturating_sub(first.0) as usize;
    if skip >= cells.len() {
        return Ok(());
    }
    assembler.push_device_cells(epoch, CellIndex(first.0 + skip as u64), &cells[skip..])
}

/// One analysed subscription stream.
#[derive(Debug)]
pub(crate) struct Analysis {
    pub(crate) key: StreamKey,
    pub(crate) capture_epoch: NonZeroU64,
    /// The end of the mic audio delivered so far (data or known missing).
    pub(crate) mic_end: Option<Sample>,
    /// The first mic sample delivered.
    pub(crate) mic_start: Option<Sample>,
    /// Evidence waiting for a policy; `None` once a policy took it.
    pub(crate) pending: Option<Evidence>,
    /// The loudness, VAD and gaps of the whole stream, for a reopened reply.
    pub(crate) log: Evidence,
    /// The first loudness cell the stream carries.
    pub(crate) cells_from: Option<CellIndex>,
    /// The stream's native AFE records by capture period start (an `afe_metadata`
    /// session), for the turn's chart.
    pub(crate) afe: std::collections::BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
    /// The device's `CLOCK_MONOTONIC` time of each mic block's first sample, newest last
    /// (at most [`MIC_CLOCK_ANCHORS`]): the mic stream's clock.
    pub(crate) mic_clock: std::collections::VecDeque<(u64, u64)>,
}

/// Mic block stamps kept per stream: 5 s of 80 ms blocks.
const MIC_CLOCK_ANCHORS: usize = 64;
/// Device nanoseconds per 16 kHz capture sample.
const NS_PER_SAMPLE: u64 = 62_500;

impl Analysis {
    pub(crate) fn new(key: StreamKey, capture_epoch: NonZeroU64) -> Self {
        Self {
            key,
            capture_epoch,
            mic_end: None,
            mic_start: None,
            pending: Some(Evidence::default()),
            log: Evidence::default(),
            cells_from: None,
            afe: std::collections::BTreeMap::new(),
            mic_clock: std::collections::VecDeque::new(),
        }
    }

    pub(crate) fn epoch(&self) -> Epoch {
        self.key.epoch
    }

    /// Notes that mic sample `sample` was captured at device time `mono_ns`.
    pub(crate) fn note_clock(&mut self, sample: u64, mono_ns: u64) {
        if mono_ns == 0 || self.mic_clock.iter().any(|&(s, _)| s == sample) {
            return;
        }
        if self.mic_clock.len() == MIC_CLOCK_ANCHORS {
            self.mic_clock.pop_front();
        }
        self.mic_clock.push_back((sample, mono_ns));
    }

    /// The device time mic sample `sample` was captured at: from the nearest block stamp
    /// at or before it (or the earliest after it) at the nominal 16 kHz; `None` before any
    /// stamp.
    pub(crate) fn mono_at(&self, sample: u64) -> Option<u64> {
        let before = self
            .mic_clock
            .iter()
            .filter(|&&(s, _)| s <= sample)
            .max_by_key(|&&(s, _)| s);
        match before {
            Some(&(s, t)) => Some(t + (sample - s) * NS_PER_SAMPLE),
            None => {
                let &(s, t) = self.mic_clock.iter().min_by_key(|&&(s, _)| s)?;
                t.checked_sub((s - sample) * NS_PER_SAMPLE)
            }
        }
    }
}

/// Evidence a chunk brought that its owner's policy takes at once: nothing while the
/// analysis keeps its evidence for a policy still to come.
pub(crate) enum Fresh {
    Nothing,
    Cells(CellIndex, Vec<DeviceInput>),
    Gap(SampleSpan),
}

impl Fresh {
    /// Hands the evidence to a policy's assembler.
    pub(crate) fn push(
        self,
        epoch: Epoch,
        assembler: &mut impl Pushes,
    ) -> Result<(), EvidenceError> {
        match self {
            Self::Nothing => Ok(()),
            Self::Cells(first, cells) => push_cells(assembler, epoch, first, &cells),
            Self::Gap(span) => assembler.mark_gap(epoch, span),
        }
    }

    pub(crate) fn is_nothing(&self) -> bool {
        matches!(self, Self::Nothing)
    }
}

/// The worker's VAD, echo labels and ASR for an owner whose policy takes them now.
pub(crate) struct WorkerEvidence {
    pub(crate) vad: Option<(CellIndex, Vec<VadInput>)>,
    pub(crate) echo: Option<(CellIndex, Vec<EchoCell>)>,
    pub(crate) asr: Vec<AsrObservation>,
}

/// What became of an analysed subscription's chunk, for the owner to act on.
pub(crate) enum StreamNews {
    /// Mic audio advanced (data or a known gap).
    Mic,
    /// The subscription ended.
    Ended(Ended),
    Other,
}

impl Analysis {
    /// Routes one chunk of the subscription: its worker jobs under `scope` (none once
    /// that is cancelled), and what the analysis records of it. `None` when the chunk
    /// gives the owner nothing to act on.
    pub(crate) fn chunk(
        &mut self,
        link: &mut EffectLink,
        scope: &WorkScope,
        chunk: Chunk,
    ) -> Option<(StreamNews, Fresh)> {
        let key = self.key.clone();
        let span = SampleSpan::new(Sample(chunk.span.start), Sample(chunk.span.end));
        let routed = match (chunk.stream, chunk.body) {
            (
                StreamId::Mic,
                ChunkBody::Data {
                    meta,
                    discontinuity,
                    data: ChunkData::Pcm(pcm),
                },
            ) => {
                self.note_mic(span);
                self.note_clock(span.start.0, meta.mono_ns);
                mic_job(
                    link,
                    scope,
                    MicJob::Mic {
                        key,
                        first: span.start,
                        mono_ns: meta.mono_ns,
                        discontinuity,
                        pcm: Pcm16Le::new(pcm.as_bytes().clone()),
                    },
                );
                (StreamNews::Mic, Fresh::Nothing)
            }
            (StreamId::Mic, ChunkBody::Missing(_)) => {
                self.note_mic(span);
                mic_job(link, scope, MicJob::MicMissing { key, span });
                (StreamNews::Mic, self.gap(span))
            }
            (StreamId::Reference, body) => {
                let reference_epoch = Epoch::new(chunk.epoch.get())?;
                let first = chunk.span.start;
                match body {
                    ChunkBody::Data {
                        meta,
                        data: ChunkData::Pcm(pcm),
                        ..
                    } => {
                        let pcm = Pcm16Le::new(pcm.as_bytes().clone());
                        mic_job(
                            link,
                            scope,
                            MicJob::Reference {
                                key: key.clone(),
                                reference_epoch,
                                first,
                                mono_ns: meta.mono_ns,
                                coverage: OutputCoverage::Full,
                                pcm: pcm.clone(),
                            },
                        );
                        reference_job(
                            link,
                            scope,
                            ReferenceJob::Pcm {
                                key,
                                reference_epoch,
                                first,
                                pcm,
                            },
                        );
                    }
                    ChunkBody::Data {
                        meta,
                        data: ChunkData::DigitalSilence { frames },
                        ..
                    } => {
                        mic_job(
                            link,
                            scope,
                            MicJob::ReferenceSilence {
                                key: key.clone(),
                                reference_epoch,
                                first,
                                frames,
                                mono_ns: meta.mono_ns,
                                coverage: OutputCoverage::Full,
                            },
                        );
                        reference_job(
                            link,
                            scope,
                            ReferenceJob::Silence {
                                key,
                                reference_epoch,
                                first,
                                frames,
                            },
                        );
                    }
                    ChunkBody::Missing(_) => {
                        mic_job(
                            link,
                            scope,
                            MicJob::ReferenceMissing {
                                key: key.clone(),
                                reference_epoch,
                                first,
                                end: chunk.span.end,
                            },
                        );
                        reference_job(
                            link,
                            scope,
                            ReferenceJob::Missing {
                                key,
                                reference_epoch,
                                first,
                                end: chunk.span.end,
                            },
                        );
                    }
                    ChunkBody::Data { .. } => {}
                }
                (StreamNews::Other, Fresh::Nothing)
            }
            (StreamId::Cells, body) => {
                let first = CellIndex(chunk.span.start / echomuse_turn::time::CELL_SAMPLES);
                let cells: Vec<DeviceInput> = match body {
                    ChunkBody::Data {
                        data: ChunkData::Cells(cells),
                        ..
                    } => cells
                        .iter()
                        .map(|cell| {
                            DeviceCell::from_wire(cell.e_centi, cell.flags.bits(), cell.mask.bits())
                                .map_or(DeviceInput::Unknown, DeviceInput::Cell)
                        })
                        .collect(),
                    // Loudness that never arrives is unknown, not silence and not a gap
                    // in the audio.
                    ChunkBody::Missing(_) => {
                        let count = chunk.span.end.div_ceil(512) - chunk.span.start / 512;
                        vec![DeviceInput::Unknown; count as usize]
                    }
                    ChunkBody::Data { .. } => Vec::new(),
                };
                let fresh = if cells.is_empty() {
                    Fresh::Nothing
                } else {
                    mic_job(
                        link,
                        scope,
                        MicJob::DeviceCells {
                            key,
                            first,
                            cells: cells.clone(),
                        },
                    );
                    self.cells(first, cells)
                };
                (StreamNews::Other, fresh)
            }
            (
                StreamId::Afe,
                ChunkBody::Data {
                    data: ChunkData::Afe(records),
                    ..
                },
            ) => {
                store_afe(&mut self.afe, chunk.span.start, records);
                (StreamNews::Other, Fresh::Nothing)
            }
            _ => (StreamNews::Other, Fresh::Nothing),
        };
        Some(routed)
    }

    fn note_mic(&mut self, span: SampleSpan) {
        self.mic_start.get_or_insert(span.start);
        if self.mic_end.is_none_or(|end| span.end > end) {
            self.mic_end = Some(span.end);
        }
    }

    /// Loudness cells: kept until a policy takes the stream, or fresh for it.
    fn cells(&mut self, first: CellIndex, cells: Vec<DeviceInput>) -> Fresh {
        self.log.cells.push((first, cells.clone()));
        match self.pending.as_mut() {
            Some(pending) => {
                pending.cells.push((first, cells));
                Fresh::Nothing
            }
            None => Fresh::Cells(first, cells),
        }
    }

    fn gap(&mut self, span: SampleSpan) -> Fresh {
        self.log.gaps.push(span);
        match self.pending.as_mut() {
            Some(pending) => {
                pending.gaps.push(span);
                Fresh::Nothing
            }
            None => Fresh::Gap(span),
        }
    }

    /// The worker's VAD, echo labels and ASR: the VAD joins the stream's log; all of it
    /// is kept until a policy takes the stream, or returned for the policy that has it.
    pub(crate) fn worker_evidence(
        &mut self,
        vad: Option<(CellIndex, Vec<VadInput>)>,
        echo: Option<(CellIndex, Vec<EchoCell>)>,
        asr: Vec<AsrObservation>,
    ) -> Option<WorkerEvidence> {
        if let Some(vad) = &vad {
            self.log.vad.push(vad.clone());
        }
        match self.pending.as_mut() {
            Some(pending) => {
                if let Some(vad) = vad {
                    pending.vad.push(vad);
                }
                if let Some(echo) = echo {
                    pending.echo.push(echo);
                }
                pending.asr.extend(asr);
                None
            }
            None => Some(WorkerEvidence { vad, echo, asr }),
        }
    }
}

/// Queues a microphone worker job under `scope`, unless that is cancelled.
pub(crate) fn mic_job(link: &mut EffectLink, scope: &WorkScope, job: MicJob) {
    if scope.is_cancelled() {
        return;
    }
    let key = match &job {
        MicJob::Reset => None,
        MicJob::Open { key, .. }
        | MicJob::StartUtterance { key, .. }
        | MicJob::Mic { key, .. }
        | MicJob::MicMissing { key, .. }
        | MicJob::Reference { key, .. }
        | MicJob::ReferenceSilence { key, .. }
        | MicJob::ReferenceMissing { key, .. }
        | MicJob::DeviceCells { key, .. }
        | MicJob::CompareCandidate { key, .. }
        | MicJob::Close { key } => Some(key.clone()),
    };
    // Loudness cells that arrive in a run (a subscription's history comes as the many
    // small packets it was sent in, two cells each) go to the worker as one job, so ten
    // seconds of history do not overflow its queue.
    if let MicJob::DeviceCells { key, first, cells } = &job
        && let Some(Effect::Mic {
            job:
                MicJob::DeviceCells {
                    key: last_key,
                    first: last_first,
                    cells: last_cells,
                },
            ..
        }) = link.effects.last_mut()
        && last_key == key
        && last_first.0 + last_cells.len() as u64 == first.0
    {
        last_cells.extend_from_slice(cells);
        return;
    }
    link.effects.push(Effect::Mic {
        scope: scope.clone(),
        key,
        job,
    });
}

/// Queues a reference scoring job under `scope`, unless that is cancelled.
pub(crate) fn reference_job(link: &mut EffectLink, scope: &WorkScope, job: ReferenceJob) {
    if scope.is_cancelled() {
        return;
    }
    let key = match &job {
        ReferenceJob::Reset => None,
        ReferenceJob::Open { key, .. }
        | ReferenceJob::Pcm { key, .. }
        | ReferenceJob::Silence { key, .. }
        | ReferenceJob::Missing { key, .. }
        | ReferenceJob::Judge { key, .. }
        | ReferenceJob::Close { key } => Some(key.clone()),
    };
    link.effects.push(Effect::Reference {
        scope: scope.clone(),
        key,
        job,
    });
}

/// Pushes the worker's evidence into an assembler.
pub(crate) fn push_worker_evidence(
    assembler: &mut impl Pushes,
    epoch: Epoch,
    evidence: WorkerEvidence,
) -> Result<(), EvidenceError> {
    if let Some((first, vad)) = evidence.vad {
        skip_outside(assembler.push_vad(epoch, first, &vad))?;
    }
    if let Some((first, echo)) = evidence.echo {
        skip_outside(assembler.push_echo(epoch, first, &echo))?;
    }
    for observation in evidence.asr {
        assembler.push_asr(epoch, observation)?;
    }
    Ok(())
}

/// Files AFE records delivered from capture sample `first`, one per period.
pub(crate) fn store_afe(
    into: &mut std::collections::BTreeMap<u64, echomuse_protocol::ema::AfeRecord>,
    first: u64,
    records: Vec<echomuse_protocol::ema::AfeRecord>,
) {
    for (i, record) in records.into_iter().enumerate() {
        into.insert(first + i as u64 * crate::afe::PERIOD, record);
    }
}
