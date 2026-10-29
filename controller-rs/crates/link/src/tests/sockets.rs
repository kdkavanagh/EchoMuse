//! Socket pairing and session lifetime (spec §3.1.1).

use std::time::Duration;

use futures_util::SinkExt;
use tokio_tungstenite::tungstenite::Message;

use super::{Harness, Route, closes, mic_packet, refused_status};
use crate::{CloseReason, Ingress, SocketKind, Transport, UrgentEvent};

#[tokio::test]
async fn audio_and_assets_join_the_control_session() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0001").await;
    let (control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();

    let mut audio = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, &session)
        .await
        .unwrap();
    let mut status = admitted.session.link.watch_status();
    status.wait_for(|s| s.audio_joined).await.unwrap();

    audio
        .send(Message::Binary(mic_packet(9, 0, 0)))
        .await
        .unwrap();
    let item = admitted.session.ingress.recv().await.unwrap();
    assert!(matches!(item.ingress, Ingress::Audio(_)));
}

#[tokio::test]
async fn an_upgrade_naming_no_live_session_is_refused_before_upgrading() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0002").await;
    for path in [crate::PATH_AUDIO, crate::PATH_ASSETS] {
        let missing = h.dial(&Route::Tcp, path, &[]).await;
        assert_eq!(refused_status(missing), 403, "{path} without X-EM-Session");
        let unknown = device
            .dial_joined(
                &h,
                &Route::Tcp,
                path,
                "4c4b0d36-8d0f-4b7e-9a51-5c1f4a8b2e10",
            )
            .await;
        assert_eq!(refused_status(unknown), 403, "{path} naming no session");
    }
    // A session that has ended is no longer joinable.
    let (control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    admitted.session.link.close();
    admitted.session.link.ended().await;
    let late = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await;
    assert_eq!(refused_status(late), 403);
}

#[tokio::test]
async fn audio_on_the_other_kind_of_listener_is_refused() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0003").await;
    let (control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let tls_kind = Route::Pipe(Transport::Tls);
    let refused = device
        .dial_joined(&h, &tls_kind, crate::PATH_AUDIO, &session)
        .await;
    assert_eq!(refused_status(refused), 403);
    let refused = device
        .dial_joined(&h, &tls_kind, crate::PATH_ASSETS, &session)
        .await;
    assert_eq!(refused_status(refused), 403);
    // The refusal did not take the session's audio slot.
    device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    assert_eq!(admitted.session.link.end_reason(), None);
}

#[tokio::test]
async fn audio_with_a_mismatched_token_or_a_second_audio_socket_is_refused() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0004").await;
    let (control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut impostor = device.clone();
    impostor.token = Some("forged".to_owned());
    let refused = impostor
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await;
    assert_eq!(refused_status(refused), 403);

    device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    let second = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await;
    assert_eq!(refused_status(second), 403);
}

#[tokio::test]
async fn losing_control_or_audio_ends_the_session_and_losing_assets_does_not() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0005").await;

    // Assets lost: the session lives on and the device can redial.
    let (control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let audio = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    let assets = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, &session)
        .await
        .unwrap();
    drop(assets);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(admitted.session.link.end_reason(), None);
    device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, &session)
        .await
        .unwrap();

    // Audio lost: the session ends.
    drop(audio);
    match admitted.session.urgent.recv().await {
        Some(UrgentEvent::Ended(CloseReason::SocketClosed {
            socket: SocketKind::Audio,
            ..
        })) => {}
        other => panic!("expected the audio loss, got {other:?}"),
    }
    let mut control = control;
    assert!(
        closes(&mut control.ws).await,
        "the control socket closes too"
    );

    // Control lost: the session ends.
    let (control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    drop(control);
    match admitted.session.urgent.recv().await {
        Some(UrgentEvent::Ended(CloseReason::SocketClosed {
            socket: SocketKind::Control,
            ..
        })) => {}
        other => panic!("expected the control loss, got {other:?}"),
    }
    assert!(admitted.session.ingress.recv().await.is_none());
}

#[tokio::test]
async fn a_new_hello_replaces_the_old_session_first_and_old_loss_cannot_end_the_new() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0006").await;
    let (old_control, mut old) = device.connect(&mut h, &Route::Tcp).await;
    let old_session = old_control.session().to_owned();
    let mut old_audio = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &old_session)
        .await
        .unwrap();

    let (new_control, new) = device.connect(&mut h, &Route::Tcp).await;
    let new_session = new_control.session().to_owned();
    assert_ne!(old_session, new_session);
    // The old session was invalidated before the new one was admitted.
    assert_eq!(new.earlier, vec![Some(CloseReason::Replaced)]);
    assert_eq!(
        old.session.urgent.recv().await,
        Some(UrgentEvent::Ended(CloseReason::Replaced))
    );
    assert!(old.session.ingress.recv().await.is_none());
    assert!(
        closes(&mut old_audio).await,
        "the old audio socket is closed"
    );

    // The old sockets going away afterwards does not touch the new session.
    drop(old_control);
    drop(old_audio);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(new.session.link.end_reason(), None);
    let joined_old = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &old_session)
        .await;
    assert_eq!(refused_status(joined_old), 403, "the old session is gone");
    device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &new_session)
        .await
        .unwrap();
    let mut status = new.session.link.watch_status();
    status.wait_for(|s| s.audio_joined).await.unwrap();
    assert_eq!(new.session.link.end_reason(), None);
}

#[tokio::test]
async fn a_rejected_hello_does_not_replace_the_live_session() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0007").await;
    let (_control, live) = device.connect(&mut h, &Route::Tcp).await;
    let mut impostor = device.clone();
    impostor.token = Some("forged".to_owned());
    let (_, reply) = impostor.hello(&h, &Route::Tcp).await;
    assert_eq!(reply["body"]["reason"], "unauthorized");
    assert_eq!(live.session.link.end_reason(), None);
}

#[tokio::test]
async fn a_session_whose_audio_never_joins_ends() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0008").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Pipe(Transport::Plain)).await;
    tokio::time::pause();
    // Keep the control socket alive past the audio deadline.
    let heartbeats = async {
        loop {
            control
                .send("heartbeat", 0, serde_json::json!({"mono_ns": "1"}))
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    };
    let ended = tokio::select! {
        ended = admitted.session.urgent.recv() => ended,
        _ = heartbeats => unreachable!(),
    };
    assert_eq!(ended, Some(UrgentEvent::Ended(CloseReason::AudioNotJoined)));
}

#[tokio::test]
async fn shutdown_ends_every_session_and_waits_for_its_tasks() {
    let mut h = Harness::start().await;
    let device = h.approved("G0PAIR0009").await;
    let (mut control, mut admitted) = device.connect(&mut h, &Route::Tcp).await;
    let session = control.session().to_owned();
    let mut audio = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    h.server.shutdown().await;
    assert_eq!(
        admitted.session.urgent.recv().await,
        Some(UrgentEvent::Ended(CloseReason::Shutdown))
    );
    assert!(closes(&mut control.ws).await);
    assert!(closes(&mut audio).await);
}
