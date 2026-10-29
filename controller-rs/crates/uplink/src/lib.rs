//! The device audio timeline and its subscriptions: per-epoch stream buffers,
//! uplink subscriptions, backfill grants and the stream starts the device
//! computes (spec §3.1.5–§3.1.6, §5.2, §5.7; `docs/protocol-v1.md` §4.5 and
//! §5; the firmware's `device/internal/uplink`, which is authoritative).
//!
//! A device sends mic (EMA1 kind 1), reference (kind 2), cell (kind 4) and AFE
//! (kind 5) packets only while an audio subscription wants them. The device
//! opens a candidate subscription itself with `wake.candidate`; the controller
//! opens the others with `uplink.open`, converts a candidate into a turn with
//! `uplink.renew` at the next generation, renews every live subscription each
//! second with a 3 s TTL, and closes them with `uplink.close`. The device ends
//! them on its own with `uplink.ended`, and ends all of them when the capture
//! epoch changes, the mute goes on or the session ends.
//!
//! Everything here is owned and driven by the device runtime, one [`Uplink`]
//! per session: no threads, no I/O, no clock reads. Time is passed in as
//! [`MonoInstant`](echomuse_turn::MonoInstant) values.
//!
//! # Using it from the device runtime
//!
//! - **Session.** Build [`SessionTerms`] from the hello's capabilities and
//!   whether `session.ready` carried `afe_metadata`, and an [`Uplink`] with
//!   the hello's mute state.
//! - **Ingress, in order.** Check `stream.open`, `stream.end` and every audio
//!   packet with the session's stream registry first, then hand the accepted
//!   ones to [`Uplink::stream_opened`], [`Uplink::stream_ended`] and
//!   [`Uplink::ingest`]. Hand `uplink.ended` to [`Uplink::device_ended`].
//! - **Urgent events.** Call [`Uplink::mute`] on `privacy.changed` muted,
//!   before any audio still queued behind it, and [`Uplink::unmute`] when it
//!   goes off. Call [`Uplink::session_lost`] when the session ends.
//! - **Decisions.** [`Uplink::accept_candidate`] before acknowledging a
//!   `wake.candidate` `accepted`; [`Uplink::convert_to_turn`] when the wake
//!   becomes a turn; the `open_*` methods and [`Uplink::close`] otherwise.
//!   Apply every returned [`Grant`] to the stream registry immediately, then
//!   send [`Uplink::take_commands`] in order, and report a `command.ack`
//!   rejection with [`Uplink::command_rejected`].
//! - **Time.** Call [`Uplink::poll`] at [`Uplink::next_deadline`]: renewals
//!   every second, holes that waited too long, drains that ran out.
//! - **Consumers.** After each call, [`Uplink::drain`] the deliveries and
//!   route each to its subscription's consumer (the turn policy, the speech
//!   workers, a recorder). Each subscription receives, per stream and epoch,
//!   data and missing ranges in sample order from its start, then one
//!   [`Ended`]. [`Uplink::backlog`] and [`Uplink::delivered`] give the byte
//!   accounting; a consumer that lets its backlog exceed the bounds gets its
//!   subscription closed with [`EndCause::Overflow`].
//! - **Reads.** [`Uplink::timeline`] reads any retained range of a named
//!   stream epoch, with gaps, missing ranges and unknown positions explicit.
//!
//! Not here: deciding whether to accept a candidate, the turn and reply
//! policies that decide when to open and close subscriptions (the response
//! recording's 1 s / 30 s close, the diagnostic modes' 30 minute limit), the
//! stream registry's validation, sending commands and correlating their
//! acknowledgements, and what consumers do with the audio.

pub mod buffer;
pub mod chunk;
pub mod config;
pub mod starts;
pub mod timeline;
pub mod uplink;
pub mod wire;

#[cfg(test)]
mod testutil;

pub use buffer::{BlockMeta, Segment, SegmentKind, Span};
pub use chunk::{
    Accounting, Backlog, Chunk, ChunkBody, ChunkData, Delivery, EndCause, Ended, MissingCause, Pcm,
};
pub use config::{Retention, SessionTerms, UplinkConfig};
pub use starts::CandidateStarts;
pub use timeline::{ReadError, Timeline};
pub use uplink::{
    CandidateOpen, CommandError, Grant, Ingested, NearMissOpen, OpenError, Opened,
    SubscriptionView, Uplink,
};
pub use wire::{
    CloseReason, Generation, OpenReason, Outgoing, Owner, StreamStart, SubscriptionId,
    SubscriptionReason, UplinkClose, UplinkCommand, UplinkOpen, UplinkRenew,
};
