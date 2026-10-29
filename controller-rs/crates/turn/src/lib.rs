//! Speech policy for one turn: evidence assembly, cell attribution, the endpoint reducer that
//! decides when the user has finished speaking, the wake decision, and the wake-phrase text rules
//! (Rust controller evaluation §4.1, §4.5–§4.6, §5.3, §6.1; post-AFE architecture §8 and §16.6).
//!
//! Everything here is a pure, deterministic reducer the device runtime calls: no threads, no
//! I/O, no clock reads. Sample time ([`Sample`]) and the controller's monotonic clock
//! ([`MonoInstant`]) are separate types, and missing evidence stays unknown rather than being
//! read as silence.
//!
//! # Using it from the device runtime
//!
//! - **Wake.** On `wake.candidate`, build a [`WakeCandidate`] and call [`WakeDecider::arrive`].
//!   An idle candidate is decided at once. For a producing-sound one, pass the reference scorer's
//!   overlap, the candidate echo comparison and the verifier's transcript as they arrive, and
//!   call [`WakeDecider::on_timer`] at [`WakeDecider::next_deadline`].
//! - **Turn.** On acceptance (or a button press or a reply window), build a [`TurnSpec`] and a
//!   [`TurnPolicy`]. Push device cells, VAD, echo labels, gaps and ASR results into
//!   [`TurnPolicy::evidence_mut`] as they arrive, call [`TurnPolicy::advance`] after each batch,
//!   and act on every [`Decision`] it appends. Route control events (mute, session loss,
//!   overrun, worker errors, discontinuities) to their methods, sentence-matcher answers to
//!   [`TurnPolicy::recognizer_answer`], the pause server's requests and answers to
//!   [`TurnPolicy::pause_server_asked`] and [`TurnPolicy::pause_server_answer`], and call
//!   [`TurnPolicy::on_timer`] at [`TurnPolicy::next_deadline`].
//! - **Worker.** The speech worker uses [`PauseFinalizer`] to know which block carries the pause
//!   decode.
//! - **Text.** After HA's STT returns, remove the wake phrase with [`text::locate_in_streaming`]
//!   and [`text::strip_final`].
//!
//! Not here: the completeness grammar (an input, [`Completeness`]), the echo comparator and
//! reference wake scorer (their results are inputs), wake arbitration between devices, local
//! commands, the commit re-decode, the pause server's network requests, the reply chain
//! limits and VAD snapshot priming, and persistence of the traces.

pub mod asr;
pub mod classify;
pub mod completeness;
pub mod endpoint;
pub mod evidence;
pub mod level;
pub mod pause;
pub mod reply;
pub mod text;
pub mod time;
pub mod trace;
pub mod wake;

pub use asr::PauseFinalizer;
pub use classify::{AttributionPolicy, CellClass, ClassRule};
pub use completeness::{Completeness, NoGrammar, RecognizerAnswer, RecognizerError, TextClass};
pub use endpoint::{
    CloseCause, CloseReason, Commit, Decision, EndBound, EndpointPolicy, ReplyOrigin, ReplyReopen,
    ReplySpec, Route, TurnConfig, TurnKind, TurnPolicy, TurnSpec, WakeAnchor,
};
pub use evidence::{
    AsrKind, AsrObservation, CellAudio, CellEvidence, DeviceCell, DeviceInput, EchoCell, EchoLabel,
    EvidenceAssembler, EvidenceError, EvidencePlan, ReferenceCoverage, Released, Token, VadInput,
};
pub use pause::{
    PauseError, PauseRecord, PauseServerAnswer, PauseWords, ServerFailure, TextSource,
};
pub use reply::{EarlyAnswerPolicy, EarlyAnswerWatch};
pub use time::{CellIndex, Epoch, MonoInstant, Sample, SampleSpan, Samples};
pub use trace::{EndpointEvent, UtteranceTrace};
pub use wake::{
    ArrivalConditions, CandidateEcho, ReferenceScore, VerifierResult, WakeCandidate, WakeDecider,
    WakeDecision, WakeHop, WakeOutcome, WakePolicy, WakeProfile, WakeRefusal, WakeTrace,
};
