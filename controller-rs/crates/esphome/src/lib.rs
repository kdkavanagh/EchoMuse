//! The ESPHome native API servers that present each device to Home Assistant
//! (spec §3.3.1 and §3.3.2): a voice satellite for every connected device
//! and, on a Dot with the proxy enabled, a separate Bluetooth proxy.
//!
//! Each node is plaintext only, reports API version 1.10 and asks for no
//! password. The device runtime starts a node when its device session is
//! attached and shuts it down when the session ends (§5.7); advertising the
//! node over mDNS is the discovery crate's job.
//!
//! - [`voice::start_voice_satellite`] returns a [`voice::VoiceSatellite`]
//!   handle, through which the runtime sets entity states, fires button
//!   events and runs controller-started voice runs, and a channel of
//!   [`voice::SatelliteRequest`]s from Home Assistant.
//! - [`bluetooth::start_bluetooth_proxy`] returns a
//!   [`bluetooth::BluetoothProxy`] that forwards advertisements while Home
//!   Assistant is subscribed.
//!
//! [`api`] and [`frame`] are the wire layer, public so that a test client can
//! speak the protocol with the same message types.

#![warn(missing_docs)]

pub mod api;
pub mod bluetooth;
pub mod entities;
pub mod frame;
pub mod identity;
mod server;
pub mod voice;

pub use server::{CloseReason, ServerStatus, ServerStopped, StartError};
