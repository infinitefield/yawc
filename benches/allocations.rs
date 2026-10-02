//! Counts allocator calls after warming up reusable connections and buffers.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    env,
    hint::black_box,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use bytes::{Bytes, BytesMut};
use futures::SinkExt;
use tokio::{io::duplex, runtime::Builder};
use tokio_util::codec::{Decoder as _, Encoder as _};
use yawc::{
    codec::{Decoder, Encoder},
    Frame, Options, Role, WebSocket,
};

struct CountingAllocator;

static COUNTING: AtomicBool = AtomicBool::new(false);
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static REALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Every operation delegates to System with the original pointer and layout.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.load(Ordering::Relaxed) {
            REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            ALLOCATED_BYTES.fetch_add(size, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

const WARMUP: usize = 1024;
const ITERATIONS: usize = 4096;

fn start_counting() {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    REALLOCATIONS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);
    COUNTING.store(true, Ordering::Relaxed);
}

fn report(case: &str, size: usize, window: usize, assert_zero: bool) {
    COUNTING.store(false, Ordering::Relaxed);
    println!(
        "{{\"case\":\"{case}\",\"payload_bytes\":{size},\"messages\":{},\"window\":{window},\"allocations\":{},\"reallocations\":{},\"allocated_bytes\":{}}}",
        ITERATIONS * window,
        ALLOCATIONS.load(Ordering::Relaxed),
        REALLOCATIONS.load(Ordering::Relaxed),
        ALLOCATED_BYTES.load(Ordering::Relaxed),
    );
    if assert_zero {
        assert_eq!(
            ALLOCATIONS.load(Ordering::Relaxed),
            0,
            "{case}/{size}/{window}: allocations"
        );
        assert_eq!(
            REALLOCATIONS.load(Ordering::Relaxed),
            0,
            "{case}/{size}/{window}: reallocations"
        );
    }
}

fn main() {
    let assert_zero = env::args().any(|arg| arg == "--assert-zero");
    start_counting();
    let mut probe = Vec::with_capacity(8);
    black_box(&mut probe).resize(64, 0u8);
    black_box(probe);
    COUNTING.store(false, Ordering::Relaxed);
    assert!(
        ALLOCATIONS.load(Ordering::Relaxed) > 0,
        "allocator instrumentation must observe allocations"
    );
    assert!(
        REALLOCATIONS.load(Ordering::Relaxed) > 0,
        "allocator instrumentation must observe reallocations"
    );

    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    for size in [20, 1024, 16384, 65536] {
        let payload = Bytes::from(vec![0x5a; size]);
        let mut encoder = Encoder::new(Role::Client);
        let mut decoder = Decoder::new(Role::Server, 1024 * 1024);
        let mut wire = BytesMut::with_capacity(size + 16);
        for iteration in 0..WARMUP + ITERATIONS {
            if iteration == WARMUP {
                start_counting();
            }
            encoder
                .encode(Frame::binary(payload.clone()), &mut wire)
                .unwrap();
            let frame = decoder.decode(&mut wire).unwrap().unwrap();
            assert_eq!(frame.payload(), &payload);
            black_box(frame);
        }
        report("codec", size, 1, assert_zero);

        for window in [1, 16] {
            let (client, server) = duplex((size + 16) * window * 2);
            let options = Options::default()
                .with_read_buffer_capacity(128 * 1024)
                .with_backpressure_boundary(64 * 1024);
            let mut client = WebSocket::from_stream(client, Role::Client, options.clone()).unwrap();
            let mut server = WebSocket::from_stream(server, Role::Server, options).unwrap();
            runtime.block_on(async {
                for iteration in 0..WARMUP + ITERATIONS {
                    if iteration == WARMUP {
                        start_counting();
                    }
                    for _ in 0..window {
                        client.feed(Frame::binary(payload.clone())).await.unwrap();
                    }
                    client.flush().await.unwrap();
                    for _ in 0..window {
                        let frame = server.next_frame().await.unwrap();
                        server.feed(frame).await.unwrap();
                    }
                    server.flush().await.unwrap();
                    for _ in 0..window {
                        let frame = client.next_frame().await.unwrap();
                        assert_eq!(frame.payload(), &payload);
                        black_box(frame);
                    }
                }
            });
            report("echo", size, window, assert_zero);
        }

        let (client, server) = duplex((size + 16) * 4);
        let mut client = WebSocket::from_stream(client, Role::Client, Options::default()).unwrap();
        let mut server = WebSocket::from_stream(server, Role::Server, Options::default()).unwrap();
        runtime.block_on(async {
            for iteration in 0..WARMUP + ITERATIONS {
                if iteration == WARMUP {
                    start_counting();
                }
                client
                    .feed(Frame::binary(payload.slice(..size / 2)).with_fin(false))
                    .await
                    .unwrap();
                client
                    .feed(Frame::continuation(payload.slice(size / 2..)))
                    .await
                    .unwrap();
                client.flush().await.unwrap();
                let frame = server.next_frame().await.unwrap();
                assert_eq!(frame.payload(), &payload);
                server.send(frame).await.unwrap();
                let frame = client.next_frame().await.unwrap();
                assert_eq!(frame.payload(), &payload);
                black_box(frame);
            }
        });
        report("fragmented_echo", size, 1, assert_zero);
    }
}
