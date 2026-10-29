//! The integrator's side of the link: deciding what an admitted session is granted,
//! and finding asset content.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_db::devices::DeviceRow;
use echomuse_protocol::session::{ReadyGrant, SessionHello};
use echomuse_protocol::{LinkSessionId, Platform, Sha256};

use crate::auth::Transport;
use crate::session::LinkSession;

/// A hello that passed every link check: protocol, token and approval.
#[derive(Clone, Debug)]
pub struct AdmissionRequest {
    /// The device (`ro.serialno`).
    pub device: DeviceId,
    /// The id `session.ready` will carry.
    pub session_id: LinkSessionId,
    /// Which listener the control socket arrived on.
    pub transport: Transport,
    /// The control socket's peer address.
    pub peer: SocketAddr,
    /// The hello. Its privacy state must be installed before capture is accepted.
    pub hello: SessionHello,
    /// The supported platform the hello named.
    pub platform: Platform,
    /// The device's row after this attempt was recorded (and the device approved, when
    /// the policy is `auto`).
    pub device_row: DeviceRow,
}

/// The integrator's answer to an [`AdmissionRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum AdmissionDecision {
    /// Admit: write `session.ready` with this grant. The [`LinkSession`] given with the
    /// request starts delivering once it is written.
    Ready(ReadyGrant),
    /// Do not admit, for example because the device's runtime is not ready. The control
    /// socket is closed without a reply (protocol v1 has no rejection reason for this)
    /// and the device redials; the diagnostic is logged and becomes the session's
    /// [`CloseReason::Declined`](crate::CloseReason::Declined).
    Decline(String),
}

/// Decides what an admitted session is granted. The link calls it once per admitted
/// hello, after ending the device's previous session, and bounds it by the time left
/// before the device stops waiting for `session.ready`.
///
/// The implementation attaches the session to the device runtime (which serializes
/// admission per device and installs the hello's privacy state), keeps the
/// [`LinkSession`], and returns the grant. Dropping the `LinkSession` declines in
/// effect: the session ends as soon as it would start.
pub trait Admission: Send + Sync + 'static {
    /// Admits or declines one session.
    fn admit(
        &self,
        request: AdmissionRequest,
        session: LinkSession,
    ) -> impl Future<Output = AdmissionDecision> + Send;
}

/// Finds asset content for the asset socket (spec §3.1.9).
pub trait AssetSource: Send + Sync + 'static {
    /// The bytes stored under `sha256`; `None` when the controller has no such asset.
    /// The link checks that they hash to `sha256` before serving any of them, and
    /// answers `not_found` when they do not. An error closes the asset socket, and the
    /// device redials and asks again.
    fn load(&self, sha256: Sha256) -> impl Future<Output = io::Result<Option<Bytes>>> + Send;
}

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [`Admission`] behind a pointer, so the server is not generic over it.
pub(crate) trait DynAdmission: Send + Sync {
    fn admit(
        &self,
        request: AdmissionRequest,
        session: LinkSession,
    ) -> BoxFuture<'_, AdmissionDecision>;
}

impl<A: Admission> DynAdmission for A {
    fn admit(
        &self,
        request: AdmissionRequest,
        session: LinkSession,
    ) -> BoxFuture<'_, AdmissionDecision> {
        Box::pin(Admission::admit(self, request, session))
    }
}

/// [`AssetSource`] behind a pointer.
pub(crate) trait DynAssets: Send + Sync {
    fn load(&self, sha256: Sha256) -> BoxFuture<'_, io::Result<Option<Bytes>>>;
}

impl<S: AssetSource> DynAssets for S {
    fn load(&self, sha256: Sha256) -> BoxFuture<'_, io::Result<Option<Bytes>>> {
        Box::pin(AssetSource::load(self, sha256))
    }
}
