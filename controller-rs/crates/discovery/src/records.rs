//! The mDNS records the controller publishes, built as plain values.

use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroU16;

use echomuse_config::MdnsName;
use mdns_sd::ServiceInfo;

/// The service devices browse for (`device/internal/discovery/mdns.go`).
pub const CONTROLLER_SERVICE_TYPE: &str = "_emcontroller._tcp.local.";

/// The service Home Assistant's ESPHome integration discovers.
pub const ESPHOME_SERVICE_TYPE: &str = "_esphomelib._tcp.local.";

/// One DNS-SD service: its instance, host, single IPv4 address, port and TXT
/// properties in publication order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRecord {
    /// The service type, ending in `._tcp.local.`.
    pub service_type: &'static str,
    /// The instance label.
    pub instance: String,
    /// The host name, ending in `.local.`.
    pub host: String,
    /// The only address published; records are IPv4 only.
    pub address: Ipv4Addr,
    /// The service port.
    pub port: u16,
    /// TXT properties as key and value.
    pub txt: Vec<(&'static str, String)>,
}

impl ServiceRecord {
    /// The instance's full DNS-SD name, `{instance}.{service_type}`.
    pub fn fullname(&self) -> String {
        format!("{}.{}", self.instance, self.service_type)
    }

    /// The TXT value for `key`.
    pub fn txt_value(&self, key: &str) -> Option<&str> {
        self.txt
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value.as_str())
    }

    /// The record in the responder's form.
    pub(crate) fn to_service_info(&self) -> Result<ServiceInfo, mdns_sd::Error> {
        let txt: Vec<(&str, &str)> = self.txt.iter().map(|(k, v)| (*k, v.as_str())).collect();
        ServiceInfo::new(
            self.service_type,
            &self.instance,
            &self.host,
            IpAddr::V4(self.address),
            self.port,
            txt.as_slice(),
        )
    }
}

/// What the controller advertises to devices (spec §3.1.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerAdvertisement {
    /// The instance and host label (`MDNS_NAME`).
    pub name: MdnsName,
    /// The address devices connect to.
    pub address: Ipv4Addr,
    /// The plain device listener (`SERVER_PORT`).
    pub port: NonZeroU16,
    /// The TLS device listener, set only while it is actually listening.
    pub tls_port: Option<NonZeroU16>,
}

impl ControllerAdvertisement {
    /// The `_emcontroller._tcp` record: instance `{name}`, host
    /// `{name}.local.`, TXT `version=1`, `server={name}` and, while TLS is
    /// active, `tls_port`.
    pub fn record(&self) -> ServiceRecord {
        let mut txt = vec![
            ("version", "1".to_owned()),
            ("server", self.name.to_string()),
        ];
        if let Some(tls_port) = self.tls_port {
            txt.push(("tls_port", tls_port.to_string()));
        }
        ServiceRecord {
            service_type: CONTROLLER_SERVICE_TYPE,
            instance: self.name.to_string(),
            host: format!("{}.local.", self.name),
            address: self.address,
            port: self.port.get(),
            txt,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn advert(tls_port: Option<u16>) -> ControllerAdvertisement {
        ControllerAdvertisement {
            name: MdnsName::new("echomuse").unwrap(),
            address: Ipv4Addr::new(192, 168, 1, 20),
            port: NonZeroU16::new(8767).unwrap(),
            tls_port: tls_port.and_then(NonZeroU16::new),
        }
    }

    /// The firmware's reading of `tls_port` (`parseTLSPort` in
    /// `device/internal/discovery/mdns.go`): the first `tls_port=` entry whose
    /// value parses as 1..65535, else 0.
    fn device_tls_port(txt: &[String]) -> u16 {
        txt.iter()
            .filter_map(|entry| entry.strip_prefix("tls_port="))
            .find_map(|value| value.parse::<u16>().ok().filter(|port| *port > 0))
            .unwrap_or(0)
    }

    fn txt_strings(record: &ServiceRecord) -> Vec<String> {
        record.txt.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    #[test]
    fn controller_record_without_tls() {
        let record = advert(None).record();
        assert_eq!(record.fullname(), "echomuse._emcontroller._tcp.local.");
        assert_eq!(record.host, "echomuse.local.");
        assert_eq!(record.address, Ipv4Addr::new(192, 168, 1, 20));
        assert_eq!(record.port, 8767);
        assert_eq!(txt_strings(&record), ["version=1", "server=echomuse"]);
        assert_eq!(device_tls_port(&txt_strings(&record)), 0);
    }

    #[test]
    fn controller_record_with_tls() {
        let record = advert(Some(8770)).record();
        assert_eq!(
            txt_strings(&record),
            ["version=1", "server=echomuse", "tls_port=8770"]
        );
        assert_eq!(device_tls_port(&txt_strings(&record)), 8770);
    }

    #[test]
    fn the_responder_form_keeps_names_address_port_and_txt() {
        let info = advert(Some(8770)).record().to_service_info().unwrap();
        assert_eq!(info.get_fullname(), "echomuse._emcontroller._tcp.local.");
        assert_eq!(info.get_type(), CONTROLLER_SERVICE_TYPE);
        assert_eq!(info.get_hostname(), "echomuse.local.");
        assert_eq!(info.get_port(), 8767);
        let v4: Vec<Ipv4Addr> = info.get_addresses_v4().into_iter().copied().collect();
        assert_eq!(v4, [Ipv4Addr::new(192, 168, 1, 20)]);
        assert_eq!(info.get_addresses().len(), 1);
        assert_eq!(info.get_property_val_str("version"), Some("1"));
        assert_eq!(info.get_property_val_str("server"), Some("echomuse"));
        assert_eq!(info.get_property_val_str("tls_port"), Some("8770"));
        assert_eq!(info.get_properties().len(), 3);
    }
}
