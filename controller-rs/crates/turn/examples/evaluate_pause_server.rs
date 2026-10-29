//! Offline check of pause transcription by a server (Rust controller evaluation §4.7, §6.1)
//! against field turns: does this controller end each turn where, and on whose words, the
//! deployed controller did?
//!
//! For every stored wake turn whose decision trace has pause server words and whose turn
//! recording is kept, the tool plays the speech worker over the recording (device loudness
//! per 512-sample cell, Silero VAD on the +30 dB input, Kroko from `support_start − 4800` on
//! the +20 dB evidence copy, looked at after every mic packet the device sent, with a fresh
//! pause decode 320 ms into each pause) and runs the real turn policy with the real grammar.
//! The packets are rebuilt as the device cut them: 1,280 samples from the mic start through
//! the candidate's backfill, then 1,280 samples on the absolute grid ([`live_from`]); every
//! block boundary in the stored traces lies on those packet ends. Outside answers come from
//! the stored trace, never from a server: each of this run's pause decodes that lines up
//! with a stored pause gets that pause's stored server words just before the block they
//! were judged at (`at`), a stored failure at its stored time, and nothing for a request the
//! field dropped or answered after the decision; each question to HA's sentence matcher gets
//! the stored class for the same text, from the block after the one it arrived at. No echo
//! labels exist offline (unknown).
//! The grammar runs with HA's names, areas and floors from `--vocabulary` (a JSON object of
//! `names`, `areas` and `floors` string arrays, as Home Assistant exposed them), and
//! without them (open lists) when it is not given.
//!
//! The field's commit is its last pending end (route, boundary, `since`, which is the end of
//! the block it was decided in) and its `source`; this run's is compared by the same block.
//! The tool reports how often this run's agree, within one 32 ms cell and within 200 ms,
//! and groups the rest by cause. The same turns run once more with Kroko alone, to show what
//! the server's words change. `--texts` writes every text the run judged with its class,
//! and every question it put to HA's matcher. Nothing is written into the repository: the
//! rows and recordings are private.
//!
//! ```text
//! cargo run -p echomuse-turn --features evaluate --release --example evaluate_pause_server -- \
//!   --db DB --recordings DIR --kroko DIR --silero FILE --ort-lib FILE --manifest FILE \
//!   [--threads N] [--out FILE] [--texts FILE] [--vocabulary FILE]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use echomuse_grammar::{Completeness as G, Grammar, Vocabulary};
use echomuse_speech::{
    AsrResult, KrokoModelFiles, KrokoRecognizer, MonoWav, NativeLibraries, OrtRuntime, PinnedFile,
    SherpaRuntime, SileroVad, SpeechBundleManifest,
};
use echomuse_turn::{
    AsrKind, AsrObservation, CellIndex, Completeness, Decision, DeviceCell, DeviceInput, EchoCell,
    Epoch, MonoInstant, PauseFinalizer, PauseServerAnswer, RecognizerAnswer, Route, Sample,
    ServerFailure, TextClass, TextSource, TurnConfig, TurnKind, TurnPolicy, TurnSpec,
    UtteranceTrace, VadInput, WakeAnchor, WakePolicy,
};
use serde_json::Value;

const CELL: u64 = 512;
const BLOCK: u64 = 1_280;
const FLUSH: usize = 24_000;
const PHRASE: &str = "ophelia";
/// 200 ms in samples.
const NEAR: u64 = 3_200;

struct RealGrammar(Grammar);

impl Row {
    /// Where the streaming recognizer starts, as this controller places it
    /// ([`TurnSpec::wake_start`]); it lies in the recording, which starts at the mic start.
    fn utterance_start(&self) -> u64 {
        TurnSpec::wake_start(Sample(self.support_start)).0
    }
}

/// The stored pause this run's pause decode through `run` stands for: the closest within one
/// block. Both look at the utterance at the same packet ends, so a pause decode on the same
/// VAD lands on the same block; one block either way allows for VAD that differs at the
/// threshold.
fn stored_pause(pauses: &[(u64, Stored)], run: u64) -> Option<&(u64, Stored)> {
    pauses
        .iter()
        .filter(|(t, _)| t.abs_diff(run) <= BLOCK)
        .min_by_key(|(t, _)| t.abs_diff(run))
}

/// Where the device's live mic packets began: the end of the candidate's backfill. The
/// device sends the backfill in 1,280-sample packets from the mic start, up to where its ring
/// stood when the controller accepted the candidate, and then live packets of 1,280 samples
/// from there; the ring on an Echo Dot ends on the absolute 1,280-sample grid. The ring
/// position is not stored, so it is placed at the first grid point at or after both the
/// candidate's open and every stored block boundary that lies on the backfill's grid only.
/// It matters only when the two grids differ, and only for the first few blocks after the
/// open.
fn live_from(start: u64, open: u64, boundaries: &[u64]) -> u64 {
    let backfill_only = boundaries
        .iter()
        .copied()
        .filter(|b| b.abs_diff(start) % BLOCK == 0 && b % BLOCK != 0)
        .max()
        .unwrap_or(0);
    open.max(backfill_only).div_ceil(BLOCK) * BLOCK
}

/// The ends of the device's mic packets after `from`, up to `end`.
fn packet_ends(row: &Row, from: u64, end: u64) -> impl Iterator<Item = u64> {
    let backfill = (1..)
        .map(move |k| row.start + k * BLOCK)
        .take_while(move |e| *e < row.live_from);
    let live = (0..)
        .map(move |k| row.live_from + k * BLOCK)
        .take_while(move |e| *e <= end);
    backfill.chain(live).filter(move |e| *e > from && *e <= end)
}

/// How much mic the speech worker already holds when the utterance starts. It reads that
/// pre-roll as one block, and its pause check never counts the pre-roll's VAD. A candidate
/// accepted at once starts its turn before its backfill arrives, so nothing is held; one the
/// controller verified first (`producing_sound`) starts once the verifier has heard 480 ms
/// past the open, so the pre-roll runs to the first packet end at or after that. The
/// deployed controller behaved the same: none of its verified turns has a pause decode or a
/// question to HA within ten blocks of the trigger.
fn preroll_through(row: &Row, verified: bool) -> Option<u64> {
    let heard = row.open + WakePolicy::default().verification_lookahead.0;
    verified
        .then(|| packet_ends(row, heard - 1, u64::MAX).next())
        .flatten()
}

impl Completeness for RealGrammar {
    fn classify(&self, text: &str) -> TextClass {
        match self.0.classify(text) {
            G::Complete => TextClass::Complete,
            G::Extendable => TextClass::Extendable,
            G::NeedsMore => TextClass::NeedsMore,
            G::Unknown => TextClass::Unknown,
        }
    }
}

fn u(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn class(v: &Value) -> Option<TextClass> {
    match v.as_str()? {
        "complete" => Some(TextClass::Complete),
        "extendable" => Some(TextClass::Extendable),
        "needs_more" => Some(TextClass::NeedsMore),
        "unknown" => Some(TextClass::Unknown),
        _ => None,
    }
}

/// What the stored trace says the server did for one pause.
#[derive(Clone, Debug)]
enum Stored {
    /// Words judged from block `at`.
    Judged { text: String, at: u64 },
    /// Words that came back but were never judged (empty, or after the decision): `ms` after.
    Unjudged { text: String, ms: Option<u64> },
    /// A failure `ms` after the request.
    Failed { error: String, ms: Option<u64> },
    /// Dropped, or no answer before the decision: nothing to hand over.
    Nothing,
}

#[derive(Clone, Debug)]
struct FieldCommit {
    route: String,
    boundary: u64,
    since: u64,
    source: String,
}

#[derive(Clone, Debug)]
struct Row {
    id: i64,
    file: PathBuf,
    policy: String,
    support_start: u64,
    open: u64,
    start: u64,
    /// Where the device's live packets began ([`live_from`]).
    live_from: u64,
    /// The end of the pre-roll the speech worker holds when the utterance starts
    /// ([`preroll_through`]).
    preroll: Option<u64>,
    trigger: u64,
    /// The trace's utterance start (the streaming recognizer's), when stored.
    stored_start: Option<u64>,
    first_audio_ms: Option<u64>,
    commit: Option<FieldCommit>,
    /// Each stored pause: its `through` and what the server did.
    pauses: Vec<(u64, Stored)>,
    /// HA's stored answers: text, class, and the block they arrived at.
    recognizer: Vec<(String, Option<TextClass>, u64)>,
    endpoint_class: Option<String>,
}

fn rows(db: &Path, recordings: &Path) -> (Vec<Row>, BTreeMap<&'static str, usize>) {
    let db = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let mut stmt = db
        .prepare(
            "select id, device_id, decision_trace, policy_hash, endpoint_class from turns \
             where decision_trace is not null order by id",
        )
        .unwrap();
    let mut skipped = BTreeMap::new();
    let mut out = Vec::new();
    let mut q = stmt.query([]).unwrap();
    while let Some(r) = q.next().unwrap() {
        let id: i64 = r.get(0).unwrap();
        let device: String = r.get(1).unwrap();
        let trace: Value = serde_json::from_str(&r.get::<_, String>(2).unwrap()).unwrap();
        let utt = &trace["utterance"];
        let Some(pauses) = utt["pauses"].as_array() else {
            continue;
        };
        if !pauses.iter().any(|p| p["server"].is_object()) {
            continue;
        }
        let file = recordings.join(format!("{device}_{id}.wav"));
        if !file.exists() {
            *skipped.entry("no recording").or_insert(0) += 1;
            continue;
        }
        if trace["trigger"] != "wake" {
            *skipped
                .entry("button or reply: recording start not recoverable")
                .or_insert(0) += 1;
            continue;
        }
        let support = u(&trace["afe_evidence"]["support_start"]);
        let open = trace["hops"]
            .as_array()
            .and_then(|h| h.last())
            .and_then(|h| u(&h["end_sample"]));
        let (Some(trigger), Some(support_start), Some(open)) = (u(&utt["trigger"]), support, open)
        else {
            *skipped.entry("no trigger, support or hops").or_insert(0) += 1;
            continue;
        };
        let events = utt["endpoint"].as_array().cloned().unwrap_or_default();
        let since = events
            .iter()
            .filter(|e| e["event"] == "pending")
            .filter_map(|e| u(&e["since"]))
            .next_back();
        let commit = events
            .iter()
            .find(|e| e["event"] == "commit")
            .and_then(|e| {
                Some(FieldCommit {
                    route: e["route"].as_str()?.to_owned(),
                    boundary: u(&e["boundary"])?,
                    since: since?,
                    source: e["source"].as_str().unwrap_or("").to_owned(),
                })
            });
        let pauses = pauses
            .iter()
            .filter_map(|p| {
                let through = u(&p["through"])?;
                let s = &p["server"];
                let ms = u(&s["ms"]);
                let stored = match (s["text"].as_str(), u(&s["at"]), s["error"].as_str()) {
                    (_, _, Some("superseded")) | (None, _, None) => Stored::Nothing,
                    (_, _, Some(error)) => Stored::Failed {
                        error: error.to_owned(),
                        ms,
                    },
                    (Some(text), Some(at), None) => Stored::Judged {
                        text: text.to_owned(),
                        at,
                    },
                    (Some(text), None, None) => Stored::Unjudged {
                        text: text.to_owned(),
                        ms,
                    },
                };
                Some((through, stored))
            })
            .collect();
        let recognizer: Vec<(String, Option<TextClass>, u64)> = utt["recognizer"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|r| {
                        Some((
                            r["text"].as_str()?.to_owned(),
                            class(&r["klass"]),
                            u(&r["at"])?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut boundaries: Vec<u64> = utt["endpoint"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| u(&e["since"]))
            .collect();
        for p in utt["pauses"].as_array().into_iter().flatten() {
            boundaries.extend(
                [&p["through"], &p["kroko"]["at"], &p["server"]["at"]]
                    .into_iter()
                    .filter_map(u),
            );
        }
        boundaries.extend(recognizer.iter().map(|(_, _, at)| *at));
        let start = TurnSpec::candidate_mic_start(Sample(support_start)).0;
        let mut row = Row {
            id,
            file,
            policy: r.get::<_, Option<String>>(3).unwrap().unwrap_or_default(),
            support_start,
            open,
            start,
            live_from: live_from(start, open, &boundaries),
            preroll: None,
            trigger,
            stored_start: u(&utt["start"]),
            first_audio_ms: u(&trace["first_audio_ms"]),
            commit,
            pauses,
            recognizer,
            endpoint_class: r.get(4).unwrap(),
        };
        row.preroll = preroll_through(&row, trace["producing_sound"] == true);
        out.push(row);
    }
    (out, skipped)
}

fn gained(pcm: &[i16], gain: f64) -> Vec<f32> {
    pcm.iter()
        .map(|&s| ((f64::from(s) * gain).clamp(-32_768.0, 32_767.0) / 32_768.0) as f32)
        .collect()
}

fn decode_fresh(kroko: &KrokoRecognizer, audio: &[f32]) -> AsrResult {
    let mut s = kroko.create_stream().unwrap();
    s.accept(audio).unwrap();
    s.accept(&vec![0.0; FLUSH]).unwrap();
    s.decode_ready();
    s.result().unwrap()
}

/// What the speech worker would have produced for one recording.
struct Evidence {
    energy: Vec<i16>,
    vad: Vec<f32>,
    blocks: Vec<AsrObservation>,
}

fn evidence(row: &Row, pcm: &[i16], kroko: &KrokoRecognizer, vad: &mut SileroVad) -> Evidence {
    let energy = pcm
        .as_chunks::<{ CELL as usize }>()
        .0
        .iter()
        .map(|c| DeviceCell::loudness_centi(c))
        .collect();
    vad.reset();
    let probs: Vec<f32> = gained(pcm, 10f64.powf(1.5))
        .as_chunks::<{ CELL as usize }>()
        .0
        .iter()
        .map(|c| vad.process_cell(c).unwrap())
        .collect();
    let copy = gained(pcm, 10.0);
    // The streaming recognizer starts at the utterance start, which lies within the
    // recording (it starts at the candidate's mic start, a cell boundary), and is looked at
    // after every mic packet the device sent, as the speech worker does.
    let utt = row.utterance_start();
    let offset = (utt - row.start) as usize;
    let mut stream = kroko.create_stream().unwrap();
    let mut finalizer = PauseFinalizer::new(0.5);
    let mut blocks = Vec::new();
    let mut observed = ((utt - row.start) / CELL) as usize;
    let mut fed = utt;
    // The pre-roll held when the utterance starts is one block whose VAD never arms the
    // pause check.
    let preroll = row.preroll.unwrap_or(utt);
    let ends = packet_ends(row, utt, row.start + pcm.len() as u64).filter(|e| *e >= preroll);
    for through in ends {
        stream
            .accept(&copy[(fed - row.start) as usize..(through - row.start) as usize])
            .unwrap();
        fed = through;
        stream.decode_ready();
        let complete_cells = ((through - row.start) / CELL) as usize;
        while observed < complete_cells.min(probs.len()) {
            let cell = CellIndex(row.start / CELL + observed as u64);
            let held = cell.end().0 <= preroll;
            finalizer.observe_cell(cell, (!held).then_some(probs[observed]));
            observed += 1;
        }
        let (r, kind) = if finalizer.due() {
            (
                decode_fresh(kroko, &copy[offset..(through - row.start) as usize]),
                AsrKind::Finalized,
            )
        } else {
            (stream.result().unwrap(), AsrKind::Live)
        };
        blocks.push(AsrObservation::from_recognizer(
            Sample(utt),
            Sample(through),
            &r.tokens,
            &r.timestamps,
            r.num_trailing_blanks,
            kind,
        ));
    }
    Evidence {
        energy,
        vad: probs,
        blocks,
    }
}

/// This run's commit, and what it asked.
#[derive(Clone, Debug, Default)]
struct Outcome {
    /// Route, boundary, the block its pending end was decided at (the field's `since` is that
    /// block's end; this policy's own `since` is the last cell end within it), source, class.
    commit: Option<(Route, u64, u64, TextSource, Option<TextClass>)>,
    closed: Option<String>,
    /// Texts asked of HA's matcher with no stored answer.
    unanswered: usize,
    /// This run's pause decodes before its decision, and how many lined up with a stored one.
    pauses: Vec<u64>,
    /// The policy's trace at the end: every judged text with its class, and HA's answers.
    trace: Option<UtteranceTrace>,
}

/// The sample from which a stored answer `ms` after a pause decode through `through` is
/// handed over: never before the next block.
fn arrival(through: u64, ms: Option<u64>) -> u64 {
    through + (ms.unwrap_or(0) * 16).max(1)
}

fn replay(row: &Row, ev: &Evidence, grammar: &RealGrammar, server: bool) -> Outcome {
    let epoch = Epoch::new(1).unwrap();
    let start = Sample(row.utterance_start());
    let recording = Sample(row.start);
    let spec = TurnSpec {
        epoch,
        start,
        trigger: Sample(row.trigger),
        cells_from: recording.cell(),
        vad_from: recording.cell(),
        kind: TurnKind::Wake(WakeAnchor {
            support_start: Sample(row.support_start),
            candidate_open: Sample(row.open),
            phrase: PHRASE.into(),
        }),
    };
    let config = TurnConfig {
        recognizer_available: true,
        pause_server: server,
        ..TurnConfig::default()
    };
    let mut p = TurnPolicy::new(config, spec, MonoInstant(0)).unwrap();
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
    let first = recording.cell().0;
    let mut out = Vec::new();
    let mut o = Outcome::default();
    let mut pushed = 0usize;
    // Answers due before the block covering through the key is pushed.
    let mut server_due: Vec<(u64, PauseServerAnswer)> = Vec::new();
    let mut asked: Vec<(String, u64, bool)> = Vec::new();
    let mut last_since = None;
    for obs in &ev.blocks {
        // Wall time follows the audio: the field ran these blocks as their packets came.
        let now = MonoInstant((obs.through.0 - start.0) * 62_500);
        let through = obs.through.0;
        let cells = (((through - row.start) / CELL) as usize).min(vads.len());
        let at = CellIndex(first + pushed as u64);
        let e = p.evidence_mut();
        e.push_device_cells(epoch, at, &device[pushed..cells])
            .unwrap();
        e.push_vad(epoch, at, &vads[pushed..cells]).unwrap();
        e.push_echo(epoch, at, &vec![EchoCell::MISSING; cells - pushed])
            .unwrap();
        pushed = cells;
        if obs.kind == AsrKind::Finalized {
            o.pauses.push(through);
            if server {
                p.pause_server_asked(obs.through, now);
                if let Some((_, stored)) = stored_pause(&row.pauses, through) {
                    let answer = |outcome| PauseServerAnswer {
                        pause: obs.through,
                        outcome,
                    };
                    match stored {
                        Stored::Judged { text, at } => {
                            server_due.push((*at, answer(Ok(text.clone()))));
                        }
                        Stored::Unjudged { text, ms } if text.trim().is_empty() => {
                            server_due.push((arrival(through, *ms), answer(Ok(text.clone()))))
                        }
                        Stored::Failed { error, ms } => {
                            let failure = if error == "timeout" {
                                ServerFailure::Timeout
                            } else {
                                ServerFailure::Failed(error.clone())
                            };
                            server_due.push((arrival(through, *ms), answer(Err(failure))));
                        }
                        Stored::Unjudged { .. } | Stored::Nothing => {}
                    }
                }
            }
        }
        // Stored answers due at this block arrive just before its result.
        let (now_due, later): (Vec<_>, Vec<_>) =
            server_due.drain(..).partition(|(at, _)| *at <= through);
        server_due = later;
        for (_, answer) in now_due {
            p.pause_server_answer(answer, now);
        }
        // HA's stored class for a text, from the first block after both its stored arrival
        // and this run's question.
        for (text, asked_at, delivered) in &mut asked {
            if !*delivered
                && let Some((_, Some(c), stored_at)) =
                    row.recognizer.iter().find(|(t, _, _)| t == text)
                && through > (*stored_at).max(*asked_at)
            {
                *delivered = true;
                p.recognizer_answer(
                    RecognizerAnswer {
                        text: text.clone(),
                        outcome: Ok(*c),
                    },
                    now,
                );
            }
        }
        p.evidence_mut().push_asr(epoch, obs.clone()).unwrap();
        let before = out.len();
        p.advance(grammar, now, &mut out);
        for d in &out[before..] {
            match d {
                Decision::AskRecognizer { text } => {
                    if !row
                        .recognizer
                        .iter()
                        .any(|(t, c, _)| t == text && c.is_some())
                    {
                        o.unanswered += 1;
                    }
                    asked.push((text.clone(), through, false));
                }
                Decision::EndPending { .. } => last_since = Some(through),
                Decision::Commit(c) => {
                    o.commit = Some((
                        c.route,
                        c.boundary.0,
                        last_since.unwrap_or(c.at.0),
                        c.source,
                        c.class,
                    ));
                }
                Decision::Close { reason, .. } => o.closed = Some(format!("{reason:?}")),
                _ => {}
            }
        }
        if p.is_finished() {
            break;
        }
    }
    o.trace = Some(p.trace().clone());
    o
}

fn route_name(r: Route) -> &'static str {
    match r {
        Route::A => "A",
        Route::B => "B",
        Route::R => "R",
        Route::Fallback => "fallback",
    }
}

fn source_name(s: TextSource) -> &'static str {
    match s {
        TextSource::Streaming => "streaming",
        TextSource::Kroko => "kroko",
        TextSource::Server => "server",
    }
}

fn class_name(c: Option<TextClass>) -> &'static str {
    match c {
        Some(TextClass::Complete) => "complete",
        Some(TextClass::Extendable) => "extendable",
        Some(TextClass::NeedsMore) => "needs_more",
        Some(TextClass::Unknown) => "unknown",
        None => "none",
    }
}

struct Evaluated {
    row: Row,
    server: Outcome,
    alone: Outcome,
}

/// Why this run's commit differs from the field's, first cause that applies.
fn cause(r: &Evaluated) -> &'static str {
    let Some(f) = &r.row.commit else {
        return "the field did not commit";
    };
    let Some((route, _, since, _, class)) = r.server.commit else {
        return "this run did not commit";
    };
    if r.row.stored_start != Some(r.row.utterance_start()) {
        return "the field's utterance started elsewhere";
    }
    let field_pauses: Vec<u64> = r
        .row
        .pauses
        .iter()
        .map(|(t, _)| *t)
        .filter(|t| *t <= f.since)
        .collect();
    let run_pauses: Vec<u64> = r
        .server
        .pauses
        .iter()
        .copied()
        .filter(|t| *t <= since.max(f.since))
        .collect();
    if !field_pauses.iter().all(|t| {
        run_pauses
            .iter()
            .any(|r| stored_pause(&[(*t, Stored::Nothing)], *r).is_some())
    }) {
        return "pause decodes fell on other blocks";
    }
    if r.server.unanswered > 0 {
        return "HA's matcher asked about text the field never asked";
    }
    if r.row.endpoint_class.as_deref() != Some(class_name(class)) {
        return "the grammar judged the committed text differently";
    }
    if route_name(route) != f.route {
        return "another route";
    }
    "other"
}

fn main() {
    let mut map = BTreeMap::new();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        map.insert(k.trim_start_matches("--").to_owned(), it.next().unwrap());
    }
    let get = |k: &str| PathBuf::from(map.get(k).unwrap_or_else(|| panic!("--{k}")));
    let manifest = SpeechBundleManifest::load(&get("manifest")).unwrap();
    let native = NativeLibraries::pinned(get("ort-lib"), None).unwrap();
    let ort = OrtRuntime::load(&native.onnxruntime).unwrap();
    let sherpa = SherpaRuntime::verify(&native.sherpa).unwrap();
    let kroko_dir = get("kroko");
    let pin = |name: &str, d| PinnedFile::new(kroko_dir.join(name), d);
    let files = KrokoModelFiles {
        encoder: pin("encoder.onnx", manifest.asr.files.encoder.sha256),
        decoder: pin("decoder.onnx", manifest.asr.files.decoder.sha256),
        joiner: pin("joiner.onnx", manifest.asr.files.joiner.sha256),
        tokens: pin("tokens.txt", manifest.asr.files.tokens.sha256),
    };
    let kroko = KrokoRecognizer::load(&sherpa, &files).unwrap();
    let silero = PinnedFile::new(get("silero"), manifest.vad.sha256);
    let threads: usize = map.get("threads").map_or(4, |t| t.parse().unwrap());
    let mut grammar = Grammar::new().unwrap();
    if let Some(path) = map.get("vocabulary") {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let list = |k: &str| {
            v[k].as_array().map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
        };
        grammar = grammar.with_vocabulary(&Vocabulary {
            names: list("names"),
            areas: list("areas"),
            floors: list("floors"),
        });
    }
    let grammar = RealGrammar(grammar);

    let (rows, skipped) = rows(&get("db"), &get("recordings"));
    println!("turns replayed: {}; skipped {skipped:?}", rows.len());
    let next = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut vad = SileroVad::load(&ort, &silero).unwrap();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(row) = rows.get(i) else { break };
                    let wav = MonoWav::read(&row.file).unwrap();
                    assert_eq!(wav.sample_rate, 16_000);
                    let rec_end = row.start + wav.samples.len() as u64;
                    // Room audio until the field's response became audible.
                    let end = match (&row.commit, row.first_audio_ms) {
                        (Some(c), Some(fa)) => (c.since + 3_072 + fa * 16).min(rec_end),
                        _ => rec_end,
                    };
                    let pcm = &wav.samples[..(end - row.start) as usize];
                    let ev = evidence(row, pcm, &kroko, &mut vad);
                    let server = replay(row, &ev, &grammar, true);
                    let alone = replay(row, &ev, &grammar, false);
                    results.lock().unwrap().push(Evaluated {
                        row: row.clone(),
                        server,
                        alone,
                    });
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.row.id);
    report("all", &results);
    let current: Vec<&Evaluated> = results
        .iter()
        .filter(|r| r.row.policy.starts_with("post_afe_10"))
        .collect();
    report_refs("policy post_afe_10 only", &current);
    if let Some(out) = map.get("out") {
        let mut text = String::new();
        for r in &results {
            text.push_str(&format!(
                "{}\t{}\tfield {:?}\tserver {:?} {:?}\talone {:?} {:?}\t{}\tpauses field {:?} run {:?}\n",
                r.row.id,
                r.row.policy,
                r.row.commit.as_ref().map(|c| (&c.route, c.boundary, c.since, &c.source)),
                r.server.commit,
                r.server.closed,
                r.alone.commit,
                r.alone.closed,
                cause(r),
                r.row
                    .pauses
                    .iter()
                    .map(|(t, _)| t - r.row.utterance_start())
                    .collect::<Vec<_>>(),
                r.server
                    .pauses
                    .iter()
                    .map(|t| t - r.row.utterance_start())
                    .collect::<Vec<_>>(),
            ));
        }
        std::fs::write(out, text).unwrap();
    }
    if let Some(out) = map.get("texts") {
        std::fs::write(out, texts(&results)).unwrap();
    }
}

/// Every text the server run judged, in order, and every question it put to HA's matcher:
/// `id`, `text`, ms from the utterance start, source, the grammar's class, the text; and
/// `id`, `asked`, ms, the class HA's stored answer gave (`none` without one), the text.
fn texts(results: &[Evaluated]) -> String {
    let mut text = String::new();
    for r in results {
        let Some(trace) = &r.server.trace else {
            continue;
        };
        let ms = |s: Sample| s.0.saturating_sub(r.row.utterance_start()) / 16;
        for change in &trace.text {
            text.push_str(&format!(
                "{}\ttext\t{}\t{}\t{}\t{}\n",
                r.row.id,
                ms(change.at),
                source_name(change.source),
                class_name(change.grammar),
                change.text
            ));
        }
        for asked in &trace.recognizer {
            text.push_str(&format!(
                "{}\tasked\t{}\t\t{}\t{}\n",
                r.row.id,
                ms(asked.asked_at),
                class_name(asked.class),
                asked.text
            ));
        }
    }
    text
}

fn report(label: &str, results: &[Evaluated]) {
    report_refs(label, &results.iter().collect::<Vec<_>>());
}

fn report_refs(label: &str, results: &[&Evaluated]) {
    let field: Vec<_> = results.iter().filter(|r| r.row.commit.is_some()).collect();
    println!(
        "\n== {label}: {} turns, {} committed in the field",
        results.len(),
        field.len()
    );
    for (name, pick) in [
        ("with the stored server words", true),
        ("Kroko alone", false),
    ] {
        let mut cell = 0;
        let mut near = 0;
        let mut boundary = 0;
        let mut source = 0;
        let mut committed = 0;
        for r in &field {
            let f = r.row.commit.as_ref().unwrap();
            let o = if pick { &r.server } else { &r.alone };
            let Some((_, b, since, s, _)) = o.commit else {
                continue;
            };
            committed += 1;
            let d = since.abs_diff(f.since);
            cell += usize::from(d <= CELL);
            near += usize::from(d <= NEAR);
            boundary += usize::from(b.abs_diff(f.boundary) <= CELL);
            source += usize::from(source_name(s) == f.source);
        }
        let pct = |n: usize| 100.0 * n as f64 / field.len().max(1) as f64;
        println!(
            "{name}: committed {committed}; commit decision within one cell {cell} ({:.1}%), \
             within 200 ms {near} ({:.1}%); boundary within one cell {boundary} ({:.1}%); \
             source {source} ({:.1}%)",
            pct(cell),
            pct(near),
            pct(boundary),
            pct(source)
        );
    }
    let mut causes: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for r in &field {
        let f = r.row.commit.as_ref().unwrap();
        let agrees = r.server.commit.is_some_and(|(_, _, since, s, _)| {
            since.abs_diff(f.since) <= CELL && source_name(s) == f.source
        });
        if !agrees {
            causes.entry(cause(r)).or_default().push(r.row.id);
        }
    }
    println!("mismatches (decision beyond one cell, or another source), by cause:");
    for (c, ids) in causes {
        println!(
            "  {c}: {} (e.g. turns {:?})",
            ids.len(),
            &ids[..ids.len().min(6)]
        );
    }
}
