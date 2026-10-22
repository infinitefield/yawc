#![cfg(not(target_arch = "wasm32"))]

//! A connection that negotiated permessage-deflate must not allocate deflate state
//! before it compresses or decompresses anything.
//!
//! This file holds a single test because the counting allocator is process-wide.
mod common;

use futures::{SinkExt, StreamExt};
use tokio::io::duplex;
use yawc::{Frame, Options, Role, WebSocket};

/// Two 8 KiB I/O buffers plus small bookkeeping. Eager deflate state is over 300 KiB.
const MAX_OPEN_BYTES: isize = 32 * 1024;

#[tokio::test]
async fn deflate_state_is_built_on_first_use() {
    let extensions = Some("permessage-deflate");
    let options = Options::default().with_low_latency_compression();
    let (client, server) = duplex(64 * 1024);

    let before = common::live_bytes();
    let mut server =
        WebSocket::from_stream_with_extensions(server, Role::Server, extensions, options.clone())
            .expect("the server accepts permessage-deflate");
    let opened = common::live_bytes() - before;
    assert!(
        opened < MAX_OPEN_BYTES,
        "opening a deflate connection allocated {opened} bytes"
    );

    // The lazily built state still works in both directions.
    let mut client =
        WebSocket::from_stream_with_extensions(client, Role::Client, extensions, options)
            .expect("the client accepts permessage-deflate");
    client
        .send(Frame::text("request"))
        .await
        .expect("client sends");
    let request = server.next().await.expect("server receives");
    assert_eq!(request.as_str(), "request");
    server
        .send(Frame::text("reply"))
        .await
        .expect("server sends");
    let reply = client.next().await.expect("client receives");
    assert_eq!(reply.as_str(), "reply");
}
