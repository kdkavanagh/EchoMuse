//! EchoMuse controller application runtime.
//!
//! This crate assembles the controller: the device runtime threads and their speech
//! workers, the registry of runtimes, device link admission, the device's ESPHome
//! nodes, the alert service's device link, firmware delivery, the dashboard's view of
//! the fleet, and the startup and shutdown order of every shared service. Shared
//! identities and service contracts live in `echomuse-core`; each external contract has
//! its own crate.

pub mod admission;
pub mod alert_link;
pub mod alerts_dashboard;
pub mod app;
pub mod capture;
pub mod clip_windows;
pub mod control_plane;
pub mod correction;
pub mod dashboard;
pub mod device;
pub mod device_events;
pub mod device_logs;
pub mod dispatcher;
pub mod esphome_node;
pub mod firmware_control;
pub mod kiosk;
pub mod recorder_link;
pub mod recording_modes;
pub mod runtimes;
pub mod shell_bridge;
pub mod sounds;
pub mod speech_models;
pub mod speech_workers;
pub mod supervision;
pub mod taps;
pub mod text_fix;
pub mod voice_session;
pub mod wake_qualify;
pub mod wifi;
