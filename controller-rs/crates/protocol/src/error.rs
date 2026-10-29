//! `protocol.error`: the codes the controller reports before it closes a
//! session that broke the wire contract.

use std::fmt;

use serde::Serialize;

use crate::envelope::ControlBody;
use crate::vocab::MessageType;

wire_enum! {
    /// A `protocol.error` code (spec §3.1.3).
    pub enum ProtocolErrorCode {
        /// The envelope's `protocol` is not 1, or a hello's `protocols` lacks 1.
        UnsupportedProtocol => "unsupported_protocol",
        /// A control frame that is binary, not JSON, or breaks a body rule.
        MalformedMessage => "malformed_message",
        /// An audio frame that breaks an EMA1 rule.
        MalformedFrame => "malformed_frame",
        /// A control frame over 256 KiB.
        Oversize => "oversize",
        /// An envelope whose `session_id` is not the live session's.
        WrongSession => "wrong_session",
        /// An envelope whose `device_id` is not the session's device.
        WrongDevice => "wrong_device",
        /// A message that is not allowed at this point of the session.
        UnexpectedMessage => "unexpected_message",
        /// A `stream.open` or `stream.end` naming no stream, or parameters
        /// that do not match its stream.
        BadStream => "bad_stream",
        /// A packet or `stream.end` for an epoch that was never opened.
        UnknownEpoch => "unknown_epoch",
        /// A packet for an epoch that `stream.end` already ended.
        StreamEnded => "stream_ended",
        /// A packet whose sequence does not increase within its epoch.
        Sequence => "sequence",
        /// A packet that goes back in time outside a granted backfill run.
        SampleOrder => "sample_order",
        /// An audio frame of a kind the device may not send.
        UnexpectedKind => "unexpected_kind",
    }
}

/// A violation of the device link contract: the controller answers it with
/// `protocol.error {code, detail}` and closes the session. It is also the
/// body of that message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    /// The machine-readable code.
    pub code: ProtocolErrorCode,
    /// A sentence for the log.
    pub detail: String,
}

impl ProtocolError {
    /// A violation with `code` and `detail`.
    pub fn new(code: ProtocolErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }

    pub(crate) fn malformed(detail: impl Into<String>) -> Self {
        Self::new(ProtocolErrorCode::MalformedMessage, detail)
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProtocolError {}

impl ControlBody for ProtocolError {
    const MESSAGE_TYPE: MessageType = MessageType::ProtocolError;
}
