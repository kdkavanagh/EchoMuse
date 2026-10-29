//! Link token rules (spec §3.1.2).

use std::fmt;

use echomuse_db::devices::Secret;
use http::HeaderMap;
use subtle::ConstantTimeEq;

/// The header every device socket carries its link token in.
pub const HEADER_TOKEN: &str = "x-em-token";
/// The header audio and asset sockets name their session in.
pub const HEADER_SESSION: &str = "x-em-session";

/// Which listener a socket arrived on.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Transport {
    /// The plain WebSocket listener (`SERVER_PORT`).
    Plain,
    /// The TLS listener (`SERVER_TLS_PORT`).
    Tls,
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Plain => "ws",
            Self::Tls => "wss",
        })
    }
}

/// The `X-EM-Token` a socket presented, as raw header bytes. Never shown in `Debug`.
#[derive(Clone)]
pub(crate) struct PresentedToken(Vec<u8>);

impl PresentedToken {
    pub(crate) fn from_headers(headers: &HeaderMap) -> Option<Self> {
        headers
            .get(HEADER_TOKEN)
            .map(|value| Self(value.as_bytes().to_vec()))
    }
}

impl fmt::Debug for PresentedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PresentedToken(..)")
    }
}

/// Why a socket's token was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TokenRefusal {
    /// A token was presented and the device holds a different one.
    Mismatch,
    /// `REQUIRE_DEVICE_TLS` is set and the socket is not TLS.
    NotTls,
    /// `REQUIRE_DEVICE_TLS` is set and the socket presented no token.
    Missing,
    /// `REQUIRE_DEVICE_TLS` is set and the device holds no token to match.
    NoStoredToken,
}

impl fmt::Display for TokenRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Mismatch => "the presented token does not match the stored one",
            Self::NotTls => "REQUIRE_DEVICE_TLS refuses an unencrypted socket",
            Self::Missing => "REQUIRE_DEVICE_TLS refuses a socket without a token",
            Self::NoStoredToken => "REQUIRE_DEVICE_TLS refuses a device that holds no token",
        })
    }
}

/// Applies the admission rules to one socket:
///
/// - with `REQUIRE_DEVICE_TLS`, the socket must be TLS and present the stored token;
/// - otherwise a presented token must equal the stored one, a missing token is allowed,
///   and a token presented by a device with no stored token (a deleted device) is
///   ignored.
///
/// The comparison takes the same time wherever the two tokens differ.
pub(crate) fn authorize(
    presented: Option<&PresentedToken>,
    stored: Option<&Secret>,
    transport: Transport,
    require_tls: bool,
) -> Result<(), TokenRefusal> {
    let matches = |presented: &PresentedToken, stored: &Secret| -> bool {
        bool::from(presented.0.as_slice().ct_eq(stored.expose().as_bytes()))
    };
    if require_tls {
        if transport != Transport::Tls {
            return Err(TokenRefusal::NotTls);
        }
        let presented = presented.ok_or(TokenRefusal::Missing)?;
        let stored = stored.ok_or(TokenRefusal::NoStoredToken)?;
        return if matches(presented, stored) {
            Ok(())
        } else {
            Err(TokenRefusal::Mismatch)
        };
    }
    match (presented, stored) {
        (Some(presented), Some(stored)) if !matches(presented, stored) => {
            Err(TokenRefusal::Mismatch)
        }
        _ => Ok(()),
    }
}
