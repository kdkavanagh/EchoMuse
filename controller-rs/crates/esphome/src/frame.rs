//! Plaintext framing of the ESPHome native API.
//!
//! A frame is the indicator byte `0x00`, the payload length as a protobuf
//! varint, the message type as a varint, then the payload. A node without
//! encryption treats any other first byte (a Noise client's `0x01`, for
//! example) as an error: it answers with [`BAD_INDICATOR_REPLY`] so the
//! client learns the node speaks plaintext, then closes. Frames longer than
//! [`MAX_FRAME_LEN`] are refused in both directions.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use thiserror::Error;

use crate::api::ApiMessage;

/// The first byte of every plaintext frame.
pub const PLAINTEXT_INDICATOR: u8 = 0x00;

/// The largest payload a frame may carry, in bytes. It matches the 16-bit
/// length of the Noise transport, which Home Assistant's client also applies
/// to plaintext.
pub const MAX_FRAME_LEN: usize = 65_535;

/// What an ESPHome node without encryption writes before closing when a
/// frame starts with anything but `0x00`: a plaintext indicator followed by a
/// short text, at least the three bytes a client's header parser needs.
pub const BAD_INDICATOR_REPLY: &[u8; 19] = b"\x00Bad indicator byte";

/// The longest varint the header may use: five bytes hold any `u32`.
const MAX_VARINT_LEN: usize = 5;

/// One received frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    /// The message type from the header.
    pub message_type: u16,
    /// The protobuf payload, a slice of the received buffer.
    pub payload: Bytes,
}

impl Frame {
    /// Decodes the payload as `M`. The caller has matched `message_type`
    /// against `M::ID`.
    pub fn decode<M: ApiMessage>(&self) -> Result<M, prost::DecodeError> {
        M::decode(self.payload.clone())
    }
}

/// Why the received bytes are not a valid frame. Every one of these ends the
/// connection.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FrameError {
    /// The frame did not start with [`PLAINTEXT_INDICATOR`]; `0x01` is a
    /// Noise (encrypted) client.
    #[error("frame indicator {0:#04x} is not plaintext")]
    BadIndicator(u8),
    /// The header declared a payload longer than [`MAX_FRAME_LEN`].
    #[error("frame length {0} exceeds {MAX_FRAME_LEN} bytes")]
    TooLarge(u64),
    /// A header varint ran past five bytes.
    #[error("frame header varint is longer than {MAX_VARINT_LEN} bytes")]
    VarintTooLong,
    /// The message type does not fit the 16 bits ESPHome allows.
    #[error("message type {0} exceeds 65535")]
    TypeOutOfRange(u64),
}

/// A message too large to send in one frame.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("message of {0} bytes exceeds the {MAX_FRAME_LEN}-byte frame limit")]
pub struct FrameTooLarge(pub usize);

/// Encodes `message` as one frame.
pub fn encode<M: ApiMessage>(message: &M) -> Result<Bytes, FrameTooLarge> {
    let len = message.encoded_len();
    if len > MAX_FRAME_LEN {
        return Err(FrameTooLarge(len));
    }
    let mut out = BytesMut::with_capacity(1 + 3 + 3 + len);
    out.put_u8(PLAINTEXT_INDICATOR);
    prost::encoding::encode_varint(len as u64, &mut out);
    prost::encoding::encode_varint(u64::from(M::ID), &mut out);
    message
        .encode(&mut out)
        .expect("BytesMut grows to fit the message");
    Ok(out.freeze())
}

/// Encodes a frame from a message type and an already encoded payload.
pub fn encode_raw(message_type: u16, payload: &[u8]) -> Result<Bytes, FrameTooLarge> {
    if payload.len() > MAX_FRAME_LEN {
        return Err(FrameTooLarge(payload.len()));
    }
    let mut out = BytesMut::with_capacity(1 + 3 + 3 + payload.len());
    out.put_u8(PLAINTEXT_INDICATOR);
    prost::encoding::encode_varint(payload.len() as u64, &mut out);
    prost::encoding::encode_varint(u64::from(message_type), &mut out);
    out.put_slice(payload);
    Ok(out.freeze())
}

/// Reassembles frames from a byte stream. Feed it whatever the socket
/// returned and take frames until it reports that it needs more.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: BytesMut,
}

/// The outcome of reading one varint from the front of a buffer.
enum Varint {
    Complete { value: u64, len: usize },
    Incomplete,
}

fn read_varint(bytes: &[u8]) -> Result<Varint, FrameError> {
    let mut value = 0u64;
    for (index, byte) in bytes.iter().take(MAX_VARINT_LEN).enumerate() {
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            return Ok(Varint::Complete {
                value,
                len: index + 1,
            });
        }
    }
    if bytes.len() >= MAX_VARINT_LEN {
        Err(FrameError::VarintTooLong)
    } else {
        Ok(Varint::Incomplete)
    }
}

impl FrameDecoder {
    /// An empty decoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// The buffer the socket reads into; bytes appended here are decoded by
    /// [`FrameDecoder::next_frame`].
    pub fn buffer_mut(&mut self) -> &mut BytesMut {
        &mut self.buffer
    }

    /// Appends received bytes.
    pub fn extend(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Takes the next complete frame, `Ok(None)` if more bytes are needed.
    /// A bad indicator or an oversized length is reported as soon as the
    /// header shows it, before any payload arrives.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let Some(&indicator) = self.buffer.first() else {
            return Ok(None);
        };
        if indicator != PLAINTEXT_INDICATOR {
            return Err(FrameError::BadIndicator(indicator));
        }
        let (len, len_size) = match read_varint(&self.buffer[1..])? {
            Varint::Complete { value, len } => (value, len),
            Varint::Incomplete => return Ok(None),
        };
        if len > MAX_FRAME_LEN as u64 {
            return Err(FrameError::TooLarge(len));
        }
        let type_start = 1 + len_size;
        let (message_type, type_size) = match read_varint(&self.buffer[type_start..])? {
            Varint::Complete { value, len } => (value, len),
            Varint::Incomplete => return Ok(None),
        };
        let message_type =
            u16::try_from(message_type).map_err(|_| FrameError::TypeOutOfRange(message_type))?;
        let header = type_start + type_size;
        // `len` is at most MAX_FRAME_LEN, checked above.
        let total = header + len as usize;
        if self.buffer.len() < total {
            self.buffer.reserve(total - self.buffer.len());
            return Ok(None);
        }
        self.buffer.advance(header);
        let payload = self.buffer.split_to(len as usize).freeze();
        Ok(Some(Frame {
            message_type,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{HelloRequest, PingRequest};

    #[test]
    fn a_frame_round_trips_through_the_decoder_in_pieces() {
        let hello = HelloRequest {
            client_info: "Home Assistant 2026.8.1".to_owned(),
            api_version_major: 1,
            api_version_minor: 14,
        };
        let mut wire = encode(&hello).unwrap().to_vec();
        wire.extend_from_slice(&encode(&PingRequest {}).unwrap());
        assert_eq!(&wire[wire.len() - 3..], &[0x00, 0x00, 0x07]);

        let mut decoder = FrameDecoder::new();
        let mut frames = Vec::new();
        for byte in wire {
            decoder.extend(&[byte]);
            while let Some(frame) = decoder.next_frame().unwrap() {
                frames.push(frame);
            }
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].message_type, 1);
        assert_eq!(frames[0].decode::<HelloRequest>().unwrap(), hello);
        assert_eq!(frames[1].message_type, 7);
        assert!(frames[1].payload.is_empty());
    }

    #[test]
    fn a_noise_hello_is_a_bad_indicator() {
        let mut decoder = FrameDecoder::new();
        decoder.extend(&[0x01, 0x00, 0x01, 0x00]);
        assert_eq!(decoder.next_frame(), Err(FrameError::BadIndicator(0x01)));
    }

    #[test]
    fn the_length_limit_is_checked_from_the_header_alone() {
        let mut decoder = FrameDecoder::new();
        // 65 535 is allowed: the header parses and the decoder waits for the payload.
        decoder.extend(&[0x00, 0xff, 0xff, 0x03, 0x07]);
        assert_eq!(decoder.next_frame(), Ok(None));

        let mut decoder = FrameDecoder::new();
        // 65 536 is not.
        decoder.extend(&[0x00, 0x80, 0x80, 0x04, 0x07]);
        assert_eq!(decoder.next_frame(), Err(FrameError::TooLarge(65_536)));
    }

    #[test]
    fn overlong_varints_and_types_are_errors() {
        let mut decoder = FrameDecoder::new();
        decoder.extend(&[0x00, 0x80, 0x80, 0x80, 0x80, 0x80]);
        assert_eq!(decoder.next_frame(), Err(FrameError::VarintTooLong));

        let mut decoder = FrameDecoder::new();
        decoder.extend(&[0x00, 0x00, 0x80, 0x80, 0x04]);
        assert_eq!(
            decoder.next_frame(),
            Err(FrameError::TypeOutOfRange(65_536))
        );
    }

    #[test]
    fn oversized_messages_are_not_encoded() {
        assert_eq!(
            encode_raw(1, &vec![0; MAX_FRAME_LEN + 1]),
            Err(FrameTooLarge(MAX_FRAME_LEN + 1))
        );
        let frame = encode_raw(1, &vec![0; MAX_FRAME_LEN]).unwrap();
        assert_eq!(&frame[..5], &[0x00, 0xff, 0xff, 0x03, 0x01]);
    }
}
