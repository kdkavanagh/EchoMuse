//! `WS /api/events` over real sockets (§3.4).

mod common;

use std::{num::NonZeroUsize, time::Duration};

use common::{DAY, Options, PASSWORD, START, TestServer};
use echomuse_api::{ApiEvent, DeviceStatePatch, EventsConfig, FeatureStatus, HaStatus};
use echomuse_core::types::DeviceId;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{
        Error as WsError, Message,
        client::IntoClientRequest,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const KITCHEN: &str = "G0SYNTHETIC00001";
const WAIT: Duration = Duration::from_secs(10);

fn kitchen() -> DeviceId {
    DeviceId(KITCHEN.to_owned())
}

async fn connect(server: &TestServer, token: &str) -> Socket {
    let (socket, _) = connect_async(server.ws_url(&format!("/api/events?token={token}")))
        .await
        .unwrap();
    socket
}

async fn refused(request: tokio_tungstenite::tungstenite::handshake::client::Request) -> u16 {
    match connect_async(request).await {
        Err(WsError::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("unexpected error {other}"),
        Ok(_) => panic!("the socket opened"),
    }
}

/// The next text message as JSON.
async fn next_json(socket: &mut Socket) -> Value {
    loop {
        let message = tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("a message in time")
            .expect("the socket is open")
            .expect("no socket error");
        match message {
            Message::Text(text) => return serde_json::from_str(&text).unwrap(),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Read until the server closes the socket; returns the close code it gave, if any.
async fn closed_with(socket: &mut Socket) -> Option<CloseCode> {
    loop {
        match tokio::time::timeout(WAIT, socket.next())
            .await
            .expect("closed in time")
        {
            Some(Ok(Message::Close(frame))) => return frame.map(|f: CloseFrame| f.code),
            Some(Ok(_)) => {}
            Some(Err(_)) | None => return None,
        }
    }
}

async fn signed_in(server: &TestServer) -> String {
    server.add_user("admin", "admin");
    server.login("admin", PASSWORD).await
}

#[tokio::test]
async fn the_socket_needs_a_live_session() {
    let server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;

    let anonymous = server.ws_url("/api/events").into_client_request().unwrap();
    assert_eq!(refused(anonymous).await, 401);
    let unknown = server
        .ws_url(&format!("/api/events?token={}", "f".repeat(64)))
        .into_client_request()
        .unwrap();
    assert_eq!(refused(unknown).await, 401);

    // The session cookie works from the controller's own page, not another site's.
    let mut cookie = server.ws_url("/api/events").into_client_request().unwrap();
    cookie
        .headers_mut()
        .insert("cookie", format!("session={token}").parse().unwrap());
    let mut foreign = cookie.clone();
    foreign
        .headers_mut()
        .insert("origin", "http://evil.example".parse().unwrap());
    assert_eq!(refused(foreign).await, 403);
    let (mut socket, _) = connect_async(cookie).await.unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");

    // So does the Authorization header.
    let mut bearer = server.ws_url("/api/events").into_client_request().unwrap();
    bearer
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = connect_async(bearer).await.unwrap();
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");

    // A plain GET with a session is told this is a WebSocket address.
    let (status, body) = server.get("/api/events", Some(&token)).await;
    assert_eq!(status / 100, 4);
    assert_eq!(body["code"], "bad_request");
}

#[tokio::test]
async fn snapshot_first_then_published_events_in_order() {
    let server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;
    server.add_device(KITCHEN, Some("Kitchen"), true).await;

    let mut socket = connect(&server, &token).await;
    let snapshot = next_json(&mut socket).await;
    assert_eq!(snapshot["type"], "snapshot");
    // The same objects GET /api/devices answers with.
    let (status, listed) = server.get("/api/devices", Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(snapshot["devices"], listed);
    assert_eq!(snapshot["devices"][0]["device_id"], KITCHEN);
    assert_eq!(snapshot["devices"][0]["label"], "Kitchen");
    assert_eq!(snapshot["devices"][0]["connected"], false);
    assert_eq!(server.publisher.open_sockets(), 1);

    server.publisher.publish(&ApiEvent::DeviceConnected {
        device_id: kitchen(),
    });
    let Value::Object(state) = json!({"listening": true}) else {
        unreachable!()
    };
    server.publisher.publish(&ApiEvent::DeviceUpdate {
        device_id: kitchen(),
        state: DeviceStatePatch(state),
    });
    server.publisher.publish(&ApiEvent::TurnComplete {
        device_id: kitchen(),
    });
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "device_connected", "device_id": KITCHEN})
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "device_update", "device_id": KITCHEN, "state": {"listening": true}})
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "turn_complete", "device_id": KITCHEN})
    );

    socket.close(None).await.unwrap();
    let deadline = tokio::time::Instant::now() + WAIT;
    while server.publisher.open_sockets() > 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the socket's task ended"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_slow_client_is_dropped_without_holding_up_a_fast_one() {
    let server = TestServer::start(Options {
        events: EventsConfig {
            buffer: NonZeroUsize::new(4).unwrap(),
            write_timeout: Duration::from_millis(300),
        },
        ..Options::default()
    })
    .await;
    let token = signed_in(&server).await;
    let mut slow = connect(&server, &token).await;
    assert_eq!(next_json(&mut slow).await["type"], "snapshot");
    let mut fast = connect(&server, &token).await;
    assert_eq!(next_json(&mut fast).await["type"], "snapshot");
    assert_eq!(server.publisher.open_sockets(), 2);

    // Large events fill the slow client's socket buffers quickly. The fast client reads
    // each one as it is published.
    let filler = "x".repeat(64 * 1024);
    let mut published = 0;
    while server.publisher.open_sockets() == 2 {
        assert!(published < 2_000, "the slow client was never dropped");
        let Value::Object(state) = json!({"seq": published, "filler": filler}) else {
            unreachable!()
        };
        server.publisher.publish(&ApiEvent::DeviceUpdate {
            device_id: kitchen(),
            state: DeviceStatePatch(state),
        });
        let event = next_json(&mut fast).await;
        assert_eq!(event["state"]["seq"], published, "in order, none missing");
        published += 1;
    }
    assert_eq!(server.publisher.open_sockets(), 1);

    // The fast client carries on.
    server.publisher.publish(&ApiEvent::TurnComplete {
        device_id: kitchen(),
    });
    assert_eq!(next_json(&mut fast).await["type"], "turn_complete");

    // The slow one finds its stream ended once it reads what was already delivered.
    loop {
        match tokio::time::timeout(WAIT, slow.next())
            .await
            .expect("ends in time")
        {
            Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
            Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            Some(Ok(other)) => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn ending_a_session_closes_its_sockets() {
    let server = TestServer::start(Options::default()).await;
    let first = signed_in(&server).await;
    let second = server.login("admin", PASSWORD).await;
    let third = server.login("admin", PASSWORD).await;

    let mut on_first = connect(&server, &first).await;
    let mut on_second = connect(&server, &second).await;
    let mut on_third = connect(&server, &third).await;
    for socket in [&mut on_first, &mut on_second, &mut on_third] {
        assert_eq!(next_json(socket).await["type"], "snapshot");
    }

    assert_eq!(
        server
            .post("/api/auth/logout", json!({}), Some(&first))
            .await
            .0,
        200
    );
    assert_eq!(closed_with(&mut on_first).await, Some(CloseCode::Policy));

    let (status, _) = server
        .post(
            "/api/auth/change-password",
            json!({"current_password": PASSWORD, "new_password": "a new password"}),
            Some(&third),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(closed_with(&mut on_second).await, Some(CloseCode::Policy));

    // The session that changed the password keeps its socket.
    server.publisher.publish(&ApiEvent::DeviceDeleted {
        device_id: kitchen(),
    });
    assert_eq!(
        next_json(&mut on_third).await,
        json!({"type": "device_deleted", "device_id": KITCHEN})
    );
}

#[tokio::test]
async fn a_socket_closes_when_its_session_expires() {
    let server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;
    server.clock.set(START + 30 * DAY - 1);
    let mut socket = connect(&server, &token).await;
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");
    assert_eq!(closed_with(&mut socket).await, Some(CloseCode::Policy));
}

#[tokio::test]
async fn no_snapshot_closes_the_socket() {
    let server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;
    // A row the device list cannot read makes the snapshot unavailable.
    server
        .sqlite()
        .execute(
            "INSERT INTO devices (device_id, approved) VALUES ('G0SYNTHETICBAD01', 'not a flag')",
            [],
        )
        .unwrap();
    let mut socket = connect(&server, &token).await;
    assert_eq!(closed_with(&mut socket).await, Some(CloseCode::Error));
}

#[tokio::test]
async fn shutdown_closes_open_sockets() {
    let mut server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;
    let mut socket = connect(&server, &token).await;
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");
    server.stop().await;
    assert_eq!(closed_with(&mut socket).await, Some(CloseCode::Away));
    let _ = socket.send(Message::Close(None)).await;
}

/// dashboard.jsx `AlertPanel`: `msg.type === EVENT_TYPE.HA_STATUS` replaces the panel's
/// provisioning list with `msg.features`; `GET /api/ha/status` gives `calendar_url`.
#[tokio::test]
async fn a_new_home_assistant_status_reaches_open_dashboards() {
    let server = TestServer::start(Options::default()).await;
    let token = signed_in(&server).await;
    let mut socket = connect(&server, &token).await;
    assert_eq!(next_json(&mut socket).await["type"], "snapshot");

    server
        .ha_status
        .send(HaStatus {
            features: [(
                "calendar".to_owned(),
                FeatureStatus {
                    ok: false,
                    detail: Some("Local Calendar is not installed".to_owned()),
                },
            )]
            .into(),
            calendar_url: Some("http://ha.invalid/calendar".to_owned()),
        })
        .unwrap();
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type": "ha_status", "features": {
            "calendar": {"ok": false, "detail": "Local Calendar is not installed"}}})
    );
    let (status, body) = server.get("/api/ha/status", Some(&token)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({"features": {"calendar": {"ok": false, "detail": "Local Calendar is not installed"}},
               "calendar_url": "http://ha.invalid/calendar"})
    );
}
