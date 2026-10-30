//! Who each node says it is (spec §3.3.1 and §3.3.2).
//!
//! Home Assistant keys its device on the MAC and checks the node name
//! against the one stored in its config entry, so both come from the
//! device ID through the discovery crate's functions and never change.
//! Entities are keyed on the MAC, the entity type and the entity name, so
//! the names in [`crate::entities`] are just as fixed.

use echomuse_core::types::DeviceId;
use echomuse_discovery::{
    MacAddress, NodeName, bluetooth_proxy_mac, bluetooth_proxy_node_name, project_name,
    voice_friendly_name, voice_mac, voice_node_name,
};
use echomuse_protocol::Platform;

use crate::api::{DeviceInfoResponse, HelloResponse};

/// The `manufacturer` every node reports.
pub const MANUFACTURER: &str = "EchoMuse";

/// The API version the nodes speak. Clients older than 1.11 derive a media
/// player's features from `supports_pause` instead of its feature flags, and
/// from 1.10 on read the voice-assistant feature flags directly.
pub const API_VERSION: (u32, u32) = (1, 10);

/// What the hello's `server_info` says; clients only log it.
pub const SERVER_INFO: &str = "EchoMuse";

/// Voice-assistant feature flags of the satellite: voice assistant (1), audio
/// over the API (4), timers (8), announcements (16) and start conversation
/// (32). It does not claim a speaker (2): responses play through the media
/// player.
pub const VOICE_ASSISTANT_FEATURE_FLAGS: u32 = 1 | 4 | 8 | 16 | 32;

/// Bluetooth proxy feature flags of the proxy: passive scanning (1) and raw
/// advertisements (32).
pub const BLUETOOTH_PROXY_FEATURE_FLAGS: u32 = 1 | 32;

/// The `model` each platform reports, which also names the project.
pub fn model(platform: Platform) -> &'static str {
    match platform {
        Platform::FireOs6 => "Echo Dot Gen 2 (biscuit)",
        Platform::FireOs6Crown => "Echo Show 8 Gen 1 (crown)",
        Platform::Lineage18Crown => "Echo Show 8 Gen 1 (crown, LineageOS)",
    }
}

/// The Bluetooth proxy's friendly name for a device label.
pub fn bluetooth_proxy_friendly_name(label: &str) -> String {
    format!("{label} BT Proxy")
}

/// The identity one node presents in its hello and device info.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeIdentity {
    /// The node name: the hello's and device info's `name`.
    pub name: NodeName,
    /// The MAC Home Assistant keys the device on.
    pub mac: MacAddress,
    /// The name Home Assistant gives the device.
    pub friendly_name: String,
    /// The hardware model.
    pub model: &'static str,
    /// `EchoMuse.{model}`.
    pub project_name: String,
    /// `ESPHOME_PROJECT_VERSION`.
    pub project_version: String,
    /// Voice-assistant feature flags; 0 on the proxy.
    pub voice_assistant_feature_flags: u32,
    /// Bluetooth proxy feature flags; 0 on the satellite.
    pub bluetooth_proxy_feature_flags: u32,
}

impl NodeIdentity {
    /// The voice satellite every approved device has.
    pub fn voice_satellite(
        device: &DeviceId,
        label: &str,
        platform: Platform,
        project_version: &str,
    ) -> Self {
        let model = model(platform);
        Self {
            name: voice_node_name(device),
            mac: voice_mac(device),
            friendly_name: voice_friendly_name(label),
            model,
            project_name: project_name(model),
            project_version: project_version.to_owned(),
            voice_assistant_feature_flags: VOICE_ASSISTANT_FEATURE_FLAGS,
            bluetooth_proxy_feature_flags: 0,
        }
    }

    /// A Dot's Bluetooth proxy.
    pub fn bluetooth_proxy(
        device: &DeviceId,
        label: &str,
        platform: Platform,
        project_version: &str,
    ) -> Self {
        let model = model(platform);
        Self {
            name: bluetooth_proxy_node_name(device),
            mac: bluetooth_proxy_mac(device),
            friendly_name: bluetooth_proxy_friendly_name(label),
            model,
            project_name: project_name(model),
            project_version: project_version.to_owned(),
            voice_assistant_feature_flags: 0,
            bluetooth_proxy_feature_flags: BLUETOOTH_PROXY_FEATURE_FLAGS,
        }
    }

    /// The answer to a client's hello.
    pub fn hello_response(&self) -> HelloResponse {
        HelloResponse {
            api_version_major: API_VERSION.0,
            api_version_minor: API_VERSION.1,
            server_info: SERVER_INFO.to_owned(),
            name: self.name.to_string(),
        }
    }

    /// The answer to a device-info request. ESPHome's own version and build
    /// time are empty: this is not ESPHome firmware.
    pub fn device_info(&self) -> DeviceInfoResponse {
        DeviceInfoResponse {
            name: self.name.to_string(),
            mac_address: self.mac.to_string(),
            model: self.model.to_owned(),
            project_name: self.project_name.clone(),
            project_version: self.project_version.clone(),
            manufacturer: MANUFACTURER.to_owned(),
            friendly_name: self.friendly_name.clone(),
            voice_assistant_feature_flags: self.voice_assistant_feature_flags,
            bluetooth_proxy_feature_flags: self.bluetooth_proxy_feature_flags,
            ..DeviceInfoResponse::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_satellite_identity_follows_the_device_id_and_label() {
        let device = DeviceId("G090LF0965260F1J".to_owned());
        let identity =
            NodeIdentity::voice_satellite(&device, "Kitchen", Platform::FireOs6, "2.0.0");
        let info = identity.device_info();
        assert_eq!(info.name, "echomuse-lf0965260f1j");
        assert_eq!(info.mac_address, "90:F0:96:52:60:F1");
        assert_eq!(info.friendly_name, "Kitchen Voice Assistant");
        assert_eq!(info.model, "Echo Dot Gen 2 (biscuit)");
        assert_eq!(info.project_name, "EchoMuse.Echo Dot Gen 2 (biscuit)");
        assert_eq!(info.manufacturer, "EchoMuse");
        assert_eq!(info.voice_assistant_feature_flags, 61);
        assert_eq!(info.bluetooth_proxy_feature_flags, 0);
        let hello = identity.hello_response();
        assert_eq!((hello.api_version_major, hello.api_version_minor), (1, 10));
        assert_eq!(hello.name, "echomuse-lf0965260f1j");
    }

    #[test]
    fn the_proxy_identity_is_distinct() {
        let device = DeviceId("G090LF0965260F1J".to_owned());
        let identity =
            NodeIdentity::bluetooth_proxy(&device, "Kitchen", Platform::FireOs6, "2.0.0");
        let info = identity.device_info();
        assert_eq!(info.name, "echomuse-lf0965260f1j-bt");
        assert_eq!(info.mac_address, "92:F0:96:52:60:F1");
        assert_eq!(info.friendly_name, "Kitchen BT Proxy");
        assert_eq!(info.bluetooth_proxy_feature_flags, 33);
        assert_eq!(info.voice_assistant_feature_flags, 0);
    }

    #[test]
    fn each_platform_has_its_model() {
        assert_eq!(
            model(Platform::Lineage18Crown),
            "Echo Show 8 Gen 1 (crown, LineageOS)"
        );
        assert_eq!(model(Platform::FireOs6Crown), "Echo Show 8 Gen 1 (crown)");
    }
}
