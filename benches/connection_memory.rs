//! Measures the memory an idle server connection holds, per connection.
//!
//! Opens many server-side connections over an in-memory stream that allocates
//! nothing itself, then reports live heap bytes (from a counting allocator) and
//! anonymous resident memory (from `/proc`, Linux only) after each phase:
//!
//! - `open`: the connection was built, as right after the handshake.
//! - `recv`: it read one small message from the client.
//! - `send`: it sent one small message back.
//!
//! cargo bench --bench connection_memory
//! cargo bench --bench connection_memory --features zlib
//! CONNECTIONS=10000 cargo bench --bench connection_memory
use std::{
    alloc::{GlobalAlloc, Layout, System},
    env, fs,
    io::Result as IoResult,
    pin::Pin,
    sync::{
        atomic::{AtomicIsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

use bytes::{Buf, Bytes};
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use yawc::{Frame, OpCode, Options, Role, WebSocket};

struct CountingAllocator;

static LIVE: AtomicIsize = AtomicIsize::new(0);

fn add_live(bytes: usize) {
    LIVE.fetch_add(bytes as isize, Ordering::Relaxed);
}

fn sub_live(bytes: usize) {
    LIVE.fetch_sub(bytes as isize, Ordering::Relaxed);
}

// SAFETY: Every operation delegates to System with the original pointer and layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        add_live(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        add_live(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        sub_live(layout.size());
        add_live(size);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        sub_live(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// A small client request, shaped like a Nostr `REQ`.
const REQUEST: &str = r#"["REQ","subscription-1",{"kinds":[1],"limit":10}]"#;
/// A small server reply, shaped like a Nostr `EOSE`.
const REPLY: &str = r#"["EOSE","subscription-1"]"#;
const DEFAULT_CONNECTIONS: usize = 1_000;

struct Mode {
    name: &'static str,
    extensions: Option<&'static str>,
    compression_threshold: usize,
    buffer_capacity: Option<usize>,
}

const MODES: [Mode; 5] = [
    Mode {
        name: "no compression",
        extensions: None,
        compression_threshold: 0,
        buffer_capacity: None,
    },
    Mode {
        name: "deflate",
        extensions: Some("permessage-deflate"),
        compression_threshold: 0,
        buffer_capacity: None,
    },
    Mode {
        name: "deflate, no context takeover",
        extensions: Some(
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
        ),
        compression_threshold: 0,
        buffer_capacity: None,
    },
    Mode {
        name: "deflate, threshold 64",
        extensions: Some("permessage-deflate"),
        compression_threshold: 64,
        buffer_capacity: None,
    },
    Mode {
        name: "deflate, threshold 64, 1 KiB",
        extensions: Some("permessage-deflate"),
        compression_threshold: 64,
        buffer_capacity: Some(1024),
    },
];

/// An in-memory stream: reads return `incoming` once, writes are dropped
/// unless `captured` is set.
///
/// It allocates nothing on its own, so every byte counted belongs to yawc.
struct Wire {
    incoming: Bytes,
    captured: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Wire {
    fn reading(incoming: Bytes) -> Self {
        Self {
            incoming,
            captured: None,
        }
    }

    fn capturing(captured: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            incoming: Bytes::new(),
            captured: Some(captured),
        }
    }
}

impl AsyncRead for Wire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<IoResult<()>> {
        if self.incoming.is_empty() {
            // An idle peer: nothing more to read, and nothing will wake us.
            return Poll::Pending;
        }
        let len = self.incoming.len().min(buf.remaining());
        buf.put_slice(&self.incoming[..len]);
        self.incoming.advance(len);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Wire {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        if let Some(captured) = &self.captured {
            captured
                .lock()
                .expect("the capture buffer is only used on this thread")
                .extend_from_slice(buf);
        }
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

fn options(mode: &Mode) -> Options {
    let options = Options::default()
        .with_low_latency_compression()
        .with_compression_threshold(mode.compression_threshold);
    match mode.buffer_capacity {
        Some(capacity) => options
            .with_read_buffer_capacity(capacity)
            .with_write_buffer_capacity(capacity),
        None => options,
    }
}

/// The bytes a client sends for `REQUEST` under `mode`, masked and compressed.
///
/// The client compresses every message, as browsers do, whatever threshold the
/// server uses.
fn client_request(mode: &Mode) -> Bytes {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let wire = Wire::capturing(Arc::clone(&captured));
    let client_options = Options::default().with_low_latency_compression();
    let mut client =
        WebSocket::from_stream_with_extensions(wire, Role::Client, mode.extensions, client_options)
            .expect("the client accepts the extensions it is given");
    client
        .send(Frame::text(REQUEST))
        .now_or_never()
        .expect("writes to the in-memory wire complete at once")
        .expect("sending to the in-memory wire cannot fail");
    drop(client);
    let captured = captured
        .lock()
        .expect("the capture buffer is only used on this thread");
    Bytes::copy_from_slice(&captured)
}

/// Anonymous resident memory of this process in KiB, from `/proc` (Linux only).
fn rss_anon_kib() -> Option<isize> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("RssAnon:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

struct Snapshot {
    heap: isize,
    rss_kib: Option<isize>,
}

impl Snapshot {
    fn take() -> Self {
        Self {
            heap: LIVE.load(Ordering::Relaxed),
            rss_kib: rss_anon_kib(),
        }
    }

    fn report(&self, base: &Self, mode: &Mode, phase: &str, connections: usize) {
        let per = connections as f64;
        let heap = (self.heap - base.heap) as f64 / 1024.0 / per;
        let rss = match (self.rss_kib, base.rss_kib) {
            (Some(now), Some(then)) => format!("{:10.1}", (now - then) as f64 / per),
            _ => format!("{:>10}", "n/a"),
        };
        println!("{:<30} {phase:<6} {heap:10.1} {rss}", mode.name);
    }
}

fn measure(mode: &Mode, connections: usize) {
    let request = client_request(mode);
    let mut wires: Vec<Wire> = (0..connections)
        .map(|_| Wire::reading(request.clone()))
        .collect();
    let mut servers = Vec::with_capacity(connections);

    let base = Snapshot::take();
    for wire in wires.drain(..) {
        let server = WebSocket::from_stream_with_extensions(
            wire,
            Role::Server,
            mode.extensions,
            options(mode),
        )
        .expect("the server accepts the extensions it is given");
        servers.push(server);
    }
    Snapshot::take().report(&base, mode, "open", connections);

    for server in &mut servers {
        let frame = server
            .next()
            .now_or_never()
            .expect("the request is already on the wire")
            .expect("the connection is open");
        assert_eq!(frame.opcode(), OpCode::Text);
        assert_eq!(frame.as_str(), REQUEST);
    }
    Snapshot::take().report(&base, mode, "recv", connections);

    for server in &mut servers {
        server
            .send(Frame::text(REPLY))
            .now_or_never()
            .expect("writes to the in-memory wire complete at once")
            .expect("sending to the in-memory wire cannot fail");
    }
    Snapshot::take().report(&base, mode, "send", connections);
}

fn main() {
    let connections = env::var("CONNECTIONS")
        .ok()
        .map(|value| value.parse().expect("CONNECTIONS must be a number"))
        .unwrap_or(DEFAULT_CONNECTIONS);

    println!("{connections} connections, KiB per connection");
    println!(
        "{:<30} {:<6} {:>10} {:>10}",
        "mode", "phase", "heap", "rss_anon"
    );
    for mode in &MODES {
        measure(mode, connections);
    }
}
