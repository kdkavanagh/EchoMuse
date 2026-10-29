//! The asset socket and the shell dial-back (spec §3.1.9).

use std::time::Duration;

use bytes::Bytes;
use echomuse_core::types::DeviceId;
use echomuse_protocol::Sha256;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use super::{Client, Harness, Route, TestAssets, closes, next_binary, refused_status, test_config};
use crate::{ShellInput, ShellMode, ShellOpen};

/// 150 KiB of patterned bytes: two full chunks and a partial one.
fn content() -> Bytes {
    (0..150 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<u8>>()
        .into()
}

async fn assets_harness() -> (Harness, Sha256, Sha256) {
    let good = content();
    let good_sha = Sha256::of(&good);
    let corrupt_sha = Sha256::of(b"what the file should have held");
    let mut assets = TestAssets::default();
    assets.files.insert(good_sha.clone(), good);
    assets
        .files
        .insert(corrupt_sha.clone(), Bytes::from_static(b"bit rot"));
    (
        Harness::with(test_config(), assets).await,
        good_sha,
        corrupt_sha,
    )
}

async fn request(ws: &mut Client, sha: &Sha256, offset: u64) {
    let text = json!({"sha256": sha.as_str(), "offset": offset}).to_string();
    ws.send(Message::text(text)).await.unwrap();
}

/// Reads binary chunks until the closing text frame.
async fn reply(ws: &mut Client) -> (Vec<Bytes>, Value) {
    let mut chunks = Vec::new();
    loop {
        match ws.next().await.unwrap().unwrap() {
            Message::Binary(chunk) => chunks.push(chunk),
            Message::Text(text) => return (chunks, serde_json::from_str(text.as_str()).unwrap()),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[tokio::test]
async fn assets_are_served_in_chunks_resumed_by_offset_or_not_found() {
    let (mut h, good, corrupt) = assets_harness().await;
    let device = h.approved("G0ASSET001").await;
    let (control, _admitted) = device.connect(&mut h, &Route::Tcp).await;
    let mut ws = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, control.session())
        .await
        .unwrap();

    request(&mut ws, &good, 0).await;
    let (chunks, done) = reply(&mut ws).await;
    assert_eq!(
        chunks.iter().map(Bytes::len).collect::<Vec<_>>(),
        [65536, 65536, 22528]
    );
    assert_eq!(chunks.concat(), content().to_vec());
    assert_eq!(
        done,
        json!({"sha256": good.as_str(), "size": 153600, "done": true})
    );

    request(&mut ws, &good, 100_000).await;
    let (chunks, done) = reply(&mut ws).await;
    assert_eq!(chunks.concat(), content()[100_000..].to_vec());
    assert_eq!(done["size"], 153600);

    // Past the end: nothing, then the true size.
    request(&mut ws, &good, 200_000).await;
    let (chunks, done) = reply(&mut ws).await;
    assert!(chunks.is_empty());
    assert_eq!(done["size"], 153600);

    let missing = Sha256::of(b"nothing");
    request(&mut ws, &missing, 0).await;
    assert_eq!(
        reply(&mut ws).await,
        (vec![], json!({"error": "not_found"}))
    );

    // A file that does not hash to its name is never served.
    request(&mut ws, &corrupt, 0).await;
    assert_eq!(
        reply(&mut ws).await,
        (vec![], json!({"error": "not_found"}))
    );
}

#[tokio::test]
async fn a_second_request_before_the_reply_ends_closes_the_asset_socket_only() {
    let (mut h, good, _) = assets_harness().await;
    let device = h.approved("G0ASSET002").await;
    let (control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let mut ws = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, control.session())
        .await
        .unwrap();
    request(&mut ws, &good, 0).await;
    request(&mut ws, &good, 0).await;
    let mut closed = false;
    let mut payload = 0;
    while let Some(message) = ws.next().await {
        match message {
            Ok(Message::Binary(chunk)) => payload += chunk.len(),
            Ok(Message::Text(text)) if text.as_str().contains("\"done\"") => break,
            Ok(Message::Close(_)) | Err(_) => {
                closed = true;
                break;
            }
            _ => {}
        }
    }
    assert!(closed, "closed after {payload} bytes");
    assert_eq!(
        admitted.session.link.end_reason(),
        None,
        "the session survives"
    );
    // A malformed request closes it too.
    let mut ws = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, control.session())
        .await
        .unwrap();
    ws.send(Message::text("{\"sha256\": 5}")).await.unwrap();
    assert!(closes(&mut ws).await);
}

#[tokio::test]
async fn a_pty_shell_frames_input_and_resizes_and_returns_output() {
    let mut h = Harness::start().await;
    let device = h.approved("G0SHELL001").await;
    let (mut control, admitted) = device.connect(&mut h, &Route::Tcp).await;
    let shells = h.server.shells();

    let pending = shells.expect(DeviceId(device.id.clone()));
    admitted
        .session
        .link
        .send(&ShellOpen { pty: true }, 0)
        .unwrap();
    let open = control.next_message().await.unwrap();
    assert_eq!(open["type"], "shell_open");
    assert_eq!(open["body"], json!({"pty": true}));

    // The device dials back with its token and ?pty=1.
    let path = format!("{}{}?pty=1", crate::PATH_SHELL_PREFIX, device.id);
    let token = device.token.clone().unwrap();
    let mut remote = h
        .dial(&Route::Tcp, &path, &[("X-EM-Token", &token)])
        .await
        .unwrap();
    let mut shell = tokio::time::timeout(Duration::from_secs(5), pending.connected())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(shell.mode(), ShellMode::Pty);

    shell
        .send(ShellInput::Data(Bytes::from_static(b"ls\n")))
        .await
        .unwrap();
    assert_eq!(
        next_binary(&mut remote).await.unwrap(),
        Bytes::from_static(b"\x00ls\n")
    );
    shell
        .send(ShellInput::Resize {
            cols: 300,
            rows: 40,
        })
        .await
        .unwrap();
    assert_eq!(
        next_binary(&mut remote).await.unwrap(),
        Bytes::from_static(&[0x01, 0x01, 0x2c, 0x00, 0x28])
    );
    remote
        .send(Message::Binary(Bytes::from_static(b"$ ")))
        .await
        .unwrap();
    assert_eq!(shell.recv().await.unwrap(), Bytes::from_static(b"$ "));

    // A new dial-back replaces the old shell.
    let next = shells.expect(DeviceId(device.id.clone()));
    let _replacement = h
        .dial(&Route::Tcp, &path, &[("X-EM-Token", &token)])
        .await
        .unwrap();
    next.connected().await.unwrap();
    assert_eq!(shell.recv().await, None);
}

#[tokio::test]
async fn a_pipe_shell_carries_raw_bytes_and_bridges() {
    let h = Harness::start().await;
    let device = h.approved("G0SHELL002").await;
    let shells = h.server.shells();
    let pending = shells.expect(DeviceId(device.id.clone()));
    let path = format!("{}{}", crate::PATH_SHELL_PREFIX, device.id);
    let token = device.token.clone().unwrap();
    let mut remote = h
        .dial(&Route::Tcp, &path, &[("X-EM-Token", &token)])
        .await
        .unwrap();
    let mut shell = pending.connected().await.unwrap();
    assert_eq!(shell.mode(), ShellMode::Pipe);
    assert_eq!(
        shell.send(ShellInput::Resize { cols: 1, rows: 1 }).await,
        Err(crate::ShellError::ResizeInPipeMode)
    );

    let input = futures_util::stream::iter([ShellInput::Data(Bytes::from_static(b"id\n"))])
        .chain(futures_util::stream::pending());
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel(8);
    let output = tokio_util::sync::PollSender::new(output_tx);
    let bridge = tokio::spawn(shell.bridge(input, output));
    assert_eq!(
        next_binary(&mut remote).await.unwrap(),
        Bytes::from_static(b"id\n")
    );
    remote
        .send(Message::Binary(Bytes::from_static(b"uid=0(root)\n")))
        .await
        .unwrap();
    assert_eq!(
        output_rx.recv().await.unwrap(),
        Bytes::from_static(b"uid=0(root)\n")
    );
    // The device's shell exiting ends the bridge.
    drop(remote);
    bridge.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_shell_nobody_asked_for_or_with_a_bad_token_is_refused() {
    let h = Harness::start().await;
    let device = h.approved("G0SHELL003").await;
    let path = format!("{}{}", crate::PATH_SHELL_PREFIX, device.id);
    let token = device.token.clone().unwrap();
    let refused = h.dial(&Route::Tcp, &path, &[("X-EM-Token", &token)]).await;
    assert_eq!(refused_status(refused), 403);

    let _pending = h.server.shells().expect(DeviceId(device.id.clone()));
    let refused = h
        .dial(&Route::Tcp, &path, &[("X-EM-Token", "forged")])
        .await;
    assert_eq!(refused_status(refused), 403);

    let unknown = h.dial(&Route::Tcp, "/device/v2/control", &[]).await;
    assert_eq!(refused_status(unknown), 404);
}
