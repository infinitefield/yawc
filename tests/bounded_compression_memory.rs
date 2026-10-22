#![cfg(not(target_arch = "wasm32"))]

//! While the transport accepts nothing, sending a large compressed message holds
//! about one fragment of compressed output, and the message still completes once
//! the transport drains.
//!
//! This file holds a single test because the counting allocator is process-wide.
mod common;

use std::{
    io::Result as IoResult,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::io::{empty, join, sink, AsyncWrite};
use yawc::{Frame, Options, Role, WebSocket};

const EXTENSIONS: Option<&str> = Some("permessage-deflate");
const MESSAGE_SIZE: usize = 4 << 20;
const FRAGMENT_SIZE: usize = 16 * 1024;
/// The write buffer up to the backpressure boundary plus a frame, and the
/// compressor's output for one frame, with room for buffer growth. Compressing the
/// whole message at once holds over 4 MiB.
const MAX_STALLED_BYTES: isize = 128 * 1024;

/// A writer that accepts `budget` more bytes and keeps them, then stalls.
#[derive(Clone, Default)]
struct Throttle {
    written: Arc<Mutex<Vec<u8>>>,
    budget: Arc<AtomicUsize>,
}

impl AsyncWrite for Throttle {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        let len = self.budget.load(Ordering::SeqCst).min(buf.len());
        if len == 0 {
            return Poll::Pending;
        }
        self.budget.fetch_sub(len, Ordering::SeqCst);
        self.written
            .lock()
            .expect("the capture is not poisoned")
            .extend_from_slice(&buf[..len]);
        Poll::Ready(Ok(len))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

fn incompressible(size: usize) -> Bytes {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let bytes: Vec<u8> = (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            b'!' + (state % 90) as u8
        })
        .collect();
    Bytes::from(bytes)
}

#[tokio::test]
async fn a_stalled_writer_holds_about_one_compressed_fragment() {
    let throttle = Throttle::default();
    throttle.budget.store(usize::MAX, Ordering::SeqCst);
    let mut server = WebSocket::from_stream_with_extensions(
        join(empty(), throttle.clone()),
        Role::Server,
        EXTENSIONS,
        Options::default()
            .with_low_latency_compression()
            .with_max_fragment_size(FRAGMENT_SIZE),
    )
    .expect("the server accepts permessage-deflate");
    // Builds the deflate state, so only the message's buffers are measured.
    server
        .send(Frame::text("hello"))
        .await
        .expect("the writer accepts it");
    let message = incompressible(MESSAGE_SIZE);
    // Room for the frames buffered by a growing Vec below, so its growth is not
    // counted.
    throttle
        .written
        .lock()
        .expect("the capture is not poisoned")
        .reserve(2 * MESSAGE_SIZE);

    let before = common::live_bytes();
    throttle.budget.store(64 * 1024, Ordering::SeqCst);
    // The writer stalls partway through; the send future is dropped while pending.
    assert!(server
        .send(Frame::text(message.clone()))
        .now_or_never()
        .is_none());
    let stalled = common::live_bytes() - before;
    assert!(
        stalled < MAX_STALLED_BYTES,
        "a stalled send holds {stalled} bytes"
    );

    // Dropping the send future neither lost nor repeated any output.
    throttle.budget.store(usize::MAX, Ordering::SeqCst);
    server
        .flush()
        .await
        .expect("the writer accepts everything now");
    let wire = throttle
        .written
        .lock()
        .expect("the capture is not poisoned")
        .clone();
    let mut client = WebSocket::from_stream_with_extensions(
        join(wire.as_slice(), sink()),
        Role::Client,
        EXTENSIONS,
        Options::default()
            .with_low_latency_compression()
            .with_max_payload_read(2 * MESSAGE_SIZE),
    )
    .expect("the client accepts permessage-deflate");
    assert_eq!(client.next().await.expect("hello").as_str(), "hello");
    let received = client.next().await.expect("the message arrives");
    assert_eq!(received.payload(), &message);
}
