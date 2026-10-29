//! Audio of the analysed subscriptions (a wake candidate's, a turn's, a reply window's):
//! where each delivery goes, and how the workers' evidence reaches the turn policy.
//!
//! Every chunk of an analysed subscription goes to the microphone analysis worker (mic,
//! reference and loudness cells: VAD, streaming ASR and echo attribution) and its
//! reference also to the reference scoring worker. The device's loudness cells and the
//! known gaps go straight to the turn policy; VAD, echo labels and ASR blocks come back
//! from the worker. Until a turn policy exists for the stream (a candidate still being
//! verified, a reply window before its utterance opens) that evidence is kept in
//! [`Evidence`], and the policy takes it when it is built: the policy's assembler
//! releases cells in sample order whatever order its inputs came in.

use std::num::NonZeroU64;

use echomuse_core::types::RuntimeError;
use echomuse_echo::OutputCoverage;
use echomuse_protocol::StreamId;
use echomuse_turn::{
    AsrObservation, CellIndex, DeviceCell, DeviceInput, EchoCell, Epoch, EvidenceError, Sample,
    SampleSpan, VadInput,
};
use echomuse_uplink::{ChunkBody, ChunkData, Delivery, Ended, SubscriptionId};

use crate::{
    evidence::Pushes,
    io::{Effect, Now},
    machine::Voice,
    speech::{MicJob, MicOutput, Pcm16Le, ReferenceJob, ReferenceOutput, StreamKey},
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

/// Which owner a stream belongs to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Owner {
    Candidate,
    Turn,
    Window,
    /// The early-answer watch while the controller's own question plays.
    Watch,
}

/// What became of an analysed subscription's chunk, for the owner to act on.
pub(crate) enum StreamNews {
    /// Mic audio advanced (data or a known gap).
    Mic,
    /// The subscription ended.
    Ended(Ended),
    Other,
}

impl Voice {
    pub(crate) fn owner_of(&self, subscription: &SubscriptionId) -> Option<Owner> {
        if self
            .candidate
            .as_ref()
            .is_some_and(|c| &c.message.subscription == subscription)
        {
            return Some(Owner::Candidate);
        }
        if self
            .turn
            .as_ref()
            .is_some_and(|t| t.subscription.as_ref() == Some(subscription))
        {
            return Some(Owner::Turn);
        }
        if self
            .window
            .as_ref()
            .is_some_and(|w| &w.subscription == subscription)
        {
            return Some(Owner::Window);
        }
        if self
            .question_watch
            .as_ref()
            .is_some_and(|w| &w.subscription == subscription)
        {
            return Some(Owner::Watch);
        }
        None
    }

    fn owner_by_key(&self, key: &StreamKey) -> Option<Owner> {
        if self
            .candidate
            .as_ref()
            .is_some_and(|c| &c.analysis.key == key)
        {
            return Some(Owner::Candidate);
        }
        if self
            .turn
            .as_ref()
            .and_then(|t| t.analysis.as_ref())
            .is_some_and(|a| &a.key == key)
        {
            return Some(Owner::Turn);
        }
        if self.window.as_ref().is_some_and(|w| w.analysis.key == *key) {
            return Some(Owner::Window);
        }
        if self
            .question_watch
            .as_ref()
            .is_some_and(|w| w.analysis.key == *key)
        {
            return Some(Owner::Watch);
        }
        None
    }

    fn analysis_mut(&mut self, owner: Owner) -> Option<&mut Analysis> {
        match owner {
            Owner::Candidate => self.candidate.as_mut().map(|c| &mut c.analysis),
            Owner::Turn => self.turn.as_mut().and_then(|t| t.analysis.as_mut()),
            Owner::Window => self.window.as_mut().map(|w| &mut w.analysis),
            Owner::Watch => self.question_watch.as_mut().map(|w| &mut w.analysis),
        }
    }

    /// The scope a stream's jobs run under: the turn's own for a turn, capture for the
    /// rest. `None` once capture is revoked.
    fn job_scope(&self, owner: Owner) -> Option<echomuse_core::types::WorkScope> {
        let scope = match owner {
            Owner::Turn => self.turn.as_ref().map(|t| t.scope.clone()),
            Owner::Candidate => self.candidate.as_ref().map(|c| c.scope.clone()),
            Owner::Window => self.window.as_ref().map(|w| w.scope.clone()),
            Owner::Watch => self.question_watch.as_ref().map(|w| w.scope.clone()),
        }?;
        (!scope.is_cancelled()).then_some(scope)
    }

    pub(crate) fn mic_job(&mut self, owner: Owner, job: MicJob) {
        let Some(scope) = self.job_scope(owner) else {
            return;
        };
        let key = match &job {
            MicJob::Reset => None,
            MicJob::Open { key, .. }
            | MicJob::StartUtterance { key, .. }
            | MicJob::EndUtterance { key }
            | MicJob::Mic { key, .. }
            | MicJob::MicMissing { key, .. }
            | MicJob::Reference { key, .. }
            | MicJob::ReferenceSilence { key, .. }
            | MicJob::ReferenceMissing { key, .. }
            | MicJob::ReferenceEnded { key }
            | MicJob::DeviceCells { key, .. }
            | MicJob::CompareCandidate { key, .. }
            | MicJob::Close { key } => Some(key.clone()),
        };
        // Loudness cells that arrive in a run (a subscription's history comes as the many
        // small packets it was sent in, two cells each) go to the worker as one job, so
        // ten seconds of history do not overflow its queue.
        if let MicJob::DeviceCells { key, first, cells } = &job
            && let Some(Effect::Mic {
                job:
                    MicJob::DeviceCells {
                        key: last_key,
                        first: last_first,
                        cells: last_cells,
                    },
                ..
            }) = self.link.effects.last_mut()
            && last_key == key
            && last_first.0 + last_cells.len() as u64 == first.0
        {
            last_cells.extend_from_slice(cells);
            return;
        }
        self.link.effects.push(Effect::Mic { scope, key, job });
    }

    pub(crate) fn reference_job(&mut self, owner: Owner, job: ReferenceJob) {
        let Some(scope) = self.job_scope(owner) else {
            return;
        };
        let key = match &job {
            ReferenceJob::Reset => None,
            ReferenceJob::Open { key, .. }
            | ReferenceJob::Pcm { key, .. }
            | ReferenceJob::Silence { key, .. }
            | ReferenceJob::Missing { key, .. }
            | ReferenceJob::Ended { key }
            | ReferenceJob::Judge { key, .. }
            | ReferenceJob::Close { key } => Some(key.clone()),
        };
        self.link
            .effects
            .push(Effect::Reference { scope, key, job });
    }

    /// Closes a stream on both workers. Uses the session scope: the stream's own may
    /// already be cancelled, and the workers must still forget its audio.
    pub(crate) fn close_analysis(&mut self, key: StreamKey) {
        if self.session_scope.is_cancelled() {
            return;
        }
        self.link.effects.push(Effect::Mic {
            scope: self.session_scope.clone(),
            key: Some(key.clone()),
            job: MicJob::Close { key: key.clone() },
        });
        self.link.effects.push(Effect::Reference {
            scope: self.session_scope.clone(),
            key: Some(key.clone()),
            job: ReferenceJob::Close { key },
        });
    }

    /// Routes one chunk (or the end) of an analysed subscription.
    pub(crate) fn analysed_delivery(&mut self, delivery: Delivery, now: Now) {
        let Some(owner) = self.owner_of(delivery.subscription()) else {
            return;
        };
        let news = match delivery {
            Delivery::Ended(ended) => StreamNews::Ended(ended),
            Delivery::Chunk(chunk) => {
                let Some(key) = self.analysis_mut(owner).map(|a| a.key.clone()) else {
                    return;
                };
                let span = SampleSpan::new(Sample(chunk.span.start), Sample(chunk.span.end));
                match (chunk.stream, chunk.body) {
                    (
                        StreamId::Mic,
                        ChunkBody::Data {
                            meta,
                            discontinuity,
                            data: ChunkData::Pcm(pcm),
                        },
                    ) => {
                        self.note_mic(owner, span);
                        if let Some(analysis) = self.analysis_mut(owner) {
                            analysis.note_clock(span.start.0, meta.mono_ns);
                        }
                        self.mic_job(
                            owner,
                            MicJob::Mic {
                                key,
                                first: span.start,
                                mono_ns: meta.mono_ns,
                                discontinuity,
                                pcm: Pcm16Le::new(pcm.as_bytes().clone()),
                            },
                        );
                        StreamNews::Mic
                    }
                    (StreamId::Mic, ChunkBody::Missing(_)) => {
                        self.note_mic(owner, span);
                        self.mic_job(
                            owner,
                            MicJob::MicMissing {
                                key: key.clone(),
                                span,
                            },
                        );
                        self.evidence_gap(owner, &key, span, now);
                        StreamNews::Mic
                    }
                    (StreamId::Reference, body) => {
                        let Some(reference_epoch) = Epoch::new(chunk.epoch.get()) else {
                            return;
                        };
                        let first = chunk.span.start;
                        match body {
                            ChunkBody::Data {
                                meta,
                                data: ChunkData::Pcm(pcm),
                                ..
                            } => {
                                let pcm = Pcm16Le::new(pcm.as_bytes().clone());
                                self.mic_job(
                                    owner,
                                    MicJob::Reference {
                                        key: key.clone(),
                                        reference_epoch,
                                        first,
                                        mono_ns: meta.mono_ns,
                                        coverage: OutputCoverage::Full,
                                        pcm: pcm.clone(),
                                    },
                                );
                                self.reference_job(
                                    owner,
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
                                self.mic_job(
                                    owner,
                                    MicJob::ReferenceSilence {
                                        key: key.clone(),
                                        reference_epoch,
                                        first,
                                        frames,
                                        mono_ns: meta.mono_ns,
                                        coverage: OutputCoverage::Full,
                                    },
                                );
                                self.reference_job(
                                    owner,
                                    ReferenceJob::Silence {
                                        key,
                                        reference_epoch,
                                        first,
                                        frames,
                                    },
                                );
                            }
                            ChunkBody::Missing(_) => {
                                self.mic_job(
                                    owner,
                                    MicJob::ReferenceMissing {
                                        key: key.clone(),
                                        reference_epoch,
                                        first,
                                        end: chunk.span.end,
                                    },
                                );
                                self.reference_job(
                                    owner,
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
                        StreamNews::Other
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
                                    DeviceCell::from_wire(
                                        cell.e_centi,
                                        cell.flags.bits(),
                                        cell.mask.bits(),
                                    )
                                    .map_or(DeviceInput::Unknown, DeviceInput::Cell)
                                })
                                .collect(),
                            // Loudness that never arrives is unknown, not silence and
                            // not a gap in the audio.
                            ChunkBody::Missing(_) => {
                                let count = chunk.span.end.div_ceil(512) - chunk.span.start / 512;
                                vec![DeviceInput::Unknown; count as usize]
                            }
                            ChunkBody::Data { .. } => Vec::new(),
                        };
                        if !cells.is_empty() {
                            self.mic_job(
                                owner,
                                MicJob::DeviceCells {
                                    key: key.clone(),
                                    first,
                                    cells: cells.clone(),
                                },
                            );
                            self.evidence_cells(owner, &key, first, cells, now);
                        }
                        StreamNews::Other
                    }
                    (
                        StreamId::Afe,
                        ChunkBody::Data {
                            data: ChunkData::Afe(records),
                            ..
                        },
                    ) => {
                        if let Some(analysis) = self.analysis_mut(owner) {
                            store_afe(&mut analysis.afe, chunk.span.start, records);
                        }
                        StreamNews::Other
                    }
                    _ => StreamNews::Other,
                }
            }
        };
        match owner {
            Owner::Candidate => self.candidate_news(news, now),
            Owner::Turn => self.turn_news(news, now),
            Owner::Window => self.window_news(news, now),
            Owner::Watch => self.watch_news(news, now),
        }
    }

    fn note_mic(&mut self, owner: Owner, span: SampleSpan) {
        if let Some(analysis) = self.analysis_mut(owner) {
            analysis.mic_start.get_or_insert(span.start);
            if analysis.mic_end.is_none_or(|end| span.end > end) {
                analysis.mic_end = Some(span.end);
            }
        }
    }

    /// Loudness cells for the owner's policy, or kept until it has one.
    fn evidence_cells(
        &mut self,
        owner: Owner,
        key: &StreamKey,
        first: CellIndex,
        cells: Vec<DeviceInput>,
        now: Now,
    ) {
        if let Some(analysis) = self.analysis_mut(owner) {
            analysis.log.cells.push((first, cells.clone()));
            if let Some(pending) = analysis.pending.as_mut() {
                pending.cells.push((first, cells));
                return;
            }
        }
        if owner == Owner::Turn {
            let result = self
                .turn_pushes(now)
                .map(|mut pushes| push_cells(&mut pushes, key.epoch, first, &cells));
            if let Some(Err(error)) = result {
                self.evidence_failed(error);
            }
        }
        if owner == Owner::Watch {
            self.watch_assembler(|assembler| push_cells(assembler, key.epoch, first, &cells));
        }
    }

    fn evidence_gap(&mut self, owner: Owner, key: &StreamKey, span: SampleSpan, now: Now) {
        if let Some(analysis) = self.analysis_mut(owner) {
            analysis.log.gaps.push(span);
            if let Some(pending) = analysis.pending.as_mut() {
                pending.gaps.push(span);
                return;
            }
        }
        if owner == Owner::Turn {
            let result = self
                .turn_pushes(now)
                .map(|mut pushes| pushes.mark_gap(key.epoch, span));
            if let Some(Err(error)) = result {
                self.evidence_failed(error);
            }
        }
        if owner == Owner::Watch {
            self.watch_assembler(|assembler| assembler.mark_gap(key.epoch, span));
        }
    }

    /// The microphone worker's answer.
    pub(crate) fn mic_result(
        &mut self,
        key: Option<crate::speech::StreamKey>,
        result: Result<MicOutput, RuntimeError>,
        now: Now,
    ) {
        let Some(key) = key else {
            return;
        };
        let Some(owner) = self.owner_by_key(&key) else {
            self.counters.stale_results += 1;
            return;
        };
        let output = match result {
            Ok(output) => output,
            Err(error) => {
                self.worker_failed(&error);
                self.stream_failed(owner, now);
                return;
            }
        };
        if let Some(failure) = &output.failure {
            tracing::warn!(device = %self.device, failure, "microphone analysis failed");
        }
        if let Some(alignment) = output.alignment
            && owner == Owner::Candidate
        {
            self.candidate_alignment(alignment, now);
        }
        for (candidate, comparison) in output.comparisons.iter().cloned() {
            self.candidate_comparison(&candidate, comparison, now);
        }
        let failed = output.failure.is_some();
        if let (Some(vad), Some(analysis)) = (&output.vad, self.analysis_mut(owner)) {
            analysis.log.vad.push(vad.clone());
        }
        // Evidence for the owner's policy, or kept until it has one.
        if let Some(pending) = self.analysis_mut(owner).and_then(|a| a.pending.as_mut()) {
            if let Some(vad) = output.vad {
                pending.vad.push(vad);
            }
            if let Some(echo) = output.echo {
                pending.echo.push(echo);
            }
            pending.asr.extend(output.asr);
        } else if owner == Owner::Turn {
            let epoch = key.epoch;
            if let Some(turn) = self.turn.as_mut() {
                turn.note_tokens(&output.asr);
            }
            // The pause server is asked when the worker starts a pause decode; should that
            // notice have been lost, the pause decode's own result asks.
            for observation in &output.asr {
                if observation.kind == echomuse_turn::AsrKind::Finalized {
                    self.ask_pause_server(observation.through, now);
                }
            }
            let result = self
                .turn_pushes(now)
                .map(|mut pushes| push_worker_evidence(&mut pushes, epoch, output));
            if let Some(Err(error)) = result {
                self.evidence_failed(error);
            }
        } else if owner == Owner::Watch {
            let epoch = key.epoch;
            self.watch_assembler(|assembler| push_worker_evidence(assembler, epoch, output));
        }
        if failed {
            self.stream_failed(owner, now);
            return;
        }
        match owner {
            Owner::Turn => self.turn_evidence(now),
            Owner::Window => self.window_evidence(now),
            Owner::Candidate => {}
            Owner::Watch => self.watch_evidence(now),
        }
    }

    /// A worker could not analyse the stream: its evidence is unknown from here.
    fn stream_failed(&mut self, owner: Owner, now: Now) {
        match owner {
            Owner::Turn => self.turn_worker_error(now),
            Owner::Candidate => self.candidate_evidence_failed(now),
            Owner::Window => self.window_failed(now),
            Owner::Watch => self.watch_failed(now),
        }
    }

    /// The assembler refused evidence (it conflicts or is out of order): the turn's
    /// evidence can no longer be trusted, so it closes as a worker error.
    fn evidence_failed(&mut self, error: EvidenceError) {
        tracing::warn!(device = %self.device, ?error, "turn evidence refused");
        if let Some(turn) = self.turn.as_mut() {
            turn.evidence_broken = true;
        }
    }

    pub(crate) fn reference_result(
        &mut self,
        key: Option<StreamKey>,
        result: Result<ReferenceOutput, RuntimeError>,
        now: Now,
    ) {
        let Some(key) = key else {
            return;
        };
        if self.owner_by_key(&key).is_none() {
            self.counters.stale_results += 1;
            return;
        }
        match result {
            Ok(output) => {
                if let Some(failure) = &output.failure {
                    tracing::warn!(device = %self.device, failure, "reference scoring failed");
                }
                for (candidate, judgement) in output.judgements {
                    self.candidate_judgement(&candidate, judgement, now);
                }
            }
            Err(error) => {
                self.worker_failed(&error);
                self.candidate_reference_failed(now);
            }
        }
    }
}

/// Pushes one worker output into an assembler.
fn push_worker_evidence(
    assembler: &mut impl Pushes,
    epoch: Epoch,
    output: MicOutput,
) -> Result<(), EvidenceError> {
    if let Some((first, vad)) = output.vad {
        skip_outside(assembler.push_vad(epoch, first, &vad))?;
    }
    if let Some((first, echo)) = output.echo {
        skip_outside(assembler.push_echo(epoch, first, &echo))?;
    }
    for observation in output.asr {
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
