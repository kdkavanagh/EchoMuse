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
pub mod evidence;
pub mod speech;
pub mod wire;

#[cfg(any(test, feature = "testing"))]
pub mod sim;
#[cfg(test)]
mod tests;

mod afe;
mod analysis;
mod config;
mod conversation;
mod io;
mod link;
mod machine;
mod output;
mod rows;
mod session;
mod wake;

pub use conversation::pause_server::ServerRequest;
pub use io::{
    AlertReply, AlertRequest, CallId, Correction, CorrectionRequest, DecodeCommand, DecodeReport,
    Effect, HaReply, HaRequest, Input, Now, Outbound, PersistReply, PersistRequest, PreviewId,
    PreviewOutcome, SatelliteCommand, VolumeSet, recognizer_sentences,
};
pub use machine::diagnostic::DiagnosticAudio;
pub use machine::{Voice, VoiceConfig, VoiceCounters, VoiceStatus, WakeModelInfo};
