//! Model qualification in a short-lived child process (§5.5 of the Rust controller evaluation).
//!
//! Before the main process loads a model (the speech bundle at startup, every wake-model
//! upload), the same binary is started again with the [`QUALIFY_MODEL_SUBCOMMAND`] argument.
//! The child reads a [`QualificationRequest`] as JSON on stdin, checks the files' hashes, the
//! model's input and output names and shapes, runs one representative inference, checks that
//! every output is finite, and prints one report line on stdout. The parent waits a finite
//! time. A native crash, an exit without a report, or a timeout in the child becomes a
//! [`QualificationRejection`]; it never takes the parent down.
//!
//! The integrator routes the subcommand to [`child_main`]:
//!
//! ```no_run
//! # fn real_main() -> std::process::ExitCode { std::process::ExitCode::SUCCESS }
//! fn main() -> std::process::ExitCode {
//!     if std::env::args().nth(1).as_deref() == Some(echomuse_speech::qualify::QUALIFY_MODEL_SUBCOMMAND) {
//!         return echomuse_speech::qualify::child_main();
//!     }
//!     real_main()
//! }
//! ```
//!
//! Qualification pins the bytes it checked, by hash. It is not a guarantee that later inference
//! cannot fail.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use wait_timeout::ChildExt;

use crate::audio::{LinearResampler, MonoWav};
use crate::bcresnet::{
    BcresnetModelFiles, BcresnetScorer, WAKE_WINDOW_SAMPLES, WindowScore, prepare_window,
};
use crate::digest::PinnedFile;
use crate::dtln::{DtlnDenoiser, DtlnModelFiles};
use crate::error::SpeechError;
use crate::kroko::{KROKO_SAMPLE_RATE, KrokoModelFiles, KrokoRecognizer};
use crate::native::{NativeLibraries, OrtRuntime, SherpaRuntime};
use crate::silero::{SILERO_CELL_SAMPLES, SileroVad};

/// The argument that makes the binary act as a qualification child.
pub const QUALIFY_MODEL_SUBCOMMAND: &str = "qualify-model";

/// The child's report follows this marker on one line. The child starts it on a fresh line,
/// and the parent finds it anywhere in a line, so output from native libraries or a test
/// harness cannot be mistaken for it or hide it.
pub const REPORT_MARKER: &str = "echomuse-qualify-report:";

/// The accepted range of Kroko's blank-frame rate over trailing silence, in frames per second
/// (post-AFE architecture §16.6: 22–27; one frame is 40 ms, so 25 is exact).
pub const TRAILING_BLANK_RATE: std::ops::RangeInclusive<f64> = 22.0..=27.0;

/// Seconds of digital silence fed after the Kroko test WAV.
const KROKO_SILENCE_SECONDS: usize = 4;

const MAX_REQUEST_BYTES: u64 = 1 << 20;
const MAX_STDOUT_BYTES: usize = 1 << 20;
const STDERR_TAIL_BYTES: usize = 8 << 10;

/// What to qualify, and with which native libraries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualificationRequest {
    /// The native libraries the child must load and verify.
    pub native: NativeLibraries,
    /// The model.
    pub model: ModelUnderTest,
}

/// One model to qualify.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelUnderTest {
    /// Kroko streaming ASR: decode the test WAV followed by 4 s of silence.
    Kroko {
        /// The model files.
        files: KrokoModelFiles,
        /// A 16-bit mono WAV of speech, at any rate (resampled to 16 kHz).
        test_wav: PinnedFile,
        /// The exact final transcript required, or `None` to require only a non-empty one.
        expected_transcript: Option<String>,
    },
    /// A BCResNet wake graph and sidecar: score a deterministic noise window.
    Bcresnet {
        /// The graph and sidecar.
        files: BcresnetModelFiles,
    },
    /// Silero VAD v5: run deterministic cells and check determinism after reset.
    SileroVad {
        /// The model file.
        model: PinnedFile,
    },
    /// DTLN: denoise one second of deterministic noise and tone.
    Dtln {
        /// The two stages.
        files: DtlnModelFiles,
    },
}

/// What the representative inference produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum QualificationEvidence {
    /// Kroko.
    Kroko {
        /// The final transcript.
        transcript: String,
        /// Fitted blank frames per second over trailing silence.
        trailing_blanks_per_second: f64,
        /// Encoder chunks decoded.
        decode_steps: u32,
    },
    /// BCResNet.
    Bcresnet {
        /// Logits of the noise window.
        logits: Vec<f32>,
        /// Its wake probability.
        probability: f64,
    },
    /// Silero.
    SileroVad {
        /// Per-cell speech probabilities.
        probabilities: Vec<f32>,
    },
    /// DTLN.
    Dtln {
        /// RMS of the denoised second.
        output_rms: f64,
    },
}

/// A model that passed: the pins of the bytes checked, and the evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualifiedModel {
    /// Every file that was checked, with the digest of the bytes that passed.
    pub files: Vec<PinnedFile>,
    /// What the representative inference produced.
    pub evidence: QualificationEvidence,
}

/// Why a child rejected a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The request could not be parsed or is invalid.
    Request,
    /// A file could not be read.
    Io,
    /// A file's digest is not the pinned one.
    HashMismatch,
    /// The native libraries are missing or not the pinned builds.
    NativeLibrary,
    /// The runtime refused to load the model.
    Load,
    /// The wake sidecar is invalid.
    Sidecar,
    /// The model's inputs or outputs are not the expected ones.
    ModelContract,
    /// The runtime failed while running the model.
    Inference,
    /// An output was not finite or out of range.
    InvalidOutput,
    /// The test audio could not be read.
    TestAudio,
    /// Kroko's transcript was not the expected one.
    Transcript,
    /// Kroko's trailing blank rate was outside [`TRAILING_BLANK_RATE`].
    TrailingBlankRate,
}

/// A rejection the child reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{kind:?}: {detail}")]
pub struct QualificationFailure {
    /// The category.
    pub kind: FailureKind,
    /// The details.
    pub detail: String,
}

impl QualificationFailure {
    fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Which part of qualification an error came from, to tell load failures from run failures.
#[derive(Clone, Copy)]
enum Phase {
    Load,
    Run,
}

fn failure(phase: Phase, e: SpeechError) -> QualificationFailure {
    let kind = match (&e, phase) {
        (SpeechError::Io { .. }, _) => FailureKind::Io,
        (SpeechError::HashMismatch { .. }, _) => FailureKind::HashMismatch,
        (SpeechError::NativeLibrary(_), _) => FailureKind::NativeLibrary,
        (SpeechError::Onnx(_) | SpeechError::NullHandle(_), Phase::Load) => FailureKind::Load,
        (SpeechError::Onnx(_) | SpeechError::NullHandle(_), Phase::Run) => FailureKind::Inference,
        (SpeechError::ModelContract { .. }, _) => FailureKind::ModelContract,
        (SpeechError::Sidecar(_), _) => FailureKind::Sidecar,
        (SpeechError::InvalidInput(_) | SpeechError::Manifest(_), _) => FailureKind::Request,
        (SpeechError::InvalidOutput { .. } | SpeechError::MalformedResult(_), _) => {
            FailureKind::InvalidOutput
        }
        (SpeechError::Wav { .. }, _) => FailureKind::TestAudio,
    };
    QualificationFailure::new(kind, e.to_string())
}

/// The child's one-line report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ChildReport {
    Qualified(QualifiedModel),
    Rejected(QualificationFailure),
}

/// Why the parent did not accept a model.
#[derive(Debug, thiserror::Error)]
pub enum QualificationRejection {
    /// The child ran to completion and rejected the model.
    #[error("model rejected: {0}")]
    Rejected(QualificationFailure),
    /// The child was killed by a signal, typically a native crash.
    #[error("qualification child crashed with signal {signal}")]
    Crashed {
        /// The signal number.
        signal: i32,
        /// The end of the child's stderr.
        stderr_tail: String,
    },
    /// The child exited with a non-zero status.
    #[error("qualification child exited with status {exit_code}")]
    AbnormalExit {
        /// The exit status.
        exit_code: i32,
        /// The end of the child's stderr.
        stderr_tail: String,
    },
    /// The child exited cleanly but printed no report.
    #[error("qualification child printed no report")]
    NoReport {
        /// The end of the child's stderr.
        stderr_tail: String,
    },
    /// The child printed a report line that does not parse.
    #[error("qualification child printed a malformed report: {detail}")]
    MalformedReport {
        /// The parse error.
        detail: String,
        /// The end of the child's stderr.
        stderr_tail: String,
    },
    /// The child did not finish in time and was killed.
    #[error("qualification child did not finish within {after:?}")]
    TimedOut {
        /// The deadline.
        after: Duration,
        /// The end of the child's stderr.
        stderr_tail: String,
    },
    /// The child could not be started or waited for.
    #[error("qualification child: {0}")]
    Spawn(#[source] io::Error),
}

/// How to start the qualification child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifierCommand {
    /// The program.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Extra environment variables.
    pub env: Vec<(OsString, OsString)>,
}

impl QualifierCommand {
    /// This executable with the [`QUALIFY_MODEL_SUBCOMMAND`] argument.
    pub fn current_exe() -> io::Result<Self> {
        Ok(Self::new(
            std::env::current_exe()?,
            [OsString::from(QUALIFY_MODEL_SUBCOMMAND)],
        ))
    }

    /// Any program and arguments, for example a test harness that routes to [`run_child`].
    pub fn new(program: impl Into<PathBuf>, args: impl IntoIterator<Item = OsString>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            env: Vec::new(),
        }
    }

    /// Adds an environment variable for the child.
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Keeps the last `limit` bytes read from `r`.
fn read_tail(mut r: impl Read, limit: usize) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > limit {
                    tail.drain(..tail.len() - limit);
                }
            }
        }
    }
    tail
}

/// Keeps the first `limit` bytes read from `r`, draining the rest so the writer never blocks.
fn read_head(mut r: impl Read, limit: usize) -> Vec<u8> {
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(head.len());
                head.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    head
}

/// Runs qualification in a child process and waits at most `timeout` for it.
pub fn qualify_in_child(
    command: &QualifierCommand,
    request: &QualificationRequest,
    timeout: Duration,
) -> Result<QualifiedModel, QualificationRejection> {
    let body = serde_json::to_vec(request).map_err(|e| {
        QualificationRejection::Spawn(io::Error::new(io::ErrorKind::InvalidInput, e))
    })?;
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .envs(command.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(QualificationRejection::Spawn)?;

    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(QualificationRejection::Spawn(io::Error::other(
            "child pipes missing",
        )));
    };
    // The request goes in on its own thread: a child that never reads must not block us. A
    // child that exits first makes this write fail with a broken pipe, which its exit status
    // then explains.
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let out_reader = std::thread::spawn(move || read_head(stdout, MAX_STDOUT_BYTES));
    let err_reader = std::thread::spawn(move || read_tail(stderr, STDERR_TAIL_BYTES));

    let status = match child.wait_timeout(timeout) {
        Ok(Some(status)) => Ok(status),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(None)
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(Some(e))
        }
    };
    let _ = writer.join();
    let stdout = out_reader.join().unwrap_or_default();
    let stderr_tail = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();

    let status = match status {
        Ok(s) => s,
        Err(None) => {
            return Err(QualificationRejection::TimedOut {
                after: timeout,
                stderr_tail,
            });
        }
        Err(Some(e)) => return Err(QualificationRejection::Spawn(e)),
    };
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return Err(QualificationRejection::Crashed {
            signal,
            stderr_tail,
        });
    }
    match status.code() {
        Some(0) => {}
        Some(exit_code) => {
            return Err(QualificationRejection::AbnormalExit {
                exit_code,
                stderr_tail,
            });
        }
        None => return Err(QualificationRejection::NoReport { stderr_tail }),
    }
    let Some(line) = stdout.split(|&b| b == b'\n').find_map(|l| {
        let l = String::from_utf8_lossy(l);
        l.find(REPORT_MARKER)
            .map(|i| l[i + REPORT_MARKER.len()..].to_owned())
    }) else {
        return Err(QualificationRejection::NoReport { stderr_tail });
    };
    match serde_json::from_str::<ChildReport>(&line) {
        Ok(ChildReport::Qualified(model)) => Ok(model),
        Ok(ChildReport::Rejected(f)) => Err(QualificationRejection::Rejected(f)),
        Err(e) => Err(QualificationRejection::MalformedReport {
            detail: e.to_string(),
            stderr_tail,
        }),
    }
}

/// The child side: reads the request from stdin, qualifies, prints the report line on stdout.
/// Exits 0 whenever a report was written, whether the model passed or not.
pub fn child_main() -> ExitCode {
    match run_child(io::stdin().lock(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("qualify-model: {e}");
            ExitCode::from(2)
        }
    }
}

/// The child side over any reader and writer.
pub fn run_child(input: impl Read, mut output: impl Write) -> io::Result<()> {
    let mut body = Vec::new();
    input.take(MAX_REQUEST_BYTES).read_to_end(&mut body)?;
    let report = match serde_json::from_slice::<QualificationRequest>(&body) {
        Err(e) => ChildReport::Rejected(QualificationFailure::new(
            FailureKind::Request,
            format!("request JSON: {e}"),
        )),
        Ok(request) => match qualify_in_this_process(&request) {
            Ok(model) => ChildReport::Qualified(model),
            Err(f) => ChildReport::Rejected(f),
        },
    };
    let line = serde_json::to_string(&report).map_err(io::Error::other)?;
    write!(output, "\n{REPORT_MARKER}{line}\n")?;
    output.flush()
}

/// Qualifies a model in the calling process. Only the qualification child should call this:
/// a bad model can crash the process that runs it.
pub fn qualify_in_this_process(
    request: &QualificationRequest,
) -> Result<QualifiedModel, QualificationFailure> {
    match &request.model {
        ModelUnderTest::Kroko {
            files,
            test_wav,
            expected_transcript,
        } => qualify_kroko(request, files, test_wav, expected_transcript.as_deref()),
        ModelUnderTest::Bcresnet { files } => qualify_bcresnet(request, files),
        ModelUnderTest::SileroVad { model } => qualify_silero(request, model),
        ModelUnderTest::Dtln { files } => qualify_dtln(request, files),
    }
}

fn ort_runtime(request: &QualificationRequest) -> Result<OrtRuntime, QualificationFailure> {
    OrtRuntime::load(&request.native.onnxruntime).map_err(|e| failure(Phase::Load, e))
}

/// Kroko's blank-frame rate over trailing silence, from the decode events seen while it was
/// fed: `(seconds of input fed, num_trailing_blanks)` for every block after which at least one
/// encoder chunk ran. Each chunk adds a whole run of blank frames at once (32 frames, 1.28 s,
/// for the pinned model), so the rate is the least-squares slope over decode events since the
/// last emitted token, not over every fed block. `None` with fewer than two such events.
pub fn trailing_blank_rate(events: &[(f64, f64)]) -> Option<f64> {
    // The run since the last emission: after the last point where the blank count fell.
    let start = events
        .windows(2)
        .rposition(|w| w[1].1 < w[0].1)
        .map_or(0, |i| i + 1);
    slope(&events[start..])
}

/// Least-squares slope of `y` over `x`, or `None` with fewer than two distinct `x`.
fn slope(points: &[(f64, f64)]) -> Option<f64> {
    let n = points.len() as f64;
    if points.len() < 2 {
        return None;
    }
    let mx = points.iter().map(|p| p.0).sum::<f64>() / n;
    let my = points.iter().map(|p| p.1).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    (sxx > 0.0).then(|| sxy / sxx)
}

/// The Kroko protocol: the WAV followed by [`KROKO_SILENCE_SECONDS`] of zeros, at the WAV's
/// own rate, in 80 ms blocks, each resampled to 16 kHz with the parameters sherpa-onnx uses
/// internally and decoded while ready.
fn qualify_kroko(
    request: &QualificationRequest,
    files: &KrokoModelFiles,
    test_wav: &PinnedFile,
    expected: Option<&str>,
) -> Result<QualifiedModel, QualificationFailure> {
    let load = |e| failure(Phase::Load, e);
    let run = |e| failure(Phase::Run, e);
    let sherpa = SherpaRuntime::verify(&request.native.sherpa).map_err(load)?;
    test_wav.verify().map_err(load)?;
    let wav = MonoWav::read(&test_wav.path).map_err(load)?;
    let rate = wav.sample_rate as usize;
    let block = (rate * 80 / 1000).max(1);
    let mut audio = wav.to_f32();
    let speech_len = audio.len();
    audio.extend(std::iter::repeat_n(0.0, rate * KROKO_SILENCE_SECONDS));
    let mut resampler = if wav.sample_rate == KROKO_SAMPLE_RATE {
        None
    } else {
        Some(LinearResampler::to_kroko_rate(wav.sample_rate).map_err(load)?)
    };
    let recognizer = KrokoRecognizer::load(&sherpa, files).map_err(load)?;
    let mut stream = recognizer.create_stream().map_err(load)?;

    let mut steps = 0usize;
    let mut events = Vec::new();
    for (i, piece) in audio.chunks(block).enumerate() {
        match resampler.as_mut() {
            Some(r) => stream.accept(&r.resample(piece, false).map_err(run)?),
            None => stream.accept(piece),
        }
        .map_err(run)?;
        let ran = stream.decode_ready();
        steps += ran;
        let fed = (i + 1) * block;
        if ran > 0 && fed > speech_len {
            let blanks = stream.result().map_err(run)?.num_trailing_blanks;
            events.push((fed as f64 / rate as f64, f64::from(blanks)));
        }
    }
    let transcript = stream.result().map_err(run)?.text.trim().to_owned();
    match expected {
        Some(want) if transcript != want.trim() => {
            return Err(QualificationFailure::new(
                FailureKind::Transcript,
                format!("transcribed {transcript:?}, expected {want:?}"),
            ));
        }
        None if transcript.is_empty() => {
            return Err(QualificationFailure::new(
                FailureKind::Transcript,
                "the test WAV produced no text",
            ));
        }
        _ => {}
    }
    let blank_rate = trailing_blank_rate(&events).unwrap_or(f64::NAN);
    if !TRAILING_BLANK_RATE.contains(&blank_rate) {
        return Err(QualificationFailure::new(
            FailureKind::TrailingBlankRate,
            format!(
                "{blank_rate:.2} blank frames per second over trailing silence ({} decode events)",
                events.len()
            ),
        ));
    }
    Ok(QualifiedModel {
        files: vec![
            files.encoder.clone(),
            files.decoder.clone(),
            files.joiner.clone(),
            files.tokens.clone(),
            test_wav.clone(),
        ],
        evidence: QualificationEvidence::Kroko {
            transcript,
            trailing_blanks_per_second: blank_rate,
            decode_steps: u32::try_from(steps).unwrap_or(u32::MAX),
        },
    })
}

/// A deterministic int16 signal from a 31-bit linear congruential generator, as the device's
/// BCResNet fixture generator writes it: sample `i` is `((x >> 8) % modulus) − offset`.
fn lcg_window(seed: u32, modulus: u32, offset: i32, len: usize) -> Vec<i16> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = 1_103_515_245u32.wrapping_mul(x).wrapping_add(12_345) & 0x7FFF_FFFF;
            (((x >> 8) % modulus) as i32 - offset) as i16
        })
        .collect()
}

fn qualify_bcresnet(
    request: &QualificationRequest,
    files: &BcresnetModelFiles,
) -> Result<QualifiedModel, QualificationFailure> {
    let rt = ort_runtime(request)?;
    let mut scorer = BcresnetScorer::load(&rt, files).map_err(|e| failure(Phase::Load, e))?;
    let run = |e| failure(Phase::Run, e);
    // The fixture's "noise_loud" recipe: well above the RMS floor.
    let pcm = lcg_window(12_345, 20_001, 10_000, WAKE_WINDOW_SAMPLES);
    let probability = match scorer.score(&pcm).map_err(run)? {
        WindowScore::Scored { probability } => probability,
        WindowScore::BelowRmsFloor => {
            return Err(QualificationFailure::new(
                FailureKind::InvalidOutput,
                "a loud noise window was below the RMS floor",
            ));
        }
    };
    let mut prepared = vec![0.0; WAKE_WINDOW_SAMPLES];
    prepare_window(&mut prepared, &pcm, scorer.sidecar().norm_peak).map_err(run)?;
    let logits = scorer.logits(&prepared).map_err(run)?.to_vec();
    if scorer
        .score(&vec![0i16; WAKE_WINDOW_SAMPLES])
        .map_err(run)?
        != WindowScore::BelowRmsFloor
    {
        return Err(QualificationFailure::new(
            FailureKind::InvalidOutput,
            "a silent window was scored",
        ));
    }
    Ok(QualifiedModel {
        files: vec![files.graph.clone(), files.sidecar.clone()],
        evidence: QualificationEvidence::Bcresnet {
            logits,
            probability,
        },
    })
}

fn qualify_silero(
    request: &QualificationRequest,
    model: &PinnedFile,
) -> Result<QualifiedModel, QualificationFailure> {
    let rt = ort_runtime(request)?;
    let mut vad = SileroVad::load(&rt, model).map_err(|e| failure(Phase::Load, e))?;
    let run = |e| failure(Phase::Run, e);
    let noise = lcg_window(4_242, 20_001, 10_000, SILERO_CELL_SAMPLES * 16);
    let input: Vec<f32> = noise.iter().map(|&s| f32::from(s) / 32768.0).collect();
    let (cells, _) = input.as_chunks::<SILERO_CELL_SAMPLES>();
    let mut probabilities = Vec::with_capacity(cells.len());
    for cell in cells {
        probabilities.push(vad.process_cell(cell).map_err(run)?);
    }
    vad.reset();
    let again = vad.process_cell(&cells[0]).map_err(run)?;
    if again.to_bits() != probabilities[0].to_bits() {
        return Err(QualificationFailure::new(
            FailureKind::InvalidOutput,
            format!(
                "first cell gave {} then {again} after reset",
                probabilities[0]
            ),
        ));
    }
    Ok(QualifiedModel {
        files: vec![model.clone()],
        evidence: QualificationEvidence::SileroVad { probabilities },
    })
}

fn qualify_dtln(
    request: &QualificationRequest,
    files: &DtlnModelFiles,
) -> Result<QualifiedModel, QualificationFailure> {
    let rt = ort_runtime(request)?;
    let mut ns = DtlnDenoiser::load(&rt, files).map_err(|e| failure(Phase::Load, e))?;
    let noise = lcg_window(99, 2_001, 1_000, 16_000);
    let input: Vec<f32> = noise
        .iter()
        .enumerate()
        .map(|(i, &s)| {
            f32::from(s) / 32768.0
                + 0.1 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 16_000.0).sin()
        })
        .collect();
    let out = ns
        .denoise_span(&input)
        .map_err(|e| failure(Phase::Run, e))?;
    if out.len() != input.len() {
        return Err(QualificationFailure::new(
            FailureKind::InvalidOutput,
            format!("{} samples out for {} in", out.len(), input.len()),
        ));
    }
    let output_rms =
        (out.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / out.len() as f64).sqrt();
    Ok(QualifiedModel {
        files: vec![files.model_1.clone(), files.model_2.clone()],
        evidence: QualificationEvidence::Dtln { output_rms },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> QualifierCommand {
        QualifierCommand::new("/bin/sh", ["-c".into(), script.into()])
    }

    fn request() -> QualificationRequest {
        QualificationRequest {
            native: NativeLibraries::pinned("/nonexistent/libonnxruntime.so", None).unwrap(),
            model: ModelUnderTest::SileroVad {
                model: PinnedFile::new("/nonexistent/vad.onnx", crate::Sha256Digest::of_bytes(b"")),
            },
        }
    }

    #[test]
    fn request_round_trips_through_json() {
        let r = request();
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"silero_vad\""));
        assert_eq!(
            serde_json::from_str::<QualificationRequest>(&json).unwrap(),
            r
        );
    }

    #[test]
    fn child_reports_a_malformed_request() {
        let mut out = Vec::new();
        run_child(&b"{not json"[..], &mut out).unwrap();
        let line = String::from_utf8(out).unwrap();
        let body = line.trim().strip_prefix(REPORT_MARKER).unwrap();
        match serde_json::from_str::<ChildReport>(body).unwrap() {
            ChildReport::Rejected(f) => assert_eq!(f.kind, FailureKind::Request),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn child_reports_a_missing_runtime_library() {
        let mut out = Vec::new();
        run_child(serde_json::to_vec(&request()).unwrap().as_slice(), &mut out).unwrap();
        let line = String::from_utf8(out).unwrap();
        let body = line.trim().strip_prefix(REPORT_MARKER).unwrap();
        match serde_json::from_str::<ChildReport>(body).unwrap() {
            ChildReport::Rejected(f) => assert_eq!(f.kind, FailureKind::Io, "{f}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parent_reads_the_report_among_other_output() {
        let report = ChildReport::Rejected(QualificationFailure::new(FailureKind::Load, "bad"));
        let line = serde_json::to_string(&report).unwrap();
        let script = format!("cat >/dev/null; echo noise; echo '{REPORT_MARKER}{line}'; echo more");
        match qualify_in_child(&sh(&script), &request(), Duration::from_secs(10)) {
            Err(QualificationRejection::Rejected(f)) => {
                assert_eq!(f.kind, FailureKind::Load);
                assert_eq!(f.detail, "bad");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_crashing_child_is_a_rejection() {
        match qualify_in_child(
            &sh("echo dying >&2; kill -SEGV $$"),
            &request(),
            Duration::from_secs(10),
        ) {
            Err(QualificationRejection::Crashed {
                signal,
                stderr_tail,
            }) => {
                assert_eq!(signal, libc::SIGSEGV);
                assert!(stderr_tail.contains("dying"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exits_without_a_report_are_rejections() {
        assert!(matches!(
            qualify_in_child(&sh("exit 3"), &request(), Duration::from_secs(10)),
            Err(QualificationRejection::AbnormalExit { exit_code: 3, .. })
        ));
        assert!(matches!(
            qualify_in_child(
                &sh("cat >/dev/null; echo hello"),
                &request(),
                Duration::from_secs(10)
            ),
            Err(QualificationRejection::NoReport { .. })
        ));
        let script = format!("cat >/dev/null; echo '{REPORT_MARKER}{{\"qualified\": 3}}'");
        assert!(matches!(
            qualify_in_child(&sh(&script), &request(), Duration::from_secs(10)),
            Err(QualificationRejection::MalformedReport { .. })
        ));
    }

    #[test]
    fn a_hanging_child_times_out_and_is_killed() {
        let started = std::time::Instant::now();
        match qualify_in_child(&sh("exec sleep 30"), &request(), Duration::from_millis(200)) {
            Err(QualificationRejection::TimedOut { after, .. }) => {
                assert_eq!(after, Duration::from_millis(200))
            }
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let cmd = QualifierCommand::new("/nonexistent/echomuse", []);
        assert!(matches!(
            qualify_in_child(&cmd, &request(), Duration::from_secs(1)),
            Err(QualificationRejection::Spawn(_))
        ));
    }

    #[test]
    fn slope_fits_a_line() {
        let pts: Vec<(f64, f64)> = (0..10)
            .map(|i| (i as f64 * 0.08, 3.0 + 25.0 * i as f64 * 0.08))
            .collect();
        assert!((slope(&pts).unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(slope(&pts[..1]), None);
        assert_eq!(slope(&[(1.0, 2.0), (1.0, 3.0)]), None);
    }

    #[test]
    fn blank_rate_uses_the_run_since_the_last_emission() {
        // Decode events as the pinned model produces them: a jump of 32 frames per 1.28 s
        // chunk, after a token emitted at the first event reset the count.
        let events = [
            (2.5, 40.0),
            (2.66, 0.0),
            (3.94, 32.0),
            (5.22, 64.0),
            (6.5, 96.0),
        ];
        assert!((trailing_blank_rate(&events).unwrap() - 25.0).abs() < 1e-9);
        assert_eq!(trailing_blank_rate(&events[..2]), None);
        assert_eq!(trailing_blank_rate(&[]), None);
    }

    #[test]
    fn lcg_matches_the_fixture_generator() {
        // The generator's recurrence computed in unbounded (here 64-bit) arithmetic.
        let w = lcg_window(12_345, 20_001, 10_000, 3);
        let mut x: u64 = 12_345;
        for &s in &w {
            x = (1_103_515_245 * x + 12_345) & 0x7FFF_FFFF;
            assert_eq!(i64::from(s), ((x >> 8) % 20_001) as i64 - 10_000);
        }
    }
}
