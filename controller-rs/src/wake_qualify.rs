//! Qualification of uploaded wake models (§4.11, §5.5): every model runs in short-lived
//! child processes of this same binary before the controller accepts it, so a model
//! that crashes or hangs ONNX Runtime takes down only the child.
//!
//! Two children run per upload:
//!
//! 1. `qualify-model` ([`echomuse_speech::qualify::child_main`]): the speech crate's
//!    qualification checks the graph's inputs and outputs against its sidecar, scores its
//!    deterministic loud-noise window (the device fixture's `noise_loud` recipe) and
//!    checks that a silent window is not scored. Its noise score is the probe's `noise`.
//! 2. `qualify-wake-probes` ([`probe_child_main`]): scores the two windows the speech
//!    crate does not, with its [`BcresnetScorer`]. Both are 1.4 s at 16 kHz:
//!    - **silence**: seeded white noise, every sample in −8..=8 LSB (RMS about
//!      1.4 × 10⁻⁴ of full scale), from the same 31-bit linear congruential generator
//!      as the noise window with seed 54 321. Exact zeros cannot be the silence probe:
//!      they sit below the scorer's RMS floor and are never scored, here or on the device;
//!    - **tone**: a 1 kHz sine at −6 dBFS (amplitude 16 384).
//!
//! The registry then refuses a model whose probe score reaches its reference threshold.
//! Qualification needs the host's pinned ONNX Runtime 1.20.1 (`ONNXRUNTIME_LIB`); when
//! that library is missing or fails its pin at startup, every upload is refused naming
//! the problem, and the controller runs on without it.

use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use echomuse_api::{ModelCandidate, ModelQualifier, ModelRefused};
use echomuse_settings::registry::ProbeResult;
use echomuse_speech::{
    BcresnetModelFiles, BcresnetScorer, NativeLibraries, OrtRuntime, PinnedFile, Sha256Digest,
    WindowScore,
    bcresnet::WAKE_WINDOW_SAMPLES,
    qualify::{
        ModelUnderTest, QUALIFY_MODEL_SUBCOMMAND, QualificationEvidence, QualificationRequest,
        QualifierCommand, qualify_in_child,
    },
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

/// The argument that makes the binary score the extra probe windows.
pub const PROBE_SUBCOMMAND: &str = "qualify-wake-probes";
/// The probe child's report follows this marker on one line.
const PROBE_MARKER: &str = "echomuse-wake-probes:";
/// How long each child may take.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REQUEST_BYTES: u64 = 1 << 20;
const SILENCE_SEED: u32 = 54_321;
const SILENCE_PEAK_LSB: i32 = 8;
const TONE_HZ: f64 = 1_000.0;
const TONE_AMPLITUDE: f64 = 16_384.0;
const SAMPLE_RATE: f64 = 16_000.0;

/// What the probe child is asked to run.
#[derive(Debug, Serialize, Deserialize)]
struct ProbeRequest {
    onnxruntime: PinnedFile,
    files: BcresnetModelFiles,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeReport {
    Scored { silence: f64, tone: f64 },
    Refused { detail: String },
}

/// A deterministic int16 signal from a 31-bit linear congruential generator, the
/// device fixture generator's recipe: sample `i` is `((x >> 8) % modulus) − offset`.
fn lcg_window(seed: u32, modulus: u32, offset: i32) -> Vec<i16> {
    let mut x = seed;
    (0..WAKE_WINDOW_SAMPLES)
        .map(|_| {
            x = 1_103_515_245u32.wrapping_mul(x).wrapping_add(12_345) & 0x7FFF_FFFF;
            (((x >> 8) % modulus) as i32 - offset) as i16
        })
        .collect()
}

/// The silence probe window (see the module documentation).
fn silence_window() -> Vec<i16> {
    lcg_window(
        SILENCE_SEED,
        (2 * SILENCE_PEAK_LSB + 1) as u32,
        SILENCE_PEAK_LSB,
    )
}

/// The tone probe window (see the module documentation).
fn tone_window() -> Vec<i16> {
    (0..WAKE_WINDOW_SAMPLES)
        .map(|n| {
            let phase = std::f64::consts::TAU * TONE_HZ * n as f64 / SAMPLE_RATE;
            (TONE_AMPLITUDE * phase.sin()).round() as i16
        })
        .collect()
}

fn probes(request: &ProbeRequest) -> Result<(f64, f64), String> {
    let runtime = OrtRuntime::load(&request.onnxruntime).map_err(|e| e.to_string())?;
    let mut scorer = BcresnetScorer::load(&runtime, &request.files).map_err(|e| e.to_string())?;
    let mut score = |name: &str, window: &[i16]| match scorer.score(window) {
        Ok(WindowScore::Scored { probability }) => Ok(probability),
        Ok(WindowScore::BelowRmsFloor) => Err(format!("the {name} window was not scored")),
        Err(error) => Err(format!("scoring the {name} window: {error}")),
    };
    Ok((
        score("silence", &silence_window())?,
        score("tone", &tone_window())?,
    ))
}

/// The probe child: reads a request on stdin, prints one report line on stdout. Exits 0
/// whenever a report was written.
pub fn probe_child_main() -> ExitCode {
    let mut body = Vec::new();
    if let Err(error) = io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES)
        .read_to_end(&mut body)
    {
        eprintln!("{PROBE_SUBCOMMAND}: {error}");
        return ExitCode::from(2);
    }
    let report = match serde_json::from_slice::<ProbeRequest>(&body) {
        Err(error) => ProbeReport::Refused {
            detail: format!("request JSON: {error}"),
        },
        Ok(request) => match probes(&request) {
            Ok((silence, tone)) => ProbeReport::Scored { silence, tone },
            Err(detail) => ProbeReport::Refused { detail },
        },
    };
    let Ok(line) = serde_json::to_string(&report) else {
        return ExitCode::from(2);
    };
    let mut out = io::stdout().lock();
    if write!(out, "\n{PROBE_MARKER}{line}\n")
        .and_then(|()| out.flush())
        .is_err()
    {
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

enum State {
    Ready {
        native: NativeLibraries,
        program: PathBuf,
        scratch: PathBuf,
    },
    Unavailable(String),
}

/// [`ModelQualifier`] over the child processes. Cheap to clone.
#[derive(Clone)]
pub struct WakeModelQualifier {
    state: Arc<State>,
}

static NEXT_SCRATCH: AtomicU64 = AtomicU64::new(0);

impl WakeModelQualifier {
    /// Checks `onnxruntime_lib` against its pin, off the async runtime. `program` is
    /// the binary that routes the two subcommands (this executable in production);
    /// `scratch` is where candidate files are written for the children.
    pub async fn prepare(onnxruntime_lib: PathBuf, program: PathBuf, scratch: PathBuf) -> Self {
        let checked = tokio::task::spawn_blocking(move || {
            let native = NativeLibraries::pinned(onnxruntime_lib.clone(), None)
                .map_err(|e| e.to_string())?;
            native
                .onnxruntime
                .verify()
                .map_err(|e| format!("ONNXRUNTIME_LIB {}: {e}", onnxruntime_lib.display()))?;
            Ok::<_, String>(native)
        })
        .await
        .unwrap_or_else(|error| Err(format!("the library check failed: {error}")));
        let state = match checked {
            Ok(native) => State::Ready {
                native,
                program,
                scratch,
            },
            Err(reason) => {
                tracing::warn!(%reason, "wake-model uploads are refused");
                State::Unavailable(format!("wake-model qualification is unavailable: {reason}"))
            }
        };
        Self {
            state: Arc::new(state),
        }
    }

    /// Why uploads are refused, when they are.
    pub fn unavailable(&self) -> Option<&str> {
        match &*self.state {
            State::Unavailable(reason) => Some(reason),
            State::Ready { .. } => None,
        }
    }
}

impl ModelQualifier for WakeModelQualifier {
    async fn qualify(&self, candidate: ModelCandidate) -> Result<ProbeResult, ModelRefused> {
        let (native, program, scratch) = match &*self.state {
            State::Unavailable(reason) => return Err(ModelRefused(reason.clone())),
            State::Ready {
                native,
                program,
                scratch,
            } => (native.clone(), program.clone(), scratch.clone()),
        };
        let dir = scratch.join(format!(
            ".qualifying-{}-{}",
            std::process::id(),
            NEXT_SCRATCH.fetch_add(1, Ordering::Relaxed)
        ));
        let result = qualify_files(&native, &program, &dir, &candidate).await;
        let cleanup = dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(cleanup)).await;
        result
    }
}

fn pin(path: PathBuf, sha: &echomuse_protocol::Sha256) -> Result<PinnedFile, ModelRefused> {
    let digest = Sha256Digest::parse_hex(sha.as_str())
        .map_err(|_| ModelRefused("the candidate's hash is unreadable".to_owned()))?;
    Ok(PinnedFile::new(path, digest))
}

async fn qualify_files(
    native: &NativeLibraries,
    program: &Path,
    dir: &Path,
    candidate: &ModelCandidate,
) -> Result<ProbeResult, ModelRefused> {
    let graph = dir.join("graph.onnx");
    let sidecar = dir.join("sidecar.json");
    tokio::fs::create_dir_all(dir)
        .await
        .and(tokio::fs::write(&graph, &candidate.graph).await)
        .and(tokio::fs::write(&sidecar, &candidate.sidecar).await)
        .map_err(|error| ModelRefused(format!("the candidate could not be staged: {error}")))?;
    let files = BcresnetModelFiles {
        graph: pin(graph, &candidate.graph_sha256)?,
        sidecar: pin(sidecar, &candidate.sidecar_sha256)?,
    };

    let request = QualificationRequest {
        native: native.clone(),
        model: ModelUnderTest::Bcresnet {
            files: files.clone(),
        },
    };
    let command = QualifierCommand::new(program, [QUALIFY_MODEL_SUBCOMMAND.into()]);
    let qualified =
        tokio::task::spawn_blocking(move || qualify_in_child(&command, &request, CHILD_TIMEOUT))
            .await
            .map_err(|error| ModelRefused(format!("qualification did not run: {error}")))?
            .map_err(|rejection| ModelRefused(rejection.to_string()))?;
    let QualificationEvidence::Bcresnet { probability, .. } = qualified.evidence else {
        return Err(ModelRefused(
            "qualification reported evidence for another kind of model".to_owned(),
        ));
    };

    let (silence, tone) = run_probes(
        program,
        &ProbeRequest {
            onnxruntime: native.onnxruntime.clone(),
            files,
        },
    )
    .await?;
    ProbeResult::new(silence, probability, tone)
        .map_err(|error| ModelRefused(format!("probe scores: {error}")))
}

/// Runs the probe child and reads its report, under [`CHILD_TIMEOUT`]. A crash, a
/// timeout or a missing report is a refusal.
async fn run_probes(program: &Path, request: &ProbeRequest) -> Result<(f64, f64), ModelRefused> {
    let body = serde_json::to_vec(request)
        .map_err(|error| ModelRefused(format!("the probe request: {error}")))?;
    let mut child = tokio::process::Command::new(program)
        .arg(PROBE_SUBCOMMAND)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ModelRefused(format!("the probe child did not start: {error}")))?;
    let mut stdin = child.stdin.take();
    // wait_with_output reads stdout and stderr together, so neither can fill and stall
    // the child; dropping the future at the deadline kills it.
    let output = tokio::time::timeout(CHILD_TIMEOUT, async {
        if let Some(stdin) = stdin.as_mut() {
            // A child that exits early makes this fail; its exit status explains it.
            let _ = stdin.write_all(&body).await;
        }
        drop(stdin.take());
        child.wait_with_output().await
    })
    .await
    .map_err(|_| {
        ModelRefused(format!(
            "the probe child did not finish within {CHILD_TIMEOUT:?}"
        ))
    })?
    .map_err(|error| ModelRefused(format!("the probe child: {error}")))?;
    let (status, stdout, stderr) = (output.status, output.stdout, output.stderr);
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        let tail = String::from_utf8_lossy(&stderr[stderr.len().saturating_sub(2048)..]);
        let how = match status.signal() {
            Some(signal) => format!("crashed with signal {signal}"),
            None => format!("exited with {status}"),
        };
        return Err(ModelRefused(format!("the probe child {how}: {tail}")));
    }
    let report = String::from_utf8_lossy(&stdout)
        .lines()
        .find_map(|line| {
            line.split_once(PROBE_MARKER)
                .map(|(_, json)| json.to_owned())
        })
        .ok_or_else(|| ModelRefused("the probe child printed no report".to_owned()))?;
    match serde_json::from_str::<ProbeReport>(&report) {
        Ok(ProbeReport::Scored { silence, tone }) => Ok((silence, tone)),
        Ok(ProbeReport::Refused { detail }) => Err(ModelRefused(detail)),
        Err(error) => Err(ModelRefused(format!(
            "the probe child's report is unreadable: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(window: &[i16]) -> f64 {
        let sum: f64 = window
            .iter()
            .map(|s| (f64::from(*s) / 32_768.0).powi(2))
            .sum();
        (sum / window.len() as f64).sqrt()
    }

    #[test]
    fn the_probe_windows_are_what_the_documentation_says() {
        let silence = silence_window();
        assert_eq!(silence.len(), WAKE_WINDOW_SAMPLES);
        assert!(silence.iter().all(|s| (-8..=8).contains(s)));
        // Above the scorer's RMS floor of 1e-4, so it is scored.
        let level = rms(&silence);
        assert!(level > 1.0e-4 && level < 2.0e-4, "{level}");
        assert_eq!(silence, silence_window());

        let tone = tone_window();
        assert_eq!(tone.iter().copied().max(), Some(16_384));
        // -6 dBFS peak: RMS of a sine is peak / sqrt(2).
        let level = rms(&tone);
        assert!((level - 0.5 / 2f64.sqrt()).abs() < 1.0e-3, "{level}");
    }
}
