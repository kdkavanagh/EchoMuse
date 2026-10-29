//! The asset socket (spec §3.1.9, docs/protocol-v1.md §6): one request at a time,
//! `{sha256, offset}`, answered with binary chunks of at most 64 KiB from `offset` and
//! `{sha256, size, done: true}`, or `{"error": "not_found"}`.
//!
//! The socket belongs to a live session but does not keep it alive: it is served until
//! it closes or the session ends, and the device redials it after a failure. Anything
//! the device sends while a reply is still being written breaks the one-request rule
//! and closes the socket.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use echomuse_protocol::Sha256;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::admission::DynAssets;
use crate::config::MAX_ASSET_CHUNK_BYTES;
use crate::control::{Ws, WsSink, WsStream};

struct AssetRequest {
    sha256: Sha256,
    offset: u64,
}

impl AssetRequest {
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let sha256 = Sha256::parse(value.get("sha256")?.as_str()?)?;
        let offset = value.get("offset")?.as_u64()?;
        Some(Self { sha256, offset })
    }
}

/// The socket closed, failed, stalled, or broke the one-request rule.
struct SocketStopped;

pub(crate) async fn serve_assets(
    source: Arc<dyn DynAssets>,
    ws: Ws,
    session_stop: CancellationToken,
    write: Duration,
) {
    let (mut sink, mut stream) = ws.split();
    loop {
        let message = tokio::select! {
            biased;
            _ = session_stop.cancelled() => break,
            message = stream.next() => message,
        };
        let request = match message {
            Some(Ok(Message::Text(text))) => match AssetRequest::parse(text.as_str()) {
                Some(request) => request,
                None => {
                    tracing::info!("closing an asset socket after a malformed request");
                    break;
                }
            },
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
            _ => break,
        };
        let served = tokio::select! {
            biased;
            _ = session_stop.cancelled() => break,
            served = serve_one(source.as_ref(), request, &mut sink, &mut stream, write) => served,
        };
        if served.is_err() {
            break;
        }
    }
    let _ = timeout(write, sink.send(Message::Close(None))).await;
}

async fn serve_one(
    source: &dyn DynAssets,
    request: AssetRequest,
    sink: &mut WsSink,
    stream: &mut WsStream,
    write: Duration,
) -> Result<(), SocketStopped> {
    let AssetRequest { sha256, offset } = request;
    let content = match source.load(sha256.clone()).await {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(%sha256, %error, "could not load an asset; closing the asset socket");
            return Err(SocketStopped);
        }
    };
    let content = match content {
        Some(content) if hashes_to(&content, &sha256).await => content,
        Some(_) => {
            tracing::warn!(%sha256, "an asset's content does not hash to its name; answering not_found");
            return send(sink, stream, text(json!({"error": "not_found"})), write).await;
        }
        None => return send(sink, stream, text(json!({"error": "not_found"})), write).await,
    };
    let size = content.len();
    // An offset past the end sends nothing; the device then sees the size disagree with
    // its partial file and starts over.
    let mut position = usize::try_from(offset).unwrap_or(usize::MAX).min(size);
    while position < size {
        let end = (position + MAX_ASSET_CHUNK_BYTES).min(size);
        send(
            sink,
            stream,
            Message::Binary(content.slice(position..end)),
            write,
        )
        .await?;
        position = end;
    }
    let done = json!({"sha256": sha256.as_str(), "size": size, "done": true});
    send(sink, stream, text(done), write).await
}

fn text(value: Value) -> Message {
    Message::text(value.to_string())
}

/// Hashes on a blocking thread: an asset can be tens of megabytes.
async fn hashes_to(content: &Bytes, sha256: &Sha256) -> bool {
    let content = content.clone();
    match tokio::task::spawn_blocking(move || Sha256::of(&content)).await {
        Ok(actual) => actual == *sha256,
        Err(_) => false,
    }
}

/// Writes one frame while watching the reading half: any data the device sent before
/// the reply ends, already buffered or arriving during the write, is a second request.
async fn send(
    sink: &mut WsSink,
    stream: &mut WsStream,
    message: Message,
    write: Duration,
) -> Result<(), SocketStopped> {
    // Let the I/O driver run first, so a request the device already sent is seen even
    // while every write completes at once.
    tokio::task::yield_now().await;
    let sending = timeout(write, sink.send(message));
    tokio::pin!(sending);
    loop {
        tokio::select! {
            biased;
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                Some(Ok(Message::Text(_) | Message::Binary(_))) => {
                    tracing::info!("closing an asset socket that sent a request before the reply ended");
                    return Err(SocketStopped);
                }
                _ => return Err(SocketStopped),
            },
            sent = &mut sending => return match sent {
                Ok(Ok(())) => Ok(()),
                _ => Err(SocketStopped),
            },
        }
    }
}
