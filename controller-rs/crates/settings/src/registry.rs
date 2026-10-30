//! The wake-model registry under `oww_models/` (§3.5.2, §4.11): `registry.json` and the
//! content-addressed graph and sidecar files it names.
//!
//! `registry.json` is version 2: `{"models": [entry, …], "version": 2}`, written compact
//! with sorted keys and a final newline. An entry is `{graph_file, graph_sha256, probe
//! {noise, silence, tone}, sidecar_file, sidecar_sha256, thresholds {idle, playback,
//! reference}, verify_core, wake_phrase}`. File names are taken as the entry names them
//! and never renamed; files this controller adds are named `<sha256>.onnx` and
//! `<sha256>.json`. Every write goes through a temporary file and an atomic rename.
//!
//! Opening reads every entry's files and checks they hash to their names, since the
//! asset socket must serve only files that do (§3.1.9). An entry this controller cannot
//! read, or whose files are missing or wrong, is kept in `registry.json` as it was and
//! reported; it is not offered for selection or served.
//!
//! Every operation here is blocking file I/O. The settings service runs them off the
//! async runtime; adding and deleting models go only through it, so the in-use check
//! and the change happen in one place.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use echomuse_protocol::Sha256;

use crate::json::{Json, JsonObject};
use crate::values::DEPLOYED_WAKE_GRAPH_SHA256;

/// The registry index's file name.
pub const REGISTRY_FILE: &str = "registry.json";
/// The index format this controller reads and writes.
pub const REGISTRY_VERSION: i64 = 2;
/// The largest graph accepted (configuration.md "Adding a model").
pub const MAX_GRAPH_BYTES: usize = 20 * 1024 * 1024;
/// The largest sidecar accepted.
pub const MAX_SIDECAR_BYTES: usize = 64 * 1024;
/// The sidecar SHA-256 of the deployed model (post-afe-audio-architecture.md §5.1).
pub const DEPLOYED_SIDECAR_SHA256: &str =
    "25da0c652c562bf0a45a8f34bb46e38788bfc689e31401f33950831a0f2af51f";

/// Why the registry could not be opened or changed.
#[derive(Debug)]
pub enum RegistryError {
    /// File I/O failed.
    Io {
        /// What was being done.
        action: String,
        /// The error.
        source: io::Error,
    },
    /// `registry.json` is not a readable index; it is left as it is.
    UnreadableIndex(String),
    /// `registry.json` is a format version this controller does not know.
    UnsupportedVersion(Option<i64>),
    /// The seed's file does not hash to the value it must have.
    SeedMismatch {
        /// Which file.
        file: PathBuf,
        /// What it must hash to.
        expected: Sha256,
        /// What it hashes to.
        found: Sha256,
    },
    /// A new model's graph or sidecar is empty or too large.
    Size {
        /// `graph` or `sidecar`.
        what: &'static str,
        /// Its size.
        bytes: usize,
        /// The most allowed.
        max: usize,
    },
    /// The sidecar is not a valid BCResNet sidecar.
    Sidecar(SidecarError),
    /// A probe score is not below the model's reference threshold.
    ProbeTooHigh {
        /// Which probe.
        probe: &'static str,
        /// Its score.
        score: f64,
        /// The reference threshold.
        reference: f64,
    },
    /// A file named by a hash exists with other content; it is left alone.
    ExistingFileDiffers(PathBuf),
    /// No model has this graph.
    UnknownModel(Sha256),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { action, source } => write!(f, "{action}: {source}"),
            Self::UnreadableIndex(reason) => write!(f, "{REGISTRY_FILE} is unreadable: {reason}"),
            Self::UnsupportedVersion(v) => match v {
                Some(v) => write!(
                    f,
                    "{REGISTRY_FILE} is version {v}, this controller reads {REGISTRY_VERSION}"
                ),
                None => write!(f, "{REGISTRY_FILE} has no version"),
            },
            Self::SeedMismatch {
                file,
                expected,
                found,
            } => write!(
                f,
                "{} hashes to {found}, not the expected {expected}",
                file.display()
            ),
            Self::Size { what, bytes, max } => {
                write!(f, "the {what} is {bytes} bytes; it must be 1 to {max}")
            }
            Self::Sidecar(e) => write!(f, "sidecar: {e}"),
            Self::ProbeTooHigh {
                probe,
                score,
                reference,
            } => write!(
                f,
                "the {probe} probe scored {score}, not below the reference threshold {reference}"
            ),
            Self::ExistingFileDiffers(path) => {
                write!(f, "{} exists and does not hash to its name", path.display())
            }
            Self::UnknownModel(sha) => write!(f, "no registry model has graph {sha}"),
        }
    }
}

impl std::error::Error for RegistryError {}

fn io_error(action: impl Into<String>) -> impl FnOnce(io::Error) -> RegistryError {
    let action = action.into();
    move |source| RegistryError::Io { action, source }
}

/// Why a sidecar is refused, as the firmware's `bcresnet.Sidecar.Validate` refuses it.
#[derive(Clone, Debug, PartialEq)]
pub enum SidecarError {
    /// Not JSON, or a field of the wrong type.
    Unreadable(String),
    /// `type` is not `bcresnet`.
    Type(String),
    /// `sampleRate` is not 16000.
    SampleRate(i64),
    /// `window` is not 22400.
    Window(i64),
    /// No labels.
    NoLabels,
    /// `wakeIndex` outside the labels.
    WakeIndex(i64),
    /// `normPeak` outside (0, 1].
    NormPeak(f64),
    /// `nMels` or `clipSeconds` present but not positive.
    NotPositive(&'static str),
}

impl fmt::Display for SidecarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable(r) => write!(f, "unreadable: {r}"),
            Self::Type(t) => write!(f, "type {t:?}, want \"bcresnet\""),
            Self::SampleRate(r) => write!(f, "sampleRate {r}, want 16000"),
            Self::Window(w) => write!(f, "window {w}, want 22400"),
            Self::NoLabels => f.write_str("no labels"),
            Self::WakeIndex(i) => write!(f, "wakeIndex {i} outside the labels"),
            Self::NormPeak(p) => write!(f, "normPeak {p} outside (0, 1]"),
            Self::NotPositive(field) => write!(f, "{field} must be positive"),
        }
    }
}

/// A BCResNet sidecar: the JSON shipped beside the graph (field names are the
/// exporter's).
#[derive(Clone, Debug, PartialEq)]
pub struct Sidecar {
    /// `labels`: the graph's output classes.
    pub labels: Vec<String>,
    /// `wakeIndex`: the wake word's class.
    pub wake_index: usize,
    /// `normPeak`: the peak each window is scaled to.
    pub norm_peak: f64,
    /// `nMels`, when present.
    pub n_mels: Option<u32>,
    /// `clipSeconds`, when present.
    pub clip_seconds: Option<f64>,
}

impl Sidecar {
    /// The only sample rate the graph takes.
    pub const SAMPLE_RATE: i64 = 16_000;
    /// The graph's input length, 1.4 s at 16 kHz.
    pub const WINDOW: i64 = 22_400;

    /// Parse and check a sidecar the way the firmware does: type `bcresnet`, 16 kHz, a
    /// 22,400-sample window, labels with the wake index inside them, `normPeak` in (0, 1].
    pub fn parse(bytes: &[u8]) -> Result<Self, SidecarError> {
        let text =
            std::str::from_utf8(bytes).map_err(|e| SidecarError::Unreadable(e.to_string()))?;
        let Json::Object(object) =
            Json::parse(text).map_err(|e| SidecarError::Unreadable(e.to_string()))?
        else {
            return Err(SidecarError::Unreadable("not a JSON object".to_owned()));
        };
        let wrong = |field: &str| {
            SidecarError::Unreadable(format!("{field} is missing or of the wrong type"))
        };
        let kind = match object.get("type") {
            Some(Json::String(t)) => t.clone(),
            _ => return Err(wrong("type")),
        };
        if kind != "bcresnet" {
            return Err(SidecarError::Type(kind));
        }
        let rate = object
            .get("sampleRate")
            .and_then(Json::as_whole)
            .ok_or_else(|| wrong("sampleRate"))?;
        if rate != Self::SAMPLE_RATE {
            return Err(SidecarError::SampleRate(rate));
        }
        let window = object
            .get("window")
            .and_then(Json::as_whole)
            .ok_or_else(|| wrong("window"))?;
        if window != Self::WINDOW {
            return Err(SidecarError::Window(window));
        }
        let labels = match object.get("labels") {
            Some(Json::Array(items)) => items
                .iter()
                .map(|i| match i {
                    Json::String(s) => Ok(s.clone()),
                    _ => Err(wrong("labels")),
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(wrong("labels")),
        };
        if labels.is_empty() {
            return Err(SidecarError::NoLabels);
        }
        let wake_index = object
            .get("wakeIndex")
            .and_then(Json::as_whole)
            .ok_or_else(|| wrong("wakeIndex"))?;
        let wake_index = usize::try_from(wake_index)
            .ok()
            .filter(|i| *i < labels.len())
            .ok_or(SidecarError::WakeIndex(wake_index))?;
        let norm_peak = object
            .get("normPeak")
            .and_then(Json::as_f64)
            .ok_or_else(|| wrong("normPeak"))?;
        if !(norm_peak > 0.0 && norm_peak <= 1.0) {
            return Err(SidecarError::NormPeak(norm_peak));
        }
        let n_mels = match object.get("nMels") {
            None => None,
            Some(v) => Some(
                v.as_whole()
                    .and_then(|n| u32::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or(SidecarError::NotPositive("nMels"))?,
            ),
        };
        let clip_seconds = match object.get("clipSeconds") {
            None => None,
            Some(v) => Some(
                v.as_f64()
                    .filter(|s| *s > 0.0 && s.is_finite())
                    .ok_or(SidecarError::NotPositive("clipSeconds"))?,
            ),
        };
        Ok(Self {
            labels,
            wake_index,
            norm_peak,
            n_mels,
            clip_seconds,
        })
    }
}

/// A model's thresholds `{idle, playback, reference}`, each in (0, 1] with
/// reference ≤ playback ≤ idle (configuration.md "Adding a model"). Idle and playback go
/// to the device; reference is the controller's threshold for its own audio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ModelThresholds {
    idle: f64,
    playback: f64,
    reference: f64,
}

/// Why thresholds or a probe result are refused.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ModelValueError {
    /// A value outside its range.
    OutOfRange {
        /// Which value.
        name: &'static str,
        /// The value.
        value: f64,
    },
    /// Not reference ≤ playback ≤ idle.
    Order,
}

impl fmt::Display for ModelValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfRange { name, value } => write!(f, "{name} {value} is out of range"),
            Self::Order => f.write_str("thresholds must satisfy reference <= playback <= idle"),
        }
    }
}

impl std::error::Error for ModelValueError {}

impl ModelThresholds {
    /// The deployed model's thresholds (post-afe-audio-architecture.md §5.1).
    pub const DEPLOYED: Self = Self {
        idle: 0.90,
        playback: 0.65,
        reference: 0.30,
    };

    /// Thresholds, when each is in (0, 1] and reference ≤ playback ≤ idle.
    pub fn new(idle: f64, playback: f64, reference: f64) -> Result<Self, ModelValueError> {
        for (name, value) in [
            ("idle", idle),
            ("playback", playback),
            ("reference", reference),
        ] {
            if !(value > 0.0 && value <= 1.0) {
                return Err(ModelValueError::OutOfRange { name, value });
            }
        }
        if !(reference <= playback && playback <= idle) {
            return Err(ModelValueError::Order);
        }
        Ok(Self {
            idle,
            playback,
            reference,
        })
    }

    /// The wake threshold while the device plays nothing.
    pub fn idle(&self) -> f64 {
        self.idle
    }

    /// The wake threshold while it plays content or rings.
    pub fn playback(&self) -> f64 {
        self.playback
    }

    /// The controller's threshold for the device's own audio.
    pub fn reference(&self) -> f64 {
        self.reference
    }
}

/// The validation probe's scores on silence, noise and a tone, each in [0, 1]. The
/// probe runs the model, which the speech crate does; this type carries its result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeResult {
    silence: f64,
    noise: f64,
    tone: f64,
}

impl ProbeResult {
    /// The scores, when each is in [0, 1].
    pub fn new(silence: f64, noise: f64, tone: f64) -> Result<Self, ModelValueError> {
        for (name, value) in [("silence", silence), ("noise", noise), ("tone", tone)] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ModelValueError::OutOfRange { name, value });
            }
        }
        Ok(Self {
            silence,
            noise,
            tone,
        })
    }

    /// The silence score.
    pub fn silence(&self) -> f64 {
        self.silence
    }

    /// The noise score.
    pub fn noise(&self) -> f64 {
        self.noise
    }

    /// The tone score.
    pub fn tone(&self) -> f64 {
        self.tone
    }

    fn scores(&self) -> [(&'static str, f64); 3] {
        [
            ("silence", self.silence),
            ("noise", self.noise),
            ("tone", self.tone),
        ]
    }
}

/// A model's spoken form: `wake_phrase` (removed from transcripts) or `verify_core`
/// (found in recognised speech to confirm a wake). Lowercase letters only.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SpokenForm(String);

impl SpokenForm {
    /// The most characters.
    pub const MAX_CHARS: usize = 64;

    /// The form, when it is 1 to 64 lowercase ASCII letters.
    pub fn new(text: &str) -> Result<Self, ModelValueError> {
        if !text.is_empty()
            && text.len() <= Self::MAX_CHARS
            && text.bytes().all(|b| b.is_ascii_lowercase())
        {
            Ok(Self(text.to_owned()))
        } else {
            Err(ModelValueError::OutOfRange {
                name: "spoken form (lowercase letters only)",
                value: text.len() as f64,
            })
        }
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether a file of an entry was found as named.
#[derive(Clone, Debug, PartialEq)]
pub enum FileStatus {
    /// It exists and hashes to its SHA-256.
    Verified,
    /// The entry's name is not a plain file name inside the directory.
    UnsafeName,
    /// It could not be read.
    Missing(String),
    /// It hashes to something else.
    HashMismatch(Sha256),
}

/// One registry model.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeModel {
    graph_file: String,
    graph_sha256: Sha256,
    sidecar_file: String,
    sidecar_sha256: Sha256,
    thresholds: ModelThresholds,
    wake_phrase: SpokenForm,
    verify_core: SpokenForm,
    probe: Option<ProbeResult>,
    /// Fields of the entry this controller does not know, written back as they were.
    extra: JsonObject,
    graph_status: FileStatus,
    sidecar_status: FileStatus,
    sidecar: Option<Sidecar>,
}

impl WakeModel {
    /// The graph's SHA-256: the model's identity and the `wakeModel` value.
    pub fn graph_sha256(&self) -> &Sha256 {
        &self.graph_sha256
    }

    /// The sidecar's SHA-256.
    pub fn sidecar_sha256(&self) -> &Sha256 {
        &self.sidecar_sha256
    }

    /// The graph's file name in the registry directory.
    pub fn graph_file(&self) -> &str {
        &self.graph_file
    }

    /// The sidecar's file name in the registry directory.
    pub fn sidecar_file(&self) -> &str {
        &self.sidecar_file
    }

    /// The thresholds.
    pub fn thresholds(&self) -> ModelThresholds {
        self.thresholds
    }

    /// `wake_phrase`.
    pub fn wake_phrase(&self) -> &SpokenForm {
        &self.wake_phrase
    }

    /// `verify_core`.
    pub fn verify_core(&self) -> &SpokenForm {
        &self.verify_core
    }

    /// The validation probe's result; `None` when unknown (a registry seeded without
    /// running the model).
    pub fn probe(&self) -> Option<ProbeResult> {
        self.probe
    }

    /// Whether both files verified and the sidecar parsed, so the model can be served
    /// and selected.
    pub fn is_usable(&self) -> bool {
        self.graph_status == FileStatus::Verified
            && self.sidecar_status == FileStatus::Verified
            && self.sidecar.is_some()
    }

    fn to_json(&self) -> Json {
        let mut object = self.extra.clone();
        object.insert("graph_file", Json::String(self.graph_file.clone()));
        object.insert("graph_sha256", Json::String(self.graph_sha256.to_string()));
        object.insert(
            "probe",
            match self.probe {
                Some(p) => Json::Object(
                    [("noise", p.noise), ("silence", p.silence), ("tone", p.tone)]
                        .into_iter()
                        .map(|(k, v)| (k.to_owned(), float(v)))
                        .collect(),
                ),
                None => Json::Null,
            },
        );
        object.insert("sidecar_file", Json::String(self.sidecar_file.clone()));
        object.insert(
            "sidecar_sha256",
            Json::String(self.sidecar_sha256.to_string()),
        );
        object.insert(
            "thresholds",
            Json::Object(
                [
                    ("idle", self.thresholds.idle),
                    ("playback", self.thresholds.playback),
                    ("reference", self.thresholds.reference),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), float(v)))
                .collect(),
            ),
        );
        object.insert("verify_core", Json::String(self.verify_core.0.clone()));
        object.insert("wake_phrase", Json::String(self.wake_phrase.0.clone()));
        Json::Object(object)
    }
}

/// A float as Python stores one: always with a fraction or exponent.
fn float(value: f64) -> Json {
    Json::float(value).expect("validated finite")
}

/// A registry entry problem, reported and otherwise left alone.
#[derive(Clone, Debug, PartialEq)]
pub enum RegistryIssue {
    /// An entry this controller cannot read; kept in the index as it was.
    UnreadableEntry {
        /// Its position in `models`.
        index: usize,
        /// Why.
        reason: String,
    },
    /// A model whose files are not usable; kept, but not served or selectable.
    UnusableFiles {
        /// The model's graph SHA-256.
        graph_sha256: Sha256,
        /// The graph file's status.
        graph: FileStatus,
        /// The sidecar file's status.
        sidecar: FileStatus,
        /// Why the sidecar did not parse, when it did not.
        sidecar_error: Option<SidecarError>,
    },
    /// A second entry for a graph already listed; kept, the first one is used.
    DuplicateGraph(Sha256),
}

#[derive(Clone, Debug, PartialEq)]
enum Entry {
    Model(Box<WakeModel>),
    Unreadable(Json),
}

/// The registry: the directory and the index as last read or written. Cheap to clone.
#[derive(Clone, Debug, PartialEq)]
pub struct WakeRegistry {
    dir: PathBuf,
    entries: Arc<Vec<Entry>>,
    /// Top-level fields of the index this controller does not know.
    extra: Arc<JsonObject>,
    issues: Arc<Vec<RegistryIssue>>,
}

/// The model a fresh registry is seeded with.
#[derive(Clone, Debug)]
pub struct SeedModel {
    /// The graph to copy in.
    pub graph_path: PathBuf,
    /// The sidecar to copy in.
    pub sidecar_path: PathBuf,
    /// What the graph must hash to, when pinned.
    pub expected_graph: Option<Sha256>,
    /// What the sidecar must hash to, when pinned.
    pub expected_sidecar: Option<Sha256>,
    /// Its thresholds.
    pub thresholds: ModelThresholds,
    /// Its wake phrase.
    pub wake_phrase: SpokenForm,
    /// Its verification core.
    pub verify_core: SpokenForm,
    /// Its probe result, `None` when it was not run; then the entry stores `null`.
    pub probe: Option<ProbeResult>,
}

impl SeedModel {
    /// The deployed Ophelia model from the repository's `bcresnet_audio.onnx` and
    /// `.json`, pinned to the deployed hashes, with thresholds {0.90, 0.65, 0.30},
    /// wake phrase `ophelia` and verification core `ophel`
    /// (post-afe-audio-architecture.md §5.1). Its probe result is unknown.
    pub fn deployed(graph_path: impl Into<PathBuf>, sidecar_path: impl Into<PathBuf>) -> Self {
        Self {
            graph_path: graph_path.into(),
            sidecar_path: sidecar_path.into(),
            expected_graph: Sha256::parse(DEPLOYED_WAKE_GRAPH_SHA256),
            expected_sidecar: Sha256::parse(DEPLOYED_SIDECAR_SHA256),
            thresholds: ModelThresholds::DEPLOYED,
            wake_phrase: SpokenForm("ophelia".to_owned()),
            verify_core: SpokenForm("ophel".to_owned()),
            probe: None,
        }
    }
}

/// A validated model to add.
#[derive(Clone, Debug)]
pub struct NewModel {
    /// The ONNX graph's bytes.
    pub graph: Vec<u8>,
    /// The sidecar's bytes.
    pub sidecar: Vec<u8>,
    /// Its thresholds.
    pub thresholds: ModelThresholds,
    /// Its wake phrase.
    pub wake_phrase: SpokenForm,
    /// Its verification core.
    pub verify_core: SpokenForm,
    /// The speech crate's probe of this graph and sidecar.
    pub probe: ProbeResult,
}

/// What adding a model did.
#[derive(Clone, Debug, PartialEq)]
pub enum AddOutcome {
    /// The model was added; it is not selected anywhere yet.
    Added(Box<WakeModel>),
    /// A model with this graph was already registered; it is returned unchanged.
    AlreadyRegistered(Box<WakeModel>),
}

impl WakeRegistry {
    /// Open the registry in `dir`, or create it with `seed` when it has no
    /// `registry.json`. An existing index is read, never rewritten, and nothing in the
    /// directory is renamed.
    pub fn open_or_seed(dir: impl Into<PathBuf>, seed: &SeedModel) -> Result<Self, RegistryError> {
        let dir = dir.into();
        let index = dir.join(REGISTRY_FILE);
        match fs::read(&index) {
            Ok(bytes) => Self::from_index(dir, &bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::seed(dir, seed),
            Err(e) => Err(io_error(format!("reading {}", index.display()))(e)),
        }
    }

    fn from_index(dir: PathBuf, bytes: &[u8]) -> Result<Self, RegistryError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| RegistryError::UnreadableIndex(e.to_string()))?;
        let Json::Object(mut top) =
            Json::parse(text).map_err(|e| RegistryError::UnreadableIndex(e.to_string()))?
        else {
            return Err(RegistryError::UnreadableIndex(
                "not a JSON object".to_owned(),
            ));
        };
        let version = top.remove("version").and_then(|v| v.as_whole());
        if version != Some(REGISTRY_VERSION) {
            return Err(RegistryError::UnsupportedVersion(version));
        }
        let Some(Json::Array(raw_entries)) = top.remove("models") else {
            return Err(RegistryError::UnreadableIndex(
                "models is not a list".to_owned(),
            ));
        };
        let mut entries = Vec::with_capacity(raw_entries.len());
        let mut issues = Vec::new();
        for (index, raw) in raw_entries.into_iter().enumerate() {
            let mut model = match parse_entry(&raw) {
                Ok(model) => model,
                Err(reason) => {
                    issues.push(RegistryIssue::UnreadableEntry { index, reason });
                    entries.push(Entry::Unreadable(raw));
                    continue;
                }
            };
            let duplicate = entries
                .iter()
                .any(|e| matches!(e, Entry::Model(m) if m.graph_sha256 == model.graph_sha256));
            if duplicate {
                issues.push(RegistryIssue::DuplicateGraph(model.graph_sha256.clone()));
                entries.push(Entry::Unreadable(raw));
                continue;
            }
            let sidecar_error = verify_files(&dir, &mut model);
            if !model.is_usable() {
                issues.push(RegistryIssue::UnusableFiles {
                    graph_sha256: model.graph_sha256.clone(),
                    graph: model.graph_status.clone(),
                    sidecar: model.sidecar_status.clone(),
                    sidecar_error,
                });
            }
            entries.push(Entry::Model(Box::new(model)));
        }
        Ok(Self {
            dir,
            entries: Arc::new(entries),
            extra: Arc::new(top),
            issues: Arc::new(issues),
        })
    }

    fn seed(dir: PathBuf, seed: &SeedModel) -> Result<Self, RegistryError> {
        fs::create_dir_all(&dir).map_err(io_error(format!("creating {}", dir.display())))?;
        let graph = fs::read(&seed.graph_path)
            .map_err(io_error(format!("reading {}", seed.graph_path.display())))?;
        let sidecar = fs::read(&seed.sidecar_path)
            .map_err(io_error(format!("reading {}", seed.sidecar_path.display())))?;
        for (path, bytes, expected) in [
            (&seed.graph_path, &graph, &seed.expected_graph),
            (&seed.sidecar_path, &sidecar, &seed.expected_sidecar),
        ] {
            let found = Sha256::of(bytes);
            if let Some(expected) = expected
                && *expected != found
            {
                return Err(RegistryError::SeedMismatch {
                    file: path.clone(),
                    expected: expected.clone(),
                    found,
                });
            }
        }
        let empty = Self {
            dir,
            entries: Arc::new(Vec::new()),
            extra: Arc::new(JsonObject::new()),
            issues: Arc::new(Vec::new()),
        };
        let model = empty.store_files(
            &graph,
            &sidecar,
            seed.thresholds,
            seed.wake_phrase.clone(),
            seed.verify_core.clone(),
            seed.probe,
        )?;
        empty.with_entries(vec![Entry::Model(Box::new(model))])
    }

    /// The directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every readable model, in index order, usable or not.
    pub fn models(&self) -> impl Iterator<Item = &WakeModel> {
        self.entries.iter().filter_map(|e| match e {
            Entry::Model(m) => Some(m.as_ref()),
            Entry::Unreadable(_) => None,
        })
    }

    /// The model with this graph.
    pub fn model(&self, graph_sha256: &Sha256) -> Option<&WakeModel> {
        self.models().find(|m| m.graph_sha256 == *graph_sha256)
    }

    /// What was found wrong when the registry was read.
    pub fn issues(&self) -> &[RegistryIssue] {
        &self.issues
    }

    /// The file to serve for an asset SHA-256: the graph or sidecar of a model whose
    /// files verified.
    pub fn asset_path(&self, sha256: &Sha256) -> Option<PathBuf> {
        self.models().filter(|m| m.is_usable()).find_map(|m| {
            if m.graph_sha256 == *sha256 {
                Some(self.dir.join(&m.graph_file))
            } else if m.sidecar_sha256 == *sha256 {
                Some(self.dir.join(&m.sidecar_file))
            } else {
                None
            }
        })
    }

    /// The index text in the stored shape.
    pub fn index_text(&self) -> String {
        let mut top = (*self.extra).clone();
        let models = self
            .entries
            .iter()
            .map(|e| match e {
                Entry::Model(m) => m.to_json(),
                Entry::Unreadable(raw) => raw.clone(),
            })
            .collect();
        top.insert("models", Json::Array(models));
        top.insert("version", Json::int(REGISTRY_VERSION));
        let mut text = Json::Object(top).to_compact_sorted_text();
        text.push('\n');
        text
    }

    /// Add a model: write its files under their hashes, then the index with the new
    /// entry. A graph already registered returns that entry and changes nothing.
    pub(crate) fn added(&self, new: NewModel) -> Result<(Self, AddOutcome), RegistryError> {
        for (what, bytes, max) in [
            ("graph", new.graph.len(), MAX_GRAPH_BYTES),
            ("sidecar", new.sidecar.len(), MAX_SIDECAR_BYTES),
        ] {
            if bytes == 0 || bytes > max {
                return Err(RegistryError::Size { what, bytes, max });
            }
        }
        Sidecar::parse(&new.sidecar).map_err(RegistryError::Sidecar)?;
        let graph_sha256 = Sha256::of(&new.graph);
        if let Some(existing) = self.model(&graph_sha256) {
            return Ok((
                self.clone(),
                AddOutcome::AlreadyRegistered(Box::new(existing.clone())),
            ));
        }
        let reference = new.thresholds.reference;
        if let Some((probe, score)) = new
            .probe
            .scores()
            .into_iter()
            .find(|(_, s)| *s >= reference)
        {
            return Err(RegistryError::ProbeTooHigh {
                probe,
                score,
                reference,
            });
        }
        let model = self.store_files(
            &new.graph,
            &new.sidecar,
            new.thresholds,
            new.wake_phrase,
            new.verify_core,
            Some(new.probe),
        )?;
        let mut entries = (*self.entries).clone();
        entries.push(Entry::Model(Box::new(model.clone())));
        let registry = self.with_entries(entries)?;
        Ok((registry, AddOutcome::Added(Box::new(model))))
    }

    /// Remove a model's entry, then delete its files unless another entry names them.
    /// The caller has checked that nothing selects it.
    pub(crate) fn without(&self, graph_sha256: &Sha256) -> Result<Self, RegistryError> {
        let Some(model) = self.model(graph_sha256).cloned() else {
            return Err(RegistryError::UnknownModel(graph_sha256.clone()));
        };
        let entries: Vec<Entry> = self
            .entries
            .iter()
            .filter(|e| !matches!(e, Entry::Model(m) if m.graph_sha256 == *graph_sha256))
            .cloned()
            .collect();
        let registry = self.with_entries(entries)?;
        // The index no longer names the files, so a crash from here leaves only unnamed
        // files behind, never an entry without its files.
        for file in [&model.graph_file, &model.sidecar_file] {
            let still_named = registry
                .models()
                .any(|m| m.graph_file == *file || m.sidecar_file == *file);
            if !still_named && safe_name(file) {
                match fs::remove_file(self.dir.join(file)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_error(format!("deleting {file}"))(e)),
                }
            }
        }
        sync_dir(&self.dir)?;
        Ok(registry)
    }

    fn store_files(
        &self,
        graph: &[u8],
        sidecar_bytes: &[u8],
        thresholds: ModelThresholds,
        wake_phrase: SpokenForm,
        verify_core: SpokenForm,
        probe: Option<ProbeResult>,
    ) -> Result<WakeModel, RegistryError> {
        let sidecar = Sidecar::parse(sidecar_bytes).map_err(RegistryError::Sidecar)?;
        let graph_sha256 = Sha256::of(graph);
        let sidecar_sha256 = Sha256::of(sidecar_bytes);
        let graph_file = format!("{graph_sha256}.onnx");
        let sidecar_file = format!("{sidecar_sha256}.json");
        write_content_addressed(&self.dir, &graph_file, graph, &graph_sha256)?;
        write_content_addressed(&self.dir, &sidecar_file, sidecar_bytes, &sidecar_sha256)?;
        Ok(WakeModel {
            graph_file,
            graph_sha256,
            sidecar_file,
            sidecar_sha256,
            thresholds,
            wake_phrase,
            verify_core,
            probe,
            extra: JsonObject::new(),
            graph_status: FileStatus::Verified,
            sidecar_status: FileStatus::Verified,
            sidecar: Some(sidecar),
        })
    }

    fn with_entries(&self, entries: Vec<Entry>) -> Result<Self, RegistryError> {
        let registry = Self {
            dir: self.dir.clone(),
            entries: Arc::new(entries),
            extra: self.extra.clone(),
            issues: self.issues.clone(),
        };
        write_atomic(&self.dir, REGISTRY_FILE, registry.index_text().as_bytes())?;
        Ok(registry)
    }
}

fn parse_entry(raw: &Json) -> Result<WakeModel, String> {
    let Json::Object(object) = raw else {
        return Err("not an object".to_owned());
    };
    let text = |field: &str| match object.get(field) {
        Some(Json::String(s)) => Ok(s.clone()),
        _ => Err(format!("{field} is missing or not a string")),
    };
    let sha = |field: &str| {
        Sha256::parse(&text(field)?).ok_or_else(|| format!("{field} is not a SHA-256"))
    };
    let numbers = |field: &str, names: [&str; 3]| -> Result<[f64; 3], String> {
        let Some(Json::Object(inner)) = object.get(field) else {
            return Err(format!("{field} is missing or not an object"));
        };
        if let Some((k, _)) = inner.iter().find(|(k, _)| !names.contains(k)) {
            return Err(format!("{field} has an unknown field {k:?}"));
        }
        let mut out = [0.0; 3];
        for (slot, name) in out.iter_mut().zip(names) {
            *slot = inner
                .get(name)
                .and_then(Json::as_f64)
                .ok_or_else(|| format!("{field}.{name} is missing or not a number"))?;
        }
        Ok(out)
    };
    let [idle, playback, reference] = numbers("thresholds", ["idle", "playback", "reference"])?;
    let thresholds =
        ModelThresholds::new(idle, playback, reference).map_err(|e| format!("thresholds: {e}"))?;
    let probe = match object.get("probe") {
        None | Some(Json::Null) => None,
        Some(_) => {
            let [silence, noise, tone] = numbers("probe", ["silence", "noise", "tone"])?;
            Some(ProbeResult::new(silence, noise, tone).map_err(|e| format!("probe: {e}"))?)
        }
    };
    let spoken = |field: &str| SpokenForm::new(&text(field)?).map_err(|e| format!("{field}: {e}"));
    const KNOWN: [&str; 8] = [
        "graph_file",
        "graph_sha256",
        "probe",
        "sidecar_file",
        "sidecar_sha256",
        "thresholds",
        "verify_core",
        "wake_phrase",
    ];
    let extra = object
        .iter()
        .filter(|(k, _)| !KNOWN.contains(k))
        .map(|(k, v)| (k.to_owned(), v.clone()))
        .collect();
    Ok(WakeModel {
        graph_file: text("graph_file")?,
        graph_sha256: sha("graph_sha256")?,
        sidecar_file: text("sidecar_file")?,
        sidecar_sha256: sha("sidecar_sha256")?,
        thresholds,
        wake_phrase: spoken("wake_phrase")?,
        verify_core: spoken("verify_core")?,
        probe,
        extra,
        graph_status: FileStatus::Missing("not checked".to_owned()),
        sidecar_status: FileStatus::Missing("not checked".to_owned()),
        sidecar: None,
    })
}

/// Check the model's files against their hashes and parse its sidecar. Returns why the
/// sidecar did not parse, when its file verified but did not.
fn verify_files(dir: &Path, model: &mut WakeModel) -> Option<SidecarError> {
    model.graph_status = file_status(dir, &model.graph_file, &model.graph_sha256).0;
    let (status, bytes) = file_status(dir, &model.sidecar_file, &model.sidecar_sha256);
    model.sidecar_status = status;
    match Sidecar::parse(&bytes?) {
        Ok(sidecar) => {
            model.sidecar = Some(sidecar);
            None
        }
        Err(error) => Some(error),
    }
}

/// The file's status, and its bytes when it verified.
fn file_status(dir: &Path, name: &str, sha256: &Sha256) -> (FileStatus, Option<Vec<u8>>) {
    if !safe_name(name) {
        return (FileStatus::UnsafeName, None);
    }
    match fs::read(dir.join(name)) {
        Ok(bytes) => {
            let found = Sha256::of(&bytes);
            if found == *sha256 {
                (FileStatus::Verified, Some(bytes))
            } else {
                (FileStatus::HashMismatch(found), None)
            }
        }
        Err(e) => (FileStatus::Missing(e.to_string()), None),
    }
}

/// A plain file name inside the directory: no separators, not hidden, not `..`.
fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\', '\0'])
}

/// Write a file named by its hash, unless it already exists with that content. A file
/// of that name with other content is left alone and refused.
fn write_content_addressed(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    sha256: &Sha256,
) -> Result<(), RegistryError> {
    let path = dir.join(name);
    match fs::read(&path) {
        Ok(existing) if Sha256::of(&existing) == *sha256 => Ok(()),
        Ok(_) => Err(RegistryError::ExistingFileDiffers(path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => write_atomic(dir, name, bytes),
        Err(e) => Err(io_error(format!("reading {}", path.display()))(e)),
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `name` in `dir` through a temporary file, flushed to disk, renamed over the
/// target, and the directory synced, so a reader sees the old file or the new one.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<(), RegistryError> {
    let temp = dir.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = File::options().write(true).create_new(true).open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(name))
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(io_error(format!("writing {name}"))(e));
    }
    sync_dir(dir)
}

fn sync_dir(dir: &Path) -> Result<(), RegistryError> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(io_error(format!("syncing {}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repository's deployed graph and sidecar.
    fn repo_model() -> (PathBuf, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        (
            root.join("bcresnet_audio.onnx"),
            root.join("bcresnet_audio.json"),
        )
    }

    fn seeded(dir: &Path) -> WakeRegistry {
        let (graph, sidecar) = repo_model();
        WakeRegistry::open_or_seed(dir.join("oww_models"), &SeedModel::deployed(graph, sidecar))
            .unwrap()
    }

    /// A sidecar the firmware accepts, different from the deployed one.
    fn synthetic_sidecar(label: &str) -> Vec<u8> {
        format!(
            r#"{{"type": "bcresnet", "sampleRate": 16000, "window": 22400, "labels": ["noise", "{label}", "unknown"], "wakeIndex": 1, "nMels": 40, "clipSeconds": 1.4, "normPeak": 0.8}}"#
        )
        .into_bytes()
    }

    fn new_model(graph: &[u8], label: &str) -> NewModel {
        NewModel {
            graph: graph.to_vec(),
            sidecar: synthetic_sidecar(label),
            thresholds: ModelThresholds::new(0.85, 0.6, 0.25).unwrap(),
            wake_phrase: SpokenForm::new(label).unwrap(),
            verify_core: SpokenForm::new(&label[..3]).unwrap(),
            probe: ProbeResult::new(0.001, 0.01, 0.002).unwrap(),
        }
    }

    fn dir_listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_fresh_registry_is_seeded_content_addressed() {
        let temp = tempfile::tempdir().unwrap();
        let registry = seeded(temp.path());
        let dir = registry.dir().to_owned();
        assert_eq!(
            dir_listing(&dir),
            [
                format!("{DEPLOYED_SIDECAR_SHA256}.json"),
                format!("{DEPLOYED_WAKE_GRAPH_SHA256}.onnx"),
                "registry.json".to_owned()
            ]
        );
        let model = registry.models().next().unwrap();
        assert!(model.is_usable());
        assert_eq!(
            fs::read_to_string(dir.join(REGISTRY_FILE)).unwrap(),
            format!(
                concat!(
                    r#"{{"models":[{{"graph_file":"{g}.onnx","graph_sha256":"{g}","probe":null,"#,
                    r#""sidecar_file":"{s}.json","sidecar_sha256":"{s}","#,
                    r#""thresholds":{{"idle":0.9,"playback":0.65,"reference":0.3}},"#,
                    r#""verify_core":"ophel","wake_phrase":"ophelia"}}],"version":2}}"#,
                    "\n"
                ),
                g = DEPLOYED_WAKE_GRAPH_SHA256,
                s = DEPLOYED_SIDECAR_SHA256
            )
        );
        // Opening again reads the same registry and writes nothing.
        let reopened = seeded(temp.path());
        assert_eq!(reopened, registry);
    }

    #[test]
    fn an_existing_index_is_read_as_stored_and_never_renamed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("oww_models");
        fs::create_dir(&dir).unwrap();
        let (graph, sidecar) = repo_model();
        // Names an earlier controller might have used; they must be served as named.
        fs::copy(&graph, dir.join("bcresnet_audio.onnx")).unwrap();
        fs::copy(&sidecar, dir.join("bcresnet_audio.json")).unwrap();
        // The deployed index's shape, with synthetic probe scores and an unknown field.
        let index = format!(
            concat!(
                r#"{{"models":[{{"graph_file":"bcresnet_audio.onnx","graph_sha256":"{g}","#,
                r#""note":"kept","probe":{{"noise":0.0123,"silence":0.0045,"tone":3.3e-05}},"#,
                r#""sidecar_file":"bcresnet_audio.json","sidecar_sha256":"{s}","#,
                r#""thresholds":{{"idle":0.9,"playback":0.65,"reference":0.3}},"#,
                r#""verify_core":"ophel","wake_phrase":"ophelia"}},{{"graph_file":"x"}}],"version":2}}"#,
                "\n"
            ),
            g = DEPLOYED_WAKE_GRAPH_SHA256,
            s = DEPLOYED_SIDECAR_SHA256
        );
        fs::write(dir.join(REGISTRY_FILE), &index).unwrap();
        let before = dir_listing(&dir);
        let registry =
            WakeRegistry::open_or_seed(&dir, &SeedModel::deployed(&graph, &sidecar)).unwrap();
        assert_eq!(dir_listing(&dir), before);
        assert_eq!(registry.index_text(), index);
        assert_eq!(registry.models().count(), 1);
        assert!(matches!(
            registry.issues(),
            [RegistryIssue::UnreadableEntry { index: 1, .. }]
        ));
        let graph_sha = Sha256::parse(DEPLOYED_WAKE_GRAPH_SHA256).unwrap();
        assert_eq!(
            registry.asset_path(&graph_sha),
            Some(dir.join("bcresnet_audio.onnx"))
        );
        let probe = registry.model(&graph_sha).unwrap().probe().unwrap();
        assert_eq!(probe.tone(), 3.3e-5);
    }

    #[test]
    fn files_that_do_not_hash_to_their_names_are_not_served() {
        let temp = tempfile::tempdir().unwrap();
        let registry = seeded(temp.path());
        let graph_sha = Sha256::parse(DEPLOYED_WAKE_GRAPH_SHA256).unwrap();
        fs::write(registry.dir().join(format!("{graph_sha}.onnx")), b"corrupt").unwrap();
        let (graph, sidecar) = repo_model();
        let reopened =
            WakeRegistry::open_or_seed(registry.dir(), &SeedModel::deployed(graph, sidecar))
                .unwrap();
        assert_eq!(reopened.asset_path(&graph_sha), None);
        assert!(matches!(
            reopened.issues(),
            [RegistryIssue::UnusableFiles { .. }]
        ));
    }

    #[test]
    fn unknown_versions_and_wrong_seeds_are_refused() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("m");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(REGISTRY_FILE), r#"{"models":[],"version":3}"#).unwrap();
        let (graph, sidecar) = repo_model();
        assert!(matches!(
            WakeRegistry::open_or_seed(&dir, &SeedModel::deployed(&graph, &sidecar)),
            Err(RegistryError::UnsupportedVersion(Some(3)))
        ));
        let wrong = SeedModel::deployed(&sidecar, &sidecar);
        assert!(matches!(
            WakeRegistry::open_or_seed(temp.path().join("fresh"), &wrong),
            Err(RegistryError::SeedMismatch { .. })
        ));
    }

    #[test]
    fn add_writes_hash_named_files_and_delete_removes_them() {
        let temp = tempfile::tempdir().unwrap();
        let registry = seeded(temp.path());
        let new = new_model(b"synthetic graph bytes", "tara");
        let graph_sha = Sha256::of(&new.graph);
        let sidecar_sha = Sha256::of(&new.sidecar);
        let (added, outcome) = registry.added(new.clone()).unwrap();
        assert!(matches!(outcome, AddOutcome::Added(_)));
        assert!(added.dir().join(format!("{graph_sha}.onnx")).exists());
        assert!(added.dir().join(format!("{sidecar_sha}.json")).exists());
        assert_eq!(
            added.asset_path(&sidecar_sha),
            Some(added.dir().join(format!("{sidecar_sha}.json")))
        );
        // Adding the same graph again returns the entry unchanged.
        let mut again = new.clone();
        again.thresholds = ModelThresholds::new(0.5, 0.4, 0.3).unwrap();
        let (same, outcome) = added.added(again).unwrap();
        assert!(
            matches!(outcome, AddOutcome::AlreadyRegistered(m) if m.thresholds().idle() == 0.85)
        );
        assert_eq!(same, added);
        // Reading it back gives the same models.
        let (graph, sidecar) = repo_model();
        let reread =
            WakeRegistry::open_or_seed(added.dir(), &SeedModel::deployed(graph, sidecar)).unwrap();
        assert_eq!(reread.index_text(), added.index_text());
        // Delete removes the entry and the files.
        let removed = added.without(&graph_sha).unwrap();
        assert!(removed.model(&graph_sha).is_none());
        assert!(!added.dir().join(format!("{graph_sha}.onnx")).exists());
        assert!(!added.dir().join(format!("{sidecar_sha}.json")).exists());
        assert_eq!(removed.index_text(), registry.index_text());
    }

    #[test]
    fn add_refuses_bad_probes_and_sidecars() {
        let temp = tempfile::tempdir().unwrap();
        let registry = seeded(temp.path());
        let mut loud = new_model(b"g1", "tara");
        loud.probe = ProbeResult::new(0.0, 0.3, 0.0).unwrap();
        assert!(matches!(
            registry.added(loud),
            Err(RegistryError::ProbeTooHigh { probe: "noise", .. })
        ));
        let mut bad = new_model(b"g2", "tara");
        bad.sidecar = br#"{"type": "bcresnet", "sampleRate": 8000}"#.to_vec();
        assert!(matches!(
            registry.added(bad),
            Err(RegistryError::Sidecar(SidecarError::SampleRate(8000)))
        ));
        assert!(ModelThresholds::new(0.5, 0.6, 0.3).is_err());
        assert!(SpokenForm::new("Ophelia").is_err());
    }
}
