//! Measures the memory an idle server connection holds, per connection.
//!
//! Opens many server-side connections over an in-memory stream that allocates
//! nothing itself, then reports live heap bytes (from a counting allocator) and
//! anonymous resident memory (from `/proc`, Linux only) after each phase:
//!
//! - `open`: the connection was built, as right after the handshake.
//! - `recv`: it read one small message from the client.
//! - `send`: it sent one small message back.
//! - `recv big`: it read one 256 KiB message from the client.
//! - `send big`: it sent one 256 KiB message back.
//!
//! cargo bench --bench connection_memory
//! cargo bench --bench connection_memory --features zlib
//! CONNECTIONS=10000 cargo bench --bench connection_memory
use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::VecDeque,
    env, fs,
    io::Result as IoResult,
    mem,
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
use yawc::{DeflatePool, Frame, OpCode, Options, Role, WebSocket};

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
/// The size of the large message each side sends once, within the limits relays use.
const LARGE_SIZE: usize = 256 * 1024;

struct Mode {
    name: &'static str,
    extensions: Option<&'static str>,
    compression_threshold: usize,
    buffer_capacity: Option<usize>,
    pooled: bool,
}

const MODES: [Mode; 6] = [
    Mode {
        name: "no compression",
        extensions: None,
        compression_threshold: 0,
        buffer_capacity: None,
        pooled: false,
    },
    Mode {
        name: "deflate",
        extensions: Some("permessage-deflate"),
        compression_threshold: 0,
        buffer_capacity: None,
        pooled: false,
    },
    Mode {
        name: "deflate, no context takeover",
        extensions: Some(
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
        ),
        compression_threshold: 0,
        buffer_capacity: None,
        pooled: false,
    },
    Mode {
        name: "deflate, threshold 64",
        extensions: Some("permessage-deflate"),
        compression_threshold: 64,
        buffer_capacity: None,
        pooled: false,
    },
    Mode {
        name: "deflate, threshold 64, 1 KiB",
        extensions: Some("permessage-deflate"),
        compression_threshold: 64,
        buffer_capacity: Some(1024),
        pooled: false,
    },
    Mode {
        name: "no context takeover, pool, 1 KiB",
        extensions: Some(
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover",
        ),
        compression_threshold: 0,
        buffer_capacity: Some(1024),
        pooled: true,
    },
];

/// An in-memory stream: reads return the `incoming` chunks once, writes are
/// dropped unless `captured` is set.
///
/// A read never spans two chunks, so each phase reads only its own message. The
/// stream allocates nothing once built, so every byte counted belongs to yawc.
struct Wire {
    incoming: VecDeque<Bytes>,
    captured: Option<Arc<Mutex<Vec<u8>>>>,
}

impl Wire {
    fn reading(incoming: &[Bytes]) -> Self {
        Self {
            incoming: incoming.iter().cloned().collect(),
            captured: None,
        }
    }

    fn capturing(captured: Arc<Mutex<Vec<u8>>>) -> Self {
        Self {
            incoming: VecDeque::new(),
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
        while self.incoming.front().is_some_and(Bytes::is_empty) {
            self.incoming.pop_front();
        }
        let Some(chunk) = self.incoming.front_mut() else {
            // An idle peer: nothing more to read, and nothing will wake us.
            return Poll::Pending;
        };
        let len = chunk.len().min(buf.remaining());
        buf.put_slice(&chunk[..len]);
        chunk.advance(len);
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

/// Server options for `mode`. Clones share one pool, as connections would.
fn options(mode: &Mode) -> Options {
    let mut options = Options::default()
        .with_low_latency_compression()
        .with_compression_threshold(mode.compression_threshold);
    if mode.pooled {
        options = options.with_deflate_pool(DeflatePool::new(4));
    }
    match mode.buffer_capacity {
        Some(capacity) => options
            .with_read_buffer_capacity(capacity)
            .with_write_buffer_capacity(capacity),
        None => options,
    }
}

/// A large text payload that compresses about as well as JSON events do.
fn large_payload() -> Bytes {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let text: String = (0..LARGE_SIZE / 16)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            format!("{:016x}", state)
        })
        .collect();
    Bytes::from(text)
}

/// The bytes a client sends under `mode`, masked and compressed: first
/// `REQUEST`, then `large`, one chunk per message.
///
/// The client compresses every message, as browsers do, whatever threshold the
/// server uses.
fn client_messages(mode: &Mode, large: &Bytes) -> [Bytes; 2] {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let wire = Wire::capturing(Arc::clone(&captured));
    let client_options = Options::default().with_low_latency_compression();
    let mut client =
        WebSocket::from_stream_with_extensions(wire, Role::Client, mode.extensions, client_options)
            .expect("the client accepts the extensions it is given");
    [Frame::text(REQUEST), Frame::text(large.clone())].map(|frame| {
        client
            .send(frame)
            .now_or_never()
            .expect("writes to the in-memory wire complete at once")
            .expect("sending to the in-memory wire cannot fail");
        let mut captured = captured
            .lock()
            .expect("the capture buffer is only used on this thread");
        Bytes::from(mem::take(&mut *captured))
    })
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
        println!("{:<34} {phase:<8} {heap:10.1} {rss}", mode.name);
    }
}

fn measure(mode: &Mode, large: &Bytes, connections: usize) {
    let messages = client_messages(mode, large);
    let mut wires: Vec<Wire> = (0..connections).map(|_| Wire::reading(&messages)).collect();
    let mut servers = Vec::with_capacity(connections);
    let options = options(mode);

    let base = Snapshot::take();
    for wire in wires.drain(..) {
        let server = WebSocket::from_stream_with_extensions(
            wire,
            Role::Server,
            mode.extensions,
            options.clone(),
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

    for server in &mut servers {
        let frame = server
            .next()
            .now_or_never()
            .expect("the large message is already on the wire")
            .expect("the connection is open");
        assert_eq!(frame.payload().len(), large.len());
    }
    Snapshot::take().report(&base, mode, "recv big", connections);

    for server in &mut servers {
        server
            .send(Frame::text(large.clone()))
            .now_or_never()
            .expect("writes to the in-memory wire complete at once")
            .expect("sending to the in-memory wire cannot fail");
    }
    Snapshot::take().report(&base, mode, "send big", connections);
}

fn main() {
    let connections = env::var("CONNECTIONS")
        .ok()
        .map(|value| value.parse().expect("CONNECTIONS must be a number"))
        .unwrap_or(DEFAULT_CONNECTIONS);

    println!("{connections} connections, KiB per connection");
    println!(
        "{:<34} {:<8} {:>10} {:>10}",
        "mode", "phase", "heap", "rss_anon"
    );
    let large = large_payload();
    for mode in &MODES {
        measure(mode, &large, connections);
    }
}
