//! The hand-off to the runtime (spec §5.2): one ordered channel for both sockets, the
//! urgent path, telemetry, overload and protocol violations.

use std::time::Duration;

use echomuse_core::types::{DeviceId, RuntimeId, SessionId, WorkIdentity, WorkScope};
use echomuse_protocol::envelope::DeviceMessage;
use echomuse_protocol::{ProtocolError, ProtocolErrorCode};
use futures_util::SinkExt;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use super::{Client, Harness, Route, closes, mic_packet, test_config};
use crate::{CloseReason, Ingress, PrivacyChange, QueueKind, TelemetryKind, UrgentEvent};

fn stream_open(epoch: u64) -> serde_json::Value {
    json!({"stream_id": "mic", "epoch": epoch.to_string(), "kind": 1, "sample_rate": 16000,
           "format": 1, "reason": "start"})
}

async fn audio(h: &Harness, device: &super::SimDevice, session: &str) -> Client {
    device
        .dial_joined(h, &Route::Tcp, crate::PATH_AUDIO, session)
        .await
        .unwrap()
}

fn kind_of(item: &crate::IngressItem) -> String {
    match &item.ingress {
        Ingress::Control(envelope) => envelope.message.message_type().to_string(),
        Ingress::Audio(packet) => format!("audio:{}", packet.header().sequence),
    }
}

#[tokio::test]
async fn control_and_audio_share_one_ordered_channel() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0001").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = audio(&h, &device, &session).await;
    let ingress = &mut admitted.session.ingress;

    // Each send waits for its item, so the observed enqueue order is known.
    control.send("stream.open", 0, stream_open(9)).await;
    let first = ingress.recv().await.unwrap();
    audio
        .send(Message::Binary(mic_packet(9, 0, 0)))
        .await
        .unwrap();
    let second = ingress.recv().await.unwrap();
    control
        .send(
            "stream.end",
            0,
            json!({"stream_id": "mic", "epoch": "9", "final_sample": "160"}),
        )
        .await;
    let third = ingress.recv().await.unwrap();
    assert_eq!(
        [&first, &second, &third].map(kind_of),
        ["stream.open", "audio:0", "stream.end"]
    );
    assert_eq!(
        [first.ordinal.0, second.ordinal.0, third.ordinal.0],
        [0, 1, 2]
    );
    match &second.ingress {
        Ingress::Audio(packet) => assert_eq!(packet.payload().len(), 320),
        other => panic!("{other:?}"),
    }

    // Interleaved without waiting: one channel, gapless ordinals, each socket's order
    // kept.
    for n in 0..20u64 {
        control.send("wake.candidate_end", 0, json!({"n": n})).await;
        audio
            .send(Message::Binary(mic_packet(9, n + 1, 160 * (n + 1))))
            .await
            .unwrap();
    }
    let mut control_seen = Vec::new();
    let mut audio_seen = Vec::new();
    for expected in 3..43u64 {
        let item = ingress.recv().await.unwrap();
        assert_eq!(item.ordinal.0, expected);
        match item.ingress {
            Ingress::Control(envelope) => {
                let DeviceMessage::Other { body, .. } = envelope.message else {
                    panic!("wake.candidate_end passes through")
                };
                control_seen.push(body["n"].as_u64().unwrap());
            }
            Ingress::Audio(packet) => audio_seen.push(packet.header().sequence),
        }
    }
    assert_eq!(control_seen, (0..20).collect::<Vec<_>>());
    assert_eq!(audio_seen, (1..21).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_mute_overtakes_queued_audio_and_revokes_capture_first() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0002").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = audio(&h, &device, &session).await;
    let scope = WorkScope::new(
        WorkIdentity {
            device: DeviceId("G0INGR0002".to_owned()),
            runtime: RuntimeId(1),
            session: SessionId(1),
            turn: None,
        },
        CancellationToken::new(),
    );
    admitted.session.link.publish_capture_scope(scope.clone());

    for n in 0..10u64 {
        audio
            .send(Message::Binary(mic_packet(9, n, 160 * n)))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    control
        .send(
            "privacy.changed",
            0,
            json!({"muted": true, "capture_epoch": null, "physical_seq": 12}),
        )
        .await;

    // The runtime services urgent first: the mute arrives while the audio still waits.
    match admitted.session.urgent.recv().await {
        Some(UrgentEvent::Privacy { change, .. }) => assert_eq!(
            change,
            PrivacyChange {
                muted: true,
                capture_epoch: None,
                physical_seq: Some(12)
            }
        ),
        other => panic!("expected the mute, got {other:?}"),
    }
    assert!(
        scope.is_cancelled(),
        "capture was revoked before the runtime heard"
    );
    for n in 0..10u64 {
        let item = admitted.session.ingress.recv().await.unwrap();
        assert_eq!(kind_of(&item), format!("audio:{n}"));
    }
}

#[tokio::test]
async fn replaceable_telemetry_coalesces_and_never_takes_ingress_room() {
    let mut config = test_config();
    config.limits.ingress.messages = 2;
    let mut h = Harness::with(config, Default::default()).await;
    let device = h.approved("G0INGR0003").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    for n in 0..50 {
        control.send("stats", 0, json!({"n": n})).await;
    }
    control.send("log", 0, json!({"message": "hello"})).await;
    control
        .send("clock.request", 0, json!({"nonce": "sync"}))
        .await;
    // The clock reply proves everything before it was processed.
    assert_eq!(control.next_message().await.unwrap()["type"], "clock.reply");

    let telemetry = &mut admitted.session.telemetry;
    let mut seen = Vec::new();
    for _ in 0..2 {
        let item = telemetry.recv().await.unwrap();
        let DeviceMessage::Other { body, .. } = &item.envelope.message else {
            panic!()
        };
        seen.push((item.kind, body.clone()));
    }
    seen.sort_by_key(|(kind, _)| *kind == TelemetryKind::Stats);
    assert_eq!(seen[0].0, TelemetryKind::Log);
    assert_eq!(
        seen[1],
        (
            TelemetryKind::Stats,
            json!({"n": 49}).as_object().unwrap().clone()
        )
    );
    assert_eq!(admitted.session.link.end_reason(), None);
}

#[tokio::test]
async fn an_ingress_overload_closes_the_session_without_a_protocol_error() {
    let mut config = test_config();
    config.limits.ingress.messages = 4;
    let mut h = Harness::with(config, Default::default()).await;
    let device = h.approved("G0INGR0004").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = audio(&h, &device, &session).await;
    for n in 0..10u64 {
        if audio
            .send(Message::Binary(mic_packet(9, n, 160 * n)))
            .await
            .is_err()
        {
            break;
        }
    }
    match admitted.session.urgent.recv().await {
        Some(UrgentEvent::Ended(CloseReason::Overloaded {
            queue: QueueKind::Ingress,
            ..
        })) => {}
        other => panic!("expected an ingress overload, got {other:?}"),
    }
    assert!(
        admitted.session.ingress.recv().await.is_none(),
        "queued audio is discarded"
    );
    let seen = control.drain_until_closed().await;
    assert!(seen.is_empty(), "no invented wire code: {seen:?}");
}

#[tokio::test]
async fn an_urgent_overload_closes_the_session() {
    let mut config = test_config();
    config.limits.urgent_events = 2;
    let mut h = Harness::with(config, Default::default()).await;
    let device = h.approved("G0INGR0005").await;
    let (mut control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    for seq in 0..3 {
        control
            .send(
                "privacy.changed",
                0,
                json!({"muted": seq % 2 == 0, "physical_seq": seq}),
            )
            .await;
    }
    let reason = admitted.session.link.ended().await;
    assert!(
        matches!(
            reason,
            CloseReason::Overloaded {
                queue: QueueKind::Urgent,
                ..
            }
        ),
        "{reason:?}"
    );
}

async fn expect_protocol_error(control: &mut super::Control, code: &str) {
    let seen = control.drain_until_closed().await;
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0]["type"], "protocol.error");
    assert_eq!(seen[0]["session_id"], json!(control.session()));
    assert_eq!(seen[0]["body"]["code"], code);
}

#[tokio::test]
async fn a_wrong_session_id_is_answered_with_protocol_error_then_close() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0006").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let text = super::envelope(
        "heartbeat",
        Some("4c4b0d36-8d0f-4b7e-9a51-5c1f4a8b2e10"),
        &device.id,
        0,
        json!({"mono_ns": "1"}),
    );
    control.send_text(text).await;
    expect_protocol_error(&mut control, "wrong_session").await;
    match admitted.session.urgent.recv().await {
        Some(UrgentEvent::Ended(CloseReason::ProtocolViolation(error))) => {
            assert_eq!(error.code, ProtocolErrorCode::WrongSession);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn malformed_control_and_audio_frames_are_protocol_errors() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0007").await;

    let (mut control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    control
        .ws
        .send(Message::Binary(vec![1, 2, 3].into()))
        .await
        .unwrap();
    expect_protocol_error(&mut control, "malformed_message").await;

    let (mut control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    control
        .send("privacy.changed", 0, json!({"muted": "yes"}))
        .await;
    expect_protocol_error(&mut control, "malformed_message").await;

    let (mut control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = audio(&h, &device, &session).await;
    audio
        .send(Message::Binary(vec![0u8; 10].into()))
        .await
        .unwrap();
    expect_protocol_error(&mut control, "malformed_frame").await;
    assert!(closes(&mut audio).await);

    let (mut control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    control.send("session.hello", 0, super::hello_body()).await;
    expect_protocol_error(&mut control, "unexpected_message").await;
}

#[tokio::test]
async fn a_runtime_found_violation_is_reported_then_closed() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0008").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = audio(&h, &device, &session).await;
    audio
        .send(Message::Binary(mic_packet(77, 0, 0)))
        .await
        .unwrap();
    // The runtime's stream registry has never seen epoch 77.
    let mut registry = echomuse_protocol::streams::StreamRegistry::new(false);
    let item = admitted.session.ingress.recv().await.unwrap();
    let Ingress::Audio(packet) = item.ingress else {
        panic!()
    };
    let error: ProtocolError = registry.check(&packet).unwrap_err();
    admitted.session.link.protocol_violation(error);
    expect_protocol_error(&mut control, "unknown_epoch").await;
}

#[tokio::test]
async fn an_unknown_message_type_is_ignored_and_a_device_protocol_error_ends_the_session() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0009").await;
    let (mut control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    control
        .send("teleport.begin", 0, json!({"anything": 1}))
        .await;
    control
        .send(
            "protocol.error",
            0,
            json!({"code": "malformed_frame", "detail": "bad render"}),
        )
        .await;
    match admitted.session.link.ended().await {
        CloseReason::DeviceProtocolError(report) => assert_eq!(report.detail, "bad render"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_item_waiting_past_the_residence_limit_closes_the_session() {
    let mut config = test_config();
    config.limits.ingress.max_age = Duration::from_millis(500);
    let mut h = Harness::with(config, Default::default()).await;
    let device = h.approved("G0INGR0010").await;
    let route = Route::Pipe(crate::Transport::Plain);
    let (mut control, mut admitted) = device.connect(&mut h, &route).await;
    tokio::time::pause();
    control.send("wake.candidate_end", 0, json!({})).await;
    control
        .send("clock.request", 0, json!({"nonce": "sync"}))
        .await;
    assert_eq!(control.next_message().await.unwrap()["type"], "clock.reply");
    tokio::time::advance(Duration::from_millis(600)).await;
    assert!(admitted.session.ingress.recv().await.is_none());
    match admitted.session.link.end_reason() {
        Some(CloseReason::Overloaded {
            queue: QueueKind::Ingress,
            ..
        }) => {}
        other => panic!("expected a residence overload, got {other:?}"),
    }
}

#[tokio::test]
async fn render_frames_reach_the_audio_socket_and_cancelled_playbacks_are_purged() {
    let mut h = Harness::start().await;
    let device = h.approved("G0INGR0011").await;
    let (control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let link = admitted.session.link.clone();
    let frame = |generation: u32, sequence: u64| {
        echomuse_protocol::ema::encode_render(
            42,
            sequence,
            sequence * 3840,
            generation,
            echomuse_protocol::ema::Flags::NONE,
            &[0i16; 3840],
        )
        .unwrap()
    };
    let cancelled = CancellationToken::new();
    let live = CancellationToken::new();
    // Queued before audio joins, so nothing has been written yet.
    link.try_send_render(frame(1, 0), &cancelled).unwrap();
    link.try_send_render(frame(1, 1), &cancelled).unwrap();
    link.send_render(frame(2, 0), &live).await.unwrap();
    cancelled.cancel();
    assert!(matches!(
        link.try_send_render(frame(1, 2), &cancelled),
        Err(crate::RenderError::PlaybackCancelled)
    ));
    assert!(matches!(
        link.try_send_render(mic_packet(9, 0, 0), &live),
        Err(crate::RenderError::NotRender)
    ));

    let mut audio = audio(&h, &device, control.session()).await;
    let received = super::next_binary(&mut audio).await.unwrap();
    let header = echomuse_protocol::ema::decode_header(&received).unwrap();
    assert_eq!((header.generation, header.sequence), (2, 0));
    assert_eq!(received, frame(2, 0));
}
