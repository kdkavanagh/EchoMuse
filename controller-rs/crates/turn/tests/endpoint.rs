//! The endpoint reducer against synthetic evidence streams, one stated case per test (§4.5,
//! §4.6, §6.1). Each stream is built cell by cell: a VAD probability, a device loudness and an
//! echo label per 32 ms cell, and recognizer tokens with emission samples. Blocks are 80 ms;
//! the policy is advanced after every block unless a test says otherwise.

use std::collections::HashMap;

use echomuse_turn::endpoint::{self, CloseCause, ReplySpec};
use echomuse_turn::trace::EndpointEvent;
use echomuse_turn::{
    AsrKind, AsrObservation, CellIndex, CloseReason, Completeness, Decision, DeviceCell,
    DeviceInput, EchoCell, EchoLabel, EndBound, Epoch, MonoInstant, PauseError, PauseRecord,
    PauseServerAnswer, RecognizerAnswer, RecognizerError, ReferenceCoverage, ReplyOrigin, Route,
    Sample, SampleSpan, Samples, ServerFailure, TextClass, TextSource, Token, TurnConfig, TurnKind,
    TurnPolicy, TurnSpec, VadInput, WakeAnchor,
};

/// The utterance start: cell 100, also a block boundary.
const U: u64 = 51_200;
const CELL: u64 = 512;
const BLOCK: u64 = 1_280;
/// Cells of quiet room before the utterance, so the background level is known from the start.
const HISTORY_CELLS: u64 = 100;

fn epoch() -> Epoch {
    Epoch::new(3).unwrap()
}

/// The sample `n` cells after the utterance start.
fn c(n: u64) -> Sample {
    Sample(U + n * CELL)
}

/// The evidence frontier after block `k`: the end of the last whole cell it covers.
fn frontier_of_block(k: u64) -> Sample {
    Sample(U + (k * BLOCK) / CELL * CELL)
}

/// The first block whose frontier reaches `at`, and that frontier.
fn first_frontier_at_least(at: Sample) -> (u64, Sample) {
    (1..)
        .map(|k| (k, frontier_of_block(k)))
        .find(|(_, f)| *f >= at)
        .unwrap()
}

#[derive(Clone, Copy, Debug)]
struct CellSpec {
    vad: Option<f32>,
    e: Option<i16>,
    echo: EchoLabel,
    gap: bool,
    muted: bool,
}

const fn spec(vad: Option<f32>, e: Option<i16>) -> CellSpec {
    CellSpec {
        vad,
        e,
        echo: EchoLabel::NoReference,
        gap: false,
        muted: false,
    }
}

/// A quiet room at -75 dB.
const QUIET: CellSpec = spec(Some(0.05), Some(-7_500));
/// The commander at -50 dB.
const SPEECH: CellSpec = spec(Some(0.95), Some(-5_000));
/// A TV across the room: speech 15 dB below the commander.
const TV: CellSpec = spec(Some(0.9), Some(-6_500));
/// A cell with no VAD result: unknown, never silence.
const NO_VAD: CellSpec = spec(None, Some(-7_500));
const GAP: CellSpec = CellSpec { gap: true, ..QUIET };
const MUTED: CellSpec = CellSpec {
    muted: true,
    ..QUIET
};

#[derive(Clone, Default)]
struct Timeline {
    cells: Vec<CellSpec>,
    words: Vec<(String, Sample)>,
}

impl Timeline {
    fn then(mut self, s: CellSpec, n: usize) -> Self {
        self.cells.extend(std::iter::repeat_n(s, n));
        self
    }

    /// A token emitted at `at`.
    fn word(mut self, text: &str, at: Sample) -> Self {
        self.words.push((format!(" {text}"), at));
        self
    }

    /// The recognizer's live result for a block covering through `through`.
    fn asr(&self, through: Sample) -> AsrObservation {
        let tokens: Vec<Token> = self
            .words
            .iter()
            .filter(|(_, at)| *at <= through)
            .map(|(t, at)| Token {
                text: t.clone(),
                emitted: *at,
            })
            .collect();
        let last = tokens.last().map_or(Sample(U), |t| t.emitted);
        AsrObservation {
            through,
            tokens,
            trailing_blanks: (through.since(last).0 / 640) as u32,
            kind: AsrKind::Live,
        }
    }
}

#[derive(Default)]
struct Grammar(HashMap<&'static str, TextClass>);

impl Grammar {
    fn with(mut self, text: &'static str, class: TextClass) -> Self {
        self.0.insert(text, class);
        self
    }
}

impl Completeness for Grammar {
    fn classify(&self, text: &str) -> TextClass {
        self.0.get(text).copied().unwrap_or(TextClass::Unknown)
    }
}

fn button(trigger: Sample) -> TurnSpec {
    TurnSpec {
        epoch: epoch(),
        start: Sample(U),
        trigger,
        cells_from: CellIndex(U / CELL - HISTORY_CELLS),
        vad_from: Sample(U).cell(),
        kind: TurnKind::Button,
    }
}

fn ms(n: u64) -> MonoInstant {
    MonoInstant(n * 1_000_000)
}

/// A decision and the block after which it was returned.
type Logged = (u64, Decision);

struct Run {
    policy: TurnPolicy,
    tl: Timeline,
    grammar: Grammar,
    block: u64,
    log: Vec<Logged>,
}

impl Run {
    fn new(spec: TurnSpec, config: TurnConfig, tl: Timeline, grammar: Grammar) -> Self {
        let mut policy = TurnPolicy::new(config, spec, ms(0)).unwrap();
        let history = vec![DeviceInput::Cell(DeviceCell::audio(-7_500, 0)); HISTORY_CELLS as usize];
        policy
            .evidence_mut()
            .push_device_cells(epoch(), CellIndex(U / CELL - HISTORY_CELLS), &history)
            .unwrap();
        Self {
            policy,
            tl,
            grammar,
            block: 0,
            log: Vec::new(),
        }
    }

    fn button(tl: Timeline, grammar: Grammar) -> Self {
        Self::new(button(c(10)), TurnConfig::default(), tl, grammar)
    }

    /// Pushes cells `[from, to)` of the timeline (indices from the utterance start).
    fn push_cells(&mut self, from: u64, to: u64) {
        for i in from..to.min(self.tl.cells.len() as u64) {
            let s = self.tl.cells[i as usize];
            let cell = Sample(U + i * CELL).cell();
            let ev = self.policy.evidence_mut();
            if s.gap {
                ev.mark_gap(epoch(), SampleSpan::new(cell.start(), cell.end()))
                    .unwrap();
                continue;
            }
            let device = if s.muted {
                DeviceInput::Cell(DeviceCell::from_wire(-12_000, 1 | 2, 0).unwrap())
            } else {
                match s.e {
                    Some(e) => DeviceInput::Cell(DeviceCell::audio(e, 0)),
                    None => DeviceInput::Unknown,
                }
            };
            ev.push_device_cells(epoch(), cell, &[device]).unwrap();
            let vad = s
                .vad
                .map_or(VadInput::Unknown, |p| VadInput::probability(p).unwrap());
            ev.push_vad(epoch(), cell, &[vad]).unwrap();
            let coverage = match s.echo {
                EchoLabel::NoReference => ReferenceCoverage::Silent,
                _ => ReferenceCoverage::Full,
            };
            ev.push_echo(
                epoch(),
                cell,
                &[EchoCell {
                    label: s.echo,
                    coverage,
                }],
            )
            .unwrap();
        }
    }

    fn now(&self) -> MonoInstant {
        ms(self.block * 80)
    }

    /// Feeds block `block + 1` and advances.
    fn step(&mut self) {
        let k = self.block + 1;
        let through = Sample(U + k * BLOCK);
        let prev_cells = (U + (k - 1) * BLOCK).div_ceil(CELL) - U / CELL;
        let cells = (k * BLOCK).div_ceil(CELL);
        self.push_cells(prev_cells.min(cells), cells);
        let obs = self.tl.asr(through);
        self.policy.evidence_mut().push_asr(epoch(), obs).unwrap();
        self.block = k;
        let mut out = Vec::new();
        self.policy.advance(&self.grammar, self.now(), &mut out);
        self.log.extend(out.into_iter().map(|d| (k, d)));
    }

    /// Steps until the policy finishes or the timeline runs out.
    fn run(&mut self) -> &mut Self {
        let max_blocks = (self.tl.cells.len() as u64 * CELL) / BLOCK;
        while !self.policy.is_finished() && self.block < max_blocks {
            self.step();
        }
        self
    }

    fn run_to_block(&mut self, k: u64) -> &mut Self {
        while !self.policy.is_finished() && self.block < k {
            self.step();
        }
        self
    }

    fn decisions(&self) -> Vec<Decision> {
        self.log.iter().map(|(_, d)| d.clone()).collect()
    }

    fn find<T>(&self, f: impl Fn(&Decision) -> Option<T>) -> Option<(u64, T)> {
        self.log.iter().find_map(|(k, d)| f(d).map(|t| (*k, t)))
    }

    fn pending(&self) -> Option<(u64, (Route, Sample, Sample))> {
        self.find(|d| match d {
            Decision::EndPending {
                route,
                boundary,
                since,
            } => Some((*route, *boundary, *since)),
            _ => None,
        })
    }

    fn commit(&self) -> Option<(u64, echomuse_turn::Commit)> {
        self.find(|d| match d {
            Decision::Commit(c) => Some(c.clone()),
            _ => None,
        })
    }

    fn close(&self) -> Option<(u64, (CloseReason, Option<Sample>))> {
        self.find(|d| match d {
            Decision::Close { reason, at } => Some((*reason, *at)),
            _ => None,
        })
    }

    fn close_cause(&self) -> Option<CloseCause> {
        self.policy.trace().endpoint.iter().find_map(|e| match e {
            EndpointEvent::Close { cause, .. } => Some(*cause),
            _ => None,
        })
    }
}

/// "turn off the lights" spoken in cells 10..`end` (the trigger is cell 10), then quiet.
fn lights(end: u64) -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, (end - 10) as usize)
        .then(QUIET, 400)
        .word("turn", c(12))
        .word("off", c(15))
        .word("the", c(17))
        .word("lights", c(end))
}

fn lights_grammar(class: TextClass) -> Grammar {
    Grammar::default().with("turn off the lights", class)
}

#[test]
fn route_a_sets_the_end_exactly_at_the_complete_pause_and_commits_after_the_lookahead() {
    // Command speech ends at cell 21 (U + 10752); 608 ms later is U + 20480, a block frontier.
    let mut r = Run::button(lights(21), lights_grammar(TextClass::Complete));
    r.run();
    let (k, (route, boundary, since)) = r.pending().expect("pending");
    assert_eq!(
        (route, boundary, since),
        (Route::A, c(21), Sample(U + 20_480))
    );
    assert_eq!(since.since(boundary), Samples::from_ms(608));
    // The block before reached only 512 ms of pause.
    assert_eq!(
        frontier_of_block(k - 1).since(boundary),
        Samples::from_ms(512)
    );
    // The commit waits for 192 ms of new audio after the decision.
    let (_, commit) = r.commit().expect("commit");
    let (_, expected_at) = first_frontier_at_least(since + Samples::from_ms(192));
    assert_eq!(commit.at, expected_at);
    assert_eq!(commit.route, Route::A);
    assert_eq!(
        commit.span,
        SampleSpan::new(Sample(U), c(21) + Samples::from_ms(192))
    );
    assert_eq!(commit.text, "turn off the lights");
    assert_eq!(commit.class, Some(TextClass::Complete));
    assert!(!commit.recheck);
    // The first decision is the start of command speech, at the trigger.
    assert_eq!(r.decisions()[0], Decision::CommandStarted { at: c(10) });
}

#[test]
fn route_a_one_cell_later_waits_one_more_block() {
    let mut r = Run::button(lights(22), lights_grammar(TextClass::Complete));
    r.run();
    let (_, (_, boundary, since)) = r.pending().unwrap();
    assert_eq!(boundary, c(22));
    let (_, expected) = first_frontier_at_least(c(22) + Samples::from_ms(608));
    assert_eq!(since, expected);
    assert!(since.since(boundary) >= Samples::from_ms(608));
}

#[test]
fn the_pause_depends_on_the_text_class() {
    for (class, pause_ms) in [
        (TextClass::Complete, 608),
        (TextClass::Extendable, 1_216),
        (TextClass::NeedsMore, 1_792),
        (TextClass::Unknown, 1_792),
    ] {
        let mut r = Run::button(lights(21), lights_grammar(class));
        r.run();
        let (k, (_, boundary, since)) = r.pending().unwrap();
        let (_, expected) = first_frontier_at_least(boundary + Samples::from_ms(pause_ms));
        assert_eq!(since, expected, "{class:?}");
        assert!(frontier_of_block(k - 1).since(boundary) < Samples::from_ms(pause_ms));
    }
}

#[test]
fn command_speech_in_the_lookahead_revokes_the_end_up_to_its_last_cell() {
    // Pending at U + 20480 (cell 40). The lookahead completes at the block whose frontier is
    // cell 47 (U + 24064). Four command cells in cells 43..47, the last of them in that very
    // block, revoke instead of committing.
    let tl = lights(21);
    let mut cells = tl.cells.clone();
    for c in &mut cells[43..47] {
        *c = SPEECH;
    }
    let tl = Timeline { cells, ..tl };
    let mut r = Run::button(tl, lights_grammar(TextClass::Complete));
    r.run_to_block(19);
    let (k, (boundary, at)) = r
        .find(|d| match d {
            Decision::EndRevoked { boundary, at } => Some((*boundary, *at)),
            _ => None,
        })
        .expect("revoked");
    assert_eq!((k, boundary, at), (19, c(21), c(47)));
    assert!(r.commit().is_none());
    // The utterance goes on and ends after the resumed speech.
    r.run();
    let (_, commit) = r.commit().expect("commit after the resumed speech");
    assert_eq!(commit.boundary, c(47));

    // Three command cells (44..47) do not revoke: the commit happens at the same block.
    let tl = lights(21);
    let mut cells = tl.cells.clone();
    for c in &mut cells[44..47] {
        *c = SPEECH;
    }
    let tl = Timeline { cells, ..tl };
    let mut r = Run::button(tl, lights_grammar(TextClass::Complete));
    r.run();
    let (k, commit) = r.commit().unwrap();
    assert_eq!((k, commit.boundary, commit.at), (19, c(21), c(47)));
}

#[test]
fn a_complete_command_under_background_speech_ends_where_the_command_ended() {
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(TV, 200)
        .word("stop", c(21));
    let mut r = Run::button(tl, Grammar::default().with("stop", TextClass::Complete));
    r.run();
    let (_, (route, boundary, since)) = r.pending().unwrap();
    assert_eq!((route, boundary), (Route::B, c(21)));
    // The prefix changed at the block covering the word's emission (block 9, U + 11520), and
    // route B waits 608 ms from there.
    let (_, expected) = first_frontier_at_least(Sample(U + 11_520) + Samples::from_ms(608));
    assert_eq!(since, expected);
    let (_, commit) = r.commit().unwrap();
    assert_eq!(commit.route, Route::B);
    assert!(commit.recheck, "route B re-decodes before dispatch");
    assert_eq!(commit.span.end, c(21) + Samples::from_ms(192));
    // The TV was background speech, never command speech.
    assert!(
        r.policy
            .trace()
            .segments
            .iter()
            .any(|s| s.class == echomuse_turn::CellClass::BackgroundSpeech)
    );
}

#[test]
fn after_unknown_text_background_speech_counts_as_pause() {
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(TV, 200)
        .word("what's", c(14))
        .word("the", c(16))
        .word("score", c(21));
    let mut r = Run::button(tl, Grammar::default());
    r.run();
    let (_, (route, boundary, since)) = r.pending().unwrap();
    assert_eq!((route, boundary), (Route::A, c(21)));
    assert_eq!(
        since,
        first_frontier_at_least(c(21) + Samples::from_ms(1_792)).1
    );
}

#[test]
fn five_seconds_without_command_speech_is_no_input() {
    let tl = Timeline::default().then(QUIET, 400);
    let mut r = Run::button(tl, Grammar::default());
    r.run();
    let (k, (reason, at)) = r.close().unwrap();
    let (expected_k, expected) = first_frontier_at_least(c(10) + Samples::from_ms(5_000));
    assert_eq!(
        (k, reason, at),
        (expected_k, CloseReason::NoInput, Some(expected))
    );
    assert!(frontier_of_block(k - 1) < c(10) + Samples::from_ms(5_000));
}

#[test]
fn unknown_evidence_is_never_silence() {
    // "stop" then cells without VAD: the quiet vote fails, so route A never fires; only the
    // no-progress fallback ends the turn, 3 s after the text last changed.
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(NO_VAD, 200)
        .word("stop", c(21));
    let mut r = Run::button(
        tl.clone(),
        Grammar::default().with("stop", TextClass::Complete),
    );
    r.run();
    assert!(r.pending().is_none());
    let (_, commit) = r.commit().unwrap();
    assert_eq!(commit.route, Route::Fallback);
    assert_eq!(commit.boundary, c(21));
    assert!(commit.recheck);
    let progress = Sample(U + 11_520);
    assert_eq!(
        commit.at,
        first_frontier_at_least(progress + Samples::from_ms(3_000)).1
    );
    assert_eq!(
        r.policy.trace().evidence.vad_unknown,
        200.min(commit.at.since(c(21)).0 / CELL) as u32
    );

    // Without complete text the same stream is a retry: nothing is dispatched.
    let mut r = Run::button(tl, Grammar::default());
    r.run();
    let (_, (reason, _)) = r.close().unwrap();
    assert_eq!(reason, CloseReason::Retry);
    assert_eq!(r.close_cause(), Some(CloseCause::NoProgress));

    // Loudness unknown: speech still opens a run (r is unavailable), never background.
    let unknown_level = spec(Some(0.95), None);
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(unknown_level, 11)
        .then(QUIET, 200)
        .word("stop", c(21));
    let mut r = Run::button(tl, Grammar::default().with("stop", TextClass::Complete));
    r.run();
    assert_eq!(r.commit().unwrap().1.boundary, c(21));

    // An unknown echo label is not "no echo": with a loud VAD the cell is judged on VAD and
    // level, like any other, and only an echo_only label makes it the device's own output.
    let unknown_echo = CellSpec {
        echo: EchoLabel::Unknown,
        ..SPEECH
    };
    let echo_only = CellSpec {
        echo: EchoLabel::EchoOnly,
        ..SPEECH
    };
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(echo_only, 20)
        .then(unknown_echo, 11)
        .then(QUIET, 200)
        .word("stop", c(41));
    let mut r = Run::button(tl, Grammar::default().with("stop", TextClass::Complete));
    r.run();
    assert_eq!(r.decisions()[0], Decision::CommandStarted { at: c(30) });
    assert_eq!(r.commit().unwrap().1.boundary, c(41));
}

#[test]
fn utterances_are_limited_to_fifteen_or_thirty_seconds() {
    let mut tl = Timeline::default().then(QUIET, 10).then(SPEECH, 1_000);
    // A new word every second keeps the text progressing, so only the length limit applies.
    for i in 0..32 {
        tl = tl.word(&format!("w{i}"), c(10 + 31 * i));
    }
    for (extended, limit_ms) in [(false, 15_000), (true, 30_000)] {
        let config = TurnConfig {
            extended_utterances: extended,
            ..TurnConfig::default()
        };
        let mut r = Run::new(button(c(10)), config, tl.clone(), Grammar::default());
        r.run();
        let (k, (reason, at)) = r.close().unwrap();
        let (expected_k, expected) = first_frontier_at_least(c(10) + Samples::from_ms(limit_ms));
        assert_eq!(
            (k, reason, at),
            (expected_k, CloseReason::TooLong, Some(expected))
        );
    }
}

#[test]
fn speech_without_text_progress_ends_at_the_no_progress_fallback() {
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 300)
        .word("um", c(12));
    let mut r = Run::button(tl, Grammar::default());
    r.run();
    let (_, (reason, at)) = r.close().unwrap();
    assert_eq!(reason, CloseReason::Retry);
    // Progress is the block where "um" appeared (block 5, U + 6400), later than the start of
    // command speech at the trigger.
    assert_eq!(
        at,
        Some(first_frontier_at_least(Sample(U + 6_400) + Samples::from_ms(3_000)).1)
    );
}

#[test]
fn gaps_and_muted_cells_end_the_turn() {
    let tl = lights(21);
    let mut cells = tl.cells.clone();
    cells[25] = GAP;
    let mut r = Run::button(
        Timeline {
            cells,
            ..tl.clone()
        },
        lights_grammar(TextClass::Complete),
    );
    r.run();
    let (_, (reason, at)) = r.close().unwrap();
    assert_eq!((reason, at), (CloseReason::Interrupted, Some(c(26))));
    assert_eq!(r.close_cause(), Some(CloseCause::Gap));
    assert!(r.commit().is_none());

    let mut cells = tl.cells.clone();
    cells[25] = MUTED;
    let mut r = Run::button(
        Timeline { cells, ..tl },
        lights_grammar(TextClass::Complete),
    );
    r.run();
    assert_eq!(r.close().unwrap().1.0, CloseReason::Muted);
    assert_eq!(r.close_cause(), Some(CloseCause::MutedCell));
}

#[test]
fn control_events_end_the_turn_with_their_own_reason() {
    type Event = fn(&mut TurnPolicy, &mut Vec<Decision>);
    let cases: [(Event, CloseReason, CloseCause); 5] = [
        (
            TurnPolicy::audio_overrun,
            CloseReason::AudioOverrun,
            CloseCause::Overrun,
        ),
        (TurnPolicy::mute, CloseReason::Muted, CloseCause::Mute),
        (
            TurnPolicy::session_lost,
            CloseReason::SessionLost,
            CloseCause::SessionLost,
        ),
        (
            TurnPolicy::worker_error,
            CloseReason::Interrupted,
            CloseCause::WorkerError,
        ),
        (
            TurnPolicy::discontinuity,
            CloseReason::Interrupted,
            CloseCause::Discontinuity,
        ),
    ];
    for (event, reason, cause) in cases {
        let mut r = Run::button(lights(21), lights_grammar(TextClass::Complete));
        r.run_to_block(12);
        let mut out = Vec::new();
        event(&mut r.policy, &mut out);
        assert_eq!(
            out,
            [Decision::Close {
                reason,
                at: Some(frontier_of_block(12))
            }]
        );
        assert_eq!(r.close_cause(), Some(cause));
        // Nothing more happens once it is closed.
        r.step();
        assert_eq!(
            r.log.len(),
            1,
            "only CommandStarted, and nothing after the close"
        );
        event(&mut r.policy, &mut out);
        assert_eq!(out.len(), 1);
    }
}

#[test]
fn the_evidence_stall_backstop_runs_on_the_monotonic_clock() {
    let mut r = Run::button(lights(21), lights_grammar(TextClass::Complete));
    r.run_to_block(5);
    let last = r.now();
    let deadline = last.after(std::time::Duration::from_secs(3));
    assert_eq!(r.policy.next_deadline(), Some(deadline));
    let mut out = Vec::new();
    r.policy.on_timer(MonoInstant(deadline.0 - 1), &mut out);
    assert!(out.is_empty());
    r.policy.on_timer(deadline, &mut out);
    assert_eq!(
        out[0],
        Decision::Close {
            reason: CloseReason::Interrupted,
            at: Some(frontier_of_block(5))
        }
    );
    assert_eq!(r.close_cause(), Some(CloseCause::EvidenceStalled));
    assert_eq!(r.policy.next_deadline(), None);
}

fn recognizer_config() -> TurnConfig {
    TurnConfig {
        recognizer_available: true,
        ..TurnConfig::default()
    }
}

fn time_question() -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(QUIET, 400)
        .word("what", c(13))
        .word("time", c(15))
        .word("is", c(17))
        .word("it", c(21))
}

#[test]
fn a_recognizer_answer_takes_effect_at_the_next_block_and_records_it() {
    let mut r = Run::new(
        button(c(10)),
        recognizer_config(),
        time_question(),
        Grammar::default(),
    );
    r.run_to_block(10);
    let (asked_k, text) = r
        .find(|d| match d {
            Decision::AskRecognizer { text } => (text == "what time is it").then(|| text.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!((asked_k, text.as_str()), (9, "what time is it"));
    // The answer arrives after block 16 was evaluated, so it applies at block 17.
    r.run_to_block(16);
    assert!(
        r.pending().is_none(),
        "unknown text waits for the long pause"
    );
    let at = r.now();
    r.policy.recognizer_answer(
        RecognizerAnswer {
            text: "what time is it".into(),
            outcome: Ok(TextClass::Complete),
        },
        at,
    );
    r.run();
    let record = r
        .policy
        .trace()
        .recognizer
        .iter()
        .find(|x| x.text == "what time is it")
        .unwrap()
        .clone();
    assert_eq!(record.applied_at, Some(Sample(U + 17 * BLOCK)));
    assert_eq!(record.class, Some(TextClass::Complete));
    assert_eq!(record.round_trip_ms, Some(7 * 80));
    let (k, (_, _, since)) = r.pending().unwrap();
    assert_eq!((k, since), (17, frontier_of_block(17)));
}

#[test]
fn a_late_or_failed_recognizer_answer_changes_nothing() {
    for (delay_ms, outcome, error) in [
        (2_001, Ok(TextClass::Complete), RecognizerError::Timeout),
        (100, Err(RecognizerError::Failed), RecognizerError::Failed),
    ] {
        let mut r = Run::new(
            button(c(10)),
            recognizer_config(),
            time_question(),
            Grammar::default(),
        );
        r.run_to_block(9);
        let asked = r.now();
        r.policy.recognizer_answer(
            RecognizerAnswer {
                text: "what time is it".into(),
                outcome,
            },
            asked.after(std::time::Duration::from_millis(delay_ms)),
        );
        r.run();
        let rec = r
            .policy
            .trace()
            .recognizer
            .iter()
            .find(|x| x.text == "what time is it")
            .unwrap();
        assert_eq!(rec.error, Some(error));
        assert_eq!(rec.applied_at, None);
        let (_, (_, boundary, since)) = r.pending().unwrap();
        assert_eq!(
            since,
            first_frontier_at_least(boundary + Samples::from_ms(1_792)).1
        );
    }
}

#[test]
fn an_answer_about_other_text_does_not_apply() {
    let mut r = Run::new(
        button(c(10)),
        recognizer_config(),
        time_question(),
        Grammar::default(),
    );
    r.run_to_block(10);
    let now = r.now();
    r.policy.recognizer_answer(
        RecognizerAnswer {
            text: "what time is it now".into(),
            outcome: Ok(TextClass::Complete),
        },
        now,
    );
    r.run();
    let (_, (_, boundary, since)) = r.pending().unwrap();
    assert_eq!(
        since,
        first_frontier_at_least(boundary + Samples::from_ms(1_792)).1
    );
    assert!(
        r.policy
            .trace()
            .recognizer
            .iter()
            .any(|x| x.error == Some(RecognizerError::NotAsked))
    );
}

/// Every evidence item of `tl` for blocks 1..=k, pushed in the given order of input kinds.
fn push_everything(r: &mut Run, k: u64, reverse_vad: bool) {
    let cells = (k * BLOCK).div_ceil(CELL);
    let ev = r.policy.evidence_mut();
    for i in 0..cells {
        let s = r.tl.cells[i as usize];
        let cell = Sample(U + i * CELL).cell();
        ev.push_device_cells(
            epoch(),
            cell,
            &[DeviceInput::Cell(DeviceCell::audio(s.e.unwrap(), 0))],
        )
        .unwrap();
        ev.push_echo(epoch(), cell, &[EchoCell::SILENT_REFERENCE])
            .unwrap();
    }
    for b in 1..=k {
        let obs = r.tl.asr(Sample(U + b * BLOCK));
        r.policy.evidence_mut().push_asr(epoch(), obs).unwrap();
    }
    let order: Vec<u64> = if reverse_vad {
        (0..cells).rev().collect()
    } else {
        (0..cells).collect()
    };
    for i in order {
        let s = r.tl.cells[i as usize];
        let cell = Sample(U + i * CELL).cell();
        r.policy
            .evidence_mut()
            .push_vad(
                epoch(),
                cell,
                &[VadInput::probability(s.vad.unwrap()).unwrap()],
            )
            .unwrap();
        if reverse_vad {
            // Advance after every late VAD result, as a runtime would.
            let mut out = Vec::new();
            r.policy.advance(&r.grammar, ms(0), &mut out);
            r.log.extend(out.into_iter().map(|d| (0, d)));
        }
    }
}

#[test]
fn every_decision_of_a_batch_is_returned_in_order() {
    let mut incremental = Run::button(lights(21), lights_grammar(TextClass::Complete));
    incremental.run();
    let expected = incremental.decisions();
    assert!(matches!(expected.last(), Some(Decision::Commit(_))));
    assert!(expected.len() >= 3, "{expected:?}");

    // All of it at once, one advance: the same decisions in the same order.
    let mut batch = Run::button(lights(21), lights_grammar(TextClass::Complete));
    push_everything(&mut batch, 30, false);
    let mut out = Vec::new();
    batch.policy.advance(&batch.grammar, ms(0), &mut out);
    assert_eq!(out, expected);
    assert_eq!(
        batch.policy.trace().segments,
        incremental.policy.trace().segments
    );
    assert_eq!(
        batch.policy.trace().endpoint,
        incremental.policy.trace().endpoint
    );
}

#[test]
fn worker_completion_order_does_not_change_decisions() {
    let mut incremental = Run::button(lights(21), lights_grammar(TextClass::Complete));
    incremental.run();
    // VAD results arrive last and newest first: nothing is released until the oldest arrives.
    let mut late = Run::button(lights(21), lights_grammar(TextClass::Complete));
    push_everything(&mut late, 30, true);
    assert_eq!(late.decisions(), incremental.decisions());
    assert_eq!(
        late.policy.trace().segments,
        incremental.policy.trace().segments
    );
}

fn reply_spec(origin: ReplyOrigin, trigger: Sample, guard: bool) -> TurnSpec {
    let policy = TurnConfig::default().endpoint;
    let reply = if guard {
        ReplySpec::at_drain(origin, trigger, ms(0), &policy)
    } else {
        ReplySpec::at_early_answer(origin, trigger, ms(0), &policy)
    };
    TurnSpec {
        kind: TurnKind::Reply(reply),
        ..button(trigger)
    }
}

/// A reply window that opens at cell 25 (U + 12800).
const REPLY_TRIGGER: u64 = 25;

#[test]
fn a_silent_reply_window_closes_after_seven_seconds() {
    let trigger = c(REPLY_TRIGGER);
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        Timeline::default().then(QUIET, 600),
        Grammar::default(),
    );
    r.run();
    let (_, (reason, at)) = r.close().unwrap();
    assert_eq!(reason, CloseReason::NoInput);
    assert_eq!(
        at,
        Some(first_frontier_at_least(trigger + Samples::from_ms(7_000)).1)
    );
}

#[test]
fn a_reply_window_also_closes_on_the_monotonic_clock() {
    let trigger = c(REPLY_TRIGGER);
    // A long stall backstop isolates the window's own wall-clock bound.
    let mut config = TurnConfig::default();
    config.endpoint.evidence_stall_ms = 60_000;
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        config,
        Timeline::default().then(QUIET, 600),
        Grammar::default(),
    );
    r.run_to_block(10);
    assert_eq!(r.policy.next_deadline(), Some(ms(10_000)));
    // Audio stopped coming at 0.8 s of window; the window still closes 10 s after it opened.
    let mut out = Vec::new();
    r.policy.on_timer(ms(9_999), &mut out);
    assert!(out.is_empty());
    r.policy.on_timer(ms(10_000), &mut out);
    assert!(matches!(
        out[0],
        Decision::Close {
            reason: CloseReason::NoInput,
            ..
        }
    ));
    assert_eq!(r.close_cause(), Some(CloseCause::ReplyWallClock));
}

#[test]
fn a_reply_guard_holds_the_run_and_the_span_starts_before_the_answer() {
    // The answer starts exactly as the window opens and lasts 20 cells.
    let trigger = c(REPLY_TRIGGER);
    let tl = Timeline::default()
        .then(QUIET, REPLY_TRIGGER as usize)
        .then(SPEECH, 20)
        .then(QUIET, 300)
        .word("yes", c(45));
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        tl,
        Grammar::default().with("yes", TextClass::Complete),
    );
    r.run();
    let trace = r.policy.trace();
    assert_eq!(trace.guard_until, Some(trigger + Samples::from_ms(320)));
    assert!(
        trace
            .segments
            .iter()
            .any(|s| s.rule == echomuse_turn::ClassRule::Guard)
    );
    // The run opened at cell 35 (the guard's end) but is dated back to the answer's start.
    assert_eq!(trace.command_start, Some(c(35)));
    assert_eq!(trace.answer_start, Some(trigger));
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.span.start, trigger - Samples::from_ms(300));
    assert_eq!(commit.boundary, c(45));
}

#[test]
fn a_short_answer_counts_the_cells_it_was_dated_back_over() {
    // "yes" lasting 13 cells from the window's opening: only 3 cells open after the guard, but
    // the 10 dated-back cells count toward route A's 192 ms, so the answer still ends at its
    // pause.
    let trigger = c(REPLY_TRIGGER);
    let tl = Timeline::default()
        .then(QUIET, REPLY_TRIGGER as usize)
        .then(SPEECH, 13)
        .then(QUIET, 300)
        .word("yes", c(38));
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        tl,
        Grammar::default().with("yes", TextClass::Complete),
    );
    r.run();
    assert_eq!(r.policy.trace().command_cells, 3);
    let (_, (route, boundary, _)) = r.pending().unwrap();
    assert_eq!((route, boundary), (Route::A, c(38)));
}

#[test]
fn speech_under_way_when_the_window_opens_is_not_the_answer() {
    // Speech from cell 5 (640 ms before the trigger) to cell 45, then quiet: background with
    // rule under_way, and the window closes no_input at 7 s.
    let trigger = c(REPLY_TRIGGER);
    let tl = Timeline::default()
        .then(QUIET, 5)
        .then(SPEECH, 40)
        .then(QUIET, 600)
        .word("blah", c(30));
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        tl,
        Grammar::default(),
    );
    r.run();
    assert!(
        r.policy
            .trace()
            .segments
            .iter()
            .any(|s| s.rule == echomuse_turn::ClassRule::UnderWay)
    );
    assert_eq!(r.close().unwrap().1.0, CloseReason::NoInput);
}

#[test]
fn a_reply_gap_before_command_speech_reopens_and_after_it_interrupts() {
    let trigger = c(REPLY_TRIGGER);
    let tl = Timeline::default()
        .then(QUIET, 30)
        .then(GAP, 3)
        .then(QUIET, 300);
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        tl,
        Grammar::default(),
    );
    r.run();
    let reopen = r
        .find(|d| match d {
            Decision::ReopenReply(x) => Some(*x),
            _ => None,
        })
        .unwrap()
        .1;
    assert_eq!(reopen.start, c(33));
    assert_eq!(reopen.trigger, c(33));
    assert_eq!(reopen.no_input_at, trigger + Samples::from_ms(7_000));
    assert!(r.policy.is_finished());
    assert!(r.close().is_none());

    let tl = Timeline::default()
        .then(QUIET, REPLY_TRIGGER as usize)
        .then(SPEECH, 20)
        .then(GAP, 1)
        .then(QUIET, 300)
        .word("yes", c(45));
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, true),
        TurnConfig::default(),
        tl,
        Grammar::default(),
    );
    r.run();
    assert_eq!(r.close().unwrap().1.0, CloseReason::Interrupted);
}

#[test]
fn a_reply_to_an_ha_conversation_ends_at_its_first_long_pause() {
    let trigger = c(REPLY_TRIGGER);
    let tl = Timeline::default()
        .then(QUIET, REPLY_TRIGGER as usize)
        .then(SPEECH, 20)
        .then(QUIET, 300)
        .word("the", c(30))
        .word("bedroom", c(40))
        .word("fan", c(45));
    let mut r = Run::new(
        reply_spec(ReplyOrigin::HaConversation, trigger, false),
        TurnConfig {
            recognizer_available: true,
            ..TurnConfig::default()
        },
        tl,
        Grammar::default(),
    );
    r.run();
    let (_, (route, boundary, since)) = r.pending().unwrap();
    assert_eq!((route, boundary), (Route::R, c(45)));
    assert_eq!(
        since,
        first_frontier_at_least(c(45) + Samples::from_ms(1_024)).1
    );
    // Route R reads no completeness, so HA is never asked.
    assert!(
        !r.decisions()
            .iter()
            .any(|d| matches!(d, Decision::AskRecognizer { .. }))
    );
    // Without a guard the answer is command speech from its first cell.
    assert_eq!(r.policy.trace().command_start, Some(trigger));
}

#[test]
fn a_reply_counts_too_long_from_its_first_command_speech() {
    let trigger = c(REPLY_TRIGGER);
    let mut tl = Timeline::default().then(QUIET, 150).then(SPEECH, 600);
    for i in 0..20 {
        tl = tl.word(&format!("w{i}"), c(150 + 31 * i));
    }
    let mut r = Run::new(
        reply_spec(ReplyOrigin::Expectation, trigger, false),
        TurnConfig::default(),
        tl,
        Grammar::default(),
    );
    r.run();
    let (_, (reason, at)) = r.close().unwrap();
    assert_eq!(reason, CloseReason::TooLong);
    assert_eq!(
        at,
        Some(first_frontier_at_least(c(150) + Samples::from_ms(15_000)).1)
    );
}

fn wake_spec(trigger: Sample) -> TurnSpec {
    TurnSpec {
        kind: TurnKind::Wake(WakeAnchor {
            support_start: Sample(U + 4_800),
            candidate_open: trigger,
            phrase: "ophelia".into(),
        }),
        ..button(trigger)
    }
}

#[test]
fn a_wake_word_is_never_the_command() {
    // The wake word fills cells 10..50 (the trigger is cell 50), then silence.
    let trigger = c(50);
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 40)
        .then(QUIET, 400)
        .word("Oph", c(45))
        .word("ilia", c(49));
    let mut r = Run::new(
        wake_spec(trigger),
        TurnConfig::default(),
        tl,
        Grammar::default(),
    );
    r.run();
    assert!(r.policy.trace().foreground_seeded);
    assert_eq!(r.policy.trace().command_cells, 0);
    let (_, (reason, at)) = r.close().unwrap();
    assert_eq!(reason, CloseReason::NoInput);
    assert_eq!(
        at,
        Some(first_frontier_at_least(trigger + Samples::from_ms(5_000)).1)
    );
}

#[test]
fn a_wake_turn_judges_the_command_after_the_wake_phrase() {
    let trigger = c(50);
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 40)
        .then(QUIET, 10)
        .then(SPEECH, 12)
        .then(QUIET, 400)
        .word("Oph", c(45))
        .word("ilia", c(49))
        .word("stop", c(72));
    let mut r = Run::new(
        wake_spec(trigger),
        TurnConfig::default(),
        tl,
        Grammar::default().with("stop", TextClass::Complete),
    );
    r.run();
    assert_eq!(r.decisions()[0], Decision::CommandStarted { at: c(60) });
    let commit = r.commit().unwrap().1;
    assert_eq!((commit.text.as_str(), commit.boundary), ("stop", c(72)));
    assert_eq!(
        commit.span.start,
        Sample(U),
        "the committed audio keeps the wake word"
    );
}

#[test]
fn a_seeded_foreground_makes_quieter_speech_background() {
    // The wake word at -50 dB seeds F; a TV 15 dB down after it is background, never the
    // command, so the turn ends no_input.
    let trigger = c(50);
    let tl = Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 40)
        .then(TV, 300)
        .word("Ophelia", c(49))
        .word("news", c(80));
    let mut r = Run::new(
        wake_spec(trigger),
        TurnConfig::default(),
        tl.clone(),
        Grammar::default(),
    );
    r.run();
    assert_eq!(r.close().unwrap().1.0, CloseReason::NoInput);
    // The same TV after a button press, with no wake word to seed F, is command speech.
    let mut r = Run::new(button(c(50)), TurnConfig::default(), tl, Grammar::default());
    r.run();
    assert!(
        r.decisions()
            .iter()
            .any(|d| matches!(d, Decision::CommandStarted { .. }))
    );
}

#[test]
fn a_finalized_pause_decode_supplies_the_trailing_quiet() {
    // The live decoder's blank count is stuck at 0 (its last chunk edge), so route A waits;
    // a finalized result makes the real audio since the last speech-positive cell count.
    let tl = lights(21);
    let mut r = Run::button(tl.clone(), lights_grammar(TextClass::Complete));
    // Replace every live result with one whose blank count never grows.
    for k in 1..=20u64 {
        let through = Sample(U + k * BLOCK);
        let cells = (k * BLOCK).div_ceil(CELL);
        let prev = ((k - 1) * BLOCK).div_ceil(CELL);
        r.push_cells(prev, cells);
        let mut obs = tl.asr(through);
        obs.trailing_blanks = 0;
        if k == 13 {
            obs.kind = AsrKind::Finalized;
        }
        r.policy.evidence_mut().push_asr(epoch(), obs).unwrap();
        let mut out = Vec::new();
        r.policy.advance(&r.grammar, ms(k * 80), &mut out);
        r.log.extend(out.into_iter().map(|d| (k, d)));
        r.block = k;
    }
    let (_, (_, boundary, since)) = r.pending().expect("the finalized result unblocks route A");
    assert_eq!((boundary, since), (c(21), Sample(U + 20_480)));
    assert_eq!(r.policy.trace().finalized_at, vec![Sample(U + 13 * BLOCK)]);

    // Without the finalized result nothing ends by route A.
    let mut r = Run::button(tl.clone(), lights_grammar(TextClass::Complete));
    for k in 1..=40u64 {
        let cells = (k * BLOCK).div_ceil(CELL);
        let prev = ((k - 1) * BLOCK).div_ceil(CELL);
        r.push_cells(prev, cells);
        let mut obs = tl.asr(Sample(U + k * BLOCK));
        obs.trailing_blanks = 0;
        r.policy.evidence_mut().push_asr(epoch(), obs).unwrap();
        let mut out = Vec::new();
        r.policy.advance(&r.grammar, ms(k * 80), &mut out);
        r.log.extend(out.into_iter().map(|d| (k, d)));
    }
    assert!(r.pending().is_none());
}

#[test]
fn the_trace_serializes() {
    let mut r = Run::button(lights(21), lights_grammar(TextClass::Complete));
    r.run();
    let json = serde_json::to_string(r.policy.trace()).unwrap();
    assert!(json.contains("\"event\":\"commit\""));
    assert!(json.contains("\"command_speech\""));
    let back: echomuse_turn::UtteranceTrace = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, r.policy.trace());
}

/// "turn off the lights" as command speech in cells 10..21, then a final word said softly
/// (15 dB under the commander, so background speech by level) in cells 21..30, emitted by the
/// recognizer at the end of cell 30, then quiet.
fn soft_last_word() -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(TV, 9)
        .then(QUIET, 400)
        .word("turn", c(12))
        .word("off", c(15))
        .word("the", c(17))
        .word("lights", c(20))
        .word("please", c(30))
}

#[test]
fn a_soft_final_word_the_recognizer_heard_extends_the_committed_audio() {
    let mut r = Run::button(soft_last_word(), Grammar::default());
    r.run();
    let (_, (route, boundary, since)) = r.pending().unwrap();
    // The decision is unchanged: the pause counts from the last command-speech cell.
    assert_eq!((route, boundary), (Route::A, c(21)));
    assert_eq!(
        since,
        first_frontier_at_least(c(21) + Samples::from_ms(1_792)).1
    );
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.end_bound, EndBound::AsrWord);
    assert_eq!(commit.last_word, Some(c(30)));
    assert_eq!(
        commit.span.end,
        c(30) + Samples::from_ms(endpoint::ASR_WORD_MARGIN_MS)
    );
    assert!(commit.span.end > c(21) + Samples::from_ms(192));
    // The trace says which bound set the end.
    assert!(r.policy.trace().endpoint.iter().any(|e| matches!(
        e,
        EndpointEvent::Commit {
            end_bound: EndBound::AsrWord,
            ..
        }
    )));
}

#[test]
fn a_loud_final_word_keeps_the_cell_bound() {
    let mut r = Run::button(lights(21), lights_grammar(TextClass::Complete));
    r.run();
    let commit = r.commit().unwrap().1;
    // "lights" was emitted as its last command-speech cell ended: 64 ms past it is still inside
    // the 192 ms tail.
    assert_eq!(commit.end_bound, EndBound::Cell);
    assert_eq!(commit.last_word, Some(c(21)));
    assert_eq!(commit.span.end, c(21) + Samples::from_ms(192));
}

#[test]
fn words_emitted_after_the_decision_never_extend_the_committed_audio() {
    // Pending at cell 40; a soft word (background, so it cannot revoke) in cells 41..45 is
    // emitted at cell 45, before the commit at cell 47. It is past the decision, so the end
    // stays at the cell bound.
    let tl = lights(21);
    let mut cells = tl.cells.clone();
    for c in &mut cells[41..45] {
        *c = TV;
    }
    let tl = Timeline { cells, ..tl }.word("please", c(45));
    let mut r = Run::button(tl, lights_grammar(TextClass::Complete));
    r.run();
    let (_, (_, _, since)) = r.pending().unwrap();
    assert_eq!(since, c(40));
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.at, c(47));
    assert_eq!(commit.end_bound, EndBound::Cell);
    assert_eq!(commit.last_word, Some(c(21)));
    assert_eq!(commit.span.end, c(21) + Samples::from_ms(192));
}

#[test]
fn the_lookahead_counts_from_a_word_end_past_the_decision() {
    // With a 2 s margin the soft word's end lies past the decision, so the commit waits for 192
    // ms of audio after that end; the decision itself does not move.
    let mut config = TurnConfig::default();
    config.endpoint.asr_word_margin = Samples::from_ms(2_000);
    let mut r = Run::new(button(c(10)), config, soft_last_word(), Grammar::default());
    r.run();
    let (_, (_, _, since)) = r.pending().unwrap();
    assert_eq!(
        since,
        first_frontier_at_least(c(21) + Samples::from_ms(1_792)).1
    );
    let end = c(30) + Samples::from_ms(2_000);
    assert!(end > since);
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.span.end, end);
    assert_eq!(
        commit.at,
        first_frontier_at_least(end + Samples::from_ms(192)).1
    );
}

// ── Pause transcription by a server (§4.7) ─────────────────────────────────────────────────

/// The block whose result is the pause decode: 320 ms past the speech that ends at cell 21.
const PAUSE_BLOCK: u64 = 13;

fn pause_through() -> Sample {
    Sample(U + PAUSE_BLOCK * BLOCK)
}

/// "what time is it" said in cells 10..21 and misheard by Kroko as "watt tie miss it".
fn misheard() -> Timeline {
    Timeline::default()
        .then(QUIET, 10)
        .then(SPEECH, 11)
        .then(QUIET, 400)
        .word("watt", c(12))
        .word("tie", c(15))
        .word("miss", c(18))
        .word("it", c(21))
}

fn time_grammar() -> Grammar {
    Grammar::default().with("what time is it", TextClass::Complete)
}

fn server_config() -> TurnConfig {
    TurnConfig {
        pause_server: true,
        ..TurnConfig::default()
    }
}

/// Runs a button turn whose block [`PAUSE_BLOCK`] is the pause decode, sent to the server
/// 60 ms before that block's result; `answer` arrives 20 ms before block `k` is pushed.
fn pause_run(
    tl: Timeline,
    config: TurnConfig,
    answer: Option<(u64, Result<&str, ServerFailure>)>,
) -> Run {
    pause_run_with(tl, config, time_grammar(), answer)
}

/// [`pause_run`] with another grammar.
fn pause_run_with(
    tl: Timeline,
    config: TurnConfig,
    grammar: Grammar,
    answer: Option<(u64, Result<&str, ServerFailure>)>,
) -> Run {
    let mut r = Run::new(button(c(10)), config, tl, grammar);
    let max_blocks = (r.tl.cells.len() as u64 * CELL) / BLOCK;
    while !r.policy.is_finished() && r.block < max_blocks {
        let k = r.block + 1;
        let through = Sample(U + k * BLOCK);
        r.push_cells(((k - 1) * BLOCK).div_ceil(CELL), (k * BLOCK).div_ceil(CELL));
        if k == PAUSE_BLOCK {
            r.policy.pause_server_asked(through, ms(k * 80 - 60));
        }
        if let Some((before, outcome)) = &answer
            && *before == k
        {
            let answer = PauseServerAnswer {
                pause: pause_through(),
                outcome: outcome.clone().map(str::to_owned),
            };
            r.policy.pause_server_answer(answer, ms(k * 80 - 20));
        }
        let mut obs = r.tl.asr(through);
        if k == PAUSE_BLOCK {
            obs.kind = AsrKind::Finalized;
        }
        r.policy.evidence_mut().push_asr(epoch(), obs).unwrap();
        r.block = k;
        let mut out = Vec::new();
        r.policy.advance(&r.grammar, r.now(), &mut out);
        r.log.extend(out.into_iter().map(|d| (k, d)));
    }
    r
}

fn the_pause(r: &Run) -> PauseRecord {
    let pauses = &r.policy.trace().pauses;
    assert_eq!(pauses.len(), 1, "{pauses:?}");
    pauses[0].clone()
}

#[test]
fn the_servers_words_decide_the_pause_while_the_pause_decode_stands() {
    let r = pause_run(
        misheard(),
        server_config(),
        Some((16, Ok("What time is it?"))),
    );
    // From the block after the answer, the server's words are the prefix; they are complete,
    // so the 608 ms pause, already reached at that block, ends the command there.
    let (k, (route, boundary, since)) = r.pending().expect("pending");
    assert_eq!((k, route, boundary), (16, Route::A, c(21)));
    assert_eq!(since, frontier_of_block(16));
    let (_, commit) = r.commit().expect("commit");
    assert_eq!(commit.text, "what time is it");
    assert_eq!(commit.class, Some(TextClass::Complete));
    assert_eq!(commit.source, TextSource::Server);
    assert_eq!(
        commit.at,
        first_frontier_at_least(since + Samples::from_ms(192)).1
    );
    let pause = the_pause(&r);
    assert_eq!(pause.through, pause_through());
    assert_eq!(pause.kroko.text.as_deref(), Some("watt tie miss it"));
    assert_eq!(pause.kroko.judged.as_deref(), Some("watt tie miss it"));
    assert_eq!(pause.kroko.at, Some(pause_through()));
    let server = pause.server.expect("a server request");
    assert_eq!(server.text.as_deref(), Some("What time is it?"));
    assert_eq!(server.judged.as_deref(), Some("what time is it"));
    assert_eq!(server.at, Some(Sample(U + 16 * BLOCK)));
    // From the request to the answer: 60 ms before block 13 to 20 ms before block 16.
    assert_eq!(server.ms, Some(280));
    assert_eq!(server.error, None);
    assert_eq!(r.policy.trace().heard, "What time is it?");
    let changes: Vec<(&str, TextSource)> = r
        .policy
        .trace()
        .text
        .iter()
        .map(|t| (t.text.as_str(), t.source))
        .collect();
    assert_eq!(
        changes.last(),
        Some(&("what time is it", TextSource::Server))
    );
    // Kroko's live words were the same as its pause decode's, so that text first changed on
    // the streaming result; a commit on it still counts as the pause decode's (below).
    assert!(changes.contains(&("watt tie miss it", TextSource::Streaming)));

    // Kroko alone waits out the 1,792 ms pause its unknown words need, and commits on its
    // pause decode.
    let alone = pause_run(misheard(), TurnConfig::default(), None);
    let (_, kroko) = alone.commit().expect("commit");
    assert_eq!(kroko.text, "watt tie miss it");
    assert_eq!(kroko.source, TextSource::Kroko);
    assert!(kroko.at > commit.at);
    assert_eq!(the_pause(&alone).server, None);
}

#[test]
fn an_answer_in_before_the_pause_decode_is_reported_counts_from_that_block() {
    let r = pause_run(
        misheard(),
        server_config(),
        Some((PAUSE_BLOCK, Ok("What time is it?"))),
    );
    let pause = the_pause(&r);
    // Kroko's words were never judged: the server's were already in.
    assert_eq!(pause.kroko.text.as_deref(), Some("watt tie miss it"));
    assert_eq!(pause.kroko.judged, None);
    assert_eq!(pause.kroko.at, None);
    let server = pause.server.unwrap();
    assert_eq!(server.at, Some(pause_through()));
    assert_eq!(server.ms, Some(40));
    assert_eq!(r.commit().unwrap().1.source, TextSource::Server);
}

#[test]
fn new_speech_supersedes_an_outstanding_request_and_its_answer_is_ignored() {
    // A word added after the pause decode ends it before the answer arrives.
    let tl = misheard().word("now", Sample(U + 15 * BLOCK));
    let r = pause_run(tl, server_config(), Some((16, Ok("What time is it?"))));
    let server = the_pause(&r).server.unwrap();
    assert_eq!(server.error, Some(PauseError::Superseded));
    assert_eq!(server.text, None);
    assert_eq!(server.judged, None);
    // Dropped when block 15 ended the pause decode: 60 ms before block 13 to block 15.
    assert_eq!(server.ms, Some(220));
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.text, "watt tie miss it now");
    assert_eq!(commit.source, TextSource::Streaming);
    // The stored shape: the error is a bare string.
    let json = serde_json::to_value(&r.policy.trace().pauses).unwrap();
    assert_eq!(json[0]["server"]["error"], "superseded");
    assert_eq!(json[0]["kroko"]["error"], serde_json::Value::Null);
    assert_eq!(json[0]["through"], pause_through().0);
}

#[test]
fn a_failed_or_empty_answer_leaves_krokos_words() {
    for (outcome, error, text) in [
        (Err(ServerFailure::Timeout), Some(PauseError::Timeout), None),
        (
            Err(ServerFailure::Failed("could not connect".into())),
            Some(PauseError::Failed("could not connect".into())),
            None,
        ),
        (Ok("  "), None, Some(String::new())),
    ] {
        let r = pause_run(misheard(), server_config(), Some((16, outcome)));
        let server = the_pause(&r).server.unwrap();
        assert_eq!((server.error, server.text), (error, text));
        assert_eq!((server.judged, server.at), (None, None));
        let commit = r.commit().unwrap().1;
        assert_eq!(commit.text, "watt tie miss it");
        assert_eq!(commit.source, TextSource::Kroko);
        assert_eq!(r.policy.trace().heard, "watt tie miss it");
    }
}

#[test]
fn an_answer_after_the_decision_is_not_recorded() {
    let mut r = pause_run(misheard(), server_config(), None);
    let commit = r.commit().unwrap().1;
    assert_eq!(commit.source, TextSource::Kroko);
    let answer = PauseServerAnswer {
        pause: pause_through(),
        outcome: Ok("What time is it?".into()),
    };
    r.policy.pause_server_answer(answer, ms(10_000));
    r.step();
    let server = the_pause(&r).server.unwrap();
    assert!(server.is_open(), "{server:?}");
}

#[test]
fn words_that_arrive_in_the_lookahead_do_not_change_the_commit() {
    // Kroko's words are complete, so its pause decode sets the end; the server's differ and
    // arrive while the end is pending. The deployed controller committed the decision's
    // text and class (turns 688, 755, 852, 953, 811 and 832) and asked HA's matcher about
    // the new words only on a block that did not commit (688 and 852 asked, 811 and 832 did
    // not).
    let grammar = || Grammar::default().with("watt tie miss it", TextClass::Complete);
    let config = TurnConfig {
        recognizer_available: true,
        ..server_config()
    };
    let run = |answer| pause_run_with(misheard(), config, grammar(), answer);
    let base = run(None);
    let (decided, _) = base.pending().expect("pending");
    let (committed, _) = base.commit().expect("commit");
    assert!(committed > decided + 1);
    let asked = |r: &Run, text: &str| {
        r.log
            .iter()
            .any(|(_, d)| matches!(d, Decision::AskRecognizer { text: t } if t == text))
    };
    for (arrives, asks) in [(decided + 1, true), (committed, false)] {
        let r = run(Some((arrives, Ok("What's what time is it?"))));
        assert_eq!(r.pending().unwrap().0, decided);
        let (at, commit) = r.commit().expect("commit");
        assert_eq!(at, committed);
        assert_eq!(commit.text, "watt tie miss it");
        assert_eq!(commit.class, Some(TextClass::Complete));
        assert_eq!(commit.source, TextSource::Kroko);
        // The words are still recorded as judged where they arrived.
        let server = the_pause(&r).server.unwrap();
        assert_eq!(server.judged.as_deref(), Some("what's what time is it"));
        assert_eq!(server.at, Some(Sample(U + arrives * BLOCK)));
        assert_eq!(
            asked(&r, "what's what time is it"),
            asks,
            "arriving at {arrives}"
        );
    }
}
