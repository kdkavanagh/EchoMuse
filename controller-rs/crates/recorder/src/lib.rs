//! The opt-in diagnostic session recorder and the decision replay that feeds recorded sessions
//! back through the speech policy under a controlled clock (Rust controller evaluation §6.2,
//! with §5.3 for the clocks and §6.1 for evidence ordering).
//!
//! The recorder is development and diagnostic tooling and is off by default: nothing is
//! recorded until an administrator grants one device a bounded interval, and nothing about it
//! changes the ordinary `saveUtterances` / `saveWakeClips` clips, which have their own settings
//! and files.
//!
//! # Consent and privacy
//!
//! - **No grant, no recording.** A [`SessionRecorder`] can only be started with a
//!   [`GrantPass`], which only [`GrantBook::pass`] makes and only while the device's grant is
//!   live. Every recorder call checks the grant again; at its end, or when an administrator
//!   withdraws it, the recording ends. [`GrantStatus`] is what the dashboard shows.
//! - **Scope.** A grant names what may be kept ([`StreamScope`]): mic audio, reference audio,
//!   loudness cells, AFE records, transcripts. Frames of other streams are never queued; text
//!   outside the scope becomes [`Transcript::Withheld`]; HA bodies become [`HaBody::Withheld`].
//!   Decision evidence that is neither audio nor text (VAD probabilities, echo labels, wake and
//!   endpoint decisions, timings, identities) is kept under every grant.
//! - **Mute and capture.** While the device is muted or capture is not permitted, no frame is
//!   taken; the count is recorded as [`Event::AudioWithheld`]. When the device mutes, every
//!   frame not yet written is discarded, in the queue and in the writer's hands, and the mute
//!   is recorded ([`Event::Privacy`]). Frames that carry the EMA1 muted flag are never kept.
//! - **Withdrawal** discards everything not yet written, from whichever thread withdraws, and
//!   ends the recording. What was already written stays until its retention ends or it is
//!   deleted.
//! - **Credentials.** HA bodies are redacted inside the recorder before they are queued
//!   ([`redact()`]): tokens, cookies, passwords, secrets, `Authorization` values, JSON Web Tokens
//!   and signed-URL parameters. Only the HA operations of the controller's `HomeAssistant`
//!   service can be recorded ([`HaOperation`]): there is no way to record shell traffic or
//!   other HTTP, and [`SessionRecorder::frame`] takes only valid EMA1 uplink frames.
//! - **Files** are written under one configured directory ([`RecorderConfig`]) with mode
//!   `0600` in `0700` directories, carry their retention in their header, and are deleted by
//!   [`RecordingStore::sweep`], [`RecordingStore::delete`] and
//!   [`RecordingStore::delete_device`]; deletion stops a recording still being written first.
//! - **Honest completeness.** Lost, discarded, withheld and redacted entries are counted and
//!   marked in the file ([`Integrity`]). A replay of a recording that is incomplete or redacted
//!   where decision replay reads it reports [`Equivalence::Unproven`], never `Proven`.
//!
//! # Using it from the device runtime
//!
//! - **Dashboard.** [`GrantBook::grant`], [`GrantBook::withdraw`] and [`GrantBook::status`];
//!   call [`GrantBook::expire`] from any periodic task; [`GrantBook::forget`] and
//!   [`RecordingStore::delete_device`] when a device is deleted.
//! - **Attach.** After installing the hello's privacy state, if [`GrantBook::pass`] gives a
//!   pass, [`SessionRecorder::start`] with the session's identities and revisions. Also call it
//!   when a grant is given to a device already attached.
//! - **Ingress, in order.** [`SessionRecorder::frame`] for every accepted EMA1 uplink frame
//!   with its ingress ordinal; [`SessionRecorder::stream_opened`] / `stream_ended` for
//!   `stream.open` / `stream.end`; [`SessionRecorder::privacy`] on `privacy.changed`, before any
//!   audio queued behind it, and whenever the runtime stops or resumes permitting capture.
//! - **Wake.** After each [`WakeDecider`](echomuse_turn::WakeDecider) call, the matching
//!   `wake_*` method with the decision it returned.
//! - **Turns.** After [`TurnPolicy::new`](echomuse_turn::TurnPolicy::new),
//!   [`SessionRecorder::turn_opened`]; after each evidence push, the matching `turn_*` method
//!   with the same values; run `advance` with a [`GrammarTap`] and then call
//!   [`SessionRecorder::turn_advanced`]; `turn_recognizer_answer`,
//!   `turn_pause_server_asked`, `turn_pause_server_answer`, `turn_timer` and `turn_control`
//!   after those calls.
//! - **Echo.** After each [`EchoAnalyzer`](echomuse_echo::EchoAnalyzer) input,
//!   [`SessionRecorder::echo_input`]; after `poll` and `compare_candidate`,
//!   [`SessionRecorder::echo_polled`] / [`SessionRecorder::echo_compared`].
//! - **HA.** [`SessionRecorder::ha_request`] as a request is sent and
//!   [`SessionRecorder::ha_reply`] when it is answered or fails.
//! - **Lifecycle.** [`SessionRecorder::cancelled`] when the runtime cancels a turn, candidate,
//!   exchange or playback; [`SessionRecorder::poll`] at [`SessionRecorder::next_deadline`];
//!   [`SessionRecorder::disconnected`] when the session ends, or
//!   [`SessionRecorder::finish`].
//!
//! Every call takes the controller's monotonic `now` and returns at once. Entries go to the
//! recording's writer thread through a bounded queue ([`QueueBounds`]); when it is full the
//! entry is dropped, counted and marked, and the runtime is never made to wait.
//!
//! # File format, version 1
//!
//! A session file is a 16-byte preamble followed by records.
//!
//! - **Preamble:** the magic `45 4D 52 45 43 0D 0A 1A` (`EMREC\r\n^Z`), the format version as
//!   a little-endian `u16` (1), and six zero bytes. A reader refuses any other version.
//! - **Record:** a little-endian `u32` body length, the body, and the CRC-32 (IEEE) of the
//!   body as a little-endian `u32`. The body is one type byte and the payload. A record cut
//!   short marks the file unended; a bad length, type or checksum marks it corrupt, and
//!   nothing after it is read.
//!
//! | Type | Payload | Where |
//! |---|---|---|
//! | 1 header | JSON [`Header`] | first, exactly once |
//! | 2 event | JSON [`Entry`]: `seq`, `t`, [`Event`] | any number |
//! | 3 frame | `seq`, `t`, ingress ordinal as little-endian `u64`s, then the EMA1 frame exactly as received (64-byte header and payload) | any number |
//! | 4 marker | JSON [`Marker`]: entries lost or discarded since the last marker | any number |
//! | 5 end | JSON [`End`]: time, [`EndReason`], totals, last sequence number | last, only when the recording ended in order |
//!
//! Every JSON value is self-describing: enums carry their variant names as tags, and every
//! field is named. Times `t` are nanoseconds of the controller's monotonic clock since the
//! recording started ([`RelTime`]); the header's `anchor` is the only wall-clock value in the
//! file. Sample positions are on their stream epoch's timeline and are never compared across
//! epochs. `seq` is the runtime's order of recording; a gap is an entry lost or discarded.
//!
//! A file is written as `<recording id>.emrec.part` and renamed to `<recording id>.emrec`
//! after its end record is on disk; see [`store`] for the directory layout.
//!
//! # Decision replay
//!
//! [`replay()`] drives fresh wake deciders and turn policies (and, with
//! [`EchoSource::Recompute`], echo analyzers over the recorded frames) through the recorded
//! inputs in order, firing their deadlines on the recording's clock, and compares the
//! externally meaningful outcomes ([`ReplayReport`]). [`ReplayOptions`] changes the policy
//! configuration, the wake policy, the echo source or the grammar.
//!
//! # Not here
//!
//! End-to-end replay through the real models (VAD, ASR, verifier) and simulated HA and device
//! services; the text actually dispatched to HA after STT and wake-phrase removal, local
//! commands and spoken responses (end-to-end replay outcomes); deliberate export of a
//! recording as a shareable fixture; the dashboard API routes that call the grant book and the
//! store. The voice machine's calls are its `evidence` module (`echomuse_voice::evidence`):
//! the device runtime hands each piece to [`SessionRecorder`] through it, and
//! [`SessionRecorder::turn_advanced_with`] takes a policy's recognizer answers without the
//! policy. Echo analyzer inputs (`echo_input`) are not recorded, since the analyzers run on
//! the speech workers: [`EchoSource::Recompute`] has no recorded inputs to run from.

pub mod format;
pub mod grant;
pub mod queue;
pub mod reader;
pub mod record;
pub mod redact;
pub mod replay;
pub mod store;
pub mod writer;

#[cfg(test)]
mod tests;

pub use format::{FORMAT_VERSION, MAGIC};
pub use grant::{
    AdministratorId, GrantBook, GrantError, GrantId, GrantPass, GrantRequest, GrantStatus,
    MAX_GRANT_INTERVAL, StreamScope, UnixMillis,
};
pub use queue::QueueBounds;
pub use reader::{Integrity, Limitation, ReadError, RecordedItem, Recording, Tail, read_header};
pub use record::{
    AnalyzerId, CancelTarget, DisconnectReason, EchoStep, End, EndReason, Entry, Event, ExchangeId,
    FrameEntry, GrammarAnswer, HaBody, HaEntry, HaOperation, Header, IngressOrdinal, Marker,
    ModelRevision, ModelRole, PrivacyState, RecordingId, RelTime, Revisions, Sha256Hex, Tally,
    Transcript, TurnCause, TurnControl,
};
pub use redact::redact;
pub use replay::{
    CommittedSpan, Divergence, EchoComparison, EchoSource, Equivalence, GrammarSource, Mismatch,
    ReplayOptions, ReplayReport, Terminal, TurnComparison, TurnOutcome, WakeComparison, replay,
};
pub use store::{RecorderConfig, RecordingStore, StoredRecording};
pub use writer::{
    Finishing, FrameTaken, GrammarTap, RecorderState, SessionIdentity, SessionRecorder, StartError,
};
