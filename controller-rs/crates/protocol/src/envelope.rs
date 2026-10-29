//! Control envelopes (docs/protocol-v1.md §2, spec §3.1.3): one JSON text
//! frame per message in either direction.
//!
//! [`parse_envelope`] validates one device frame and decodes its body;
//! [`parse_hello`] applies the extra rules of a session's first frame and
//! [`check_live_session`] those of every later one. [`encode_envelope`] wraps
//! a controller body in an envelope with a fresh UUIDv4 `message_id`.

use std::fmt;

use echomuse_core::types::DeviceId;
use serde::Serialize;
use serde_json::Value;

use crate::error::{ProtocolError, ProtocolErrorCode};
use crate::json::{Fields, Object};
use crate::session::{ClockRequest, CommandAck, Heartbeat, ProtocolErrorReport, SessionHello};
use crate::streams::{StreamEnd, StreamOpen, UplinkEnded};
use crate::vocab::MessageType;

/// The only protocol revision.
pub const PROTOCOL_VERSION: u64 = 1;

/// The largest control frame in bytes, inclusive.
pub const MAX_CONTROL_BYTES: usize = 256 * 1024;

/// A `session_id`: the UUIDv4 text the controller chose in `session.ready`.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LinkSessionId(String);

impl LinkSessionId {
    /// A fresh UUIDv4 session id.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as it travels.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for LinkSessionId {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl fmt::Display for LinkSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for LinkSessionId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// A `message_id`: the sender's UUIDv4 for one message, which `command.ack`
/// echoes.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MessageId(String);

impl MessageId {
    /// A fresh UUIDv4 message id.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as it travels.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for MessageId {
    fn from(text: String) -> Self {
        Self(text)
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for MessageId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// One frame received on the control socket.
#[derive(Clone, Copy, Debug)]
pub enum ControlFrame<'a> {
    /// A text frame.
    Text(&'a str),
    /// A binary frame, which is always a protocol error.
    Binary(&'a [u8]),
}

/// The envelope fields every message carries besides its type and body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvelopeHeader {
    /// The session, `None` only on `session.hello`.
    pub session_id: Option<LinkSessionId>,
    /// The sender's id for this message.
    pub message_id: MessageId,
    /// The device (its `ro.serialno`).
    pub device_id: DeviceId,
    /// The owner generation of the object the message acts on; 0 when none.
    pub generation: u32,
}

/// A validated device envelope.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    /// A type the device sends, with its body decoded.
    Known(DeviceEnvelope),
    /// A type this controller does not know. It counts as a sign the link is
    /// alive and is otherwise ignored.
    Unknown(UnknownEnvelope),
}

impl Inbound {
    /// The envelope fields.
    pub fn header(&self) -> &EnvelopeHeader {
        match self {
            Inbound::Known(env) => &env.header,
            Inbound::Unknown(env) => &env.header,
        }
    }
}

/// A device envelope of a known type.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceEnvelope {
    /// The envelope fields.
    pub header: EnvelopeHeader,
    /// The decoded body.
    pub message: DeviceMessage,
}

/// A device envelope of a type this controller does not know.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownEnvelope {
    /// The `type` as sent.
    pub message_type: String,
    /// The envelope fields.
    pub header: EnvelopeHeader,
}

/// A device message body.
#[derive(Clone, Debug, PartialEq)]
pub enum DeviceMessage {
    /// `session.hello`.
    SessionHello(Box<SessionHello>),
    /// `heartbeat`.
    Heartbeat(Heartbeat),
    /// `clock.request`.
    ClockRequest(ClockRequest),
    /// `command.ack`.
    CommandAck(CommandAck),
    /// `protocol.error`.
    ProtocolError(ProtocolErrorReport),
    /// `stream.open`.
    StreamOpen(StreamOpen),
    /// `stream.end`.
    StreamEnd(StreamEnd),
    /// `uplink.ended`.
    UplinkEnded(UplinkEnded),
    /// Any other device message type. This crate does not decode its body
    /// yet; the module that owns the message parses `body`.
    Other {
        /// The message type, always one the device sends.
        message_type: MessageType,
        /// The body object as received.
        body: Object,
    },
}

impl DeviceMessage {
    /// The message's type.
    pub fn message_type(&self) -> MessageType {
        match self {
            DeviceMessage::SessionHello(_) => MessageType::SessionHello,
            DeviceMessage::Heartbeat(_) => MessageType::Heartbeat,
            DeviceMessage::ClockRequest(_) => MessageType::ClockRequest,
            DeviceMessage::CommandAck(_) => MessageType::CommandAck,
            DeviceMessage::ProtocolError(_) => MessageType::ProtocolError,
            DeviceMessage::StreamOpen(_) => MessageType::StreamOpen,
            DeviceMessage::StreamEnd(_) => MessageType::StreamEnd,
            DeviceMessage::UplinkEnded(_) => MessageType::UplinkEnded,
            DeviceMessage::Other { message_type, .. } => *message_type,
        }
    }

    fn parse(message_type: MessageType, body: Object) -> Result<Self, ProtocolError> {
        Ok(match message_type {
            MessageType::SessionHello => {
                DeviceMessage::SessionHello(Box::new(SessionHello::parse(&body)?))
            }
            MessageType::Heartbeat => DeviceMessage::Heartbeat(Heartbeat::parse(&body)?),
            MessageType::ClockRequest => DeviceMessage::ClockRequest(ClockRequest::parse(&body)?),
            MessageType::CommandAck => DeviceMessage::CommandAck(CommandAck::parse(&body)?),
            MessageType::ProtocolError => {
                DeviceMessage::ProtocolError(ProtocolErrorReport::parse(&body)?)
            }
            MessageType::StreamOpen => DeviceMessage::StreamOpen(StreamOpen::parse(&body)?),
            MessageType::StreamEnd => DeviceMessage::StreamEnd(StreamEnd::parse(&body)?),
            MessageType::UplinkEnded => DeviceMessage::UplinkEnded(UplinkEnded::parse(&body)?),
            other => DeviceMessage::Other {
                message_type: other,
                body,
            },
        })
    }
}

/// An envelope whose fields are valid, before its body is decoded.
struct Unpacked {
    message_type: String,
    header: EnvelopeHeader,
    body: Object,
}

/// Validates the envelope of one frame. On failure the device id is
/// returned too whenever the frame named one, so a refused hello can still
/// be recorded against its device.
fn unpack(frame: ControlFrame<'_>) -> Result<Unpacked, (Option<DeviceId>, ProtocolError)> {
    let text = match frame {
        ControlFrame::Text(text) => text,
        ControlFrame::Binary(_) => {
            return Err((None, ProtocolError::malformed("control frames are text")));
        }
    };
    if text.len() > MAX_CONTROL_BYTES {
        let detail = format!(
            "control frame of {} bytes exceeds {MAX_CONTROL_BYTES}",
            text.len()
        );
        return Err((
            None,
            ProtocolError::new(ProtocolErrorCode::Oversize, detail),
        ));
    }
    let mut object = match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(object)) => object,
        Ok(_) => return Err((None, ProtocolError::malformed("envelope is not an object"))),
        Err(err) => {
            return Err((
                None,
                ProtocolError::malformed(format!("invalid JSON: {err}")),
            ));
        }
    };
    let device_id = object
        .get("device_id")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_owned);
    let known_device = device_id.clone().map(DeviceId);
    let fail = |error: ProtocolError| (known_device.clone(), error);

    match object.get("protocol") {
        Some(protocol) if protocol.as_u64() == Some(PROTOCOL_VERSION) => {}
        other => {
            let detail = format!(
                "protocol {}",
                other.map_or_else(|| "absent".to_owned(), Value::to_string)
            );
            return Err(fail(ProtocolError::new(
                ProtocolErrorCode::UnsupportedProtocol,
                detail,
            )));
        }
    }
    let fields = Fields::new(&object, "envelope");
    let message_type = fields.nonempty_string("type").map_err(fail)?.to_owned();
    let message_id = fields
        .nonempty_string("message_id")
        .map_err(fail)?
        .to_owned();
    let Some(device_id) = device_id else {
        return Err(fail(
            fields.error("device_id", "must be a non-empty string"),
        ));
    };
    let session_id = match fields.raw("session_id") {
        Some(Value::Null) => None,
        Some(Value::String(id)) => Some(LinkSessionId(id.clone())),
        _ => return Err(fail(fields.error("session_id", "must be a string or null"))),
    };
    let generation = fields
        .raw("generation")
        .and_then(Value::as_u64)
        .and_then(|g| u32::try_from(g).ok())
        .ok_or_else(|| fail(fields.error("generation", "must be a uint32")))?;
    let body = match object.remove("body") {
        Some(Value::Object(body)) => body,
        _ => {
            return Err(fail(ProtocolError::malformed(
                "envelope.body must be an object",
            )));
        }
    };
    let header = EnvelopeHeader {
        session_id,
        message_id: MessageId(message_id),
        device_id: DeviceId(device_id),
        generation,
    };
    Ok(Unpacked {
        message_type,
        header,
        body,
    })
}

fn decode(unpacked: Unpacked) -> Result<Inbound, (Option<DeviceId>, ProtocolError)> {
    let Unpacked {
        message_type,
        header,
        body,
    } = unpacked;
    let Some(known) = MessageType::from_wire(&message_type) else {
        return Ok(Inbound::Unknown(UnknownEnvelope {
            message_type,
            header,
        }));
    };
    if !known.sent_by_device() {
        let error = ProtocolError::new(
            ProtocolErrorCode::UnexpectedMessage,
            format!("{known} is not a device message"),
        );
        return Err((Some(header.device_id), error));
    }
    match DeviceMessage::parse(known, body) {
        Ok(message) => Ok(Inbound::Known(DeviceEnvelope { header, message })),
        Err(error) => Err((Some(header.device_id), error)),
    }
}

/// Validates one device control frame and decodes its body.
///
/// The frame must be text of at most [`MAX_CONTROL_BYTES`] bytes holding a
/// JSON object with `protocol` 1, non-empty string `type`, `message_id` and
/// `device_id`, a `session_id` string or null, a uint32 `generation` and an
/// object `body`. A type the controller does not know is a well-formed
/// [`Inbound::Unknown`]; a known type the device never sends is
/// `unexpected_message`. The caller checks the session and device with
/// [`parse_hello`] or [`check_live_session`].
pub fn parse_envelope(frame: ControlFrame<'_>) -> Result<Inbound, ProtocolError> {
    unpack(frame).and_then(decode).map_err(|(_, error)| error)
}

/// A session's first frame, accepted as a hello.
#[derive(Clone, Debug, PartialEq)]
pub struct HelloRequest {
    /// The envelope fields; `session_id` is `None`.
    pub header: EnvelopeHeader,
    /// The decoded hello.
    pub hello: SessionHello,
}

/// A first frame that is not an acceptable hello. The controller answers it
/// with `session.rejected` reason `protocol` (spec §3.1.4) and closes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelloRefused {
    /// The device the frame named, when it named one.
    pub device_id: Option<DeviceId>,
    /// What was wrong, for the log.
    pub error: ProtocolError,
}

/// Validates a session's first control frame: a valid envelope of type
/// `session.hello` (else `unexpected_message`), with a null `session_id`
/// (else `wrong_session`) and generation 0, since a hello acts on no
/// generation (else `malformed_message`).
pub fn parse_hello(frame: ControlFrame<'_>) -> Result<HelloRequest, HelloRefused> {
    let refused = |(device_id, error)| HelloRefused { device_id, error };
    let inbound = unpack(frame).and_then(decode).map_err(refused)?;
    let device_id = Some(inbound.header().device_id.clone());
    let refuse = |code, detail: String| HelloRefused {
        device_id: device_id.clone(),
        error: ProtocolError::new(code, detail),
    };
    let (header, hello) = match inbound {
        Inbound::Known(DeviceEnvelope {
            header,
            message: DeviceMessage::SessionHello(hello),
        }) => (header, hello),
        Inbound::Known(env) => {
            let detail = format!("first message is {}", env.message.message_type());
            return Err(refuse(ProtocolErrorCode::UnexpectedMessage, detail));
        }
        Inbound::Unknown(env) => {
            let detail = format!("first message is {}", env.message_type);
            return Err(refuse(ProtocolErrorCode::UnexpectedMessage, detail));
        }
    };
    if let Some(session) = &header.session_id {
        return Err(refuse(
            ProtocolErrorCode::WrongSession,
            format!("session.hello carries session_id {session}"),
        ));
    }
    if header.generation != 0 {
        let detail = format!("session.hello generation {} must be 0", header.generation);
        return Err(refuse(ProtocolErrorCode::MalformedMessage, detail));
    }
    Ok(HelloRequest {
        header,
        hello: *hello,
    })
}

/// Checks a frame received on a live session: its `session_id` must be the
/// session's (`wrong_session`), its `device_id` the session's device
/// (`wrong_device`), and it may not be another hello (`unexpected_message`).
/// The rules apply to unknown types too.
pub fn check_live_session(
    inbound: &Inbound,
    session: &LinkSessionId,
    device: &DeviceId,
) -> Result<(), ProtocolError> {
    let header = inbound.header();
    if header.session_id.as_ref() != Some(session) {
        let sent = header
            .session_id
            .as_ref()
            .map_or("null", LinkSessionId::as_str);
        return Err(ProtocolError::new(
            ProtocolErrorCode::WrongSession,
            format!("session_id {sent}"),
        ));
    }
    if header.device_id != *device {
        return Err(ProtocolError::new(
            ProtocolErrorCode::WrongDevice,
            format!("device_id {}", header.device_id),
        ));
    }
    if let Inbound::Known(DeviceEnvelope {
        message: DeviceMessage::SessionHello(_),
        ..
    }) = inbound
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::UnexpectedMessage,
            "session.hello on a live session",
        ));
    }
    Ok(())
}

/// A controller message body. Implementations must serialize to a JSON
/// object and name a type the controller sends.
pub trait ControlBody: Serialize {
    /// The envelope `type`.
    const MESSAGE_TYPE: MessageType;
}

/// One encoded controller envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedEnvelope {
    /// The fresh id, which a `command.ack` will echo.
    pub message_id: MessageId,
    /// The frame text.
    pub text: String,
}

/// Why a controller envelope could not be encoded.
#[derive(Debug)]
pub enum EncodeError {
    /// The body's type is one only the device sends.
    NotControllerMessage(MessageType),
    /// The frame would exceed [`MAX_CONTROL_BYTES`].
    Oversize {
        /// The message type.
        message_type: MessageType,
        /// The frame's length in bytes.
        bytes: usize,
    },
    /// The body could not be serialized.
    Json(serde_json::Error),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::NotControllerMessage(t) => write!(f, "{t} is not a controller message"),
            EncodeError::Oversize {
                message_type,
                bytes,
            } => {
                write!(
                    f,
                    "{message_type} of {bytes} bytes exceeds the {MAX_CONTROL_BYTES}-byte control limit"
                )
            }
            EncodeError::Json(err) => write!(f, "body does not serialize: {err}"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Serialize)]
struct WireEnvelope<'a, B> {
    protocol: u64,
    #[serde(rename = "type")]
    message_type: MessageType,
    session_id: Option<&'a str>,
    message_id: &'a str,
    device_id: &'a str,
    generation: u32,
    body: &'a B,
}

/// Wraps `body` in an envelope with a fresh UUIDv4 `message_id`.
/// `session_id` is `None` only for `session.rejected`.
pub fn encode_envelope<B: ControlBody>(
    body: &B,
    session_id: Option<&LinkSessionId>,
    device_id: &DeviceId,
    generation: u32,
) -> Result<EncodedEnvelope, EncodeError> {
    let message_type = B::MESSAGE_TYPE;
    if !message_type.sent_by_controller() {
        return Err(EncodeError::NotControllerMessage(message_type));
    }
    let message_id = MessageId::generate();
    let text = serde_json::to_string(&WireEnvelope {
        protocol: PROTOCOL_VERSION,
        message_type,
        session_id: session_id.map(LinkSessionId::as_str),
        message_id: message_id.as_str(),
        device_id: &device_id.0,
        generation,
        body,
    })
    .map_err(EncodeError::Json)?;
    if text.len() > MAX_CONTROL_BYTES {
        return Err(EncodeError::Oversize {
            message_type,
            bytes: text.len(),
        });
    }
    Ok(EncodedEnvelope { message_id, text })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::session::SessionRejected;
    use crate::vocab::{AckStatus, RejectReason};

    const SESSION: &str = "4c4b0d36-8d0f-4b7e-9a51-5c1f4a8b2e10";

    fn envelope(message_type: &str, body: Value) -> Value {
        json!({"protocol": 1, "type": message_type, "session_id": SESSION,
               "message_id": "0f7e2a4c-1b3d-4e5f-8a9b-0c1d2e3f4a5b", "device_id": "G090XX", "generation": 0,
               "body": body})
    }

    fn parse(value: &Value) -> Result<Inbound, ProtocolError> {
        parse_envelope(ControlFrame::Text(&value.to_string()))
    }

    fn code(value: &Value) -> ProtocolErrorCode {
        parse(value).expect_err("rejected").code
    }

    fn hello_body() -> Value {
        json!({"capabilities": ["audio_timeline_v1"], "platform": "fireos6", "protocols": [1]})
    }

    #[test]
    fn a_valid_envelope_yields_its_header_and_typed_body() {
        let mut env = envelope("heartbeat", json!({"mono_ns": "123456789"}));
        env["generation"] = json!(u32::MAX);
        let Inbound::Known(env) = parse(&env).unwrap() else {
            panic!("known")
        };
        assert_eq!(env.header.session_id, Some(LinkSessionId(SESSION.into())));
        assert_eq!(env.header.device_id, DeviceId("G090XX".into()));
        assert_eq!(env.header.generation, u32::MAX);
        assert_eq!(
            env.message,
            DeviceMessage::Heartbeat(Heartbeat {
                mono_ns: 123_456_789
            })
        );
    }

    #[test]
    fn binary_and_non_json_frames_are_malformed() {
        let err = parse_envelope(ControlFrame::Binary(b"{}")).unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::MalformedMessage);
        for text in [
            "",
            "not json",
            "[1]",
            "\"text\"",
            "{\"protocol\":1",
            "{\"a\":NaN}",
        ] {
            let err = parse_envelope(ControlFrame::Text(text)).unwrap_err();
            assert_eq!(err.code, ProtocolErrorCode::MalformedMessage, "{text:?}");
        }
    }

    #[test]
    fn frames_over_256_kib_are_oversize_and_the_limit_itself_is_allowed() {
        let base = envelope("log", json!({"message": ""})).to_string();
        let pad = MAX_CONTROL_BYTES - base.len();
        let at_limit = base.replace(
            "\"message\":\"\"",
            &format!("\"message\":\"{}\"", "x".repeat(pad)),
        );
        assert_eq!(at_limit.len(), MAX_CONTROL_BYTES);
        assert!(parse_envelope(ControlFrame::Text(&at_limit)).is_ok());
        let over = at_limit.replace("\"message\":\"", "\"message\":\"y");
        assert_eq!(
            parse_envelope(ControlFrame::Text(&over)).unwrap_err().code,
            ProtocolErrorCode::Oversize
        );
        let multibyte = base.replace(
            "\"message\":\"\"",
            &format!("\"message\":\"{}\"", "é".repeat(pad / 2 + 1)),
        );
        assert_eq!(
            parse_envelope(ControlFrame::Text(&multibyte))
                .unwrap_err()
                .code,
            ProtocolErrorCode::Oversize
        );
    }

    #[test]
    fn protocol_must_be_the_integer_one() {
        for protocol in [
            json!(2),
            json!(0),
            json!(1.0),
            json!("1"),
            json!(true),
            json!(null),
        ] {
            let mut env = envelope("heartbeat", json!({"mono_ns": "1"}));
            env["protocol"] = protocol.clone();
            assert_eq!(
                code(&env),
                ProtocolErrorCode::UnsupportedProtocol,
                "{protocol}"
            );
        }
        let mut env = envelope("heartbeat", json!({"mono_ns": "1"}));
        env.as_object_mut().unwrap().remove("protocol");
        assert_eq!(code(&env), ProtocolErrorCode::UnsupportedProtocol);
    }

    #[test]
    fn every_required_envelope_field_is_checked() {
        let cases: [(&str, Option<Value>); 13] = [
            ("type", None),
            ("type", Some(json!(""))),
            ("type", Some(json!(3))),
            ("message_id", None),
            ("message_id", Some(json!(""))),
            ("device_id", Some(json!(""))),
            ("device_id", Some(json!(null))),
            ("session_id", None),
            ("session_id", Some(json!(7))),
            ("generation", Some(json!(-1))),
            ("generation", Some(json!(u64::from(u32::MAX) + 1))),
            ("generation", Some(json!(1.5))),
            ("body", Some(json!([]))),
        ];
        for (key, value) in cases {
            let mut env = envelope("heartbeat", json!({"mono_ns": "1"}));
            match &value {
                Some(v) => env[key] = v.clone(),
                None => {
                    env.as_object_mut().unwrap().remove(key);
                }
            }
            assert_eq!(
                code(&env),
                ProtocolErrorCode::MalformedMessage,
                "{key} = {value:?}"
            );
        }
    }

    #[test]
    fn an_unknown_type_is_well_formed_and_keeps_its_header() {
        let env = envelope("teleport.begin", json!({"anything": [1, 2]}));
        let Inbound::Unknown(unknown) = parse(&env).unwrap() else {
            panic!("unknown")
        };
        assert_eq!(unknown.message_type, "teleport.begin");
        assert_eq!(unknown.header.device_id, DeviceId("G090XX".into()));
        let mut bad = env;
        bad["generation"] = json!(-3);
        assert_eq!(
            code(&bad),
            ProtocolErrorCode::MalformedMessage,
            "the envelope rules still apply"
        );
    }

    #[test]
    fn a_controller_only_type_from_the_device_is_unexpected() {
        assert_eq!(
            code(&envelope("session.ready", json!({}))),
            ProtocolErrorCode::UnexpectedMessage
        );
        assert_eq!(
            code(&envelope("render.start", json!({}))),
            ProtocolErrorCode::UnexpectedMessage
        );
    }

    #[test]
    fn bodies_without_a_decoder_here_pass_through_as_received() {
        let body = json!({"candidate_id": "c", "hops": []});
        let Inbound::Known(env) = parse(&envelope("wake.candidate", body.clone())).unwrap() else {
            panic!()
        };
        let DeviceMessage::Other {
            message_type,
            body: got,
        } = env.message
        else {
            panic!("other")
        };
        assert_eq!(message_type, MessageType::WakeCandidate);
        assert_eq!(Value::Object(got), body);
    }

    #[test]
    fn heartbeat_clock_request_and_command_ack_bodies_are_validated() {
        assert_eq!(
            code(&envelope("heartbeat", json!({}))),
            ProtocolErrorCode::MalformedMessage
        );
        assert_eq!(
            code(&envelope("heartbeat", json!({"mono_ns": 5}))),
            ProtocolErrorCode::MalformedMessage
        );
        assert_eq!(
            code(&envelope("clock.request", json!({"nonce": ""}))),
            ProtocolErrorCode::MalformedMessage
        );
        let ok = envelope(
            "command.ack",
            json!({"message_id": "m", "status": "durable", "error": null}),
        );
        let Inbound::Known(env) = parse(&ok).unwrap() else {
            panic!()
        };
        assert_eq!(
            env.message,
            DeviceMessage::CommandAck(CommandAck {
                message_id: MessageId("m".into()),
                status: AckStatus::Durable,
                error: None
            })
        );
        for body in [
            json!({"message_id": "m", "status": "done"}),
            json!({"message_id": "", "status": "accepted"}),
            json!({"message_id": "m", "status": "rejected", "error": 4}),
        ] {
            assert_eq!(
                code(&envelope("command.ack", body.clone())),
                ProtocolErrorCode::MalformedMessage,
                "{body}"
            );
        }
    }

    fn hello_frame(session_id: Value, generation: u32, message_type: &str) -> String {
        let mut env = envelope(message_type, hello_body());
        env["session_id"] = session_id;
        env["generation"] = json!(generation);
        env.to_string()
    }

    #[test]
    fn a_hello_needs_a_null_session_and_generation_zero() {
        let ok = parse_hello(ControlFrame::Text(&hello_frame(
            Value::Null,
            0,
            "session.hello",
        )))
        .unwrap();
        assert_eq!(ok.header.session_id, None);
        assert_eq!(ok.header.device_id, DeviceId("G090XX".into()));

        let refused = parse_hello(ControlFrame::Text(&hello_frame(
            json!(SESSION),
            0,
            "session.hello",
        )))
        .unwrap_err();
        assert_eq!(refused.error.code, ProtocolErrorCode::WrongSession);
        assert_eq!(refused.device_id, Some(DeviceId("G090XX".into())));
        let refused = parse_hello(ControlFrame::Text(&hello_frame(
            Value::Null,
            1,
            "session.hello",
        )))
        .unwrap_err();
        assert_eq!(refused.error.code, ProtocolErrorCode::MalformedMessage);
    }

    #[test]
    fn a_first_message_other_than_hello_is_unexpected() {
        let mut first = envelope("heartbeat", json!({"mono_ns": "1"}));
        first["session_id"] = Value::Null;
        let refused = parse_hello(ControlFrame::Text(&first.to_string())).unwrap_err();
        assert_eq!(refused.error.code, ProtocolErrorCode::UnexpectedMessage);
        let refused =
            parse_hello(ControlFrame::Text(&hello_frame(Value::Null, 0, "hello.v2"))).unwrap_err();
        assert_eq!(refused.error.code, ProtocolErrorCode::UnexpectedMessage);
    }

    #[test]
    fn a_malformed_hello_body_is_refused_with_the_device_named() {
        let mut env = envelope(
            "session.hello",
            json!({"capabilities": "leds", "protocols": [1]}),
        );
        env["session_id"] = Value::Null;
        let refused = parse_hello(ControlFrame::Text(&env.to_string())).unwrap_err();
        assert_eq!(refused.error.code, ProtocolErrorCode::MalformedMessage);
        assert_eq!(refused.device_id, Some(DeviceId("G090XX".into())));
        let refused = parse_hello(ControlFrame::Binary(&[1, 2])).unwrap_err();
        assert_eq!(refused.device_id, None);
    }

    #[test]
    fn live_session_frames_must_name_the_session_and_device() {
        let session = LinkSessionId(SESSION.into());
        let device = DeviceId("G090XX".into());
        let ok = parse(&envelope("heartbeat", json!({"mono_ns": "1"}))).unwrap();
        assert_eq!(check_live_session(&ok, &session, &device), Ok(()));

        let mut other = envelope("teleport", json!({}));
        other["session_id"] = json!("another");
        let err = check_live_session(&parse(&other).unwrap(), &session, &device).unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::WrongSession);
        let mut null = envelope("heartbeat", json!({"mono_ns": "1"}));
        null["session_id"] = Value::Null;
        let err = check_live_session(&parse(&null).unwrap(), &session, &device).unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::WrongSession);
        let err = check_live_session(&ok, &session, &DeviceId("OTHER".into())).unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::WrongDevice);
        let hello = parse(&envelope("session.hello", hello_body())).unwrap();
        let err = check_live_session(&hello, &session, &device).unwrap_err();
        assert_eq!(err.code, ProtocolErrorCode::UnexpectedMessage);
    }

    #[test]
    fn encoded_envelopes_carry_every_field_and_a_fresh_uuid4() {
        let device = DeviceId("G090XX".into());
        let session = LinkSessionId(SESSION.into());
        let a =
            encode_envelope(&Heartbeat { mono_ns: u64::MAX }, Some(&session), &device, 7).unwrap();
        let b = encode_envelope(&Heartbeat { mono_ns: 1 }, Some(&session), &device, 7).unwrap();
        assert_ne!(a.message_id, b.message_id);
        let id = uuid::Uuid::parse_str(a.message_id.as_str()).unwrap();
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(
            id.to_string(),
            a.message_id.as_str(),
            "lowercase hyphenated"
        );
        let value: Value = serde_json::from_str(&a.text).unwrap();
        assert_eq!(
            value,
            json!({"protocol": 1, "type": "heartbeat", "session_id": SESSION, "message_id": a.message_id.as_str(),
                   "device_id": "G090XX", "generation": 7, "body": {"mono_ns": "18446744073709551615"}})
        );
        let rejected = encode_envelope(
            &SessionRejected {
                reason: RejectReason::PendingApproval,
            },
            None,
            &device,
            0,
        )
        .unwrap();
        let value: Value = serde_json::from_str(&rejected.text).unwrap();
        assert_eq!(value["session_id"], Value::Null);
        assert_eq!(value["body"], json!({"reason": "pending_approval"}));
    }

    #[test]
    fn an_encoded_frame_over_the_limit_is_refused() {
        let device = DeviceId("G090XX".into());
        let big = ProtocolError::new(
            ProtocolErrorCode::MalformedMessage,
            "x".repeat(MAX_CONTROL_BYTES),
        );
        assert!(matches!(
            encode_envelope(&big, None, &device, 0),
            Err(EncodeError::Oversize {
                message_type: MessageType::ProtocolError,
                ..
            })
        ));
    }

    #[test]
    fn a_body_typed_as_a_device_message_cannot_be_encoded() {
        #[derive(Serialize)]
        struct Hello {}
        impl ControlBody for Hello {
            const MESSAGE_TYPE: MessageType = MessageType::SessionHello;
        }
        let err = encode_envelope(&Hello {}, None, &DeviceId("d".into()), 0).unwrap_err();
        assert!(matches!(
            err,
            EncodeError::NotControllerMessage(MessageType::SessionHello)
        ));
    }
}
