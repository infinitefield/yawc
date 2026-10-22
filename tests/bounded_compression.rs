#![cfg(not(target_arch = "wasm32"))]

//! With compression and a fragment size, no compressed frame on the wire is larger
//! than the fragment size, and the frames still form one message.
use std::{
    io::{Cursor, Result as IoResult},
    pin::Pin,
    slice,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

use bytes::Bytes;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::io::{join, sink, AsyncWrite};
use yawc::{Frame, OpCode, Options, Role, WebSocket};

const CONTEXT: &str = "permessage-deflate";
const NO_CONTEXT: &str =
    "permessage-deflate; server_no_context_takeover; client_no_context_takeover";

/// Collects everything written to it.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("the capture is not poisoned").clone()
    }
}

impl AsyncWrite for Capture {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        self.0
            .lock()
            .expect("the capture is not poisoned")
            .extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug)]
struct WireFrame {
    fin: bool,
    rsv1: bool,
    opcode: u8,
    len: usize,
}

/// Parses the unmasked frames a server wrote.
fn wire_frames(mut wire: &[u8]) -> Vec<WireFrame> {
    let mut frames = Vec::new();
    while !wire.is_empty() {
        let (head, rest) = wire.split_at(2);
        let (len, rest) = match head[1] & 0x7f {
            126 => (u16::from_be_bytes([rest[0], rest[1]]) as usize, &rest[2..]),
            127 => {
                let len = u64::from_be_bytes(rest[..8].try_into().expect("eight bytes"));
                (usize::try_from(len).expect("fits in memory"), &rest[8..])
            }
            len => (usize::from(len), rest),
        };
        frames.push(WireFrame {
            fin: head[0] & 0x80 != 0,
            rsv1: head[0] & 0x40 != 0,
            opcode: head[0] & 0x0f,
            len,
        });
        wire = &rest[len..];
    }
    frames
}

/// Text that deflate cannot shrink.
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

fn compressible(size: usize) -> Bytes {
    let text = r#"{"kind":1,"content":"hello","tags":[]},"#;
    Bytes::from(text.repeat(size / text.len() + 1)[..size].to_owned())
}

/// Sends `messages` from a server with `fragment_size`, and returns what it wrote.
async fn send(extensions: &str, fragment_size: usize, messages: &[Bytes]) -> Vec<u8> {
    let capture = Capture::default();
    let options = Options::default()
        .with_low_latency_compression()
        .with_max_fragment_size(fragment_size);
    let mut server = WebSocket::from_stream_with_extensions(
        join(tokio::io::empty(), capture.clone()),
        Role::Server,
        Some(extensions),
        options,
    )
    .expect("the server accepts the extensions");
    for message in messages {
        server
            .send(Frame::text(message.clone()))
            .await
            .expect("the capture accepts every write");
    }
    capture.bytes()
}

/// Reads every message in `wire` as a client would.
async fn receive(extensions: &str, wire: Vec<u8>, count: usize) -> Vec<Bytes> {
    let options = Options::default()
        .with_low_latency_compression()
        .with_max_payload_read(16 << 20);
    let mut client = WebSocket::from_stream_with_extensions(
        join(wire.as_slice(), sink()),
        Role::Client,
        Some(extensions),
        options,
    )
    .expect("the client accepts the extensions");
    let mut messages = Vec::new();
    while messages.len() < count {
        let frame = client.next().await.expect("the message arrives");
        if frame.opcode().is_control() {
            continue;
        }
        assert_eq!(frame.opcode(), OpCode::Text);
        messages.push(frame.payload().clone());
    }
    messages
}

/// Checks that each message is one compressed frame sequence within `limit`.
/// Control frames may come between the frames of a message.
fn assert_bounded(frames: &[WireFrame], limit: usize) {
    let mut first = true;
    for frame in frames.iter().filter(|frame| frame.opcode < 8) {
        assert!(
            frame.len <= limit,
            "a {} byte frame exceeds {limit}",
            frame.len
        );
        assert_eq!(frame.rsv1, first, "RSV1 is set on the first frame only");
        let opcode = if first { 1 } else { 0 };
        assert_eq!(frame.opcode, opcode, "{frame:?}");
        first = frame.fin;
    }
    assert!(first, "the last frame sets FIN");
}

#[tokio::test]
async fn incompressible_messages_stay_within_the_fragment_size() {
    for extensions in [CONTEXT, NO_CONTEXT] {
        let messages = [incompressible(256 * 1024), incompressible(100 * 1024)];
        let wire = send(extensions, 16 * 1024, &messages).await;
        assert_bounded(&wire_frames(&wire), 16 * 1024);
        assert_eq!(receive(extensions, wire, 2).await, messages);
    }
}

#[tokio::test]
async fn compressed_frames_are_filled_to_the_fragment_size() {
    for extensions in [CONTEXT, NO_CONTEXT] {
        let message = compressible(1 << 20);
        let wire = send(extensions, 1024, slice::from_ref(&message)).await;
        let frames = wire_frames(&wire);
        assert_bounded(&frames, 1024);
        // Fragmenting the compressed output, not the input, fills every frame but
        // the last.
        let (last, full) = frames.split_last().expect("at least one frame");
        assert!(full.iter().all(|frame| frame.len == 1024), "{frames:?}");
        assert!(last.len <= 1024);
        assert_eq!(receive(extensions, wire, 1).await, [message]);
    }
}

#[tokio::test]
async fn every_small_fragment_size_round_trips() {
    // Covers frame boundaries that fall inside the deflate block suffix.
    let messages = [incompressible(300), compressible(300), Bytes::new()];
    for fragment_size in 1..=48 {
        for extensions in [CONTEXT, NO_CONTEXT] {
            let wire = send(extensions, fragment_size, &messages).await;
            assert_bounded(&wire_frames(&wire), fragment_size);
            assert_eq!(receive(extensions, wire, 3).await, messages);
        }
    }
}

/// A writer that accepts `budget` more bytes, then stalls until it is raised.
#[derive(Clone, Default)]
struct Throttle {
    capture: Capture,
    budget: Arc<AtomicUsize>,
}

impl AsyncWrite for Throttle {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<IoResult<usize>> {
        let len = self.budget.load(Ordering::SeqCst).min(buf.len());
        if len == 0 {
            return Poll::Pending;
        }
        self.budget.fetch_sub(len, Ordering::SeqCst);
        Pin::new(&mut self.get_mut().capture).poll_write(cx, &buf[..len])
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn a_pong_can_go_out_between_the_frames_of_a_message() {
    // A ping from the client, as the server reads it.
    let ping = {
        let capture = Capture::default();
        let mut client = WebSocket::from_stream_with_extensions(
            join(tokio::io::empty(), capture.clone()),
            Role::Client,
            Some(CONTEXT),
            Options::default().with_low_latency_compression(),
        )
        .expect("the client accepts the extensions");
        client
            .send(Frame::ping("ping"))
            .await
            .expect("the ping is captured");
        capture.bytes()
    };
    let throttle = Throttle::default();
    throttle.budget.store(2048, Ordering::SeqCst);
    let mut server = WebSocket::from_stream_with_extensions(
        join(Cursor::new(ping), throttle.clone()),
        Role::Server,
        Some(CONTEXT),
        Options::default()
            .with_low_latency_compression()
            .with_max_fragment_size(1024),
    )
    .expect("the server accepts the extensions");

    let message = incompressible(64 * 1024);
    // The writer stalls partway through the message.
    assert!(server
        .send(Frame::text(message.clone()))
        .now_or_never()
        .is_none());
    let frame = server
        .next()
        .now_or_never()
        .flatten()
        .expect("the ping is already readable");
    assert_eq!(frame.opcode(), OpCode::Ping);

    // The next send answers the ping first, then finishes the message.
    throttle.budget.store(usize::MAX, Ordering::SeqCst);
    server
        .send(Frame::text("after"))
        .await
        .expect("the writer accepts everything now");

    let wire = throttle.capture.bytes();
    let frames = wire_frames(&wire);
    let pong = frames
        .iter()
        .position(|frame| frame.opcode == 0xa)
        .expect("a pong was sent");
    let last_data = frames
        .iter()
        .rposition(|frame| frame.opcode == 0 && frame.fin)
        .expect("the message ends");
    assert!(pong < last_data, "the pong waited for the whole message");
    assert_bounded(&frames, 1024);
    assert_eq!(
        receive(CONTEXT, wire, 2).await,
        [message, Bytes::from_static(b"after")]
    );
}
