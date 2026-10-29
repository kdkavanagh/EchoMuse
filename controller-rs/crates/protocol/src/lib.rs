//! Device link protocol v1: control envelopes, EMA1 audio frames, stream epoch
//! validation and the canonical JSON used by alert snapshots.
//!
//! The wire contract comes from `docs/protocol-v1.md`, §3.1 of
//! `docs/rust-controller-evaluation.md` and the Go firmware under `device/`,
//! which is authoritative for every byte on the link. This crate holds no
//! sockets, clocks or tasks: it parses and validates what a device sends and
//! encodes what the controller sends, and the application decides what to do.
//!
//! - [`envelope`]: one control text frame in either direction, the session and
//!   handshake checks, and the [`ControlBody`] trait every controller message
//!   implements.
//! - [`session`]: the handshake and lifecycle bodies, from `session.hello` to
//!   `uplink.ended`, and the admission check.
//! - [`ema`]: the 64-byte EMA1 audio frame header and its payload records.
//! - [`streams`]: the stream epochs a device announces and the per-epoch
//!   ordering rules every uplink packet must follow.
//! - [`canonical`]: the canonical JSON form whose SHA-256 the device checks.
//! - [`alerts`]: alert cache objects and `alert.snapshot` pages.

#![warn(missing_docs)]

#[macro_use]
mod macros;

pub mod alerts;
pub mod canonical;
pub mod ema;
pub mod envelope;
pub mod error;
mod json;
pub mod session;
pub mod streams;
pub mod vocab;

pub use envelope::{
    ControlBody, ControlFrame, DeviceEnvelope, DeviceMessage, EncodeError, EncodedEnvelope,
    EnvelopeHeader, HelloRefused, HelloRequest, Inbound, LinkSessionId, MAX_CONTROL_BYTES,
    MessageId, PROTOCOL_VERSION, UnknownEnvelope, check_live_session, encode_envelope,
    parse_envelope, parse_hello,
};
pub use error::{ProtocolError, ProtocolErrorCode};
pub use vocab::{
    Capabilities, Capability, MessageType, Platform, REQUIRED_CAPABILITIES, RejectReason, Reported,
    Sha256, StreamId,
};
