//! A connection that negotiated permessage-deflate must not allocate deflate state
//! before it compresses or decompresses anything.
//!
//! This file holds a single test because the counting allocator is process-wide.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicIsize, Ordering},
};

use futures::{SinkExt, StreamExt};
use tokio::io::duplex;
use yawc::{Frame, Options, Role, WebSocket};

struct CountingAllocator;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: Every operation delegates to System with the original pointer and layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(size as isize - layout.size() as isize, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Two 8 KiB I/O buffers plus small bookkeeping. Eager deflate state is over 300 KiB.
const MAX_OPEN_BYTES: isize = 32 * 1024;

#[tokio::test]
async fn deflate_state_is_built_on_first_use() {
    let extensions = Some("permessage-deflate");
    let options = Options::default().with_low_latency_compression();
    let (client, server) = duplex(64 * 1024);

    let before = LIVE.load(Ordering::Relaxed);
    let mut server =
        WebSocket::from_stream_with_extensions(server, Role::Server, extensions, options.clone())
            .expect("the server accepts permessage-deflate");
    let opened = LIVE.load(Ordering::Relaxed) - before;
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
