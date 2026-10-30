//! Offline evaluation of the endpoint reducer on saved wake-turn recordings.
//!
//! A wake turn's recording starts at its mic stream's start, `round_down(support_start − 4800,
//! 512)` (checked against the rows' own cell segments: loud cells line up with the recorded
//! command speech only at that offset), and its decision-trace row gives the candidate's support
//! start, hops and trigger. Button and reply recordings have no start this tool can recover from
//! the row, so they are left out.
//!
//! For each recording the tool plays the speech worker: the device's loudness per 512-sample
//! cell, Silero VAD on the +30 dB VAD input, and Kroko on the +20 dB evidence copy in 80 ms
//! blocks, with a fresh pause decode 320 ms into each pause. The recordings carry no reference
//! audio, so every echo label is unknown. Audio is used only until the field's response became
//! audible; after that the device's own reply is in the mic. The tool feeds the reducer and
//! reports where it would commit relative to the last command token of a full decode of that
//! audio, under three completeness stand-ins:
//!
//! - `Unknown`: every text is unknown (the 1,792 ms pause, the latest any route waits);
//! - `Terminal`: only the spec's terminal-token rule (a text ending in a, an, the, for, to, in,
//!   on, at, and, or, with is never complete: `needs_more`), everything else unknown;
//! - `Complete`: everything complete except the terminal-token rule: the shortest pause, so the
//!   earliest any grammar could cut.
//!
//! The rows' outcomes are used only to locate audio and to label field situations for
//! discussion. Nothing here is committed: the recordings and rows are private.
//!
//! ```text
//! SHERPA_ONNX_LIB_DIR=… LD_LIBRARY_PATH=… cargo run -p echomuse-turn --features evaluate \
//!   --release --example evaluate_recordings -- --db DB --recordings DIR --kroko DIR \
//!   --silero FILE --ort-lib FILE --manifest controller/speech_bundle.json [--threads N] \
//!   [--out FILE] [--margin-ms MS]
//! ```
//!
//! `--margin-ms` replaces the policy's margin after the last command word's emission, to
//! compare values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use echomuse_speech::{
    AsrResult, KrokoModelFiles, KrokoRecognizer, MonoWav, NativeLibraries, OrtRuntime, PinnedFile,
    SherpaRuntime, SileroVad, SpeechBundleManifest,
};
use echomuse_turn::text::{TimedTranscript, locate_in_streaming, normalize};
use echomuse_turn::{
    AsrKind, AsrObservation, CellIndex, Completeness, Decision, DeviceCell, DeviceInput, EchoCell,
    EndBound, Epoch, MonoInstant, NoGrammar, PauseFinalizer, Sample, Samples, TextClass,
    TurnConfig, TurnKind, TurnPolicy, TurnSpec, VadInput, WakeAnchor,
};
use serde_json::Value;

const CELL: u64 = 512;
const BLOCK: u64 = 1_280;
/// The flush the spec gives every fresh decode: 1.5 s of zeros, never counted as audio.
const FLUSH: usize = 24_000;
const PHRASE: &str = "ophelia";
const TERMINAL: [&str; 11] = [
    "a", "an", "the", "for", "to", "in", "on", "at", "and", "or", "with",
];

struct Args {
    db: PathBuf,
    recordings: PathBuf,
    kroko: PathBuf,
    silero: PathBuf,
    ort_lib: PathBuf,
    manifest: PathBuf,
    threads: usize,
    out: Option<PathBuf>,
    margin_ms: Option<u64>,
}

fn args() -> Args {
    let mut map = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let v = it.next().unwrap_or_else(|| panic!("{k} needs a value"));
        map.insert(k.trim_start_matches("--").to_owned(), v);
    }
    let get = |k: &str| {
        PathBuf::from(
            map.get(k)
                .unwrap_or_else(|| panic!("--{k} is required"))
                .clone(),
        )
    };
    Args {
        db: get("db"),
        recordings: get("recordings"),
        kroko: get("kroko"),
        silero: get("silero"),
        ort_lib: get("ort-lib"),
        manifest: get("manifest"),
        threads: map.get("threads").map_or(4, |t| t.parse().unwrap()),
        out: map.get("out").map(PathBuf::from),
        margin_ms: map.get("margin-ms").map(|m| m.parse().unwrap()),
    }
}

/// The three completeness stand-ins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Unknown,
    Terminal,
    Complete,
}

const MODES: [Mode; 3] = [Mode::Unknown, Mode::Terminal, Mode::Complete];

struct Stand(Mode);

impl Completeness for Stand {
    fn classify(&self, text: &str) -> TextClass {
        let terminal = text
            .rsplit(' ')
            .next()
            .is_some_and(|w| TERMINAL.contains(&w));
        match self.0 {
            Mode::Unknown => NoGrammar.classify(text),
            _ if terminal => TextClass::NeedsMore,
            Mode::Terminal => TextClass::Unknown,
            Mode::Complete => TextClass::Complete,
        }
    }
}

/// One wake row: where its recording and utterance sit, and what happened in the field.
#[derive(Clone, Debug)]
struct Row {
    id: i64,
    file: PathBuf,
    support_start: u64,
    open: u64,
    /// The mic start, where the recording and the utterance begin.
    start: u64,
    trigger: u64,
    /// The field's commit: boundary, span end, and the frontier it committed at.
    field_commit: Option<(u64, u64, u64)>,
    first_audio_ms: Option<u64>,
    terminal: String,
    endpoint_class: Option<String>,
    stt_raw: Option<String>,
}

fn u(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn rows(args: &Args) -> (Vec<Row>, BTreeMap<String, usize>) {
    let db =
        rusqlite::Connection::open_with_flags(&args.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = db
        .prepare(
            "select id, device_id, decision_trace, terminal_reason, endpoint_class, stt_raw \
             from turns where decision_trace is not null order by id",
        )
        .unwrap();
    let mut skipped = BTreeMap::new();
    let mut skip = |why: &str| *skipped.entry(why.to_owned()).or_insert(0) += 1;
    let mut out = Vec::new();
    let mut q = stmt.query([]).unwrap();
    while let Some(r) = q.next().unwrap() {
        let id: i64 = r.get(0).unwrap();
        let device: String = r.get(1).unwrap();
        let file = args.recordings.join(format!("{device}_{id}.wav"));
        if !file.exists() {
            skip("no recording");
            continue;
        }
        let trace: Value = serde_json::from_str(&r.get::<_, String>(2).unwrap()).unwrap();
        if trace["trigger"] != "wake" {
            skip("button or reply: recording start not recoverable");
            continue;
        }
        let utt = &trace["utterance"];
        let support = u(&trace["afe_evidence"]["support_start"]);
        let open = trace["hops"]
            .as_array()
            .and_then(|h| h.last())
            .and_then(|h| u(&h["end_sample"]));
        let (Some(trigger), Some(support_start), Some(open)) = (u(&utt["trigger"]), support, open)
        else {
            skip("wake row without trigger, support or hops");
            continue;
        };
        let events = utt["endpoint"].as_array().cloned().unwrap_or_default();
        let since = events
            .iter()
            .filter(|e| e["event"] == "pending")
            .filter_map(|e| u(&e["since"]))
            .next_back();
        let field_commit = events
            .iter()
            .find(|e| e["event"] == "commit")
            .and_then(|e| {
                let end = u(&e["end"])?;
                Some((u(&e["boundary"])?, end, since.map_or(end, |s| s + 3_072)))
            });
        out.push(Row {
            id,
            file,
            support_start,
            open,
            start: TurnSpec::candidate_mic_start(Sample(support_start)).0,
            trigger,
            field_commit,
            first_audio_ms: u(&trace["first_audio_ms"]),
            terminal: r.get::<_, Option<String>>(3).unwrap().unwrap_or_default(),
            endpoint_class: r.get(4).unwrap(),
            stt_raw: r.get(5).unwrap(),
        });
    }
    (out, skipped)
}

/// Canonical PCM at a fixed linear gain, saturated to int16, as floats.
fn gained(pcm: &[i16], gain: f64) -> Vec<f32> {
    pcm.iter()
        .map(|&s| {
            let v = (f64::from(s) * gain).clamp(-32_768.0, 32_767.0);
            (v / 32_768.0) as f32
        })
        .collect()
}

/// The evidence the worker would have produced for one recording.
struct Evidence {
    /// Device loudness per cell from the start.
    energy: Vec<i16>,
    /// VAD per cell from the start.
    vad: Vec<f32>,
    /// One ASR result per block.
    blocks: Vec<AsrObservation>,
    /// Tokens of a full decode of the span, for labelling.
    full: Vec<(String, u64)>,
    /// The evidence copy, to re-decode committed spans.
    copy: Vec<f32>,
}

/// A result's tokens with their emission samples.
fn timed(row: &Row, r: &AsrResult) -> Vec<(String, u64)> {
    r.tokens
        .iter()
        .zip(&r.timestamps)
        .map(|(t, s)| {
            (
                t.clone(),
                row.start + (f64::from(*s) * 16_000.0).round() as u64,
            )
        })
        .collect()
}

fn decode_fresh(kroko: &KrokoRecognizer, audio: &[f32]) -> AsrResult {
    let mut s = kroko.create_stream().unwrap();
    s.accept(audio).unwrap();
    s.accept(&vec![0.0; FLUSH]).unwrap();
    s.decode_ready();
    s.result().unwrap()
}

fn evidence(row: &Row, pcm: &[i16], kroko: &KrokoRecognizer, vad: &mut SileroVad) -> Evidence {
    let energy = pcm
        .as_chunks::<{ CELL as usize }>()
        .0
        .iter()
        .map(|c| DeviceCell::loudness_centi(c))
        .collect();
    vad.reset();
    let vad_input = gained(pcm, 10f64.powf(1.5));
    let probs: Vec<f32> = vad_input
        .as_chunks::<{ CELL as usize }>()
        .0
        .iter()
        .map(|c| vad.process_cell(c).unwrap())
        .collect();
    let copy = gained(pcm, 10.0);
    let mut stream = kroko.create_stream().unwrap();
    let mut finalizer = PauseFinalizer::new(0.5);
    let mut blocks = Vec::new();
    let mut observed = 0usize;
    for (k, block) in copy.as_chunks::<{ BLOCK as usize }>().0.iter().enumerate() {
        let k = k as u64 + 1;
        let through = row.start + k * BLOCK;
        stream.accept(block).unwrap();
        stream.decode_ready();
        let complete_cells = ((k * BLOCK) / CELL) as usize;
        while observed < complete_cells.min(probs.len()) {
            let cell = CellIndex(row.start / CELL + observed as u64);
            finalizer.observe_cell(cell, Some(probs[observed]));
            observed += 1;
        }
        let (r, kind) = if finalizer.due() {
            (
                decode_fresh(kroko, &copy[..(k * BLOCK) as usize]),
                AsrKind::Finalized,
            )
        } else {
            (stream.result().unwrap(), AsrKind::Live)
        };
        blocks.push(AsrObservation::from_recognizer(
            Sample(row.start),
            Sample(through),
            &r.tokens,
            &r.timestamps,
            r.num_trailing_blanks,
            kind,
        ));
    }
    let full = decode_fresh(kroko, &copy);
    Evidence {
        energy,
        vad: probs,
        blocks,
        full: timed(row, &full),
        copy,
    }
}

/// The command words of a fresh decode of the committed span `[start, end)`, as the spec's
/// re-check would hear them.
fn span_words(row: &Row, ev: &Evidence, end: u64, kroko: &KrokoRecognizer) -> String {
    let r = decode_fresh(kroko, &ev.copy[..(end - row.start) as usize]);
    words_of(&command_tokens(row, &timed(row, &r)))
}

#[derive(Clone, Debug, Default)]
struct Outcome {
    /// A route ("A", "B", "Fallback"), a close reason, or "undecided".
    result: String,
    boundary: Option<u64>,
    span_end: Option<u64>,
    at: Option<u64>,
    class: Option<TextClass>,
    text: String,
    /// The command words of a fresh decode of the committed span.
    span_words: Option<String>,
    /// Which bound set the committed audio's end.
    bound: Option<EndBound>,
}

fn replay(row: &Row, ev: &Evidence, mode: Mode, config: TurnConfig) -> Outcome {
    let epoch = Epoch::new(1).unwrap();
    let start = Sample(row.start);
    let spec = TurnSpec {
        epoch,
        start,
        trigger: Sample(row.trigger),
        cells_from: start.cell(),
        vad_from: start.cell(),
        kind: TurnKind::Wake(WakeAnchor {
            support_start: Sample(row.support_start),
            candidate_open: Sample(row.open),
            phrase: PHRASE.into(),
        }),
    };
    let mut p = TurnPolicy::new(config, spec, MonoInstant(0)).unwrap();
    let first = start.cell().0;
    let device: Vec<DeviceInput> = ev
        .energy
        .iter()
        .map(|&e| DeviceInput::Cell(DeviceCell::audio(e, 0)))
        .collect();
    let vads: Vec<VadInput> = ev
        .vad
        .iter()
        .map(|&v| VadInput::probability(v).unwrap())
        .collect();
    let grammar = Stand(mode);
    let mut out = Vec::new();
    let mut pushed = 0usize;
    for (k, obs) in ev.blocks.iter().enumerate() {
        // Hand the cells to the policy as the workers would: loudness, VAD and echo per
        // completed cell, then the block's ASR result.
        let cells = (((obs.through.0 - row.start) / CELL) as usize).min(vads.len());
        let at = CellIndex(first + pushed as u64);
        let ev_in = p.evidence_mut();
        ev_in
            .push_device_cells(epoch, at, &device[pushed..cells])
            .unwrap();
        ev_in.push_vad(epoch, at, &vads[pushed..cells]).unwrap();
        ev_in
            .push_echo(epoch, at, &vec![EchoCell::MISSING; cells - pushed])
            .unwrap();
        pushed = cells;
        ev_in.push_asr(epoch, obs.clone()).unwrap();
        p.advance(&grammar, MonoInstant((k as u64 + 1) * 80_000_000), &mut out);
        if p.is_finished() {
            break;
        }
    }
    let mut o = Outcome {
        result: "undecided".into(),
        ..Outcome::default()
    };
    for d in out {
        match d {
            Decision::Commit(c) => {
                o.result = format!("{:?}", c.route);
                o.boundary = Some(c.boundary.0);
                o.span_end = Some(c.span.end.0);
                o.at = Some(c.at.0);
                o.class = c.class;
                o.bound = Some(c.end_bound);
                o.text = c.text;
            }
            Decision::Close { reason, at } => {
                o.result = format!("{reason:?}");
                o.at = at.map(|a| a.0);
            }
            _ => {}
        }
    }
    o
}

/// The command tokens of the full decode: after the wake phrase (§16.6 step 2), emitted after the
/// trigger, and not punctuation. Returns their emission samples and the words.
fn command_tokens(row: &Row, full: &[(String, u64)]) -> Vec<(String, u64)> {
    let t = TimedTranscript::from_tokens(full.iter().map(|(s, e)| (s.as_str(), Sample(*e))));
    let w = locate_in_streaming(&t, Sample(row.open) + Samples::from_ms(480), PHRASE);
    let mut skip_until = 0usize;
    if let Some(win) = w.window {
        let last_word_end = t.words()[win.end - 1].end;
        let mut bytes = 0usize;
        for (i, (s, _)) in full.iter().enumerate() {
            bytes += s.len();
            if bytes >= last_word_end {
                skip_until = i + 1;
                break;
            }
        }
    }
    full[skip_until..]
        .iter()
        .filter(|(s, e)| *e > row.trigger && s.chars().any(char::is_alphanumeric))
        .cloned()
        .collect()
}

/// Normalized words of a token sequence.
fn words_of(tokens: &[(String, u64)]) -> String {
    normalize(&tokens.iter().map(|(s, _)| s.as_str()).collect::<String>())
}

/// The end of the last speech-positive cell (VAD ≥ 0.5) after the trigger.
fn last_voiced(row: &Row, ev: &Evidence) -> Option<u64> {
    ev.vad
        .iter()
        .enumerate()
        .rev()
        .map(|(i, p)| (row.start + (i as u64 + 1) * CELL, *p))
        .find(|(end, p)| *p >= 0.5 && *end > row.trigger)
        .map(|(end, _)| end)
}

/// How far the audio of the last command word runs past Kroko's emission of it, in ms (negative
/// when the word's audio ended before its emission), for each loudness bar over the room.
///
/// Only a final word with no other token emitted within 1 s after it counts, so the stretch
/// measured is that word's. The room level is the 20th percentile of the cells after the
/// trigger. The word's audio is the stretch of cells at least `above` dB over the room that holds
/// the emission (gaps of up to two cells inside a word allowed), or, when the emission falls in
/// a quiet cell, the nearest such stretch ending within 1 s before it.
fn emission_leads(row: &Row, ev: &Evidence, cmd: &[(String, u64)]) -> Option<[i64; 2]> {
    let (_, last) = cmd.last()?;
    if ev
        .full
        .iter()
        .any(|(_, e)| *e > *last && *e <= last + 16_000)
    {
        return None;
    }
    let first_after = (row.trigger.saturating_sub(row.start) / CELL) as usize;
    let mut after: Vec<i16> = ev.energy.get(first_after..)?.to_vec();
    if after.len() < 32 {
        return None;
    }
    after.sort_unstable();
    let room = f64::from(after[after.len() / 5]) / 100.0;
    let at = ((last - row.start) / CELL) as usize;
    let mut out = [0i64; 2];
    for (slot, above) in out.iter_mut().zip([6.0, 10.0]) {
        let loud = |i: usize| {
            ev.energy
                .get(i)
                .is_some_and(|&e| f64::from(e) / 100.0 >= room + above)
        };
        // The loud cell holding the emission, or the nearest one before it within 1 s.
        let mut i = (at.saturating_sub(31)..=at).rev().find(|&i| loud(i))?;
        // Walk forward through the word, bridging dips of up to two cells.
        while loud(i + 1) || loud(i + 2) || loud(i + 3) {
            i += if loud(i + 1) {
                1
            } else if loud(i + 2) {
                2
            } else {
                3
            };
            if i > at + 31 {
                return None;
            }
        }
        let end = row.start + (i as u64 + 1) * CELL;
        *slot = (end as i64 - *last as i64) / 16;
    }
    Some(out)
}

struct Evaluated {
    row: Row,
    /// The command tokens of the full decode, with their emission samples.
    cmd: Vec<(String, u64)>,
    first_cmd: Option<u64>,
    last_cmd: Option<u64>,
    last_voiced: Option<u64>,
    /// [`emission_leads`] at 6 and 10 dB over the room.
    leads: Option<[i64; 2]>,
    words: String,
    outcomes: Vec<(Mode, Outcome)>,
}

/// Tokens the full decode emits within this long after a committed span's end belong to the
/// same stretch of speech as the cut; later ones are speech after a pause.
const SAME_STRETCH: u64 = 600 * 16;

fn percentiles(mut v: Vec<i64>) -> String {
    if v.is_empty() {
        return "n=0".into();
    }
    v.sort_unstable();
    let p = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    format!(
        "n={} min={} p10={} p25={} p50={} p75={} p90={} p95={} max={}",
        v.len(),
        v[0],
        p(0.1),
        p(0.25),
        p(0.5),
        p(0.75),
        p(0.9),
        p(0.95),
        v[v.len() - 1]
    )
}

/// Samples to milliseconds.
fn ms(samples: i64) -> i64 {
    samples / 16
}

fn main() {
    let args = args();
    let manifest = SpeechBundleManifest::load(&args.manifest).unwrap();
    let native = NativeLibraries::pinned(&args.ort_lib).unwrap();
    let ort = OrtRuntime::load(&native.onnxruntime).unwrap();
    let sherpa = SherpaRuntime::verify(&native.sherpa).unwrap();
    let pin = |name: &str, d| PinnedFile::new(args.kroko.join(name), d);
    let files = KrokoModelFiles {
        encoder: pin("encoder.onnx", manifest.asr.files.encoder.sha256),
        decoder: pin("decoder.onnx", manifest.asr.files.decoder.sha256),
        joiner: pin("joiner.onnx", manifest.asr.files.joiner.sha256),
        tokens: pin("tokens.txt", manifest.asr.files.tokens.sha256),
    };
    let kroko = KrokoRecognizer::load(&sherpa, &files).unwrap();
    let silero = PinnedFile::new(&args.silero, manifest.vad.sha256);
    let mut config = TurnConfig::default();
    if let Some(m) = args.margin_ms {
        config.endpoint.asr_word_margin = Samples::from_ms(m);
    }
    println!(
        "margin after the last command word: {} ms",
        config.endpoint.asr_word_margin.as_ms()
    );

    let (rows, skipped) = rows(&args);
    println!(
        "wake recordings evaluated: {}; skipped {skipped:?}",
        rows.len()
    );
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..args.threads {
            s.spawn(|| {
                let mut vad = SileroVad::load(&ort, &silero).unwrap();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(row) = rows.get(i) else { break };
                    let wav = MonoWav::read(&row.file).unwrap();
                    assert_eq!(wav.sample_rate, 16_000);
                    let rec_end = row.start + wav.samples.len() as u64;
                    // Room audio until the response became audible; after it the device's own
                    // reply is in the mic with no reference to label it.
                    let end = match (row.field_commit, row.first_audio_ms) {
                        (Some((_, _, at)), Some(fa)) => (at + fa * 16).min(rec_end),
                        _ => rec_end,
                    };
                    let pcm = &wav.samples[..(end - row.start) as usize];
                    let ev = evidence(row, pcm, &kroko, &mut vad);
                    let cmd = command_tokens(row, &ev.full);
                    let words = words_of(&cmd);
                    let outcomes = MODES
                        .iter()
                        .map(|&m| {
                            let mut o = replay(row, &ev, m, config);
                            o.span_words = o.span_end.map(|e| span_words(row, &ev, e, &kroko));
                            (m, o)
                        })
                        .collect();
                    let leads = emission_leads(row, &ev, &cmd);
                    results.lock().unwrap().push(Evaluated {
                        row: row.clone(),
                        first_cmd: cmd.first().map(|t| t.1),
                        last_cmd: cmd.last().map(|t| t.1),
                        cmd,
                        last_voiced: last_voiced(row, &ev),
                        leads,
                        words,
                        outcomes,
                    });
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.row.id);
    report(&results);
    if let Some(path) = &args.out {
        write_details(path, &results);
    }
}

fn outcome(r: &Evaluated, mode: Mode) -> &Outcome {
    &r.outcomes.iter().find(|(m, _)| *m == mode).unwrap().1
}

fn report(results: &[Evaluated]) {
    let with_words = results.iter().filter(|r| r.last_cmd.is_some()).count();
    println!(
        "with command tokens in the full decode: {with_words}; without: {}",
        results.len() - with_words
    );
    for (i, bar) in [6, 10].iter().enumerate() {
        let v: Vec<i64> = results
            .iter()
            .filter_map(|r| r.leads.map(|l| l[i]))
            .collect();
        println!(
            "final-word audio end − its emission (ms), cells ≥ room + {bar} dB: {}",
            percentiles(v)
        );
    }
    let mut field: BTreeMap<String, usize> = BTreeMap::new();
    for r in results {
        *field.entry(r.row.terminal.clone()).or_default() += 1;
    }
    println!("field terminals: {field:?}");
    for mode in MODES {
        println!("\n=== completeness stand-in: {mode:?} ===");
        let mut outcomes: BTreeMap<String, usize> = BTreeMap::new();
        let mut margin = Vec::new();
        let mut voiced_margin = Vec::new();
        let mut latency: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        let mut word_cut = Vec::new();
        let mut later = Vec::new();
        let mut vs_field = Vec::new();
        for r in results {
            let o = outcome(r, mode);
            let words = if r.last_cmd.is_some() {
                "words"
            } else {
                "no words"
            };
            *outcomes
                .entry(format!("{words}: {}", o.result))
                .or_default() += 1;
            if let (Some(end), Some(last)) = (o.span_end, r.last_cmd) {
                let m = ms(end as i64 - last as i64);
                margin.push(m);
                // A cut word: the full decode has more command words emitted by 600 ms after
                // the span's end than a fresh decode of the committed span itself hears.
                let span = o.span_words.clone().unwrap_or_default();
                let near: Vec<(String, u64)> = r
                    .cmd
                    .iter()
                    .filter(|t| t.1 <= end + SAME_STRETCH)
                    .cloned()
                    .collect();
                let near = words_of(&near);
                let count = |s: &str| s.split(' ').filter(|w| !w.is_empty()).count();
                if count(&near) > count(&span) {
                    word_cut.push((r.row.id, span.clone(), near));
                }
                if r.cmd.iter().any(|t| t.1 > end + SAME_STRETCH) {
                    later.push((r.row.id, o.text.clone(), r.words.clone()));
                }
                let class = format!("{:?}", o.class.unwrap());
                latency
                    .entry(class)
                    .or_default()
                    .push(ms(o.at.unwrap() as i64 - last as i64));
            }
            if let (Some(end), Some(v)) = (o.span_end, r.last_voiced) {
                voiced_margin.push(ms(end as i64 - v as i64));
            }
            if let (Some(end), Some((_, field_end, _))) = (o.span_end, r.row.field_commit) {
                vs_field.push(ms(end as i64 - field_end as i64));
            }
        }
        for (k, v) in &outcomes {
            println!("  {k}: {v}");
        }
        println!(
            "  span end − last command token emission (ms): {}",
            percentiles(margin.clone())
        );
        println!(
            "  span end − end of last speech-positive cell (ms): {}",
            percentiles(voiced_margin)
        );
        println!(
            "  word cut (the full decode has more words by 600 ms after the span than the span's own decode): {}",
            word_cut.len()
        );
        println!(
            "  commits followed by more command speech after 600 ms: {}",
            later.len()
        );
        for (class, v) in latency {
            println!(
                "  commit decision − last command token (ms), class {class}: {}",
                percentiles(v)
            );
        }
        println!(
            "  our span end − field span end (ms): {}",
            percentiles(vs_field)
        );
        let asr_bound = results
            .iter()
            .filter(|r| outcome(r, mode).bound == Some(EndBound::AsrWord))
            .count();
        println!("  commits whose end the last ASR word set: {asr_bound}");
        println!("  named rows:");
        for r in results
            .iter()
            .filter(|r| [642, 673, 688, 803, 915].contains(&r.row.id))
        {
            let o = outcome(r, mode);
            let vs_field = match (o.span_end, r.row.field_commit) {
                (Some(e), Some((_, f, _))) => format!("{:+}", ms(e as i64 - f as i64)),
                _ => "-".into(),
            };
            println!(
                "    row {}: {} bound={:?} span end vs field {vs_field} ms; span decodes {:?}; full decode {:?}",
                r.row.id,
                o.result,
                o.bound,
                o.span_words.as_deref().unwrap_or(""),
                r.words
            );
        }
        println!("  word cuts:");
        for (id, span, near) in &word_cut {
            println!("    row {id}: span decodes {span:?}; full decode to +600 ms {near:?}");
        }
        println!("  speech after the commit:");
        for (id, text, words) in &later {
            println!("    row {id}: committed {text:?}; full decode {words:?}");
        }
    }
    println!("\n=== field situations ===");
    let mid = |s: &str| {
        normalize(s)
            .rsplit(' ')
            .next()
            .is_some_and(|x| TERMINAL.contains(&x))
    };
    for r in results {
        let stt = r.row.stt_raw.as_deref().unwrap_or("");
        let interesting = mid(stt)
            || matches!(
                r.row.terminal.as_str(),
                "retry" | "too_long" | "no_input" | "interrupted"
            );
        if !interesting {
            continue;
        }
        let outs: Vec<String> = r
            .outcomes
            .iter()
            .map(|(m, o)| {
                let margin = match (o.span_end, r.last_cmd) {
                    (Some(e), Some(l)) => format!(" {:+}ms", ms(e as i64 - l as i64)),
                    _ => String::new(),
                };
                format!("{m:?}={}{margin}", o.result)
            })
            .collect();
        println!(
            "  row {} field={} class={:?} stt={stt:?} full={:?} | {}",
            r.row.id,
            r.row.terminal,
            r.row.endpoint_class,
            r.words,
            outs.join(", ")
        );
    }
}

fn write_details(path: &Path, results: &[Evaluated]) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).unwrap();
    for r in results {
        let outs: Vec<Value> = r
            .outcomes
            .iter()
            .map(|(m, o)| {
                serde_json::json!({
                    "mode": format!("{m:?}"), "result": o.result, "boundary": o.boundary,
                    "span_end": o.span_end, "at": o.at, "class": o.class, "text": o.text,
                })
            })
            .collect();
        let line = serde_json::json!({
            "id": r.row.id, "start": r.row.start, "trigger": r.row.trigger,
            "first_cmd": r.first_cmd, "last_cmd": r.last_cmd, "last_voiced": r.last_voiced,
            "words": r.words, "field_terminal": r.row.terminal, "field_commit": r.row.field_commit,
            "outcomes": outs,
        });
        writeln!(f, "{line}").unwrap();
    }
}
