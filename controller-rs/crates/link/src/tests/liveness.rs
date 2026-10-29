//! Transport-owned work (spec §3.1.3, §5.2): heartbeats, liveness, clock replies and
//! acknowledgement correlation. Time is paused once the session is up, over an
//! in-process pipe so nothing outside the runtime can move it.

use std::time::Duration;

use echomuse_protocol::session::CommandAck;
use echomuse_protocol::vocab::AckStatus;
use echomuse_protocol::{ControlBody, MessageType};
use serde::Serialize;
use serde_json::json;
use tokio::time::Instant;

use super::{Harness, Route, UTC_MS};
use crate::{CloseReason, CommandFailure, CommandProgress, SocketKind, Transport, UrgentEvent};

const PIPE: Route = Route::Pipe(Transport::Plain);

#[derive(Serialize)]
struct FocusAcquire {
    focus: &'static str,
}

impl ControlBody for FocusAcquire {
    const MESSAGE_TYPE: MessageType = MessageType::FocusAcquire;
}

#[tokio::test]
async fn heartbeats_flow_and_three_seconds_of_silence_lose_the_session() {
    let mut h = Harness::start().await;
    let device = h.approved("G0LIVE0001").await;
    let (mut control, mut admitted) = device.connect(&mut h, &PIPE).await;
    let session = control.session().to_owned();
    tokio::time::pause();

    // The controller heartbeats every second, on the session.
    let first = Instant::now();
    let hb = control.next_json().await.unwrap();
    assert_eq!(hb["type"], "heartbeat");
    assert_eq!(hb["session_id"], json!(session));
    assert!(hb["body"]["mono_ns"].is_string());
    control.next_json().await.unwrap();
    let gap = Instant::now() - first;
    assert!(gap >= Duration::from_millis(900), "{gap:?}");

    // The device's heartbeat is recorded with its monotonic clock.
    control
        .send("heartbeat", 0, json!({"mono_ns": "123456789"}))
        .await;
    let mut status = admitted.session.link.watch_status();
    let seen = status
        .wait_for(|s| s.last_heartbeat.is_some())
        .await
        .unwrap()
        .last_heartbeat;
    assert_eq!(seen.unwrap().device_mono_ns, 123_456_789);
    let last_rx = Instant::now();

    // Two seconds of silence degrades the link; any control message restores it.
    status.wait_for(|s| s.degraded).await.unwrap();
    assert!(Instant::now() - last_rx >= Duration::from_secs(2));
    control.send("teleport.begin", 0, json!({})).await;
    status.wait_for(|s| !s.degraded).await.unwrap();
    let last_rx = Instant::now();

    // Three seconds of silence loses the session.
    let ended = admitted.session.urgent.recv().await;
    assert_eq!(ended, Some(UrgentEvent::Ended(CloseReason::HeartbeatLost)));
    let silence = Instant::now() - last_rx;
    assert!(
        silence >= Duration::from_secs(3) && silence < Duration::from_millis(3200),
        "{silence:?}"
    );
    assert_eq!(admitted.session.urgent.recv().await, None);
    let rest = control.drain_until_closed().await;
    assert!(
        rest.is_empty(),
        "no protocol.error for a lost session: {rest:?}"
    );
}

#[tokio::test]
async fn clock_requests_are_answered_with_utc() {
    let mut h = Harness::start().await;
    let device = h.approved("G0LIVE0002").await;
    let (mut control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    control
        .send("clock.request", 0, json!({"nonce": "n-1"}))
        .await;
    let reply = control.next_message().await.unwrap();
    assert_eq!(reply["type"], "clock.reply");
    assert_eq!(reply["session_id"], json!(control.session()));
    assert_eq!(
        reply["body"],
        json!({"nonce": "n-1", "utc_ms": UTC_MS.to_string()})
    );
}

#[tokio::test]
async fn an_acknowledged_command_moves_from_enqueued_to_written_to_acknowledged() {
    let mut h = Harness::start().await;
    let device = h.approved("G0LIVE0003").await;
    let (mut control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let link = admitted.session.link.clone();

    let mut pending = link
        .request(
            &FocusAcquire {
                focus: "dialog_input",
            },
            4,
            Duration::from_secs(5),
        )
        .unwrap();
    pending.written().await.unwrap();
    let sent = control.next_message().await.unwrap();
    assert_eq!(sent["type"], "focus.acquire");
    assert_eq!(sent["generation"], 4);
    assert_eq!(sent["session_id"], json!(control.session()));
    assert_eq!(sent["message_id"], json!(pending.message_id().as_str()));
    assert_eq!(pending.progress(), CommandProgress::Written);

    // An acknowledgement for something else is ignored.
    control
        .send(
            "command.ack",
            0,
            json!({"message_id": "someone-else", "status": "accepted", "error": null}),
        )
        .await;
    control
        .send(
            "command.ack",
            0,
            json!({"message_id": pending.message_id().as_str(), "status": "rejected", "error": "invalid"}),
        )
        .await;
    let ack = pending.acknowledged().await.unwrap();
    assert_eq!(
        ack,
        CommandAck {
            message_id: ack.message_id.clone(),
            status: AckStatus::Rejected,
            error: Some("invalid".to_owned()),
        }
    );
}

#[tokio::test]
async fn an_unacknowledged_command_times_out() {
    let mut h = Harness::start().await;
    let device = h.approved("G0LIVE0004").await;
    let (mut control, admitted) = device.connect(&mut h, &PIPE).await;
    tokio::time::pause();
    let start = Instant::now();
    let mut pending = admitted
        .session
        .link
        .request(
            &FocusAcquire {
                focus: "dialog_output",
            },
            1,
            Duration::from_millis(1500),
        )
        .unwrap();
    pending.written().await.unwrap();
    assert_eq!(
        control.next_message().await.unwrap()["type"],
        "focus.acquire"
    );
    // Keep the session alive while the deadline passes.
    control.send("heartbeat", 0, json!({"mono_ns": "1"})).await;
    let failure = pending.acknowledged().await.unwrap_err();
    assert_eq!(failure, CommandFailure::Timeout { written: true });
    let waited = Instant::now() - start;
    assert!(
        waited >= Duration::from_millis(1500) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
    assert_eq!(admitted.session.link.end_reason(), None);
}

#[tokio::test]
async fn session_loss_fails_waiting_commands() {
    let mut h = Harness::start().await;
    let device = h.approved("G0LIVE0005").await;
    let (mut control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let mut pending = admitted
        .session
        .link
        .request(
            &FocusAcquire {
                focus: "dialog_input",
            },
            1,
            Duration::from_secs(30),
        )
        .unwrap();
    pending.written().await.unwrap();
    assert_eq!(
        control.next_message().await.unwrap()["type"],
        "focus.acquire"
    );
    drop(control);
    match pending.acknowledged().await.unwrap_err() {
        CommandFailure::SessionEnded {
            reason:
                CloseReason::SocketClosed {
                    socket: SocketKind::Control,
                    ..
                },
            written: true,
        } => {}
        other => panic!("expected the session loss, got {other:?}"),
    }
    let after = admitted.session.link.request(
        &FocusAcquire {
            focus: "dialog_input",
        },
        1,
        Duration::from_secs(30),
    );
    assert!(matches!(after, Err(crate::SendError::SessionEnded(_))));
}
