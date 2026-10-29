//! The controller's audio subscription commands, as the Go firmware decodes
//! them (`device/internal/proto/proto.go` "Uplink subscriptions",
//! `device/internal/uplink`, `docs/protocol-v1.md` §4.5).
//!
//! The wire still calls a subscription a lease, so the JSON key is `lease_id`.
//! The generation travels in the envelope, never in the body: a subscription
//! starts at generation 1, and converting a candidate into a turn sends
//! `uplink.renew` at the next generation with `reason: "turn"` and the turn as
//! owner. The device acknowledges every command with `command.ack`, rejecting
//! with `unknown_lease`, `stale_generation`, `muted` or `invalid`.
//!
//! `uplink.ended`, the device's report, is parsed by the protocol crate
//! ([`echomuse_protocol::streams::UplinkEnded`]).

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use echomuse_protocol::{ControlBody, MessageType, StreamId};
use echomuse_turn::Sample;
use serde::{Serialize, Serializer};

/// How long the device keeps a subscription without a renewal: the `ttl_ms`
/// the controller sends. The firmware's own default for a missing or zero
/// `ttl_ms` is the same 3 s (`uplink.DefaultTTL`).
pub const SUBSCRIPTION_TTL: Duration = Duration::from_secs(3);

/// How often the controller renews a live subscription: every second, like
/// dialog focus leases (post-AFE architecture §4.4).
pub const RENEW_INTERVAL: Duration = Duration::from_secs(1);

/// How long the device waits for the controller's `command.ack` of a
/// `wake.candidate` before ending the candidate subscription with `ttl`
/// (`uplink.CandidateAckWait`). Informational: the controller acknowledges at
/// once.
pub const CANDIDATE_ACK_WAIT: Duration = Duration::from_secs(1);

/// The wire's `"live"` start: from the stream's position when the device
/// applies the command.
pub const START_LIVE: &str = "live";

macro_rules! vocabulary {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $wire:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// The exact wire spelling.
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $wire, )+
                }
            }

            /// Parses the exact wire spelling; anything else is `None`.
            pub fn from_wire(text: &str) -> Option<Self> {
                match text {
                    $( $wire => Some($name::$variant), )+
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

vocabulary! {
    /// What a subscription is for. The device opens `candidate` itself with
    /// `wake.candidate`; the controller opens the others with `uplink.open`,
    /// or reaches `turn` by converting a candidate.
    pub enum SubscriptionReason {
        /// A wake candidate's audio, opened by the device.
        Candidate => "candidate",
        /// A turn's utterance, or the response recording after a commit.
        Turn => "turn",
        /// A reply window's listening.
        Reply => "reply",
        /// A dashboard recording mode.
        Diagnostic => "diagnostic",
        /// One near miss's recording (`near_miss_v1` only).
        NearMiss => "near_miss",
    }
}

vocabulary! {
    /// `uplink.open.reason`: every reason but `candidate`.
    pub enum OpenReason {
        /// A button turn, or the response recording after a commit.
        Turn => "turn",
        /// A reply window.
        Reply => "reply",
        /// A dashboard recording mode.
        Diagnostic => "diagnostic",
        /// A near miss's recording.
        NearMiss => "near_miss",
    }
}

impl From<OpenReason> for SubscriptionReason {
    fn from(reason: OpenReason) -> Self {
        match reason {
            OpenReason::Turn => SubscriptionReason::Turn,
            OpenReason::Reply => SubscriptionReason::Reply,
            OpenReason::Diagnostic => SubscriptionReason::Diagnostic,
            OpenReason::NearMiss => SubscriptionReason::NearMiss,
        }
    }
}

vocabulary! {
    /// `uplink.close.reason`.
    pub enum CloseReason {
        /// The wake candidate was refused.
        Rejected => "rejected",
        /// Another device took the wake.
        ArbitrationLost => "arbitration_lost",
        /// The utterance committed.
        Committed => "committed",
        /// Anything else: the subscription is simply no longer needed.
        Closed => "closed",
    }
}

/// A subscription's ID: the wire's `lease_id`. The device chooses it for a
/// candidate subscription; the controller chooses a UUIDv4 for its own.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SubscriptionId(String);

impl SubscriptionId {
    /// A fresh UUIDv4 for a subscription the controller opens.
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The ID the device sent, unless it is empty.
    pub fn from_device(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        (!id.is_empty()).then_some(Self(id))
    }

    /// The wire text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SubscriptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a subscription serves: a turn, reply expectation, near miss or
/// diagnostic mode's ID. A candidate subscription is owned by its own ID
/// until a conversion names the turn, as the firmware records it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Owner(String);

impl Owner {
    /// An owner, unless `id` is empty: the firmware refuses a conversion
    /// without one.
    pub fn new(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        (!id.is_empty()).then_some(Self(id))
    }

    /// The wire text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A candidate subscription's owner: its own ID, as the firmware records
    /// it until a conversion names the turn.
    pub(crate) fn of_subscription(id: &SubscriptionId) -> Self {
        Self(id.0.clone())
    }
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A subscription's generation, the envelope `generation` of every command
/// about it. It starts at 1 and only a candidate's conversion raises it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(u32);

impl Generation {
    /// Every subscription's first generation.
    pub const FIRST: Generation = Generation(1);

    /// The envelope value.
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The generation after this one, which converts a candidate.
    pub const fn next(self) -> Generation {
        Generation(self.0.saturating_add(1))
    }
}

/// Where a stream of a subscription starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamStart {
    /// At a capture-epoch sample. The device rounds it down to the stream's
    /// grid (mic and cells to 512, AFE to 1,280; the reference it first maps
    /// to the reference epoch and rounds to 2,560) and clips it to its ring.
    At(Sample),
    /// From the stream's position when the device applies the command.
    Live,
}

impl Serialize for StreamStart {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            StreamStart::At(sample) => serializer.collect_str(&sample.0),
            StreamStart::Live => serializer.serialize_str(START_LIVE),
        }
    }
}

fn ttl_ms<S: Serializer>(ttl: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX))
}

/// `uplink.open`, at generation 1. An omitted stream is not wanted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UplinkOpen {
    /// The subscription.
    #[serde(rename = "lease_id")]
    pub subscription_id: SubscriptionId,
    /// What it serves.
    pub owner: Owner,
    /// Why it is open.
    pub reason: OpenReason,
    /// Each wanted stream's start.
    pub streams: BTreeMap<StreamId, StreamStart>,
    /// How long the device keeps it without a renewal.
    #[serde(rename = "ttl_ms", serialize_with = "ttl_ms")]
    pub ttl: Duration,
}

impl ControlBody for UplinkOpen {
    const MESSAGE_TYPE: MessageType = MessageType::UplinkOpen;
}

/// The marker of a conversion: `reason: "turn"`, the only reason a renewal
/// may carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ConversionReason {
    /// Into the accepted turn's subscription.
    #[serde(rename = "turn")]
    Turn,
}

/// The fields that turn a renewal into a candidate's conversion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Conversion {
    /// Always `turn`.
    pub reason: ConversionReason,
    /// The turn that now owns the subscription.
    pub owner: Owner,
}

/// `uplink.renew`: at the subscription's generation it only extends the TTL
/// and must carry neither `reason` nor `owner`; at the next generation, with a
/// [`Conversion`], it converts an accepted candidate subscription in place.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UplinkRenew {
    /// The subscription.
    #[serde(rename = "lease_id")]
    pub subscription_id: SubscriptionId,
    /// The new time to live from the device's receipt.
    #[serde(rename = "ttl_ms", serialize_with = "ttl_ms")]
    pub ttl: Duration,
    /// Present only when converting a candidate.
    #[serde(flatten)]
    pub conversion: Option<Conversion>,
}

impl ControlBody for UplinkRenew {
    const MESSAGE_TYPE: MessageType = MessageType::UplinkRenew;
}

/// `uplink.close`, at the subscription's generation. The device answers with
/// `uplink.ended` `closed`; closing a candidate releases its provisional duck.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct UplinkClose {
    /// The subscription.
    #[serde(rename = "lease_id")]
    pub subscription_id: SubscriptionId,
    /// Why.
    pub reason: CloseReason,
}

impl ControlBody for UplinkClose {
    const MESSAGE_TYPE: MessageType = MessageType::UplinkClose;
}

/// One subscription command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UplinkCommand {
    /// `uplink.open`.
    Open(UplinkOpen),
    /// `uplink.renew`.
    Renew(UplinkRenew),
    /// `uplink.close`.
    Close(UplinkClose),
}

impl UplinkCommand {
    /// The envelope `type`.
    pub fn message_type(&self) -> MessageType {
        match self {
            UplinkCommand::Open(_) => UplinkOpen::MESSAGE_TYPE,
            UplinkCommand::Renew(_) => UplinkRenew::MESSAGE_TYPE,
            UplinkCommand::Close(_) => UplinkClose::MESSAGE_TYPE,
        }
    }

    /// The subscription it concerns.
    pub fn subscription_id(&self) -> &SubscriptionId {
        match self {
            UplinkCommand::Open(b) => &b.subscription_id,
            UplinkCommand::Renew(b) => &b.subscription_id,
            UplinkCommand::Close(b) => &b.subscription_id,
        }
    }
}

/// A command for the device runtime to send in an envelope carrying
/// `generation`. The device acknowledges it; a rejection goes back through
/// [`Uplink::command_rejected`](crate::Uplink::command_rejected).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Outgoing {
    /// The envelope generation.
    pub generation: Generation,
    /// The body.
    pub command: UplinkCommand,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn open_serializes_as_the_firmware_decodes_it() {
        let open = UplinkOpen {
            subscription_id: SubscriptionId::from_device("L1").unwrap(),
            owner: Owner::new("turn-1").unwrap(),
            reason: OpenReason::NearMiss,
            streams: BTreeMap::from([
                (StreamId::Mic, StreamStart::At(Sample(1_364_068_352))),
                (StreamId::Reference, StreamStart::Live),
            ]),
            ttl: SUBSCRIPTION_TTL,
        };
        assert_eq!(
            serde_json::to_value(&open).unwrap(),
            json!({"lease_id": "L1", "owner": "turn-1", "reason": "near_miss",
                   "streams": {"mic": "1364068352", "reference": "live"}, "ttl_ms": 3000})
        );
    }

    #[test]
    fn a_plain_renewal_carries_neither_reason_nor_owner() {
        let id = SubscriptionId::from_device("L1").unwrap();
        let plain = UplinkRenew {
            subscription_id: id.clone(),
            ttl: SUBSCRIPTION_TTL,
            conversion: None,
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            json!({"lease_id": "L1", "ttl_ms": 3000})
        );
        let convert = UplinkRenew {
            subscription_id: id,
            ttl: SUBSCRIPTION_TTL,
            conversion: Some(Conversion {
                reason: ConversionReason::Turn,
                owner: Owner::new("turn-9").unwrap(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&convert).unwrap(),
            json!({"lease_id": "L1", "ttl_ms": 3000, "reason": "turn", "owner": "turn-9"})
        );
    }

    #[test]
    fn close_and_vocabularies_use_the_wire_spellings() {
        let close = UplinkClose {
            subscription_id: SubscriptionId::from_device("L1").unwrap(),
            reason: CloseReason::ArbitrationLost,
        };
        assert_eq!(
            serde_json::to_value(&close).unwrap(),
            json!({"lease_id": "L1", "reason": "arbitration_lost"})
        );
        for reason in [
            SubscriptionReason::Candidate,
            SubscriptionReason::Turn,
            SubscriptionReason::Reply,
            SubscriptionReason::Diagnostic,
            SubscriptionReason::NearMiss,
        ] {
            assert_eq!(SubscriptionReason::from_wire(reason.as_str()), Some(reason));
        }
        assert_eq!(OpenReason::from_wire("candidate"), None);
        assert_eq!(Generation::FIRST.next().get(), 2);
        assert!(SubscriptionId::from_device("").is_none());
        assert!(Owner::new("").is_none());
        assert_ne!(SubscriptionId::generate(), SubscriptionId::generate());
    }
}
