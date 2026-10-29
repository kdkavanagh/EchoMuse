//! Speech work: the models a controller loads once, and the state each of a device's four
//! speech workers keeps between jobs (spec §5.1).
//!
//! The device runtime never calls a model. It sends each worker typed jobs in order and
//! gets typed results back; the worker owns its model sessions and stream state:
//!
//! | Worker | State | Jobs | Results |
//! |---|---|---|---|
//! | microphone analysis | [`mic::MicAnalysis`]: per analysed subscription, Silero VAD, the Kroko utterance stream, the pause finalizer and the echo analyzer | [`MicJob`] | [`MicOutput`] |
//! | reference scoring | [`reference::ReferenceScoring`]: per subscription, the reference wake scorer on the device's wake graph | [`ReferenceJob`] | [`ReferenceOutput`] |
//! | wake verification | [`verify::WakeVerification`]: a Kroko decode of one verification window | [`VerifyJob`] | [`VerifyOutput`] |
//! | span decoding | [`span::SpanDecoding`]: the commit re-decode and the STT copy (gain, DTLN, trailing silence) | [`SpanJob`] | [`SpanOutput`] |
//!
//! Every job names the stream it concerns ([`StreamKey`]: the runtime session, the
//! subscription and its capture epoch), so a result can be matched to what asked for it
//! and a result for a stream that has since ended is recognised as stale (§5.7).
//!
//! Models come from a [`models::ModelSet`]: the pinned speech bundle in production
//! ([`models::BundleModels`]), stand-ins in tests (`testing`, with the `testing` feature).
//!
//! Audio conventions (docs/post-afe-audio-architecture.md "Evidence copy and VAD input"):
//! the uploaded canonical PCM is never changed; Kroko reads the **evidence copy**,
//! canonical × 10 (+20 dB) saturated to int16, as f32 in [-1, 1]; Silero reads its own
//! **VAD input**, canonical × 10^1.5 (+30 dB) saturated the same way. Every fresh decode
//! (pause finalize, wake verification, commit re-decode) is followed by 1.5 s of zeros.

use std::sync::Arc;

use bytes::Bytes;
use echomuse_core::types::SessionId;
use echomuse_echo::{Alignment, CandidateComparison, EchoPlan, OutputCoverage, ReferenceJudgement};
use echomuse_speech::{AsrResult, BcresnetModelFiles};
use echomuse_turn::{
    AsrObservation, CellIndex, DeviceInput, EchoCell, Epoch, Sample, SampleSpan, VadInput,
    VerifierResult,
};
use echomuse_uplink::SubscriptionId;

pub mod mic;
pub mod models;
pub mod reference;
pub mod span;
pub mod verify;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

/// One analysed subscription stream: the session it belongs to, the subscription, and
/// the capture epoch its positions are on. Positions of different epochs never mix.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct StreamKey {
    pub session: SessionId,
    pub subscription: SubscriptionId,
    pub epoch: Epoch,
}

/// Little-endian PCM16 as uploaded: a slice of the received frame, never copied on the
/// way to a worker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pcm16Le(Bytes);

impl Pcm16Le {
    /// `bytes` holds whole samples; a trailing odd byte is ignored.
    pub fn new(bytes: Bytes) -> Self {
        Self(bytes)
    }

    pub fn len(&self) -> usize {
        self.0.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn samples(&self) -> impl ExactSizeIterator<Item = i16> + '_ {
        self.0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
    }

    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }
}

/// A job for the microphone analysis worker, in the order the runtime received the
/// audio. Jobs for one stream are applied in submission order.
#[derive(Clone, Debug)]
pub enum MicJob {
    /// Opens analysis of a subscription's capture epoch. VAD runs on every mic cell from
    /// `plan.vad_from` (its state starts fresh at the subscription's first mic sample);
    /// the echo analyzer is opened with `plan`. Opening a key that is open resets it.
    /// Opening a key of another session forgets every stream of older sessions.
    Open { key: StreamKey, plan: EchoPlan },
    /// Starts an utterance at `from` (a cell start at or after `plan.vad_from`): a fresh
    /// Kroko stream fed with the evidence copy from `from` (mic already received from
    /// `from` on is fed at once), echo labels from `from`, and a fresh pause finalizer.
    /// Replaces an utterance already under way on the stream.
    StartUtterance { key: StreamKey, from: Sample },
    /// Ends the stream's utterance: no further ASR observations or echo labels. VAD and
    /// the stream stay open (a reply window can start another utterance later).
    EndUtterance { key: StreamKey },
    /// Mic audio, `first` on the capture timeline. `discontinuity`: the device flagged a
    /// discontinuity before it, so VAD state and any utterance's ASR restart.
    Mic {
        key: StreamKey,
        first: Sample,
        mono_ns: u64,
        discontinuity: bool,
        pcm: Pcm16Le,
    },
    /// Mic samples that will never arrive. Their cells are gaps; an utterance under way
    /// ends its ASR there (the turn's evidence closes ASR at a gap too).
    MicMissing { key: StreamKey, span: SampleSpan },
    /// Reference audio on `reference_epoch` (reference sample positions).
    Reference {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        mono_ns: u64,
        coverage: OutputCoverage,
        pcm: Pcm16Le,
    },
    /// A digital-silence reference packet (no payload).
    ReferenceSilence {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        frames: u64,
        mono_ns: u64,
        coverage: OutputCoverage,
    },
    /// Reference samples `[first, end)` that will never arrive.
    ReferenceMissing {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        end: u64,
    },
    /// The subscription's reference ended (or it never had one): cells still waiting on
    /// reference become unknown.
    ReferenceEnded { key: StreamKey },
    /// The device's loudness cells (the cells stream), for the echo analyzer's
    /// background level.
    DeviceCells {
        key: StreamKey,
        first: CellIndex,
        cells: Vec<DeviceInput>,
    },
    /// Compares a wake candidate's support with the reference. Answered in the first
    /// output in which every input it reads is present (or known never to arrive), not
    /// necessarily this job's.
    CompareCandidate {
        key: StreamKey,
        candidate: String,
        support: SampleSpan,
    },
    /// Forgets the stream and everything retained for it.
    Close { key: StreamKey },
    /// Forgets every stream (mute, session loss): no audio is retained afterwards.
    Reset,
}

/// What one mic job produced. Every vector is in increasing position order and
/// continues where the stream's previous output stopped.
#[derive(Clone, Debug)]
pub struct MicOutput {
    /// `None` for [`MicJob::Reset`].
    pub key: Option<StreamKey>,
    /// VAD of newly analysed cells, from the cell given.
    pub vad: Option<(CellIndex, Vec<VadInput>)>,
    /// Echo labels that became ready, from the cell given.
    pub echo: Option<(CellIndex, Vec<EchoCell>)>,
    /// One observation per completed 80 ms block of the utterance (a finalized decode at
    /// a pause, live otherwise).
    pub asr: Vec<AsrObservation>,
    /// Answers to [`MicJob::CompareCandidate`] that became available.
    pub comparisons: Vec<(String, CandidateComparison)>,
    /// The reference placement on the mic timeline, once both clocks are fitted;
    /// repeated whenever it changes.
    pub alignment: Option<Alignment>,
    /// A model call failed: the stream's evidence from here is unknown.
    pub failure: Option<String>,
}

/// A job for the reference scoring worker.
#[derive(Clone, Debug)]
pub enum ReferenceJob {
    /// Opens a subscription's reference scoring with the device's wake graph; `None`
    /// when the selected graph's files cannot be used (every judgement is then
    /// unavailable). Opening a key of another session forgets older sessions.
    Open {
        key: StreamKey,
        graph: Option<BcresnetModelFiles>,
    },
    Pcm {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        pcm: Pcm16Le,
    },
    Silence {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        frames: u64,
    },
    Missing {
        key: StreamKey,
        reference_epoch: Epoch,
        first: u64,
        end: u64,
    },
    Ended {
        key: StreamKey,
    },
    /// Judges a mic candidate's support against the reference candidates. Answered once
    /// the windows it needs are scored (or the reference ended).
    Judge {
        key: StreamKey,
        candidate: String,
        support: SampleSpan,
        alignment: Alignment,
    },
    Close {
        key: StreamKey,
    },
    Reset,
}

#[derive(Clone, Debug)]
pub struct ReferenceOutput {
    pub key: Option<StreamKey>,
    pub judgements: Vec<(String, ReferenceJudgement)>,
    pub failure: Option<String>,
}

/// Decodes one wake candidate's verification window.
#[derive(Clone, Debug)]
pub struct VerifyJob {
    pub candidate: String,
    /// Canonical PCM of the verification window.
    pub pcm: Arc<[i16]>,
}

#[derive(Clone, Debug)]
pub struct VerifyOutput {
    pub candidate: String,
    pub result: VerifierResult,
}

/// How the STT copy's gain is chosen (spec §4.7).
#[derive(Clone, Debug)]
pub enum SttGain {
    /// From the wake word's level: canonical PCM of the wake support.
    FromWake(Arc<[i16]>),
    /// No wake word to measure (button and reply turns): the nominal +20 dB.
    Nominal,
}

/// A job for the span decoding worker.
#[derive(Clone, Debug)]
pub enum SpanJob {
    /// A fresh decode of a committed span (evidence copy + flush), for the commit
    /// re-check. Token emission times are relative to the span's start.
    Redecode { turn: uuid::Uuid, pcm: Arc<[i16]> },
    /// The STT copy of a committed span: gain, DTLN when `denoise`, then 0.5 s of zeros.
    SttCopy {
        turn: uuid::Uuid,
        pcm: Arc<[i16]>,
        gain: SttGain,
        denoise: bool,
    },
}

#[derive(Clone, Debug)]
pub enum SpanOutput {
    Redecode {
        turn: uuid::Uuid,
        result: Result<AsrResult, String>,
    },
    SttCopy {
        turn: uuid::Uuid,
        result: Result<SttCopy, String>,
    },
}

/// The audio Home Assistant's STT receives.
#[derive(Clone, Debug)]
pub struct SttCopy {
    pub pcm: Arc<[i16]>,
    pub gain_db: f64,
    /// DTLN was applied.
    pub denoised: bool,
}
