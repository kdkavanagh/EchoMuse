//! Synthetic fixtures for the analyzer and the reference wake scorer (Rust controller
//! evaluation §6.1). No recorded reference audio exists, so every fixture is built from
//! deterministic synthetic signals; each states its levels and tolerances.
//!
//! The common scene: the microphone stream starts at capture sample 51,200 (cell 100), after
//! 100 device loudness cells of room noise at −50 dB that give `B` = −50 dB. The device's
//! reference epoch began at capture sample 30,000, so reference sample `r` was played at
//! microphone sample `r + 30,000` in the clock estimate; the microphone hears it `lag` samples
//! later still. Reference is speech-like noise at −20 dBFS; the echo path is a 20 dB loss with a
//! loudspeaker-like low-pass (taps 0.06, 0.03, 0.01: 0.5 samples of group delay), so the echo
//! sits near −40 dBFS, 10 dB above `B`, as a native-AEC residual would; the microphone adds
//! white noise at −70 dBFS. VAD is 0.9 throughout, as echoed speech would score.

use echomuse_speech::WindowScore;
use echomuse_turn::{
    ArrivalConditions, CandidateEcho, CellIndex, DeviceCell, DeviceInput, EchoCell, EchoLabel,
    Epoch, MonoInstant, ReferenceCoverage, ReferenceScore, Sample, SampleSpan, VadInput,
    VerifierResult, WakeCandidate, WakeDecider, WakeHop, WakeOutcome, WakePolicy, WakeProfile,
    WakeRefusal,
};

use crate::synth;
use crate::*;

const MIC_EPOCH: u64 = 7;
const REF_EPOCH: u64 = 9;
const T0: u64 = 5_000_000_000;
/// Reference sample `r` plays at microphone sample `r + OFFSET`.
const OFFSET: u64 = 30_000;
const MIC_START_CELL: u64 = 100;
const MIC_START: u64 = MIC_START_CELL * 512;
/// Microphone chunks: two cells, so every chunk carries whole device cells and VAD results.
const MIC_CHUNK: u64 = 1_024;
/// Reference chunks: one 80 ms capture block.
const REF_CHUNK: u64 = 1_280;

fn mic_epoch() -> Epoch {
    Epoch::new(MIC_EPOCH).unwrap()
}

fn ref_epoch() -> Epoch {
    Epoch::new(REF_EPOCH).unwrap()
}

fn mono(mic_sample: u64) -> u64 {
    T0 + mic_sample * 62_500
}

#[derive(Clone)]
struct Scene {
    /// Microphone samples from `MIC_START`.
    mic: Vec<f32>,
    /// Missing microphone ranges, in capture samples.
    mic_missing: Vec<(u64, u64)>,
    /// Reference samples from reference sample 0.
    reference: Vec<f32>,
    /// Missing reference ranges, in reference samples.
    ref_missing: Vec<(u64, u64)>,
    coverage: OutputCoverage,
    /// VAD per microphone cell.
    vad: Vec<f32>,
    /// Device loudness cells before the microphone (for `B`).
    history_cells: u64,
}

impl Scene {
    /// `seconds` of microphone audio that hears the reference through the echo path `lag`
    /// samples after the clock estimate.
    fn echo(lag: u64, seconds: u64) -> Self {
        // A whole number of cells.
        let n = (seconds * 16_000 / 512 * 512) as usize;
        let ref_len = (MIC_START + n as u64 - OFFSET) as usize;
        let reference = synth::speech_like(ref_len, 21, -20.0);
        let echoed = synth::filter(&reference, &[0.06, 0.03, 0.01]);
        let noise = synth::white(n, 4, -70.0);
        let mic = (0..n)
            .map(|i| {
                let r = (MIC_START + i as u64) as i64 - OFFSET as i64 - lag as i64;
                let e = if r >= 0 { echoed[r as usize] } else { 0.0 };
                e + noise[i]
            })
            .collect();
        Self {
            mic,
            mic_missing: Vec::new(),
            reference,
            ref_missing: Vec::new(),
            coverage: OutputCoverage::Full,
            vad: vec![0.9; n / 512],
            history_cells: MIC_START_CELL,
        }
    }

    /// Adds a near-end talker over microphone cells `[from, to)`, `snr_db` relative to the
    /// echo's −40.5 dBFS.
    fn with_talker(mut self, from: u64, to: u64, snr_db: f64) -> Self {
        let a = ((from - MIC_START_CELL) * 512) as usize;
        let b = ((to - MIC_START_CELL) * 512) as usize;
        let talker = synth::speech_like(b - a, 500, -40.5 + snr_db);
        for (m, t) in self.mic[a..b].iter_mut().zip(talker) {
            *m += t;
        }
        self
    }

    fn mic_cells(&self) -> u64 {
        self.mic.len() as u64 / 512
    }

    fn last_cell(&self) -> CellIndex {
        CellIndex(MIC_START_CELL + self.mic_cells() - 1)
    }

    fn plan(&self) -> EchoPlan {
        EchoPlan {
            epoch: mic_epoch(),
            cells_from: CellIndex(MIC_START_CELL - self.history_cells),
            vad_from: CellIndex(MIC_START_CELL),
        }
    }

    fn mic_is_missing(&self, from: u64, to: u64) -> bool {
        self.mic_missing.iter().any(|&(a, b)| from < b && a < to)
    }

    /// Pushes the microphone chunk `[from, to)` with its device cells and VAD.
    fn feed_mic(&self, a: &mut EchoAnalyzer, from: u64, to: u64) {
        let e = mic_epoch();
        if self.mic_is_missing(from, to) {
            a.push_mic_missing(e, SampleSpan::new(Sample(from), Sample(to)))
                .unwrap();
            let gap = DeviceCell::from_wire(-12_000, 1, 0).unwrap();
            let cells: Vec<DeviceInput> = (from / 512..to / 512)
                .map(|_| DeviceInput::Cell(gap))
                .collect();
            a.push_device_cells(e, CellIndex(from / 512), &cells)
                .unwrap();
            return;
        }
        let i = (from - MIC_START) as usize;
        let pcm = synth::pcm(&self.mic[i..i + (to - from) as usize]);
        a.push_mic(e, Sample(from), mono(from), pcm.iter().copied())
            .unwrap();
        let cells: Vec<DeviceInput> = pcm
            .chunks(512)
            .map(|c| DeviceInput::Cell(DeviceCell::audio(DeviceCell::loudness_centi(c), 4)))
            .collect();
        a.push_device_cells(e, CellIndex(from / 512), &cells)
            .unwrap();
        let first_cell = (from - MIC_START) / 512;
        let vad: Vec<VadInput> = (first_cell..first_cell + (to - from) / 512)
            .map(|c| VadInput::probability(self.vad[c as usize]).unwrap())
            .collect();
        a.push_vad(e, CellIndex(from / 512), &vad).unwrap();
    }

    /// Pushes the reference chunk `[from, to)` (reference samples).
    fn feed_reference(&self, a: &mut EchoAnalyzer, from: u64, to: u64) {
        let to = to.min(self.reference.len() as u64);
        if from >= to {
            return;
        }
        if self.ref_missing.iter().any(|&(x, y)| from < y && x < to) {
            a.push_reference_missing(mic_epoch(), ref_epoch(), from, to)
                .unwrap();
            return;
        }
        let pcm = synth::pcm(&self.reference[from as usize..to as usize]);
        if pcm.iter().all(|&s| s == 0) {
            a.push_reference_silence(
                mic_epoch(),
                ref_epoch(),
                from,
                to - from,
                mono(from + OFFSET),
                self.coverage,
            )
            .unwrap();
        } else {
            a.push_reference(
                mic_epoch(),
                ref_epoch(),
                from,
                mono(from + OFFSET),
                self.coverage,
                pcm,
            )
            .unwrap();
        }
    }

    fn open(&self, a: &mut EchoAnalyzer) {
        a.open(self.plan()).unwrap();
        a.start_labels(CellIndex(MIC_START_CELL)).unwrap();
        let history: Vec<DeviceInput> = (0..self.history_cells)
            .map(|_| DeviceInput::Cell(DeviceCell::audio(-5_000, 0)))
            .collect();
        a.push_device_cells(mic_epoch(), self.plan().cells_from, &history)
            .unwrap();
    }

    /// Feeds everything, stream by stream as a backfill delivers it: microphone with its cells
    /// and VAD, then the reference.
    fn feed_all(&self, a: &mut EchoAnalyzer) {
        let end = MIC_START + self.mic.len() as u64;
        let mut m = MIC_START;
        while m < end {
            self.feed_mic(a, m, (m + MIC_CHUNK).min(end));
            m += MIC_CHUNK;
        }
        let mut r = 0;
        while r < self.reference.len() as u64 {
            self.feed_reference(a, r, r + REF_CHUNK);
            r += REF_CHUNK;
        }
    }

    /// A fresh analyzer fed everything as a backfill, polled once at the end.
    fn run_backfill(&self) -> (EchoAnalyzer, Vec<CellLabel>) {
        let mut a = EchoAnalyzer::new(EchoPolicy::default());
        self.open(&mut a);
        self.feed_all(&mut a);
        let mut out = Vec::new();
        a.poll(&mut out);
        (a, out)
    }

    /// Feeds everything chunk by chunk in time order, reference before microphone, polling
    /// after every chunk as a live worker would.
    fn run_live(&self) -> (EchoAnalyzer, Vec<CellLabel>) {
        let mut a = EchoAnalyzer::new(EchoPolicy::default());
        self.open(&mut a);
        let mut out = Vec::new();
        let end = MIC_START + self.mic.len() as u64;
        // Reference that played before the microphone started.
        let mut r = 0;
        while r + OFFSET < MIC_START {
            let to = (r + REF_CHUNK).min(MIC_START - OFFSET);
            self.feed_reference(&mut a, r, to);
            r = to;
        }
        let mut m = MIC_START;
        while m < end {
            let to = (m + MIC_CHUNK).min(end);
            self.feed_reference(&mut a, m - OFFSET, to - OFFSET);
            a.poll(&mut out);
            self.feed_mic(&mut a, m, to);
            a.poll(&mut out);
            m = to;
        }
        (a, out)
    }
}

fn labels_of(out: &[CellLabel]) -> Vec<EchoLabel> {
    out.iter().map(|l| l.echo.label).collect()
}

/// Echo alone: once the first lag estimate is in, every cell is `echo_only` with full
/// coverage, and the lag is the true delay to within ±1 sample (the path's 0.5-sample group
/// delay), across the search range.
#[test]
fn delayed_filtered_echo_is_echo_only() {
    for lag in [300, 2_400, 7_000] {
        let scene = Scene::echo(lag, 3);
        let (a, out) = scene.run_backfill();
        assert_eq!(
            out.len() as u64,
            scene.mic_cells(),
            "lag {lag}: every cell labelled"
        );
        assert_eq!(out[0].cell, CellIndex(MIC_START_CELL));
        assert_eq!(out.last().unwrap().cell, scene.last_cell());
        for l in &out {
            assert_eq!(
                l.echo,
                EchoCell {
                    label: EchoLabel::EchoOnly,
                    coverage: ReferenceCoverage::Full
                },
                "lag {lag}, cell {:?}: {:?}",
                l.cell,
                l.undecided
            );
            assert!(l.lag.unwrap().0.abs_diff(lag as u32) <= 1, "{l:?}");
        }
        let established = a.lag().unwrap();
        assert!(established.0.abs_diff(lag as u32) <= 1);
        // Two estimates ran: at 1 s and 2 s of microphone audio (2.976 s in all).
        assert_eq!(a.estimates().count(), 2);

        // The candidate comparison over a 1.7 s support says the same.
        let support = SampleSpan::new(Sample(MIC_START + 4_800), Sample(MIC_START + 32_000));
        let c = a_compare(&scene, support);
        assert_eq!(c.echo, CandidateEcho::EchoOnly, "lag {lag}: {c:?}");
        assert!(c.lag.unwrap().lag().unwrap().0.abs_diff(lag as u32) <= 1);
        assert_eq!(c.coverage, ReferenceCoverage::Full);
    }
}

fn a_compare(scene: &Scene, support: SampleSpan) -> CandidateComparison {
    let (mut a, _) = scene.run_backfill();
    a.compare_candidate(support).unwrap()
}

/// A near-end talker over the echo at 0 dB and +6 dB is `near_end_present` in every cell whose
/// six-cell window lies inside the talk; cells before it stay `echo_only`. At −20 dB the talker
/// is 10 dB under `B` + 6 dB and only 5% of the cell energy: the echo still explains the cells,
/// which is the comparator's documented limit, not double talk.
#[test]
fn double_talk_is_near_end_present() {
    for snr in [0.0, 6.0] {
        let scene = Scene::echo(2_400, 3).with_talker(140, 165, snr);
        let (_, out) = scene.run_backfill();
        for l in &out {
            let c = l.cell.0;
            if c < 140 {
                assert_eq!(l.echo.label, EchoLabel::EchoOnly, "snr {snr}, cell {c}");
            } else if (145..165).contains(&c) {
                assert_eq!(
                    l.echo.label,
                    EchoLabel::NearEndPresent,
                    "snr {snr}, cell {c}: {:?}",
                    l.undecided
                );
            } else if c >= 170 {
                assert_eq!(l.echo.label, EchoLabel::EchoOnly, "snr {snr}, cell {c}");
            }
        }
        let support = SampleSpan::new(Sample(MIC_START + 4_800), Sample(170 * 512));
        assert_eq!(
            a_compare(&scene, support).echo,
            CandidateEcho::NearEndPresent,
            "snr {snr}"
        );
    }
    let scene = Scene::echo(2_400, 3).with_talker(140, 165, -20.0);
    let (_, out) = scene.run_backfill();
    assert!(
        labels_of(&out).iter().all(|l| *l == EchoLabel::EchoOnly),
        "{:?}",
        out.iter()
            .filter(|l| l.echo.label != EchoLabel::EchoOnly)
            .collect::<Vec<_>>()
    );
}

/// Digital silence on the reference is `no_reference` with silent coverage for every cell,
/// whatever the microphone hears. Nothing is computed for it: no lag estimate even runs, so the
/// labels do not wait for one.
#[test]
fn digital_silence_is_no_reference() {
    let mut scene = Scene::echo(2_400, 2);
    scene.mic = synth::speech_like(scene.mic.len(), 8, -30.0);
    scene.reference.iter_mut().for_each(|s| *s = 0.0);
    let (mut a, out) = scene.run_backfill();
    assert_eq!(out.len() as u64, scene.mic_cells());
    assert!(
        out.iter()
            .all(|l| l.echo == EchoCell::SILENT_REFERENCE && l.lag.is_none())
    );
    assert_eq!(a.estimates().count(), 0);
    assert_eq!(a.lag(), None);
    let c = a
        .compare_candidate(SampleSpan::new(
            Sample(MIC_START),
            Sample(MIC_START + 24_000),
        ))
        .unwrap();
    assert_eq!(
        (c.echo, c.coverage),
        (CandidateEcho::Unknown, ReferenceCoverage::Silent)
    );

    // Reference hiss at −65 dBFS is under the −60 dBFS line: still no reference. At −55 dBFS it
    // is above it, and the cells are compared (here the microphone does not hear it: unknown).
    for (db, silent) in [(-65.0, true), (-55.0, false)] {
        let mut scene = Scene::echo(2_400, 2);
        scene.mic = synth::speech_like(scene.mic.len(), 8, -30.0);
        scene.reference = synth::white(scene.reference.len(), 12, db);
        let (_, out) = scene.run_backfill();
        assert_eq!(
            out.iter().all(|l| l.echo == EchoCell::SILENT_REFERENCE),
            silent,
            "{db} dBFS"
        );
        if !silent {
            assert!(out.iter().all(|l| l.echo.label == EchoLabel::Unknown));
        }
    }
}

/// A reference gap is unknown with missing coverage for every cell whose window needs it, and
/// a reference that never arrives at all is unknown everywhere once the runtime says it has
/// ended: never silence.
#[test]
fn missing_reference_is_unknown() {
    let mut scene = Scene::echo(2_400, 3);
    // Reference samples 39,680–40,960: microphone samples 72,080–73,360 at lag 2,400.
    scene.ref_missing.push((39_680, 40_960));
    let (_, out) = scene.run_backfill();
    assert_eq!(out.len() as u64, scene.mic_cells());
    for l in &out {
        let start = l.cell.0 * 512;
        // The window (six cells back, ±2 samples) touches the gap.
        let touches = start + 512 > 72_080 - 2 && start.saturating_sub(5 * 512) < 73_360 + 2;
        if touches {
            assert_eq!(l.echo, EchoCell::MISSING, "{:?}", l.cell);
        } else if l.cell.0 > 160 || l.cell.0 < 135 {
            assert_eq!(l.echo.label, EchoLabel::EchoOnly, "{:?}", l.cell);
        }
    }

    // No reference ever: the cells wait, then are missing once the reference has ended.
    let mut scene = Scene::echo(2_400, 2);
    scene.reference.clear();
    let (mut a, out) = scene.run_backfill();
    assert!(out.is_empty(), "labels wait for the reference");
    a.reference_ended(mic_epoch()).unwrap();
    let mut out = Vec::new();
    a.poll(&mut out);
    assert_eq!(out.len() as u64, scene.mic_cells());
    assert!(out.iter().all(|l| l.echo == EchoCell::MISSING));
    assert!(
        a.estimates()
            .all(|e| e.outcome == LagOutcome::Unknown(LagUnknown::ReferenceMissing))
    );
}

/// A microphone gap leaves its windows with too few valid cells: unknown, not echo.
#[test]
fn microphone_gaps_are_unknown() {
    let mut scene = Scene::echo(2_400, 3);
    // Cells 150–153 (four of them, two microphone chunks) are missing.
    scene.mic_missing.push((150 * 512, 154 * 512));
    let (_, out) = scene.run_backfill();
    for l in &out {
        let c = l.cell.0;
        // A window holds 6 cells; with 2 of them missing only 4 are valid (67% < 80%).
        let missing_in_window = (c.saturating_sub(5)..=c)
            .filter(|x| (150..154).contains(x))
            .count();
        if missing_in_window >= 2 {
            assert_eq!(
                (l.echo.label, l.undecided),
                (EchoLabel::Unknown, Some(Undecided::TooFewValid)),
                "cell {c}"
            );
        } else {
            assert_eq!(l.echo.label, EchoLabel::EchoOnly, "cell {c}");
        }
    }
}

/// Output that bypassed the reference makes every non-silent cell unknown with partial
/// coverage.
#[test]
fn partial_coverage_is_unknown() {
    let mut scene = Scene::echo(2_400, 2);
    scene.coverage = OutputCoverage::Partial;
    let (_, out) = scene.run_backfill();
    assert!(out.iter().all(|l| l.echo
        == EchoCell {
            label: EchoLabel::Unknown,
            coverage: ReferenceCoverage::Partial
        }));
}

/// A microphone that does not hear the reference gives no lag, so its cells are unknown.
#[test]
fn unrelated_sound_has_no_lag() {
    let mut scene = Scene::echo(2_400, 2);
    scene.mic = synth::speech_like(scene.mic.len(), 31, -30.0);
    let (a, out) = scene.run_backfill();
    assert!(a.estimates().all(|e| matches!(
        e.outcome,
        LagOutcome::Unknown(LagUnknown::NotDistinct { .. })
    )));
    assert!(out.iter().all(|l| l.echo
        == EchoCell {
            label: EchoLabel::Unknown,
            coverage: ReferenceCoverage::Full
        }
        && l.undecided == Some(Undecided::NoLag)));
}

/// Without 1 s of room history `B` never becomes available (every microphone cell is speech),
/// and a comparison without `B` is unknown.
#[test]
fn missing_background_is_unknown() {
    let mut scene = Scene::echo(2_400, 2);
    scene.history_cells = 0;
    let (_, out) = scene.run_backfill();
    assert_eq!(out.len() as u64, scene.mic_cells());
    assert!(out.iter().all(|l| l.echo.label == EchoLabel::Unknown
        && l.undecided == Some(Undecided::BackgroundUnavailable)));
}

/// The labels depend only on the inputs: a backfill delivered stream by stream and the same
/// audio delivered live, chunk by chunk with polls in between, give identical labels.
#[test]
fn labels_do_not_depend_on_arrival_order() {
    let scene = Scene::echo(2_400, 3).with_talker(140, 165, 0.0);
    let (_, backfill) = scene.run_backfill();
    let (_, live) = scene.run_live();
    assert_eq!(backfill, live);
}

/// Reset, a new epoch and a reference epoch change each forget what they must.
#[test]
fn reset_semantics() {
    let scene = Scene::echo(2_400, 2);
    let (mut a, out) = scene.run_backfill();
    assert!(!out.is_empty() && a.lag().is_some());
    assert!(a.alignment().is_some());

    // A reference epoch change (the device restarted its render sink) drops the lag and the
    // old reference.
    a.push_reference(
        mic_epoch(),
        Epoch::new(10).unwrap(),
        0,
        mono(90_000),
        OutputCoverage::Full,
        [100i16; 1_280],
    )
    .unwrap();
    assert_eq!(a.lag(), None);
    assert_eq!(
        a.alignment().unwrap().reference_epoch,
        Epoch::new(10).unwrap()
    );

    // Mute: everything is forgotten and nothing is accepted until a plan opens again.
    a.reset();
    assert_eq!(a.plan(), None);
    assert_eq!(
        a.push_mic(mic_epoch(), Sample(MIC_START), mono(MIC_START), [0i16; 512]),
        Err(EchoError::NotOpen)
    );
    let mut out = Vec::new();
    a.poll(&mut out);
    assert!(out.is_empty());

    // A new capture epoch refuses the old one's input and starts with no lag or labels.
    let new_epoch = Epoch::new(8).unwrap();
    a.open(EchoPlan {
        epoch: new_epoch,
        cells_from: CellIndex(0),
        vad_from: CellIndex(0),
    })
    .unwrap();
    assert_eq!(
        a.push_vad(mic_epoch(), CellIndex(0), &[VadInput::Unknown]),
        Err(EchoError::ForeignEpoch {
            expected: 8,
            got: MIC_EPOCH
        })
    );
    assert_eq!(a.lag(), None);
    assert_eq!(a.estimates().count(), 0);
    assert!(a.alignment().is_none());

    // The same scene through the reused analyzer gives the same labels as a fresh one.
    let mut b = a;
    scene.open(&mut b);
    scene.feed_all(&mut b);
    let mut again = Vec::new();
    b.poll(&mut again);
    assert_eq!(again, scene.run_backfill().1);

    // Stopping labels stops them.
    b.stop_labels();
    assert_eq!(b.lag(), None);
}

// ---- Reference wake scoring ----

/// A stand-in for the wake graph: 0.95 when a loud marker (|x| ≥ 0.5) lies in the window's last
/// 0.8 s, else 0.02; below the model's RMS floor it does not score. It counts its runs.
struct MarkerScorer {
    runs: usize,
}

#[derive(Debug, thiserror::Error)]
#[error("inference failed")]
struct Failed;

impl WakeWindowScorer for MarkerScorer {
    type Error = Failed;
    fn score_window(&mut self, pcm: &[i16]) -> Result<WindowScore, Failed> {
        let ms = pcm
            .iter()
            .map(|&s| (f64::from(s) / 32_768.0).powi(2))
            .sum::<f64>()
            / pcm.len() as f64;
        if ms.sqrt() < 1e-4 {
            return Ok(WindowScore::BelowRmsFloor);
        }
        self.runs += 1;
        let marker = pcm[pcm.len() - 12_800..]
            .iter()
            .any(|&s| s.unsigned_abs() >= 16_384);
        Ok(WindowScore::Scored {
            probability: if marker { 0.95 } else { 0.02 },
        })
    }
}

/// Scripted raw scores, one per call.
struct Script(std::collections::VecDeque<f64>);

impl WakeWindowScorer for Script {
    type Error = Failed;
    fn score_window(&mut self, _: &[i16]) -> Result<WindowScore, Failed> {
        self.0
            .pop_front()
            .map(|p| WindowScore::Scored { probability: p })
            .ok_or(Failed)
    }
}

const LAG: u32 = 2_400;

/// The alignment of the common scene with the lag established.
fn alignment() -> Alignment {
    Alignment {
        mic_epoch: mic_epoch(),
        reference_epoch: ref_epoch(),
        mic: ClockFit {
            ns_ref: T0,
            position_at_ref: 0.0,
            samples_per_ns: 1.0 / 62_500.0,
        },
        reference: ClockFit {
            ns_ref: T0 + OFFSET * 62_500,
            position_at_ref: 0.0,
            samples_per_ns: 1.0 / 62_500.0,
        },
        lag: Some(Lag(LAG)),
    }
}

/// Reference: quiet speech at −40 dBFS, with a loud wake-like chirp at reference samples
/// 40,000–48,000.
fn wake_reference(len: usize) -> Vec<i16> {
    let mut x = synth::speech_like(len, 3, -40.0);
    let chirp = synth::chirp(8_000, 300.0, 2_000.0, -6.0);
    x[40_000..48_000].copy_from_slice(&chirp);
    synth::pcm(&x)
}

fn feed(s: &mut ReferenceWakeScorer<impl WakeWindowScorer>, pcm: &[i16], from: usize) {
    let mut r = from;
    while r < pcm.len() {
        let to = (r + REF_CHUNK as usize).min(pcm.len());
        s.push_pcm(ref_epoch(), r as u64, pcm[r..to].iter().copied());
        s.advance().unwrap();
        r = to;
    }
}

/// The microphone span of a candidate opening `open` samples into the microphone timeline.
fn mic_support(start: u64, open: u64) -> SampleSpan {
    SampleSpan::new(Sample(start), Sample(open))
}

#[test]
fn hops_follow_the_reference_grid() {
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    let pcm = wake_reference(60_000);
    // The subscription's reference starts at 2,560: the first full window ends at 25,600.
    feed(&mut s, &pcm, 2_560);
    let ends: Vec<u64> = s.hops().map(|h| h.end).collect();
    assert_eq!(ends[0], 25_600);
    assert!(ends.windows(2).all(|w| w[1] - w[0] == 2_560));
    assert_eq!(*ends.last().unwrap(), 58_880);
}

#[test]
fn candidate_rules_match_the_device() {
    let scores = [0.1, 0.5, 0.2, 0.1, 0.0, 0.0, 0.95];
    let mut s = ReferenceWakeScorer::new(
        Script(scores.into_iter().collect()),
        ReferenceWakePolicy::default(),
    );
    let noise = synth::pcm(&synth::white(23_040 + 6 * 2_560, 1, -30.0));
    feed(&mut s, &noise, 0);
    let c: Vec<_> = s.candidates().copied().collect();
    // Hop 2 (end 25,600): mean(0.1, 0.5) = 0.30 opens, its support from the first window.
    // Smoothed then 0.267, 0.267, 0.10 (below 0.15 once), 0.033 (twice): closed at 35,840.
    // Hop 7: mean(0, 0, 0.95) = 0.32 opens a second one.
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].first_crossing_end, 25_600);
    assert_eq!(c[0].support_start, 23_040 - 22_400);
    assert_eq!(c[0].support_end_at_open, 25_600);
    assert_eq!(c[0].closed_at, Some(35_840));
    assert_eq!(c[1].first_crossing_end, 38_400);
    assert_eq!(c[1].support_start, 33_280 - 22_400);
}

/// The device's own wake-like output overlapping the microphone candidate is `Overlap`; the
/// wake decision then refuses as `self_output`, unless the echo comparison found double talk.
#[test]
fn reference_wake_feeds_the_wake_decision() {
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    let pcm = wake_reference(100_000);
    feed(&mut s, &pcm, 0);
    let echo = EchoPolicy::default();
    let a = alignment();
    // The chirp plays at microphone 70,000–78,000 and is heard 2,400 samples later. A
    // microphone candidate on it: support 58,000, open 82,000.
    let j = s.judge(mic_support(58_000, 82_000), &a, &echo).unwrap();
    assert_eq!(j.score, ReferenceScore::Overlap, "{j:?}");
    // A microphone candidate two seconds later: the reference had nothing wake-like there.
    let j = s.judge(mic_support(100_000, 124_000), &a, &echo).unwrap();
    assert_eq!(j.score, ReferenceScore::NoOverlap, "{j:?}");
    // Without an established lag the whole 0–500 ms is allowed: still overlapping.
    let unlagged = Alignment { lag: None, ..a };
    let j = s
        .judge(mic_support(64_000, 82_000), &unlagged, &echo)
        .unwrap();
    assert_eq!(j.score, ReferenceScore::Overlap);

    // Into the speech policy's wake decision.
    let candidate = WakeCandidate {
        candidate_id: "c1".into(),
        profile: WakeProfile::Playback,
        threshold: 0.65,
        producing_sound: true,
        first_crossing_end: Sample(82_000),
        support_start: Sample(58_000),
        hops: vec![WakeHop {
            end: Sample(82_000),
            raw: Some(0.9),
            smoothed: Some(0.7),
        }],
    };
    let ms = |n: u64| MonoInstant(n * 1_000_000);
    let reference = s.judge(mic_support(58_000, 82_000), &a, &echo).unwrap();
    let mut d = WakeDecider::arrive(
        candidate.clone(),
        ArrivalConditions::default(),
        WakePolicy::default(),
        ms(0),
    );
    assert_eq!(d.reference_scored(reference.score, ms(40)), None);
    let decided = d.echo_compared(CandidateEcho::EchoOnly, ms(60)).unwrap();
    assert_eq!(
        decided.outcome,
        WakeOutcome::Refused(WakeRefusal::SelfOutput)
    );
    // Double talk rescues it, and the verifier decides.
    let mut d = WakeDecider::arrive(
        candidate,
        ArrivalConditions::default(),
        WakePolicy::default(),
        ms(0),
    );
    d.reference_scored(reference.score, ms(40));
    assert_eq!(d.echo_compared(CandidateEcho::NearEndPresent, ms(60)), None);
    let decided = d
        .verified(VerifierResult::Text("ophelia".into()), ms(300))
        .unwrap();
    assert_eq!(decided.outcome, WakeOutcome::Accepted);
}

#[test]
fn silent_reference_is_no_overlap_without_running_the_model() {
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    let mut r = 0;
    while r < 100_000 {
        s.push_silence(ref_epoch(), r, REF_CHUNK);
        s.advance().unwrap();
        r += REF_CHUNK;
    }
    assert!(s.hops().all(|h| h.outcome == HopOutcome::BelowRmsFloor));
    assert_eq!(s.scorer_mut().runs, 0);
    let j = s
        .judge(
            mic_support(58_000, 82_000),
            &alignment(),
            &EchoPolicy::default(),
        )
        .unwrap();
    assert_eq!(j.score, ReferenceScore::NoOverlap);
}

#[test]
fn unscored_reference_is_unavailable_or_pending() {
    let echo = EchoPolicy::default();
    let pcm = wake_reference(100_000);
    let a = alignment();

    // A gap in the windows the candidate needs.
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    feed(&mut s, &pcm[..60_000], 0);
    s.push_missing(ref_epoch(), 60_000, 61_280);
    feed(&mut s, &pcm, 61_280);
    let j = s.judge(mic_support(100_000, 124_000), &a, &echo).unwrap();
    assert_eq!(j.score, ReferenceScore::Unavailable);
    assert_eq!(j.unavailable, Some(ReferenceUnavailable::InvalidWindow));

    // Not yet scored far enough: wait. Once the reference ends: unavailable.
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    feed(&mut s, &pcm[..80_000], 0);
    assert_eq!(s.judge(mic_support(100_000, 124_000), &a, &echo), None);
    s.reference_ended();
    assert_eq!(
        s.judge(mic_support(100_000, 124_000), &a, &echo)
            .unwrap()
            .unavailable,
        Some(ReferenceUnavailable::Ended)
    );

    // A reference that starts too late to cover the candidate's earliest windows.
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    feed(&mut s, &pcm, 35_840);
    assert_eq!(
        s.judge(mic_support(58_000, 82_000), &a, &echo)
            .unwrap()
            .unavailable,
        Some(ReferenceUnavailable::NotEnoughHistory)
    );

    // Another reference epoch than the alignment's.
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    s.push_pcm(Epoch::new(77).unwrap(), 0, pcm.iter().copied());
    assert_eq!(
        s.judge(mic_support(58_000, 82_000), &a, &echo)
            .unwrap()
            .unavailable,
        Some(ReferenceUnavailable::EpochMismatch)
    );

    // An inference error makes its hop invalid, is reported, and scoring continues.
    let mut s = ReferenceWakeScorer::new(
        Script([0.1, 0.2].into_iter().collect()),
        ReferenceWakePolicy::default(),
    );
    let noise = synth::pcm(&synth::white(23_040 + 2 * 2_560, 1, -30.0));
    s.push_pcm(ref_epoch(), 0, noise.iter().copied());
    assert!(s.advance().is_err());
    assert_eq!(s.hops().last().unwrap().outcome, HopOutcome::Invalid);
    assert_eq!(s.advance().unwrap(), 0);
}

#[test]
fn reset_forgets_the_reference() {
    let mut s = ReferenceWakeScorer::new(MarkerScorer { runs: 0 }, ReferenceWakePolicy::default());
    feed(&mut s, &wake_reference(100_000), 0);
    assert!(s.candidates().count() > 0);
    s.reset();
    assert_eq!(s.epoch(), None);
    assert_eq!(s.hops().count(), 0);
    assert_eq!(s.candidates().count(), 0);
    // A new epoch starts its history at its own first sample.
    s.push_silence(Epoch::new(11).unwrap(), 5_120, 30_000);
    s.advance().unwrap();
    assert_eq!(s.hops().next().unwrap().end, 28_160);
}
