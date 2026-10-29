//! The microphone analysis worker's state: per analysed subscription stream, Silero VAD
//! over every mic cell, the utterance's streaming Kroko decode with its pause finalizer,
//! and the echo analyzer (spec §5.1).
//!
//! **VAD.** Every 32 ms cell from the plan's `vad_from` is scored on the VAD input
//! (+30 dB). The session starts fresh at the stream's first mic sample and is reset at a
//! discontinuity (before the cell holding the first sample after it) and after missing
//! audio, whose 64-sample context and recurrent state would otherwise describe audio that
//! does not precede the next cell. Cells that touch missing audio get no VAD: the turn's
//! evidence assembler marks those cells as gaps itself, and a value for them would be
//! invented.
//!
//! **Utterance.** A fresh Kroko stream from the utterance's start reads the evidence copy
//! (+20 dB) in blocks that end where the device's mic packets end, and every completed
//! block yields one [`AsrObservation`]. The device sends at most 80 ms (1,280 samples) per
//! packet: a wake candidate's pre-roll in 1,280-sample packets from the subscription's mic
//! start, then live audio in 1,280-sample packets from where its ring stood when the
//! subscription began, which on an Echo Dot is the absolute 1,280-sample grid. The
//! deployed controller looked at the utterance after every packet, and every block
//! boundary in its stored turns lies on these packet ends, so the blocks here end there
//! too rather than every 1,280 samples from the utterance start; a packet longer than
//! 1,280 samples is cut every 1,280 samples. Mic already retained when the utterance starts
//! (a wake the controller verified before accepting) is read as one block, as the deployed
//! controller caught up its pre-roll. The pause finalizer reads the VAD of each completed
//! cell scored after the utterance started, never the pre-roll's: in the deployed
//! controller none of the verified wakes made a pause decode within ten blocks of the
//! trigger, while this worker, counting the pre-roll, made one after the wake word in every
//! one of them. On the block it marks, the observation is a fresh decode of the utterance
//! so far plus 1.5 s of zeros ([`AsrKind::Finalized`]) instead of the live result. The
//! utterance's ASR ends at the first missing audio or discontinuity after its start, as the
//! turn's evidence closes ASR at a gap: a decode across a hole would join audio that was
//! never adjacent.
//!
//! **Retention.** The canonical mic of the last 40 s is kept (from `vad_from` on), so an
//! utterance that starts in the past (a wake decided after its support arrived) is fed
//! from its start at once, and so the pause finalizer can re-decode the whole utterance.
//! 40 s covers the longest utterance the turn policy allows (30 s extended) plus its
//! pre-roll; an utterance that somehow outlives it gets live results at its pauses, never
//! a finalized decode of audio that is gone.
//!
//! **Ordering.** Mic positions arrive in order. A packet that overlaps audio already
//! analysed contributes only its new samples; a packet that skips ahead is treated as if
//! the skipped samples had been reported missing.
//!
//! **Failures.** A model call that fails ends that part of the stream's evidence: VAD stops
//! for the stream, or the utterance's ASR stops, and the output carries `failure`. Nothing
//! is filled in for what was not computed. Echo analyzer refusals are reported the same
//! way.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use echomuse_core::types::SessionId;
use echomuse_echo::{Alignment, CellLabel, EchoAnalyzer, EchoPlan, EchoPolicy};
use echomuse_turn::{
    AsrKind, AsrObservation, AttributionPolicy, CellIndex, EchoCell, PauseFinalizer, Sample,
    SampleSpan, VadInput,
};

use super::models::{
    AsrStream, ModelSet, Vad, evidence_copy, evidence_sample, fresh_decode, vad_sample,
};
use super::{MicJob, MicOutput, StreamKey};

const CELL: usize = 512;
/// The most audio one block holds: 80 ms, the device's largest live mic packet.
const BLOCK: usize = 1_280;
/// Canonical mic kept per stream: 40 s at 16 kHz (see the module documentation).
const RETAIN_SAMPLES: usize = 40 * 16_000;

/// The microphone analysis worker's state for every stream it analyses.
pub struct MicAnalysis {
    models: Arc<dyn ModelSet>,
    streams: HashMap<StreamKey, MicStream>,
    /// VAD sessions of closed streams, kept so a new stream does not load the model again.
    spare_vads: Vec<Box<dyn Vad>>,
    labels: Vec<CellLabel>,
}

/// One contiguous run of retained canonical mic.
struct Segment {
    id: u64,
    start: Sample,
    pcm: VecDeque<i16>,
}

impl Segment {
    fn end(&self) -> Sample {
        Sample(self.start.0 + self.pcm.len() as u64)
    }
}

/// Where a position lies in the retained audio.
enum Located {
    /// In segment `index`, `offset` samples from its start.
    In { index: usize, offset: usize },
    /// Not received yet.
    Ahead,
    /// In audio that is missing, or across a discontinuity.
    Missing,
    /// Before what is still retained.
    Dropped,
}

struct Utterance {
    from: Sample,
    stream: Box<dyn AsrStream>,
    finalizer: PauseFinalizer,
    /// The next cell the finalizer observes.
    finalizer_next: CellIndex,
    /// The first cell whose VAD the finalizer counts: VAD scored before the utterance
    /// started (its pre-roll) never arms a pause decode.
    finalizer_counts_from: CellIndex,
    /// The next sample to feed.
    fed: Sample,
    /// The end of the mic already retained when the utterance started, read as one block.
    preroll_end: Sample,
    /// Evidence copy of the block being filled.
    block: Vec<f32>,
    /// The segment the utterance reads; its ASR ends where that segment ends.
    segment: Option<u64>,
    stopped: bool,
}

struct MicStream {
    plan: EchoPlan,
    analyzer: EchoAnalyzer,
    vad: Option<Box<dyn Vad>>,
    vad_failed: bool,
    vad_reset: bool,
    /// The cell VAD is filling.
    vad_next: CellIndex,
    /// VAD input of `vad_next` so far.
    vad_cell: Vec<f32>,
    /// VAD of every cell from `vad_history_base` up to `vad_next`; `None` where none was
    /// computed.
    vad_history: VecDeque<Option<f32>>,
    vad_history_base: CellIndex,
    segments: VecDeque<Segment>,
    /// The end of every mic packet in the retained audio, in order: where blocks end.
    packet_ends: VecDeque<Sample>,
    retained: usize,
    next_segment: u64,
    /// Whether the next in-order sample continues the last segment.
    open: bool,
    /// Audio before this was dropped by the retention bound.
    dropped_before: Sample,
    /// The next mic sample expected.
    next_sample: Sample,
    utterance: Option<Utterance>,
    comparisons: Vec<(String, SampleSpan)>,
    alignment: Option<Alignment>,
}

/// What one job produced for one stream, before it becomes a [`MicOutput`].
#[derive(Default)]
struct Produced {
    vad_first: Option<CellIndex>,
    vad: Vec<VadInput>,
    asr: Vec<AsrObservation>,
    failures: Vec<String>,
}

impl Produced {
    fn vad(&mut self, cell: CellIndex, v: VadInput) {
        self.vad_first.get_or_insert(cell);
        self.vad.push(v);
    }
}

impl MicStream {
    fn new(plan: EchoPlan, analyzer: EchoAnalyzer) -> Self {
        Self {
            plan,
            analyzer,
            vad: None,
            vad_failed: false,
            vad_reset: false,
            vad_next: plan.vad_from,
            vad_cell: Vec::with_capacity(CELL),
            vad_history: VecDeque::new(),
            vad_history_base: plan.vad_from,
            segments: VecDeque::new(),
            packet_ends: VecDeque::new(),
            retained: 0,
            next_segment: 0,
            open: false,
            dropped_before: plan.vad_from.start(),
            next_sample: plan.vad_from.start(),
            utterance: None,
            comparisons: Vec::new(),
            alignment: None,
        }
    }

    fn vad_at(&self, cell: CellIndex) -> Option<f32> {
        let i = cell.0.checked_sub(self.vad_history_base.0)?;
        self.vad_history
            .get(usize::try_from(i).ok()?)
            .copied()
            .flatten()
    }

    fn locate(&self, at: Sample) -> Located {
        if at < self.dropped_before {
            return Located::Dropped;
        }
        if at >= self.next_sample {
            return Located::Ahead;
        }
        for (index, s) in self.segments.iter().enumerate() {
            if s.start <= at && at < s.end() {
                return Located::In {
                    index,
                    offset: (at.0 - s.start.0) as usize,
                };
            }
        }
        Located::Missing
    }

    /// Canonical PCM of `[from, to)` when it lies in one retained segment.
    fn retained_span(&self, from: Sample, to: Sample) -> Option<Vec<i16>> {
        let Located::In { index, offset } = self.locate(from) else {
            return None;
        };
        let s = &self.segments[index];
        let len = usize::try_from(to.0.checked_sub(from.0)?).ok()?;
        (offset + len <= s.pcm.len()).then(|| s.pcm.range(offset..offset + len).copied().collect())
    }

    /// Records missing audio `[start, end)` (already clipped to what is new).
    fn missing(&mut self, start: Sample, end: Sample) {
        if end <= start {
            return;
        }
        self.open = false;
        self.next_sample = end;
        // VAD restarts at the first whole cell after the hole.
        let resume = CellIndex(end.0.div_ceil(CELL as u64));
        self.vad_cell.clear();
        while self.vad_next < resume {
            self.vad_history.push_back(None);
            self.vad_next = self.vad_next.next();
        }
        self.vad_reset = true;
    }

    fn append(&mut self, first: Sample, pcm: &[i16], discontinuity: bool) {
        if discontinuity || !self.open || self.segments.is_empty() {
            self.segments.push_back(Segment {
                id: self.next_segment,
                start: first,
                pcm: VecDeque::new(),
            });
            self.next_segment += 1;
        }
        self.open = true;
        if let Some(s) = self.segments.back_mut() {
            s.pcm.extend(pcm.iter().copied());
        }
        self.retained += pcm.len();
        self.next_sample = Sample(first.0 + pcm.len() as u64);
        self.packet_ends.push_back(self.next_sample);
        self.prune();
    }

    /// Drops the oldest audio beyond the retention bound, with the VAD of its cells.
    fn prune(&mut self) {
        while self.retained > RETAIN_SAMPLES {
            let excess = self.retained - RETAIN_SAMPLES;
            let Some(front) = self.segments.front_mut() else {
                break;
            };
            let n = excess.min(front.pcm.len());
            front.pcm.drain(..n);
            front.start = Sample(front.start.0 + n as u64);
            self.retained -= n;
            self.dropped_before = front.start;
            if front.pcm.is_empty() && self.segments.len() > 1 {
                self.segments.pop_front();
            }
        }
        while self.vad_history_base.end() <= self.dropped_before && !self.vad_history.is_empty() {
            self.vad_history.pop_front();
            self.vad_history_base = self.vad_history_base.next();
        }
        while self
            .packet_ends
            .front()
            .is_some_and(|&end| end <= self.dropped_before)
        {
            self.packet_ends.pop_front();
        }
    }

    /// Scores every cell `pcm` (from `first`) completes.
    fn run_vad(
        &mut self,
        models: &dyn ModelSet,
        spare: &mut Vec<Box<dyn Vad>>,
        first: Sample,
        pcm: &[i16],
        out: &mut Produced,
    ) {
        let skip = usize::try_from(self.vad_next.start().0.saturating_sub(first.0))
            .unwrap_or(usize::MAX)
            .min(pcm.len());
        for &s in &pcm[skip..] {
            self.vad_cell.push(vad_sample(s));
            if self.vad_cell.len() < CELL {
                continue;
            }
            let cell = self.vad_next;
            let p = self.score_cell(models, spare, out);
            self.vad_cell.clear();
            self.vad_history.push_back(p);
            self.vad_next = cell.next();
            if let Some(p) = p {
                match VadInput::probability(p) {
                    Ok(v) => out.vad(cell, v),
                    Err(_) => {
                        self.vad_failed = true;
                        out.failures
                            .push(format!("VAD returned {p}, not a probability"));
                    }
                }
            }
        }
    }

    fn score_cell(
        &mut self,
        models: &dyn ModelSet,
        spare: &mut Vec<Box<dyn Vad>>,
        out: &mut Produced,
    ) -> Option<f32> {
        if self.vad_failed {
            return None;
        }
        if self.vad.is_none() {
            match spare.pop().map_or_else(|| models.vad(), Ok) {
                Ok(mut v) => {
                    v.reset();
                    self.vad = Some(v);
                    self.vad_reset = false;
                }
                Err(e) => {
                    self.vad_failed = true;
                    out.failures.push(format!("VAD could not be loaded: {e}"));
                    return None;
                }
            }
        }
        let vad = self.vad.as_mut()?;
        if std::mem::take(&mut self.vad_reset) {
            vad.reset();
        }
        let cell: &[f32; CELL] = self.vad_cell.as_slice().try_into().ok()?;
        match vad.process_cell(cell) {
            Ok(p) => Some(p),
            Err(e) => {
                self.vad_failed = true;
                out.failures.push(format!("VAD failed: {e}"));
                None
            }
        }
    }

    /// Feeds the utterance everything retained that it has not read, block by block: a block
    /// ends at the end of a mic packet, or after 1,280 samples within a longer one.
    fn feed(&mut self, models: &dyn ModelSet, pause: &mut dyn FnMut(Sample), out: &mut Produced) {
        let Some(mut u) = self.utterance.take() else {
            return;
        };
        while !u.stopped {
            match self.locate(u.fed) {
                Located::In { index, offset } => {
                    let seg = &self.segments[index];
                    if u.segment.is_some_and(|id| id != seg.id) {
                        u.stopped = true;
                        break;
                    }
                    u.segment = Some(seg.id);
                    // The pre-roll held when the utterance started is one block. After it,
                    // a block ends at a packet end: every retained sample came in a packet
                    // whose end is kept, so the segment's end is only a fallback.
                    let (block_end, room) = if u.fed < u.preroll_end {
                        (u.preroll_end.min(seg.end()), usize::MAX)
                    } else {
                        let packet_end = self
                            .packet_ends
                            .get(self.packet_ends.partition_point(|&e| e <= u.fed))
                            .copied()
                            .unwrap_or_else(|| seg.end());
                        (packet_end, BLOCK - u.block.len())
                    };
                    let take = room
                        .min(seg.pcm.len() - offset)
                        .min((block_end.0 - u.fed.0) as usize);
                    u.block.extend(
                        seg.pcm
                            .range(offset..offset + take)
                            .map(|&s| evidence_sample(s)),
                    );
                    u.fed = Sample(u.fed.0 + take as u64);
                    if u.fed == block_end || (u.block.len() == BLOCK && u.fed >= u.preroll_end) {
                        self.finish_block(models, &mut u, pause, out);
                    }
                }
                Located::Ahead => {
                    // The next audio starts a new segment: the utterance already read the
                    // end of its own.
                    if u.segment.is_some() && !self.open {
                        u.stopped = true;
                    }
                    break;
                }
                Located::Missing => u.stopped = true,
                Located::Dropped => {
                    u.stopped = true;
                    out.failures
                        .push("the utterance's start is no longer retained".to_owned());
                }
            }
        }
        self.utterance = Some(u);
    }

    fn finish_block(
        &mut self,
        models: &dyn ModelSet,
        u: &mut Utterance,
        pause: &mut dyn FnMut(Sample),
        out: &mut Produced,
    ) {
        let through = u.fed;
        let fed = u.stream.accept(&u.block);
        u.block.clear();
        if let Err(e) = fed {
            u.stopped = true;
            out.failures.push(format!("ASR failed: {e}"));
            return;
        }
        u.stream.decode_ready();
        while u.finalizer_next.end() <= through {
            let vad = (u.finalizer_next >= u.finalizer_counts_from)
                .then(|| self.vad_at(u.finalizer_next))
                .flatten();
            u.finalizer.observe_cell(u.finalizer_next, vad);
            u.finalizer_next = u.finalizer_next.next();
        }
        let finalized = if u.finalizer.due() {
            self.retained_span(u.from, through).map(|pcm| {
                pause(through);
                fresh_decode(models, &evidence_copy(&pcm))
            })
        } else {
            None
        };
        let (result, kind) = match finalized {
            Some(r) => (r, AsrKind::Finalized),
            None => (u.stream.result(), AsrKind::Live),
        };
        match result {
            Ok(r) => out.asr.push(AsrObservation::from_recognizer(
                u.from,
                through,
                &r.tokens,
                &r.timestamps,
                r.num_trailing_blanks,
                kind,
            )),
            Err(e) => {
                u.stopped = true;
                out.failures.push(format!("ASR failed: {e}"));
            }
        }
    }
}

impl MicAnalysis {
    pub fn new(models: Arc<dyn ModelSet>) -> Self {
        Self {
            models,
            streams: HashMap::new(),
            spare_vads: Vec::new(),
            labels: Vec::new(),
        }
    }

    /// Applies one job and returns what it produced.
    pub fn run(&mut self, job: MicJob) -> MicOutput {
        self.run_noting_pauses(job, &mut |_| {})
    }

    /// [`MicAnalysis::run`], calling `pause` with a pause decode's end sample just before
    /// that decode starts, so the pause server can be asked about the same audio at the
    /// same moment (spec §4.7).
    pub fn run_noting_pauses(&mut self, job: MicJob, pause: &mut dyn FnMut(Sample)) -> MicOutput {
        let key = match job {
            MicJob::Reset => {
                self.reset();
                return empty(None);
            }
            MicJob::Open { key, plan } => return self.open(key, plan),
            MicJob::Close { key } => {
                self.close(&key);
                return empty(Some(key));
            }
            MicJob::StartUtterance { ref key, .. }
            | MicJob::EndUtterance { ref key }
            | MicJob::Mic { ref key, .. }
            | MicJob::MicMissing { ref key, .. }
            | MicJob::Reference { ref key, .. }
            | MicJob::ReferenceSilence { ref key, .. }
            | MicJob::ReferenceMissing { ref key, .. }
            | MicJob::ReferenceEnded { ref key }
            | MicJob::DeviceCells { ref key, .. }
            | MicJob::CompareCandidate { ref key, .. } => key.clone(),
        };
        let Some(stream) = self.streams.get_mut(&key) else {
            // A job for a stream that is not open (closed, reset or never opened) is stale.
            return empty(Some(key));
        };
        let models = &*self.models;
        let epoch = key.epoch;
        let mut out = Produced::default();
        let echo = |r: Result<(), echomuse_echo::EchoError>, out: &mut Produced| {
            if let Err(e) = r {
                out.failures
                    .push(format!("echo analysis refused input: {e}"));
            }
        };
        match job {
            MicJob::StartUtterance { from, .. } => {
                start_utterance(stream, models, from, &mut out);
            }
            MicJob::EndUtterance { .. } => {
                stream.utterance = None;
                stream.analyzer.stop_labels();
            }
            MicJob::Mic {
                first,
                mono_ns,
                discontinuity,
                pcm,
                ..
            } => {
                let samples: Vec<i16> = pcm.samples().collect();
                let end = Sample(first.0 + samples.len() as u64);
                if end > stream.next_sample {
                    let mut first = first;
                    let mut samples = samples.as_slice();
                    if first < stream.next_sample {
                        let skip = (stream.next_sample.0 - first.0) as usize;
                        samples = &samples[skip..];
                        first = stream.next_sample;
                    } else if first > stream.next_sample {
                        let hole = SampleSpan::new(stream.next_sample, first);
                        echo(stream.analyzer.push_mic_missing(epoch, hole), &mut out);
                        stream.missing(hole.start, hole.end);
                    }
                    if discontinuity {
                        stream.vad_reset = true;
                    }
                    stream.append(first, samples, discontinuity);
                    stream.run_vad(models, &mut self.spare_vads, first, samples, &mut out);
                    echo(
                        stream
                            .analyzer
                            .push_mic(epoch, first, mono_ns, samples.iter().copied()),
                        &mut out,
                    );
                }
            }
            MicJob::MicMissing { span, .. } => {
                echo(stream.analyzer.push_mic_missing(epoch, span), &mut out);
                if span.end > stream.next_sample {
                    let start = span.start.max(stream.next_sample);
                    stream.missing(start, span.end);
                }
            }
            MicJob::Reference {
                reference_epoch,
                first,
                mono_ns,
                coverage,
                pcm,
                ..
            } => echo(
                stream.analyzer.push_reference(
                    epoch,
                    reference_epoch,
                    first,
                    mono_ns,
                    coverage,
                    pcm.samples(),
                ),
                &mut out,
            ),
            MicJob::ReferenceSilence {
                reference_epoch,
                first,
                frames,
                mono_ns,
                coverage,
                ..
            } => echo(
                stream.analyzer.push_reference_silence(
                    epoch,
                    reference_epoch,
                    first,
                    frames,
                    mono_ns,
                    coverage,
                ),
                &mut out,
            ),
            MicJob::ReferenceMissing {
                reference_epoch,
                first,
                end,
                ..
            } => echo(
                stream
                    .analyzer
                    .push_reference_missing(epoch, reference_epoch, first, end),
                &mut out,
            ),
            MicJob::ReferenceEnded { .. } => {
                echo(stream.analyzer.reference_ended(epoch), &mut out);
            }
            MicJob::DeviceCells { first, cells, .. } => {
                echo(
                    stream.analyzer.push_device_cells(epoch, first, &cells),
                    &mut out,
                );
            }
            MicJob::CompareCandidate {
                candidate, support, ..
            } => stream.comparisons.push((candidate, support)),
            MicJob::Reset | MicJob::Open { .. } | MicJob::Close { .. } => {}
        }

        // The VAD this job produced feeds the echo analyzer before labels are polled.
        if let Some(first) = out.vad_first {
            echo(stream.analyzer.push_vad(epoch, first, &out.vad), &mut out);
        }
        stream.feed(models, pause, &mut out);

        self.labels.clear();
        stream.analyzer.poll(&mut self.labels);
        let echo_cells = self.labels.first().map(|l| {
            (
                l.cell,
                self.labels
                    .iter()
                    .map(|l| l.echo)
                    .collect::<Vec<EchoCell>>(),
            )
        });

        let mut comparisons = Vec::new();
        let analyzer = &mut stream.analyzer;
        stream.comparisons.retain(|(candidate, support)| {
            match analyzer.compare_candidate(*support) {
                Some(c) => {
                    comparisons.push((candidate.clone(), c));
                    false
                }
                None => true,
            }
        });

        let alignment = stream.analyzer.alignment();
        let alignment = match alignment {
            Some(a) if Some(a) != stream.alignment => {
                stream.alignment = Some(a);
                Some(a)
            }
            Some(_) => None,
            None => {
                stream.alignment = None;
                None
            }
        };

        MicOutput {
            key: Some(key),
            vad: out.vad_first.map(|first| (first, out.vad)),
            echo: echo_cells,
            asr: out.asr,
            comparisons,
            alignment,
            failure: (!out.failures.is_empty()).then(|| out.failures.join("; ")),
        }
    }

    fn open(&mut self, key: StreamKey, plan: EchoPlan) -> MicOutput {
        self.forget_other_sessions(&key.session);
        self.close(&key);
        let mut analyzer = EchoAnalyzer::new(EchoPolicy::default());
        if let Err(e) = analyzer.open(plan) {
            let mut out = empty(Some(key));
            out.failure = Some(format!("echo analysis refused the plan: {e}"));
            return out;
        }
        self.streams
            .insert(key.clone(), MicStream::new(plan, analyzer));
        empty(Some(key))
    }

    fn forget_other_sessions(&mut self, session: &SessionId) {
        let stale: Vec<StreamKey> = self
            .streams
            .keys()
            .filter(|k| &k.session != session)
            .cloned()
            .collect();
        for k in stale {
            self.close(&k);
        }
    }

    fn close(&mut self, key: &StreamKey) {
        if let Some(stream) = self.streams.remove(key)
            && let Some(mut vad) = stream.vad
        {
            // Its state holds the stream's last audio; nothing of it outlives the stream.
            vad.reset();
            self.spare_vads.push(vad);
        }
    }

    fn reset(&mut self) {
        let keys: Vec<StreamKey> = self.streams.keys().cloned().collect();
        for k in keys {
            self.close(&k);
        }
    }
}

fn start_utterance(
    stream: &mut MicStream,
    models: &dyn ModelSet,
    from: Sample,
    out: &mut Produced,
) {
    stream.utterance = None;
    if from < stream.plan.vad_from.start() {
        out.failures.push(format!(
            "the utterance starts at {} before the subscription's first mic sample {}",
            from.0,
            stream.plan.vad_from.start().0
        ));
        return;
    }
    if let Err(e) = stream.analyzer.start_labels(from.cell()) {
        out.failures
            .push(format!("echo analysis refused the utterance: {e}"));
    }
    match models.asr_stream() {
        Ok(asr) => {
            stream.utterance = Some(Utterance {
                from,
                stream: asr,
                // Speech-positive as the speech policy counts it.
                finalizer: PauseFinalizer::new(AttributionPolicy::default().speech_positive_min),
                finalizer_next: from.cell(),
                finalizer_counts_from: stream.vad_next,
                fed: from,
                preroll_end: stream.next_sample.max(from),
                block: Vec::with_capacity(BLOCK),
                segment: None,
                stopped: false,
            });
        }
        Err(e) => out.failures.push(format!("ASR could not start: {e}")),
    }
}

fn empty(key: Option<StreamKey>) -> MicOutput {
    MicOutput {
        key,
        vad: None,
        echo: None,
        asr: Vec::new(),
        comparisons: Vec::new(),
        alignment: None,
        failure: None,
    }
}

#[cfg(test)]
mod tests {
    use echomuse_echo::OutputCoverage;
    use echomuse_turn::{DeviceCell, DeviceInput, Epoch};
    use echomuse_uplink::SubscriptionId;

    use super::*;
    use crate::speech::testing::{ToneModels, pcm16, silence, utterance};

    const WORDS: [&str; 6] = ["ophelia", "turn", "on", "the", "kitchen", "light"];
    const VAD_FROM: CellIndex = CellIndex(100);
    const START: u64 = 100 * 512;

    fn epoch() -> Epoch {
        Epoch::new(1).unwrap()
    }

    fn key(session: u64) -> StreamKey {
        StreamKey {
            session: SessionId(session),
            subscription: SubscriptionId::generate(),
            epoch: epoch(),
        }
    }

    fn plan() -> EchoPlan {
        EchoPlan {
            epoch: epoch(),
            cells_from: VAD_FROM,
            vad_from: VAD_FROM,
        }
    }

    fn mic(key: &StreamKey, first: u64, pcm: &[i16]) -> MicJob {
        MicJob::Mic {
            key: key.clone(),
            first: Sample(first),
            mono_ns: first * 62_500,
            discontinuity: false,
            pcm: pcm16(pcm),
        }
    }

    /// "turn on", then silence long enough for the pause finalizer.
    fn command(m: &ToneModels) -> Vec<i16> {
        let mut pcm = silence(320);
        pcm.extend(utterance(m, &["turn", "on"], 240, 160, 1000));
        pcm.extend(silence(1200));
        pcm
    }

    fn worker(m: &ToneModels) -> (MicAnalysis, StreamKey) {
        let mut w = MicAnalysis::new(Arc::new(m.clone()));
        let k = key(1);
        let out = w.run(MicJob::Open {
            key: k.clone(),
            plan: plan(),
        });
        assert!(out.failure.is_none());
        (w, k)
    }

    /// Feeds `pcm` from `first` in 80 ms packets.
    fn feed(w: &mut MicAnalysis, k: &StreamKey, first: u64, pcm: &[i16]) -> Vec<MicOutput> {
        pcm.chunks(1_280)
            .enumerate()
            .map(|(i, c)| w.run(mic(k, first + (i * 1_280) as u64, c)))
            .collect()
    }

    /// Every VAD result, checking that each output continues where the last one stopped.
    fn vad_run(outputs: &[MicOutput]) -> (CellIndex, Vec<f32>) {
        let mut first = None;
        let mut all = Vec::new();
        for o in outputs {
            if let Some((c, v)) = &o.vad {
                let start = *first.get_or_insert(*c);
                assert_eq!(
                    c.0,
                    start.0 + all.len() as u64,
                    "VAD output is not contiguous"
                );
                all.extend(v.iter().map(|v| match v {
                    VadInput::Probability(p) => p.get(),
                    VadInput::Unknown => panic!("the worker never reports unknown VAD"),
                }));
            }
        }
        (first.unwrap_or(VAD_FROM), all)
    }

    fn asr(outputs: &[MicOutput]) -> Vec<AsrObservation> {
        outputs.iter().flat_map(|o| o.asr.iter().cloned()).collect()
    }

    fn words(o: &AsrObservation) -> Vec<&str> {
        o.tokens.iter().map(|t| t.text.trim()).collect()
    }

    #[test]
    fn vad_covers_every_cell_from_the_first_in_order() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let pcm = command(&m);
        let outs = feed(&mut w, &k, START, &pcm);
        let (first, vad) = vad_run(&outs);
        assert_eq!(first, VAD_FROM);
        assert_eq!(vad.len(), pcm.len() / 512);
        // 320 ms of silence is ten quiet cells; the first word starts at cell 10.
        assert!(vad[..10].iter().all(|&p| p < 0.5));
        assert!(vad[10..17].iter().all(|&p| p > 0.5));
        assert!(outs.iter().all(|o| o.failure.is_none()));
    }

    #[test]
    fn utterance_yields_a_block_per_80_ms_and_a_finalized_decode_at_the_pause() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let start = w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        assert!(start.asr.is_empty());
        let pcm = command(&m);
        let obs = asr(&feed(&mut w, &k, START, &pcm));
        assert_eq!(obs.len(), pcm.len() / 1_280);
        for (i, o) in obs.iter().enumerate() {
            assert_eq!(o.through, Sample(START + (i as u64 + 1) * 1_280));
        }
        let finalized: Vec<_> = obs
            .iter()
            .filter(|o| o.kind == AsrKind::Finalized)
            .collect();
        assert_eq!(finalized.len(), 1, "one pause, one finalized decode");
        let f = finalized[0];
        assert_eq!(words(f), vec!["turn", "on"]);
        // "on" ends at 320 + 240 + 160 + 240 = 960 ms (cell 30); the finalize is due once
        // VAD has analysed ten cells past it, in the block that completes cell 39.
        assert_eq!(f.through, Sample(START + 16 * 1_280));
        // Token times are on the capture timeline.
        assert!(f.tokens[0].emitted > Sample(START + 560 * 16));
        assert_eq!(words(obs.last().unwrap()), vec!["turn", "on"]);
    }

    #[test]
    fn blocks_end_where_the_packets_end() {
        let m = ToneModels::new(&WORDS);
        let pcm = command(&m);
        // The utterance starts 448 samples into the first packet; a short packet (a live
        // packet sent before it filled) and a packet longer than 80 ms follow.
        let from = START + 448;
        let packets: [(u64, u64); 4] = [(0, 1_280), (1_280, 1_920), (1_920, 5_120), (5_120, 6_400)];
        // Started before any audio, and started once the first two packets are held: the
        // held pre-roll is one block.
        for (held, expected) in [
            (0, vec![1_280, 1_920, 3_200, 4_480, 5_120, 6_400]),
            (2, vec![1_920, 3_200, 4_480, 5_120, 6_400]),
        ] {
            let (mut w, k) = worker(&m);
            let mut outs = Vec::new();
            for (i, (a, b)) in packets.into_iter().enumerate() {
                if i == held {
                    outs.push(w.run(MicJob::StartUtterance {
                        key: k.clone(),
                        from: Sample(from),
                    }));
                }
                outs.push(w.run(mic(&k, START + a, &pcm[a as usize..b as usize])));
            }
            let through: Vec<u64> = asr(&outs).iter().map(|o| o.through.0 - START).collect();
            assert_eq!(through, expected, "packets held at the start: {held}");
        }
    }

    #[test]
    fn a_late_start_reads_the_pre_roll_as_one_block_and_its_vad_never_arms_a_pause() {
        let m = ToneModels::new(&WORDS);
        let pcm = command(&m);

        let (mut live, k) = worker(&m);
        live.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let expected = asr(&feed(&mut live, &k, START, &pcm));
        assert!(expected.iter().any(|o| o.kind == AsrKind::Finalized));

        // "on" ends at 960 ms (15,360 samples): the late start holds all of the command.
        let held = 15_360;
        let (mut late, k) = worker(&m);
        let early = feed(&mut late, &k, START, &pcm[..held]);
        assert!(asr(&early).is_empty());
        let mut outs = vec![late.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        })];
        outs.extend(feed(&mut late, &k, START + held as u64, &pcm[held..]));
        let obs = asr(&outs);
        assert_eq!(obs[0].through, Sample(START + held as u64));
        for (i, o) in obs.iter().enumerate().skip(1) {
            assert_eq!(o.through, Sample(START + (held + i * 1_280) as u64));
        }
        assert!(obs.iter().all(|o| o.kind == AsrKind::Live));
        assert_eq!(obs.last().unwrap().tokens, expected.last().unwrap().tokens);
    }

    #[test]
    fn missing_audio_ends_the_utterance_asr_and_its_cells_get_no_vad() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let pcm = command(&m);
        let before = feed(&mut w, &k, START, &pcm[..6_400]);
        assert_eq!(asr(&before).len(), 5);
        // 1,000 samples go missing, then the audio resumes.
        let gap = w.run(MicJob::MicMissing {
            key: k.clone(),
            span: SampleSpan::new(Sample(START + 6_400), Sample(START + 7_400)),
        });
        assert!(gap.vad.is_none() && gap.asr.is_empty());
        let after = feed(&mut w, &k, START + 7_400, &pcm[7_400..]);
        assert!(asr(&after).is_empty(), "no ASR across a gap");
        // Cells 12 and 13 touch the gap; VAD resumes at the first whole cell after it.
        let (first, _) = vad_run(&after);
        assert_eq!(first, CellIndex(100 + 15));
        assert!(after.iter().all(|o| o.failure.is_none()));
    }

    #[test]
    fn a_packet_that_skips_ahead_is_a_gap() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let pcm = command(&m);
        feed(&mut w, &k, START, &pcm[..2_560]);
        let later = feed(&mut w, &k, START + 5_120, &pcm[5_120..]);
        assert!(asr(&later).is_empty());
        assert_eq!(vad_run(&later).0, CellIndex(110));
    }

    #[test]
    fn overlapping_audio_is_analysed_once() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let pcm = command(&m);
        let mut outs = feed(&mut w, &k, START, &pcm[..2_560]);
        outs.extend(feed(&mut w, &k, START + 1_280, &pcm[1_280..5_120]));
        let (_, vad) = vad_run(&outs);
        assert_eq!(vad.len(), 10);
    }

    #[test]
    fn a_comparison_is_answered_once_its_inputs_arrive() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let asked = w.run(MicJob::CompareCandidate {
            key: k.clone(),
            candidate: "c1".into(),
            support: SampleSpan::new(Sample(START), Sample(START + 4_096)),
        });
        assert!(asked.comparisons.is_empty());
        w.run(MicJob::ReferenceEnded { key: k.clone() });
        let pcm = command(&m);
        let cells: Vec<DeviceInput> = pcm
            .as_chunks::<512>()
            .0
            .iter()
            .map(|c| DeviceInput::Cell(DeviceCell::audio(DeviceCell::loudness_centi(c), 4)))
            .collect();
        w.run(MicJob::DeviceCells {
            key: k.clone(),
            first: VAD_FROM,
            cells,
        });
        let outs = feed(&mut w, &k, START, &pcm);
        let answered: Vec<_> = outs.iter().flat_map(|o| o.comparisons.iter()).collect();
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].0, "c1");
        // It is answered in the output where the support's last cell arrived.
        let at = outs.iter().position(|o| !o.comparisons.is_empty()).unwrap();
        assert_eq!(at, 3);
    }

    #[test]
    fn echo_labels_are_in_cell_order_and_the_alignment_is_published() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let r_epoch = Epoch::new(7).unwrap();
        // Silence from a second before the mic (any lag of the search reaches back
        // 500 ms) to well after it.
        let reference = w.run(MicJob::ReferenceSilence {
            key: k.clone(),
            reference_epoch: r_epoch,
            first: START - 16_000,
            frames: 96_000,
            mono_ns: (START - 16_000) * 62_500,
            coverage: OutputCoverage::Full,
        });
        assert!(reference.alignment.is_none(), "no mic clock yet");
        w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let outs = feed(&mut w, &k, START, &command(&m));
        let mut next = VAD_FROM;
        let mut labelled = 0;
        for o in &outs {
            if let Some((c, labels)) = &o.echo {
                assert_eq!(*c, next);
                assert!(labels.iter().all(|l| *l == EchoCell::SILENT_REFERENCE));
                next = CellIndex(c.0 + labels.len() as u64);
                labelled += labels.len();
            }
        }
        assert!(labelled > 50);
        let first = outs.iter().find_map(|o| o.alignment).unwrap();
        assert_eq!(first.reference_epoch, r_epoch);
        // Repeated only when it changes.
        let published: Vec<_> = outs.iter().filter_map(|o| o.alignment).collect();
        assert!(published.windows(2).all(|p| p[0] != p[1]));
    }

    #[test]
    fn reset_forgets_every_stream() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let pcm = command(&m);
        feed(&mut w, &k, START, &pcm[..5_120]);
        let reset = w.run(MicJob::Reset);
        assert!(reset.key.is_none());
        let start = w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        assert!(start.asr.is_empty() && start.failure.is_none());
        let more = feed(&mut w, &k, START + 5_120, &pcm[5_120..]);
        assert!(more.iter().all(|o| o.vad.is_none() && o.asr.is_empty()));
    }

    #[test]
    fn opening_a_newer_session_forgets_older_streams() {
        let m = ToneModels::new(&WORDS);
        let (mut w, old) = worker(&m);
        let pcm = command(&m);
        feed(&mut w, &old, START, &pcm[..2_560]);
        let new = key(2);
        w.run(MicJob::Open {
            key: new.clone(),
            plan: plan(),
        });
        let stale = w.run(mic(&old, START + 2_560, &pcm[2_560..3_840]));
        assert_eq!(stale.key, Some(old));
        assert!(stale.vad.is_none());
        let fresh = feed(&mut w, &new, START, &pcm[..2_560]);
        assert_eq!(vad_run(&fresh).1.len(), 5);
    }

    #[test]
    fn a_start_older_than_the_retained_mic_is_a_failure() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        let quiet = silence(42_000);
        feed(&mut w, &k, START, &quiet);
        let out = w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        assert!(out.asr.is_empty());
        assert!(out.failure.unwrap().contains("no longer retained"));
    }

    #[test]
    fn a_vad_failure_stops_vad_and_is_reported() {
        let m = ToneModels::new(&WORDS).failing_vad();
        let (mut w, k) = worker(&m);
        let outs = feed(&mut w, &k, START, &command(&m)[..5_120]);
        assert!(outs.iter().all(|o| o.vad.is_none()));
        assert!(outs[0].failure.as_deref().unwrap().contains("VAD failed"));
        assert!(outs[1..].iter().all(|o| o.failure.is_none()));
    }

    #[test]
    fn an_asr_failure_stops_the_utterance_and_is_reported() {
        let m = ToneModels::new(&WORDS).failing_asr();
        let (mut w, k) = worker(&m);
        w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let outs = feed(&mut w, &k, START, &command(&m));
        assert!(asr(&outs).is_empty());
        assert!(outs[0].failure.as_deref().unwrap().contains("ASR failed"));
        assert!(outs[1..].iter().all(|o| o.failure.is_none()));
        // VAD is unaffected.
        assert!(!vad_run(&outs).1.is_empty());
    }

    #[test]
    fn ending_the_utterance_stops_observations() {
        let m = ToneModels::new(&WORDS);
        let (mut w, k) = worker(&m);
        w.run(MicJob::StartUtterance {
            key: k.clone(),
            from: Sample(START),
        });
        let pcm = command(&m);
        assert_eq!(asr(&feed(&mut w, &k, START, &pcm[..2_560])).len(), 2);
        w.run(MicJob::EndUtterance { key: k.clone() });
        let rest = feed(&mut w, &k, START + 2_560, &pcm[2_560..]);
        assert!(asr(&rest).is_empty());
        assert!(!vad_run(&rest).1.is_empty());
    }
}
