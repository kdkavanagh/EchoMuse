//! The IPv4 address the controller advertises over mDNS.

use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

use thiserror::Error;

use crate::settings::NetworkConfig;

/// Where the advertised address came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSource {
    /// `SERVER_IP` named it.
    Configured,
    /// `SERVER_IP` was unset and the default route's interface supplied it.
    /// The caller should warn that devices may need `SERVER_IP` set.
    DefaultRoute,
}

/// The address devices are told to connect to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvertisedAddress {
    /// The IPv4 address.
    pub ip: Ipv4Addr,
    /// Where it came from.
    pub source: AddressSource,
}

/// Why no default route address could be found.
#[derive(Debug, Error)]
pub enum DefaultRouteError {
    /// The operating system has no IPv4 route to pick an interface from.
    #[error("SERVER_IP is not set and there is no IPv4 default route to detect it from: {0}")]
    NoRoute(#[source] io::Error),
    /// The route selected an address devices cannot connect to.
    #[error("SERVER_IP is not set and the default route gave the unusable address {0}")]
    Unusable(Ipv4Addr),
}

/// An address in TEST-NET-1 (RFC 5737), which no real host uses. Connecting a
/// UDP socket to it makes the kernel pick the outgoing interface for the
/// default route without sending anything.
const ROUTE_PROBE: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9);

/// The IPv4 address of the interface that holds the default route.
pub fn default_route_ipv4() -> Result<Ipv4Addr, DefaultRouteError> {
    let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))
        .map_err(DefaultRouteError::NoRoute)?;
    socket
        .connect(ROUTE_PROBE)
        .map_err(DefaultRouteError::NoRoute)?;
    let local = socket.local_addr().map_err(DefaultRouteError::NoRoute)?;
    match local.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Ok(ip),
        std::net::IpAddr::V4(ip) => Err(DefaultRouteError::Unusable(ip)),
        // A socket bound to an IPv4 address cannot report an IPv6 one.
        std::net::IpAddr::V6(_) => Err(DefaultRouteError::Unusable(Ipv4Addr::UNSPECIFIED)),
    }
}

impl NetworkConfig {
    /// `SERVER_IP` when set, otherwise the default route's address.
    pub fn advertised_address(&self) -> Result<AdvertisedAddress, DefaultRouteError> {
        match self.server_ip {
            Some(ip) => Ok(AdvertisedAddress {
                ip,
                source: AddressSource::Configured,
            }),
            None => default_route_ipv4().map(|ip| AdvertisedAddress {
                ip,
                source: AddressSource::DefaultRoute,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use super::*;
    use crate::settings::MdnsName;

    fn network(server_ip: Option<Ipv4Addr>) -> NetworkConfig {
        NetworkConfig {
            server_host: "0.0.0.0".into(),
            server_port: NonZeroU16::new(8767).unwrap(),
            server_tls_port: None,
            api_port: NonZeroU16::new(8768).unwrap(),
            server_ip,
            mdns_name: MdnsName::new("echomuse").unwrap(),
            require_device_tls: false,
        }
    }

    #[test]
    fn a_configured_address_is_used_without_probing() {
        let ip = Ipv4Addr::new(10, 1, 2, 3);
        assert_eq!(
            network(Some(ip)).advertised_address().unwrap(),
            AdvertisedAddress {
                ip,
                source: AddressSource::Configured
            }
        );
    }

    #[test]
    fn detection_yields_a_usable_address_or_a_typed_error() {
        // The sandbox may have no IPv4 route; both outcomes are valid, but a
        // success must never be the unspecified address or a non-route source.
        match network(None).advertised_address() {
            Ok(address) => {
                assert_eq!(address.source, AddressSource::DefaultRoute);
                assert!(!address.ip.is_unspecified());
            }
            Err(DefaultRouteError::NoRoute(_) | DefaultRouteError::Unusable(_)) => {}
        }
    }
}
