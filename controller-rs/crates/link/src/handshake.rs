//! A control socket from upgrade to `session.ready` or `session.rejected` (spec
//! §3.1.2, §3.1.4, §5.7 step 1).
//!
//! In order: the hello must arrive within 10 s; every attempt that names a device is
//! recorded; then the protocol check (required capabilities, platform), the token rules
//! and approval decide between `session.rejected` reasons `protocol`, `unauthorized`
//! and `pending_approval`. An admitted hello first ends the device's previous session,
//! then asks the integrator's [`Admission`](crate::Admission) for the grant that
//! `session.ready` carries.

use std::net::SocketAddr;
use std::sync::Arc;

use echomuse_core::types::DeviceId;
use echomuse_db::devices::{
    ApprovalMode, ApproveDevice, DeviceRow, ReadApprovalMode, RecordConnectionAttempt,
};
use echomuse_protocol::envelope::{ControlFrame, HelloRefused, HelloRequest};
use echomuse_protocol::session::{SessionReady, SessionRejected, check_admission};
use echomuse_protocol::{RejectReason, encode_envelope, parse_hello};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout, timeout_at};
use tokio_tungstenite::tungstenite::Message;

use crate::admission::{AdmissionDecision, AdmissionRequest};
use crate::auth::{PresentedToken, Transport, authorize};
use crate::config::utc_ms;
use crate::control::{Ws, WsSink, WsStream, new_session};
use crate::server::Shared;
use crate::session::{CloseReason, SessionCore};

/// What the checks before admission decided.
enum Verdict {
    Admit(Box<(HelloRequest, DeviceRow)>),
    Reject {
        device: DeviceId,
        reason: RejectReason,
    },
    /// Close without a reply: the frame named no device, or the controller could not
    /// decide (a database failure). The device redials.
    Close,
}

/// Serves one control socket for its whole life.
pub(crate) async fn serve_control(
    shared: Arc<Shared>,
    ws: Ws,
    transport: Transport,
    peer: SocketAddr,
    token: Option<PresentedToken>,
) {
    let timings = shared.config.timings;
    let (mut sink, mut stream) = ws.split();
    let first = tokio::select! {
        _ = shared.shutdown.cancelled() => None,
        first = timeout(timings.hello, next_data(&mut stream)) => first.ok().flatten(),
    };
    let Some(first) = first else {
        close(&mut sink, &shared).await;
        return;
    };
    let deadline = Instant::now() + timings.ready;
    let parsed = match &first {
        Message::Text(text) => parse_hello(ControlFrame::Text(text.as_str())),
        Message::Binary(bytes) => parse_hello(ControlFrame::Binary(bytes)),
        _ => unreachable!("next_data returns data frames"),
    };
    let checks = check_hello(&shared, parsed, transport, peer, token.as_ref());
    let verdict = tokio::select! {
        _ = shared.shutdown.cancelled() => Verdict::Close,
        verdict = timeout_at(deadline, checks) => verdict.unwrap_or_else(|_| {
            tracing::warn!(%peer, "admission checks missed the ready deadline");
            Verdict::Close
        }),
    };
    match verdict {
        Verdict::Close => close(&mut sink, &shared).await,
        Verdict::Reject { device, reason } => {
            if let Ok(rejected) = encode_envelope(&SessionRejected { reason }, None, &device, 0) {
                let _ = timeout(timings.write, sink.send(Message::text(rejected.text))).await;
            }
            close(&mut sink, &shared).await;
        }
        Verdict::Admit(admitted) => {
            let (hello, row) = *admitted;
            admit(&shared, hello, row, transport, peer, deadline, sink, stream).await;
        }
    }
}

/// The next text or binary frame; `None` when the socket closes or fails first.
async fn next_data(stream: &mut WsStream) -> Option<Message> {
    loop {
        match stream.next().await? {
            Ok(message @ (Message::Text(_) | Message::Binary(_))) => return Some(message),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Close(_)) | Err(_) => return None,
        }
    }
}

async fn close(sink: &mut WsSink, shared: &Shared) {
    let _ = timeout(shared.config.timings.write, sink.send(Message::Close(None))).await;
}

async fn check_hello(
    shared: &Shared,
    parsed: Result<HelloRequest, HelloRefused>,
    transport: Transport,
    peer: SocketAddr,
    token: Option<&PresentedToken>,
) -> Verdict {
    let request = match parsed {
        Ok(request) => request,
        Err(HelloRefused {
            device_id: Some(device),
            error,
        }) => {
            tracing::info!(%device, %peer, %error, "refusing a malformed hello");
            // Still an attempt: the provisioning wizard looks for the device's address.
            let _ = record_attempt(shared, &device, peer, None, None).await;
            return Verdict::Reject {
                device,
                reason: RejectReason::Protocol,
            };
        }
        Err(HelloRefused {
            device_id: None,
            error,
        }) => {
            tracing::info!(%peer, %error, "closing a control socket whose first frame names no device");
            return Verdict::Close;
        }
    };
    let device = request.header.device_id.clone();
    let hello = &request.hello;
    let Some(row) = record_attempt(
        shared,
        &device,
        peer,
        hello.firmware_version.clone(),
        hello.os_version.clone(),
    )
    .await
    else {
        return Verdict::Close;
    };
    if let Err(refusal) = check_admission(hello) {
        tracing::info!(%device, %refusal, "refusing a hello");
        return Verdict::Reject {
            device,
            reason: refusal.reason(),
        };
    }
    let require_tls = shared.config.require_device_tls;
    if let Err(refusal) = authorize(token, row.token.as_ref(), transport, require_tls) {
        tracing::info!(%device, %transport, %refusal, "refusing an unauthorized hello");
        return Verdict::Reject {
            device,
            reason: RejectReason::Unauthorized,
        };
    }
    let row = if row.approved {
        row
    } else {
        match approve(shared, row).await {
            Approval::Approved(row) => *row,
            Approval::Pending => {
                return Verdict::Reject {
                    device,
                    reason: RejectReason::PendingApproval,
                };
            }
            Approval::Failed => return Verdict::Close,
        }
    };
    Verdict::Admit(Box::new((request, row)))
}

/// Records one connection attempt; `None` (logged) when the database refused it.
async fn record_attempt(
    shared: &Shared,
    device: &DeviceId,
    peer: SocketAddr,
    firmware_version: Option<String>,
    os_version: Option<String>,
) -> Option<DeviceRow> {
    let command = RecordConnectionAttempt {
        device: device.clone(),
        ip: peer.ip().to_canonical(),
        firmware_version,
        os_version,
        at: shared.config.wall_clock.now(),
    };
    match shared.db.write(command).await {
        Ok(attempt) => Some(attempt.device),
        Err(error) => {
            tracing::warn!(%device, %error, "could not record a connection attempt");
            None
        }
    }
}

enum Approval {
    Approved(Box<DeviceRow>),
    Pending,
    Failed,
}

/// Approves a device that has never been approved when the policy is `auto`, under
/// its label or else `Unknown` plus the first eight characters of its serial.
async fn approve(shared: &Shared, mut row: DeviceRow) -> Approval {
    let query = ReadApprovalMode {
        fallback: shared.config.approval_fallback,
    };
    let mode = match shared.db.read(query).await {
        Ok(mode) => mode,
        Err(error) => {
            tracing::warn!(device = %row.device_id, %error, "could not read the approval mode");
            return Approval::Failed;
        }
    };
    if mode == ApprovalMode::Strict {
        tracing::info!(device = %row.device_id, "device is pending approval");
        return Approval::Pending;
    }
    let label = row.label.clone().unwrap_or_else(|| {
        let prefix: String = row.device_id.0.chars().take(8).collect();
        format!("Unknown {prefix}")
    });
    let command = ApproveDevice {
        device: row.device_id.clone(),
        label: label.clone(),
    };
    match shared.db.write(command).await {
        Ok(()) => {
            tracing::info!(device = %row.device_id, %label, "approved a new device automatically");
            row.approved = true;
            row.label = Some(label);
            Approval::Approved(Box::new(row))
        }
        Err(error) => {
            tracing::warn!(device = %row.device_id, %error, "could not approve a device");
            Approval::Failed
        }
    }
}

/// Forgets a claimed session however its handshake task ends.
struct ReleaseGuard<'a> {
    shared: &'a Shared,
    core: Arc<SessionCore>,
}

impl Drop for ReleaseGuard<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.core.end(CloseReason::TaskPanicked("control"));
        }
        self.shared.registry.release(&self.core);
    }
}

#[allow(clippy::too_many_arguments)]
async fn admit(
    shared: &Shared,
    request: HelloRequest,
    row: DeviceRow,
    transport: Transport,
    peer: SocketAddr,
    deadline: Instant,
    mut sink: WsSink,
    stream: WsStream,
) {
    let HelloRequest { header, hello } = request;
    let device = header.device_id;
    let platform = match check_admission(&hello) {
        Ok(platform) => platform,
        Err(_) => unreachable!("check_hello admitted the hello"),
    };
    let (session, runner) = new_session(
        device.clone(),
        transport,
        &shared.config.limits,
        &shared.shutdown,
    );
    let core = Arc::clone(runner.core());
    let _release = ReleaseGuard {
        shared,
        core: Arc::clone(&core),
    };
    // The old session is invalidated before its replacement is admitted (§5.7).
    if let Some(old) = shared.registry.claim(Arc::clone(&core)) {
        old.end(CloseReason::Replaced);
    }
    let capabilities = hello.capabilities.clone();
    let request = AdmissionRequest {
        device: device.clone(),
        session_id: core.session_id.clone(),
        transport,
        peer,
        hello,
        platform,
        device_row: row,
    };
    let decision = tokio::select! {
        _ = core.stop.cancelled() => None,
        decision = timeout_at(deadline, shared.admission.admit(request, session)) => Some(decision),
    };
    let grant = match decision {
        None => {
            close(&mut sink, shared).await;
            return;
        }
        Some(Ok(AdmissionDecision::Ready(grant))) => grant,
        Some(Ok(AdmissionDecision::Decline(detail))) => {
            core.end(CloseReason::Declined(detail));
            close(&mut sink, shared).await;
            return;
        }
        Some(Err(_)) => {
            core.end(CloseReason::Declined(
                "admission did not answer before the ready deadline".to_owned(),
            ));
            close(&mut sink, shared).await;
            return;
        }
    };
    let Some(utc_ms) = utc_ms(shared.config.wall_clock.as_ref()) else {
        core.end(CloseReason::Declined(
            "the wall clock reads before 1970".to_owned(),
        ));
        close(&mut sink, shared).await;
        return;
    };
    let ready = SessionReady::new(
        &grant,
        &capabilities,
        core.session_id.clone(),
        shared.server_boot_id,
        utc_ms,
    );
    let encoded = match encode_envelope(&ready, Some(&core.session_id), &device, 0) {
        Ok(encoded) => encoded,
        Err(error) => {
            core.end(CloseReason::Declined(format!("session.ready: {error}")));
            close(&mut sink, shared).await;
            return;
        }
    };
    // Open the session to its audio and asset sockets before the device can read
    // session.ready and dial them.
    let (audio_tx, audio_rx) = oneshot::channel();
    if !shared.registry.activate(&core, audio_tx) {
        core.end(CloseReason::Replaced);
        close(&mut sink, shared).await;
        return;
    }
    if core.stop.is_cancelled() {
        close(&mut sink, shared).await;
        return;
    }
    let written = timeout(
        shared.config.timings.write,
        sink.send(Message::text(encoded.text)),
    )
    .await;
    if let Some(detail) = match written {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("session.ready stalled past the write deadline".to_owned()),
    } {
        core.end(CloseReason::SocketClosed {
            socket: crate::session::SocketKind::Control,
            detail,
        });
        return;
    }
    tracing::info!(%device, session = %core.session_id, %transport, %peer, "device session ready");
    runner.run(shared, sink, stream, audio_rx).await;
}
