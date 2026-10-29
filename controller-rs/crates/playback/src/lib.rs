//! Network playback to a device: media decoding, equalization, render flow
//! control, dialog focus leases and the per-device media player (spec §3.1.5,
//! §3.1.7, §4.5, §4.8, §4.9, §5.2).
//!
//! Every stateful type here is an owned value the device runtime holds and
//! drives; none spawns a background service or shares mutable state.
//!
//! - [`RenderClient`]: one per device session. It starts playbacks with
//!   `render.start`, holds audio until the device acknowledges, streams
//!   3,840-frame kind-3 packets within the 258,304-frame window, ends them
//!   with `render.end`, cancels them with a purge, and enforces the response
//!   limits of spoken answers. It reports through [`RenderEvent`]s.
//! - [`FocusLease`]: one `dialog_input` or `dialog_output` lease, renewed
//!   every second with a 3 s time to live.
//! - [`Decoder`]: fetches a URL and decodes it to 48 kHz mono PCM16 through
//!   an `ffmpeg` child on the shared runtime, streaming, bounded and
//!   cancellable.
//! - [`Equalizer`]: the dashboard's 8-band equalizer and speech boost.
//! - [`MediaPlayer`]: Home Assistant's media player for the device, with
//!   bookmarks, dialog deferral and the announcement queue.
//!
//! # How the runtime drives them
//!
//! The device link implements [`RenderLink`] (and so [`CommandLink`]):
//! `request` sends a [`Command`] at a generation and returns the message id
//! whose `command.ack` the runtime later hands back as an [`AckOutcome`];
//! `queue_frame` queues a render frame without waiting; `withdraw_frames`
//! drops a cancelled playback's queued frames. The runtime parses
//! `render.progress` and `render.finished` bodies with
//! [`RenderReport::parse`] and hands them, with the envelope's generation, to
//! [`RenderClient::on_report`].
//!
//! Time is passed in: every component takes `now` and reports its next
//! deadline, which the runtime turns into a timer.
//!
//! A media stream runs like this: the [`MediaPlayer`] emits
//! [`MediaEffect::StartContent`]; the runtime starts a [`Decoder`] stream and
//! a `content` playback; it moves decoded chunks into
//! [`RenderClient::push_audio`] while [`RenderClient::wants_audio`] holds,
//! calls [`RenderClient::finish_audio`] at the end, and reports the
//! playback's progress and end back to the player.

#![warn(missing_docs)]

pub mod decode;
pub mod eq;
pub mod focus;
pub mod link;
pub mod media;
pub mod render;
pub mod wire;

#[cfg(test)]
mod sim;

pub use decode::{
    DecodeEnd, DecodeError, DecodeEvent, DecodeRequest, DecodeStream, DecodeSummary, Decoder,
    FetchError, Seekability, resolve_media_url,
};
pub use eq::{EqSettings, EqSettingsError, Equalizer};
pub use focus::{FocusEvent, FocusLease, GenerationError, NotThisLease};
pub use link::{CommandLink, FramePurge, FrameRefused, LinkClosed, RenderFrame, RenderLink};
pub use media::{
    AnnouncementKey, AnnouncementPlan, CommandOutcome, ContentEnd, ContentEndReason, MediaEffect,
    MediaPlayer, Opener, PlayerState, SatelliteAnnouncement, StreamId, Unavailable,
};
pub use render::{
    AudioRefused, CancelDelivery, LocalStartError, NetworkStart, NotLive, PlaybackEnd, PlaybackRef,
    RenderClient, RenderEvent, ResponseLimit, ResponseLimits,
};
pub use wire::{
    AckFailure, AckOutcome, AlertPreviewSound, CancelReason, Command, Earcon, FocusKind,
    FocusOwner, LocalSource, NetworkClass, PlaybackId, RenderReport, SourceClass,
};
