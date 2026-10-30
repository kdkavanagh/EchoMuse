//! Admission: `session.ready`, every rejection reason and the token rules (spec §3.1.2,
//! §3.1.4).

use echomuse_core::types::DeviceId;
use echomuse_db::devices::{GetDevice, UnixSeconds};
use echomuse_protocol::Platform;
use serde_json::{Value, json};

use super::{Decide, Harness, Route, SimDevice, UTC_MS, hello_body, test_config};
use crate::{CloseReason, Transport, UrgentEvent};

async fn row(h: &Harness, id: &str) -> echomuse_db::devices::DeviceRow {
    h.db.read(GetDevice {
        device: DeviceId(id.to_owned()),
    })
    .await
    .unwrap()
    .expect("a device row")
}

fn assert_rejected(reply: &Value, reason: &str) {
    assert_eq!(reply["type"], "session.rejected", "{reply}");
    assert_eq!(reply["session_id"], Value::Null);
    assert_eq!(reply["body"], json!({"reason": reason}));
}

#[tokio::test]
async fn an_admitted_hello_gets_session_ready_built_from_the_grant() {
    let mut h = Harness::start().await;
    let device = h.approved("G0SYNTH0001").await;
    let (mut control, reply) = device.hello(&h, &Route::Tcp).await;

    assert_eq!(reply["type"], "session.ready");
    let session = reply["body"]["session_id"].as_str().unwrap().to_owned();
    assert_eq!(
        reply["session_id"],
        json!(session),
        "ready carries its session id"
    );
    assert_eq!(reply["device_id"], "G0SYNTH0001");
    let body = &reply["body"];
    assert_eq!(body["protocol"], 1);
    assert_eq!(
        body["server_boot_id"],
        json!(h.server.server_boot_id().to_string())
    );
    assert_eq!(body["capture_permitted"], true);
    assert_eq!(body["utc_ms"], json!(UTC_MS.to_string()));

    let admitted = h.next_admitted().await;
    assert_eq!(admitted.request.device, DeviceId("G0SYNTH0001".to_owned()));
    assert_eq!(admitted.request.session_id.as_str(), session);
    assert_eq!(admitted.request.platform, Platform::FireOs6);
    assert_eq!(admitted.request.transport, Transport::Plain);
    assert!(admitted.request.device_row.approved);
    assert_eq!(admitted.session.link.session_id().as_str(), session);

    // The attempt was recorded.
    let stored = row(&h, "G0SYNTH0001").await;
    assert_eq!(stored.ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(stored.firmware_version.as_deref(), Some("fw-test"));
    assert_eq!(stored.os_version.as_deref(), Some("Fire OS test"));
    assert_eq!(stored.last_seen, Some(UnixSeconds((UTC_MS / 1000) as i64)));

    // Every later controller envelope carries the session id.
    let hb = control.next_json().await.unwrap();
    assert_eq!(hb["type"], "heartbeat");
    assert_eq!(hb["session_id"], json!(session));
}

#[tokio::test]
async fn a_missing_required_capability_is_rejected_as_protocol_and_recorded() {
    let h = Harness::start().await;
    let device = h.approved("G0SYNTH0002").await;
    let mut body = hello_body();
    body["capabilities"] = json!(["audio_timeline_v1", "leds"]);
    let (mut control, reply) = device.hello_with(&h, &Route::Tcp, body).await;
    assert_rejected(&reply, "protocol");
    assert!(
        control.next_json().await.is_none(),
        "closed after the rejection"
    );
    assert_eq!(
        row(&h, "G0SYNTH0002").await.firmware_version.as_deref(),
        Some("fw-test")
    );
}

#[tokio::test]
async fn an_unknown_platform_is_rejected_as_protocol() {
    let h = Harness::start().await;
    let device = h.approved("G0SYNTH0003").await;
    let mut body = hello_body();
    body["platform"] = json!("fireos5");
    let (_, reply) = device.hello_with(&h, &Route::Tcp, body).await;
    assert_rejected(&reply, "protocol");
}

#[tokio::test]
async fn a_malformed_hello_is_rejected_as_protocol_and_still_recorded() {
    let h = Harness::start().await;
    let device = SimDevice::unknown("G0SYNTH0004", None);
    let mut body = hello_body();
    body["capabilities"] = json!("audio_timeline_v1");
    let (_, reply) = device.hello_with(&h, &Route::Tcp, body).await;
    assert_rejected(&reply, "protocol");
    let stored = row(&h, "G0SYNTH0004").await;
    assert!(!stored.approved);
    assert_eq!(stored.ip.as_deref(), Some("127.0.0.1"));
    assert_eq!(
        stored.firmware_version, None,
        "the malformed hello reported nothing readable"
    );
}

#[tokio::test]
async fn a_first_frame_naming_no_device_is_closed_without_a_reply() {
    let h = Harness::start().await;
    let mut ws = h.dial(&Route::Tcp, crate::PATH_CONTROL, &[]).await.unwrap();
    use futures_util::SinkExt;
    ws.send(tokio_tungstenite::tungstenite::Message::text("not json"))
        .await
        .unwrap();
    assert!(super::closes(&mut ws).await);
}

#[tokio::test]
async fn a_mismatched_token_is_unauthorized() {
    let h = Harness::start().await;
    let mut device = h.approved("G0SYNTH0005").await;
    device.token = Some("not-the-stored-token".to_owned());
    let (_, reply) = device.hello(&h, &Route::Tcp).await;
    assert_rejected(&reply, "unauthorized");
}

#[tokio::test]
async fn a_new_device_in_strict_mode_is_pending_approval_and_registered() {
    let h = Harness::start().await;
    let device = SimDevice::unknown("G0SYNTH0006", None);
    let (_, reply) = device.hello(&h, &Route::Tcp).await;
    assert_rejected(&reply, "pending_approval");
    let stored = row(&h, "G0SYNTH0006").await;
    assert!(!stored.approved);
    assert_eq!(stored.label, None);
    assert_eq!(stored.firmware_version.as_deref(), Some("fw-test"));
}

#[tokio::test]
async fn a_new_device_in_auto_mode_is_approved_and_admitted() {
    let mut h = Harness::start().await;
    h.set_setting("device_approval", "auto").await;
    let device = SimDevice::unknown("G0SYNTH0007XYZ", None);
    let (_, admitted) = device.connect(&mut h, &Route::Tcp).await;
    assert!(admitted.request.device_row.approved);
    let stored = row(&h, "G0SYNTH0007XYZ").await;
    assert!(stored.approved);
    assert_eq!(stored.label.as_deref(), Some("Unknown G0SYNTH0"));
}

#[tokio::test]
async fn a_missing_token_is_allowed_without_require_device_tls() {
    let mut h = Harness::start().await;
    let mut device = h.approved("G0SYNTH0008").await;
    device.token = None;
    device.connect(&mut h, &Route::Tcp).await;
}

#[tokio::test]
async fn a_token_from_a_device_with_none_stored_is_ignored() {
    let mut h = Harness::start().await;
    // Approved, but its token was revoked: the presented one is ignored.
    let mut device = h.approved_without_token("G0SYNTH0009").await;
    device.token = Some("left-over-token".to_owned());
    device.connect(&mut h, &Route::Tcp).await;

    // Deleted (no row at all): ignored too, and the device registers as pending.
    let orphan = SimDevice::unknown("G0SYNTH0010", Some("left-over-token"));
    let (_, reply) = orphan.hello(&h, &Route::Tcp).await;
    assert_rejected(&reply, "pending_approval");
    assert!(!row(&h, "G0SYNTH0010").await.approved);
}

#[tokio::test]
async fn require_device_tls_needs_tls_and_the_matching_token() {
    let mut config = test_config();
    config.require_device_tls = true;
    let mut h = Harness::with(config, Default::default()).await;
    let device = h.approved("G0SYNTH0011").await;

    let (_, reply) = device.hello(&h, &Route::Tcp).await;
    assert_rejected(&reply, "unauthorized");

    let tls = Route::Pipe(Transport::Tls);
    let mut tokenless = device.clone();
    tokenless.token = None;
    let (_, reply) = tokenless.hello(&h, &tls).await;
    assert_rejected(&reply, "unauthorized");

    let mut wrong = device.clone();
    wrong.token = Some("wrong".to_owned());
    let (_, reply) = wrong.hello(&h, &tls).await;
    assert_rejected(&reply, "unauthorized");

    let mut revoked = h.approved_without_token("G0SYNTH0012").await;
    revoked.token = Some("anything".to_owned());
    let (_, reply) = revoked.hello(&h, &tls).await;
    assert_rejected(&reply, "unauthorized");

    let (_, admitted) = device.connect(&mut h, &tls).await;
    assert_eq!(admitted.request.transport, Transport::Tls);
}

#[tokio::test]
async fn a_declined_admission_closes_without_ready() {
    let mut h = Harness::start().await;
    *h.decide.lock().unwrap() = Decide::Decline;
    let device = h.approved("G0SYNTH0013").await;
    let ws = h
        .dial(
            &Route::Tcp,
            crate::PATH_CONTROL,
            &[("X-EM-Token", device.token.as_deref().unwrap())],
        )
        .await
        .unwrap();
    let mut control = super::Control {
        ws,
        device: device.id.clone(),
        session: None,
    };
    control
        .send_text(super::envelope(
            "session.hello",
            None,
            &device.id,
            0,
            hello_body(),
        ))
        .await;
    assert!(
        control.next_json().await.is_none(),
        "closed without a reply"
    );
    let mut admitted = h.next_admitted().await;
    assert_eq!(
        admitted.session.urgent.recv().await,
        Some(UrgentEvent::Ended(CloseReason::Declined(
            "runtime not ready".to_owned()
        )))
    );
}

#[tokio::test]
async fn a_control_socket_without_a_hello_is_closed() {
    let h = Harness::start().await;
    let mut ws = h
        .dial(&Route::Pipe(Transport::Plain), crate::PATH_CONTROL, &[])
        .await
        .unwrap();
    tokio::time::pause();
    // No test timeout here: paused time would fire it before the server's deadline.
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let closed = loop {
        match ws.next().await {
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(_)) | Err(_)) | None => break true,
            Some(Ok(_)) => break false,
        }
    };
    assert!(closed);
}
