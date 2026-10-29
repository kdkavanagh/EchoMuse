//! Device configuration (fleet-wide settings with per-device section overrides, §4.10)
//! and the content-addressed wake-model registry (§3.5.2, §4.11).
//!
//! What the application calls:
//!
//! 1. At start, off the async runtime: [`registry::WakeRegistry::open_or_seed`] with
//!    [`registry::SeedModel::deployed`] for the repository's `bcresnet_audio.onnx` and
//!    `.json`. Then [`service::SettingsService::start`] with the database handle and the
//!    registry, and [`service::SettingsService::handle`] for callers.
//! 2. A device's settings: [`service::SettingsHandle::effective`] (an
//!    `Arc<`[`DeviceSettings`]`>`), or [`service::SettingsHandle::watch_device`] to follow
//!    them. After admission creates or changes a device row,
//!    [`service::SettingsHandle::refresh_device`].
//! 3. `session.ready`: [`service::SettingsSnapshot::ready_grant`] on the current snapshot,
//!    or [`ready::compose_ready`] directly. Graph and sidecar files to serve:
//!    [`registry::WakeRegistry::asset_path`] on the snapshot's registry.
//! 4. Dashboard edits: [`edit::SettingsPatch::from_value`] or
//!    [`edit::DeviceEdit::from_value`] to parse, then
//!    [`service::SettingsHandle::apply_fleet_edit`] or
//!    [`service::SettingsHandle::apply_device_edit`]. Wake models:
//!    [`service::SettingsHandle::add_wake_model`] and
//!    [`service::SettingsHandle::delete_wake_model`].
//! 5. A control's capability conditions: [`SettingKey::requirements`] and
//!    [`SettingKey::unmet`]; the reason shown for a gap is the caller's.

#![warn(missing_docs)]

pub mod db;
pub mod edit;
pub mod json;
pub mod ready;
pub mod registry;
pub mod service;
pub mod settings;
pub mod stored;
pub mod values;

pub use settings::{
    DeviceSettings, Requirement, Scope, Section, SectionSet, SettingKey, SettingValue,
};
