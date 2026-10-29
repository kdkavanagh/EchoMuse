//! Controller configuration from the environment and the Home Assistant add-on
//! options, the address advertised to devices, and the TLS credentials devices
//! pin.
//!
//! Start-up order for the binary:
//! 1. collect the process environment into an [`Environment`];
//! 2. merge `/data/options.json` with [`addon::merge_options_file`] (a no-op
//!    outside the add-on);
//! 3. build [`ControllerConfig::from_env`];
//! 4. resolve [`NetworkConfig::advertised_address`];
//! 5. when the TLS listener is enabled, load [`tls::DeviceLinkTls`] from
//!    [`PathsConfig::tls_dir`].

pub mod addon;
pub mod network;
pub mod settings;
pub mod tls;

pub use network::{AddressSource, AdvertisedAddress, DefaultRouteError, default_route_ipv4};
pub use settings::{
    ConfigError, ConfigErrors, ConfigProblem, ControllerConfig, DeviceApproval, EnvVar,
    Environment, HaBaseUrl, HomeAssistantAccess, MdnsName, MdnsNameError, NetworkConfig,
    PathsConfig,
};
pub use tls::{DeviceLinkTls, TLS_SERVER_NAME, TlsError};
