//! The ESPHome native API messages, generated at build time from the vendored
//! schema in `proto/` (aioesphomeapi 45.6.1, the version Home Assistant 2026.8.1
//! pins).
//!
//! Every message that can cross the wire implements [`ApiMessage`], which
//! carries the message-type number the frame header uses. The structs and
//! enums themselves are prost's: field names follow the schema in snake case.

/// Which side of a connection may send a message, from the schema's
/// `(source)` option. The ESPHome node is the server; Home Assistant is the
/// client.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageSource {
    /// Either side.
    Both,
    /// Only the node.
    Server,
    /// Only Home Assistant.
    Client,
}

/// A message that has a wire type number.
pub trait ApiMessage: prost::Message + Default {
    /// The message type the frame header carries.
    const ID: u16;
    /// The message's name in the schema.
    const NAME: &'static str;
    /// Which side may send it.
    const SOURCE: MessageSource;
}

#[allow(missing_docs, deprecated, clippy::all, clippy::pedantic)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/esphome_api.rs"));
    include!(concat!(env!("OUT_DIR"), "/message_ids.rs"));
}

pub use generated::*;

/// The name of message type `id`, if the schema has one.
pub fn message_name(id: u16) -> Option<&'static str> {
    MESSAGE_TYPES
        .binary_search_by_key(&id, |(number, _, _)| *number)
        .ok()
        .map(|index| MESSAGE_TYPES[index].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_has_every_message_type_once_in_order() {
        // aioesphomeapi 45.6.1's api.proto numbers 148 messages.
        assert_eq!(MESSAGE_TYPES.len(), 148);
        assert!(MESSAGE_TYPES.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert_eq!(MESSAGE_TYPES[0], (1, "HelloRequest", MessageSource::Client));
    }

    #[test]
    fn generated_impls_carry_the_schema_numbers() {
        assert_eq!(HelloRequest::ID, 1);
        assert_eq!(HelloResponse::ID, 2);
        assert_eq!(DisconnectRequest::SOURCE, MessageSource::Both);
        assert_eq!(DeviceInfoResponse::ID, 10);
        assert_eq!(BluetoothLeRawAdvertisementsResponse::ID, 93);
        assert_eq!(VoiceAssistantRequest::ID, 90);
        assert_eq!(VoiceAssistantRequest::SOURCE, MessageSource::Server);
        assert_eq!(VoiceAssistantAnnounceFinished::ID, 120);
        assert_eq!(message_name(106), Some("VoiceAssistantAudio"));
        assert_eq!(message_name(0), None);
    }
}
