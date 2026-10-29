//! The device link transport: control, audio, asset and shell WebSocket listeners,
//! admission, liveness and the ordered hand-off of device traffic to its runtime
//! (spec §3.1.1–§3.1.4, §3.1.9, §5.2, §5.7 steps 1–3).
//!
//! # How the runtime plugs in
//!
//! 1. Build a [`LinkConfig`] ([`LinkConfig::from_controller`]) and start
//!    [`LinkServer::bind`] with the database handle, an [`Admission`] and an
//!    [`AssetSource`].
//! 2. For every hello that passes the protocol, token and approval checks, the link
//!    ends the device's previous session, then calls [`Admission::admit`] with an
//!    [`AdmissionRequest`] and the new session's [`LinkSession`]. The integrator
//!    attaches the device runtime, keeps the `LinkSession`, and answers
//!    [`AdmissionDecision::Ready`] with the [`ReadyGrant`](echomuse_protocol::session::ReadyGrant)
//!    (`capture_permitted`, assets, detector policy, AFE request). The link writes
//!    `session.ready` and starts the session.
//! 3. The runtime then reads, urgent first:
//!    - [`UrgentReceiver`]: privacy changes, then exactly one [`UrgentEvent::Ended`].
//!      On a mute the link has already cancelled the scope published with
//!      [`SessionLink::publish_capture_scope`]; at the end it has cancelled the
//!      session's token ([`SessionLink::cancellation`]) too.
//!    - [`IngressReceiver`]: [`Ingress::Control`] and [`Ingress::Audio`] in the order
//!      they were queued, numbered by [`IngressOrdinal`] as they are taken.
//!    - [`TelemetryReceiver`]: coalesced and best-effort reports.
//! 4. It sends with [`SessionLink::send`] (fire and forget) and
//!    [`SessionLink::request`] (returns a [`PendingAck`] that resolves to the device's
//!    `command.ack`), and render frames with [`SessionLink::send_render`]. Every
//!    envelope carries the session's id.
//! 5. It reports a stream registry violation with
//!    [`SessionLink::protocol_violation`]; the link sends `protocol.error` and
//!    closes.
//!
//! # The stream registry belongs to the runtime
//!
//! Stream epoch state ([`StreamRegistry`](echomuse_protocol::streams::StreamRegistry))
//! has one owner, the device runtime (spec §5.2): it applies `stream.open` and
//! `stream.end`, validates each packet and grants backfill when a subscription starts
//! wanting a stream, all in ingress order. Backfill grants come from subscription
//! decisions only the runtime makes, so a registry in the link would need those
//! decisions sent back across tasks and could check a packet before the grant that
//! allows it. The link therefore checks each frame's own rules (envelope, body and
//! EMA1 header, the `protocol.error` codes that need no stream state) and hands
//! `stream.open`, `stream.end` and every packet over in one ordered channel.
//!
//! # What the link handles itself
//!
//! Heartbeats both ways (degraded after 2 s of control silence, lost after 3 s),
//! `clock.request`, `command.ack` correlation, unknown message types (liveness only),
//! the device's `protocol.error`, the asset socket and shell dial-backs. None of this
//! waits for the runtime.
//!
//! # Shared state
//!
//! Per-session state lives in the session's own tasks. The two routing tables
//! (sessions by id for audio and asset upgrades, shells by device) sit behind short
//! locks because an upgrade must be accepted or refused before it happens; nothing
//! is awaited while they are held.

#![warn(missing_docs)]

mod admission;
mod assets;
mod auth;
mod commands;
mod config;
mod control;
mod handshake;
mod ingress;
mod registry;
mod server;
mod session;
mod shell;

#[cfg(test)]
mod tests;

pub use admission::{Admission, AdmissionDecision, AdmissionRequest, AssetSource};
pub use auth::{HEADER_SESSION, HEADER_TOKEN, Transport};
pub use commands::{CommandFailure, CommandProgress, PendingAck};
pub use config::{
    EgressLimits, LinkConfig, LinkConfigError, LinkLimits, LinkTimings, MAX_ASSET_CHUNK_BYTES,
    MAX_FRAME_BYTES, SystemClock, TlsListenerConfig, WallClock,
};
pub use ingress::{
    Ingress, IngressItem, IngressOrdinal, IngressReceiver, PrivacyChange, Telemetry, TelemetryKind,
    TelemetryReceiver, UrgentEvent, UrgentReceiver,
};
pub use server::{LinkError, LinkServer, PATH_ASSETS, PATH_AUDIO, PATH_CONTROL, PATH_SHELL_PREFIX};
pub use session::{
    CloseReason, HeartbeatSample, LinkSession, LinkStatus, QueueKind, RenderError, SendError,
    SessionLink, SocketKind,
};
pub use shell::{
    ShellBroker, ShellClose, ShellConnection, ShellError, ShellInput, ShellMode, ShellOpen,
    ShellPending, ShellReader, ShellWriter,
};
