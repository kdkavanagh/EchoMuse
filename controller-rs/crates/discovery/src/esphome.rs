//! The ESPHome identities and `_esphomelib._tcp` records for each device
//! (spec §3.3.1 and §3.3.2).
//!
//! Home Assistant keys its device registry on the MAC address and names the
//! node after the record's host (`{node}.local.`), so both are derived from the
//! device's ID alone and must never change.

use std::fmt;
use std::net::Ipv4Addr;
use std::num::NonZeroU16;

use echomuse_core::types::DeviceId;

use crate::records::{ESPHOME_SERVICE_TYPE, ServiceRecord};

/// The two ESPHome nodes a device can have.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EsphomeNodeKind {
    /// The voice satellite every approved device has.
    VoiceSatellite,
    /// The optional Bluetooth proxy of a Dot.
    BluetoothProxy,
}

/// An ESPHome node name: the mDNS instance, the host label and the name the
/// node reports in its hello.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NodeName(String);

impl NodeName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NodeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The last `count` characters of `text`.
fn last_chars(text: &str, count: usize) -> &str {
    let start = text
        .char_indices()
        .rev()
        .nth(count.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    if count == 0 { "" } else { &text[start..] }
}

/// The voice satellite's node name: `echomuse-` and the last 12 characters
/// of the device ID, lowercased.
pub fn voice_node_name(device: &DeviceId) -> NodeName {
    NodeName(format!(
        "echomuse-{}",
        last_chars(&device.0, 12).to_lowercase()
    ))
}

/// The Bluetooth proxy's node name: the voice node name with `-bt`.
pub fn bluetooth_proxy_node_name(device: &DeviceId) -> NodeName {
    NodeName(format!("{}-bt", voice_node_name(device)))
}

/// A 48-bit MAC address. It displays as `AA:BB:CC:DD:EE:FF`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MacAddress(pub [u8; 6]);

impl MacAddress {
    /// The form of the ESPHome `mac` TXT property: 12 lowercase hex digits.
    pub fn txt_form(&self) -> String {
        self.0.iter().map(|octet| format!("{octet:02x}")).collect()
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, octet) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(":")?;
            }
            write!(f, "{octet:02X}")?;
        }
        Ok(())
    }
}

/// The voice satellite's MAC: the hex digits of the device ID, the last 12 of
/// them, left-padded with `0`.
pub fn voice_mac(device: &DeviceId) -> MacAddress {
    let digits: Vec<u8> = device
        .0
        .chars()
        .filter_map(|c| c.to_digit(16))
        .map(|digit| digit as u8)
        .collect();
    let kept = &digits[digits.len().saturating_sub(12)..];
    let mut padded = [0u8; 12];
    padded[12 - kept.len()..].copy_from_slice(kept);
    let mut octets = [0u8; 6];
    let (pairs, _) = padded.as_chunks::<2>();
    for (octet, [high, low]) in octets.iter_mut().zip(pairs) {
        *octet = (high << 4) | low;
    }
    MacAddress(octets)
}

/// The Bluetooth proxy's MAC: the voice MAC with the locally administered bit
/// of the first octet flipped, so it always differs from the voice MAC.
pub fn bluetooth_proxy_mac(device: &DeviceId) -> MacAddress {
    let mut mac = voice_mac(device);
    mac.0[0] ^= 0x02;
    mac
}

/// The voice satellite's friendly name for a device label.
pub fn voice_friendly_name(label: &str) -> String {
    format!("{label} Voice Assistant")
}

/// The ESPHome `project_name` for a device model.
pub fn project_name(model: &str) -> String {
    format!("EchoMuse.{model}")
}

/// One ESPHome node to advertise.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EsphomeAdvertisement {
    /// The device the node belongs to.
    pub device: DeviceId,
    /// Which of the device's nodes this is.
    pub kind: EsphomeNodeKind,
    /// The name Home Assistant shows when it discovers the node.
    pub friendly_name: String,
    /// The node's ESPHome API port.
    pub port: NonZeroU16,
    /// The `project_name` property, from [`project_name`].
    pub project_name: String,
    /// The project version (`ESPHOME_PROJECT_VERSION`).
    pub project_version: String,
}

impl EsphomeAdvertisement {
    /// The node name for this device and kind.
    pub fn node_name(&self) -> NodeName {
        match self.kind {
            EsphomeNodeKind::VoiceSatellite => voice_node_name(&self.device),
            EsphomeNodeKind::BluetoothProxy => bluetooth_proxy_node_name(&self.device),
        }
    }

    /// The MAC address for this device and kind.
    pub fn mac(&self) -> MacAddress {
        match self.kind {
            EsphomeNodeKind::VoiceSatellite => voice_mac(&self.device),
            EsphomeNodeKind::BluetoothProxy => bluetooth_proxy_mac(&self.device),
        }
    }

    /// The `_esphomelib._tcp` record at `address`: instance and host from the
    /// node name, and TXT `version`, `friendly_name`, `mac`, `network=ethwifi`,
    /// `project_name` and `project_version`. `version` carries the project
    /// version, the only version this node has.
    pub fn record(&self, address: Ipv4Addr) -> ServiceRecord {
        let node = self.node_name();
        ServiceRecord {
            service_type: ESPHOME_SERVICE_TYPE,
            host: format!("{node}.local."),
            instance: node.0,
            address,
            port: self.port.get(),
            txt: vec![
                ("version", self.project_version.clone()),
                ("friendly_name", self.friendly_name.clone()),
                ("mac", self.mac().txt_form()),
                ("network", "ethwifi".to_owned()),
                ("project_name", self.project_name.clone()),
                ("project_version", self.project_version.clone()),
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(text: &str) -> DeviceId {
        DeviceId(text.to_owned())
    }

    #[test]
    fn node_names_use_the_last_twelve_characters_lowercased() {
        assert_eq!(
            voice_node_name(&id("G090LF1172340ABC")).as_str(),
            "echomuse-lf1172340abc"
        );
        assert_eq!(
            bluetooth_proxy_node_name(&id("G090LF1172340ABC")).as_str(),
            "echomuse-lf1172340abc-bt"
        );
        assert_eq!(voice_node_name(&id("ABC123")).as_str(), "echomuse-abc123");
        assert_eq!(voice_node_name(&id("")).as_str(), "echomuse-");
        // Characters, not bytes, are counted.
        assert_eq!(
            voice_node_name(&id("xxÄBCDEFGHIJKL")).as_str(),
            "echomuse-äbcdefghijkl"
        );
    }

    #[test]
    fn mac_uses_the_last_twelve_hex_digits() {
        // Non-hex characters are skipped before the last 12 digits are taken.
        // G090LF1172340ABC has the hex digits 090F1172340ABC; the last 12 are kept.
        assert_eq!(
            voice_mac(&id("G090LF1172340ABC")).to_string(),
            "0F:11:72:34:0A:BC"
        );
        assert_eq!(
            voice_mac(&id("g090lf1172340abc")).txt_form(),
            "0f1172340abc"
        );
        assert_eq!(
            voice_mac(&id("00:1a:2B:3c:4D:5e")).to_string(),
            "00:1A:2B:3C:4D:5E"
        );
    }

    #[test]
    fn short_ids_are_left_padded_with_zeros() {
        assert_eq!(voice_mac(&id("ABC")).to_string(), "00:00:00:00:0A:BC");
        assert_eq!(voice_mac(&id("zz")).to_string(), "00:00:00:00:00:00");
        assert_eq!(voice_mac(&id("")).txt_form(), "000000000000");
    }

    #[test]
    fn the_proxy_mac_flips_bit_one_of_the_first_octet() {
        assert_eq!(
            bluetooth_proxy_mac(&id("G090LF1172340ABC")).to_string(),
            "0D:11:72:34:0A:BC"
        );
        assert_eq!(
            bluetooth_proxy_mac(&id("FEDCBA987654")).to_string(),
            "FC:DC:BA:98:76:54"
        );
        for device in ["", "1", "0200000000", "FFFFFFFFFFFF"] {
            assert_ne!(voice_mac(&id(device)), bluetooth_proxy_mac(&id(device)));
        }
    }

    #[test]
    fn voice_record_matches_the_esphome_contract() {
        let advert = EsphomeAdvertisement {
            device: id("G090LF1172340ABC"),
            kind: EsphomeNodeKind::VoiceSatellite,
            friendly_name: voice_friendly_name("Kitchen"),
            port: NonZeroU16::new(16001).unwrap(),
            project_name: project_name("Echo Dot Gen 2"),
            project_version: "2.0.0".to_owned(),
        };
        let record = advert.record(Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(
            record.fullname(),
            "echomuse-lf1172340abc._esphomelib._tcp.local."
        );
        // Home Assistant names the node after the host label.
        assert_eq!(record.host, "echomuse-lf1172340abc.local.");
        assert_eq!(record.address, Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(record.port, 16001);
        let txt: Vec<(&str, &str)> = record.txt.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(
            txt,
            [
                ("version", "2.0.0"),
                ("friendly_name", "Kitchen Voice Assistant"),
                ("mac", "0f1172340abc"),
                ("network", "ethwifi"),
                ("project_name", "EchoMuse.Echo Dot Gen 2"),
                ("project_version", "2.0.0"),
            ]
        );
        assert!(record.txt_value("api_encryption").is_none());
        record.to_service_info().unwrap();
    }

    #[test]
    fn proxy_record_uses_the_proxy_identity() {
        let advert = EsphomeAdvertisement {
            device: id("G090LF1172340ABC"),
            kind: EsphomeNodeKind::BluetoothProxy,
            friendly_name: "Kitchen Bluetooth Proxy".to_owned(),
            port: NonZeroU16::new(17001).unwrap(),
            project_name: project_name("Echo Dot Gen 2"),
            project_version: "2.0.0".to_owned(),
        };
        let record = advert.record(Ipv4Addr::new(10, 0, 0, 2));
        assert_eq!(record.instance, "echomuse-lf1172340abc-bt");
        assert_eq!(record.host, "echomuse-lf1172340abc-bt.local.");
        assert_eq!(record.txt_value("mac"), Some("0d1172340abc"));
        assert_eq!(record.port, 17001);
    }
}
