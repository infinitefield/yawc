#![cfg(not(target_arch = "wasm32"))]

//! A connection gives back the memory one large message made its buffers grow to.
//!
//! This file holds a single test because the counting allocator is process-wide.
mod common;

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::io::duplex;
use yawc::{Frame, Options, Role, WebSocket};

const LARGE_SIZE: usize = 256 * 1024;
/// Room for allocator noise. Keeping the buffers a large message grew costs both
/// connections well over 1 MiB.
const MAX_KEPT_BYTES: isize = 32 * 1024;

/// Text that compresses about as well as JSON does, so deflate output stays large.
fn large_payload() -> Bytes {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let text: String = (0..LARGE_SIZE / 16)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            format!("{state:016x}")
        })
        .collect();
    Bytes::from(text)
}

#[tokio::test]
async fn buffers_shrink_after_a_large_message() {
    let extensions = Some("permessage-deflate");
    let options = Options::default().with_low_latency_compression();
    // The pipe keeps what it grows to as well, so keep it small.
    let (client_io, server_io) = duplex(4 * 1024);
    let mut server = WebSocket::from_stream_with_extensions(
        server_io,
        Role::Server,
        extensions,
        options.clone(),
    )
    .expect("the server accepts permessage-deflate");
    // The client fragments, so the server also assembles the message.
    let mut client = WebSocket::from_stream_with_extensions(
        client_io,
        Role::Client,
        extensions,
        options.with_max_fragment_size(16 * 1024),
    )
    .expect("the client accepts permessage-deflate");
    let large = large_payload();

    // Build the deflate state on both sides first, so only buffers are measured.
    client
        .send(Frame::text("hello"))
        .await
        .expect("client sends");
    assert_eq!(
        server.next().await.expect("server receives").as_str(),
        "hello"
    );
    server
        .send(Frame::text("hello"))
        .await
        .expect("server sends");
    assert_eq!(
        client.next().await.expect("client receives").as_str(),
        "hello"
    );
    let before = common::live_bytes();

    let sender = tokio::spawn(async move {
        client
            .send(Frame::text(large_payload()))
            .await
            .expect("client sends");
        client
    });
    let received = server.next().await.expect("server receives");
    assert_eq!(received.payload(), &large);
    drop(received);
    let mut client = sender.await.expect("the client task finishes");

    let reply = tokio::spawn(async move {
        server
            .send(Frame::text(large_payload()))
            .await
            .expect("server sends");
        server
    });
    let received = client.next().await.expect("client receives");
    assert_eq!(received.payload(), &large);
    drop(received);
    let server = reply.await.expect("the server task finishes");

    let kept = common::live_bytes() - before;
    assert!(
        kept < MAX_KEPT_BYTES,
        "the two connections kept {kept} bytes after one large message each way"
    );
    drop((client, server));
}
