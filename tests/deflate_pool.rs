//! Connections without context takeover that share a `DeflatePool` hold no deflate
//! state between messages.
//!
//! This file holds a single test because the counting allocator is process-wide.
mod common;

use futures::{SinkExt, StreamExt};
use tokio::io::duplex;
use yawc::{DeflatePool, Frame, Options, Role, WebSocket};

const NO_CONTEXT: Option<&str> =
    Some("permessage-deflate; server_no_context_takeover; client_no_context_takeover");
const PAIRS: usize = 16;
/// Buffers for 32 connections, plus one compressor and one decompressor in the pool.
/// Without the pool, 32 connections hold over 10 MiB of deflate state.
const MAX_BYTES: isize = 2 * 1024 * 1024;

#[tokio::test]
async fn pooled_connections_hold_no_deflate_state_between_messages() {
    let pool = DeflatePool::new(4);
    let options = Options::default()
        .with_low_latency_compression()
        .with_deflate_pool(pool);
    let streams: Vec<_> = (0..PAIRS).map(|_| duplex(16 * 1024)).collect();

    let before = common::live_bytes();
    let mut pairs = Vec::with_capacity(PAIRS);
    for (server_io, client_io) in streams {
        let server = WebSocket::from_stream_with_extensions(
            server_io,
            Role::Server,
            NO_CONTEXT,
            options.clone(),
        )
        .expect("the server accepts the extensions");
        let client = WebSocket::from_stream_with_extensions(
            client_io,
            Role::Client,
            NO_CONTEXT,
            options.clone(),
        )
        .expect("the client accepts the extensions");
        pairs.push((server, client));
    }

    let request = r#"["REQ","subscription-1",{"kinds":[1],"limit":10}]"#;
    let reply = r#"["EOSE","subscription-1"]"#;
    for (server, client) in &mut pairs {
        client.send(Frame::text(request)).await.unwrap();
        assert_eq!(
            server.next().await.expect("server receives").as_str(),
            request
        );
        server.send(Frame::text(reply)).await.unwrap();
        assert_eq!(
            client.next().await.expect("client receives").as_str(),
            reply
        );
    }

    let held = common::live_bytes() - before;
    assert!(
        held < MAX_BYTES,
        "{} pooled connections hold {held} bytes",
        PAIRS * 2
    );
}
