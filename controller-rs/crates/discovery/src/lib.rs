//! mDNS advertisement: the controller service devices browse for, and the
//! ESPHome nodes Home Assistant discovers.
//!
//! [`records`] and [`esphome`] build the records as plain values; [`service`]
//! publishes them through one task that owns the mDNS responder.

pub mod esphome;
pub mod records;
pub mod service;

pub use esphome::{
    EsphomeAdvertisement, EsphomeNodeKind, MacAddress, NodeName, bluetooth_proxy_mac,
    bluetooth_proxy_node_name, project_name, voice_friendly_name, voice_mac, voice_node_name,
};
pub use records::{
    CONTROLLER_SERVICE_TYPE, ControllerAdvertisement, ESPHOME_SERVICE_TYPE, ServiceRecord,
};
pub use service::{
    DiscoveryError, DiscoveryHandle, DiscoveryHealth, REANNOUNCE_INTERVAL, Withdrawn, start,
};
