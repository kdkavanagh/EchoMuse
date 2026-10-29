//! The per-device voice orchestrator: wake candidates, turns, replies and their effects,
//! as an owned state machine the device runtime drives (spec §4, §5.1).
//!
//! [`Voice`] is one device session's voice path. The device runtime owns it on the
//! device's thread, hands it every relevant input in the order it happened ([`Input`]:
//! ordered ingress, urgent privacy and session loss, speech-worker results, service
//! answers, timers) and carries out what it asks ([`Effect`]: messages to the device,
//! speech-worker jobs, Home Assistant requests, decodes, alert operations, records,
//! satellite updates). The machine performs no I/O and reads no clock, so its rules are
//! tested with synthetic inputs.
//!
//! The speech workers' own state lives in [`speech`]; the cross-device wake arbiter in
//! [`arbiter`].

pub mod arbiter;
pub mod config;
pub mod evidence;
pub mod speech;
pub mod wire;

#[cfg(any(test, feature = "testing"))]
pub mod sim;
#[cfg(test)]
mod tests;

mod afe;
mod analysis;
mod button;
mod candidate;
mod diagnostic;
mod early;
mod io;
mod linger;
mod link;
mod local;
mod machine;
mod output;
mod pause_server;
mod record;
mod reply;
mod turn;

pub use diagnostic::DiagnosticAudio;
pub use io::{
    AlertReply, AlertRequest, CallId, Correction, CorrectionRequest, DecodeCommand, DecodeReport,
    Effect, HaReply, HaRequest, Input, Now, Outbound, PersistReply, PersistRequest, PreviewId,
    PreviewOutcome, RECOGNIZER_PROBE, SatelliteCommand, VolumeSet, recognizer_sentences,
};
pub use machine::{Voice, VoiceConfig, VoiceCounters, VoiceStatus, WakeModelInfo};
pub use pause_server::{PAUSE_SERVER_LIMIT, SPAN_SERVER_LIMIT, ServerRequest};
pub use record::POLICY;
